//! Process-level singleton for the live Kindle session.
//!
//! An exclusive advisory lock (`File::try_lock`: `flock(LOCK_EX|LOCK_NB)` on Unix,
//! `LockFileEx` on Windows) is the authority. The lock file may remain after exit;
//! only the kernel lock matters. Process death (including SIGKILL) releases it.
//!
//! Held on a local filesystem (`/tmp` on Kindle), not `/mnt/us`.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

/// Kindle / Unix product path. Local tmpfs; not userstore FUSE.
pub const DEFAULT_LOCK_PATH: &str = "/tmp/scribe-app.lock";

/// Reserved process exit status when this invocation lost the singleton.
/// Must stay in lockstep with `scribe-lifecycle` `CHILD_EXIT_ALREADY_RUNNING`.
pub const EXIT_ALREADY_RUNNING: u8 = 79;

/// RAII owner of the process singleton. Keep this value alive for the whole
/// session — dropping it would let another `scribe-app` start.
#[must_use]
pub struct SingletonGuard {
    _file: File,
}

/// Outcome of non-blocking singleton arbitration.
pub enum StartupGate {
    /// This process owns the live session. Retain the guard until process exit.
    Proceed(SingletonGuard),
    /// Another live `scribe-app` already holds the lock. Do not open devices.
    AlreadyRunning { owner_pid: Option<u32> },
}

/// Try to become the unique live `scribe-app`. Never waits, never signals,
/// never opens framebuffer or input.
pub fn gate_startup(lock_path: &Path) -> Result<StartupGate, String> {
    if let Some(parent) = lock_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("scribe_singleton create {}: {e}", parent.display()))?;
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|e| format!("scribe_singleton open {}: {e}", lock_path.display()))?;
    match file.try_lock() {
        Ok(()) => {
            let pid = std::process::id();
            if let Err(e) = write_pid(&file, pid) {
                eprintln!(
                    "scribe_singleton pid_write_failed lock={} err={e}",
                    lock_path.display()
                );
            }
            println!("scribe_singleton acquired=true pid={pid}");
            let _ = std::io::stdout().flush();
            crate::launch_clock::mark("singleton_acquired");
            Ok(StartupGate::Proceed(SingletonGuard { _file: file }))
        }
        Err(TryLockError::WouldBlock) => {
            let pid = std::process::id();
            let owner_pid = read_owner_pid(lock_path);
            match owner_pid {
                Some(owner) => println!(
                    "scribe_singleton acquired=false already_running=true pid={pid} owner_pid={owner}"
                ),
                None => println!(
                    "scribe_singleton acquired=false already_running=true pid={pid}"
                ),
            }
            let _ = std::io::stdout().flush();
            crate::launch_clock::mark("singleton_already_running");
            Ok(StartupGate::AlreadyRunning { owner_pid })
        }
        Err(TryLockError::Error(e)) => Err(format!(
            "scribe_singleton lock {}: {e}",
            lock_path.display()
        )),
    }
}

/// Run `body` only after winning singleton arbitration. A loser never calls `body`
/// (so framebuffer / input / sync / library code inside it cannot run).
pub fn run_with_singleton<R>(
    lock_path: &Path,
    body: impl FnOnce() -> R,
) -> Result<SingletonRun<R>, String> {
    match gate_startup(lock_path)? {
        StartupGate::Proceed(guard) => {
            let result = body();
            drop(guard);
            Ok(SingletonRun::Ran(result))
        }
        StartupGate::AlreadyRunning { owner_pid } => Ok(SingletonRun::AlreadyRunning { owner_pid }),
    }
}

pub enum SingletonRun<R> {
    Ran(R),
    AlreadyRunning { owner_pid: Option<u32> },
}

/// Map singleton arbitration to the process exit code `scribe-lifecycle` observes.
pub fn process_exit_code(run: SingletonRun<i32>) -> u8 {
    match run {
        SingletonRun::AlreadyRunning { .. } => EXIT_ALREADY_RUNNING,
        SingletonRun::Ran(0) => 0,
        SingletonRun::Ran(_) => 1,
    }
}

fn write_pid(file: &File, pid: u32) -> io::Result<()> {
    file.set_len(0)?;
    let mut file = file;
    file.seek(SeekFrom::Start(0))?;
    write!(file, "{pid}")?;
    file.flush()?;
    Ok(())
}

fn read_owner_pid(path: &Path) -> Option<u32> {
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_lock_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "scribe-app-singleton-{}-{}-{tag}.lock",
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn leftover_lock_file_does_not_block() {
        let path = unique_lock_path("leftover");
        std::fs::write(&path, b"999999\n").unwrap();
        match gate_startup(&path).unwrap() {
            StartupGate::Proceed(guard) => {
                drop(guard);
            }
            StartupGate::AlreadyRunning { .. } => {
                panic!("stale file contents must not be treated as a live holder")
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn second_acquire_fails_immediately_and_skips_device_hooks() {
        let path = unique_lock_path("second");
        let fb_opened = AtomicBool::new(false);
        let input_opened = AtomicBool::new(false);
        let first = match gate_startup(&path).unwrap() {
            StartupGate::Proceed(g) => g,
            StartupGate::AlreadyRunning { owner_pid } => {
                panic!("first acquire must win, owner_pid={owner_pid:?}")
            }
        };

        match run_with_singleton(&path, || {
            fb_opened.store(true, Ordering::SeqCst);
            input_opened.store(true, Ordering::SeqCst);
            "should-not-run"
        })
        .unwrap()
        {
            SingletonRun::AlreadyRunning { owner_pid } => {
                if let Some(owner) = owner_pid {
                    assert_eq!(owner, std::process::id());
                }
            }
            SingletonRun::Ran(_) => panic!("loser must not run the session body"),
        }
        assert!(
            !fb_opened.load(Ordering::SeqCst),
            "loser must not open framebuffer"
        );
        assert!(
            !input_opened.load(Ordering::SeqCst),
            "loser must not open input"
        );

        drop(first);
        let ran = AtomicBool::new(false);
        match run_with_singleton(&path, || {
            ran.store(true, Ordering::SeqCst);
            0i32
        })
        .unwrap()
        {
            SingletonRun::Ran(code) => assert_eq!(code, 0),
            SingletonRun::AlreadyRunning { .. } => {
                panic!("winner after drop must run body")
            }
        }
        assert!(ran.load(Ordering::SeqCst));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn winner_runs_body_with_lock_held() {
        let path = unique_lock_path("winner");
        let nested_lost = AtomicBool::new(false);
        match run_with_singleton(&path, || {
            match gate_startup(&path).unwrap() {
                StartupGate::AlreadyRunning { .. } => nested_lost.store(true, Ordering::SeqCst),
                StartupGate::Proceed(_) => panic!("nested acquire must lose while body holds lock"),
            }
            42
        })
        .unwrap()
        {
            SingletonRun::Ran(code) => assert_eq!(code, 42),
            SingletonRun::AlreadyRunning { .. } => panic!("first run_with_singleton must proceed"),
        }
        assert!(nested_lost.load(Ordering::SeqCst));
        let _ = std::fs::remove_file(&path);
    }

    /// Child of [`process_death_releases_without_explicit_unlock`]: acquire, skip Drop, `_exit`.
    #[test]
    fn singleton_child_hold_and_exit() {
        let Ok(path) = std::env::var("SCRIBE_SINGLETON_CHILD_LOCK") else {
            return;
        };
        match gate_startup(Path::new(&path)).unwrap() {
            StartupGate::Proceed(guard) => {
                std::mem::forget(guard);
                std::process::exit(0);
            }
            StartupGate::AlreadyRunning { .. } => std::process::exit(2),
        }
    }

    #[test]
    fn process_death_releases_without_explicit_unlock() {
        let path = unique_lock_path("death");
        let exe = std::env::current_exe().expect("current_exe");
        let status = Command::new(&exe)
            .arg("--exact")
            .arg("singleton::tests::singleton_child_hold_and_exit")
            .arg("--nocapture")
            .env("SCRIBE_SINGLETON_CHILD_LOCK", &path)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .expect("spawn singleton child");
        assert!(
            status.success(),
            "child must acquire then exit(0) without Drop; status={status}"
        );
        match gate_startup(&path).unwrap() {
            StartupGate::Proceed(_) => {}
            StartupGate::AlreadyRunning { owner_pid } => panic!(
                "kernel must release the lock on process death; owner_pid={owner_pid:?} (no PID-file delete)"
            ),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn second_process_loses_while_parent_holds() {
        let path = unique_lock_path("crossproc");
        let _guard = match gate_startup(&path).unwrap() {
            StartupGate::Proceed(g) => g,
            StartupGate::AlreadyRunning { .. } => panic!("parent must win"),
        };
        let exe = std::env::current_exe().expect("current_exe");
        let output = Command::new(&exe)
            .arg("--exact")
            .arg("singleton::tests::singleton_child_try_only")
            .arg("--nocapture")
            .env("SCRIBE_SINGLETON_CHILD_LOCK", &path)
            .output()
            .expect("spawn try-only child");
        assert!(
            output.status.success(),
            "loser child should exit 0; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            combined.contains("already_running=true"),
            "child must log already_running; output={combined}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn singleton_child_try_only() {
        let Ok(path) = std::env::var("SCRIBE_SINGLETON_CHILD_LOCK") else {
            return;
        };
        match gate_startup(Path::new(&path)).unwrap() {
            StartupGate::AlreadyRunning { .. } => std::process::exit(0),
            StartupGate::Proceed(_) => std::process::exit(3),
        }
    }

    #[test]
    fn process_exit_codes_keep_session_and_already_running_distinct() {
        assert_eq!(process_exit_code(SingletonRun::Ran(0)), 0);
        assert_eq!(process_exit_code(SingletonRun::Ran(1)), 1);
        assert_eq!(process_exit_code(SingletonRun::Ran(2)), 1);
        assert_eq!(
            process_exit_code(SingletonRun::AlreadyRunning { owner_pid: None }),
            EXIT_ALREADY_RUNNING
        );
        assert_ne!(EXIT_ALREADY_RUNNING, 0);
        assert_ne!(EXIT_ALREADY_RUNNING, 1);
        assert_eq!(EXIT_ALREADY_RUNNING, 79);
    }
}
