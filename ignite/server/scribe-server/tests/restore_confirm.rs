//! Slice 1F-B2: one-time confirmed restoration through a real `scribe-server`: the reviewed
//! archive and mapping are restored exactly once, every other caller or mismatch publishes
//! nothing, and the result outlives a lost response.

mod common;

use std::collections::BTreeSet;
use std::sync::{mpsc, Barrier};

use common::*;
use scribe_server::restore::CleanupGate;
use sync_protocol::SyncSnapshotV1;

// ------------------------------------------------------------------ helpers

fn quick() -> RestoreLimits {
    RestoreLimits {
        sweep_interval: Duration::from_millis(50),
        ..RestoreLimits::default()
    }
}

/// Upload `b` and return the 201 summary (with its one-time confirmation).
fn prepare(srv: &Srv, admin: &str, b: &Backup) -> Value {
    let r = upload(srv, &cookie(admin), &b.bytes);
    assert_eq!(r.status, 201, "{:?}", r.body);
    r.body
}

fn id_of(v: &Value) -> String {
    v["job_id"].as_str().unwrap().to_string()
}

/// The confirmation body that echoes exactly the prepared summary.
fn confirmation(v: &Value) -> Value {
    json!({
        "v": 1,
        "confirmation": v["confirmation"],
        "archive_sha256": v["archive"]["sha256"],
        "destinations": v["libraries"].as_array().unwrap().iter().map(|l| json!({
            "source_library_id": l["source_library_id"],
            "destination_library_id": l["destination_library_id"],
        })).collect::<Vec<_>>(),
    })
}

fn confirm(srv: &Srv, h: &H, id: &str, body: &Value) -> Resp {
    call(
        &srv.base,
        "POST",
        &format!("{UPLOAD}/{id}/confirm"),
        h,
        Some(body.clone()),
    )
}

fn mapping(v: &Value) -> Vec<(String, String)> {
    v["libraries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["source_library_id"].as_str().unwrap().to_string(),
                l["destination_library_id"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// Wait for the job to reach `succeeded` or `failed`; returns its summary.
fn finished(srv: &Srv, admin: &str, id: &str) -> Value {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let g = job(srv, "GET", id, &cookie(admin));
        assert_eq!(g.status, 200, "{:?}", g.body);
        let state = g.body["state"].as_str().unwrap().to_string();
        if state == "succeeded" || state == "failed" {
            return g.body;
        }
        assert!(Instant::now() < end, "still {state}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Every row and blob file of `before` is still present, byte for byte, in `after`.
fn preserved(before: &LedgerState, after: &LedgerState) {
    for (t, rows) in &before.tables {
        for row in rows {
            assert!(
                after.tables[t].contains(row),
                "{t}: an existing row changed"
            );
        }
    }
    for (name, bytes) in &before.files {
        assert_eq!(
            after.files.get(name),
            Some(bytes),
            "blob file {name} changed"
        );
    }
}

fn library_rows(s: &LedgerState) -> usize {
    s.tables["libraries"].len()
}

/// What the restored genesis of `src` under `dst` must be: the archive's snapshot, re-keyed.
fn expected_genesis(b: &Backup, src: &str, dst: &str) -> Vec<u8> {
    let name = format!("libraries/{src}/snapshot.json");
    let (_, bytes) = b.entries().into_iter().find(|(n, _)| *n == name).unwrap();
    let mut s = SyncSnapshotV1::parse(&bytes).unwrap();
    s.library_id = LibraryId::parse(dst).unwrap();
    s.to_canonical_bytes().unwrap()
}

/// The restored libraries hold exactly the archive's content under the approved ids.
fn check_restored(store: &Store, b: &Backup, map: &[(String, String)]) {
    for (src, dst) in map {
        let info = store.library(dst).unwrap();
        assert_eq!(
            info.head_server_seq, 0,
            "a restored library starts at sequence 0"
        );
        let want = expected_genesis(b, src, dst);
        assert_eq!(store.genesis(dst).unwrap(), want);
        assert_eq!(store.snapshot(dst).unwrap().snapshot_bytes, want);
        assert!(store.verify_replay(dst).unwrap());
    }
    let (bg, _) = testkit::blob_background();
    for blob in [bg, second_blob()] {
        assert_eq!(store.get_blob(sha256_hex(&blob).as_str()).unwrap(), blob);
    }
}

fn store_of(srv: &Srv) -> &Store {
    &srv.running().handle.store
}

// ------------------------------------------------------------------ tests

#[test]
fn a_confirmed_restore_publishes_exactly_the_reviewed_mapping_once() {
    let env = Env::new("confirm-ok");
    let b = backup();
    let srv = env.start(quick());
    let admin = claim(&env, &srv);
    let before = env.ledger();
    let prepared = prepare(&srv, &admin, &b);
    let id = id_of(&prepared);
    let map = mapping(&prepared);
    let secret = prepared["confirmation"].as_str().unwrap().to_string();

    let r = confirm(&srv, &cookie(&admin), &id, &confirmation(&prepared));
    assert_eq!(r.status, 202, "{:?}", r.body);
    assert_eq!(
        r.body,
        json!({ "v": 1, "job_id": id, "state": "executing" })
    );
    let done = finished(&srv, &admin, &id);
    assert_eq!(done["state"], "succeeded");
    assert_eq!(done["usable"], false);
    let report = &done["result"]["report"];
    assert_eq!(done["result"]["outcome"], "succeeded");
    assert_eq!(done["result"]["publication"], "complete");
    assert_eq!(report["archive_sha256"], sha256_hex(&b.bytes).as_str());
    assert_eq!(report["archive_bytes"], b.bytes.len());
    let reported: Vec<(String, String)> = report["libraries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["source_library_id"].as_str().unwrap().to_string(),
                l["library_id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        reported.iter().collect::<BTreeSet<_>>(),
        map.iter().collect::<BTreeSet<_>>()
    );
    // testkit's background was already in the ledger; the second library's is new.
    assert_eq!(
        (&report["blobs_installed"], &report["blobs_already_present"]),
        (&json!(1), &json!(1))
    );
    check_restored(store_of(&srv), &b, &map);
    let after = env.ledger();
    preserved(&before, &after);
    assert_eq!(library_rows(&after), library_rows(&before) + 2);

    // The staging is deleted; the result stays readable without the secret.
    eventually("staging deleted", || env.staging().is_empty());
    let done = job(&srv, "GET", &id, &cookie(&admin)).body;
    assert_eq!(done["cleanup_pending"], false);
    let listed = call(&srv.base, "GET", UPLOAD, &cookie(&admin), None).body;
    assert_eq!(listed["jobs"][0], done);
    for v in [&done, &listed] {
        assert!(
            !v.to_string().contains(&secret),
            "the secret never reappears"
        );
    }
    assert!(!env.log().contains(&secret));
    assert!(env
        .log()
        .contains(&format!("event=restore_succeeded job_id={id}")));

    // Replays never run again.
    let again = confirm(&srv, &cookie(&admin), &id, &confirmation(&prepared));
    assert_eq!(
        (again.status, again.body["error"].as_str()),
        (409, Some("already_confirmed"))
    );
    assert_eq!(library_rows(&env.ledger()), library_rows(&before) + 2);
    // A finished job without staging uses no capacity: the same admin can prepare again.
    let next = prepare(&srv, &admin, &b);
    assert_eq!(
        job(&srv, "DELETE", &id_of(&next), &cookie(&admin)).status,
        200
    );

    // The result expires.
    env.advance(15 * 60 * 1000);
    eventually("the sweeper forgets the result", || {
        srv.running().plane.restore.job_count() == 0
    });
    assert_eq!(job(&srv, "GET", &id, &cookie(&admin)).status, 404);
    assert!(env
        .log()
        .contains(&format!("event=restore_result_expired job_id={id}")));

    // Durable: reopen the ledger directly.
    srv.stop();
    let store = Store::open(&env.dir.join("ledger")).unwrap();
    check_restored(&store, &b, &map);
    assert!(store.unindexed_blob_files().unwrap().is_empty());
    drop(store);
    preserved(&before, &env.ledger());
}

#[test]
fn only_the_owning_browser_admin_with_the_exact_confirmation_can_restore() {
    let env = Env::new("confirm-authz");
    let b = backup();
    let srv = env.start(RestoreLimits {
        sweep_interval: Duration::from_secs(3600),
        ..RestoreLimits::default()
    });
    let admin = claim(&env, &srv);
    let member = browser(&srv, &admin, "Member browser", false);
    let kindle = kindle(&srv, &admin);
    let other = browser(&srv, &admin, "Second admin", true);
    let before = env.ledger();
    let prepared = prepare(&srv, &admin, &b);
    let id = id_of(&prepared);
    let good = confirmation(&prepared);
    let edit = |f: &dyn Fn(&mut Value)| {
        let mut v = good.clone();
        f(&mut v);
        v
    };
    let mut swapped = good.clone();
    let (d0, d1) = (
        swapped["destinations"][0]["destination_library_id"].clone(),
        swapped["destinations"][1]["destination_library_id"].clone(),
    );
    swapped["destinations"][0]["destination_library_id"] = d1;
    swapped["destinations"][1]["destination_library_id"] = d0;
    let mut reordered = good.clone();
    reordered["destinations"].as_array_mut().unwrap().reverse();
    let fresh = LibraryId::from_uuid(testkit::uuid7(9, 77))
        .unwrap()
        .to_string();

    let cases: Vec<(&str, H, Value, u16, &str)> = vec![
        ("anonymous", csrf(), good.clone(), 401, "unauthorized"),
        (
            "Kindle bearer",
            H {
                bearer: Some(kindle.clone()),
                csrf: true,
                ..H::default()
            },
            good.clone(),
            403,
            "browser_session_required",
        ),
        (
            "admin as bearer",
            H {
                bearer: Some(admin.clone()),
                csrf: true,
                ..H::default()
            },
            good.clone(),
            403,
            "browser_session_required",
        ),
        (
            "member browser",
            cookie(&member),
            good.clone(),
            403,
            "forbidden",
        ),
        (
            "no CSRF marker",
            H {
                csrf: false,
                ..cookie(&admin)
            },
            good.clone(),
            403,
            "csrf",
        ),
        (
            "another administrator",
            cookie(&other),
            good.clone(),
            404,
            "not_found",
        ),
        (
            "wrong secret",
            cookie(&admin),
            edit(&|v| v["confirmation"] = json!("A".repeat(43))),
            403,
            "confirmation_mismatch",
        ),
        (
            "malformed secret",
            cookie(&admin),
            edit(&|v| v["confirmation"] = json!("***")),
            403,
            "confirmation_mismatch",
        ),
        (
            "wrong archive hash",
            cookie(&admin),
            edit(&|v| v["archive_sha256"] = json!(sha256_hex(b"x").as_str())),
            403,
            "confirmation_mismatch",
        ),
        (
            "swapped destinations",
            cookie(&admin),
            swapped,
            403,
            "confirmation_mismatch",
        ),
        (
            "incomplete mapping",
            cookie(&admin),
            edit(&|v| {
                v["destinations"].as_array_mut().unwrap().pop();
            }),
            403,
            "confirmation_mismatch",
        ),
        (
            "duplicated entry",
            cookie(&admin),
            edit(&|v| {
                let d = v["destinations"][0].clone();
                v["destinations"].as_array_mut().unwrap()[1] = d;
            }),
            403,
            "confirmation_mismatch",
        ),
        (
            "a destination of the caller's choosing",
            cookie(&admin),
            edit(&|v| v["destinations"][0]["destination_library_id"] = json!(fresh)),
            403,
            "confirmation_mismatch",
        ),
        (
            "unknown version",
            cookie(&admin),
            edit(&|v| v["v"] = json!(2)),
            400,
            "unsupported_version",
        ),
        (
            "unknown field",
            cookie(&admin),
            edit(&|v| v["extra"] = json!(1)),
            400,
            "bad_request",
        ),
        (
            "oversized field",
            cookie(&admin),
            edit(&|v| v["confirmation"] = json!("A".repeat(65))),
            400,
            "bad_request",
        ),
    ];
    for (what, h, body, status, code) in &cases {
        let r = confirm(&srv, h, &id, body);
        assert_eq!(
            (r.status, r.body["error"].as_str()),
            (*status, Some(*code)),
            "{what}"
        );
        assert!(!r
            .body
            .to_string()
            .contains(prepared["confirmation"].as_str().unwrap()));
        assert_eq!(env.ledger(), before, "{what}: nothing published");
    }
    // Wrong content type, and a non-loopback peer.
    let r = upload_to(
        &srv,
        "POST",
        &format!("{UPLOAD}/{id}/confirm"),
        &cookie(&admin),
        good.to_string().as_bytes(),
    );
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (400, Some("wrong_content_type"))
    );
    let hooks = &srv.running().plane.restore.hooks;
    hooks.treat_peers_as_remote.store(true, Ordering::SeqCst);
    assert_eq!(confirm(&srv, &cookie(&admin), &id, &good).status, 403);
    hooks.treat_peers_as_remote.store(false, Ordering::SeqCst);
    // Still ready: none of the refusals consumed the secret.
    assert_eq!(job(&srv, "GET", &id, &cookie(&admin)).body["usable"], true);
    assert_eq!(env.ledger(), before);

    // A cancelled job cannot be confirmed.
    let theirs = prepare(&srv, &other, &b);
    assert_eq!(
        job(&srv, "DELETE", &id_of(&theirs), &cookie(&other)).status,
        200
    );
    let r = confirm(
        &srv,
        &cookie(&other),
        &id_of(&theirs),
        &confirmation(&theirs),
    );
    assert_eq!(r.status, 404);

    // A revoked administrator can never start a restore.
    let me = device_id(&srv, &admin);
    let mine = job(&srv, "GET", &id, &cookie(&admin)).body;
    srv.running().plane.store.revoke(&me).unwrap();
    assert_eq!(confirm(&srv, &cookie(&admin), &id, &reordered).status, 401);
    assert_eq!(mine["usable"], true);

    // An expired job cannot be confirmed (array order of the mapping does not matter).
    let theirs = prepare(&srv, &other, &b);
    env.advance(15 * 60 * 1000);
    let mut body = confirmation(&theirs);
    body["destinations"].as_array_mut().unwrap().reverse();
    let r = confirm(&srv, &cookie(&other), &id_of(&theirs), &body);
    assert_eq!((r.status, r.body["error"].as_str()), (410, Some("expired")));
    assert_eq!(env.ledger(), before);
    srv.stop();
}

#[test]
fn the_reviewed_archive_identity_is_enforced_at_execution() {
    let env = Env::new("confirm-identity");
    let b = backup();
    let srv = env.start(quick());
    let admin = claim(&env, &srv);
    let before = env.ledger();
    let staged = |env: &Env| {
        let dir = env
            .staging()
            .into_iter()
            .find(|d| d.starts_with("job-upload-"))
            .unwrap();
        env.dir
            .join("ledger/restore-staging")
            .join(dir)
            .join("archive.zip")
    };
    // Tampered after inspection: refused before any blob is installed.
    let prepared = prepare(&srv, &admin, &b);
    let path = staged(&env);
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 30;
    bytes[last] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(
        confirm(
            &srv,
            &cookie(&admin),
            &id_of(&prepared),
            &confirmation(&prepared)
        )
        .status,
        202
    );
    let done = finished(&srv, &admin, &id_of(&prepared));
    assert_eq!(done["state"], "failed");
    assert_eq!(done["result"]["error"], "archive_changed");
    assert_eq!(done["result"]["publication"], "none");
    assert_eq!(done["result"]["unindexed_blob_files_possible"], false);
    assert!(
        !done.to_string().contains("restore-staging"),
        "no internal paths"
    );
    assert_eq!(
        env.ledger(),
        before,
        "no blob file, no index row, no library"
    );
    eventually("staging deleted", || env.staging().is_empty());

    // Replaced by another well-formed archive (the same entries in another order, so the same
    // length): still not the reviewed one.
    let prepared = prepare(&srv, &admin, &b);
    let other = b.variant(|e| e.reverse());
    assert_eq!(other.len(), b.bytes.len());
    assert_ne!(sha256_hex(&other), sha256_hex(&b.bytes));
    std::fs::write(staged(&env), &other).unwrap();
    assert_eq!(
        confirm(
            &srv,
            &cookie(&admin),
            &id_of(&prepared),
            &confirmation(&prepared)
        )
        .status,
        202
    );
    assert_eq!(
        finished(&srv, &admin, &id_of(&prepared))["result"]["error"],
        "archive_changed"
    );
    assert_eq!(env.ledger(), before);
    eventually("staging deleted", || env.staging().is_empty());

    // Missing: an acquisition failure, nothing published.
    let prepared = prepare(&srv, &admin, &b);
    std::fs::remove_file(staged(&env)).unwrap();
    assert_eq!(
        confirm(
            &srv,
            &cookie(&admin),
            &id_of(&prepared),
            &confirmation(&prepared)
        )
        .status,
        202
    );
    let done = finished(&srv, &admin, &id_of(&prepared));
    assert_eq!(done["result"]["error"], "io_error");
    assert_eq!(done["result"]["publication"], "none");
    assert_eq!(env.ledger(), before);
    srv.stop();
}

#[test]
fn concurrent_confirmations_cleanup_and_status_are_safe() {
    let env = Env::new("confirm-race");
    let b = backup();
    let srv = env.start(quick());
    let admin = claim(&env, &srv);
    let other = browser(&srv, &admin, "Second admin", true);
    let before = env.ledger();
    let restore = &srv.running().plane.restore;
    let wait = Duration::from_secs(10);
    let (gate, started, resume) = CleanupGate::new();
    *restore.hooks.execution_gate.lock().unwrap() = Some(gate);

    // Two simultaneous confirmations of one job: exactly one starts it.
    let a = prepare(&srv, &admin, &b);
    let body = confirmation(&a);
    let barrier = Barrier::new(2);
    let statuses: Vec<u16> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..2)
            .map(|_| {
                s.spawn(|| {
                    barrier.wait();
                    confirm(&srv, &cookie(&admin), &id_of(&a), &body).status
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut sorted = statuses.clone();
    sorted.sort();
    assert_eq!(sorted, vec![202, 409], "{statuses:?}");
    started.recv_timeout(wait).expect("the worker started");

    // While it runs: status is visible, cancellation and expiry cannot touch its archive,
    // and a second prepared job cannot start (its secret is not consumed).
    let g = job(&srv, "GET", &id_of(&a), &cookie(&admin));
    assert_eq!(
        (&g.body["state"], &g.body["usable"]),
        (&json!("executing"), &json!(false))
    );
    let d = job(&srv, "DELETE", &id_of(&a), &cookie(&admin));
    assert_eq!(
        (d.status, d.body["error"].as_str()),
        (409, Some("job_executing"))
    );
    env.advance(15 * 60 * 1000);
    assert_eq!(
        call(&srv.base, "GET", UPLOAD, &cookie(&admin), None).status,
        200
    ); // sweeps
    assert_eq!(
        env.staging().len(),
        1,
        "the executing job's archive is untouched"
    );
    let c = prepare(&srv, &other, &b);
    let busy = confirm(&srv, &cookie(&other), &id_of(&c), &confirmation(&c));
    assert_eq!(
        (busy.status, busy.body["error"].as_str()),
        (429, Some("restore_busy"))
    );
    assert!(restore.restore_in_progress());
    assert_eq!(library_rows(&env.ledger()), library_rows(&before));

    *restore.hooks.execution_gate.lock().unwrap() = None;
    resume.send(()).unwrap();
    assert_eq!(finished(&srv, &admin, &id_of(&a))["state"], "succeeded");
    eventually("the slot is free", || !restore.restore_in_progress());
    // The busy job was left ready and now runs.
    assert_eq!(
        confirm(&srv, &cookie(&other), &id_of(&c), &confirmation(&c)).status,
        202
    );
    assert_eq!(finished(&srv, &other, &id_of(&c))["state"], "succeeded");
    let after = env.ledger();
    preserved(&before, &after);
    assert_eq!(library_rows(&after), library_rows(&before) + 4);
    assert_eq!(
        env.log().matches("event=restore_succeeded").count(),
        2,
        "exactly one execution per job"
    );
    eventually("staging deleted", || env.staging().is_empty());

    // Confirmation racing a cancellation: exactly one of them wins.
    let d = prepare(&srv, &admin, &b);
    let (did, body, barrier) = (id_of(&d), confirmation(&d), Barrier::new(2));
    let (confirmed, cancelled) = std::thread::scope(|s| {
        let c = s.spawn(|| {
            barrier.wait();
            confirm(&srv, &cookie(&admin), &did, &body).status
        });
        let x = s.spawn(|| {
            barrier.wait();
            job(&srv, "DELETE", &did, &cookie(&admin)).status
        });
        (c.join().unwrap(), x.join().unwrap())
    });
    let rows = library_rows(&env.ledger());
    match (confirmed, cancelled) {
        (202, 409) => {
            assert_eq!(finished(&srv, &admin, &id_of(&d))["state"], "succeeded");
            assert_eq!(library_rows(&env.ledger()), library_rows(&after) + 2);
        }
        (404 | 410, 200) => assert_eq!(rows, library_rows(&after), "nothing restored"),
        other => panic!("inconsistent race outcome {other:?}"),
    }
    eventually("staging deleted", || env.staging().is_empty());

    // A client that never reads its 202: the restore still runs exactly once.
    let e = prepare(&srv, &admin, &b);
    let rows = library_rows(&env.ledger());
    let body = confirmation(&e).to_string();
    let mut s = connect(&srv);
    let h = H {
        content_type: Some("application/json"),
        ..cookie(&admin)
    };
    s.write_all(
        headers(
            &srv,
            "POST",
            &format!("{UPLOAD}/{}/confirm", id_of(&e)),
            &h,
            &format!("Content-Length: {}\r\n", body.len()),
        )
        .as_bytes(),
    )
    .unwrap();
    s.write_all(body.as_bytes()).unwrap();
    // Accepted (no longer ready), then the connection is dropped with the 202 unread.
    eventually("the confirmation is accepted", || {
        job(&srv, "GET", &id_of(&e), &cookie(&admin)).body["state"] != "ready"
    });
    drop(s);
    assert_eq!(finished(&srv, &admin, &id_of(&e))["state"], "succeeded");
    assert_eq!(library_rows(&env.ledger()), rows + 2);
    let tag = format!("event=restore_confirmed job_id={}", id_of(&e));
    assert_eq!(env.log().matches(&tag).count(), 1);
    srv.stop();
}

#[test]
fn restore_failures_publish_nothing_and_cleanup_keeps_capacity() {
    let env = Env::new("confirm-fail");
    let b = backup();
    let srv = env.start(quick());
    let admin = claim(&env, &srv);
    let before = env.ledger();
    let store = store_of(&srv);
    let run = |f: &dyn Fn()| -> Value {
        let p = prepare(&srv, &admin, &b);
        f();
        assert_eq!(
            confirm(&srv, &cookie(&admin), &id_of(&p), &confirmation(&p)).status,
            202
        );
        let done = finished(&srv, &admin, &id_of(&p));
        eventually("staging deleted", || env.staging().is_empty());
        done
    };
    let no_destination = |done: &Value| {
        for l in done.get("libraries").and_then(Value::as_array).unwrap() {
            let d = l["destination_library_id"].as_str().unwrap();
            assert!(store.library(d).is_err(), "{d} must not exist");
        }
    };

    // A blob directory cannot be synced: refused before the library transaction.
    let done = run(&|| store.hooks.fail_blob_dir_sync.store(1, Ordering::SeqCst));
    store.hooks.fail_blob_dir_sync.store(0, Ordering::SeqCst);
    assert_eq!(done["result"]["error"], "store_internal");
    assert_eq!(done["result"]["publication"], "none");
    assert_eq!(done["result"]["unindexed_blob_files_possible"], true);
    no_destination(&done);
    assert_eq!(env.ledger().tables, before.tables, "no row at all");

    // The library transaction fails after its first library: all or nothing.
    let done = run(&|| {
        store
            .hooks
            .fail_bootstrap_many_after_libraries
            .store(1, Ordering::SeqCst)
    });
    store
        .hooks
        .fail_bootstrap_many_after_libraries
        .store(0, Ordering::SeqCst);
    assert_eq!(done["result"]["error"], "store_internal");
    assert_eq!(done["result"]["publication"], "none");
    no_destination(&done);
    assert_eq!(env.ledger().tables, before.tables);
    preserved(&before, &env.ledger());

    // A destination identity that appeared after inspection: refused, never replaced.
    let p = prepare(&srv, &admin, &b);
    let dest = p["libraries"][0]["destination_library_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut squatter = testkit::genesis();
    squatter.library_id = LibraryId::parse(&dest).unwrap();
    let squatter = squatter.to_canonical_bytes().unwrap();
    assert!(store.bootstrap(&squatter).unwrap().1);
    let with_squatter = env.ledger();
    assert_eq!(
        confirm(&srv, &cookie(&admin), &id_of(&p), &confirmation(&p)).status,
        202
    );
    let done = finished(&srv, &admin, &id_of(&p));
    assert_eq!(done["result"]["error"], "conflict");
    // The squatter predates the restore: it is never attributed to it.
    assert_eq!(done["result"]["publication"], "none");
    assert!(done["result"].get("libraries_published").is_none());
    assert_eq!(store.genesis(&dest).unwrap(), squatter, "never replaced");
    let other = p["libraries"][1]["destination_library_id"]
        .as_str()
        .unwrap();
    assert!(store.library(other).is_err());
    assert_eq!(env.ledger(), with_squatter);
    eventually("staging deleted", || env.staging().is_empty());

    // COMMIT itself reports a failure: whether the libraries were published cannot be known
    // by the restore, so it says "unknown" (here the transaction did in fact commit).
    let p = prepare(&srv, &admin, &b);
    store
        .hooks
        .fail_bootstrap_many_after_commit
        .store(true, Ordering::SeqCst);
    assert_eq!(
        confirm(&srv, &cookie(&admin), &id_of(&p), &confirmation(&p)).status,
        202
    );
    let done = finished(&srv, &admin, &id_of(&p));
    store
        .hooks
        .fail_bootstrap_many_after_commit
        .store(false, Ordering::SeqCst);
    assert_eq!(done["state"], "failed");
    assert_eq!(done["result"]["error"], "store_commit_uncertain");
    assert_eq!(done["result"]["publication"], "unknown");
    let uncertain = mapping(&p);
    for (_, d) in &uncertain {
        assert!(store.library(d).is_ok(), "the ledger is the authority");
    }
    eventually("staging deleted", || env.staging().is_empty());

    // Success whose staging cannot be deleted: still a success, capacity held until deleted.
    let restore = &srv.running().plane.restore;
    let p = prepare(&srv, &admin, &b);
    restore.hooks.fail_cleanup.store(true, Ordering::SeqCst);
    assert_eq!(
        confirm(&srv, &cookie(&admin), &id_of(&p), &confirmation(&p)).status,
        202
    );
    let done = finished(&srv, &admin, &id_of(&p));
    assert_eq!(
        (&done["state"], &done["cleanup_pending"]),
        (&json!("succeeded"), &json!(true))
    );
    let r = headers_only(&srv, "POST", UPLOAD, &cookie(&admin), 10);
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (429, Some("too_many_jobs"))
    );
    let d = job(&srv, "DELETE", &id_of(&p), &cookie(&admin));
    assert_eq!(
        (d.status, d.body["error"].as_str()),
        (500, Some("cleanup_failed"))
    );
    restore.hooks.fail_cleanup.store(false, Ordering::SeqCst);
    eventually("the sweeper deletes it", || env.staging().is_empty());
    let kept = job(&srv, "GET", &id_of(&p), &cookie(&admin)).body;
    assert_eq!(
        (&kept["state"], &kept["cleanup_pending"]),
        (&json!("succeeded"), &json!(false))
    );
    let restored = mapping(&p);
    check_restored(store, &b, &restored);

    // Reopen: the failed restores left no library; the successful one is durable.
    srv.stop();
    let store = Store::open(&env.dir.join("ledger")).unwrap();
    check_restored(&store, &b, &restored);
    assert_eq!(store.genesis(&dest).unwrap(), squatter);
    // The squatter, the uncertain commit's two libraries and the successful restore's two.
    assert_eq!(
        store.list_libraries().unwrap().len(),
        library_rows(&before) + 1 + 2 + 2
    );
    drop(store);
    preserved(&before, &env.ledger());
}

#[test]
fn shutdown_joins_a_running_restore_before_releasing_the_ledger() {
    let env = Env::new("confirm-shutdown");
    let b = backup();
    let srv = env.start(quick());
    let admin = claim(&env, &srv);
    let before = env.ledger();
    let restore = Arc::clone(&srv.running().plane.restore);
    let (gate, started, resume) = CleanupGate::new();
    *restore.hooks.execution_gate.lock().unwrap() = Some(gate);
    let p = prepare(&srv, &admin, &b);
    assert_eq!(
        confirm(&srv, &cookie(&admin), &id_of(&p), &confirmation(&p)).status,
        202
    );
    started
        .recv_timeout(Duration::from_secs(10))
        .expect("the worker started");
    let staged = env.staging();
    assert_eq!(staged.len(), 1);

    let (done_tx, done_rx) = mpsc::channel();
    let stopper = std::thread::spawn(move || {
        srv.stop();
        done_tx.send(()).unwrap();
    });
    eventually("shutdown began", || restore.shutting_down());
    // Shutdown waits for the paused worker: its staging is intact, the ledger still held.
    assert_eq!(env.staging(), staged);
    assert!(
        Store::open(&env.dir.join("ledger")).is_err(),
        "the ledger is still in use"
    );
    assert!(done_rx.try_recv().is_err(), "shutdown has not completed");
    resume.send(()).unwrap();
    done_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("shutdown completes");
    stopper.join().unwrap();

    // By the time shutdown completed, the restore had finished, recorded and cleaned up, and
    // the ledger was released (even though this test still holds the restore jobs).
    assert!(env.log().contains("event=restore_succeeded"));
    assert!(!restore.restore_in_progress());
    assert!(
        env.staging().is_empty(),
        "the worker deleted its own staging"
    );
    let store = Store::open(&env.dir.join("ledger")).unwrap();
    check_restored(&store, &b, &mapping(&p));
    drop(store);
    preserved(&before, &env.ledger());
}

#[test]
fn a_restore_refused_at_shutdown_publishes_nothing_and_frees_the_slot() {
    let env = Env::new("confirm-refused");
    let b = backup();
    let srv = env.start(quick());
    let admin = claim(&env, &srv);
    let before = env.ledger();
    let restore = &srv.running().plane.restore;
    let p = prepare(&srv, &admin, &b);
    // The confirmation is accepted (secret consumed, slot taken) and its worker registration
    // has begun when shutdown closes the registry: the worker is refused, never run.
    let (gate, started, resume) = CleanupGate::new();
    *restore.hooks.registration_gate.lock().unwrap() = Some(gate);
    let r = std::thread::scope(|s| {
        let c = s.spawn(|| confirm(&srv, &cookie(&admin), &id_of(&p), &confirmation(&p)));
        started
            .recv_timeout(Duration::from_secs(10))
            .expect("the registration began");
        assert!(restore.restore_in_progress(), "the slot was taken");
        restore.join_workers();
        resume.send(()).unwrap();
        c.join().unwrap()
    });
    *restore.hooks.registration_gate.lock().unwrap() = None;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (503, Some("shutting_down"))
    );
    let g = job(&srv, "GET", &id_of(&p), &cookie(&admin)).body;
    assert_eq!(
        (&g["state"], &g["usable"]),
        (&json!("failed"), &json!(false))
    );
    assert_eq!(g["result"]["error"], "shutting_down");
    assert_eq!(g["result"]["publication"], "none");
    assert!(!restore.restore_in_progress(), "the execution slot is free");
    // A replay is refused (the server is shutting down) and the job stays failed.
    let again = confirm(&srv, &cookie(&admin), &id_of(&p), &confirmation(&p));
    assert_eq!(again.status, 503);
    assert_eq!(
        job(&srv, "GET", &id_of(&p), &cookie(&admin)).body["state"],
        "failed"
    );
    assert!(!env.log().contains("event=restore_succeeded"));
    for (_, d) in mapping(&p) {
        assert!(store_of(&srv).library(&d).is_err());
    }
    assert_eq!(env.ledger(), before);
    srv.stop();
}
