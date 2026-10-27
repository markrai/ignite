#![cfg(target_os = "linux")]

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const UNIX_SOCKET_PATH_MAX: usize = 107;

struct FixtureProcess {
    name: String,
    child: Child,
    stderr_path: PathBuf,
}

impl Drop for FixtureProcess {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
    }
}

struct TestTree {
    root: PathBuf,
    update: PathBuf,
    binary: PathBuf,
}

impl TestTree {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "ignite-lg-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&root).unwrap();
        let update = root.join("update");
        fs::create_dir(&update).unwrap();
        File::create(update.join(".launch-health-evidence.v1.lock")).unwrap();
        let control = root.join("control.sock");
        assert!(
            control.as_os_str().as_bytes().len() <= UNIX_SOCKET_PATH_MAX,
            "test control socket path exceeds Linux sockaddr_un capacity: {} bytes: {}",
            control.as_os_str().as_bytes().len(),
            control.display()
        );
        Self {
            root,
            update,
            binary: PathBuf::from(env!("CARGO_BIN_EXE_scribe-lifecycle")),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .env("IGNITE_TEST_LAUNCH_GATE_PATH", self.root.join("gate.lock"))
            .env("IGNITE_TEST_LAUNCH_HOST_PATH", self.root.join("host.lock"))
            .env(
                "IGNITE_TEST_LAUNCH_CHILD_PATH",
                self.root.join("child.lock"),
            )
            .env(
                "IGNITE_TEST_EVIDENCE_WORKER_PATH",
                self.root.join("worker.lock"),
            )
            .env(
                "IGNITE_TEST_LAUNCH_CONTROL_PATH",
                self.root.join("control.sock"),
            )
            .env("IGNITE_TEST_UPDATE_DIR", &self.update)
            .env(
                "IGNITE_TEST_UPDATER_LOCK_PATH",
                self.root.join("updater.lock"),
            )
            .env("IGNITE_TEST_DECISION_TIMEOUT_MS", "200")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    }

    fn start_host(&self, mode: &str, label: &str) -> FixtureProcess {
        let ready = self.root.join(format!("{label}.ready"));
        let mut command = self.command();
        command
            .args(["--test-launch-coordination-host", mode])
            .arg(&ready);
        let mut fixture = self.spawn_fixture(&format!("host-{label}-{mode}"), &mut command);
        wait_for_file(&ready, &mut fixture);
        fixture
    }

    fn decision_check(&self) -> ExitStatus {
        self.command()
            .arg("--check-promotion-decision-boundary")
            .status()
            .unwrap()
    }

    fn start_decision_holder(&self, label: &str, milliseconds: u64) -> FixtureProcess {
        let ready = self.root.join(format!("{label}.ready"));
        let mut command = self.command();
        command
            .arg("--test-hold-promotion-decision-boundary")
            .arg(&ready)
            .arg(milliseconds.to_string());
        let mut fixture = self.spawn_fixture(&format!("decision-{label}"), &mut command);
        wait_for_file(&ready, &mut fixture);
        fixture
    }

    fn admission(&self) -> ExitStatus {
        self.command()
            .arg("--test-launch-admission")
            .status()
            .unwrap()
    }

    fn spawn_fixture(&self, name: &str, command: &mut Command) -> FixtureProcess {
        let safe_name: String = name
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '-' {
                    character
                } else {
                    '_'
                }
            })
            .collect();
        let stderr_path = self.root.join(format!("{safe_name}.stderr"));
        let stderr = File::create(&stderr_path).unwrap();
        command.stderr(Stdio::from(stderr));
        FixtureProcess {
            name: name.to_owned(),
            child: command.spawn().unwrap(),
            stderr_path,
        }
    }
}

impl Drop for TestTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fixture_stderr(fixture: &FixtureProcess) -> String {
    fs::read_to_string(&fixture.stderr_path)
        .unwrap_or_else(|error| format!("<unable to read stderr: {error}>"))
}

fn terminate_bounded(fixture: &mut FixtureProcess) -> Option<ExitStatus> {
    if let Ok(Some(status)) = fixture.child.try_wait() {
        return Some(status);
    }
    let _ = fixture.child.kill();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        match fixture.child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => return None,
        }
    }
    None
}

fn wait_for_file(path: &Path, fixture: &mut FixtureProcess) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    while !path.exists() {
        match fixture.child.try_wait() {
            Ok(Some(status)) => panic!(
                "fixture {:?} exited before publishing {}: status={status}; stderr={:?}",
                fixture.name,
                path.display(),
                fixture_stderr(fixture)
            ),
            Ok(None) => {}
            Err(error) => {
                let _ = terminate_bounded(fixture);
                panic!(
                    "fixture {:?} status check failed before publishing {}: {error}; stderr={:?}",
                    fixture.name,
                    path.display(),
                    fixture_stderr(fixture)
                );
            }
        }
        if Instant::now() >= deadline {
            let cleanup_status = terminate_bounded(fixture);
            panic!(
                "fixture {:?} timed out publishing {}; cleanup_status={cleanup_status:?}; stderr={:?}",
                fixture.name,
                path.display(),
                fixture_stderr(fixture)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn kill(fixture: &mut FixtureProcess) {
    let status = terminate_bounded(fixture);
    assert!(
        status.is_some(),
        "fixture {:?} did not terminate within cleanup deadline; stderr={:?}",
        fixture.name,
        fixture_stderr(fixture)
    );
}

fn wait_for_exit(fixture: &mut FixtureProcess) -> ExitStatus {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        match fixture.child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) => {}
            Err(error) => {
                let _ = terminate_bounded(fixture);
                panic!(
                    "fixture {:?} status check failed: {error}; stderr={:?}",
                    fixture.name,
                    fixture_stderr(fixture)
                );
            }
        }
        if Instant::now() >= deadline {
            let cleanup_status = terminate_bounded(fixture);
            panic!(
                "fixture {:?} exit timed out; cleanup_status={cleanup_status:?}; stderr={:?}",
                fixture.name,
                fixture_stderr(fixture)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn held_decision_refuses_later_launch_and_crash_releases_every_lock() {
    let tree = TestTree::new("decision-crash");
    let mut host = tree.start_host("ready", "host");
    let mut decision = tree.start_decision_holder("decision", 60_000);

    assert_eq!(tree.admission().code(), Some(75));
    kill(&mut decision);
    assert!(tree.admission().success());
    assert!(tree.decision_check().success());
    kill(&mut host);
}

#[test]
fn queued_and_evidence_blocked_workers_refuse_bounded_quiescence() {
    let tree = TestTree::new("worker-contention");
    let mut queued = tree.start_host("worker", "queued");
    assert_eq!(tree.decision_check().code(), Some(75));
    kill(&mut queued);

    let evidence = OpenOptions::new()
        .read(true)
        .write(true)
        .open(tree.update.join(".launch-health-evidence.v1.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(evidence.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let mut blocked = tree.start_host("worker-blocked", "blocked");
    assert_eq!(tree.decision_check().code(), Some(75));
    assert_eq!(
        unsafe { libc::flock(evidence.as_raw_fd(), libc::LOCK_UN) },
        0
    );
    kill(&mut blocked);
}

#[test]
fn child_missing_host_and_restarted_incomplete_host_all_refuse() {
    let tree = TestTree::new("host-restart");
    let mut child_host = tree.start_host("child", "child");
    assert_eq!(tree.decision_check().code(), Some(75));
    kill(&mut child_host);

    assert_eq!(tree.decision_check().code(), Some(1));
    let mut restarted = tree.start_host("incomplete", "restart");
    assert_eq!(tree.decision_check().code(), Some(1));
    kill(&mut restarted);
}

#[test]
fn updater_contention_obeys_lock_order_and_releases_launch_gate() {
    let tree = TestTree::new("updater-order");
    let mut host = tree.start_host("ready", "host");
    let marker = tree.root.join("updater.ready");
    let mut updater_command = tree.command();
    updater_command
        .args([
            "--with-updater-transaction-lock",
            "--",
            "/bin/sh",
            "-c",
            "printf 'ready\\n' > \"$1\"; exec sleep 60",
            "sh",
        ])
        .arg(&marker);
    let mut updater = tree.spawn_fixture("updater-holder", &mut updater_command);
    wait_for_file(&marker, &mut updater);

    assert_eq!(tree.decision_check().code(), Some(75));
    assert!(
        tree.admission().success(),
        "failed decision must release gate"
    );
    kill(&mut updater);
    assert!(tree.decision_check().success());
    kill(&mut host);
}

#[test]
fn internal_decision_completion_releases_locks_and_arbitrary_exec_is_rejected() {
    let tree = TestTree::new("normal-release");
    let mut host = tree.start_host("ready", "host");
    let mut decision = tree.start_decision_holder("decision", 20);
    assert!(wait_for_exit(&mut decision).success());
    assert!(tree.admission().success());

    // The former arbitrary executor is intentionally not a recognized interface. A program that
    // closes inherited descriptors can no longer run inside a purported decision boundary.
    let escaped = tree.root.join("arbitrary-program-ran");
    let status = tree
        .command()
        .args([
            "--with-promotion-decision-boundary",
            "--",
            "/bin/sh",
            "-c",
            "exec 3>&- 4>&- 5>&- 6>&- 7>&-; printf 'unsafe\\n' > \"$1\"",
            "sh",
        ])
        .arg(&escaped)
        .status()
        .unwrap();
    assert!(!status.success());
    assert!(!escaped.exists());
    assert!(tree.admission().success());
    kill(&mut host);
}

#[test]
fn failed_worker_token_remains_blocked_after_late_success_report() {
    let tree = TestTree::new("sticky-worker-failure");
    let mut host = tree.start_host("worker-token-failed-success", "host");
    assert_eq!(tree.decision_check().code(), Some(1));
    assert!(
        tree.admission().success(),
        "failed decision must release gate"
    );
    kill(&mut host);
}

#[test]
fn production_go_dispatch_refuses_spawn_and_ack_while_decision_is_held() {
    let tree = TestTree::new("production-go");
    let host_ready = tree.root.join("host.ready");
    let trigger = tree.root.join("go.trigger");
    let child_marker = tree.root.join("child.spawned");
    let ack_marker = tree.root.join("go.acknowledged");
    let result = tree.root.join("go.result");
    let log = tree.root.join("host.log");
    let mut host_command = tree.command();
    host_command.arg("--test-production-go-dispatch").args([
        &host_ready,
        &trigger,
        &child_marker,
        &ack_marker,
        &result,
        &log,
    ]);
    let mut host = tree.spawn_fixture("production-go-host", &mut host_command);
    wait_for_file(&host_ready, &mut host);
    let mut decision = tree.start_decision_holder("decision", 60_000);

    fs::write(&trigger, b"go\n").unwrap();
    wait_for_file(&result, &mut host);
    assert!(wait_for_exit(&mut host).success());
    assert_eq!(fs::read_to_string(&result).unwrap(), "1\n");
    assert!(
        !child_marker.exists(),
        "production spawn path ran under gate"
    );
    assert!(
        !ack_marker.exists(),
        "production callback acknowledged refused go"
    );

    kill(&mut decision);
}
