#![cfg(target_os = "linux")]

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);

struct TestTree {
    root: PathBuf,
    update: PathBuf,
    binary: PathBuf,
}

impl TestTree {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "ignite-launch-gate-integration-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&root).unwrap();
        let update = root.join("update");
        fs::create_dir(&update).unwrap();
        File::create(update.join(".launch-health-evidence.v1.lock")).unwrap();
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

    fn start_host(&self, mode: &str, label: &str) -> Child {
        let ready = self.root.join(format!("{label}.ready"));
        let mut child = self
            .command()
            .args(["--test-launch-coordination-host", mode])
            .arg(&ready)
            .spawn()
            .unwrap();
        wait_for_file(&ready, &mut child);
        child
    }

    fn decision_check(&self) -> ExitStatus {
        self.command()
            .arg("--check-promotion-decision-boundary")
            .status()
            .unwrap()
    }

    fn start_decision_holder(&self, label: &str, milliseconds: u64) -> Child {
        let ready = self.root.join(format!("{label}.ready"));
        let mut child = self
            .command()
            .arg("--test-hold-promotion-decision-boundary")
            .arg(&ready)
            .arg(milliseconds.to_string())
            .spawn()
            .unwrap();
        wait_for_file(&ready, &mut child);
        child
    }

    fn admission(&self) -> ExitStatus {
        self.command()
            .arg("--test-launch-admission")
            .status()
            .unwrap()
    }
}

impl Drop for TestTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn wait_for_file(path: &Path, child: &mut Child) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    while !path.exists() {
        assert!(child.try_wait().unwrap().is_none(), "fixture exited early");
        assert!(Instant::now() < deadline, "fixture readiness timed out");
        thread::sleep(Duration::from_millis(10));
    }
}

fn kill(child: &mut Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

fn wait_for_exit(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "child exit timed out");
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
    let mut updater = tree
        .command()
        .args([
            "--with-updater-transaction-lock",
            "--",
            "/bin/sh",
            "-c",
            "printf 'ready\\n' > \"$1\"; exec sleep 60",
            "sh",
        ])
        .arg(&marker)
        .spawn()
        .unwrap();
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
    let mut host = tree
        .command()
        .arg("--test-production-go-dispatch")
        .args([
            &host_ready,
            &trigger,
            &child_marker,
            &ack_marker,
            &result,
            &log,
        ])
        .spawn()
        .unwrap();
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
