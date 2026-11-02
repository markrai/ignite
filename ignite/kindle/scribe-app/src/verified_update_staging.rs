//! Verified Kindle update download/staging (Slice 3D-B2a.4.3i).
//!
//! Acquires the six publication assets, persists them under a dedicated staging
//! root, and reuses the existing publication trust chain + streaming Scriptlet
//! verifier. Success produces observational staging facts only — never install,
//! activation, promotion, or Scriptlet execution authority.
//!
//! A persisted `STAGING_RECORD` is an observation / recovery hint. Authenticity
//! always requires re-verification of the signed publication assets and Scriptlet.
//!
//! Default device root: `/mnt/us/scribe-app/state/verified-update`
//! Fixture rehearsal root: `/mnt/us/scribe-app/state/verified-update-fixture-3i`

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use ed25519_dalek::VerifyingKey;

use crate::publication_trust_boundary::{
    authenticate_official_publication_chain, authenticate_publication_chain_with_key,
    production_trust_report_lines, AuthenticatedPublicationChain, EvidenceClaimStatus,
    ProductionTrustError, VerificationProvenance,
};
use crate::release_acquisition::{
    ensure_free_space, AcquisitionError, AssetSource, ReleaseAssetNames, MAX_ASSET_BYTES,
    MAX_METADATA_ASSET_BYTES, REQUIRED_ASSET_KEYS,
};
use crate::release_manifest::decode_hex;

pub const COMMAND: &str = "--stage-verified-update";
pub const RECORD_HEADER: &str = "IGNITE_VERIFIED_UPDATE_STAGING_V1";
pub const RECORD_SCHEMA: &str = "1";
pub const RECORD_NAME: &str = "STAGING_RECORD";
pub const INCOMPLETE_MARKER: &str = "INCOMPLETE";
pub const DEFAULT_STAGING_ROOT: &str = "/mnt/us/scribe-app/state/verified-update";
pub const FIXTURE_STAGING_ROOT: &str = "/mnt/us/scribe-app/state/verified-update-fixture-3i";
pub const UPDATER_STATE_PARENT: &str = "/mnt/us/scribe-app/state";
/// Ceiling for cached publication metadata reads (manifest/sig/evidence/binding).
pub const MAX_CACHED_METADATA_BYTES: u64 = MAX_METADATA_ASSET_BYTES;
/// Staging observation record is tiny; keep a tighter ceiling than metadata assets.
pub const MAX_STAGING_RECORD_BYTES: u64 = 16 * 1024;

const ASSETS_DIR: &str = "assets";
const ASSETS_NEXT: &str = "assets.next";
const ASSETS_PREV: &str = "assets.prev";
const RECORD_NEXT: &str = "STAGING_RECORD.next";
const RECORD_PREV: &str = "STAGING_RECORD.prev";

/// Paths that cleanup / staging must never delete or traverse into.
const PROTECTED_PREFIXES: &[&str] = &[
    "/mnt/us/scribe-data",
    "/mnt/us/documents",
    "/mnt/us/scribe-app/bin",
    "/mnt/us/scribe-app/assets",
    "/mnt/us/scribe-app/tools",
    "/mnt/us/scribe-app/previous-release",
    "/mnt/us/scribe-app/state/device",
    "/mnt/us/scribe-app/state/sync",
    "/var/local/ignite-recovery",
];

const RECORD_FIXED_PREFIX: &[&str] = &[
    RECORD_HEADER,
    "schema=1",
    // expected_tag=… is dynamic at index 2
];

#[derive(Debug, PartialEq, Eq)]
pub enum StagingError {
    Acquisition(AcquisitionError),
    Trust(ProductionTrustError),
    ScriptletVerify(String),
    UnsafeRoot(String),
    ProtectedPath(String),
    IncompleteRefused(String),
    Io(String),
    Usage(String),
    FixtureKeyRequired,
    ProductionPinBlocks,
    ClaimsNotValidated,
    ExistingCandidate(String),
    RecordInvalid(String),
    TrustModeMismatch,
    RevalidationFailed(String),
    CrossModeRoot(String),
    InterruptedPublication(String),
}

impl std::fmt::Display for StagingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acquisition(e) => write!(f, "{e}"),
            Self::Trust(e) => write!(f, "{e}"),
            Self::ScriptletVerify(e) => write!(f, "scriptlet verification failed: {e}"),
            Self::UnsafeRoot(e) => write!(f, "unsafe staging root: {e}"),
            Self::ProtectedPath(e) => write!(f, "refuses protected path: {e}"),
            Self::IncompleteRefused(e) => write!(f, "incomplete staging: {e}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Usage(e) => write!(f, "{e}"),
            Self::FixtureKeyRequired => write!(
                f,
                "fixture staging requires an explicit --fixture-public-key path"
            ),
            Self::ProductionPinBlocks => write!(
                f,
                "production staging refused: official release signing key is not provisioned"
            ),
            Self::ClaimsNotValidated => write!(
                f,
                "completed staging requires validated production publication claims"
            ),
            Self::ExistingCandidate(tag) => write!(
                f,
                "refusing to replace existing completed staging for different tag {tag}"
            ),
            Self::RecordInvalid(e) => write!(f, "invalid staging record: {e}"),
            Self::TrustModeMismatch => write!(
                f,
                "existing staging provenance does not match the requested trust mode"
            ),
            Self::RevalidationFailed(e) => write!(f, "staging revalidation failed: {e}"),
            Self::CrossModeRoot(e) => write!(f, "staging root/mode mismatch: {e}"),
            Self::InterruptedPublication(e) => write!(f, "interrupted publication: {e}"),
        }
    }
}

impl std::error::Error for StagingError {}

impl From<AcquisitionError> for StagingError {
    fn from(value: AcquisitionError) -> Self {
        Self::Acquisition(value)
    }
}

impl From<ProductionTrustError> for StagingError {
    fn from(value: ProductionTrustError) -> Self {
        Self::Trust(value)
    }
}

impl From<std::io::Error> for StagingError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustMode {
    /// Production entry: official pin (fails closed while UNPROVISIONED).
    Production,
    /// Explicit fixture harness only — never a production authority grant.
    Fixture { public_key: [u8; 32] },
}

/// Production paths must be absolute dedicated updater roots. Host unit tests opt into
/// [`StagingRootKind::HostTest`] explicitly — never via an unrestricted production fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagingRootKind {
    Production,
    HostTest,
}

#[derive(Clone, Debug)]
pub struct StageRequest {
    pub expected_tag: String,
    pub version: String,
    pub staging_root: PathBuf,
    pub source: AssetSource,
    pub trust: TrustMode,
    pub root_kind: StagingRootKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagingObservations {
    pub download_complete: bool,
    pub publisher_authenticated: bool,
    pub publication_claims_validated: bool,
    pub scriptlet_verified: bool,
    pub staging_complete: bool,
    pub provenance: Option<VerificationProvenance>,
}

impl StagingObservations {
    fn incomplete() -> Self {
        Self {
            download_complete: false,
            publisher_authenticated: false,
            publication_claims_validated: false,
            scriptlet_verified: false,
            staging_complete: false,
            provenance: None,
        }
    }

    fn all_complete(&self) -> bool {
        self.download_complete
            && self.publisher_authenticated
            && self.publication_claims_validated
            && self.scriptlet_verified
            && self.staging_complete
    }
}

#[derive(Clone, Debug)]
pub struct StageResult {
    pub observations: StagingObservations,
    pub record_path: PathBuf,
    pub assets_dir: PathBuf,
    pub trust_report: Vec<String>,
    pub release_tag: Option<String>,
}

#[derive(Debug)]
pub enum StageCommand {
    Stage(StageRequest),
    NotRequested,
}

pub fn parse_command(args: &[String]) -> Result<StageCommand, StagingError> {
    let requested = args.iter().filter(|a| a.as_str() == COMMAND).count();
    if requested == 0 {
        return Ok(StageCommand::NotRequested);
    }
    if requested != 1 || args.first().map(String::as_str) != Some(COMMAND) {
        return Err(StagingError::Usage(format!(
            "{COMMAND} must be the first and only diagnostic command"
        )));
    }

    let expected_tag = required_value(args, "--expected-tag")?;
    if !expected_tag.starts_with('v') || expected_tag.len() < 2 {
        return Err(StagingError::Usage(
            "--expected-tag must look like vX.Y.Z".into(),
        ));
    }
    let version = expected_tag[1..].to_string();

    let explicit_staging_root = optional_value(args, "--staging-root").map(PathBuf::from);

    let fixture_assets = optional_value(args, "--fixture-assets-dir").map(PathBuf::from);
    let github = optional_value(args, "--github-release");
    let fixture_key = optional_value(args, "--fixture-public-key").map(PathBuf::from);

    if fixture_assets.is_some() && github.is_some() {
        return Err(StagingError::Usage(
            "provide only one of --fixture-assets-dir or --github-release".into(),
        ));
    }
    if fixture_assets.is_none() && github.is_none() {
        return Err(StagingError::Usage(
            "require --fixture-assets-dir or --github-release=owner/repo".into(),
        ));
    }

    let source = if let Some(root) = fixture_assets {
        AssetSource::LocalDirectory { root }
    } else {
        let origin = crate::release_acquisition::GithubReleaseOrigin::parse(
            github.as_deref().unwrap_or(""),
        )?;
        AssetSource::GithubHttps {
            origin,
            tag: expected_tag.to_string(),
            connect_timeout: std::time::Duration::from_secs(
                crate::release_acquisition::DEFAULT_CONNECT_TIMEOUT_SECS,
            ),
            transfer_timeout: std::time::Duration::from_secs(
                crate::release_acquisition::DEFAULT_TRANSFER_TIMEOUT_SECS,
            ),
        }
    };

    let trust = if let Some(key_path) = fixture_key {
        if !matches!(source, AssetSource::LocalDirectory { .. }) {
            return Err(StagingError::Usage(
                "--fixture-public-key is only valid with --fixture-assets-dir".into(),
            ));
        }
        let hex = fs::read_to_string(&key_path).map_err(|e| {
            StagingError::Usage(format!(
                "cannot read --fixture-public-key {}: {e}",
                key_path.display()
            ))
        })?;
        let public = decode_hex::<32>(hex.trim().as_bytes()).ok_or_else(|| {
            StagingError::Usage("--fixture-public-key must be 64 lowercase hex bytes".into())
        })?;
        VerifyingKey::from_bytes(&public).map_err(|_| {
            StagingError::Usage("--fixture-public-key is not a valid Ed25519 public key".into())
        })?;
        TrustMode::Fixture { public_key: public }
    } else {
        TrustMode::Production
    };

    if matches!(source, AssetSource::LocalDirectory { .. })
        && matches!(trust, TrustMode::Production)
    {
        return Err(StagingError::FixtureKeyRequired);
    }

    let staging_root = explicit_staging_root.unwrap_or_else(|| match trust {
        TrustMode::Fixture { .. } => PathBuf::from(FIXTURE_STAGING_ROOT),
        TrustMode::Production => PathBuf::from(DEFAULT_STAGING_ROOT),
    });

    enforce_trust_root_isolation(&staging_root, &trust, StagingRootKind::Production)?;

    Ok(StageCommand::Stage(StageRequest {
        expected_tag: expected_tag.to_string(),
        version,
        staging_root,
        source,
        trust,
        root_kind: StagingRootKind::Production,
    }))
}

fn required_value<'a>(args: &'a [String], flag: &str) -> Result<&'a str, StagingError> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            return args
                .get(i + 1)
                .map(String::as_str)
                .ok_or_else(|| StagingError::Usage(format!("{flag} requires a value")));
        }
        i += 1;
    }
    Err(StagingError::Usage(format!("missing {flag}")))
}

fn optional_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            return args.get(i + 1).map(String::as_str);
        }
        if let Some(rest) = args[i].strip_prefix(&format!("{flag}=")) {
            return Some(rest);
        }
        i += 1;
    }
    None
}

fn path_text(root: &Path) -> Option<&str> {
    root.to_str()
}

/// Exact production staging root only — no nested descendants.
fn is_exact_production_staging_root(root: &Path) -> bool {
    path_text(root) == Some(DEFAULT_STAGING_ROOT)
}

/// Exact fixture staging root only — no nested descendants.
fn is_exact_fixture_staging_root(root: &Path) -> bool {
    path_text(root) == Some(FIXTURE_STAGING_ROOT)
}

fn is_exact_approved_staging_root(root: &Path) -> bool {
    is_exact_production_staging_root(root) || is_exact_fixture_staging_root(root)
}

/// Production-mode trust pairing uses the exact approved roots (not descendants).
fn is_production_staging_namespace(root: &Path) -> bool {
    is_exact_production_staging_root(root)
}

fn is_fixture_staging_namespace(root: &Path) -> bool {
    is_exact_fixture_staging_root(root)
}

fn is_dedicated_updater_namespace(root: &Path) -> bool {
    is_exact_approved_staging_root(root)
}

/// Walk every existing prefix of `path` with non-following metadata.
///
/// Rejects symlinks and non-directory ancestors. Components that do not yet
/// exist are skipped (the staging root may be created later). This is not a
/// race-free sandbox: a concurrent actor can still substitute a symlink after
/// validation and before a subsequent open/rename/remove.
fn validate_existing_path_components(path: &Path) -> Result<(), StagingError> {
    use std::path::Component;

    let text = path_text(path).unwrap_or("");
    if text.contains("//") || text.ends_with('/') || text.ends_with('\\') {
        return Err(StagingError::UnsafeRoot(format!(
            "noncanonical staging path: {}",
            path.display()
        )));
    }

    let mut accumulated = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                accumulated.push(prefix.as_os_str());
            }
            Component::RootDir => {
                accumulated.push(component.as_os_str());
            }
            Component::CurDir => {
                return Err(StagingError::UnsafeRoot(
                    "`.` path components are refused".into(),
                ));
            }
            Component::ParentDir => {
                return Err(StagingError::UnsafeRoot(
                    "`..` path components are refused".into(),
                ));
            }
            Component::Normal(name) => {
                let name_str = name.to_string_lossy();
                if name_str.is_empty() || name_str == "." || name_str == ".." {
                    return Err(StagingError::UnsafeRoot(format!(
                        "ambiguous path component in {}",
                        path.display()
                    )));
                }
                if name_str.contains('/') || name_str.contains('\\') {
                    return Err(StagingError::UnsafeRoot(format!(
                        "separator inside path component: {name_str}"
                    )));
                }
                accumulated.push(name);
                match fs::symlink_metadata(&accumulated) {
                    Ok(meta) => {
                        if meta.file_type().is_symlink() {
                            return Err(StagingError::UnsafeRoot(format!(
                                "symlink refused in staging path: {}",
                                accumulated.display()
                            )));
                        }
                        if !meta.is_dir() {
                            return Err(StagingError::UnsafeRoot(format!(
                                "staging path ancestor must be a directory: {}",
                                accumulated.display()
                            )));
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        // Remainder does not exist yet; cannot probe further without
                        // creating or following links.
                        break;
                    }
                    Err(err) => return Err(StagingError::Io(err.to_string())),
                }
            }
        }
    }
    Ok(())
}

/// Fixture staging must not populate the production namespace, and vice versa.
pub fn enforce_trust_root_isolation(
    root: &Path,
    trust: &TrustMode,
    kind: StagingRootKind,
) -> Result<(), StagingError> {
    if matches!(kind, StagingRootKind::HostTest) {
        return Ok(());
    }
    match trust {
        TrustMode::Production => {
            if is_fixture_staging_namespace(root) {
                return Err(StagingError::CrossModeRoot(
                    "production trust cannot use the fixture staging root".into(),
                ));
            }
            if !is_production_staging_namespace(root) {
                return Err(StagingError::CrossModeRoot(
                    "production trust requires the production staging namespace".into(),
                ));
            }
        }
        TrustMode::Fixture { .. } => {
            if is_production_staging_namespace(root) {
                return Err(StagingError::CrossModeRoot(
                    "fixture trust cannot use the production staging root".into(),
                ));
            }
            if !is_fixture_staging_namespace(root) {
                return Err(StagingError::CrossModeRoot(
                    "fixture trust requires the fixture staging namespace".into(),
                ));
            }
        }
    }
    Ok(())
}

/// True when interrupted `.next`/`.prev` publication artifacts are present.
pub fn staging_has_interrupted_publication(root: &Path) -> bool {
    has_publish_leftovers(root)
}

fn has_publish_leftovers(root: &Path) -> bool {
    root.join(ASSETS_NEXT).exists()
        || root.join(RECORD_NEXT).exists()
        || root.join(ASSETS_PREV).exists()
        || root.join(RECORD_PREV).exists()
}

/// Published assets directory name under a staging root (`assets/`).
pub const STAGED_ASSETS_DIR: &str = ASSETS_DIR;

/// Conservatively reconcile interrupted `.next`/`.prev` publish artifacts.
///
/// Ambiguous mid-swap states refuse. Abandoned `.next` beside an intact published
/// tree is dropped. Restores `.prev` only when the live name is missing.
fn reconcile_interrupted_publication(root: &Path) -> Result<(), StagingError> {
    if !has_publish_leftovers(root) {
        return Ok(());
    }

    let next_assets = root.join(ASSETS_NEXT);
    let next_record = root.join(RECORD_NEXT);
    let prev_assets = root.join(ASSETS_PREV);
    let prev_record = root.join(RECORD_PREV);
    let assets = root.join(ASSETS_DIR);
    let record = root.join(RECORD_NAME);

    // Ambiguous: both next and prev present while live names are missing.
    if !assets.exists() && next_assets.exists() && prev_assets.exists() {
        return Err(StagingError::InterruptedPublication(
            "ambiguous assets.next/assets.prev swap".into(),
        ));
    }
    if !record.exists() && next_record.exists() && prev_record.exists() {
        return Err(StagingError::InterruptedPublication(
            "ambiguous STAGING_RECORD.next/.prev swap".into(),
        ));
    }

    // Abandoned new candidate beside intact published tree.
    if assets.is_dir() && next_assets.exists() {
        let _ = fs::remove_dir_all(&next_assets);
    }
    if record.is_file() && next_record.exists() {
        let _ = fs::remove_file(&next_record);
    }

    // Restore previous published names when the live name never landed.
    if !assets.exists() && prev_assets.exists() && !next_assets.exists() {
        fs::rename(&prev_assets, &assets)?;
    }
    if !record.exists() && prev_record.exists() && !next_record.exists() {
        fs::rename(&prev_record, &record)?;
    }

    // Incomplete new candidate with no live published tree: drop next.
    if next_assets.exists() && !assets.exists() {
        let _ = fs::remove_dir_all(&next_assets);
    }
    if next_record.exists() && !record.exists() {
        let _ = fs::remove_file(&next_record);
    }

    // Stale prev after successful live restore.
    if assets.exists() && prev_assets.exists() {
        let _ = fs::remove_dir_all(&prev_assets);
    }
    if record.exists() && prev_record.exists() {
        let _ = fs::remove_file(&prev_record);
    }

    if has_publish_leftovers(root) {
        return Err(StagingError::InterruptedPublication(
            "unresolved .next/.prev publication artifacts".into(),
        ));
    }
    Ok(())
}

pub fn validate_staging_root(root: &Path, kind: StagingRootKind) -> Result<(), StagingError> {
    let display = root.display().to_string();
    if display.is_empty() || display == "." || display == ".." {
        return Err(StagingError::UnsafeRoot(display));
    }
    if root.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    }) {
        return Err(StagingError::UnsafeRoot(display));
    }
    if matches!(
        root.components().next(),
        Some(std::path::Component::RootDir)
    ) && root.components().count() <= 1
    {
        return Err(StagingError::UnsafeRoot("filesystem root".into()));
    }

    for protected in PROTECTED_PREFIXES {
        let p = Path::new(protected);
        if root == p || root.starts_with(p) {
            return Err(StagingError::ProtectedPath(display));
        }
        // Refuse ancestors that would let cleanup erase a protected tree.
        if p.starts_with(root) {
            return Err(StagingError::ProtectedPath(format!(
                "{} is an ancestor of protected {protected}",
                root.display()
            )));
        }
    }

    match kind {
        StagingRootKind::Production => {
            // Unix absolute paths must be accepted even when the host OS is Windows
            // (device path strings are validated here for Kindle deployment).
            let unix_absolute = root
                .to_str()
                .map(|s| s.starts_with('/') && !s.starts_with("//"))
                .unwrap_or(false);
            if !root.is_absolute() && !unix_absolute {
                return Err(StagingError::UnsafeRoot(
                    "production staging root must be absolute".into(),
                ));
            }
            // Exact approved roots only — nested descendants are refused so a
            // symlink planted under the namespace cannot retarget cleanup/writes.
            if !is_dedicated_updater_namespace(root) {
                return Err(StagingError::UnsafeRoot(format!(
                    "{} is not an exact approved updater staging root",
                    root.display()
                )));
            }
        }
        StagingRootKind::HostTest => {
            // Explicit test-only escape hatch: relative or absolute temp trees that are
            // not protected prefixes. Still rejects state/ itself and protected paths.
            if root == Path::new(UPDATER_STATE_PARENT) {
                return Err(StagingError::UnsafeRoot(
                    "Ignite state parent is not a staging root".into(),
                ));
            }
        }
    }

    // Ancestor + leaf checks use symlink_metadata (no follow). Applies to both
    // Production and HostTest before any staging write, recovery, or cleanup.
    validate_existing_path_components(root)?;
    Ok(())
}

/// Cleanup may only remove files under an approved dedicated staging root.
pub fn cleanup_staging_root(root: &Path, kind: StagingRootKind) -> Result<(), StagingError> {
    validate_staging_root(root, kind)?;
    if !root.exists() {
        return Ok(());
    }
    // Final hard guard before destructive delete.
    if matches!(kind, StagingRootKind::Production) && !is_exact_approved_staging_root(root) {
        return Err(StagingError::UnsafeRoot(root.display().to_string()));
    }
    for protected in PROTECTED_PREFIXES {
        if root.starts_with(protected) || Path::new(protected).starts_with(root) {
            return Err(StagingError::ProtectedPath(protected.to_string()));
        }
    }
    // Re-check leaf metadata immediately before delete (still TOCTOU-limited).
    let meta = fs::symlink_metadata(root)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(StagingError::UnsafeRoot(format!(
            "cleanup refused non-directory or symlink root: {}",
            root.display()
        )));
    }
    fs::remove_dir_all(root)?;
    Ok(())
}

pub fn stage_verified_update(request: &StageRequest) -> Result<StageResult, StagingError> {
    validate_staging_root(&request.staging_root, request.root_kind)?;
    enforce_trust_root_isolation(&request.staging_root, &request.trust, request.root_kind)?;
    // Production mode must fail closed on an UNPROVISIONED pin before any network
    // acquisition or work-tree mutation.
    if matches!(request.trust, TrustMode::Production) {
        crate::release_manifest::official_release_public_key().map_err(|err| match err {
            crate::release_manifest::ReleaseVerifyError::OfficialKeyNotProvisioned => {
                StagingError::ProductionPinBlocks
            }
            other => StagingError::Trust(ProductionTrustError::from(other)),
        })?;
    }
    let names = ReleaseAssetNames::for_version(&request.version)?;

    // Fail-closed reconciliation of interrupted .next/.prev artifacts before any
    // success path. Ambiguous mid-swap states refuse; abandoned next beside an
    // intact published tree is dropped; .prev is restored only when live names
    // are missing.
    reconcile_interrupted_publication(&request.staging_root)?;

    // Distinguish absent vs valid vs corrupt records. Never `.ok()` away parse errors:
    // malformed/oversized/unreadable/unsafe records fail closed.
    let existing_record = inspect_existing_staging_record(&request.staging_root.join(RECORD_NAME))?;
    let assets_published = request.staging_root.join(ASSETS_DIR).is_dir();

    match existing_record {
        Some(hint) if hint.observations.staging_complete => {
            if hint.expected_tag != request.expected_tag {
                return Err(StagingError::ExistingCandidate(hint.expected_tag));
            }
            // Completed candidate: never silently reacquire.
            if !assets_published {
                return Err(StagingError::RevalidationFailed(
                    "completed staging record present but published assets directory missing"
                        .into(),
                ));
            }
            let result = revalidate_published_assets(request, &names)?;
            if has_publish_leftovers(&request.staging_root) {
                return Err(StagingError::InterruptedPublication(
                    "publish leftovers remain after revalidation".into(),
                ));
            }
            return Ok(result);
        }
        Some(_incomplete) => {
            // Valid incomplete observation: try revalidation, else crash-recovery acquire.
            if assets_published {
                match revalidate_published_assets(request, &names) {
                    Ok(result) => {
                        if has_publish_leftovers(&request.staging_root) {
                            return Err(StagingError::InterruptedPublication(
                                "publish leftovers remain after revalidation".into(),
                            ));
                        }
                        return Ok(result);
                    }
                    Err(StagingError::RevalidationFailed(_)) => {}
                    Err(err) => return Err(err),
                }
            }
        }
        None => {
            // No record: incomplete published tree may still revalidate; otherwise acquire.
            if assets_published {
                match revalidate_published_assets(request, &names) {
                    Ok(result) => {
                        if has_publish_leftovers(&request.staging_root) {
                            return Err(StagingError::InterruptedPublication(
                                "publish leftovers remain after revalidation".into(),
                            ));
                        }
                        return Ok(result);
                    }
                    Err(StagingError::RevalidationFailed(_)) => {}
                    Err(err) => return Err(err),
                }
            }
        }
    }

    let work = request.staging_root.join("work");
    let assets = work.join(ASSETS_DIR);
    if work.exists() {
        fs::remove_dir_all(&work)?;
    }
    // Defensive: ensure next/prev names are clear before writing a new candidate.
    // reconcile_interrupted_publication already ran; this covers races with a
    // concurrent crash marker drop during acquire.
    let _ = fs::remove_dir_all(request.staging_root.join(ASSETS_NEXT));
    let _ = fs::remove_file(request.staging_root.join(RECORD_NEXT));
    let _ = fs::remove_dir_all(request.staging_root.join(ASSETS_PREV));
    let _ = fs::remove_file(request.staging_root.join(RECORD_PREV));

    fs::create_dir_all(&assets)?;
    write_bytes(&work.join(INCOMPLETE_MARKER), b"download_incomplete\n")?;

    let budget = MAX_ASSET_BYTES.saturating_add(5 * MAX_METADATA_ASSET_BYTES);
    ensure_free_space(&request.staging_root, budget)?;

    let mut seen = std::collections::BTreeSet::new();
    for (key, name) in names.all() {
        if !seen.insert(name.to_string()) {
            return Err(StagingError::Acquisition(AcquisitionError::DuplicateAsset(
                name.to_string(),
            )));
        }
        if !REQUIRED_ASSET_KEYS.contains(&key) {
            return Err(StagingError::Acquisition(
                AcquisitionError::UnexpectedAsset(key.to_string()),
            ));
        }
        let dest = assets.join(name);
        let limit = ReleaseAssetNames::size_limit_for_key(key);
        request
            .source
            .fetch_named(key, name, &dest, limit)
            .map_err(StagingError::from)?;
        let meta = fs::symlink_metadata(&dest)?;
        if meta.file_type().is_symlink() {
            return Err(StagingError::UnsafeRoot(dest.display().to_string()));
        }
    }

    let mut observations = StagingObservations::incomplete();
    observations.download_complete = true;
    write_draft_record(&work, &request.expected_tag, &observations, &[])?;

    let (auth, mut observations) = authenticate_assets_dir(&assets, &names, request, observations)?;

    let scriptlet_path = assets.join(&names.scriptlet);
    {
        let mut file = File::open(&scriptlet_path)?;
        auth.release()
            .verify_installer(auth.release().installer_filename(), &mut file)
            .map_err(|e| StagingError::ScriptletVerify(e.to_string()))?;
    }
    observations.scriptlet_verified = true;

    if !observations.publication_claims_validated {
        return Err(StagingError::ClaimsNotValidated);
    }

    observations.staging_complete = true;
    if !observations.all_complete() {
        return Err(StagingError::IncompleteRefused(
            "refusing to publish incomplete observations".into(),
        ));
    }

    let mut trust_report = production_trust_report_lines(Ok(&auth));
    append_observation_lines(&mut trust_report, &observations);
    write_draft_record(&work, &request.expected_tag, &observations, &trust_report)?;

    publish_work_to_root(&request.staging_root, &work)?;

    if has_publish_leftovers(&request.staging_root) {
        return Err(StagingError::InterruptedPublication(
            "publish leftovers remain after publication".into(),
        ));
    }

    Ok(StageResult {
        observations,
        record_path: request.staging_root.join(RECORD_NAME),
        assets_dir: request.staging_root.join(ASSETS_DIR),
        trust_report,
        release_tag: Some(auth.release().tag().to_string()),
    })
}

fn revalidate_published_assets(
    request: &StageRequest,
    names: &ReleaseAssetNames,
) -> Result<StageResult, StagingError> {
    let assets = request.staging_root.join(ASSETS_DIR);
    if !assets.is_dir() {
        return Err(StagingError::RevalidationFailed(
            "published assets directory missing".into(),
        ));
    }
    for (_, name) in names.all() {
        let path = assets.join(name);
        if !path.is_file() {
            return Err(StagingError::RevalidationFailed(format!(
                "missing published asset {name}"
            )));
        }
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(StagingError::UnsafeRoot(path.display().to_string()));
        }
    }

    let observations = StagingObservations {
        download_complete: true,
        ..StagingObservations::incomplete()
    };
    let (auth, mut observations) = authenticate_assets_dir(&assets, names, request, observations)?;

    // Provenance must match the requested trust mode — fixture records never
    // satisfy production requests.
    match (&request.trust, auth.provenance()) {
        (TrustMode::Production, VerificationProvenance::OfficialProductionPin) => {}
        (TrustMode::Fixture { .. }, VerificationProvenance::TestOnlyFixtureKey) => {}
        (TrustMode::Production, VerificationProvenance::TestOnlyFixtureKey) => {
            return Err(StagingError::TrustModeMismatch);
        }
        _ => return Err(StagingError::TrustModeMismatch),
    }

    let scriptlet_path = assets.join(&names.scriptlet);
    {
        let mut file = File::open(&scriptlet_path)?;
        auth.release()
            .verify_installer(auth.release().installer_filename(), &mut file)
            .map_err(|e| StagingError::ScriptletVerify(e.to_string()))?;
    }
    observations.scriptlet_verified = true;
    if !observations.publication_claims_validated {
        return Err(StagingError::ClaimsNotValidated);
    }
    observations.staging_complete = true;
    if !observations.all_complete() {
        return Err(StagingError::IncompleteRefused(
            "revalidation observations incomplete".into(),
        ));
    }

    let mut trust_report = production_trust_report_lines(Ok(&auth));
    append_observation_lines(&mut trust_report, &observations);

    // Refresh the observation record only after successful revalidation. A forged
    // prior record cannot grant success without reaching this point.
    let work = request.staging_root.join("work");
    if work.exists() {
        fs::remove_dir_all(&work)?;
    }
    fs::create_dir_all(&work)?;
    write_draft_record(&work, &request.expected_tag, &observations, &trust_report)?;
    // Publish record only (assets already in place).
    let next = request.staging_root.join(RECORD_NEXT);
    fs::rename(work.join(RECORD_NAME), &next)?;
    let final_record = request.staging_root.join(RECORD_NAME);
    if final_record.exists() {
        let _ = fs::rename(&final_record, request.staging_root.join(RECORD_PREV));
    }
    fs::rename(&next, &final_record)?;
    let _ = fs::remove_file(request.staging_root.join(RECORD_PREV));
    let _ = fs::remove_dir_all(&work);

    Ok(StageResult {
        observations,
        record_path: final_record,
        assets_dir: assets,
        trust_report,
        release_tag: Some(auth.release().tag().to_string()),
    })
}

/// Read at most `max_bytes` from a regular file. The limit is enforced while
/// reading (`take(max+1)`), not only via a prior `metadata().len()` check.
fn read_bounded_file(path: &Path, max_bytes: u64, label: &str) -> Result<Vec<u8>, StagingError> {
    let meta = fs::symlink_metadata(path).map_err(|e| StagingError::Io(e.to_string()))?;
    if meta.file_type().is_symlink() {
        return Err(StagingError::UnsafeRoot(format!(
            "symlink refused while reading {label}: {}",
            path.display()
        )));
    }
    if !meta.is_file() {
        return Err(StagingError::Io(format!(
            "{label} is not a regular file: {}",
            path.display()
        )));
    }
    let file = File::open(path)?;
    let mut buf = Vec::new();
    let n = file
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)?;
    if n as u64 > max_bytes {
        return Err(StagingError::Acquisition(
            AcquisitionError::SizeLimitExceeded {
                name: label.to_string(),
                limit: max_bytes,
            },
        ));
    }
    Ok(buf)
}

fn authenticate_assets_dir(
    assets: &Path,
    names: &ReleaseAssetNames,
    request: &StageRequest,
    mut observations: StagingObservations,
) -> Result<(AuthenticatedPublicationChain, StagingObservations), StagingError> {
    let release_manifest = read_bounded_file(
        &assets.join(&names.release_manifest),
        MAX_CACHED_METADATA_BYTES,
        &names.release_manifest,
    )?;
    let release_signature = read_bounded_file(
        &assets.join(&names.release_signature),
        MAX_CACHED_METADATA_BYTES,
        &names.release_signature,
    )?;
    let publication_evidence = read_bounded_file(
        &assets.join(&names.publication_evidence),
        MAX_CACHED_METADATA_BYTES,
        &names.publication_evidence,
    )?;
    let publication_binding = read_bounded_file(
        &assets.join(&names.publication_binding),
        MAX_CACHED_METADATA_BYTES,
        &names.publication_binding,
    )?;
    let binding_signature = read_bounded_file(
        &assets.join(&names.binding_signature),
        MAX_CACHED_METADATA_BYTES,
        &names.binding_signature,
    )?;

    let auth = match &request.trust {
        TrustMode::Production => {
            match authenticate_official_publication_chain(
                &release_manifest,
                &release_signature,
                &publication_binding,
                &binding_signature,
                &publication_evidence,
                &request.expected_tag,
            ) {
                Ok(auth) => auth,
                Err(ProductionTrustError::OfficialKeyNotProvisioned) => {
                    return Err(StagingError::ProductionPinBlocks);
                }
                Err(err) => return Err(StagingError::Trust(err)),
            }
        }
        TrustMode::Fixture { public_key } => authenticate_publication_chain_with_key(
            &release_manifest,
            &release_signature,
            &publication_binding,
            &binding_signature,
            &publication_evidence,
            &request.expected_tag,
            public_key,
        )?,
    };

    observations.publisher_authenticated = true;
    observations.provenance = Some(auth.provenance());
    observations.publication_claims_validated =
        auth.evidence_claims() == EvidenceClaimStatus::ProductionClaimsValidated;
    Ok((auth, observations))
}

/// Publish a fully verified work tree without leaving an unverified candidate complete.
///
/// FSP/VFAT cannot guarantee multi-rename transactions. The staging record is published
/// last; a crash before that leaves assets.next / missing record, which revalidation
/// treats as incomplete.
fn publish_work_to_root(root: &Path, work: &Path) -> Result<(), StagingError> {
    let next_assets = root.join(ASSETS_NEXT);
    let next_record = root.join(RECORD_NEXT);
    let final_assets = root.join(ASSETS_DIR);
    let final_record = root.join(RECORD_NAME);
    let prev_assets = root.join(ASSETS_PREV);
    let prev_record = root.join(RECORD_PREV);

    let _ = fs::remove_dir_all(&next_assets);
    let _ = fs::remove_file(&next_record);
    let _ = fs::remove_dir_all(&prev_assets);
    let _ = fs::remove_file(&prev_record);

    fs::rename(work.join(ASSETS_DIR), &next_assets)?;
    fs::rename(work.join(RECORD_NAME), &next_record)?;

    if final_assets.exists() {
        fs::rename(&final_assets, &prev_assets)?;
    }
    fs::rename(&next_assets, &final_assets)?;

    if final_record.exists() {
        fs::rename(&final_record, &prev_record)?;
    }
    // Record published last — readiness gate.
    fs::rename(&next_record, &final_record)?;

    let _ = fs::remove_dir_all(&prev_assets);
    let _ = fs::remove_file(&prev_record);
    let _ = fs::remove_dir_all(work);
    let _ = fs::remove_file(root.join(INCOMPLETE_MARKER));
    Ok(())
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), StagingError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        let _ = f.sync_all();
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn write_draft_record(
    work: &Path,
    expected_tag: &str,
    observations: &StagingObservations,
    trust_report: &[String],
) -> Result<(), StagingError> {
    let provenance_line = match observations.provenance {
        Some(VerificationProvenance::OfficialProductionPin) => {
            "publisher_authentication=verified-under-production-pin"
        }
        Some(VerificationProvenance::TestOnlyFixtureKey) => {
            "publisher_authentication=verified-under-test-only-fixture-key"
        }
        None => "publisher_authentication=unavailable",
    };
    let mut lines = vec![
        RECORD_HEADER.to_string(),
        format!("schema={RECORD_SCHEMA}"),
        format!("expected_tag={expected_tag}"),
        format!(
            "download_complete={}",
            bool_word(observations.download_complete)
        ),
        format!(
            "publisher_authenticated={}",
            bool_word(observations.publisher_authenticated)
        ),
        format!(
            "publication_claims_validated={}",
            bool_word(observations.publication_claims_validated)
        ),
        format!(
            "scriptlet_verified={}",
            bool_word(observations.scriptlet_verified)
        ),
        format!(
            "staging_complete={}",
            bool_word(observations.staging_complete)
        ),
        provenance_line.to_string(),
        "installation_binding=unverified".to_string(),
        "execution_binding=unverified".to_string(),
        "first_launch_admission=unavailable".to_string(),
        "data_compatibility=unknown".to_string(),
        "promotion_authorized=false".to_string(),
        "authorization=none".to_string(),
    ];
    for line in trust_report {
        let key = line.split_once('=').map(|(k, _)| k);
        let reserved = matches!(
            key,
            Some(
                "schema"
                    | "expected_tag"
                    | "download_complete"
                    | "publisher_authenticated"
                    | "publication_claims_validated"
                    | "scriptlet_verified"
                    | "staging_complete"
                    | "publisher_authentication"
                    | "installation_binding"
                    | "execution_binding"
                    | "first_launch_admission"
                    | "data_compatibility"
                    | "promotion_authorized"
                    | "authorization"
            )
        );
        if reserved || lines.iter().any(|existing| existing == line) {
            continue;
        }
        lines.push(line.clone());
    }
    let mut body = lines.join("\n");
    body.push('\n');
    write_bytes(&work.join(RECORD_NAME), body.as_bytes())
}

fn append_observation_lines(report: &mut Vec<String>, observations: &StagingObservations) {
    report.push(format!(
        "download_complete={}",
        bool_word(observations.download_complete)
    ));
    report.push(format!(
        "publisher_authenticated={}",
        bool_word(observations.publisher_authenticated)
    ));
    report.push(format!(
        "publication_claims_validated={}",
        bool_word(observations.publication_claims_validated)
    ));
    report.push(format!(
        "scriptlet_verified={}",
        bool_word(observations.scriptlet_verified)
    ));
    report.push(format!(
        "staging_complete={}",
        bool_word(observations.staging_complete)
    ));
}

fn bool_word(v: bool) -> &'static str {
    if v {
        "true"
    } else {
        "false"
    }
}

fn parse_bool_token(token: &str) -> Result<bool, StagingError> {
    match token {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(StagingError::RecordInvalid(format!(
            "noncanonical boolean {token}"
        ))),
    }
}

#[derive(Clone, Debug)]
struct ParsedRecord {
    expected_tag: String,
    observations: StagingObservations,
    lines: Vec<String>,
}

/// Probe for an existing staging record without discarding validation failures.
///
/// - `Ok(None)` — path does not exist (`NotFound` only)
/// - `Ok(Some(_))` — regular file parsed successfully
/// - `Err(_)` — present but malformed, oversized, unreadable, symlink, or non-file
fn inspect_existing_staging_record(path: &Path) -> Result<Option<ParsedRecord>, StagingError> {
    match fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(StagingError::Io(err.to_string())),
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(StagingError::RecordInvalid("symlink record".into()));
            }
            if !meta.is_file() {
                return Err(StagingError::RecordInvalid(
                    "staging record is not a regular file".into(),
                ));
            }
            Ok(Some(parse_strict_record(path)?))
        }
    }
}

/// Strict observation-record parser. Never treats the record as a trust credential.
fn parse_strict_record(path: &Path) -> Result<ParsedRecord, StagingError> {
    let meta = fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            StagingError::RecordInvalid("missing record".into())
        } else {
            StagingError::Io(e.to_string())
        }
    })?;
    if meta.file_type().is_symlink() {
        return Err(StagingError::RecordInvalid("symlink record".into()));
    }
    if !meta.is_file() {
        return Err(StagingError::RecordInvalid(
            "staging record is not a regular file".into(),
        ));
    }
    let bytes = match read_bounded_file(path, MAX_STAGING_RECORD_BYTES, RECORD_NAME) {
        Ok(b) => b,
        Err(StagingError::Acquisition(AcquisitionError::SizeLimitExceeded { .. })) => {
            return Err(StagingError::RecordInvalid(format!(
                "staging record exceeds {MAX_STAGING_RECORD_BYTES} byte limit"
            )));
        }
        Err(err) => return Err(err),
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| StagingError::RecordInvalid("staging record is not valid UTF-8".into()))?;
    if !text.ends_with('\n') || text.contains('\r') {
        return Err(StagingError::RecordInvalid(
            "record must be canonical LF text".into(),
        ));
    }
    let lines: Vec<String> = text[..text.len() - 1]
        .split('\n')
        .map(str::to_string)
        .collect();
    if lines.len() < 15 {
        return Err(StagingError::RecordInvalid("too few fields".into()));
    }
    if lines[0] != RECORD_FIXED_PREFIX[0] || lines[1] != RECORD_FIXED_PREFIX[1] {
        return Err(StagingError::RecordInvalid("header/schema mismatch".into()));
    }
    let expected_tag = lines[2]
        .strip_prefix("expected_tag=")
        .ok_or_else(|| StagingError::RecordInvalid("missing expected_tag".into()))?
        .to_string();
    if !expected_tag.starts_with('v') || expected_tag.len() < 2 {
        return Err(StagingError::RecordInvalid("invalid expected_tag".into()));
    }

    let mut seen = std::collections::BTreeSet::new();
    for line in &lines {
        let key = line
            .split_once('=')
            .map(|(k, _)| k)
            .unwrap_or(line.as_str());
        if matches!(
            key,
            "expected_tag"
                | "download_complete"
                | "publisher_authenticated"
                | "publication_claims_validated"
                | "scriptlet_verified"
                | "staging_complete"
                | "publisher_authentication"
                | "installation_binding"
                | "execution_binding"
                | "first_launch_admission"
                | "data_compatibility"
                | "promotion_authorized"
                | "authorization"
                | "schema"
        ) {
            if !seen.insert(key.to_string()) {
                return Err(StagingError::RecordInvalid(format!(
                    "duplicate field {key}"
                )));
            }
        }
    }

    let download_complete = parse_bool_token(
        lines[3]
            .strip_prefix("download_complete=")
            .ok_or_else(|| StagingError::RecordInvalid("download_complete".into()))?,
    )?;
    let publisher_authenticated = parse_bool_token(
        lines[4]
            .strip_prefix("publisher_authenticated=")
            .ok_or_else(|| StagingError::RecordInvalid("publisher_authenticated".into()))?,
    )?;
    let publication_claims_validated = parse_bool_token(
        lines[5]
            .strip_prefix("publication_claims_validated=")
            .ok_or_else(|| StagingError::RecordInvalid("publication_claims_validated".into()))?,
    )?;
    let scriptlet_verified = parse_bool_token(
        lines[6]
            .strip_prefix("scriptlet_verified=")
            .ok_or_else(|| StagingError::RecordInvalid("scriptlet_verified".into()))?,
    )?;
    let staging_complete = parse_bool_token(
        lines[7]
            .strip_prefix("staging_complete=")
            .ok_or_else(|| StagingError::RecordInvalid("staging_complete".into()))?,
    )?;
    let provenance = match lines[8].as_str() {
        "publisher_authentication=verified-under-production-pin" => {
            Some(VerificationProvenance::OfficialProductionPin)
        }
        "publisher_authentication=verified-under-test-only-fixture-key" => {
            Some(VerificationProvenance::TestOnlyFixtureKey)
        }
        "publisher_authentication=unavailable" => None,
        other => {
            return Err(StagingError::RecordInvalid(format!(
                "unknown provenance {other}"
            )))
        }
    };
    for (idx, expected) in [
        (9, "installation_binding=unverified"),
        (10, "execution_binding=unverified"),
        (11, "first_launch_admission=unavailable"),
        (12, "data_compatibility=unknown"),
        (13, "promotion_authorized=false"),
        (14, "authorization=none"),
    ] {
        if lines.get(idx).map(String::as_str) != Some(expected) {
            return Err(StagingError::RecordInvalid(format!(
                "missing fixed field {expected}"
            )));
        }
    }

    let observations = StagingObservations {
        download_complete,
        publisher_authenticated,
        publication_claims_validated,
        scriptlet_verified,
        staging_complete,
        provenance,
    };

    // Consistency: a "complete" observation set must have all five true and a
    // verified provenance line. This still does not establish authenticity.
    if staging_complete {
        if !observations.all_complete() {
            return Err(StagingError::RecordInvalid(
                "staging_complete inconsistent with other flags".into(),
            ));
        }
        if provenance.is_none() {
            return Err(StagingError::RecordInvalid(
                "complete record requires publisher provenance".into(),
            ));
        }
    }

    Ok(ParsedRecord {
        expected_tag,
        observations,
        lines,
    })
}

/// Completed staging observation returned by the strict 3i record parser.
///
/// This is never an authorization credential. Callers that need authenticity
/// must independently re-verify signed assets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedStagingObservation {
    pub expected_tag: String,
    pub observations: StagingObservations,
}

/// Load a published staging *observation* hint. Incomplete/missing records refuse.
///
/// Callers that need authenticity must re-verify signed assets (see
/// [`stage_verified_update`]).
pub fn load_staging_record(
    root: &Path,
    kind: StagingRootKind,
) -> Result<StagingObservations, StagingError> {
    Ok(require_completed_staging_observation(root, kind)?.observations)
}

/// Strictly require an existing, canonical, fully completed staging observation.
///
/// Reuses the 3i parser. Distinguishes absent (`NotFound`) from malformed,
/// oversized, incomplete, symlinked, or otherwise invalid records.
pub fn require_completed_staging_observation(
    root: &Path,
    kind: StagingRootKind,
) -> Result<CompletedStagingObservation, StagingError> {
    validate_staging_root(root, kind)?;
    if root.join("work").join(INCOMPLETE_MARKER).exists() {
        return Err(StagingError::IncompleteRefused(
            "work directory still marked incomplete".into(),
        ));
    }
    if has_publish_leftovers(root) {
        return Err(StagingError::InterruptedPublication(
            "interrupted publish leftovers present".into(),
        ));
    }
    match inspect_existing_staging_record(&root.join(RECORD_NAME))? {
        None => Err(StagingError::RecordInvalid("staging record absent".into())),
        Some(parsed) => {
            if !parsed.observations.all_complete() {
                return Err(StagingError::IncompleteRefused(
                    "staging record is not fully verified".into(),
                ));
            }
            Ok(CompletedStagingObservation {
                expected_tag: parsed.expected_tag,
                observations: parsed.observations,
            })
        }
    }
}

pub fn emit_stage_report(result: &StageResult) {
    println!("{RECORD_HEADER}");
    println!("schema={RECORD_SCHEMA}");
    if let Some(tag) = &result.release_tag {
        println!("release_tag={tag}");
    }
    println!(
        "download_complete={}",
        bool_word(result.observations.download_complete)
    );
    println!(
        "publisher_authenticated={}",
        bool_word(result.observations.publisher_authenticated)
    );
    println!(
        "publication_claims_validated={}",
        bool_word(result.observations.publication_claims_validated)
    );
    println!(
        "scriptlet_verified={}",
        bool_word(result.observations.scriptlet_verified)
    );
    println!(
        "staging_complete={}",
        bool_word(result.observations.staging_complete)
    );
    println!("record={}", result.record_path.display());
    println!("assets={}", result.assets_dir.display());
    println!("installation_authorization=not-granted");
    for line in &result.trust_report {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publication_evidence::build_minimal_production_evidence;
    use crate::publication_trust_boundary::{
        BINDING_HEADER, BINDING_SCHEMA, BINDING_SCOPE, BINDING_SIGNATURE_SCHEMA,
    };
    use crate::release_manifest::{encode_hex, MANIFEST_SCHEMA, SIGNATURE_SCHEMA};
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};

    const VERSION: &str = "3.3.1";
    const TAG: &str = "v3.3.1";
    const INSTALLER: &[u8] = b"#!/bin/sh\necho fixture-scriptlet\n";

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[11_u8; 32])
    }

    fn sign_envelope(message: &[u8], schema: &str, key: &SigningKey) -> Vec<u8> {
        let signature = key.sign(message);
        format!(
            "schema={schema}\nsignature={}\n",
            encode_hex(signature.to_bytes().as_ref())
        )
        .into_bytes()
    }

    fn production_format_evidence() -> Vec<u8> {
        let digest = encode_hex(&Sha256::digest(INSTALLER));
        build_minimal_production_evidence(
            VERSION,
            INSTALLER.len() as u64,
            &digest,
            &"ab".repeat(32),
        )
        .expect("minimal production evidence")
    }

    fn write_fixture_dir(dir: &Path, key: &SigningKey) {
        let evidence = production_format_evidence();
        let digest = Sha256::digest(INSTALLER);
        let release = format!(
            "schema={MANIFEST_SCHEMA}\nversion={VERSION}\ntag={TAG}\nartifact=Install Ignite v{VERSION}.sh\nbytes={}\nsha256={}\n",
            INSTALLER.len(),
            encode_hex(&digest),
        )
        .into_bytes();
        let binding = format!(
            "{BINDING_HEADER}\nschema={BINDING_SCHEMA}\nrelease_tag={TAG}\nrelease_version={VERSION}\nrelease_manifest_sha256={}\nscriptlet_sha256={}\npublication_evidence_sha256={}\npublisher_authentication_scope={BINDING_SCOPE}\n",
            encode_hex(&Sha256::digest(&release)),
            encode_hex(&digest),
            encode_hex(&Sha256::digest(&evidence)),
        )
        .into_bytes();
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("Install Ignite v{VERSION}.sh")), INSTALLER).unwrap();
        fs::write(
            dir.join(format!("ignite-kindle-v{VERSION}.manifest")),
            &release,
        )
        .unwrap();
        fs::write(
            dir.join(format!("ignite-kindle-v{VERSION}.manifest.sig")),
            sign_envelope(&release, SIGNATURE_SCHEMA, key),
        )
        .unwrap();
        fs::write(
            dir.join(format!("ignite-kindle-v{VERSION}.publication-evidence")),
            &evidence,
        )
        .unwrap();
        fs::write(
            dir.join(format!("ignite-kindle-v{VERSION}.publication-binding")),
            &binding,
        )
        .unwrap();
        fs::write(
            dir.join(format!("ignite-kindle-v{VERSION}.publication-binding.sig")),
            sign_envelope(&binding, BINDING_SIGNATURE_SCHEMA, key),
        )
        .unwrap();
        fs::write(
            dir.join("public_key.hex"),
            encode_hex(key.verifying_key().as_bytes()),
        )
        .unwrap();
    }

    fn temp_pair(label: &str) -> (PathBuf, PathBuf) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("ignite-3i-{label}-{stamp}"));
        let src = base.join("src");
        let staging = base.join("staging");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&src).unwrap();
        (src, staging)
    }

    fn fixture_request(src: &Path, staging: &Path, key: &SigningKey) -> StageRequest {
        StageRequest {
            expected_tag: TAG.into(),
            version: VERSION.into(),
            staging_root: staging.to_path_buf(),
            source: AssetSource::LocalDirectory {
                root: src.to_path_buf(),
            },
            trust: TrustMode::Fixture {
                public_key: key.verifying_key().to_bytes(),
            },
            root_kind: StagingRootKind::HostTest,
        }
    }

    #[test]
    fn fixture_staging_verifies_with_validated_claims_and_is_not_production_authority() {
        let key = signing_key();
        let (src, staging) = temp_pair("ok");
        write_fixture_dir(&src, &key);
        let req = fixture_request(&src, &staging, &key);
        let result = stage_verified_update(&req).expect("fixture stage");
        assert!(result.observations.all_complete());
        assert!(result.observations.publication_claims_validated);
        assert_eq!(
            result.observations.provenance,
            Some(VerificationProvenance::TestOnlyFixtureKey)
        );
        assert!(result
            .trust_report
            .iter()
            .any(|l| l == "publisher_authentication=verified-under-test-only-fixture-key"));
        assert!(result
            .trust_report
            .iter()
            .any(|l| l == "promotion_authorized=false"));
        let loaded = load_staging_record(&staging, StagingRootKind::HostTest).expect("complete");
        assert!(loaded.all_complete());

        // Repeated valid fixture staging with the same key.
        let again = stage_verified_update(&req).expect("idempotent");
        assert!(again.observations.all_complete());

        cleanup_staging_root(&staging, StagingRootKind::HostTest).unwrap();
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn fixture_success_then_production_request_refuses() {
        let key = signing_key();
        let (src, staging) = temp_pair("fix-after-fixture");
        write_fixture_dir(&src, &key);
        let fixture = fixture_request(&src, &staging, &key);
        stage_verified_update(&fixture).expect("fixture");
        let production = StageRequest {
            expected_tag: TAG.into(),
            version: VERSION.into(),
            staging_root: staging.clone(),
            source: AssetSource::LocalDirectory { root: src.clone() },
            trust: TrustMode::Production,
            root_kind: StagingRootKind::HostTest,
        };
        let err = stage_verified_update(&production).expect_err("production blocked");
        assert!(
            matches!(
                err,
                StagingError::ProductionPinBlocks
                    | StagingError::TrustModeMismatch
                    | StagingError::Trust(_)
            ),
            "unexpected {err}"
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn fixture_success_then_different_fixture_key_fails() {
        let key = signing_key();
        let other = SigningKey::from_bytes(&[12_u8; 32]);
        let (src, staging) = temp_pair("other-key");
        write_fixture_dir(&src, &key);
        stage_verified_update(&fixture_request(&src, &staging, &key)).unwrap();
        let mut req = fixture_request(&src, &staging, &other);
        req.trust = TrustMode::Fixture {
            public_key: other.verifying_key().to_bytes(),
        };
        assert!(stage_verified_update(&req).is_err());
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn scriptlet_mutation_after_staging_fails_revalidation() {
        let key = signing_key();
        let (src, staging) = temp_pair("mutate-scriptlet");
        write_fixture_dir(&src, &key);
        let req = fixture_request(&src, &staging, &key);
        stage_verified_update(&req).unwrap();
        fs::write(
            staging
                .join(ASSETS_DIR)
                .join(format!("Install Ignite v{VERSION}.sh")),
            b"mutated-after-stage",
        )
        .unwrap();
        assert!(stage_verified_update(&req).is_err());
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn missing_asset_after_staging_fails_revalidation() {
        let key = signing_key();
        let (src, staging) = temp_pair("missing-asset");
        write_fixture_dir(&src, &key);
        let req = fixture_request(&src, &staging, &key);
        stage_verified_update(&req).unwrap();
        fs::remove_file(
            staging
                .join(ASSETS_DIR)
                .join(format!("ignite-kindle-v{VERSION}.manifest.sig")),
        )
        .unwrap();
        // Fixture source remains intact. A completed candidate must fail closed
        // rather than silently reacquire from source.
        assert!(src
            .join(format!("ignite-kindle-v{VERSION}.manifest.sig"))
            .is_file());
        let err = stage_verified_update(&req).expect_err("must not reacquire");
        assert!(
            matches!(err, StagingError::RevalidationFailed(_)),
            "unexpected {err}"
        );
        // Published tree must not have been reconstructed from source.
        assert!(!staging
            .join(ASSETS_DIR)
            .join(format!("ignite-kindle-v{VERSION}.manifest.sig"))
            .exists());
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn oversized_cached_metadata_is_refused() {
        let key = signing_key();
        let (src, staging) = temp_pair("oversize-meta");
        write_fixture_dir(&src, &key);
        let req = fixture_request(&src, &staging, &key);
        stage_verified_update(&req).unwrap();
        let evidence = staging
            .join(ASSETS_DIR)
            .join(format!("ignite-kindle-v{VERSION}.publication-evidence"));
        let oversized = vec![b'X'; (MAX_CACHED_METADATA_BYTES as usize) + 1];
        fs::write(&evidence, &oversized).unwrap();
        let err = stage_verified_update(&req).expect_err("oversized metadata");
        assert!(
            matches!(
                err,
                StagingError::Acquisition(AcquisitionError::SizeLimitExceeded { .. })
            ),
            "unexpected {err}"
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    fn oversized_record_bytes() -> Vec<u8> {
        let mut huge = format!(
            "{RECORD_HEADER}\nschema=1\nexpected_tag={TAG}\ndownload_complete=true\npublisher_authenticated=true\npublication_claims_validated=true\nscriptlet_verified=true\nstaging_complete=true\npublisher_authentication=verified-under-test-only-fixture-key\ninstallation_binding=unverified\nexecution_binding=unverified\nfirst_launch_admission=unavailable\ndata_compatibility=unknown\npromotion_authorized=false\nauthorization=none\n"
        );
        while huge.len() <= MAX_STAGING_RECORD_BYTES as usize {
            huge.push_str("pad=xxxxxxxx\n");
        }
        huge.into_bytes()
    }

    #[test]
    fn oversized_staging_record_is_refused() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ignite-3i-oversize-record-{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(RECORD_NAME), oversized_record_bytes()).unwrap();
        assert!(matches!(
            parse_strict_record(&dir.join(RECORD_NAME)),
            Err(StagingError::RecordInvalid(_))
        ));
        assert!(matches!(
            load_staging_record(&dir, StagingRootKind::HostTest),
            Err(StagingError::RecordInvalid(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_record_with_missing_asset_fails_closed_without_reacquire() {
        let key = signing_key();
        let (src, staging) = temp_pair("oversize-missing");
        write_fixture_dir(&src, &key);
        let req = fixture_request(&src, &staging, &key);
        stage_verified_update(&req).unwrap();

        let sig_name = format!("ignite-kindle-v{VERSION}.manifest.sig");
        fs::write(staging.join(RECORD_NAME), oversized_record_bytes()).unwrap();
        fs::remove_file(staging.join(ASSETS_DIR).join(&sig_name)).unwrap();
        assert!(src.join(&sig_name).is_file());

        let before_record = fs::read(staging.join(RECORD_NAME)).unwrap();
        let err = stage_verified_update(&req).expect_err("must refuse corrupt record");
        assert!(
            matches!(err, StagingError::RecordInvalid(_)),
            "unexpected {err}"
        );
        assert_eq!(
            fs::read(staging.join(RECORD_NAME)).unwrap(),
            before_record,
            "corrupt record must not be rewritten"
        );
        assert!(
            !staging.join(ASSETS_DIR).join(&sig_name).exists(),
            "must not reacquire missing asset from intact source"
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn malformed_existing_record_cannot_be_silently_rewritten() {
        let key = signing_key();
        let (src, staging) = temp_pair("malformed-record");
        write_fixture_dir(&src, &key);
        let req = fixture_request(&src, &staging, &key);
        stage_verified_update(&req).unwrap();

        let malformed = b"NOT_A_STAGING_RECORD\nstaging_complete=true\n";
        fs::write(staging.join(RECORD_NAME), malformed).unwrap();
        let err = stage_verified_update(&req).expect_err("malformed record");
        assert!(
            matches!(err, StagingError::RecordInvalid(_)),
            "unexpected {err}"
        );
        assert_eq!(fs::read(staging.join(RECORD_NAME)).unwrap(), malformed);
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn absent_record_still_allows_first_time_acquisition() {
        let key = signing_key();
        let (src, staging) = temp_pair("absent-record");
        write_fixture_dir(&src, &key);
        assert!(!staging.join(RECORD_NAME).exists());
        let result =
            stage_verified_update(&fixture_request(&src, &staging, &key)).expect("first acquire");
        assert!(result.observations.all_complete());
        assert!(staging.join(RECORD_NAME).is_file());
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn forged_staging_record_does_not_grant_success() {
        let key = signing_key();
        let (src, staging) = temp_pair("forged-record");
        write_fixture_dir(&src, &key);
        fs::create_dir_all(&staging).unwrap();
        // Fabricated complete record without assets.
        let forged = format!(
            "{RECORD_HEADER}\nschema=1\nexpected_tag={TAG}\ndownload_complete=true\npublisher_authenticated=true\npublication_claims_validated=true\nscriptlet_verified=true\nstaging_complete=true\npublisher_authentication=verified-under-test-only-fixture-key\ninstallation_binding=unverified\nexecution_binding=unverified\nfirst_launch_admission=unavailable\ndata_compatibility=unknown\npromotion_authorized=false\nauthorization=none\n"
        );
        fs::write(staging.join(RECORD_NAME), forged.as_bytes()).unwrap();
        // Observation loader may accept the schema, but stage must revalidate assets.
        let req = fixture_request(&src, &staging, &key);
        // No assets dir → falls through to acquire from source and succeeds for real.
        // To prove forged record alone is insufficient, stage with empty source:
        let empty = staging.parent().unwrap().join("empty-src");
        fs::create_dir_all(&empty).unwrap();
        let mut bad = req.clone();
        bad.source = AssetSource::LocalDirectory { root: empty };
        assert!(stage_verified_update(&bad).is_err());
        // And load_staging_record must not be treated as authentication — document that
        // callers must revalidate. Here we assert stage with no assets + empty source fails
        // even though a forged complete record exists.
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn forged_record_without_assets_is_not_authentic() {
        let (src, staging) = temp_pair("forged-only");
        fs::create_dir_all(&staging).unwrap();
        let forged = format!(
            "{RECORD_HEADER}\nschema=1\nexpected_tag={TAG}\ndownload_complete=true\npublisher_authenticated=true\npublication_claims_validated=true\nscriptlet_verified=true\nstaging_complete=true\npublisher_authentication=verified-under-production-pin\ninstallation_binding=unverified\nexecution_binding=unverified\nfirst_launch_admission=unavailable\ndata_compatibility=unknown\npromotion_authorized=false\nauthorization=none\n"
        );
        fs::write(staging.join(RECORD_NAME), forged.as_bytes()).unwrap();
        // Hint may parse, but authenticity requires assets — stage production refuses.
        let req = StageRequest {
            expected_tag: TAG.into(),
            version: VERSION.into(),
            staging_root: staging.clone(),
            source: AssetSource::LocalDirectory { root: src.clone() },
            trust: TrustMode::Production,
            root_kind: StagingRootKind::HostTest,
        };
        let err = stage_verified_update(&req).expect_err("forged record insufficient");
        assert!(matches!(
            err,
            StagingError::Acquisition(_)
                | StagingError::ProductionPinBlocks
                | StagingError::FixtureKeyRequired
                | StagingError::RevalidationFailed(_)
                | StagingError::Trust(_)
        ));
        let _ = fs::remove_dir_all(staging.parent().unwrap());
    }

    #[test]
    fn strict_record_rejects_duplicates_and_bad_booleans() {
        let dir = std::env::temp_dir().join(format!(
            "ignite-3i-record-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let dup = format!(
            "{RECORD_HEADER}\nschema=1\nexpected_tag={TAG}\ndownload_complete=true\npublisher_authenticated=true\npublication_claims_validated=true\nscriptlet_verified=true\nstaging_complete=true\npublisher_authentication=verified-under-test-only-fixture-key\ninstallation_binding=unverified\nexecution_binding=unverified\nfirst_launch_admission=unavailable\ndata_compatibility=unknown\npromotion_authorized=false\nauthorization=none\nstaging_complete=false\n"
        );
        fs::write(dir.join(RECORD_NAME), dup).unwrap();
        assert!(parse_strict_record(&dir.join(RECORD_NAME)).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn placeholder_evidence_cannot_complete_staging() {
        let key = signing_key();
        let (src, staging) = temp_pair("placeholder");
        // Build release/binding around placeholder evidence (claims unverified).
        let evidence = format!(
            "{}\nschema=1\npublisher_authentication_scope=production-pin-required\n",
            crate::publication_trust_boundary::PLACEHOLDER_EVIDENCE_HEADER
        )
        .into_bytes();
        let digest = Sha256::digest(INSTALLER);
        let release = format!(
            "schema={MANIFEST_SCHEMA}\nversion={VERSION}\ntag={TAG}\nartifact=Install Ignite v{VERSION}.sh\nbytes={}\nsha256={}\n",
            INSTALLER.len(),
            encode_hex(&digest),
        )
        .into_bytes();
        let binding = format!(
            "{BINDING_HEADER}\nschema={BINDING_SCHEMA}\nrelease_tag={TAG}\nrelease_version={VERSION}\nrelease_manifest_sha256={}\nscriptlet_sha256={}\npublication_evidence_sha256={}\npublisher_authentication_scope={BINDING_SCOPE}\n",
            encode_hex(&Sha256::digest(&release)),
            encode_hex(&digest),
            encode_hex(&Sha256::digest(&evidence)),
        )
        .into_bytes();
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join(format!("Install Ignite v{VERSION}.sh")), INSTALLER).unwrap();
        fs::write(
            src.join(format!("ignite-kindle-v{VERSION}.manifest")),
            &release,
        )
        .unwrap();
        fs::write(
            src.join(format!("ignite-kindle-v{VERSION}.manifest.sig")),
            sign_envelope(&release, SIGNATURE_SCHEMA, &key),
        )
        .unwrap();
        fs::write(
            src.join(format!("ignite-kindle-v{VERSION}.publication-evidence")),
            &evidence,
        )
        .unwrap();
        fs::write(
            src.join(format!("ignite-kindle-v{VERSION}.publication-binding")),
            &binding,
        )
        .unwrap();
        fs::write(
            src.join(format!("ignite-kindle-v{VERSION}.publication-binding.sig")),
            sign_envelope(&binding, BINDING_SIGNATURE_SCHEMA, &key),
        )
        .unwrap();
        let err = stage_verified_update(&fixture_request(&src, &staging, &key))
            .expect_err("placeholder cannot complete");
        assert!(matches!(err, StagingError::ClaimsNotValidated));
        assert!(load_staging_record(&staging, StagingRootKind::HostTest).is_err());
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn corrupted_scriptlet_fails_closed_without_complete_record() {
        let key = signing_key();
        let (src, staging) = temp_pair("bad-scriptlet");
        write_fixture_dir(&src, &key);
        fs::write(
            src.join(format!("Install Ignite v{VERSION}.sh")),
            b"tampered",
        )
        .unwrap();
        assert!(stage_verified_update(&fixture_request(&src, &staging, &key)).is_err());
        assert!(load_staging_record(&staging, StagingRootKind::HostTest).is_err());
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn production_mode_refuses_unprovisioned_pin() {
        let key = signing_key();
        let (src, staging) = temp_pair("prod-pin");
        write_fixture_dir(&src, &key);
        let req = StageRequest {
            expected_tag: TAG.into(),
            version: VERSION.into(),
            staging_root: staging.clone(),
            source: AssetSource::LocalDirectory { root: src.clone() },
            trust: TrustMode::Production,
            root_kind: StagingRootKind::HostTest,
        };
        let err = stage_verified_update(&req).expect_err("production refused");
        assert!(
            matches!(
                err,
                StagingError::ProductionPinBlocks | StagingError::Trust(_)
            ),
            "unexpected {err}"
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn local_fixture_without_fixture_key_refused_by_parser() {
        let args = vec![
            COMMAND.into(),
            "--expected-tag".into(),
            TAG.into(),
            "--fixture-assets-dir".into(),
            "/tmp/x".into(),
        ];
        let err = parse_command(&args).expect_err("need fixture key");
        assert!(matches!(err, StagingError::FixtureKeyRequired));
    }

    #[test]
    fn protected_paths_and_state_parent_rejected() {
        assert!(validate_staging_root(
            Path::new("/mnt/us/scribe-data"),
            StagingRootKind::Production
        )
        .is_err());
        assert!(
            validate_staging_root(Path::new("/mnt/us/documents"), StagingRootKind::Production)
                .is_err()
        );
        assert!(validate_staging_root(
            Path::new("/mnt/us/scribe-app/bin"),
            StagingRootKind::Production
        )
        .is_err());
        assert!(validate_staging_root(
            Path::new(UPDATER_STATE_PARENT),
            StagingRootKind::Production
        )
        .is_err());
        assert!(validate_staging_root(Path::new("."), StagingRootKind::Production).is_err());
        assert!(
            validate_staging_root(Path::new("relative/staging"), StagingRootKind::Production)
                .is_err()
        );
        validate_staging_root(Path::new(DEFAULT_STAGING_ROOT), StagingRootKind::Production)
            .expect("default production staging root must be accepted");
        validate_staging_root(Path::new(FIXTURE_STAGING_ROOT), StagingRootKind::Production)
            .expect("fixture production staging root must be accepted");
        // Nested descendants under the approved roots are refused.
        assert!(validate_staging_root(
            Path::new(&format!("{DEFAULT_STAGING_ROOT}/nested")),
            StagingRootKind::Production
        )
        .is_err());
        assert!(validate_staging_root(
            Path::new(&format!("{FIXTURE_STAGING_ROOT}/staging")),
            StagingRootKind::Production
        )
        .is_err());
    }

    fn try_symlink_dir(original: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(original, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(original, link).is_ok()
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (original, link);
            false
        }
    }

    #[test]
    fn staging_root_path_safety_refuses_symlinks_and_nested_roots() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("ignite-3i-path-safety-{stamp}"));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        let real_parent = base.join("real-parent");
        let outside = base.join("outside-sentinel");
        fs::create_dir_all(&real_parent).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("KEEP"), b"alive").unwrap();

        // Valid HostTest root under real directories.
        let ok_root = real_parent.join("staging");
        fs::create_dir_all(&ok_root).unwrap();
        validate_staging_root(&ok_root, StagingRootKind::HostTest).expect("plain dir ok");

        // Symlinked staging root refused.
        let link_root = real_parent.join("staging-link");
        if try_symlink_dir(&ok_root, &link_root) {
            let err = validate_staging_root(&link_root, StagingRootKind::HostTest)
                .expect_err("symlink root");
            assert!(
                matches!(err, StagingError::UnsafeRoot(_)),
                "unexpected {err}"
            );
        }

        // Symlinked parent refused for a child staging path.
        let linked_parent = base.join("linked-parent");
        if try_symlink_dir(&real_parent, &linked_parent) {
            let under_link = linked_parent.join("staging-child");
            // Child may not exist yet; ancestor symlink must still be caught.
            let err = validate_staging_root(&under_link, StagingRootKind::HostTest)
                .expect_err("symlink parent");
            assert!(
                matches!(err, StagingError::UnsafeRoot(_)),
                "unexpected {err}"
            );

            // Protected material reachable only via the symlink parent must not
            // become a valid cleanup target: refuse the linked path, sentinel intact.
            let via_link = linked_parent.join("staging-child");
            assert!(cleanup_staging_root(&via_link, StagingRootKind::HostTest).is_err());
            assert_eq!(fs::read(outside.join("KEEP")).unwrap(), b"alive");
        }

        // Exact approved production/fixture strings still accepted (may not exist).
        validate_staging_root(Path::new(DEFAULT_STAGING_ROOT), StagingRootKind::Production)
            .unwrap();
        validate_staging_root(Path::new(FIXTURE_STAGING_ROOT), StagingRootKind::Production)
            .unwrap();

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn cleanup_refuses_symlinked_root_and_leaves_outside_sentinel() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("ignite-3i-cleanup-{stamp}"));
        let protected = base.join("notebooks-sentinel");
        let staging = base.join("staging");
        fs::create_dir_all(&protected).unwrap();
        fs::write(protected.join("KEEP"), b"alive").unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("tmp"), b"x").unwrap();

        // Symlink that would aim cleanup at the outside sentinel's parent.
        let attack_link = base.join("attack-as-staging");
        if try_symlink_dir(&protected, &attack_link) {
            assert!(cleanup_staging_root(&attack_link, StagingRootKind::HostTest).is_err());
            assert_eq!(fs::read(protected.join("KEEP")).unwrap(), b"alive");
            assert!(protected.exists());
        }

        cleanup_staging_root(&staging, StagingRootKind::HostTest).unwrap();
        assert!(!staging.exists());
        assert_eq!(fs::read(protected.join("KEEP")).unwrap(), b"alive");
        assert!(cleanup_staging_root(
            Path::new("/mnt/us/scribe-data"),
            StagingRootKind::Production
        )
        .is_err());
        assert!(
            cleanup_staging_root(Path::new(UPDATER_STATE_PARENT), StagingRootKind::Production)
                .is_err()
        );
        // Nested production cleanup refused.
        assert!(cleanup_staging_root(
            Path::new(&format!("{DEFAULT_STAGING_ROOT}/nested")),
            StagingRootKind::Production
        )
        .is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn different_tag_completed_candidate_refused() {
        let key = signing_key();
        let (src, staging) = temp_pair("diff-tag");
        write_fixture_dir(&src, &key);
        stage_verified_update(&fixture_request(&src, &staging, &key)).unwrap();
        let mut other = fixture_request(&src, &staging, &key);
        other.expected_tag = "v9.9.9".into();
        other.version = "9.9.9".into();
        let err = stage_verified_update(&other).expect_err("different tag");
        assert!(matches!(err, StagingError::ExistingCandidate(_)));
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn interrupted_publish_leftovers_are_not_complete() {
        let key = signing_key();
        let (src, staging) = temp_pair("interrupt");
        write_fixture_dir(&src, &key);
        stage_verified_update(&fixture_request(&src, &staging, &key)).unwrap();
        // Simulate crash markers.
        fs::create_dir_all(staging.join(ASSETS_NEXT)).unwrap();
        assert!(matches!(
            load_staging_record(&staging, StagingRootKind::HostTest),
            Err(StagingError::InterruptedPublication(_))
        ));
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn interrupt_at_asset_rename_agrees_with_record_reader() {
        let key = signing_key();
        let (src, staging) = temp_pair("interrupt-assets");
        write_fixture_dir(&src, &key);
        let req = fixture_request(&src, &staging, &key);
        stage_verified_update(&req).unwrap();

        // Crash after assets → assets.prev and assets.next exists (mid-swap).
        fs::rename(staging.join(ASSETS_DIR), staging.join(ASSETS_PREV)).unwrap();
        fs::create_dir_all(staging.join(ASSETS_NEXT)).unwrap();
        fs::write(staging.join(ASSETS_NEXT).join("partial"), b"x").unwrap();

        assert!(matches!(
            load_staging_record(&staging, StagingRootKind::HostTest),
            Err(StagingError::InterruptedPublication(_))
        ));
        // Ambiguous next+prev with live assets missing → refuse (no contradictory Ok).
        let err = stage_verified_update(&req).expect_err("ambiguous swap");
        assert!(
            matches!(err, StagingError::InterruptedPublication(_)),
            "unexpected {err}"
        );
        assert!(matches!(
            load_staging_record(&staging, StagingRootKind::HostTest),
            Err(StagingError::InterruptedPublication(_))
        ));
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn interrupt_at_record_rename_reconciles_or_refuses_consistently() {
        let key = signing_key();
        let (src, staging) = temp_pair("interrupt-record");
        write_fixture_dir(&src, &key);
        let req = fixture_request(&src, &staging, &key);
        stage_verified_update(&req).unwrap();

        // Crash after assets published, before record lands: STAGING_RECORD.next present,
        // live record moved to .prev.
        fs::rename(staging.join(RECORD_NAME), staging.join(RECORD_PREV)).unwrap();
        fs::write(staging.join(RECORD_NEXT), b"partial-next-record\n").unwrap();

        assert!(matches!(
            load_staging_record(&staging, StagingRootKind::HostTest),
            Err(StagingError::InterruptedPublication(_))
        ));

        // Ambiguous next+prev with live record missing → refuse.
        let err = stage_verified_update(&req).expect_err("ambiguous record swap");
        assert!(
            matches!(err, StagingError::InterruptedPublication(_)),
            "unexpected {err}"
        );

        // Abandoned .next beside intact published tree: drop next, revalidate, agree.
        fs::rename(staging.join(RECORD_PREV), staging.join(RECORD_NAME)).unwrap();
        let _ = fs::remove_file(staging.join(RECORD_NEXT));
        fs::write(staging.join(RECORD_NEXT), b"abandoned\n").unwrap();
        assert!(matches!(
            load_staging_record(&staging, StagingRootKind::HostTest),
            Err(StagingError::InterruptedPublication(_))
        ));
        let result = stage_verified_update(&req).expect("reconcile abandoned next");
        assert!(result.observations.all_complete());
        assert!(!has_publish_leftovers(&staging));
        let loaded = load_staging_record(&staging, StagingRootKind::HostTest).expect("agree");
        assert!(loaded.all_complete());
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn abandoned_assets_next_beside_published_tree_reconciles() {
        let key = signing_key();
        let (src, staging) = temp_pair("abandon-next");
        write_fixture_dir(&src, &key);
        let req = fixture_request(&src, &staging, &key);
        stage_verified_update(&req).unwrap();
        fs::create_dir_all(staging.join(ASSETS_NEXT)).unwrap();
        fs::write(staging.join(ASSETS_NEXT).join("junk"), b"x").unwrap();
        assert!(load_staging_record(&staging, StagingRootKind::HostTest).is_err());
        let result = stage_verified_update(&req).expect("drop abandoned next");
        assert!(result.observations.all_complete());
        assert!(!staging.join(ASSETS_NEXT).exists());
        load_staging_record(&staging, StagingRootKind::HostTest).expect("agree complete");
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn cli_defaults_separate_fixture_and_production_roots() {
        let key = signing_key();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("ignite-3i-cli-roots-{stamp}"));
        let src = base.join("src");
        let key_path = base.join("public_key.hex");
        fs::create_dir_all(&src).unwrap();
        write_fixture_dir(&src, &key);
        fs::write(&key_path, encode_hex(key.verifying_key().as_bytes())).unwrap();

        let fixture_args = vec![
            COMMAND.into(),
            "--expected-tag".into(),
            TAG.into(),
            "--fixture-assets-dir".into(),
            src.display().to_string(),
            "--fixture-public-key".into(),
            key_path.display().to_string(),
        ];
        match parse_command(&fixture_args).unwrap() {
            StageCommand::Stage(req) => {
                assert_eq!(req.staging_root, PathBuf::from(FIXTURE_STAGING_ROOT));
                assert!(matches!(req.trust, TrustMode::Fixture { .. }));
            }
            other => panic!("unexpected {other:?}"),
        }

        let production_args = vec![
            COMMAND.into(),
            "--expected-tag".into(),
            TAG.into(),
            "--github-release".into(),
            "acme/ignite".into(),
        ];
        match parse_command(&production_args).unwrap() {
            StageCommand::Stage(req) => {
                assert_eq!(req.staging_root, PathBuf::from(DEFAULT_STAGING_ROOT));
                assert!(matches!(req.trust, TrustMode::Production));
            }
            other => panic!("unexpected {other:?}"),
        }

        // Explicit cross-mode roots refused.
        let cross_fixture_on_prod = vec![
            COMMAND.into(),
            "--expected-tag".into(),
            TAG.into(),
            "--fixture-assets-dir".into(),
            src.display().to_string(),
            "--fixture-public-key".into(),
            key_path.display().to_string(),
            "--staging-root".into(),
            DEFAULT_STAGING_ROOT.into(),
        ];
        assert!(matches!(
            parse_command(&cross_fixture_on_prod),
            Err(StagingError::CrossModeRoot(_))
        ));

        let cross_prod_on_fixture = vec![
            COMMAND.into(),
            "--expected-tag".into(),
            TAG.into(),
            "--github-release".into(),
            "acme/ignite".into(),
            "--staging-root".into(),
            FIXTURE_STAGING_ROOT.into(),
        ];
        assert!(matches!(
            parse_command(&cross_prod_on_fixture),
            Err(StagingError::CrossModeRoot(_))
        ));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn enforce_trust_root_isolation_host_test_escape_hatch() {
        let fixture_key = [1_u8; 32];
        let trust = TrustMode::Fixture {
            public_key: fixture_key,
        };
        // HostTest may use arbitrary non-protected temp roots.
        enforce_trust_root_isolation(
            Path::new("/tmp/ignite-host-test-staging"),
            &trust,
            StagingRootKind::HostTest,
        )
        .unwrap();
        // Production kind still enforces namespace + mode pairing.
        assert!(matches!(
            enforce_trust_root_isolation(
                Path::new(DEFAULT_STAGING_ROOT),
                &trust,
                StagingRootKind::Production
            ),
            Err(StagingError::CrossModeRoot(_))
        ));
        assert!(matches!(
            enforce_trust_root_isolation(
                Path::new(FIXTURE_STAGING_ROOT),
                &TrustMode::Production,
                StagingRootKind::Production
            ),
            Err(StagingError::CrossModeRoot(_))
        ));
    }

    #[test]
    fn wrong_publisher_key_fails() {
        let key = signing_key();
        let other = SigningKey::from_bytes(&[12_u8; 32]);
        let (src, staging) = temp_pair("wrong-key");
        write_fixture_dir(&src, &key);
        let mut req = fixture_request(&src, &staging, &key);
        req.trust = TrustMode::Fixture {
            public_key: other.verifying_key().to_bytes(),
        };
        assert!(stage_verified_update(&req).is_err());
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn truncated_manifest_fails() {
        let key = signing_key();
        let (src, staging) = temp_pair("trunc");
        write_fixture_dir(&src, &key);
        fs::write(
            src.join(format!("ignite-kindle-v{VERSION}.manifest")),
            b"schema=1\n",
        )
        .unwrap();
        assert!(stage_verified_update(&fixture_request(&src, &staging, &key)).is_err());
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }
}
