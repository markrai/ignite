use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

const OWNER_FILE: &str = ".ignite-test-root-owner";
static NEXT: AtomicU64 = AtomicU64::new(1);

struct OwnedTestRoot {
    path: PathBuf,
    token: String,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl OwnedTestRoot {
    fn new() -> Self {
        for _ in 0..128 {
            let path = std::env::temp_dir().join(format!(
                "scribe-compatibility-preflight-cli-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                uuid::Uuid::now_v7()
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    let root_metadata = fs::symlink_metadata(&path).unwrap();
                    assert!(root_metadata.is_dir());
                    assert!(!root_metadata.file_type().is_symlink());
                    let token = uuid::Uuid::now_v7().to_string();
                    let owner = path.join(OWNER_FILE);
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&owner)
                        .unwrap();
                    file.write_all(token.as_bytes()).unwrap();
                    return Self {
                        path,
                        token,
                        #[cfg(unix)]
                        device: {
                            use std::os::unix::fs::MetadataExt as _;
                            root_metadata.dev()
                        },
                        #[cfg(unix)]
                        inode: {
                            use std::os::unix::fs::MetadataExt as _;
                            root_metadata.ino()
                        },
                    };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create {}: {error}", path.display()),
            }
        }
        panic!("no exclusive compatibility-preflight CLI root was available");
    }
}

impl Drop for OwnedTestRoot {
    fn drop(&mut self) {
        let owner = self.path.join(OWNER_FILE);
        let root_owned = fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
        #[cfg(unix)]
        let root_owned = root_owned
            && fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
                use std::os::unix::fs::MetadataExt as _;
                metadata.dev() == self.device && metadata.ino() == self.inode
            });
        let marker_owned = fs::symlink_metadata(&owner)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink());
        #[cfg(unix)]
        let marker_owned = marker_owned
            && fs::symlink_metadata(&owner).is_ok_and(|metadata| {
                use std::os::unix::fs::MetadataExt as _;
                metadata.nlink() == 1
            });
        let owned = root_owned
            && marker_owned
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

struct Fixture {
    _owner: OwnedTestRoot,
    app: PathBuf,
    library: PathBuf,
    sync: PathBuf,
    device: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let owner = OwnedTestRoot::new();
        let app = owner.path.join("app");
        let state = app.join("state");
        let sync = state.join("sync");
        let device = state.join("device");
        let library = owner.path.join("library");
        for path in [&app, &state, &sync, &device, &library] {
            fs::create_dir(path).unwrap();
        }
        fs::write(library.join("library.meta"), b"preserve-library\n").unwrap();
        fs::write(sync.join("outbox.jsonl"), b"preserve-outbox\n").unwrap();
        fs::write(device.join("preferences.json"), b"{}\n").unwrap();
        Self {
            _owner: owner,
            app,
            library,
            sync,
            device,
            state,
        }
    }

    fn args(&self) -> Vec<String> {
        vec![
            "--inspect-compatibility-preflight".into(),
            "--app-root".into(),
            self.app.to_string_lossy().into_owned(),
            "--data-dir".into(),
            self.library.to_string_lossy().into_owned(),
            "--sync-dir".into(),
            self.sync.to_string_lossy().into_owned(),
            "--device-dir".into(),
            self.device.to_string_lossy().into_owned(),
            "--state-dir".into(),
            self.state.to_string_lossy().into_owned(),
        ]
    }
}

#[test]
fn command_runs_before_initialization_preserves_inputs_and_never_authorizes() {
    let fixture = Fixture::new();
    let before = manifest(&fixture._owner.path);
    let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
        .args(fixture.args())
        .current_dir(&fixture._owner.path)
        .env("HOME", fixture._owner.path.join("isolated-home"))
        .env("USERPROFILE", fixture._owner.path.join("isolated-profile"))
        .output()
        .expect("run compatibility preflight");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("IGNITE_COMPATIBILITY_PREFLIGHT_V1\n"));
    assert!(stdout.contains("observational_stability=equal\n"));
    assert!(stdout.contains("writer_exclusion=unverified\n"));
    assert!(stdout.contains("installation_binding=unverified\n"));
    assert!(stdout.contains("bidirectional_compatibility=unavailable\n"));
    assert!(stdout.contains("atomic_snapshot=false\n"));
    assert!(stdout.contains("data_compatibility=unknown\n"));
    assert!(stdout.contains("promotion_authorized=false\n"));
    assert!(stdout.contains("inspection_action=report-only\n"));
    assert!(!stdout.contains("BIDIRECTIONAL_COMPATIBLE"));
    assert_eq!(before, manifest(&fixture._owner.path));
}

#[test]
fn malformed_or_mixed_commands_fail_before_reading_or_writing_roots() {
    let fixture = Fixture::new();
    let valid = fixture.args();
    let before = manifest(&fixture._owner.path);
    let cases = [
        valid[..10].to_vec(),
        [valid.clone(), valid.clone()].concat(),
        [valid.clone(), vec!["--inspect-writer-exclusion".into()]].concat(),
        {
            let mut args = valid.clone();
            args[4] = "relative".into();
            args
        },
        {
            let mut args = valid.clone();
            args.swap(3, 5);
            args
        },
    ];
    for args in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
            .args(args)
            .current_dir(&fixture._owner.path)
            .output()
            .expect("run invalid compatibility preflight");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    assert_eq!(before, manifest(&fixture._owner.path));
}
