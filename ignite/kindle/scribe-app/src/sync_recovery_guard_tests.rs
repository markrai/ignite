//! Slice 18E.10I: request-time collected-tunnel HTTP guard. Disposable loopback only.

use super::{
    ensure_outbox, fetch_tail_guarded, read_intent, recovery_http_env_lock, recovery_http_exchange,
    recovery_http_exchange_guarded, release_recovery_hold_with_tunnel_guard, submit_recovery,
    submit_recovery_with_tunnel_guard, write_intent, ExecOutcome, FrozenOp, RecoveryExecHooks,
    RecoveryIntentV1, RecoveryPhase, ReleaseHealth, ReleaseHooks, SubmitTarget, ENVELOPE_VERSION,
    RECOVERY_INTENT_VERSION,
};
use crate::sync_recovery_attest::{
    bind_attestation_to_recovery_policy, missing_tunnel_attestation_blocks_ordinary_authoring,
    DropbearClientEvent, ProcessStartIdentity, SupervisedTunnelIdentity, TunnelObservationSnapshot,
};
use crate::sync_recovery_observe::{
    CollectTunnelObservationRequest, DisposableTunnelHttpGuard, ObservationSourceKind,
    SyntheticFsState, SyntheticProcfs,
};
use crate::sync_recovery_policy::{RecoveryGate, LIVE_NON_LOOPBACK_POST_ENABLED};
use crate::sync_recovery_tunnel::{
    build_dropbear_invocation, fingerprint_of_b64, HostKeyTrustInput, RecoveryTunnelPlan,
    DROPBEAR_CLIENT_APPLET, KINDLE_DROPBEARMULTI,
};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use std::cell::RefCell;
use sync_engine::testkit::{genesis, line, replica, sk};
use sync_protocol::{LibraryId, OperationId, SyncEnvelopeV1, SyncOperationV1};

thread_local! {
    static TAIL_KILL: RefCell<Option<SyntheticProcfs>> = const { RefCell::new(None) };
}

fn kill_tunnel_after_tail() {
    TAIL_KILL.with(|slot| {
        if let Some(fs) = slot.borrow().as_ref() {
            fs.replace_now(SyntheticFsState::default());
        }
    });
}

const SYN_BEARER: &str = "synthetic-recovery-bearer-18e10i";
const PIN_BLOB: &[u8] = b"ssh-ed25519\0synthetic-host-key-blob";

fn b64_encode(data: &[u8]) -> String {
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = if i + 1 < data.len() { data[i + 1] } else { 0 };
        let b2 = if i + 2 < data.len() { data[i + 2] } else { 0 };
        out.push(B64[(b0 >> 2) as usize] as char);
        out.push(B64[(((b0 & 3) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < data.len() {
            out.push(B64[(((b1 & 15) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < data.len() {
            out.push(B64[(b2 & 63) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

fn leaked_plan_host(listen_port: u16) -> (RecoveryTunnelPlan<'static>, HostKeyTrustInput<'static>) {
    let b64 = Box::leak(b64_encode(PIN_BLOB).into_boxed_str());
    let fp = Box::leak(fingerprint_of_b64(b64).unwrap().into_boxed_str());
    let line = Box::leak(format!("ssh.example.test ssh-ed25519 {b64}\n").into_boxed_str());
    let p = RecoveryTunnelPlan {
        executable: KINDLE_DROPBEARMULTI,
        applet: DROPBEAR_CLIENT_APPLET,
        listen_addr: "127.0.0.1",
        listen_port,
        ssh_user: "syncfwd",
        ssh_host: "ssh.example.test",
        ssh_port: 22,
        remote_forward_host: "sync.example.test",
        remote_forward_port: 8080,
        host_key_algorithm: "ssh-ed25519",
        pinned_host_key_sha256: fp,
        identity_file: "/mnt/us/scribe-app/state/recovery-tunnel/id_ed25519",
        known_hosts_home: "/mnt/us/scribe-app/state/recovery-tunnel",
        library_id: "lib-aaaa",
        replica_id: "rep-bbbb",
        genesis_sha256: "gen-cccc",
        transaction_id: "txn-synthetic-1",
    };
    let hk = HostKeyTrustInput {
        known_hosts_text: Some(line),
        expected_hostname: "ssh.example.test",
        expected_ssh_port: 22,
        planned_ssh_port: 22,
        expected_algorithm: "ssh-ed25519",
        expected_key_b64: b64,
        expected_fingerprint_sha256: fp,
        tofu_permitted: false,
        accept_unknown_host: false,
        host_key_verification_disabled: false,
    };
    (p, hk)
}

fn stat_line(pid: u32, state: char, starttime: u64) -> String {
    let rest = "1 ".repeat(18);
    format!("{pid} (dropbearmulti) {state} {rest}{starttime} 0 0 0 0 0 0 0 0 0 0")
}

fn tcp_table(port: u16, inode: u64) -> String {
    format!(
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:{:04X} 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 {inode} 1 0000000000000000 100 0 0 10 0\n",
        port
    )
}

fn live_state(
    pid: u32,
    starttime: u64,
    argv: &[String],
    port: u16,
    inode: u64,
) -> SyntheticFsState {
    let mut st = SyntheticFsState::default();
    st.stat.insert(pid, Ok(stat_line(pid, 'S', starttime)));
    st.exe.insert(pid, Ok(KINDLE_DROPBEARMULTI.to_string()));
    let mut raw = Vec::new();
    for (i, a) in argv.iter().enumerate() {
        if i > 0 {
            raw.push(0);
        }
        raw.extend(a.as_bytes());
    }
    raw.push(0);
    st.cmdline.insert(pid, Ok(raw));
    st.fds.insert(pid, Ok(vec![format!("socket:[{inode}]")]));
    st.tcp = Ok(tcp_table(port, inode));
    st.tcp6 = Ok(None);
    st
}

struct CountingServer {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    bearer: Arc<AtomicBool>,
    body: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    _join: thread::JoinHandle<()>,
}

impl Drop for CountingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn spawn_counting() -> CountingServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let bearer = Arc::new(AtomicBool::new(false));
    let body = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let hits_c = hits.clone();
    let bearer_c = bearer.clone();
    let body_c = body.clone();
    let stop_c = stop.clone();
    let join = thread::spawn(move || {
        while !stop_c.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    loop {
                        match stream.read(&mut tmp) {
                            Ok(0) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    hits_c.fetch_add(1, Ordering::SeqCst);
                    let header = String::from_utf8_lossy(&buf);
                    if header.to_ascii_lowercase().contains("authorization") {
                        bearer_c.store(true, Ordering::SeqCst);
                    }
                    if header.contains("POST") {
                        body_c.store(true, Ordering::SeqCst);
                    }
                    let payload = if header.starts_with("GET") {
                        b"{\"operations\":[]}".as_slice()
                    } else {
                        b"{}".as_slice()
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.write_all(payload);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    CountingServer {
        addr,
        hits,
        bearer,
        body,
        stop,
        _join: join,
    }
}

fn target_for(addr: SocketAddr) -> SubmitTarget {
    SubmitTarget {
        base_url: format!("http://{addr}"),
        token: SYN_BEARER.into(),
    }
}

fn make_guard(
    port: u16,
    procfs: SyntheticProcfs,
    argv: Vec<String>,
    pid: u32,
    starttime: u64,
) -> DisposableTunnelHttpGuard {
    let (plan, hk) = leaked_plan_host(port);
    let sup = SupervisedTunnelIdentity {
        identity: ProcessStartIdentity { pid, starttime },
        generation: 1,
        expected_exe: KINDLE_DROPBEARMULTI.to_string(),
        expected_cmdline: argv,
    };
    DisposableTunnelHttpGuard::from_collected_observation(
        &plan,
        &hk,
        sup,
        ObservationSourceKind::SyntheticFixture,
        Box::new(procfs),
    )
    .unwrap()
}

fn world(port: u16) -> (SyntheticProcfs, Vec<String>, SyntheticFsState) {
    let (plan, _) = leaked_plan_host(port);
    let argv = build_dropbear_invocation(&plan).unwrap().argv;
    let st = live_state(4242, 9001, &argv, port, 7777);
    (SyntheticProcfs::new(st.clone()), argv, st)
}

fn get_path(lib: LibraryId) -> String {
    format!("/v1/libraries/{lib}/operations?after=0&limit=50")
}

fn post_path(lib: LibraryId) -> String {
    format!("/v1/libraries/{lib}/operations")
}

fn lib() -> LibraryId {
    LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap()
}

fn assert_no_network(srv: &CountingServer) {
    thread::sleep(Duration::from_millis(40));
    assert_eq!(srv.hits.load(Ordering::SeqCst), 0, "HTTP request began");
    assert!(
        !srv.bearer.load(Ordering::SeqCst),
        "Authorization transmitted"
    );
    assert!(!srv.body.load(Ordering::SeqCst), "body transmitted");
}

fn refuse_get(
    procfs: SyntheticProcfs,
    argv: Vec<String>,
    pid: u32,
    start: u64,
    srv: &CountingServer,
) {
    let t = target_for(srv.addr);
    let g = make_guard(srv.addr.port(), procfs, argv, pid, start);
    let err = recovery_http_exchange_guarded(&t, "GET", &get_path(lib()), None, g).unwrap_err();
    assert!(
        err.contains("REFUSE TUNNEL") || err.contains("REFUSE HOST KEY"),
        "{err}"
    );
    assert!(!err.contains(SYN_BEARER));
    assert_no_network(srv);
}

fn tmp(label: &str) -> std::path::PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("scribe-guard-{label}-{n}"))
}

fn freeze_one(sync: &std::path::Path, library: LibraryId) -> RecoveryIntentV1 {
    let op = SyncOperationV1::AddStroke {
        notebook_id: sync_engine::testkit::nb(1),
        page_id: sync_engine::testkit::pg(1),
        stroke: line(sk(1), sk(1), (200.0, 200.0), (800.0, 200.0), 4, false),
    };
    std::fs::create_dir_all(sync).unwrap();
    let env = SyncEnvelopeV1 {
        v: ENVELOPE_VERSION,
        operation_id: OperationId::from_uuid(uuid::Uuid::now_v7()).unwrap(),
        library_id: library,
        replica_id: replica(1),
        client_seq: 1,
        operation: op,
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
fn valid_synthetic_attestation_permits_disposable_get() {
    let srv = spawn_counting();
    let (fs, argv, _) = world(srv.addr.port());
    let t = target_for(srv.addr);
    let g = make_guard(srv.addr.port(), fs, argv, 4242, 9001);
    let (s, body) = recovery_http_exchange_guarded(&t, "GET", &get_path(lib()), None, g).unwrap();
    assert_eq!(s, 200);
    assert!(body.starts_with(b"{\"operations\""));
    assert!(srv.hits.load(Ordering::SeqCst) >= 1);
    assert!(srv.bearer.load(Ordering::SeqCst));
}

#[test]
fn valid_synthetic_attestation_permits_disposable_post() {
    let srv = spawn_counting();
    let (fs, argv, _) = world(srv.addr.port());
    let t = target_for(srv.addr);
    let g = make_guard(srv.addr.port(), fs, argv, 4242, 9001);
    let (s, _) = recovery_http_exchange_guarded(
        &t,
        "POST",
        &post_path(lib()),
        Some(b"{\"envelopes\":[]}"),
        g,
    )
    .unwrap();
    assert_eq!(s, 200);
    assert!(srv.hits.load(Ordering::SeqCst) >= 1);
    assert!(srv.body.load(Ordering::SeqCst));
}

#[test]
fn missing_supervised_process_prevents_get() {
    let srv = spawn_counting();
    let (plan, _) = leaked_plan_host(srv.addr.port());
    let argv = build_dropbear_invocation(&plan).unwrap().argv;
    refuse_get(
        SyntheticProcfs::new(SyntheticFsState::default()),
        argv,
        4242,
        9001,
        &srv,
    );
}

#[test]
fn missing_supervised_process_prevents_post() {
    let srv = spawn_counting();
    let (plan, _) = leaked_plan_host(srv.addr.port());
    let argv = build_dropbear_invocation(&plan).unwrap().argv;
    let t = target_for(srv.addr);
    let g = make_guard(
        srv.addr.port(),
        SyntheticProcfs::new(SyntheticFsState::default()),
        argv,
        4242,
        9001,
    );
    let err = recovery_http_exchange_guarded(
        &t,
        "POST",
        &post_path(lib()),
        Some(b"{\"envelopes\":[]}"),
        g,
    )
    .unwrap_err();
    assert!(err.contains("REFUSE TUNNEL"));
    assert!(!err.contains(SYN_BEARER));
    assert_no_network(&srv);
}

#[test]
fn pid_reuse_and_changed_start_time_prevent_request() {
    let srv = spawn_counting();
    let (fs, argv, mut st) = world(srv.addr.port());
    st.stat.insert(4242, Ok(stat_line(4242, 'S', 111)));
    fs.replace_now(st);
    refuse_get(fs, argv, 4242, 9001, &srv);
}

#[test]
fn wrong_executable_prevents_request() {
    let srv = spawn_counting();
    let (fs, argv, mut st) = world(srv.addr.port());
    st.exe.insert(4242, Ok("/bin/false".into()));
    fs.replace_now(st);
    refuse_get(fs, argv, 4242, 9001, &srv);
}

#[test]
fn wrong_dropbear_argv_prevents_request() {
    let srv = spawn_counting();
    let (fs, argv, mut st) = world(srv.addr.port());
    let mut observed = argv.clone();
    observed.pop();
    observed.push("other@host".into());
    let mut raw = Vec::new();
    for (i, a) in observed.iter().enumerate() {
        if i > 0 {
            raw.push(0);
        }
        raw.extend(a.as_bytes());
    }
    st.cmdline.insert(4242, Ok(raw));
    fs.replace_now(st);
    refuse_get(fs, argv, 4242, 9001, &srv);
}

#[test]
fn missing_loopback_listener_prevents_request() {
    let srv = spawn_counting();
    let (fs, argv, mut st) = world(srv.addr.port());
    st.tcp = Ok("  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n".into());
    fs.replace_now(st);
    refuse_get(fs, argv, 4242, 9001, &srv);
}

#[test]
fn listener_owned_by_another_process_prevents_request() {
    let srv = spawn_counting();
    let (fs, argv, mut st) = world(srv.addr.port());
    st.fds.insert(4242, Ok(vec!["socket:[1]".into()]));
    st.fds.insert(99, Ok(vec!["socket:[7777]".into()]));
    fs.replace_now(st);
    refuse_get(fs, argv, 4242, 9001, &srv);
}

#[test]
fn changed_socket_inode_and_missing_fd_prevent_request() {
    let srv = spawn_counting();
    let (fs, argv, mut st) = world(srv.addr.port());
    st.tcp = Ok(tcp_table(srv.addr.port(), 9999));
    fs.replace_now(st.clone());
    refuse_get(fs.clone(), argv.clone(), 4242, 9001, &srv);
    let srv2 = spawn_counting();
    let (fs2, argv2, mut st2) = world(srv2.addr.port());
    st2.fds.insert(4242, Ok(vec!["pipe:[1]".into()]));
    fs2.replace_now(st2);
    refuse_get(fs2, argv2, 4242, 9001, &srv2);
}

#[test]
fn changed_tunnel_generation_prevents_later_request() {
    let srv = spawn_counting();
    let (fs, argv, _) = world(srv.addr.port());
    let origin = format!("http://127.0.0.1:{}", srv.addr.port());
    let g = make_guard(srv.addr.port(), fs, argv, 4242, 9001);
    g.require_fresh_before_http(&origin).unwrap();
    g.test_set_generation(2);
    let err = g.require_fresh_before_http(&origin).unwrap_err();
    assert!(
        err.iter()
            .any(|e| e.contains("stale attestation") || e.contains("REFUSE TUNNEL")),
        "{err:?}"
    );
    assert_no_network(&srv);
}

#[test]
fn invalid_host_key_trust_prevents_request() {
    let srv = spawn_counting();
    let (fs, argv, _) = world(srv.addr.port());
    let (mut plan, mut hk) = leaked_plan_host(srv.addr.port());
    let fp = Box::leak("0".repeat(64).into_boxed_str());
    plan.pinned_host_key_sha256 = fp;
    hk.expected_fingerprint_sha256 = fp;
    let sup = SupervisedTunnelIdentity {
        identity: ProcessStartIdentity {
            pid: 4242,
            starttime: 9001,
        },
        generation: 1,
        expected_exe: KINDLE_DROPBEARMULTI.to_string(),
        expected_cmdline: argv,
    };
    let g = DisposableTunnelHttpGuard::from_collected_observation(
        &plan,
        &hk,
        sup,
        ObservationSourceKind::SyntheticFixture,
        Box::new(fs),
    )
    .unwrap();
    let err =
        recovery_http_exchange_guarded(&target_for(srv.addr), "GET", &get_path(lib()), None, g)
            .unwrap_err();
    assert!(err.contains("REFUSE HOST KEY") || err.contains("REFUSE TUNNEL"));
    assert!(!err.contains(SYN_BEARER));
    assert_no_network(&srv);
}

#[test]
fn unguarded_loopback_path_is_separate_from_collected_tunnel() {
    let srv = spawn_counting();
    let t = target_for(srv.addr);
    let (s, _) = recovery_http_exchange(&t, "GET", &get_path(lib()), None).unwrap();
    assert_eq!(s, 200);
    assert!(srv.hits.load(Ordering::SeqCst) >= 1);
}

#[test]
fn prior_evidence_cannot_authorize_after_process_exit() {
    let srv = spawn_counting();
    let (fs, argv, _) = world(srv.addr.port());
    let t = target_for(srv.addr);
    let g = make_guard(srv.addr.port(), fs.clone(), argv, 4242, 9001);
    let (s, _) = recovery_http_exchange_guarded(&t, "GET", &get_path(lib()), None, g).unwrap();
    assert_eq!(s, 200);
    let hits_after_ok = srv.hits.load(Ordering::SeqCst);
    fs.replace_now(SyntheticFsState::default());
    let g2 = make_guard(
        srv.addr.port(),
        fs,
        {
            let (plan, _) = leaked_plan_host(srv.addr.port());
            build_dropbear_invocation(&plan).unwrap().argv
        },
        4242,
        9001,
    );
    let err = recovery_http_exchange_guarded(&t, "GET", &get_path(lib()), None, g2).unwrap_err();
    assert!(err.contains("REFUSE TUNNEL"));
    thread::sleep(Duration::from_millis(40));
    assert_eq!(srv.hits.load(Ordering::SeqCst), hits_after_ok);
}

#[test]
fn wrong_http_origin_is_refused() {
    let srv = spawn_counting();
    let (fs, argv, _) = world(srv.addr.port());
    let t = SubmitTarget {
        base_url: "http://127.0.0.1:9".into(),
        token: SYN_BEARER.into(),
    };
    let g = make_guard(srv.addr.port(), fs, argv, 4242, 9001);
    let err = recovery_http_exchange_guarded(&t, "GET", &get_path(lib()), None, g).unwrap_err();
    assert!(
        err.contains("does not match") || err.contains("REFUSE TUNNEL") || err.contains("origin")
    );
    assert!(!err.contains(SYN_BEARER));
    assert_no_network(&srv);
}

fn spawn_once(
    status_line: &'static str,
    extra_headers: String,
    payload: &'static [u8],
) -> CountingServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let bearer = Arc::new(AtomicBool::new(false));
    let body = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let hits_c = hits.clone();
    let bearer_c = bearer.clone();
    let body_c = body.clone();
    let stop_c = stop.clone();
    let join = thread::spawn(move || {
        let deadline = SystemTime::now() + Duration::from_secs(4);
        while !stop_c.load(Ordering::SeqCst) && SystemTime::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    loop {
                        match stream.read(&mut tmp) {
                            Ok(0) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    hits_c.fetch_add(1, Ordering::SeqCst);
                    let header = String::from_utf8_lossy(&buf);
                    if header.to_ascii_lowercase().contains("authorization") {
                        bearer_c.store(true, Ordering::SeqCst);
                    }
                    if header.contains("POST") {
                        body_c.store(true, Ordering::SeqCst);
                    }
                    let resp = format!(
                        "{status_line}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.write_all(payload);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    CountingServer {
        addr,
        hits,
        bearer,
        body,
        stop,
        _join: join,
    }
}

#[test]
fn redirect_remains_blocked_on_guarded_path() {
    let dest = spawn_counting();
    let dest_url = format!("http://{}/v1/libraries/x/operations", dest.addr);
    let src = spawn_once(
        "HTTP/1.1 302 Found",
        format!("Location: {dest_url}\r\n"),
        b"",
    );
    let (fs, argv, _) = world(src.addr.port());
    let err = recovery_http_exchange_guarded(
        &target_for(src.addr),
        "POST",
        &post_path(lib()),
        Some(b"{\"envelopes\":[]}"),
        make_guard(src.addr.port(), fs, argv, 4242, 9001),
    )
    .unwrap_err();
    assert!(err.contains("redirect"), "{err}");
    assert!(!err.contains(SYN_BEARER));
    thread::sleep(Duration::from_millis(40));
    assert_eq!(dest.hits.load(Ordering::SeqCst), 0);
    assert!(!dest.bearer.load(Ordering::SeqCst));
    assert!(!dest.body.load(Ordering::SeqCst));
    assert_eq!(src.hits.load(Ordering::SeqCst), 1);
}

#[test]
fn environment_proxy_cannot_intercept_guarded_request() {
    let dest = spawn_counting();
    let proxy = spawn_counting();
    let (fs, argv, _) = world(dest.addr.port());
    let _g = recovery_http_env_lock();
    let prev_http = std::env::var("HTTP_PROXY").ok();
    std::env::set_var("HTTP_PROXY", format!("http://{}", proxy.addr));
    let result = recovery_http_exchange_guarded(
        &target_for(dest.addr),
        "GET",
        &get_path(lib()),
        None,
        make_guard(dest.addr.port(), fs, argv, 4242, 9001),
    );
    match prev_http {
        Some(v) => std::env::set_var("HTTP_PROXY", v),
        None => std::env::remove_var("HTTP_PROXY"),
    }
    result.unwrap();
    thread::sleep(Duration::from_millis(30));
    assert_eq!(proxy.hits.load(Ordering::SeqCst), 0);
    assert!(dest.hits.load(Ordering::SeqCst) >= 1);
}

#[test]
fn valid_tunnel_evidence_does_not_authorize_gate_d() {
    let srv = spawn_counting();
    let (fs, argv, _) = world(srv.addr.port());
    let (plan, hk) = leaked_plan_host(srv.addr.port());
    let att = crate::sync_recovery_observe::attest_collected_tunnel_runtime(
        &plan,
        &hk,
        &SupervisedTunnelIdentity {
            identity: ProcessStartIdentity {
                pid: 4242,
                starttime: 9001,
            },
            generation: 1,
            expected_exe: KINDLE_DROPBEARMULTI.to_string(),
            expected_cmdline: argv.clone(),
        },
        &CollectTunnelObservationRequest {
            plan: &plan,
            supervised: &SupervisedTunnelIdentity {
                identity: ProcessStartIdentity {
                    pid: 4242,
                    starttime: 9001,
                },
                generation: 1,
                expected_exe: KINDLE_DROPBEARMULTI.to_string(),
                expected_cmdline: argv.clone(),
            },
            expected_listen_addr: plan.listen_addr,
            expected_listen_port: plan.listen_port,
            expected_identity: ProcessStartIdentity {
                pid: 4242,
                starttime: 9001,
            },
            source: ObservationSourceKind::SyntheticFixture,
        },
        &fs,
    );
    let ready = att.ready().unwrap();
    let err = bind_attestation_to_recovery_policy(ready, RecoveryGate::DIrreversibleExecution)
        .unwrap_err();
    assert!(err.iter().any(|e| e.contains("Gate D")));
    assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
    let _ = srv;
}

#[test]
fn hold_release_still_requires_accepted_history() {
    let srv = spawn_counting();
    let (fs, argv, _) = world(srv.addr.port());
    let sync = tmp("rel");
    let library = lib();
    freeze_one(&sync, library);
    let libroot = tmp("rel-lib");
    std::fs::create_dir_all(&libroot).unwrap();
    let err = release_recovery_hold_with_tunnel_guard(
        &sync,
        &libroot,
        &target_for(srv.addr),
        &ReleaseHealth {
            seq_ready: true,
            ..Default::default()
        },
        &ReleaseHooks::default(),
        make_guard(srv.addr.port(), fs, argv, 4242, 9001),
    )
    .unwrap_err();
    assert!(err.contains("REFUSE RELEASE") || err.contains("identity"));
    assert_no_network(&srv);
}

#[test]
fn normal_offline_editing_does_not_collect_tunnel_evidence() {
    assert!(!missing_tunnel_attestation_blocks_ordinary_authoring());
    assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
}

#[test]
fn submit_guard_missing_process_does_not_set_submit_attempted() {
    let srv = spawn_counting();
    let (plan, _) = leaked_plan_host(srv.addr.port());
    let argv = build_dropbear_invocation(&plan).unwrap().argv;
    let sync = tmp("no-proc");
    let library = lib();
    freeze_one(&sync, library);
    let out = submit_recovery_with_tunnel_guard(
        &sync,
        None,
        &target_for(srv.addr),
        &RecoveryExecHooks::default(),
        make_guard(
            srv.addr.port(),
            SyntheticProcfs::new(SyntheticFsState::default()),
            argv,
            4242,
            9001,
        ),
    )
    .unwrap();
    match out {
        ExecOutcome::Halted { .. } => {}
        other => panic!("{other:?}"),
    }
    let intent = read_intent(&sync).unwrap().unwrap();
    assert!(!intent.submit_attempted);
    assert_eq!(intent.accepted_prefix, 0);
    assert_eq!(intent.ops.len(), 1);
    assert!(!intent.ops[0].envelope.is_empty());
    assert_no_network(&srv);
}

#[test]
fn one_accepted_post_then_tunnel_loss_preserves_prefix() {
    use scribe_sync_server::{generate_token, ServerConfig, ServerHandle, Store, TokenVerifier};
    let dir = tmp("srv");
    let token = generate_token().unwrap();
    let store = Store::open(&dir).unwrap();
    let h = ServerHandle::start_with(
        "127.0.0.1:0".parse().unwrap(),
        store,
        ServerConfig {
            auth: Some(TokenVerifier::for_token(&token)),
            web_root: None,
            control: None,
        },
    )
    .unwrap();
    let url = h.base_url();
    let addr: SocketAddr = url.trim_start_matches("http://").parse().unwrap();
    let port = addr.port();
    let (fs, argv, live) = world(port);
    let snap = genesis();
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let bytes = sync_protocol::to_canonical_json(&snap).unwrap();
    let (blob, _) = sync_engine::testkit::blob_background();
    let sha = sync_protocol::sha256_hex(&blob);
    ureq::request("PUT", &format!("{url}/v1/blobs/{sha}"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_bytes(&blob)
        .unwrap();
    ureq::request("POST", &format!("{url}/v1/libraries"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_bytes(&bytes)
        .unwrap();
    let library = snap.library_id;
    let page = snap.notebooks[0].pages[0].page_id;
    let ops = vec![
        SyncOperationV1::AddStroke {
            notebook_id: snap.notebooks[0].notebook_id,
            page_id: page,
            stroke: line(sk(10), sk(10), (1.0, 1.0), (2.0, 2.0), 4, false),
        },
        SyncOperationV1::AddStroke {
            notebook_id: snap.notebooks[0].notebook_id,
            page_id: page,
            stroke: line(sk(11), sk(11), (3.0, 3.0), (4.0, 4.0), 4, false),
        },
    ];
    let sync = tmp("prefix-guard");
    let mut seq = 1u64;
    let mut frozen = Vec::new();
    for operation in ops {
        let env = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: OperationId::from_uuid(uuid::Uuid::now_v7()).unwrap(),
            library_id: library,
            replica_id: replica(1),
            client_seq: seq,
            operation,
        };
        frozen.push(FrozenOp {
            client_seq: seq,
            operation_id: env.operation_id,
            envelope: String::from_utf8(env.to_canonical_bytes().unwrap()).unwrap(),
        });
        seq += 1;
    }
    let intent = RecoveryIntentV1 {
        v: RECOVERY_INTENT_VERSION,
        transaction_id: uuid::Uuid::now_v7().hyphenated().to_string(),
        phase: RecoveryPhase::SequencesAssigned,
        library_id: library,
        replica_id: replica(1),
        genesis_sha256: genesis_sha,
        checkpoint_cursor: 0,
        expected_server_head: 0,
        acked_client_seq: 0,
        next_client_seq_after: seq,
        local_snapshot_sha256: "t".into(),
        identity_sha256: "t".into(),
        plan_fingerprint: "t".into(),
        ops: frozen.clone(),
        accepted_prefix: 0,
        upload_authorized: false,
        halt_reason: None,
        mutation_gen: 0,
        submit_attempted: false,
    };
    write_intent(&sync, &intent, &RecoveryExecHooks::default()).unwrap();
    ensure_outbox(&sync, &intent.ops).unwrap();
    let target = SubmitTarget {
        base_url: url,
        token: token.clone(),
    };
    let first = submit_recovery_with_tunnel_guard(
        &sync,
        None,
        &target,
        &RecoveryExecHooks {
            halt_after_new_accepts: Some(1),
            ..Default::default()
        },
        make_guard(port, fs.clone(), argv.clone(), 4242, 9001),
    )
    .unwrap();
    match first {
        ExecOutcome::Progress {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 1),
        other => panic!("{other:?}"),
    }
    let ids_before = read_intent(&sync).unwrap().unwrap().ops;
    fs.replace_now(SyntheticFsState::default());
    let second = submit_recovery_with_tunnel_guard(
        &sync,
        None,
        &target,
        &RecoveryExecHooks::default(),
        make_guard(port, fs, argv, 4242, 9001),
    )
    .unwrap();
    match second {
        ExecOutcome::Halted {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 1),
        other => panic!("{other:?}"),
    }
    let after = read_intent(&sync).unwrap().unwrap();
    assert_eq!(after.accepted_prefix, 1);
    assert_eq!(after.ops, ids_before);
    assert!(after.submit_attempted);
    assert!(!after.upload_authorized);
    let _ = live;
    h.shutdown();
}

#[test]
fn exact_retry_keeps_frozen_bytes() {
    let sync = tmp("retry");
    let library = lib();
    let intent = freeze_one(&sync, library);
    let bytes = intent.ops[0].envelope.clone();
    let id = intent.ops[0].operation_id;
    let seq = intent.ops[0].client_seq;
    let again = read_intent(&sync).unwrap().unwrap();
    assert_eq!(again.ops[0].envelope, bytes);
    assert_eq!(again.ops[0].operation_id, id);
    assert_eq!(again.ops[0].client_seq, seq);
}

#[test]
fn unguarded_submit_still_works_without_tunnel() {
    let srv = spawn_counting();
    let sync = tmp("unguarded");
    freeze_one(&sync, lib());
    let out = submit_recovery(
        &sync,
        None,
        &target_for(srv.addr),
        &RecoveryExecHooks::default(),
    )
    .unwrap();
    match out {
        ExecOutcome::Complete { accepted_prefix } => assert_eq!(accepted_prefix, 1),
        other => panic!("{other:?}"),
    }
    assert!(srv.hits.load(Ordering::SeqCst) >= 2);
    assert!(srv.bearer.load(Ordering::SeqCst));
}

#[test]
fn localhost_hostname_is_refused() {
    let srv = spawn_counting();
    let (fs, argv, _) = world(srv.addr.port());
    let t = SubmitTarget {
        base_url: format!("http://localhost:{}", srv.addr.port()),
        token: SYN_BEARER.into(),
    };
    let err = recovery_http_exchange_guarded(
        &t,
        "GET",
        &get_path(lib()),
        None,
        make_guard(srv.addr.port(), fs, argv, 4242, 9001),
    )
    .unwrap_err();
    assert!(err.contains("localhost") || err.contains("REFUSE RECOVERY TRANSPORT"));
    assert!(!err.contains(SYN_BEARER));
    assert_no_network(&srv);
}

#[test]
fn caller_asserted_authentication_cannot_permit_guarded_request() {
    let srv = spawn_counting();
    let (plan, _) = leaked_plan_host(srv.addr.port());
    let argv = build_dropbear_invocation(&plan).unwrap().argv;
    let _caller_snapshot = TunnelObservationSnapshot {
        generation: 1,
        process: None,
        listeners: vec![],
        dropbear_event: DropbearClientEvent::RunningQuiet,
    };
    refuse_get(
        SyntheticProcfs::new(SyntheticFsState::default()),
        argv,
        4242,
        9001,
        &srv,
    );
}

#[test]
fn fetch_tail_guarded_requires_fresh_evidence() {
    let srv = spawn_counting();
    let (fs, argv, _) = world(srv.addr.port());
    let rows = fetch_tail_guarded(
        &target_for(srv.addr),
        lib(),
        0,
        make_guard(srv.addr.port(), fs, argv, 4242, 9001),
    )
    .unwrap();
    assert!(rows.is_empty());
    assert!(srv.hits.load(Ordering::SeqCst) >= 1);
}

#[test]
fn get_then_post_invalidation_does_not_transmit_second_request() {
    let srv = spawn_counting();
    let (fs, argv, _) = world(srv.addr.port());
    let sync = tmp("after-tail");
    freeze_one(&sync, lib());
    TAIL_KILL.with(|slot| *slot.borrow_mut() = Some(fs.clone()));
    let out = submit_recovery_with_tunnel_guard(
        &sync,
        None,
        &target_for(srv.addr),
        &RecoveryExecHooks {
            after_tail: Some(kill_tunnel_after_tail),
            ..Default::default()
        },
        make_guard(srv.addr.port(), fs, argv, 4242, 9001),
    )
    .unwrap();
    TAIL_KILL.with(|slot| *slot.borrow_mut() = None);
    match out {
        ExecOutcome::Halted {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 0),
        other => panic!("{other:?}"),
    }
    thread::sleep(Duration::from_millis(40));
    assert_eq!(srv.hits.load(Ordering::SeqCst), 1, "only the tail GET");
    assert!(!srv.body.load(Ordering::SeqCst), "POST body not sent");
    let intent = read_intent(&sync).unwrap().unwrap();
    assert_eq!(intent.accepted_prefix, 0);
    assert_eq!(intent.ops.len(), 1);
    assert!(!intent.ops[0].envelope.is_empty());
    assert!(intent.submit_attempted);
}

#[test]
fn timeout_after_acceptance_uses_authoritative_tail() {
    use scribe_sync_server::{generate_token, ServerConfig, ServerHandle, Store, TokenVerifier};
    let dir = tmp("timeout-srv");
    let token = generate_token().unwrap();
    let store = Store::open(&dir).unwrap();
    let h = ServerHandle::start_with(
        "127.0.0.1:0".parse().unwrap(),
        store,
        ServerConfig {
            auth: Some(TokenVerifier::for_token(&token)),
            web_root: None,
            control: None,
        },
    )
    .unwrap();
    let url = h.base_url();
    let addr: SocketAddr = url.trim_start_matches("http://").parse().unwrap();
    let port = addr.port();
    let (fs, argv, _) = world(port);
    let snap = genesis();
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let bytes = sync_protocol::to_canonical_json(&snap).unwrap();
    let (blob, _) = sync_engine::testkit::blob_background();
    let sha = sync_protocol::sha256_hex(&blob);
    ureq::request("PUT", &format!("{url}/v1/blobs/{sha}"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_bytes(&blob)
        .unwrap();
    ureq::request("POST", &format!("{url}/v1/libraries"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_bytes(&bytes)
        .unwrap();
    let library = snap.library_id;
    let page = snap.notebooks[0].pages[0].page_id;
    let env = SyncEnvelopeV1 {
        v: ENVELOPE_VERSION,
        operation_id: OperationId::from_uuid(uuid::Uuid::now_v7()).unwrap(),
        library_id: library,
        replica_id: replica(1),
        client_seq: 1,
        operation: SyncOperationV1::AddStroke {
            notebook_id: snap.notebooks[0].notebook_id,
            page_id: page,
            stroke: line(sk(20), sk(20), (1.0, 1.0), (2.0, 2.0), 4, false),
        },
    };
    let frozen = FrozenOp {
        client_seq: 1,
        operation_id: env.operation_id,
        envelope: String::from_utf8(env.to_canonical_bytes().unwrap()).unwrap(),
    };
    let bytes_before = frozen.envelope.clone();
    let id_before = frozen.operation_id;
    let sync = tmp("timeout-guard");
    let intent = RecoveryIntentV1 {
        v: RECOVERY_INTENT_VERSION,
        transaction_id: uuid::Uuid::now_v7().hyphenated().to_string(),
        phase: RecoveryPhase::SequencesAssigned,
        library_id: library,
        replica_id: replica(1),
        genesis_sha256: genesis_sha,
        checkpoint_cursor: 0,
        expected_server_head: 0,
        acked_client_seq: 0,
        next_client_seq_after: 2,
        local_snapshot_sha256: "t".into(),
        identity_sha256: "t".into(),
        plan_fingerprint: "t".into(),
        ops: vec![frozen],
        accepted_prefix: 0,
        upload_authorized: false,
        halt_reason: None,
        mutation_gen: 0,
        submit_attempted: false,
    };
    write_intent(&sync, &intent, &RecoveryExecHooks::default()).unwrap();
    ensure_outbox(&sync, &intent.ops).unwrap();
    let out = submit_recovery_with_tunnel_guard(
        &sync,
        None,
        &SubmitTarget {
            base_url: url,
            token: token.clone(),
        },
        &RecoveryExecHooks {
            drop_next_ack: true,
            ..Default::default()
        },
        make_guard(port, fs, argv, 4242, 9001),
    )
    .unwrap();
    match out {
        ExecOutcome::Complete { accepted_prefix } => assert_eq!(accepted_prefix, 1),
        ExecOutcome::Progress {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 1),
        other => panic!("{other:?}"),
    }
    let after = read_intent(&sync).unwrap().unwrap();
    assert_eq!(after.ops[0].envelope, bytes_before);
    assert_eq!(after.ops[0].operation_id, id_before);
    assert_eq!(after.ops[0].client_seq, 1);
    assert_eq!(after.accepted_prefix, 1);
    h.shutdown();
}
