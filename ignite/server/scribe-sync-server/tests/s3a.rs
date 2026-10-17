//! S3A over real HTTP: read-only library discovery (`GET /v1/libraries`), the poll head's
//! snapshot hash, and optional same-origin static hosting of the Scribe Web bundle.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use scribe_sync_server::{
    generate_token, ServerConfig, ServerHandle, Store, TokenVerifier, WebRoot,
};
use serde_json::Value;
use sync_engine::testkit::{self, page_spec, pg, replica, Author};
use sync_protocol::{
    snapshot_sha256, to_canonical_json, BackgroundRef, LibraryId, SyncOperationV1 as Op,
    SyncSnapshotV1,
};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!(
        "scribe-s3a-{label}-{n}-{}",
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

struct Headers(Vec<(String, String)>);

impl Headers {
    fn header(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn call(
    base: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
) -> (u16, Vec<u8>, Headers) {
    let mut req = ureq::request(method, &format!("{base}{path}"));
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let res = match body {
        Some(b) => req.send_bytes(b),
        None => req.call(),
    };
    let resp = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("transport: {e}"),
    };
    let status = resp.status();
    let headers = Headers(
        resp.headers_names()
            .into_iter()
            .filter_map(|n| resp.header(&n).map(|v| (n.clone(), v.to_string())))
            .collect(),
    );
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut out).unwrap();
    (status, out, headers)
}

fn json(base: &str, path: &str, token: Option<&str>) -> (u16, Value) {
    let (s, b, _) = call(base, "GET", path, token, None);
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

struct Srv {
    h: ServerHandle,
    token: Option<String>,
    _data: PathBuf,
}

impl Srv {
    fn base(&self) -> String {
        self.h.base_url()
    }
    fn tok(&self) -> Option<&str> {
        self.token.as_deref()
    }
    fn bootstrap(&self, snap: &SyncSnapshotV1) {
        for bg in snap
            .notebooks
            .iter()
            .flat_map(|n| &n.pages)
            .map(|p| &p.background)
        {
            if let BackgroundRef::Blob { sha256, .. } = bg {
                let (bytes, _) = testkit::blob_background();
                assert_eq!(sync_protocol::sha256_hex(&bytes), *sha256);
                let (s, _, _) = call(
                    &self.base(),
                    "PUT",
                    &format!("/v1/blobs/{sha256}"),
                    self.tok(),
                    Some(&bytes),
                );
                assert!(s == 200 || s == 201, "{s}");
            }
        }
        let (s, b, _) = call(
            &self.base(),
            "POST",
            "/v1/libraries",
            self.tok(),
            Some(&to_canonical_json(snap).unwrap()),
        );
        assert_eq!(s, 201, "{}", String::from_utf8_lossy(&b));
    }
}

fn server(auth: bool, web: Option<&Path>) -> Srv {
    let data = tmp("ledger");
    let token = auth.then(|| generate_token().unwrap());
    let web_root = web.map(|w| WebRoot::new(w, &[data.as_path()]).unwrap());
    let h = ServerHandle::start_with(
        "127.0.0.1:0".parse().unwrap(),
        Store::open(&data).unwrap(),
        ServerConfig {
            auth: token.as_deref().map(TokenVerifier::for_token),
            web_root,
            control: None,
        },
    )
    .unwrap();
    Srv {
        h,
        token,
        _data: data,
    }
}

fn library_with_id(n: u64) -> SyncSnapshotV1 {
    let mut s = testkit::genesis();
    s.library_id = LibraryId::from_uuid(testkit::uuid7(9, n)).unwrap();
    s
}

// ------------------------------------------------------------------ GET /v1/libraries

#[test]
fn library_list_empty_one_many_sorted_with_counts_heads_and_hashes() {
    let srv = server(true, None);
    let t = srv.tok();
    let (s, v) = json(&srv.base(), "/v1/libraries", t);
    assert_eq!(s, 200);
    assert_eq!(v, serde_json::json!({ "v": 1, "libraries": [] }));

    // One library.
    let a = library_with_id(20);
    srv.bootstrap(&a);
    let (_, v) = json(&srv.base(), "/v1/libraries", t);
    let libs = v["libraries"].as_array().unwrap();
    assert_eq!(libs.len(), 1);
    let pages: usize = a.notebooks.iter().map(|n| n.pages.len()).sum();
    assert_eq!(libs[0]["library_id"], a.library_id.to_string());
    assert_eq!(libs[0]["head_server_seq"], 0);
    assert_eq!(libs[0]["notebook_count"], a.notebooks.len());
    assert_eq!(libs[0]["page_count"], pages);
    assert_eq!(
        libs[0]["snapshot_sha256"],
        snapshot_sha256(&a).unwrap().as_str()
    );
    let keys: Vec<&String> = libs[0].as_object().unwrap().keys().collect();
    assert_eq!(
        keys.len(),
        5,
        "no invented title or other metadata: {keys:?}"
    );

    // More libraries, inserted out of order → listed by LibraryId.
    let c = library_with_id(30);
    let b = library_with_id(25);
    srv.bootstrap(&c);
    srv.bootstrap(&b);
    // Advance one library: head and hash follow the ledger; counts follow the snapshot.
    let lib_b = b.library_id;
    let mut author = Author::new(replica(7));
    author.library = lib_b;
    let env = author.env(Op::CreatePage {
        notebook_id: b.notebooks[0].notebook_id,
        page: page_spec(pg(900)),
        after_page_id: None,
    });
    let body = format!(
        "{{\"envelopes\":[{}]}}",
        String::from_utf8(to_canonical_json(&env).unwrap()).unwrap()
    );
    let (s, _, _) = call(
        &srv.base(),
        "POST",
        &format!("/v1/libraries/{lib_b}/operations"),
        t,
        Some(body.as_bytes()),
    );
    assert_eq!(s, 200);

    let (_, v) = json(&srv.base(), "/v1/libraries", t);
    let libs = v["libraries"].as_array().unwrap();
    let ids: Vec<&str> = libs
        .iter()
        .map(|l| l["library_id"].as_str().unwrap())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
    assert_eq!(ids.len(), 3);
    let row_b = libs
        .iter()
        .find(|l| l["library_id"] == lib_b.to_string())
        .unwrap();
    assert_eq!(row_b["head_server_seq"], 1);
    assert_eq!(row_b["page_count"], pages + 1);
    let (_, snap) = json(&srv.base(), &format!("/v1/libraries/{lib_b}/snapshot"), t);
    assert_eq!(row_b["snapshot_sha256"], snap["snapshot_sha256"]);
    // The per-library poll head carries the same snapshot hash.
    let (_, head) = json(&srv.base(), &format!("/v1/libraries/{lib_b}"), t);
    assert_eq!(head["head_server_seq"], 1);
    assert_eq!(head["snapshot_sha256"], snap["snapshot_sha256"]);
    // Listing is read-only: nothing moved.
    let (_, again) = json(&srv.base(), "/v1/libraries", t);
    assert_eq!(again, v);
}

#[test]
fn library_list_requires_auth_like_every_v1_route() {
    let srv = server(true, None);
    srv.bootstrap(&library_with_id(1));
    let (s, _) = json(&srv.base(), "/v1/libraries", None);
    assert_eq!(s, 401);
    let (s, _) = json(
        &srv.base(),
        "/v1/libraries",
        Some(&generate_token().unwrap()),
    );
    assert_eq!(s, 401);
    let (s, v) = json(&srv.base(), "/v1/libraries", srv.tok());
    assert_eq!(s, 200);
    assert_eq!(v["libraries"].as_array().unwrap().len(), 1);
    // Other methods on the collection stay refused/unchanged.
    let (s, _, _) = call(&srv.base(), "DELETE", "/v1/libraries", srv.tok(), None);
    assert_eq!(s, 405);
}

// ------------------------------------------------------------------ static web root

const INDEX_HTML: &[u8] =
    b"<!doctype html><title>Scribe</title><link rel=\"manifest\" href=\"/manifest.webmanifest\">";

fn web_root() -> PathBuf {
    let d = tmp("web");
    std::fs::create_dir_all(d.join("assets")).unwrap();
    std::fs::create_dir_all(d.join("icons")).unwrap();
    std::fs::write(d.join("index.html"), INDEX_HTML).unwrap();
    std::fs::write(d.join("assets/index-AbC123.js"), b"console.log(1)").unwrap();
    std::fs::write(d.join("assets/index-AbC123.css"), b"body{}").unwrap();
    std::fs::write(d.join("assets/scribe_web_wasm-AbC123.wasm"), b" asm   ").unwrap();
    std::fs::write(
        d.join("manifest.webmanifest"),
        br#"{"name":"Scribe","start_url":"/","display":"standalone"}"#,
    )
    .unwrap();
    std::fs::write(d.join("icons/icon-192.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    std::fs::write(d.join("boot.css"), b"html{background:#f4f2ee}").unwrap();
    std::fs::write(d.join("sw.js"), b"const APP_VERSION='0';").unwrap();
    std::fs::write(d.join(".secret"), b"hidden").unwrap();
    d
}

#[test]
fn static_root_serves_index_assets_and_spa_routes_with_cache_policy() {
    let web = web_root();
    let srv = server(true, Some(&web));
    let base = srv.base();
    // Static is public (the token is entered in the page itself), /v1 stays authenticated.
    let (s, body, r) = call(&base, "GET", "/", None, None);
    assert_eq!(s, 200);
    assert_eq!(body.as_slice(), INDEX_HTML);
    assert!(r.header("Content-Type").unwrap().starts_with("text/html"));
    assert_eq!(r.header("Cache-Control"), Some("no-cache"));
    let csp = r.header("Content-Security-Policy").unwrap();
    assert!(csp.contains("default-src 'self'"));
    // S3B: WebAssembly compilation only — never generic 'unsafe-eval'; connect-src same-origin.
    assert!(csp.contains("script-src 'self' 'wasm-unsafe-eval';"));
    assert!(csp.contains("worker-src 'self';"));
    assert!(!csp
        .replace("'wasm-unsafe-eval'", "")
        .contains("unsafe-eval"));
    assert!(!csp.contains("unsafe-inline"));
    assert!(csp.contains("connect-src 'self';"));
    assert!(!csp.contains("  "));
    assert_eq!(r.header("X-Content-Type-Options"), Some("nosniff"));
    assert!(r.header("Access-Control-Allow-Origin").is_none());

    // Phase 2 install metadata: stable root paths (not under /assets/), so no-cache and
    // updateable; MIME for .webmanifest / icons / boot.css.
    let (s, body, r) = call(&base, "GET", "/manifest.webmanifest", None, None);
    assert_eq!(s, 200);
    assert_eq!(r.header("Content-Type"), Some("application/manifest+json"));
    assert_eq!(r.header("Cache-Control"), Some("no-cache"));
    assert!(body.starts_with(b"{"));
    let (s, _, r) = call(&base, "GET", "/icons/icon-192.png", None, None);
    assert_eq!(s, 200);
    assert_eq!(r.header("Content-Type"), Some("image/png"));
    assert_eq!(r.header("Cache-Control"), Some("no-cache"));
    let (s, _, r) = call(&base, "GET", "/boot.css", None, None);
    assert_eq!(s, 200);
    assert!(r.header("Content-Type").unwrap().starts_with("text/css"));
    assert_eq!(r.header("Cache-Control"), Some("no-cache"));
    let (s, _, r) = call(&base, "GET", "/sw.js", None, None);
    assert_eq!(s, 200);
    assert!(r
        .header("Content-Type")
        .unwrap()
        .starts_with("text/javascript"));
    assert_eq!(r.header("Cache-Control"), Some("no-cache"));

    let (s, body, r) = call(&base, "GET", "/assets/index-AbC123.js", None, None);
    assert_eq!(s, 200);
    assert_eq!(body, b"console.log(1)");
    assert!(r
        .header("Content-Type")
        .unwrap()
        .starts_with("text/javascript"));
    assert_eq!(
        r.header("Cache-Control"),
        Some("public, max-age=31536000, immutable")
    );
    let (_, _, r) = call(&base, "GET", "/assets/index-AbC123.css", None, None);
    assert!(r.header("Content-Type").unwrap().starts_with("text/css"));
    // S3B: the protocol bridge is served as application/wasm (streaming compile), immutable.
    let (s, body, r) = call(
        &base,
        "GET",
        "/assets/scribe_web_wasm-AbC123.wasm",
        None,
        None,
    );
    assert_eq!((s, body.as_slice()), (200, b" asm   ".as_slice()));
    assert_eq!(r.header("Content-Type"), Some("application/wasm"));
    assert_eq!(
        r.header("Cache-Control"),
        Some("public, max-age=31536000, immutable")
    );

    // Client-side routes fall back to index; missing files and assets are 404.
    for route in ["/app", "/app/library/x", "/notebooks"] {
        let (s, body, _) = call(&base, "GET", route, None, None);
        assert_eq!((s, body.as_slice()), (200, INDEX_HTML), "{route}");
    }
    assert_eq!(call(&base, "GET", "/missing.js", None, None).0, 404);
    assert_eq!(call(&base, "GET", "/assets/nope", None, None).0, 404);
    // HEAD has headers only; mutations are refused on static paths.
    let (s, body, _) = call(&base, "HEAD", "/", None, None);
    assert_eq!((s, body.len()), (200, 0));
    assert_eq!(call(&base, "POST", "/", None, Some(b"x")).0, 405);
    assert_eq!(
        call(&base, "PUT", "/assets/index-AbC123.js", None, Some(b"x")).0,
        405
    );

    // /v1 is still the authenticated API.
    assert_eq!(json(&base, "/v1/libraries", None).0, 401);
    assert_eq!(json(&base, "/v1/libraries", srv.tok()).0, 200);
    assert_eq!(json(&base, "/v1", None).0, 401);
}

/// Raw request line (no client-side normalization of `..`).
fn raw_get(base: &str, target: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let addr = base.trim_start_matches("http://");
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    write!(
        s,
        "GET {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    (status, out)
}

#[test]
fn static_root_refuses_traversal_dotfiles_and_server_private_files() {
    let web = web_root();
    let srv = server(true, Some(&web));
    let base = srv.base();
    for target in [
        "/../../etc/passwd",
        "/assets/../../index.html",
        "/%2e%2e/%2e%2e/etc/passwd",
        "/%2E%2E%2F%2E%2E%2Fledger.sqlite",
        "/assets/..%2f..%2findex.html",
        "/..%5c..%5cWindows%5cwin.ini",
        "/C:/Windows/win.ini",
        "/.secret",
        "/assets/%00x.js",
    ] {
        let (s, body) = raw_get(&base, target);
        assert!(s == 400 || s == 404, "{target} → {s}");
        assert!(
            !body.contains("hidden") && !body.contains("[fonts]") && !body.contains("root:"),
            "{target} leaked"
        );
    }
    // Double encoding is only a literal name, never traversal (SPA fallback or 404).
    let (s, body) = raw_get(&base, "/%252e%252e/%252e%252e/etc/passwd");
    assert!(s == 404 || body.contains("<title>Scribe</title>"), "{s}");
    // No directory listing.
    let (s, _) = raw_get(&base, "/assets/");
    assert_eq!(s, 404);
    // The ledger is never reachable through static routes.
    for f in ["/ledger.lock", "/ledger.sqlite", "/blobs"] {
        let (s, body) = raw_get(&base, f);
        assert!(!body.contains("SQLite format"), "{f}");
        assert!(
            s == 404 || body.contains("<title>Scribe</title>"),
            "{f} → {s}"
        );
    }
}

#[test]
fn web_root_overlapping_server_private_paths_is_refused() {
    let web = web_root();
    // Ledger inside the web root, web root inside the ledger, verifier inside the web root.
    assert!(WebRoot::new(&web, &[&web.join("ledger")]).is_err());
    assert!(WebRoot::new(&web.join("assets").join(".."), &[&web]).is_err());
    let verifier = web.join("verifier");
    std::fs::write(&verifier, "x").unwrap();
    assert!(WebRoot::new(&web, &[&verifier]).is_err());
    // A root without index.html is refused.
    let empty = tmp("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert!(WebRoot::new(&empty, &[]).is_err());
    assert!(WebRoot::new(&tmp("missing"), &[]).is_err());
}

#[test]
fn without_web_root_behavior_is_unchanged() {
    // Authenticated: every path (static-looking or not) is 401 without a token, as before.
    let srv = server(true, None);
    for p in ["/", "/index.html", "/assets/x.js"] {
        assert_eq!(json(&srv.base(), p, None).0, 401, "{p}");
        let (s, v) = json(&srv.base(), p, srv.tok());
        assert_eq!(
            (s, v["error"].as_str()),
            (404, Some("unknown_route")),
            "{p}"
        );
    }
    // No-auth loopback dev server: plain API 404s, no static content.
    let dev = server(false, None);
    let (s, v) = json(&dev.base(), "/", None);
    assert_eq!((s, v["error"].as_str()), (404, Some("unknown_route")));
}
