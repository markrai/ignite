#![cfg(target_os = "linux")]

use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const UNIX_SOCKET_PATH_MAX: usize = 107;

struct FixtureProcess {
    name: String,
    child: Child,
    stdout_path: PathBuf,
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
    app: PathBuf,
    lifecycle: PathBuf,
    previous: PathBuf,
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
        let app = root.join("candidate-app");
        let lifecycle = root.join("candidate-lifecycle");
        let previous = root.join("previous-release");
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
            app,
            lifecycle,
            previous,
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
            .env("IGNITE_TEST_ASSESS_APP_PATH", &self.app)
            .env("IGNITE_TEST_ASSESS_LIFECYCLE_PATH", &self.lifecycle)
            .env("IGNITE_TEST_ASSESS_PREVIOUS_DIR", &self.previous)
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

    fn boundary_assessment(&self) -> Output {
        self.command()
            .arg("--assess-promotion-with-boundary")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    fn start_boundary_assessment(&self, label: &str) -> (FixtureProcess, PathBuf) {
        let ready = self.root.join(format!("{label}.ready"));
        let continue_path = self.root.join(format!("{label}.continue"));
        let mut command = self.command();
        command
            .arg("--assess-promotion-with-boundary")
            .env("IGNITE_TEST_BOUNDARY_ASSESSMENT_READY", &ready)
            .env("IGNITE_TEST_BOUNDARY_ASSESSMENT_CONTINUE", &continue_path);
        let mut fixture = self.spawn_fixture(&format!("assessment-{label}"), &mut command);
        wait_for_file(&ready, &mut fixture);
        (fixture, continue_path)
    }

    fn updater_attempt(&self) -> ExitStatus {
        self.command()
            .args(["--with-updater-transaction-lock", "--", "/bin/true"])
            .status()
            .unwrap()
    }

    fn write_healthy_assessment_state(&self) {
        const LAUNCH: &str = "00112233445566778899aabbccddeeff";
        const ENTRIES: [&str; 18] = [
            "scribe-app",
            "scribe-lifecycle",
            "scribe-lifecycle-host.sh",
            "Scribe.sh",
            "scribe-icon.png",
            "OFL.txt",
            "DESCRIPTION.en_us.html",
            "upstream_info.md",
            "source-sans-3-OFL.txt",
            "install-runtime.sh",
            "uninstall-runtime.sh",
            "backup-runtime.sh",
            "register-helpers.sh",
            "install-recovery.sh",
            "Uninstall Ignite.sh",
            "Backup Ignite Data.sh",
            "Diagnose Ignite.sh",
            "appreg-handler.sql",
        ];
        fs::write(&self.app, b"candidate app B").unwrap();
        fs::write(&self.lifecycle, b"candidate lifecycle B").unwrap();
        fs::create_dir(&self.previous).unwrap();
        fs::write(self.previous.join("scribe-app"), b"retained app A").unwrap();
        fs::write(
            self.previous.join("scribe-lifecycle"),
            b"retained lifecycle A",
        )
        .unwrap();
        fs::write(
            self.previous.join("scribe-lifecycle-host.sh"),
            b"#!/bin/sh\n",
        )
        .unwrap();

        let mut inventory =
            "IGNITE_LKG_INVENTORY_V1\nschema=1\nkind=ignite-application-release\nentry_count=18\n"
                .to_owned();
        for name in ENTRIES {
            match fs::read(self.previous.join(name)) {
                Ok(bytes) => inventory.push_str(&format!(
                    "entry={name}|file|{}|{}\n",
                    bytes.len(),
                    sha256(&bytes)
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    inventory.push_str(&format!("entry={name}|absent|0|-\n"));
                }
                Err(error) => panic!("retained fixture read failed: {error}"),
            }
        }
        let inventory = inventory.into_bytes();
        fs::write(self.previous.join("ignite-lkg-inventory.v1"), &inventory).unwrap();

        let app = fs::read(&self.app).unwrap();
        let lifecycle = fs::read(&self.lifecycle).unwrap();
        let inventory_sha256 = sha256(&inventory);
        let pending = format!(
            "IGNITE_PENDING_HEALTH_V1\n\
             schema=1\n\
             status=pending-health\n\
             candidate_version=7.8.1\n\
             candidate_txn=20261024T120000Z-123\n\
             candidate_app_bytes={}\n\
             candidate_app_sha256={}\n\
             candidate_lifecycle_bytes={}\n\
             candidate_lifecycle_sha256={}\n\
             retained_version=unknown\n\
             retained_release_identity=sha256:{}\n\
             lkg_inventory_bytes={}\n\
             lkg_inventory_sha256={}\n\
             data_compatibility=unknown\n",
            app.len(),
            sha256(&app),
            lifecycle.len(),
            sha256(&lifecycle),
            inventory_sha256,
            inventory.len(),
            inventory_sha256,
        );
        fs::write(self.update.join("pending-health.v1"), &pending).unwrap();
        let pending_sha256 = sha256(pending.as_bytes());
        fs::write(
            self.update.join("launch-health-owner.v1"),
            format!(
                "IGNITE_LAUNCH_HEALTH_OWNER_V1\n\
                 schema=1\n\
                 boot_id=ffeeddccbbaa99887766554433221100\n\
                 monotonic_ns=1\n\
                 process_id=1\n\
                 sequence=1\n\
                 launch_uuid={LAUNCH}\n"
            ),
        )
        .unwrap();
        fs::write(
            self.update.join("launch-health-evidence.v1"),
            format!(
                "IGNITE_LAUNCH_HEALTH_EVIDENCE_V1\n\
                 schema=1\n\
                 candidate_version=7.8.1\n\
                 candidate_txn=20261024T120000Z-123\n\
                 pending_sha256={pending_sha256}\n\
                 candidate_app_bytes={}\n\
                 candidate_app_sha256={}\n\
                 candidate_lifecycle_bytes={}\n\
                 candidate_lifecycle_sha256={}\n\
                 launch_uuid={LAUNCH}\n\
                 outcome=intentional_stop\n\
                 ready=true\n\
                 readiness=ready\n\
                 process=stopped\n\
                 observation_complete=true\n\
                 ready_elapsed_ms=1000\n\
                 post_ready_observed_ms=61000\n\
                 qualification=unqualified\n\
                 data_compatibility=unknown\n",
                app.len(),
                sha256(&app),
                lifecycle.len(),
                sha256(&lifecycle),
            ),
        )
        .unwrap();
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
        let stdout_path = self.root.join(format!("{safe_name}.stdout"));
        let stderr = File::create(&stderr_path).unwrap();
        let stdout = File::create(&stdout_path).unwrap();
        command
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        FixtureProcess {
            name: name.to_owned(),
            child: command.spawn().unwrap(),
            stdout_path,
            stderr_path,
        }
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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

fn fixture_stdout(fixture: &FixtureProcess) -> String {
    fs::read_to_string(&fixture.stdout_path)
        .unwrap_or_else(|error| format!("<unable to read stdout: {error}>"))
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
                "fixture {:?} exited before publishing {}: status={status}; stdout={:?}; stderr={:?}",
                fixture.name,
                path.display(),
                fixture_stdout(fixture),
                fixture_stderr(fixture)
            ),
            Ok(None) => {}
            Err(error) => {
                let _ = terminate_bounded(fixture);
                panic!(
                    "fixture {:?} status check failed before publishing {}: {error}; stdout={:?}; stderr={:?}",
                    fixture.name,
                    path.display(),
                    fixture_stdout(fixture),
                    fixture_stderr(fixture)
                );
            }
        }
        if Instant::now() >= deadline {
            let cleanup_status = terminate_bounded(fixture);
            panic!(
                "fixture {:?} timed out publishing {}; cleanup_status={cleanup_status:?}; stdout={:?}; stderr={:?}",
                fixture.name,
                path.display(),
                fixture_stdout(fixture),
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
        "fixture {:?} did not terminate within cleanup deadline; stdout={:?}; stderr={:?}",
        fixture.name,
        fixture_stdout(fixture),
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
                    "fixture {:?} status check failed: {error}; stdout={:?}; stderr={:?}",
                    fixture.name,
                    fixture_stdout(fixture),
                    fixture_stderr(fixture)
                );
            }
        }
        if Instant::now() >= deadline {
            let cleanup_status = terminate_bounded(fixture);
            panic!(
                "fixture {:?} exit timed out; cleanup_status={cleanup_status:?}; stdout={:?}; stderr={:?}",
                fixture.name,
                fixture_stdout(fixture),
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
fn boundary_assessment_is_in_process_read_only_and_non_authorizing() {
    let tree = TestTree::new("boundary-assessment");
    tree.write_healthy_assessment_state();
    let mut host = tree.start_host("ready", "host");
    let output = tree.boundary_assessment();
    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("IGNITE_PROMOTION_ASSESSMENT_V1\n"));
    assert!(stdout.contains("mode=boundary-read-only\n"));
    assert!(stdout.contains("check.decision_boundary=PASS"));
    assert!(stdout.contains("check.assessment_lock=PASS"));
    assert!(stdout.contains("check.pending_and_live_candidate=PASS"));
    assert!(stdout.contains("check.retained_release=PASS"));
    assert!(stdout.contains("check.lifecycle_generation=PASS"));
    assert!(stdout.contains("check.intentional_stop=PASS"));
    assert!(stdout.contains("check.data_compatibility=UNKNOWN"));
    assert!(stdout.contains("overall=REVIEW_REQUIRED\n"));
    assert!(stdout.contains("promotion_authorized=false\n"));
    kill(&mut host);
}

#[test]
fn pending_identity_changed_before_exclusive_snapshot_is_revalidated() {
    let tree = TestTree::new("boundary-pending-change");
    tree.write_healthy_assessment_state();
    let mut host = tree.start_host("ready", "host");
    let (mut assessment, continue_path) = tree.start_boundary_assessment("assessment");
    let pending_path = tree.update.join("pending-health.v1");
    let pending = fs::read_to_string(&pending_path).unwrap().replace(
        "candidate_txn=20261024T120000Z-123",
        "candidate_txn=20261024T120000Z-124",
    );
    fs::write(pending_path, pending).unwrap();

    fs::write(continue_path, b"continue\n").unwrap();
    assert_eq!(wait_for_exit(&mut assessment).code(), Some(1));
    let stdout = fixture_stdout(&assessment);
    assert!(stdout.contains("check.decision_boundary=PASS"));
    assert!(stdout.contains("check.evidence_identity=FAIL"));
    assert!(stdout.contains("overall=BLOCKED\n"));
    kill(&mut host);
}

#[test]
fn queued_and_evidence_blocked_workers_refuse_bounded_quiescence() {
    let tree = TestTree::new("worker-contention");
    let mut queued = tree.start_host("worker", "queued");
    assert_eq!(tree.decision_check().code(), Some(75));
    assert_eq!(tree.boundary_assessment().status.code(), Some(75));
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
    assert_eq!(tree.boundary_assessment().status.code(), Some(75));
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
    assert_eq!(tree.boundary_assessment().status.code(), Some(75));
    kill(&mut child_host);

    assert_eq!(tree.decision_check().code(), Some(1));
    let mut restarted = tree.start_host("incomplete", "restart");
    assert_eq!(tree.decision_check().code(), Some(1));
    kill(&mut restarted);
}

#[test]
fn host_disappearance_during_assessment_blocks_and_releases_every_lock() {
    let tree = TestTree::new("assessment-host-death");
    let mut host = tree.start_host("ready", "host");
    let (mut assessment, continue_path) = tree.start_boundary_assessment("assessment");
    assert_eq!(tree.admission().code(), Some(75));
    assert_eq!(tree.updater_attempt().code(), Some(75));

    kill(&mut host);
    fs::write(continue_path, b"continue\n").unwrap();
    assert_eq!(wait_for_exit(&mut assessment).code(), Some(1));
    let stdout = fixture_stdout(&assessment);
    assert!(stdout.contains("check.decision_boundary=FAIL class=coordination_failure"));
    assert!(stdout.contains("overall=BLOCKED\n"));
    assert!(
        tree.admission().success(),
        "failed assessment retained launch gate"
    );
    assert!(
        tree.updater_attempt().success(),
        "failed assessment retained updater lock"
    );
    let evidence = OpenOptions::new()
        .read(true)
        .write(true)
        .open(tree.update.join(".launch-health-evidence.v1.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(evidence.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "failed assessment retained evidence lock"
    );
}

#[test]
fn host_restart_with_same_uuid_during_assessment_is_detected() {
    let tree = TestTree::new("assessment-host-restart");
    let mut first_host = tree.start_host("ready", "first");
    let (mut assessment, continue_path) = tree.start_boundary_assessment("assessment");
    kill(&mut first_host);
    let mut restarted_host = tree.start_host("ready", "restarted");

    fs::write(continue_path, b"continue\n").unwrap();
    assert_eq!(wait_for_exit(&mut assessment).code(), Some(1));
    let stdout = fixture_stdout(&assessment);
    assert!(stdout.contains("check.decision_boundary=FAIL class=coordination_failure"));
    assert!(stdout.contains("generation changed"));
    kill(&mut restarted_host);
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
    assert_eq!(tree.boundary_assessment().status.code(), Some(75));
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
    let (mut assessment, continue_path) = tree.start_boundary_assessment("assessment");

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

    fs::write(continue_path, b"continue\n").unwrap();
    assert_eq!(wait_for_exit(&mut assessment).code(), Some(1));
}
