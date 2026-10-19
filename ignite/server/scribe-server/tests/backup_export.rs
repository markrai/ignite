//! Slice 1G-D1: administrator backup export and streamed download, end to end against a
//! disposable product server (loopback, ephemeral port, injectable clock). Downloads use a raw
//! HTTP/1.1 client so the tests see exactly the bytes and headers the server sends.

mod common;

use common::*;
use scribe_server::export::{ExportLimits, ARCHIVE_FILE};
use scribe_server::restore::CleanupGate;
use scribe_server::serve_with_limits;

const EXPORTS: &str = "/control/v1/admin/backup/exports";

fn start(env: &Env, export: ExportLimits) -> Srv {
    let c = env.clock.clone();
    let e = env.events.clone();
    let running = serve_with_limits(
        &env.config(),
        Arc::new(move || c.load(Ordering::SeqCst)),
        Arc::new(move |line: &str| e.lock().unwrap().push(line.to_string())),
        Arc::new(LanPolicy::for_this_host()),
        limits(),
        export,
    )
    .unwrap();
    Srv {
        addr: running.handle.addr(),
        base: running.handle.base_url(),
        running: Some(running),
    }
}

/// Small chunks, so every download of the fixture spans many of them.
fn small_chunks() -> ExportLimits {
    ExportLimits {
        download_chunk: 256,
        ..ExportLimits::default()
    }
}

/// The fixture ledger plus a library whose background is a large, incompressible blob: its
/// archive (about 8 MiB) is larger than loopback socket buffers, so a client that stops
/// reading really holds a download open.
fn large_archive(label: &str) -> Env {
    let env = Env::new(label);
    let store = Store::open(&env.dir.join("ledger")).unwrap();
    let (w, h) = (2048u32, 4096u32);
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let bytes: Vec<u8> = (0..w * h)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let sha = sha256_hex(&bytes);
    store.put_blob(sha.as_str(), &bytes).unwrap();
    let mut g = testkit::genesis();
    g.library_id = LibraryId::from_uuid(testkit::uuid7(1, 8)).unwrap();
    for nb in &mut g.notebooks {
        for page in &mut nb.pages {
            if let sync_protocol::BackgroundRef::Blob {
                sha256,
                byte_len,
                format,
            } = &mut page.background
            {
                *sha256 = sha.clone();
                *byte_len = bytes.len() as u64;
                (format.width, format.height, format.stride) = (w, h, w);
            }
        }
    }
    store.bootstrap(&g.to_canonical_bytes().unwrap()).unwrap();
    env
}

/// The fixture ledger plus a second library that shares testkit's background blob.
fn two_libraries(label: &str) -> Env {
    let env = Env::new(label);
    let store = Store::open(&env.dir.join("ledger")).unwrap();
    let mut g = testkit::genesis();
    g.library_id = LibraryId::from_uuid(testkit::uuid7(1, 7)).unwrap();
    store.bootstrap(&g.to_canonical_bytes().unwrap()).unwrap();
    env
}

fn export_dirs(env: &Env) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(env.dir.join("ledger").join("export-staging"))
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn start_export(srv: &Srv, h: &H) -> Resp {
    call(&srv.base, "POST", EXPORTS, h, None)
}

fn status(srv: &Srv, id: &str, h: &H) -> Resp {
    call(&srv.base, "GET", &format!("{EXPORTS}/{id}"), h, None)
}

fn dismiss(srv: &Srv, id: &str, h: &H) -> Resp {
    call(&srv.base, "DELETE", &format!("{EXPORTS}/{id}"), h, None)
}

fn download_path(id: &str) -> String {
    format!("{EXPORTS}/{id}/download")
}

/// Start an export and wait until it is no longer preparing; returns its final summary.
fn prepared(srv: &Srv, h: &H) -> Value {
    let r = start_export(srv, h);
    assert_eq!(r.status, 202, "{}", r.body);
    assert_eq!(r.body["state"], "preparing");
    let id = r.body["export_id"].as_str().unwrap().to_string();
    let mut last = Value::Null;
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        last = status(srv, &id, h).body;
        if last["state"] != "preparing" {
            return last;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("export never finished: {last}");
}

/// A raw response with its body as bytes.
struct Raw {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

impl Raw {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim() == name).then_some(v.trim())
        })
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

fn split(buf: &[u8]) -> Raw {
    let at = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a complete response head");
    let head = String::from_utf8_lossy(&buf[..at]).to_ascii_lowercase();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    Raw {
        status,
        headers: head,
        body: buf[at + 4..].to_vec(),
    }
}

fn send(srv: &Srv, method: &str, path: &str, h: &H, extra: &str) -> TcpStream {
    let mut s = connect(srv);
    s.write_all(headers(srv, method, path, h, extra).as_bytes())
        .unwrap();
    s
}

fn get_raw(srv: &Srv, method: &str, path: &str, h: &H, extra: &str) -> Raw {
    let mut s = send(srv, method, path, h, extra);
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    split(&buf)
}

fn download(srv: &Srv, id: &str, h: &H) -> Raw {
    get_raw(srv, "GET", &download_path(id), h, "")
}

fn admin_h(c: &str) -> H {
    H {
        content_type: None,
        ..cookie(c)
    }
}

/// The repository's exporter, run against the same server with a Kindle credential.
fn cli_export(srv: &Srv, bearer: &str, dir: &std::path::Path) -> Vec<u8> {
    let path = dir.join("cli.zip");
    let src = HttpSource::new(
        &srv.base,
        Some(Credential::Bearer(Secret::new(bearer.to_string()))),
    )
    .unwrap();
    export_backup(&src, &path, &Limits::DEFAULT).unwrap();
    std::fs::read(&path).unwrap()
}

// ------------------------------------------------------------------ the archive itself

#[test]
fn an_export_downloads_a_verified_restorable_archive_identical_to_the_cli_export() {
    let env = two_libraries("roundtrip");
    let before = env.ledger();
    let srv = start(&env, small_chunks());
    let admin = claim(&env, &srv);
    let kindle_c = kindle(&srv, &admin);
    let h = admin_h(&admin);

    let ready = prepared(&srv, &h);
    assert_eq!(ready["state"], "ready", "{ready}");
    assert_eq!(ready["consistency"], "per_library_snapshot");
    assert_eq!(ready["error"], Value::Null);
    let id = ready["export_id"].as_str().unwrap();
    assert_eq!(ready["download"], download_path(id).as_str());
    assert_eq!(ready["archive"]["file_name"], ARCHIVE_FILE);
    assert_eq!(ready["totals"]["libraries"], 2);
    assert_eq!(
        ready["totals"]["blobs"], 1,
        "the shared blob is stored once"
    );
    assert!(ready["expires_in_s"].as_u64().unwrap() > 0);

    let r = download(&srv, id, &h);
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let bytes = r.body.clone();
    assert!(bytes.len() > 4 * 256, "spans several 256-byte chunks");
    assert_eq!(r.header("content-type"), Some("application/zip"));
    assert_eq!(
        r.header("content-disposition"),
        Some("attachment; filename=\"ignite-backup.zip\"")
    );
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(r.header("accept-ranges"), Some("none"));
    assert_eq!(
        r.header("content-length"),
        Some(bytes.len().to_string().as_str())
    );
    assert!(r.header("transfer-encoding").is_none());
    assert_eq!(ready["archive"]["bytes"], bytes.len() as u64);
    assert_eq!(
        ready["archive"]["sha256"],
        sha256_hex(&bytes).as_str(),
        "the advertised hash is the downloaded bytes' hash"
    );

    // The strict reader accepts it, with the manifest the server describes.
    let dir = temp("roundtrip-out");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("downloaded.zip");
    std::fs::write(&file, &bytes).unwrap();
    let plan =
        backup_archive::inspect_backup(&file, &scribe_server::restore::verification_limits())
            .unwrap();
    assert_eq!(plan.libraries.len(), 2);
    assert_eq!(plan.blob_count, 1);
    assert_eq!(ready["archive"]["format"], plan.format);
    assert_eq!(ready["archive"]["format_version"], plan.format_version);
    assert_eq!(ready["totals"]["notebooks"], plan.total_notebooks);
    assert_eq!(ready["totals"]["pages"], plan.total_pages);
    assert_eq!(ready["totals"]["blob_bytes"], plan.total_blob_bytes);
    let mut ids: Vec<String> = plan
        .libraries
        .iter()
        .map(|l| l.source_library_id.to_string())
        .collect();
    ids.sort();
    let mut expected = vec![
        testkit::genesis().library_id.to_string(),
        LibraryId::from_uuid(testkit::uuid7(1, 7))
            .unwrap()
            .to_string(),
    ];
    expected.sort();
    assert_eq!(ids, expected);

    // Same format, same bytes as the CLI exporter reading over HTTP.
    assert_eq!(
        cli_export(&srv, &kindle_c, &dir),
        bytes,
        "byte-identical to the CLI export"
    );

    // Restorable into a disposable ledger.
    let target = Store::open(&dir.join("target")).unwrap();
    let mapping: BTreeMap<LibraryId, LibraryId> = plan
        .libraries
        .iter()
        .enumerate()
        .map(|(i, l)| {
            (
                l.source_library_id,
                LibraryId::from_uuid(testkit::uuid7(9, i as u64 + 1)).unwrap(),
            )
        })
        .collect();
    let report = backup_restore::restore_backup(
        &target,
        &file,
        &mapping,
        &backup_restore::RestoreOptions::default(),
    )
    .unwrap();
    assert_eq!(report.libraries.len(), 2);
    assert_eq!(report.blobs_installed, 1);
    drop(target);

    // A second download is the same file; the ledger was never written.
    assert_eq!(download(&srv, id, &h).body, bytes);
    assert_eq!(env.ledger(), before);
    assert!(env.staging().is_empty(), "restore staging untouched");
    assert!(env.log().contains("event=backup_export_ready"));
    srv.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ who may export

#[test]
fn only_the_owning_loopback_admin_browser_may_export_or_download() {
    let env = Env::new("auth");
    let before = env.ledger();
    let srv = start(&env, ExportLimits::default());
    let admin = claim(&env, &srv);
    let other = browser(&srv, &admin, "Other admin", true);
    let member = browser(&srv, &admin, "Member", false);
    let kindle_c = kindle(&srv, &admin);
    let h = admin_h(&admin);

    let bearer = |b: &str| H {
        bearer: Some(b.to_string()),
        csrf: true,
        ..H::default()
    };
    let refused = [
        ("anonymous", csrf(), 401),
        ("member", admin_h(&member), 403),
        ("Kindle", bearer(&kindle_c), 403),
        ("admin as Bearer", bearer(&admin), 403),
        (
            "admin cookie plus Authorization",
            H {
                bearer: Some(kindle_c.clone()),
                ..admin_h(&admin)
            },
            403,
        ),
        (
            "missing CSRF",
            H {
                csrf: false,
                ..admin_h(&admin)
            },
            403,
        ),
        (
            "cross-origin",
            H {
                origin: Some("http://evil.example".into()),
                ..admin_h(&admin)
            },
            403,
        ),
    ];
    for (what, hh, code) in &refused {
        let r = get_raw(&srv, "POST", EXPORTS, hh, "Content-Length: 0\r\n");
        assert_eq!(r.status, *code, "POST {what}: {}", r.json());
    }
    assert!(export_dirs(&env).is_empty(), "nothing started");

    // Non-loopback (simulated), with and without forged forwarding headers.
    let hooks = &srv.running().plane.restore.hooks;
    hooks.treat_peers_as_remote.store(true, Ordering::SeqCst);
    for extra in [
        "",
        "X-Forwarded-For: 127.0.0.1\r\nForwarded: for=127.0.0.1\r\nX-Real-IP: 127.0.0.1\r\n",
    ] {
        let r = get_raw(
            &srv,
            "POST",
            EXPORTS,
            &h,
            &format!("{extra}Content-Length: 0\r\n"),
        );
        assert_eq!(
            (r.status, r.json()["error"].as_str()),
            (403, Some("loopback_required"))
        );
    }
    hooks.treat_peers_as_remote.store(false, Ordering::SeqCst);

    let ready = prepared(&srv, &h);
    let id = ready["export_id"].as_str().unwrap().to_string();
    assert_eq!(ready["state"], "ready");

    // Another administrator's export is "not found", whatever they try; their list is empty.
    let unknown = status(&srv, "01900000-0000-7000-8000-000000000000", &h);
    assert_eq!(unknown.status, 404);
    let oh = admin_h(&other);
    assert_eq!(status(&srv, &id, &oh).body, unknown.body);
    assert_eq!(status(&srv, &id, &oh).status, 404);
    assert_eq!(dismiss(&srv, &id, &oh).status, 404);
    let d = download(&srv, &id, &oh);
    assert_eq!((d.status, d.json()), (404, unknown.body.clone()));
    let l = call(&srv.base, "GET", EXPORTS, &oh, None);
    assert_eq!(l.body["exports"], json!([]));
    let mine = call(&srv.base, "GET", EXPORTS, &h, None);
    assert_eq!(mine.body["exports"][0]["export_id"], id.as_str());

    // Downloads are authorized again, every time: no bytes of the archive for anyone else.
    for (what, hh, code) in [
        ("anonymous", H::default(), 401),
        ("member", admin_h(&member), 403),
        ("Kindle", bearer(&kindle_c), 403),
        ("admin as Bearer", bearer(&admin), 403),
    ] {
        let r = download(&srv, &id, &hh);
        assert_eq!(r.status, code, "download {what}");
        assert_eq!(r.header("content-type"), Some("application/json"), "{what}");
    }
    hooks.treat_peers_as_remote.store(true, Ordering::SeqCst);
    assert_eq!(download(&srv, &id, &h).status, 403);
    assert_eq!(status(&srv, &id, &h).status, 403);
    hooks.treat_peers_as_remote.store(false, Ordering::SeqCst);
    // Mutations of an existing export need the CSRF marker too.
    let no_csrf = H {
        csrf: false,
        ..admin_h(&admin)
    };
    assert_eq!(dismiss(&srv, &id, &no_csrf).status, 403);
    assert_eq!(download(&srv, &id, &h).status, 200);

    // A revoked administrator is refused everywhere.
    let me = device_id(&srv, &admin);
    srv.running().plane.store.revoke(&me).unwrap();
    assert_eq!(download(&srv, &id, &h).status, 401);
    assert_eq!(status(&srv, &id, &h).status, 401);
    assert_eq!(start_export(&srv, &h).status, 401);
    assert_eq!(env.ledger(), before);
    srv.stop();
}

#[test]
fn unsupported_methods_and_ranges_never_send_a_partial_archive() {
    let env = Env::new("methods");
    let srv = start(&env, small_chunks());
    let admin = claim(&env, &srv);
    let h = admin_h(&admin);
    let ready = prepared(&srv, &h);
    let id = ready["export_id"].as_str().unwrap();
    let path = download_path(id);
    let full = download(&srv, id, &h).body;

    for method in ["HEAD", "POST", "PUT", "DELETE"] {
        let r = get_raw(&srv, method, &path, &h, "Content-Length: 0\r\n");
        assert_eq!(r.status, 405, "{method}");
        assert!(r.header("content-disposition").is_none(), "{method}");
        assert_ne!(r.header("content-type"), Some("application/zip"));
    }
    let r = get_raw(&srv, "PUT", EXPORTS, &h, "Content-Length: 0\r\n");
    assert_eq!(r.status, 404);
    // A Range request gets the whole verified file (never 206 or a slice).
    let r = get_raw(&srv, "GET", &path, &h, "Range: bytes=0-99\r\n");
    assert_eq!(r.status, 200);
    assert_eq!(r.header("accept-ranges"), Some("none"));
    assert!(r.header("content-range").is_none());
    assert_eq!(r.body, full);
    srv.stop();
}

// ------------------------------------------------------------------ streaming

#[test]
fn slow_and_interrupted_downloads_release_the_file_and_never_block_cleanup() {
    let env = large_archive("stream");
    let srv = start(&env, ExportLimits::default());
    let admin = claim(&env, &srv);
    let h = admin_h(&admin);
    let ready = prepared(&srv, &h);
    let id = ready["export_id"].as_str().unwrap().to_string();
    let len = ready["archive"]["bytes"].as_u64().unwrap() as usize;
    let exports = Arc::clone(&srv.running().plane.exports);

    // Slow: the client reads a few bytes at a time with pauses; the result is still exact.
    let mut s = send(&srv, "GET", &download_path(&id), &h, "");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    for reads in 0.. {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => panic!("{e}"),
        }
        if reads < 200 {
            std::thread::sleep(Duration::from_millis(5)); // about a second of stalling
        }
    }
    let slow = split(&buf);
    assert_eq!(slow.status, 200);
    assert_eq!(slow.body.len(), len);
    assert_eq!(sha256_hex(&slow.body).as_str(), ready["archive"]["sha256"]);
    eventually("the slow download is released", || {
        exports.downloads_in_progress() == 0
    });

    // Interrupted: the client reads the head and a little body, then goes away.
    let mut s = send(&srv, "GET", &download_path(&id), &h, "");
    let mut first = [0u8; 512];
    let n = s.read(&mut first).unwrap();
    assert!(n > 0);
    eventually("the download is in progress", || {
        exports.downloads_in_progress() == 1
    });
    // Dismissal while it is being read is deferred: the file stays for the reader.
    let d = dismiss(&srv, &id, &h);
    assert_eq!(
        (d.status, d.body["cleanup"].as_str()),
        (200, Some("deferred"))
    );
    assert_eq!(export_dirs(&env).len(), 1);
    assert_eq!(status(&srv, &id, &h).body["state"], "cleanup_pending");
    assert_eq!(download(&srv, &id, &h).status, 410, "never offered again");
    drop(s);
    eventually("the interrupted download is released", || {
        exports.downloads_in_progress() == 0
    });
    assert!(exports.sweep().is_empty());
    assert!(export_dirs(&env).is_empty(), "deleted once nobody reads it");
    assert_eq!(status(&srv, &id, &h).status, 404);
    srv.stop();
}

#[test]
fn concurrent_downloads_are_capped() {
    let env = large_archive("dlcap");
    let srv = start(
        &env,
        ExportLimits {
            max_downloads: 1,
            ..ExportLimits::default()
        },
    );
    let admin = claim(&env, &srv);
    let h = admin_h(&admin);
    let id = prepared(&srv, &h)["export_id"]
        .as_str()
        .unwrap()
        .to_string();
    let exports = Arc::clone(&srv.running().plane.exports);
    let mut s = send(&srv, "GET", &download_path(&id), &h, "");
    let mut first = [0u8; 64];
    s.read_exact(&mut first).unwrap();
    eventually("one download open", || exports.downloads_in_progress() == 1);
    let r = download(&srv, &id, &h);
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (429, Some("download_busy"))
    );
    drop(s);
    eventually("released", || exports.downloads_in_progress() == 0);
    assert_eq!(download(&srv, &id, &h).status, 200);
    srv.stop();
}

// ------------------------------------------------------------------ resources and lifecycle

#[test]
fn one_generation_at_a_time_and_per_admin_and_total_caps() {
    let env = Env::new("caps");
    let srv = start(&env, ExportLimits::default());
    let a = claim(&env, &srv);
    let b = browser(&srv, &a, "Admin B", true);
    let c = browser(&srv, &a, "Admin C", true);
    let (ha, hb, hc) = (admin_h(&a), admin_h(&b), admin_h(&c));
    let exports = Arc::clone(&srv.running().plane.exports);

    let (gate, started, resume) = CleanupGate::new();
    *exports.hooks.generation_gate.lock().unwrap() = Some(gate);
    let r = start_export(&srv, &ha);
    assert_eq!(r.status, 202);
    let id_a = r.body["export_id"].as_str().unwrap().to_string();
    started.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(exports.generating());

    // Duplicates and a second generation are refused while it runs; it cannot be cancelled.
    let dup = start_export(&srv, &ha);
    assert_eq!(
        (dup.status, dup.body["error"].as_str()),
        (429, Some("export_exists"))
    );
    let busy = start_export(&srv, &hb);
    assert_eq!(
        (busy.status, busy.body["error"].as_str()),
        (429, Some("export_busy"))
    );
    let d = dismiss(&srv, &id_a, &ha);
    assert_eq!(
        (d.status, d.body["error"].as_str()),
        (409, Some("export_preparing"))
    );
    let dl = download(&srv, &id_a, &ha);
    assert_eq!(
        (dl.status, dl.json()["error"].as_str()),
        (409, Some("export_preparing"))
    );
    assert_eq!(status(&srv, &id_a, &ha).body["state"], "preparing");
    assert_eq!(export_dirs(&env).len(), 1);

    *exports.hooks.generation_gate.lock().unwrap() = None;
    resume.send(()).unwrap();
    eventually("A is ready", || {
        status(&srv, &id_a, &ha).body["state"] == "ready"
    });
    assert!(!exports.generating());

    // B may start now; C is refused by the total cap (2) while A and B hold staging.
    assert_eq!(prepared(&srv, &hb)["state"], "ready");
    let full = start_export(&srv, &hc);
    assert_eq!(
        (full.status, full.body["error"].as_str()),
        (429, Some("too_many_exports"))
    );
    assert_eq!(export_dirs(&env).len(), 2);

    // Dismissing A frees capacity at once.
    assert_eq!(dismiss(&srv, &id_a, &ha).status, 200);
    assert_eq!(status(&srv, &id_a, &ha).status, 404);
    assert_eq!(export_dirs(&env).len(), 1);
    assert_eq!(prepared(&srv, &hc)["state"], "ready");
    assert!(env.log().contains("event=backup_export_dismissed"));
    srv.stop();
}

#[test]
fn ready_exports_expire_and_are_deleted() {
    let env = Env::new("expiry");
    let srv = start(&env, ExportLimits::default());
    let admin = claim(&env, &srv);
    let h = admin_h(&admin);
    let ready = prepared(&srv, &h);
    let id = ready["export_id"].as_str().unwrap().to_string();
    assert_eq!(ready["expires_in_s"], 15 * 60);
    env.advance(15 * 60 * 1000);
    let r = download(&srv, &id, &h);
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (410, Some("expired"))
    );
    let s = status(&srv, &id, &h);
    assert_eq!((s.status, s.body["error"].as_str()), (410, Some("expired")));
    assert!(srv.running().plane.exports.sweep().is_empty());
    assert!(export_dirs(&env).is_empty());
    assert_eq!(status(&srv, &id, &h).status, 404);
    // A new one can be started (no automatic restart happened).
    assert_eq!(prepared(&srv, &h)["state"], "ready");
    srv.stop();
}

#[test]
fn generation_and_verification_failures_offer_nothing_and_clean_up() {
    let env = Env::new("failures");
    let before = env.ledger();
    let srv = start(&env, ExportLimits::default());
    let admin = claim(&env, &srv);
    let h = admin_h(&admin);
    let exports = Arc::clone(&srv.running().plane.exports);

    exports.hooks.fail_generation.store(true, Ordering::SeqCst);
    let failed = prepared(&srv, &h);
    exports.hooks.fail_generation.store(false, Ordering::SeqCst);
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["error"]["code"], "io_error");
    assert_eq!(
        (failed["archive"].clone(), failed["download"].clone()),
        (Value::Null, Value::Null)
    );
    let id = failed["export_id"].as_str().unwrap().to_string();
    let r = download(&srv, &id, &h);
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (409, Some("export_failed"))
    );
    assert!(export_dirs(&env).is_empty(), "partial files deleted");

    // The final, strict verification refuses a damaged archive: never offered.
    exports.hooks.corrupt_archive.store(true, Ordering::SeqCst);
    let bad = prepared(&srv, &h);
    exports.hooks.corrupt_archive.store(false, Ordering::SeqCst);
    assert_eq!(bad["state"], "failed", "{bad}");
    assert_eq!(bad["error"]["code"], "verification_failed");
    let bad_id = bad["export_id"].as_str().unwrap();
    assert_eq!(download(&srv, bad_id, &h).status, 409);
    assert!(export_dirs(&env).is_empty());

    // Failed results can be dismissed; a working export still follows.
    assert_eq!(dismiss(&srv, &id, &h).status, 200);
    assert_eq!(status(&srv, &id, &h).status, 404);
    assert_eq!(prepared(&srv, &h)["state"], "ready");
    assert!(env.log().contains("event=backup_export_failed"));
    assert_eq!(env.ledger(), before);
    srv.stop();
}

#[test]
fn a_failed_deletion_keeps_the_export_withdrawn_and_is_retried() {
    let env = Env::new("cleanup");
    let srv = start(&env, ExportLimits::default());
    let admin = claim(&env, &srv);
    let h = admin_h(&admin);
    let exports = Arc::clone(&srv.running().plane.exports);
    let id = prepared(&srv, &h)["export_id"]
        .as_str()
        .unwrap()
        .to_string();

    exports.hooks.fail_cleanup.store(true, Ordering::SeqCst);
    let d = dismiss(&srv, &id, &h);
    assert_eq!(
        (d.status, d.body["error"].as_str()),
        (500, Some("cleanup_failed"))
    );
    assert_eq!(export_dirs(&env).len(), 1);
    assert_eq!(status(&srv, &id, &h).body["state"], "cleanup_pending");
    assert_eq!(download(&srv, &id, &h).status, 410);
    let busy = start_export(&srv, &h);
    assert_eq!(
        (busy.status, busy.body["error"].as_str()),
        (429, Some("export_exists")),
        "still counted"
    );
    assert_eq!(exports.sweep(), vec![id.clone()]);

    exports.hooks.fail_cleanup.store(false, Ordering::SeqCst);
    assert!(exports.sweep().is_empty());
    assert!(export_dirs(&env).is_empty());
    assert_eq!(status(&srv, &id, &h).status, 404);
    assert!(env.log().contains("event=backup_export_cleanup_failed"));
    srv.stop();
}

#[test]
fn startup_removes_leftover_exports_and_nothing_else() {
    let env = Env::new("startup");
    let root = env.dir.join("ledger").join("export-staging");
    std::fs::create_dir_all(root.join("job-export-0123abcd")).unwrap();
    std::fs::write(root.join("job-export-0123abcd").join(ARCHIVE_FILE), b"old").unwrap();
    std::fs::write(root.join("keep.txt"), b"not ours").unwrap();
    let srv = start(&env, ExportLimits::default());
    assert_eq!(export_dirs(&env), vec!["keep.txt".to_string()]);
    assert!(env.log().contains("event=export_staging_removed count=1"));
    let admin = claim(&env, &srv);
    let l = call(&srv.base, "GET", EXPORTS, &admin_h(&admin), None);
    assert_eq!(
        l.body["exports"],
        json!([]),
        "nothing of the old process is offered"
    );
    srv.stop();
}

#[test]
fn shutdown_waits_for_the_export_being_prepared_and_releases_the_ledger() {
    let env = Env::new("shutdown");
    let srv = start(&env, ExportLimits::default());
    let admin = claim(&env, &srv);
    let exports = Arc::clone(&srv.running().plane.exports);
    let (gate, started, resume) = CleanupGate::new();
    *exports.hooks.generation_gate.lock().unwrap() = Some(gate);
    assert_eq!(start_export(&srv, &admin_h(&admin)).status, 202);
    started.recv_timeout(Duration::from_secs(10)).unwrap();

    let stopped = Arc::new(AtomicU64::new(0));
    let flag = Arc::clone(&stopped);
    let t = std::thread::spawn(move || {
        srv.stop();
        flag.store(1, Ordering::SeqCst);
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        stopped.load(Ordering::SeqCst),
        0,
        "shutdown waits for the export"
    );
    resume.send(()).unwrap();
    t.join().unwrap();
    drop(exports);
    // The ledger's process lock is free again: a new server can open it.
    let store = Store::open(&env.dir.join("ledger")).unwrap();
    assert_eq!(store.list_libraries().unwrap().len(), 1);
}

#[test]
fn a_ready_file_changed_on_disk_is_never_served() {
    let env = Env::new("tamper");
    let srv = start(&env, ExportLimits::default());
    let admin = claim(&env, &srv);
    let h = admin_h(&admin);
    let id = prepared(&srv, &h)["export_id"]
        .as_str()
        .unwrap()
        .to_string();
    let dirs = export_dirs(&env);
    let file = env
        .dir
        .join("ledger")
        .join("export-staging")
        .join(&dirs[0])
        .join(ARCHIVE_FILE);
    let len = std::fs::metadata(&file).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .unwrap()
        .set_len(len - 1)
        .unwrap();
    let r = download(&srv, &id, &h);
    assert_eq!(r.status, 500);
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert!(r.header("content-disposition").is_none());
    assert_eq!(
        srv.running().plane.exports.downloads_in_progress(),
        0,
        "the refused download released its slot"
    );
    srv.stop();
}

/// Bytes of every file under `dir`.
fn disk_usage(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| match e.metadata() {
                    Ok(m) if m.is_dir() => disk_usage(&e.path()),
                    Ok(m) => m.len(),
                    Err(_) => 0,
                })
                .sum()
        })
        .unwrap_or(0)
}

/// Run one failing export while polling the export staging tree; its largest observed size.
fn failing_export_peak(srv: &Srv, env: &Env, h: &H) -> (Value, u64) {
    let root = env.dir.join("ledger").join("export-staging");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s = Arc::clone(&stop);
    let poll = std::thread::spawn(move || {
        let mut peak = 0;
        while !s.load(Ordering::SeqCst) {
            peak = peak.max(disk_usage(&root));
        }
        peak
    });
    let done = prepared(srv, h);
    stop.store(true, Ordering::SeqCst);
    (done, poll.join().unwrap())
}

#[test]
fn generation_stops_within_its_disk_budget() {
    // About 8 MiB of incompressible blob: staged it is ~8 MiB, and the ZIP another ~8 MiB.
    let env = large_archive("budget");
    let before = env.ledger();
    let budget = 12 << 20;
    let srv = start(
        &env,
        ExportLimits {
            max_staging_bytes: budget,
            ..ExportLimits::default()
        },
    );
    let admin = claim(&env, &srv);
    let h = admin_h(&admin);

    // Everything is staged (within budget); the ZIP is stopped part-way.
    let (failed, peak) = failing_export_peak(&srv, &env, &h);
    assert_eq!(failed["state"], "failed", "{failed}");
    assert_eq!(failed["error"]["code"], "too_large");
    assert!(peak <= budget, "staging peaked at {peak} > {budget}");
    assert!(
        peak > 8 << 20,
        "the blob was staged before the ZIP hit the budget ({peak})"
    );
    assert!(export_dirs(&env).is_empty(), "cleaned up");
    let id = failed["export_id"].as_str().unwrap();
    assert_eq!(download(&srv, id, &h).status, 409);
    srv.stop();

    // A budget below the blob's declared size: refused before anything large is written.
    let small = 1 << 20;
    let srv = start(
        &env,
        ExportLimits {
            max_staging_bytes: small,
            ..ExportLimits::default()
        },
    );
    let h = admin_h(&admin);
    let (failed, peak) = failing_export_peak(&srv, &env, &h);
    assert_eq!(failed["error"]["code"], "too_large");
    assert!(peak <= small, "staging peaked at {peak} > {small}");
    assert!(export_dirs(&env).is_empty());
    srv.stop();

    // With the default budget the same ledger exports fine.
    let srv = start(&env, ExportLimits::default());
    assert_eq!(prepared(&srv, &admin_h(&admin))["state"], "ready");
    assert_eq!(env.ledger(), before);
    srv.stop();
}

#[test]
fn failed_results_are_bounded_per_admin_and_in_total() {
    let env = Env::new("failcap");
    let srv = start(
        &env,
        ExportLimits {
            max_failed_results: 3,
            max_failed_results_per_owner: 2,
            ..ExportLimits::default()
        },
    );
    let a = claim(&env, &srv);
    let b = browser(&srv, &a, "Admin B", true);
    let (ha, hb) = (admin_h(&a), admin_h(&b));
    let exports = Arc::clone(&srv.running().plane.exports);
    let ids = |h: &H| -> Vec<String> {
        call(&srv.base, "GET", EXPORTS, h, None).body["exports"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["export_id"].as_str().unwrap().to_string())
            .collect()
    };
    let fail = |h: &H| -> String {
        let r = prepared(&srv, h);
        assert_eq!(r["state"], "failed");
        assert_eq!(r["error"]["code"], "io_error", "the diagnostic is kept");
        r["export_id"].as_str().unwrap().to_string()
    };

    exports.hooks.fail_generation.store(true, Ordering::SeqCst);
    let a_ids: Vec<String> = (0..5).map(|_| fail(&ha)).collect();
    let mut kept = ids(&ha);
    kept.sort();
    let mut newest = a_ids[3..].to_vec();
    newest.sort();
    assert_eq!(kept, newest, "A keeps its two newest results");
    assert_eq!(exports.export_count(), 2);
    assert_eq!(
        status(&srv, &a_ids[0], &ha).status,
        404,
        "the oldest is forgotten"
    );

    // B's failures push the total to its cap: the oldest result server-wide (A's) goes.
    let b_ids: Vec<String> = (0..3).map(|_| fail(&hb)).collect();
    assert_eq!(exports.export_count(), 3);
    assert_eq!(ids(&hb).len(), 2);
    assert!(ids(&hb).contains(&b_ids[2]));
    assert_eq!(ids(&ha), vec![a_ids[4].clone()]);

    // A ready export is never dropped to make room; nothing was retried.
    exports.hooks.fail_generation.store(false, Ordering::SeqCst);
    let ready = prepared(&srv, &ha);
    assert_eq!(ready["state"], "ready");
    exports.hooks.fail_generation.store(true, Ordering::SeqCst);
    for _ in 0..4 {
        fail(&hb);
    }
    let ready_id = ready["export_id"].as_str().unwrap();
    assert_eq!(status(&srv, ready_id, &ha).body["state"], "ready");
    assert!(
        exports.export_count() <= 2 + 3,
        "records {} beyond max_exports + max_failed_results",
        exports.export_count()
    );
    assert!(ids(&hb).len() <= 2);
    let starts = env.log().matches("event=backup_export_started").count();
    assert_eq!(
        starts,
        5 + 3 + 1 + 4,
        "one start per request, no automatic retries"
    );
    srv.stop();
}
