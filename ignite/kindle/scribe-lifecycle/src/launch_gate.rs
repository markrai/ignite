//! Cross-process launch admission and evidence-worker quiescence.
//!
//! Ordinary `go` handling takes a shared launch-gate lock only while it decides whether to spawn,
//! spawns the child, and hands the already-acquired worker token to the asynchronous evidence
//! thread. A future promotion decision takes the gate exclusively, then the updater transaction
//! lock, drains worker activity, and finally takes the evidence lock. No release state is changed.

use crate::candidate_health::{
    acquire_decision_evidence_lock, DecisionEvidenceLock, EvidenceReport, DEFAULT_UPDATE_DIR,
};
use crate::updater_lock::{self, UpdaterTransactionGuard};
use std::ffi::OsStr;
#[cfg(debug_assertions)]
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const DECISION_CHECK_ARGUMENT: &str = "--check-promotion-decision-boundary";
const DEFAULT_GATE_PATH: &str = "/tmp/ignite-lifecycle-launch-gate.lock";
const DEFAULT_HOST_PATH: &str = "/tmp/ignite-lifecycle-host.lock";
const DEFAULT_CHILD_PATH: &str = "/tmp/ignite-lifecycle-child.lock";
const DEFAULT_WORKER_PATH: &str = "/tmp/ignite-lifecycle-evidence-worker.lock";
const DEFAULT_CONTROL_PATH: &str = "/tmp/ignite-lifecycle-decision.sock";
const CONTROL_REQUEST: &[u8] = b"IGNITE_LIFECYCLE_QUIESCENCE_V1\n";
const DECISION_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(debug_assertions)]
const TEST_HOST_ARGUMENT: &str = "--test-launch-coordination-host";
#[cfg(debug_assertions)]
const TEST_ADMISSION_ARGUMENT: &str = "--test-launch-admission";
#[cfg(debug_assertions)]
const TEST_HOLD_DECISION_ARGUMENT: &str = "--test-hold-promotion-decision-boundary";

const TEST_GATE_ENV: &str = "IGNITE_TEST_LAUNCH_GATE_PATH";
const TEST_HOST_ENV: &str = "IGNITE_TEST_LAUNCH_HOST_PATH";
const TEST_CHILD_ENV: &str = "IGNITE_TEST_LAUNCH_CHILD_PATH";
const TEST_WORKER_ENV: &str = "IGNITE_TEST_EVIDENCE_WORKER_PATH";
const TEST_CONTROL_ENV: &str = "IGNITE_TEST_LAUNCH_CONTROL_PATH";
const TEST_UPDATE_ENV: &str = "IGNITE_TEST_UPDATE_DIR";
#[cfg(debug_assertions)]
const TEST_TIMEOUT_ENV: &str = "IGNITE_TEST_DECISION_TIMEOUT_MS";

#[derive(Clone, Debug)]
struct CoordinationPaths {
    gate: PathBuf,
    host: PathBuf,
    child: PathBuf,
    worker: PathBuf,
    control: PathBuf,
    update_dir: PathBuf,
}

impl CoordinationPaths {
    fn configured() -> Self {
        Self {
            gate: configured_path(DEFAULT_GATE_PATH, test_env(TEST_GATE_ENV)),
            host: configured_path(DEFAULT_HOST_PATH, test_env(TEST_HOST_ENV)),
            child: configured_path(DEFAULT_CHILD_PATH, test_env(TEST_CHILD_ENV)),
            worker: configured_path(DEFAULT_WORKER_PATH, test_env(TEST_WORKER_ENV)),
            control: configured_path(DEFAULT_CONTROL_PATH, test_env(TEST_CONTROL_ENV)),
            update_dir: configured_path(DEFAULT_UPDATE_DIR, test_env(TEST_UPDATE_ENV)),
        }
    }
}

#[cfg(debug_assertions)]
fn test_env(name: &'static str) -> Option<&'static str> {
    Some(name)
}

#[cfg(not(debug_assertions))]
fn test_env(_name: &'static str) -> Option<&'static str> {
    None
}

fn configured_path(default: &str, test_name: Option<&str>) -> PathBuf {
    if let Some(name) = test_name {
        if let Some(path) = std::env::var_os(name) {
            return PathBuf::from(path);
        }
    }
    PathBuf::from(default)
}

fn decision_timeout() -> Duration {
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var(TEST_TIMEOUT_ENV) {
        if let Ok(milliseconds) = value.parse::<u64>() {
            return Duration::from_millis(milliseconds.min(30_000));
        }
    }
    DECISION_TIMEOUT
}

#[derive(Debug)]
pub enum BoundaryError {
    Usage,
    Contended(&'static str),
    Unavailable(String),
    Unsafe(String),
    Io {
        action: &'static str,
        source: io::Error,
    },
}

impl BoundaryError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage => 2,
            Self::Contended(_) => 75,
            Self::Unavailable(_) | Self::Unsafe(_) | Self::Io { .. } => 1,
        }
    }
}

impl fmt::Display for BoundaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => write!(f, "usage: scribe-lifecycle {DECISION_CHECK_ARGUMENT}"),
            Self::Contended(boundary) => write!(f, "{boundary} is busy"),
            Self::Unavailable(detail) => write!(f, "decision boundary unavailable: {detail}"),
            Self::Unsafe(detail) => write!(f, "unsafe decision coordination path: {detail}"),
            Self::Io { action, source } => {
                write!(f, "decision boundary {action} failed: {source}")
            }
        }
    }
}

fn io_error(action: &'static str, source: io::Error) -> BoundaryError {
    BoundaryError::Io { action, source }
}

#[derive(Clone, Copy)]
enum LockMode {
    Shared,
    Exclusive,
}

struct KernelLock(File);

impl KernelLock {
    fn acquire(
        path: &Path,
        mode: LockMode,
        timeout: Duration,
        label: &'static str,
    ) -> Result<Self, BoundaryError> {
        let file = open_safe_lock(path, label)?;
        let operation = match mode {
            LockMode::Shared => libc::LOCK_SH,
            LockMode::Exclusive => libc::LOCK_EX,
        } | libc::LOCK_NB;
        let deadline = Instant::now() + timeout;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
                return Ok(Self(file));
            }
            let error = io::Error::last_os_error();
            if !is_contended(&error) {
                return Err(io_error("lock", error));
            }
            if timeout.is_zero() || Instant::now() >= deadline {
                return Err(BoundaryError::Contended(label));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn open_safe_lock(path: &Path, label: &'static str) -> Result<File, BoundaryError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| io_error("open coordination lock", error))?;
    let metadata = file
        .metadata()
        .map_err(|error| io_error("inspect coordination lock", error))?;
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(BoundaryError::Unsafe(format!(
            "{label} is not a singly linked regular file owned by the effective user"
        )));
    }
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| io_error("reinspect coordination lock", error))?;
    if path_metadata.file_type().is_symlink()
        || path_metadata.dev() != metadata.dev()
        || path_metadata.ino() != metadata.ino()
    {
        return Err(BoundaryError::Unsafe(format!(
            "{label} changed while it was opened"
        )));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| io_error("set coordination lock permissions", error))?;
    Ok(file)
}

fn is_contended(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
    )
}

fn clear_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub struct LaunchAdmission {
    _lock: KernelLock,
}

pub struct EvidenceWorkerActivity {
    _lock: KernelLock,
}

pub struct RunningChildActivity {
    lock: KernelLock,
}

impl RunningChildActivity {
    /// Arrange for the application process itself to retain the activity token. If the lifecycle
    /// host dies, an orphan child therefore continues to block a decision until it exits.
    pub fn configure_command(&self, command: &mut Command) {
        let fd = self.lock.0.as_raw_fd();
        unsafe {
            command.pre_exec(move || clear_cloexec(fd));
        }
    }
}

#[derive(Debug)]
enum LatestWorker {
    NoLaunchInThisHost,
    InProgress { launch_uuid: String },
    Complete { launch_uuid: String },
    Uncertain { launch_uuid: String },
    CoordinationUncertain { launch_uuid: String },
}

impl LatestWorker {
    fn response(&self) -> String {
        match self {
            Self::Complete { launch_uuid } => format!("READY launch_uuid={launch_uuid}\n"),
            Self::NoLaunchInThisHost => {
                "BLOCKED reason=no_launch_completed_in_current_host\n".to_owned()
            }
            Self::InProgress { launch_uuid } => {
                format!("BLOCKED reason=worker_in_progress launch_uuid={launch_uuid}\n")
            }
            Self::Uncertain { launch_uuid } => {
                format!("BLOCKED reason=worker_incomplete launch_uuid={launch_uuid}\n")
            }
            Self::CoordinationUncertain { launch_uuid } => {
                format!("BLOCKED reason=worker_coordination_unproven launch_uuid={launch_uuid}\n")
            }
        }
    }
}

/// Host-owned coordination. Failure to construct this disables future decisions but need not make
/// Ignite unavailable; a decision requires the live control endpoint and therefore fails closed.
pub struct HostLaunchCoordination {
    paths: CoordinationPaths,
    _host_lease: KernelLock,
    latest: Arc<Mutex<LatestWorker>>,
    _server: thread::JoinHandle<()>,
}

impl HostLaunchCoordination {
    pub fn start() -> Result<Self, BoundaryError> {
        Self::start_at(CoordinationPaths::configured())
    }

    fn start_at(paths: CoordinationPaths) -> Result<Self, BoundaryError> {
        let host_lease = KernelLock::acquire(
            &paths.host,
            LockMode::Exclusive,
            Duration::ZERO,
            "lifecycle host lease",
        )?;
        let listener = bind_control_socket(&paths.control)?;
        let latest = Arc::new(Mutex::new(LatestWorker::NoLaunchInThisHost));
        let server_latest = Arc::clone(&latest);
        let server = thread::Builder::new()
            .name("ignite-lifecycle-control".to_owned())
            .spawn(move || serve_control(listener, server_latest))
            .map_err(|error| io_error("start lifecycle control server", error))?;
        Ok(Self {
            paths,
            _host_lease: host_lease,
            latest,
            _server: server,
        })
    }

    pub fn admit_launch(&self) -> Result<LaunchAdmission, BoundaryError> {
        KernelLock::acquire(
            &self.paths.gate,
            LockMode::Shared,
            Duration::ZERO,
            "lifecycle launch gate",
        )
        .map(|lock| LaunchAdmission { _lock: lock })
    }

    pub fn evidence_worker_activity(&self) -> Result<EvidenceWorkerActivity, BoundaryError> {
        KernelLock::acquire(
            &self.paths.worker,
            LockMode::Shared,
            Duration::ZERO,
            "evidence worker activity",
        )
        .map(|lock| EvidenceWorkerActivity { _lock: lock })
    }

    pub fn running_child_activity(&self) -> Result<RunningChildActivity, BoundaryError> {
        KernelLock::acquire(
            &self.paths.child,
            LockMode::Shared,
            Duration::ZERO,
            "running lifecycle child",
        )
        .map(|lock| RunningChildActivity { lock })
    }

    pub fn begin_launch(&self, launch_uuid: &str) {
        if let Ok(mut latest) = self.latest.lock() {
            // An untracked worker may outlive later launches. Only host-process death proves all
            // such threads are gone, so a new launch cannot clear this host-generation poison.
            if matches!(&*latest, LatestWorker::CoordinationUncertain { .. }) {
                return;
            }
            *latest = LatestWorker::InProgress {
                launch_uuid: launch_uuid.to_owned(),
            };
        }
    }

    pub fn evidence_report(&self, report: &EvidenceReport) {
        if !report.terminal {
            return;
        }
        let Ok(mut latest) = self.latest.lock() else {
            return;
        };
        // Durable evidence can describe what an untracked worker observed, but it cannot
        // retroactively prove that no worker escaped the decision boundary. Only a still-tracked
        // InProgress launch may become complete; coordination loss poisons the host generation.
        if matches!(
            &*latest,
            LatestWorker::InProgress { launch_uuid } if launch_uuid == &report.launch_uuid
        ) {
            *latest = if report.result.is_ok() {
                LatestWorker::Complete {
                    launch_uuid: report.launch_uuid.clone(),
                }
            } else {
                LatestWorker::Uncertain {
                    launch_uuid: report.launch_uuid.clone(),
                }
            };
        }
    }

    pub fn mark_worker_unavailable(&self, launch_uuid: &str) {
        if let Ok(mut latest) = self.latest.lock() {
            let current = matches!(
                &*latest,
                LatestWorker::InProgress { launch_uuid: current } if current == launch_uuid
            );
            if current {
                *latest = LatestWorker::CoordinationUncertain {
                    launch_uuid: launch_uuid.to_owned(),
                };
            }
        }
    }
}

/// If full host coordination cannot be established, keep the launch gate shared for the host's
/// lifetime. Ordinary launches remain possible, but no decision can mistake a different/stale host
/// lease for coordination with this process.
pub fn block_decisions_after_setup_failure() -> Result<LaunchAdmission, BoundaryError> {
    let paths = CoordinationPaths::configured();
    KernelLock::acquire(
        &paths.gate,
        LockMode::Shared,
        Duration::ZERO,
        "degraded lifecycle decision blocker",
    )
    .map(|lock| LaunchAdmission { _lock: lock })
}

fn bind_control_socket(path: &Path) -> Result<UnixListener, BoundaryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket()
                || metadata.nlink() != 1
                || metadata.uid() != unsafe { libc::geteuid() }
            {
                return Err(BoundaryError::Unsafe(
                    "lifecycle control path is not an owned socket".to_owned(),
                ));
            }
            fs::remove_file(path)
                .map_err(|error| io_error("remove stale control socket", error))?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("inspect lifecycle control socket", error)),
    }
    let listener = UnixListener::bind(path)
        .map_err(|error| io_error("bind lifecycle control socket", error))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| io_error("set lifecycle control socket permissions", error))?;
    Ok(listener)
}

fn serve_control(listener: UnixListener, latest: Arc<Mutex<LatestWorker>>) {
    for connection in listener.incoming() {
        let Ok(mut connection) = connection else {
            break;
        };
        let _ = connection.set_read_timeout(Some(CONTROL_TIMEOUT));
        let _ = connection.set_write_timeout(Some(CONTROL_TIMEOUT));
        let mut request = [0_u8; CONTROL_REQUEST.len()];
        if connection.read_exact(&mut request).is_err() || request != CONTROL_REQUEST {
            let _ = connection.write_all(b"BLOCKED reason=invalid_request\n");
            continue;
        }
        let response = latest
            .lock()
            .map(|state| state.response())
            .unwrap_or_else(|_| "BLOCKED reason=host_state_unavailable\n".to_owned());
        let _ = connection.write_all(response.as_bytes());
    }
}

struct DecisionBoundary {
    _gate: KernelLock,
    _updater: UpdaterTransactionGuard,
    _child: KernelLock,
    _workers: KernelLock,
    _evidence: DecisionEvidenceLock,
    latest_launch_uuid: String,
}

impl DecisionBoundary {
    fn acquire() -> Result<Self, BoundaryError> {
        let paths = CoordinationPaths::configured();
        let timeout = decision_timeout();
        let gate = KernelLock::acquire(
            &paths.gate,
            LockMode::Exclusive,
            timeout,
            "lifecycle launch gate",
        )?;
        let updater = updater_lock::acquire_for_decision().map_err(|error| {
            if error.exit_code() == 75 {
                BoundaryError::Contended("updater transaction lock")
            } else {
                BoundaryError::Unavailable(error.to_string())
            }
        })?;
        require_live_host(&paths.host)?;
        let child = KernelLock::acquire(
            &paths.child,
            LockMode::Exclusive,
            Duration::ZERO,
            "running lifecycle child",
        )?;
        let workers = KernelLock::acquire(
            &paths.worker,
            LockMode::Exclusive,
            timeout,
            "evidence worker quiescence",
        )?;
        let latest_launch_uuid = query_host(&paths.control)?;
        let evidence = acquire_decision_evidence_lock(&paths.update_dir, timeout)
            .map_err(|error| BoundaryError::Unavailable(error.to_string()))?;
        Ok(Self {
            _gate: gate,
            _updater: updater,
            _child: child,
            _workers: workers,
            _evidence: evidence,
            latest_launch_uuid,
        })
    }
}

fn require_live_host(path: &Path) -> Result<(), BoundaryError> {
    match KernelLock::acquire(
        path,
        LockMode::Exclusive,
        Duration::ZERO,
        "lifecycle host lease",
    ) {
        Err(BoundaryError::Contended(_)) => Ok(()),
        Ok(_available) => Err(BoundaryError::Unavailable(
            "no lifecycle host owns the current host lease".to_owned(),
        )),
        Err(error) => Err(error),
    }
}

fn query_host(path: &Path) -> Result<String, BoundaryError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| io_error("inspect lifecycle control socket", error))?;
    if !metadata.file_type().is_socket()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(BoundaryError::Unsafe(
            "lifecycle control endpoint is not an owned socket".to_owned(),
        ));
    }
    let mut stream = UnixStream::connect(path)
        .map_err(|error| io_error("connect to lifecycle control endpoint", error))?;
    stream
        .set_read_timeout(Some(CONTROL_TIMEOUT))
        .map_err(|error| io_error("configure lifecycle control read timeout", error))?;
    stream
        .set_write_timeout(Some(CONTROL_TIMEOUT))
        .map_err(|error| io_error("configure lifecycle control write timeout", error))?;
    stream
        .write_all(CONTROL_REQUEST)
        .map_err(|error| io_error("request lifecycle quiescence", error))?;
    let mut response = Vec::new();
    stream
        .take(256)
        .read_to_end(&mut response)
        .map_err(|error| io_error("read lifecycle quiescence response", error))?;
    let response = std::str::from_utf8(&response)
        .map_err(|_| BoundaryError::Unavailable("non-UTF-8 host response".to_owned()))?;
    let Some(uuid) = response
        .strip_prefix("READY launch_uuid=")
        .and_then(|value| value.strip_suffix('\n'))
    else {
        return Err(BoundaryError::Unavailable(format!(
            "lifecycle host refused quiescence: {}",
            response.trim_end()
        )));
    };
    if !crate::launch_health::valid_attempt_id(uuid) {
        return Err(BoundaryError::Unavailable(
            "lifecycle host returned a malformed launch UUID".to_owned(),
        ));
    }
    Ok(uuid.to_owned())
}

fn check_decision_boundary() -> Result<(), BoundaryError> {
    if std::env::args_os().count() != 2 {
        return Err(BoundaryError::Usage);
    }
    let boundary = DecisionBoundary::acquire()?;
    println!(
        "IGNITE_PROMOTION_DECISION_BOUNDARY_V1\nstatus=ready\nlaunch_uuid={}\nauthorization=not_granted",
        boundary.latest_launch_uuid
    );
    Ok(())
}

/// Debug-only subprocess fixture. The decision operation and every guard remain in this process;
/// no arbitrary program receives descriptors capable of shortening the boundary lifetime.
#[cfg(debug_assertions)]
fn run_test_decision_holder() -> Result<(), BoundaryError> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.len() != 3 || args[0] != OsStr::new(TEST_HOLD_DECISION_ARGUMENT) {
        return Err(BoundaryError::Usage);
    }
    let ready = PathBuf::from(&args[1]);
    let hold_milliseconds = args[2]
        .to_str()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value <= 60_000)
        .ok_or_else(|| BoundaryError::Unavailable("invalid fixture hold duration".to_owned()))?;
    let _boundary = DecisionBoundary::acquire()?;
    fs::write(&ready, b"ready\n")
        .map_err(|error| io_error("write decision fixture readiness", error))?;
    thread::sleep(Duration::from_millis(hold_milliseconds));
    Ok(())
}

#[cfg(debug_assertions)]
fn run_test_host_fixture() -> Result<(), BoundaryError> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.len() != 3 || args[0] != OsStr::new(TEST_HOST_ARGUMENT) {
        return Err(BoundaryError::Usage);
    }
    let mode = args[1]
        .to_str()
        .ok_or_else(|| BoundaryError::Unavailable("invalid fixture mode".to_owned()))?;
    let ready = PathBuf::from(&args[2]);
    let coordination = HostLaunchCoordination::start()?;
    let launch_uuid = "00112233445566778899aabbccddeeff";
    coordination.begin_launch(launch_uuid);
    let mut worker_activity = None;
    let mut child_activity = None;
    match mode {
        "ready" => coordination.evidence_report(&EvidenceReport {
            launch_uuid: launch_uuid.to_owned(),
            outcome: "fixture_complete",
            result: Ok(()),
            terminal: true,
        }),
        "incomplete" => {}
        "worker" | "worker-blocked" => {
            worker_activity = Some(coordination.evidence_worker_activity()?);
        }
        "worker-token-failed-success" => {
            let paths = CoordinationPaths::configured();
            let forced_failure = KernelLock::acquire(
                &paths.worker,
                LockMode::Exclusive,
                Duration::ZERO,
                "forced worker-token failure",
            )?;
            if coordination.evidence_worker_activity().is_ok() {
                return Err(BoundaryError::Unavailable(
                    "worker-token failure fixture unexpectedly acquired activity".to_owned(),
                ));
            }
            coordination.mark_worker_unavailable(launch_uuid);
            drop(forced_failure);
            coordination.evidence_report(&EvidenceReport {
                launch_uuid: launch_uuid.to_owned(),
                outcome: "fixture_late_success",
                result: Ok(()),
                terminal: true,
            });
        }
        "child" => {
            child_activity = Some(coordination.running_child_activity()?);
        }
        _ => {
            return Err(BoundaryError::Unavailable(
                "unknown launch coordination fixture mode".to_owned(),
            ));
        }
    }
    fs::write(&ready, b"ready\n").map_err(|error| io_error("write fixture readiness", error))?;
    if mode == "worker-blocked" {
        let evidence_path = CoordinationPaths::configured()
            .update_dir
            .join(".launch-health-evidence.v1.lock");
        let _blocked = KernelLock::acquire(
            &evidence_path,
            LockMode::Exclusive,
            Duration::from_secs(60),
            "fixture evidence lock",
        )?;
    }
    let _worker_activity = worker_activity;
    let _child_activity = child_activity;
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(debug_assertions)]
fn run_test_admission_fixture() -> Result<(), BoundaryError> {
    if std::env::args_os().count() != 2 {
        return Err(BoundaryError::Usage);
    }
    let paths = CoordinationPaths::configured();
    let _admission = KernelLock::acquire(
        &paths.gate,
        LockMode::Shared,
        Duration::ZERO,
        "lifecycle launch gate",
    )?;
    Ok(())
}

pub fn run_requested_command() -> Option<Result<(), BoundaryError>> {
    match std::env::args_os().nth(1).as_deref() {
        Some(argument) if argument == OsStr::new(DECISION_CHECK_ARGUMENT) => {
            Some(check_decision_boundary())
        }
        #[cfg(debug_assertions)]
        Some(argument) if argument == OsStr::new(TEST_HOST_ARGUMENT) => {
            Some(run_test_host_fixture())
        }
        #[cfg(debug_assertions)]
        Some(argument) if argument == OsStr::new(TEST_ADMISSION_ARGUMENT) => {
            Some(run_test_admission_fixture())
        }
        #[cfg(debug_assertions)]
        Some(argument) if argument == OsStr::new(TEST_HOLD_DECISION_ARGUMENT) => {
            Some(run_test_decision_holder())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock_test_support::{descriptor_holders, RELEASE_VISIBILITY_DEADLINE};
    use std::sync::mpsc;

    struct TestPaths {
        root: PathBuf,
        paths: CoordinationPaths,
    }

    impl TestPaths {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "ignite-launch-gate-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir(&root).unwrap();
            let update_dir = root.join("update");
            fs::create_dir(&update_dir).unwrap();
            Self {
                paths: CoordinationPaths {
                    gate: root.join("gate.lock"),
                    host: root.join("host.lock"),
                    child: root.join("child.lock"),
                    worker: root.join("worker.lock"),
                    control: root.join("control.sock"),
                    update_dir,
                },
                root,
            }
        }
    }

    impl Drop for TestPaths {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Acquire a lock this test has just released. See `lock_test_support` for why release is only
    /// eventually visible under the parallel harness; a lock that stays held still fails.
    fn acquire_after_release(path: &Path, mode: LockMode, label: &'static str) -> KernelLock {
        KernelLock::acquire(path, mode, RELEASE_VISIBILITY_DEADLINE, label).unwrap_or_else(|error| {
            panic!(
                "{label} still unavailable {RELEASE_VISIBILITY_DEADLINE:?} after release ({error}); \
                 descriptor holders of {}: {:?}",
                path.display(),
                descriptor_holders(path)
            )
        })
    }

    fn report(uuid: &str, ok: bool) -> EvidenceReport {
        EvidenceReport {
            launch_uuid: uuid.to_owned(),
            outcome: "terminal-test",
            result: if ok {
                Ok(())
            } else {
                Err(io::Error::other("controlled failure").into())
            },
            terminal: true,
        }
    }

    #[test]
    fn exclusive_decision_gate_refuses_later_launch_admission() {
        let tree = TestPaths::new("admission");
        let host = HostLaunchCoordination::start_at(tree.paths.clone()).unwrap();
        let decision = KernelLock::acquire(
            &tree.paths.gate,
            LockMode::Exclusive,
            Duration::ZERO,
            "test gate",
        )
        .unwrap();
        assert!(matches!(
            host.admit_launch(),
            Err(BoundaryError::Contended(_))
        ));
        drop(decision);
        // Admission itself never waits. Once a shared probe succeeds, every copy of the
        // exclusive decision description is closed, so admission must then succeed at once.
        let _released = acquire_after_release(&tree.paths.gate, LockMode::Shared, "test gate");
        host.admit_launch().unwrap();
    }

    #[test]
    fn queued_worker_prevents_false_quiescence_until_it_finishes() {
        let tree = TestPaths::new("queued-worker");
        let host = HostLaunchCoordination::start_at(tree.paths.clone()).unwrap();
        let admission = host.admit_launch().unwrap();
        let worker = host.evidence_worker_activity().unwrap();
        drop(admission);
        let gate = acquire_after_release(&tree.paths.gate, LockMode::Exclusive, "test gate");
        assert!(matches!(
            KernelLock::acquire(
                &tree.paths.worker,
                LockMode::Exclusive,
                Duration::from_millis(30),
                "test workers"
            ),
            Err(BoundaryError::Contended(_))
        ));
        drop(worker);
        acquire_after_release(&tree.paths.worker, LockMode::Exclusive, "test workers");
        drop(gate);
    }

    #[test]
    fn evidence_lock_blocked_worker_cannot_be_mistaken_for_quiescence() {
        let tree = TestPaths::new("evidence-contention");
        let evidence_path = tree.paths.update_dir.join("evidence.lock");
        let evidence_holder = KernelLock::acquire(
            &evidence_path,
            LockMode::Exclusive,
            Duration::ZERO,
            "test evidence",
        )
        .unwrap();
        let worker = KernelLock::acquire(
            &tree.paths.worker,
            LockMode::Shared,
            Duration::ZERO,
            "test workers",
        )
        .unwrap();
        let evidence_path_for_worker = evidence_path.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let blocked = thread::spawn(move || {
            let _worker = worker;
            started_tx.send(()).unwrap();
            KernelLock::acquire(
                &evidence_path_for_worker,
                LockMode::Exclusive,
                Duration::from_secs(1),
                "test evidence",
            )
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            KernelLock::acquire(
                &tree.paths.worker,
                LockMode::Exclusive,
                Duration::from_millis(30),
                "test workers"
            ),
            Err(BoundaryError::Contended(_))
        ));
        drop(evidence_holder);
        assert!(blocked.join().unwrap().is_ok());
    }

    #[test]
    fn child_activity_survives_host_side_drop_until_actual_child_exit() {
        let tree = TestPaths::new("orphan-child");
        let activity = RunningChildActivity {
            lock: KernelLock::acquire(
                &tree.paths.child,
                LockMode::Shared,
                Duration::ZERO,
                "test child",
            )
            .unwrap(),
        };
        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        activity.configure_command(&mut command);
        let mut child = command.spawn().unwrap();
        drop(activity);
        assert!(matches!(
            KernelLock::acquire(
                &tree.paths.child,
                LockMode::Exclusive,
                Duration::ZERO,
                "test child"
            ),
            Err(BoundaryError::Contended(_))
        ));
        child.kill().unwrap();
        child.wait().unwrap();
        acquire_after_release(&tree.paths.child, LockMode::Exclusive, "test child");
    }

    #[test]
    fn rapid_launches_ignore_late_completion_from_an_older_worker() {
        let tree = TestPaths::new("rapid");
        let host = HostLaunchCoordination::start_at(tree.paths.clone()).unwrap();
        let first = "00112233445566778899aabbccddeeff";
        let second = "ffeeddccbbaa99887766554433221100";
        host.begin_launch(first);
        host.begin_launch(second);
        host.evidence_report(&report(first, true));
        assert!(matches!(
            &*host.latest.lock().unwrap(),
            LatestWorker::InProgress { launch_uuid } if launch_uuid == second
        ));
        host.evidence_report(&report(second, true));
        assert!(matches!(
            &*host.latest.lock().unwrap(),
            LatestWorker::Complete { launch_uuid } if launch_uuid == second
        ));
    }

    #[test]
    fn fresh_or_failed_host_generation_is_never_reported_quiescent() {
        let tree = TestPaths::new("restart");
        let host = HostLaunchCoordination::start_at(tree.paths.clone()).unwrap();
        assert!(host
            .latest
            .lock()
            .unwrap()
            .response()
            .starts_with("BLOCKED"));
        let launch = "00112233445566778899aabbccddeeff";
        host.begin_launch(launch);
        host.evidence_report(&report(launch, false));
        assert!(host
            .latest
            .lock()
            .unwrap()
            .response()
            .starts_with("BLOCKED"));
    }

    #[test]
    fn failed_worker_token_is_sticky_despite_successful_terminal_evidence() {
        let tree = TestPaths::new("sticky-worker-failure");
        let host = HostLaunchCoordination::start_at(tree.paths.clone()).unwrap();
        let _exclusive_worker = KernelLock::acquire(
            &tree.paths.worker,
            LockMode::Exclusive,
            Duration::ZERO,
            "forced worker-token failure",
        )
        .unwrap();
        let launch = "00112233445566778899aabbccddeeff";
        host.begin_launch(launch);
        assert!(matches!(
            host.evidence_worker_activity(),
            Err(BoundaryError::Contended(_))
        ));
        host.mark_worker_unavailable(launch);
        host.evidence_report(&report(launch, true));
        let later = "ffeeddccbbaa99887766554433221100";
        host.begin_launch(later);
        host.evidence_report(&report(later, true));
        assert!(matches!(
            &*host.latest.lock().unwrap(),
            LatestWorker::CoordinationUncertain { launch_uuid } if launch_uuid == launch
        ));
        assert!(host
            .latest
            .lock()
            .unwrap()
            .response()
            .starts_with("BLOCKED"));
    }

    #[test]
    fn a_second_uncoordinated_host_can_hold_a_shared_decision_blocker() {
        let tree = TestPaths::new("degraded-host");
        let _first = HostLaunchCoordination::start_at(tree.paths.clone()).unwrap();
        assert!(matches!(
            HostLaunchCoordination::start_at(tree.paths.clone()),
            Err(BoundaryError::Contended(_))
        ));
        let _blocker = KernelLock::acquire(
            &tree.paths.gate,
            LockMode::Shared,
            Duration::ZERO,
            "degraded test host",
        )
        .unwrap();
        assert!(matches!(
            KernelLock::acquire(
                &tree.paths.gate,
                LockMode::Exclusive,
                Duration::ZERO,
                "test decision"
            ),
            Err(BoundaryError::Contended(_))
        ));
    }
}
