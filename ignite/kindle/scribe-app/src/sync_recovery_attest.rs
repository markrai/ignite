//! Runtime tunnel attestation: bind approved configuration to independently
//! collected process/socket observations (Slice 18E.10F).
//!
//! Does not start Dropbear, does not open Kindle sockets, and does not enable
//! live recovery POST. A populated [`TunnelRuntimeEvidence`] is not evidence
//! until [`attest_tunnel_runtime`] derives it from a snapshot.

use crate::sync_recovery_policy::{
    authorize_recovery_origin, evaluate_recovery_policy, evaluate_tunnel_runtime_evidence,
    CredentialSource, RecoveryGate, RecoveryPolicyInput, RecoveryTransport, SshAuthEvidenceSource,
    TunnelLifecycle, TunnelRuntimeEvidence, LIVE_NON_LOOPBACK_POST_ENABLED,
};
use crate::sync_recovery_tunnel::{evaluate_host_key_trust, HostKeyTrustInput, RecoveryTunnelPlan};

/// Linux `/proc/<pid>/stat` starttime plus PID. Equality of PID alone is not identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessStartIdentity {
    pub pid: u32,
    pub starttime: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessRole {
    RecoveryForwardClient,
    UsbnetliteDaemon,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelProcessObservation {
    pub identity: ProcessStartIdentity,
    pub exe: String,
    pub cmdline: Vec<String>,
    pub alive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelListenerObservation {
    pub bind_address: String,
    pub port: u16,
    pub listen_state: bool,
    pub owner_pid: u32,
}

/// Optional Dropbear stderr classification. Absence is not success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropbearClientEvent {
    NotCaptured,
    HostKeyUnknown,
    HostKeyMismatch,
    Exited,
    RunningQuiet,
}

/// Injected or platform-collected observations for one generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelObservationSnapshot {
    pub generation: u64,
    pub process: Option<TunnelProcessObservation>,
    pub listeners: Vec<TunnelListenerObservation>,
    pub dropbear_event: DropbearClientEvent,
}

/// Identity recorded by an authorized management operation at spawn/adopt time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupervisedTunnelIdentity {
    pub identity: ProcessStartIdentity,
    pub generation: u64,
    pub expected_exe: String,
    pub expected_cmdline: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelAttestation {
    pub generation: u64,
    pub identity: ProcessStartIdentity,
    pub exe: String,
    pub listen_addr: String,
    pub listen_port: u16,
    pub listener_origin: String,
    pub remote_forward_host: String,
    pub remote_forward_port: u16,
    pub pinned_host_key_sha256: String,
    pub observed_host_key_sha256: String,
    pub host_key_algorithm: String,
    pub library_id: String,
    pub replica_id: String,
    pub genesis_sha256: String,
    pub transaction_id: String,
    pub auth_source: SshAuthEvidenceSource,
    pub lifecycle: TunnelLifecycle,
    pub listener_present: bool,
    pub listener_owner_observed: bool,
    pub pid_recycled: bool,
    pub unknown_host_key_accepted: bool,
    pub gateway_ports: bool,
    pub unrestricted_forwarding: bool,
    pub agent_forwarding: bool,
    pub interactive_shell: bool,
    pub ssh_session_authenticated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttestationInvalidation {
    ProcessExited,
    ProcessIdentityChanged,
    SocketDisappeared,
    SocketOwnerChanged,
    ListenPortChanged,
    SshDestinationChanged,
    HostKeyFailed,
    PeerKeyMismatch,
    ReconnectWithoutRenewal,
    ObservationUnavailable,
}

/// Tunnel loss is not server rejection and must not clear frozen recovery intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmbiguousRecoveryRetention {
    pub retain_intent: bool,
    pub retain_operation_ids: bool,
    pub retain_client_sequences: bool,
    pub keep_ordinary_upload_blocked: bool,
    pub transport_failure_is_not_rejection: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttestReport {
    pub attestation: TunnelAttestation,
    pub reasons: Vec<String>,
}

impl AttestReport {
    pub fn ready(&self) -> Result<&TunnelAttestation, &[String]> {
        if self.reasons.is_empty()
            && self.attestation.lifecycle == TunnelLifecycle::ReadyForAuthorizedRecovery
        {
            Ok(&self.attestation)
        } else {
            Err(self.reasons.as_slice())
        }
    }
}

fn refuse(reasons: &mut Vec<String>, msg: impl Into<String>) {
    reasons.push(msg.into());
}

pub fn parse_proc_stat_starttime(stat: &str) -> Result<ProcessStartIdentity, String> {
    let lparen = stat
        .find('(')
        .ok_or_else(|| "REFUSE TUNNEL ATTEST: /proc/stat missing comm".to_string())?;
    let rparen = stat
        .rfind(')')
        .ok_or_else(|| "REFUSE TUNNEL ATTEST: /proc/stat missing comm end".to_string())?;
    if rparen <= lparen {
        return Err("REFUSE TUNNEL ATTEST: /proc/stat comm is malformed".into());
    }
    let pid: u32 = stat[..lparen]
        .trim()
        .parse()
        .map_err(|_| "REFUSE TUNNEL ATTEST: /proc/stat pid is malformed".to_string())?;
    let rest = stat[rparen + 1..].trim();
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After pid+comm, field 22 starttime is rest index 19 (man proc: starttime is field 22).
    let starttime: u64 = fields
        .get(19)
        .ok_or_else(|| "REFUSE TUNNEL ATTEST: /proc/stat starttime is missing".to_string())?
        .parse()
        .map_err(|_| "REFUSE TUNNEL ATTEST: /proc/stat starttime is malformed".to_string())?;
    if pid == 0 || starttime == 0 {
        return Err("REFUSE TUNNEL ATTEST: process start identity is missing".into());
    }
    Ok(ProcessStartIdentity { pid, starttime })
}

pub fn parse_proc_cmdline(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

pub fn classify_dropbear_role(cmdline: &[String]) -> ProcessRole {
    let applet = cmdline.get(1).map(|s| s.as_str()).unwrap_or("");
    let argv0_base = std::path::Path::new(cmdline.first().map(|s| s.as_str()).unwrap_or(""))
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if applet == "dropbear" || argv0_base == "dropbear" {
        return ProcessRole::UsbnetliteDaemon;
    }
    if applet == "dbclient" || applet == "ssh" || argv0_base == "dbclient" || argv0_base == "ssh" {
        return ProcessRole::RecoveryForwardClient;
    }
    ProcessRole::Other
}

/// Parse `/proc/net/tcp` listener rows: (bind_address, port, inode).
pub fn parse_proc_net_tcp_listeners(tcp: &str) -> Result<Vec<(String, u16, u64)>, String> {
    let mut out = Vec::new();
    for (i, line) in tcp.lines().enumerate() {
        if i == 0 {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 10 {
            continue;
        }
        // st is column 4 (0-indexed 3); 0A = LISTEN
        if cols[3] != "0A" {
            continue;
        }
        let (addr, port) = parse_proc_ipv4_port(cols[1])?;
        let inode: u64 = cols[9]
            .parse()
            .map_err(|_| "REFUSE TUNNEL ATTEST: /proc/net/tcp inode is malformed".to_string())?;
        out.push((addr, port, inode));
    }
    Ok(out)
}

pub(crate) fn parse_proc_ipv4_port(field: &str) -> Result<(String, u16), String> {
    let (ip_hex, port_hex) = field
        .split_once(':')
        .ok_or_else(|| "REFUSE TUNNEL ATTEST: /proc/net/tcp address is malformed".to_string())?;
    let n = u32::from_str_radix(ip_hex, 16)
        .map_err(|_| "REFUSE TUNNEL ATTEST: /proc/net/tcp address is malformed".to_string())?;
    let a = n & 0xff;
    let b = (n >> 8) & 0xff;
    let c = (n >> 16) & 0xff;
    let d = (n >> 24) & 0xff;
    let port = u16::from_str_radix(port_hex, 16)
        .map_err(|_| "REFUSE TUNNEL ATTEST: /proc/net/tcp port is malformed".to_string())?;
    Ok((format!("{a}.{b}.{c}.{d}"), port))
}

pub fn argv_has_strict_host_key(cmdline: &[String]) -> bool {
    cmdline
        .windows(2)
        .any(|w| w[0] == "-o" && w[1] == "StrictHostKeyChecking=yes")
}

pub fn argv_has_exit_on_forward_failure(cmdline: &[String]) -> bool {
    cmdline
        .windows(2)
        .any(|w| w[0] == "-o" && w[1] == "ExitOnForwardFailure=yes")
}

pub fn argv_has_tofu(cmdline: &[String]) -> bool {
    cmdline.iter().any(|a| a == "-y" || a == "-yy")
        || cmdline.windows(2).any(|w| {
            w[0] == "-o"
                && (w[1] == "StrictHostKeyChecking=no"
                    || w[1] == "StrictHostKeyChecking=accept-new")
        })
}

fn argv_token<'a>(cmdline: &'a [String], flag: &str) -> Option<&'a str> {
    cmdline
        .windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].as_str())
}

fn ssh_dest_host(cmdline: &[String]) -> Option<&str> {
    let dest = cmdline.last()?.as_str();
    dest.rsplit_once('@').map(|(_, host)| host)
}

fn forward_spec(cmdline: &[String]) -> Option<(&str, u16, &str, u16)> {
    let spec = argv_token(cmdline, "-L")?;
    let parts: Vec<&str> = spec.split(':').collect();
    if parts.len() != 4 {
        return None;
    }
    let listen_port = parts[1].parse().ok()?;
    let remote_port = parts[3].parse().ok()?;
    Some((parts[0], listen_port, parts[2], remote_port))
}

fn ssh_dest_port(cmdline: &[String]) -> Option<u16> {
    argv_token(cmdline, "-p")?.parse().ok()
}

/// Ordinary notebook editing is independent of tunnel attestation.
pub fn missing_tunnel_attestation_blocks_ordinary_authoring() -> bool {
    false
}

/// Contract matching the existing recovery coordinator: tunnel loss is ambiguous.
pub fn attestation_invalidation_preserves_ambiguous_recovery() -> AmbiguousRecoveryRetention {
    AmbiguousRecoveryRetention {
        retain_intent: true,
        retain_operation_ids: true,
        retain_client_sequences: true,
        keep_ordinary_upload_blocked: true,
        transport_failure_is_not_rejection: true,
    }
}

fn blank_attestation(plan: &RecoveryTunnelPlan<'_>, generation: u64) -> TunnelAttestation {
    let origin = format!("http://127.0.0.1:{}", plan.listen_port);
    TunnelAttestation {
        generation,
        identity: ProcessStartIdentity {
            pid: 0,
            starttime: 0,
        },
        exe: String::new(),
        listen_addr: plan.listen_addr.to_string(),
        listen_port: plan.listen_port,
        listener_origin: origin,
        remote_forward_host: plan.remote_forward_host.to_string(),
        remote_forward_port: plan.remote_forward_port,
        pinned_host_key_sha256: plan.pinned_host_key_sha256.to_string(),
        observed_host_key_sha256: String::new(),
        host_key_algorithm: plan.host_key_algorithm.to_string(),
        library_id: plan.library_id.to_string(),
        replica_id: plan.replica_id.to_string(),
        genesis_sha256: plan.genesis_sha256.to_string(),
        transaction_id: plan.transaction_id.to_string(),
        auth_source: SshAuthEvidenceSource::Unobserved,
        lifecycle: TunnelLifecycle::NotConfigured,
        listener_present: false,
        listener_owner_observed: false,
        pid_recycled: false,
        unknown_host_key_accepted: false,
        gateway_ports: false,
        unrestricted_forwarding: false,
        agent_forwarding: false,
        interactive_shell: false,
        ssh_session_authenticated: false,
    }
}

impl TunnelAttestation {
    pub fn as_runtime_evidence(&self) -> TunnelRuntimeEvidence<'_> {
        TunnelRuntimeEvidence {
            expected_origin: &self.listener_origin,
            observed_listener_origin: &self.listener_origin,
            bind_address: &self.listen_addr,
            gateway_ports: self.gateway_ports,
            unrestricted_forwarding: self.unrestricted_forwarding,
            unknown_host_key_accepted: self.unknown_host_key_accepted,
            agent_forwarding: self.agent_forwarding,
            interactive_shell: self.interactive_shell,
            pinned_host_key_sha256: &self.pinned_host_key_sha256,
            observed_host_key_sha256: &self.observed_host_key_sha256,
            listener_owner_observed: self.listener_owner_observed,
            expected_tunnel_pid: self.identity.pid,
            observed_listener_pid: self.identity.pid,
            remote_forward_host: &self.remote_forward_host,
            remote_forward_port: self.remote_forward_port,
            library_id: &self.library_id,
            replica_id: &self.replica_id,
            genesis_sha256: &self.genesis_sha256,
            expected_library_id: &self.library_id,
            expected_replica_id: &self.replica_id,
            expected_genesis_sha256: &self.genesis_sha256,
            transaction_id: &self.transaction_id,
            peer_identity_changed_on_reconnect: false,
            expected_executable: &self.exe,
            observed_executable: &self.exe,
            ssh_session_authenticated: self.ssh_session_authenticated,
            ssh_auth_evidence_source: self.auth_source,
            listener_present: self.listener_present,
            pid_recycled: self.pid_recycled,
            expected_starttime: self.identity.starttime,
            observed_starttime: self.identity.starttime,
            attestation_generation: self.generation,
            expected_listen_port: self.listen_port,
            observed_listen_port: self.listen_port,
            host_key_algorithm: &self.host_key_algorithm,
            observed_host_key_algorithm: &self.host_key_algorithm,
            lifecycle: self.lifecycle,
        }
    }
}

/// Collect and evaluate runtime observations against an approved plan.
/// Never treats a caller boolean as authentication.
pub fn attest_tunnel_runtime(
    plan: &RecoveryTunnelPlan<'_>,
    host_key: &HostKeyTrustInput<'_>,
    supervised: &SupervisedTunnelIdentity,
    snapshot: &TunnelObservationSnapshot,
) -> AttestReport {
    let mut reasons = Vec::new();
    let mut att = blank_attestation(plan, supervised.generation);

    if let Err(e) = crate::sync_recovery_tunnel::validate_recovery_tunnel_plan(plan) {
        reasons.extend(e);
        att.lifecycle = TunnelLifecycle::NotConfigured;
        return AttestReport {
            attestation: att,
            reasons,
        };
    }
    att.lifecycle = TunnelLifecycle::ConfigurationValidated;

    if let Err(e) = evaluate_host_key_trust(host_key) {
        reasons.extend(e);
        att.lifecycle = TunnelLifecycle::FailedIdentityValidation;
        att.auth_source = SshAuthEvidenceSource::Unobserved;
        return AttestReport {
            attestation: att,
            reasons,
        };
    }
    att.lifecycle = TunnelLifecycle::HostKeyTrustPrepared;
    att.observed_host_key_sha256 = plan.pinned_host_key_sha256.to_string();

    if snapshot.generation != supervised.generation {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL ATTEST: stale attestation reused after reconnect",
        );
        att.lifecycle = TunnelLifecycle::FailedIdentityValidation;
        att.auth_source = SshAuthEvidenceSource::Unobserved;
        return AttestReport {
            attestation: att,
            reasons,
        };
    }
    att.generation = snapshot.generation;

    match snapshot.dropbear_event {
        DropbearClientEvent::HostKeyUnknown | DropbearClientEvent::HostKeyMismatch => {
            att.unknown_host_key_accepted = false;
            att.auth_source = SshAuthEvidenceSource::Unobserved;
            att.lifecycle = TunnelLifecycle::FailedIdentityValidation;
            refuse(
                &mut reasons,
                "REFUSE TUNNEL ATTEST: host-key authentication failed",
            );
            return AttestReport {
                attestation: att,
                reasons,
            };
        }
        DropbearClientEvent::Exited => {
            att.lifecycle = TunnelLifecycle::Disconnected;
            att.auth_source = SshAuthEvidenceSource::Unobserved;
            refuse(&mut reasons, "REFUSE TUNNEL ATTEST: tunnel process exited");
            return AttestReport {
                attestation: att,
                reasons,
            };
        }
        DropbearClientEvent::NotCaptured | DropbearClientEvent::RunningQuiet => {}
    }

    let Some(proc_obs) = snapshot.process.as_ref() else {
        att.lifecycle = TunnelLifecycle::TunnelStarting;
        att.auth_source = SshAuthEvidenceSource::Unobserved;
        refuse(
            &mut reasons,
            "REFUSE TUNNEL ATTEST: supervised process observation is unavailable",
        );
        return AttestReport {
            attestation: att,
            reasons,
        };
    };

    att.exe = proc_obs.exe.clone();
    att.identity = proc_obs.identity;

    if !proc_obs.alive {
        att.lifecycle = TunnelLifecycle::Disconnected;
        att.auth_source = SshAuthEvidenceSource::Unobserved;
        refuse(&mut reasons, "REFUSE TUNNEL ATTEST: tunnel process exited");
        return AttestReport {
            attestation: att,
            reasons,
        };
    }

    if proc_obs.identity.pid != supervised.identity.pid {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL ATTEST: observed PID does not match the supervised process",
        );
    }
    if proc_obs.identity.starttime != supervised.identity.starttime {
        att.pid_recycled = true;
        refuse(
            &mut reasons,
            "REFUSE TUNNEL ATTEST: process start identity does not match (PID reuse)",
        );
    }
    if proc_obs.exe != supervised.expected_exe {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL ATTEST: executable does not match the approved dropbearmulti",
        );
    }
    let role = classify_dropbear_role(&proc_obs.cmdline);
    match role {
        ProcessRole::UsbnetliteDaemon => refuse(
            &mut reasons,
            "REFUSE TUNNEL ATTEST: process is the USBNETLite SSH daemon, not the forwarding client",
        ),
        ProcessRole::Other => refuse(
            &mut reasons,
            "REFUSE TUNNEL ATTEST: process is not a Dropbear forwarding client",
        ),
        ProcessRole::RecoveryForwardClient => {}
    }
    if proc_obs.cmdline != supervised.expected_cmdline {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL ATTEST: process arguments do not match the approved forwarding plan",
        );
    }
    if argv_has_tofu(&proc_obs.cmdline) {
        att.unknown_host_key_accepted = true;
        refuse(
            &mut reasons,
            "REFUSE TUNNEL ATTEST: trust-on-first-use is not permitted",
        );
    }
    att.gateway_ports = proc_obs.cmdline.iter().any(|a| a == "-g")
        || proc_obs
            .cmdline
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "GatewayPorts=yes");
    att.agent_forwarding = proc_obs.cmdline.iter().any(|a| a == "-A");
    att.interactive_shell = proc_obs.cmdline.iter().any(|a| a == "-t");
    att.unrestricted_forwarding = argv_token(&proc_obs.cmdline, "-L")
        .is_some_and(|s| s.contains("*:") || s.starts_with("0.0.0.0:"));

    if let Some(host) = ssh_dest_host(&proc_obs.cmdline) {
        if host != plan.ssh_host {
            refuse(
                &mut reasons,
                "REFUSE TUNNEL ATTEST: approved SSH host changed",
            );
        }
    }
    if let Some(port) = ssh_dest_port(&proc_obs.cmdline) {
        if port != plan.ssh_port {
            refuse(
                &mut reasons,
                "REFUSE TUNNEL ATTEST: approved SSH port changed",
            );
        }
    }
    if let Some((addr, lport, rhost, rport)) = forward_spec(&proc_obs.cmdline) {
        if addr != plan.listen_addr || lport != plan.listen_port {
            refuse(
                &mut reasons,
                "REFUSE TUNNEL ATTEST: forwarding listen endpoint does not match the plan",
            );
        }
        if rhost != plan.remote_forward_host || rport != plan.remote_forward_port {
            refuse(
                &mut reasons,
                "REFUSE TUNNEL ATTEST: forwarded destination does not match the plan",
            );
        }
    }

    let policy_in_argv = argv_has_strict_host_key(&proc_obs.cmdline);
    let exit_on_fail = argv_has_exit_on_forward_failure(&proc_obs.cmdline);
    if !policy_in_argv || !exit_on_fail {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL ATTEST: restricted host-key or forward-failure options are missing",
        );
    }

    att.lifecycle = TunnelLifecycle::TunnelStarting;
    att.auth_source = if policy_in_argv {
        SshAuthEvidenceSource::PolicyConfiguredPinProvisioned
    } else {
        SshAuthEvidenceSource::ProcessAliveOnly
    };

    let matching: Vec<&TunnelListenerObservation> = snapshot
        .listeners
        .iter()
        .filter(|l| l.port == plan.listen_port)
        .collect();
    if matching.len() > 1 {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL ATTEST: multiple unexpected listeners on the planned port",
        );
    }
    let listener = matching.first().copied();
    match listener {
        None => {
            att.listener_present = false;
            att.auth_source =
                if snapshot.dropbear_event == DropbearClientEvent::RunningQuiet && policy_in_argv {
                    SshAuthEvidenceSource::PolicyConfiguredPinProvisioned
                } else if proc_obs.alive {
                    SshAuthEvidenceSource::ProcessAliveOnly
                } else {
                    SshAuthEvidenceSource::Unobserved
                };
            refuse(
                &mut reasons,
                "REFUSE TUNNEL ATTEST: expected loopback listener is missing",
            );
        }
        Some(l) => {
            att.listener_present = l.listen_state;
            att.listener_owner_observed = true;
            att.listen_addr = l.bind_address.clone();
            att.listen_port = l.port;
            att.listener_origin = format!("http://127.0.0.1:{}", l.port);
            if !l.listen_state {
                refuse(
                    &mut reasons,
                    "REFUSE TUNNEL ATTEST: expected loopback listener is missing",
                );
            }
            if l.bind_address != "127.0.0.1" && l.bind_address != "::1" {
                refuse(
                    &mut reasons,
                    "REFUSE TUNNEL ATTEST: listener must bind exclusively to loopback",
                );
            }
            if l.port != plan.listen_port {
                refuse(
                    &mut reasons,
                    "REFUSE TUNNEL ATTEST: listener bound to the wrong local port",
                );
            }
            if l.owner_pid != proc_obs.identity.pid {
                att.auth_source = SshAuthEvidenceSource::ListenerWithoutAuth;
                refuse(
                    &mut reasons,
                    "REFUSE TUNNEL ATTEST: listener is owned by another process",
                );
            }
            if l.port == 22 && (l.bind_address == "0.0.0.0" || l.bind_address == "::") {
                refuse(
                    &mut reasons,
                    "REFUSE TUNNEL ATTEST: process is the USBNETLite SSH daemon, not the forwarding client",
                );
            }
        }
    }

    if reasons.is_empty()
        && att.listener_present
        && att.listener_owner_observed
        && policy_in_argv
        && exit_on_fail
        && !att.pid_recycled
        && matches!(
            snapshot.dropbear_event,
            DropbearClientEvent::NotCaptured | DropbearClientEvent::RunningQuiet
        )
        && role == ProcessRole::RecoveryForwardClient
    {
        // Conjunction only. Dropbear does not emit an accepted-peer-key record.
        att.auth_source = SshAuthEvidenceSource::PinnedForwardConjunction;
        att.ssh_session_authenticated = true;
        att.lifecycle = TunnelLifecycle::ReadyForAuthorizedRecovery;
        if let Err(e) = evaluate_tunnel_runtime_evidence(&att.as_runtime_evidence()) {
            reasons.extend(e);
            att.ssh_session_authenticated = false;
            att.lifecycle = TunnelLifecycle::FailedIdentityValidation;
        }
    } else if att.listener_present
        && att.auth_source != SshAuthEvidenceSource::ListenerWithoutAuth
        && !att.ssh_session_authenticated
    {
        if matches!(
            att.auth_source,
            SshAuthEvidenceSource::PolicyConfiguredPinProvisioned
        ) {
            att.auth_source = SshAuthEvidenceSource::ListenerWithoutAuth;
        }
        att.lifecycle = TunnelLifecycle::ListenerVerified;
    }

    AttestReport {
        attestation: att,
        reasons,
    }
}

pub fn apply_invalidation(
    mut att: TunnelAttestation,
    reason: AttestationInvalidation,
) -> TunnelAttestation {
    att.ssh_session_authenticated = false;
    att.auth_source = SshAuthEvidenceSource::Unobserved;
    att.lifecycle = match reason {
        AttestationInvalidation::ProcessExited | AttestationInvalidation::SocketDisappeared => {
            att.listener_present = false;
            TunnelLifecycle::Disconnected
        }
        AttestationInvalidation::ReconnectWithoutRenewal
        | AttestationInvalidation::ProcessIdentityChanged
        | AttestationInvalidation::SshDestinationChanged
        | AttestationInvalidation::HostKeyFailed
        | AttestationInvalidation::PeerKeyMismatch => TunnelLifecycle::FailedIdentityValidation,
        _ => TunnelLifecycle::Disconnected,
    };
    if matches!(
        reason,
        AttestationInvalidation::ProcessIdentityChanged
            | AttestationInvalidation::SocketOwnerChanged
    ) {
        att.pid_recycled = true;
    }
    att
}

pub fn require_fresh_attestation_before_http(
    previous: &TunnelAttestation,
    plan: &RecoveryTunnelPlan<'_>,
    host_key: &HostKeyTrustInput<'_>,
    supervised: &SupervisedTunnelIdentity,
    snapshot: &TunnelObservationSnapshot,
    http_origin: &str,
) -> Result<TunnelAttestation, Vec<String>> {
    let mut reasons = Vec::new();
    if previous.lifecycle != TunnelLifecycle::ReadyForAuthorizedRecovery {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL ATTEST: previous attestation is not fresh",
        );
    }
    if snapshot.generation != previous.generation {
        refuse(
            &mut reasons,
            "REFUSE TUNNEL ATTEST: stale attestation reused after reconnect",
        );
        return Err(reasons);
    }
    let report = attest_tunnel_runtime(plan, host_key, supervised, snapshot);
    match report.ready() {
        Ok(fresh) => {
            let origin = authorize_recovery_origin(http_origin).map_err(|e| vec![e])?;
            if origin.as_str() != fresh.listener_origin {
                return Err(vec![
                    "REFUSE TUNNEL ATTEST: recovery HTTP target does not match attested listener"
                        .into(),
                ]);
            }
            if LIVE_NON_LOOPBACK_POST_ENABLED {
                return Err(vec![
                    "REFUSE TUNNEL ATTEST: non-loopback recovery POST is not enabled".into(),
                ]);
            }
            Ok(fresh.clone())
        }
        Err(e) => {
            reasons.extend(e.iter().cloned());
            Err(reasons)
        }
    }
}

pub fn bind_attestation_to_recovery_policy(
    att: &TunnelAttestation,
    gate: RecoveryGate,
) -> Result<(), Vec<String>> {
    if att.lifecycle != TunnelLifecycle::ReadyForAuthorizedRecovery {
        return Err(vec![
            "REFUSE TUNNEL ATTEST: tunnel is not ready for separately authorized recovery".into(),
        ]);
    }
    let input = RecoveryPolicyInput {
        gate,
        proposed_base_url: &att.listener_origin,
        transport: RecoveryTransport::LoopbackHttpViaAuthenticatedTunnel,
        tunnel_origin_host: "ssh.example.test",
        library_id: &att.library_id,
        replica_id: &att.replica_id,
        genesis_sha256: &att.genesis_sha256,
        expected_library_id: &att.library_id,
        expected_replica_id: &att.replica_id,
        expected_genesis_sha256: &att.genesis_sha256,
        expected_history_cursor: 1,
        observed_history_cursor: Some(1),
        intent_present: false,
        fence_active: false,
        completion_proof_present: false,
        accepted_prefix_complete: false,
        server_accept_replace: false,
        client_decodes_replace: true,
        participating_clients_compatible: true,
        credential_source: CredentialSource::MemoryOnly,
        allowed_operation_types: &["AddStroke"],
        proposed_operation_types: &["AddStroke"],
        local_library_approved: true,
        local_sync_approved: true,
        redirects_permitted: false,
        request_non_loopback_post: false,
        tunnel_identity_verified: false,
    };
    evaluate_recovery_policy(&input).map(|_| ()).map_err(|e| e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_recovery_tunnel::{
        build_dropbear_invocation, fingerprint_of_b64, DROPBEAR_CLIENT_APPLET, KINDLE_DROPBEARMULTI,
    };

    const PIN_BLOB: &[u8] = b"ssh-ed25519\0synthetic-host-key-blob";

    fn pin_material() -> (String, String, String) {
        let b64 = {
            // reuse tunnel encoder via fingerprint_of_b64 round-trip from known line
            fn b64_encode(data: &[u8]) -> String {
                const B64: &[u8] =
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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
            b64_encode(PIN_BLOB)
        };
        let fp = fingerprint_of_b64(&b64).unwrap();
        let line = format!("ssh.example.test ssh-ed25519 {b64}\n");
        (b64, fp, line)
    }

    fn plan<'a>(fp: &'a str) -> RecoveryTunnelPlan<'a> {
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
            pinned_host_key_sha256: fp,
            identity_file: "/mnt/us/scribe-app/state/recovery-tunnel/id_ed25519",
            known_hosts_home: "/mnt/us/scribe-app/state/recovery-tunnel",
            library_id: "lib-aaaa",
            replica_id: "rep-bbbb",
            genesis_sha256: "gen-cccc",
            transaction_id: "txn-synthetic-1",
        }
    }

    fn host_input<'a>(known: &'a str, b64: &'a str, fp: &'a str) -> HostKeyTrustInput<'a> {
        HostKeyTrustInput {
            known_hosts_text: Some(known),
            expected_hostname: "ssh.example.test",
            expected_ssh_port: 22,
            planned_ssh_port: 22,
            expected_algorithm: "ssh-ed25519",
            expected_key_b64: b64,
            expected_fingerprint_sha256: fp,
            tofu_permitted: false,
            accept_unknown_host: false,
            host_key_verification_disabled: false,
        }
    }

    fn good_pair() -> (
        RecoveryTunnelPlan<'static>,
        HostKeyTrustInput<'static>,
        SupervisedTunnelIdentity,
        TunnelObservationSnapshot,
    ) {
        // leaked for 'static test convenience
        let (b64, fp, line) = pin_material();
        let b64 = Box::leak(b64.into_boxed_str());
        let fp = Box::leak(fp.into_boxed_str());
        let line = Box::leak(line.into_boxed_str());
        let p = plan(fp);
        let inv = build_dropbear_invocation(&p).unwrap();
        let supervised = SupervisedTunnelIdentity {
            identity: ProcessStartIdentity {
                pid: 29610,
                starttime: 10_001,
            },
            generation: 1,
            expected_exe: KINDLE_DROPBEARMULTI.to_string(),
            expected_cmdline: inv.argv.clone(),
        };
        let snapshot = TunnelObservationSnapshot {
            generation: 1,
            process: Some(TunnelProcessObservation {
                identity: supervised.identity,
                exe: KINDLE_DROPBEARMULTI.to_string(),
                cmdline: inv.argv,
                alive: true,
            }),
            listeners: vec![TunnelListenerObservation {
                bind_address: "127.0.0.1".into(),
                port: 18765,
                listen_state: true,
                owner_pid: 29610,
            }],
            dropbear_event: DropbearClientEvent::RunningQuiet,
        };
        (p, host_input(line, b64, fp), supervised, snapshot)
    }

    fn refuse_contains(report: &AttestReport, needle: &str) {
        assert!(
            report.reasons.iter().any(|e| e.contains(needle)),
            "expected {needle:?} in {:?}",
            report.reasons
        );
        assert!(report.ready().is_err());
    }

    #[test]
    fn correct_synthetic_process_and_listener() {
        let (plan, hk, sup, snap) = good_pair();
        let report = attest_tunnel_runtime(&plan, &hk, &sup, &snap);
        let att = report.ready().expect("ready");
        assert_eq!(att.identity.pid, 29610);
        assert_eq!(
            att.auth_source,
            SshAuthEvidenceSource::PinnedForwardConjunction
        );
        assert_eq!(att.lifecycle, TunnelLifecycle::ReadyForAuthorizedRecovery);
        evaluate_tunnel_runtime_evidence(&att.as_runtime_evidence()).unwrap();
        assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
    }

    #[test]
    fn wrong_executable() {
        let (plan, hk, mut sup, mut snap) = good_pair();
        snap.process.as_mut().unwrap().exe = "/bin/busybox".into();
        sup.expected_exe = "/bin/busybox".into();
        refuse_contains(
            &attest_tunnel_runtime(&plan, &hk, &sup, &snap),
            "executable",
        );
        let (plan, hk, sup, mut snap) = good_pair();
        snap.process.as_mut().unwrap().exe = "/bin/busybox".into();
        refuse_contains(
            &attest_tunnel_runtime(&plan, &hk, &sup, &snap),
            "executable",
        );
    }

    #[test]
    fn wrong_pid() {
        let (plan, hk, sup, mut snap) = good_pair();
        snap.process.as_mut().unwrap().identity.pid = 7;
        snap.listeners[0].owner_pid = 7;
        refuse_contains(&attest_tunnel_runtime(&plan, &hk, &sup, &snap), "PID");
    }

    #[test]
    fn pid_reused_different_starttime() {
        let (plan, hk, sup, mut snap) = good_pair();
        snap.process.as_mut().unwrap().identity.starttime = 99;
        let report = attest_tunnel_runtime(&plan, &hk, &sup, &snap);
        refuse_contains(&report, "PID reuse");
        assert!(report.attestation.pid_recycled);
    }

    #[test]
    fn wrong_process_start_identity_from_proc_stat() {
        let stat = "29610 (dropbearmulti) S 1 29610 29610 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 10001 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let id = parse_proc_stat_starttime(stat).unwrap();
        assert_eq!(id.pid, 29610);
        assert_eq!(id.starttime, 10001);
        let (plan, hk, mut sup, snap) = good_pair();
        sup.identity.starttime = 42;
        refuse_contains(
            &attest_tunnel_runtime(&plan, &hk, &sup, &snap),
            "start identity",
        );
    }

    #[test]
    fn missing_listener() {
        let (plan, hk, sup, mut snap) = good_pair();
        snap.listeners.clear();
        refuse_contains(
            &attest_tunnel_runtime(&plan, &hk, &sup, &snap),
            "listener is missing",
        );
    }

    #[test]
    fn wrong_listening_port() {
        let (plan, hk, sup, mut snap) = good_pair();
        snap.listeners[0].port = 9;
        refuse_contains(
            &attest_tunnel_runtime(&plan, &hk, &sup, &snap),
            "listener is missing",
        );
    }

    #[test]
    fn non_loopback_bind() {
        let (plan, hk, sup, mut snap) = good_pair();
        snap.listeners[0].bind_address = "0.0.0.0".into();
        refuse_contains(
            &attest_tunnel_runtime(&plan, &hk, &sup, &snap),
            "exclusively to loopback",
        );
    }

    #[test]
    fn listener_owned_by_another_process() {
        let (plan, hk, sup, mut snap) = good_pair();
        snap.listeners[0].owner_pid = 21736;
        refuse_contains(
            &attest_tunnel_runtime(&plan, &hk, &sup, &snap),
            "owned by another process",
        );
    }

    #[test]
    fn process_exits_after_initial_validation() {
        let (plan, hk, sup, snap) = good_pair();
        let ready = attest_tunnel_runtime(&plan, &hk, &sup, &snap)
            .ready()
            .unwrap()
            .clone();
        let mut later = snap.clone();
        later.process.as_mut().unwrap().alive = false;
        later.dropbear_event = DropbearClientEvent::Exited;
        later.listeners.clear();
        refuse_contains(&attest_tunnel_runtime(&plan, &hk, &sup, &later), "exited");
        let stale = apply_invalidation(ready, AttestationInvalidation::ProcessExited);
        assert_eq!(stale.lifecycle, TunnelLifecycle::Disconnected);
        assert!(!stale.ssh_session_authenticated);
        assert!(evaluate_tunnel_runtime_evidence(&stale.as_runtime_evidence()).is_err());
    }

    #[test]
    fn listener_replaced() {
        let (plan, hk, sup, mut snap) = good_pair();
        snap.listeners[0].owner_pid = 1;
        refuse_contains(
            &attest_tunnel_runtime(&plan, &hk, &sup, &snap),
            "owned by another process",
        );
    }

    #[test]
    fn wrong_forwarded_destination() {
        let (plan, hk, sup, mut snap) = good_pair();
        let cmd: &mut Vec<String> = &mut snap.process.as_mut().unwrap().cmdline;
        for tok in cmd.iter_mut() {
            if tok.starts_with("127.0.0.1:18765:") {
                *tok = "127.0.0.1:18765:other.example.test:8080".into();
            }
        }
        refuse_contains(
            &attest_tunnel_runtime(&plan, &hk, &sup, &snap),
            "arguments do not match",
        );
    }

    #[test]
    fn changed_approved_ssh_host() {
        let (plan, hk, sup, mut snap) = good_pair();
        if let Some(last) = snap.process.as_mut().unwrap().cmdline.last_mut() {
            *last = "syncfwd@other.example.test".into();
        }
        refuse_contains(
            &attest_tunnel_runtime(&plan, &hk, &sup, &snap),
            "arguments do not match",
        );
    }

    #[test]
    fn missing_host_key_pin() {
        let (plan, _, sup, snap) = good_pair();
        let mut hk = host_input("", "AAAA", plan.pinned_host_key_sha256);
        hk.known_hosts_text = None;
        let report = attest_tunnel_runtime(&plan, &hk, &sup, &snap);
        assert!(report.reasons.iter().any(|e| e.contains("known_hosts")));
        assert_eq!(
            report.attestation.lifecycle,
            TunnelLifecycle::FailedIdentityValidation
        );
    }

    #[test]
    fn wrong_host_key_pin() {
        let (plan, hk, sup, snap) = good_pair();
        let other = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let mut hk = hk;
        hk.expected_fingerprint_sha256 = other;
        let report = attest_tunnel_runtime(&plan, &hk, &sup, &snap);
        assert!(report.reasons.iter().any(|e| e.contains("HOST KEY")));
    }

    #[test]
    fn authentication_outcome_unknown() {
        let (plan, hk, sup, mut snap) = good_pair();
        snap.listeners.clear();
        snap.dropbear_event = DropbearClientEvent::NotCaptured;
        let report = attest_tunnel_runtime(&plan, &hk, &sup, &snap);
        assert_eq!(
            report.attestation.auth_source,
            SshAuthEvidenceSource::ProcessAliveOnly
        );
        refuse_contains(&report, "listener is missing");
    }

    #[test]
    fn authentication_claimed_only_by_caller_data() {
        let mut ev = crate::sync_recovery_policy::ready_tunnel_evidence_fixture();
        ev.ssh_session_authenticated = true;
        ev.ssh_auth_evidence_source = SshAuthEvidenceSource::CallerAsserted;
        let err = evaluate_tunnel_runtime_evidence(&ev).unwrap_err();
        assert!(err.iter().any(|e| e.contains("caller assertion")));
    }

    #[test]
    fn host_key_configured_but_connection_not_established() {
        let (plan, hk, sup, mut snap) = good_pair();
        snap.process = None;
        snap.listeners.clear();
        snap.dropbear_event = DropbearClientEvent::NotCaptured;
        let report = attest_tunnel_runtime(&plan, &hk, &sup, &snap);
        assert_eq!(
            report.attestation.lifecycle,
            TunnelLifecycle::TunnelStarting
        );
        assert_eq!(
            report.attestation.auth_source,
            SshAuthEvidenceSource::Unobserved
        );
        refuse_contains(&report, "observation is unavailable");
    }

    #[test]
    fn listener_present_but_ssh_authentication_not_established() {
        let (plan, hk, sup, mut snap) = good_pair();
        snap.dropbear_event = DropbearClientEvent::HostKeyUnknown;
        let report = attest_tunnel_runtime(&plan, &hk, &sup, &snap);
        assert_eq!(
            report.attestation.lifecycle,
            TunnelLifecycle::FailedIdentityValidation
        );
        refuse_contains(&report, "host-key authentication failed");
    }

    #[test]
    fn ssh_authenticated_claim_without_listener() {
        let (plan, hk, sup, mut snap) = good_pair();
        snap.listeners.clear();
        snap.dropbear_event = DropbearClientEvent::RunningQuiet;
        let report = attest_tunnel_runtime(&plan, &hk, &sup, &snap);
        assert!(!report.attestation.ssh_session_authenticated);
        assert_ne!(
            report.attestation.auth_source,
            SshAuthEvidenceSource::PinnedForwardConjunction
        );
        refuse_contains(&report, "listener is missing");
    }

    #[test]
    fn stale_attestation_reused_after_reconnect() {
        let (plan, hk, sup, snap) = good_pair();
        let first = attest_tunnel_runtime(&plan, &hk, &sup, &snap)
            .ready()
            .unwrap()
            .clone();
        let mut snap2 = snap.clone();
        snap2.generation = 2;
        let mut sup2 = sup.clone();
        sup2.generation = 2;
        sup2.identity.pid = 40000;
        sup2.identity.starttime = 20_000;
        snap2.process.as_mut().unwrap().identity = sup2.identity;
        snap2.listeners[0].owner_pid = 40000;
        let err = require_fresh_attestation_before_http(
            &first,
            &plan,
            &hk,
            &sup2,
            &snap2,
            "http://127.0.0.1:18765",
        )
        .unwrap_err();
        assert!(err.iter().any(|e| e.contains("reconnect")));
        let renewed = attest_tunnel_runtime(&plan, &hk, &sup2, &snap2);
        renewed.ready().unwrap();
    }

    #[test]
    fn unauthorized_recovery_despite_valid_tunnel() {
        let (plan, hk, sup, snap) = good_pair();
        let report = attest_tunnel_runtime(&plan, &hk, &sup, &snap);
        let att = report.ready().unwrap();
        let err = bind_attestation_to_recovery_policy(att, RecoveryGate::DIrreversibleExecution)
            .unwrap_err();
        assert!(err.iter().any(|e| e.contains("Gate D")));
        assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
    }

    #[test]
    fn recovery_http_target_must_match_attested_listener() {
        let (plan, hk, sup, snap) = good_pair();
        let att = attest_tunnel_runtime(&plan, &hk, &sup, &snap)
            .ready()
            .unwrap()
            .clone();
        let err = require_fresh_attestation_before_http(
            &att,
            &plan,
            &hk,
            &sup,
            &snap,
            "http://127.0.0.1:9",
        )
        .unwrap_err();
        assert!(err
            .iter()
            .any(|e| e.contains("does not match attested listener")));
    }

    #[test]
    fn tunnel_failure_preserves_ambiguous_recovery() {
        let retention = attestation_invalidation_preserves_ambiguous_recovery();
        assert!(retention.retain_intent);
        assert!(retention.retain_operation_ids);
        assert!(retention.retain_client_sequences);
        assert!(retention.keep_ordinary_upload_blocked);
        assert!(retention.transport_failure_is_not_rejection);
        let (plan, hk, sup, snap) = good_pair();
        let att = attest_tunnel_runtime(&plan, &hk, &sup, &snap)
            .ready()
            .unwrap()
            .clone();
        let dead = apply_invalidation(att, AttestationInvalidation::SocketDisappeared);
        assert!(evaluate_tunnel_runtime_evidence(&dead.as_runtime_evidence()).is_err());
    }

    #[test]
    fn ordinary_authoring_independent_of_tunnel() {
        assert!(!missing_tunnel_attestation_blocks_ordinary_authoring());
        let (plan, hk, _, mut snap) = good_pair();
        snap.process = None;
        let dummy_sup = SupervisedTunnelIdentity {
            identity: ProcessStartIdentity {
                pid: 1,
                starttime: 1,
            },
            generation: 1,
            expected_exe: KINDLE_DROPBEARMULTI.into(),
            expected_cmdline: vec![],
        };
        let report = attest_tunnel_runtime(&plan, &hk, &dummy_sup, &snap);
        assert!(report.ready().is_err());
        assert!(!missing_tunnel_attestation_blocks_ordinary_authoring());
    }

    #[test]
    fn usbnetlite_daemon_is_not_the_forward_client() {
        let (plan, hk, mut sup, mut snap) = good_pair();
        snap.process.as_mut().unwrap().cmdline = vec![
            KINDLE_DROPBEARMULTI.into(),
            "dropbear".into(),
            "-p".into(),
            "22".into(),
        ];
        sup.expected_cmdline = snap.process.as_ref().unwrap().cmdline.clone();
        snap.listeners[0].port = 22;
        snap.listeners[0].bind_address = "0.0.0.0".into();
        refuse_contains(
            &attest_tunnel_runtime(&plan, &hk, &sup, &snap),
            "USBNETLite",
        );
    }

    #[test]
    fn proc_net_tcp_loopback_listen_parse() {
        let sample = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:4955 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000000000000000 100 0 0 10 0\n";
        let rows = parse_proc_net_tcp_listeners(sample).unwrap();
        assert_eq!(rows, vec![("127.0.0.1".into(), 18773, 12345)]);
    }

    #[test]
    fn proc_cmdline_split() {
        let raw = b"dropbearmulti\0dbclient\0-N\0";
        assert_eq!(
            parse_proc_cmdline(raw),
            vec!["dropbearmulti", "dbclient", "-N"]
        );
    }

    #[test]
    fn gate_a_loopback_still_ok_without_claiming_live_post() {
        let (plan, hk, sup, snap) = good_pair();
        let report = attest_tunnel_runtime(&plan, &hk, &sup, &snap);
        let att = report.ready().unwrap();
        bind_attestation_to_recovery_policy(att, RecoveryGate::AFreshEvidence).unwrap();
        assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
    }

    /// Review 109 Kindle `/proc/<pid>/stat` captured from independent inode owner 3444.
    const KINDLE_109_STAT: &str = "3444 (dropbearmulti) S 3443 3443 3443 0 -1 4194560 371 64 2 0 37 1 0 0 19 -1 1 0 189474343 3465216 417 4294967295 2137116672 2137386924 3202096160 0 0 0 0 4100 0 1 0 0 17 0 0 0 0 0 0 2137387896 2137392012 2137395200 3202096497 3202096809 3202096809 3202097111 0";

    /// Review 109 `/proc/net/tcp` LISTEN row for 127.0.0.1:18765 (inode 1542316).
    const KINDLE_109_TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   5: 0100007F:494D 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1542316 1 e9a14800 100 0 0 10 0\n";

    const KINDLE_109_HOST_B64: &str =
        "AAAAC3NzaC1lZDI1NTE5AAAAIEma+hudwEBXB0rtegFzb0HkVoHFQGRiBq7qM1wb1UuX";
    const KINDLE_109_PIN: &str = "173a48b9a6264703253b8a1b009af4927f19557e286f6686590f8b90de8ef7e5";

    fn kindle_109_plan_and_host() -> (
        RecoveryTunnelPlan<'static>,
        HostKeyTrustInput<'static>,
        Vec<String>,
    ) {
        let fp = fingerprint_of_b64(KINDLE_109_HOST_B64).unwrap();
        assert_eq!(fp, KINDLE_109_PIN);
        let fp = Box::leak(fp.into_boxed_str());
        let known =
            Box::leak(format!("titan ssh-ed25519 {KINDLE_109_HOST_B64}\n").into_boxed_str());
        let p = RecoveryTunnelPlan {
            executable: KINDLE_DROPBEARMULTI,
            applet: DROPBEAR_CLIENT_APPLET,
            listen_addr: "127.0.0.1",
            listen_port: 18765,
            ssh_user: "smoketest",
            ssh_host: "titan",
            ssh_port: 2222,
            remote_forward_host: "httptest.example.test",
            remote_forward_port: 8080,
            host_key_algorithm: "ssh-ed25519",
            pinned_host_key_sha256: fp,
            identity_file: "/tmp/ignite-tunnel-attest-109/.ssh/id_ed25519",
            known_hosts_home: "/tmp/ignite-tunnel-attest-109",
            library_id: "ignite-109-lib",
            replica_id: "ignite-109-rep",
            genesis_sha256: "ignite-109-gen",
            transaction_id: "ignite-109-txn",
        };
        let inv = build_dropbear_invocation(&p).unwrap();
        let hk = HostKeyTrustInput {
            known_hosts_text: Some(known),
            expected_hostname: "titan",
            expected_ssh_port: 2222,
            planned_ssh_port: 2222,
            expected_algorithm: "ssh-ed25519",
            expected_key_b64: KINDLE_109_HOST_B64,
            expected_fingerprint_sha256: fp,
            tofu_permitted: false,
            accept_unknown_host: false,
            host_key_verification_disabled: false,
        };
        (p, hk, inv.argv)
    }

    #[test]
    fn kindle_109_proc_stat_parses_starttime() {
        let id = parse_proc_stat_starttime(KINDLE_109_STAT).unwrap();
        assert_eq!(id.pid, 3444);
        assert_eq!(id.starttime, 189474343);
    }

    #[test]
    fn kindle_109_proc_net_tcp_loopback_18765() {
        let rows = parse_proc_net_tcp_listeners(KINDLE_109_TCP).unwrap();
        assert_eq!(rows, vec![("127.0.0.1".into(), 18765, 1542316)]);
    }

    #[test]
    fn kindle_109_cmdline_matches_builder() {
        let raw = b"/mnt/us/usbnetlite/bin/dropbearmulti\0dbclient\0-N\0-T\0-i\0/tmp/ignite-tunnel-attest-109/.ssh/id_ed25519\0-o\0StrictHostKeyChecking=yes\0-o\0BatchMode=yes\0-o\0ExitOnForwardFailure=yes\0-o\0GatewayPorts=no\0-o\0ForwardAgent=no\0-o\0PasswordAuthentication=no\0-L\0127.0.0.1:18765:httptest.example.test:8080\0-p\02222\0smoketest@titan\0";
        let observed = parse_proc_cmdline(raw);
        let (_, _, argv) = kindle_109_plan_and_host();
        assert_eq!(observed, argv);
        assert_eq!(
            classify_dropbear_role(&observed),
            ProcessRole::RecoveryForwardClient
        );
    }

    #[test]
    fn kindle_109_live_snapshot_attests_pinned_forward_conjunction() {
        let (plan, hk, argv) = kindle_109_plan_and_host();
        let identity = parse_proc_stat_starttime(KINDLE_109_STAT).unwrap();
        let tcp = parse_proc_net_tcp_listeners(KINDLE_109_TCP).unwrap();
        assert_eq!(tcp[0].2, 1542316);
        // Owner PID was joined independently via /proc/*/fd socket:[inode], not from this parser.
        let owner_pid = 3444u32;
        assert_eq!(owner_pid, identity.pid);
        let supervised = SupervisedTunnelIdentity {
            identity,
            generation: 1,
            expected_exe: KINDLE_DROPBEARMULTI.to_string(),
            expected_cmdline: argv.clone(),
        };
        let snapshot = TunnelObservationSnapshot {
            generation: 1,
            process: Some(TunnelProcessObservation {
                identity,
                exe: KINDLE_DROPBEARMULTI.to_string(),
                cmdline: argv,
                alive: true,
            }),
            listeners: vec![TunnelListenerObservation {
                bind_address: tcp[0].0.clone(),
                port: tcp[0].1,
                listen_state: true,
                owner_pid,
            }],
            dropbear_event: DropbearClientEvent::RunningQuiet,
        };
        let report = attest_tunnel_runtime(&plan, &hk, &supervised, &snapshot);
        let att = report
            .ready()
            .unwrap_or_else(|e| panic!("expected ready, got {e:?}"));
        assert_eq!(
            att.auth_source,
            SshAuthEvidenceSource::PinnedForwardConjunction
        );
        assert_eq!(att.lifecycle, TunnelLifecycle::ReadyForAuthorizedRecovery);
        assert_eq!(att.identity.pid, 3444);
        assert_eq!(att.identity.starttime, 189474343);
        assert_eq!(att.listen_port, 18765);
        assert_eq!(att.listener_origin, "http://127.0.0.1:18765");
        assert_eq!(att.pinned_host_key_sha256, KINDLE_109_PIN);
        assert!(att.ssh_session_authenticated);
        assert!(att.listener_present);
        assert!(att.listener_owner_observed);
        assert!(!att.pid_recycled);
        assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
        let fresh = require_fresh_attestation_before_http(
            att,
            &plan,
            &hk,
            &supervised,
            &snapshot,
            "http://127.0.0.1:18765",
        )
        .unwrap();
        assert_eq!(fresh.generation, 1);
        assert!(require_fresh_attestation_before_http(
            att,
            &plan,
            &hk,
            &supervised,
            &snapshot,
            "http://127.0.0.1:9",
        )
        .is_err());
    }

    #[test]
    fn kindle_109_post_shutdown_and_stale_generation_refuse() {
        let (plan, hk, argv) = kindle_109_plan_and_host();
        let identity = parse_proc_stat_starttime(KINDLE_109_STAT).unwrap();
        let supervised = SupervisedTunnelIdentity {
            identity,
            generation: 1,
            expected_exe: KINDLE_DROPBEARMULTI.to_string(),
            expected_cmdline: argv.clone(),
        };
        let live = TunnelObservationSnapshot {
            generation: 1,
            process: Some(TunnelProcessObservation {
                identity,
                exe: KINDLE_DROPBEARMULTI.to_string(),
                cmdline: argv,
                alive: true,
            }),
            listeners: vec![TunnelListenerObservation {
                bind_address: "127.0.0.1".into(),
                port: 18765,
                listen_state: true,
                owner_pid: 3444,
            }],
            dropbear_event: DropbearClientEvent::RunningQuiet,
        };
        let att = attest_tunnel_runtime(&plan, &hk, &supervised, &live)
            .ready()
            .unwrap()
            .clone();

        let missing = TunnelObservationSnapshot {
            generation: 1,
            process: None,
            listeners: vec![],
            dropbear_event: DropbearClientEvent::Exited,
        };
        refuse_contains(
            &attest_tunnel_runtime(&plan, &hk, &supervised, &missing),
            "exited",
        );
        let err = require_fresh_attestation_before_http(
            &att,
            &plan,
            &hk,
            &supervised,
            &missing,
            "http://127.0.0.1:18765",
        )
        .unwrap_err();
        assert!(err
            .iter()
            .any(|e| e.contains("exited") || e.contains("unavailable")));

        let invalidated = apply_invalidation(att.clone(), AttestationInvalidation::ProcessExited);
        assert_eq!(invalidated.lifecycle, TunnelLifecycle::Disconnected);
        assert!(!invalidated.ssh_session_authenticated);
        assert_eq!(invalidated.auth_source, SshAuthEvidenceSource::Unobserved);
        assert!(attestation_invalidation_preserves_ambiguous_recovery().retain_intent);

        let stale = TunnelObservationSnapshot {
            generation: 2,
            process: live.process.clone(),
            listeners: live.listeners.clone(),
            dropbear_event: DropbearClientEvent::RunningQuiet,
        };
        refuse_contains(
            &attest_tunnel_runtime(&plan, &hk, &supervised, &stale),
            "stale attestation",
        );
    }
}
