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
        format!(
            "{SCRIPTLET_CONTRACT_CLAIM_PREFIX}'{}'",
            self.storage_contract
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContractError {
    Missing,
    Symlink,
    NotFile,
    Io(String),
    Malformed(String),
    Mismatch {
        expected: String,
        actual: String,
    },
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
    let text =
        std::str::from_utf8(bytes).map_err(|_| ContractError::Malformed("not utf-8".into()))?;
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

pub fn write_contract_file_atomic(
    dir: &Path,
    contract: &StorageContract,
) -> Result<(), ContractError> {
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
pub fn require_same_storage_contract(
    staged_contract_dir: &Path,
) -> Result<StorageContract, ContractError> {
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

/// Hard ceiling for SFX shell-prefix reads. Real stamped prefixes are ~7 KiB; this
/// bound refuses pathological offsets without loading a multi-megabyte installer.
pub const SFX_PREFIX_READ_MAX: usize = 64 * 1024;

/// Bound the authenticated SFX shell prefix without decoding the gzip payload as UTF-8.
///
/// Real `build_sfx.py` installers are `prefix || gzip(tar)`. Prefer the stamped
/// `IGNITE_ARCHIVE_OFFSET=` value; otherwise stop at the first non-text byte.
pub fn sfx_shell_prefix_bound(scriptlet_bytes: &[u8]) -> usize {
    if let Some(offset) = parse_archive_offset(scriptlet_bytes) {
        if offset > 0 && offset <= scriptlet_bytes.len() {
            return offset;
        }
    }
    scriptlet_bytes
        .iter()
        .position(|&b| b == 0 || b >= 0x80)
        .unwrap_or(scriptlet_bytes.len())
}

fn parse_archive_offset(scriptlet_bytes: &[u8]) -> Option<usize> {
    const HEAD: usize = 8192;
    let head = &scriptlet_bytes[..scriptlet_bytes.len().min(HEAD)];
    for line in head.split(|&b| b == b'\n') {
        const PREFIX: &[u8] = b"IGNITE_ARCHIVE_OFFSET=";
        if let Some(raw) = line.strip_prefix(PREFIX) {
            if let Ok(text) = std::str::from_utf8(raw) {
                if let Ok(offset) = text.trim().parse::<usize>() {
                    return Some(offset);
                }
            }
        }
    }
    None
}

/// Read only the bounded SFX shell prefix from an authenticated installer path.
///
/// Does not load the gzip payload. Callers must already have stream-verified the
/// full installer digest before trusting the returned prefix bytes.
pub fn read_sfx_shell_prefix_from_path(path: &Path) -> Result<Vec<u8>, ContractError> {
    let meta = fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ContractError::Io("scriptlet missing".into())
        } else {
            ContractError::Io(e.to_string())
        }
    })?;
    if meta.file_type().is_symlink() {
        return Err(ContractError::Io("scriptlet must not be a symlink".into()));
    }
    if !meta.is_file() {
        return Err(ContractError::Io("scriptlet is not a regular file".into()));
    }
    let mut file = File::open(path).map_err(|e| ContractError::Io(e.to_string()))?;
    read_sfx_shell_prefix_from_reader(&mut file)
}

/// Read at most [`SFX_PREFIX_READ_MAX`] bytes and truncate to the SFX shell prefix.
pub fn read_sfx_shell_prefix_from_reader(reader: &mut impl Read) -> Result<Vec<u8>, ContractError> {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 8192];
    while buf.len() < SFX_PREFIX_READ_MAX {
        let want = (SFX_PREFIX_READ_MAX - buf.len()).min(chunk.len());
        let n = reader
            .read(&mut chunk[..want])
            .map_err(|e| ContractError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    if let Some(offset) = parse_archive_offset(&buf) {
        if offset > buf.len() {
            return Err(ContractError::Malformed(format!(
                "SFX shell prefix offset {offset} exceeds read bound {}",
                buf.len()
            )));
        }
    }
    let bound = sfx_shell_prefix_bound(&buf);
    buf.truncate(bound);
    Ok(buf)
}

fn scriptlet_contains_contract_claim(scriptlet_bytes: &[u8], claim_line: &str) -> bool {
    let bound = sfx_shell_prefix_bound(scriptlet_bytes);
    let prefix = &scriptlet_bytes[..bound];
    let Ok(text) = std::str::from_utf8(prefix) else {
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

/// Materialize `storage-contract.v1` from an authenticated Scriptlet claim.
/// The file is derived from authenticated release bytes — never trusted as an
/// independent sidecar.
pub fn materialize_contract_from_authenticated_scriptlet(
    staged_contract_dir: &Path,
    authenticated_scriptlet_bytes: &[u8],
) -> Result<StorageContract, ContractError> {
    let current = StorageContract::current();
    let claim = current.scriptlet_claim_line();
    if !scriptlet_contains_contract_claim(authenticated_scriptlet_bytes, &claim) {
        return Err(ContractError::UnauthenticatedClaim(format!(
            "authenticated Scriptlet missing claim line {claim}"
        )));
    }
    write_contract_file_atomic(staged_contract_dir, &current)?;
    Ok(current)
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
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
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
        let err =
            require_release_bound_storage_contract(&dir, b"#!/bin/sh\necho no-contract-claim\n")
                .unwrap_err();
        assert!(matches!(err, ContractError::UnauthenticatedClaim(_)));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn matching_contract_file_with_scriptlet_claim_is_accepted() {
        let dir = temp_dir("bound");
        let contract = StorageContract::current();
        write_contract_file_atomic(&dir, &contract).unwrap();
        let scriptlet = format!("#!/bin/sh\n{}\necho ok\n", contract.scriptlet_claim_line());
        require_release_bound_storage_contract(&dir, scriptlet.as_bytes()).unwrap();
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn large_sfx_payload_uses_bounded_prefix_read_only() {
        let contract = StorageContract::current();
        let claim = contract.scriptlet_claim_line();
        let work = temp_dir("large-sfx");
        let path = work.join("Install-Ignite-v9.9.9.sh");
        let mut prefix =
            format!("#!/bin/sh\nIGNITE_ARCHIVE_OFFSET=0000000128\n{claim}\necho prefix\n")
                .into_bytes();
        while prefix.len() < 128 {
            prefix.push(b'\n');
        }
        prefix.truncate(128);
        {
            use std::io::Write as _;
            let mut file = File::create(&path).unwrap();
            file.write_all(&prefix).unwrap();
            // Append a multi-megabyte binary payload without loading it as a Vec in the reader path.
            const PAYLOAD: usize = 8 * 1024 * 1024;
            let chunk = vec![0x1f_u8; 64 * 1024];
            let mut written = 0usize;
            while written < PAYLOAD {
                let n = (PAYLOAD - written).min(chunk.len());
                file.write_all(&chunk[..n]).unwrap();
                written += n;
            }
            file.sync_all().unwrap();
        }
        let meta = fs::metadata(&path).unwrap();
        assert!(meta.len() > 8 * 1024 * 1024);
        let got = read_sfx_shell_prefix_from_path(&path).expect("bounded prefix read");
        assert_eq!(got.len(), 128);
        assert!(got.len() < SFX_PREFIX_READ_MAX);
        assert!(scriptlet_contains_contract_claim(&got, &claim));
        materialize_contract_from_authenticated_scriptlet(&work.join("assets"), &got).unwrap();
        let _ = fs::remove_dir_all(work);
    }

    #[test]
    fn binary_sfx_payload_does_not_poison_prefix_claim_scan() {
        let contract = StorageContract::current();
        let claim = contract.scriptlet_claim_line();
        // Simulate stamped SFX: UTF-8 prefix + gzip magic / binary tail.
        let mut scriptlet =
            format!("#!/bin/sh\nIGNITE_ARCHIVE_OFFSET=0000000096\n{claim}\necho prefix\n")
                .into_bytes();
        while scriptlet.len() < 96 {
            scriptlet.push(b'\n');
        }
        scriptlet.truncate(96);
        scriptlet.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00]); // gzip magic
        scriptlet.extend_from_slice(&[0xff, 0xfe, 0x00, 0x80]); // non-UTF-8 payload
        let bound = sfx_shell_prefix_bound(&scriptlet);
        assert_eq!(bound, 96);
        assert!(scriptlet_contains_contract_claim(&scriptlet, &claim));
        // Full-buffer UTF-8 decode would fail; prefix-bounded scan must still work.
        assert!(std::str::from_utf8(&scriptlet).is_err());
    }

    #[test]
    fn build_sfx_py_generated_installer_binds_contract_claim() {
        let deploy = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../deployment/kindle");
        let build_sfx = deploy.join("sfx/build_sfx.py");
        let template = deploy.join("sfx/Install-Ignite.sfx.sh");
        if !build_sfx.is_file() || !template.is_file() {
            return;
        }
        let py = which_python();
        let work = temp_dir("build-sfx");
        let payload = work.join("payload");
        fs::create_dir_all(payload.join("bin")).unwrap();
        fs::write(
            payload.join("bin/scribe-app"),
            (0u8..=255).cycle().take(4096).collect::<Vec<_>>(),
        )
        .unwrap();
        fs::write(payload.join("bin/scribe-lifecycle"), b"lifecycle\n").unwrap();
        let out = work.join("Install-Ignite-v0.0.99.sh");
        let output = std::process::Command::new(&py)
            .arg(&build_sfx)
            .args([
                "--payload",
                payload.to_str().unwrap(),
                "--version",
                "0.0.99",
                "--out",
                out.to_str().unwrap(),
                "--template",
                template.to_str().unwrap(),
            ])
            .output()
            .expect("spawn build_sfx.py");
        assert!(
            output.status.success(),
            "build_sfx.py failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let blob = fs::read(&out).expect("sfx bytes");
        assert!(
            std::str::from_utf8(&blob).is_err(),
            "real SFX must include binary gzip payload"
        );
        let dir = temp_dir("sfx-contract");
        write_contract_file_atomic(&dir, &StorageContract::current()).unwrap();
        require_release_bound_storage_contract(&dir, &blob)
            .expect("prefix-bounded claim must bind on real SFX");
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_dir_all(work);
    }

    fn which_python() -> String {
        for candidate in ["python3", "python"] {
            if std::process::Command::new(candidate)
                .arg("-c")
                .arg("import sys")
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return candidate.to_string();
            }
        }
        "python3".into()
    }
}
