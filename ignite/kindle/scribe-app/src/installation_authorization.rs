//! Installation authorization gate (Slice 3D-B2a.4.3j).
//!
//! Consumes a previously staged 3i candidate and evaluates whether it is
//! eligible for installation. This module never installs, executes a Scriptlet,
//! activates a slot, restarts services, or writes an authorization token.
//!
//! A 3i `STAGING_RECORD` is an observation hint only. Every authorization
//! decision re-validates signed assets under official production trust (or a
//! test-only harness that cannot be reached from the shipping CLI).

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

#[cfg(test)]
use crate::publication_trust_boundary::authenticate_publication_chain_with_key;
use crate::publication_trust_boundary::{
    authenticate_official_publication_chain, AuthenticatedPublicationChain, EvidenceClaimStatus,
    ProductionTrustError, VerificationProvenance,
};
use crate::software_update::{compare_stable_versions, is_stable_release_version};
use crate::release_acquisition::{ReleaseAssetNames, MAX_METADATA_ASSET_BYTES};
#[cfg(test)]
use crate::verified_update_staging::RECORD_NAME;
use crate::verified_update_staging::{
    require_completed_staging_observation, staging_has_interrupted_publication,
    validate_staging_root, StagingError, StagingRootKind, DEFAULT_STAGING_ROOT,
    FIXTURE_STAGING_ROOT, INCOMPLETE_MARKER, STAGED_ASSETS_DIR,
};

pub const COMMAND: &str = "--authorize-verified-update";
pub const DECISION_HEADER: &str = "IGNITE_INSTALLATION_AUTHORIZATION_V1";
pub const DECISION_SCHEMA: &str = "1";

/// Kindle app / recovery / notebook roots used for observational prerequisites.
pub const DEFAULT_APP_ROOT: &str = "/mnt/us/scribe-app";
pub const DEFAULT_DATA_ROOT: &str = "/mnt/us/scribe-data";
pub const DEFAULT_RECOVERY_ROOT: &str = "/var/local/ignite-recovery";
pub const DEFAULT_DOCUMENTS_ROOT: &str = "/mnt/us/documents";

/// Supported architecture token for the verified Scribe matrix.
pub const SUPPORTED_ARCHITECTURE: &str = "armv7l";
/// Supported firmware string for the verified Scribe matrix.
pub const SUPPORTED_FIRMWARE: &str = "5.18.6";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrerequisiteStatus {
    Satisfied,
    Refused(String),
    NotImplemented(String),
}

impl PrerequisiteStatus {
    pub fn as_word(&self) -> &'static str {
        match self {
            Self::Satisfied => "satisfied",
            Self::Refused(_) => "refused",
            Self::NotImplemented(_) => "not_implemented",
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Satisfied => None,
            Self::Refused(s) | Self::NotImplemented(s) => Some(s.as_str()),
        }
    }

    pub fn is_satisfied(&self) -> bool {
        matches!(self, Self::Satisfied)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizationDecision {
    pub expected_tag: String,
    pub staging_root: PathBuf,
    pub provenance: Option<VerificationProvenance>,
    pub candidate_authenticated: PrerequisiteStatus,
    pub publication_claims_validated: PrerequisiteStatus,
    pub artifact_identity_bound: PrerequisiteStatus,
    pub device_compatible: PrerequisiteStatus,
    pub upgrade_compatible: PrerequisiteStatus,
    pub recovery_ready: PrerequisiteStatus,
    pub data_compatibility_verified: PrerequisiteStatus,
    /// Scoped no-migration updater policy: staged `storage-contract.v1` must match
    /// the running binary. Separate from incomplete capability diagnostics.
    pub storage_contract_compatible: PrerequisiteStatus,
    pub execution_binding: PrerequisiteStatus,
    pub installation_policy_satisfied: PrerequisiteStatus,
    pub installation_authorized: bool,
    pub refusal_reasons: Vec<String>,
    pub report_lines: Vec<String>,
    pub bound_scriptlet_path: Option<PathBuf>,
    pub bound_scriptlet_sha256: Option<String>,
}

impl AuthorizationDecision {
    fn all_gate_statuses(&self) -> [&PrerequisiteStatus; 10] {
        [
            &self.candidate_authenticated,
            &self.publication_claims_validated,
            &self.artifact_identity_bound,
            &self.device_compatible,
            &self.upgrade_compatible,
            &self.recovery_ready,
            &self.data_compatibility_verified,
            &self.storage_contract_compatible,
            &self.execution_binding,
            &self.installation_policy_satisfied,
        ]
    }
}

/// Observable device / layout facts. Injected in host tests; collected on device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceFacts {
    pub architecture: String,
    pub firmware: Option<String>,
    pub app_root: PathBuf,
    pub data_root: PathBuf,
    pub recovery_root: PathBuf,
    pub documents_root: PathBuf,
}

impl DeviceFacts {
    pub fn kindle_defaults_from_host() -> Self {
        Self {
            architecture: detect_architecture(),
            firmware: detect_firmware(),
            app_root: PathBuf::from(DEFAULT_APP_ROOT),
            data_root: PathBuf::from(DEFAULT_DATA_ROOT),
            recovery_root: PathBuf::from(DEFAULT_RECOVERY_ROOT),
            documents_root: PathBuf::from(DEFAULT_DOCUMENTS_ROOT),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AuthorizeRequest {
    pub expected_tag: String,
    pub version: String,
    pub staging_root: PathBuf,
    pub root_kind: StagingRootKind,
    pub device: DeviceFacts,
    pub installed_version: String,
    pub user_approved: bool,
}

#[derive(Debug)]
pub enum AuthorizeCommand {
    Authorize(AuthorizeRequest),
    NotRequested,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AuthorizeError {
    Usage(String),
    Staging(StagingError),
    Io(String),
}

impl std::fmt::Display for AuthorizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(e) => write!(f, "{e}"),
            Self::Staging(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl From<StagingError> for AuthorizeError {
    fn from(value: StagingError) -> Self {
        Self::Staging(value)
    }
}

impl From<std::io::Error> for AuthorizeError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

pub fn parse_command(args: &[String]) -> Result<AuthorizeCommand, AuthorizeError> {
    let requested = args.iter().filter(|a| a.as_str() == COMMAND).count();
    if requested == 0 {
        return Ok(AuthorizeCommand::NotRequested);
    }
    if requested != 1 || args.first().map(String::as_str) != Some(COMMAND) {
        return Err(AuthorizeError::Usage(format!(
            "{COMMAND} must be the first and only diagnostic command"
        )));
    }

    let expected_tag = required_value(args, "--expected-tag")?;
    if !expected_tag.starts_with('v') || expected_tag.len() < 2 {
        return Err(AuthorizeError::Usage(
            "--expected-tag must look like vX.Y.Z".into(),
        ));
    }
    let version = expected_tag[1..].to_string();

    // Shipping CLI: official production trust only. No fixture-key bypass.
    if optional_value(args, "--fixture-public-key").is_some() {
        return Err(AuthorizeError::Usage(
            "--fixture-public-key is not accepted by the installation authorization diagnostic"
                .into(),
        ));
    }

    let staging_root = optional_value(args, "--staging-root")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STAGING_ROOT));

    // Production authorization inspects the production staging namespace only.
    if staging_root.to_str() == Some(FIXTURE_STAGING_ROOT)
        || staging_root
            .to_str()
            .is_some_and(|s| s.starts_with(&format!("{FIXTURE_STAGING_ROOT}/")))
    {
        return Err(AuthorizeError::Usage(
            "production installation authorization cannot target the fixture staging root".into(),
        ));
    }

    validate_staging_root(&staging_root, StagingRootKind::Production)?;

    Ok(AuthorizeCommand::Authorize(AuthorizeRequest {
        expected_tag: expected_tag.to_string(),
        version,
        staging_root,
        root_kind: StagingRootKind::Production,
        device: DeviceFacts::kindle_defaults_from_host(),
        installed_version: crate::settings_ui::product_version_digits().to_string(),
        user_approved: false,
    }))
}

/// Shipping entry: official production pin only. Never authorizes while gates are incomplete.
pub fn evaluate_installation_authorization(
    request: &AuthorizeRequest,
) -> Result<AuthorizationDecision, AuthorizeError> {
    validate_staging_root(&request.staging_root, request.root_kind)?;
    evaluate_with_authenticator(request, Authenticator::OfficialProductionPin)
}

/// Test-only fixture evaluation. May exercise intermediate checks; never yields
/// `installation_authorized=true` (fixture provenance cannot authorize production install).
///
/// Compiled only for tests — not present in shipping library builds.
#[cfg(test)]
pub(crate) fn evaluate_installation_authorization_fixture(
    request: &AuthorizeRequest,
    public_key: &[u8; 32],
) -> Result<AuthorizationDecision, AuthorizeError> {
    validate_staging_root(&request.staging_root, request.root_kind)?;
    evaluate_with_authenticator(
        request,
        Authenticator::FixtureEval {
            public_key: *public_key,
        },
    )
}

enum Authenticator {
    OfficialProductionPin,
    /// cfg(test) only — never linked into shipping builds.
    #[cfg(test)]
    FixtureEval {
        public_key: [u8; 32],
    },
    /// cfg(test) harness only — never wired to CLI.
    #[cfg(test)]
    TestHarnessProductionKey {
        public_key: [u8; 32],
    },
}

impl Authenticator {
    fn expected_record_provenance(&self) -> VerificationProvenance {
        match self {
            Self::OfficialProductionPin => VerificationProvenance::OfficialProductionPin,
            #[cfg(test)]
            Self::FixtureEval { .. } => VerificationProvenance::TestOnlyFixtureKey,
            #[cfg(test)]
            Self::TestHarnessProductionKey { .. } => VerificationProvenance::TestOnlyFixtureKey,
        }
    }
}

fn evaluate_with_authenticator(
    request: &AuthorizeRequest,
    authenticator: Authenticator,
) -> Result<AuthorizationDecision, AuthorizeError> {
    let names = ReleaseAssetNames::for_version(&request.version).map_err(|e| {
        AuthorizeError::Usage(format!("invalid release version for authorization: {e}"))
    })?;

    // Interrupted / incomplete staging fails closed before trust work.
    if staging_has_interrupted_publication(&request.staging_root)
        || request
            .staging_root
            .join("work")
            .join(INCOMPLETE_MARKER)
            .exists()
    {
        return Ok(refused_all(
            request,
            None,
            PrerequisiteStatus::Refused(
                "interrupted or incomplete staging publication artifacts present".into(),
            ),
            "interrupted_or_incomplete_staging",
        ));
    }

    // Require a valid completed 3i observation first. The record is never a
    // credential — publisher authentication still runs independently below.
    let completed =
        match require_completed_staging_observation(&request.staging_root, request.root_kind) {
            Ok(obs) => obs,
            Err(err) => {
                return Ok(refused_all(
                    request,
                    None,
                    PrerequisiteStatus::Refused(format!("staging observation refused: {err}")),
                    "invalid_or_incomplete_staging_record",
                ));
            }
        };
    if completed.expected_tag != request.expected_tag {
        return Ok(refused_all(
            request,
            completed.observations.provenance,
            PrerequisiteStatus::Refused(format!(
                "staging record expected_tag {} does not match authorization request {}",
                completed.expected_tag, request.expected_tag
            )),
            "staging_tag_mismatch",
        ));
    }
    let expected_prov = authenticator.expected_record_provenance();
    match completed.observations.provenance {
        Some(p) if p == expected_prov => {}
        other => {
            return Ok(refused_all(
                request,
                other,
                PrerequisiteStatus::Refused(format!(
                    "staging record provenance {:?} inconsistent with authorization context {:?}",
                    other, expected_prov
                )),
                "staging_provenance_mismatch",
            ));
        }
    }

    let assets = request.staging_root.join(STAGED_ASSETS_DIR);
    // Non-following check: refuse symlink / non-directory parents before opening assets.
    if let Err(status) = require_real_assets_directory(&assets) {
        return Ok(refused_all(
            request,
            completed.observations.provenance,
            status,
            "invalid_staged_assets_directory",
        ));
    }

    let evaluated = evaluate_staged_publication_and_scriptlet(
        &assets,
        &names,
        &request.expected_tag,
        &authenticator,
    );

    #[cfg_attr(not(test), allow(unused_mut))]
    let mut provenance = evaluated.provenance;
    #[cfg(test)]
    if matches!(
        authenticator,
        Authenticator::TestHarnessProductionKey { .. }
    ) {
        // cfg(test) harness only: disposable key material treated as official pin
        // for the all-gates conjunction proof.
        if evaluated.candidate_authenticated.is_satisfied() {
            provenance = Some(VerificationProvenance::OfficialProductionPin);
        }
    }

    let candidate_authenticated = evaluated.candidate_authenticated;
    let publication_claims_validated = evaluated.publication_claims_validated;
    let artifact_identity_bound = evaluated.artifact_identity_bound;
    let release_tag = evaluated.release_tag;
    // Fixture provenance never yields installation_authorized=true (see derive_*);
    // candidate_authenticated may still report satisfied when the fixture chain verifies.

    let device_compatible = evaluate_device_compatibility(&request.device);
    let upgrade_compatible =
        evaluate_upgrade_compatible(&request.version, &request.installed_version);
    let recovery_ready = evaluate_recovery_readiness(&request.device);
    let data_compatibility_verified = evaluate_data_compatibility_verified(
        &request.device.data_root,
        &request.device.app_root,
        &request.staging_root,
    );
    let storage_contract_compatible =
        evaluate_storage_contract_compatible(&assets, &names, &artifact_identity_bound);
    let (execution_binding, bound_scriptlet_path, bound_scriptlet_sha256) =
        if artifact_identity_bound.is_satisfied() {
            if let Some(ref auth) = evaluated.auth {
                evaluate_execution_binding(&assets, &names, auth)
            } else {
                (
                    PrerequisiteStatus::Refused(
                        "authenticated chain unavailable for execution binding".into(),
                    ),
                    None,
                    None,
                )
            }
        } else {
            (
                PrerequisiteStatus::Refused(
                    "artifact identity not bound; cannot bind execution".into(),
                ),
                None,
                None,
            )
        };
    let installation_policy_satisfied = evaluate_installation_policy(request.user_approved);

    let mut decision = AuthorizationDecision {
        expected_tag: request.expected_tag.clone(),
        staging_root: request.staging_root.clone(),
        provenance,
        candidate_authenticated,
        publication_claims_validated,
        artifact_identity_bound,
        device_compatible,
        upgrade_compatible,
        recovery_ready,
        data_compatibility_verified,
        storage_contract_compatible,
        execution_binding,
        installation_policy_satisfied,
        installation_authorized: false,
        refusal_reasons: Vec::new(),
        report_lines: Vec::new(),
        bound_scriptlet_path,
        bound_scriptlet_sha256,
    };

    decision.installation_authorized = derive_installation_authorized(&decision);
    decision.refusal_reasons = collect_refusal_reasons(&decision);
    decision.report_lines = build_report_lines(&decision, &request.device, release_tag.as_deref());
    Ok(decision)
}

fn derive_installation_authorized(decision: &AuthorizationDecision) -> bool {
    if decision.provenance != Some(VerificationProvenance::OfficialProductionPin) {
        return false;
    }
    decision
        .all_gate_statuses()
        .iter()
        .all(|s| s.is_satisfied())
}

fn collect_refusal_reasons(decision: &AuthorizationDecision) -> Vec<String> {
    let mut out = Vec::new();
    if decision.provenance != Some(VerificationProvenance::OfficialProductionPin) {
        out.push(match decision.provenance {
            None => "publisher provenance unavailable".into(),
            Some(VerificationProvenance::TestOnlyFixtureKey) => {
                "fixture provenance cannot authorize production installation".into()
            }
            Some(VerificationProvenance::OfficialProductionPin) => unreachable!(),
        });
    }
    for status in decision.all_gate_statuses() {
        if let Some(detail) = status.detail() {
            out.push(format!("{}: {detail}", status.as_word()));
        }
    }
    if !decision.installation_authorized {
        out.push("installation_authorized=false".into());
    }
    out
}

fn refused_all(
    request: &AuthorizeRequest,
    provenance: Option<VerificationProvenance>,
    primary: PrerequisiteStatus,
    code: &str,
) -> AuthorizationDecision {
    let detail = primary.detail().unwrap_or(code).to_string();
    let blocked = PrerequisiteStatus::Refused(detail.clone());
    let mut decision = AuthorizationDecision {
        expected_tag: request.expected_tag.clone(),
        staging_root: request.staging_root.clone(),
        provenance,
        candidate_authenticated: primary,
        publication_claims_validated: blocked.clone(),
        artifact_identity_bound: blocked,
        device_compatible: evaluate_device_compatibility(&request.device),
        upgrade_compatible: evaluate_upgrade_compatible(&request.version, &request.installed_version),
        recovery_ready: evaluate_recovery_readiness(&request.device),
        data_compatibility_verified: evaluate_data_compatibility_verified(
            &request.device.data_root,
            &request.device.app_root,
            &request.staging_root,
        ),
        storage_contract_compatible: PrerequisiteStatus::Refused(
            "storage-contract cannot be release-bound until Scriptlet identity is authenticated"
                .into(),
        ),
        execution_binding: PrerequisiteStatus::Refused(
            "no staged artifact available; authentication failed before execution binding".into(),
        ),
        installation_policy_satisfied: evaluate_installation_policy(request.user_approved),
        installation_authorized: false,
        refusal_reasons: Vec::new(),
        report_lines: Vec::new(),
        bound_scriptlet_path: None,
        bound_scriptlet_sha256: None,
    };
    decision.installation_authorized = derive_installation_authorized(&decision);
    decision.refusal_reasons = collect_refusal_reasons(&decision);
    decision.report_lines = build_report_lines(&decision, &request.device, None);
    let _ = code;
    decision
}

/// Non-following validation of the staged `assets/` directory.
///
/// Path checks alone are not race-free against concurrent symlink substitution.
fn require_real_assets_directory(assets: &Path) -> Result<(), PrerequisiteStatus> {
    match fs::symlink_metadata(assets) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(PrerequisiteStatus::Refused(
            "published staging assets directory missing".into(),
        )),
        Err(err) => Err(PrerequisiteStatus::Refused(format!(
            "cannot stat staged assets directory: {err}"
        ))),
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(PrerequisiteStatus::Refused(
                    "symlink refused for staged assets directory".into(),
                ));
            }
            if !meta.is_dir() {
                return Err(PrerequisiteStatus::Refused(
                    "staged assets path is not a directory".into(),
                ));
            }
            Ok(())
        }
    }
}

struct StagedAssetEvaluation {
    provenance: Option<VerificationProvenance>,
    candidate_authenticated: PrerequisiteStatus,
    publication_claims_validated: PrerequisiteStatus,
    artifact_identity_bound: PrerequisiteStatus,
    release_tag: Option<String>,
    auth: Option<AuthenticatedPublicationChain>,
}

/// Authenticate the publication chain and stream-verify the Scriptlet as
/// independent diagnostic outcomes. Publisher success is not discarded when
/// Scriptlet verification fails.
fn evaluate_staged_publication_and_scriptlet(
    assets: &Path,
    names: &ReleaseAssetNames,
    expected_tag: &str,
    authenticator: &Authenticator,
) -> StagedAssetEvaluation {
    let auth = match authenticate_publication_metadata(assets, names, expected_tag, authenticator) {
        Ok(auth) => auth,
        Err(status) => {
            return StagedAssetEvaluation {
                provenance: None,
                candidate_authenticated: status,
                publication_claims_validated: PrerequisiteStatus::Refused(
                    "publication claims unavailable because candidate authentication failed".into(),
                ),
                artifact_identity_bound: PrerequisiteStatus::Refused(
                    "artifact identity unavailable because candidate authentication failed".into(),
                ),
                release_tag: None,
                auth: None,
            };
        }
    };

    let claims = match auth.evidence_claims() {
        EvidenceClaimStatus::ProductionClaimsValidated => PrerequisiteStatus::Satisfied,
        EvidenceClaimStatus::DigestAuthenticatedClaimsUnverified => PrerequisiteStatus::Refused(
            "publication evidence claims are not production-validated".into(),
        ),
    };
    let artifact = verify_staged_scriptlet(assets, names, &auth);
    let provenance = auth.provenance();
    let release_tag = auth.release().tag().to_string();
    StagedAssetEvaluation {
        provenance: Some(provenance),
        candidate_authenticated: PrerequisiteStatus::Satisfied,
        publication_claims_validated: claims,
        artifact_identity_bound: artifact,
        release_tag: Some(release_tag),
        auth: Some(auth),
    }
}

fn authenticate_publication_metadata(
    assets: &Path,
    names: &ReleaseAssetNames,
    expected_tag: &str,
    authenticator: &Authenticator,
) -> Result<AuthenticatedPublicationChain, PrerequisiteStatus> {
    let release_manifest = read_bounded_file(
        &assets.join(&names.release_manifest),
        MAX_METADATA_ASSET_BYTES,
        &names.release_manifest,
    )?;
    let release_signature = read_bounded_file(
        &assets.join(&names.release_signature),
        MAX_METADATA_ASSET_BYTES,
        &names.release_signature,
    )?;
    let publication_evidence = read_bounded_file(
        &assets.join(&names.publication_evidence),
        MAX_METADATA_ASSET_BYTES,
        &names.publication_evidence,
    )?;
    let publication_binding = read_bounded_file(
        &assets.join(&names.publication_binding),
        MAX_METADATA_ASSET_BYTES,
        &names.publication_binding,
    )?;
    let binding_signature = read_bounded_file(
        &assets.join(&names.binding_signature),
        MAX_METADATA_ASSET_BYTES,
        &names.binding_signature,
    )?;

    match authenticator {
        Authenticator::OfficialProductionPin => {
            match authenticate_official_publication_chain(
                &release_manifest,
                &release_signature,
                &publication_binding,
                &binding_signature,
                &publication_evidence,
                expected_tag,
            ) {
                Ok(auth) => Ok(auth),
                Err(ProductionTrustError::OfficialKeyNotProvisioned) => {
                    Err(PrerequisiteStatus::Refused(
                        "official release signing key is not provisioned (UNPROVISIONED)".into(),
                    ))
                }
                Err(err) => Err(PrerequisiteStatus::Refused(format!(
                    "official publication authentication failed: {err}"
                ))),
            }
        }
        #[cfg(test)]
        Authenticator::FixtureEval { public_key } => authenticate_publication_chain_with_key(
            &release_manifest,
            &release_signature,
            &publication_binding,
            &binding_signature,
            &publication_evidence,
            expected_tag,
            public_key,
        )
        .map_err(|err| {
            PrerequisiteStatus::Refused(format!("fixture publication authentication failed: {err}"))
        }),
        #[cfg(test)]
        Authenticator::TestHarnessProductionKey { public_key } => {
            authenticate_publication_chain_with_key(
                &release_manifest,
                &release_signature,
                &publication_binding,
                &binding_signature,
                &publication_evidence,
                expected_tag,
                public_key,
            )
            .map_err(|err| {
                PrerequisiteStatus::Refused(format!(
                    "harness publication authentication failed: {err}"
                ))
            })
        }
    }
}

fn verify_staged_scriptlet(
    assets: &Path,
    names: &ReleaseAssetNames,
    auth: &AuthenticatedPublicationChain,
) -> PrerequisiteStatus {
    let scriptlet = assets.join(&names.scriptlet);
    let meta = match fs::symlink_metadata(&scriptlet) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return PrerequisiteStatus::Refused("staged Scriptlet missing".into());
        }
        Err(err) => {
            return PrerequisiteStatus::Refused(format!("cannot stat staged Scriptlet: {err}"));
        }
    };
    if meta.file_type().is_symlink() {
        return PrerequisiteStatus::Refused("staged Scriptlet must not be a symlink".into());
    }
    if !meta.is_file() {
        return PrerequisiteStatus::Refused("staged Scriptlet is not a regular file".into());
    }
    let mut file = match File::open(&scriptlet) {
        Ok(f) => f,
        Err(err) => {
            return PrerequisiteStatus::Refused(format!("cannot open staged Scriptlet: {err}"));
        }
    };
    match auth
        .release()
        .verify_installer(auth.release().installer_filename(), &mut file)
    {
        Ok(()) => PrerequisiteStatus::Satisfied,
        Err(err) => {
            PrerequisiteStatus::Refused(format!("staged Scriptlet identity mismatch: {err}"))
        }
    }
}

fn evaluate_device_compatibility(device: &DeviceFacts) -> PrerequisiteStatus {
    // Exact verified matrix only — generic `arm` / `armv7` are not accepted.
    if device.architecture != SUPPORTED_ARCHITECTURE {
        return PrerequisiteStatus::Refused(format!(
            "unsupported architecture '{}'; require exact {SUPPORTED_ARCHITECTURE}",
            device.architecture
        ));
    }
    match &device.firmware {
        Some(fw) if fw == SUPPORTED_FIRMWARE => PrerequisiteStatus::Satisfied,
        Some(fw) => PrerequisiteStatus::Refused(format!(
            "unsupported firmware '{fw}'; verified matrix is {SUPPORTED_FIRMWARE}"
        )),
        None => PrerequisiteStatus::NotImplemented(
            "firmware string unavailable; device compatibility gate cannot affirm".into(),
        ),
    }
}

fn evaluate_upgrade_compatible(candidate_version: &str, installed_version: &str) -> PrerequisiteStatus {
    if !is_stable_release_version(candidate_version) {
        return PrerequisiteStatus::Refused(format!(
            "candidate version '{candidate_version}' is not a stable release version"
        ));
    }
    if !is_stable_release_version(installed_version) {
        return PrerequisiteStatus::Refused(format!(
            "installed version '{installed_version}' is not a recognizable stable release version"
        ));
    }
    match compare_stable_versions(candidate_version, installed_version) {
        Some(std::cmp::Ordering::Greater) => PrerequisiteStatus::Satisfied,
        Some(std::cmp::Ordering::Equal) => PrerequisiteStatus::Refused(format!(
            "candidate {candidate_version} == installed {installed_version}; not an upgrade"
        )),
        Some(std::cmp::Ordering::Less) => PrerequisiteStatus::Refused(format!(
            "candidate {candidate_version} < installed {installed_version}; downgrade refused"
        )),
        None => PrerequisiteStatus::Refused(format!(
            "cannot compare candidate '{candidate_version}' with installed '{installed_version}'"
        )),
    }
}

fn evaluate_data_compatibility_verified(
    data_root: &Path,
    app_root: &Path,
    staging_root: &Path,
) -> PrerequisiteStatus {
    let meta = match fs::symlink_metadata(data_root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return PrerequisiteStatus::Satisfied;
        }
        Err(e) => return PrerequisiteStatus::Refused(format!("cannot stat data_root: {e}")),
        Ok(m) => m,
    };
    if meta.file_type().is_symlink() {
        return PrerequisiteStatus::Refused("data_root must not be a symlink".into());
    }
    if !meta.is_dir() {
        return PrerequisiteStatus::Refused(
            "data_root exists but is not a real directory".into(),
        );
    }
    // Fail-closed: existing data_root must resolve; never silently proceed on canonicalize failure.
    let canon_data = match data_root.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return PrerequisiteStatus::Refused(format!(
                "cannot resolve data_root path: {e}"
            ));
        }
    };
    let canon_app = match app_root.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return PrerequisiteStatus::Refused(format!(
                "cannot resolve app_root path for data compatibility: {e}"
            ));
        }
    };
    let canon_staging = match staging_root.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return PrerequisiteStatus::Refused(format!(
                "cannot resolve staging_root path for data compatibility: {e}"
            ));
        }
    };
    if canon_data.starts_with(&canon_app) || canon_data.starts_with(&canon_staging) {
        return PrerequisiteStatus::Refused(
            "data_root overlaps app_root or staging_root".into(),
        );
    }
    let library_meta = data_root.join("library.meta");
    let library_meta_verified = match fs::symlink_metadata(&library_meta) {
        Ok(lm) if lm.is_file() && !lm.file_type().is_symlink() => {
            let bytes = match fs::read(&library_meta) {
                Ok(b) => b,
                Err(e) => {
                    return PrerequisiteStatus::Refused(format!(
                        "cannot read library.meta: {e}"
                    ));
                }
            };
            let v: serde_json::Value = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(_) => {
                    return PrerequisiteStatus::Refused(
                        "library.meta is not valid JSON; existing data cannot be positively verified".into(),
                    );
                }
            };
            match v.get("v") {
                Some(serde_json::Value::Number(n)) if n.as_i64() == Some(1) => true,
                _ => {
                    return PrerequisiteStatus::Refused(
                        "library.meta schema version must be integer 1; existing data cannot be positively verified".into(),
                    );
                }
            }
        }
        Ok(_) => {
            return PrerequisiteStatus::Refused(
                "library.meta exists but is not a regular file; existing data cannot be positively verified".into(),
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return PrerequisiteStatus::Refused(format!("cannot stat library.meta: {e}"));
        }
    };
    let notebooks = data_root.join("notebooks");
    match fs::symlink_metadata(&notebooks) {
        Ok(nm) => {
            if nm.file_type().is_symlink() {
                return PrerequisiteStatus::Refused(
                    "data_root/notebooks must not be a symlink".into(),
                );
            }
            if !nm.is_dir() {
                return PrerequisiteStatus::Refused(
                    "data_root/notebooks exists but is not a directory".into(),
                );
            }
            // Notebooks indicate existing customer data; require positively verified library.meta.
            if !library_meta_verified {
                return PrerequisiteStatus::Refused(
                    "existing notebooks present without positively verified library.meta; refusing without migration".into(),
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return PrerequisiteStatus::Refused(format!(
                "cannot stat data_root/notebooks: {e}"
            ));
        }
    }
    PrerequisiteStatus::Satisfied
}

fn evaluate_storage_contract_compatible(
    assets_dir: &Path,
    names: &ReleaseAssetNames,
    artifact_identity_bound: &PrerequisiteStatus,
) -> PrerequisiteStatus {
    // Contract file alone is not a trust anchor. Require the claim inside the
    // already-authenticated Scriptlet bytes (or refuse when artifact is unbound).
    if !artifact_identity_bound.is_satisfied() {
        return PrerequisiteStatus::Refused(
            "storage-contract cannot be release-bound until Scriptlet identity is authenticated"
                .into(),
        );
    }
    let scriptlet_path = assets_dir.join(&names.scriptlet);
    let scriptlet_bytes = match fs::read(&scriptlet_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            return PrerequisiteStatus::Refused(format!(
                "cannot read authenticated Scriptlet for storage-contract claim: {err}"
            ));
        }
    };
    match crate::storage_contract::require_release_bound_storage_contract(
        assets_dir,
        &scriptlet_bytes,
    ) {
        Ok(_) => PrerequisiteStatus::Satisfied,
        Err(err) => PrerequisiteStatus::Refused(err.to_string()),
    }
}

/// Mirrors installer `guard_pending_health`: refuse when pending-health, ambiguous LKG
/// metadata, or orphan temps are present. Never deletes evidence.
fn evaluate_pending_health_preflight(app_root: &Path) -> PrerequisiteStatus {
    let update_dir = app_root.join("state").join("update");
    let pending = update_dir.join("pending-health.v1");
    // symlink_metadata Ok covers regular files and symlinks (including dangling), matching
    // installer `[ -e ] || [ -L ]` without following or deleting.
    match fs::symlink_metadata(&pending) {
        Ok(_) => {
            return PrerequisiteStatus::Refused(
                "update blocked: candidate is still pending health qualification; retained last-known-good was preserved"
                    .into(),
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return PrerequisiteStatus::Refused(format!(
                "cannot stat pending-health.v1: {e}"
            ));
        }
    }

    let prev = app_root.join("previous-release");
    let lkg_inventory = prev.join("ignite-lkg-inventory.v1");
    let lkg_handler = prev.join("appreg-handler.sql");
    let lkg_inventory_present = match fs::symlink_metadata(&lkg_inventory) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return PrerequisiteStatus::Refused(format!(
                "cannot stat LKG inventory: {e}"
            ));
        }
    };
    let lkg_handler_present = match fs::symlink_metadata(&lkg_handler) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return PrerequisiteStatus::Refused(format!(
                "cannot stat LKG handler: {e}"
            ));
        }
    };
    if lkg_inventory_present || lkg_handler_present {
        return PrerequisiteStatus::Refused(
            "pending health state is missing but retained qualification metadata remains; refusing ambiguous installation without deleting evidence"
                .into(),
        );
    }

    match fs::read_dir(&update_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let orphan = name.starts_with(".pending-health.") && name.ends_with(".tmp")
                    || name.starts_with(".pending-verify.") && name.ends_with(".tmp")
                    || name.starts_with(".lkg-verify.") && name.ends_with(".tmp");
                if orphan {
                    return PrerequisiteStatus::Refused(
                        "pending health state has an orphaned temporary file; refusing ambiguous installation without deleting evidence"
                            .into(),
                    );
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return PrerequisiteStatus::Refused(format!(
                "cannot read update state directory: {e}"
            ));
        }
    }
    PrerequisiteStatus::Satisfied
}

fn refuse_unresolved_recovery_leftover(path: &Path, label: &str) -> PrerequisiteStatus {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => PrerequisiteStatus::Satisfied,
        Err(e) => PrerequisiteStatus::Refused(format!("cannot stat {label}: {e}")),
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                PrerequisiteStatus::Refused(format!(
                    "{label} must not be a symlink; unresolved recovery evidence preserved"
                ))
            } else {
                PrerequisiteStatus::Refused(format!(
                    "{label} present; incomplete retention refuses authorization without deleting evidence"
                ))
            }
        }
    }
}

fn evaluate_recovery_readiness(device: &DeviceFacts) -> PrerequisiteStatus {
    let app_root = &device.app_root;
    match fs::symlink_metadata(app_root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return PrerequisiteStatus::Refused(format!(
                "app_root '{}' does not exist; cannot verify recovery readiness",
                app_root.display()
            ));
        }
        Err(e) => {
            return PrerequisiteStatus::Refused(format!(
                "cannot stat app_root: {e}"
            ));
        }
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return PrerequisiteStatus::Refused("app_root must not be a symlink".into());
            }
            if !meta.is_dir() {
                return PrerequisiteStatus::Refused("app_root is not a directory".into());
            }
        }
    }

    // Pending-health / LKG preflight must refuse before any install authorization.
    if let status @ PrerequisiteStatus::Refused(_) = evaluate_pending_health_preflight(app_root) {
        return status;
    }

    let prev = app_root.join("previous-release");
    match fs::symlink_metadata(&prev) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return PrerequisiteStatus::Refused(
                    "previous-release must not be a symlink".into(),
                );
            }
            if !meta.is_dir() {
                return PrerequisiteStatus::Refused(
                    "previous-release exists but is not a directory".into(),
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return PrerequisiteStatus::Refused(format!(
                "cannot stat previous-release: {e}"
            ));
        }
    }

    // Match installer spirit: refuse unresolved previous-release.staging / .prev leftovers.
    if let status @ PrerequisiteStatus::Refused(_) =
        refuse_unresolved_recovery_leftover(&app_root.join("previous-release.staging"), "previous-release.staging")
    {
        return status;
    }
    if let status @ PrerequisiteStatus::Refused(_) =
        refuse_unresolved_recovery_leftover(&app_root.join("previous-release.prev"), "previous-release.prev")
    {
        return status;
    }

    let journal = app_root.join("run").join("ignite-activate.journal");
    let committed = app_root.join("run").join("ignite-activate.committed");
    match fs::symlink_metadata(&journal) {
        Ok(_) => {
            if !committed.exists() {
                match fs::symlink_metadata(&prev) {
                    Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                        // Installer snapshot is flat: previous-release/scribe-app (not bin/).
                        let scribe_app = prev.join("scribe-app");
                        if !scribe_app.exists() {
                            return PrerequisiteStatus::Refused(
                                "activate journal without commit marker present; previous-release/scribe-app missing; rollback not possible".into(),
                            );
                        }
                    }
                    _ => {
                        return PrerequisiteStatus::Refused(
                            "activate journal without commit marker present; previous-release missing; rollback not possible".into(),
                        );
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return PrerequisiteStatus::Refused(format!(
                "cannot stat activate journal: {e}"
            ));
        }
    }
    PrerequisiteStatus::Satisfied
}

fn evaluate_execution_binding(
    assets: &Path,
    names: &ReleaseAssetNames,
    auth: &AuthenticatedPublicationChain,
) -> (PrerequisiteStatus, Option<PathBuf>, Option<String>) {
    let scriptlet = assets.join(&names.scriptlet);
    let meta = match fs::symlink_metadata(&scriptlet) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (
                PrerequisiteStatus::Refused("staged Scriptlet missing for execution binding".into()),
                None,
                None,
            );
        }
        Err(e) => {
            return (
                PrerequisiteStatus::Refused(format!("cannot stat Scriptlet for binding: {e}")),
                None,
                None,
            );
        }
    };
    if meta.file_type().is_symlink() {
        return (
            PrerequisiteStatus::Refused("Scriptlet must not be a symlink".into()),
            None,
            None,
        );
    }
    if !meta.is_file() {
        return (
            PrerequisiteStatus::Refused("Scriptlet is not a regular file".into()),
            None,
            None,
        );
    }
    let bytes = match fs::read(&scriptlet) {
        Ok(b) => b,
        Err(e) => {
            return (
                PrerequisiteStatus::Refused(format!("cannot read Scriptlet for binding: {e}")),
                None,
                None,
            );
        }
    };
    let sha256_bytes = Sha256::digest(&bytes);
    let sha256_hex = encode_hex_bytes(&sha256_bytes);
    let mut cursor = std::io::Cursor::new(&bytes[..]);
    match auth
        .release()
        .verify_installer(auth.release().installer_filename(), &mut cursor)
    {
        Ok(()) => {
            let abs_path = match scriptlet.canonicalize() {
                Ok(p) => p,
                Err(_) => scriptlet.clone(),
            };
            (PrerequisiteStatus::Satisfied, Some(abs_path), Some(sha256_hex))
        }
        Err(e) => (
            PrerequisiteStatus::Refused(format!(
                "Scriptlet re-verification failed during execution binding: {e}"
            )),
            None,
            None,
        ),
    }
}

fn evaluate_installation_policy(user_approved: bool) -> PrerequisiteStatus {
    if user_approved {
        PrerequisiteStatus::Satisfied
    } else {
        PrerequisiteStatus::Refused("explicit user installation approval required".into())
    }
}

pub fn revalidate_execution_binding(path: &Path, expected_sha256: &str) -> bool {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return false;
    }
    let actual = encode_hex_bytes(&Sha256::digest(&bytes));
    actual == expected_sha256
}

fn encode_hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn read_bounded_file(
    path: &Path,
    max_bytes: u64,
    label: &str,
) -> Result<Vec<u8>, PrerequisiteStatus> {
    let meta = fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            PrerequisiteStatus::Refused(format!("missing staged asset {label}"))
        } else {
            PrerequisiteStatus::Refused(format!("cannot stat {label}: {e}"))
        }
    })?;
    if meta.file_type().is_symlink() {
        return Err(PrerequisiteStatus::Refused(format!(
            "symlink refused for staged asset {label}"
        )));
    }
    if !meta.is_file() {
        return Err(PrerequisiteStatus::Refused(format!(
            "staged asset {label} is not a regular file"
        )));
    }
    let file = File::open(path)
        .map_err(|e| PrerequisiteStatus::Refused(format!("cannot open {label}: {e}")))?;
    let mut buf = Vec::new();
    let n = file
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| PrerequisiteStatus::Refused(format!("read failed for {label}: {e}")))?;
    if n as u64 > max_bytes {
        return Err(PrerequisiteStatus::Refused(format!(
            "staged asset {label} exceeds {max_bytes} byte metadata ceiling"
        )));
    }
    Ok(buf)
}

fn detect_architecture() -> String {
    #[cfg(unix)]
    {
        if let Ok(output) = std::process::Command::new("uname").arg("-m").output() {
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout).trim().to_string();
            }
        }
    }
    std::env::consts::ARCH.to_string()
}

fn detect_firmware() -> Option<String> {
    // Candidate paths observed/documented for Kindle Linux. Hardware qualification of
    // exact file contents is pending while the device is unreachable over SSH.
    for candidate in ["/etc/prettyversion.txt", "/etc/version.txt"] {
        if let Ok(text) = fs::read_to_string(candidate) {
            if let Some(ver) = parse_kindle_firmware_version(&text) {
                return Some(ver);
            }
        }
    }
    None
}

/// Extract a positively recognized Kindle customer firmware version.
///
/// Accepted forms (documented / assumed — mark hardware-qualified when sampled):
/// - a whole line that is exactly `X.Y.Z` (e.g. `5.18.6`)
/// - a token matching `X.Y.Z` inside a line such as
///   `Amazon Kindle 5.18.6 (041-juno_1806_barolo_bellatrix3-458622)`
///
/// Unrecognized formats return `None` (unavailable), never a guessed first token.
pub fn parse_kindle_firmware_version(text: &str) -> Option<String> {
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if is_dotted_firmware_version(line) {
            return Some(line.to_string());
        }
        for token in line.split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
            let token = token.trim().trim_matches(|c| c == ',' || c == ';');
            if is_dotted_firmware_version(token) {
                return Some(token.to_string());
            }
        }
    }
    None
}

fn is_dotted_firmware_version(token: &str) -> bool {
    let mut parts = token.split('.');
    let mut count = 0;
    for part in parts.by_ref() {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        count += 1;
        if count > 3 {
            return false;
        }
    }
    count == 3
}

fn build_report_lines(
    decision: &AuthorizationDecision,
    device: &DeviceFacts,
    authenticated_release_tag: Option<&str>,
) -> Vec<String> {
    let mut lines = vec![
        DECISION_HEADER.to_string(),
        format!("schema={DECISION_SCHEMA}"),
        format!("expected_tag={}", decision.expected_tag),
        format!("staging_root={}", decision.staging_root.display()),
        format!(
            "publisher_provenance={}",
            match decision.provenance {
                Some(VerificationProvenance::OfficialProductionPin) => {
                    "verified-under-production-pin"
                }
                Some(VerificationProvenance::TestOnlyFixtureKey) => {
                    "verified-under-test-only-fixture-key"
                }
                None => "unavailable",
            }
        ),
        format!(
            "candidate_authenticated={}",
            decision.candidate_authenticated.as_word()
        ),
        format!(
            "publication_claims_validated={}",
            decision.publication_claims_validated.as_word()
        ),
        format!(
            "artifact_identity_bound={}",
            decision.artifact_identity_bound.as_word()
        ),
        format!("device_compatible={}", decision.device_compatible.as_word()),
        format!(
            "upgrade_compatible={}",
            decision.upgrade_compatible.as_word()
        ),
        format!("recovery_ready={}", decision.recovery_ready.as_word()),
        format!(
            "data_compatibility_verified={}",
            decision.data_compatibility_verified.as_word()
        ),
        format!(
            "storage_contract_compatible={}",
            decision.storage_contract_compatible.as_word()
        ),
        format!("execution_binding={}", decision.execution_binding.as_word()),
        format!(
            "installation_policy_satisfied={}",
            decision.installation_policy_satisfied.as_word()
        ),
        format!(
            "installation_authorized={}",
            if decision.installation_authorized {
                "true"
            } else {
                "false"
            }
        ),
        format!("device_architecture={}", device.architecture),
        format!(
            "device_firmware={}",
            device.firmware.as_deref().unwrap_or("unavailable")
        ),
        "installation_performed=false".into(),
        "scriptlet_executed=false".into(),
        "slot_activated=false".into(),
        "authorization_token_written=false".into(),
    ];
    if let Some(tag) = authenticated_release_tag {
        lines.push(format!("authenticated_release_tag={tag}"));
    }
    if let Some(ref p) = decision.bound_scriptlet_path {
        lines.push(format!("bound_scriptlet_path={}", p.display()));
    }
    if let Some(ref sha) = decision.bound_scriptlet_sha256 {
        lines.push(format!("bound_scriptlet_sha256={sha}"));
    }
    for status in decision.all_gate_statuses() {
        if let Some(detail) = status.detail() {
            lines.push(format!("detail_{}={detail}", status.as_word()));
        }
    }
    for reason in &decision.refusal_reasons {
        lines.push(format!("refusal={reason}"));
    }
    lines
}

pub fn emit_authorization_report(decision: &AuthorizationDecision) {
    for line in &decision.report_lines {
        println!("{line}");
    }
}

fn required_value<'a>(args: &'a [String], flag: &str) -> Result<&'a str, AuthorizeError> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            return args
                .get(i + 1)
                .map(String::as_str)
                .ok_or_else(|| AuthorizeError::Usage(format!("missing value for {flag}")));
        }
        i += 1;
    }
    Err(AuthorizeError::Usage(format!("missing {flag}")))
}

fn optional_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            return args.get(i + 1).map(String::as_str);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publication_evidence::build_minimal_production_evidence;
    use crate::publication_trust_boundary::{
        BINDING_HEADER, BINDING_SCHEMA, BINDING_SCOPE, BINDING_SIGNATURE_SCHEMA,
    };
    use crate::release_acquisition::AssetSource;
    use crate::release_manifest::{encode_hex, MANIFEST_SCHEMA, SIGNATURE_SCHEMA};
    use crate::verified_update_staging::{stage_verified_update, StageRequest, TrustMode};
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};

    const VERSION: &str = "3.3.1";
    const TAG: &str = "v3.3.1";
    const INSTALLER: &[u8] =
        b"#!/bin/sh\nIGNITE_STORAGE_CONTRACT='ignite-storage-v1'\necho fixture-scriptlet\n";

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[21_u8; 32])
    }

    fn sign_envelope(message: &[u8], schema: &str, key: &SigningKey) -> Vec<u8> {
        let signature = key.sign(message);
        format!(
            "schema={schema}\nsignature={}\n",
            encode_hex(signature.to_bytes().as_ref())
        )
        .into_bytes()
    }

    fn write_fixture_dir(dir: &Path, key: &SigningKey) {
        let digest = Sha256::digest(INSTALLER);
        let evidence = build_minimal_production_evidence(
            VERSION,
            INSTALLER.len() as u64,
            &encode_hex(&digest),
            &"cd".repeat(32),
        )
        .expect("evidence");
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
    }

    fn temp_pair(label: &str) -> (PathBuf, PathBuf) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("ignite-3j-{label}-{stamp}"));
        let src = base.join("src");
        let staging = base.join("staging");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&src).unwrap();
        (src, staging)
    }

    fn kindle_like_device(base: &Path) -> DeviceFacts {
        DeviceFacts {
            architecture: SUPPORTED_ARCHITECTURE.into(),
            firmware: Some(SUPPORTED_FIRMWARE.into()),
            app_root: base.join("app"),
            data_root: base.join("data"),
            recovery_root: base.join("recovery"),
            documents_root: base.join("documents"),
        }
    }

    fn stage_fixture(src: &Path, staging: &Path, key: &SigningKey) {
        write_fixture_dir(src, key);
        stage_verified_update(&StageRequest {
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
        })
        .expect("fixture stage");
        crate::storage_contract::write_contract_file_atomic(
            &staging.join(STAGED_ASSETS_DIR),
            &crate::storage_contract::StorageContract::current(),
        )
        .expect("write staged storage contract");
    }

    fn authorize_req(staging: &Path, device: DeviceFacts) -> AuthorizeRequest {
        AuthorizeRequest {
            expected_tag: TAG.into(),
            version: VERSION.into(),
            staging_root: staging.to_path_buf(),
            root_kind: StagingRootKind::HostTest,
            device,
            installed_version: "0.9.14".into(),
            user_approved: false,
        }
    }

    fn rewrite_record_line(staging: &Path, old: &str, new: &str) {
        let path = staging.join(RECORD_NAME);
        let text = fs::read_to_string(&path).unwrap();
        let updated = text.replacen(old, new, 1);
        assert_ne!(text, updated, "expected to rewrite {old}");
        fs::write(&path, updated).unwrap();
    }

    fn oversized_record_bytes() -> Vec<u8> {
        use crate::verified_update_staging::MAX_STAGING_RECORD_BYTES;
        let mut huge = format!(
            "IGNITE_VERIFIED_UPDATE_STAGING_V1\nschema=1\nexpected_tag={TAG}\ndownload_complete=true\npublisher_authenticated=true\npublication_claims_validated=true\nscriptlet_verified=true\nstaging_complete=true\npublisher_authentication=verified-under-test-only-fixture-key\ninstallation_binding=unverified\nexecution_binding=unverified\nfirst_launch_admission=unavailable\ndata_compatibility=unknown\npromotion_authorized=false\nauthorization=none\n"
        );
        while huge.len() <= MAX_STAGING_RECORD_BYTES as usize {
            huge.push_str("pad=xxxxxxxx\n");
        }
        huge.into_bytes()
    }

    #[test]
    fn production_eval_refuses_fixture_provenance_mismatch() {
        let key = signing_key();
        let (src, staging) = temp_pair("prov-mismatch");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        let decision =
            evaluate_installation_authorization(&authorize_req(&staging, device)).unwrap();
        assert!(!decision.installation_authorized);
        assert!(
            decision
                .refusal_reasons
                .iter()
                .any(|r| r.contains("provenance") || r.contains("inconsistent")),
            "{:?}",
            decision.refusal_reasons
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn production_pin_unprovisioned_refuses_authorization() {
        let key = signing_key();
        let (src, staging) = temp_pair("unprov");
        stage_fixture(&src, &staging, &key);
        // Observation claims official provenance so the gate reaches official-pin auth.
        rewrite_record_line(
            &staging,
            "publisher_authentication=verified-under-test-only-fixture-key",
            "publisher_authentication=verified-under-production-pin",
        );
        let device = kindle_like_device(src.parent().unwrap());
        let decision =
            evaluate_installation_authorization(&authorize_req(&staging, device)).unwrap();
        assert!(!decision.installation_authorized);
        assert!(matches!(
            decision.candidate_authenticated,
            PrerequisiteStatus::Refused(_)
        ));
        assert!(
            decision
                .refusal_reasons
                .iter()
                .any(|r| {
                    r.contains("UNPROVISIONED")
                        || r.contains("not provisioned")
                        || r.contains("signature is invalid")
                        || r.contains("authentication failed")
                        || r.contains("provenance unavailable")
                }),
            "{:?}",
            decision.refusal_reasons
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn completed_staging_record_required_before_asset_auth() {
        let key = signing_key();
        let (src, staging) = temp_pair("record-required");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        let pk = key.verifying_key().to_bytes();

        // 1. Delete only STAGING_RECORD; six authentic assets remain.
        let record_backup = fs::read(staging.join(RECORD_NAME)).unwrap();
        fs::remove_file(staging.join(RECORD_NAME)).unwrap();
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device.clone()),
            &pk,
        )
        .unwrap();
        assert!(!decision.installation_authorized);
        assert!(decision
            .refusal_reasons
            .iter()
            .any(|r| r.contains("absent") || r.contains("staging observation")));

        // 2. Malformed record.
        fs::write(staging.join(RECORD_NAME), b"NOT_A_RECORD\n").unwrap();
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device.clone()),
            &pk,
        )
        .unwrap();
        assert!(!decision.installation_authorized);
        assert!(matches!(
            decision.candidate_authenticated,
            PrerequisiteStatus::Refused(_)
        ));

        // 3. Oversized record.
        fs::write(staging.join(RECORD_NAME), oversized_record_bytes()).unwrap();
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device.clone()),
            &pk,
        )
        .unwrap();
        assert!(!decision.installation_authorized);

        // 4. Wrong expected tag in record.
        fs::write(&staging.join(RECORD_NAME), &record_backup).unwrap();
        rewrite_record_line(&staging, "expected_tag=v3.3.1", "expected_tag=v9.9.9");
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device.clone()),
            &pk,
        )
        .unwrap();
        assert!(!decision.installation_authorized);
        assert!(decision
            .refusal_reasons
            .iter()
            .any(|r| r.contains("expected_tag") || r.contains("does not match")));

        // 5. Provenance rewritten inconsistently with fixture context.
        fs::write(&staging.join(RECORD_NAME), &record_backup).unwrap();
        rewrite_record_line(
            &staging,
            "publisher_authentication=verified-under-test-only-fixture-key",
            "publisher_authentication=verified-under-production-pin",
        );
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device.clone()),
            &pk,
        )
        .unwrap();
        assert!(!decision.installation_authorized);
        assert!(decision
            .refusal_reasons
            .iter()
            .any(|r| r.contains("provenance") || r.contains("inconsistent")));

        // 6. Incomplete record.
        fs::write(&staging.join(RECORD_NAME), &record_backup).unwrap();
        rewrite_record_line(&staging, "staging_complete=true", "staging_complete=false");
        // Keep other flags true → parser may reject inconsistency; either way refuse.
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device.clone()),
            &pk,
        )
        .unwrap();
        assert!(!decision.installation_authorized);

        // 7. Restore valid completed record: fixture eval never production-authorizes.
        fs::write(&staging.join(RECORD_NAME), &record_backup).unwrap();
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device.clone()),
            &pk,
        )
        .unwrap();
        assert!(!decision.installation_authorized);
        assert_eq!(
            decision.provenance,
            Some(VerificationProvenance::TestOnlyFixtureKey)
        );

        // 8. Mutate Scriptlet after staging → artifact refused; publisher auth stays.
        fs::write(
            staging
                .join(STAGED_ASSETS_DIR)
                .join(format!("Install Ignite v{VERSION}.sh")),
            b"mutated-after-complete-record",
        )
        .unwrap();
        let decision =
            evaluate_installation_authorization_fixture(&authorize_req(&staging, device), &pk)
                .unwrap();
        assert!(!decision.installation_authorized);
        assert!(matches!(
            decision.candidate_authenticated,
            PrerequisiteStatus::Satisfied
        ));
        assert!(matches!(
            decision.publication_claims_validated,
            PrerequisiteStatus::Satisfied
        ));
        assert!(matches!(
            decision.artifact_identity_bound,
            PrerequisiteStatus::Refused(_)
        ));
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn fixture_eval_never_authorizes_production_install() {
        let key = signing_key();
        let (src, staging) = temp_pair("fixture-eval");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device),
            &key.verifying_key().to_bytes(),
        )
        .unwrap();
        assert!(!decision.installation_authorized);
        assert_eq!(
            decision.provenance,
            Some(VerificationProvenance::TestOnlyFixtureKey)
        );
        assert!(matches!(
            decision.candidate_authenticated,
            PrerequisiteStatus::Satisfied
        ));
        assert!(matches!(
            decision.publication_claims_validated,
            PrerequisiteStatus::Satisfied
        ));
        assert!(matches!(
            decision.artifact_identity_bound,
            PrerequisiteStatus::Satisfied
        ));
        assert!(decision
            .report_lines
            .iter()
            .any(|l| l == "authenticated_release_tag=v3.3.1"));
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn missing_or_modified_scriptlet_refuses_artifact_independently() {
        let key = signing_key();
        let (src, staging) = temp_pair("scriptlet");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::write(
            staging
                .join(STAGED_ASSETS_DIR)
                .join(format!("Install Ignite v{VERSION}.sh")),
            b"mutated",
        )
        .unwrap();
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device.clone()),
            &key.verifying_key().to_bytes(),
        )
        .unwrap();
        assert!(!decision.installation_authorized);
        assert!(matches!(
            decision.candidate_authenticated,
            PrerequisiteStatus::Satisfied
        ));
        assert!(matches!(
            decision.publication_claims_validated,
            PrerequisiteStatus::Satisfied
        ));
        assert!(matches!(
            decision.artifact_identity_bound,
            PrerequisiteStatus::Refused(_)
        ));

        fs::remove_file(
            staging
                .join(STAGED_ASSETS_DIR)
                .join(format!("Install Ignite v{VERSION}.sh")),
        )
        .unwrap();
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device),
            &key.verifying_key().to_bytes(),
        )
        .unwrap();
        assert!(!decision.installation_authorized);
        assert!(matches!(
            decision.candidate_authenticated,
            PrerequisiteStatus::Satisfied
        ));
        assert!(matches!(
            decision.artifact_identity_bound,
            PrerequisiteStatus::Refused(_)
        ));
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn forged_record_does_not_authorize() {
        let (src, staging) = temp_pair("forged");
        fs::create_dir_all(&staging).unwrap();
        fs::write(
            staging.join(RECORD_NAME),
            format!(
                "IGNITE_VERIFIED_UPDATE_STAGING_V1\nschema=1\nexpected_tag={TAG}\ndownload_complete=true\npublisher_authenticated=true\npublication_claims_validated=true\nscriptlet_verified=true\nstaging_complete=true\npublisher_authentication=verified-under-production-pin\ninstallation_binding=unverified\nexecution_binding=unverified\nfirst_launch_admission=unavailable\ndata_compatibility=unknown\npromotion_authorized=false\nauthorization=none\n"
            ),
        )
        .unwrap();
        let device = kindle_like_device(src.parent().unwrap());
        let decision =
            evaluate_installation_authorization(&authorize_req(&staging, device)).unwrap();
        assert!(!decision.installation_authorized);
        assert!(matches!(
            decision.candidate_authenticated,
            PrerequisiteStatus::Refused(_)
        ));
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn wrong_release_identity_refuses() {
        let key = signing_key();
        let (src, staging) = temp_pair("wrong-tag");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        let mut req = authorize_req(&staging, device);
        req.expected_tag = "v9.9.9".into();
        req.version = "9.9.9".into();
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        assert!(!decision.installation_authorized);
        assert!(matches!(
            decision.candidate_authenticated,
            PrerequisiteStatus::Refused(_)
        ));
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn unsupported_architecture_and_firmware_refuse_device_gate() {
        let key = signing_key();
        let (src, staging) = temp_pair("device");
        stage_fixture(&src, &staging, &key);
        let mut device = kindle_like_device(src.parent().unwrap());
        device.architecture = "x86_64".into();
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device.clone()),
            &key.verifying_key().to_bytes(),
        )
        .unwrap();
        assert!(matches!(
            decision.device_compatible,
            PrerequisiteStatus::Refused(_)
        ));
        assert!(!decision.installation_authorized);

        // Generic `arm` is not accepted as ARMv7 proof.
        device.architecture = "arm".into();
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device.clone()),
            &key.verifying_key().to_bytes(),
        )
        .unwrap();
        assert!(matches!(
            decision.device_compatible,
            PrerequisiteStatus::Refused(_)
        ));

        device.architecture = SUPPORTED_ARCHITECTURE.into();
        device.firmware = Some("9.9.9".into());
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device),
            &key.verifying_key().to_bytes(),
        )
        .unwrap();
        assert!(matches!(
            decision.device_compatible,
            PrerequisiteStatus::Refused(_)
        ));
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn firmware_parser_accepts_documented_forms_only() {
        assert_eq!(
            parse_kindle_firmware_version("5.18.6\n"),
            Some("5.18.6".into())
        );
        assert_eq!(
            parse_kindle_firmware_version(
                "Amazon Kindle 5.18.6 (041-juno_1806_barolo_bellatrix3-458622)\n"
            ),
            Some("5.18.6".into())
        );
        // Assumption: parenthesized build id alone is not a customer firmware version.
        assert_eq!(
            parse_kindle_firmware_version("041-juno_1806_barolo_bellatrix3-458622\n"),
            None
        );
        // Do not guess the first whitespace token when it is not X.Y.Z.
        assert_eq!(
            parse_kindle_firmware_version("KindleScribe firmware-build-only\n"),
            None
        );
        assert_eq!(parse_kindle_firmware_version(""), None);
    }

    #[test]
    fn interrupted_staging_refuses() {
        let key = signing_key();
        let (src, staging) = temp_pair("interrupt");
        stage_fixture(&src, &staging, &key);
        fs::create_dir_all(staging.join("assets.next")).unwrap();
        let device = kindle_like_device(src.parent().unwrap());
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device),
            &key.verifying_key().to_bytes(),
        )
        .unwrap();
        assert!(!decision.installation_authorized);
        assert!(matches!(
            decision.candidate_authenticated,
            PrerequisiteStatus::Refused(_)
        ));
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn policy_gate_refuses_without_user_approval() {
        let key = signing_key();
        let (src, staging) = temp_pair("policy-gate");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device),
            &key.verifying_key().to_bytes(),
        )
        .unwrap();
        assert!(
            matches!(decision.installation_policy_satisfied, PrerequisiteStatus::Refused(_)),
            "policy must be Refused when user_approved=false"
        );
        assert!(!decision.installation_authorized);
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn upgrade_compatible_gate_refuses_downgrade() {
        let key = signing_key();
        let (src, staging) = temp_pair("downgrade");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        let mut req = authorize_req(&staging, device);
        req.installed_version = "9.9.9".into();
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        assert!(
            matches!(decision.upgrade_compatible, PrerequisiteStatus::Refused(_)),
            "downgrade must be refused; got {:?}",
            decision.upgrade_compatible
        );
        assert!(!decision.installation_authorized);
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn user_approved_with_good_layout_satisfies_policy_but_not_production_authorized() {
        let key = signing_key();
        let (src, staging) = temp_pair("policy-ok");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        assert!(
            matches!(decision.installation_policy_satisfied, PrerequisiteStatus::Satisfied),
            "policy must be Satisfied with user_approved=true"
        );
        assert!(
            matches!(decision.upgrade_compatible, PrerequisiteStatus::Satisfied),
            "upgrade_compatible must be Satisfied with older installed version"
        );
        assert!(
            matches!(decision.recovery_ready, PrerequisiteStatus::Satisfied),
            "recovery_ready must be Satisfied with real app_root"
        );
        assert!(
            matches!(decision.data_compatibility_verified, PrerequisiteStatus::Satisfied),
            "data_compatibility_verified must be Satisfied with absent data_root"
        );
        assert!(
            matches!(decision.execution_binding, PrerequisiteStatus::Satisfied),
            "execution_binding must be Satisfied when artifact passes re-verification"
        );
        assert!(
            decision.bound_scriptlet_sha256.is_some(),
            "bound_scriptlet_sha256 must be recorded on successful binding"
        );
        assert!(
            !decision.installation_authorized,
            "fixture provenance cannot authorize production installation"
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
        let _ = device;
    }

    #[test]
    fn revalidate_execution_binding_detects_tamper() {
        let key = signing_key();
        let (src, staging) = temp_pair("toctou");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        let path = decision.bound_scriptlet_path.expect("binding must produce path");
        let sha256 = decision.bound_scriptlet_sha256.expect("binding must produce sha256");
        assert!(
            revalidate_execution_binding(&path, &sha256),
            "revalidation must succeed on untampered file"
        );
        fs::write(&path, b"tampered-after-binding").unwrap();
        assert!(
            !revalidate_execution_binding(&path, &sha256),
            "revalidation must fail after file is modified"
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
        let _ = device;
    }

    #[test]
    fn prior_favorable_fixture_eval_is_not_authoritative_after_mutation() {
        let key = signing_key();
        let (src, staging) = temp_pair("stale");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        let first = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device.clone()),
            &key.verifying_key().to_bytes(),
        )
        .unwrap();
        assert!(!first.installation_authorized);
        assert!(matches!(
            first.candidate_authenticated,
            PrerequisiteStatus::Satisfied
        ));
        assert!(matches!(
            first.artifact_identity_bound,
            PrerequisiteStatus::Satisfied
        ));
        fs::write(
            staging
                .join(STAGED_ASSETS_DIR)
                .join(format!("Install Ignite v{VERSION}.sh")),
            b"changed-after-diagnostic",
        )
        .unwrap();
        let second = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device),
            &key.verifying_key().to_bytes(),
        )
        .unwrap();
        assert!(!second.installation_authorized);
        assert!(matches!(
            second.candidate_authenticated,
            PrerequisiteStatus::Satisfied
        ));
        assert!(matches!(
            second.artifact_identity_bound,
            PrerequisiteStatus::Refused(_)
        ));
        let _ = fs::remove_dir_all(src.parent().unwrap());
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

    fn hash_tree(root: &Path) -> Vec<(String, String)> {
        let mut entries = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.file_type().is_symlink() {
                    entries.push((rel, "symlink".into()));
                } else if meta.is_dir() {
                    stack.push(path);
                } else {
                    let bytes = fs::read(&path).unwrap();
                    entries.push((rel, encode_hex(&Sha256::digest(&bytes))));
                }
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    #[test]
    fn staged_assets_directory_must_be_real_directory_not_symlink() {
        let key = signing_key();
        let (src, staging) = temp_pair("assets-dir");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        let pk = key.verifying_key().to_bytes();
        let base = src.parent().unwrap().to_path_buf();

        // Normal real directory: publisher authentication can proceed.
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device.clone()),
            &pk,
        )
        .unwrap();
        assert!(matches!(
            decision.candidate_authenticated,
            PrerequisiteStatus::Satisfied
        ));
        assert!(!decision.installation_authorized);

        let assets = staging.join(STAGED_ASSETS_DIR);
        let outside = base.join("outside-signed-assets");
        fs::rename(&assets, &outside).unwrap();
        let outside_before = hash_tree(&outside);

        if try_symlink_dir(&outside, &assets) {
            let decision = evaluate_installation_authorization_fixture(
                &authorize_req(&staging, device.clone()),
                &pk,
            )
            .unwrap();
            assert!(!decision.installation_authorized);
            match &decision.candidate_authenticated {
                PrerequisiteStatus::Refused(detail) => {
                    assert!(
                        detail.contains("symlink"),
                        "expected symlink refusal before publisher auth, got {detail}"
                    );
                }
                other => panic!("expected assets-dir symlink refusal, got {other:?}"),
            }
            assert!(
                !matches!(
                    decision.candidate_authenticated,
                    PrerequisiteStatus::Satisfied
                ),
                "must not authenticate through a symlinked assets directory"
            );
            assert_eq!(
                outside_before,
                hash_tree(&outside),
                "outside signed assets must remain untouched"
            );
            // Directory symlink: prefer remove_dir; fall back for platforms that
            // expose the link as a removable file entry.
            if fs::remove_dir(&assets).is_err() {
                fs::remove_file(&assets).unwrap();
            }
        }

        // Non-directory assets object at the staged path.
        fs::write(&assets, b"not-a-directory").unwrap();
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device.clone()),
            &pk,
        )
        .unwrap();
        assert!(!decision.installation_authorized);
        match &decision.candidate_authenticated {
            PrerequisiteStatus::Refused(detail) => {
                assert!(
                    detail.contains("not a directory"),
                    "expected non-directory refusal, got {detail}"
                );
            }
            other => panic!("expected non-directory assets refusal, got {other:?}"),
        }
        assert_eq!(outside_before, hash_tree(&outside));

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn invalid_publication_signatures_refuse_candidate_and_dependents() {
        let key = signing_key();
        let (src, staging) = temp_pair("bad-sig");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::write(
            staging
                .join(STAGED_ASSETS_DIR)
                .join(format!("ignite-kindle-v{VERSION}.manifest.sig")),
            b"schema=release-signature-v1\nsignature=00\n",
        )
        .unwrap();
        let decision = evaluate_installation_authorization_fixture(
            &authorize_req(&staging, device),
            &key.verifying_key().to_bytes(),
        )
        .unwrap();
        assert!(!decision.installation_authorized);
        assert!(matches!(
            decision.candidate_authenticated,
            PrerequisiteStatus::Refused(_)
        ));
        match &decision.publication_claims_validated {
            PrerequisiteStatus::Refused(detail) => {
                assert!(detail.contains("candidate authentication failed"));
            }
            other => panic!("expected dependent claims refusal, got {other:?}"),
        }
        match &decision.artifact_identity_bound {
            PrerequisiteStatus::Refused(detail) => {
                assert!(detail.contains("candidate authentication failed"));
            }
            other => panic!("expected dependent artifact refusal, got {other:?}"),
        }
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn cli_rejects_fixture_key_and_fixture_root() {
        let args = vec![
            COMMAND.into(),
            "--expected-tag".into(),
            TAG.into(),
            "--fixture-public-key".into(),
            "/tmp/key".into(),
        ];
        assert!(matches!(
            parse_command(&args),
            Err(AuthorizeError::Usage(_))
        ));
        let args = vec![
            COMMAND.into(),
            "--expected-tag".into(),
            TAG.into(),
            "--staging-root".into(),
            FIXTURE_STAGING_ROOT.into(),
        ];
        assert!(matches!(
            parse_command(&args),
            Err(AuthorizeError::Usage(_))
        ));
    }

    #[test]
    fn decision_model_requires_every_gate_and_official_provenance() {
        let mut decision = AuthorizationDecision {
            expected_tag: TAG.into(),
            staging_root: PathBuf::from("/tmp/x"),
            provenance: Some(VerificationProvenance::OfficialProductionPin),
            candidate_authenticated: PrerequisiteStatus::Satisfied,
            publication_claims_validated: PrerequisiteStatus::Satisfied,
            artifact_identity_bound: PrerequisiteStatus::Satisfied,
            device_compatible: PrerequisiteStatus::Satisfied,
            upgrade_compatible: PrerequisiteStatus::Satisfied,
            recovery_ready: PrerequisiteStatus::Satisfied,
            data_compatibility_verified: PrerequisiteStatus::Satisfied,
            storage_contract_compatible: PrerequisiteStatus::Satisfied,
            execution_binding: PrerequisiteStatus::Satisfied,
            installation_policy_satisfied: PrerequisiteStatus::Satisfied,
            installation_authorized: false,
            refusal_reasons: Vec::new(),
            report_lines: Vec::new(),
            bound_scriptlet_path: None,
            bound_scriptlet_sha256: None,
        };
        assert!(derive_installation_authorized(&decision));
        decision.execution_binding =
            PrerequisiteStatus::Refused("missing execution binding".into());
        assert!(!derive_installation_authorized(&decision));
        decision.execution_binding = PrerequisiteStatus::Satisfied;
        decision.provenance = Some(VerificationProvenance::TestOnlyFixtureKey);
        assert!(!derive_installation_authorized(&decision));
    }

    #[test]
    fn test_harness_can_demonstrate_authorized_conjunction() {
        let key = signing_key();
        let (src, staging) = temp_pair("harness");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        req.installed_version = "0.9.14".into();
        let decision = evaluate_with_authenticator(
            &req,
            Authenticator::TestHarnessProductionKey {
                public_key: key.verifying_key().to_bytes(),
            },
        )
        .unwrap();
        assert!(
            decision.installation_authorized,
            "harness must prove all-satisfied conjunction; got {:?}",
            decision.refusal_reasons
        );
        assert_eq!(
            decision.provenance,
            Some(VerificationProvenance::OfficialProductionPin)
        );
        assert!(decision.bound_scriptlet_path.is_some());
        assert!(decision.bound_scriptlet_sha256.is_some());
        let _ = fs::remove_dir_all(src.parent().unwrap());
        let _ = device;
    }

    #[test]
    fn shipping_cli_entry_never_returns_authorized_while_gates_incomplete() {
        let key = signing_key();
        let (src, staging) = temp_pair("cli-entry");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        let decision =
            evaluate_installation_authorization(&authorize_req(&staging, device)).unwrap();
        assert!(!decision.installation_authorized);
        assert!(decision
            .report_lines
            .iter()
            .any(|l| l == "installation_authorized=false"));
        assert!(decision
            .report_lines
            .iter()
            .any(|l| l == "scriptlet_executed=false"));
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn data_compat_satisfied_when_data_root_absent() {
        let key = signing_key();
        let (src, staging) = temp_pair("dc-absent");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        assert!(
            matches!(decision.data_compatibility_verified, PrerequisiteStatus::Satisfied),
            "absent data_root must be Satisfied (fresh install): got {:?}",
            decision.data_compatibility_verified
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
        let _ = device;
    }

    #[test]
    fn data_compat_refuses_when_data_root_is_a_file() {
        let key = signing_key();
        let (src, staging) = temp_pair("dc-file");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        fs::write(&device.data_root, b"not-a-directory").unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        assert!(
            matches!(decision.data_compatibility_verified, PrerequisiteStatus::Refused(_)),
            "data_root that is a file must be Refused: got {:?}",
            decision.data_compatibility_verified
        );
        assert!(!decision.installation_authorized);
        let _ = fs::remove_dir_all(src.parent().unwrap());
        let _ = device;
    }

    #[cfg(unix)]
    #[test]
    fn data_compat_refuses_on_enotdir_stat_error() {
        let key = signing_key();
        let (src, staging) = temp_pair("dc-enotdir");
        stage_fixture(&src, &staging, &key);
        let mut device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        // File used as a directory prefix → symlink_metadata on the child returns ENOTDIR.
        let blocker = device.data_root.parent().unwrap().join("block-file");
        fs::write(&blocker, b"blocking-file").unwrap();
        let inner = blocker.join("child");
        device.data_root = inner;
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        match &decision.data_compatibility_verified {
            PrerequisiteStatus::Refused(detail) => {
                assert!(
                    detail.contains("cannot stat data_root"),
                    "expected fail-closed refusal message, got: {detail}"
                );
            }
            other => panic!(
                "ENOTDIR on data_root stat must be Refused (fail-closed), got {:?}",
                other
            ),
        }
        assert!(!decision.installation_authorized);
        let _ = fs::remove_dir_all(src.parent().unwrap());
        let _ = device;
    }

    #[test]
    fn authorized_decision_bound_path_and_sha256_are_set() {
        let key = signing_key();
        let (src, staging) = temp_pair("bound-path");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision = evaluate_with_authenticator(
            &req,
            Authenticator::TestHarnessProductionKey {
                public_key: key.verifying_key().to_bytes(),
            },
        )
        .unwrap();
        assert!(decision.installation_authorized);
        let path = decision.bound_scriptlet_path.expect("bound_scriptlet_path must be set");
        let sha256 = decision.bound_scriptlet_sha256.expect("bound_scriptlet_sha256 must be set");
        assert!(path.exists(), "bound_scriptlet_path must point to an existing file");
        assert_eq!(sha256.len(), 64, "sha256 hex must be 64 characters");
        assert!(
            revalidate_execution_binding(&path, &sha256),
            "bound path and sha256 from decision must pass revalidation"
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
        let _ = device;
    }

    #[test]
    fn install_authorization_bound_path_differs_from_stale_hint_after_file_replace() {
        let key = signing_key();
        let (src, staging) = temp_pair("stale-hint");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let first = evaluate_with_authenticator(
            &req,
            Authenticator::TestHarnessProductionKey {
                public_key: key.verifying_key().to_bytes(),
            },
        )
        .unwrap();
        assert!(first.installation_authorized);
        let stale_path = first.bound_scriptlet_path.as_ref().unwrap();
        let stale_sha = first.bound_scriptlet_sha256.as_ref().unwrap();
        fs::write(stale_path, b"tampered-installer").unwrap();
        let second = evaluate_with_authenticator(
            &req,
            Authenticator::TestHarnessProductionKey {
                public_key: key.verifying_key().to_bytes(),
            },
        )
        .unwrap();
        assert!(
            !second.installation_authorized,
            "tampered scriptlet must refuse authorization"
        );
        assert!(
            !revalidate_execution_binding(stale_path, stale_sha),
            "stale sha256 hint must fail revalidation after scriptlet tamper"
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
        let _ = device;
    }

    #[test]
    fn pending_health_v1_refuses_recovery_and_preserves_evidence() {
        let key = signing_key();
        let (src, staging) = temp_pair("pending-health");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        let update_dir = device.app_root.join("state").join("update");
        fs::create_dir_all(&update_dir).unwrap();
        let pending = update_dir.join("pending-health.v1");
        let evidence = b"IGNITE_PENDING_HEALTH_V1\nschema=1\nstatus=pending-health\n";
        fs::write(&pending, evidence).unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision = evaluate_with_authenticator(
            &req,
            Authenticator::TestHarnessProductionKey {
                public_key: key.verifying_key().to_bytes(),
            },
        )
        .unwrap();
        match &decision.recovery_ready {
            PrerequisiteStatus::Refused(detail) => {
                assert!(
                    detail.contains("pending health qualification"),
                    "expected pending-health refusal, got {detail}"
                );
            }
            other => panic!("pending-health.v1 must refuse recovery_ready, got {other:?}"),
        }
        assert!(!decision.installation_authorized);
        assert!(
            pending.exists(),
            "pending-health.v1 must not be deleted by authorization"
        );
        assert_eq!(fs::read(&pending).unwrap(), evidence);
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn pending_health_absent_allows_recovery_preflight() {
        let key = signing_key();
        let (src, staging) = temp_pair("pending-absent");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        assert!(
            matches!(decision.recovery_ready, PrerequisiteStatus::Satisfied),
            "absent pending-health must leave recovery preflight Satisfied: {:?}",
            decision.recovery_ready
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn lkg_metadata_without_pending_refuses_ambiguous_recovery() {
        let key = signing_key();
        let (src, staging) = temp_pair("lkg-no-pending");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        let prev = device.app_root.join("previous-release");
        fs::create_dir_all(&prev).unwrap();
        fs::write(prev.join("ignite-lkg-inventory.v1"), b"inventory").unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        match &decision.recovery_ready {
            PrerequisiteStatus::Refused(detail) => {
                assert!(
                    detail.contains("retained qualification metadata"),
                    "expected ambiguous LKG refusal, got {detail}"
                );
            }
            other => panic!("LKG without pending must refuse, got {other:?}"),
        }
        assert!(prev.join("ignite-lkg-inventory.v1").exists());
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn flat_previous_release_scribe_app_satisfies_journal_recovery() {
        let key = signing_key();
        let (src, staging) = temp_pair("flat-prev");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        let prev = device.app_root.join("previous-release");
        fs::create_dir_all(&prev).unwrap();
        fs::write(prev.join("scribe-app"), b"#!/bin/sh\n").unwrap();
        let run = device.app_root.join("run");
        fs::create_dir_all(&run).unwrap();
        fs::write(run.join("ignite-activate.journal"), b"journal").unwrap();
        // No commit marker → journal recovery path requires flat previous-release/scribe-app.
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        assert!(
            matches!(decision.recovery_ready, PrerequisiteStatus::Satisfied),
            "flat previous-release/scribe-app must satisfy journal recovery: {:?}",
            decision.recovery_ready
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn nested_bin_previous_release_scribe_app_is_not_sufficient_for_journal_recovery() {
        let key = signing_key();
        let (src, staging) = temp_pair("nested-bin-prev");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        let nested = device
            .app_root
            .join("previous-release")
            .join("bin");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("scribe-app"), b"#!/bin/sh\n").unwrap();
        let run = device.app_root.join("run");
        fs::create_dir_all(&run).unwrap();
        fs::write(run.join("ignite-activate.journal"), b"journal").unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        match &decision.recovery_ready {
            PrerequisiteStatus::Refused(detail) => {
                assert!(
                    detail.contains("previous-release/scribe-app missing"),
                    "nested bin/ path must not satisfy flat layout, got {detail}"
                );
            }
            other => panic!("bin/ nested previous-release must refuse, got {other:?}"),
        }
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn previous_release_staging_leftover_refuses_recovery() {
        let key = signing_key();
        let (src, staging) = temp_pair("prev-staging");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        fs::create_dir_all(device.app_root.join("previous-release.staging")).unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        assert!(
            matches!(decision.recovery_ready, PrerequisiteStatus::Refused(_)),
            "previous-release.staging leftover must refuse: {:?}",
            decision.recovery_ready
        );
        assert!(device.app_root.join("previous-release.staging").exists());
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn data_compat_refuses_unresolved_canonicalize() {
        let key = signing_key();
        let (src, staging) = temp_pair("dc-unresolved");
        stage_fixture(&src, &staging, &key);
        let mut device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        // Create data_root, then replace its parent path component with a broken symlink so
        // symlink_metadata on the final path fails or canonicalize cannot resolve.
        let data_parent = device.data_root.parent().unwrap().to_path_buf();
        fs::create_dir_all(&device.data_root).unwrap();
        // Point data_root at a path whose canonicalize fails: file used as directory prefix.
        let blocker = data_parent.join("block-file");
        fs::write(&blocker, b"x").unwrap();
        device.data_root = blocker.join("nested-data");
        // Ensure staging/app exist for other gates; data_root stat may NotFound or ENOTDIR.
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        assert!(
            matches!(
                decision.data_compatibility_verified,
                PrerequisiteStatus::Refused(_)
            ),
            "unresolved data_root must refuse: {:?}",
            decision.data_compatibility_verified
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn data_compat_refuses_notebooks_without_verified_library_meta() {
        let key = signing_key();
        let (src, staging) = temp_pair("dc-notebooks");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        fs::create_dir_all(device.data_root.join("notebooks")).unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision =
            evaluate_installation_authorization_fixture(&req, &key.verifying_key().to_bytes())
                .unwrap();
        match &decision.data_compatibility_verified {
            PrerequisiteStatus::Refused(detail) => {
                assert!(
                    detail.contains("positively verified"),
                    "expected unverified existing data refusal, got {detail}"
                );
            }
            other => panic!("notebooks without library.meta must refuse, got {other:?}"),
        }
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }

    #[test]
    fn ready_to_install_does_not_mean_install_runtime_accepts_live_process() {
        // Contract documentation: ReadyToInstall / installation_authorized only means
        // cryptographic + policy gates passed. install-runtime.sh still refuses while
        // scribe-app is running (spawn-before-exit). No post-exit supervisor exists yet.
        let key = signing_key();
        let (src, staging) = temp_pair("live-proc-contract");
        stage_fixture(&src, &staging, &key);
        let device = kindle_like_device(src.parent().unwrap());
        fs::create_dir_all(&device.app_root).unwrap();
        let mut req = authorize_req(&staging, device.clone());
        req.user_approved = true;
        let decision = evaluate_with_authenticator(
            &req,
            Authenticator::TestHarnessProductionKey {
                public_key: key.verifying_key().to_bytes(),
            },
        )
        .unwrap();
        assert!(
            decision.installation_authorized,
            "authorization can succeed while the process is still live"
        );
        let mut update_state = crate::software_update::SoftwareUpdateState::default();
        update_state.phase = crate::software_update::UpdateCheckPhase::ReadyToInstall {
            handoff: crate::software_update::UpdateDiscoveryHandoff {
                installed_version: "0.9.14".into(),
                candidate_version: VERSION.into(),
                candidate_tag: TAG.into(),
                official_origin: "markrai/ignite".into(),
                eligibility: crate::software_update::UpdateEligibility::NewerStableAvailable,
                release_name: None,
                published_at: None,
            },
            staging_root: staging.clone(),
            scriptlet_path: decision.bound_scriptlet_path.clone().unwrap(),
            scriptlet_sha256: decision.bound_scriptlet_sha256.clone().unwrap(),
        };
        let (headline, details) = crate::software_update::status_copy(&update_state);
        let blob = format!("{headline} {}", details.join(" ")).to_lowercase();
        assert!(
            blob.contains("cannot start while") || blob.contains("exit") || blob.contains("leave"),
            "ReadyToInstall copy must not imply install-runtime accepts a live process: {blob}"
        );
        assert!(
            !blob.contains("will restart into") && !blob.contains("install is underway"),
            "must not claim successful install handoff: {blob}"
        );
        let _ = fs::remove_dir_all(src.parent().unwrap());
    }
}
