use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use scribe_app::storage_release_provenance::COMPONENT_ROLES;
use sha2::{Digest, Sha256};

const OWNER_FILE: &str = ".ignite-test-root-owner";
const TRANSACTION: &str = "20261030T120000Z-7";
static NEXT: AtomicU64 = AtomicU64::new(1);

const INVENTORY_NAMES: [&str; 18] = [
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

struct OwnedTestRoot {
    path: PathBuf,
    token: String,
}

impl OwnedTestRoot {
    fn new(prefix: &str) -> Self {
        for _ in 0..128 {
            let path = std::env::temp_dir().join(format!(
                "{prefix}-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                uuid::Uuid::now_v7()
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    let token = uuid::Uuid::now_v7().to_string();
                    fs::write(path.join(OWNER_FILE), &token).unwrap();
                    return Self { path, token };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create {}: {error}", path.display()),
            }
        }
        panic!("no exclusive slot-binding CLI test root was available");
    }
}

impl Drop for OwnedTestRoot {
    fn drop(&mut self) {
        let marker = self.path.join(OWNER_FILE);
        let owned = fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            && fs::symlink_metadata(&marker)
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
            && fs::read_to_string(&marker).is_ok_and(|token| token.as_str() == self.token.as_str());
        if owned {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct Fixture {
    root: PathBuf,
    _owner: OwnedTestRoot,
    app: PathBuf,
    documents: PathBuf,
    recovery: PathBuf,
    components: Vec<(String, PathBuf)>,
}

impl Fixture {
    fn new() -> Self {
        let owner = OwnedTestRoot::new("ignite-installed-slot-cli");
        let root = owner.path.clone();
        let app = root.join("app");
        let documents = root.join("documents");
        let recovery = root.join("recovery");
        let evidence = root.join("evidence");
        for directory in [
            app.join("bin"),
            app.join("assets/licenses/noto-emoji"),
            app.join("assets/licenses/source-sans-3"),
            app.join("tools/ignite-release/lib"),
            app.join("state/update"),
            app.join("run"),
            app.join("previous-release"),
            documents.clone(),
            recovery.join("bin"),
            recovery.join("manifest"),
            evidence.clone(),
        ] {
            fs::create_dir_all(directory).unwrap();
        }

        let mut components = Vec::new();
        for role in COMPONENT_ROLES {
            let path = candidate_path(&app, &documents, &recovery, role)
                .unwrap_or_else(|| evidence.join(format!("unmapped-{role}")));
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, format!("component {role}\n")).unwrap();
            components.push((role.into(), path));
        }
        // recovery-version has no canonical signed-role mapping, but the real installer creates it.
        fs::write(recovery.join("manifest/version"), b"dropbear-2024.85\n").unwrap();

        let retained = app.join("previous-release");
        for name in INVENTORY_NAMES {
            fs::write(retained.join(name), format!("retained {name}\n")).unwrap();
        }
        let inventory = inventory_bytes(&retained);
        fs::write(retained.join("ignite-lkg-inventory.v1"), &inventory).unwrap();
        let app_bytes = fs::read(app.join("bin/scribe-app")).unwrap();
        let life_bytes = fs::read(app.join("bin/scribe-lifecycle")).unwrap();
        fs::write(
            app.join("state/update/pending-health.v1"),
            pending_bytes(&app_bytes, &life_bytes, &inventory),
        )
        .unwrap();
        for name in [
            "record",
            "record.sig",
            "release.manifest",
            "release.manifest.sig",
            "capabilities",
        ] {
            fs::write(evidence.join(name), b"synthetic\n").unwrap();
        }

        Self {
            root,
            _owner: owner,
            app,
            documents,
            recovery,
            components,
        }
    }

    fn command(&self) -> Command {
        let evidence = self.root.join("evidence");
        let mut command = Command::new(env!("CARGO_BIN_EXE_scribe-app"));
        command.args([
            "--inspect-installed-slot-binding",
            "--release-role",
            "candidate-b",
            "--expected-transaction",
            TRANSACTION,
            "--app-root",
        ]);
        command.arg(&self.app);
        command.arg("--documents-root");
        command.arg(&self.documents);
        command.arg("--recovery-root");
        command.arg(&self.recovery);
        command.args(["--expected-tag", "v1.2.3", "--record"]);
        command.arg(evidence.join("record"));
        command.arg("--record-signature");
        command.arg(evidence.join("record.sig"));
        command.arg("--release-manifest");
        command.arg(evidence.join("release.manifest"));
        command.arg("--release-manifest-signature");
        command.arg(evidence.join("release.manifest.sig"));
        command.arg("--capabilities");
        command.arg(evidence.join("capabilities"));
        for (role, path) in &self.components {
            command.arg("--component");
            command.arg(format!("{role}={}", path.display()));
        }
        command
    }
}

fn candidate_path(app: &Path, documents: &Path, recovery: &Path, role: &str) -> Option<PathBuf> {
    Some(match role {
        "scribe-app" => app.join("bin/scribe-app"),
        "scribe-lifecycle" => app.join("bin/scribe-lifecycle"),
        "scribe-lifecycle-host" => app.join("bin/scribe-lifecycle-host.sh"),
        "launcher" => documents.join("Scribe.sh"),
        "registration-source" | "recovery-version" => return None,
        "icon" => app.join("assets/scribe-icon.png"),
        "license-noto-emoji" => app.join("assets/licenses/noto-emoji/OFL.txt"),
        "license-noto-emoji-description" => {
            app.join("assets/licenses/noto-emoji/DESCRIPTION.en_us.html")
        }
        "license-noto-emoji-upstream-info" => {
            app.join("assets/licenses/noto-emoji/upstream_info.md")
        }
        "license-source-sans-3" => app.join("assets/licenses/source-sans-3/OFL.txt"),
        "install-runtime" => app.join("tools/ignite-release/lib/install-runtime.sh"),
        "uninstall-runtime" => app.join("tools/ignite-release/lib/uninstall-runtime.sh"),
        "backup-runtime" => app.join("tools/ignite-release/lib/backup-runtime.sh"),
        "register-helpers" => app.join("tools/ignite-release/lib/register-helpers.sh"),
        "install-recovery" => app.join("tools/ignite-release/lib/install-recovery.sh"),
        "uninstall-entry" => documents.join("Uninstall Ignite.sh"),
        "backup-entry" => documents.join("Backup Ignite Data.sh"),
        "diagnose-entry" => documents.join("Diagnose Ignite.sh"),
        "recovery-dropbear" => recovery.join("bin/dropbearmulti"),
        "recovery-elf-sha256" => recovery.join("manifest/elf.sha256"),
        "recovery-provenance" => recovery.join("manifest/provenance.txt"),
        _ => return None,
    })
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn inventory_bytes(root: &Path) -> Vec<u8> {
    let mut output = String::from(
        "IGNITE_LKG_INVENTORY_V1\nschema=1\nkind=ignite-application-release\nentry_count=18\n",
    );
    for name in INVENTORY_NAMES {
        let bytes = fs::read(root.join(name)).unwrap();
        writeln!(
            output,
            "entry={name}|file|{}|{}",
            bytes.len(),
            hex(&Sha256::digest(&bytes))
        )
        .unwrap();
    }
    output.into_bytes()
}

fn pending_bytes(app: &[u8], lifecycle: &[u8], inventory: &[u8]) -> Vec<u8> {
    let inventory_sha = hex(&Sha256::digest(inventory));
    format!(
        "IGNITE_PENDING_HEALTH_V1\nschema=1\nstatus=pending-health\ncandidate_version=1.2.3\ncandidate_txn={TRANSACTION}\ncandidate_app_bytes={}\ncandidate_app_sha256={}\ncandidate_lifecycle_bytes={}\ncandidate_lifecycle_sha256={}\nretained_version=unknown\nretained_release_identity=sha256:{inventory_sha}\nlkg_inventory_bytes={}\nlkg_inventory_sha256={inventory_sha}\ndata_compatibility=unknown\n",
        app.len(),
        hex(&Sha256::digest(app)),
        lifecycle.len(),
        hex(&Sha256::digest(lifecycle)),
        inventory.len(),
    )
    .into_bytes()
}

fn snapshot(root: &Path) -> BTreeMap<OsString, Vec<u8>> {
    fs::read_dir(root)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            entry
                .file_type()
                .unwrap()
                .is_file()
                .then(|| (entry.file_name(), fs::read(entry.path()).unwrap()))
        })
        .collect()
}

#[test]
fn official_cli_is_report_only_and_never_reports_admission_success() {
    let fixture = Fixture::new();
    let before = snapshot(&fixture.root.join("evidence"));
    let output = fixture.command().output().unwrap();
    let after = snapshot(&fixture.root.join("evidence"));

    assert_ne!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(before, after);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("IGNITE_INSTALLED_RELEASE_BINDING_V1\n"));
    assert!(
        stdout.contains("result=AUTHORITY_UNAVAILABLE\n")
            || stdout.contains("result=INCONSISTENT\n")
    );
    assert!(stdout.contains("installation_binding=unverified\n"));
    assert!(stdout.contains("execution_binding=unverified\n"));
    assert!(stdout.contains("first_launch_admission=unavailable\n"));
    assert!(stdout.contains("data_compatibility=unknown\n"));
    assert!(stdout.contains("promotion_authorized=false\n"));
    assert!(stdout.contains("inspection_action=report-only\n"));
    assert!(!stdout.contains("launch_ms"));
}

#[test]
fn incomplete_or_relative_cli_input_is_rejected_before_application_startup() {
    let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
        .args([
            "--inspect-installed-slot-binding",
            "--release-role",
            "candidate-b",
            "--expected-transaction",
            TRANSACTION,
            "--app-root",
            "relative",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}
