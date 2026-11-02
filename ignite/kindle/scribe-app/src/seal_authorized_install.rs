//! Seal an authenticated install Scriptlet for deferred SFX-compatible execution.
//!
//! Intent files are not trust anchors. This module re-runs installation authorization,
//! binds the opened Scriptlet bytes to that decision, enforces the scoped storage
//! contract, and materializes an immutable sealed copy the lifecycle can exec via
//! `sh <sealed-path>` (compatible with Install-Ignite.sfx.sh `$0` / `dd` / optional `su`).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::installation_authorization::{
    evaluate_installation_authorization, AuthorizeRequest, AuthorizationDecision, DeviceFacts,
};
use crate::storage_contract::{
    require_release_bound_storage_contract, StorageContract, CONTRACT_FILE_NAME, STORAGE_CONTRACT_ID,
};
use crate::release_acquisition::ReleaseAssetNames;
use crate::verified_update_staging::STAGED_ASSETS_DIR;

pub const SEAL_HEADER: &str = "IGNITE_AUTHORIZED_INSTALL_SEAL_V1";
pub const SEAL_SCHEMA: u32 = 1;
pub const SEAL_RECEIPT_NAME: &str = "authorized-install-seal.v1";
const MAX_SCRIPTLET_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealRequest {
    pub expected_tag: String,
    pub version: String,
    pub staging_root: PathBuf,
    pub sealed_scriptlet_path: PathBuf,
    pub seal_receipt_path: PathBuf,
    pub installed_version: String,
    pub device: DeviceFacts,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealResult {
    pub sealed_scriptlet_path: PathBuf,
    pub scriptlet_sha256: String,
    pub storage_contract: String,
    pub bound_scriptlet_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SealError {
    Usage(String),
    AuthorizationRefused(String),
    Authorization(String),
    Contract(String),
    Scriptlet(String),
    Io(String),
}

impl std::fmt::Display for SealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(d) => write!(f, "{d}"),
            Self::AuthorizationRefused(d) => write!(f, "authorization refused: {d}"),
            Self::Authorization(d) => write!(f, "authorization error: {d}"),
            Self::Contract(d) => write!(f, "storage contract: {d}"),
            Self::Scriptlet(d) => write!(f, "scriptlet: {d}"),
            Self::Io(d) => write!(f, "seal io: {d}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SealCommand {
    NotRequested,
    Seal(SealRequest),
}

pub fn parse_command(args: &[String]) -> Result<SealCommand, SealError> {
    let requested = args
        .iter()
        .filter(|a| a.as_str() == "--seal-authorized-install")
        .count();
    if requested == 0 {
        return Ok(SealCommand::NotRequested);
    }
    if requested != 1 {
        return Err(SealError::Usage(
            "--seal-authorized-install must appear exactly once".into(),
        ));
    }
    // --seal-authorized-install --expected-tag T --version V --staging-root S \
    //   --sealed-out PATH --receipt-out PATH
    if args.len() != 11
        || args[0] != "--seal-authorized-install"
        || args[1] != "--expected-tag"
        || args[3] != "--version"
        || args[5] != "--staging-root"
        || args[7] != "--sealed-out"
        || args[9] != "--receipt-out"
        || args[2].is_empty()
        || args[4].is_empty()
        || args[6].is_empty()
        || args[8].is_empty()
        || args[10].is_empty()
    {
        return Err(SealError::Usage(
            "usage: --seal-authorized-install --expected-tag TAG --version VER --staging-root DIR --sealed-out PATH --receipt-out PATH".into(),
        ));
    }
    Ok(SealCommand::Seal(SealRequest {
        expected_tag: args[2].clone(),
        version: args[4].clone(),
        staging_root: PathBuf::from(&args[6]),
        sealed_scriptlet_path: PathBuf::from(&args[8]),
        seal_receipt_path: PathBuf::from(&args[10]),
        installed_version: crate::settings_ui::product_version_digits().to_string(),
        device: DeviceFacts::kindle_defaults_from_host(),
    }))
}

pub fn seal_authorized_install(request: &SealRequest) -> Result<SealResult, SealError> {
    let assets = request.staging_root.join(STAGED_ASSETS_DIR);
    let names = ReleaseAssetNames::for_version(&request.version)
        .map_err(|e| SealError::Usage(e.to_string()))?;
    let scriptlet_path = assets.join(&names.scriptlet);
    let scriptlet_preview = read_regular_file_bytes(&scriptlet_path)?;
    require_release_bound_storage_contract(&assets, &scriptlet_preview)
        .map_err(|e| SealError::Contract(e.to_string()))?;

    let auth_req = AuthorizeRequest {
        expected_tag: request.expected_tag.clone(),
        version: request.version.clone(),
        staging_root: request.staging_root.clone(),
        root_kind: crate::verified_update_staging::StagingRootKind::Production,
        device: request.device.clone(),
        installed_version: request.installed_version.clone(),
        user_approved: true,
    };
    let decision = evaluate_installation_authorization(&auth_req)
        .map_err(|e| SealError::Authorization(e.to_string()))?;
    if !decision.installation_authorized {
        let reason = decision
            .refusal_reasons
            .first()
            .cloned()
            .unwrap_or_else(|| "installation not authorized".into());
        return Err(SealError::AuthorizationRefused(reason));
    }
    seal_from_authorized_decision(request, &decision)
}

/// Test/fixture path: seal using a decision that was already produced by the caller.
pub fn seal_from_authorized_decision(
    request: &SealRequest,
    decision: &AuthorizationDecision,
) -> Result<SealResult, SealError> {
    if !decision.installation_authorized {
        return Err(SealError::AuthorizationRefused(
            "decision is not authorized".into(),
        ));
    }
    let bound_path = decision
        .bound_scriptlet_path
        .as_ref()
        .ok_or_else(|| SealError::AuthorizationRefused("missing bound scriptlet path".into()))?;
    let bound_sha = decision
        .bound_scriptlet_sha256
        .as_ref()
        .ok_or_else(|| SealError::AuthorizationRefused("missing bound scriptlet sha256".into()))?;

    let assets = request.staging_root.join(STAGED_ASSETS_DIR);
    let names = ReleaseAssetNames::for_version(&request.version)
        .map_err(|e| SealError::Usage(e.to_string()))?;
    let expected_asset = assets.join(&names.scriptlet);
    if bound_path != &expected_asset {
        // Allow canonicalized equality.
        let left = bound_path.canonicalize().unwrap_or_else(|_| bound_path.clone());
        let right = expected_asset
            .canonicalize()
            .unwrap_or_else(|_| expected_asset.clone());
        if left != right {
            return Err(SealError::Scriptlet(format!(
                "bound path {} is not the staged asset {}",
                bound_path.display(),
                expected_asset.display()
            )));
        }
    }

    let bytes = read_regular_file_bytes(bound_path)?;
    let actual_sha = encode_hex(&Sha256::digest(&bytes));
    if actual_sha != *bound_sha {
        return Err(SealError::Scriptlet(
            "opened scriptlet bytes do not match authorized binding".into(),
        ));
    }

    write_bytes_exclusive(&request.sealed_scriptlet_path, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(
            &request.sealed_scriptlet_path,
            fs::Permissions::from_mode(0o700),
        );
    }

    let contract = StorageContract::current();
    let receipt = format!(
        "{SEAL_HEADER}\n\
         schema={SEAL_SCHEMA}\n\
         expected_tag={}\n\
         version={}\n\
         staging_root={}\n\
         bound_scriptlet_path={}\n\
         scriptlet_sha256={actual_sha}\n\
         sealed_scriptlet_path={}\n\
         storage_contract={STORAGE_CONTRACT_ID}\n\
         storage_contract_file={}\n\
         authorization_revalidated=true\n\
         promotion_authorized=false\n",
        request.expected_tag,
        request.version,
        request.staging_root.display(),
        bound_path.display(),
        request.sealed_scriptlet_path.display(),
        assets.join(CONTRACT_FILE_NAME).display(),
    );
    write_bytes_exclusive(&request.seal_receipt_path, receipt.as_bytes())?;

    Ok(SealResult {
        sealed_scriptlet_path: request.sealed_scriptlet_path.clone(),
        scriptlet_sha256: actual_sha,
        storage_contract: contract.storage_contract,
        bound_scriptlet_path: bound_path.clone(),
    })
}

fn read_regular_file_bytes(path: &Path) -> Result<Vec<u8>, SealError> {
    let meta = fs::symlink_metadata(path).map_err(|e| SealError::Scriptlet(e.to_string()))?;
    if meta.file_type().is_symlink() {
        return Err(SealError::Scriptlet("scriptlet must not be a symlink".into()));
    }
    if !meta.is_file() {
        return Err(SealError::Scriptlet("scriptlet is not a regular file".into()));
    }
    if meta.len() > MAX_SCRIPTLET_BYTES {
        return Err(SealError::Scriptlet("scriptlet exceeds size limit".into()));
    }
    let mut file = File::open(path).map_err(|e| SealError::Scriptlet(e.to_string()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| SealError::Scriptlet(e.to_string()))?;
    Ok(bytes)
}

fn write_bytes_exclusive(path: &Path, bytes: &[u8]) -> Result<(), SealError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| SealError::Io(e.to_string()))?;
    }
    if path.exists() {
        let meta = fs::symlink_metadata(path).map_err(|e| SealError::Io(e.to_string()))?;
        if meta.file_type().is_symlink() {
            return Err(SealError::Io("refusing to replace symlink seal path".into()));
        }
        fs::remove_file(path).map_err(|e| SealError::Io(e.to_string()))?;
    }
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("seal")
    ));
    if tmp.exists() {
        fs::remove_file(&tmp).map_err(|e| SealError::Io(e.to_string()))?;
    }
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| SealError::Io(e.to_string()))?;
        file.write_all(bytes)
            .map_err(|e| SealError::Io(e.to_string()))?;
        file.sync_all()
            .map_err(|e| SealError::Io(e.to_string()))?;
    }
    fs::rename(&tmp, path).map_err(|e| SealError::Io(e.to_string()))?;
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
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

pub fn emit_seal_report(result: &SealResult) {
    println!("{SEAL_HEADER}");
    println!("schema={SEAL_SCHEMA}");
    println!("sealed_scriptlet_path={}", result.sealed_scriptlet_path.display());
    println!("scriptlet_sha256={}", result.scriptlet_sha256);
    println!("storage_contract={}", result.storage_contract);
    println!("bound_scriptlet_path={}", result.bound_scriptlet_path.display());
    println!("authorization_revalidated=true");
    println!("promotion_authorized=false");
}
