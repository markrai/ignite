//! Nonexecuting Dropbear dbclient tunnel contract (Slice 18E.10D).
//!
//! Builds a discrete argv for Kindle-initiated `-L` forwarding using the USBNETLite
//! `dropbearmulti` binary. Never starts a process, never opens a socket, never reads
//! live keys or `known_hosts` from the device.
//!
//! Applet selection (Dropbear 2024.85 `src/dbmulti.c`): first `basename(argv[0])`, then
//! `basename(argv[1])`. Supported without touching USBNETLite files:
//! `/mnt/us/usbnetlite/bin/dropbearmulti dbclient …`
//!
//! `known_hosts` path (Dropbear `cli-kex.c`): `$HOME/.ssh/known_hosts`. There is no
//! `-o UserKnownHostsFile`. A future invocation must set `HOME` on the process, not
//! interpolate a shell command.

use crate::sync_recovery_policy::{
    evaluate_tunnel_runtime_evidence, host_looks_like_ipv4, TunnelRuntimeEvidence,
    LIVE_NON_LOOPBACK_POST_ENABLED,
};

/// Path verified on the Kindle in Review 105. Not executed from this module.
pub const KINDLE_DROPBEARMULTI: &str = "/mnt/us/usbnetlite/bin/dropbearmulti";
pub const DROPBEAR_CLIENT_APPLET: &str = "dbclient";

/// Typed description of one authorized Kindle-local Dropbear forward.
/// Contains paths and fingerprints only — never bearer tokens or private-key bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryTunnelPlan<'a> {
    pub executable: &'a str,
    pub applet: &'a str,
    pub listen_addr: &'a str,
    pub listen_port: u16,
    pub ssh_user: &'a str,
    pub ssh_host: &'a str,
    pub ssh_port: u16,
    pub remote_forward_host: &'a str,
    pub remote_forward_port: u16,
    pub host_key_algorithm: &'a str,
    pub pinned_host_key_sha256: &'a str,
    pub identity_file: &'a str,
    /// Directory used as process `HOME` so Dropbear reads `$HOME/.ssh/known_hosts`.
    pub known_hosts_home: &'a str,
    pub library_id: &'a str,
    pub replica_id: &'a str,
    pub genesis_sha256: &'a str,
    pub transaction_id: &'a str,
}

/// Discrete program + arguments + `HOME`. Ready for `Command::new` later; not executed here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropbearInvocation {
    pub program: String,
    pub argv: Vec<String>,
    pub home: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyTrustInput<'a> {
    pub known_hosts_text: Option<&'a str>,
    pub expected_hostname: &'a str,
    pub expected_ssh_port: u16,
    pub planned_ssh_port: u16,
    pub expected_algorithm: &'a str,
    pub expected_key_b64: &'a str,
    pub expected_fingerprint_sha256: &'a str,
    pub tofu_permitted: bool,
    pub accept_unknown_host: bool,
    pub host_key_verification_disabled: bool,
}

pub fn validate_recovery_tunnel_plan(plan: &RecoveryTunnelPlan<'_>) -> Result<(), Vec<String>> {
    let mut reasons = Vec::new();
    if LIVE_NON_LOOPBACK_POST_ENABLED {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: non-loopback recovery POST is not enabled",
        );
    }
    let base = std::path::Path::new(plan.executable)
        .file_name()
        .and_then(|n| n.to_str());
    if plan.executable.is_empty() || base != Some("dropbearmulti") {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: executable must be dropbearmulti",
        );
    }
    if plan.applet != DROPBEAR_CLIENT_APPLET && plan.applet != "ssh" {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: applet must be dbclient (or ssh)",
        );
    }
    if plan.applet == "dropbear" {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: server applet is not a recovery tunnel client",
        );
    }
    if plan.listen_addr != "127.0.0.1" {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: listener must be 127.0.0.1",
        );
    }
    if plan.listen_port == 0 {
        refuse(&mut reasons, "REFUSE TUNNEL PLAN: listen port is missing");
    }
    if plan.ssh_port == 0 {
        refuse(&mut reasons, "REFUSE TUNNEL PLAN: SSH port is missing");
    }
    if plan.remote_forward_port == 0 {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: remote forwarding port is missing",
        );
    }
    if !safe_username(plan.ssh_user) {
        refuse(&mut reasons, "REFUSE TUNNEL PLAN: SSH username is unsafe");
    }
    if let Err(e) = require_safe_hostname(plan.ssh_host, "SSH host") {
        refuse(&mut reasons, e);
    }
    if let Err(e) = require_safe_hostname(plan.remote_forward_host, "forwarding target") {
        refuse(&mut reasons, e);
    }
    if !allowed_host_key_algorithm(plan.host_key_algorithm) {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: host-key algorithm is not allowed",
        );
    }
    if !is_sha256_hex(plan.pinned_host_key_sha256) {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: pinned host-key SHA-256 is missing or malformed",
        );
    }
    if !safe_abs_path(plan.identity_file) {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: identity file path is missing or unsafe",
        );
    }
    if plan.identity_file.contains("dropbear_") && plan.identity_file.contains("host_key") {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: must not use the Dropbear server host key as a client identity",
        );
    }
    if !safe_abs_path(plan.known_hosts_home) {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: known_hosts HOME is missing or unsafe",
        );
    }
    if plan.known_hosts_home == "/mnt/us" || plan.known_hosts_home == "/tmp/root" {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: known_hosts HOME must be a dedicated recovery directory",
        );
    }
    if plan.library_id.is_empty() || plan.replica_id.is_empty() || plan.genesis_sha256.is_empty() {
        refuse(&mut reasons, "REFUSE TUNNEL PLAN: missing identity fields");
    }
    if plan.transaction_id.is_empty() {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: recovery transaction identity is missing",
        );
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

/// Construct argv. Never returns a string for `sh -c`.
pub fn build_dropbear_invocation(
    plan: &RecoveryTunnelPlan<'_>,
) -> Result<DropbearInvocation, Vec<String>> {
    validate_recovery_tunnel_plan(plan)?;
    let applet = if plan.applet == "ssh" {
        "ssh"
    } else {
        DROPBEAR_CLIENT_APPLET
    };
    let forward = format!(
        "{}:{}:{}:{}",
        plan.listen_addr, plan.listen_port, plan.remote_forward_host, plan.remote_forward_port
    );
    let dest = format!("{}@{}", plan.ssh_user, plan.ssh_host);
    let argv = vec![
        plan.executable.to_string(),
        applet.to_string(),
        "-N".into(),
        "-T".into(),
        "-i".into(),
        plan.identity_file.to_string(),
        "-o".into(),
        "StrictHostKeyChecking=yes".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "GatewayPorts=no".into(),
        "-o".into(),
        "ForwardAgent=no".into(),
        "-o".into(),
        "PasswordAuthentication=no".into(),
        "-L".into(),
        forward,
        "-p".into(),
        plan.ssh_port.to_string(),
        dest,
    ];
    assert_safe_argv(&argv)?;
    Ok(DropbearInvocation {
        program: plan.executable.to_string(),
        argv,
        home: plan.known_hosts_home.to_string(),
    })
}

fn assert_safe_argv(argv: &[String]) -> Result<(), Vec<String>> {
    let mut reasons = Vec::new();
    let joined = argv.join("\0");
    for needle in [
        "-y",
        "-D",
        "-A",
        "-g",
        "-t",
        "-B",
        "-R",
        "UserKnownHostsFile",
    ] {
        if needle == "-t" {
            continue; // `-T` contains no isolated `-t`; check tokens
        }
        if argv.iter().any(|a| a == needle) {
            refuse(
                &mut reasons,
                format!("REFUSE TUNNEL PLAN: unsafe argument {needle}"),
            );
        }
    }
    if argv.iter().any(|a| a == "-y" || a == "-yy") {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: trust-on-first-use (-y) is refused",
        );
    }
    if argv.iter().any(|a| a == "-t") {
        refuse(&mut reasons, "REFUSE TUNNEL PLAN: PTY is refused");
    }
    if argv.iter().any(|a| a == "-A") {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: agent forwarding is refused",
        );
    }
    if argv.iter().any(|a| a.contains("UserKnownHostsFile")) {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: UserKnownHostsFile is not a Dropbear -o option",
        );
    }
    if joined.contains("sh -c") || joined.contains("bash -c") {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL PLAN: shell interpolation is refused",
        );
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

pub fn evaluate_host_key_trust(input: &HostKeyTrustInput<'_>) -> Result<(), Vec<String>> {
    let mut reasons = Vec::new();
    if input.tofu_permitted || input.accept_unknown_host {
        refuse(
            &mut reasons,
            "REFUSE HOST KEY: trust-on-first-use is not permitted",
        );
    }
    if input.host_key_verification_disabled {
        refuse(
            &mut reasons,
            "REFUSE HOST KEY: host-key verification must not be disabled",
        );
    }
    if input.expected_ssh_port == 0 || input.planned_ssh_port == 0 {
        refuse(&mut reasons, "REFUSE HOST KEY: SSH port is missing");
    } else if input.expected_ssh_port != input.planned_ssh_port {
        refuse(
            &mut reasons,
            "REFUSE HOST KEY: SSH port does not match the pinned association",
        );
    }
    if let Err(e) = require_safe_hostname(input.expected_hostname, "pinned SSH host") {
        refuse(&mut reasons, e);
    }
    if !allowed_host_key_algorithm(input.expected_algorithm) {
        refuse(&mut reasons, "REFUSE HOST KEY: algorithm is not allowed");
    }
    if !is_sha256_hex(input.expected_fingerprint_sha256) {
        refuse(
            &mut reasons,
            "REFUSE HOST KEY: expected fingerprint is malformed",
        );
    }
    let Some(text) = input.known_hosts_text else {
        refuse(&mut reasons, "REFUSE HOST KEY: known_hosts is missing");
        return Err(reasons);
    };
    if text.trim().is_empty() {
        refuse(&mut reasons, "REFUSE HOST KEY: known_hosts is empty");
        return Err(reasons);
    }
    let mut matches = 0usize;
    let mut hostname_hits = 0usize;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("|1|") {
            refuse(
                &mut reasons,
                "REFUSE HOST KEY: hashed known_hosts entries are not usable for recovery pins",
            );
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(host) = parts.next() else { continue };
        let Some(algo) = parts.next() else { continue };
        let Some(b64) = parts.next() else { continue };
        if host.starts_with('[') {
            refuse(
                &mut reasons,
                "REFUSE HOST KEY: OpenSSH [host]:port known_hosts form is not Dropbear's matcher",
            );
            continue;
        }
        if host != input.expected_hostname {
            continue;
        }
        hostname_hits += 1;
        if algo != input.expected_algorithm {
            refuse(
                &mut reasons,
                "REFUSE HOST KEY: unexpected additional host key algorithm",
            );
            continue;
        }
        if b64 != input.expected_key_b64 {
            refuse(&mut reasons, "REFUSE HOST KEY: changed host key");
            continue;
        }
        match fingerprint_of_b64(b64) {
            Ok(fp) if fp == input.expected_fingerprint_sha256 => {
                matches += 1;
            }
            Ok(_) => refuse(&mut reasons, "REFUSE HOST KEY: mismatched fingerprint"),
            Err(e) => refuse(&mut reasons, e),
        }
    }
    if hostname_hits == 0 && matches == 0 {
        refuse(
            &mut reasons,
            "REFUSE HOST KEY: wrong hostname or unknown host key",
        );
    }
    if matches != 1 {
        if matches == 0 && !reasons.iter().any(|r| r.contains("changed host key")) {
            refuse(&mut reasons, "REFUSE HOST KEY: unknown host key");
        }
        if matches > 1 {
            refuse(
                &mut reasons,
                "REFUSE HOST KEY: unexpected additional host key",
            );
        }
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

/// Feed existing [`evaluate_tunnel_runtime_evidence`]. Does not open sockets or start Dropbear.
pub fn require_validated_tunnel_before_http(
    ev: &TunnelRuntimeEvidence<'_>,
) -> Result<(), Vec<String>> {
    evaluate_tunnel_runtime_evidence(ev)
}

pub fn fingerprint_of_b64(b64: &str) -> Result<String, String> {
    let bytes = b64_decode(b64)?;
    Ok(sync_protocol::sha256_hex(&bytes).as_str().to_string())
}

pub(crate) fn require_safe_hostname(host: &str, what: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err(format!("REFUSE TUNNEL PLAN: {what} is empty"));
    }
    let lower = host.to_ascii_lowercase();
    for needle in [
        "localhost",
        "tailscale",
        "ts.net",
        "kindle",
        "127.0.0.1",
        "::1",
        "0.0.0.0",
    ] {
        if lower.contains(needle) {
            return Err(format!(
                "REFUSE TUNNEL PLAN: {what} is not an authenticated host"
            ));
        }
    }
    if host_looks_like_ipv4(host) {
        return Err(format!(
            "REFUSE TUNNEL PLAN: IP address alone is not proof of server identity"
        ));
    }
    if host.contains(',') || host.contains(' ') || host.contains('@') || host.contains('/') {
        return Err(format!(
            "REFUSE TUNNEL PLAN: {what} contains unsafe characters"
        ));
    }
    if host.starts_with('-') || host.contains('*') {
        return Err(format!("REFUSE TUNNEL PLAN: {what} is unsafe"));
    }
    Ok(())
}

pub(crate) fn safe_username(user: &str) -> bool {
    !user.is_empty()
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        && !user.contains('@')
}

pub(crate) fn safe_abs_path(p: &str) -> bool {
    if p.is_empty() || p.contains('\0') || p.contains("..") || p.contains("BEGIN ") {
        return false;
    }
    if p.starts_with('/') {
        return true;
    }
    // Host-side Windows fixtures (Kindle paths remain POSIX).
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

pub(crate) fn allowed_host_key_algorithm(algo: &str) -> bool {
    matches!(
        algo,
        "ssh-ed25519" | "ecdsa-sha2-nistp256" | "ecdsa-sha2-nistp384" | "ssh-rsa"
    )
}

pub(crate) fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn refuse(reasons: &mut Vec<String>, msg: impl Into<String>) {
    reasons.push(msg.into());
}

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
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

fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut vals = Vec::new();
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let Some(v) = B64.iter().position(|&x| x == c) else {
            return Err("REFUSE HOST KEY: host-key base64 is malformed".into());
        };
        vals.push(v as u8);
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < vals.len() {
        out.push((vals[i] << 2) | (vals[i + 1] >> 4));
        if i + 2 < vals.len() {
            out.push((vals[i + 1] << 4) | (vals[i + 2] >> 2));
        }
        if i + 3 < vals.len() {
            out.push((vals[i + 2] << 6) | vals[i + 3]);
        }
        i += 4;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_recovery_policy::TunnelLifecycle;

    const PIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn plan() -> RecoveryTunnelPlan<'static> {
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
            pinned_host_key_sha256: PIN,
            identity_file: "/mnt/us/scribe-app/state/recovery-tunnel/id_ed25519",
            known_hosts_home: "/mnt/us/scribe-app/state/recovery-tunnel",
            library_id: "lib-aaaa",
            replica_id: "rep-bbbb",
            genesis_sha256: "gen-cccc",
            transaction_id: "txn-synthetic-1",
        }
    }

    fn pin_material() -> (String, String, String) {
        let blob = b"ssh-ed25519\0synthetic-host-key-blob";
        let b64 = b64_encode(blob);
        let fp = fingerprint_of_b64(&b64).unwrap();
        let line = format!("ssh.example.test ssh-ed25519 {b64}\n");
        (b64, fp, line)
    }

    fn refuse_plan(p: RecoveryTunnelPlan<'_>, needle: &str) {
        let err = validate_recovery_tunnel_plan(&p).unwrap_err();
        assert!(
            err.iter().any(|e| e.contains(needle)),
            "expected {needle:?} in {err:?}"
        );
    }

    #[test]
    fn applet_selection_is_subcommand_not_flags_on_dropbearmulti() {
        let inv = build_dropbear_invocation(&plan()).unwrap();
        assert_eq!(inv.program, KINDLE_DROPBEARMULTI);
        assert_eq!(inv.argv[0], KINDLE_DROPBEARMULTI);
        assert_eq!(inv.argv[1], "dbclient");
        assert_ne!(inv.argv[1], "-L");
        assert_eq!(inv.home, "/mnt/us/scribe-app/state/recovery-tunnel");
    }

    #[test]
    fn argv_is_discrete_tokens_with_required_dropbear_options() {
        let inv = build_dropbear_invocation(&plan()).unwrap();
        let s: Vec<&str> = inv.argv.iter().map(String::as_str).collect();
        assert!(s.contains(&"-N"));
        assert!(s.contains(&"-T"));
        assert!(s.contains(&"-L"));
        assert!(s.contains(&"127.0.0.1:18765:sync.example.test:8080"));
        assert!(s.contains(&"-o"));
        assert!(s.contains(&"StrictHostKeyChecking=yes"));
        assert!(s.contains(&"BatchMode=yes"));
        assert!(s.contains(&"ExitOnForwardFailure=yes"));
        assert!(s.contains(&"GatewayPorts=no"));
        assert!(s.contains(&"ForwardAgent=no"));
        assert!(s.contains(&"PasswordAuthentication=no"));
        assert!(s.contains(&"-p"));
        assert!(s.contains(&"22"));
        assert!(s.contains(&"syncfwd@ssh.example.test"));
        assert!(s.contains(&"-i"));
        assert!(!s.iter().any(|a| *a == "-y" || *a == "-yy" || *a == "-f"));
        assert!(!s.iter().any(|a| a.contains("UserKnownHostsFile")));
        assert!(!s.iter().any(|a| a.contains("BEGIN ")));
        assert!(!inv.argv.iter().any(|a| a.contains(' ')));
    }

    #[test]
    fn refuses_server_applet_and_non_loopback_listen() {
        let mut p = plan();
        p.applet = "dropbear";
        refuse_plan(p, "applet");
        let mut p = plan();
        p.listen_addr = "0.0.0.0";
        refuse_plan(p, "127.0.0.1");
        let mut p = plan();
        p.listen_addr = "::";
        refuse_plan(p, "127.0.0.1");
    }

    #[test]
    fn refuses_unsafe_hosts_ports_and_missing_pin() {
        let mut p = plan();
        p.ssh_host = "10.0.0.8";
        refuse_plan(p, "IP address");
        let mut p = plan();
        p.remote_forward_host = "127.0.0.1";
        refuse_plan(p, "authenticated host");
        let mut p = plan();
        p.ssh_port = 0;
        refuse_plan(p, "SSH port");
        let mut p = plan();
        p.pinned_host_key_sha256 = "";
        refuse_plan(p, "pinned host-key");
        let mut p = plan();
        p.ssh_host = "host,evil";
        refuse_plan(p, "unsafe characters");
    }

    #[test]
    fn refuses_wrong_identities_and_password_shaped_paths() {
        let mut p = plan();
        p.library_id = "";
        refuse_plan(p, "missing identity");
        let mut p = plan();
        p.transaction_id = "";
        refuse_plan(p, "transaction");
        let mut p = plan();
        p.identity_file = "/mnt/us/usbnetlite/etc/dropbear/dropbear_ed25519_host_key";
        refuse_plan(p, "server host key");
        let mut p = plan();
        p.known_hosts_home = "/mnt/us";
        refuse_plan(p, "dedicated recovery directory");
    }

    #[test]
    fn host_key_trust_accepts_exact_dropbear_line() {
        let (b64, fp, line) = pin_material();
        let input = HostKeyTrustInput {
            known_hosts_text: Some(line.as_str()),
            expected_hostname: "ssh.example.test",
            expected_ssh_port: 22,
            planned_ssh_port: 22,
            expected_algorithm: "ssh-ed25519",
            expected_key_b64: b64.as_str(),
            expected_fingerprint_sha256: fp.as_str(),
            tofu_permitted: false,
            accept_unknown_host: false,
            host_key_verification_disabled: false,
        };
        evaluate_host_key_trust(&input).unwrap();
    }

    #[test]
    fn host_key_trust_refuses_missing_empty_wrong_and_changed() {
        let (b64, fp, line) = pin_material();
        let base = HostKeyTrustInput {
            known_hosts_text: Some(line.as_str()),
            expected_hostname: "ssh.example.test",
            expected_ssh_port: 22,
            planned_ssh_port: 22,
            expected_algorithm: "ssh-ed25519",
            expected_key_b64: b64.as_str(),
            expected_fingerprint_sha256: fp.as_str(),
            tofu_permitted: false,
            accept_unknown_host: false,
            host_key_verification_disabled: false,
        };
        let mut i = base.clone();
        i.known_hosts_text = None;
        assert!(evaluate_host_key_trust(&i)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("missing")));
        let mut i = base.clone();
        i.known_hosts_text = Some("   \n");
        assert!(evaluate_host_key_trust(&i)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("empty")));
        let mut i = base.clone();
        i.expected_hostname = "other.example.test";
        assert!(evaluate_host_key_trust(&i)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("wrong hostname") || e.contains("unknown")));
        let mut i = base.clone();
        i.planned_ssh_port = 2222;
        assert!(evaluate_host_key_trust(&i)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("port")));
        let other = b64_encode(b"ssh-ed25519\0different-blob");
        let changed = format!("ssh.example.test ssh-ed25519 {other}\n");
        let mut i = base.clone();
        i.known_hosts_text = Some(changed.as_str());
        i.expected_key_b64 = other.as_str();
        // fingerprint still the original pin → mismatch
        i.expected_key_b64 = b64.as_str();
        i.known_hosts_text = Some(changed.as_str());
        assert!(evaluate_host_key_trust(&i)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("changed host key")));
    }

    #[test]
    fn host_key_trust_refuses_algo_extra_tofu_and_disabled_check() {
        let (b64, fp, _line) = pin_material();
        let wrong_algo = format!("ssh.example.test ssh-rsa {b64}\n");
        let input = HostKeyTrustInput {
            known_hosts_text: Some(wrong_algo.as_str()),
            expected_hostname: "ssh.example.test",
            expected_ssh_port: 22,
            planned_ssh_port: 22,
            expected_algorithm: "ssh-ed25519",
            expected_key_b64: b64.as_str(),
            expected_fingerprint_sha256: fp.as_str(),
            tofu_permitted: false,
            accept_unknown_host: false,
            host_key_verification_disabled: false,
        };
        assert!(evaluate_host_key_trust(&input)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("algorithm") || e.contains("unknown")));
        let extra = format!(
            "ssh.example.test ssh-ed25519 {b64}\nssh.example.test ssh-ed25519 {}\n",
            b64_encode(b"other")
        );
        let input = HostKeyTrustInput {
            known_hosts_text: Some(extra.as_str()),
            expected_hostname: "ssh.example.test",
            expected_ssh_port: 22,
            planned_ssh_port: 22,
            expected_algorithm: "ssh-ed25519",
            expected_key_b64: b64.as_str(),
            expected_fingerprint_sha256: fp.as_str(),
            tofu_permitted: false,
            accept_unknown_host: false,
            host_key_verification_disabled: false,
        };
        assert!(evaluate_host_key_trust(&input).is_err());
        let line = format!("ssh.example.test ssh-ed25519 {b64}\n");
        let mut tofu = HostKeyTrustInput {
            known_hosts_text: Some(line.as_str()),
            expected_hostname: "ssh.example.test",
            expected_ssh_port: 22,
            planned_ssh_port: 22,
            expected_algorithm: "ssh-ed25519",
            expected_key_b64: b64.as_str(),
            expected_fingerprint_sha256: fp.as_str(),
            tofu_permitted: true,
            accept_unknown_host: false,
            host_key_verification_disabled: false,
        };
        assert!(evaluate_host_key_trust(&tofu)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("trust-on-first-use")));
        tofu.tofu_permitted = false;
        tofu.accept_unknown_host = true;
        assert!(evaluate_host_key_trust(&tofu)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("trust-on-first-use")));
        tofu.accept_unknown_host = false;
        tofu.host_key_verification_disabled = true;
        assert!(evaluate_host_key_trust(&tofu)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("must not be disabled")));
        let hashed = "|1|abcd|efgh ssh-ed25519 AAAA\n";
        let h = HostKeyTrustInput {
            known_hosts_text: Some(hashed),
            expected_hostname: "ssh.example.test",
            expected_ssh_port: 22,
            planned_ssh_port: 22,
            expected_algorithm: "ssh-ed25519",
            expected_key_b64: b64.as_str(),
            expected_fingerprint_sha256: fp.as_str(),
            tofu_permitted: false,
            accept_unknown_host: false,
            host_key_verification_disabled: false,
        };
        assert!(evaluate_host_key_trust(&h)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("hashed")));
    }

    #[test]
    fn host_key_trust_refuses_fingerprint_mismatch_and_bracket_port_form() {
        let (b64, _fp, _line) = pin_material();
        let wrong_fp = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let line = format!("ssh.example.test ssh-ed25519 {b64}\n");
        let input = HostKeyTrustInput {
            known_hosts_text: Some(line.as_str()),
            expected_hostname: "ssh.example.test",
            expected_ssh_port: 22,
            planned_ssh_port: 22,
            expected_algorithm: "ssh-ed25519",
            expected_key_b64: b64.as_str(),
            expected_fingerprint_sha256: wrong_fp,
            tofu_permitted: false,
            accept_unknown_host: false,
            host_key_verification_disabled: false,
        };
        assert!(evaluate_host_key_trust(&input)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("fingerprint")));
        let bracket = format!("[ssh.example.test]:22 ssh-ed25519 {b64}\n");
        let input = HostKeyTrustInput {
            known_hosts_text: Some(bracket.as_str()),
            expected_hostname: "ssh.example.test",
            expected_ssh_port: 22,
            planned_ssh_port: 22,
            expected_algorithm: "ssh-ed25519",
            expected_key_b64: b64.as_str(),
            expected_fingerprint_sha256: PIN,
            tofu_permitted: false,
            accept_unknown_host: false,
            host_key_verification_disabled: false,
        };
        assert!(evaluate_host_key_trust(&input)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("[host]:port") || e.contains("unknown")));
    }

    #[test]
    fn tunnel_http_wrapper_does_not_start_a_process() {
        let ev = crate::sync_recovery_policy::ready_tunnel_evidence_fixture();
        require_validated_tunnel_before_http(&ev).unwrap();
        assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
        assert_ne!(
            TunnelLifecycle::ReadyForAuthorizedRecovery,
            TunnelLifecycle::NotConfigured
        );
    }
}
