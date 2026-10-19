mod common;

use std::collections::BTreeMap;
use std::sync::Mutex;

use backup_export::{
    export_backup, Credential, ExportErrorKind as K, HttpSource, ReadOnlySource, Secret,
};
use common::*;
use portable_backup::{BackupErrorCode as C, BackupManifestV1, BackupVerifier, EntryPath, Limits};
use sync_protocol::{sha256_hex, LibraryId};

fn bearer() -> Option<Credential> {
    Some(Credential::Bearer(Secret::new(TOKEN)))
}

fn kind<T: std::fmt::Debug>(r: Result<T, backup_export::ExportError>) -> K {
    r.unwrap_err().kind
}

/// Independent inspection: entries in order, Deflate, then BackupVerifier over the extracted
/// logical entries.
fn inspect(path: &std::path::Path) -> (BackupManifestV1, BTreeMap<String, Vec<u8>>) {
    let entries = read_zip(path);
    let names: Vec<&str> = entries.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(names[0], "manifest.json");
    assert!(entries
        .iter()
        .all(|(_, _, c)| *c == zip::CompressionMethod::Deflated));
    let mut z = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    for i in 0..z.len() {
        let e = z.by_index_raw(i).unwrap();
        assert_eq!(
            e.last_modified(),
            Some(zip::DateTime::default()),
            "fixed timestamp"
        );
        assert_eq!(e.unix_mode().map(|m| m & 0o777), Some(0o644));
        assert!(e.is_file() && !e.encrypted());
    }
    let manifest = BackupManifestV1::parse(&entries[0].1, &Limits::DEFAULT).unwrap();
    let mut expected = vec!["manifest.json".to_string()];
    expected.extend(
        manifest
            .libraries
            .iter()
            .map(|l| format!("libraries/{}/snapshot.json", l.source_library_id)),
    );
    expected.extend(manifest.blobs.iter().map(|b| format!("blobs/{}", b.sha256)));
    assert_eq!(names, expected);

    let mut v = BackupVerifier::new(&entries[0].1, &Limits::DEFAULT).unwrap();
    for (name, bytes, _) in &entries[1..] {
        match EntryPath::parse(name).unwrap() {
            EntryPath::Snapshot(id) => {
                v.add_snapshot(id, &bytes[..]).unwrap();
            }
            EntryPath::Blob(sha) => v.verify_blob(&sha, &bytes[..]).unwrap(),
            EntryPath::Manifest => panic!("manifest twice"),
        }
    }
    assert_eq!(v.finish().unwrap().manifest(), &manifest);
    (
        manifest,
        entries.into_iter().map(|(n, b, _)| (n, b)).collect(),
    )
}

#[test]
fn exports_every_library_verified_with_get_requests_only() {
    let server = Server::start();
    let proxy = Proxy::start(&server.base_url());
    let out = TempDir::new("ok");
    let dest = out.join("ignite-backup.zip");
    let source = HttpSource::new(&proxy.url(), bearer()).unwrap();
    let report = export_backup(&source, &dest, &Limits::DEFAULT).unwrap();

    assert_eq!(report.path, dest);
    assert_no_leftovers(&out, &["ignite-backup.zip"]);
    let file = std::fs::read(&dest).unwrap();
    assert_eq!(report.archive_bytes, file.len() as u64);
    assert_eq!(report.archive_sha256, sha256_hex(&file));

    let (manifest, entries) = inspect(&dest);
    assert_eq!(manifest, report.manifest);
    let ids: Vec<LibraryId> = manifest
        .libraries
        .iter()
        .map(|l| l.source_library_id)
        .collect();
    assert_eq!(
        ids,
        vec![lib_a(), lib_b(), lib_c()],
        "source identities retained"
    );
    assert_eq!(manifest.blobs.len(), 2, "shared background stored once");

    for l in &manifest.libraries {
        let (through, bytes, sha) = server.snapshot(l.source_library_id);
        let entry = &entries[&format!("libraries/{}/snapshot.json", l.source_library_id)];
        assert_eq!(entry, &bytes, "original canonical bytes");
        assert_eq!(
            l.source_snapshot_sha256.to_string(),
            sha,
            "server's own snapshot hash"
        );
        assert_eq!(l.source_through_server_seq, through);
    }
    assert_eq!(manifest.libraries[0].source_through_server_seq, 2);
    assert_eq!(
        (
            manifest.libraries[0].notebook_count,
            manifest.libraries[0].page_count
        ),
        (3, 4)
    );
    for (sha, bytes) in &server.blobs {
        assert_eq!(&entries[&format!("blobs/{sha}")], bytes);
    }

    let lines = proxy.request_lines();
    assert_eq!(lines.len(), 1 + 3 + 2, "{lines:#?}");
    assert!(
        lines.iter().all(|l| l.starts_with("GET /v1/")),
        "{lines:#?}"
    );
    for sha in server.blobs.keys() {
        assert_eq!(lines.iter().filter(|l| l.contains(sha.as_str())).count(), 1);
    }
    // The credential went in the Authorization header only, never into a request line.
    assert!(lines.iter().all(|l| !l.contains(TOKEN)));
    assert!(proxy
        .heads
        .lock()
        .unwrap()
        .iter()
        .all(|h| h.contains(&format!("Bearer {TOKEN}"))));
}

#[test]
fn equal_backups_produce_identical_archives() {
    let server = Server::start();
    let out = TempDir::new("det");
    let source = HttpSource::new(&server.base_url(), bearer()).unwrap();
    let one = export_backup(&source, &out.join("one.zip"), &Limits::DEFAULT).unwrap();
    let two = export_backup(&source, &out.join("two.zip"), &Limits::DEFAULT).unwrap();
    assert_eq!(one.archive_sha256, two.archive_sha256);
    assert_eq!(
        std::fs::read(out.join("one.zip")).unwrap(),
        std::fs::read(out.join("two.zip")).unwrap()
    );
}

#[test]
fn missing_or_wrong_credentials_fail_cleanly() {
    let server = Server::start();
    let proxy = Proxy::start(&server.base_url());
    let out = TempDir::new("auth");
    let dest = out.join("b.zip");
    for cred in [
        None,
        Some(Credential::Bearer(Secret::new("not-the-token"))),
        Some(Credential::SessionCookie(Secret::new(
            "session-secret-value",
        ))),
    ] {
        let e = export_backup(
            &HttpSource::new(&proxy.url(), cred).unwrap(),
            &dest,
            &Limits::DEFAULT,
        )
        .unwrap_err();
        assert_eq!(e.kind, K::Unauthorized, "{e}");
        assert!(
            !e.message.contains("not-the-token") && !e.message.contains("session-secret-value")
        );
        assert_no_leftovers(&out, &[]);
    }
    let heads = proxy.heads.lock().unwrap().clone();
    assert_eq!(heads.len(), 3, "stops at the first refusal");
    assert!(heads.iter().all(|h| h.starts_with("GET /v1/libraries ")));
    assert!(
        heads[2].contains("Cookie: scribe_session=session-secret-value")
            && !heads[2].contains("Authorization")
    );
}

#[test]
fn an_existing_destination_is_never_touched() {
    let server = Server::start();
    let fake = FakeSource::from(server.capture());
    let out = TempDir::new("exists");
    let dest = out.join("b.zip");
    std::fs::write(&dest, b"precious").unwrap();
    assert_eq!(
        kind(export_backup(&fake, &dest, &Limits::DEFAULT)),
        K::Destination
    );
    assert!(fake.requests().is_empty(), "refused before any request");
    assert_eq!(std::fs::read(&dest).unwrap(), b"precious");
    assert_no_leftovers(&out, &["b.zip"]);
}

#[test]
fn a_destination_created_during_export_is_never_overwritten() {
    let server = Server::start();
    let out = TempDir::new("race");
    let dest = out.join("b.zip");
    let last_blob = format!("/v1/blobs/{}", server.blobs.keys().last().unwrap());
    let hooked = Hooked {
        inner: FakeSource::from(server.capture()),
        after: last_blob,
        hook: Mutex::new(Some(Box::new(|| {
            std::fs::write(&dest, b"someone else's").unwrap()
        }))),
    };
    assert_eq!(
        kind(export_backup(&hooked, &dest, &Limits::DEFAULT)),
        K::Destination
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"someone else's");
    assert_no_leftovers(&out, &["b.zip"]);
}

#[test]
fn unsafe_destinations_are_refused() {
    let server = Server::start();
    let fake = FakeSource::from(server.capture());
    let out = TempDir::new("dest");
    std::fs::create_dir(out.join("dir.zip")).unwrap();
    for bad in [
        out.join("backup.tar"),
        out.join(".hidden.zip"),
        out.join(".zip"),
        out.join("missing").join("b.zip"),
        out.join("dir.zip"),
    ] {
        assert_eq!(
            kind(export_backup(&fake, &bad, &Limits::DEFAULT)),
            K::Destination,
            "{}",
            bad.display()
        );
    }
    assert!(fake.requests().is_empty());
    assert_no_leftovers(&out, &["dir.zip"]);
}

#[test]
fn snapshot_changes_during_export_use_the_snapshot_response_sequence() {
    let mut server = Server::start();
    let list_head = {
        let (_, body) = server.call("GET", "/v1/libraries", None);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        v["libraries"][0]["head_server_seq"].as_u64().unwrap()
    };
    assert_eq!(list_head, 2);
    let base = server.base_url();
    let out = TempDir::new("moving");
    let dest = out.join("b.zip");
    let report = {
        let srv = Mutex::new(&mut server);
        let hooked = Hooked {
            inner: HttpSource::new(&base, bearer()).unwrap(),
            after: "/v1/libraries".into(),
            hook: Mutex::new(Some(Box::new(|| srv.lock().unwrap().write_to_a()))),
        };
        export_backup(&hooked, &dest, &Limits::DEFAULT).unwrap()
    };
    let (through, bytes, _) = server.snapshot(lib_a());
    assert_eq!(through, 3);
    let a = &report.manifest.libraries[0];
    assert_eq!(
        a.source_through_server_seq, 3,
        "not the listed head {list_head}"
    );
    assert_eq!(a.source_snapshot_sha256, sha256_hex(&bytes));
    let (_, entries) = inspect(&dest);
    assert_eq!(
        entries[&format!("libraries/{}/snapshot.json", lib_a())],
        bytes
    );
}

fn fails(fake: &FakeSource, want: K) {
    let out = TempDir::new("fail");
    let dest = out.join("b.zip");
    let e = export_backup(fake, &dest, &Limits::DEFAULT).unwrap_err();
    assert_eq!(e.kind, want, "{e}");
    assert!(!dest.exists());
    assert_no_leftovers(&out, &[]);
}

fn edit_list(fake: &mut FakeSource, f: impl FnOnce(&mut serde_json::Value)) {
    let (_, body) = fake.routes.get_mut("/v1/libraries").unwrap();
    let mut v: serde_json::Value = serde_json::from_slice(body).unwrap();
    f(&mut v);
    *body = serde_json::to_vec(&v).unwrap();
}

#[test]
fn malformed_server_responses_are_refused() {
    let server = Server::start();
    let base = server.capture();

    let mut f = FakeSource::from(base.clone());
    f.routes
        .insert("/v1/libraries".into(), (200, b"<html>".to_vec()));
    fails(&f, K::Malformed);

    let mut f = FakeSource::from(base.clone());
    edit_list(&mut f, |v| v["v"] = 2.into());
    fails(&f, K::Malformed);

    let mut f = FakeSource::from(base.clone());
    edit_list(&mut f, |v| v["libraries"][0]["owner"] = "x".into());
    fails(&f, K::Malformed);

    let mut f = FakeSource::from(base.clone());
    edit_list(&mut f, |v| {
        let first = v["libraries"][0].clone();
        v["libraries"].as_array_mut().unwrap().push(first);
    });
    fails(&f, K::Malformed);

    let mut f = FakeSource::from(base.clone());
    edit_list(&mut f, |v| {
        v["libraries"][0]["library_id"] = "NOT-A-UUID".into()
    });
    fails(&f, K::Malformed);

    let mut f = FakeSource::from(base.clone());
    edit_list(&mut f, |v| v["libraries"] = serde_json::json!([]));
    fails(&f, K::Backup(C::InvalidManifest));

    let mut f = FakeSource::from(base.clone());
    f.edit_snapshot(lib_b(), |m| {
        m.insert("extra".into(), raw("1"));
    });
    fails(&f, K::Malformed);

    let mut f = FakeSource::from(base.clone());
    f.edit_snapshot(lib_b(), |m| {
        m.remove("through_server_seq");
    });
    fails(&f, K::Malformed);

    let mut f = FakeSource::from(base.clone());
    f.edit_snapshot(lib_b(), |m| {
        m.insert("through_server_seq".into(), raw("-1"));
    });
    fails(&f, K::Malformed);

    let mut f = FakeSource::from(base.clone());
    f.edit_snapshot(lib_b(), |m| {
        m.insert("library_id".into(), raw(&format!("\"{}\"", lib_c())));
    });
    fails(&f, K::Malformed);

    let mut f = FakeSource::from(base.clone());
    f.routes.insert(
        format!("/v1/libraries/{}/snapshot", lib_c()),
        (500, b"{}".to_vec()),
    );
    fails(&f, K::Http(500));
}

#[test]
fn snapshot_integrity_is_checked_against_the_actual_bytes() {
    let server = Server::start();
    let base = server.capture();

    // Reported hash does not match the bytes.
    let mut f = FakeSource::from(base.clone());
    f.edit_snapshot(lib_b(), |m| {
        m.insert(
            "snapshot_sha256".into(),
            raw(&format!("\"{}\"", "0".repeat(64))),
        );
    });
    fails(&f, K::Backup(C::HashMismatch));

    // Consistent hash, but not canonical bytes.
    let mut f = FakeSource::from(base.clone());
    let pretty = {
        let v: serde_json::Value = serde_json::from_slice(&f.raw_snapshot(lib_b())).unwrap();
        serde_json::to_vec_pretty(&v).unwrap()
    };
    f.set_raw_snapshot(lib_b(), "0", &pretty);
    fails(&f, K::Backup(C::NonCanonical));

    // The response says library B, the snapshot inside is library C.
    let mut f = FakeSource::from(base.clone());
    let c = f.raw_snapshot(lib_c());
    f.set_raw_snapshot(lib_b(), "0", &c);
    fails(&f, K::Backup(C::LibraryMismatch));

    // Semantically invalid snapshot (sync-protocol refusal).
    let mut f = FakeSource::from(base.clone());
    let broken = String::from_utf8(f.raw_snapshot(lib_c()))
        .unwrap()
        .replace("\"Empty\"", "\"\"");
    f.set_raw_snapshot(lib_c(), "0", broken.as_bytes());
    fails(&f, K::Backup(C::InvalidSnapshot));
}

#[test]
fn sequence_numbers_are_preserved_losslessly() {
    let server = Server::start();
    let mut f = FakeSource::from(server.capture());
    let raw_b = f.raw_snapshot(lib_b());
    f.set_raw_snapshot(lib_b(), &u64::MAX.to_string(), &raw_b);
    let out = TempDir::new("u64");
    let dest = out.join("b.zip");
    let report = export_backup(&f, &dest, &Limits::DEFAULT).unwrap();
    assert_eq!(
        report.manifest.libraries[1].source_through_server_seq,
        u64::MAX
    );
    let (manifest, _) = inspect(&dest);
    assert_eq!(manifest.libraries[1].source_through_server_seq, u64::MAX);
    // One past u64 is refused rather than rounded.
    f.set_raw_snapshot(lib_b(), "18446744073709551616", &raw_b);
    fails(&f, K::Malformed);
}

#[test]
fn blob_failures_are_refused() {
    let server = Server::start();
    let base = server.capture();
    let (sha, bytes) = server
        .blobs
        .iter()
        .next()
        .map(|(s, b)| (s.clone(), b.clone()))
        .unwrap();
    let key = format!("/v1/blobs/{sha}");

    let mut f = FakeSource::from(base.clone());
    f.routes.remove(&key);
    fails(&f, K::Backup(C::MissingBlob));

    let mut f = FakeSource::from(base.clone());
    let mut wrong = bytes.clone();
    wrong[7] ^= 0x55;
    f.routes.insert(key.clone(), (200, wrong));
    fails(&f, K::Backup(C::HashMismatch));

    let mut f = FakeSource::from(base.clone());
    f.routes
        .insert(key.clone(), (200, bytes[..bytes.len() - 1].to_vec()));
    fails(&f, K::Backup(C::SizeMismatch));

    let mut f = FakeSource::from(base.clone());
    let mut longer = bytes.clone();
    longer.extend_from_slice(&[0; 10]);
    f.routes.insert(key.clone(), (200, longer));
    fails(&f, K::Backup(C::SizeMismatch));

    let mut f = FakeSource::from(base.clone());
    f.routes.insert(key.clone(), (503, vec![]));
    fails(&f, K::Http(503));

    let mut f = FakeSource::from(base.clone());
    f.routes.insert(key.clone(), (401, vec![]));
    fails(&f, K::Unauthorized);
}

#[test]
fn interrupted_transfers_and_failed_reads_leave_nothing_behind() {
    let server = Server::start();
    let base = server.capture();
    let blob = format!("/v1/blobs/{}", server.blobs.keys().next().unwrap());
    let snap = format!("/v1/libraries/{}/snapshot", lib_a());

    let mut f = FakeSource::from(base.clone());
    f.interrupt.insert(blob.clone(), 500);
    fails(&f, K::Transport);

    let mut f = FakeSource::from(base.clone());
    f.interrupt.insert(snap.clone(), 100);
    fails(&f, K::Transport);

    let mut f = FakeSource::from(base.clone());
    f.interrupt.insert("/v1/libraries".into(), 10);
    fails(&f, K::Transport);

    let mut f = FakeSource::from(base.clone());
    f.unreachable.push(blob);
    fails(&f, K::Transport);

    // A real server that goes away mid-export.
    let server2 = Server::start();
    let base_url = server2.base_url();
    let out = TempDir::new("gone");
    let dest = out.join("b.zip");
    let holder = Mutex::new(Some(server2));
    let hooked = Hooked {
        inner: HttpSource::new(&base_url, bearer()).unwrap(),
        after: format!("/v1/libraries/{}/snapshot", lib_a()),
        hook: Mutex::new(Some(Box::new(|| {
            let s = holder.lock().unwrap().take().unwrap();
            s.handle.shutdown();
        }))),
    };
    let e = export_backup(&hooked, &dest, &Limits::DEFAULT).unwrap_err();
    assert_eq!(e.kind, K::Transport, "{e}");
    assert_no_leftovers(&out, &[]);
}

#[test]
fn limits_are_applied_before_and_while_downloading() {
    let server = Server::start();
    let f = FakeSource::from(server.capture());
    let out = TempDir::new("limits");
    let dest = out.join("b.zip");
    let check = |l: Limits, want: C| {
        let e = export_backup(&f, &dest, &l).unwrap_err();
        assert_eq!(e.kind, K::Backup(want), "{e}");
        assert_no_leftovers(&out, &[]);
    };
    check(
        Limits {
            max_libraries: 2,
            ..Limits::DEFAULT
        },
        C::LimitExceeded,
    );
    check(
        Limits {
            max_snapshot_bytes: 300,
            ..Limits::DEFAULT
        },
        C::LimitExceeded,
    );
    check(
        Limits {
            max_blobs: 1,
            ..Limits::DEFAULT
        },
        C::LimitExceeded,
    );
    check(
        Limits {
            max_blob_bytes: 1000,
            ..Limits::DEFAULT
        },
        C::LimitExceeded,
    );
    check(
        Limits {
            max_total_blob_bytes: 2000,
            ..Limits::DEFAULT
        },
        C::LimitExceeded,
    );
    check(
        Limits {
            max_total_snapshot_bytes: 1000,
            ..Limits::DEFAULT
        },
        C::LimitExceeded,
    );

    // An oversized response body is cut off, not buffered.
    let mut huge = FakeSource::from(server.capture());
    let mut body = b"{\"v\":1,\"libraries\":[".to_vec();
    body.resize(10 << 20, b' ');
    huge.routes.insert("/v1/libraries".into(), (200, body));
    let e = export_backup(
        &huge,
        &dest,
        &Limits {
            max_libraries: 4,
            ..Limits::DEFAULT
        },
    )
    .unwrap_err();
    assert_eq!(e.kind, K::Backup(C::LimitExceeded), "{e}");
}

#[test]
fn http_source_refuses_unsafe_urls_redirects_and_never_prints_secrets() {
    for bad in [
        "https://host:8787",
        "ftp://host",
        "http://",
        "http://user:pw@host:8787",
        "http://host:8787/v1",
        "http://host:8787?x=1",
        "host:8787",
    ] {
        assert!(HttpSource::new(bad, None).is_err(), "{bad}");
    }
    assert!(HttpSource::new("http://h:1", Some(Credential::Bearer(Secret::new("a b")))).is_err());
    assert!(HttpSource::new(
        "http://h:1",
        Some(Credential::SessionCookie(Secret::new("")))
    )
    .is_err());
    let s = HttpSource::new(
        "http://127.0.0.1:9/",
        Some(Credential::Bearer(Secret::new("very-secret"))),
    )
    .unwrap();
    assert!(!format!("{s:?}").contains("very-secret"));
    assert!(s.get("/control/v1/status").is_err());
    assert!(s.get("/v1/../control").is_err());

    // A redirect is reported, never followed (it could carry the credential elsewhere).
    let elsewhere = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    elsewhere.set_nonblocking(true).unwrap();
    let target = elsewhere.local_addr().unwrap();
    let redirector = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = redirector.local_addr().unwrap();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for c in redirector.incoming().take(1) {
            let mut c = c.unwrap();
            let mut buf = [0u8; 4096];
            let _ = c.read(&mut buf);
            let _ = write!(c, "HTTP/1.1 302 Found\r\nLocation: http://{target}/v1/libraries\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    let src = HttpSource::new(&format!("http://{addr}"), bearer()).unwrap();
    let out = TempDir::new("redirect");
    assert_eq!(
        kind(export_backup(&src, &out.join("b.zip"), &Limits::DEFAULT)),
        K::Http(302)
    );
    assert!(elsewhere.accept().is_err(), "redirect target was contacted");
    assert_no_leftovers(&out, &[]);
}
