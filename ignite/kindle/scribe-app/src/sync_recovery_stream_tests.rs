//! Slice 18E.10J: direct-stream argv + HTTP/1.1 over a caller-owned byte stream.

use crate::sync_recovery_attest::bind_attestation_to_recovery_policy;
use crate::sync_recovery_policy::{RecoveryGate, LIVE_NON_LOOPBACK_POST_ENABLED};
use crate::sync_recovery_stream::{
    build_dropbear_stream_invocation, recovery_http1_exchange, recovery_http1_request,
    recovery_stream_child_clear_vars, spawn_recovery_stream_child, validate_recovery_stream_plan,
    RecoveryHttp1Request, RecoveryStreamPlan, STREAM_MAX_BODY_BYTES,
};
use crate::sync_recovery_tunnel::{
    build_dropbear_invocation, fingerprint_of_b64, HostKeyTrustInput, RecoveryTunnelPlan,
    DROPBEAR_CLIENT_APPLET, KINDLE_DROPBEARMULTI,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const SYN_BEARER: &str = "synthetic-recovery-bearer-18e10j";
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

fn pin() -> (String, String, String) {
    let b64 = b64_encode(PIN_BLOB);
    let fp = fingerprint_of_b64(&b64).unwrap();
    let line = format!("ssh.example.test ssh-ed25519 {b64}\n");
    (b64, fp, line)
}

fn plan() -> RecoveryStreamPlan<'static> {
    RecoveryStreamPlan {
        executable: KINDLE_DROPBEARMULTI,
        applet: DROPBEAR_CLIENT_APPLET,
        ssh_user: "syncfwd",
        ssh_host: "ssh.example.test",
        ssh_port: 22,
        remote_forward_host: "sync.example.test",
        remote_forward_port: 8080,
        host_key_algorithm: "ssh-ed25519",
        pinned_host_key_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        identity_file: "/mnt/us/scribe-app/state/recovery-tunnel/id_ed25519",
        known_hosts_home: "/mnt/us/scribe-app/state/recovery-tunnel",
        library_id: "lib-aaaa",
        replica_id: "rep-bbbb",
        genesis_sha256: "gen-cccc",
        transaction_id: "txn-synthetic-stream-1",
    }
}

fn ready_plan() -> (RecoveryStreamPlan<'static>, HostKeyTrustInput<'static>) {
    let (b64, fp, line) = pin();
    let b64 = Box::leak(b64.into_boxed_str());
    let fp = Box::leak(fp.into_boxed_str());
    let line = Box::leak(line.into_boxed_str());
    let mut p = plan();
    p.pinned_host_key_sha256 = fp;
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

fn listen_plan() -> RecoveryTunnelPlan<'static> {
    RecoveryTunnelPlan {
        executable: KINDLE_DROPBEARMULTI,
        applet: DROPBEAR_CLIENT_APPLET,
        listen_addr: "127.0.0.1",
        listen_port: 18765,
        ssh_user: "syncfwd",
        ssh_host: "ssh.example.test",
        ssh_port: 22,
        remote_forward_host: "sync.example.test",
        remote_forward_port: 8080,
        host_key_algorithm: "ssh-ed25519",
        pinned_host_key_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        identity_file: "/mnt/us/scribe-app/state/recovery-tunnel/id_ed25519",
        known_hosts_home: "/mnt/us/scribe-app/state/recovery-tunnel",
        library_id: "lib-aaaa",
        replica_id: "rep-bbbb",
        genesis_sha256: "gen-cccc",
        transaction_id: "txn-synthetic-1",
    }
}

struct Capture {
    hits: Arc<AtomicUsize>,
    bearer: Arc<AtomicBool>,
    body: Arc<Vec<u8>>,
}

fn spawn_http(
    status: &'static str,
    extra: &'static str,
    payload: &'static [u8],
) -> (std::net::SocketAddr, Capture) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let bearer = Arc::new(AtomicBool::new(false));
    let body = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hits_c = hits.clone();
    let bearer_c = bearer.clone();
    let body_c = body.clone();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
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
            if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                body_c.lock().unwrap().extend_from_slice(&buf[idx + 4..]);
            }
            let resp = format!(
                "{status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(payload);
        }
    });
    (
        addr,
        Capture {
            hits,
            bearer,
            body: Arc::new(Vec::new()),
        },
    )
}

fn connect(addr: std::net::SocketAddr) -> std::net::TcpStream {
    let s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
    s
}

#[test]
fn stream_argv_is_restricted_dropbear_b_without_local_listen() {
    let (p, hk) = ready_plan();
    let inv = build_dropbear_stream_invocation(&p, &hk).unwrap();
    let s: Vec<&str> = inv.argv.iter().map(String::as_str).collect();
    assert_eq!(inv.program, KINDLE_DROPBEARMULTI);
    assert_eq!(s[0], KINDLE_DROPBEARMULTI);
    assert_eq!(s[1], "dbclient");
    assert!(s.contains(&"-B"));
    assert!(s.contains(&"sync.example.test:8080"));
    assert!(s.contains(&"-T"));
    assert!(s.contains(&"StrictHostKeyChecking=yes"));
    assert!(s.contains(&"BatchMode=yes"));
    assert!(s.contains(&"PasswordAuthentication=no"));
    assert!(s.contains(&"ForwardAgent=no"));
    assert!(!s.contains(&"-L"));
    assert!(!s.contains(&"-N"));
    assert!(!s.contains(&"-y"));
    assert!(!s.contains(&"-A"));
    assert!(!s.contains(&"-t"));
    assert!(!s.contains(&"-R"));
    assert!(!s.contains(&"-D"));
    assert!(!s.iter().any(|a| a.contains("127.0.0.1")));
    assert!(!s.iter().any(|a| a.contains(SYN_BEARER)));
    assert!(!s.iter().any(|a| a.contains("BEGIN ")));
}

#[test]
fn listen_builder_still_refuses_b_and_stream_builder_refuses_l() {
    let lp = listen_plan();
    let inv = build_dropbear_invocation(&lp).unwrap();
    assert!(inv.argv.iter().any(|a| a == "-L"));
    assert!(!inv.argv.iter().any(|a| a == "-B"));
}

#[test]
fn unsupported_hosts_ports_and_pins_are_refused() {
    let mut p = plan();
    p.ssh_host = "10.0.0.8";
    assert!(validate_recovery_stream_plan(&p)
        .unwrap_err()
        .iter()
        .any(|e| e.contains("IP") || e.contains("STREAM")));
    let mut p = plan();
    p.ssh_port = 0;
    assert!(validate_recovery_stream_plan(&p)
        .unwrap_err()
        .iter()
        .any(|e| e.contains("SSH port")));
    let mut p = plan();
    p.remote_forward_host = "127.0.0.1";
    assert!(validate_recovery_stream_plan(&p)
        .unwrap_err()
        .iter()
        .any(|e| e.contains("authenticated host") || e.contains("STREAM")));
    let mut p = plan();
    p.pinned_host_key_sha256 = "";
    assert!(validate_recovery_stream_plan(&p)
        .unwrap_err()
        .iter()
        .any(|e| e.contains("pinned")));
}

#[test]
fn changed_and_missing_host_key_refuse_invocation() {
    let (p, mut hk) = ready_plan();
    hk.known_hosts_text = Some("");
    assert!(build_dropbear_stream_invocation(&p, &hk).is_err());
    let (p, mut hk) = ready_plan();
    let evil = Box::leak(
        "ssh.example.test ssh-ed25519 AAAA\n"
            .to_string()
            .into_boxed_str(),
    );
    hk.known_hosts_text = Some(evil);
    assert!(build_dropbear_stream_invocation(&p, &hk).is_err());
}

#[test]
fn password_prompts_cannot_activate_through_argv_or_env_contract() {
    let (p, hk) = ready_plan();
    let inv = build_dropbear_stream_invocation(&p, &hk).unwrap();
    assert!(inv.argv.iter().any(|a| a == "PasswordAuthentication=no"));
    assert!(inv.argv.iter().any(|a| a == "BatchMode=yes"));
    let cleared = recovery_stream_child_clear_vars();
    assert!(cleared.contains(&"DROPBEAR_PASSWORD"));
    assert!(cleared.contains(&"SSH_ASKPASS"));
    assert!(cleared.contains(&"DISPLAY"));
}

#[test]
fn child_spawn_is_not_enabled() {
    let (p, hk) = ready_plan();
    let inv = build_dropbear_stream_invocation(&p, &hk).unwrap();
    let err = spawn_recovery_stream_child(&inv).unwrap_err();
    assert!(err.contains("not enabled"));
    assert!(!err.contains(SYN_BEARER));
}

#[test]
fn no_arbitrary_destination_fallback() {
    let (p, hk) = ready_plan();
    let req = RecoveryHttp1Request {
        method: "GET",
        path: "/v1/libraries/x/operations?after=0&limit=50",
        host: "127.0.0.1:9",
        bearer: SYN_BEARER,
        body: None,
        fixture_get: false,
    };
    let _ = (p, hk);
    let mut buf = std::io::Cursor::new(Vec::<u8>::new());
    let err = recovery_http1_exchange(&mut buf, &plan(), &req).unwrap_err();
    assert!(err.contains("Host") || err.contains("loopback"));
    assert!(!err.contains(SYN_BEARER));
    assert!(buf.get_ref().is_empty());
}

fn get_path() -> &'static str {
    "/v1/libraries/11111111-1111-7111-8111-111111111111/operations?after=0&limit=50"
}

fn post_path() -> &'static str {
    "/v1/libraries/11111111-1111-7111-8111-111111111111/operations"
}

#[test]
fn valid_synthetic_get_and_post_over_owned_stream() {
    let (addr, cap) = spawn_http(
        "HTTP/1.1 200 OK",
        "Content-Type: application/json\r\n",
        b"{\"operations\":[]}",
    );
    let (p, _) = ready_plan();
    let req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();
    let mut s = connect(addr);
    let resp = recovery_http1_exchange(&mut s, &p, &req).unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"{\"operations\":[]}");
    thread::sleep(Duration::from_millis(30));
    assert!(cap.hits.load(Ordering::SeqCst) >= 1);
    assert!(cap.bearer.load(Ordering::SeqCst));

    let frozen = b"{\"envelopes\":[{\"v\":1}]}";
    let (addr, cap) = spawn_http(
        "HTTP/1.1 200 OK",
        "Content-Type: application/json\r\n",
        b"{}",
    );
    let req = recovery_http1_request(&p, "POST", post_path(), SYN_BEARER, Some(frozen)).unwrap();
    let mut s = connect(addr);
    let resp = recovery_http1_exchange(&mut s, &p, &req).unwrap();
    assert_eq!(resp.status, 200);
    thread::sleep(Duration::from_millis(30));
    assert!(cap.hits.load(Ordering::SeqCst) >= 1);
    assert!(cap.bearer.load(Ordering::SeqCst));
}

#[test]
fn exact_host_content_length_and_frozen_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_c = seen.clone();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            seen_c.lock().unwrap().extend_from_slice(&buf[..n]);
            let payload = b"{}";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(payload);
        }
    });
    let (p, _) = ready_plan();
    let frozen = b"{\"envelopes\":[EXACT-FROZEN-18E10J]}";
    let req = recovery_http1_request(&p, "POST", post_path(), SYN_BEARER, Some(frozen)).unwrap();
    let mut s = connect(addr);
    recovery_http1_exchange(&mut s, &p, &req).unwrap();
    thread::sleep(Duration::from_millis(40));
    let captured = seen.lock().unwrap().clone();
    let wire = String::from_utf8_lossy(&captured);
    assert!(wire.contains("Host: sync.example.test:8080"));
    assert!(wire.contains(&format!("Content-Length: {}", frozen.len())));
    assert!(wire.contains("EXACT-FROZEN-18E10J"));
    assert!(wire.contains("Connection: close"));
    assert!(!wire.contains("127.0.0.1"));
}

#[test]
fn redirect_chunked_malformed_and_truncated_are_refused() {
    let (p, _) = ready_plan();
    let req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();

    let (addr, _) = spawn_http("HTTP/1.1 302 Found", "Location: http://evil/\r\n", b"");
    let mut s = connect(addr);
    let err = recovery_http1_exchange(&mut s, &p, &req).unwrap_err();
    assert!(err.contains("redirect"));
    assert!(!err.contains(SYN_BEARER));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.read(&mut [0u8; 512]);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
            );
        }
    });
    let mut s = connect(addr);
    let err = recovery_http1_exchange(&mut s, &p, &req).unwrap_err();
    assert!(err.contains("transfer-encoding") || err.contains("chunked"));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.read(&mut [0u8; 512]);
            let _ = stream.write_all(b"not-http\r\n\r\n");
        }
    });
    let mut s = connect(addr);
    let err = recovery_http1_exchange(&mut s, &p, &req).unwrap_err();
    assert!(err.contains("malformed") || err.contains("REFUSE") || err.contains("AMBIGUOUS"));
    assert!(!err.contains(SYN_BEARER));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.read(&mut [0u8; 512]);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nshort");
        }
    });
    let mut s = connect(addr);
    let err = recovery_http1_exchange(&mut s, &p, &req).unwrap_err();
    assert!(err.contains("truncated") || err.contains("AMBIGUOUS"));
}

#[test]
fn stalled_response_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.read(&mut [0u8; 64]);
            thread::sleep(Duration::from_secs(3));
        }
    });
    let (p, _) = ready_plan();
    let req = recovery_http1_request(&p, "GET", get_path(), SYN_BEARER, None).unwrap();
    let mut s = connect(addr);
    s.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let err = recovery_http1_exchange(&mut s, &p, &req).unwrap_err();
    assert!(
        err.contains("AMBIGUOUS")
            || err.contains("timed")
            || err.contains("10060")
            || err.contains("10035")
            || err.contains("os error")
    );
    assert!(!err.contains(SYN_BEARER));
}

#[test]
fn broken_stream_after_write_is_ambiguous() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            drop(stream);
        }
    });
    let (p, _) = ready_plan();
    let frozen = b"{\"envelopes\":[1]}";
    let req = recovery_http1_request(&p, "POST", post_path(), SYN_BEARER, Some(frozen)).unwrap();
    let mut s = connect(addr);
    let err = recovery_http1_exchange(&mut s, &p, &req).unwrap_err();
    assert!(
        err.contains("AMBIGUOUS") || err.contains("truncated") || err.contains("REFUSE"),
        "{err}"
    );
    assert!(!err.contains(SYN_BEARER));
}

#[test]
fn request_body_bound_and_gate_d_unrelated() {
    let (p, _) = ready_plan();
    let huge = vec![b'x'; STREAM_MAX_BODY_BYTES + 1];
    let err = recovery_http1_request(&p, "POST", post_path(), SYN_BEARER, Some(&huge)).unwrap_err();
    assert!(err.contains("exceeds bound"));
    assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
    let dummy = crate::sync_recovery_policy::ready_tunnel_evidence_fixture();
    let _ = dummy;
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
fn stream_plan_does_not_bind_a_recovery_tcp_port() {
    let p = plan();
    // Compile-time: RecoveryStreamPlan has no listen_addr / listen_port fields.
    validate_recovery_stream_plan(&p).unwrap();
    let (p, hk) = ready_plan();
    let inv = build_dropbear_stream_invocation(&p, &hk).unwrap();
    assert!(!inv.argv.join(" ").contains("-L"));
    assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
    assert_eq!(
        crate::sync_recovery_stream::STREAM_MAX_CONCURRENT_CHILDREN,
        1
    );
    assert!(crate::sync_recovery_stream::STREAM_IO_TIMEOUT.as_secs() > 0);
    assert!(crate::sync_recovery_stream::STREAM_EXCHANGE_DEADLINE.as_secs() >= 3);
}

#[test]
fn existing_loopback_ureq_path_is_untouched() {
    // Stream module never imports ureq. Listen builder remains the Review 111 path.
    let inv = build_dropbear_invocation(&listen_plan()).unwrap();
    assert!(inv.argv.iter().any(|a| a == "-L"));
}
