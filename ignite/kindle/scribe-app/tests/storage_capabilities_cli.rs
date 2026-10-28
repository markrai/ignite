use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn isolated_root(name: &str) -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "scribe-storage-capabilities-{}-{nonce}-{name}",
        std::process::id()
    ))
}

#[test]
fn command_emits_only_canonical_bytes_without_startup_side_effects() {
    let root = isolated_root("isolated");
    fs::create_dir_all(&root).unwrap();
    let library = root.join("library");
    let sync = root.join("sync");
    let device = root.join("device");
    let singleton = root.join("scribe-app.lock");

    let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
        .arg("--storage-capabilities")
        .current_dir(&root)
        .env("HOME", &root)
        .env("USERPROFILE", &root)
        .env("TMPDIR", &root)
        .env("SCRIBE_DATA_DIR", &library)
        .env("SCRIBE_SYNC_DIR", &sync)
        .env("SCRIBE_DEVICE_DIR", &device)
        .env("SCRIBE_SINGLETON_PATH", &singleton)
        .output()
        .expect("run scribe-app capability command");

    assert!(
        output.status.success(),
        "status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        scribe_app::storage_capabilities::canonical_bytes().unwrap()
    );
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("launch_ms"));
    assert!(!stdout.contains("scribe_singleton"));
    assert!(!library.exists());
    assert!(!sync.exists());
    assert!(!device.exists());
    assert!(!singleton.exists());
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);

    fs::remove_dir(&root).unwrap();
}

#[test]
fn mixed_capability_invocation_fails_without_normal_startup() {
    let root = isolated_root("invalid");
    fs::create_dir_all(&root).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
        .args(["--storage-capabilities", "--help"])
        .current_dir(&root)
        .output()
        .expect("run invalid capability command");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be used alone"));
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::remove_dir(&root).unwrap();
}

#[test]
fn ordinary_help_retains_the_normal_startup_path() {
    let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
        .arg("--help")
        .output()
        .expect("run scribe-app help");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("launch_ms phase=process_entry ms="));
    assert!(stdout.contains("--storage-capabilities"));
}
