//! Disposable loopback sync server with realistic libraries, a request-recording TCP proxy,
//! and an in-memory source seeded from the real server's responses.
//!
//! ```text
//! library A (testkit::library())  folders, 3 notebooks / 4 pages, erased fragments, pressure,
//!                                 text, valid + dangling links, blob A; +2 operations (head 2)
//! library B (uuid7(1,2))          folder, 1 notebook / 2 pages, blob A (shared) + blob B, text
//! library C (uuid7(1,3))          1 notebook / 1 blank page, no blobs
//! ```
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use backup_export::{ReadOnlySource, SourceError, SourceResponse};
use scribe_sync_server::{ServerConfig, ServerHandle, Store, TokenVerifier};
use serde_json::value::RawValue;
use sync_engine::testkit::{
    blob_background, fd, genesis, line, nb, page_link, pg, render, replica, sk, text_obj, uuid7,
    Author,
};
use sync_protocol::{
    sha256_hex, BackgroundEncoding, BackgroundFormat, BackgroundRef, LibraryId, Sha256Hex,
    SnapshotFolder, SnapshotNotebook, SnapshotPage, SyncOperationV1, SyncSnapshotV1,
};

pub const TOKEN: &str = "exporter-test-token-0123456789abcdef";

pub fn lib_a() -> LibraryId {
    sync_engine::testkit::library()
}
pub fn lib_b() -> LibraryId {
    LibraryId::from_uuid(uuid7(1, 2)).unwrap()
}
pub fn lib_c() -> LibraryId {
    LibraryId::from_uuid(uuid7(1, 3)).unwrap()
}

pub fn blob_b() -> (Vec<u8>, BackgroundRef) {
    let (w, h) = (40u32, 30u32);
    let bytes: Vec<u8> = (0..w * h)
        .map(|i| (i.wrapping_mul(7) % 256) as u8)
        .collect();
    let r = BackgroundRef::Blob {
        sha256: sha256_hex(&bytes),
        byte_len: bytes.len() as u64,
        format: BackgroundFormat {
            encoding: BackgroundEncoding::Gray8,
            width: w,
            height: h,
            stride: w,
        },
    };
    (bytes, r)
}

pub fn sha_of(r: &BackgroundRef) -> Sha256Hex {
    match r {
        BackgroundRef::Blob { sha256, .. } => sha256.clone(),
        BackgroundRef::Blank => unreachable!(),
    }
}

fn library_a() -> SyncSnapshotV1 {
    let mut s = genesis();
    s.folders = vec![
        SnapshotFolder {
            folder_id: fd(1),
            title: "Projects".into(),
        },
        SnapshotFolder {
            folder_id: fd(2),
            title: "Archive".into(),
        },
    ];
    s.notebooks[1].folder_id = Some(fd(1));
    s.notebooks[2].folder_id = Some(fd(2));
    let p1 = &mut s.notebooks[0].pages[0];
    p1.texts.push(text_obj(1, "Agenda\nitem one"));
    p1.links.push(page_link(1, nb(1), pg(1), nb(2), pg(3)));
    s.notebooks[0].pages[1]
        .links
        .push(page_link(3, nb(1), pg(2), nb(9), pg(99)));
    let p3 = &mut s.notebooks[1].pages[0];
    p3.texts.push(text_obj(2, "Work page text"));
    p3.links.push(page_link(2, nb(2), pg(3), nb(1), pg(2)));
    s
}

fn library_b() -> SyncSnapshotV1 {
    let (_, bg_a) = blob_background();
    let (_, bg_b) = blob_b();
    SyncSnapshotV1 {
        v: 1,
        library_id: lib_b(),
        page_space: sync_protocol::PageSpace::CANONICAL,
        notebooks: vec![SnapshotNotebook {
            notebook_id: nb(10),
            title: "Meeting notes".into(),
            pages: vec![
                SnapshotPage {
                    page_id: pg(10),
                    background: bg_a,
                    render: render(true),
                    strokes: vec![line(
                        sk(10),
                        sk(10),
                        (900.0, 900.0),
                        (9000.0, 12000.0),
                        25,
                        true,
                    )],
                    texts: vec![],
                    links: vec![],
                },
                SnapshotPage {
                    page_id: pg(11),
                    background: bg_b,
                    render: render(false),
                    strokes: vec![
                        line(sk(12), sk(11), (100.0, 100.0), (900.0, 100.0), 5, false),
                        line(sk(13), sk(11), (1500.0, 100.0), (2500.0, 100.0), 5, false),
                    ],
                    texts: vec![text_obj(10, "Follow up")],
                    links: vec![],
                },
            ],
            folder_id: Some(fd(10)),
        }],
        folders: vec![SnapshotFolder {
            folder_id: fd(10),
            title: "Inbox".into(),
        }],
    }
}

fn library_c() -> SyncSnapshotV1 {
    SyncSnapshotV1 {
        v: 1,
        library_id: lib_c(),
        page_space: sync_protocol::PageSpace::CANONICAL,
        notebooks: vec![SnapshotNotebook {
            notebook_id: nb(20),
            title: "Empty".into(),
            pages: vec![SnapshotPage {
                page_id: pg(20),
                background: BackgroundRef::Blank,
                render: render(true),
                strokes: vec![],
                texts: vec![],
                links: vec![],
            }],
            folder_id: None,
        }],
        folders: vec![],
    }
}

/// Library A author (replica 1); the server holds its client_seq 1..=2 after `start`.
pub fn author_a() -> Author {
    Author::new(replica(1))
}

pub fn add_stroke_env(author: &mut Author, n: u64) -> Vec<u8> {
    author
        .env(SyncOperationV1::AddStroke {
            notebook_id: nb(1),
            page_id: pg(2),
            stroke: line(
                sk(100 + n),
                sk(100 + n),
                (2000.0, 2000.0 + n as f32 * 100.0),
                (6000.0, 2500.0),
                6,
                true,
            ),
        })
        .to_canonical_bytes()
        .unwrap()
}

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "backup-export-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// File names currently in the directory (staging leftovers would show up here).
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct Server {
    pub handle: ServerHandle,
    _dir: TempDir,
    pub blobs: BTreeMap<Sha256Hex, Vec<u8>>,
    pub author_a: Author,
}

impl Server {
    /// Authenticated loopback server (Bearer `TOKEN`) with libraries A (head 2), B and C.
    pub fn start() -> Self {
        let dir = TempDir::new("server");
        let store = Store::open(&dir.0).unwrap();
        let mut blobs = BTreeMap::new();
        for (bytes, r) in [blob_background(), blob_b()] {
            store.put_blob(sha_of(&r).as_str(), &bytes).unwrap();
            blobs.insert(sha_of(&r), bytes);
        }
        for s in [library_a(), library_b(), library_c()] {
            assert!(store.bootstrap(&s.to_canonical_bytes().unwrap()).unwrap().1);
        }
        let mut author = author_a();
        let envs = vec![
            add_stroke_env(&mut author, 1),
            add_stroke_env(&mut author, 2),
        ];
        store.submit(&lib_a().to_string(), &envs).unwrap();
        let handle = ServerHandle::start_with(
            "127.0.0.1:0".parse().unwrap(),
            store,
            ServerConfig {
                auth: Some(TokenVerifier::for_token(TOKEN)),
                ..ServerConfig::default()
            },
        )
        .unwrap();
        Self {
            handle,
            _dir: dir,
            blobs,
            author_a: author,
        }
    }

    pub fn base_url(&self) -> String {
        self.handle.base_url()
    }

    /// Test-side authenticated request (never through the exporter).
    pub fn call(&self, method: &str, path: &str, body: Option<&[u8]>) -> (u16, Vec<u8>) {
        let req = ureq::request(method, &format!("{}{path}", self.base_url()))
            .set("Authorization", &format!("Bearer {TOKEN}"));
        let r = match body {
            Some(b) => req.set("Content-Type", "application/json").send_bytes(b),
            None => req.call(),
        };
        let resp = match r {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{e}"),
        };
        let status = resp.status();
        let mut out = Vec::new();
        resp.into_reader().read_to_end(&mut out).unwrap();
        (status, out)
    }

    /// Current stored snapshot bytes and sha of `lib` (test-side read).
    pub fn snapshot(&self, lib: LibraryId) -> (u64, Vec<u8>, String) {
        let (s, body) = self.call("GET", &format!("/v1/libraries/{lib}/snapshot"), None);
        assert_eq!(s, 200);
        let m = fields(&body);
        (
            m["through_server_seq"].get().parse().unwrap(),
            m["snapshot"].get().as_bytes().to_vec(),
            serde_json::from_str(m["snapshot_sha256"].get()).unwrap(),
        )
    }

    /// Append one more library-A stroke through the API (a concurrent writer).
    pub fn write_to_a(&mut self) {
        let n = self.author_a.next_seq;
        let env = add_stroke_env(&mut self.author_a, n);
        let mut body = b"{\"envelopes\":[".to_vec();
        body.extend_from_slice(&env);
        body.extend_from_slice(b"]}");
        let (s, b) = self.call(
            "POST",
            &format!("/v1/libraries/{}/operations", lib_a()),
            Some(&body),
        );
        assert_eq!(s, 200, "{}", String::from_utf8_lossy(&b));
    }

    /// Every GET response the exporter needs, captured verbatim from the real server.
    pub fn capture(&self) -> BTreeMap<String, (u16, Vec<u8>)> {
        let mut m = BTreeMap::new();
        m.insert(
            "/v1/libraries".into(),
            self.call("GET", "/v1/libraries", None),
        );
        for lib in [lib_a(), lib_b(), lib_c()] {
            let p = format!("/v1/libraries/{lib}/snapshot");
            m.insert(p.clone(), self.call("GET", &p, None));
        }
        for sha in self.blobs.keys() {
            let p = format!("/v1/blobs/{sha}");
            m.insert(p.clone(), self.call("GET", &p, None));
        }
        m
    }
}

// --------------------------------------------------------------------------- recording proxy

/// TCP proxy in front of the real server. It records the head (request line + headers) of
/// every HTTP request the exporter sends. Exporter requests have no bodies, so each head ends
/// at `\r\n\r\n` and the next byte starts a new request line; a request with a body would
/// already have been recorded with its (non-GET) method.
pub struct Proxy {
    pub addr: SocketAddr,
    pub heads: Arc<Mutex<Vec<String>>>,
}

impl Proxy {
    pub fn start(upstream: &str) -> Self {
        let upstream: SocketAddr = upstream.trim_start_matches("http://").parse().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let heads = Arc::new(Mutex::new(Vec::new()));
        let h = Arc::clone(&heads);
        thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { return };
                let Ok(server) = TcpStream::connect(upstream) else {
                    return;
                };
                let (mut c_read, mut s_write) =
                    (client.try_clone().unwrap(), server.try_clone().unwrap());
                let (mut s_read, mut c_write) = (server, client);
                let h = Arc::clone(&h);
                thread::spawn(move || {
                    let mut pending: Vec<u8> = Vec::new();
                    let mut buf = [0u8; 8192];
                    loop {
                        let n = match c_read.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        pending.extend_from_slice(&buf[..n]);
                        while let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&pending[..end]).into_owned();
                            h.lock().unwrap().push(head);
                            pending.drain(..end + 4);
                        }
                        if s_write.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    let _ = s_write.shutdown(std::net::Shutdown::Write);
                });
                thread::spawn(move || {
                    let _ = std::io::copy(&mut s_read, &mut c_write);
                    let _ = c_write.shutdown(std::net::Shutdown::Write);
                });
            }
        });
        Self { addr, heads }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn request_lines(&self) -> Vec<String> {
        self.heads
            .lock()
            .unwrap()
            .iter()
            .map(|h| h.lines().next().unwrap_or("").to_string())
            .collect()
    }
}

// --------------------------------------------------------------------------- in-memory source

/// Body that yields `ok` bytes, then fails (an interrupted transfer).
pub struct Interrupted {
    pub data: Vec<u8>,
    pub pos: usize,
    pub ok: usize,
}

impl Read for Interrupted {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.ok {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset",
            ));
        }
        let n = out
            .len()
            .min(self.ok - self.pos)
            .min(self.data.len() - self.pos);
        out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[derive(Default)]
pub struct FakeSource {
    pub routes: BTreeMap<String, (u16, Vec<u8>)>,
    /// Path → number of body bytes delivered before the transfer fails.
    pub interrupt: BTreeMap<String, usize>,
    /// Paths whose GET fails at the transport level.
    pub unreachable: Vec<String>,
    pub log: Mutex<Vec<String>>,
}

impl FakeSource {
    pub fn from(routes: BTreeMap<String, (u16, Vec<u8>)>) -> Self {
        Self {
            routes,
            ..Self::default()
        }
    }

    pub fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// Edit one snapshot response's top-level fields; every value (the snapshot included)
    /// stays verbatim raw JSON.
    pub fn edit_snapshot(
        &mut self,
        lib: LibraryId,
        f: impl FnOnce(&mut BTreeMap<String, Box<RawValue>>),
    ) {
        let key = format!("/v1/libraries/{lib}/snapshot");
        let (_, body) = self.routes.get_mut(&key).unwrap();
        let mut m = fields(body);
        f(&mut m);
        *body = serde_json::to_vec(&m).unwrap();
    }

    /// Replace one library's snapshot bytes with `raw` (spliced verbatim) and its reported
    /// sha256 with the true hash of `raw`, so only the snapshot content itself is wrong.
    pub fn set_raw_snapshot(&mut self, lib: LibraryId, through: &str, raw: &[u8]) {
        let key = format!("/v1/libraries/{lib}/snapshot");
        let mut body = format!(
            "{{\"library_id\":\"{lib}\",\"through_server_seq\":{through},\"snapshot_sha256\":\"{}\",\"snapshot\":",
            sha256_hex(raw)
        )
        .into_bytes();
        body.extend_from_slice(raw);
        body.push(b'}');
        self.routes.insert(key, (200, body));
    }

    pub fn raw_snapshot(&self, lib: LibraryId) -> Vec<u8> {
        let (_, body) = &self.routes[&format!("/v1/libraries/{lib}/snapshot")];
        fields(body)["snapshot"].get().as_bytes().to_vec()
    }
}

impl ReadOnlySource for FakeSource {
    fn get(&self, path: &str) -> Result<SourceResponse<'_>, SourceError> {
        self.log.lock().unwrap().push(path.to_string());
        if self.unreachable.iter().any(|p| p == path) {
            return Err(SourceError(format!("GET {path}: connection refused")));
        }
        let Some((status, body)) = self.routes.get(path) else {
            return Ok(SourceResponse {
                status: 404,
                body: Box::new(&b"{}"[..]),
            });
        };
        if let Some(&ok) = self.interrupt.get(path) {
            return Ok(SourceResponse {
                status: *status,
                body: Box::new(Interrupted {
                    data: body.clone(),
                    pos: 0,
                    ok,
                }),
            });
        }
        Ok(SourceResponse {
            status: *status,
            body: Box::new(&body[..]),
        })
    }
}

/// Wraps a source and runs `hook` once, right after the request whose path equals `after`.
pub struct Hooked<'a, S: ReadOnlySource> {
    pub inner: S,
    pub after: String,
    pub hook: Mutex<Option<Box<dyn FnOnce() + 'a>>>,
}

impl<S: ReadOnlySource> ReadOnlySource for Hooked<'_, S> {
    fn get(&self, path: &str) -> Result<SourceResponse<'_>, SourceError> {
        let r = self.inner.get(path);
        if path == self.after {
            if let Some(h) = self.hook.lock().unwrap().take() {
                h();
            }
        }
        r
    }
}

pub fn assert_no_leftovers(dir: &TempDir, expected: &[&str]) {
    let names = dir.names();
    assert_eq!(
        names,
        expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "leftovers in {}",
        dir.0.display()
    );
}

pub fn read_zip(path: &Path) -> Vec<(String, Vec<u8>, zip::CompressionMethod)> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    (0..z.len())
        .map(|i| {
            let mut e = z.by_index(i).unwrap();
            let mut b = Vec::new();
            e.read_to_end(&mut b).unwrap();
            (e.name().to_string(), b, e.compression())
        })
        .collect()
}

/// Top-level fields of a JSON object, each kept as verbatim raw JSON (no key reordering).
pub fn fields(body: &[u8]) -> BTreeMap<String, Box<RawValue>> {
    serde_json::from_slice(body).unwrap()
}

pub fn raw(json: &str) -> Box<RawValue> {
    RawValue::from_string(json.to_string()).unwrap()
}
