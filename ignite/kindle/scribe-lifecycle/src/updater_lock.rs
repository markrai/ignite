//! Process-scoped serialization for operations that mutate the installed release.
//!
//! The descriptor is deliberately inherited across `exec`: the installer shell becomes the
//! lock holder, so a killed wrapper cannot release the lock while a child keeps mutating state.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const RUN_ARGUMENT: &str = "--with-updater-transaction-lock";
pub const VERIFY_ARGUMENT: &str = "--verify-updater-transaction-lock";
pub const DEFAULT_LOCK_PATH: &str = "/tmp/ignite-updater-transaction.lock";
pub const HELD_ENV: &str = "IGNITE_UPDATER_LOCK_HELD";
pub const FD_ENV: &str = "IGNITE_UPDATER_LOCK_FD";
#[cfg(debug_assertions)]
const TEST_LOCK_PATH_ENV: &str = "IGNITE_TEST_UPDATER_LOCK_PATH";

#[derive(Debug)]
pub enum LockError {
    Usage(&'static str),
    Contended,
    Unsafe(String),
    Io {
        action: &'static str,
        source: io::Error,
    },
}

impl LockError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            Self::Contended => 75,
            Self::Unsafe(_) | Self::Io { .. } => 1,
        }
    }
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(detail) => write!(f, "{detail}"),
            Self::Contended => write!(f, "updater transaction is already in progress"),
            Self::Unsafe(detail) => write!(f, "unsafe updater lock: {detail}"),
            Self::Io { action, source } => write!(f, "updater lock {action} failed: {source}"),
        }
    }
}

fn io_error(action: &'static str, source: io::Error) -> LockError {
    LockError::Io { action, source }
}

fn configured_lock_path() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os(TEST_LOCK_PATH_ENV) {
        return PathBuf::from(path);
    }
    PathBuf::from(DEFAULT_LOCK_PATH)
}

fn try_lock_fd(fd: RawFd) -> Result<(), LockError> {
    let result = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
    ) {
        return Err(LockError::Contended);
    }
    Err(io_error("acquire", error))
}

fn acquire_at(path: &Path) -> Result<File, LockError> {
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
        return Err(LockError::Unsafe(
            "lock path is not a singly linked regular file".to_owned(),
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(LockError::Unsafe(
            "lock file is not owned by the current effective user".to_owned(),
        ));
    }

    let path_metadata =
        std::fs::symlink_metadata(path).map_err(|error| io_error("reinspect", error))?;
    if path_metadata.file_type().is_symlink()
        || path_metadata.dev() != metadata.dev()
        || path_metadata.ino() != metadata.ino()
    {
        return Err(LockError::Unsafe(
            "lock path changed while it was being opened".to_owned(),
        ));
    }

    try_lock_fd(file.as_raw_fd())?;

    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| io_error("set permissions", error))?;
    Ok(file)
}

/// Held by a future promotion decision after it has acquired the lifecycle launch gate.
/// Keeping the file alive keeps the kernel lock alive; no pathname or environment marker is
/// treated as authority.
pub(crate) struct UpdaterTransactionGuard {
    _lock: File,
}

pub(crate) fn acquire_for_decision() -> Result<UpdaterTransactionGuard, LockError> {
    acquire_at(&configured_lock_path()).map(|lock| UpdaterTransactionGuard { _lock: lock })
}

fn verify_inherited_fd_at(fd: RawFd, path: &Path) -> Result<(), LockError> {
    let descriptor_path = PathBuf::from(format!("/proc/self/fd/{fd}"));
    let descriptor_metadata = std::fs::metadata(&descriptor_path)
        .map_err(|error| io_error("inspect inherited descriptor", error))?;
    if !descriptor_metadata.file_type().is_file() || descriptor_metadata.nlink() != 1 {
        return Err(LockError::Unsafe(
            "inherited descriptor is not a singly linked regular file".to_owned(),
        ));
    }
    if descriptor_metadata.uid() != unsafe { libc::geteuid() } {
        return Err(LockError::Unsafe(
            "inherited lock descriptor is not owned by the effective user".to_owned(),
        ));
    }

    let path_metadata = std::fs::symlink_metadata(path)
        .map_err(|error| io_error("inspect inherited lock path", error))?;
    if path_metadata.file_type().is_symlink()
        || path_metadata.dev() != descriptor_metadata.dev()
        || path_metadata.ino() != descriptor_metadata.ino()
    {
        return Err(LockError::Unsafe(
            "inherited descriptor does not identify the updater lock file".to_owned(),
        ));
    }

    // This operates on the same open file description inherited by the shell. If it was forged but
    // unlocked, this establishes the lock before mutation; if another description owns the lock,
    // the check fails with the same deterministic contention result as initial acquisition.
    try_lock_fd(fd)
}

fn clear_cloexec(fd: RawFd) -> Result<(), LockError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io_error(
            "read descriptor flags",
            io::Error::last_os_error(),
        ));
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io_error(
            "make descriptor inheritable",
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn parse_command(args: impl IntoIterator<Item = OsString>) -> Result<Vec<OsString>, LockError> {
    let args: Vec<OsString> = args.into_iter().collect();
    if args.len() < 3 || args[0] != OsStr::new(RUN_ARGUMENT) || args[1] != OsStr::new("--") {
        return Err(LockError::Usage(
            "usage: scribe-lifecycle --with-updater-transaction-lock -- PROGRAM [ARG ...]",
        ));
    }
    Ok(args[2..].to_vec())
}

fn exec_from_environment() -> Result<(), LockError> {
    let command = parse_command(std::env::args_os().skip(1))?;
    exec_with_lock(&configured_lock_path(), &command)
}

fn verify_from_environment() -> Result<(), LockError> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.len() != 2 || args[0] != OsStr::new(VERIFY_ARGUMENT) {
        return Err(LockError::Usage(
            "usage: scribe-lifecycle --verify-updater-transaction-lock FD",
        ));
    }
    let fd_text = args[1].to_str().ok_or(LockError::Usage(
        "updater lock descriptor must be a decimal integer",
    ))?;
    if fd_text.is_empty() || !fd_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(LockError::Usage(
            "updater lock descriptor must be a decimal integer",
        ));
    }
    let fd = fd_text.parse::<RawFd>().map_err(|_| {
        LockError::Usage("updater lock descriptor must be a valid decimal descriptor")
    })?;
    verify_inherited_fd_at(fd, &configured_lock_path())
}

pub fn run_requested_command() -> Option<Result<(), LockError>> {
    match std::env::args_os().nth(1).as_deref() {
        Some(argument) if argument == OsStr::new(RUN_ARGUMENT) => Some(exec_from_environment()),
        Some(argument) if argument == OsStr::new(VERIFY_ARGUMENT) => {
            Some(verify_from_environment())
        }
        _ => None,
    }
}

fn exec_with_lock(path: &Path, command: &[OsString]) -> Result<(), LockError> {
    let lock = acquire_at(path)?;
    let fd = lock.as_raw_fd();
    clear_cloexec(fd)?;

    let error = Command::new(&command[0])
        .args(&command[1..])
        .env(HELD_ENV, "1")
        .env(FD_ENV, fd.to_string())
        .exec();
    Err(io_error("exec", error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock_test_support::{descriptor_holders, RELEASE_VISIBILITY_DEADLINE};
    use std::os::unix::fs::symlink;
    use std::process::{Child, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    struct TestPath(PathBuf);

    impl TestPath {
        fn new(label: &str) -> Self {
            Self(std::env::temp_dir().join(format!(
                "ignite-updater-lock-{label}-{}-{}",
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

    fn wait_for(path: &Path, child: &mut Child) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                child.try_wait().expect("poll holder").is_none(),
                "holder exited before publishing readiness"
            );
            assert!(Instant::now() < deadline, "holder readiness timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn lock_is_exclusive() {
        let path = TestPath::new("exclusive");
        let first = acquire_at(&path.0).expect("first lock");
        assert!(matches!(acquire_at(&path.0), Err(LockError::Contended)));
        drop(first);
        acquire_after_release(&path.0);
    }

    /// Acquire a lock this test has just released. See `lock_test_support` for why release is only
    /// eventually visible under the parallel harness; a lock that stays held still fails.
    fn acquire_after_release(path: &Path) -> File {
        let deadline = Instant::now() + RELEASE_VISIBILITY_DEADLINE;
        loop {
            match acquire_at(path) {
                Err(LockError::Contended) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(LockError::Contended) => panic!(
                    "lock still contended {RELEASE_VISIBILITY_DEADLINE:?} after release; \
                     descriptor holders of {}: {:?}",
                    path.display(),
                    descriptor_holders(path)
                ),
                result => return result.expect("lock after release"),
            }
        }
    }

    #[test]
    fn verifier_establishes_a_missing_lock_before_returning_success() {
        let path = TestPath::new("verify-acquire");
        let unlocked = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path.0)
            .expect("unlocked descriptor");
        verify_inherited_fd_at(unlocked.as_raw_fd(), &path.0).expect("verify and acquire");
        assert!(matches!(acquire_at(&path.0), Err(LockError::Contended)));
    }

    #[test]
    fn verifier_preserves_an_existing_inherited_lock() {
        let path = TestPath::new("verify-existing");
        let holder = acquire_at(&path.0).expect("holder");
        verify_inherited_fd_at(holder.as_raw_fd(), &path.0).expect("verify existing lock");
        assert!(matches!(acquire_at(&path.0), Err(LockError::Contended)));
    }

    #[test]
    fn verifier_rejects_an_unlocked_description_while_another_holder_owns_lock() {
        let path = TestPath::new("verify-forgery");
        let forged = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path.0)
            .expect("forged descriptor");
        let holder = acquire_at(&path.0).expect("real holder");
        assert!(matches!(
            verify_inherited_fd_at(forged.as_raw_fd(), &path.0),
            Err(LockError::Contended)
        ));
        drop(holder);
    }

    #[test]
    fn symlink_is_refused() {
        let target = TestPath::new("target");
        std::fs::write(&target.0, b"target").expect("target");
        let link = TestPath::new("symlink");
        symlink(&target.0, &link.0).expect("symlink");
        assert!(matches!(acquire_at(&link.0), Err(LockError::Io { .. })));
    }

    #[test]
    fn killed_holder_releases_kernel_lock() {
        let lock_path = TestPath::new("crash-lock");
        let ready_path = TestPath::new("crash-ready");
        let executable = std::env::current_exe().expect("test executable");
        let mut child = Command::new(executable)
            .args([
                "--exact",
                "updater_lock::tests::holder_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("IGNITE_TEST_LOCK_PATH", &lock_path.0)
            .env("IGNITE_TEST_LOCK_READY", &ready_path.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn holder");
        wait_for(&ready_path.0, &mut child);
        assert!(matches!(
            acquire_at(&lock_path.0),
            Err(LockError::Contended)
        ));
        child.kill().expect("kill holder");
        child.wait().expect("reap holder");
        acquire_at(&lock_path.0).expect("lock after holder death");
    }

    #[test]
    #[ignore = "subprocess fixture"]
    fn holder_fixture() {
        let path = PathBuf::from(std::env::var_os("IGNITE_TEST_LOCK_PATH").expect("lock path"));
        let executable = std::env::current_exe().expect("test executable");
        let command = vec![
            executable.into_os_string(),
            OsString::from("--exact"),
            OsString::from("updater_lock::tests::inherited_holder_fixture"),
            OsString::from("--ignored"),
            OsString::from("--nocapture"),
        ];
        exec_with_lock(&path, &command).expect("exec inherited holder");
    }

    #[test]
    #[ignore = "subprocess fixture"]
    fn inherited_holder_fixture() {
        let ready = PathBuf::from(std::env::var_os("IGNITE_TEST_LOCK_READY").expect("ready path"));
        let fd: RawFd = std::env::var(FD_ENV)
            .expect("inherited fd")
            .parse()
            .expect("numeric fd");
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0, "lock descriptor is open after exec");
        assert_eq!(flags & libc::FD_CLOEXEC, 0, "lock descriptor is inherited");
        std::fs::write(ready, b"ready\n").expect("publish ready");
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    fn command_syntax_is_strict() {
        assert!(matches!(
            parse_command([OsString::from(RUN_ARGUMENT)]),
            Err(LockError::Usage(_))
        ));
        let parsed = parse_command([
            OsString::from(RUN_ARGUMENT),
            OsString::from("--"),
            OsString::from("/bin/true"),
        ])
        .expect("command");
        assert_eq!(parsed, [OsString::from("/bin/true")]);
    }
}
