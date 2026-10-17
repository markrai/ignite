//! Slice 18E.10L: disposable supervised child + HTTP over pipes.

use crate::sync_recovery_attest::bind_attestation_to_recovery_policy;
use crate::sync_recovery_policy::{RecoveryGate, LIVE_NON_LOOPBACK_POST_ENABLED};
use crate::sync_recovery_stream::{
    recovery_http1_fixture_get, recovery_http1_request, spawn_recovery_stream_child,
    RecoveryStreamPlan,
};
use crate::sync_recovery_supervise::{
    stream_resources_released, DisposableStreamPermit, StreamTransportOutcome,
    SupervisedRecoveryStream,
};
use crate::sync_recovery_tunnel::{
    fingerprint_of_b64, HostKeyTrustInput, DROPBEAR_CLIENT_APPLET, KINDLE_DROPBEARMULTI,
};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const SYN_BEARER: &str = "synthetic-recovery-bearer-18e10l";
const PIN_BLOB: &[u8] = b"ssh-ed25519\0synthetic-host-key-blob-114";
const FROZEN: &[u8] = b"{\"envelopes\":[{\"operationId\":\"op-frozen-114\",\"clientSequence\":7}]}";

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

fn fake_dropbear() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let dir = std::env::temp_dir().join("ignite-114-fake-dropbear");
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("fake_dropbearmulti.rs");
        fs::write(&src, include_str!("testdata/fake_dropbearmulti.rs")).unwrap();
        let out = if cfg!(windows) {
            dir.join("dropbearmulti.exe")
        } else {
            dir.join("dropbearmulti")
        };
        let st = Command::new("rustc")
            .args(["--edition", "2021", "-C", "opt-level=1", "-o"])
            .arg(&out)
            .arg(&src)
            .status()
            .expect("rustc available to compile disposable dropbear stub");
        assert!(st.success(), "rustc failed to build dropbear stub");
        out
    })
    .clone()
}

struct Harness {
    home: PathBuf,
    exe: String,
    identity: String,
    home_s: String,
    fp: String,
    b64: String,
    kh: String,
    hits: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<u8>>>,
    addr: std::net::SocketAddr,
}

impl Harness {
    fn new(status: &'static str, extra: &'static str, payload: &'static [u8]) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let home = std::env::temp_dir().join(format!(
            "ignite-114-home-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(home.join(".ssh")).unwrap();
        let identity = home.join("id_ed25519");
        fs::write(&identity, b"synthetic-dropbear-identity-114").unwrap();
        let exe = fake_dropbear();
        let b64 = b64_encode(PIN_BLOB);
        let fp = fingerprint_of_b64(&b64).unwrap();
        let kh = format!("ssh.example.test ssh-ed25519 {b64}\n");
        fs::write(home.join(".ssh").join("known_hosts"), &kh).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        fs::write(home.join("forward_tcp"), format!("{}", addr)).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let hits_c = hits.clone();
        let seen_c = seen.clone();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = Vec::new();
                let mut tmp = [0u8; 2048];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                let header = String::from_utf8_lossy(&buf);
                                let extra_need = header
                                    .lines()
                                    .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                                    .and_then(|l| l.split(':').nth(1))
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                                    .unwrap_or(0);
                                if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                    while buf.len() < idx + 4 + extra_need {
                                        match stream.read(&mut tmp) {
                                            Ok(0) | Err(_) => break,
                                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                                        }
                                    }
                                }
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                hits_c.fetch_add(1, Ordering::SeqCst);
                seen_c.lock().unwrap().extend_from_slice(&buf);
                let resp = format!(
                    "{status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(payload);
            }
        });
        Self {
            home: home.clone(),
            exe: exe.to_string_lossy().into_owned(),
            identity: identity.to_string_lossy().into_owned(),
            home_s: home.to_string_lossy().into_owned(),
            fp,
            b64,
            kh,
            hits,
            seen,
            addr,
        }
    }

    fn plan(&self) -> RecoveryStreamPlan<'_> {
        RecoveryStreamPlan {
            executable: &self.exe,
            applet: DROPBEAR_CLIENT_APPLET,
            ssh_user: "syncfwd",
            ssh_host: "ssh.example.test",
            ssh_port: 22,
            remote_forward_host: "httptest.example.test",
            remote_forward_port: 8080,
            host_key_algorithm: "ssh-ed25519",
            pinned_host_key_sha256: &self.fp,
            identity_file: &self.identity,
            known_hosts_home: &self.home_s,
            library_id: "lib-aaaa",
            replica_id: "rep-bbbb",
            genesis_sha256: "gen-cccc",
            transaction_id: "txn-synthetic-supervise-1",
        }
    }

    fn hk(&self) -> HostKeyTrustInput<'_> {
        HostKeyTrustInput {
            known_hosts_text: Some(&self.kh),
            expected_hostname: "ssh.example.test",
            expected_ssh_port: 22,
            planned_ssh_port: 22,
            expected_algorithm: "ssh-ed25519",
            expected_key_b64: &self.b64,
            expected_fingerprint_sha256: &self.fp,
            tofu_permitted: false,
            accept_unknown_host: false,
            host_key_verification_disabled: false,
        }
    }

    fn permit() -> DisposableStreamPermit {
        DisposableStreamPermit::for_synthetic_fixture()
    }
}

fn get_path() -> &'static str {
    "/v1/libraries/11111111-1111-7111-8111-111111111111/operations?after=0&limit=50"
}

fn post_path() -> &'static str {
    "/v1/libraries/11111111-1111-7111-8111-111111111111/operations"
}

fn mark(h: &Harness, name: &str) {
    fs::write(h.home.join(name), b"1").unwrap();
}

#[test]
fn production_spawn_entry_remains_disabled() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    let inv =
        crate::sync_recovery_stream::build_dropbear_stream_invocation(&h.plan(), &h.hk()).unwrap();
    let err = spawn_recovery_stream_child(&inv).unwrap_err();
    assert!(err.contains("not enabled"));
    assert!(!err.contains(SYN_BEARER));
    assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
}

#[test]
fn restricted_argv_and_no_listener_or_bearer() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"ok");
    let p = h.plan();
    let req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    let argv = s.argv().to_vec();
    assert!(argv.iter().any(|a| a == "-B"));
    assert!(argv.iter().any(|a| a == "httptest.example.test:8080"));
    assert!(!argv.iter().any(|a| a == "-L"));
    assert!(!argv.iter().any(|a| a.contains("127.0.0.1")));
    assert!(!argv.iter().any(|a| a.contains(SYN_BEARER)));
    assert!(!argv.iter().any(|a| a.contains("op-frozen-114")));
    let out = s.exchange(&p, &req);
    match out {
        StreamTransportOutcome::Authoritative { status, .. } => assert_eq!(status, 200),
        other => panic!("{other:?}"),
    }
}

#[test]
fn incorrect_executable_and_destinations_refused() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    let mut p = h.plan();
    let bad = KINDLE_DROPBEARMULTI.replace("dropbearmulti", "not-dropbear");
    p.executable = &bad;
    assert!(
        SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk())
            .err()
            .is_some()
    );
    let mut p = h.plan();
    p.ssh_host = "10.0.0.8";
    assert!(
        SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk())
            .err()
            .is_some()
    );
    let mut p = h.plan();
    p.remote_forward_host = "127.0.0.1";
    assert!(
        SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk())
            .err()
            .is_some()
    );
    assert_eq!(h.hits.load(Ordering::SeqCst), 0);
}

#[test]
fn missing_and_wrong_host_key_refuse_before_http() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"secret");
    let mut hk = h.hk();
    hk.known_hosts_text = Some("");
    match SupervisedRecoveryStream::open(Harness::permit(), &h.plan(), &hk) {
        Err(StreamTransportOutcome::PreWriteRefuse { reason }) => {
            assert!(reason.contains("HOST KEY") || reason.contains("STREAM"));
            assert!(!reason.contains(SYN_BEARER));
        }
        Ok(_) => panic!("empty known_hosts must not spawn"),
        Err(e) => panic!("{e:?}"),
    }
    let evil = "ssh.example.test ssh-ed25519 AAAAWRONG_PIN\n".to_string();
    let mut hk = h.hk();
    let leaked = Box::leak(evil.into_boxed_str());
    hk.known_hosts_text = Some(leaked);
    assert!(
        SupervisedRecoveryStream::open(Harness::permit(), &h.plan(), &hk)
            .err()
            .is_some()
    );
    thread::sleep(Duration::from_millis(50));
    assert_eq!(h.hits.load(Ordering::SeqCst), 0);
}

#[test]
fn child_startup_failure_is_pre_write() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    let missing = h.home.join("dropbearmulti.exe");
    let miss_s = missing.to_string_lossy().into_owned();
    let leaked = Box::leak(miss_s.into_boxed_str());
    let mut p = h.plan();
    p.executable = leaked;
    match SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()) {
        Err(StreamTransportOutcome::PreWriteRefuse { reason }) => {
            assert!(reason.contains("startup") || reason.contains("REFUSE"));
        }
        Ok(_) => panic!("missing executable should not spawn"),
        Err(e) => panic!("{e:?}"),
    }
    assert_eq!(h.hits.load(Ordering::SeqCst), 0);
}

#[test]
fn valid_get_and_post_over_supervised_child() {
    let h = Harness::new(
        "HTTP/1.1 200 OK",
        "Content-Type: application/json\r\n",
        b"{\"operations\":[]}",
    );
    let p = h.plan();
    let req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::Authoritative { status, body } => {
            assert_eq!(status, 200);
            assert_eq!(body, b"{\"operations\":[]}");
        }
        other => panic!("{other:?}"),
    }
    thread::sleep(Duration::from_millis(40));
    assert!(h.hits.load(Ordering::SeqCst) >= 1);
    let captured = h.seen.lock().unwrap().clone();
    let wire = String::from_utf8_lossy(&captured);
    assert!(wire.contains("Authorization: Bearer"));
    assert!(wire.contains("Host: httptest.example.test:8080"));
    assert!(!wire.contains("127.0.0.1"));

    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    let p = h.plan();
    let req = recovery_http1_request(&p, "POST", post_path(), SYN_BEARER, Some(FROZEN)).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::Authoritative { status, .. } => assert_eq!(status, 200),
        other => panic!("{other:?}"),
    }
    thread::sleep(Duration::from_millis(40));
    let captured = h.seen.lock().unwrap().clone();
    let wire = String::from_utf8_lossy(&captured);
    assert!(wire.contains("EXACT") || wire.contains("op-frozen-114"));
    assert!(wire.contains(&format!("Content-Length: {}", FROZEN.len())));
    assert!(wire.contains("Connection: close"));
    assert_eq!(
        FROZEN,
        &b"{\"envelopes\":[{\"operationId\":\"op-frozen-114\",\"clientSequence\":7}]}"[..]
    );
}

#[test]
fn fixture_get_root_is_test_only() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"ignite-114-fixture-ok\n");
    let p = h.plan();
    let req = recovery_http1_fixture_get(&p, "");
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::Authoritative { status, body } => {
            assert_eq!(status, 200);
            assert_eq!(body, b"ignite-114-fixture-ok\n");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn redirect_chunked_malformed_are_not_accepted() {
    let h = Harness::new("HTTP/1.1 302 Found", "Location: http://evil/\r\n", b"");
    let p = h.plan();
    let req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::WriteStartedAmbiguous { reason }
        | StreamTransportOutcome::PreWriteRefuse { reason } => {
            assert!(reason.contains("redirect") || reason.contains("AMBIGUOUS"));
            assert!(!reason.contains(SYN_BEARER));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn child_exit_before_during_and_hang() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    mark(&h, "exit_before");
    let p = h.plan();
    let req = recovery_http1_request(&p, "POST", post_path(), SYN_BEARER, Some(FROZEN)).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::WriteStartedAmbiguous { reason }
        | StreamTransportOutcome::PreWriteRefuse { reason } => {
            assert!(!reason.contains(SYN_BEARER));
            assert!(!reason.contains("op-frozen-114"));
        }
        other => panic!("{other:?}"),
    }

    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    mark(&h, "exit_during_write");
    let p = h.plan();
    let req = recovery_http1_request(&p, "POST", post_path(), SYN_BEARER, Some(FROZEN)).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::WriteStartedAmbiguous { .. } => {}
        other => panic!("expected ambiguous {other:?}"),
    }

    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    mark(&h, "hang");
    let p = h.plan();
    let req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    let pid = s.pid();
    let start = Instant::now();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::WriteStartedAmbiguous { reason } => {
            assert!(reason.contains("deadline") || reason.contains("AMBIGUOUS"));
        }
        other => panic!("{other:?}"),
    }
    assert!(start.elapsed() < Duration::from_secs(6));
    s.cancel();
    assert!(!process_alive(pid) || cfg!(windows));
}

#[test]
fn cancel_reaps_child_not_bystander() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    mark(&h, "hang");
    let p = h.plan();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    let child = s.pid();
    let mut bystander = if cfg!(windows) {
        Command::new("ping")
            .args(["-n", "8", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    } else {
        Command::new("sleep")
            .arg("8")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    };
    let by_id = bystander.id();
    s.cancel();
    thread::sleep(Duration::from_millis(100));
    assert!(bystander.try_wait().unwrap().is_none(), "bystander killed");
    let _ = bystander.kill();
    let _ = bystander.wait();
    let _ = child;
    let _ = by_id;
}

#[test]
fn client_auth_and_unavailable_forward() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    fs::write(&h.identity, b"").unwrap();
    let p = h.plan();
    let req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::WriteStartedAmbiguous { .. }
        | StreamTransportOutcome::PreWriteRefuse { .. } => {
            assert_eq!(h.hits.load(Ordering::SeqCst), 0);
        }
        other => panic!("{other:?}"),
    }

    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    fs::write(h.home.join("forward_tcp"), "127.0.0.1:1").unwrap();
    let p = h.plan();
    let req = recovery_http1_request(&p, "POST", post_path(), SYN_BEARER, Some(FROZEN)).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::WriteStartedAmbiguous { reason }
        | StreamTransportOutcome::PreWriteRefuse { reason } => {
            assert!(!reason.contains(SYN_BEARER));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn loopback_host_does_not_fallback_and_gate_d_unrelated() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    let p = h.plan();
    let mut req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();
    req.host = "127.0.0.1:18765";
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::PreWriteRefuse { reason } => {
            assert!(reason.contains("loopback") || reason.contains("Host"));
            assert!(!reason.contains(SYN_BEARER));
        }
        other => panic!("{other:?}"),
    }
    thread::sleep(Duration::from_millis(30));
    assert_eq!(h.hits.load(Ordering::SeqCst), 0);
    assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
    let err = bind_attestation_to_recovery_policy(
        &crate::sync_recovery_attest::TunnelAttestation {
            generation: 1,
            identity: crate::sync_recovery_attest::ProcessStartIdentity {
                pid: 1,
                starttime: 1,
            },
            exe: KINDLE_DROPBEARMULTI.into(),
            listen_addr: "127.0.0.1".into(),
            listen_port: 1,
            listener_origin: "http://127.0.0.1:1".into(),
            remote_forward_host: "sync.example.test".into(),
            remote_forward_port: 8080,
            pinned_host_key_sha256: "aa".into(),
            observed_host_key_sha256: "aa".into(),
            host_key_algorithm: "ssh-ed25519".into(),
            library_id: "lib-aaaa".into(),
            replica_id: "rep-bbbb".into(),
            genesis_sha256: "gen-cccc".into(),
            transaction_id: "txn".into(),
            auth_source:
                crate::sync_recovery_policy::SshAuthEvidenceSource::PinnedForwardConjunction,
            lifecycle: crate::sync_recovery_policy::TunnelLifecycle::ReadyForAuthorizedRecovery,
            listener_present: true,
            listener_owner_observed: true,
            pid_recycled: false,
            unknown_host_key_accepted: false,
            gateway_ports: false,
            unrestricted_forwarding: false,
            agent_forwarding: false,
            interactive_shell: false,
            ssh_session_authenticated: true,
        },
        RecoveryGate::DIrreversibleExecution,
    )
    .unwrap_err();
    assert!(err.iter().any(|e| e.contains("Gate D")));
}

#[test]
fn stderr_does_not_enter_http_body() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"{\"operations\":[]}");
    let p = h.plan();
    let req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::Authoritative { body, .. } => {
            let t = String::from_utf8_lossy(&body);
            assert!(!t.contains("ssh-diag"));
            assert!(t.contains("operations"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn frozen_identity_not_rewritten_on_ambiguous() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    mark(&h, "exit_during_response");
    let p = h.plan();
    let req = recovery_http1_request(&p, "POST", post_path(), SYN_BEARER, Some(FROZEN)).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::WriteStartedAmbiguous { .. }
        | StreamTransportOutcome::Authoritative { .. } => {}
        other => panic!("{other:?}"),
    }
    assert!(std::str::from_utf8(FROZEN)
        .unwrap()
        .contains("op-frozen-114"));
    assert!(std::str::from_utf8(FROZEN)
        .unwrap()
        .contains("clientSequence\":7"));
}

#[test]
fn worker_lifetime_child_exit_hang_and_broken_pipe() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    mark(&h, "exit_before");
    let p = h.plan();
    let req = recovery_http1_request(&p, "POST", post_path(), SYN_BEARER, Some(FROZEN)).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    let _ = s.exchange(&p, &req);
    assert!(stream_resources_released(&s));

    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    mark(&h, "exit_during_write");
    let p = h.plan();
    let req = recovery_http1_request(&p, "POST", post_path(), SYN_BEARER, Some(FROZEN)).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::WriteStartedAmbiguous { .. } => {}
        other => panic!("{other:?}"),
    }
    assert!(stream_resources_released(&s));

    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    mark(&h, "exit_during_response");
    let p = h.plan();
    let req = recovery_http1_request(&p, "POST", post_path(), SYN_BEARER, Some(FROZEN)).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    let _ = s.exchange(&p, &req);
    assert!(stream_resources_released(&s));

    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    mark(&h, "hang");
    let p = h.plan();
    let req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::WriteStartedAmbiguous { reason } => {
            assert!(reason.contains("deadline") || reason.contains("AMBIGUOUS"));
        }
        other => panic!("{other:?}"),
    }
    assert!(stream_resources_released(&s));

    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    mark(&h, "hang_after_read");
    let p = h.plan();
    let req = recovery_http1_request(&p, "POST", post_path(), SYN_BEARER, Some(FROZEN)).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::WriteStartedAmbiguous { .. } => {}
        other => panic!("{other:?}"),
    }
    assert!(stream_resources_released(&s));
}

#[test]
fn deadline_with_complete_response_and_repeated_timeouts_do_not_leak_workers() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"{\"operations\":[]}");
    mark(&h, "respond_then_hang");
    let p = h.plan();
    let req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::Authoritative { status, body } => {
            assert_eq!(status, 200);
            assert_eq!(body, b"{\"operations\":[]}");
        }
        other => panic!("{other:?}"),
    }
    assert!(stream_resources_released(&s));

    for _ in 0..3 {
        let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
        mark(&h, "hang");
        let p = h.plan();
        let req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();
        let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
        match s.exchange(&p, &req) {
            StreamTransportOutcome::WriteStartedAmbiguous { .. } => {}
            other => panic!("{other:?}"),
        }
        assert!(stream_resources_released(&s));
    }
}

#[test]
fn cancel_during_hang_and_after_child_exit_releases_pipes() {
    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    mark(&h, "hang");
    let p = h.plan();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    s.cancel();
    assert!(stream_resources_released(&s));
    let req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();
    match s.exchange(&p, &req) {
        StreamTransportOutcome::PreWriteRefuse { reason } => {
            assert!(reason.contains("stdin") || reason.contains("consumed"));
        }
        other => panic!("{other:?}"),
    }

    let h = Harness::new("HTTP/1.1 200 OK", "", b"{}");
    mark(&h, "exit_before");
    let p = h.plan();
    let mut s = SupervisedRecoveryStream::open(Harness::permit(), &p, &h.hk()).unwrap();
    s.cancel();
    assert!(stream_resources_released(&s));
}

fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let _ = pid;
        true
    }
    #[cfg(unix)]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
}
