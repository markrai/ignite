//! Software Update discovery for Settings → Preferences.
//!
//! GitHub Releases metadata is advisory only. It never authenticates an installer.
//! Installation authority remains the production signing pin and signed-release path.

use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use crate::release_acquisition::{
    parse_https_url, AcquisitionError, GithubReleaseOrigin,
};
use crate::release_manifest::valid_release_version;
use crate::settings_ui::product_version_digits;

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
        matches!(self.phase, UpdateCheckPhase::Checking { .. })
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

    pub fn handoff(&self) -> Option<&UpdateDiscoveryHandoff> {
        match &self.phase {
            UpdateCheckPhase::Available(h) => Some(h),
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
        let tmp = std::env::temp_dir().join(format!(
            "ignite-update-meta-{}.json",
            std::process::id()
        ));
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
            return Err(UpdateDiscoveryError::Transport(if stderr.trim().is_empty() {
                format!("curl failed ({code_txt})")
            } else {
                stderr.trim().to_string()
            }));
        }
        let code: u32 = code_txt
            .parse()
            .map_err(|_| UpdateDiscoveryError::Transport(format!("bad http code {code_txt}")))?;
        if code != 200 {
            let _ = std::fs::remove_file(&tmp);
            return Err(UpdateDiscoveryError::HttpStatus(code));
        }
        let bytes = std::fs::read(&tmp).map_err(|e| UpdateDiscoveryError::Transport(e.to_string()))?;
        let _ = std::fs::remove_file(&tmp);
        if bytes.len() as u64 > self.size_limit {
            return Err(UpdateDiscoveryError::Oversized);
        }
        Ok(bytes)
    }
}

pub fn official_origin() -> Result<GithubReleaseOrigin, UpdateDiscoveryError> {
    GithubReleaseOrigin::parse(OFFICIAL_ORIGIN_SPEC)
        .map_err(UpdateDiscoveryError::Acquisition)
}

pub fn latest_release_api_url(origin: &GithubReleaseOrigin) -> Result<String, UpdateDiscoveryError> {
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
        return Err(UpdateDiscoveryError::MalformedVersion(candidate.to_string()));
    }
    match compare_stable_versions(candidate, installed_version) {
        None => Err(UpdateDiscoveryError::MalformedVersion(format!(
            "{candidate} vs {installed_version}"
        ))),
        Some(std::cmp::Ordering::Greater) => Ok(UpdateCheckOutcome::Available(
            UpdateDiscoveryHandoff {
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
            },
        )),
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
    }
}

pub fn primary_action_label(state: &SoftwareUpdateState) -> Option<&'static str> {
    match &state.phase {
        UpdateCheckPhase::Idle | UpdateCheckPhase::UpToDate { .. } | UpdateCheckPhase::Available(_) => {
            Some("CHECK FOR UPDATES")
        }
        UpdateCheckPhase::Error { .. } => Some("RETRY"),
        UpdateCheckPhase::Checking { .. } => None,
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
        assert!(primary_action_label(&state).is_some_and(|l| l.contains("CHECK")));
        assert!(primary_action_label(&state).is_some_and(|l| !l.contains("INSTALL")));
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
}
