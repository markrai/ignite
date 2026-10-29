use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

static NEXT: AtomicU64 = AtomicU64::new(1);
const OWNER_FILE: &str = ".ignite-test-root-owner";

struct OwnedTestRoot {
    path: PathBuf,
    token: String,
}

impl OwnedTestRoot {
    fn new() -> Self {
        for _ in 0..128 {
            let path = std::env::temp_dir().join(format!(
                "scribe-writer-exclusion-cli-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                uuid::Uuid::new_v4()
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    let token = uuid::Uuid::new_v4().to_string();
                    fs::write(path.join(OWNER_FILE), token.as_bytes()).unwrap();
                    return Self { path, token };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create {}: {error}", path.display()),
            }
        }
        panic!("no exclusive process-test root candidate was available");
    }
}

impl Drop for OwnedTestRoot {
    fn drop(&mut self) {
        let owner = self.path.join(OWNER_FILE);
        let owned = fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            && fs::symlink_metadata(&owner)
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
            && fs::read_to_string(&owner)
                .is_ok_and(|contents| contents.as_str() == self.token.as_str());
        if owned {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn manifest(root: &Path) -> BTreeMap<String, (u64, String)> {
    fn walk(root: &Path, at: &Path, output: &mut BTreeMap<String, (u64, String)>) {
        let mut entries = fs::read_dir(at)
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, output);
            } else {
                let bytes = fs::read(&path).unwrap();
                output.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    (bytes.len() as u64, format!("{:x}", Sha256::digest(bytes))),
                );
            }
        }
    }
    let mut output = BTreeMap::new();
    walk(root, root, &mut output);
    output
}

fn valid_args(root: &Path) -> Vec<String> {
    vec![
        "--inspect-writer-exclusion".into(),
        "--data-dir".into(),
        root.join("library").to_string_lossy().into_owned(),
        "--sync-dir".into(),
        root.join("sync").to_string_lossy().into_owned(),
    ]
}

#[test]
fn command_runs_before_initialization_is_read_only_and_never_establishes_exclusion() {
    let owner = OwnedTestRoot::new();
    let root = &owner.path;
    fs::create_dir(root.join("library")).unwrap();
    fs::create_dir(root.join("sync")).unwrap();
    fs::write(root.join("library/user-data"), b"preserve-library").unwrap();
    fs::write(root.join("sync/outbox.jsonl"), b"preserve-outbox\n").unwrap();
    let before = manifest(root);

    let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
        .args(valid_args(root))
        .current_dir(root)
        .env("HOME", root.join("isolated-home"))
        .env("USERPROFILE", root.join("isolated-profile"))
        .env("TMPDIR", root.join("isolated-tmp"))
        .output()
        .expect("run writer-exclusion diagnostic");

    assert_eq!(output.status.code(), Some(3));
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("IGNITE_WRITER_EXCLUSION_INSPECTION_V1\n"));
    assert!(stdout.contains("writer_exclusion=unverified\n"));
    assert!(!stdout.contains("writer_exclusion=established"));
    assert!(stdout.contains("usb_exclusion=unavailable\n"));
    assert!(stdout.contains("atomic_snapshot=false\n"));
    assert!(stdout.contains("data_compatibility=unknown\n"));
    assert!(stdout.contains("promotion_authorized=false\n"));
    assert!(stdout.contains("inspection_action=report-only\n"));
    assert_eq!(before, manifest(root));
}

#[test]
fn command_rejects_mixed_missing_duplicate_and_reordered_arguments_without_writes() {
    let owner = OwnedTestRoot::new();
    let root = &owner.path;
    let valid = valid_args(root);
    let before = manifest(root);
    let cases = [
        valid[..4].to_vec(),
        [valid.clone(), valid.clone()].concat(),
        [valid.clone(), vec!["--storage-capabilities".into()]].concat(),
        vec![
            "--inspect-writer-exclusion".into(),
            "--sync-dir".into(),
            root.join("sync").to_string_lossy().into_owned(),
            "--data-dir".into(),
            root.join("library").to_string_lossy().into_owned(),
        ],
    ];
    for args in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
            .args(args)
            .current_dir(root)
            .output()
            .expect("run invalid writer-exclusion command");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    assert_eq!(before, manifest(root));
}
