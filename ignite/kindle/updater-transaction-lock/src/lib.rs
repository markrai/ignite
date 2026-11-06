//! Shared updater transaction lock for Kindle release mutation and verified staging.
//!
//! Uses the same kernel flock identity as `scribe-lifecycle`: non-blocking exclusive
//! acquisition on a singly linked, owner-matched regular file, refusing symlinks and
//! pathname substitution. Holding the returned guard keeps the kernel lock alive.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub const DEFAULT_LOCK_PATH: &str = "/tmp/ignite-updater-transaction.lock";

#[derive(Debug)]
pub enum AcquireError {
    Contended,
    Unsafe(String),
    Io {
        action: &'static str,
        source: io::Error,
    },
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contended => write!(f, "updater transaction is already in progress"),
            Self::Unsafe(detail) => write!(f, "unsafe updater lock: {detail}"),
            Self::Io { action, source } => write!(f, "updater lock {action} failed: {source}"),
        }
    }
}

impl std::error::Error for AcquireError {}

fn io_error(action: &'static str, source: io::Error) -> AcquireError {
    AcquireError::Io { action, source }
}

/// RAII holder for the updater transaction flock. Drop releases the kernel lock.
#[must_use]
pub struct UpdaterLockGuard {
    file: File,
    path: PathBuf,
    #[cfg(unix)]
    identity: UnixIdentity,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
struct UnixIdentity {
    dev: u64,
    ino: u64,
    uid: u32,
}

impl UpdaterLockGuard {
    /// Re-check that `path` still names the same locked regular file (no symlink swap).
    pub fn revalidate(&self, path: &Path) -> Result<(), AcquireError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let path_metadata =
                std::fs::symlink_metadata(path).map_err(|error| io_error("revalidate", error))?;
            if path_metadata.file_type().is_symlink()
                || !path_metadata.file_type().is_file()
                || path_metadata.nlink() != 1
                || path_metadata.dev() != self.identity.dev
                || path_metadata.ino() != self.identity.ino
                || path_metadata.uid() != self.identity.uid
            {
                return Err(AcquireError::Unsafe(
                    "updater lock path no longer identifies the held lock file".to_owned(),
                ));
            }
            if path != self.path.as_path() {
                // Pathname may differ by string form; identity check above is authoritative.
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            let meta = std::fs::symlink_metadata(&self.path)
                .map_err(|error| io_error("revalidate", error))?;
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(AcquireError::Unsafe(
                    "updater lock path is no longer a regular file".to_owned(),
                ));
            }
            Ok(())
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn into_file(self) -> File {
        self.file
    }

    #[cfg(unix)]
    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.file.as_raw_fd()
    }
}

/// Acquire a non-blocking exclusive updater transaction lock at `path`.
pub fn acquire_exclusive(path: &Path) -> Result<UpdaterLockGuard, AcquireError> {
    #[cfg(unix)]
    {
        acquire_exclusive_unix(path)
    }
    #[cfg(not(unix))]
    {
        acquire_exclusive_host(path)
    }
}

#[cfg(unix)]
fn acquire_exclusive_unix(path: &Path) -> Result<UpdaterLockGuard, AcquireError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| io_error("create parent", error))?;
        }
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| io_error("open", error))?;

    let metadata = file
        .metadata()
        .map_err(|error| io_error("inspect", error))?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(AcquireError::Unsafe(
            "lock path is not a singly linked regular file".to_owned(),
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(AcquireError::Unsafe(
            "lock file is not owned by the current effective user".to_owned(),
        ));
    }

    let path_metadata =
        std::fs::symlink_metadata(path).map_err(|error| io_error("reinspect", error))?;
    if path_metadata.file_type().is_symlink()
        || path_metadata.dev() != metadata.dev()
        || path_metadata.ino() != metadata.ino()
    {
        return Err(AcquireError::Unsafe(
            "lock path changed while it was being opened".to_owned(),
        ));
    }

    try_lock_fd(file.as_raw_fd())?;

    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| io_error("set permissions", error))?;

    Ok(UpdaterLockGuard {
        file,
        path: path.to_path_buf(),
        identity: UnixIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
            uid: metadata.uid(),
        },
    })
}

#[cfg(unix)]
fn try_lock_fd(fd: std::os::fd::RawFd) -> Result<(), AcquireError> {
    let result = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
    ) {
        return Err(AcquireError::Contended);
    }
    Err(io_error("acquire", error))
}

#[cfg(not(unix))]
fn acquire_exclusive_host(path: &Path) -> Result<UpdaterLockGuard, AcquireError> {
    use std::fs::TryLockError;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| io_error("create parent", error))?;
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| io_error("open", error))?;
    let metadata = file
        .metadata()
        .map_err(|error| io_error("inspect", error))?;
    if !metadata.is_file() {
        return Err(AcquireError::Unsafe(
            "lock path is not a regular file".to_owned(),
        ));
    }
    let path_metadata =
        std::fs::symlink_metadata(path).map_err(|error| io_error("reinspect", error))?;
    if path_metadata.file_type().is_symlink() {
        return Err(AcquireError::Unsafe(
            "lock path changed while it was being opened".to_owned(),
        ));
    }
    match file.try_lock() {
        Ok(()) => Ok(UpdaterLockGuard {
            file,
            path: path.to_path_buf(),
        }),
        Err(TryLockError::WouldBlock) => Err(AcquireError::Contended),
        Err(TryLockError::Error(error)) => Err(io_error("acquire", error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::process::{Command, Stdio};
    use std::sync::{Mutex, OnceLock};
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    /// Serialize flock-holding tests in this process.
    ///
    /// Linux `fork` duplicates open file descriptions; `O_CLOEXEC` only drops
    /// them at `exec`. A parallel `Command::spawn` can therefore keep another
    /// test's exclusive flock alive across that test's `drop`, yielding a false
    /// `Contended` on reacquire. This is test harness interference, not a
    /// production lock-lifetime defect (staging/lifecycle do not fork mid-hold
    /// except the intentional inheritable lifecycle handoff).
    fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct TestPath(PathBuf);

    impl TestPath {
        fn new(label: &str) -> Self {
            Self(std::env::temp_dir().join(format!(
                "ignite-updater-txlock-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            )))
        }
    }

    impl Drop for TestPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn exclusive_acquisition_contends_and_releases() {
        let _serial = serial_guard();
        let path = TestPath::new("exclusive");
        let first = acquire_exclusive(&path.0).expect("first");
        assert!(matches!(
            acquire_exclusive(&path.0),
            Err(AcquireError::Contended)
        ));
        first.revalidate(&path.0).expect("identity stable");
        drop(first);
        let second = acquire_exclusive(&path.0).expect("after release");
        second.revalidate(&path.0).expect("revalidate");
    }

    #[cfg(unix)]
    #[test]
    fn lock_fd_is_cloexec_and_not_inherited_across_exec() {
        let _serial = serial_guard();
        let path = TestPath::new("cloexec");
        let guard = acquire_exclusive(&path.0).expect("acquire");
        let fd = guard.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(
            flags & libc::FD_CLOEXEC != 0,
            "lock fd must be CLOEXEC, flags={flags:#x}"
        );
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "if [ -e /proc/self/fd/{fd} ]; then echo yes; else echo no; fi"
            ))
            .stdin(Stdio::null())
            .output()
            .expect("spawn");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "no",
            "child inherited lock fd: {stdout:?} stderr={:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        drop(guard);
        let again = acquire_exclusive(&path.0).expect("after drop");
        drop(again);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_lock_path_is_refused() {
        let _serial = serial_guard();
        use std::os::unix::fs::symlink;
        let target = TestPath::new("target");
        std::fs::write(&target.0, b"target").expect("target");
        let link = TestPath::new("symlink");
        symlink(&target.0, &link.0).expect("symlink");
        assert!(matches!(
            acquire_exclusive(&link.0),
            Err(AcquireError::Io { .. } | AcquireError::Unsafe(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn killed_holder_releases_kernel_lock() {
        let _serial = serial_guard();
        let lock_path = TestPath::new("crash-lock");
        let ready_path = TestPath::new("crash-ready");
        let executable = std::env::current_exe().expect("test executable");
        let mut child = Command::new(&executable)
            .args([
                "--exact",
                "tests::holder_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("IGNITE_TEST_UPDATER_TXLOCK_PATH", &lock_path.0)
            .env("IGNITE_TEST_UPDATER_TXLOCK_READY", &ready_path.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn holder");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready_path.0.exists() {
            assert!(
                child.try_wait().expect("poll").is_none(),
                "holder exited early"
            );
            assert!(Instant::now() < deadline, "holder readiness timed out");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(
            acquire_exclusive(&lock_path.0),
            Err(AcquireError::Contended)
        ));
        child.kill().expect("kill");
        child.wait().expect("reap");
        let _held = acquire_exclusive(&lock_path.0).expect("after death");
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess fixture"]
    fn holder_fixture() {
        // No serial_guard: runs in a child process while the parent
        // killed_holder_releases_kernel_lock already holds the mutex.
        let path =
            PathBuf::from(std::env::var_os("IGNITE_TEST_UPDATER_TXLOCK_PATH").expect("lock path"));
        let ready =
            PathBuf::from(std::env::var_os("IGNITE_TEST_UPDATER_TXLOCK_READY").expect("ready"));
        let _guard = acquire_exclusive(&path).expect("hold");
        std::fs::write(ready, b"ready\n").expect("ready");
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }
}
