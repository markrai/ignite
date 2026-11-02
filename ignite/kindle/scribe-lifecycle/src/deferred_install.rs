//! Post-shutdown deferred install: reauthenticate, seal, exec under updater lock.
//!
//! Must not run inside the LIPC `on_unload` callback. The host acknowledges unload and
//! confirms child death first; this module runs on the lifecycle-owned post-shutdown path.
//!
//! `install-intent.v1` is an instruction only. Official publication authentication and
//! sealed-byte binding happen via `scribe-app --seal-authorized-install` before exec.
//! Sealed path execution preserves Install-Ignite.sfx.sh `$0`/`dd`/`su` behavior.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::updater_lock::{self, LockError};

const INTENT_NAME: &str = "install-intent.v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallIntent {
    pub expected_tag: String,
    pub version: String,
    pub staging_root: PathBuf,
    pub storage_contract: String,
    pub intent_txn: String,
}

#[derive(Debug)]
pub enum DeferredError {
    MissingIntent,
    Malformed(String),
    Seal(String),
    Lock(LockError),
    Io(String),
    Exec(String),
}

impl std::fmt::Display for DeferredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingIntent => write!(f, "install-intent.v1 absent"),
            Self::Malformed(d) | Self::Seal(d) | Self::Io(d) | Self::Exec(d) => write!(f, "{d}"),
            Self::Lock(e) => write!(f, "{e}"),
        }
    }
}

pub fn intent_path(update_dir: &Path) -> PathBuf {
    update_dir.join(INTENT_NAME)
}

pub fn load_intent(update_dir: &Path) -> Result<InstallIntent, DeferredError> {
    let path = intent_path(update_dir);
    let meta = match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(DeferredError::MissingIntent);
        }
        Err(e) => return Err(DeferredError::Io(e.to_string())),
        Ok(m) => m,
    };
    if meta.file_type().is_symlink() {
        return Err(DeferredError::Malformed("intent must not be a symlink".into()));
    }
    if !meta.is_file() {
        return Err(DeferredError::Malformed("intent is not a regular file".into()));
    }
    let mut file = File::open(&path).map_err(|e| DeferredError::Io(e.to_string()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| DeferredError::Io(e.to_string()))?;
    parse_intent(&bytes)
}

fn parse_intent(bytes: &[u8]) -> Result<InstallIntent, DeferredError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| DeferredError::Malformed("intent not utf-8".into()))?;
    if text.contains('\r') || text.contains('\0') || !text.ends_with('\n') {
        return Err(DeferredError::Malformed("intent encoding refused".into()));
    }
    let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    if lines.len() != 9 || lines[0] != "IGNITE_INSTALL_INTENT_V1" {
        return Err(DeferredError::Malformed("intent header/shape refused".into()));
    }
    if lines[1] != "schema=1" || lines[7] != "trust_anchor=false" {
        return Err(DeferredError::Malformed(
            "intent is not a non-trust-anchor v1 instruction".into(),
        ));
    }
    if lines[8] != "note=reauthenticate-before-exec" {
        return Err(DeferredError::Malformed("intent missing reauth note".into()));
    }
    let expected_tag = field(lines[2], "expected_tag=")?;
    let version = field(lines[3], "version=")?;
    let staging_root = PathBuf::from(field(lines[4], "staging_root=")?);
    let storage_contract = field(lines[5], "storage_contract=")?;
    let intent_txn = field(lines[6], "intent_txn=")?;
    if !staging_root.is_absolute() {
        return Err(DeferredError::Malformed(
            "staging_root must be absolute".into(),
        ));
    }
    Ok(InstallIntent {
        expected_tag: expected_tag.to_string(),
        version: version.to_string(),
        staging_root,
        storage_contract: storage_contract.to_string(),
        intent_txn: intent_txn.to_string(),
    })
}

fn field<'a>(line: &'a str, prefix: &str) -> Result<&'a str, DeferredError> {
    line.strip_prefix(prefix)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| DeferredError::Malformed(format!("missing {prefix}")))
}

fn default_scribe_app() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("IGNITE_TEST_SCRIBE_APP_PATH") {
        return PathBuf::from(path);
    }
    PathBuf::from("/mnt/us/scribe-app/bin/scribe-app")
}

fn default_run_dir() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("IGNITE_TEST_APP_RUN_DIR") {
        return PathBuf::from(path);
    }
    PathBuf::from("/mnt/us/scribe-app/run")
}

#[cfg(debug_assertions)]
pub const TEST_RUN_ARGUMENT: &str = "--test-run-deferred-install";

#[cfg(debug_assertions)]
pub fn run_requested_command() -> Option<Result<(), DeferredError>> {
    let mut args = std::env::args_os().skip(1);
    match args.next().as_deref() {
        Some(argument) if argument == std::ffi::OsStr::new(TEST_RUN_ARGUMENT) => {
            let Some(update_dir) = args.next() else {
                return Some(Err(DeferredError::Malformed(
                    "usage: --test-run-deferred-install UPDATE_DIR".into(),
                )));
            };
            if args.next().is_some() {
                return Some(Err(DeferredError::Malformed(
                    "usage: --test-run-deferred-install UPDATE_DIR".into(),
                )));
            }
            Some(run_deferred_install(Path::new(&update_dir)))
        }
        _ => None,
    }
}

/// Reauthenticate via scribe-app seal CLI, then exec sealed Scriptlet while holding updater lock.
/// On success this function does not return (`exec`).
pub fn run_deferred_install(update_dir: &Path) -> Result<(), DeferredError> {
    let intent = load_intent(update_dir)?;
    let run_dir = default_run_dir();
    fs::create_dir_all(&run_dir).map_err(|e| DeferredError::Io(e.to_string()))?;
    let sealed = run_dir.join(format!(
        "ignite-authorized-install-{}.sh",
        intent.intent_txn.replace('/', "_")
    ));
    let receipt = run_dir.join(format!(
        "ignite-authorized-install-{}.receipt",
        intent.intent_txn.replace('/', "_")
    ));

    let status = Command::new(default_scribe_app())
        .args([
            "--seal-authorized-install",
            "--expected-tag",
            &intent.expected_tag,
            "--version",
            &intent.version,
            "--staging-root",
            intent.staging_root.to_str().ok_or_else(|| {
                DeferredError::Malformed("staging_root not utf-8".into())
            })?,
            "--sealed-out",
            sealed.to_str().ok_or_else(|| {
                DeferredError::Malformed("sealed path not utf-8".into())
            })?,
            "--receipt-out",
            receipt
                .to_str()
                .ok_or_else(|| DeferredError::Malformed("receipt path not utf-8".into()))?,
        ])
        .status()
        .map_err(|e| DeferredError::Seal(format!("failed to spawn seal helper: {e}")))?;
    if !status.success() {
        return Err(DeferredError::Seal(format!(
            "seal-authorized-install failed with {status}"
        )));
    }
    if !sealed.is_file() {
        return Err(DeferredError::Seal(
            "seal helper did not produce sealed scriptlet".into(),
        ));
    }

    // Verify receipt declares revalidation and matching contract (not a trust anchor alone).
    let receipt_text =
        fs::read_to_string(&receipt).map_err(|e| DeferredError::Seal(e.to_string()))?;
    if !receipt_text.contains("authorization_revalidated=true\n") {
        return Err(DeferredError::Seal(
            "seal receipt missing authorization_revalidated=true".into(),
        ));
    }
    if !receipt_text.contains(&format!("storage_contract={}\n", intent.storage_contract)) {
        return Err(DeferredError::Seal(
            "seal receipt storage_contract mismatch vs intent".into(),
        ));
    }

    let command = vec![OsString::from("sh"), sealed.as_os_str().to_os_string()];
    updater_lock::exec_with_lock_for_deferred(&command).map_err(DeferredError::Lock)?;
    Err(DeferredError::Exec(
        "exec returned unexpectedly after deferred install".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn temp_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ignite-deferred-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn intent_parser_requires_non_trust_anchor() {
        let good = b"IGNITE_INSTALL_INTENT_V1\n\
schema=1\n\
expected_tag=v0.9.15\n\
version=0.9.15\n\
staging_root=/tmp/stage\n\
storage_contract=ignite-storage-v1\n\
intent_txn=t1\n\
trust_anchor=false\n\
note=reauthenticate-before-exec\n";
        let intent = parse_intent(good).unwrap();
        assert_eq!(intent.expected_tag, "v0.9.15");

        let bad = b"IGNITE_INSTALL_INTENT_V1\n\
schema=1\n\
expected_tag=v0.9.15\n\
version=0.9.15\n\
staging_root=/tmp/stage\n\
storage_contract=ignite-storage-v1\n\
intent_txn=t1\n\
trust_anchor=true\n\
note=reauthenticate-before-exec\n";
        assert!(parse_intent(bad).is_err());
    }

    #[test]
    fn deferred_install_seal_failure_leaves_intent_for_recovery() {
        let root = temp_root("seal-fail");
        let update = root.join("update");
        let run = root.join("run");
        let staging = root.join("staging");
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
        fs::write(update.join(INTENT_NAME), intent).unwrap();

        let mock_app = root.join("mock-scribe-app");
        fs::write(&mock_app, "#!/bin/sh\nexit 97\n").unwrap();
        let mut perms = fs::metadata(&mock_app).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&mock_app, perms).unwrap();

        let err = {
            std::env::set_var("IGNITE_TEST_SCRIBE_APP_PATH", &mock_app);
            std::env::set_var("IGNITE_TEST_APP_RUN_DIR", &run);
            let result = run_deferred_install(&update);
            std::env::remove_var("IGNITE_TEST_SCRIBE_APP_PATH");
            std::env::remove_var("IGNITE_TEST_APP_RUN_DIR");
            result.unwrap_err()
        };
        assert!(
            matches!(err, DeferredError::Seal(_)),
            "expected seal failure, got {err}"
        );
        assert!(
            intent_path(&update).is_file(),
            "intent must remain for retry after seal failure"
        );
        let _ = fs::remove_dir_all(root);
    }
}
