//! Crash-recoverable pending-health qualification for the scoped no-migration updater.
//!
//! Does not claim capability-inventory completeness. Qualification requires boundary-held
//! runtime health evidence plus matching storage-contract identity. Promotion of
//! executables is already done; this only retires pending-health so the next OTA may proceed.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub const JOURNAL_HEADER: &str = "IGNITE_QUALIFY_JOURNAL_V1";
pub const JOURNAL_SCHEMA: u32 = 1;
pub const JOURNAL_NAME: &str = "ignite-qualify.journal";
pub const PENDING_NAME: &str = "pending-health.v1";
pub const LKG_INVENTORY_NAME: &str = "ignite-lkg-inventory.v1";
pub const LKG_HANDLER_NAME: &str = "appreg-handler.sql";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QualifyPhase {
    Prepared,
    PendingRemoved,
    Complete,
}

impl QualifyPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::PendingRemoved => "pending_removed",
            Self::Complete => "complete",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "prepared" => Some(Self::Prepared),
            "pending_removed" => Some(Self::PendingRemoved),
            "complete" => Some(Self::Complete),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QualifyJournal {
    pub pending_txn: String,
    pub pending_sha256: String,
    pub storage_contract: String,
    pub phase: QualifyPhase,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QualifyError {
    Io(String),
    Malformed(String),
    Unsafe(String),
    NotEligible(String),
}

impl std::fmt::Display for QualifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(d) | Self::Malformed(d) | Self::Unsafe(d) | Self::NotEligible(d) => {
                write!(f, "{d}")
            }
        }
    }
}

pub fn journal_path(update_dir: &Path) -> PathBuf {
    update_dir.join(JOURNAL_NAME)
}

pub fn parse_journal_bytes(bytes: &[u8]) -> Result<QualifyJournal, QualifyError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| QualifyError::Malformed("journal not utf-8".into()))?;
    if text.contains('\r') || text.contains('\0') || !text.ends_with('\n') {
        return Err(QualifyError::Malformed("journal encoding refused".into()));
    }
    let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    if lines.len() != 6 {
        return Err(QualifyError::Malformed(format!(
            "expected 6 journal lines, got {}",
            lines.len()
        )));
    }
    if lines[0] != JOURNAL_HEADER {
        return Err(QualifyError::Malformed("bad journal header".into()));
    }
    if lines[1] != format!("schema={JOURNAL_SCHEMA}") {
        return Err(QualifyError::Malformed("unsupported journal schema".into()));
    }
    let pending_txn = require(lines[2], "pending_txn=")?;
    let pending_sha256 = require(lines[3], "pending_sha256=")?;
    let storage_contract = require(lines[4], "storage_contract=")?;
    let phase_raw = require(lines[5], "phase=")?;
    let phase = QualifyPhase::parse(phase_raw)
        .ok_or_else(|| QualifyError::Malformed("unknown phase".into()))?;
    if pending_sha256.len() != 64 || !pending_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(QualifyError::Malformed("pending_sha256 invalid".into()));
    }
    Ok(QualifyJournal {
        pending_txn: pending_txn.to_string(),
        pending_sha256: pending_sha256.to_string(),
        storage_contract: storage_contract.to_string(),
        phase,
    })
}

fn require<'a>(line: &'a str, prefix: &str) -> Result<&'a str, QualifyError> {
    line.strip_prefix(prefix)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| QualifyError::Malformed(format!("missing {prefix}")))
}

pub fn load_journal(update_dir: &Path) -> Result<Option<QualifyJournal>, QualifyError> {
    let path = journal_path(update_dir);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(QualifyError::Io(e.to_string())),
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(QualifyError::Unsafe("qualify journal is a symlink".into()));
            }
            if !meta.is_file() {
                return Err(QualifyError::Unsafe(
                    "qualify journal is not a regular file".into(),
                ));
            }
            let mut file = File::open(&path).map_err(|e| QualifyError::Io(e.to_string()))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|e| QualifyError::Io(e.to_string()))?;
            Ok(Some(parse_journal_bytes(&bytes)?))
        }
    }
}

fn write_journal(update_dir: &Path, journal: &QualifyJournal) -> Result<(), QualifyError> {
    fs::create_dir_all(update_dir).map_err(|e| QualifyError::Io(e.to_string()))?;
    let final_path = journal_path(update_dir);
    let tmp = update_dir.join(format!(".ignite-qualify.{}.tmp", journal.pending_txn));
    if tmp.exists() {
        let meta = fs::symlink_metadata(&tmp).map_err(|e| QualifyError::Io(e.to_string()))?;
        if meta.file_type().is_symlink() {
            return Err(QualifyError::Unsafe("qualify tmp is a symlink".into()));
        }
        fs::remove_file(&tmp).map_err(|e| QualifyError::Io(e.to_string()))?;
    }
    let bytes = format!(
        "{JOURNAL_HEADER}\n\
         schema={JOURNAL_SCHEMA}\n\
         pending_txn={}\n\
         pending_sha256={}\n\
         storage_contract={}\n\
         phase={}\n",
        journal.pending_txn,
        journal.pending_sha256,
        journal.storage_contract,
        journal.phase.as_str()
    )
    .into_bytes();
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| QualifyError::Io(e.to_string()))?;
        file.write_all(&bytes)
            .map_err(|e| QualifyError::Io(e.to_string()))?;
        file.sync_all()
            .map_err(|e| QualifyError::Io(e.to_string()))?;
    }
    fs::rename(&tmp, &final_path).map_err(|e| QualifyError::Io(e.to_string()))?;
    if let Ok(dir) = File::open(update_dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<(), QualifyError> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(QualifyError::Io(e.to_string())),
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(QualifyError::Unsafe(format!(
                    "refusing to unlink symlink {}",
                    path.display()
                )));
            }
            fs::remove_file(path).map_err(|e| QualifyError::Io(e.to_string()))
        }
    }
}

pub fn pending_digest(update_dir: &Path) -> Result<(String, String), QualifyError> {
    pending_sha256(update_dir)
}

pub fn write_journal_for_test(
    update_dir: &Path,
    journal: &QualifyJournal,
) -> Result<(), QualifyError> {
    write_journal(update_dir, journal)
}

fn pending_sha256(update_dir: &Path) -> Result<(String, String), QualifyError> {
    let path = update_dir.join(PENDING_NAME);
    let meta = fs::symlink_metadata(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            QualifyError::NotEligible("pending-health.v1 absent".into())
        } else {
            QualifyError::Io(e.to_string())
        }
    })?;
    if meta.file_type().is_symlink() {
        return Err(QualifyError::Unsafe("pending-health is a symlink".into()));
    }
    let mut file = File::open(&path).map_err(|e| QualifyError::Io(e.to_string()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| QualifyError::Io(e.to_string()))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| QualifyError::Malformed("pending-health not utf-8".into()))?;
    let txn = text
        .lines()
        .find_map(|line| line.strip_prefix("candidate_txn="))
        .ok_or_else(|| QualifyError::Malformed("pending-health missing candidate_txn".into()))?
        .to_string();
    Ok((txn, encode_hex(&Sha256::digest(&bytes))))
}

/// Finish interrupted qualification before installer `guard_pending_health`.
///
/// Crash between pending removal and LKG cleanup must not permanently block updates.
pub fn reconcile_interrupted_qualification(
    update_dir: &Path,
    previous_release_dir: &Path,
) -> Result<(), QualifyError> {
    let Some(journal) = load_journal(update_dir)? else {
        return Ok(());
    };
    match journal.phase {
        QualifyPhase::Prepared => {
            // Crash window: prepared journal but pending already removed before
            // pending_removed could be recorded. Finish LKG cleanup so install
            // preflight cannot stay stuck on ambiguous LKG-without-pending.
            if !update_dir.join(PENDING_NAME).exists() {
                finish_lkg_cleanup(previous_release_dir)?;
                remove_if_present(&journal_path(update_dir))?;
                if let Ok(dir) = File::open(update_dir) {
                    let _ = dir.sync_all();
                }
            }
            Ok(())
        }
        QualifyPhase::PendingRemoved | QualifyPhase::Complete => {
            finish_lkg_cleanup(previous_release_dir)?;
            remove_if_present(&journal_path(update_dir))?;
            if let Ok(dir) = File::open(update_dir) {
                let _ = dir.sync_all();
            }
            Ok(())
        }
    }
}

fn finish_lkg_cleanup(previous_release_dir: &Path) -> Result<(), QualifyError> {
    remove_if_present(&previous_release_dir.join(LKG_INVENTORY_NAME))?;
    remove_if_present(&previous_release_dir.join(LKG_HANDLER_NAME))?;
    Ok(())
}

/// Commit qualification after caller has verified runtime + contract eligibility.
pub fn commit_qualification(
    update_dir: &Path,
    previous_release_dir: &Path,
    storage_contract: &str,
) -> Result<(), QualifyError> {
    if let Some(existing) = load_journal(update_dir)? {
        match existing.phase {
            QualifyPhase::PendingRemoved | QualifyPhase::Complete => {
                return reconcile_interrupted_qualification(update_dir, previous_release_dir);
            }
            QualifyPhase::Prepared => {}
        }
    }

    let (pending_txn, pending_sha) = pending_sha256(update_dir)?;
    let mut journal = QualifyJournal {
        pending_txn,
        pending_sha256: pending_sha,
        storage_contract: storage_contract.to_string(),
        phase: QualifyPhase::Prepared,
    };
    write_journal(update_dir, &journal)?;

    // Commit: remove pending-health while journal marks prepared → then pending_removed.
    remove_if_present(&update_dir.join(PENDING_NAME))?;
    if let Ok(dir) = File::open(update_dir) {
        let _ = dir.sync_all();
    }
    journal.phase = QualifyPhase::PendingRemoved;
    write_journal(update_dir, &journal)?;

    finish_lkg_cleanup(previous_release_dir)?;
    journal.phase = QualifyPhase::Complete;
    write_journal(update_dir, &journal)?;
    remove_if_present(&journal_path(update_dir))?;
    if let Ok(dir) = File::open(update_dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn encode_hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn temp_tree() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "ignite-qualify-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let update = root.join("update");
        let prev = root.join("previous-release");
        fs::create_dir_all(&update).unwrap();
        fs::create_dir_all(&prev).unwrap();
        (update, prev)
    }

    fn write_pending(update: &Path, txn: &str) {
        let body = format!(
            "IGNITE_PENDING_HEALTH_V1\n\
             schema=1\n\
             status=pending-health\n\
             candidate_version=0.9.15\n\
             candidate_txn={txn}\n\
             candidate_app_bytes=1\n\
             candidate_app_sha256={}\n\
             candidate_lifecycle_bytes=1\n\
             candidate_lifecycle_sha256={}\n\
             retained_release_identity=previous-release\n\
             lkg_inventory_bytes=1\n\
             lkg_inventory_sha256={}\n\
             data_compatibility=unknown\n",
            "ab".repeat(32),
            "cd".repeat(32),
            "ef".repeat(32)
        );
        fs::write(update.join(PENDING_NAME), body).unwrap();
    }

    #[test]
    fn successful_qualification_clears_pending_and_lkg_metadata() {
        let (update, prev) = temp_tree();
        write_pending(&update, "txn-1");
        fs::write(prev.join(LKG_INVENTORY_NAME), b"inventory\n").unwrap();
        fs::write(prev.join(LKG_HANDLER_NAME), b"handler\n").unwrap();
        commit_qualification(&update, &prev, "ignite-storage-v1").unwrap();
        assert!(!update.join(PENDING_NAME).exists());
        assert!(!prev.join(LKG_INVENTORY_NAME).exists());
        assert!(!prev.join(LKG_HANDLER_NAME).exists());
        assert!(!journal_path(&update).exists());
        let _ = fs::remove_dir_all(update.parent().unwrap());
    }

    #[test]
    fn crash_after_pending_removed_recovers_without_blocking() {
        let (update, prev) = temp_tree();
        write_pending(&update, "txn-2");
        fs::write(prev.join(LKG_INVENTORY_NAME), b"inventory\n").unwrap();
        fs::write(prev.join(LKG_HANDLER_NAME), b"handler\n").unwrap();
        let (txn, sha) = pending_sha256(&update).unwrap();
        write_journal(
            &update,
            &QualifyJournal {
                pending_txn: txn,
                pending_sha256: sha,
                storage_contract: "ignite-storage-v1".into(),
                phase: QualifyPhase::Prepared,
            },
        )
        .unwrap();
        fs::remove_file(update.join(PENDING_NAME)).unwrap();
        write_journal(
            &update,
            &QualifyJournal {
                pending_txn: "txn-2".into(),
                pending_sha256: "aa".repeat(32),
                storage_contract: "ignite-storage-v1".into(),
                phase: QualifyPhase::PendingRemoved,
            },
        )
        .unwrap();
        reconcile_interrupted_qualification(&update, &prev).unwrap();
        assert!(!prev.join(LKG_INVENTORY_NAME).exists());
        assert!(!prev.join(LKG_HANDLER_NAME).exists());
        assert!(!journal_path(&update).exists());
        // Ambiguous LKG-without-pending is cleared; next install guard can proceed.
        assert!(!update.join(PENDING_NAME).exists());
        let _ = fs::remove_dir_all(update.parent().unwrap());
    }

    #[test]
    fn prepared_journal_with_pending_left_intact() {
        let (update, prev) = temp_tree();
        write_pending(&update, "txn-3");
        let (txn, sha) = pending_sha256(&update).unwrap();
        write_journal(
            &update,
            &QualifyJournal {
                pending_txn: txn,
                pending_sha256: sha,
                storage_contract: "ignite-storage-v1".into(),
                phase: QualifyPhase::Prepared,
            },
        )
        .unwrap();
        reconcile_interrupted_qualification(&update, &prev).unwrap();
        assert!(update.join(PENDING_NAME).exists());
        assert!(journal_path(&update).exists());
        let _ = fs::remove_dir_all(update.parent().unwrap());
    }

    #[test]
    fn repeated_commit_is_idempotent_after_success() {
        let (update, prev) = temp_tree();
        write_pending(&update, "txn-4");
        commit_qualification(&update, &prev, "ignite-storage-v1").unwrap();
        let err = commit_qualification(&update, &prev, "ignite-storage-v1").unwrap_err();
        assert!(matches!(err, QualifyError::NotEligible(_)));
        let _ = fs::remove_dir_all(update.parent().unwrap());
    }
}
