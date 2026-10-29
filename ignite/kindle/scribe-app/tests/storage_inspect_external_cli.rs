use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use scribe_app::storage_inspect_external::{inspect_external, ExternalInspectionConfig};
use sha2::{Digest, Sha256};

static NEXT: AtomicU64 = AtomicU64::new(1);
const OWNER_FILE: &str = ".ignite-test-root-owner";

struct OwnedTestRoot {
    path: PathBuf,
    token: String,
}

impl OwnedTestRoot {
    fn from_candidates(candidates: impl IntoIterator<Item = PathBuf>) -> Result<Self, String> {
        for path in candidates {
            match fs::create_dir(&path) {
                Ok(()) => {
                    let token = uuid::Uuid::now_v7().to_string();
                    let owner = path.join(OWNER_FILE);
                    let mut file = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&owner)
                        .map_err(|error| format!("create {}: {error}", owner.display()))?;
                    file.write_all(token.as_bytes())
                        .map_err(|error| format!("write {}: {error}", owner.display()))?;
                    return Ok(Self { path, token });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("create {}: {error}", path.display())),
            }
        }
        Err("no exclusive process-test root candidate was available".into())
    }

    fn new() -> Self {
        let candidates = (0..128).map(|_| {
            std::env::temp_dir().join(format!(
                "scribe-external-cli-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                uuid::Uuid::now_v7()
            ))
        });
        Self::from_candidates(candidates).expect("create exclusive process-test root")
    }
}

impl Drop for OwnedTestRoot {
    fn drop(&mut self) {
        let owner = self.path.join(OWNER_FILE);
        let owned = fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            && fs::symlink_metadata(&owner)
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
            && fs::read_to_string(&owner).is_ok_and(|token| token.as_str() == self.token.as_str());
        if owned {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct Fixture {
    root: PathBuf,
    _owner: OwnedTestRoot,
    config: ExternalInspectionConfig,
}

impl Fixture {
    fn new() -> Self {
        let owner = OwnedTestRoot::new();
        let root = owner.path.clone();
        let config = ExternalInspectionConfig {
            library_root: root.join("library"),
            sync_root: root.join("sync"),
            device_root: root.join("device"),
            state_root: root.join("state"),
        };
        fs::create_dir_all(&config.library_root).unwrap();
        let mut manifest = serde_json::to_vec(&serde_json::json!({
            "v": 1,
            "notebook_ids": ["notebook-1"],
            "active_notebook_id": "notebook-1",
            "next_notebook_ordinal": 2
        }))
        .unwrap();
        manifest.push(b'\n');
        fs::write(config.library_root.join("library.meta"), manifest).unwrap();
        Self {
            config,
            root,
            _owner: owner,
        }
    }

    fn args(&self) -> Vec<String> {
        vec![
            "--inspect-storage-external".into(),
            "--data-dir".into(),
            self.config.library_root.to_string_lossy().into_owned(),
            "--sync-dir".into(),
            self.config.sync_root.to_string_lossy().into_owned(),
            "--device-dir".into(),
            self.config.device_root.to_string_lossy().into_owned(),
            "--state-dir".into(),
            self.config.state_root.to_string_lossy().into_owned(),
        ]
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
                    (bytes.len() as u64, format!("{:x}", Sha256::digest(&bytes))),
                );
            }
        }
    }
    let mut output = BTreeMap::new();
    walk(root, root, &mut output);
    output
}

#[test]
fn process_fixture_claim_is_exclusive_and_preserves_collisions() {
    let parent = OwnedTestRoot::new();
    let collision = parent.path.join("collision");
    let candidate = parent.path.join("owned");
    fs::create_dir(&collision).unwrap();
    let sentinel = collision.join("foreign-sentinel");
    fs::write(&sentinel, b"preserve").unwrap();

    let owned = OwnedTestRoot::from_candidates([collision.clone(), candidate.clone()])
        .expect("skip collision and claim a distinct root");
    assert_eq!(owned.path, candidate);
    drop(owned);

    assert!(!candidate.exists());
    assert_eq!(fs::read(sentinel).unwrap(), b"preserve");
}

#[test]
fn process_cli_runs_before_initialization_and_matches_library_api() {
    let fixture = Fixture::new();
    let isolated = fixture.root.join("isolated-runtime");
    let before = manifest(&fixture.root);
    let expected = inspect_external(&fixture.config).canonical_bytes().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
        .args(fixture.args())
        .current_dir(&fixture.root)
        .env("HOME", &isolated)
        .env("USERPROFILE", &isolated)
        .env("TMPDIR", &isolated)
        .env("SCRIBE_SINGLETON_PATH", isolated.join("scribe-app.lock"))
        .output()
        .expect("run external inspection command");

    assert!(
        output.status.success(),
        "status={} stderr={} stdout={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout, expected);
    assert!(!isolated.exists());
    assert_eq!(before, manifest(&fixture.root));
}

#[test]
fn process_cli_returns_issue_status_and_preserves_pending_bytes() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.config.device_root).unwrap();
    let pending = fixture.config.device_root.join("server-profile.tmp");
    fs::write(&pending, b"do-not-consume").unwrap();
    let before = manifest(&fixture.root);
    let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
        .args(fixture.args())
        .output()
        .expect("run external inspection command");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("temporary_device_state_object_present")
    );
    assert_eq!(fs::read(&pending).unwrap(), b"do-not-consume");
    assert_eq!(before, manifest(&fixture.root));
}

#[test]
fn process_cli_never_reports_missing_required_library_as_valid() {
    let fixture = Fixture::new();
    fs::remove_dir_all(&fixture.config.library_root).unwrap();
    let before = manifest(&fixture.root);
    let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
        .args(fixture.args())
        .output()
        .expect("run external inspection with missing library");

    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("result=ISSUES_DETECTED"));
    assert!(stdout.contains("required_library_root_missing"));
    assert!(!stdout.contains("result=VALID\n"));
    assert_eq!(before, manifest(&fixture.root));
}

#[test]
fn process_cli_rejects_missing_duplicate_mixed_and_reordered_arguments() {
    let fixture = Fixture::new();
    let valid = fixture.args();
    let cases = [
        valid[..8].to_vec(),
        [valid.clone(), valid.clone()].concat(),
        [valid.clone(), vec!["--storage-capabilities".into()]].concat(),
        vec![
            "--inspect-storage-external".into(),
            "--sync-dir".into(),
            "sync".into(),
            "--data-dir".into(),
            "library".into(),
            "--device-dir".into(),
            "device".into(),
            "--state-dir".into(),
            "state".into(),
        ],
    ];
    for args in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
            .args(args)
            .output()
            .expect("run invalid external inspection command");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}
