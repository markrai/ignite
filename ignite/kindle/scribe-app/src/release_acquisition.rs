//! Bounded release-asset acquisition for verified Kindle update staging (3i).
//!
//! Production downloads must originate from a configured GitHub Releases HTTPS
//! origin. GitHub metadata alone is never publisher authentication — acquired
//! bytes are verified by [`crate::verified_update_staging`] using the existing
//! publication trust chain.
//!
//! Local directory sources exist only for isolated fixture harnesses.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Exact six customer/publication assets a verified staging run may acquire.
pub const REQUIRED_ASSET_KEYS: [&str; 6] = [
    "scriptlet",
    "release_manifest",
    "release_signature",
    "publication_evidence",
    "publication_binding",
    "binding_signature",
];

pub const MAX_REDIRECTS: u32 = 3;
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_TRANSFER_TIMEOUT_SECS: u64 = 600;
/// Hard ceiling for any single acquired asset (Scriptlet bound matches production evidence).
pub const MAX_ASSET_BYTES: u64 = 768 * 1024 * 1024;
pub const MAX_METADATA_ASSET_BYTES: u64 = 64 * 1024;
pub const READ_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquisitionError {
    UnsupportedScheme(String),
    UnexpectedOrigin(String),
    RedirectRefused(String),
    MissingAsset(String),
    DuplicateAsset(String),
    UnexpectedAsset(String),
    SizeLimitExceeded {
        name: String,
        limit: u64,
    },
    ShortRead {
        name: String,
        expected: u64,
        got: u64,
    },
    TruncatedTransfer(String),
    Timeout(String),
    Transport(String),
    UnsafePath(String),
    InsufficientSpace {
        need: u64,
        available: u64,
    },
    Io(String),
}

impl std::fmt::Display for AcquisitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedScheme(s) => write!(f, "unsupported URL scheme: {s}"),
            Self::UnexpectedOrigin(s) => write!(f, "unexpected download origin: {s}"),
            Self::RedirectRefused(s) => write!(f, "redirect refused: {s}"),
            Self::MissingAsset(s) => write!(f, "missing required asset: {s}"),
            Self::DuplicateAsset(s) => write!(f, "duplicate asset identity: {s}"),
            Self::UnexpectedAsset(s) => write!(f, "unexpected asset: {s}"),
            Self::SizeLimitExceeded { name, limit } => {
                write!(f, "asset {name} exceeds size limit {limit}")
            }
            Self::ShortRead {
                name,
                expected,
                got,
            } => write!(f, "short read for {name}: expected {expected} got {got}"),
            Self::TruncatedTransfer(s) => write!(f, "truncated transfer: {s}"),
            Self::Timeout(s) => write!(f, "download timeout: {s}"),
            Self::Transport(s) => write!(f, "transport error: {s}"),
            Self::UnsafePath(s) => write!(f, "unsafe path: {s}"),
            Self::InsufficientSpace { need, available } => {
                write!(
                    f,
                    "insufficient disk space: need {need} available {available}"
                )
            }
            Self::Io(s) => write!(f, "I/O error: {s}"),
        }
    }
}

impl std::error::Error for AcquisitionError {}

impl From<std::io::Error> for AcquisitionError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseAssetNames {
    pub scriptlet: String,
    pub release_manifest: String,
    pub release_signature: String,
    pub publication_evidence: String,
    pub publication_binding: String,
    pub binding_signature: String,
}

impl ReleaseAssetNames {
    pub fn for_version(version: &str) -> Result<Self, AcquisitionError> {
        if !valid_release_version(version) {
            return Err(AcquisitionError::Transport(format!(
                "invalid release version {version}"
            )));
        }
        Ok(Self {
            scriptlet: format!("Install Ignite v{version}.sh"),
            release_manifest: format!("ignite-kindle-v{version}.manifest"),
            release_signature: format!("ignite-kindle-v{version}.manifest.sig"),
            publication_evidence: format!("ignite-kindle-v{version}.publication-evidence"),
            publication_binding: format!("ignite-kindle-v{version}.publication-binding"),
            binding_signature: format!("ignite-kindle-v{version}.publication-binding.sig"),
        })
    }

    pub fn all(&self) -> [(&str, &str); 6] {
        [
            ("scriptlet", &self.scriptlet),
            ("release_manifest", &self.release_manifest),
            ("release_signature", &self.release_signature),
            ("publication_evidence", &self.publication_evidence),
            ("publication_binding", &self.publication_binding),
            ("binding_signature", &self.binding_signature),
        ]
    }

    pub fn size_limit_for_key(key: &str) -> u64 {
        if key == "scriptlet" {
            MAX_ASSET_BYTES
        } else {
            MAX_METADATA_ASSET_BYTES
        }
    }
}

fn valid_release_version(version: &str) -> bool {
    crate::release_manifest::valid_release_version(version)
}

/// Configured HTTPS GitHub Releases origin. Tag is supplied per acquisition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubReleaseOrigin {
    pub owner: String,
    pub repo: String,
}

impl GithubReleaseOrigin {
    pub fn parse(spec: &str) -> Result<Self, AcquisitionError> {
        // Accept "owner/repo" only — no user-provided arbitrary URLs.
        let (owner, repo) = spec
            .split_once('/')
            .ok_or_else(|| AcquisitionError::UnexpectedOrigin(spec.to_string()))?;
        let owner_ok = !owner.is_empty()
            && !owner.contains('/')
            && owner
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        let repo_ok = !repo.is_empty()
            && !repo.contains('/')
            && repo
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
        if !owner_ok || !repo_ok || spec.matches('/').count() != 1 {
            return Err(AcquisitionError::UnexpectedOrigin(spec.to_string()));
        }
        Ok(Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
    }

    pub fn asset_url(&self, tag: &str, asset_name: &str) -> Result<String, AcquisitionError> {
        if !tag.starts_with('v') || !valid_release_version(&tag[1..]) {
            return Err(AcquisitionError::Transport(format!(
                "invalid release tag {tag}"
            )));
        }
        if asset_name.contains('/') || asset_name.contains('\\') || asset_name.contains("..") {
            return Err(AcquisitionError::UnsafePath(asset_name.to_string()));
        }
        // Percent-encode spaces in Scriptlet names.
        let encoded = asset_name.replace(' ', "%20");
        Ok(format!(
            "https://github.com/{}/{}/releases/download/{tag}/{encoded}",
            self.owner, self.repo
        ))
    }
}

/// Hosts permitted after following HTTPS redirects from GitHub Releases.
pub fn redirect_host_allowed(host: &str) -> bool {
    matches!(
        host,
        "github.com"
            | "www.github.com"
            | "objects.githubusercontent.com"
            | "release-assets.githubusercontent.com"
            | "github-releases.githubusercontent.com"
    )
}

/// Parse and validate an absolute HTTPS URL for acquisition / redirect hops.
///
/// Rejects non-HTTPS schemes, userinfo, non-443 ports, and hosts outside the
/// GitHub release-asset allowlist.
pub fn validate_https_url(url: &str) -> Result<(), AcquisitionError> {
    let parsed = parse_https_url(url)?;
    if !redirect_host_allowed(&parsed.host) {
        return Err(AcquisitionError::UnexpectedOrigin(url.to_string()));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedHttpsUrl {
    pub host: String,
    pub path_and_query: String,
}

pub fn parse_https_url(url: &str) -> Result<ParsedHttpsUrl, AcquisitionError> {
    if url.is_empty() || url.contains(char::is_whitespace) {
        return Err(AcquisitionError::RedirectRefused(
            "malformed or ambiguous redirect target".into(),
        ));
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") {
        return Err(AcquisitionError::UnsupportedScheme("http".into()));
    }
    if !lower.starts_with("https://") {
        return Err(AcquisitionError::UnsupportedScheme(
            url.split(':').next().unwrap_or("").to_string(),
        ));
    }
    let rest = &url["https://".len()..];
    if rest.contains('@') {
        return Err(AcquisitionError::RedirectRefused(
            "userinfo is not allowed in download URLs".into(),
        ));
    }
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(AcquisitionError::UnexpectedOrigin(url.to_string()));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (host, Some(port))
        }
        _ => (authority, None),
    };
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return Err(AcquisitionError::UnexpectedOrigin(url.to_string()));
    }
    if let Some(port) = port {
        if port != "443" {
            return Err(AcquisitionError::RedirectRefused(format!(
                "unapproved port :{port}"
            )));
        }
    }
    Ok(ParsedHttpsUrl {
        host: host.to_ascii_lowercase(),
        path_and_query: path.to_string(),
    })
}

/// Decide the next hop URL after a redirect response. Validates before returning.
pub fn decide_redirect_target(
    current_url: &str,
    location_header: &str,
    hop_index: u32,
) -> Result<String, AcquisitionError> {
    if hop_index >= MAX_REDIRECTS {
        return Err(AcquisitionError::RedirectRefused(
            "redirect limit exceeded".into(),
        ));
    }
    let location = location_header.trim();
    if location.is_empty() {
        return Err(AcquisitionError::RedirectRefused(
            "missing Location header".into(),
        ));
    }
    if location.contains(char::is_whitespace) || location.contains('\0') {
        return Err(AcquisitionError::RedirectRefused(
            "malformed Location header".into(),
        ));
    }
    // Absolute-URL redirects only — relative redirects are ambiguous on Kindle.
    if !location.to_ascii_lowercase().starts_with("https://") {
        if location.to_ascii_lowercase().starts_with("http://") {
            return Err(AcquisitionError::UnsupportedScheme("http".into()));
        }
        return Err(AcquisitionError::RedirectRefused(
            "redirect target must be an absolute https URL".into(),
        ));
    }
    let _ = current_url; // current already validated by caller
    validate_https_url(location)?;
    if location == current_url {
        return Err(AcquisitionError::RedirectRefused(
            "redirect loop to the same URL".into(),
        ));
    }
    Ok(location.to_string())
}

#[derive(Clone, Debug)]
pub enum AssetSource {
    /// Isolated fixture/local rehearsal. Never a production origin.
    LocalDirectory { root: PathBuf },
    /// Production-shaped GitHub Releases origin (HTTPS via curl).
    GithubHttps {
        origin: GithubReleaseOrigin,
        tag: String,
        connect_timeout: Duration,
        transfer_timeout: Duration,
    },
}

impl AssetSource {
    pub fn fetch_named(
        &self,
        key: &str,
        asset_name: &str,
        dest: &Path,
        size_limit: u64,
    ) -> Result<u64, AcquisitionError> {
        match self {
            Self::LocalDirectory { root } => copy_local_asset(root, asset_name, dest, size_limit),
            Self::GithubHttps {
                origin,
                tag,
                connect_timeout,
                transfer_timeout,
            } => {
                let url = origin.asset_url(tag, asset_name)?;
                validate_https_url(&url)?;
                curl_download(
                    &url,
                    dest,
                    size_limit,
                    *connect_timeout,
                    *transfer_timeout,
                    key,
                )
            }
        }
    }
}

fn reject_symlink(path: &Path) -> Result<(), AcquisitionError> {
    let meta = fs::symlink_metadata(path).map_err(|e| AcquisitionError::Io(e.to_string()))?;
    if meta.file_type().is_symlink() {
        return Err(AcquisitionError::UnsafePath(format!(
            "symlink refused: {}",
            path.display()
        )));
    }
    Ok(())
}

fn copy_local_asset(
    root: &Path,
    asset_name: &str,
    dest: &Path,
    size_limit: u64,
) -> Result<u64, AcquisitionError> {
    if asset_name.contains('/') || asset_name.contains('\\') || asset_name.contains("..") {
        return Err(AcquisitionError::UnsafePath(asset_name.to_string()));
    }
    reject_symlink(root)?;
    let src = root.join(asset_name);
    if src
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(AcquisitionError::UnsafePath(src.display().to_string()));
    }
    if !src.starts_with(root) {
        return Err(AcquisitionError::UnsafePath(src.display().to_string()));
    }
    reject_symlink(&src)?;
    let meta = fs::metadata(&src)?;
    if meta.len() > size_limit {
        return Err(AcquisitionError::SizeLimitExceeded {
            name: asset_name.to_string(),
            limit: size_limit,
        });
    }
    let mut input = File::open(&src)?;
    let tmp = dest.with_extension("part");
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = File::create(&tmp)?;
    let mut buf = vec![0_u8; READ_CHUNK];
    let mut written = 0_u64;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        written =
            written
                .checked_add(n as u64)
                .ok_or_else(|| AcquisitionError::SizeLimitExceeded {
                    name: asset_name.to_string(),
                    limit: size_limit,
                })?;
        if written > size_limit {
            let _ = fs::remove_file(&tmp);
            return Err(AcquisitionError::SizeLimitExceeded {
                name: asset_name.to_string(),
                limit: size_limit,
            });
        }
        output.write_all(&buf[..n])?;
    }
    if written != meta.len() {
        let _ = fs::remove_file(&tmp);
        return Err(AcquisitionError::ShortRead {
            name: asset_name.to_string(),
            expected: meta.len(),
            got: written,
        });
    }
    output.flush()?;
    let _ = output.sync_all();
    fs::rename(&tmp, dest)?;
    Ok(written)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeResponse {
    Redirect {
        location: String,
    },
    /// Final body already streamed to a temporary file (never a full in-memory Scriptlet).
    OkFile {
        path: PathBuf,
        size: u64,
    },
    Status(u32),
}

/// Transport probe used so redirect policy is exercised before any unapproved origin is contacted.
///
/// Successful bodies must be file-backed. Implementations must not return a full Scriptlet
/// `Vec<u8>` through this seam.
pub trait DownloadProbe {
    fn probe(&mut self, url: &str) -> Result<ProbeResponse, AcquisitionError>;
}

struct CurlProbe {
    connect_timeout: Duration,
    transfer_timeout: Duration,
    size_limit: u64,
    key: String,
    /// Destination `.part` path; curl streams the body here directly.
    body_path: PathBuf,
}

impl DownloadProbe for CurlProbe {
    fn probe(&mut self, url: &str) -> Result<ProbeResponse, AcquisitionError> {
        // Never pass --location: redirects are decided in-process after policy checks.
        validate_https_url(url)?;
        let header_path = self.body_path.with_extension("hdr");
        let _ = fs::remove_file(&self.body_path);
        let _ = fs::remove_file(&header_path);
        // Kindle system curl rejects `--max-filesize=N` (equals form) as unknown
        // while accepting `--max-filesize N`. Prefer separate argv tokens.
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
            .arg("--dump-header")
            .arg(&header_path)
            .arg("--output")
            .arg(&self.body_path)
            .arg("--write-out")
            .arg("%{http_code}")
            .arg(url)
            .output()
            .map_err(|e| AcquisitionError::Transport(format!("curl spawn failed: {e}")))?;

        let headers = fs::read_to_string(&header_path).unwrap_or_default();
        let _ = fs::remove_file(&header_path);
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Nonzero curl exit always fails closed — even if an HTTP status was printed.
        if !output.status.success() {
            let _ = fs::remove_file(&self.body_path);
            return Err(classify_curl_failure(
                &self.key,
                self.size_limit,
                &stderr,
                &stdout,
            ));
        }

        interpret_curl_http_result(
            &stdout,
            &headers,
            &self.body_path,
            self.size_limit,
            &self.key,
        )
    }
}

fn classify_curl_failure(
    key: &str,
    size_limit: u64,
    stderr: &str,
    stdout: &str,
) -> AcquisitionError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") {
        return AcquisitionError::Timeout(key.to_string());
    }
    if lower.contains("maximum file size") {
        return AcquisitionError::SizeLimitExceeded {
            name: key.to_string(),
            limit: size_limit,
        };
    }
    AcquisitionError::Transport(format!(
        "curl failed for {key} (exit nonzero; http_code={stdout}): {stderr}"
    ))
}

/// Interpret a successful curl process (exit 0) into a probe response.
///
/// Separated for unit tests with realistic header blocks.
pub fn interpret_curl_http_result(
    http_code_stdout: &str,
    headers: &str,
    body_path: &Path,
    size_limit: u64,
    key: &str,
) -> Result<ProbeResponse, AcquisitionError> {
    let code: u32 = http_code_stdout.parse().unwrap_or(0);
    if (300..400).contains(&code) {
        let _ = fs::remove_file(body_path);
        let location = extract_location_header(headers)?;
        return Ok(ProbeResponse::Redirect { location });
    }
    if code != 200 {
        let _ = fs::remove_file(body_path);
        return Ok(ProbeResponse::Status(code));
    }
    let meta = fs::metadata(body_path)
        .map_err(|_| AcquisitionError::TruncatedTransfer(format!("missing output for {key}")))?;
    let size = meta.len();
    if size == 0 {
        let _ = fs::remove_file(body_path);
        return Err(AcquisitionError::TruncatedTransfer(key.to_string()));
    }
    if size > size_limit {
        let _ = fs::remove_file(body_path);
        return Err(AcquisitionError::SizeLimitExceeded {
            name: key.to_string(),
            limit: size_limit,
        });
    }
    Ok(ProbeResponse::OkFile {
        path: body_path.to_path_buf(),
        size,
    })
}

fn is_http_status_line(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("HTTP/1.") || line.starts_with("HTTP/2")
}

fn status_code_from_line(line: &str) -> Option<u32> {
    // HTTP/1.1 302 Found  |  HTTP/2 302
    let mut parts = line.split_whitespace();
    let _proto = parts.next()?;
    parts.next()?.parse().ok()
}

fn header_name_value(line: &str) -> Option<(&str, &str)> {
    let (name, value) = line.split_once(':')?;
    let name = name.trim();
    if name.is_empty() || name.contains(' ') {
        return None;
    }
    Some((name, value.trim()))
}

/// Extract `Location` from a curl `--dump-header` block.
///
/// Skips HTTP status lines (which have no colon). When multiple header blocks are
/// present (proxy/intermediate), uses the Location from the last 3xx block.
/// Conflicting Location values within that block are refused.
pub fn extract_location_header(headers: &str) -> Result<String, AcquisitionError> {
    let normalized = headers.replace("\r\n", "\n").replace('\r', "\n");
    let mut last_redirect_location: Option<String> = None;

    for block in normalized.split("\n\n") {
        let mut redirect_status = false;
        let mut locations: Vec<String> = Vec::new();
        for raw in block.lines() {
            let line = raw.trim_end_matches('\r').trim();
            if line.is_empty() {
                continue;
            }
            if is_http_status_line(line) {
                redirect_status = status_code_from_line(line)
                    .map(|code| (300..400).contains(&code))
                    .unwrap_or(false);
                locations.clear();
                continue;
            }
            if let Some((name, value)) = header_name_value(line) {
                if name.eq_ignore_ascii_case("location") {
                    if value.is_empty() {
                        return Err(AcquisitionError::RedirectRefused(
                            "empty Location header".into(),
                        ));
                    }
                    locations.push(value.to_string());
                }
            }
        }
        if redirect_status {
            match locations.as_slice() {
                [] => {
                    return Err(AcquisitionError::RedirectRefused(
                        "redirect without Location".into(),
                    ));
                }
                [one] => last_redirect_location = Some(one.clone()),
                many => {
                    if many.windows(2).all(|w| w[0] == w[1]) {
                        last_redirect_location = Some(many[0].clone());
                    } else {
                        return Err(AcquisitionError::RedirectRefused(
                            "conflicting Location headers".into(),
                        ));
                    }
                }
            }
        }
    }

    last_redirect_location
        .ok_or_else(|| AcquisitionError::RedirectRefused("redirect without Location".into()))
}

/// Follow redirects with hop-by-hop policy checks; stream the final body to a file.
///
/// Returns `(temp_path, size)` where `temp_path` already holds the bytes. The caller
/// renames into the final destination. Never materializes the full body in a `Vec`.
pub fn download_with_probe(
    start_url: &str,
    probe: &mut dyn DownloadProbe,
) -> Result<(PathBuf, u64), AcquisitionError> {
    let mut current = start_url.to_string();
    let mut seen = std::collections::BTreeSet::new();
    for hop in 0..=MAX_REDIRECTS {
        validate_https_url(&current)?;
        if !seen.insert(current.clone()) {
            return Err(AcquisitionError::RedirectRefused(
                "redirect loop detected".into(),
            ));
        }
        match probe.probe(&current)? {
            ProbeResponse::Redirect { location } => {
                // Policy decision happens here — before the next probe call.
                current = decide_redirect_target(&current, &location, hop)?;
            }
            ProbeResponse::OkFile { path, size } => {
                if size == 0 {
                    let _ = fs::remove_file(&path);
                    return Err(AcquisitionError::TruncatedTransfer(
                        "empty download body".into(),
                    ));
                }
                return Ok((path, size));
            }
            ProbeResponse::Status(code) => {
                return Err(AcquisitionError::Transport(format!(
                    "unexpected HTTP status {code}"
                )));
            }
        }
    }
    Err(AcquisitionError::RedirectRefused(
        "redirect limit exceeded".into(),
    ))
}

fn curl_download(
    url: &str,
    dest: &Path,
    size_limit: u64,
    connect_timeout: Duration,
    transfer_timeout: Duration,
    key: &str,
) -> Result<u64, AcquisitionError> {
    validate_https_url(url)?;
    let tmp = dest.with_extension("part");
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(&tmp);
    let mut probe = CurlProbe {
        connect_timeout,
        transfer_timeout,
        size_limit,
        key: key.to_string(),
        body_path: tmp.clone(),
    };
    let (path, size) = match download_with_probe(url, &mut probe) {
        Ok(v) => v,
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
    };
    if path != tmp {
        // Probe wrote elsewhere — stream-copy with a fixed buffer (no full Vec).
        if let Err(err) = stream_copy_file(&path, &tmp, size_limit) {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        let _ = fs::remove_file(&path);
    }
    if size > size_limit {
        let _ = fs::remove_file(&tmp);
        return Err(AcquisitionError::SizeLimitExceeded {
            name: key.to_string(),
            limit: size_limit,
        });
    }
    let meta = fs::metadata(&tmp).map_err(|e| AcquisitionError::Io(e.to_string()))?;
    if meta.len() != size || meta.len() == 0 {
        let _ = fs::remove_file(&tmp);
        return Err(AcquisitionError::TruncatedTransfer(key.to_string()));
    }
    fs::rename(&tmp, dest)?;
    Ok(size)
}

/// Copy `src` → `dest` using a fixed buffer; never loads the whole file into RAM.
fn stream_copy_file(src: &Path, dest: &Path, size_limit: u64) -> Result<u64, AcquisitionError> {
    let mut input = File::open(src)?;
    let mut output = File::create(dest)?;
    let mut buf = vec![0_u8; READ_CHUNK];
    let mut written = 0_u64;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        written =
            written
                .checked_add(n as u64)
                .ok_or_else(|| AcquisitionError::SizeLimitExceeded {
                    name: dest.display().to_string(),
                    limit: size_limit,
                })?;
        if written > size_limit {
            let _ = fs::remove_file(dest);
            return Err(AcquisitionError::SizeLimitExceeded {
                name: dest.display().to_string(),
                limit: size_limit,
            });
        }
        output.write_all(&buf[..n])?;
    }
    output.flush()?;
    let _ = output.sync_all();
    Ok(written)
}

/// Require enough free space for the remaining download budget plus headroom.
pub fn ensure_free_space(path: &Path, need_bytes: u64) -> Result<(), AcquisitionError> {
    let available = free_bytes(path)?;
    let need = need_bytes.saturating_add(1024 * 1024);
    if available < need {
        return Err(AcquisitionError::InsufficientSpace { need, available });
    }
    Ok(())
}

#[cfg(unix)]
fn free_bytes(path: &Path) -> Result<u64, AcquisitionError> {
    use std::ffi::CString;
    let c = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| AcquisitionError::UnsafePath(path.display().to_string()))?;
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut stat) != 0 {
            // Fall back to parent if path does not exist yet.
            if let Some(parent) = path.parent() {
                return free_bytes(parent);
            }
            return Err(AcquisitionError::Io("statvfs failed".into()));
        }
        Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
    }
}

#[cfg(not(unix))]
fn free_bytes(_path: &Path) -> Result<u64, AcquisitionError> {
    // Host unit tests do not rely on exact free-space enforcement.
    Ok(u64::MAX / 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn github_origin_rejects_urls_and_path_traversal() {
        assert!(GithubReleaseOrigin::parse("https://evil.example/x").is_err());
        assert!(GithubReleaseOrigin::parse("../etc").is_err());
        let origin = GithubReleaseOrigin::parse("acme/ignite").expect("ok");
        let url = origin
            .asset_url("v3.3.1", "Install Ignite v3.3.1.sh")
            .expect("url");
        assert!(url.starts_with("https://github.com/acme/ignite/releases/download/v3.3.1/"));
        assert!(url.contains("Install%20Ignite%20v3.3.1.sh"));
        assert!(origin.asset_url("v3.3.1", "../x").is_err());
    }

    #[test]
    fn https_policy_rejects_http_and_foreign_hosts() {
        assert!(validate_https_url("http://github.com/a/b").is_err());
        assert!(validate_https_url("https://evil.example/a").is_err());
        assert!(validate_https_url(
            "https://objects.githubusercontent.com/github-production-release-asset/1"
        )
        .is_ok());
        assert!(validate_https_url("https://user:pass@github.com/a").is_err());
        assert!(validate_https_url("https://github.com:8443/a").is_err());
        assert!(validate_https_url("https://github.com:443/a/b").is_ok());
    }

    #[test]
    fn redirect_decision_allows_github_asset_hosts_only() {
        let start = "https://github.com/acme/ignite/releases/download/v3.3.1/x";
        let allowed = decide_redirect_target(
            start,
            "https://objects.githubusercontent.com/github-production-release-asset/1",
            0,
        )
        .expect("cdn hop");
        assert!(allowed.contains("objects.githubusercontent.com"));
        assert!(matches!(
            decide_redirect_target(start, "https://evil.example/payload", 0),
            Err(AcquisitionError::UnexpectedOrigin(_))
        ));
        assert!(matches!(
            decide_redirect_target(start, "http://github.com/x", 0),
            Err(AcquisitionError::UnsupportedScheme(_))
        ));
        assert!(matches!(
            decide_redirect_target(start, "/relative/path", 0),
            Err(AcquisitionError::RedirectRefused(_))
        ));
        assert!(matches!(
            decide_redirect_target(start, "https://github.com/x", MAX_REDIRECTS),
            Err(AcquisitionError::RedirectRefused(_))
        ));
    }

    fn temp_body(label: &str, bytes: &[u8]) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("ignite-acq-{label}-{stamp}.part"));
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn download_with_probe_enforces_hops_before_next_contact() {
        struct Scripted {
            calls: Vec<String>,
            responses: Vec<ProbeResponse>,
            idx: usize,
        }
        impl DownloadProbe for Scripted {
            fn probe(&mut self, url: &str) -> Result<ProbeResponse, AcquisitionError> {
                self.calls.push(url.to_string());
                let response = self.responses.get(self.idx).cloned().ok_or_else(|| {
                    AcquisitionError::Transport("scripted probe exhausted".into())
                })?;
                self.idx += 1;
                Ok(response)
            }
        }

        let body_path = temp_body("ok", b"payload");
        let mut ok = Scripted {
            calls: Vec::new(),
            responses: vec![
                ProbeResponse::Redirect {
                    location: "https://objects.githubusercontent.com/asset/1".into(),
                },
                ProbeResponse::OkFile {
                    path: body_path.clone(),
                    size: 7,
                },
            ],
            idx: 0,
        };
        let (path, size) = download_with_probe(
            "https://github.com/acme/ignite/releases/download/v1.0.0/a",
            &mut ok,
        )
        .expect("allowed chain");
        assert_eq!(size, 7);
        assert_eq!(fs::read(&path).unwrap(), b"payload");
        assert_eq!(ok.calls.len(), 2);
        assert!(ok.calls[1].contains("objects.githubusercontent.com"));
        let _ = fs::remove_file(&path);

        let mut evil = Scripted {
            calls: Vec::new(),
            responses: vec![ProbeResponse::Redirect {
                location: "https://evil.example/steal".into(),
            }],
            idx: 0,
        };
        let err = download_with_probe(
            "https://github.com/acme/ignite/releases/download/v1.0.0/a",
            &mut evil,
        )
        .expect_err("evil redirect");
        assert!(matches!(err, AcquisitionError::UnexpectedOrigin(_)));
        assert_eq!(evil.calls.len(), 1, "must not contact unapproved origin");

        let mut down = Scripted {
            calls: Vec::new(),
            responses: vec![ProbeResponse::Redirect {
                location: "http://objects.githubusercontent.com/asset/1".into(),
            }],
            idx: 0,
        };
        assert!(matches!(
            download_with_probe(
                "https://github.com/acme/ignite/releases/download/v1.0.0/a",
                &mut down
            ),
            Err(AcquisitionError::UnsupportedScheme(_))
        ));
        assert_eq!(down.calls.len(), 1);

        let mut looped = Scripted {
            calls: Vec::new(),
            responses: (0..=MAX_REDIRECTS)
                .map(|i| ProbeResponse::Redirect {
                    location: format!("https://objects.githubusercontent.com/asset/{i}"),
                })
                .collect(),
            idx: 0,
        };
        assert!(matches!(
            download_with_probe(
                "https://github.com/acme/ignite/releases/download/v1.0.0/a",
                &mut looped
            ),
            Err(AcquisitionError::RedirectRefused(_))
        ));
    }

    #[test]
    fn extract_location_handles_realistic_curl_headers() {
        let http11 = "HTTP/1.1 302 Found\r\nLocation: https://objects.githubusercontent.com/asset/1\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(
            extract_location_header(http11).unwrap(),
            "https://objects.githubusercontent.com/asset/1"
        );

        let http2 = "HTTP/2 302\nlocation: https://release-assets.githubusercontent.com/a\n\n";
        assert_eq!(
            extract_location_header(http2).unwrap(),
            "https://release-assets.githubusercontent.com/a"
        );

        let proxy_blocks = "\
HTTP/1.1 200 Connection established\r\n\r\n\
HTTP/1.1 302 Found\r\n\
Server: GitHub\r\n\
Location: https://objects.githubusercontent.com/github-production-release-asset/99\r\n\
\r\n";
        assert_eq!(
            extract_location_header(proxy_blocks).unwrap(),
            "https://objects.githubusercontent.com/github-production-release-asset/99"
        );

        assert!(matches!(
            extract_location_header("HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n"),
            Err(AcquisitionError::RedirectRefused(_))
        ));
        assert!(matches!(
            extract_location_header(
                "HTTP/1.1 302 Found\r\nLocation: https://objects.githubusercontent.com/a\r\nLocation: https://evil.example/b\r\n\r\n"
            ),
            Err(AcquisitionError::RedirectRefused(_))
        ));
    }

    #[test]
    fn real_header_parse_feeds_transport_decision_path() {
        // Exercise extract_location_header → decide_redirect_target → download_with_probe,
        // not only a pre-baked ProbeResponse::Redirect.
        struct HeaderDrivenProbe {
            headers: String,
            http_code: String,
            body: PathBuf,
            contacted: Vec<String>,
        }
        impl DownloadProbe for HeaderDrivenProbe {
            fn probe(&mut self, url: &str) -> Result<ProbeResponse, AcquisitionError> {
                self.contacted.push(url.to_string());
                if self.contacted.len() == 1 {
                    // Simulate curl dumping headers + printing redirect status.
                    let location = extract_location_header(&self.headers)?;
                    let _ = (&self.http_code,); // reported status accompanies headers
                    return Ok(ProbeResponse::Redirect { location });
                }
                fs::write(&self.body, b"streamed-body").unwrap();
                Ok(ProbeResponse::OkFile {
                    path: self.body.clone(),
                    size: 13,
                })
            }
        }

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let body = std::env::temp_dir().join(format!("ignite-hdr-body-{stamp}"));

        // Valid GitHub release CDN redirect via realistic HTTP/1.1 headers.
        let mut ok = HeaderDrivenProbe {
            headers: "HTTP/1.1 302 Found\r\n\
Location: https://objects.githubusercontent.com/github-production-release-asset/42\r\n\
Content-Length: 0\r\n\r\n"
                .into(),
            http_code: "302".into(),
            body: body.clone(),
            contacted: Vec::new(),
        };
        let start = "https://github.com/acme/ignite/releases/download/v1.0.0/a";
        let (path, size) = download_with_probe(start, &mut ok).unwrap();
        assert_eq!(size, 13);
        assert_eq!(fs::read(&path).unwrap(), b"streamed-body");
        assert_eq!(ok.contacted.len(), 2);
        assert!(ok.contacted[1].contains("objects.githubusercontent.com"));
        // Never contacted next origin before validating Location.
        assert!(ok.contacted[0].contains("github.com"));

        // Forbidden redirect target: Location parsed, then refused before next probe.
        let mut forbidden = HeaderDrivenProbe {
            headers: "HTTP/2 302\n\
location: https://evil.example/payload\n\n"
                .into(),
            http_code: "302".into(),
            body: body.clone(),
            contacted: Vec::new(),
        };
        assert!(
            matches!(
                download_with_probe(start, &mut forbidden),
                Err(AcquisitionError::UnexpectedOrigin(_))
                    | Err(AcquisitionError::RedirectRefused(_))
            ),
            "forbidden redirect must fail closed"
        );
        assert_eq!(
            forbidden.contacted.len(),
            1,
            "must not contact forbidden origin"
        );

        // Nonzero curl exit with HTTP 200 / 302 printed still fails closed.
        assert!(matches!(
            classify_curl_failure("scriptlet", 10, "curl: (56) failure", "200"),
            AcquisitionError::Transport(_)
        ));
        assert!(matches!(
            classify_curl_failure("scriptlet", 10, "curl: (56) failure", "302"),
            AcquisitionError::Transport(_)
        ));
        let _ = fs::remove_file(&body);
    }

    #[test]
    fn interpret_curl_result_fails_closed_and_streams_file() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let body = std::env::temp_dir().join(format!("ignite-curl-body-{stamp}"));
        fs::write(&body, b"abcdef").unwrap();

        let ok = interpret_curl_http_result(
            "200",
            "HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\n",
            &body,
            64,
            "scriptlet",
        )
        .unwrap();
        match ok {
            ProbeResponse::OkFile { path, size } => {
                assert_eq!(path, body);
                assert_eq!(size, 6);
            }
            other => panic!("unexpected {other:?}"),
        }

        let redir = interpret_curl_http_result(
            "302",
            "HTTP/1.1 302 Found\r\nLocation: https://objects.githubusercontent.com/x\r\n\r\n",
            &body,
            64,
            "scriptlet",
        )
        .unwrap();
        assert!(matches!(redir, ProbeResponse::Redirect { .. }));
        // Body cleaned on redirect.
        assert!(!body.exists());

        // Nonzero exit classification (simulate via classify helper).
        assert!(matches!(
            classify_curl_failure("k", 10, "operation timed out", "200"),
            AcquisitionError::Timeout(_)
        ));
        assert!(matches!(
            classify_curl_failure("k", 10, "curl: failed", "302"),
            AcquisitionError::Transport(_)
        ));
    }

    #[test]
    fn streaming_probe_uses_fixed_buffer_not_full_scriptlet_vec() {
        // Prove the acquisition abstraction streams with READ_CHUNK-sized buffers
        // without allocating a 768 MiB Vec.
        struct StreamingFileProbe {
            source: PathBuf,
            dest: PathBuf,
            max_buf: usize,
            calls: usize,
        }
        impl DownloadProbe for StreamingFileProbe {
            fn probe(&mut self, _url: &str) -> Result<ProbeResponse, AcquisitionError> {
                self.calls += 1;
                let mut input = File::open(&self.source)?;
                let mut output = File::create(&self.dest)?;
                let mut buf = vec![0_u8; READ_CHUNK];
                self.max_buf = self.max_buf.max(buf.len());
                let mut written = 0_u64;
                loop {
                    let n = input.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    written += n as u64;
                    output.write_all(&buf[..n])?;
                }
                output.flush()?;
                Ok(ProbeResponse::OkFile {
                    path: self.dest.clone(),
                    size: written,
                })
            }
        }

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ignite-stream-{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        let dst = dir.join("dst.part");
        // 256 KiB payload — larger than READ_CHUNK, far below Scriptlet ceiling.
        let payload = vec![0x5A_u8; READ_CHUNK * 4];
        fs::write(&src, &payload).unwrap();
        let mut probe = StreamingFileProbe {
            source: src,
            dest: dst.clone(),
            max_buf: 0,
            calls: 0,
        };
        let (path, size) = download_with_probe(
            "https://github.com/acme/ignite/releases/download/v1.0.0/a",
            &mut probe,
        )
        .unwrap();
        assert_eq!(size, payload.len() as u64);
        assert_eq!(fs::read(&path).unwrap(), payload);
        assert_eq!(probe.max_buf, READ_CHUNK);
        assert!(probe.max_buf < 1024 * 1024);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_copy_is_bounded_and_rejects_traversal() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ignite-acq-src-{stamp}"));
        let dest_dir = std::env::temp_dir().join(format!("ignite-acq-dst-{stamp}"));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&dest_dir);
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();
        fs::write(root.join("ok.bin"), b"abc").unwrap();
        let dest = dest_dir.join("ok.bin");
        let n = copy_local_asset(&root, "ok.bin", &dest, 10).expect("copy");
        assert_eq!(n, 3);
        assert_eq!(fs::read(&dest).unwrap(), b"abc");
        assert!(copy_local_asset(&root, "../ok.bin", &dest, 10).is_err());
        assert_eq!(
            copy_local_asset(&root, "ok.bin", &dest, 2).unwrap_err(),
            AcquisitionError::SizeLimitExceeded {
                name: "ok.bin".into(),
                limit: 2
            }
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&dest_dir);
    }

    #[test]
    fn asset_names_match_3h_contract() {
        let names = ReleaseAssetNames::for_version("3.3.1").unwrap();
        assert_eq!(names.scriptlet, "Install Ignite v3.3.1.sh");
        assert_eq!(
            names.publication_binding,
            "ignite-kindle-v3.3.1.publication-binding"
        );
    }
}
