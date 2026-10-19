//! The read-only source the exporter pulls from.
//!
//! [`ReadOnlySource`] has exactly one operation, `get`, and no method parameter: an exporter
//! built on it cannot express POST, PUT, PATCH or DELETE. [`HttpSource`] is the production
//! transport (plain HTTP, the trusted-LAN deployment); tests inject in-memory sources.

use std::fmt;
use std::io::Read;
use std::time::Duration;

/// A transport-level failure (connection, timeout, malformed HTTP). Never contains credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceError(pub String);

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One GET response. `body` is streamed; the exporter bounds how much it reads.
pub struct SourceResponse<'a> {
    pub status: u16,
    pub body: Box<dyn Read + 'a>,
}

pub trait ReadOnlySource {
    /// GET `path` (always starting with `/v1/`, no query). Implementations must issue only
    /// GET requests and must not follow redirects.
    fn get(&self, path: &str) -> Result<SourceResponse<'_>, SourceError>;
}

/// A runtime-supplied secret. Never printed: `Debug` is redacted and there is no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// How `/v1` requests authenticate. Both are the existing server mechanisms: a device bearer
/// credential (`Authorization: Bearer`) or a browser session (`Cookie: scribe_session`). GET
/// needs no CSRF marker. Nothing here pairs, stores or discovers credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credential {
    Bearer(Secret),
    SessionCookie(Secret),
}

/// Plain-HTTP [`ReadOnlySource`] over `ureq`: GET only, redirects disabled (a redirect could
/// carry the credential elsewhere), no environment proxies, bounded connect/read timeouts.
pub struct HttpSource {
    agent: ureq::Agent,
    base: String,
    credential: Option<Credential>,
}

impl fmt::Debug for HttpSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpSource")
            .field("base", &self.base)
            .field("credential", &self.credential)
            .finish()
    }
}

impl HttpSource {
    /// `base_url` is `http://host[:port]` (an optional trailing `/` is ignored). HTTPS, user
    /// info, paths, queries and fragments are refused rather than guessed.
    pub fn new(base_url: &str, credential: Option<Credential>) -> Result<Self, SourceError> {
        let base = base_url.trim_end_matches('/');
        let Some(authority) = base.strip_prefix("http://") else {
            return Err(SourceError(format!(
                "server URL {base_url:?} must start with http:// (this build speaks plain HTTP only)"
            )));
        };
        if authority.is_empty()
            || authority.chars().any(|c| {
                matches!(c, '/' | '?' | '#' | '@' | '\\') || c.is_whitespace() || c.is_control()
            })
        {
            return Err(SourceError(format!(
                "server URL {base_url:?} must be http://host[:port] with no path, query or user info"
            )));
        }
        if let Some(Credential::Bearer(Secret(s)) | Credential::SessionCookie(Secret(s))) =
            &credential
        {
            if s.is_empty()
                || s.chars()
                    .any(|c| c.is_control() || c.is_whitespace() || c == ';')
            {
                return Err(SourceError(
                    "credential is empty or contains invalid characters".into(),
                ));
            }
        }
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(120))
            .build();
        Ok(Self {
            agent,
            base: base.to_string(),
            credential,
        })
    }
}

impl ReadOnlySource for HttpSource {
    fn get(&self, path: &str) -> Result<SourceResponse<'_>, SourceError> {
        if !path.starts_with("/v1/") || path.contains("..") || path.contains('?') {
            return Err(SourceError(format!("refused non-/v1 path {path:?}")));
        }
        let mut req = self.agent.get(&format!("{}{path}", self.base));
        match &self.credential {
            Some(Credential::Bearer(Secret(s))) => {
                req = req.set("Authorization", &format!("Bearer {s}"))
            }
            Some(Credential::SessionCookie(Secret(s))) => {
                req = req.set("Cookie", &format!("scribe_session={s}"))
            }
            None => {}
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(t)) => return Err(SourceError(format!("GET {path}: {t}"))),
        };
        Ok(SourceResponse {
            status: resp.status(),
            body: Box::new(resp.into_reader()),
        })
    }
}
