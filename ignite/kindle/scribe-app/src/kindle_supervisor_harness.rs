//! Test-only Kindle harness that drives the real [`SupervisedRecoveryStream`].
//!
//! Built only with `--features kindle_supervisor_harness`. Not linked into the
//! customer `scribe-app` Scriptlet. Speaks synthetic HTTP only; never opens a
//! recovery selector, library, notebook, or production credential path.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::Duration;

use crate::sync_recovery_stream::{
    recovery_http1_fixture_get, recovery_http1_request, RecoveryStreamPlan,
};
use crate::sync_recovery_supervise::{
    stream_resources_released, DisposableStreamPermit, StreamTransportOutcome,
    SupervisedRecoveryStream,
};
use crate::sync_recovery_tunnel::{HostKeyTrustInput, DROPBEAR_CLIENT_APPLET};

const PROTECTED_DROPBEAR: &str = "/var/local/ignite-recovery/bin/dropbearmulti";
const EXPECTED_DROPBEAR_SHA256: &str =
    "d20b510a58b59ff5bf482bb5fcc66768d4c4c51705855b9f5c9e47ffaaf3d1e9";
const USB_NET_LITE: &str = "/mnt/us/usbnetlite/bin/dropbearmulti";
const SYN_BEARER: &str = "synthetic-recovery-bearer-r122";
const SYN_POST_BODY: &[u8] =
    br#"{"envelopes":[{"operationId":"op-synthetic-r122","clientSequence":1}]}"#;
const GET_PATH: &str =
    "/v1/libraries/22222222-2222-7222-8222-222222222222/operations?after=0&limit=50";
const POST_PATH: &str = "/v1/libraries/22222222-2222-7222-8222-222222222222/operations";

struct Cfg {
    executable: String,
    ssh_user: String,
    ssh_host: String,
    ssh_port: u16,
    remote_forward_host: String,
    remote_forward_port: u16,
    host_key_algorithm: String,
    pinned_host_key_sha256: String,
    expected_key_b64: String,
    known_hosts_text: String,
    identity_file: String,
    known_hosts_home: String,
}

impl Cfg {
    fn load(root: PathBuf) -> Result<Self, String> {
        let conf = read_required(root.join("harness.conf"))?;
        let get = |k: &str| -> Result<String, String> {
            conf.lines()
                .find_map(|line| {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        return None;
                    }
                    let (key, val) = line.split_once('=')?;
                    if key.trim() == k {
                        Some(val.trim().to_string())
                    } else {
                        None
                    }
                })
                .ok_or_else(|| format!("missing harness.conf key {k}"))
        };
        let ssh_port: u16 = get("ssh_port")?
            .parse()
            .map_err(|_| "ssh_port must be u16".to_string())?;
        let remote_forward_port: u16 = get("remote_forward_port")?
            .parse()
            .map_err(|_| "remote_forward_port must be u16".to_string())?;
        let identity_file = root.join("id_ed25519").to_string_lossy().into_owned();
        let known_hosts_home = root.join("home").to_string_lossy().into_owned();
        let known_hosts_text = read_required(root.join("home/.ssh/known_hosts"))?;
        let _ = root; // staging root validated via path joins above
        Ok(Self {
            executable: get("executable")?,
            ssh_user: get("ssh_user")?,
            ssh_host: get("ssh_host")?,
            ssh_port,
            remote_forward_host: get("remote_forward_host")?,
            remote_forward_port,
            host_key_algorithm: get("host_key_algorithm")?,
            pinned_host_key_sha256: get("pinned_host_key_sha256")?,
            expected_key_b64: get("expected_key_b64")?,
            known_hosts_text,
            identity_file,
            known_hosts_home,
        })
    }

    fn plan(&self) -> RecoveryStreamPlan<'_> {
        RecoveryStreamPlan {
            executable: &self.executable,
            applet: DROPBEAR_CLIENT_APPLET,
            ssh_user: &self.ssh_user,
            ssh_host: &self.ssh_host,
            ssh_port: self.ssh_port,
            remote_forward_host: &self.remote_forward_host,
            remote_forward_port: self.remote_forward_port,
            host_key_algorithm: &self.host_key_algorithm,
            pinned_host_key_sha256: &self.pinned_host_key_sha256,
            identity_file: &self.identity_file,
            known_hosts_home: &self.known_hosts_home,
            library_id: "lib-synthetic-r122",
            replica_id: "rep-synthetic-r122",
            genesis_sha256: "gen-synthetic-r122-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            transaction_id: "txn-synthetic-supervisor-r122",
        }
    }

    fn host_key(&self) -> HostKeyTrustInput<'_> {
        HostKeyTrustInput {
            known_hosts_text: Some(&self.known_hosts_text),
            expected_hostname: &self.ssh_host,
            expected_ssh_port: self.ssh_port,
            planned_ssh_port: self.ssh_port,
            expected_algorithm: &self.host_key_algorithm,
            expected_key_b64: &self.expected_key_b64,
            expected_fingerprint_sha256: &self.pinned_host_key_sha256,
            tofu_permitted: false,
            accept_unknown_host: false,
            host_key_verification_disabled: false,
        }
    }
}

pub fn run() -> ExitCode {
    match run_inner() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "HARNESS FAIL: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let mut root = None;
    let mut case = "all".to_string();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => {
                root = Some(PathBuf::from(args.next().ok_or("--root requires a path")?));
            }
            "--case" => {
                case = args.next().ok_or("--case requires a name")?;
            }
            "--help" | "-h" => {
                println!(
                    "kindle-supervisor-harness --root DIR [--case get|post|timeout|all]\n\
                     Test-only; drives SupervisedRecoveryStream against synthetic fixtures."
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let root = root.ok_or("missing --root DIR")?;
    let cfg = Cfg::load(root)?;
    verify_protected_dropbear(&cfg.executable)?;
    println!("EVIDENCE dropbear_sha256={EXPECTED_DROPBEAR_SHA256}");
    println!("EVIDENCE executable={}", cfg.executable);
    println!("EVIDENCE case={case}");

    match case.as_str() {
        "get" => run_get(&cfg)?,
        "post" => run_post(&cfg)?,
        "timeout" => run_timeout(&cfg)?,
        // get+post only; timeout needs the fixture in hang mode (host orchestrates).
        "all" => {
            run_get(&cfg)?;
            run_post(&cfg)?;
        }
        other => return Err(format!("unknown case {other}")),
    }
    assert_no_orphans(&cfg)?;
    println!("EVIDENCE harness_complete=ok");
    Ok(())
}

fn verify_protected_dropbear(executable: &str) -> Result<(), String> {
    if executable != PROTECTED_DROPBEAR {
        return Err(format!(
            "executable must be protected path {PROTECTED_DROPBEAR}, got {executable}"
        ));
    }
    if executable == USB_NET_LITE || executable.contains("usbnetlite") {
        return Err("refusing USBNETLite dropbear path".into());
    }
    let out = Command::new("sha256sum")
        .arg(executable)
        .output()
        .map_err(|e| format!("sha256sum: {e}"))?;
    if !out.status.success() {
        return Err("sha256sum failed".into());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let got = text.split_whitespace().next().ok_or("sha256sum empty")?;
    if got != EXPECTED_DROPBEAR_SHA256 {
        return Err(format!(
            "dropbear digest mismatch: got {got}, expected {EXPECTED_DROPBEAR_SHA256}"
        ));
    }
    Ok(())
}

fn open_session(cfg: &Cfg) -> Result<SupervisedRecoveryStream, String> {
    let plan = cfg.plan();
    let hk = cfg.host_key();
    let permit = DisposableStreamPermit::for_synthetic_fixture();
    SupervisedRecoveryStream::open(permit, &plan, &hk).map_err(|o| format!("open: {o:?}"))
}

fn run_get(cfg: &Cfg) -> Result<(), String> {
    let plan = cfg.plan();
    let mut session = open_session(cfg)?;
    evidence_child(&session)?;
    let pid = session.pid();
    let req = recovery_http1_fixture_get(&plan, SYN_BEARER);
    let out = session.exchange(&plan, &req);
    if !stream_resources_released(&session) {
        return Err("get: stream resources not released".into());
    }
    drop(session);
    assert_pid_gone(pid)?;
    match out {
        StreamTransportOutcome::Authoritative { status, body } => {
            if status != 200 {
                return Err(format!("get: status {status}"));
            }
            let text = String::from_utf8_lossy(&body);
            if !text.contains("fixture-get-ok-r122") {
                return Err(format!("get: unexpected body {text:?}"));
            }
            println!(
                "EVIDENCE get=authoritative status={status} body_len={}",
                body.len()
            );
            Ok(())
        }
        other => Err(format!("get: {other:?}")),
    }
}

fn run_post(cfg: &Cfg) -> Result<(), String> {
    let plan = cfg.plan();
    let mut session = open_session(cfg)?;
    evidence_child(&session)?;
    let pid = session.pid();
    let req = recovery_http1_request(&plan, "POST", POST_PATH, SYN_BEARER, Some(SYN_POST_BODY))
        .map_err(|e| format!("post request: {e}"))?;
    let out = session.exchange(&plan, &req);
    if !stream_resources_released(&session) {
        return Err("post: stream resources not released".into());
    }
    drop(session);
    assert_pid_gone(pid)?;
    match out {
        StreamTransportOutcome::Authoritative { status, body } => {
            if status != 200 {
                return Err(format!("post: status {status}"));
            }
            let text = String::from_utf8_lossy(&body);
            if !text.contains("fixture-post-ok-r122") {
                return Err(format!("post: unexpected body {text:?}"));
            }
            if !text.contains("body_sha256=") {
                return Err(format!("post: missing body digest in response {text:?}"));
            }
            let expect_hex = sync_protocol::sha256_hex(SYN_POST_BODY)
                .as_str()
                .to_string();
            if !text.contains(&format!("body_sha256={expect_hex}")) {
                return Err(format!(
                    "post: body digest mismatch in response {text:?}, expected {expect_hex}"
                ));
            }
            println!("EVIDENCE post=authoritative status={status} body_sha256={expect_hex}");
            println!("EVIDENCE post_body_exact_match=ok");
            Ok(())
        }
        other => Err(format!("post: {other:?}")),
    }
}

fn run_timeout(cfg: &Cfg) -> Result<(), String> {
    let plan = cfg.plan();
    let mut session = open_session(cfg)?;
    evidence_child(&session)?;
    let pid = session.pid();
    // Valid recovery GET path against a fixture that accepts then never responds.
    let req = recovery_http1_request(&plan, "GET", GET_PATH, SYN_BEARER, None)
        .map_err(|e| format!("timeout request: {e}"))?;
    let out = session.exchange(&plan, &req);
    if !stream_resources_released(&session) {
        return Err("timeout: stream resources not released".into());
    }
    drop(session);
    assert_pid_gone(pid)?;
    match out {
        StreamTransportOutcome::WriteStartedAmbiguous { reason } => {
            println!("EVIDENCE timeout=ambiguous reason={reason}");
            Ok(())
        }
        other => Err(format!(
            "timeout: expected WriteStartedAmbiguous, got {other:?}"
        )),
    }
}

fn evidence_child(session: &SupervisedRecoveryStream) -> Result<(), String> {
    let program = session.program();
    if program != PROTECTED_DROPBEAR {
        return Err(format!(
            "child program is not protected dropbear: {program}"
        ));
    }
    if program.contains("usbnetlite") {
        return Err("child program resolves to USBNETLite".into());
    }
    let argv = session.argv();
    if !argv.iter().any(|a| a == "-B") {
        return Err("argv missing -B".into());
    }
    if argv.iter().any(|a| a == "-L" || a.contains("127.0.0.1")) {
        return Err("argv contains local listener forwarding".into());
    }
    let pid = session.pid();
    println!("EVIDENCE child_pid={pid}");
    println!("EVIDENCE child_program={program}");
    println!("EVIDENCE child_argv={}", argv.join(" "));
    if let Ok(exe) = fs::read_link(format!("/proc/{pid}/exe")) {
        let exe_s = exe.to_string_lossy();
        println!("EVIDENCE child_proc_exe={exe_s}");
        if exe_s.contains("usbnetlite") {
            return Err("proc exe is USBNETLite".into());
        }
        if !exe_s.contains("dropbearmulti") {
            return Err(format!("proc exe unexpected: {exe_s}"));
        }
    }
    // Kindle has no per-process net namespace: /proc/PID/net/tcp is system-wide and
    // always includes USBNETLite. Attribute listeners by matching socket inodes from
    // the child's fds only.
    let owned = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ls -l /proc/{pid}/fd 2>/dev/null | sed -n 's/.*socket:\\[\\([0-9]*\\)\\]/\\1/p'"
        ))
        .output()
        .map_err(|e| e.to_string())?;
    let inodes: Vec<String> = String::from_utf8_lossy(&owned.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut child_listen = Vec::new();
    if let Ok(tcp) = fs::read_to_string("/proc/net/tcp") {
        for line in tcp.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 {
                continue;
            }
            // state 0A = LISTEN
            if cols[3] != "0A" {
                continue;
            }
            let inode = cols[9];
            if inodes.iter().any(|i| i == inode) {
                child_listen.push(line.to_string());
            }
        }
    }
    if !child_listen.is_empty() {
        return Err(format!(
            "child-owned LISTEN sockets (local forward?): {}",
            child_listen.join(" | ")
        ));
    }
    println!("EVIDENCE child_owned_listen_sockets=none");
    println!("EVIDENCE child_socket_fds={}", inodes.len());
    Ok(())
}

fn assert_pid_gone(pid: u32) -> Result<(), String> {
    // Give the kernel a moment after wait().
    thread::sleep(Duration::from_millis(50));
    if Path::new(&format!("/proc/{pid}")).exists() {
        // Zombies briefly possible; try once more.
        thread::sleep(Duration::from_millis(200));
        if Path::new(&format!("/proc/{pid}")).exists() {
            let state = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            if state.contains(" Z ") || state.split_whitespace().nth(2) == Some("Z") {
                return Err(format!("child pid {pid} left as zombie"));
            }
            return Err(format!("child pid {pid} still present"));
        }
    }
    println!("EVIDENCE child_reaped pid={pid}");
    Ok(())
}

fn assert_no_orphans(cfg: &Cfg) -> Result<(), String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ps | grep -F '{}' | grep -v grep | grep -v kindle-supervisor-harness || true",
            cfg.executable
        ))
        .output()
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout);
    // USBNETLite admin sshd may also be named dropbearmulti; ignore that path.
    for line in text.lines() {
        if line.contains(PROTECTED_DROPBEAR) || line.contains("ignite-recovery") {
            return Err(format!("orphan protected dropbear child: {line}"));
        }
        if line.contains("-B") && line.contains(&cfg.ssh_host) {
            return Err(format!("orphan supervised forward child: {line}"));
        }
    }
    println!("EVIDENCE no_orphan_supervised_children=ok");
    Ok(())
}

fn read_required(path: PathBuf) -> Result<String, String> {
    fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))
}
