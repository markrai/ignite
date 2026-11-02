//! Narrow no-migration updater storage-contract policy.
//!
//! This is intentionally separate from `storage_capabilities` diagnostics. Those
//! continue to report `coverage=incomplete` / `data_compatibility=unknown` and must
//! never be treated as complete. Matching `STORAGE_CONTRACT_ID` only authorizes the
//! bounded same-contract OTA path (no schema migrations).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub const STORAGE_CONTRACT_ID: &str = "ignite-storage-v1";
pub const CONTRACT_HEADER: &str = "IGNITE_STORAGE_CONTRACT_V1";
pub const CONTRACT_SCHEMA: u32 = 1;
pub const CONTRACT_FILE_NAME: &str = "storage-contract.v1";
/// Exact claim line that must appear in the authenticated Scriptlet / SFX prefix.
/// A matching `assets/storage-contract.v1` alone is not a trust anchor.
pub const SCRIPTLET_CONTRACT_CLAIM_PREFIX: &str = "IGNITE_STORAGE_CONTRACT=";
const MAX_CONTRACT_BYTES: u64 = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageContract {
    pub storage_contract: String,
}

impl StorageContract {
    pub fn current() -> Self {
        Self {
            storage_contract: STORAGE_CONTRACT_ID.to_string(),
        }
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        format!(
            "{CONTRACT_HEADER}\nschema={CONTRACT_SCHEMA}\nstorage_contract={}\n",
            self.storage_contract
        )
        .into_bytes()
    }

    pub fn sha256_hex(&self) -> String {
        encode_hex(&Sha256::digest(self.canonical_bytes()))
    }

    pub fn scriptlet_claim_line(&self) -> String {
        format!("{SCRIPTLET_CONTRACT_CLAIM_PREFIX}'{}'", self.storage_contract)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContractError {
    Missing,
    Symlink,
    NotFile,
    Io(String),
    Malformed(String),
    Mismatch { expected: String, actual: String },
    /// Staged contract file is not attested by the authenticated Scriptlet claim.
    UnauthenticatedClaim(String),
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "storage-contract.v1 missing"),
            Self::Symlink => write!(f, "storage-contract.v1 must not be a symlink"),
            Self::NotFile => write!(f, "storage-contract.v1 is not a regular file"),
            Self::Io(detail) => write!(f, "storage-contract io: {detail}"),
            Self::Malformed(detail) => write!(f, "storage-contract malformed: {detail}"),
            Self::Mismatch { expected, actual } => write!(
                f,
                "storage-contract mismatch: expected {expected}, got {actual}"
            ),
            Self::UnauthenticatedClaim(detail) => write!(
                f,
                "storage-contract not bound to authenticated release: {detail}"
            ),
        }
    }
}

pub fn contract_path(dir: &Path) -> PathBuf {
    dir.join(CONTRACT_FILE_NAME)
}

pub fn parse_contract_bytes(bytes: &[u8]) -> Result<StorageContract, ContractError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ContractError::Malformed("not utf-8".into()))?;
    if text.contains('\r') || text.contains('\0') {
        return Err(ContractError::Malformed("CR/NUL refused".into()));
    }
    let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    if lines.len() != 3 {
        return Err(ContractError::Malformed(format!(
            "expected 3 lines, got {}",
            lines.len()
        )));
    }
    if lines[0] != CONTRACT_HEADER {
        return Err(ContractError::Malformed("bad header".into()));
    }
    if lines[1] != format!("schema={CONTRACT_SCHEMA}") {
        return Err(ContractError::Malformed("unsupported schema".into()));
    }
    let Some(id) = lines[2].strip_prefix("storage_contract=") else {
        return Err(ContractError::Malformed("missing storage_contract".into()));
    };
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ContractError::Malformed(
            "storage_contract token invalid".into(),
        ));
    }
    if !text.ends_with('\n') {
        return Err(ContractError::Malformed("missing trailing LF".into()));
    }
    Ok(StorageContract {
        storage_contract: id.to_string(),
    })
}

pub fn load_contract_file(path: &Path) -> Result<StorageContract, ContractError> {
    let meta = fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ContractError::Missing
        } else {
            ContractError::Io(e.to_string())
        }
    })?;
    if meta.file_type().is_symlink() {
        return Err(ContractError::Symlink);
    }
    if !meta.is_file() {
        return Err(ContractError::NotFile);
    }
    if meta.len() > MAX_CONTRACT_BYTES {
        return Err(ContractError::Malformed("oversized".into()));
    }
    let mut file = File::open(path).map_err(|e| ContractError::Io(e.to_string()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| ContractError::Io(e.to_string()))?;
    parse_contract_bytes(&bytes)
}

pub fn write_contract_file_atomic(dir: &Path, contract: &StorageContract) -> Result<(), ContractError> {
    fs::create_dir_all(dir).map_err(|e| ContractError::Io(e.to_string()))?;
    let final_path = contract_path(dir);
    let tmp = dir.join(format!(".storage-contract.{}.tmp", std::process::id()));
    if tmp.exists() {
        let meta = fs::symlink_metadata(&tmp).map_err(|e| ContractError::Io(e.to_string()))?;
        if meta.file_type().is_symlink() {
            return Err(ContractError::Symlink);
        }
        fs::remove_file(&tmp).map_err(|e| ContractError::Io(e.to_string()))?;
    }
    let bytes = contract.canonical_bytes();
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| ContractError::Io(e.to_string()))?;
        file.write_all(&bytes)
            .map_err(|e| ContractError::Io(e.to_string()))?;
        file.sync_all()
            .map_err(|e| ContractError::Io(e.to_string()))?;
    }
    fs::rename(&tmp, &final_path).map_err(|e| ContractError::Io(e.to_string()))?;
    if let Ok(dir_file) = File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

/// Require staged contract to match the running binary's compiled contract.
pub fn require_same_storage_contract(staged_contract_dir: &Path) -> Result<StorageContract, ContractError> {
    let staged = load_contract_file(&contract_path(staged_contract_dir))?;
    let current = StorageContract::current();
    if staged.storage_contract != current.storage_contract {
        return Err(ContractError::Mismatch {
            expected: current.storage_contract,
            actual: staged.storage_contract,
        });
    }
    Ok(staged)
}

fn scriptlet_contains_contract_claim(scriptlet_bytes: &[u8], claim_line: &str) -> bool {
    let Ok(text) = std::str::from_utf8(scriptlet_bytes) else {
        return false;
    };
    text.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == claim_line
            || trimmed
                .strip_prefix("# ")
                .is_some_and(|rest| rest.trim() == claim_line)
    })
}

/// Require staged `storage-contract.v1` to match the compiled contract **and** be
/// attested by an identical claim line inside the authenticated Scriptlet bytes.
///
/// A matching contract file placed beside an authenticated release that lacks the
/// Scriptlet claim is refused — the file is not an independent trust anchor.
pub fn require_release_bound_storage_contract(
    staged_contract_dir: &Path,
    authenticated_scriptlet_bytes: &[u8],
) -> Result<StorageContract, ContractError> {
    let staged = require_same_storage_contract(staged_contract_dir)?;
    let claim = staged.scriptlet_claim_line();
    if !scriptlet_contains_contract_claim(authenticated_scriptlet_bytes, &claim) {
        return Err(ContractError::UnauthenticatedClaim(format!(
            "authenticated Scriptlet missing claim line {claim}"
        )));
    }
    Ok(staged)
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

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ignite-contract-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn round_trip_current_contract() {
        let dir = temp_dir("round");
        let contract = StorageContract::current();
        write_contract_file_atomic(&dir, &contract).unwrap();
        let loaded = load_contract_file(&contract_path(&dir)).unwrap();
        assert_eq!(loaded, contract);
        require_same_storage_contract(&dir).unwrap();
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn mismatch_is_refused() {
        let dir = temp_dir("mismatch");
        write_contract_file_atomic(
            &dir,
            &StorageContract {
                storage_contract: "other-contract-v9".into(),
            },
        )
        .unwrap();
        match require_same_storage_contract(&dir) {
            Err(ContractError::Mismatch { .. }) => {}
            other => panic!("expected mismatch, got {other:?}"),
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_refused() {
        assert!(parse_contract_bytes(b"nope\n").is_err());
        assert!(parse_contract_bytes(
            b"IGNITE_STORAGE_CONTRACT_V1\nschema=1\nstorage_contract=bad id\n"
        )
        .is_err());
    }

    #[test]
    fn matching_contract_file_without_scriptlet_claim_is_refused() {
        let dir = temp_dir("unauth");
        write_contract_file_atomic(&dir, &StorageContract::current()).unwrap();
        let err = require_release_bound_storage_contract(
            &dir,
            b"#!/bin/sh\necho no-contract-claim\n",
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::UnauthenticatedClaim(_)));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn matching_contract_file_with_scriptlet_claim_is_accepted() {
        let dir = temp_dir("bound");
        let contract = StorageContract::current();
        write_contract_file_atomic(&dir, &contract).unwrap();
        let scriptlet = format!(
            "#!/bin/sh\n{}\necho ok\n",
            contract.scriptlet_claim_line()
        );
        require_release_bound_storage_contract(&dir, scriptlet.as_bytes()).unwrap();
        let _ = fs::remove_dir_all(dir);
    }
}
