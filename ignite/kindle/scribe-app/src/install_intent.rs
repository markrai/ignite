//! Deferred install intent — instruction only, never a trust anchor.
//!
//! The lifecycle post-shutdown executor must reauthenticate the official signed
//! release and bind sealed executable bytes to that authorization. Fields here
//! only locate the staging tree and expected release identity.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const INTENT_HEADER: &str = "IGNITE_INSTALL_INTENT_V1";
pub const INTENT_SCHEMA: u32 = 1;
pub const INTENT_FILE_NAME: &str = "install-intent.v1";
const MAX_INTENT_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallIntent {
    pub expected_tag: String,
    pub version: String,
    pub staging_root: PathBuf,
    pub storage_contract: String,
    pub intent_txn: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntentError {
    Missing,
    Symlink,
    NotFile,
    Io(String),
    Malformed(String),
}

impl std::fmt::Display for IntentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "install-intent.v1 missing"),
            Self::Symlink => write!(f, "install-intent.v1 must not be a symlink"),
            Self::NotFile => write!(f, "install-intent.v1 is not a regular file"),
            Self::Io(detail) => write!(f, "install-intent io: {detail}"),
            Self::Malformed(detail) => write!(f, "install-intent malformed: {detail}"),
        }
    }
}

pub fn intent_path(update_dir: &Path) -> PathBuf {
    update_dir.join(INTENT_FILE_NAME)
}

impl InstallIntent {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        format!(
            "{INTENT_HEADER}\n\
             schema={INTENT_SCHEMA}\n\
             expected_tag={}\n\
             version={}\n\
             staging_root={}\n\
             storage_contract={}\n\
             intent_txn={}\n\
             trust_anchor=false\n\
             note=reauthenticate-before-exec\n",
            self.expected_tag,
            self.version,
            self.staging_root.display(),
            self.storage_contract,
            self.intent_txn,
        )
        .into_bytes()
    }
}

pub fn parse_intent_bytes(bytes: &[u8]) -> Result<InstallIntent, IntentError> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| IntentError::Malformed("not utf-8".into()))?;
    if text.contains('\r') || text.contains('\0') {
        return Err(IntentError::Malformed("CR/NUL refused".into()));
    }
    if !text.ends_with('\n') {
        return Err(IntentError::Malformed("missing trailing LF".into()));
    }
    let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    if lines.len() != 9 {
        return Err(IntentError::Malformed(format!(
            "expected 9 lines, got {}",
            lines.len()
        )));
    }
    if lines[0] != INTENT_HEADER {
        return Err(IntentError::Malformed("bad header".into()));
    }
    if lines[1] != format!("schema={INTENT_SCHEMA}") {
        return Err(IntentError::Malformed("unsupported schema".into()));
    }
    let expected_tag = require_field(lines[2], "expected_tag=")?;
    let version = require_field(lines[3], "version=")?;
    let staging_root = PathBuf::from(require_field(lines[4], "staging_root=")?);
    let storage_contract = require_field(lines[5], "storage_contract=")?;
    let intent_txn = require_field(lines[6], "intent_txn=")?;
    if lines[7] != "trust_anchor=false" {
        return Err(IntentError::Malformed(
            "intent must declare trust_anchor=false".into(),
        ));
    }
    if lines[8] != "note=reauthenticate-before-exec" {
        return Err(IntentError::Malformed("missing reauth note".into()));
    }
    if !staging_root.is_absolute() {
        return Err(IntentError::Malformed(
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

fn require_field<'a>(line: &'a str, prefix: &str) -> Result<&'a str, IntentError> {
    line.strip_prefix(prefix)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| IntentError::Malformed(format!("missing {prefix}")))
}

pub fn load_intent(update_dir: &Path) -> Result<InstallIntent, IntentError> {
    let path = intent_path(update_dir);
    let meta = fs::symlink_metadata(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            IntentError::Missing
        } else {
            IntentError::Io(e.to_string())
        }
    })?;
    if meta.file_type().is_symlink() {
        return Err(IntentError::Symlink);
    }
    if !meta.is_file() {
        return Err(IntentError::NotFile);
    }
    if meta.len() > MAX_INTENT_BYTES {
        return Err(IntentError::Malformed("oversized".into()));
    }
    let mut file = File::open(&path).map_err(|e| IntentError::Io(e.to_string()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| IntentError::Io(e.to_string()))?;
    parse_intent_bytes(&bytes)
}

pub fn write_intent_atomic(update_dir: &Path, intent: &InstallIntent) -> Result<(), IntentError> {
    fs::create_dir_all(update_dir).map_err(|e| IntentError::Io(e.to_string()))?;
    let final_path = intent_path(update_dir);
    let tmp = update_dir.join(format!(".install-intent.{}.tmp", intent.intent_txn));
    if tmp.exists() {
        let meta = fs::symlink_metadata(&tmp).map_err(|e| IntentError::Io(e.to_string()))?;
        if meta.file_type().is_symlink() {
            return Err(IntentError::Symlink);
        }
        fs::remove_file(&tmp).map_err(|e| IntentError::Io(e.to_string()))?;
    }
    let bytes = intent.canonical_bytes();
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| IntentError::Io(e.to_string()))?;
        file.write_all(&bytes)
            .map_err(|e| IntentError::Io(e.to_string()))?;
        file.sync_all()
            .map_err(|e| IntentError::Io(e.to_string()))?;
    }
    fs::rename(&tmp, &final_path).map_err(|e| IntentError::Io(e.to_string()))?;
    if let Ok(dir_file) = File::open(update_dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

pub fn clear_intent(update_dir: &Path) -> Result<(), IntentError> {
    let path = intent_path(update_dir);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(IntentError::Io(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ignite-intent-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn intent_round_trip_declares_not_trust_anchor() {
        let dir = temp_dir("round");
        let intent = InstallIntent {
            expected_tag: "v0.9.15".into(),
            version: "0.9.15".into(),
            staging_root: std::env::temp_dir().join("ignite-staging-abs"),
            storage_contract: "ignite-storage-v1".into(),
            intent_txn: "20261102T120000Z-1".into(),
        };
        assert!(intent.staging_root.is_absolute());
        write_intent_atomic(&dir, &intent).unwrap();
        let loaded = load_intent(&dir).unwrap();
        assert_eq!(loaded, intent);
        let text = String::from_utf8(intent.canonical_bytes()).unwrap();
        assert!(text.contains("trust_anchor=false\n"));
        assert!(!text.contains("scriptlet_sha256="));
        clear_intent(&dir).unwrap();
        assert!(matches!(load_intent(&dir), Err(IntentError::Missing)));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn relative_staging_root_refused() {
        let bytes = b"IGNITE_INSTALL_INTENT_V1\n\
schema=1\n\
expected_tag=v1\n\
version=1.0.0\n\
staging_root=relative\n\
storage_contract=ignite-storage-v1\n\
intent_txn=t1\n\
trust_anchor=false\n\
note=reauthenticate-before-exec\n";
        assert!(parse_intent_bytes(bytes).is_err());
    }
}
