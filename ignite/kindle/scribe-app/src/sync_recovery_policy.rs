//! Offline recovery destination and authorization policy (Slices 18E.10–18E.10B).
//!
//! Pure validation of a *proposed* recovery configuration. Does not open sockets, read
//! credentials, write identity or intent files, or submit operations. Passing validation
//! never authorizes a non-loopback POST: [`LIVE_NON_LOOPBACK_POST_ENABLED`] is false.
//!
//! [`authorize_recovery_origin`] is the shared loopback origin parser used by the executor
//! before any socket is created. A valid loopback origin is **not** proof that an
//! authenticated tunnel exists.
//!
//! [`evaluate_tunnel_runtime_evidence`] is the future interface between an authenticated
//! tunnel manager and the recovery HTTP executor: it requires pinned host identity,
//! loopback-only bind, exclusive listener PID, a restricted remote destination, and
//! matching library/replica/genesis/transaction identities. Setting
//! [`RecoveryPolicyInput::tunnel_identity_verified`] is not that evidence and cannot
//! authorize a live POST.

/// Product constant: a planned external origin is a configuration input, not a reachable
/// recovery POST destination. Loopback disposable tests keep using [`require_loopback_url`].
pub const LIVE_NON_LOOPBACK_POST_ENABLED: bool = false;

/// Kindle `ureq` is compiled with `default-features = false` (HTTP only). Direct HTTPS is a
/// future build, not a current capability.
pub const KINDLE_UREQ_HAS_TLS: bool = false;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryGate {
    AFreshEvidence,
    BCompatibility,
    CPreparation,
    DIrreversibleExecution,
    EVerifiedFinalization,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryTransport {
    /// HTTP to loopback; any encrypted hop is an external authenticated tunnel.
    LoopbackHttpViaAuthenticatedTunnel,
    /// Direct HTTPS with hostname and certificate verification (not in the Kindle build).
    DirectHttps,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    MemoryOnly,
    PairedDeviceStore,
    Interactive,
    ConfigFile,
    CommandLine,
    UrlEmbedded,
    SourceTree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryPolicyInput<'a> {
    pub gate: RecoveryGate,
    pub proposed_base_url: &'a str,
    pub transport: RecoveryTransport,
    /// Authenticated remote for a tunnel (hostname, not a loopback alias). Unused for loopback URL checks.
    pub tunnel_origin_host: &'a str,
    pub library_id: &'a str,
    pub replica_id: &'a str,
    pub genesis_sha256: &'a str,
    pub expected_library_id: &'a str,
    pub expected_replica_id: &'a str,
    pub expected_genesis_sha256: &'a str,
    pub expected_history_cursor: u64,
    pub observed_history_cursor: Option<u64>,
    pub intent_present: bool,
    pub fence_active: bool,
    pub completion_proof_present: bool,
    pub accepted_prefix_complete: bool,
    pub server_accept_replace: bool,
    pub client_decodes_replace: bool,
    pub participating_clients_compatible: bool,
    pub credential_source: CredentialSource,
    pub allowed_operation_types: &'a [&'a str],
    pub proposed_operation_types: &'a [&'a str],
    pub local_library_approved: bool,
    pub local_sync_approved: bool,
    pub redirects_permitted: bool,
    /// Operator request to POST to a non-loopback origin. Always refused in this slice.
    pub request_non_loopback_post: bool,
    /// True only for a *proposed* live-tunnel configuration. Disposable loopback execution
    /// sets this false: loopback HTTP is not tunnel authentication.
    pub tunnel_identity_verified: bool,
}

/// Canonical `http://127.0.0.1:PORT` or `http://[::1]:PORT`. Immutable after validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedOrigin {
    origin: String,
}

impl AuthorizedOrigin {
    pub fn as_str(&self) -> &str {
        &self.origin
    }
}

/// Strict loopback origin for recovery HTTP. Refuses DNS hostnames, missing ports, HTTPS,
/// userinfo, query, fragment, and non-canonical paths. Does not open a socket.
pub fn authorize_recovery_origin(url: &str) -> Result<AuthorizedOrigin, String> {
    if url.trim() != url {
        return Err("REFUSE RECOVERY TRANSPORT: surrounding whitespace in URL".into());
    }
    if url.is_empty() {
        return Err("REFUSE RECOVERY TRANSPORT: empty URL".into());
    }
    if LIVE_NON_LOOPBACK_POST_ENABLED {
        return Err("REFUSE RECOVERY TRANSPORT: non-loopback recovery POST is not enabled".into());
    }
    let lower = url.to_ascii_lowercase();
    for needle in ["tailscale", "ts.net", "kindle", "192.168.", "10.0.", "100."] {
        if lower.contains(needle) {
            return Err(format!(
                "REFUSE RECOVERY TRANSPORT: non-disposable endpoint {url}"
            ));
        }
    }
    if url_has_userinfo(url) {
        return Err("REFUSE RECOVERY TRANSPORT: URL must not contain credentials".into());
    }
    if url.contains('#') {
        return Err("REFUSE RECOVERY TRANSPORT: URL must not contain a fragment".into());
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err("REFUSE RECOVERY TRANSPORT: URL must be http(s)".into());
    };
    if scheme != "http" {
        if scheme.eq_ignore_ascii_case("https") {
            return Err("REFUSE RECOVERY TRANSPORT: HTTPS is not compiled into Kindle ureq".into());
        }
        return Err(format!(
            "REFUSE RECOVERY TRANSPORT: unexpected scheme {scheme}"
        ));
    }
    let (authority, after_host) = match rest.split_once('/') {
        Some((a, p)) => (a, p),
        None => (rest, ""),
    };
    let (authority, auth_query) = match authority.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (authority, None),
    };
    let (path, path_query) = match after_host.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (after_host, None),
    };
    if auth_query.is_some() || path_query.is_some() {
        return Err("REFUSE RECOVERY TRANSPORT: URL must not contain a query".into());
    }
    if !path.is_empty() {
        return Err("REFUSE RECOVERY TRANSPORT: unexpected path on origin".into());
    }
    if authority.is_empty() {
        return Err("REFUSE RECOVERY TRANSPORT: empty host".into());
    }
    let (host, port_s) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err("REFUSE RECOVERY TRANSPORT: malformed IPv6 host".into());
        };
        if host != "::1" {
            return Err(format!(
                "REFUSE RECOVERY TRANSPORT: server host [{host}] is not loopback"
            ));
        }
        let Some(port_s) = after.strip_prefix(':') else {
            return Err("REFUSE RECOVERY TRANSPORT: explicit port is required".into());
        };
        ("::1", port_s)
    } else {
        let Some((host, port_s)) = authority.rsplit_once(':') else {
            return Err("REFUSE RECOVERY TRANSPORT: explicit port is required".into());
        };
        if host != "127.0.0.1" {
            if host.eq_ignore_ascii_case("localhost") {
                return Err(
                    "REFUSE RECOVERY TRANSPORT: localhost requires DNS and is not allowed".into(),
                );
            }
            return Err(format!(
                "REFUSE RECOVERY TRANSPORT: server host {host} is not loopback"
            ));
        }
        ("127.0.0.1", port_s)
    };
    if port_s.is_empty() || !port_s.bytes().all(|b| b.is_ascii_digit()) {
        return Err("REFUSE RECOVERY TRANSPORT: invalid port".into());
    }
    if port_s.len() > 1 && port_s.starts_with('0') {
        return Err("REFUSE RECOVERY TRANSPORT: port must be a canonical decimal".into());
    }
    let port: u16 = port_s
        .parse()
        .map_err(|_| "REFUSE RECOVERY TRANSPORT: invalid port".to_string())?;
    if port == 0 {
        return Err("REFUSE RECOVERY TRANSPORT: port 0 is not allowed".into());
    }
    let origin = if host == "::1" {
        format!("http://[::1]:{port}")
    } else {
        format!("http://127.0.0.1:{port}")
    };
    Ok(AuthorizedOrigin { origin })
}

/// Compatibility wrapper used by existing recovery tests.
pub fn require_loopback_url(url: &str) -> Result<(), String> {
    authorize_recovery_origin(url).map(|_| ())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyDecision {
    pub gate: RecoveryGate,
    /// Configuration is internally consistent. Never means live POST is allowed.
    pub configuration_valid: bool,
    pub live_non_loopback_post_authorized: bool,
}

/// Claimed facts a future tunnel manager must supply before Gate D may treat a
/// loopback listener as an authenticated hop. None of these fields is the boolean
/// [`RecoveryPolicyInput::tunnel_identity_verified`].
///
/// This function does not open sockets, read keys, or contact a network. Callers must
/// populate fields from actual runtime observation. Empty or synthetic-true defaults
/// fail closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelRuntimeEvidence<'a> {
    pub expected_origin: &'a str,
    pub observed_listener_origin: &'a str,
    /// Literal bind address of the tunnel listener (`127.0.0.1` or `::1` only).
    pub bind_address: &'a str,
    pub gateway_ports: bool,
    pub unrestricted_forwarding: bool,
    pub unknown_host_key_accepted: bool,
    pub agent_forwarding: bool,
    pub interactive_shell: bool,
    pub pinned_host_key_sha256: &'a str,
    pub observed_host_key_sha256: &'a str,
    pub listener_owner_observed: bool,
    pub expected_tunnel_pid: u32,
    pub observed_listener_pid: u32,
    pub remote_forward_host: &'a str,
    pub remote_forward_port: u16,
    pub library_id: &'a str,
    pub replica_id: &'a str,
    pub genesis_sha256: &'a str,
    pub expected_library_id: &'a str,
    pub expected_replica_id: &'a str,
    pub expected_genesis_sha256: &'a str,
    pub transaction_id: &'a str,
    /// True when a reconnect presented a different pinned peer than the one recorded
    /// for this recovery transaction.
    pub peer_identity_changed_on_reconnect: bool,
    /// Absolute path of the process that must own the listener (`dropbearmulti`).
    pub expected_executable: &'a str,
    pub observed_executable: &'a str,
    /// Derived by the runtime attestation collector; must not be a caller-supplied boolean.
    /// True only when [`Self::ssh_auth_evidence_source`] is
    /// [`SshAuthEvidenceSource::PinnedForwardConjunction`].
    pub ssh_session_authenticated: bool,
    /// Independent classification of how authentication was concluded.
    /// [`SshAuthEvidenceSource::CallerAsserted`] is always refused.
    pub ssh_auth_evidence_source: SshAuthEvidenceSource,
    pub listener_present: bool,
    /// True when the observed PID number was reused by a different process.
    pub pid_recycled: bool,
    /// Linux `/proc/<pid>/stat` starttime (field 22). Zero is unobserved.
    pub expected_starttime: u64,
    pub observed_starttime: u64,
    /// Supervised-session generation. Zero is missing. A reconnect must increment.
    pub attestation_generation: u64,
    pub expected_listen_port: u16,
    pub observed_listen_port: u16,
    pub host_key_algorithm: &'a str,
    pub observed_host_key_algorithm: &'a str,
    pub lifecycle: TunnelLifecycle,
}

/// How SSH peer authentication was concluded. A free-form `true` is not a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshAuthEvidenceSource {
    /// No authentication outcome was observed.
    Unobserved,
    /// Caller supplied `ssh_session_authenticated = true`. Always refused.
    CallerAsserted,
    /// The process exists. Insufficient: Dropbear may still be pre-kex.
    ProcessAliveOnly,
    /// Argv requests StrictHostKeyChecking and known_hosts is pinned. Session not shown.
    PolicyConfiguredPinProvisioned,
    /// A loopback listener exists without a completed SSH session.
    ListenerWithoutAuth,
    /// Conjunction of independently observed facts: pinned known_hosts, restricted
    /// argv (`StrictHostKeyChecking=yes`, `ExitOnForwardFailure=yes`, no `-y`),
    /// supervised `dbclient` identity (PID + starttime + exe + argv), and a
    /// loopback listener owned by that process. Dropbear 2024.85 does not export
    /// the accepted peer key; Review 107 showed mismatch/unknown exits instead of
    /// leaving a listener. This is not a wire transcript of the current kex.
    PinnedForwardConjunction,
}

/// Maintenance-only tunnel states. [`TunnelLifecycle::ReadyForAuthorizedRecovery`]
/// is a transport prerequisite, not Gate D authorization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TunnelLifecycle {
    NotConfigured,
    ConfigurationValidated,
    HostKeyTrustPrepared,
    TunnelStarting,
    PeerAuthenticated,
    ListenerVerified,
    ReadyForAuthorizedRecovery,
    Disconnected,
    FailedIdentityValidation,
    Shutdown,
}

/// Fail-closed check of claimed tunnel runtime evidence. Never equivalent to setting
/// `tunnel_identity_verified = true`. Never authorizes non-loopback POST.
pub fn evaluate_tunnel_runtime_evidence(ev: &TunnelRuntimeEvidence<'_>) -> Result<(), Vec<String>> {
    let mut reasons = Vec::new();

    let expected = match authorize_recovery_origin(ev.expected_origin) {
        Ok(o) => o,
        Err(e) => {
            refuse(&mut reasons, e);
            return Err(reasons);
        }
    };
    match authorize_recovery_origin(ev.observed_listener_origin) {
        Ok(observed) => {
            if observed.as_str() != expected.as_str() {
                refuse(
                    &mut reasons,
                    "REFUSE TUNNEL EVIDENCE: observed listener origin does not match expected origin",
                );
            }
        }
        Err(e) => refuse(&mut reasons, e),
    }

    if ev.bind_address != "127.0.0.1" && ev.bind_address != "::1" {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: listener must bind exclusively to loopback",
        );
    }
    if ev.gateway_ports {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: GatewayPorts / external listener exposure is refused",
        );
    }
    if ev.unrestricted_forwarding {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: unrestricted forwarding would create an open proxy",
        );
    }
    if ev.unknown_host_key_accepted {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: unknown or unpinned host keys are refused",
        );
    }
    if ev.agent_forwarding {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: SSH agent forwarding is refused",
        );
    }
    if ev.interactive_shell {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: interactive shell / PTY is not required for recovery forwarding",
        );
    }
    if !is_sha256_hex(ev.pinned_host_key_sha256) {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: pinned host-key SHA-256 is missing or malformed",
        );
    }
    if !is_sha256_hex(ev.observed_host_key_sha256) {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: observed host-key SHA-256 is missing or malformed",
        );
    }
    if is_sha256_hex(ev.pinned_host_key_sha256)
        && is_sha256_hex(ev.observed_host_key_sha256)
        && ev.pinned_host_key_sha256 != ev.observed_host_key_sha256
    {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: remote host key does not match the pin",
        );
    }
    if ev.peer_identity_changed_on_reconnect {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: reconnect presented a changed remote identity",
        );
    }
    if !ev.listener_owner_observed {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: listener process identity was not observed",
        );
    }
    if ev.expected_tunnel_pid == 0 || ev.observed_listener_pid == 0 {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: tunnel PID must be a real process id",
        );
    } else if ev.expected_tunnel_pid != ev.observed_listener_pid {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: listener PID does not match the expected tunnel process",
        );
    }
    if ev.remote_forward_host.is_empty()
        || ev.remote_forward_host == "localhost"
        || ev.remote_forward_host == "127.0.0.1"
        || ev.remote_forward_host == "::1"
        || ev.remote_forward_host == "*"
        || ev.remote_forward_host == "0.0.0.0"
    {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: remote forwarding destination is not an authenticated host",
        );
    }
    if host_looks_like_ipv4(ev.remote_forward_host) {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: IP address alone is not proof of server identity",
        );
    }
    if ev.remote_forward_port == 0 {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: remote forwarding port is missing",
        );
    }
    if ev.library_id.is_empty() || ev.replica_id.is_empty() || ev.genesis_sha256.is_empty() {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: missing library, replica, or genesis identity",
        );
    } else {
        if ev.library_id != ev.expected_library_id {
            refuse(
                &mut reasons,
                "REFUSE TUNNEL EVIDENCE: library identity mismatch",
            );
        }
        if ev.replica_id != ev.expected_replica_id {
            refuse(
                &mut reasons,
                "REFUSE TUNNEL EVIDENCE: replica identity mismatch",
            );
        }
        if ev.genesis_sha256 != ev.expected_genesis_sha256 {
            refuse(
                &mut reasons,
                "REFUSE TUNNEL EVIDENCE: genesis identity mismatch",
            );
        }
    }
    if ev.transaction_id.is_empty() {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: recovery transaction identity is missing",
        );
    }
    if ev.expected_executable.is_empty()
        || std::path::Path::new(ev.expected_executable)
            .file_name()
            .and_then(|n| n.to_str())
            != Some("dropbearmulti")
    {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: expected executable is not dropbearmulti",
        );
    }
    if ev.observed_executable != ev.expected_executable {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: listener executable does not match the expected tunnel process",
        );
    }
    if !ev.ssh_session_authenticated {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: expected SSH authentication never completed",
        );
    }
    match ev.ssh_auth_evidence_source {
        SshAuthEvidenceSource::PinnedForwardConjunction => {
            if !ev.ssh_session_authenticated {
                refuse(
                    &mut reasons,
                    "REFUSE TUNNEL EVIDENCE: SSH authentication evidence is inconsistent",
                );
            }
        }
        SshAuthEvidenceSource::CallerAsserted => refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: SSH authentication cannot be supplied as a caller assertion",
        ),
        SshAuthEvidenceSource::Unobserved => refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: SSH authentication outcome is unknown",
        ),
        SshAuthEvidenceSource::ProcessAliveOnly => refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: process existence is not SSH authentication",
        ),
        SshAuthEvidenceSource::PolicyConfiguredPinProvisioned => refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: host-key policy is not a completed SSH session",
        ),
        SshAuthEvidenceSource::ListenerWithoutAuth => refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: listener is not proof of SSH authentication",
        ),
    }
    if ev.expected_starttime == 0 || ev.observed_starttime == 0 {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: process start identity is missing",
        );
    } else if ev.expected_starttime != ev.observed_starttime {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: process start identity does not match (PID reuse)",
        );
    }
    if ev.attestation_generation == 0 {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: attestation generation is missing",
        );
    }
    if !ev.listener_present {
        refuse(&mut reasons, "REFUSE TUNNEL EVIDENCE: listener disappeared");
    }
    if ev.pid_recycled {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: tunnel PID was reused by another process",
        );
    }
    if ev.expected_listen_port == 0 || ev.observed_listen_port == 0 {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: listen port is missing",
        );
    } else if ev.expected_listen_port != ev.observed_listen_port {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: listener bound to the wrong local port",
        );
    }
    if ev.host_key_algorithm.is_empty() || ev.host_key_algorithm != ev.observed_host_key_algorithm {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: host-key algorithm does not match the pin",
        );
    }
    if ev.lifecycle != TunnelLifecycle::ReadyForAuthorizedRecovery {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL EVIDENCE: tunnel lifecycle is not ready for separately authorized recovery",
        );
    }

    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn refuse(reasons: &mut Vec<String>, msg: impl Into<String>) {
    reasons.push(msg.into());
}

fn url_has_userinfo(url: &str) -> bool {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority.contains('@')
}

fn url_has_query_or_fragment(url: &str) -> bool {
    url.contains('?') || url.contains('#')
}

fn scheme_of(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    if rest.is_empty() || scheme.is_empty() {
        return None;
    }
    Some(scheme)
}

pub(crate) fn host_looks_like_ipv4(host: &str) -> bool {
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok())
}

fn validate_url_shape(url: &str, reasons: &mut Vec<String>) {
    let trimmed = url.trim();
    if trimmed != url {
        refuse(
            reasons,
            "REFUSE RECOVERY POLICY: surrounding whitespace in URL",
        );
    }
    if trimmed.is_empty() {
        refuse(reasons, "REFUSE RECOVERY POLICY: empty URL");
        return;
    }
    if url_has_userinfo(trimmed) {
        refuse(
            reasons,
            "REFUSE RECOVERY POLICY: URL must not contain credentials",
        );
    }
    if url_has_query_or_fragment(trimmed) {
        refuse(
            reasons,
            "REFUSE RECOVERY POLICY: URL must not contain query or fragment",
        );
    }
    match scheme_of(trimmed) {
        Some("http") | Some("https") => {}
        Some(s) => refuse(
            reasons,
            format!("REFUSE RECOVERY POLICY: unexpected scheme {s}"),
        ),
        None => refuse(reasons, "REFUSE RECOVERY POLICY: URL must be http(s)"),
    }
}

fn identities_match(input: &RecoveryPolicyInput<'_>, reasons: &mut Vec<String>) {
    if input.library_id.is_empty() || input.replica_id.is_empty() || input.genesis_sha256.is_empty()
    {
        refuse(reasons, "REFUSE RECOVERY POLICY: missing identity fields");
        return;
    }
    if input.library_id != input.expected_library_id {
        refuse(reasons, "REFUSE RECOVERY POLICY: library identity mismatch");
    }
    if input.replica_id != input.expected_replica_id {
        refuse(reasons, "REFUSE RECOVERY POLICY: replica identity mismatch");
    }
    if input.genesis_sha256 != input.expected_genesis_sha256 {
        refuse(reasons, "REFUSE RECOVERY POLICY: genesis identity mismatch");
    }
}

fn ops_allowed(input: &RecoveryPolicyInput<'_>, reasons: &mut Vec<String>) {
    for op in input.proposed_operation_types {
        if !input.allowed_operation_types.iter().any(|a| a == op) {
            refuse(
                reasons,
                format!("REFUSE RECOVERY POLICY: unsupported operation type {op}"),
            );
        }
    }
}

fn credential_ok(src: CredentialSource, reasons: &mut Vec<String>) {
    match src {
        CredentialSource::CommandLine => refuse(
            reasons,
            "REFUSE RECOVERY POLICY: credential must not be a command-line argument",
        ),
        CredentialSource::UrlEmbedded => refuse(
            reasons,
            "REFUSE RECOVERY POLICY: credential must not be embedded in the URL",
        ),
        CredentialSource::SourceTree => refuse(
            reasons,
            "REFUSE RECOVERY POLICY: credential must not live in source or fixtures",
        ),
        CredentialSource::MemoryOnly
        | CredentialSource::PairedDeviceStore
        | CredentialSource::Interactive
        | CredentialSource::ConfigFile => {}
    }
}

/// Evaluate a proposed recovery authorization. Never opens a socket.
pub fn evaluate_recovery_policy(
    input: &RecoveryPolicyInput<'_>,
) -> Result<PolicyDecision, Vec<String>> {
    let mut reasons = Vec::new();
    validate_url_shape(input.proposed_base_url, &mut reasons);
    identities_match(input, &mut reasons);
    ops_allowed(input, &mut reasons);
    credential_ok(input.credential_source, &mut reasons);

    if !input.local_library_approved {
        refuse(
            &mut reasons,
            "REFUSE RECOVERY POLICY: local library root is not approved",
        );
    }
    if !input.local_sync_approved {
        refuse(
            &mut reasons,
            "REFUSE RECOVERY POLICY: sync-state root is not approved",
        );
    }
    if input.redirects_permitted {
        refuse(
            &mut reasons,
            "REFUSE RECOVERY POLICY: redirects are not permitted",
        );
    }
    if input.request_non_loopback_post || LIVE_NON_LOOPBACK_POST_ENABLED {
        refuse(
            &mut reasons,
            "REFUSE RECOVERY POLICY: non-loopback recovery POST is not enabled",
        );
    }

    match input.transport {
        RecoveryTransport::LoopbackHttpViaAuthenticatedTunnel => {
            if let Err(e) = authorize_recovery_origin(input.proposed_base_url) {
                refuse(&mut reasons, e);
            }
            if scheme_of(input.proposed_base_url.trim()) == Some("https") && !KINDLE_UREQ_HAS_TLS {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: Kindle HTTP client has no TLS; loopback tunnel must be http",
                );
            }
            if input.tunnel_identity_verified {
                if input.tunnel_origin_host.is_empty()
                    || input.tunnel_origin_host == "localhost"
                    || input.tunnel_origin_host == "127.0.0.1"
                    || input.tunnel_origin_host == "::1"
                {
                    refuse(
                        &mut reasons,
                        "REFUSE RECOVERY POLICY: tunnel origin host is not an authenticated remote identity",
                    );
                }
                if host_looks_like_ipv4(input.tunnel_origin_host) {
                    refuse(
                        &mut reasons,
                        "REFUSE RECOVERY POLICY: IP address alone is not proof of server identity",
                    );
                }
            }
        }
        RecoveryTransport::DirectHttps => {
            if scheme_of(input.proposed_base_url.trim()) != Some("https") {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: DirectHttps requires https",
                );
            }
            if !KINDLE_UREQ_HAS_TLS {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: Kindle ureq is built without TLS",
                );
            }
        }
    }

    match input.gate {
        RecoveryGate::AFreshEvidence => {}
        RecoveryGate::BCompatibility => {
            if !input.client_decodes_replace || !input.participating_clients_compatible {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: participating clients cannot decode ReplaceStrokeLineages",
                );
            }
            if input.server_accept_replace {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: Gate B must not enable the server acceptance gate",
                );
            }
        }
        RecoveryGate::CPreparation => {
            if input.server_accept_replace {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: Gate C must not enable the server acceptance gate",
                );
            }
        }
        RecoveryGate::DIrreversibleExecution => {
            if !input.intent_present {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: Gate D requires a frozen recovery intent",
                );
            }
            if !input.fence_active {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: Gate D requires an active mutation fence",
                );
            }
            if !input.server_accept_replace {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: Gate D requires the server acceptance gate at this checkpoint",
                );
            }
            if !input.client_decodes_replace || !input.participating_clients_compatible {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: Gate D blocked: incompatible client may consume this library",
                );
            }
            if let Some(obs) = input.observed_history_cursor {
                if obs != input.expected_history_cursor {
                    refuse(
                        &mut reasons,
                        "REFUSE RECOVERY POLICY: stale authoritative head; revalidate the plan",
                    );
                }
            } else {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: Gate D requires a current accepted-history cursor",
                );
            }
        }
        RecoveryGate::EVerifiedFinalization => {
            if !input.accepted_prefix_complete {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: Gate E requires a complete accepted prefix",
                );
            }
            if !input.completion_proof_present {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: Gate E requires a durable completion proof",
                );
            }
            if !input.intent_present && !input.completion_proof_present {
                refuse(
                    &mut reasons,
                    "REFUSE RECOVERY POLICY: Gate E has no recovery state",
                );
            }
        }
    }

    if reasons.is_empty() {
        Ok(PolicyDecision {
            gate: input.gate,
            configuration_valid: true,
            live_non_loopback_post_authorized: false,
        })
    } else {
        Err(reasons)
    }
}

#[cfg(test)]
pub(crate) fn ready_tunnel_evidence_fixture() -> TunnelRuntimeEvidence<'static> {
    const PIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    TunnelRuntimeEvidence {
        expected_origin: "http://127.0.0.1:18765",
        observed_listener_origin: "http://127.0.0.1:18765",
        bind_address: "127.0.0.1",
        gateway_ports: false,
        unrestricted_forwarding: false,
        unknown_host_key_accepted: false,
        agent_forwarding: false,
        interactive_shell: false,
        pinned_host_key_sha256: PIN,
        observed_host_key_sha256: PIN,
        listener_owner_observed: true,
        expected_tunnel_pid: 4242,
        observed_listener_pid: 4242,
        remote_forward_host: "sync.example.test",
        remote_forward_port: 8080,
        library_id: "lib-aaaa",
        replica_id: "rep-bbbb",
        genesis_sha256: "gen-cccc",
        expected_library_id: "lib-aaaa",
        expected_replica_id: "rep-bbbb",
        expected_genesis_sha256: "gen-cccc",
        transaction_id: "txn-synthetic-1",
        peer_identity_changed_on_reconnect: false,
        expected_executable: "/mnt/us/usbnetlite/bin/dropbearmulti",
        observed_executable: "/mnt/us/usbnetlite/bin/dropbearmulti",
        ssh_session_authenticated: true,
        ssh_auth_evidence_source: SshAuthEvidenceSource::PinnedForwardConjunction,
        listener_present: true,
        pid_recycled: false,
        expected_starttime: 10_001,
        observed_starttime: 10_001,
        attestation_generation: 1,
        expected_listen_port: 18765,
        observed_listen_port: 18765,
        host_key_algorithm: "ssh-ed25519",
        observed_host_key_algorithm: "ssh-ed25519",
        lifecycle: TunnelLifecycle::ReadyForAuthorizedRecovery,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPS: &[&str] = &["AddStroke", "ReplaceStrokeLineages", "CreatePage"];

    fn base<'a>(
        url: &'a str,
        transport: RecoveryTransport,
        origin: &'a str,
    ) -> RecoveryPolicyInput<'a> {
        RecoveryPolicyInput {
            gate: RecoveryGate::AFreshEvidence,
            proposed_base_url: url,
            transport,
            tunnel_origin_host: origin,
            library_id: "lib-aaaa",
            replica_id: "rep-bbbb",
            genesis_sha256: "gen-cccc",
            expected_library_id: "lib-aaaa",
            expected_replica_id: "rep-bbbb",
            expected_genesis_sha256: "gen-cccc",
            expected_history_cursor: 10,
            observed_history_cursor: Some(10),
            intent_present: false,
            fence_active: false,
            completion_proof_present: false,
            accepted_prefix_complete: false,
            server_accept_replace: false,
            client_decodes_replace: true,
            participating_clients_compatible: true,
            credential_source: CredentialSource::MemoryOnly,
            allowed_operation_types: OPS,
            proposed_operation_types: OPS,
            local_library_approved: true,
            local_sync_approved: true,
            redirects_permitted: false,
            request_non_loopback_post: false,
            tunnel_identity_verified: true,
        }
    }

    fn loopback_ok() -> RecoveryPolicyInput<'static> {
        base(
            "http://127.0.0.1:8787",
            RecoveryTransport::LoopbackHttpViaAuthenticatedTunnel,
            "sync.example.test",
        )
    }

    #[test]
    fn valid_loopback_tunnel_config_passes_without_authorizing_live_post() {
        let d = evaluate_recovery_policy(&loopback_ok()).unwrap();
        assert!(d.configuration_valid);
        assert!(!d.live_non_loopback_post_authorized);
        assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
        assert!(!KINDLE_UREQ_HAS_TLS);
    }

    #[test]
    fn missing_authorization_fields_fail_closed() {
        let mut i = loopback_ok();
        i.local_library_approved = false;
        i.local_sync_approved = false;
        let err = evaluate_recovery_policy(&i).unwrap_err();
        assert!(err.iter().any(|e| e.contains("library root")));
        assert!(err.iter().any(|e| e.contains("sync-state")));
    }

    #[test]
    fn wrong_library_replica_genesis_fail() {
        let mut i = loopback_ok();
        i.expected_library_id = "other-lib";
        i.expected_replica_id = "other-rep";
        i.expected_genesis_sha256 = "other-gen";
        let err = evaluate_recovery_policy(&i).unwrap_err();
        assert!(err.iter().any(|e| e.contains("library identity")));
        assert!(err.iter().any(|e| e.contains("replica identity")));
        assert!(err.iter().any(|e| e.contains("genesis identity")));
    }

    #[test]
    fn unexpected_origin_and_insecure_transport_fail() {
        let mut i = loopback_ok();
        i.proposed_base_url = "http://192.168.1.9:8787";
        let err = evaluate_recovery_policy(&i).unwrap_err();
        assert!(err
            .iter()
            .any(|e| e.contains("loopback") || e.contains("non-disposable")));

        let mut https = loopback_ok();
        https.proposed_base_url = "https://127.0.0.1:8787";
        let err = evaluate_recovery_policy(&https).unwrap_err();
        assert!(err.iter().any(|e| e.contains("no TLS")));
    }

    #[test]
    fn unexpected_redirect_query_userinfo_and_scheme_fail() {
        let mut i = loopback_ok();
        i.redirects_permitted = true;
        assert!(evaluate_recovery_policy(&i)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("redirect")));

        let mut q = loopback_ok();
        q.proposed_base_url = "http://127.0.0.1:8787/v1?x=1";
        assert!(evaluate_recovery_policy(&q)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("query")));

        let mut u = loopback_ok();
        u.proposed_base_url = "http://user:pass@127.0.0.1:8787";
        let err = evaluate_recovery_policy(&u).unwrap_err();
        assert!(err.iter().any(|e| e.contains("credentials")));

        let mut s = loopback_ok();
        s.proposed_base_url = "ftp://127.0.0.1:21";
        assert!(evaluate_recovery_policy(&s).is_err());
    }

    #[test]
    fn invalid_credential_sources_fail() {
        for src in [
            CredentialSource::CommandLine,
            CredentialSource::UrlEmbedded,
            CredentialSource::SourceTree,
        ] {
            let mut i = loopback_ok();
            i.credential_source = src;
            assert!(evaluate_recovery_policy(&i).is_err(), "{src:?}");
        }
    }

    #[test]
    fn unknown_server_identity_ip_only_tunnel_origin_fails() {
        let mut i = loopback_ok();
        i.tunnel_origin_host = "203.0.113.10";
        let err = evaluate_recovery_policy(&i).unwrap_err();
        assert!(err.iter().any(|e| e.contains("IP address")));
    }

    #[test]
    fn stale_head_blocks_gate_d() {
        let mut i = loopback_ok();
        i.gate = RecoveryGate::DIrreversibleExecution;
        i.intent_present = true;
        i.fence_active = true;
        i.server_accept_replace = true;
        i.observed_history_cursor = Some(99);
        let err = evaluate_recovery_policy(&i).unwrap_err();
        assert!(err.iter().any(|e| e.contains("stale authoritative head")));
    }

    #[test]
    fn intent_present_is_ok_for_idempotent_prepare_not_for_missing_d() {
        let mut c = loopback_ok();
        c.gate = RecoveryGate::CPreparation;
        c.intent_present = true;
        evaluate_recovery_policy(&c).unwrap();

        let mut d = loopback_ok();
        d.gate = RecoveryGate::DIrreversibleExecution;
        d.fence_active = true;
        d.server_accept_replace = true;
        d.observed_history_cursor = Some(10);
        let err = evaluate_recovery_policy(&d).unwrap_err();
        assert!(err.iter().any(|e| e.contains("frozen recovery intent")));
    }

    #[test]
    fn fence_not_active_blocks_gate_d() {
        let mut i = loopback_ok();
        i.gate = RecoveryGate::DIrreversibleExecution;
        i.intent_present = true;
        i.server_accept_replace = true;
        i.observed_history_cursor = Some(10);
        let err = evaluate_recovery_policy(&i).unwrap_err();
        assert!(err.iter().any(|e| e.contains("mutation fence")));
    }

    #[test]
    fn unsupported_operation_type_fails() {
        let mut i = loopback_ok();
        i.proposed_operation_types = &["GenesisReset"];
        let err = evaluate_recovery_policy(&i).unwrap_err();
        assert!(err.iter().any(|e| e.contains("unsupported operation")));
    }

    #[test]
    fn incompatible_client_blocks_gates_b_and_d() {
        let mut b = loopback_ok();
        b.gate = RecoveryGate::BCompatibility;
        b.participating_clients_compatible = false;
        assert!(evaluate_recovery_policy(&b).is_err());

        let mut d = loopback_ok();
        d.gate = RecoveryGate::DIrreversibleExecution;
        d.intent_present = true;
        d.fence_active = true;
        d.server_accept_replace = true;
        d.observed_history_cursor = Some(10);
        d.client_decodes_replace = false;
        assert!(evaluate_recovery_policy(&d).is_err());
    }

    #[test]
    fn gate_b_refuses_acceptance_gate_on() {
        let mut i = loopback_ok();
        i.gate = RecoveryGate::BCompatibility;
        i.server_accept_replace = true;
        let err = evaluate_recovery_policy(&i).unwrap_err();
        assert!(err.iter().any(|e| e.contains("Gate B must not enable")));
    }

    #[test]
    fn unexpected_library_path_and_unauthorized_post() {
        let mut i = loopback_ok();
        i.local_library_approved = false;
        assert!(evaluate_recovery_policy(&i).is_err());

        let mut p = loopback_ok();
        p.request_non_loopback_post = true;
        let err = evaluate_recovery_policy(&p).unwrap_err();
        assert!(err.iter().any(|e| e.contains("non-loopback recovery POST")));
    }

    #[test]
    fn finalization_without_complete_proof_fails() {
        let mut i = loopback_ok();
        i.gate = RecoveryGate::EVerifiedFinalization;
        i.intent_present = true;
        i.accepted_prefix_complete = true;
        i.completion_proof_present = false;
        let err = evaluate_recovery_policy(&i).unwrap_err();
        assert!(err.iter().any(|e| e.contains("completion proof")));
    }

    #[test]
    fn direct_https_is_refused_without_kindle_tls() {
        let i = base(
            "https://sync.example.test",
            RecoveryTransport::DirectHttps,
            "sync.example.test",
        );
        let err = evaluate_recovery_policy(&i).unwrap_err();
        assert!(err.iter().any(|e| e.contains("without TLS")));
    }

    #[test]
    fn gate_d_valid_loopback_still_does_not_authorize_non_loopback_post() {
        let mut i = loopback_ok();
        i.gate = RecoveryGate::DIrreversibleExecution;
        i.intent_present = true;
        i.fence_active = true;
        i.server_accept_replace = true;
        i.observed_history_cursor = Some(10);
        let d = evaluate_recovery_policy(&i).unwrap();
        assert!(d.configuration_valid);
        assert!(!d.live_non_loopback_post_authorized);
    }

    #[test]
    fn submit_target_debug_redacts_token() {
        let t = crate::sync_recovery_exec::SubmitTarget {
            base_url: "http://127.0.0.1:1".into(),
            token: "super-secret-token".into(),
        };
        let s = format!("{t:?}");
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("super-secret-token"));
    }

    #[test]
    fn authorize_origin_accepts_literal_loopback_with_port() {
        let a = authorize_recovery_origin("http://127.0.0.1:8787").unwrap();
        assert_eq!(a.as_str(), "http://127.0.0.1:8787");
        let v6 = authorize_recovery_origin("http://[::1]:8787").unwrap();
        assert_eq!(v6.as_str(), "http://[::1]:8787");
        let slash = authorize_recovery_origin("http://127.0.0.1:9/").unwrap();
        assert_eq!(slash.as_str(), "http://127.0.0.1:9");
    }

    #[test]
    fn authorize_origin_refuses_localhost_dns_https_userinfo_query_fragment_and_lan() {
        assert!(authorize_recovery_origin("http://localhost:8787")
            .unwrap_err()
            .contains("localhost"));
        assert!(authorize_recovery_origin("http://127.0.0.1")
            .unwrap_err()
            .contains("port"));
        assert!(authorize_recovery_origin("https://127.0.0.1:8787")
            .unwrap_err()
            .contains("HTTPS"));
        assert!(authorize_recovery_origin("http://user:pass@127.0.0.1:8787")
            .unwrap_err()
            .contains("credential"));
        assert!(authorize_recovery_origin("http://127.0.0.1:8787?x=1")
            .unwrap_err()
            .contains("query"));
        assert!(authorize_recovery_origin("http://127.0.0.1:8787#frag")
            .unwrap_err()
            .contains("fragment"));
        assert!(authorize_recovery_origin("http://192.168.1.9:8787").is_err());
        assert!(authorize_recovery_origin("http://127.0.0.1:8787/v1").is_err());
    }

    #[test]
    fn loopback_exec_does_not_claim_tunnel_identity() {
        let mut i = loopback_ok();
        i.tunnel_identity_verified = false;
        i.tunnel_origin_host = "";
        evaluate_recovery_policy(&i).unwrap();
    }

    const PIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PIN_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn tunnel_ok() -> TunnelRuntimeEvidence<'static> {
        super::ready_tunnel_evidence_fixture()
    }

    fn refuse_contains(ev: TunnelRuntimeEvidence<'_>, needle: &str) {
        let err = evaluate_tunnel_runtime_evidence(&ev).unwrap_err();
        assert!(
            err.iter().any(|e| e.contains(needle)),
            "expected {needle:?} in {err:?}"
        );
    }

    #[test]
    fn tunnel_evidence_accepts_complete_claimed_facts() {
        evaluate_tunnel_runtime_evidence(&tunnel_ok()).unwrap();
    }

    #[test]
    fn tunnel_identity_verified_flag_is_not_runtime_evidence() {
        let mut i = loopback_ok();
        i.tunnel_identity_verified = true;
        i.tunnel_origin_host = "sync.example.test";
        evaluate_recovery_policy(&i).unwrap();
        let mut ev = tunnel_ok();
        ev.pinned_host_key_sha256 = "";
        ev.observed_host_key_sha256 = "";
        ev.listener_owner_observed = false;
        ev.expected_tunnel_pid = 0;
        ev.observed_listener_pid = 0;
        ev.transaction_id = "";
        assert!(evaluate_tunnel_runtime_evidence(&ev).is_err());
    }

    #[test]
    fn tunnel_evidence_refuses_unknown_host_key() {
        let mut ev = tunnel_ok();
        ev.unknown_host_key_accepted = true;
        refuse_contains(ev, "unknown or unpinned");
    }

    #[test]
    fn tunnel_evidence_refuses_changed_host_key() {
        let mut ev = tunnel_ok();
        ev.observed_host_key_sha256 = PIN_B;
        refuse_contains(ev, "does not match the pin");
    }

    #[test]
    fn tunnel_evidence_refuses_malformed_host_key_pin() {
        let mut ev = tunnel_ok();
        ev.pinned_host_key_sha256 = "not-a-hash";
        refuse_contains(ev, "pinned host-key");
    }

    #[test]
    fn tunnel_evidence_refuses_ip_only_remote_destination() {
        let mut ev = tunnel_ok();
        ev.remote_forward_host = "10.0.0.8";
        refuse_contains(ev, "IP address alone");
    }

    #[test]
    fn tunnel_evidence_refuses_loopback_remote_destination() {
        let mut ev = tunnel_ok();
        ev.remote_forward_host = "127.0.0.1";
        refuse_contains(ev, "not an authenticated host");
    }

    #[test]
    fn tunnel_evidence_refuses_occupied_or_mismatched_listener_pid() {
        let mut ev = tunnel_ok();
        ev.observed_listener_pid = 7;
        refuse_contains(ev, "listener PID does not match");
    }

    #[test]
    fn tunnel_evidence_refuses_unobserved_listener_owner() {
        let mut ev = tunnel_ok();
        ev.listener_owner_observed = false;
        refuse_contains(ev, "process identity was not observed");
    }

    #[test]
    fn tunnel_evidence_refuses_non_loopback_bind() {
        let mut ev = tunnel_ok();
        ev.bind_address = "0.0.0.0";
        refuse_contains(ev, "exclusively to loopback");
    }

    #[test]
    fn tunnel_evidence_refuses_unspecified_ipv6_bind() {
        let mut ev = tunnel_ok();
        ev.bind_address = "::";
        refuse_contains(ev, "exclusively to loopback");
    }

    #[test]
    fn tunnel_evidence_refuses_gateway_ports() {
        let mut ev = tunnel_ok();
        ev.gateway_ports = true;
        refuse_contains(ev, "external listener exposure");
    }

    #[test]
    fn tunnel_evidence_refuses_unrestricted_forwarding() {
        let mut ev = tunnel_ok();
        ev.unrestricted_forwarding = true;
        refuse_contains(ev, "open proxy");
    }

    #[test]
    fn tunnel_evidence_refuses_agent_forwarding_and_shell() {
        let mut ev = tunnel_ok();
        ev.agent_forwarding = true;
        refuse_contains(ev, "agent forwarding");
        let mut ev = tunnel_ok();
        ev.interactive_shell = true;
        refuse_contains(ev, "interactive shell");
    }

    #[test]
    fn tunnel_evidence_refuses_changed_peer_on_reconnect() {
        let mut ev = tunnel_ok();
        ev.peer_identity_changed_on_reconnect = true;
        refuse_contains(ev, "changed remote identity");
    }

    #[test]
    fn tunnel_evidence_refuses_wrong_library() {
        let mut ev = tunnel_ok();
        ev.library_id = "lib-other";
        refuse_contains(ev, "library identity mismatch");
    }

    #[test]
    fn tunnel_evidence_refuses_missing_transaction() {
        let mut ev = tunnel_ok();
        ev.transaction_id = "";
        refuse_contains(ev, "transaction identity is missing");
    }

    #[test]
    fn tunnel_evidence_refuses_listener_origin_mismatch() {
        let mut ev = tunnel_ok();
        ev.observed_listener_origin = "http://127.0.0.1:9";
        refuse_contains(ev, "does not match expected origin");
    }

    #[test]
    fn occupied_loopback_port_cannot_be_rebound() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(
            std::net::TcpListener::bind(addr).is_err(),
            "OS must refuse a second bind on an occupied loopback port"
        );
        let mut ev = tunnel_ok();
        ev.listener_owner_observed = false;
        ev.observed_listener_pid = 1;
        ev.expected_tunnel_pid = 2;
        assert!(evaluate_tunnel_runtime_evidence(&ev).is_err());
    }

    #[test]
    fn wildcard_bind_is_not_loopback_evidence() {
        let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(
            !addr.ip().is_loopback(),
            "0.0.0.0 is not a loopback listener"
        );
        let mut ev = tunnel_ok();
        ev.bind_address = "0.0.0.0";
        refuse_contains(ev, "exclusively to loopback");
    }

    #[test]
    fn tunnel_evidence_refuses_wrong_executable() {
        let mut ev = tunnel_ok();
        ev.observed_executable = "/bin/busybox";
        refuse_contains(ev, "listener executable does not match");
    }

    #[test]
    fn tunnel_evidence_refuses_unauthenticated_ssh_session() {
        let mut ev = tunnel_ok();
        ev.ssh_session_authenticated = false;
        refuse_contains(ev, "SSH authentication never completed");
    }

    #[test]
    fn tunnel_evidence_refuses_caller_asserted_authentication() {
        let mut ev = tunnel_ok();
        ev.ssh_auth_evidence_source = SshAuthEvidenceSource::CallerAsserted;
        refuse_contains(ev, "cannot be supplied as a caller assertion");
    }

    #[test]
    fn tunnel_evidence_refuses_missing_or_mismatched_starttime() {
        let mut ev = tunnel_ok();
        ev.observed_starttime = 0;
        refuse_contains(ev, "process start identity is missing");
        let mut ev = tunnel_ok();
        ev.observed_starttime = 99;
        refuse_contains(ev, "PID reuse");
    }

    #[test]
    fn tunnel_evidence_refuses_missing_listener_and_recycled_pid() {
        let mut ev = tunnel_ok();
        ev.listener_present = false;
        refuse_contains(ev, "listener disappeared");
        let mut ev = tunnel_ok();
        ev.pid_recycled = true;
        refuse_contains(ev, "PID was reused");
    }

    #[test]
    fn tunnel_evidence_refuses_wrong_listen_port_and_algorithm() {
        let mut ev = tunnel_ok();
        ev.observed_listen_port = 22;
        refuse_contains(ev, "wrong local port");
        let mut ev = tunnel_ok();
        ev.observed_host_key_algorithm = "ssh-rsa";
        refuse_contains(ev, "host-key algorithm");
    }

    #[test]
    fn tunnel_evidence_refuses_lifecycle_other_than_ready() {
        let mut ev = tunnel_ok();
        ev.lifecycle = TunnelLifecycle::TunnelStarting;
        refuse_contains(ev, "not ready for separately authorized recovery");
        let mut ev = tunnel_ok();
        ev.lifecycle = TunnelLifecycle::Disconnected;
        refuse_contains(ev, "not ready");
    }

    #[test]
    fn ready_lifecycle_is_not_gate_d_authorization() {
        evaluate_tunnel_runtime_evidence(&tunnel_ok()).unwrap();
        assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
        let mut i = loopback_ok();
        i.gate = RecoveryGate::DIrreversibleExecution;
        i.tunnel_identity_verified = true;
        i.tunnel_origin_host = "ssh.example.test";
        assert!(evaluate_recovery_policy(&i).is_err());
    }
}
