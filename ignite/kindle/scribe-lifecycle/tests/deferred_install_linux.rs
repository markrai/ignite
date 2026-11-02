#![cfg(target_os = "linux")]

//! Isolated-host fixtures for SFX-compatible deferred install on Linux.
//! Exercises `$0` handling, inherited updater locks, and the deferred-install CLI path.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn temp_root(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ignite-deferred-it-{label}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn write_sfx_like_sealed(path: &Path, marker: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let body = format!(
        "#!/bin/sh\n\
set -eu\n\
IGNITE_STORAGE_CONTRACT='ignite-storage-v1'\n\
# SFX-compatible $0 resolution (same pattern as Install-Ignite.sfx.sh).\n\
_self=$0\n\
case \"$_self\" in\n\
  /*) ;;\n\
  *)\n\
    _self=\"$(CDPATH= cd -- \"$(dirname -- \"$_self\")\" && pwd)/$(basename -- \"$_self\")\"\n\
    ;;\nesac\n\
[ -n \"${{IGNITE_UPDATER_LOCK_HELD:-}}\" ] || {{ echo 'missing updater lock held' >&2; exit 41; }}\n\
[ -n \"${{IGNITE_UPDATER_LOCK_FD:-}}\" ] || {{ echo 'missing updater lock fd' >&2; exit 42; }}\n\
[ -e \"/proc/self/fd/${{IGNITE_UPDATER_LOCK_FD}}\" ] || {{ echo 'lock fd closed' >&2; exit 43; }}\n\
printf 'self=%s\\nheld=%s\\nfd=%s\\n' \"$_self\" \"$IGNITE_UPDATER_LOCK_HELD\" \"$IGNITE_UPDATER_LOCK_FD\" > \"{}\"\n\
exit 0\n",
        marker.display()
    );
    fs::write(path, body).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o700);
    fs::set_permissions(path, perms).unwrap();
}

#[test]
fn sfx_like_sealed_exec_inherits_updater_lock_and_preserves_dollar_zero() {
    let root = temp_root("sfx-exec");
    let sealed = root.join("ignite-authorized-install-txn1.sh");
    let marker = root.join("sfx.ok");
    let lock_path = root.join("updater.lock");
    write_sfx_like_sealed(&sealed, &marker);

    let lifecycle = PathBuf::from(env!("CARGO_BIN_EXE_scribe-lifecycle"));
    let status = Command::new(&lifecycle)
        .args([
            "--with-updater-transaction-lock",
            "--",
            "sh",
            sealed.to_str().unwrap(),
        ])
        .env("IGNITE_TEST_UPDATER_LOCK_PATH", &lock_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .status()
        .expect("spawn deferred exec");
    assert!(status.success(), "sfx-like sealed exec failed: {status}");
    let report = fs::read_to_string(&marker).expect("sfx marker");
    assert!(
        report.contains(&format!("self={}", sealed.display())),
        "expected absolute $0 path in report: {report}"
    );
    assert!(report.contains("held=1\n"), "missing held=1: {report}");
    assert!(report.contains("fd="), "missing fd: {report}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn deferred_install_cli_runs_sfx_under_inherited_lock() {
    let root = temp_root("deferred-ok");
    let update = root.join("update");
    let run = root.join("run");
    let staging = root.join("staging");
    let lock_path = root.join("updater.lock");
    let marker = root.join("install.ok");
    fs::create_dir_all(&update).unwrap();
    fs::create_dir_all(&run).unwrap();
    fs::create_dir_all(&staging).unwrap();

    let intent = format!(
        "IGNITE_INSTALL_INTENT_V1\n\
schema=1\n\
expected_tag=v0.9.15\n\
version=0.9.15\n\
staging_root={}\n\
storage_contract=ignite-storage-v1\n\
intent_txn=txn-ok\n\
trust_anchor=false\n\
note=reauthenticate-before-exec\n",
        staging.display()
    );
    fs::write(update.join("install-intent.v1"), intent).unwrap();

    let template = root.join("sfx-template.sh");
    write_sfx_like_sealed(&template, &marker);

    let mock_app = root.join("mock-scribe-app");
    let mock_body = format!(
        "#!/bin/sh\n\
set -eu\n\
sealed_out=\n\
receipt_out=\n\
while [ \"$#\" -gt 0 ]; do\n\
  case \"$1\" in\n\
    --sealed-out) sealed_out=$2; shift 2 ;;\n\
    --receipt-out) receipt_out=$2; shift 2 ;;\n\
    *) shift ;;\n\
  esac\n\
done\n\
[ -n \"$sealed_out\" ] && [ -n \"$receipt_out\" ]\n\
cp '{}' \"$sealed_out\"\n\
chmod 700 \"$sealed_out\"\n\
printf '%s\\n' \
'IGNITE_AUTHORIZED_INSTALL_SEAL_V1' \
'schema=1' \
'authorization_revalidated=true' \
'storage_contract=ignite-storage-v1' \
> \"$receipt_out\"\n\
exit 0\n",
        template.display()
    );
    fs::write(&mock_app, mock_body).unwrap();
    let mut perms = fs::metadata(&mock_app).unwrap().permissions();
    perms.set_mode(0o700);
    fs::set_permissions(&mock_app, perms).unwrap();

    let lifecycle = PathBuf::from(env!("CARGO_BIN_EXE_scribe-lifecycle"));
    let output = Command::new(&lifecycle)
        .args(["--test-run-deferred-install", update.to_str().unwrap()])
        .env("IGNITE_TEST_SCRIBE_APP_PATH", &mock_app)
        .env("IGNITE_TEST_APP_RUN_DIR", &run)
        .env("IGNITE_TEST_UPDATER_LOCK_PATH", &lock_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn deferred install");
    assert!(
        output.status.success(),
        "run_deferred_install failed: status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let sealed = run.join("ignite-authorized-install-txn-ok.sh");
    let report = fs::read_to_string(&marker).expect("install marker");
    assert!(report.contains("held=1\n"), "{report}");
    assert!(
        report.contains(&format!("self={}", sealed.display())),
        "{report}"
    );
    assert!(update.join("install-intent.v1").is_file());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn deferred_install_cli_seal_failure_preserves_intent() {
    let root = temp_root("cli-seal-fail");
    let update = root.join("update");
    let run = root.join("run");
    let staging = root.join("staging");
    let lock_path = root.join("updater.lock");
    fs::create_dir_all(&update).unwrap();
    fs::create_dir_all(&run).unwrap();
    fs::create_dir_all(&staging).unwrap();

    let intent = format!(
        "IGNITE_INSTALL_INTENT_V1\n\
schema=1\n\
expected_tag=v0.9.15\n\
version=0.9.15\n\
staging_root={}\n\
storage_contract=ignite-storage-v1\n\
intent_txn=txn-fail\n\
trust_anchor=false\n\
note=reauthenticate-before-exec\n",
        staging.display()
    );
    fs::write(update.join("install-intent.v1"), intent).unwrap();

    let mock_app = root.join("mock-scribe-app");
    fs::write(&mock_app, "#!/bin/sh\nexit 97\n").unwrap();
    let mut perms = fs::metadata(&mock_app).unwrap().permissions();
    perms.set_mode(0o700);
    fs::set_permissions(&mock_app, perms).unwrap();

    let lifecycle = PathBuf::from(env!("CARGO_BIN_EXE_scribe-lifecycle"));
    let status = Command::new(&lifecycle)
        .args(["--test-run-deferred-install", update.to_str().unwrap()])
        .env("IGNITE_TEST_SCRIBE_APP_PATH", &mock_app)
        .env("IGNITE_TEST_APP_RUN_DIR", &run)
        .env("IGNITE_TEST_UPDATER_LOCK_PATH", &lock_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("spawn deferred install failure");
    assert_eq!(status.code(), Some(1));
    assert!(
        update.join("install-intent.v1").is_file(),
        "intent must survive seal failure for retry"
    );
    let _ = fs::remove_dir_all(root);
}
