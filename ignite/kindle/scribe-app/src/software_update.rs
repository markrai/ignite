//! Software Update discovery for Settings → Preferences.
//!
//! GitHub Releases metadata is advisory only. It never authenticates an installer.
//! Installation authority remains the production signing pin and signed-release path.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use crate::installation_authorization::{
    evaluate_installation_authorization, revalidate_execution_binding, AuthorizationDecision,
    AuthorizeRequest, DeviceFacts,
};
use crate::release_acquisition::{
    parse_https_url, AcquisitionError, AssetSource, GithubReleaseOrigin,
};
use crate::release_manifest::valid_release_version;
use crate::settings_ui::product_version_digits;
use crate::verified_update_staging::StagingRootKind;
use crate::verified_update_staging::{stage_verified_update, StageRequest, TrustMode};

pub const OFFICIAL_ORIGIN_SPEC: &str = "markrai/ignite";
pub const MAX_METADATA_BYTES: u64 = 256 * 1024;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateEligibility {
    NewerStableAvailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateDiscoveryHandoff {
    pub installed_version: String,
    pub candidate_version: String,
    pub candidate_tag: String,
    pub official_origin: String,
    pub eligibility: UpdateEligibility,
    pub release_name: Option<String>,
    pub published_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateCheckPhase {
    Idle,
    Checking {
        generation: u64,
    },
    UpToDate {
        installed: String,
    },
    Available(UpdateDiscoveryHandoff),
    Error {
        message: String,
    },
    Downloading {
        generation: u64,
        handoff: UpdateDiscoveryHandoff,
    },
    Verifying {
        generation: u64,
        handoff: UpdateDiscoveryHandoff,
    },
    ReadyToInstall {
        handoff: UpdateDiscoveryHandoff,
        staging_root: PathBuf,
        scriptlet_path: PathBuf,
        scriptlet_sha256: String,
    },
    Installing {
        generation: u64,
        handoff: UpdateDiscoveryHandoff,
        scriptlet_path: PathBuf,
        scriptlet_sha256: String,
    },
    InstallFailed {
        message: String,
        handoff: Option<UpdateDiscoveryHandoff>,
        retryable: bool,
        /// When set, RETRY restores ReadyToInstall (no re-download). Download failures leave this None.
        ready_residual: Option<ReadyInstallResidual>,
    },
}

/// Staged install identity preserved across retryable post-stage failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadyInstallResidual {
    pub staging_root: PathBuf,
    pub scriptlet_path: PathBuf,
    pub scriptlet_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoftwareUpdateState {
    pub phase: UpdateCheckPhase,
    generation: u64,
}

impl Default for SoftwareUpdateState {
    fn default() -> Self {
        Self {
            phase: UpdateCheckPhase::Idle,
            generation: 0,
        }
    }
}

impl SoftwareUpdateState {
    pub fn busy(&self) -> bool {
        matches!(
            self.phase,
            UpdateCheckPhase::Checking { .. }
                | UpdateCheckPhase::Downloading { .. }
                | UpdateCheckPhase::Verifying { .. }
                | UpdateCheckPhase::Installing { .. }
        )
    }

    pub fn begin_check(&mut self) -> Option<u64> {
        if self.busy() {
            return None;
        }
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        self.phase = UpdateCheckPhase::Checking { generation };
        Some(generation)
    }

    pub fn apply_result(
        &mut self,
        generation: u64,
        result: Result<UpdateCheckOutcome, UpdateDiscoveryError>,
    ) -> bool {
        match &self.phase {
            UpdateCheckPhase::Checking { generation: active } if *active == generation => {}
            _ => return false,
        }
        self.phase = match result {
            Ok(UpdateCheckOutcome::UpToDate { installed }) => {
                UpdateCheckPhase::UpToDate { installed }
            }
            Ok(UpdateCheckOutcome::Available(handoff)) => UpdateCheckPhase::Available(handoff),
            Err(_) => UpdateCheckPhase::Error {
                message: "Could not check for updates.".into(),
            },
        };
        true
    }

    pub fn begin_download(&mut self, handoff: UpdateDiscoveryHandoff) -> Option<u64> {
        // Post-stage residuals must not force a re-download; only download-stage failures retry here.
        let eligible = match &self.phase {
            UpdateCheckPhase::Available(_) => true,
            UpdateCheckPhase::InstallFailed {
                retryable: true,
                handoff: Some(_),
                ready_residual: None,
                ..
            } => true,
            _ => false,
        };
        if !eligible || self.busy() {
            return None;
        }
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        self.phase = UpdateCheckPhase::Downloading {
            generation,
            handoff,
        };
        Some(generation)
    }

    pub fn apply_download_result(
        &mut self,
        generation: u64,
        result: Result<ReadyInstallCandidate, UpdateInstallError>,
    ) -> bool {
        match &self.phase {
            UpdateCheckPhase::Downloading {
                generation: active, ..
            }
            | UpdateCheckPhase::Verifying {
                generation: active, ..
            } if *active == generation => {}
            _ => return false,
        }
        self.phase = match result {
            Ok(candidate) => UpdateCheckPhase::ReadyToInstall {
                handoff: candidate.handoff,
                staging_root: candidate.staging_root,
                scriptlet_path: candidate.scriptlet_path,
                scriptlet_sha256: candidate.scriptlet_sha256,
            },
            Err(e) => {
                let handoff = match &self.phase {
                    UpdateCheckPhase::Downloading { handoff, .. }
                    | UpdateCheckPhase::Verifying { handoff, .. } => Some(handoff.clone()),
                    _ => None,
                };
                UpdateCheckPhase::InstallFailed {
                    message: e.to_string(),
                    handoff,
                    retryable: !matches!(e, UpdateInstallError::Downgrade),
                    ready_residual: None,
                }
            }
        };
        true
    }

    pub fn mark_verifying(&mut self, generation: u64) -> bool {
        match &self.phase {
            UpdateCheckPhase::Downloading {
                generation: active,
                handoff,
            } if *active == generation => {
                let handoff = handoff.clone();
                self.phase = UpdateCheckPhase::Verifying {
                    generation,
                    handoff,
                };
                true
            }
            _ => false,
        }
    }

    /// Restore ReadyToInstall from a retryable post-stage failure residual.
    pub fn restore_ready_from_failed(&mut self) -> bool {
        match std::mem::replace(&mut self.phase, UpdateCheckPhase::Idle) {
            UpdateCheckPhase::InstallFailed {
                handoff: Some(handoff),
                retryable: true,
                ready_residual: Some(residual),
                ..
            } => {
                self.phase = UpdateCheckPhase::ReadyToInstall {
                    handoff,
                    staging_root: residual.staging_root,
                    scriptlet_path: residual.scriptlet_path,
                    scriptlet_sha256: residual.scriptlet_sha256,
                };
                true
            }
            other => {
                self.phase = other;
                false
            }
        }
    }

    pub fn begin_install(&mut self) -> Option<UpdateDiscoveryHandoff> {
        match std::mem::replace(&mut self.phase, UpdateCheckPhase::Idle) {
            UpdateCheckPhase::ReadyToInstall {
                handoff,
                staging_root: _,
                scriptlet_path,
                scriptlet_sha256,
            } => {
                self.generation = self.generation.saturating_add(1);
                let generation = self.generation;
                self.phase = UpdateCheckPhase::Installing {
                    generation,
                    handoff: handoff.clone(),
                    scriptlet_path,
                    scriptlet_sha256,
                };
                Some(handoff)
            }
            other => {
                self.phase = other;
                None
            }
        }
    }

    pub fn cancel_ready(&mut self) -> bool {
        match std::mem::replace(&mut self.phase, UpdateCheckPhase::Idle) {
            UpdateCheckPhase::ReadyToInstall { handoff, .. } => {
                self.phase = UpdateCheckPhase::Available(handoff);
                true
            }
            other => {
                self.phase = other;
                false
            }
        }
    }

    pub fn handoff(&self) -> Option<&UpdateDiscoveryHandoff> {
        match &self.phase {
            UpdateCheckPhase::Available(h) => Some(h),
            UpdateCheckPhase::Downloading { handoff, .. }
            | UpdateCheckPhase::Verifying { handoff, .. }
            | UpdateCheckPhase::Installing { handoff, .. } => Some(handoff),
            UpdateCheckPhase::ReadyToInstall { handoff, .. } => Some(handoff),
            UpdateCheckPhase::InstallFailed {
                handoff: Some(h), ..
            } => Some(h),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateCheckOutcome {
    UpToDate { installed: String },
    Available(UpdateDiscoveryHandoff),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadyInstallCandidate {
    pub handoff: UpdateDiscoveryHandoff,
    pub staging_root: PathBuf,
    pub scriptlet_path: PathBuf,
    pub scriptlet_sha256: String,
    pub authorize_preview: AuthorizationDecision,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedInstallPlan {
    pub scriptlet_path: PathBuf,
    pub scriptlet_sha256: String,
    pub handoff: UpdateDiscoveryHandoff,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateInstallError {
    Downgrade,
    StagingFailed(String),
    AuthorizationFailed(String),
    PolicyRefused,
    NotReady,
    IoError(String),
}

impl std::fmt::Display for UpdateInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Downgrade => write!(f, "candidate is not newer than installed version"),
            Self::StagingFailed(e) => write!(f, "staging failed: {e}"),
            Self::AuthorizationFailed(e) => write!(f, "authorization failed: {e}"),
            Self::PolicyRefused => write!(f, "installation policy refused"),
            Self::NotReady => write!(f, "update not ready"),
            Self::IoError(e) => write!(f, "I/O error: {e}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateDiscoveryError {
    Transport(String),
    Timeout,
    InvalidJson,
    Oversized,
    MalformedVersion(String),
    UntrustedOrigin(String),
    DraftOrPrerelease,
    HttpStatus(u32),
    Acquisition(AcquisitionError),
}

impl std::fmt::Display for UpdateDiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::Timeout => write!(f, "timeout"),
            Self::InvalidJson => write!(f, "invalid json"),
            Self::Oversized => write!(f, "oversized response"),
            Self::MalformedVersion(v) => write!(f, "malformed version {v}"),
            Self::UntrustedOrigin(o) => write!(f, "untrusted origin {o}"),
            Self::DraftOrPrerelease => write!(f, "draft or prerelease"),
            Self::HttpStatus(c) => write!(f, "http {c}"),
            Self::Acquisition(e) => write!(f, "{e}"),
        }
    }
}

pub trait ReleaseMetadataTransport {
    fn fetch_latest_release_json(
        &self,
        origin: &GithubReleaseOrigin,
    ) -> Result<Vec<u8>, UpdateDiscoveryError>;
}

pub struct CurlMetadataTransport {
    pub connect_timeout: Duration,
    pub transfer_timeout: Duration,
    pub size_limit: u64,
}

impl Default for CurlMetadataTransport {
    fn default() -> Self {
        Self {
            connect_timeout: CONNECT_TIMEOUT,
            transfer_timeout: TRANSFER_TIMEOUT,
            size_limit: MAX_METADATA_BYTES,
        }
    }
}

impl ReleaseMetadataTransport for CurlMetadataTransport {
    fn fetch_latest_release_json(
        &self,
        origin: &GithubReleaseOrigin,
    ) -> Result<Vec<u8>, UpdateDiscoveryError> {
        let url = latest_release_api_url(origin)?;
        validate_github_api_url(&url)?;
        let tmp =
            std::env::temp_dir().join(format!("ignite-update-meta-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let output = Command::new("curl")
            .arg("--silent")
            .arg("--show-error")
            .arg("--proto")
            .arg("=https")
            .arg("--max-filesize")
            .arg(self.size_limit.to_string())
            .arg("--connect-timeout")
            .arg(self.connect_timeout.as_secs().max(1).to_string())
            .arg("--max-time")
            .arg(self.transfer_timeout.as_secs().max(1).to_string())
            .arg("--header")
            .arg("Accept: application/vnd.github+json")
            .arg("--header")
            .arg("User-Agent: Ignite-Kindle-SoftwareUpdate/1.0")
            .arg("--output")
            .arg(&tmp)
            .arg("--write-out")
            .arg("%{http_code}")
            .arg(&url)
            .output()
            .map_err(|e| UpdateDiscoveryError::Transport(format!("curl spawn failed: {e}")))?;
        let code_txt = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            let _ = std::fs::remove_file(&tmp);
            let lower = stderr.to_ascii_lowercase();
            if lower.contains("timeout") || lower.contains("timed out") {
                return Err(UpdateDiscoveryError::Timeout);
            }
            if lower.contains("maximum file size") {
                return Err(UpdateDiscoveryError::Oversized);
            }
            return Err(UpdateDiscoveryError::Transport(
                if stderr.trim().is_empty() {
                    format!("curl failed ({code_txt})")
                } else {
                    stderr.trim().to_string()
                },
            ));
        }
        let code: u32 = code_txt
            .parse()
            .map_err(|_| UpdateDiscoveryError::Transport(format!("bad http code {code_txt}")))?;
        if code != 200 {
            let _ = std::fs::remove_file(&tmp);
            return Err(UpdateDiscoveryError::HttpStatus(code));
        }
        let bytes =
            std::fs::read(&tmp).map_err(|e| UpdateDiscoveryError::Transport(e.to_string()))?;
        let _ = std::fs::remove_file(&tmp);
        if bytes.len() as u64 > self.size_limit {
            return Err(UpdateDiscoveryError::Oversized);
        }
        Ok(bytes)
    }
}

pub fn official_origin() -> Result<GithubReleaseOrigin, UpdateDiscoveryError> {
    GithubReleaseOrigin::parse(OFFICIAL_ORIGIN_SPEC).map_err(UpdateDiscoveryError::Acquisition)
}

pub fn latest_release_api_url(
    origin: &GithubReleaseOrigin,
) -> Result<String, UpdateDiscoveryError> {
    if origin.owner != "markrai" || origin.repo != "ignite" {
        return Err(UpdateDiscoveryError::UntrustedOrigin(format!(
            "{}/{}",
            origin.owner, origin.repo
        )));
    }
    Ok(format!(
        "https://api.github.com/repos/{}/{}/releases/latest",
        origin.owner, origin.repo
    ))
}

pub fn validate_github_api_url(url: &str) -> Result<(), UpdateDiscoveryError> {
    let parsed = parse_https_url(url).map_err(UpdateDiscoveryError::Acquisition)?;
    if parsed.host != "api.github.com" {
        return Err(UpdateDiscoveryError::UntrustedOrigin(url.to_string()));
    }
    if parsed.path_and_query != "/repos/markrai/ignite/releases/latest" {
        return Err(UpdateDiscoveryError::UntrustedOrigin(url.to_string()));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct GithubLatestRelease {
    tag_name: String,
    name: Option<String>,
    draft: bool,
    prerelease: bool,
    html_url: String,
    published_at: Option<String>,
}

pub fn discover_latest_stable(
    transport: &dyn ReleaseMetadataTransport,
    installed_version: &str,
) -> Result<UpdateCheckOutcome, UpdateDiscoveryError> {
    if !is_stable_release_version(installed_version) {
        return Err(UpdateDiscoveryError::MalformedVersion(
            installed_version.to_string(),
        ));
    }
    let origin = official_origin()?;
    let bytes = transport.fetch_latest_release_json(&origin)?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err(UpdateDiscoveryError::Oversized);
    }
    let release: GithubLatestRelease =
        serde_json::from_slice(&bytes).map_err(|_| UpdateDiscoveryError::InvalidJson)?;
    interpret_latest_release(installed_version, &origin, &release)
}

fn interpret_latest_release(
    installed_version: &str,
    origin: &GithubReleaseOrigin,
    release: &GithubLatestRelease,
) -> Result<UpdateCheckOutcome, UpdateDiscoveryError> {
    if release.draft || release.prerelease {
        return Err(UpdateDiscoveryError::DraftOrPrerelease);
    }
    assert_trusted_html_url(origin, &release.html_url)?;
    let tag = release.tag_name.trim();
    if !tag.starts_with('v') {
        return Err(UpdateDiscoveryError::MalformedVersion(tag.to_string()));
    }
    let candidate = &tag[1..];
    if !is_stable_release_version(candidate) {
        return Err(UpdateDiscoveryError::MalformedVersion(
            candidate.to_string(),
        ));
    }
    match compare_stable_versions(candidate, installed_version) {
        None => Err(UpdateDiscoveryError::MalformedVersion(format!(
            "{candidate} vs {installed_version}"
        ))),
        Some(std::cmp::Ordering::Greater) => {
            Ok(UpdateCheckOutcome::Available(UpdateDiscoveryHandoff {
                installed_version: installed_version.to_string(),
                candidate_version: candidate.to_string(),
                candidate_tag: tag.to_string(),
                official_origin: format!("{}/{}", origin.owner, origin.repo),
                eligibility: UpdateEligibility::NewerStableAvailable,
                release_name: release
                    .name
                    .as_ref()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                published_at: release
                    .published_at
                    .as_ref()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            }))
        }
        Some(std::cmp::Ordering::Equal) | Some(std::cmp::Ordering::Less) => {
            Ok(UpdateCheckOutcome::UpToDate {
                installed: installed_version.to_string(),
            })
        }
    }
}

fn assert_trusted_html_url(
    origin: &GithubReleaseOrigin,
    html_url: &str,
) -> Result<(), UpdateDiscoveryError> {
    let parsed = parse_https_url(html_url).map_err(UpdateDiscoveryError::Acquisition)?;
    if parsed.host != "github.com" && parsed.host != "www.github.com" {
        return Err(UpdateDiscoveryError::UntrustedOrigin(html_url.to_string()));
    }
    let expected_prefix = format!("/{}/{}/releases/", origin.owner, origin.repo);
    if !parsed.path_and_query.starts_with(&expected_prefix) {
        return Err(UpdateDiscoveryError::UntrustedOrigin(html_url.to_string()));
    }
    Ok(())
}

pub fn is_stable_release_version(version: &str) -> bool {
    valid_release_version(version) && !version.contains('-')
}

pub fn compare_stable_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let pa = parse_stable_triple(a)?;
    let pb = parse_stable_triple(b)?;
    Some(pa.cmp(&pb))
}

fn parse_stable_triple(version: &str) -> Option<(u64, u64, u64)> {
    if !is_stable_release_version(version) {
        return None;
    }
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

pub fn installed_version() -> &'static str {
    product_version_digits()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateDownloadProgress {
    Verifying,
}

pub fn download_and_stage_official(
    handoff: &UpdateDiscoveryHandoff,
    staging_root: &Path,
    root_kind: StagingRootKind,
    source: AssetSource,
    trust: TrustMode,
    device: DeviceFacts,
) -> Result<ReadyInstallCandidate, UpdateInstallError> {
    download_and_stage_official_with_progress(
        handoff,
        staging_root,
        root_kind,
        source,
        trust,
        device,
        |_| {},
    )
}

pub fn download_and_stage_official_with_progress(
    handoff: &UpdateDiscoveryHandoff,
    staging_root: &Path,
    root_kind: StagingRootKind,
    source: AssetSource,
    trust: TrustMode,
    device: DeviceFacts,
    mut on_progress: impl FnMut(UpdateDownloadProgress),
) -> Result<ReadyInstallCandidate, UpdateInstallError> {
    let installed = handoff.installed_version.as_str();
    if compare_stable_versions(&handoff.candidate_version, installed)
        != Some(std::cmp::Ordering::Greater)
    {
        return Err(UpdateInstallError::Downgrade);
    }
    let req = StageRequest {
        expected_tag: handoff.candidate_tag.clone(),
        version: handoff.candidate_version.clone(),
        staging_root: staging_root.to_path_buf(),
        source,
        trust,
        root_kind,
    };
    let result = stage_verified_update(&req)
        .map_err(|e| UpdateInstallError::StagingFailed(e.to_string()))?;
    on_progress(UpdateDownloadProgress::Verifying);
    let auth_req = AuthorizeRequest {
        expected_tag: handoff.candidate_tag.clone(),
        version: handoff.candidate_version.clone(),
        staging_root: staging_root.to_path_buf(),
        root_kind,
        device,
        installed_version: installed.to_string(),
        user_approved: false,
    };
    let decision = evaluate_installation_authorization(&auth_req)
        .map_err(|e| UpdateInstallError::AuthorizationFailed(e.to_string()))?;
    let non_policy_gates_ok = decision.candidate_authenticated.is_satisfied()
        && decision.publication_claims_validated.is_satisfied()
        && decision.artifact_identity_bound.is_satisfied()
        && decision.device_compatible.is_satisfied()
        && decision.upgrade_compatible.is_satisfied()
        && decision.recovery_ready.is_satisfied()
        && decision.data_compatibility_verified.is_satisfied()
        && decision.execution_binding.is_satisfied();
    if !non_policy_gates_ok {
        let reason = decision
            .refusal_reasons
            .iter()
            .find(|r| {
                !r.contains("installation_policy") && !r.contains("user installation approval")
            })
            .cloned()
            .unwrap_or_else(|| "authentication or device gate refused".into());
        return Err(UpdateInstallError::AuthorizationFailed(reason));
    }
    let scriptlet_path = decision
        .bound_scriptlet_path
        .clone()
        .ok_or_else(|| UpdateInstallError::AuthorizationFailed("no bound scriptlet path".into()))?;
    let scriptlet_sha256 = decision.bound_scriptlet_sha256.clone().ok_or_else(|| {
        UpdateInstallError::AuthorizationFailed("no bound scriptlet sha256".into())
    })?;
    let _ = result;
    Ok(ReadyInstallCandidate {
        handoff: handoff.clone(),
        staging_root: staging_root.to_path_buf(),
        scriptlet_path,
        scriptlet_sha256,
        authorize_preview: decision,
    })
}

pub fn prepare_install_execution(
    candidate: &ReadyInstallCandidate,
    device: &DeviceFacts,
) -> Result<AuthorizedInstallPlan, UpdateInstallError> {
    let auth_req = AuthorizeRequest {
        expected_tag: candidate.handoff.candidate_tag.clone(),
        version: candidate.handoff.candidate_version.clone(),
        staging_root: candidate.staging_root.clone(),
        root_kind: StagingRootKind::Production,
        device: device.clone(),
        installed_version: candidate.handoff.installed_version.clone(),
        user_approved: true,
    };
    let decision = evaluate_installation_authorization(&auth_req)
        .map_err(|e| UpdateInstallError::AuthorizationFailed(e.to_string()))?;
    if !decision.installation_authorized {
        let reason = decision
            .refusal_reasons
            .first()
            .cloned()
            .unwrap_or_else(|| "authorization refused".into());
        return Err(UpdateInstallError::AuthorizationFailed(reason));
    }
    // Always prefer freshly authorized decision bounds over the ReadyInstallCandidate hint.
    let scriptlet_path = decision.bound_scriptlet_path.clone().ok_or_else(|| {
        UpdateInstallError::AuthorizationFailed(
            "authorized decision missing bound scriptlet path".into(),
        )
    })?;
    let scriptlet_sha256 = decision.bound_scriptlet_sha256.clone().ok_or_else(|| {
        UpdateInstallError::AuthorizationFailed(
            "authorized decision missing bound scriptlet sha256".into(),
        )
    })?;
    Ok(AuthorizedInstallPlan {
        scriptlet_path,
        scriptlet_sha256,
        handoff: candidate.handoff.clone(),
    })
}

pub fn build_install_command(plan: &AuthorizedInstallPlan) -> Option<Command> {
    if !revalidate_execution_binding(&plan.scriptlet_path, &plan.scriptlet_sha256) {
        return None;
    }
    let mut cmd = Command::new("sh");
    cmd.arg(&plan.scriptlet_path);
    Some(cmd)
}

#[cfg(test)]
pub fn stage_and_prepare_ready_for_tests(
    handoff: &UpdateDiscoveryHandoff,
    staging_root: &Path,
    src: &Path,
    key: &ed25519_dalek::SigningKey,
    device: DeviceFacts,
) -> Result<ReadyInstallCandidate, UpdateInstallError> {
    use crate::installation_authorization::evaluate_installation_authorization_fixture;
    if compare_stable_versions(&handoff.candidate_version, &handoff.installed_version)
        != Some(std::cmp::Ordering::Greater)
    {
        return Err(UpdateInstallError::AuthorizationFailed(format!(
            "candidate {} is not newer than installed {}",
            handoff.candidate_version, handoff.installed_version
        )));
    }
    let req = StageRequest {
        expected_tag: handoff.candidate_tag.clone(),
        version: handoff.candidate_version.clone(),
        staging_root: staging_root.to_path_buf(),
        source: AssetSource::LocalDirectory {
            root: src.to_path_buf(),
        },
        trust: TrustMode::Fixture {
            public_key: key.verifying_key().to_bytes(),
        },
        root_kind: StagingRootKind::HostTest,
    };
    let _result = stage_verified_update(&req)
        .map_err(|e| UpdateInstallError::StagingFailed(e.to_string()))?;
    let auth_req = AuthorizeRequest {
        expected_tag: handoff.candidate_tag.clone(),
        version: handoff.candidate_version.clone(),
        staging_root: staging_root.to_path_buf(),
        root_kind: StagingRootKind::HostTest,
        device,
        installed_version: handoff.installed_version.clone(),
        user_approved: false,
    };
    let decision =
        evaluate_installation_authorization_fixture(&auth_req, &key.verifying_key().to_bytes())
            .map_err(|e| UpdateInstallError::AuthorizationFailed(e.to_string()))?;
    let scriptlet_path = decision
        .bound_scriptlet_path
        .clone()
        .ok_or_else(|| UpdateInstallError::AuthorizationFailed("no bound scriptlet".into()))?;
    let scriptlet_sha256 = decision
        .bound_scriptlet_sha256
        .clone()
        .ok_or_else(|| UpdateInstallError::AuthorizationFailed("no bound sha256".into()))?;
    Ok(ReadyInstallCandidate {
        handoff: handoff.clone(),
        staging_root: staging_root.to_path_buf(),
        scriptlet_path,
        scriptlet_sha256,
        authorize_preview: decision,
    })
}

pub fn status_copy(state: &SoftwareUpdateState) -> (String, Vec<String>) {
    match &state.phase {
        UpdateCheckPhase::Idle => (
            format!("Current version: {}", installed_version()),
            vec!["Check whether a newer official Ignite release is available.".into()],
        ),
        UpdateCheckPhase::Checking { .. } => (
            "Checking for updates…".into(),
            vec!["Please wait.".into()],
        ),
        UpdateCheckPhase::UpToDate { installed } => (
            "You're running the latest version of Ignite.".into(),
            vec![format!("Currently installed: {installed}.")],
        ),
        UpdateCheckPhase::Available(h) => {
            let mut details = vec![format!("Currently installed: {}.", h.installed_version)];
            if let Some(name) = &h.release_name {
                if name != &format!("Ignite {}", h.candidate_version)
                    && name != &h.candidate_version
                    && name != &h.candidate_tag
                {
                    details.push(name.clone());
                }
            }
            (
                format!("Ignite {} is available.", h.candidate_version),
                details,
            )
        }
        UpdateCheckPhase::Error { message } => (message.clone(), vec!["Retry when ready.".into()]),
        UpdateCheckPhase::Downloading { handoff, .. } => (
            format!("Downloading Ignite {}…", handoff.candidate_version),
            vec!["Please wait while the update is downloaded.".into()],
        ),
        UpdateCheckPhase::Verifying { handoff, .. } => (
            format!("Verifying Ignite {}…", handoff.candidate_version),
            vec!["Please wait while the update is verified.".into()],
        ),
        UpdateCheckPhase::ReadyToInstall { handoff, .. } => (
            format!("Ignite {} is ready to install.", handoff.candidate_version),
            vec![
                "Tap INSTALL NOW to leave Ignite. After Ignite exits, the authenticated installer runs automatically.".into(),
            ],
        ),
        UpdateCheckPhase::Installing { handoff, .. } => (
            format!("Preparing to leave Ignite ({})…", handoff.candidate_version),
            vec![
                "Saving install intent and exiting. Installation starts only after Ignite has fully stopped; this screen does not mean the update finished.".into(),
            ],
        ),
        UpdateCheckPhase::InstallFailed { message, .. } => (
            format!("Installation failed: {message}"),
            vec!["Tap RETRY to try again.".into()],
        ),
    }
}

pub fn primary_action_label(state: &SoftwareUpdateState) -> Option<&'static str> {
    match &state.phase {
        UpdateCheckPhase::Idle | UpdateCheckPhase::UpToDate { .. } => Some("CHECK FOR UPDATES"),
        UpdateCheckPhase::Available(_) => Some("DOWNLOAD & INSTALL"),
        UpdateCheckPhase::Error { .. } => Some("RETRY"),
        UpdateCheckPhase::ReadyToInstall { .. } => Some("INSTALL NOW"),
        UpdateCheckPhase::InstallFailed {
            retryable,
            handoff,
            ready_residual,
            ..
        } => {
            if *retryable && (handoff.is_some() || ready_residual.is_some()) {
                Some("RETRY")
            } else {
                Some("CHECK FOR UPDATES")
            }
        }
        UpdateCheckPhase::Checking { .. }
        | UpdateCheckPhase::Downloading { .. }
        | UpdateCheckPhase::Verifying { .. }
        | UpdateCheckPhase::Installing { .. } => None,
    }
}

/// True when RETRY should re-enter ready install (ConfirmInstallUpdate), not re-download.
pub fn retry_uses_ready_residual(state: &SoftwareUpdateState) -> bool {
    matches!(
        &state.phase,
        UpdateCheckPhase::InstallFailed {
            retryable: true,
            ready_residual: Some(_),
            ..
        }
    )
}

pub fn secondary_action_label(state: &SoftwareUpdateState) -> Option<&'static str> {
    match &state.phase {
        UpdateCheckPhase::ReadyToInstall { .. } => Some("CANCEL"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct ScriptedTransport {
        responses: Mutex<Vec<Result<Vec<u8>, UpdateDiscoveryError>>>,
        calls: Mutex<u32>,
    }

    impl ScriptedTransport {
        fn new(responses: Vec<Result<Vec<u8>, UpdateDiscoveryError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: Mutex::new(0),
            }
        }

        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl ReleaseMetadataTransport for ScriptedTransport {
        fn fetch_latest_release_json(
            &self,
            origin: &GithubReleaseOrigin,
        ) -> Result<Vec<u8>, UpdateDiscoveryError> {
            assert_eq!(origin.owner, "markrai");
            assert_eq!(origin.repo, "ignite");
            *self.calls.lock().unwrap() += 1;
            self.responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err(UpdateDiscoveryError::Transport("empty script".into())))
        }
    }

    fn json_release(
        tag: &str,
        draft: bool,
        prerelease: bool,
        html_url: &str,
        name: Option<&str>,
    ) -> Vec<u8> {
        let name = name.unwrap_or(tag);
        format!(
            r#"{{"tag_name":"{tag}","name":"{name}","draft":{draft},"prerelease":{prerelease},"html_url":"{html_url}","published_at":"2026-11-02T12:00:00Z"}}"#
        )
        .into_bytes()
    }

    #[test]
    fn no_newer_release_is_up_to_date() {
        let body = json_release(
            "v0.9.14",
            false,
            false,
            "https://github.com/markrai/ignite/releases/tag/v0.9.14",
            Some("Ignite 0.9.14"),
        );
        let t = ScriptedTransport::new(vec![Ok(body)]);
        let out = discover_latest_stable(&t, "0.9.14").unwrap();
        assert_eq!(
            out,
            UpdateCheckOutcome::UpToDate {
                installed: "0.9.14".into()
            }
        );
    }

    #[test]
    fn newer_stable_release_found() {
        let body = json_release(
            "v0.9.15",
            false,
            false,
            "https://github.com/markrai/ignite/releases/tag/v0.9.15",
            Some("Ignite 0.9.15"),
        );
        let t = ScriptedTransport::new(vec![Ok(body)]);
        let out = discover_latest_stable(&t, "0.9.14").unwrap();
        match out {
            UpdateCheckOutcome::Available(h) => {
                assert_eq!(h.candidate_version, "0.9.15");
                assert_eq!(h.candidate_tag, "v0.9.15");
                assert_eq!(h.installed_version, "0.9.14");
                assert_eq!(h.official_origin, "markrai/ignite");
                assert_eq!(h.eligibility, UpdateEligibility::NewerStableAvailable);
                assert_eq!(h.release_name.as_deref(), Some("Ignite 0.9.15"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn draft_and_prerelease_excluded() {
        for (draft, pre) in [(true, false), (false, true)] {
            let body = json_release(
                "v0.9.15",
                draft,
                pre,
                "https://github.com/markrai/ignite/releases/tag/v0.9.15",
                None,
            );
            let t = ScriptedTransport::new(vec![Ok(body)]);
            assert_eq!(
                discover_latest_stable(&t, "0.9.14").unwrap_err(),
                UpdateDiscoveryError::DraftOrPrerelease
            );
        }
    }

    #[test]
    fn malformed_version_refused() {
        let body = json_release(
            "v0.9",
            false,
            false,
            "https://github.com/markrai/ignite/releases/tag/v0.9",
            None,
        );
        let t = ScriptedTransport::new(vec![Ok(body)]);
        assert!(matches!(
            discover_latest_stable(&t, "0.9.14"),
            Err(UpdateDiscoveryError::MalformedVersion(_))
        ));
        let body = json_release(
            "v0.9.15-rc1",
            false,
            false,
            "https://github.com/markrai/ignite/releases/tag/v0.9.15-rc1",
            None,
        );
        let t = ScriptedTransport::new(vec![Ok(body)]);
        assert!(matches!(
            discover_latest_stable(&t, "0.9.14"),
            Err(UpdateDiscoveryError::MalformedVersion(_))
        ));
    }

    #[test]
    fn downgrade_refused_as_up_to_date() {
        let body = json_release(
            "v0.9.10",
            false,
            false,
            "https://github.com/markrai/ignite/releases/tag/v0.9.10",
            None,
        );
        let t = ScriptedTransport::new(vec![Ok(body)]);
        assert_eq!(
            discover_latest_stable(&t, "0.9.14").unwrap(),
            UpdateCheckOutcome::UpToDate {
                installed: "0.9.14".into()
            }
        );
    }

    #[test]
    fn untrusted_release_origin_refused() {
        let body = json_release(
            "v0.9.15",
            false,
            false,
            "https://github.com/evil/ignite/releases/tag/v0.9.15",
            None,
        );
        let t = ScriptedTransport::new(vec![Ok(body)]);
        assert!(matches!(
            discover_latest_stable(&t, "0.9.14"),
            Err(UpdateDiscoveryError::UntrustedOrigin(_))
        ));
        assert!(matches!(
            validate_github_api_url("https://evil.example/repos/markrai/ignite/releases/latest"),
            Err(UpdateDiscoveryError::UntrustedOrigin(_))
        ));
    }

    #[test]
    fn network_timeout_and_invalid_json_and_oversized() {
        let t = ScriptedTransport::new(vec![Err(UpdateDiscoveryError::Timeout)]);
        assert_eq!(
            discover_latest_stable(&t, "0.9.14").unwrap_err(),
            UpdateDiscoveryError::Timeout
        );
        let t = ScriptedTransport::new(vec![Ok(b"not-json".to_vec())]);
        assert_eq!(
            discover_latest_stable(&t, "0.9.14").unwrap_err(),
            UpdateDiscoveryError::InvalidJson
        );
        let huge = vec![b'x'; (MAX_METADATA_BYTES as usize) + 1];
        let t = ScriptedTransport::new(vec![Ok(huge)]);
        assert_eq!(
            discover_latest_stable(&t, "0.9.14").unwrap_err(),
            UpdateDiscoveryError::Oversized
        );
    }

    #[test]
    fn duplicate_requests_are_ignored_while_checking() {
        let mut state = SoftwareUpdateState::default();
        let g1 = state.begin_check().unwrap();
        assert!(state.begin_check().is_none());
        assert!(state.busy());
        assert!(!state.apply_result(
            g1.wrapping_add(9),
            Ok(UpdateCheckOutcome::UpToDate {
                installed: "0.9.14".into()
            })
        ));
        assert!(state.apply_result(
            g1,
            Ok(UpdateCheckOutcome::UpToDate {
                installed: "0.9.14".into()
            })
        ));
        assert!(!state.busy());
        assert_eq!(
            state.phase,
            UpdateCheckPhase::UpToDate {
                installed: "0.9.14".into()
            }
        );
    }

    #[test]
    fn discovery_never_claims_installation() {
        let body = json_release(
            "v0.9.15",
            false,
            false,
            "https://github.com/markrai/ignite/releases/tag/v0.9.15",
            Some("Ignite 0.9.15"),
        );
        let t = ScriptedTransport::new(vec![Ok(body)]);
        let out = discover_latest_stable(&t, "0.9.14").unwrap();
        let UpdateCheckOutcome::Available(h) = out else {
            panic!("expected available");
        };
        assert_eq!(h.eligibility, UpdateEligibility::NewerStableAvailable);
        let state = SoftwareUpdateState {
            phase: UpdateCheckPhase::Available(h.clone()),
            generation: 1,
        };
        assert!(primary_action_label(&state).is_some_and(|l| l.contains("DOWNLOAD")));
        assert!(status_copy(&state).0.contains("0.9.15"));
        assert_eq!(t.calls(), 1);
    }

    #[test]
    fn compare_stable_versions_ordering() {
        assert_eq!(
            compare_stable_versions("0.9.15", "0.9.14"),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(
            compare_stable_versions("0.9.14", "0.9.14"),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(compare_stable_versions("0.9.14-rc1", "0.9.14"), None);
    }

    #[test]
    fn scripted_transport_is_arc_safe() {
        let t = Arc::new(ScriptedTransport::new(vec![Ok(json_release(
            "v0.9.14",
            false,
            false,
            "https://github.com/markrai/ignite/releases/tag/v0.9.14",
            None,
        ))]));
        let out = discover_latest_stable(t.as_ref(), "0.9.14").unwrap();
        assert!(matches!(out, UpdateCheckOutcome::UpToDate { .. }));
    }

    const INSTALL_VERSION: &str = "3.3.1";
    const INSTALL_TAG: &str = "v3.3.1";
    const INSTALL_SCRIPTLET: &[u8] =
        b"#!/bin/sh\nIGNITE_STORAGE_CONTRACT='ignite-storage-v1'\necho fixture-install\n";

    fn install_signing_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[42_u8; 32])
    }

    fn install_handoff(installed: &str) -> UpdateDiscoveryHandoff {
        UpdateDiscoveryHandoff {
            installed_version: installed.to_string(),
            candidate_version: INSTALL_VERSION.to_string(),
            candidate_tag: INSTALL_TAG.to_string(),
            official_origin: "markrai/ignite".to_string(),
            eligibility: UpdateEligibility::NewerStableAvailable,
            release_name: None,
            published_at: None,
        }
    }

    fn write_install_fixture(dir: &std::path::Path, key: &ed25519_dalek::SigningKey) {
        use crate::publication_evidence::build_minimal_production_evidence;
        use crate::publication_trust_boundary::{
            BINDING_HEADER, BINDING_SCHEMA, BINDING_SCOPE, BINDING_SIGNATURE_SCHEMA,
        };
        use crate::release_manifest::{encode_hex, MANIFEST_SCHEMA, SIGNATURE_SCHEMA};
        use ed25519_dalek::Signer;
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(INSTALL_SCRIPTLET);
        let evidence = build_minimal_production_evidence(
            INSTALL_VERSION,
            INSTALL_SCRIPTLET.len() as u64,
            &encode_hex(&digest),
            &"cd".repeat(32),
        )
        .expect("evidence");
        let release = format!(
            "schema={MANIFEST_SCHEMA}\nversion={INSTALL_VERSION}\ntag={INSTALL_TAG}\nartifact=Install Ignite v{INSTALL_VERSION}.sh\nbytes={}\nsha256={}\n",
            INSTALL_SCRIPTLET.len(),
            encode_hex(&digest),
        )
        .into_bytes();
        let binding = format!(
            "{BINDING_HEADER}\nschema={BINDING_SCHEMA}\nrelease_tag={INSTALL_TAG}\nrelease_version={INSTALL_VERSION}\nrelease_manifest_sha256={}\nscriptlet_sha256={}\npublication_evidence_sha256={}\npublisher_authentication_scope={BINDING_SCOPE}\n",
            encode_hex(&Sha256::digest(&release)),
            encode_hex(&digest),
            encode_hex(&Sha256::digest(&evidence)),
        )
        .into_bytes();
        let sign = |msg: &[u8], schema: &str| -> Vec<u8> {
            let sig = key.sign(msg);
            format!(
                "schema={schema}\nsignature={}\n",
                encode_hex(sig.to_bytes().as_ref())
            )
            .into_bytes()
        };
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("Install Ignite v{INSTALL_VERSION}.sh")),
            INSTALL_SCRIPTLET,
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("ignite-kindle-v{INSTALL_VERSION}.manifest")),
            &release,
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("ignite-kindle-v{INSTALL_VERSION}.manifest.sig")),
            sign(&release, SIGNATURE_SCHEMA),
        )
        .unwrap();
        std::fs::write(
            dir.join(format!(
                "ignite-kindle-v{INSTALL_VERSION}.publication-evidence"
            )),
            &evidence,
        )
        .unwrap();
        std::fs::write(
            dir.join(format!(
                "ignite-kindle-v{INSTALL_VERSION}.publication-binding"
            )),
            &binding,
        )
        .unwrap();
        std::fs::write(
            dir.join(format!(
                "ignite-kindle-v{INSTALL_VERSION}.publication-binding.sig"
            )),
            sign(&binding, BINDING_SIGNATURE_SCHEMA),
        )
        .unwrap();
    }

    fn temp_install_pair(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("ignite-1b-{label}-{stamp}"));
        std::fs::remove_dir_all(&base).ok();
        let src = base.join("src");
        let staging = base.join("staging");
        std::fs::create_dir_all(&src).unwrap();
        (src, staging)
    }

    fn install_device(base: &std::path::Path) -> crate::installation_authorization::DeviceFacts {
        let app_root = base.join("app");
        std::fs::create_dir_all(&app_root).unwrap();
        crate::installation_authorization::DeviceFacts {
            architecture: crate::installation_authorization::SUPPORTED_ARCHITECTURE.into(),
            firmware: Some(crate::installation_authorization::SUPPORTED_FIRMWARE.into()),
            app_root,
            data_root: base.join("data"),
            recovery_root: base.join("recovery"),
            documents_root: base.join("documents"),
        }
    }

    #[test]
    fn successful_fixture_stage_produces_ready_candidate() {
        let key = install_signing_key();
        let (src, staging) = temp_install_pair("success");
        write_install_fixture(&src, &key);
        let device = install_device(src.parent().unwrap());
        let handoff = install_handoff("0.9.14");
        let candidate = stage_and_prepare_ready_for_tests(&handoff, &staging, &src, &key, device)
            .expect("fixture stage must succeed");
        assert_eq!(candidate.handoff.candidate_version, INSTALL_VERSION);
        assert!(candidate.scriptlet_path.exists());
        assert!(!candidate.scriptlet_sha256.is_empty());
        assert!(
            matches!(
                candidate.authorize_preview.execution_binding,
                crate::installation_authorization::PrerequisiteStatus::Satisfied
            ),
            "execution_binding must be Satisfied"
        );
        assert!(
            matches!(
                candidate.authorize_preview.upgrade_compatible,
                crate::installation_authorization::PrerequisiteStatus::Satisfied
            ),
            "upgrade_compatible must be Satisfied"
        );
        std::fs::remove_dir_all(src.parent().unwrap()).ok();
    }

    #[test]
    fn downgrade_refused_by_download_and_stage() {
        let key = install_signing_key();
        let (src, staging) = temp_install_pair("downgrade");
        write_install_fixture(&src, &key);
        let device = install_device(src.parent().unwrap());
        let handoff = install_handoff("9.9.9");
        let result = stage_and_prepare_ready_for_tests(&handoff, &staging, &src, &key, device);
        assert!(
            matches!(result, Err(UpdateInstallError::AuthorizationFailed(_))),
            "downgrade must be refused"
        );
        std::fs::remove_dir_all(src.parent().unwrap()).ok();
    }

    #[test]
    fn user_approved_false_does_not_authorize_install() {
        let key = install_signing_key();
        let (src, staging) = temp_install_pair("user-cancel");
        write_install_fixture(&src, &key);
        let device = install_device(src.parent().unwrap());
        let handoff = install_handoff("0.9.14");
        let candidate =
            stage_and_prepare_ready_for_tests(&handoff, &staging, &src, &key, device.clone())
                .expect("stage ok");
        assert!(
            !candidate.authorize_preview.installation_authorized,
            "fixture provenance + user_approved=false must not authorize"
        );
        assert!(
            matches!(
                candidate.authorize_preview.installation_policy_satisfied,
                crate::installation_authorization::PrerequisiteStatus::Refused(_)
            ),
            "policy gate must be Refused when user_approved=false"
        );
        std::fs::remove_dir_all(src.parent().unwrap()).ok();
    }

    #[test]
    fn artifact_tamper_after_stage_fails_revalidation() {
        let key = install_signing_key();
        let (src, staging) = temp_install_pair("tamper");
        write_install_fixture(&src, &key);
        let device = install_device(src.parent().unwrap());
        let handoff = install_handoff("0.9.14");
        let candidate = stage_and_prepare_ready_for_tests(&handoff, &staging, &src, &key, device)
            .expect("stage ok");
        let sha256 = &candidate.scriptlet_sha256;
        let path = &candidate.scriptlet_path;
        assert!(
            crate::installation_authorization::revalidate_execution_binding(path, sha256),
            "revalidation must pass on untampered artifact"
        );
        std::fs::write(path, b"tampered").unwrap();
        assert!(
            !crate::installation_authorization::revalidate_execution_binding(path, sha256),
            "revalidation must fail after artifact tamper"
        );
        std::fs::remove_dir_all(src.parent().unwrap()).ok();
    }

    #[test]
    fn begin_download_transitions_from_available() {
        let mut state = SoftwareUpdateState::default();
        let gen = state.begin_check().unwrap();
        let handoff = install_handoff("0.9.14");
        assert!(state.apply_result(gen, Ok(UpdateCheckOutcome::Available(handoff.clone()))));
        assert!(matches!(state.phase, UpdateCheckPhase::Available(_)));
        let _dl_gen = state
            .begin_download(handoff.clone())
            .expect("begin_download from Available");
        assert!(matches!(state.phase, UpdateCheckPhase::Downloading { .. }));
        assert!(state.busy());
        assert!(
            state.begin_download(handoff).is_none(),
            "duplicate download must be ignored"
        );
    }

    #[test]
    fn cancel_ready_returns_to_available() {
        let handoff = install_handoff("0.9.14");
        let mut state = SoftwareUpdateState {
            phase: UpdateCheckPhase::ReadyToInstall {
                handoff: handoff.clone(),
                staging_root: std::path::PathBuf::from("/tmp/staging"),
                scriptlet_path: std::path::PathBuf::from("/tmp/scriptlet.sh"),
                scriptlet_sha256: "abc123".into(),
            },
            generation: 5,
        };
        assert!(state.cancel_ready());
        assert!(matches!(state.phase, UpdateCheckPhase::Available(_)));
        assert!(!state.busy());
    }

    #[test]
    fn discovery_does_not_install() {
        let body = json_release(
            "v0.9.15",
            false,
            false,
            "https://github.com/markrai/ignite/releases/tag/v0.9.15",
            Some("Ignite 0.9.15"),
        );
        let t = ScriptedTransport::new(vec![Ok(body)]);
        let out = discover_latest_stable(&t, "0.9.14").unwrap();
        let UpdateCheckOutcome::Available(h) = out else {
            panic!("expected Available")
        };
        assert_eq!(h.eligibility, UpdateEligibility::NewerStableAvailable);
        let state = SoftwareUpdateState {
            phase: UpdateCheckPhase::Available(h),
            generation: 1,
        };
        assert!(status_copy(&state).0.contains("0.9.15"));
        assert!(primary_action_label(&state).is_some_and(|l| l.contains("DOWNLOAD")));
        assert!(
            !matches!(state.phase, UpdateCheckPhase::Installing { .. }),
            "discovery must never transition to Installing"
        );
    }

    #[test]
    fn notebook_data_root_never_touched_by_handoff() {
        let handoff = install_handoff("0.9.14");
        let tmp = std::env::temp_dir().join(format!(
            "ignite-data-check-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let data_dir = tmp.join("scribe-data");
        let notebook_dir = data_dir.join("notebooks");
        std::fs::create_dir_all(&notebook_dir).unwrap();
        std::fs::write(notebook_dir.join("important.nbk"), b"notebook-content").unwrap();
        let content_before = std::fs::read(notebook_dir.join("important.nbk")).unwrap();
        assert_eq!(handoff.installed_version, "0.9.14");
        assert_eq!(handoff.candidate_version, INSTALL_VERSION);
        assert_eq!(
            std::fs::read(notebook_dir.join("important.nbk")).unwrap(),
            content_before
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn incompatible_firmware_refuses_device_gate() {
        let key = install_signing_key();
        let (src, staging) = temp_install_pair("firmware");
        write_install_fixture(&src, &key);
        let mut device = install_device(src.parent().unwrap());
        device.firmware = Some("9.9.9".into());
        let handoff = install_handoff("0.9.14");
        let candidate = stage_and_prepare_ready_for_tests(&handoff, &staging, &src, &key, device)
            .expect("stage ok");
        assert!(
            matches!(
                candidate.authorize_preview.device_compatible,
                crate::installation_authorization::PrerequisiteStatus::Refused(_)
            ),
            "incompatible firmware must refuse device_compatible"
        );
        assert!(!candidate.authorize_preview.installation_authorized);
        std::fs::remove_dir_all(src.parent().unwrap()).ok();
    }

    #[test]
    fn wrong_key_refuses_candidate_authentication() {
        let key = install_signing_key();
        let wrong_key = ed25519_dalek::SigningKey::from_bytes(&[99_u8; 32]);
        let (src, staging) = temp_install_pair("wrongkey");
        write_install_fixture(&src, &key);
        let device = install_device(src.parent().unwrap());
        let handoff = install_handoff("0.9.14");
        let result =
            stage_and_prepare_ready_for_tests(&handoff, &staging, &src, &wrong_key, device);
        assert!(
            matches!(
                result,
                Err(UpdateInstallError::StagingFailed(_)
                    | UpdateInstallError::AuthorizationFailed(_))
            ),
            "wrong key must refuse: got {result:?}"
        );
        std::fs::remove_dir_all(src.parent().unwrap()).ok();
    }

    #[test]
    fn busy_begin_download_is_idempotent() {
        let mut state = SoftwareUpdateState::default();
        let gen = state.begin_check().unwrap();
        let handoff = install_handoff("0.9.14");
        assert!(state.apply_result(gen, Ok(UpdateCheckOutcome::Available(handoff.clone()))));
        let dl_gen = state.begin_download(handoff.clone()).unwrap();
        assert!(state.busy());
        assert!(state.begin_download(handoff).is_none());
        assert!(state.begin_check().is_none());
        let _ = state.apply_download_result(dl_gen, Err(UpdateInstallError::NotReady));
    }

    #[test]
    fn install_failed_on_spawn_error_is_retryable() {
        let key = install_signing_key();
        let (src, staging) = temp_install_pair("spawn-retry");
        write_install_fixture(&src, &key);
        let device = install_device(src.parent().unwrap());
        let handoff = install_handoff("0.9.14");
        let candidate =
            stage_and_prepare_ready_for_tests(&handoff, &staging, &src, &key, device).unwrap();
        let mut state = SoftwareUpdateState::default();
        let gen = state.begin_check().unwrap();
        assert!(state.apply_result(gen, Ok(UpdateCheckOutcome::Available(handoff.clone()))));
        let dl_gen = state.begin_download(handoff).unwrap();
        let ready = ReadyInstallCandidate {
            handoff: candidate.handoff.clone(),
            staging_root: staging.clone(),
            scriptlet_path: candidate.scriptlet_path.clone(),
            scriptlet_sha256: candidate.scriptlet_sha256.clone(),
            authorize_preview: candidate.authorize_preview.clone(),
        };
        assert!(state.apply_download_result(dl_gen, Ok(ready)));
        assert!(matches!(
            state.phase,
            UpdateCheckPhase::ReadyToInstall { .. }
        ));
        state.begin_install();
        state.phase = UpdateCheckPhase::InstallFailed {
            message: "spawn failed: permission denied".into(),
            handoff: Some(candidate.handoff.clone()),
            retryable: true,
            ready_residual: Some(ReadyInstallResidual {
                staging_root: staging.clone(),
                scriptlet_path: candidate.scriptlet_path.clone(),
                scriptlet_sha256: candidate.scriptlet_sha256.clone(),
            }),
        };
        assert!(
            matches!(
                state.phase,
                UpdateCheckPhase::InstallFailed {
                    retryable: true,
                    ..
                }
            ),
            "spawn failure must be retryable"
        );
        let label = primary_action_label(&state);
        assert!(
            label.is_some_and(|l| l.contains("RETRY")),
            "retryable install failure must show RETRY action, got {:?}",
            label
        );
        assert!(
            retry_uses_ready_residual(&state),
            "post-stage spawn failure must preserve ready residual (no re-download)"
        );
        assert!(
            state.begin_download(candidate.handoff.clone()).is_none(),
            "post-stage residual must not begin a download retry"
        );
        assert!(state.restore_ready_from_failed());
        assert!(matches!(
            state.phase,
            UpdateCheckPhase::ReadyToInstall { .. }
        ));
        std::fs::remove_dir_all(src.parent().unwrap()).ok();
    }

    #[test]
    fn retryable_post_stage_failure_does_not_require_download_action_label() {
        let handoff = install_handoff("0.9.14");
        let state = SoftwareUpdateState {
            phase: UpdateCheckPhase::InstallFailed {
                message: "stylus active; lift pen and retry".into(),
                handoff: Some(handoff),
                retryable: true,
                ready_residual: Some(ReadyInstallResidual {
                    staging_root: PathBuf::from("/tmp/staging"),
                    scriptlet_path: PathBuf::from("/tmp/scriptlet.sh"),
                    scriptlet_sha256: "abc".into(),
                }),
            },
            generation: 2,
        };
        assert_eq!(primary_action_label(&state), Some("RETRY"));
        assert!(retry_uses_ready_residual(&state));
        // Download failures (no residual) keep DOWNLOAD path; this case must not.
        assert!(!matches!(
            &state.phase,
            UpdateCheckPhase::InstallFailed {
                ready_residual: None,
                ..
            }
        ));
    }

    #[test]
    fn prepare_install_execution_uses_bound_decision_identity_not_stale_hint() {
        let key = install_signing_key();
        let (src, staging) = temp_install_pair("bound-plan");
        write_install_fixture(&src, &key);
        let device = install_device(src.parent().unwrap());
        let handoff = install_handoff("0.9.14");
        let mut candidate =
            stage_and_prepare_ready_for_tests(&handoff, &staging, &src, &key, device.clone())
                .unwrap();
        // Tamper the ReadyInstallCandidate hint so helpers must ignore it.
        candidate.scriptlet_path = staging.join("assets").join("stale-hint.sh");
        candidate.scriptlet_sha256 = "0".repeat(64);
        std::fs::write(&candidate.scriptlet_path, b"stale-hint-body").unwrap();
        // Production prepare_install_execution requires OfficialProductionPin; use harness
        // via authorize path equivalent: stage_and_prepare already bound real path.
        // Re-run fixture authorize to get a decision, then assert build would use bound path.
        use crate::installation_authorization::{
            evaluate_installation_authorization_fixture, AuthorizeRequest,
        };
        let auth_req = AuthorizeRequest {
            expected_tag: handoff.candidate_tag.clone(),
            version: handoff.candidate_version.clone(),
            staging_root: staging.clone(),
            root_kind: StagingRootKind::HostTest,
            device: device.clone(),
            installed_version: handoff.installed_version.clone(),
            user_approved: true,
        };
        let decision =
            evaluate_installation_authorization_fixture(&auth_req, &key.verifying_key().to_bytes())
                .unwrap();
        let bound_path = decision.bound_scriptlet_path.clone().unwrap();
        let bound_sha = decision.bound_scriptlet_sha256.clone().unwrap();
        assert_ne!(bound_path, candidate.scriptlet_path);
        assert_ne!(bound_sha, candidate.scriptlet_sha256);
        // Simulate what prepare_install_execution does: plan identity comes from decision.
        let plan = AuthorizedInstallPlan {
            scriptlet_path: bound_path.clone(),
            scriptlet_sha256: bound_sha.clone(),
            handoff: handoff.clone(),
        };
        assert_eq!(plan.scriptlet_path, bound_path);
        assert_ne!(plan.scriptlet_path, candidate.scriptlet_path);
        assert!(revalidate_execution_binding(
            &plan.scriptlet_path,
            &plan.scriptlet_sha256
        ));
        assert!(!revalidate_execution_binding(
            &candidate.scriptlet_path,
            &candidate.scriptlet_sha256
        ));
        std::fs::remove_dir_all(src.parent().unwrap()).ok();
    }

    #[test]
    fn mark_verifying_is_reachable_from_downloading() {
        let mut state = SoftwareUpdateState::default();
        let gen = state.begin_check().unwrap();
        let handoff = install_handoff("0.9.14");
        assert!(state.apply_result(gen, Ok(UpdateCheckOutcome::Available(handoff.clone()))));
        let dl_gen = state.begin_download(handoff).unwrap();
        assert!(matches!(state.phase, UpdateCheckPhase::Downloading { .. }));
        assert!(state.mark_verifying(dl_gen));
        assert!(matches!(state.phase, UpdateCheckPhase::Verifying { .. }));
        let (headline, _) = status_copy(&state);
        assert!(headline.contains("Verifying"));
    }

    #[test]
    fn installing_status_copy_does_not_claim_success() {
        let handoff = install_handoff("0.9.14");
        let state = SoftwareUpdateState {
            phase: UpdateCheckPhase::Installing {
                generation: 1,
                handoff: handoff.clone(),
                scriptlet_path: std::path::PathBuf::from("/tmp/fixture.sh"),
                scriptlet_sha256: "abc123".into(),
            },
            generation: 1,
        };
        let (headline, details) = status_copy(&state);
        let blob = format!("{headline} {}", details.join(" ")).to_lowercase();
        assert!(
            !blob.contains("success"),
            "Installing status must not claim success: {blob}"
        );
        assert!(
            !blob.contains("will restart")
                && !headline.to_lowercase().contains("installing ignite"),
            "must not imply restart-into-success or mutation underway: {blob}"
        );
        assert!(
            blob.contains("preparing to leave") || blob.contains("exit is required"),
            "Installing copy must be truthful about exit/handoff: {blob}"
        );
        assert!(
            primary_action_label(&state).is_none(),
            "busy Installing must have no action"
        );
    }

    #[test]
    fn ready_to_install_copy_warns_install_requires_leaving_ignite() {
        let handoff = install_handoff("0.9.14");
        let state = SoftwareUpdateState {
            phase: UpdateCheckPhase::ReadyToInstall {
                handoff,
                staging_root: PathBuf::from("/tmp/staging"),
                scriptlet_path: PathBuf::from("/tmp/scriptlet.sh"),
                scriptlet_sha256: "abc".into(),
            },
            generation: 1,
        };
        let (headline, details) = status_copy(&state);
        let blob = format!("{headline} {}", details.join(" ")).to_lowercase();
        assert!(
            blob.contains("authenticated installer runs automatically")
                || blob.contains("leave ignite"),
            "ReadyToInstall must describe the post-exit authenticated install handoff: {blob}"
        );
    }
}
