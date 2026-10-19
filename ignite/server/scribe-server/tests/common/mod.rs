//! Shared harness of the restore HTTP tests (disposable product servers, raw HTTP client,
//! Slice 1C exports, ledger snapshots).
#![allow(dead_code)]

pub use std::collections::BTreeMap;
pub use std::io::{Read, Write};
pub use std::net::{SocketAddr, TcpStream};
pub use std::path::PathBuf;
pub use std::sync::atomic::{AtomicU64, Ordering};
pub use std::sync::{Arc, Mutex};
pub use std::time::{Duration, Instant};

pub use backup_export::{export_backup, Credential, HttpSource, Secret};
pub use portable_backup::Limits;
pub use rusqlite::types::Value as Sql;
pub use scribe_server::config::{Config, DiscoverySection, ServerSection};
pub use scribe_server::netpolicy::LanPolicy;
pub use scribe_server::restore::RestoreLimits;
pub use scribe_server::{serve_with_restore_limits, Running};
pub use scribe_sync_server::{ServerConfig, ServerHandle, Store, TokenVerifier};
pub use serde_json::{json, Value};
pub use sync_engine::testkit;
pub use sync_protocol::{sha256_hex, LibraryId};

// ------------------------------------------------------------------ harness

pub static N: AtomicU64 = AtomicU64::new(0);

pub fn temp(label: &str) -> PathBuf {
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

pub struct Env {
    pub dir: PathBuf,
    pub clock: Arc<AtomicU64>,
    pub events: Arc<Mutex<Vec<String>>>,
}

impl Env {
    /// A product state directory whose ledger already holds an unrelated library and blob.
    pub fn new(label: &str) -> Self {
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

    pub fn config(&self) -> Config {
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

    pub fn start(&self, limits: RestoreLimits) -> Srv {
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

    pub fn advance(&self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }

    pub fn log(&self) -> String {
        self.events.lock().unwrap().join("\n")
    }

    pub fn staging(&self) -> Vec<String> {
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
    pub fn ledger(&self) -> LedgerState {
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
pub struct LedgerState {
    pub tables: BTreeMap<&'static str, Vec<Vec<Sql>>>,
    pub files: BTreeMap<String, Vec<u8>>,
}

pub struct Srv {
    pub addr: SocketAddr,
    pub base: String,
    pub running: Option<Running>,
}

impl Srv {
    pub fn running(&self) -> &Running {
        self.running.as_ref().unwrap()
    }

    pub fn stop(mut self) {
        if let Some(r) = self.running.take() {
            r.handle.shutdown();
        }
    }
}

pub fn limits() -> RestoreLimits {
    RestoreLimits::default()
}

/// Wait (bounded) until `f` holds.
pub fn eventually(what: &str, f: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ------------------------------------------------------------------ JSON control calls

#[derive(Default, Clone)]
pub struct H {
    pub cookie: Option<String>,
    pub bearer: Option<String>,
    pub csrf: bool,
    pub origin: Option<String>,
    pub content_type: Option<&'static str>,
    pub expect_continue: bool,
}

pub fn cookie(c: &str) -> H {
    H {
        cookie: Some(c.into()),
        csrf: true,
        content_type: Some("application/zip"),
        ..H::default()
    }
}

pub struct Resp {
    pub status: u16,
    pub body: Value,
    pub headers: String,
    pub set_cookie: Option<String>,
}

pub fn call(base: &str, method: &str, path: &str, h: &H, body: Option<Value>) -> Resp {
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

pub fn csrf() -> H {
    H {
        csrf: true,
        ..H::default()
    }
}

pub fn cookie_of(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches("scribe_session=")
        .to_string()
}

pub fn claim(env: &Env, srv: &Srv) -> String {
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
pub fn browser(srv: &Srv, admin: &str, name: &str, as_admin: bool) -> String {
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

pub fn kindle(srv: &Srv, admin: &str) -> String {
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

pub fn device_id(srv: &Srv, c: &str) -> String {
    let s = call(&srv.base, "GET", "/control/v1/status", &cookie(c), None);
    s.body["session"]["device_id"].as_str().unwrap().to_string()
}

// ------------------------------------------------------------------ raw HTTP

pub const UPLOAD: &str = "/control/v1/admin/restore/jobs";

pub fn headers(srv: &Srv, method: &str, path: &str, h: &H, framing: &str) -> String {
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

pub fn connect(srv: &Srv) -> TcpStream {
    let s = TcpStream::connect(srv.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s
}

/// Read one response (the server closes the connection after it).
pub fn read_response(mut s: TcpStream) -> Resp {
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
pub fn headers_only(srv: &Srv, method: &str, path: &str, h: &H, declared: u64) -> Resp {
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

pub fn upload(srv: &Srv, h: &H, body: &[u8]) -> Resp {
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

pub fn upload_chunked(srv: &Srv, h: &H, body: &[u8], chunk: usize) -> Resp {
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

pub fn job(srv: &Srv, method: &str, id: &str, h: &H) -> Resp {
    call(&srv.base, method, &format!("{UPLOAD}/{id}"), h, None)
}

// ------------------------------------------------------------------ backups (Slice 1C)

pub struct Backup {
    pub dir: PathBuf,
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

impl Drop for Backup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A second background (same format as testkit's, different bytes): only the second library
/// uses it, so restoring into a ledger that already holds testkit's blob installs exactly one.
pub fn second_blob() -> Vec<u8> {
    let (mut bytes, _) = testkit::blob_background();
    bytes.reverse();
    bytes
}

/// testkit's genesis under another id, with every blob background switched to `second_blob`.
pub fn second_library() -> sync_protocol::SyncSnapshotV1 {
    let mut g = testkit::genesis();
    g.library_id = LibraryId::from_uuid(testkit::uuid7(1, 2)).unwrap();
    let blob = second_blob();
    for nb in &mut g.notebooks {
        for page in &mut nb.pages {
            if let sync_protocol::BackgroundRef::Blob { sha256, .. } = &mut page.background {
                *sha256 = sha256_hex(&blob);
            }
        }
    }
    g
}

/// A real export of a source server holding two libraries with one background blob each
/// (testkit's, which the destination ledger already holds, and `second_blob`).
pub fn backup() -> Backup {
    let dir = temp("backup");
    let data = dir.join("source");
    let store = Store::open(&data).unwrap();
    let (bytes, _) = testkit::blob_background();
    store.put_blob(sha256_hex(&bytes).as_str(), &bytes).unwrap();
    let blob = second_blob();
    store.put_blob(sha256_hex(&blob).as_str(), &blob).unwrap();
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
    pub fn entries(&self) -> Vec<(String, Vec<u8>)> {
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
    pub fn variant(&self, edit: impl FnOnce(&mut Vec<(String, Vec<u8>)>)) -> Vec<u8> {
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

pub fn edit_entry(entries: &mut [(String, Vec<u8>)], prefix: &str, f: impl FnOnce(&mut Vec<u8>)) {
    let e = entries
        .iter_mut()
        .find(|(n, _)| n.starts_with(prefix))
        .unwrap_or_else(|| panic!("no entry {prefix}"));
    f(&mut e.1);
}

pub fn upload_to(srv: &Srv, method: &str, path: &str, h: &H, body: &[u8]) -> Resp {
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

pub fn list(srv: &Srv, h: &H) -> Resp {
    call(&srv.base, "GET", UPLOAD, h, None)
}
