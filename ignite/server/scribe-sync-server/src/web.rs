//! S3A optional static hosting of the Scribe Web bundle (same origin as `/v1`, so the browser
//! needs no CORS at all).
//!
//! * Only non-`/v1` paths are static; `/v1` always stays the (authenticated) sync API.
//! * Static files are public bundle assets (no secrets): they are served without auth so the
//!   connection screen can load before a token is entered. The token is never in the bundle.
//! * Resolution is purely lexical over percent-decoded segments: `.`/`..`, dot-files, empty
//!   decodes, backslashes, drive colons and NUL are refused; the final file is canonicalized and
//!   must stay under the canonical web root (symlink escape guard). No directory listings.
//! * Unknown extension-less paths fall back to `index.html` (client-side routes); unknown
//!   file-like paths are 404.
//! * `/assets/*` (content-hashed Vite output) is `immutable`; everything else is `no-cache`.
//! * The web root must not overlap the ledger data dir or the token verifier file.

use std::path::{Component, Path, PathBuf};

/// Validated, canonical web root.
#[derive(Clone, Debug)]
pub struct WebRoot {
    root: PathBuf,
}

/// A static response (the HTTP layer adds the headers).
#[derive(Debug, PartialEq, Eq)]
pub struct StaticReply {
    pub status: u16,
    pub content_type: &'static str,
    pub cache_control: &'static str,
    pub body: Vec<u8>,
}

impl StaticReply {
    fn plain(status: u16, msg: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            cache_control: "no-store",
            body: msg.as_bytes().to_vec(),
        }
    }
}

fn canon(p: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn canon_lenient(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut rest = Vec::new();
    let mut cur = abs.as_path();
    loop {
        if let Ok(c) = std::fs::canonicalize(cur) {
            return rest.iter().rev().fold(c, |acc, r| acc.join(r));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return abs,
        }
    }
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

impl WebRoot {
    /// `forbidden`: server-private paths (ledger dir, verifier file) that must never be
    /// reachable through the web root.
    pub fn new(root: &Path, forbidden: &[&Path]) -> Result<Self, String> {
        let root = canon(root).map_err(|e| format!("REFUSE: --web-root {e}"))?;
        if !root.is_dir() {
            return Err(format!(
                "REFUSE: --web-root {} is not a directory",
                root.display()
            ));
        }
        if !root.join("index.html").is_file() {
            return Err(format!(
                "REFUSE: --web-root {} has no index.html (build web/scribe-web first)",
                root.display()
            ));
        }
        for f in forbidden {
            // A forbidden path may not exist yet (fresh ledger dir): canonicalize its nearest
            // existing ancestor so both sides use the same form (e.g. `\\?\` on Windows).
            let fp = canon_lenient(f);
            if overlaps(&root, &fp) {
                return Err(format!(
                    "REFUSE: --web-root {} overlaps server-private path {}",
                    root.display(),
                    fp.display()
                ));
            }
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Serve `raw_path` (the undecoded URL path, without query) for `method`.
    pub fn serve(&self, method: &str, raw_path: &str) -> StaticReply {
        if method != "GET" && method != "HEAD" {
            return StaticReply::plain(405, "method not allowed");
        }
        let Some(segs) = decode_segments(raw_path) else {
            return StaticReply::plain(400, "bad path");
        };
        let file_like = segs.last().is_some_and(|s| s.contains('.'));
        let under_assets = segs.first().is_some_and(|s| s == "assets");
        let mut candidate = self.root.clone();
        for s in &segs {
            candidate.push(s);
        }
        if !segs.is_empty() {
            if let Some(reply) = self.file(&candidate, under_assets) {
                return reply;
            }
            if file_like || under_assets {
                return StaticReply::plain(404, "not found");
            }
        }
        self.file(&self.root.join("index.html"), false)
            .unwrap_or_else(|| StaticReply::plain(404, "not found"))
    }

    fn file(&self, candidate: &Path, immutable: bool) -> Option<StaticReply> {
        if !candidate.is_file() {
            return None;
        }
        let real = canon(candidate).ok()?;
        if !real.starts_with(&self.root) {
            return None;
        }
        let body = std::fs::read(&real).ok()?;
        Some(StaticReply {
            status: 200,
            content_type: content_type(&real),
            cache_control: if immutable {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            },
            body,
        })
    }
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hi = hex(*b.get(i + 1)?)?;
            let lo = hex(*b.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Decoded, safe path segments; `None` = refuse.
fn decode_segments(raw_path: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for raw in raw_path.split('/') {
        if raw.is_empty() {
            continue;
        }
        let s = percent_decode(raw)?;
        if s.is_empty()
            || s.starts_with('.')
            || s.contains(['/', '\\', ':', '\0'])
            || s.chars().any(char::is_control)
        {
            return None;
        }
        // Belt and braces: the segment must be one plain path component.
        let mut comps = Path::new(&s).components();
        if !matches!(comps.next(), Some(Component::Normal(_))) || comps.next().is_some() {
            return None;
        }
        out.push(s);
    }
    Some(out)
}

fn content_type(p: &Path) -> &'static str {
    match p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        // S3B: the browser replica's protocol bridge (required for streaming compilation).
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_refuse_traversal_and_oddities() {
        for bad in [
            "/..",
            "/a/../b",
            "/%2e%2e/x",
            "/%2E%2E%2Fetc",
            "/a%2f..%2fb",
            "/a%5c..%5cb",
            "/.env",
            "/c:",
            "/x%00y",
            "/%zz",
            "/%",
        ] {
            assert!(decode_segments(bad).is_none(), "{bad}");
        }
        assert_eq!(
            decode_segments("/assets/index-AbC.js").unwrap(),
            ["assets", "index-AbC.js"]
        );
        assert_eq!(decode_segments("//a///b/").unwrap(), ["a", "b"]);
        assert!(decode_segments("/").unwrap().is_empty());
    }
}
