//! Read-only diagnostic for the existing Kindle data-writer coordination boundaries.
//!
//! This command deliberately opens only lock files that already exist. It never creates a lock
//! resource, writes lock metadata, starts the application, runs recovery, or claims an atomic
//! snapshot. Even acquiring every known lock is only partial coverage: recovery has in-process
//! coordination, lifecycle-host startup precedes `go` admission, USB export has no continuous
//! exclusion primitive, and manual/root writers need not cooperate.

use std::path::{Component, Path, PathBuf};

pub const REPORT_SCHEMA: u32 = 1;
pub const DEFAULT_LAUNCH_GATE_LOCK: &str = "/tmp/ignite-lifecycle-launch-gate.lock";
pub const DEFAULT_UPDATER_LOCK: &str = "/tmp/ignite-updater-transaction.lock";
pub const DEFAULT_CHILD_ACTIVITY_LOCK: &str = "/tmp/ignite-lifecycle-child.lock";
pub const DEFAULT_WORKER_ACTIVITY_LOCK: &str = "/tmp/ignite-lifecycle-evidence-worker.lock";
pub const DEFAULT_APPLICATION_LOCK: &str = "/tmp/scribe-app.lock";
const ACTIVE_SYNC_LOCK: &str = "sync.lock";
const REPLICA_LOCK: &str = "replica.lock";
const LIBRARY_SYNC_DIR: &str = ".scribe-sync";
const IDENTITY_LOCK: &str = "identity.lock";
const RECOVERY_MAINTENANCE: &str = "recovery-maintenance";
const RECOVERY_INTENT: &str = "recovery-intent-v1.json";
const INBOUND_APPLY: &str = "apply.json";
const ADOPTION_PLAN: &str = "plan.json";
const DIAGNOSTIC_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterExclusionConfig {
    pub library_root: PathBuf,
    pub sync_root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectionCommand {
    NotRequested,
    Inspect(WriterExclusionConfig),
}

pub fn parse_command(args: &[String]) -> Result<InspectionCommand, String> {
    let requested = args
        .iter()
        .filter(|argument| argument.as_str() == "--inspect-writer-exclusion")
        .count();
    if requested == 0 {
        return Ok(InspectionCommand::NotRequested);
    }
    if requested != 1
        || args.len() != 5
        || args[0] != "--inspect-writer-exclusion"
        || args[1] != "--data-dir"
        || args[2].is_empty()
        || args[3] != "--sync-dir"
        || args[4].is_empty()
    {
        return Err("--inspect-writer-exclusion must be used exactly as: --inspect-writer-exclusion --data-dir LIBRARY --sync-dir SYNC".into());
    }
    Ok(InspectionCommand::Inspect(WriterExclusionConfig {
        library_root: PathBuf::from(&args[2]),
        sync_root: PathBuf::from(&args[4]),
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockState {
    Acquired,
    Contended,
    Missing,
    Unsafe,
    Error,
    Unsupported,
    Cancelled,
    NotAttempted,
}

impl LockState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Acquired => "acquired",
            Self::Contended => "contended",
            Self::Missing => "missing",
            Self::Unsafe => "unsafe",
            Self::Error => "error",
            Self::Unsupported => "unsupported",
            Self::Cancelled => "cancelled",
            Self::NotAttempted => "not_attempted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LockCheck {
    name: &'static str,
    state: LockState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransactionState {
    Clear,
    Active,
    Unknown,
    NotChecked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UsbState {
    Unsupported,
    #[cfg(test)]
    Unknown,
    #[cfg(test)]
    Conflicting,
}

impl UsbState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            #[cfg(test)]
            Self::Unknown => "unknown",
            #[cfg(test)]
            Self::Conflicting => "conflicting",
        }
    }
}

impl TransactionState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Active => "active",
            Self::Unknown => "unknown",
            Self::NotChecked => "not_checked",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterExclusionReport {
    locks: Vec<LockCheck>,
    transaction_state: TransactionState,
    usb_state: UsbState,
    invalid_configuration: bool,
}

impl WriterExclusionReport {
    pub fn writer_exclusion(&self) -> &'static str {
        if self
            .locks
            .iter()
            .all(|check| check.state == LockState::Acquired)
            && self.transaction_state == TransactionState::Clear
            && !self.invalid_configuration
        {
            "partial"
        } else {
            "unverified"
        }
    }

    pub fn result_token(&self) -> &'static str {
        if self
            .locks
            .iter()
            .any(|check| check.state == LockState::Contended)
        {
            "REFUSED"
        } else if self
            .locks
            .iter()
            .any(|check| check.state == LockState::Unsupported)
        {
            "UNSUPPORTED"
        } else if self.writer_exclusion() == "partial" {
            "PARTIAL"
        } else {
            "INCOMPLETE"
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self.result_token() {
            "REFUSED" => 75,
            "PARTIAL" => 2,
            _ => 3,
        }
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let acquired = self
            .locks
            .iter()
            .filter(|check| check.state == LockState::Acquired)
            .count();
        let lock_coverage = if acquired == self.locks.len() {
            "known-lock-set-acquired"
        } else {
            "incomplete"
        };
        let mut out = String::new();
        out.push_str("IGNITE_WRITER_EXCLUSION_INSPECTION_V1\n");
        out.push_str(&format!("schema={REPORT_SCHEMA}\n"));
        out.push_str("scope=kindle-persistent-data-writers\n");
        out.push_str(&format!("result={}\n", self.result_token()));
        out.push_str(&format!("writer_exclusion={}\n", self.writer_exclusion()));
        out.push_str(&format!("lock_coverage={lock_coverage}\n"));
        out.push_str(&format!("locks_acquired={acquired}\n"));
        out.push_str(&format!("locks_required={}\n", self.locks.len()));
        for check in &self.locks {
            out.push_str(&format!("lock_{}={}\n", check.name, check.state.as_str()));
        }
        out.push_str(&format!(
            "transaction_markers={}\n",
            self.transaction_state.as_str()
        ));
        out.push_str("recovery_coordination=cross-process-unavailable\n");
        out.push_str("lifecycle_host_startup_coverage=partial\n");
        out.push_str(&format!("usb_state={}\n", self.usb_state.as_str()));
        out.push_str("usb_exclusion=unavailable\n");
        out.push_str("noncooperating_writer_exclusion=unavailable\n");
        out.push_str("incomplete_evidence=true\n");
        out.push_str("atomic_snapshot=false\n");
        out.push_str("data_compatibility=unknown\n");
        out.push_str("promotion_authorized=false\n");
        out.push_str("inspection_action=report-only\n");
        out.into_bytes()
    }
}

#[derive(Clone, Debug)]
struct CoordinationPaths {
    launch_gate: PathBuf,
    updater: PathBuf,
    child: PathBuf,
    worker: PathBuf,
    application: PathBuf,
    adoption: PathBuf,
    active_sync: PathBuf,
    replica: PathBuf,
    identity: PathBuf,
    markers: [PathBuf; 4],
}

impl CoordinationPaths {
    fn production(config: &WriterExclusionConfig) -> Result<Self, ()> {
        if !absolute_clean(&config.library_root) || !absolute_clean(&config.sync_root) {
            return Err(());
        }
        let sync_name = config.sync_root.file_name().ok_or(())?;
        let mut adoption_name = sync_name.to_os_string();
        adoption_name.push(".adopt.lock");
        let adoption = config.sync_root.with_file_name(adoption_name);
        let mut workspace_name = sync_name.to_os_string();
        workspace_name.push(".adopt");
        let adoption_workspace = config.sync_root.with_file_name(workspace_name);
        Ok(Self {
            launch_gate: PathBuf::from(DEFAULT_LAUNCH_GATE_LOCK),
            updater: PathBuf::from(DEFAULT_UPDATER_LOCK),
            child: PathBuf::from(DEFAULT_CHILD_ACTIVITY_LOCK),
            worker: PathBuf::from(DEFAULT_WORKER_ACTIVITY_LOCK),
            application: PathBuf::from(DEFAULT_APPLICATION_LOCK),
            adoption,
            active_sync: config.sync_root.join(ACTIVE_SYNC_LOCK),
            replica: config.sync_root.join(REPLICA_LOCK),
            identity: config
                .library_root
                .join(LIBRARY_SYNC_DIR)
                .join(IDENTITY_LOCK),
            markers: [
                adoption_workspace.join(ADOPTION_PLAN),
                config.sync_root.join(INBOUND_APPLY),
                config.sync_root.join(RECOVERY_INTENT),
                config
                    .library_root
                    .join(LIBRARY_SYNC_DIR)
                    .join(RECOVERY_MAINTENANCE),
            ],
        })
    }

    fn ordered_locks(&self) -> [(&'static str, &Path); 9] {
        [
            ("launch_gate", &self.launch_gate),
            ("updater_transaction", &self.updater),
            ("running_child", &self.child),
            ("evidence_worker", &self.worker),
            ("application_singleton", &self.application),
            ("adoption", &self.adoption),
            ("active_sync", &self.active_sync),
            ("replica", &self.replica),
            ("identity", &self.identity),
        ]
    }
}

fn absolute_clean(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| !matches!(component, Component::CurDir | Component::ParentDir))
}

fn invalid_report() -> WriterExclusionReport {
    WriterExclusionReport {
        locks: [
            "launch_gate",
            "updater_transaction",
            "running_child",
            "evidence_worker",
            "application_singleton",
            "adoption",
            "active_sync",
            "replica",
            "identity",
        ]
        .into_iter()
        .map(|name| LockCheck {
            name,
            state: LockState::NotAttempted,
        })
        .collect(),
        transaction_state: TransactionState::NotChecked,
        usb_state: UsbState::Unsupported,
        invalid_configuration: true,
    }
}

pub fn inspect_writer_exclusion(config: &WriterExclusionConfig) -> WriterExclusionReport {
    let Ok(paths) = CoordinationPaths::production(config) else {
        return invalid_report();
    };
    inspect_paths(&paths, DIAGNOSTIC_TIMEOUT, || false)
}

#[cfg(not(unix))]
fn inspect_paths(
    paths: &CoordinationPaths,
    _timeout: std::time::Duration,
    _cancelled: impl Fn() -> bool,
) -> WriterExclusionReport {
    WriterExclusionReport {
        locks: paths
            .ordered_locks()
            .into_iter()
            .map(|(name, _)| LockCheck {
                name,
                state: LockState::Unsupported,
            })
            .collect(),
        transaction_state: TransactionState::NotChecked,
        usb_state: UsbState::Unsupported,
        invalid_configuration: false,
    }
}

#[cfg(unix)]
fn inspect_paths(
    paths: &CoordinationPaths,
    timeout: std::time::Duration,
    cancelled: impl Fn() -> bool,
) -> WriterExclusionReport {
    use std::fs::File;
    use std::time::Instant;

    let deadline = Instant::now() + timeout;
    let mut locks = Vec::new();
    let mut held: Vec<File> = Vec::new();
    for (name, path) in paths.ordered_locks() {
        let state = acquire_existing_lock(path, deadline, &cancelled, &mut held);
        locks.push(LockCheck { name, state });
        if matches!(state, LockState::Contended | LockState::Cancelled) {
            for (remaining, _) in paths.ordered_locks().into_iter().skip(locks.len()) {
                locks.push(LockCheck {
                    name: remaining,
                    state: LockState::NotAttempted,
                });
            }
            break;
        }
    }
    let all_acquired = locks.iter().all(|check| check.state == LockState::Acquired);
    let transaction_state = if all_acquired {
        inspect_transaction_markers(&paths.markers)
    } else {
        TransactionState::NotChecked
    };
    drop(held);
    WriterExclusionReport {
        locks,
        transaction_state,
        usb_state: UsbState::Unsupported,
        invalid_configuration: false,
    }
}

#[cfg(unix)]
fn acquire_existing_lock(
    path: &Path,
    deadline: std::time::Instant,
    cancelled: &impl Fn() -> bool,
    held: &mut Vec<std::fs::File>,
) -> LockState {
    use std::fs::{self, TryLockError};
    use std::os::unix::fs::MetadataExt;
    use std::thread;
    use std::time::{Duration, Instant};

    if !absolute_clean(path) || crate::storage_inspect_library::path_chain_contains_symlink(path) {
        return LockState::Unsafe;
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return LockState::Missing,
        Err(_) => return LockState::Error,
    };
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return LockState::Unsafe;
    }
    let file = match crate::storage_inspect_library::open_regular_read_only(path, &metadata) {
        Ok(file) => file,
        Err("symlink_refused" | "hard_link_refused" | "unsupported_file_type") => {
            return LockState::Unsafe;
        }
        Err(_) => return LockState::Error,
    };
    loop {
        if cancelled() {
            return LockState::Cancelled;
        }
        match file.try_lock() {
            Ok(()) => {
                held.push(file);
                return LockState::Acquired;
            }
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(TryLockError::WouldBlock) => return LockState::Contended,
            Err(TryLockError::Error(error)) if unsupported_lock_error(&error) => {
                return LockState::Unsupported;
            }
            Err(TryLockError::Error(_)) => return LockState::Error,
        }
    }
}

#[cfg(unix)]
fn unsupported_lock_error(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == libc::ENOSYS || code == libc::EOPNOTSUPP
    )
}

#[cfg(unix)]
fn inspect_transaction_markers(paths: &[PathBuf; 4]) -> TransactionState {
    use std::fs;

    let mut active = false;
    for path in paths {
        if !absolute_clean(path)
            || crate::storage_inspect_library::path_chain_contains_symlink(path)
        {
            return TransactionState::Unknown;
        }
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => active = true,
            Ok(_) => return TransactionState::Unknown,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return TransactionState::Unknown,
        }
    }
    if active {
        TransactionState::Active
    } else {
        TransactionState::Clear
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_is_exact_and_does_not_overlap_other_diagnostics() {
        assert_eq!(parse_command(&[]).unwrap(), InspectionCommand::NotRequested);
        let valid = vec![
            "--inspect-writer-exclusion".into(),
            "--data-dir".into(),
            "/library".into(),
            "--sync-dir".into(),
            "/sync".into(),
        ];
        assert!(matches!(
            parse_command(&valid).unwrap(),
            InspectionCommand::Inspect(_)
        ));
        for invalid in [
            valid[..4].to_vec(),
            [valid.clone(), valid.clone()].concat(),
            [valid.clone(), vec!["--storage-capabilities".into()]].concat(),
            vec![
                "--inspect-writer-exclusion".into(),
                "--sync-dir".into(),
                "/sync".into(),
                "--data-dir".into(),
                "/library".into(),
            ],
        ] {
            assert!(parse_command(&invalid).is_err());
        }
    }

    #[test]
    fn relative_and_traversal_roots_fail_before_lock_inspection() {
        for config in [
            WriterExclusionConfig {
                library_root: PathBuf::from("relative"),
                sync_root: std::env::temp_dir().join("sync"),
            },
            WriterExclusionConfig {
                library_root: std::env::temp_dir().join("base").join("..").join("library"),
                sync_root: std::env::temp_dir().join("sync"),
            },
        ] {
            let report = inspect_writer_exclusion(&config);
            assert_eq!(report.result_token(), "INCOMPLETE");
            assert_eq!(report.writer_exclusion(), "unverified");
            assert!(report
                .locks
                .iter()
                .all(|lock| lock.state == LockState::NotAttempted));
        }
    }

    #[test]
    fn canonical_report_never_claims_snapshot_compatibility_or_authorization() {
        let report = invalid_report();
        let bytes = String::from_utf8(report.canonical_bytes()).unwrap();
        assert!(bytes.contains("writer_exclusion=unverified\n"));
        assert!(!bytes.contains("writer_exclusion=established"));
        assert!(bytes.contains("usb_exclusion=unavailable\n"));
        assert!(bytes.contains("atomic_snapshot=false\n"));
        assert!(bytes.contains("data_compatibility=unknown\n"));
        assert!(bytes.contains("promotion_authorized=false\n"));
        assert!(bytes.contains("inspection_action=report-only\n"));
        assert_eq!(report.canonical_bytes(), report.canonical_bytes());
    }

    #[test]
    fn unknown_and_conflicting_usb_observations_remain_unverified() {
        for usb_state in [UsbState::Unknown, UsbState::Conflicting] {
            let mut report = invalid_report();
            report.usb_state = usb_state;
            let bytes = String::from_utf8(report.canonical_bytes()).unwrap();
            assert_eq!(report.writer_exclusion(), "unverified");
            assert_eq!(report.result_token(), "INCOMPLETE");
            assert!(bytes.contains(&format!("usb_state={}\n", usb_state.as_str())));
            assert!(bytes.contains("usb_exclusion=unavailable\n"));
            assert!(!bytes.contains("writer_exclusion=established"));
        }
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::fs::{self, File, OpenOptions};
        use std::io::Write;
        use std::os::unix::fs::{symlink, PermissionsExt};
        use std::process::{Child, Command, Stdio};
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{Duration, Instant};

        static NEXT: AtomicU64 = AtomicU64::new(1);

        struct Fixture {
            root: PathBuf,
            paths: CoordinationPaths,
        }

        impl Fixture {
            fn new() -> Self {
                let root = std::env::temp_dir().join(format!(
                    "ignite-writer-exclusion-{}-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed),
                    uuid::Uuid::new_v4()
                ));
                fs::create_dir(&root).unwrap();
                let library = root.join("library");
                let sync = root.join("sync");
                fs::create_dir(&library).unwrap();
                fs::create_dir(&sync).unwrap();
                fs::create_dir(library.join(LIBRARY_SYNC_DIR)).unwrap();
                let mut paths = CoordinationPaths::production(&WriterExclusionConfig {
                    library_root: library,
                    sync_root: sync,
                })
                .unwrap();
                paths.launch_gate = root.join("launch.lock");
                paths.updater = root.join("updater.lock");
                paths.child = root.join("child.lock");
                paths.worker = root.join("worker.lock");
                paths.application = root.join("application.lock");
                for (_, path) in paths.ordered_locks() {
                    File::create(path).unwrap();
                }
                Self { root, paths }
            }

            fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
                self.paths
                    .ordered_locks()
                    .into_iter()
                    .map(|(_, path)| (path.to_path_buf(), fs::read(path).unwrap()))
                    .collect()
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.root);
            }
        }

        fn hold_child(path: &Path, root: &Path) -> (Child, PathBuf, PathBuf) {
            let ready = root.join(format!("ready-{}", uuid::Uuid::new_v4()));
            let release = root.join(format!("release-{}", uuid::Uuid::new_v4()));
            let child = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("storage_writer_exclusion::tests::unix::lock_holder_child")
                .arg("--nocapture")
                .env("IGNITE_WRITER_TEST_LOCK", path)
                .env("IGNITE_WRITER_TEST_READY", &ready)
                .env("IGNITE_WRITER_TEST_RELEASE", &release)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            (child, ready, release)
        }

        fn wait_ready(child: &mut Child, ready: &Path) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if ready.exists() {
                    return;
                }
                if let Some(status) = child.try_wait().unwrap() {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.stderr.take() {
                        use std::io::Read;
                        let _ = pipe.read_to_string(&mut stderr);
                    }
                    panic!("lock holder exited before ready: status={status} stderr={stderr}");
                }
                assert!(Instant::now() < deadline, "lock holder readiness timed out");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn release_child(mut child: Child, release: &Path) {
            fs::write(release, b"release").unwrap();
            let status = child.wait().unwrap();
            assert!(status.success(), "holder status={status}");
        }

        #[test]
        fn lock_holder_child() {
            let Some(path) = std::env::var_os("IGNITE_WRITER_TEST_LOCK") else {
                return;
            };
            let ready = PathBuf::from(std::env::var_os("IGNITE_WRITER_TEST_READY").unwrap());
            let release = PathBuf::from(std::env::var_os("IGNITE_WRITER_TEST_RELEASE").unwrap());
            let file = OpenOptions::new().read(true).open(path).unwrap();
            file.lock().unwrap();
            fs::write(ready, b"ready").unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !release.exists() {
                assert!(Instant::now() < deadline, "release timed out");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        #[test]
        fn all_known_locks_free_is_still_only_partial_and_preserves_bytes() {
            let fixture = Fixture::new();
            for (index, (_, path)) in fixture.paths.ordered_locks().into_iter().enumerate() {
                fs::write(path, format!("stale-owner={index}\n")).unwrap();
            }
            let before = fixture.snapshot();
            let report = inspect_paths(&fixture.paths, Duration::from_millis(100), || false);
            assert_eq!(report.result_token(), "PARTIAL");
            assert_eq!(report.writer_exclusion(), "partial");
            assert!(report
                .locks
                .iter()
                .all(|lock| lock.state == LockState::Acquired));
            assert_eq!(before, fixture.snapshot(), "lock bytes must be untouched");
        }

        #[test]
        fn active_application_sync_adoption_and_updater_each_refuse_cross_process() {
            for label in [
                "application_singleton",
                "active_sync",
                "adoption",
                "updater_transaction",
            ] {
                let fixture = Fixture::new();
                let path = fixture
                    .paths
                    .ordered_locks()
                    .into_iter()
                    .find_map(|(name, path)| (name == label).then_some(path.to_path_buf()))
                    .unwrap();
                let (mut child, ready, release) = hold_child(&path, &fixture.root);
                wait_ready(&mut child, &ready);
                let report = inspect_paths(&fixture.paths, Duration::from_millis(80), || false);
                assert_eq!(report.result_token(), "REFUSED", "label={label}");
                assert_eq!(report.exit_code(), 75, "label={label}");
                assert_eq!(
                    report
                        .locks
                        .iter()
                        .find(|check| check.name == label)
                        .unwrap()
                        .state,
                    LockState::Contended,
                    "label={label}"
                );
                release_child(child, &release);
            }
        }

        #[test]
        fn concurrent_inspector_is_refused_and_holder_death_releases_everything() {
            let fixture = Fixture::new();
            let path = fixture.paths.launch_gate.clone();
            let (mut child, ready, _release) = hold_child(&path, &fixture.root);
            wait_ready(&mut child, &ready);
            let refused = inspect_paths(&fixture.paths, Duration::from_millis(80), || false);
            assert_eq!(refused.result_token(), "REFUSED");
            child.kill().unwrap();
            child.wait().unwrap();
            let released = inspect_paths(&fixture.paths, Duration::from_secs(1), || false);
            assert_eq!(released.result_token(), "PARTIAL");
        }

        #[test]
        fn bounded_timeout_and_cancellation_fail_closed() {
            let fixture = Fixture::new();
            let path = fixture.paths.launch_gate.clone();
            let (mut child, ready, release) = hold_child(&path, &fixture.root);
            wait_ready(&mut child, &ready);
            let started = Instant::now();
            let timed_out = inspect_paths(&fixture.paths, Duration::from_millis(40), || false);
            assert_eq!(timed_out.result_token(), "REFUSED");
            assert!(started.elapsed() < Duration::from_secs(1));
            release_child(child, &release);

            let cancelled = inspect_paths(&fixture.paths, Duration::from_secs(5), || true);
            assert_eq!(cancelled.result_token(), "INCOMPLETE");
            assert_eq!(cancelled.locks[0].state, LockState::Cancelled);
        }

        #[test]
        fn missing_unsafe_read_only_and_transaction_states_are_conservative() {
            let fixture = Fixture::new();
            fs::remove_file(&fixture.paths.replica).unwrap();
            let missing = inspect_paths(&fixture.paths, Duration::from_millis(50), || false);
            assert_eq!(missing.result_token(), "INCOMPLETE");
            assert_eq!(missing.writer_exclusion(), "unverified");

            File::create(&fixture.paths.replica).unwrap();
            fs::remove_file(&fixture.paths.identity).unwrap();
            symlink(&fixture.paths.replica, &fixture.paths.identity).unwrap();
            let unsafe_report = inspect_paths(&fixture.paths, Duration::from_millis(50), || false);
            assert_eq!(unsafe_report.result_token(), "INCOMPLETE");
            assert_eq!(
                unsafe_report
                    .locks
                    .iter()
                    .find(|lock| lock.name == "identity")
                    .unwrap()
                    .state,
                LockState::Unsafe
            );

            fs::remove_file(&fixture.paths.identity).unwrap();
            File::create(&fixture.paths.identity).unwrap();
            for (_, path) in fixture.paths.ordered_locks() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o444)).unwrap();
            }
            let read_only = inspect_paths(&fixture.paths, Duration::from_millis(50), || false);
            assert_eq!(read_only.result_token(), "PARTIAL");

            fs::create_dir_all(fixture.paths.markers[0].parent().unwrap()).unwrap();
            fs::write(&fixture.paths.markers[0], b"pending").unwrap();
            let active = inspect_paths(&fixture.paths, Duration::from_millis(50), || false);
            assert_eq!(active.transaction_state, TransactionState::Active);
            assert_eq!(active.result_token(), "INCOMPLETE");
        }

        #[test]
        fn lock_order_is_explicit_and_stable() {
            let fixture = Fixture::new();
            let names: Vec<_> = fixture
                .paths
                .ordered_locks()
                .into_iter()
                .map(|(name, _)| name)
                .collect();
            assert_eq!(
                names,
                [
                    "launch_gate",
                    "updater_transaction",
                    "running_child",
                    "evidence_worker",
                    "application_singleton",
                    "adoption",
                    "active_sync",
                    "replica",
                    "identity",
                ]
            );
        }

        #[test]
        fn unsupported_kernel_lock_errors_are_classified_without_retry() {
            assert!(unsupported_lock_error(&std::io::Error::from_raw_os_error(
                libc::ENOSYS
            )));
            assert!(!unsupported_lock_error(&std::io::Error::from_raw_os_error(
                libc::EIO
            )));
        }
    }
}
