use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use scribe_app::storage_release_provenance::COMPONENT_ROLES;

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
                    let mut file = match fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&owner)
                    {
                        Ok(file) => file,
                        Err(error) => {
                            let _ = fs::remove_dir(&path);
                            return Err(format!("create {}: {error}", owner.display()));
                        }
                    };
                    if let Err(error) = std::io::Write::write_all(&mut file, token.as_bytes()) {
                        drop(file);
                        let _ = fs::remove_file(&owner);
                        let _ = fs::remove_dir(&path);
                        return Err(format!("write {}: {error}", owner.display()));
                    }
                    return Ok(Self { path, token });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("create {}: {error}", path.display())),
            }
        }
        Err("no exclusive provenance CLI test-root candidate was available".into())
    }

    fn new(prefix: &str) -> Self {
        let candidates = (0..128).map(|_| {
            std::env::temp_dir().join(format!(
                "{prefix}-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                uuid::Uuid::now_v7()
            ))
        });
        Self::from_candidates(candidates).expect("create exclusive provenance CLI test root")
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
}

impl Fixture {
    fn new() -> Self {
        let owner = OwnedTestRoot::new("ignite-provenance-cli");
        let root = owner.path.clone();
        for name in [
            "record",
            "record.sig",
            "release.manifest",
            "release.manifest.sig",
            "capabilities",
        ] {
            fs::write(root.join(name), b"synthetic\n").unwrap();
        }
        Self {
            root,
            _owner: owner,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_scribe-app"));
        command.args([
            "--inspect-release-provenance",
            "--subject",
            "B",
            "--expected-tag",
            "v1.2.3",
            "--record",
        ]);
        command.arg(self.root.join("record"));
        command.arg("--record-signature");
        command.arg(self.root.join("record.sig"));
        command.arg("--release-manifest");
        command.arg(self.root.join("release.manifest"));
        command.arg("--release-manifest-signature");
        command.arg(self.root.join("release.manifest.sig"));
        command.arg("--capabilities");
        command.arg(self.root.join("capabilities"));
        for role in COMPONENT_ROLES {
            command.arg("--component");
            command.arg(format!(
                "{role}={}",
                self.root.join(format!("component-{role}")).display()
            ));
        }
        command
    }
}

fn snapshot(root: &Path) -> BTreeMap<OsString, Vec<u8>> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect()
}

#[test]
fn official_entry_point_refuses_synthetic_metadata_before_application_startup() {
    let fixture = Fixture::new();
    let before = snapshot(&fixture.root);
    let output = fixture.command().output().unwrap();
    let after = snapshot(&fixture.root);

    assert_ne!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(before, after);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("IGNITE_RELEASE_PROVENANCE_INSPECTION_V1\n"));
    assert!(
        stdout.contains("status=AUTHORITY_UNAVAILABLE\n")
            || stdout.contains("status=SIGNATURE_INVALID\n")
    );
    assert!(stdout.contains("data_compatibility=unknown\n"));
    assert!(stdout.contains("promotion_authorized=false\n"));
    assert!(stdout.contains("inspection_action=report-only\n"));
    assert!(stdout.contains("installation_binding=unverified\n"));
    assert!(!stdout.contains("launch_ms"));
}

#[test]
fn process_fixture_cleanup_preserves_collisions_and_substituted_roots() {
    let parent = OwnedTestRoot::new("ignite-provenance-cli-owner-parent");
    let collision = parent.path.join("collision");
    let candidate = parent.path.join("candidate");
    fs::create_dir(&collision).unwrap();
    let collision_sentinel = collision.join("foreign-sentinel");
    fs::write(&collision_sentinel, b"preserve collision").unwrap();

    let owned = OwnedTestRoot::from_candidates([collision.clone(), candidate.clone()])
        .expect("skip collision and claim candidate");
    assert_eq!(owned.path, candidate);

    let displaced = parent.path.join("displaced");
    fs::rename(&candidate, &displaced).unwrap();
    fs::create_dir(&candidate).unwrap();
    let replacement_sentinel = candidate.join("foreign-replacement");
    fs::write(&replacement_sentinel, b"preserve replacement").unwrap();
    drop(owned);

    assert_eq!(fs::read(collision_sentinel).unwrap(), b"preserve collision");
    assert_eq!(
        fs::read(replacement_sentinel).unwrap(),
        b"preserve replacement"
    );
}

#[test]
fn incomplete_or_relative_cli_paths_are_rejected_before_inspection() {
    // This deliberately omits the remaining required paths and every component mapping.
    let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
        .args([
            "--inspect-release-provenance",
            "--subject",
            "A",
            "--expected-tag",
            "v1.2.3",
            "--record",
            "relative",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}
