//! Direct Dropbear `-B` stdin/stdout forwarding contract (Slice 18E.10J).
//!
//! Alternative to Kindle-local `-L` TCP listeners. Builds a discrete argv and a
//! bounded HTTP/1.1 exchange over a caller-owned byte stream. Does not spawn
//! Dropbear, does not call `ureq`, and is not reachable from ordinary startup or
//! [`crate::sync_recovery_exec::submit_recovery`].
//!
//! Device evidence (Review 105): installed `dropbearmulti dbclient -h` lists
//! `-B <endhost:endport> Netcat-alike forwarding`. Runtime of `-B` on the Kindle
//! is not re-tested here.

use std::io::{Read, Write};
use std::time::Duration;

use crate::sync_recovery_policy::LIVE_NON_LOOPBACK_POST_ENABLED;
use crate::sync_recovery_tunnel::{
    allowed_host_key_algorithm, is_sha256_hex, require_safe_hostname, safe_abs_path, safe_username,
    DropbearInvocation, HostKeyTrustInput, DROPBEAR_CLIENT_APPLET,
};

pub const STREAM_MAX_HEADER_BYTES: usize = 8 * 1024;
pub const STREAM_MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
pub const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(20);
pub const STREAM_EXCHANGE_DEADLINE: Duration = Duration::from_secs(60);
pub const STREAM_MAX_CONCURRENT_CHILDREN: usize = 1;

/// Forwarding plan with no Kindle-local TCP listen address or port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryStreamPlan<'a> {
    pub executable: &'a str,
    pub applet: &'a str,
    pub ssh_user: &'a str,
    pub ssh_host: &'a str,
    pub ssh_port: u16,
    pub remote_forward_host: &'a str,
    pub remote_forward_port: u16,
    pub host_key_algorithm: &'a str,
    pub pinned_host_key_sha256: &'a str,
    pub identity_file: &'a str,
    pub known_hosts_home: &'a str,
    pub library_id: &'a str,
    pub replica_id: &'a str,
    pub genesis_sha256: &'a str,
    pub transaction_id: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryHttp1Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub host: &'a str,
    pub bearer: &'a str,
    pub body: Option<&'a [u8]>,
    /// Test-only harmless fixture GET `/`. Production constructors never set this.
    pub(crate) fixture_get: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryHttp1Response {
    pub status: u16,
    pub body: Vec<u8>,
}

pub fn validate_recovery_stream_plan(plan: &RecoveryStreamPlan<'_>) -> Result<(), Vec<String>> {
    let mut reasons = Vec::new();
    if LIVE_NON_LOOPBACK_POST_ENABLED {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: non-loopback recovery POST is not enabled",
        );
    }
    let base = std::path::Path::new(plan.executable)
        .file_name()
        .and_then(|n| n.to_str());
    if plan.executable.is_empty() || !is_dropbearmulti_basename(base) {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: executable must be dropbearmulti",
        );
    }
    if plan.applet != DROPBEAR_CLIENT_APPLET && plan.applet != "ssh" {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: applet must be dbclient (or ssh)",
        );
    }
    if plan.applet == "dropbear" {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: server applet is not a recovery stream client",
        );
    }
    if plan.ssh_port == 0 {
        refuse(&mut reasons, "REFUSE STREAM PLAN: SSH port is missing");
    }
    if plan.remote_forward_port == 0 {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: remote forwarding port is missing",
        );
    }
    if !safe_username(plan.ssh_user) {
        refuse(&mut reasons, "REFUSE STREAM PLAN: SSH username is unsafe");
    }
    if let Err(e) = require_safe_hostname(plan.ssh_host, "SSH host") {
        refuse(&mut reasons, e.replace("TUNNEL PLAN", "STREAM PLAN"));
    }
    if let Err(e) = require_safe_hostname(plan.remote_forward_host, "forwarding target") {
        refuse(&mut reasons, e.replace("TUNNEL PLAN", "STREAM PLAN"));
    }
    if !allowed_host_key_algorithm(plan.host_key_algorithm) {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: host-key algorithm is not allowed",
        );
    }
    if !is_sha256_hex(plan.pinned_host_key_sha256) {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: pinned host-key SHA-256 is missing or malformed",
        );
    }
    if !safe_abs_path(plan.identity_file) {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: identity file path is missing or unsafe",
        );
    }
    if plan.identity_file.contains("dropbear_") && plan.identity_file.contains("host_key") {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: must not use the Dropbear server host key as a client identity",
        );
    }
    if !safe_abs_path(plan.known_hosts_home) {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: known_hosts HOME is missing or unsafe",
        );
    }
    if plan.known_hosts_home == "/mnt/us" || plan.known_hosts_home == "/tmp/root" {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: known_hosts HOME must be a dedicated recovery directory",
        );
    }
    if plan.library_id.is_empty() || plan.replica_id.is_empty() || plan.genesis_sha256.is_empty() {
        refuse(&mut reasons, "REFUSE STREAM PLAN: missing identity fields");
    }
    if plan.transaction_id.is_empty() {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: recovery transaction identity is missing",
        );
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

/// Discrete `dropbearmulti dbclient -B host:port` argv. Never `sh -c`. Never `-L`.
pub fn build_dropbear_stream_invocation(
    plan: &RecoveryStreamPlan<'_>,
    host_key: &HostKeyTrustInput<'_>,
) -> Result<DropbearInvocation, Vec<String>> {
    validate_recovery_stream_plan(plan)?;
    crate::sync_recovery_tunnel::evaluate_host_key_trust(host_key)?;
    if host_key.expected_hostname != plan.ssh_host {
        return Err(vec![
            "REFUSE STREAM PLAN: host-key hostname does not match the stream plan".into(),
        ]);
    }
    if host_key.expected_ssh_port != plan.ssh_port || host_key.planned_ssh_port != plan.ssh_port {
        return Err(vec![
            "REFUSE STREAM PLAN: host-key SSH port does not match the stream plan".into(),
        ]);
    }
    let applet = if plan.applet == "ssh" {
        "ssh"
    } else {
        DROPBEAR_CLIENT_APPLET
    };
    let forward = format!("{}:{}", plan.remote_forward_host, plan.remote_forward_port);
    let dest = format!("{}@{}", plan.ssh_user, plan.ssh_host);
    let argv = vec![
        plan.executable.to_string(),
        applet.to_string(),
        "-T".into(),
        "-i".into(),
        plan.identity_file.to_string(),
        "-o".into(),
        "StrictHostKeyChecking=yes".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "GatewayPorts=no".into(),
        "-o".into(),
        "ForwardAgent=no".into(),
        "-o".into(),
        "PasswordAuthentication=no".into(),
        "-B".into(),
        forward,
        "-p".into(),
        plan.ssh_port.to_string(),
        dest,
    ];
    assert_safe_stream_argv(&argv)?;
    Ok(DropbearInvocation {
        program: plan.executable.to_string(),
        argv,
        home: plan.known_hosts_home.to_string(),
    })
}

/// Environment for a future supervised child. HOME is set; password/askpass vars are cleared.
pub fn recovery_stream_child_clear_vars() -> &'static [&'static str] {
    &[
        "DROPBEAR_PASSWORD",
        "SSH_ASKPASS",
        "DISPLAY",
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ]
}

/// Production/live spawn remains disabled. Disposable supervision uses
/// [`crate::sync_recovery_supervise::SupervisedRecoveryStream`] with a test-only permit.
pub fn spawn_recovery_stream_child(_inv: &DropbearInvocation) -> Result<(), String> {
    Err("REFUSE RECOVERY STREAM: child spawn is not enabled".into())
}

pub(crate) fn is_dropbearmulti_basename(base: Option<&str>) -> bool {
    matches!(base, Some("dropbearmulti") | Some("dropbearmulti.exe"))
}

pub fn recovery_http1_request<'a>(
    plan: &'a RecoveryStreamPlan<'a>,
    method: &'a str,
    path: &'a str,
    bearer: &'a str,
    body: Option<&'a [u8]>,
) -> Result<RecoveryHttp1Request<'a>, String> {
    validate_stream_path(method, path)?;
    if let Some(b) = body {
        if b.len() > STREAM_MAX_BODY_BYTES {
            return Err("REFUSE RECOVERY STREAM: request body exceeds bound".into());
        }
    }
    if method == "POST" && body.is_none() {
        return Err("REFUSE RECOVERY STREAM: POST requires a body".into());
    }
    if method == "GET" && body.is_some() {
        return Err("REFUSE RECOVERY STREAM: GET must not have a body".into());
    }
    Ok(RecoveryHttp1Request {
        method,
        path,
        host: plan.remote_forward_host,
        bearer,
        body,
        fixture_get: false,
    })
}

/// Harmless disposable GET `/` for fixture smoke tests. Not a recovery API path.
#[cfg(any(test, feature = "kindle_supervisor_harness"))]
pub fn recovery_http1_fixture_get<'a>(
    plan: &'a RecoveryStreamPlan<'a>,
    bearer: &'a str,
) -> RecoveryHttp1Request<'a> {
    RecoveryHttp1Request {
        method: "GET",
        path: "/",
        host: plan.remote_forward_host,
        bearer,
        body: None,
        fixture_get: true,
    }
}

/// Bounded HTTP/1.1 GET/POST over an already owned stream. Never opens a TCP port.
pub fn recovery_http1_exchange<S: Read + Write>(
    stream: &mut S,
    plan: &RecoveryStreamPlan<'_>,
    req: &RecoveryHttp1Request<'_>,
) -> Result<RecoveryHttp1Response, String> {
    if LIVE_NON_LOOPBACK_POST_ENABLED {
        return Err("REFUSE RECOVERY STREAM: non-loopback recovery POST is not enabled".into());
    }
    if req.fixture_get {
        if req.method != "GET" || req.path != "/" || req.body.is_some() {
            return Err("REFUSE RECOVERY STREAM: fixture GET must be path / without a body".into());
        }
    } else {
        validate_stream_path(req.method, req.path)?;
    }
    let expected_host = format!("{}:{}", plan.remote_forward_host, plan.remote_forward_port);
    if req.host != expected_host && req.host != plan.remote_forward_host {
        return Err(redact(
            "REFUSE RECOVERY STREAM: Host does not match the approved forwarding target",
            req.bearer,
        ));
    }
    if req.host.contains("127.0.0.1")
        || req.host.eq_ignore_ascii_case("localhost")
        || req.host.contains("::1")
    {
        return Err("REFUSE RECOVERY STREAM: loopback Host is not a stream destination".into());
    }
    let wire = encode_http1_request(req, &expected_host)?;
    let mut wrote = 0usize;
    match stream.write_all(&wire) {
        Ok(()) => wrote = wire.len(),
        Err(e) => {
            return Err(classify_io(
                e,
                wrote > 0 || !wire.is_empty(),
                req.bearer,
                true,
            ));
        }
    }
    if let Err(e) = stream.flush() {
        return Err(classify_io(e, true, req.bearer, true));
    }
    parse_http1_response(stream, req.bearer, wrote > 0)
}

fn encode_http1_request(req: &RecoveryHttp1Request<'_>, host: &str) -> Result<Vec<u8>, String> {
    let body = req.body.unwrap_or(&[]);
    if body.len() > STREAM_MAX_BODY_BYTES {
        return Err("REFUSE RECOVERY STREAM: request body exceeds bound".into());
    }
    let mut out = String::new();
    out.push_str(req.method);
    out.push(' ');
    out.push_str(req.path);
    out.push_str(" HTTP/1.1\r\n");
    out.push_str("Host: ");
    out.push_str(host);
    out.push_str("\r\n");
    if !req.bearer.is_empty() {
        out.push_str("Authorization: Bearer ");
        out.push_str(req.bearer);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n");
    if req.method == "POST" {
        out.push_str("Content-Type: application/json\r\n");
        out.push_str("Content-Length: ");
        out.push_str(&body.len().to_string());
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    Ok(bytes)
}

fn parse_http1_response<S: Read>(
    stream: &mut S,
    bearer: &str,
    wrote: bool,
) -> Result<RecoveryHttp1Response, String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 512];
    loop {
        if buf.len() > STREAM_MAX_HEADER_BYTES {
            return Err(classify_fail(
                "REFUSE RECOVERY STREAM: response headers exceed bound",
                bearer,
                wrote,
            ));
        }
        match stream.read(&mut tmp) {
            Ok(0) => {
                return Err(classify_fail(
                    "response truncated before complete headers",
                    bearer,
                    wrote,
                ));
            }
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(idx) = find_header_end(&buf) {
                    let header = buf[..idx].to_vec();
                    let extra = buf[idx + 4..].to_vec();
                    return finish_response(stream, &header, extra, bearer, wrote);
                }
            }
            Err(e) => return Err(classify_io(e, wrote, bearer, false)),
        }
    }
}

fn finish_response<S: Read>(
    stream: &mut S,
    header: &[u8],
    extra: Vec<u8>,
    bearer: &str,
    wrote: bool,
) -> Result<RecoveryHttp1Response, String> {
    let text = String::from_utf8_lossy(header);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.split_whitespace();
    let ver = parts.next().unwrap_or("");
    let code = parts.next().unwrap_or("");
    if ver != "HTTP/1.1" && ver != "HTTP/1.0" {
        return Err(classify_fail(
            "REFUSE RECOVERY STREAM: malformed HTTP response",
            bearer,
            wrote,
        ));
    }
    let status: u16 = code.parse().map_err(|_| {
        classify_fail(
            "REFUSE RECOVERY STREAM: malformed HTTP status",
            bearer,
            wrote,
        )
    })?;
    if (300..400).contains(&status) {
        return Err(classify_fail(
            &format!("REFUSE RECOVERY STREAM: redirect {status} not followed"),
            bearer,
            wrote,
        ));
    }
    let mut content_len: Option<usize> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(classify_fail(
                "REFUSE RECOVERY STREAM: malformed response header",
                bearer,
                wrote,
            ));
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(classify_fail(
                "REFUSE RECOVERY STREAM: chunked or transfer-encoding responses are not accepted",
                bearer,
                wrote,
            ));
        }
        if name.eq_ignore_ascii_case("content-length") {
            let n: usize = value.parse().map_err(|_| {
                classify_fail(
                    "REFUSE RECOVERY STREAM: invalid Content-Length",
                    bearer,
                    wrote,
                )
            })?;
            if n > STREAM_MAX_BODY_BYTES {
                return Err(classify_fail(
                    "REFUSE RECOVERY STREAM: response body exceeds bound",
                    bearer,
                    wrote,
                ));
            }
            content_len = Some(n);
        }
    }
    let Some(need) = content_len else {
        return Err(classify_fail(
            "REFUSE RECOVERY STREAM: Content-Length is required",
            bearer,
            wrote,
        ));
    };
    let mut body = extra;
    while body.len() < need {
        let mut tmp = [0u8; 1024];
        let take = (need - body.len()).min(tmp.len());
        match stream.read(&mut tmp[..take]) {
            Ok(0) => {
                return Err(classify_fail("response body truncated", bearer, wrote));
            }
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(e) => return Err(classify_io(e, wrote, bearer, false)),
        }
        if body.len() > STREAM_MAX_BODY_BYTES {
            return Err(classify_fail(
                "REFUSE RECOVERY STREAM: response body exceeds bound",
                bearer,
                wrote,
            ));
        }
    }
    body.truncate(need);
    Ok(RecoveryHttp1Response { status, body })
}

fn validate_stream_path(method: &str, path: &str) -> Result<(), String> {
    if !path.starts_with("/v1/libraries/") {
        return Err("REFUSE RECOVERY STREAM: unexpected path".into());
    }
    if path.contains("://") || path.contains('@') || path.contains('#') {
        return Err("REFUSE RECOVERY STREAM: malformed path".into());
    }
    match method {
        "POST" => {
            if path.contains('?') {
                return Err("REFUSE RECOVERY STREAM: POST path must not contain a query".into());
            }
            if !path.ends_with("/operations") {
                return Err("REFUSE RECOVERY STREAM: unexpected POST path".into());
            }
        }
        "GET" => {
            let Some((base, query)) = path.split_once('?') else {
                return Err("REFUSE RECOVERY STREAM: GET tail path requires a query".into());
            };
            if !base.ends_with("/operations") {
                return Err("REFUSE RECOVERY STREAM: unexpected GET path".into());
            }
            let mut after_ok = false;
            let mut limit_ok = false;
            for part in query.split('&') {
                if let Some(v) = part.strip_prefix("after=") {
                    if v.parse::<i64>().is_err() {
                        return Err("REFUSE RECOVERY STREAM: invalid after=".into());
                    }
                    after_ok = true;
                } else if let Some(v) = part.strip_prefix("limit=") {
                    if v.parse::<u64>().is_err() {
                        return Err("REFUSE RECOVERY STREAM: invalid limit=".into());
                    }
                    limit_ok = true;
                } else {
                    return Err("REFUSE RECOVERY STREAM: unexpected query parameter".into());
                }
            }
            if !after_ok || !limit_ok {
                return Err("REFUSE RECOVERY STREAM: GET query must be after and limit".into());
            }
        }
        _ => {
            return Err(format!(
                "REFUSE RECOVERY STREAM: unsupported method {method}"
            ))
        }
    }
    Ok(())
}

fn assert_safe_stream_argv(argv: &[String]) -> Result<(), Vec<String>> {
    let mut reasons = Vec::new();
    if !argv.iter().any(|a| a == "-B") {
        refuse(&mut reasons, "REFUSE STREAM PLAN: -B is required");
    }
    for needle in ["-y", "-yy", "-D", "-A", "-g", "-t", "-L", "-R", "-N", "-f"] {
        if argv.iter().any(|a| a == needle) {
            refuse(
                &mut reasons,
                format!("REFUSE STREAM PLAN: unsafe argument {needle}"),
            );
        }
    }
    if argv.iter().any(|a| a.contains("UserKnownHostsFile")) {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: UserKnownHostsFile is not a Dropbear -o option",
        );
    }
    let joined = argv.join("\0");
    if joined.contains("sh -c") || joined.contains("bash -c") {
        refuse(
            &mut reasons,
            "REFUSE STREAM PLAN: shell interpolation is refused",
        );
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn classify_io(e: std::io::Error, wrote: bool, bearer: &str, during_write: bool) -> String {
    let kind = if wrote || during_write {
        "AMBIGUOUS RECOVERY STREAM"
    } else {
        "REFUSE RECOVERY STREAM"
    };
    redact(&format!("{kind}: {e}"), bearer)
}

fn classify_fail(msg: &str, bearer: &str, wrote: bool) -> String {
    let text = if wrote {
        if msg.starts_with("AMBIGUOUS ") {
            msg.to_string()
        } else if let Some(rest) = msg.strip_prefix("REFUSE RECOVERY STREAM:") {
            format!("AMBIGUOUS RECOVERY STREAM:{rest}")
        } else {
            format!("AMBIGUOUS RECOVERY STREAM: {msg}")
        }
    } else if msg.starts_with("REFUSE ") || msg.starts_with("AMBIGUOUS ") {
        msg.to_string()
    } else {
        format!("REFUSE RECOVERY STREAM: {msg}")
    };
    redact(&text, bearer)
}

fn redact(text: &str, bearer: &str) -> String {
    let mut s = text.to_string();
    if !bearer.is_empty() {
        s = s.replace(bearer, "<redacted>");
    }
    s
}

fn refuse(reasons: &mut Vec<String>, msg: impl Into<String>) {
    reasons.push(msg.into());
}
