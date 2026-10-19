//! Slice 1F-B1: administrative backup upload and inspection through a real `scribe-server`
//! (loopback, ephemeral port, injectable clock). Uploads use a raw HTTP/1.1 client so the tests
//! control exactly which bytes are sent (headers only, partial bodies, chunked bodies, pauses).
//! Every test checks that the notebook ledger is never modified.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use backup_export::{export_backup, Credential, HttpSource, Secret};
use portable_backup::Limits;
use rusqlite::types::Value as Sql;
use scribe_server::config::{Config, DiscoverySection, ServerSection};
use scribe_server::netpolicy::LanPolicy;
use scribe_server::restore::RestoreLimits;
use scribe_server::{serve_with_restore_limits, Running};
use scribe_sync_server::{ServerConfig, ServerHandle, Store, TokenVerifier};
use serde_json::{json, Value};
use sync_engine::testkit;
use sync_protocol::{sha256_hex, LibraryId};

// ------------------------------------------------------------------ harness

static N: AtomicU64 = AtomicU64::new(0);

fn temp(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "scribe-restore-{label}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        N.fetch_add(1, Ordering::SeqCst)
    ))
}

struct Env {
    dir: PathBuf,
    clock: Arc<AtomicU64>,
    events: Arc<Mutex<Vec<String>>>,
}

impl Env {
    /// A product state directory whose ledger already holds an unrelated library and blob.
    fn new(label: &str) -> Self {
        let env = Self {
            dir: temp(label),
            clock: Arc::new(AtomicU64::new(1_790_000_000_000)),
            events: Arc::new(Mutex::new(Vec::new())),
        };
        let store = Store::open(&env.dir.join("ledger")).unwrap();
        let (bytes, _) = testkit::blob_background();
        store.put_blob(sha256_hex(&bytes).as_str(), &bytes).unwrap();
        store
            .bootstrap(&testkit::genesis().to_canonical_bytes().unwrap())
            .unwrap();
        env
    }

    fn config(&self) -> Config {
        Config {
            server: ServerSection {
                listen: "127.0.0.1:0".parse().unwrap(),
                state_dir: self.dir.clone(),
                web_root: None,
                name: "Scribe Server on Testhost".into(),
            },
            discovery: DiscoverySection { advertise: false },
        }
    }

    fn start(&self, limits: RestoreLimits) -> Srv {
        let c = self.clock.clone();
        let e = self.events.clone();
        let running = serve_with_restore_limits(
            &self.config(),
            Arc::new(move || c.load(Ordering::SeqCst)),
            Arc::new(move |line: &str| e.lock().unwrap().push(line.to_string())),
            Arc::new(LanPolicy::for_this_host()),
            limits,
        )
        .unwrap();
        Srv {
            addr: running.handle.addr(),
            base: running.handle.base_url(),
            running: Some(running),
        }
    }

    fn advance(&self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }

    fn log(&self) -> String {
        self.events.lock().unwrap().join("\n")
    }

    fn staging(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.dir.join("ledger").join("restore-staging"))
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Every ledger row that matters and every installed blob file.
    fn ledger(&self) -> LedgerState {
        let c = rusqlite::Connection::open(self.dir.join("ledger").join("ledger.sqlite3")).unwrap();
        let mut tables = BTreeMap::new();
        for t in [
            "libraries",
            "current_state",
            "operations",
            "replicas",
            "blobs",
        ] {
            let mut st = c.prepare(&format!("SELECT * FROM {t} ORDER BY 1")).unwrap();
            let n = st.column_count();
            let rows = st
                .query_map([], |r| (0..n).map(|i| r.get::<_, Sql>(i)).collect())
                .unwrap()
                .collect::<Result<Vec<Vec<Sql>>, _>>()
                .unwrap();
            tables.insert(t, rows);
        }
        let mut files = BTreeMap::new();
        let blobs = self.dir.join("ledger").join("blobs");
        for d in std::fs::read_dir(&blobs).unwrap().flatten() {
            for f in std::fs::read_dir(d.path()).unwrap().flatten() {
                files.insert(
                    f.path().strip_prefix(&blobs).unwrap().display().to_string(),
                    std::fs::read(f.path()).unwrap(),
                );
            }
        }
        LedgerState { tables, files }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Debug, PartialEq)]
struct LedgerState {
    tables: BTreeMap<&'static str, Vec<Vec<Sql>>>,
    files: BTreeMap<String, Vec<u8>>,
}

struct Srv {
    addr: SocketAddr,
    base: String,
    running: Option<Running>,
}

impl Srv {
    fn running(&self) -> &Running {
        self.running.as_ref().unwrap()
    }

    fn stop(mut self) {
        if let Some(r) = self.running.take() {
            r.handle.shutdown();
        }
    }
}

fn limits() -> RestoreLimits {
    RestoreLimits::default()
}

/// Wait (bounded) until `f` holds.
fn eventually(what: &str, f: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ------------------------------------------------------------------ JSON control calls

#[derive(Default, Clone)]
struct H {
    cookie: Option<String>,
    bearer: Option<String>,
    csrf: bool,
    origin: Option<String>,
    content_type: Option<&'static str>,
    expect_continue: bool,
}

fn cookie(c: &str) -> H {
    H {
        cookie: Some(c.into()),
        csrf: true,
        content_type: Some("application/zip"),
        ..H::default()
    }
}

struct Resp {
    status: u16,
    body: Value,
    headers: String,
    set_cookie: Option<String>,
}

fn call(base: &str, method: &str, path: &str, h: &H, body: Option<Value>) -> Resp {
    let mut req = ureq::request(method, &format!("{base}{path}"));
    if let Some(c) = &h.cookie {
        req = req.set("Cookie", &format!("scribe_session={c}"));
    }
    if let Some(b) = &h.bearer {
        req = req.set("Authorization", &format!("Bearer {b}"));
    }
    if h.csrf {
        req = req.set("X-Scribe-Request", "1");
    }
    let r = match body {
        Some(b) => req
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => req.call(),
    };
    let resp = match r {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("transport: {e}"),
    };
    let status = resp.status();
    let set_cookie = resp.header("set-cookie").map(str::to_string);
    let text = resp.into_string().unwrap_or_default();
    Resp {
        status,
        body: serde_json::from_str(&text).unwrap_or(Value::Null),
        headers: String::new(),
        set_cookie,
    }
}

fn csrf() -> H {
    H {
        csrf: true,
        ..H::default()
    }
}

fn cookie_of(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches("scribe_session=")
        .to_string()
}

fn claim(env: &Env, srv: &Srv) -> String {
    let c = env.clock.clone();
    let (_, _, store) =
        scribe_server::prepare(&env.config(), Arc::new(move || c.load(Ordering::SeqCst))).unwrap();
    let code = store.issue_setup_code().unwrap().unwrap();
    let r = call(
        &srv.base,
        "POST",
        "/control/v1/claim",
        &csrf(),
        Some(json!({ "setup_code": code })),
    );
    assert_eq!(r.status, 200);
    cookie_of(&r.set_cookie.unwrap())
}

/// A paired browser: approved as member by `admin`, or as administrator by the host operator.
fn browser(srv: &Srv, admin: &str, name: &str, as_admin: bool) -> String {
    let c = call(
        &srv.base,
        "POST",
        "/control/v1/pairing",
        &csrf(),
        Some(json!({ "client_type": "browser", "display_name": name })),
    );
    let id = c.body["request_id"].as_str().unwrap().to_string();
    if as_admin {
        let code = c.body["comparison_code"].as_str().unwrap().replace(' ', "");
        srv.running()
            .plane
            .store
            .approve_admin_by_code(&code)
            .unwrap();
    } else {
        let a = call(
            &srv.base,
            "POST",
            &format!("/control/v1/admin/pairing/{id}/approve"),
            &cookie(admin),
            None,
        );
        assert_eq!(a.status, 200);
    }
    let p = call(
        &srv.base,
        "POST",
        &format!("/control/v1/pairing/{id}/poll"),
        &csrf(),
        Some(json!({ "request_secret": c.body["request_secret"] })),
    );
    cookie_of(&p.set_cookie.unwrap())
}

fn kindle(srv: &Srv, admin: &str) -> String {
    let c = call(
        &srv.base,
        "POST",
        "/control/v1/pairing",
        &csrf(),
        Some(json!({ "client_type": "kindle", "display_name": "Kindle Scribe" })),
    );
    let id = c.body["request_id"].as_str().unwrap().to_string();
    call(
        &srv.base,
        "POST",
        &format!("/control/v1/admin/pairing/{id}/approve"),
        &cookie(admin),
        None,
    );
    let p = call(
        &srv.base,
        "POST",
        &format!("/control/v1/pairing/{id}/poll"),
        &csrf(),
        Some(json!({ "request_secret": c.body["request_secret"] })),
    );
    p.body["credential"].as_str().unwrap().to_string()
}

fn device_id(srv: &Srv, c: &str) -> String {
    let s = call(&srv.base, "GET", "/control/v1/status", &cookie(c), None);
    s.body["session"]["device_id"].as_str().unwrap().to_string()
}

// ------------------------------------------------------------------ raw HTTP

const UPLOAD: &str = "/control/v1/admin/restore/jobs";

fn headers(srv: &Srv, method: &str, path: &str, h: &H, framing: &str) -> String {
    let mut s = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{framing}",
        srv.addr
    );
    if let Some(c) = &h.cookie {
        s.push_str(&format!("Cookie: scribe_session={c}\r\n"));
    }
    if let Some(b) = &h.bearer {
        s.push_str(&format!("Authorization: Bearer {b}\r\n"));
    }
    if h.csrf {
        s.push_str("X-Scribe-Request: 1\r\n");
    }
    if let Some(o) = &h.origin {
        s.push_str(&format!("Origin: {o}\r\n"));
    }
    if let Some(t) = h.content_type {
        s.push_str(&format!("Content-Type: {t}\r\n"));
    }
    if h.expect_continue {
        s.push_str("Expect: 100-continue\r\n");
    }
    s.push_str("\r\n");
    s
}

fn connect(srv: &Srv) -> TcpStream {
    let s = TcpStream::connect(srv.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s
}

/// Read one response (the server closes the connection after it).
fn read_response(mut s: TcpStream) -> Resp {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if buf.is_empty() => panic!("no response: {e}"),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("a complete response head");
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    Resp {
        status,
        body: serde_json::from_str(body).unwrap_or(Value::Null),
        headers: head.to_ascii_lowercase(),
        set_cookie: None,
    }
}

/// Headers only, declaring `declared` bytes that are never sent: a reply proves the server
/// decided without reading the body.
fn headers_only(srv: &Srv, method: &str, path: &str, h: &H, declared: u64) -> Resp {
    let mut s = connect(srv);
    s.write_all(
        headers(
            srv,
            method,
            path,
            h,
            &format!("Content-Length: {declared}\r\n"),
        )
        .as_bytes(),
    )
    .unwrap();
    read_response(s)
}

fn upload(srv: &Srv, h: &H, body: &[u8]) -> Resp {
    let mut s = connect(srv);
    s.write_all(
        headers(
            srv,
            "POST",
            UPLOAD,
            h,
            &format!("Content-Length: {}\r\n", body.len()),
        )
        .as_bytes(),
    )
    .unwrap();
    s.write_all(body).unwrap();
    read_response(s)
}

fn upload_chunked(srv: &Srv, h: &H, body: &[u8], chunk: usize) -> Resp {
    let mut s = connect(srv);
    s.write_all(headers(srv, "POST", UPLOAD, h, "Transfer-Encoding: chunked\r\n").as_bytes())
        .unwrap();
    for c in body.chunks(chunk) {
        s.write_all(format!("{:x}\r\n", c.len()).as_bytes())
            .unwrap();
        s.write_all(c).unwrap();
        s.write_all(b"\r\n").unwrap();
    }
    s.write_all(b"0\r\n\r\n").unwrap();
    read_response(s)
}

fn job(srv: &Srv, method: &str, id: &str, h: &H) -> Resp {
    call(&srv.base, method, &format!("{UPLOAD}/{id}"), h, None)
}

// ------------------------------------------------------------------ backups (Slice 1C)

struct Backup {
    dir: PathBuf,
    path: PathBuf,
    bytes: Vec<u8>,
}

impl Drop for Backup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn second_library() -> sync_protocol::SyncSnapshotV1 {
    let mut g = testkit::genesis();
    g.library_id = LibraryId::from_uuid(testkit::uuid7(1, 2)).unwrap();
    g
}

/// A real export of a source server holding two libraries (one shared background blob).
fn backup() -> Backup {
    let dir = temp("backup");
    let data = dir.join("source");
    let store = Store::open(&data).unwrap();
    let (bytes, _) = testkit::blob_background();
    store.put_blob(sha256_hex(&bytes).as_str(), &bytes).unwrap();
    for s in [testkit::genesis(), second_library()] {
        store.bootstrap(&s.to_canonical_bytes().unwrap()).unwrap();
    }
    let server = ServerHandle::start_with(
        "127.0.0.1:0".parse().unwrap(),
        store,
        ServerConfig {
            auth: Some(TokenVerifier::for_token("export-test-token")),
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let path = dir.join("ignite-backup.zip");
    let src = HttpSource::new(
        &server.base_url(),
        Some(Credential::Bearer(Secret::new("export-test-token"))),
    )
    .unwrap();
    export_backup(&src, &path, &Limits::DEFAULT).unwrap();
    server.shutdown();
    let bytes = std::fs::read(&path).unwrap();
    Backup { dir, path, bytes }
}

impl Backup {
    fn entries(&self) -> Vec<(String, Vec<u8>)> {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(&self.bytes)).unwrap();
        (0..z.len())
            .map(|i| {
                let mut e = z.by_index(i).unwrap();
                let mut b = Vec::new();
                e.read_to_end(&mut b).unwrap();
                (e.name().to_string(), b)
            })
            .collect()
    }

    /// The archive with its entries edited (Deflate, as the exporter writes).
    fn variant(&self, edit: impl FnOnce(&mut Vec<(String, Vec<u8>)>)) -> Vec<u8> {
        let mut entries = self.entries();
        edit(&mut entries);
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let o = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (n, d) in entries {
            w.start_file(n, o).unwrap();
            w.write_all(&d).unwrap();
        }
        w.finish().unwrap().into_inner()
    }
}

fn edit_entry(entries: &mut [(String, Vec<u8>)], prefix: &str, f: impl FnOnce(&mut Vec<u8>)) {
    let e = entries
        .iter_mut()
        .find(|(n, _)| n.starts_with(prefix))
        .unwrap_or_else(|| panic!("no entry {prefix}"));
    f(&mut e.1);
}

// ------------------------------------------------------------------ tests

#[test]
fn an_admin_upload_prepares_a_read_only_job_and_can_cancel_it() {
    let env = Env::new("ok");
    let b = backup();
    let srv = env.start(limits());
    let admin = claim(&env, &srv);
    let before = env.ledger();
    let plan = backup_archive::inspect_backup(&b.path, &Limits::DEFAULT).unwrap();

    let r = upload(&srv, &cookie(&admin), &b.bytes);
    assert_eq!(r.status, 201, "{:?}", r.body);
    assert!(r.headers.contains("cache-control: no-store"));
    let v = &r.body;
    let id = v["job_id"].as_str().unwrap().to_string();
    assert_eq!(v["v"], 1);
    assert_eq!(v["state"], "ready");
    assert_eq!(v["expires_in_s"], 900);
    assert_eq!(v["archive"]["sha256"], sha256_hex(&b.bytes).as_str());
    assert_eq!(v["archive"]["bytes"], b.bytes.len());
    assert_eq!(v["archive"]["format"], plan.format);
    assert_eq!(v["archive"]["format_version"], plan.format_version);
    assert_eq!(v["scope"], "current_state_only");
    assert_eq!(v["replaces_existing"], false);
    assert_eq!(v["not_restored"], json!(plan.not_restored));
    assert_eq!(v["totals"]["libraries"], 2);
    assert_eq!(v["totals"]["notebooks"], plan.total_notebooks);
    assert_eq!(v["totals"]["pages"], plan.total_pages);
    assert_eq!(v["totals"]["blobs"], 1);
    assert_eq!(v["totals"]["blob_bytes"], plan.total_blob_bytes);
    let confirmation = v["confirmation"].as_str().unwrap().to_string();
    assert_eq!(confirmation.len(), 43, "32 random bytes, base64url");
    // Fresh destinations: valid ids, distinct, never a source id, never an existing library
    // (the ledger already holds testkit's library, which is also a source id here).
    let mut dests = std::collections::BTreeSet::new();
    for (l, p) in v["libraries"]
        .as_array()
        .unwrap()
        .iter()
        .zip(&plan.libraries)
    {
        assert_eq!(l["source_library_id"], p.source_library_id.to_string());
        assert_eq!(
            l["source_snapshot_sha256"],
            p.source_snapshot_sha256.as_str()
        );
        assert_eq!(l["notebook_count"], p.notebook_count);
        assert_eq!(l["page_count"], p.page_count);
        let d = LibraryId::parse(l["destination_library_id"].as_str().unwrap()).unwrap();
        assert_eq!(d.as_uuid().get_version_num(), 7);
        assert!(plan.libraries.iter().all(|p| p.source_library_id != d));
        assert!(dests.insert(d));
    }
    // Only the private staged archive exists; it holds exactly the uploaded bytes.
    let staging = env.staging();
    assert_eq!(staging.len(), 1);
    assert!(staging[0].starts_with("job-upload-"));
    let staged = env
        .dir
        .join("ledger/restore-staging")
        .join(&staging[0])
        .join("archive.zip");
    assert_eq!(std::fs::read(&staged).unwrap(), b.bytes);
    assert_eq!(env.ledger(), before, "the ledger is untouched");

    // GET: same summary and the same immutable mapping, never the confirmation again.
    let g = job(&srv, "GET", &id, &cookie(&admin));
    assert_eq!(g.status, 200);
    assert!(g.body.get("confirmation").is_none());
    let mut without = v.clone();
    without.as_object_mut().unwrap().remove("confirmation");
    assert_eq!(g.body, without);
    assert!(!env.log().contains(&confirmation), "never logged");
    assert!(env
        .log()
        .contains(&format!("event=restore_upload_prepared job_id={id}")));

    // DELETE: removes the job and its staged data; a second DELETE / GET is "not found".
    let d = job(&srv, "DELETE", &id, &cookie(&admin));
    assert_eq!((d.status, &d.body["ok"]), (200, &json!(true)));
    assert!(env.staging().is_empty());
    assert_eq!(job(&srv, "DELETE", &id, &cookie(&admin)).status, 404);
    assert_eq!(job(&srv, "GET", &id, &cookie(&admin)).status, 404);

    // A chunked upload (no Content-Length) works the same way.
    let r = upload_chunked(&srv, &cookie(&admin), &b.bytes, 1000);
    assert_eq!(r.status, 201, "{:?}", r.body);
    assert_eq!(r.body["archive"]["sha256"], sha256_hex(&b.bytes).as_str());
    assert_eq!(env.ledger(), before);
    srv.stop();
}

#[test]
fn every_other_caller_is_refused_before_the_body_is_read() {
    let env = Env::new("authz");
    let b = backup();
    let srv = env.start(limits());
    let admin = claim(&env, &srv);
    let member = browser(&srv, &admin, "Member browser", false);
    let kindle = kindle(&srv, &admin);
    let other_admin = browser(&srv, &admin, "Second admin", true);
    let before = env.ledger();

    let anon = H {
        csrf: true,
        content_type: Some("application/zip"),
        ..H::default()
    };
    let cases: Vec<(&str, H, u16, &str)> = vec![
        ("anonymous", anon.clone(), 401, "unauthorized"),
        (
            "anonymous, Expect: 100-continue",
            H {
                expect_continue: true,
                ..anon.clone()
            },
            401,
            "unauthorized",
        ),
        (
            "unknown cookie",
            H {
                cookie: Some("junk".into()),
                ..anon.clone()
            },
            401,
            "unauthorized",
        ),
        (
            "Kindle bearer",
            H {
                bearer: Some(kindle.clone()),
                ..anon.clone()
            },
            403,
            "browser_session_required",
        ),
        (
            "admin credential as Bearer",
            H {
                bearer: Some(admin.clone()),
                ..anon.clone()
            },
            403,
            "browser_session_required",
        ),
        (
            "admin cookie plus a Bearer",
            H {
                bearer: Some(admin.clone()),
                ..cookie(&admin)
            },
            403,
            "browser_session_required",
        ),
        ("member browser", cookie(&member), 403, "forbidden"),
        (
            "no CSRF marker",
            H {
                csrf: false,
                ..cookie(&admin)
            },
            403,
            "csrf",
        ),
        (
            "cross-origin",
            H {
                origin: Some("http://evil.example".into()),
                ..cookie(&admin)
            },
            403,
            "csrf",
        ),
        (
            "no content type",
            H {
                content_type: None,
                ..cookie(&admin)
            },
            400,
            "wrong_content_type",
        ),
        (
            "wrong content type",
            H {
                content_type: Some("application/octet-stream"),
                ..cookie(&admin)
            },
            400,
            "wrong_content_type",
        ),
    ];
    for (what, h, status, code) in &cases {
        let r = headers_only(&srv, "POST", UPLOAD, h, 1_000_000);
        assert_eq!(
            (r.status, r.body["error"].as_str()),
            (*status, Some(*code)),
            "{what}"
        );
        assert!(r.headers.contains("connection: close"), "{what}: no drain");
        assert!(!r.headers.contains("100 continue"), "{what}");
        assert!(env.staging().is_empty(), "{what}: nothing staged");
    }
    // Oversized declared length, from the administrator.
    let r = headers_only(&srv, "POST", UPLOAD, &cookie(&admin), (2 << 30) + 1);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (413, Some("body_too_large"))
    );
    assert!(env.staging().is_empty());

    // Non-loopback peer (simulated: every peer treated as remote).
    let hooks = &srv.running().plane.restore.hooks;
    hooks.treat_peers_as_remote.store(true, Ordering::SeqCst);
    let r = headers_only(&srv, "POST", UPLOAD, &cookie(&admin), 1000);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (403, Some("loopback_required"))
    );
    hooks.treat_peers_as_remote.store(false, Ordering::SeqCst);

    // A real job: only its owner may see or cancel it.
    let r = upload(&srv, &cookie(&admin), &b.bytes);
    assert_eq!(r.status, 201);
    let id = r.body["job_id"].as_str().unwrap().to_string();
    let unknown = job(
        &srv,
        "GET",
        "01900000-0000-7000-8000-000000000000",
        &cookie(&admin),
    );
    assert_eq!(unknown.status, 404);
    for method in ["GET", "DELETE"] {
        let foreign = job(&srv, method, &id, &cookie(&other_admin));
        assert_eq!(
            (foreign.status, &foreign.body),
            (404, &unknown.body),
            "{method}"
        );
        for (what, h, status) in [
            ("anonymous", csrf(), 401),
            ("member", cookie(&member), 403),
            (
                "Kindle",
                H {
                    bearer: Some(kindle.clone()),
                    csrf: true,
                    ..H::default()
                },
                403,
            ),
            (
                "admin as Bearer",
                H {
                    bearer: Some(admin.clone()),
                    csrf: true,
                    ..H::default()
                },
                403,
            ),
        ] {
            assert_eq!(job(&srv, method, &id, &h).status, status, "{method} {what}");
        }
    }
    let no_csrf = H {
        csrf: false,
        ..cookie(&admin)
    };
    assert_eq!(job(&srv, "DELETE", &id, &no_csrf).status, 403);
    hooks.treat_peers_as_remote.store(true, Ordering::SeqCst);
    assert_eq!(job(&srv, "GET", &id, &cookie(&admin)).status, 403);
    hooks.treat_peers_as_remote.store(false, Ordering::SeqCst);
    assert_eq!(
        job(&srv, "GET", &id, &cookie(&admin)).status,
        200,
        "still there"
    );
    assert_eq!(env.staging().len(), 1);

    // A revoked administrator is refused everywhere.
    let me = device_id(&srv, &admin);
    srv.running().plane.store.revoke(&me).unwrap();
    assert_eq!(job(&srv, "GET", &id, &cookie(&admin)).status, 401);
    assert_eq!(
        headers_only(&srv, "POST", UPLOAD, &cookie(&admin), 10).status,
        401
    );
    assert_eq!(env.ledger(), before);
    srv.stop();
}

#[test]
fn an_admin_revoked_during_the_upload_gets_no_job() {
    let env = Env::new("revoked");
    let b = backup();
    let srv = env.start(limits());
    let admin = claim(&env, &srv);
    let me = device_id(&srv, &admin);
    let before = env.ledger();
    let mut s = connect(&srv);
    s.write_all(
        headers(
            &srv,
            "POST",
            UPLOAD,
            &cookie(&admin),
            &format!("Content-Length: {}\r\n", b.bytes.len()),
        )
        .as_bytes(),
    )
    .unwrap();
    let half = b.bytes.len() / 2;
    s.write_all(&b.bytes[..half]).unwrap();
    eventually("the upload is staged", || env.staging().len() == 1);
    srv.running().plane.store.revoke(&me).unwrap();
    s.write_all(&b.bytes[half..]).unwrap();
    let r = read_response(s);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (401, Some("unauthorized"))
    );
    assert!(env.staging().is_empty(), "the staged archive was destroyed");
    assert_eq!(srv.running().plane.restore.job_count(), 0);
    assert_eq!(env.ledger(), before);
    srv.stop();
}

#[test]
fn transport_limits_timeouts_and_failures_clean_up() {
    let env = Env::new("transport");
    let b = backup();
    let cap = 64 * 1024;
    let srv = env.start(RestoreLimits {
        max_upload_bytes: cap as u64,
        idle_timeout: Duration::from_millis(400),
        upload_deadline: Duration::from_secs(3),
        ..limits()
    });
    let admin = claim(&env, &srv);
    let before = env.ledger();
    let restore = &srv.running().plane.restore;
    let clean = |what: &str| {
        eventually(what, || {
            env.staging().is_empty() && !restore.upload_in_progress()
        })
    };

    // Chunked past the cap (no Content-Length): refused as soon as the cap is exceeded.
    let mut s = connect(&srv);
    s.write_all(
        headers(
            &srv,
            "POST",
            UPLOAD,
            &cookie(&admin),
            "Transfer-Encoding: chunked\r\n",
        )
        .as_bytes(),
    )
    .unwrap();
    for c in vec![7u8; cap + 1].chunks(16 * 1024) {
        s.write_all(format!("{:x}\r\n", c.len()).as_bytes())
            .unwrap();
        s.write_all(c).unwrap();
        s.write_all(b"\r\n").unwrap();
    }
    let r = read_response(s);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (413, Some("body_too_large"))
    );
    assert!(r.headers.contains("connection: close"));
    clean("413 cleanup");

    // A client that disconnects mid-upload: the writer stops and everything is released.
    let mut s = connect(&srv);
    s.write_all(
        headers(
            &srv,
            "POST",
            UPLOAD,
            &cookie(&admin),
            "Content-Length: 50000\r\n",
        )
        .as_bytes(),
    )
    .unwrap();
    s.write_all(&[1u8; 20000]).unwrap();
    eventually("the upload is staged", || env.staging().len() == 1);
    drop(s);
    clean("abort cleanup");

    // Idle: part of the body, then silence.
    let mut s = connect(&srv);
    s.write_all(
        headers(
            &srv,
            "POST",
            UPLOAD,
            &cookie(&admin),
            "Content-Length: 50000\r\n",
        )
        .as_bytes(),
    )
    .unwrap();
    s.write_all(&[1u8; 1000]).unwrap();
    let r = read_response(s);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (408, Some("upload_idle_timeout"))
    );
    clean("idle cleanup");

    // Deadline: a steady trickle that never finishes in time.
    let mut s = connect(&srv);
    s.write_all(
        headers(
            &srv,
            "POST",
            UPLOAD,
            &cookie(&admin),
            "Content-Length: 50000\r\n",
        )
        .as_bytes(),
    )
    .unwrap();
    // Each step waits up to 150 ms for the reply (never writing after it: on Windows a write
    // to the closed connection would reset it and discard the reply).
    s.set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) && s.peek(&mut [0u8; 1]).is_err() {
        s.write_all(&[1u8; 10]).unwrap();
    }
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let r = read_response(s);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (408, Some("upload_deadline"))
    );
    clean("deadline cleanup");

    // Storage exhausted while writing.
    restore
        .hooks
        .fail_write_after_bytes
        .store(100, Ordering::SeqCst);
    let r = upload(&srv, &cookie(&admin), &b.bytes);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (507, Some("insufficient_storage"))
    );
    restore
        .hooks
        .fail_write_after_bytes
        .store(0, Ordering::SeqCst);
    clean("disk-full cleanup");

    // One upload at a time; then the per-administrator job quota.
    let mut held = connect(&srv);
    held.write_all(
        headers(
            &srv,
            "POST",
            UPLOAD,
            &cookie(&admin),
            &format!("Content-Length: {}\r\n", b.bytes.len()),
        )
        .as_bytes(),
    )
    .unwrap();
    held.write_all(&b.bytes[..100]).unwrap();
    eventually("the first upload holds the slot", || {
        restore.upload_in_progress()
    });
    let r = headers_only(&srv, "POST", UPLOAD, &cookie(&admin), 10);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (429, Some("upload_busy"))
    );
    held.write_all(&b.bytes[100..]).unwrap();
    assert_eq!(read_response(held).status, 201);
    let r = headers_only(&srv, "POST", UPLOAD, &cookie(&admin), 10);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (429, Some("too_many_jobs"))
    );
    assert_eq!(env.staging().len(), 1, "only the prepared job");

    // Ordinary control routes keep the 64 KiB in-memory limit, and other methods or paths
    // never reach the streaming path (they are read by the ordinary path first).
    let big = vec![b'x'; 70 * 1024];
    for (method, path) in [
        ("POST", "/control/v1/claim"),
        ("PUT", UPLOAD),
        ("POST", "/control/v1/admin/restore/jobs/x"),
        ("POST", "/control/v1/admin/restore/jobs/x/confirm"),
    ] {
        let r = upload_to(&srv, method, path, &cookie(&admin), &big);
        assert_eq!(r.status, 413, "{method} {path}");
    }
    let small = upload_to(
        &srv,
        "POST",
        "/control/v1/admin/restore/jobs/x/confirm",
        &cookie(&admin),
        b"{}",
    );
    assert_eq!(small.status, 404, "no confirmation route in 1F-B1");
    assert_eq!(env.staging().len(), 1);
    assert_eq!(env.ledger(), before);
    srv.stop();
}

fn upload_to(srv: &Srv, method: &str, path: &str, h: &H, body: &[u8]) -> Resp {
    let mut s = connect(srv);
    s.write_all(
        headers(
            srv,
            method,
            path,
            h,
            &format!("Content-Length: {}\r\n", body.len()),
        )
        .as_bytes(),
    )
    .unwrap();
    let _ = s.write_all(body);
    read_response(s)
}

#[test]
fn a_large_upload_streams_through_a_bounded_queue() {
    let env = Env::new("large");
    let srv = env.start(limits());
    let admin = claim(&env, &srv);
    let before = env.ledger();
    // 48 MiB of non-ZIP bytes: fully streamed to disk, then refused by inspection.
    let body = vec![0u8; 48 << 20];
    let r = upload(&srv, &cookie(&admin), &body);
    assert_eq!(
        (r.status, r.body["archive_error"].as_str()),
        (422, Some("zip_not_zip"))
    );
    assert!(env.staging().is_empty());
    assert!(!srv.running().plane.restore.upload_in_progress());
    assert_eq!(env.ledger(), before);
    srv.stop();
}

#[test]
fn invalid_archives_are_refused_and_leave_nothing() {
    let env = Env::new("invalid");
    let b = backup();
    let srv = env.start(limits());
    let admin = claim(&env, &srv);
    let before = env.ledger();
    let blob = |f: fn(&mut Vec<u8>)| b.variant(|e| edit_entry(e, "blobs/", f));
    let snapshot = |f: fn(&mut Vec<u8>)| b.variant(|e| edit_entry(e, "libraries/", f));
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("garbage", b"not a zip at all".to_vec(), "zip_not_zip"),
        ("truncated", b.bytes[..b.bytes.len() / 2].to_vec(), "zip_"),
        (
            "corrupt snapshot",
            snapshot(|s| s.insert(0, b' ')),
            "backup_",
        ),
        ("corrupt blob", blob(|d| d[0] ^= 1), "backup_"),
        (
            // 40 MiB of zeros compresses to ~40 KiB; refused from its declared size before
            // inflating (a lying declaration is cut off by the bounded inflater, Slice 1D).
            "expands past the blob limit",
            blob(|d| *d = vec![0u8; 40 << 20]),
            "zip_limit_exceeded",
        ),
        (
            "unsupported version",
            b.variant(|e| {
                edit_entry(e, "manifest.json", |m| {
                    let mut v: Value = serde_json::from_slice(m).unwrap();
                    v["format_version"] = json!(99);
                    *m = serde_json::to_vec(&v).unwrap();
                })
            }),
            "backup_unsupported_version",
        ),
    ];
    for (what, body, code) in &cases {
        assert!(body.len() < 1 << 20, "{what}: small compressed size");
        let r = upload(&srv, &cookie(&admin), body);
        assert_eq!(
            (r.status, r.body["error"].as_str()),
            (422, Some("archive_invalid")),
            "{what}: {:?}",
            r.body
        );
        let got = r.body["archive_error"].as_str().unwrap();
        assert!(got.starts_with(code), "{what}: {got}");
        assert!(env.staging().is_empty(), "{what}");
        assert_eq!(srv.running().plane.restore.job_count(), 0);
    }
    assert_eq!(env.ledger(), before);
    assert!(env.log().contains("event=restore_upload_refused"));
    srv.stop();
}

#[test]
fn expansion_limits_are_enforced_while_inspecting() {
    // A valid, small (compressed) export that expands past this server's blob allowance.
    let env = Env::new("tight");
    let b = backup();
    let srv = env.start(RestoreLimits {
        verify: Limits {
            max_total_blob_bytes: 100,
            ..scribe_server::restore::verification_limits()
        },
        ..limits()
    });
    let admin = claim(&env, &srv);
    let before = env.ledger();
    let r = upload(&srv, &cookie(&admin), &b.bytes);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (422, Some("archive_invalid")),
        "{:?}",
        r.body
    );
    assert!(env.staging().is_empty());
    assert_eq!(env.ledger(), before);
    srv.stop();
}

#[test]
fn prepared_jobs_expire_without_another_request() {
    let env = Env::new("expire");
    let b = backup();
    let srv = env.start(RestoreLimits {
        sweep_interval: Duration::from_millis(50),
        ..limits()
    });
    let admin = claim(&env, &srv);
    let r = upload(&srv, &cookie(&admin), &b.bytes);
    assert_eq!(r.status, 201);
    let id = r.body["job_id"].as_str().unwrap().to_string();
    env.advance(15 * 60 * 1000 - 1);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(srv.running().plane.restore.job_count(), 1, "not yet");
    env.advance(1);
    eventually("the sweeper removes the job", || {
        srv.running().plane.restore.job_count() == 0 && env.staging().is_empty()
    });
    assert!(env
        .log()
        .contains(&format!("event=restore_job_expired job_id={id}")));
    assert_eq!(job(&srv, "GET", &id, &cookie(&admin)).status, 404);
    // The quota was released with it.
    assert_eq!(upload(&srv, &cookie(&admin), &b.bytes).status, 201);
    srv.stop();

    // Without a sweep in between, the owner's GET finds it expired (410), never revived.
    let env = Env::new("expire-lazy");
    let srv = env.start(RestoreLimits {
        sweep_interval: Duration::from_secs(3600),
        ..limits()
    });
    let admin = claim(&env, &srv);
    let r = upload(&srv, &cookie(&admin), &b.bytes);
    let id = r.body["job_id"].as_str().unwrap().to_string();
    env.advance(15 * 60 * 1000);
    let g = job(&srv, "GET", &id, &cookie(&admin));
    assert_eq!((g.status, g.body["error"].as_str()), (410, Some("expired")));
    assert!(env.staging().is_empty());
    assert_eq!(job(&srv, "GET", &id, &cookie(&admin)).status, 404);
    srv.stop();
}

#[test]
fn startup_removes_abandoned_staging() {
    let env = Env::new("startup");
    let b = backup();
    let srv = env.start(limits());
    let admin = claim(&env, &srv);
    let r = upload(&srv, &cookie(&admin), &b.bytes);
    assert_eq!(r.status, 201);
    let id = r.body["job_id"].as_str().unwrap().to_string();
    srv.stop(); // like a crash: the staged job stays on disk
                // An interrupted restore's staging too (Slice 1E layout).
    std::fs::create_dir_all(env.dir.join("ledger/restore-staging/job-1-2-3/blobs")).unwrap();
    assert_eq!(env.staging().len(), 2);
    let before = env.ledger();
    let srv = env.start(limits());
    assert!(env.staging().is_empty());
    assert!(env.log().contains("event=restore_staging_removed count=2"));
    assert_eq!(job(&srv, "GET", &id, &cookie(&admin)).status, 404);
    assert_eq!(env.ledger(), before);
    srv.stop();
}

fn list(srv: &Srv, h: &H) -> Resp {
    call(&srv.base, "GET", UPLOAD, h, None)
}

#[test]
fn a_lost_upload_response_can_be_recovered_from_the_owners_job_list() {
    let env = Env::new("lost");
    let b = backup();
    let srv = env.start(limits());
    let admin = claim(&env, &srv);
    let other_admin = browser(&srv, &admin, "Second admin", true);
    let member = browser(&srv, &admin, "Member browser", false);
    let before = env.ledger();
    let empty = list(&srv, &cookie(&admin));
    assert_eq!(
        (empty.status, &empty.body),
        (200, &json!({ "v": 1, "jobs": [] }))
    );

    // The upload completes, but the client never reads its 201.
    let mut s = connect(&srv);
    s.write_all(
        headers(
            &srv,
            "POST",
            UPLOAD,
            &cookie(&admin),
            &format!("Content-Length: {}\r\n", b.bytes.len()),
        )
        .as_bytes(),
    )
    .unwrap();
    s.write_all(&b.bytes).unwrap();
    eventually("the job is registered", || {
        srv.running().plane.restore.job_count() == 1
    });
    drop(s);

    let l = list(&srv, &cookie(&admin));
    assert_eq!(l.status, 200);
    let jobs = l.body["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    let j = &jobs[0];
    assert_eq!((&j["state"], &j["usable"]), (&json!("ready"), &json!(true)));
    assert_eq!(j["archive"]["sha256"], sha256_hex(&b.bytes).as_str());
    assert_eq!(j["libraries"].as_array().unwrap().len(), 2);
    assert!(
        j.get("confirmation").is_none(),
        "the secret is never recoverable"
    );
    let id = j["job_id"].as_str().unwrap().to_string();
    assert_eq!(job(&srv, "GET", &id, &cookie(&admin)).body, *j);

    // Nobody else sees it.
    assert_eq!(
        list(&srv, &cookie(&other_admin)).body,
        json!({ "v": 1, "jobs": [] })
    );
    assert_eq!(list(&srv, &cookie(&member)).status, 403);
    assert_eq!(list(&srv, &csrf()).status, 401);
    assert_eq!(
        list(
            &srv,
            &H {
                bearer: Some(admin.clone()),
                ..H::default()
            }
        )
        .status,
        403
    );
    let hooks = &srv.running().plane.restore.hooks;
    hooks.treat_peers_as_remote.store(true, Ordering::SeqCst);
    assert_eq!(list(&srv, &cookie(&admin)).status, 403);
    hooks.treat_peers_as_remote.store(false, Ordering::SeqCst);
    // The list is an ordinary control request: the 64 KiB limit applies.
    let big = upload_to(&srv, "GET", UPLOAD, &cookie(&admin), &vec![b'x'; 70 * 1024]);
    assert_eq!(big.status, 413);

    // Recovered: cancel it.
    assert_eq!(job(&srv, "DELETE", &id, &cookie(&admin)).status, 200);
    assert_eq!(
        list(&srv, &cookie(&admin)).body,
        json!({ "v": 1, "jobs": [] })
    );
    assert!(env.staging().is_empty());
    assert_eq!(env.ledger(), before);
    srv.stop();
}

#[test]
fn failed_cleanup_keeps_capacity_until_a_retry_succeeds() {
    let env = Env::new("cleanup");
    let b = backup();
    let srv = env.start(RestoreLimits {
        sweep_interval: Duration::from_millis(50),
        ..limits()
    });
    let admin = claim(&env, &srv);
    let other_admin = browser(&srv, &admin, "Second admin", true);
    let before = env.ledger();
    let restore = &srv.running().plane.restore;
    // Unrelated entries in the staging root that no cleanup may touch.
    let root = env.dir.join("ledger/restore-staging");
    std::fs::write(root.join("unrelated.txt"), b"keep me").unwrap();
    std::fs::create_dir_all(root.join("job-upload-untracked")).unwrap();
    std::fs::write(root.join("job-upload-untracked/archive.zip"), b"not ours").unwrap();
    let unrelated = env.staging();
    let failures = |what: &str| env.log().matches(what).count();

    // Cancelling while deletion fails: refused as cleanup_failed, the job stays (unusable) and
    // keeps its capacity; the sweeper keeps retrying.
    let r = upload(&srv, &cookie(&admin), &b.bytes);
    assert_eq!(r.status, 201);
    let a = r.body["job_id"].as_str().unwrap().to_string();
    restore.hooks.fail_cleanup.store(true, Ordering::SeqCst);
    let d = job(&srv, "DELETE", &a, &cookie(&admin));
    assert_eq!(
        (d.status, d.body["error"].as_str()),
        (500, Some("cleanup_failed"))
    );
    assert_eq!(restore.job_count(), 1);
    assert_eq!(
        env.staging().len(),
        unrelated.len() + 1,
        "the staged archive is still there"
    );
    let g = job(&srv, "GET", &a, &cookie(&admin));
    assert_eq!(
        (g.status, g.body["error"].as_str()),
        (410, Some("cancelled"))
    );
    let l = list(&srv, &cookie(&admin));
    assert_eq!(l.body["jobs"][0]["state"], "cleanup_pending");
    assert_eq!(l.body["jobs"][0]["usable"], false);
    let r = headers_only(&srv, "POST", UPLOAD, &cookie(&admin), 10);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (429, Some("too_many_jobs"))
    );
    let tag = format!("event=restore_cleanup_failed job_id={a}");
    let seen = failures(&tag);
    eventually("the sweeper retries", || failures(&tag) > seen);

    // A failed upload whose drop guard cannot delete its directory: remembered, counted
    // against the server-wide capacity (1 job + 1 orphan = 2), retried.
    let r = upload(&srv, &cookie(&other_admin), b"not a zip");
    assert_eq!(r.status, 422);
    assert_eq!(restore.pending_cleanup(), 1);
    assert_eq!(env.staging().len(), unrelated.len() + 2);
    let r = headers_only(&srv, "POST", UPLOAD, &cookie(&other_admin), 10);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (429, Some("too_many_jobs"))
    );
    eventually("the sweeper reports the orphan", || {
        env.log()
            .contains("event=restore_cleanup_failed orphaned_uploads=1")
    });

    // Deletion works again: the sweeper finishes both, capacity is released, and only the
    // job's and the orphan's own directories were removed.
    restore.hooks.fail_cleanup.store(false, Ordering::SeqCst);
    eventually("cleanup completes", || {
        restore.job_count() == 0 && restore.pending_cleanup() == 0
    });
    assert_eq!(env.staging(), unrelated);
    assert_eq!(
        std::fs::read(root.join("unrelated.txt")).unwrap(),
        b"keep me"
    );
    assert_eq!(
        std::fs::read(root.join("job-upload-untracked/archive.zip")).unwrap(),
        b"not ours"
    );
    assert_eq!(job(&srv, "GET", &a, &cookie(&admin)).status, 404);
    let r = upload(&srv, &cookie(&other_admin), &b.bytes);
    assert_eq!(r.status, 201);
    let c = r.body["job_id"].as_str().unwrap().to_string();

    // Expiry while deletion fails: retired (never usable again), retried, then removed.
    restore.hooks.fail_cleanup.store(true, Ordering::SeqCst);
    env.advance(15 * 60 * 1000);
    eventually("the job expires", || {
        env.log()
            .contains(&format!("event=restore_job_expired job_id={c}"))
    });
    assert_eq!(restore.job_count(), 1, "kept while its staging remains");
    let g = job(&srv, "GET", &c, &cookie(&other_admin));
    assert_eq!((g.status, g.body["error"].as_str()), (410, Some("expired")));
    let l = list(&srv, &cookie(&other_admin));
    assert_eq!(
        (&l.body["jobs"][0]["state"], &l.body["jobs"][0]["usable"]),
        (&json!("cleanup_pending"), &json!(false))
    );
    restore.hooks.fail_cleanup.store(false, Ordering::SeqCst);
    eventually("the expired job is removed", || restore.job_count() == 0);
    assert_eq!(env.staging(), unrelated);
    assert_eq!(env.ledger(), before);
    srv.stop();
}

#[test]
fn an_orphan_keeps_its_capacity_while_its_cleanup_is_in_progress() {
    use scribe_server::restore::CleanupGate;
    let env = Env::new("orphan-race");
    let b = backup();
    let srv = env.start(RestoreLimits {
        sweep_interval: Duration::from_millis(20),
        // An admitted upload that sends no body ends quickly (the old, racy outcome).
        idle_timeout: Duration::from_millis(500),
        ..limits()
    });
    let admin = claim(&env, &srv);
    let second = browser(&srv, &admin, "Second admin", true);
    let third = browser(&srv, &admin, "Third admin", true);
    let restore = &srv.running().plane.restore;
    let root = env.dir.join("ledger/restore-staging");
    std::fs::write(root.join("unrelated.txt"), b"keep me").unwrap();
    let before = env.ledger();
    let wait = Duration::from_secs(10);

    // One prepared job and one orphan: the two-job server-wide limit is full.
    assert_eq!(upload(&srv, &cookie(&admin), &b.bytes).status, 201);
    restore.hooks.fail_cleanup.store(true, Ordering::SeqCst);
    assert_eq!(upload(&srv, &cookie(&second), b"not a zip").status, 422);
    assert_eq!(restore.pending_cleanup(), 1);
    let (gate, started, resume) = CleanupGate::new();
    *restore.hooks.orphan_cleanup_gate.lock().unwrap() = Some(gate);

    // A sweep has begun cleaning the orphan and is paused before deleting it.
    started
        .recv_timeout(wait)
        .expect("the sweeper reached the orphan");
    let staged = env.staging();
    let r = headers_only(&srv, "POST", UPLOAD, &cookie(&third), 10);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (429, Some("too_many_jobs"))
    );
    assert_eq!(env.staging(), staged, "nothing new staged");
    // A second, concurrent sweep (the list route sweeps) must not take the same orphan.
    restore.hooks.fail_cleanup.store(false, Ordering::SeqCst);
    assert_eq!(list(&srv, &cookie(&third)).status, 200);
    assert_eq!(
        env.staging(),
        staged,
        "the orphan is left to the sweep cleaning it"
    );
    assert_eq!(restore.pending_cleanup(), 1);

    // Resume with deletion failing: still counted, still retryable.
    restore.hooks.fail_cleanup.store(true, Ordering::SeqCst);
    resume.send(()).unwrap();
    started
        .recv_timeout(wait)
        .expect("the next sweep retries the orphan");
    assert_eq!(restore.pending_cleanup(), 1);
    assert_eq!(env.staging(), staged);
    let r = headers_only(&srv, "POST", UPLOAD, &cookie(&third), 10);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (429, Some("too_many_jobs"))
    );

    // Resume with deletion succeeding: exactly that slot is released.
    restore.hooks.fail_cleanup.store(false, Ordering::SeqCst);
    *restore.hooks.orphan_cleanup_gate.lock().unwrap() = None;
    resume.send(()).unwrap();
    eventually("the orphan is deleted", || restore.pending_cleanup() == 0);
    assert_eq!(restore.job_count(), 1);
    assert_eq!(env.staging().len(), staged.len() - 1);
    assert_eq!(upload(&srv, &cookie(&third), &b.bytes).status, 201);
    let r = headers_only(&srv, "POST", UPLOAD, &cookie(&second), 10);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (429, Some("too_many_jobs"))
    );
    assert_eq!(
        std::fs::read(root.join("unrelated.txt")).unwrap(),
        b"keep me"
    );
    assert_eq!(env.ledger(), before);
    srv.stop();
}
