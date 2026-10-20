//! Slice 18E.10B: recovery HTTP hardening. Synthetic bearers, loopback sockets only.

use super::{
    ensure_outbox, intent_path, read_intent, recovery_http_env_lock, recovery_http_exchange,
    submit_recovery, write_intent, ExecOutcome, FrozenOp, RecoveryCompleteV1, RecoveryExecHooks,
    RecoveryIntentV1, RecoveryPhase, SubmitTarget, ENVELOPE_VERSION, RECOVERY_COMPLETE_VERSION,
    RECOVERY_INTENT_VERSION,
};
use crate::sync_recovery_policy::LIVE_NON_LOOPBACK_POST_ENABLED;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sync_engine::testkit::{line, nb, pg, replica, sk};
use sync_protocol::{to_canonical_json, LibraryId, OperationId, SyncEnvelopeV1, SyncOperationV1};

const SYN_BEARER: &str = "synthetic-recovery-bearer-18e10b";

fn tmp(label: &str) -> std::path::PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("scribe-http-{label}-{n}"))
}

fn orig(n: u64) -> sync_protocol::SyncStroke {
    line(
        sk(n),
        sk(n),
        (200.0, 200.0 + n as f32),
        (800.0, 200.0 + n as f32),
        4,
        false,
    )
}

struct HitServer {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    bearer: Arc<AtomicBool>,
    body: Arc<AtomicBool>,
    _join: thread::JoinHandle<()>,
}

fn spawn_http<F>(handler: F) -> HitServer
where
    F: Fn(String, Vec<u8>) -> Vec<u8> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(false).unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let bearer = Arc::new(AtomicBool::new(false));
    let body = Arc::new(AtomicBool::new(false));
    let hits_c = hits.clone();
    let bearer_c = bearer.clone();
    let body_c = body.clone();
    let join = thread::spawn(move || {
        let _ = listener.set_nonblocking(false);
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let header = String::from_utf8_lossy(&buf[..idx]);
                            let extra = buf[idx + 4..].to_vec();
                            let cl = header.lines().find_map(|l| {
                                l.split_once(':').and_then(|(k, v)| {
                                    if k.eq_ignore_ascii_case("content-length") {
                                        v.trim().parse::<usize>().ok()
                                    } else {
                                        None
                                    }
                                })
                            });
                            let mut payload = extra;
                            if let Some(need) = cl {
                                while payload.len() < need {
                                    match stream.read(&mut tmp) {
                                        Ok(0) => break,
                                        Ok(n) => payload.extend_from_slice(&tmp[..n]),
                                        Err(_) => break,
                                    }
                                }
                            }
                            hits_c.fetch_add(1, Ordering::SeqCst);
                            if header.to_ascii_lowercase().contains("authorization") {
                                bearer_c.store(true, Ordering::SeqCst);
                            }
                            if !payload.is_empty() {
                                body_c.store(true, Ordering::SeqCst);
                            }
                            let resp = handler(header.into_owned(), payload);
                            let _ = stream.write_all(&resp);
                            let _ = stream.flush();
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    });
    HitServer {
        addr,
        hits,
        bearer,
        body,
        _join: join,
    }
}

fn http_ok_empty_tail() -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\n\r\n{\"operations\":[]}"
        .to_vec()
}

fn target_for(addr: SocketAddr, token: &str) -> SubmitTarget {
    SubmitTarget {
        base_url: format!("http://{addr}"),
        token: token.into(),
    }
}

fn freeze_one(sync: &std::path::Path, library: LibraryId) -> RecoveryIntentV1 {
    let op = SyncOperationV1::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: orig(1),
    };
    fs::create_dir_all(sync).unwrap();
    let env = {
        let e = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: OperationId::from_uuid(uuid::Uuid::now_v7()).unwrap(),
            library_id: library,
            replica_id: replica(1),
            client_seq: 1,
            operation: op,
        };
        e
    };
    let bytes = env.to_canonical_bytes().unwrap();
    let intent = RecoveryIntentV1 {
        v: RECOVERY_INTENT_VERSION,
        transaction_id: uuid::Uuid::now_v7().hyphenated().to_string(),
        phase: RecoveryPhase::SequencesAssigned,
        library_id: library,
        replica_id: replica(1),
        genesis_sha256: "g".into(),
        checkpoint_cursor: 0,
        expected_server_head: 0,
        acked_client_seq: 0,
        next_client_seq_after: 2,
        local_snapshot_sha256: "test".into(),
        identity_sha256: "test".into(),
        plan_fingerprint: "test".into(),
        ops: vec![FrozenOp {
            client_seq: 1,
            operation_id: env.operation_id,
            envelope: String::from_utf8(bytes).unwrap(),
        }],
        accepted_prefix: 0,
        upload_authorized: false,
        halt_reason: None,
        mutation_gen: 0,
        submit_attempted: false,
    };
    write_intent(sync, &intent, &RecoveryExecHooks::default()).unwrap();
    ensure_outbox(sync, &intent.ops).unwrap();
    intent
}

#[test]
fn valid_loopback_get_and_post_reach_listener() {
    let srv = spawn_http(|_, _| http_ok_empty_tail());
    let t = target_for(srv.addr, SYN_BEARER);
    let lib = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    let (s, body) = recovery_http_exchange(
        &t,
        "GET",
        &format!("/v1/libraries/{lib}/operations?after=0&limit=50"),
        None,
    )
    .unwrap();
    assert_eq!(s, 200);
    assert!(body.starts_with(b"{\"operations\""));
    assert!(srv.hits.load(Ordering::SeqCst) >= 1);
}

#[test]
fn non_loopback_is_refused_before_socket() {
    let t = SubmitTarget {
        base_url: "http://8.8.8.8:9".into(),
        token: SYN_BEARER.into(),
    };
    let err =
        recovery_http_exchange(&t, "POST", "/v1/libraries/x/operations", Some(b"{}")).unwrap_err();
    assert!(
        err.contains("not loopback") || err.contains("TRANSPORT"),
        "{err}"
    );
    assert!(!err.contains(SYN_BEARER));
}

#[test]
fn userinfo_fragment_query_and_unexpected_path_are_refused() {
    for url in [
        "http://user:pass@127.0.0.1:9",
        "http://127.0.0.1:9#frag",
        "http://127.0.0.1:9?x=1",
        "http://127.0.0.1:9/v1",
    ] {
        let t = SubmitTarget {
            base_url: url.into(),
            token: SYN_BEARER.into(),
        };
        assert!(
            recovery_http_exchange(&t, "POST", "/v1/libraries/x/operations", Some(b"{}")).is_err(),
            "{url}"
        );
    }
}

#[test]
fn redirect_to_other_port_is_not_followed() {
    let dest = spawn_http(|_, _| http_ok_empty_tail());
    let dest_url = format!("http://{}/v1/libraries/x/operations", dest.addr);
    let dest_hits = dest.hits.clone();
    let dest_bearer = dest.bearer.clone();
    let dest_body = dest.body.clone();
    let src = spawn_http(move |_, _| {
        format!("HTTP/1.1 302 Found\r\nLocation: {dest_url}\r\nContent-Length: 0\r\n\r\n")
            .into_bytes()
    });
    let lib = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    let t = target_for(src.addr, SYN_BEARER);
    let err = recovery_http_exchange(
        &t,
        "POST",
        &format!("/v1/libraries/{lib}/operations"),
        Some(b"{\"envelopes\":[]}"),
    )
    .unwrap_err();
    assert!(err.contains("redirect"), "{err}");
    assert!(!err.contains(SYN_BEARER));
    thread::sleep(Duration::from_millis(50));
    assert_eq!(dest_hits.load(Ordering::SeqCst), 0);
    assert!(!dest_bearer.load(Ordering::SeqCst));
    assert!(!dest_body.load(Ordering::SeqCst));
}

#[test]
fn redirect_same_origin_and_307_are_not_followed() {
    let src = spawn_http(|_, _| {
        b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /v1/libraries/x/operations\r\nContent-Length: 0\r\n\r\n".to_vec()
    });
    let lib = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    let err = recovery_http_exchange(
        &target_for(src.addr, SYN_BEARER),
        "POST",
        &format!("/v1/libraries/{lib}/operations"),
        Some(b"{\"envelopes\":[1]}"),
    )
    .unwrap_err();
    assert!(err.contains("redirect 307"), "{err}");
    assert_eq!(src.hits.load(Ordering::SeqCst), 1);
}

#[test]
fn redirect_location_non_loopback_is_not_contacted() {
    let src = spawn_http(|_, _| {
        b"HTTP/1.1 301 Moved Permanently\r\nLocation: http://8.8.8.8:9/steal\r\nContent-Length: 0\r\n\r\n".to_vec()
    });
    let lib = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    let err = recovery_http_exchange(
        &target_for(src.addr, SYN_BEARER),
        "GET",
        &format!("/v1/libraries/{lib}/operations?after=0&limit=50"),
        None,
    )
    .unwrap_err();
    assert!(err.contains("redirect"), "{err}");
}

#[test]
fn environment_proxy_cannot_reroute_loopback() {
    let _g = recovery_http_env_lock();
    let proxy = spawn_http(|_, _| http_ok_empty_tail());
    let dest = spawn_http(|_, _| http_ok_empty_tail());
    let prev_http = std::env::var("HTTP_PROXY").ok();
    let prev_https = std::env::var("HTTPS_PROXY").ok();
    let prev_all = std::env::var("ALL_PROXY").ok();
    std::env::set_var("HTTP_PROXY", format!("http://{}", proxy.addr));
    std::env::set_var("HTTPS_PROXY", format!("http://{}", proxy.addr));
    std::env::set_var("ALL_PROXY", format!("http://{}", proxy.addr));
    let lib = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    let result = recovery_http_exchange(
        &target_for(dest.addr, SYN_BEARER),
        "GET",
        &format!("/v1/libraries/{lib}/operations?after=0&limit=50"),
        None,
    );
    match prev_http {
        Some(v) => std::env::set_var("HTTP_PROXY", v),
        None => std::env::remove_var("HTTP_PROXY"),
    }
    match prev_https {
        Some(v) => std::env::set_var("HTTPS_PROXY", v),
        None => std::env::remove_var("HTTPS_PROXY"),
    }
    match prev_all {
        Some(v) => std::env::set_var("ALL_PROXY", v),
        None => std::env::remove_var("ALL_PROXY"),
    }
    result.unwrap();
    thread::sleep(Duration::from_millis(30));
    assert_eq!(proxy.hits.load(Ordering::SeqCst), 0);
    assert!(!proxy.bearer.load(Ordering::SeqCst));
    assert!(dest.hits.load(Ordering::SeqCst) >= 1);
}

#[test]
fn connection_refused_is_bounded_and_ambiguous() {
    let t = SubmitTarget {
        base_url: "http://127.0.0.1:1".into(),
        token: SYN_BEARER.into(),
    };
    let lib = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    let start = std::time::Instant::now();
    let err = recovery_http_exchange(
        &t,
        "GET",
        &format!("/v1/libraries/{lib}/operations?after=0&limit=50"),
        None,
    )
    .unwrap_err();
    assert!(err.contains("transport"), "{err}");
    assert!(!err.contains(SYN_BEARER));
    assert!(start.elapsed() < Duration::from_secs(4));
}

#[test]
fn response_timeout_is_bounded_not_rejection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf);
            thread::sleep(Duration::from_secs(8));
        }
    });
    let lib = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    let start = std::time::Instant::now();
    let err = recovery_http_exchange(
        &target_for(addr, SYN_BEARER),
        "POST",
        &format!("/v1/libraries/{lib}/operations"),
        Some(b"{\"envelopes\":[]}"),
    )
    .unwrap_err();
    assert!(err.contains("transport"), "{err}");
    assert!(!err.contains(SYN_BEARER));
    assert!(start.elapsed() < Duration::from_secs(6));
}

#[test]
fn stalled_body_after_headers_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 80\r\n\r\n{");
            let _ = s.flush();
            thread::sleep(Duration::from_secs(8));
        }
    });
    let lib = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    let err = recovery_http_exchange(
        &target_for(addr, SYN_BEARER),
        "GET",
        &format!("/v1/libraries/{lib}/operations?after=0&limit=50"),
        None,
    )
    .unwrap_err();
    assert!(err.contains("transport"), "{err}");
    assert!(!err.contains(SYN_BEARER));
}

#[test]
fn malformed_success_body_is_not_optimistic_accept() {
    let srv = spawn_http(|_, _| {
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\nnotjson"
            .to_vec()
    });
    let sync = tmp("malformed");
    let lib = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    let before = freeze_one(&sync, lib);
    let out = submit_recovery(
        &sync,
        None,
        &target_for(srv.addr, SYN_BEARER),
        &Default::default(),
    );
    match out {
        Ok(ExecOutcome::Progress {
            accepted_prefix, ..
        }) => {
            assert_eq!(accepted_prefix, 0);
        }
        Ok(ExecOutcome::Halted {
            accepted_prefix, ..
        }) => {
            assert_eq!(accepted_prefix, 0);
        }
        Err(e) => {
            assert!(!e.contains(SYN_BEARER), "{e}");
        }
        Ok(other) => panic!("must not complete: {other:?}"),
    }
    let after = read_intent(&sync).unwrap().unwrap();
    assert_eq!(after.ops, before.ops);
    assert_eq!(after.accepted_prefix, 0);
}

#[test]
fn timeout_does_not_remint_or_advance_prefix() {
    let hang = TcpListener::bind("127.0.0.1:0").unwrap();
    let hang_addr = hang.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut s, _)) = hang.accept() {
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf);
            thread::sleep(Duration::from_secs(8));
        }
    });
    let sync = tmp("retry-sync");
    let lib = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    let intent = freeze_one(&sync, lib);
    let first = submit_recovery(
        &sync,
        None,
        &target_for(hang_addr, SYN_BEARER),
        &Default::default(),
    );
    match first {
        Ok(ExecOutcome::Progress {
            accepted_prefix, ..
        })
        | Ok(ExecOutcome::Halted {
            accepted_prefix, ..
        }) => {
            assert_eq!(accepted_prefix, 0);
        }
        Err(e) => {
            assert!(!e.contains(SYN_BEARER), "{e}");
        }
        Ok(other) => panic!("{other:?}"),
    }
    let mid = read_intent(&sync).unwrap().unwrap();
    assert_eq!(mid.ops, intent.ops);
    assert_eq!(mid.ops[0].operation_id, intent.ops[0].operation_id);
    assert_eq!(mid.ops[0].client_seq, intent.ops[0].client_seq);
    assert_eq!(mid.ops[0].envelope, intent.ops[0].envelope);
    assert!(mid.accepted_prefix <= 0 || mid.halt_reason.is_some() || mid.submit_attempted);
}

#[test]
fn wrong_library_and_replica_are_refused() {
    let sync = tmp("wrong-id");
    let lib = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    freeze_one(&sync, lib);
    let other = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    let st = crate::sync_outbound::SyncStateV1 {
        v: 2,
        library_id: other,
        replica_id: replica(1),
        genesis_sha256: sync_protocol::sha256_hex(b"g"),
        genesis_uploaded: false,
        acked_client_seq: 0,
        next_client_seq: 2,
        server_base_url: None,
        server_cursor: 0,
        server_id: None,
    };
    fs::write(
        sync.join(crate::sync_outbound::STATE_FILE),
        to_canonical_json(&st).unwrap(),
    )
    .unwrap();
    let srv = spawn_http(|_, _| http_ok_empty_tail());
    let out = submit_recovery(
        &sync,
        None,
        &target_for(srv.addr, SYN_BEARER),
        &Default::default(),
    )
    .unwrap();
    match out {
        ExecOutcome::Halted { reason, .. } => {
            assert!(
                reason.contains("library") || reason.contains("identity mismatch"),
                "{reason}"
            );
            assert!(!reason.contains(SYN_BEARER));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(srv.hits.load(Ordering::SeqCst), 0);
}

#[test]
fn protected_local_root_is_refused() {
    let sync = tmp("prot-sync");
    let lib = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    freeze_one(&sync, lib);
    let srv = spawn_http(|_, _| http_ok_empty_tail());
    let readme = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../README.md");
    let out = submit_recovery(
        &sync,
        Some(&readme),
        &target_for(srv.addr, SYN_BEARER),
        &Default::default(),
    )
    .unwrap();
    match out {
        ExecOutcome::Halted { reason, .. } => {
            assert!(reason.contains("protected"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(srv.hits.load(Ordering::SeqCst), 0);
}

#[test]
fn debug_and_transport_errors_redact_bearer() {
    let t = SubmitTarget {
        base_url: "http://127.0.0.1:1".into(),
        token: SYN_BEARER.into(),
    };
    let dbg = format!("{t:?}");
    assert!(dbg.contains("<redacted>"));
    assert!(!dbg.contains(SYN_BEARER));
    let lib = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
    let err = recovery_http_exchange(
        &t,
        "GET",
        &format!("/v1/libraries/{lib}/operations?after=0&limit=50"),
        None,
    )
    .unwrap_err();
    assert!(!err.contains(SYN_BEARER), "{err}");
    let sync = tmp("intent-redact");
    let intent = freeze_one(&sync, lib);
    let raw = fs::read(intent_path(&sync)).unwrap();
    let text = String::from_utf8(raw).unwrap();
    assert!(!text.contains(SYN_BEARER));
    assert!(!text.contains("token"));
    let complete = RecoveryCompleteV1 {
        v: RECOVERY_COMPLETE_VERSION,
        transaction_id: intent.transaction_id,
        library_id: intent.library_id,
        replica_id: intent.replica_id,
        accepted_ops: 0,
        server_cursor: 0,
        acked_client_seq: 0,
        local_snapshot_sha256: "x".into(),
        identity_sha256: "y".into(),
    };
    let c = serde_json::to_string(&complete).unwrap();
    assert!(!c.contains(SYN_BEARER));
}

#[test]
fn live_non_loopback_constant_stays_false() {
    assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
}
