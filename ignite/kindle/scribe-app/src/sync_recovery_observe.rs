//! Linux `/proc` observation collector for a *supervised* Dropbear recovery tunnel
//! (Slice 18E.10H).
//!
//! Reads kernel procfs records for one already-supervised PID and independently
//! joins the approved loopback LISTEN socket inode to that process's file
//! descriptors. Does not search the process table for a matching `dbclient`,
//! does not start Dropbear, and does not enable live recovery POST.
//!
//! [`TunnelObservationSnapshot::listeners`] `owner_pid` is derived from the inode
//! join. A caller cannot supply it as an input to this collector.
//!
//! Procfs is not an atomic snapshot. Identity is re-read after socket inspection;
//! a remaining TOCTOU window is documented in Review 110, not papered over.
//! Dropbear does not export the accepted peer-key blob through `/proc`; this
//! collector never claims that observation.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use crate::sync_recovery_attest::{
    classify_dropbear_role, parse_proc_cmdline, parse_proc_ipv4_port, DropbearClientEvent,
    ProcessRole, ProcessStartIdentity, TunnelListenerObservation, TunnelObservationSnapshot,
    TunnelProcessObservation,
};
use crate::sync_recovery_tunnel::{build_dropbear_invocation, RecoveryTunnelPlan};

const MAX_PROCFS_FILE_BYTES: usize = 256 * 1024;
const MAX_CMDLINE_BYTES: usize = 8 * 1024;
const MAX_TCP_LINES: usize = 8192;
const MAX_FD_ENTRIES: usize = 512;

const TCP_LISTEN: &str = "0A";

/// Distinguishes production kernel reads from test fixtures. A synthetic fixture
/// is not an authoritative runtime credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationSourceKind {
    LinuxProcfs,
    SyntheticFixture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObserveCheckpoint {
    AfterIdentity,
    AfterSocketJoin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObserveError {
    Missing,
    PermissionDenied,
    UnsupportedPlatform,
    TooLarge,
    Other(&'static str),
}

/// Injectable procfs. Production uses [`LinuxProcfs`]; tests use [`SyntheticProcfs`].
pub trait TunnelProcfs {
    fn read_stat(&self, pid: u32) -> Result<String, ObserveError>;
    fn read_exe(&self, pid: u32) -> Result<String, ObserveError>;
    fn read_cmdline(&self, pid: u32) -> Result<Vec<u8>, ObserveError>;
    fn read_tcp(&self) -> Result<String, ObserveError>;
    /// `Ok(None)` means `/proc/net/tcp6` is absent (not empty).
    fn read_tcp6(&self) -> Result<Option<String>, ObserveError>;
    fn list_fd_links(&self, pid: u32) -> Result<Vec<String>, ObserveError>;
    fn checkpoint(&self, _point: ObserveCheckpoint) {}
}

/// Inputs for one collection. The supervised identity must come from an
/// authorized launch or authenticated adoption, not from scanning.
pub struct CollectTunnelObservationRequest<'a> {
    pub plan: &'a RecoveryTunnelPlan<'a>,
    pub supervised: &'a crate::sync_recovery_attest::SupervisedTunnelIdentity,
    pub expected_listen_addr: &'a str,
    pub expected_listen_port: u16,
    pub expected_identity: ProcessStartIdentity,
    pub source: ObservationSourceKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcTcpRow {
    pub bind_address: String,
    pub port: u16,
    pub state_hex: String,
    pub inode: u64,
    pub listen: bool,
}

fn refuse_msg(msg: impl Into<String>) -> String {
    let s = msg.into();
    if s.starts_with("REFUSE TUNNEL OBSERVE:") {
        s
    } else {
        format!("REFUSE TUNNEL OBSERVE: {s}")
    }
}

fn err_one(msg: impl Into<String>) -> Vec<String> {
    vec![refuse_msg(msg)]
}

fn map_io(err: ObserveError, what: &str) -> Vec<String> {
    match err {
        ObserveError::Missing => err_one(format!("{what} is missing")),
        ObserveError::PermissionDenied => err_one(format!("{what} is inaccessible")),
        ObserveError::UnsupportedPlatform => err_one("Linux /proc is not available on this host"),
        ObserveError::TooLarge => err_one(format!("{what} exceeds the collection bound")),
        ObserveError::Other(s) => err_one(s),
    }
}

/// Parenthesis-safe `/proc/<pid>/stat`: field 22 (starttime) is rest index 19.
pub fn parse_supervised_stat(stat: &str) -> Result<(ProcessStartIdentity, char), String> {
    let lparen = stat
        .find('(')
        .ok_or_else(|| refuse_msg("/proc/stat missing comm"))?;
    let rparen = stat
        .rfind(')')
        .ok_or_else(|| refuse_msg("/proc/stat missing comm end"))?;
    if rparen <= lparen {
        return Err(refuse_msg("/proc/stat comm is malformed"));
    }
    let pid: u32 = stat[..lparen]
        .trim()
        .parse()
        .map_err(|_| refuse_msg("/proc/stat pid is malformed"))?;
    if pid == 0 {
        return Err(refuse_msg("PID is missing"));
    }
    let rest = stat[rparen + 1..].trim();
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let state_tok = fields
        .first()
        .ok_or_else(|| refuse_msg("/proc/stat state is missing"))?;
    if state_tok.len() != 1 {
        return Err(refuse_msg("/proc/stat state is malformed"));
    }
    let state = state_tok
        .chars()
        .next()
        .ok_or_else(|| refuse_msg("/proc/stat state is malformed"))?;
    let start_tok = fields
        .get(19)
        .ok_or_else(|| refuse_msg("start time is missing"))?;
    let starttime: u64 = start_tok
        .parse()
        .map_err(|_| refuse_msg("start time is malformed"))?;
    if starttime == 0 {
        return Err(refuse_msg("start time is zero"));
    }
    Ok((ProcessStartIdentity { pid, starttime }, state))
}

pub(crate) fn parse_proc_net_tcp_rows(tcp: &str) -> Result<Vec<ProcTcpRow>, String> {
    parse_proc_net_family_rows(tcp, AddrFamily::V4)
}

pub(crate) fn parse_proc_net_tcp6_rows(tcp6: &str) -> Result<Vec<ProcTcpRow>, String> {
    parse_proc_net_family_rows(tcp6, AddrFamily::V6)
}

#[derive(Clone, Copy)]
enum AddrFamily {
    V4,
    V6,
}

fn parse_proc_net_family_rows(text: &str, family: AddrFamily) -> Result<Vec<ProcTcpRow>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if i >= MAX_TCP_LINES {
            return Err(refuse_msg(
                "/proc/net/tcp table exceeds the collection bound",
            ));
        }
        if i == 0 {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cols: Vec<&str> = trimmed.split_whitespace().collect();
        if cols.len() < 10 {
            return Err(refuse_msg("malformed socket record"));
        }
        // local_address is column 2 (index 1) after the sl index column.
        let local = cols[1];
        let (addr, port) = match family {
            AddrFamily::V4 => parse_proc_ipv4_port(local).map_err(|e| refuse_msg(e))?,
            AddrFamily::V6 => parse_proc_ipv6_port(local)?,
        };
        let state_hex = cols[3].to_ascii_uppercase();
        if state_hex.len() != 2 || !state_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(refuse_msg("malformed socket record"));
        }
        let inode: u64 = cols[9]
            .parse()
            .map_err(|_| refuse_msg("malformed socket inode"))?;
        out.push(ProcTcpRow {
            bind_address: addr,
            port,
            listen: state_hex == TCP_LISTEN,
            state_hex,
            inode,
        });
    }
    Ok(out)
}

/// `/proc/net/tcp6` prints each 32-bit word little-endian on Kindle.
pub(crate) fn parse_proc_ipv6_port(field: &str) -> Result<(String, u16), String> {
    let (ip_hex, port_hex) = field
        .split_once(':')
        .ok_or_else(|| refuse_msg("/proc/net/tcp6 address is malformed"))?;
    if ip_hex.len() != 32 || !ip_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(refuse_msg("/proc/net/tcp6 address is malformed"));
    }
    let mut bytes = [0u8; 16];
    for i in 0..4 {
        let word = u32::from_str_radix(&ip_hex[i * 8..(i + 1) * 8], 16)
            .map_err(|_| refuse_msg("/proc/net/tcp6 address is malformed"))?;
        bytes[i * 4] = (word & 0xff) as u8;
        bytes[i * 4 + 1] = ((word >> 8) & 0xff) as u8;
        bytes[i * 4 + 2] = ((word >> 16) & 0xff) as u8;
        bytes[i * 4 + 3] = ((word >> 24) & 0xff) as u8;
    }
    let port = u16::from_str_radix(port_hex, 16)
        .map_err(|_| refuse_msg("/proc/net/tcp6 port is malformed"))?;
    Ok((format_ipv6(&bytes), port))
}

fn format_ipv6(bytes: &[u8; 16]) -> String {
    if bytes.iter().all(|b| *b == 0) {
        return "::".into();
    }
    if bytes[0..15].iter().all(|b| *b == 0) && bytes[15] == 1 {
        return "::1".into();
    }
    if bytes[0..10].iter().all(|b| *b == 0) && bytes[10] == 0xff && bytes[11] == 0xff {
        return format!(
            "::ffff:{}.{}.{}.{}",
            bytes[12], bytes[13], bytes[14], bytes[15]
        );
    }
    // Uncompressed groups; collector only accepts "::1" as IPv6 loopback.
    let mut groups = [0u16; 8];
    for i in 0..8 {
        groups[i] = u16::from_be_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
    }
    groups
        .iter()
        .map(|g| format!("{g:x}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub(crate) fn parse_socket_inode_link(link: &str) -> Option<u64> {
    let rest = link.strip_prefix("socket:[")?.strip_suffix(']')?;
    if rest.is_empty() {
        return None;
    }
    rest.parse().ok()
}

fn read_identity<S: TunnelProcfs + ?Sized>(
    procfs: &S,
    pid: u32,
) -> Result<(ProcessStartIdentity, char, String, Vec<String>), Vec<String>> {
    let stat = procfs
        .read_stat(pid)
        .map_err(|e| map_io(e, "/proc/<pid>/stat"))?;
    let (identity, state) = parse_supervised_stat(&stat).map_err(|e| vec![e])?;
    if identity.pid != pid {
        return Err(err_one(
            "observed PID does not match the supervised process",
        ));
    }
    let exe = procfs
        .read_exe(pid)
        .map_err(|e| map_io(e, "/proc/<pid>/exe"))?;
    if exe.len() > MAX_PROCFS_FILE_BYTES {
        return Err(err_one("/proc/<pid>/exe exceeds the collection bound"));
    }
    let raw = procfs
        .read_cmdline(pid)
        .map_err(|e| map_io(e, "/proc/<pid>/cmdline"))?;
    if raw.len() > MAX_CMDLINE_BYTES {
        return Err(err_one("/proc/<pid>/cmdline exceeds the collection bound"));
    }
    let cmdline = parse_proc_cmdline(&raw);
    Ok((identity, state, exe, cmdline))
}

fn select_listener_rows<'a>(
    rows: &'a [ProcTcpRow],
    addr: &str,
    port: u16,
) -> Result<&'a ProcTcpRow, Vec<String>> {
    let matching: Vec<&ProcTcpRow> = rows
        .iter()
        .filter(|r| r.port == port && r.bind_address == addr)
        .collect();
    if matching.is_empty() {
        let same_port: Vec<&ProcTcpRow> = rows.iter().filter(|r| r.port == port).collect();
        if let Some(row) = same_port.first() {
            if !row.listen {
                return Err(err_one("socket is not in LISTEN state"));
            }
            return Err(err_one("listener bound to the wrong address"));
        }
        return Err(err_one("expected loopback listener is missing"));
    }
    let listen: Vec<&ProcTcpRow> = matching.iter().copied().filter(|r| r.listen).collect();
    if listen.is_empty() {
        return Err(err_one("socket is not in LISTEN state"));
    }
    if listen.len() > 1 {
        return Err(err_one("multiple unexpected listeners on the planned port"));
    }
    let row = listen[0];
    if matching.iter().any(|r| !r.listen) {
        return Err(err_one("conflicting non-LISTEN socket on the planned port"));
    }
    if row.inode == 0 {
        return Err(err_one("socket inode is missing"));
    }
    Ok(row)
}

/// Collect a point-in-time snapshot for the supervised PID. Fail closed.
pub fn collect_tunnel_observation<S: TunnelProcfs + ?Sized>(
    req: &CollectTunnelObservationRequest<'_>,
    procfs: &S,
) -> Result<TunnelObservationSnapshot, Vec<String>> {
    match req.source {
        ObservationSourceKind::LinuxProcfs | ObservationSourceKind::SyntheticFixture => {}
    }
    if req.supervised.identity.pid == 0 || req.expected_identity.pid == 0 {
        return Err(err_one("PID is missing"));
    }
    if req.expected_listen_port == 0 || req.plan.listen_port == 0 {
        return Err(err_one("listen port is missing"));
    }
    if req.expected_listen_port != req.plan.listen_port {
        return Err(err_one(
            "expected listen port does not match the approved plan",
        ));
    }
    if req.expected_listen_addr != req.plan.listen_addr {
        return Err(err_one(
            "expected listen address does not match the approved plan",
        ));
    }
    let family_v6 = match req.expected_listen_addr {
        "127.0.0.1" => false,
        "::1" => true,
        "0.0.0.0" | "::" | "*" => {
            return Err(err_one("listener must bind exclusively to loopback"));
        }
        other => {
            return Err(err_one(format!(
                "listen address {other} is not an authorized loopback"
            )));
        }
    };

    let pid = req.supervised.identity.pid;
    if req.expected_identity != req.supervised.identity {
        return Err(err_one(
            "expected process start identity does not match the supervised identity",
        ));
    }

    let (identity, state, exe, cmdline) = match read_identity(procfs, pid) {
        Ok(v) => v,
        Err(e) => {
            return Err(e);
        }
    };
    if matches!(state, 'Z' | 'X' | 'x') {
        return Err(err_one(
            "process is a zombie or otherwise invalid for forwarding",
        ));
    }
    if identity != req.supervised.identity || identity != req.expected_identity {
        if identity.pid == pid && identity.starttime != req.supervised.identity.starttime {
            return Err(err_one("process start identity does not match (PID reuse)"));
        }
        return Err(err_one(
            "observed process start identity does not match the supervised process",
        ));
    }
    if exe != req.supervised.expected_exe || exe != req.plan.executable {
        return Err(err_one(
            "executable does not match the approved dropbearmulti",
        ));
    }
    if Path::new(&exe).file_name().and_then(|n| n.to_str()) != Some("dropbearmulti") {
        return Err(err_one(
            "executable does not match the approved dropbearmulti",
        ));
    }
    match classify_dropbear_role(&cmdline) {
        ProcessRole::UsbnetliteDaemon => {
            return Err(err_one(
                "process is the USBNETLite SSH daemon, not the forwarding client",
            ));
        }
        ProcessRole::Other => {
            return Err(err_one("expected applet invocation is absent"));
        }
        ProcessRole::RecoveryForwardClient => {}
    }
    if cmdline != req.supervised.expected_cmdline {
        return Err(err_one(
            "process arguments do not match the approved forwarding plan",
        ));
    }
    match build_dropbear_invocation(req.plan) {
        Ok(inv) => {
            if cmdline != inv.argv {
                return Err(err_one(
                    "process arguments do not match the approved forwarding plan",
                ));
            }
        }
        Err(e) => return Err(e),
    }

    procfs.checkpoint(ObserveCheckpoint::AfterIdentity);

    let rows = if family_v6 {
        match procfs.read_tcp6() {
            Ok(Some(text)) => parse_proc_net_tcp6_rows(&text).map_err(|e| vec![e])?,
            Ok(None) => {
                return Err(err_one(
                    "IPv6 listener was requested but /proc/net/tcp6 is unavailable",
                ));
            }
            Err(e) => return Err(map_io(e, "/proc/net/tcp6")),
        }
    } else {
        let text = procfs.read_tcp().map_err(|e| map_io(e, "/proc/net/tcp"))?;
        parse_proc_net_tcp_rows(&text).map_err(|e| vec![e])?
    };

    if req.expected_listen_addr == "0.0.0.0"
        || req.expected_listen_addr == "::"
        || rows.iter().any(|r| {
            r.port == req.expected_listen_port
                && r.listen
                && (r.bind_address == "0.0.0.0" || r.bind_address == "::")
        })
    {
        let wild = rows.iter().any(|r| {
            r.port == req.expected_listen_port
                && r.listen
                && (r.bind_address == "0.0.0.0" || r.bind_address == "::")
        });
        if wild {
            return Err(err_one("wildcard listener is refused"));
        }
    }

    let listener = select_listener_rows(&rows, req.expected_listen_addr, req.expected_listen_port)?;
    if listener.bind_address != "127.0.0.1" && listener.bind_address != "::1" {
        return Err(err_one("listener must bind exclusively to loopback"));
    }
    if listener.bind_address == "::ffff:127.0.0.1" {
        return Err(err_one(
            "IPv4-mapped IPv6 listener is not an authorized loopback",
        ));
    }

    let fds = procfs
        .list_fd_links(pid)
        .map_err(|e| map_io(e, "/proc/<pid>/fd"))?;
    if fds.len() > MAX_FD_ENTRIES {
        return Err(err_one("/proc/<pid>/fd exceeds the collection bound"));
    }
    let mut hits = 0u32;
    for link in &fds {
        if let Some(inode) = parse_socket_inode_link(link) {
            if inode == listener.inode {
                hits += 1;
            }
        }
    }
    if hits == 0 {
        return Err(err_one(
            "listener socket inode is not owned by the supervised process",
        ));
    }
    // Conservatively refuse if the same LISTEN inode appears as a second matching
    // row (already handled) or if fd links are empty of the inode (handled).
    // Fork-shared inodes are not detected: only the supervised PID's fds are read.

    procfs.checkpoint(ObserveCheckpoint::AfterSocketJoin);

    let (identity2, state2, exe2, cmdline2) = read_identity(procfs, pid)?;
    if matches!(state2, 'Z' | 'X' | 'x') {
        return Err(err_one(
            "process is a zombie or otherwise invalid for forwarding",
        ));
    }
    if identity2 != identity {
        return Err(err_one(
            "process start identity changed during collection (PID reuse)",
        ));
    }
    if exe2 != exe || cmdline2 != cmdline {
        return Err(err_one("process identity changed during collection"));
    }

    let rows2 = if family_v6 {
        match procfs.read_tcp6() {
            Ok(Some(text)) => parse_proc_net_tcp6_rows(&text).map_err(|e| vec![e])?,
            Ok(None) => {
                return Err(err_one("loopback listener disappeared during collection"));
            }
            Err(e) => return Err(map_io(e, "/proc/net/tcp6")),
        }
    } else {
        let text = procfs.read_tcp().map_err(|e| map_io(e, "/proc/net/tcp"))?;
        parse_proc_net_tcp_rows(&text).map_err(|e| vec![e])?
    };
    let listener2 =
        select_listener_rows(&rows2, req.expected_listen_addr, req.expected_listen_port)?;
    if listener2.inode != listener.inode
        || listener2.bind_address != listener.bind_address
        || listener2.port != listener.port
    {
        return Err(err_one("loopback listener changed during collection"));
    }

    let fds2 = procfs
        .list_fd_links(pid)
        .map_err(|e| map_io(e, "/proc/<pid>/fd"))?;
    let still = fds2
        .iter()
        .any(|link| parse_socket_inode_link(link).is_some_and(|inode| inode == listener.inode));
    if !still {
        return Err(err_one(
            "listener file descriptor disappeared during collection",
        ));
    }

    // owner_pid is derived from the inode join with the supervised PID, never an input.
    let owner_pid = identity.pid;
    Ok(TunnelObservationSnapshot {
        generation: req.supervised.generation,
        process: Some(TunnelProcessObservation {
            identity,
            exe,
            cmdline,
            alive: true,
        }),
        listeners: vec![TunnelListenerObservation {
            bind_address: listener.bind_address.clone(),
            port: listener.port,
            listen_state: true,
            owner_pid,
        }],
        dropbear_event: DropbearClientEvent::NotCaptured,
    })
}

/// Production Linux entry. Compiles on Windows and fails closed there.
pub fn collect_linux_tunnel_observation(
    req: &CollectTunnelObservationRequest<'_>,
) -> Result<TunnelObservationSnapshot, Vec<String>> {
    if req.source != ObservationSourceKind::LinuxProcfs {
        return Err(err_one(
            "Linux collection requires ObservationSourceKind::LinuxProcfs",
        ));
    }
    collect_tunnel_observation(req, &LinuxProcfs)
}

pub fn attest_collected_tunnel_runtime<S: TunnelProcfs + ?Sized>(
    plan: &RecoveryTunnelPlan<'_>,
    host_key: &crate::sync_recovery_tunnel::HostKeyTrustInput<'_>,
    supervised: &crate::sync_recovery_attest::SupervisedTunnelIdentity,
    req: &CollectTunnelObservationRequest<'_>,
    procfs: &S,
) -> crate::sync_recovery_attest::AttestReport {
    match collect_tunnel_observation(req, procfs) {
        Ok(snap) => {
            crate::sync_recovery_attest::attest_tunnel_runtime(plan, host_key, supervised, &snap)
        }
        Err(reasons) => crate::sync_recovery_attest::AttestReport {
            attestation: crate::sync_recovery_attest::TunnelAttestation {
                generation: supervised.generation,
                identity: supervised.identity,
                exe: String::new(),
                listen_addr: plan.listen_addr.to_string(),
                listen_port: plan.listen_port,
                listener_origin: format!("http://127.0.0.1:{}", plan.listen_port),
                remote_forward_host: plan.remote_forward_host.to_string(),
                remote_forward_port: plan.remote_forward_port,
                pinned_host_key_sha256: plan.pinned_host_key_sha256.to_string(),
                observed_host_key_sha256: String::new(),
                host_key_algorithm: plan.host_key_algorithm.to_string(),
                library_id: plan.library_id.to_string(),
                replica_id: plan.replica_id.to_string(),
                genesis_sha256: plan.genesis_sha256.to_string(),
                transaction_id: plan.transaction_id.to_string(),
                auth_source: crate::sync_recovery_policy::SshAuthEvidenceSource::Unobserved,
                lifecycle: crate::sync_recovery_policy::TunnelLifecycle::Disconnected,
                listener_present: false,
                listener_owner_observed: false,
                pid_recycled: reasons.iter().any(|r| r.contains("PID reuse")),
                unknown_host_key_accepted: false,
                gateway_ports: false,
                unrestricted_forwarding: false,
                agent_forwarding: false,
                interactive_shell: false,
                ssh_session_authenticated: false,
            },
            reasons,
        },
    }
}

pub fn require_fresh_collected_attestation_before_http<S: TunnelProcfs + ?Sized>(
    previous: &crate::sync_recovery_attest::TunnelAttestation,
    plan: &RecoveryTunnelPlan<'_>,
    host_key: &crate::sync_recovery_tunnel::HostKeyTrustInput<'_>,
    supervised: &crate::sync_recovery_attest::SupervisedTunnelIdentity,
    req: &CollectTunnelObservationRequest<'_>,
    procfs: &S,
    http_origin: &str,
) -> Result<crate::sync_recovery_attest::TunnelAttestation, Vec<String>> {
    let snap = collect_tunnel_observation(req, procfs)?;
    crate::sync_recovery_attest::require_fresh_attestation_before_http(
        previous,
        plan,
        host_key,
        supervised,
        &snap,
        http_origin,
    )
}

/// Owned tunnel material for the request-time HTTP guard. Not a caller-filled snapshot.
/// There is no constructor that accepts `TunnelObservationSnapshot` or
/// `ssh_session_authenticated: true`.
pub struct DisposableTunnelHttpGuard {
    executable: String,
    applet: String,
    listen_addr: String,
    listen_port: u16,
    ssh_user: String,
    ssh_host: String,
    ssh_port: u16,
    remote_forward_host: String,
    remote_forward_port: u16,
    host_key_algorithm: String,
    pinned_host_key_sha256: String,
    identity_file: String,
    known_hosts_home: String,
    library_id: String,
    replica_id: String,
    genesis_sha256: String,
    transaction_id: String,
    known_hosts_text: String,
    expected_key_b64: String,
    supervised: RefCell<crate::sync_recovery_attest::SupervisedTunnelIdentity>,
    source: ObservationSourceKind,
    procfs: Box<dyn TunnelProcfs>,
    previous: RefCell<Option<crate::sync_recovery_attest::TunnelAttestation>>,
}

impl DisposableTunnelHttpGuard {
    /// Production-facing constructor: plan + supervised identity + observation backend.
    /// Synthetic fixtures are compile-gated to tests.
    pub fn from_collected_observation(
        plan: &RecoveryTunnelPlan<'_>,
        host_key: &crate::sync_recovery_tunnel::HostKeyTrustInput<'_>,
        supervised: crate::sync_recovery_attest::SupervisedTunnelIdentity,
        source: ObservationSourceKind,
        procfs: Box<dyn TunnelProcfs>,
    ) -> Result<Self, Vec<String>> {
        match source {
            ObservationSourceKind::LinuxProcfs => {}
            ObservationSourceKind::SyntheticFixture => {
                #[cfg(not(test))]
                {
                    return Err(err_one(
                        "synthetic observation is not a production credential",
                    ));
                }
            }
        }
        if host_key.known_hosts_text.is_none() {
            return Err(err_one("known_hosts is missing"));
        }
        Ok(Self {
            executable: plan.executable.to_string(),
            applet: plan.applet.to_string(),
            listen_addr: plan.listen_addr.to_string(),
            listen_port: plan.listen_port,
            ssh_user: plan.ssh_user.to_string(),
            ssh_host: plan.ssh_host.to_string(),
            ssh_port: plan.ssh_port,
            remote_forward_host: plan.remote_forward_host.to_string(),
            remote_forward_port: plan.remote_forward_port,
            host_key_algorithm: plan.host_key_algorithm.to_string(),
            pinned_host_key_sha256: plan.pinned_host_key_sha256.to_string(),
            identity_file: plan.identity_file.to_string(),
            known_hosts_home: plan.known_hosts_home.to_string(),
            library_id: plan.library_id.to_string(),
            replica_id: plan.replica_id.to_string(),
            genesis_sha256: plan.genesis_sha256.to_string(),
            transaction_id: plan.transaction_id.to_string(),
            known_hosts_text: host_key.known_hosts_text.unwrap_or("").to_string(),
            expected_key_b64: host_key.expected_key_b64.to_string(),
            supervised: RefCell::new(supervised),
            source,
            procfs,
            previous: RefCell::new(None),
        })
    }

    fn plan(&self) -> RecoveryTunnelPlan<'_> {
        RecoveryTunnelPlan {
            executable: &self.executable,
            applet: &self.applet,
            listen_addr: &self.listen_addr,
            listen_port: self.listen_port,
            ssh_user: &self.ssh_user,
            ssh_host: &self.ssh_host,
            ssh_port: self.ssh_port,
            remote_forward_host: &self.remote_forward_host,
            remote_forward_port: self.remote_forward_port,
            host_key_algorithm: &self.host_key_algorithm,
            pinned_host_key_sha256: &self.pinned_host_key_sha256,
            identity_file: &self.identity_file,
            known_hosts_home: &self.known_hosts_home,
            library_id: &self.library_id,
            replica_id: &self.replica_id,
            genesis_sha256: &self.genesis_sha256,
            transaction_id: &self.transaction_id,
        }
    }

    fn host_key(&self) -> crate::sync_recovery_tunnel::HostKeyTrustInput<'_> {
        crate::sync_recovery_tunnel::HostKeyTrustInput {
            known_hosts_text: Some(self.known_hosts_text.as_str()),
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

    /// Recollect and attest before Authorization or body. Does not authorize Gates A–E.
    pub fn require_fresh_before_http(
        &self,
        http_origin: &str,
    ) -> Result<crate::sync_recovery_attest::TunnelAttestation, Vec<String>> {
        let plan = self.plan();
        let hk = self.host_key();
        let supervised = self.supervised.borrow().clone();
        let req = CollectTunnelObservationRequest {
            plan: &plan,
            supervised: &supervised,
            expected_listen_addr: &self.listen_addr,
            expected_listen_port: self.listen_port,
            expected_identity: supervised.identity,
            source: self.source,
        };
        let att = if let Some(prev) = self.previous.borrow().clone() {
            require_fresh_collected_attestation_before_http(
                &prev,
                &plan,
                &hk,
                &supervised,
                &req,
                &*self.procfs,
                http_origin,
            )?
        } else {
            let snap = collect_tunnel_observation(&req, &*self.procfs)?;
            let report =
                crate::sync_recovery_attest::attest_tunnel_runtime(&plan, &hk, &supervised, &snap);
            let att = report.ready().map_err(|e| e.to_vec())?.clone();
            crate::sync_recovery_attest::require_fresh_attestation_before_http(
                &att,
                &plan,
                &hk,
                &supervised,
                &snap,
                http_origin,
            )?
        };
        *self.previous.borrow_mut() = Some(att.clone());
        Ok(att)
    }

    #[cfg(test)]
    pub fn test_set_generation(&self, generation: u64) {
        self.supervised.borrow_mut().generation = generation;
    }
}

/// Real `/proc` reader. Never scans `/proc/[0-9]*` for an arbitrary dbclient.
pub struct LinuxProcfs;

impl TunnelProcfs for LinuxProcfs {
    fn read_stat(&self, pid: u32) -> Result<String, ObserveError> {
        read_text_capped(&format!("/proc/{pid}/stat"), MAX_PROCFS_FILE_BYTES)
    }
    fn read_exe(&self, pid: u32) -> Result<String, ObserveError> {
        read_link_capped(&format!("/proc/{pid}/exe"))
    }
    fn read_cmdline(&self, pid: u32) -> Result<Vec<u8>, ObserveError> {
        read_bytes_capped(&format!("/proc/{pid}/cmdline"), MAX_CMDLINE_BYTES)
    }
    fn read_tcp(&self) -> Result<String, ObserveError> {
        read_text_capped("/proc/net/tcp", MAX_PROCFS_FILE_BYTES)
    }
    fn read_tcp6(&self) -> Result<Option<String>, ObserveError> {
        match read_text_capped("/proc/net/tcp6", MAX_PROCFS_FILE_BYTES) {
            Ok(s) => Ok(Some(s)),
            Err(ObserveError::Missing) => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn list_fd_links(&self, pid: u32) -> Result<Vec<String>, ObserveError> {
        list_fd_links_capped(pid, MAX_FD_ENTRIES)
    }
}

fn io_to_observe(err: std::io::Error) -> ObserveError {
    match err.kind() {
        std::io::ErrorKind::NotFound => ObserveError::Missing,
        std::io::ErrorKind::PermissionDenied => ObserveError::PermissionDenied,
        _ => {
            if cfg!(not(unix)) {
                ObserveError::UnsupportedPlatform
            } else {
                ObserveError::Missing
            }
        }
    }
}

fn read_bytes_capped(path: &str, max: usize) -> Result<Vec<u8>, ObserveError> {
    #[cfg(not(unix))]
    {
        let _ = (path, max);
        return Err(ObserveError::UnsupportedPlatform);
    }
    #[cfg(unix)]
    {
        let data = std::fs::read(path).map_err(io_to_observe)?;
        if data.len() > max {
            return Err(ObserveError::TooLarge);
        }
        Ok(data)
    }
}

fn read_text_capped(path: &str, max: usize) -> Result<String, ObserveError> {
    let bytes = read_bytes_capped(path, max)?;
    String::from_utf8(bytes).map_err(|_| ObserveError::Other("procfs text is not UTF-8"))
}

fn read_link_capped(path: &str) -> Result<String, ObserveError> {
    #[cfg(not(unix))]
    {
        let _ = path;
        return Err(ObserveError::UnsupportedPlatform);
    }
    #[cfg(unix)]
    {
        let p = std::fs::read_link(path).map_err(io_to_observe)?;
        let s = p.to_string_lossy().into_owned();
        if s.len() > MAX_PROCFS_FILE_BYTES {
            return Err(ObserveError::TooLarge);
        }
        Ok(s)
    }
}

fn list_fd_links_capped(pid: u32, max: usize) -> Result<Vec<String>, ObserveError> {
    #[cfg(not(unix))]
    {
        let _ = (pid, max);
        return Err(ObserveError::UnsupportedPlatform);
    }
    #[cfg(unix)]
    {
        let dir = std::fs::read_dir(format!("/proc/{pid}/fd")).map_err(io_to_observe)?;
        let mut out = Vec::new();
        for ent in dir {
            let ent = ent.map_err(io_to_observe)?;
            if out.len() >= max {
                return Err(ObserveError::TooLarge);
            }
            match std::fs::read_link(ent.path()) {
                Ok(p) => out.push(p.to_string_lossy().into_owned()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_to_observe(e)),
            }
        }
        Ok(out)
    }
}

/// In-memory procfs for unit tests. Not a production observation source.
#[derive(Clone, Debug)]
pub struct SyntheticFsState {
    pub stat: HashMap<u32, Result<String, ObserveError>>,
    pub exe: HashMap<u32, Result<String, ObserveError>>,
    pub cmdline: HashMap<u32, Result<Vec<u8>, ObserveError>>,
    pub fds: HashMap<u32, Result<Vec<String>, ObserveError>>,
    pub tcp: Result<String, ObserveError>,
    pub tcp6: Result<Option<String>, ObserveError>,
}

impl Default for SyntheticFsState {
    fn default() -> Self {
        Self {
            stat: HashMap::new(),
            exe: HashMap::new(),
            cmdline: HashMap::new(),
            fds: HashMap::new(),
            tcp: Err(ObserveError::Missing),
            tcp6: Ok(None),
        }
    }
}

struct SyntheticProcfsInner {
    state: RefCell<SyntheticFsState>,
    after_identity: RefCell<Option<SyntheticFsState>>,
    after_sockets: RefCell<Option<SyntheticFsState>>,
    identity_applied: Cell<bool>,
    sockets_applied: Cell<bool>,
}

/// Clone shares the same fixture so tests can invalidate observations between requests.
#[derive(Clone)]
pub struct SyntheticProcfs {
    inner: Rc<SyntheticProcfsInner>,
}

impl SyntheticProcfs {
    pub fn new(state: SyntheticFsState) -> Self {
        Self {
            inner: Rc::new(SyntheticProcfsInner {
                state: RefCell::new(state),
                after_identity: RefCell::new(None),
                after_sockets: RefCell::new(None),
                identity_applied: Cell::new(false),
                sockets_applied: Cell::new(false),
            }),
        }
    }

    pub fn replace_after_identity(&self, next: SyntheticFsState) {
        *self.inner.after_identity.borrow_mut() = Some(next);
    }

    pub fn replace_after_sockets(&self, next: SyntheticFsState) {
        *self.inner.after_sockets.borrow_mut() = Some(next);
    }

    /// Replace the live fixture for the next collection. Resets intra-collection checkpoints.
    pub fn replace_now(&self, next: SyntheticFsState) {
        *self.inner.state.borrow_mut() = next;
        self.inner.identity_applied.set(false);
        self.inner.sockets_applied.set(false);
        *self.inner.after_identity.borrow_mut() = None;
        *self.inner.after_sockets.borrow_mut() = None;
    }
}

impl TunnelProcfs for SyntheticProcfs {
    fn read_stat(&self, pid: u32) -> Result<String, ObserveError> {
        self.inner
            .state
            .borrow()
            .stat
            .get(&pid)
            .cloned()
            .unwrap_or(Err(ObserveError::Missing))
    }
    fn read_exe(&self, pid: u32) -> Result<String, ObserveError> {
        self.inner
            .state
            .borrow()
            .exe
            .get(&pid)
            .cloned()
            .unwrap_or(Err(ObserveError::Missing))
    }
    fn read_cmdline(&self, pid: u32) -> Result<Vec<u8>, ObserveError> {
        self.inner
            .state
            .borrow()
            .cmdline
            .get(&pid)
            .cloned()
            .unwrap_or(Err(ObserveError::Missing))
    }
    fn read_tcp(&self) -> Result<String, ObserveError> {
        self.inner.state.borrow().tcp.clone()
    }
    fn read_tcp6(&self) -> Result<Option<String>, ObserveError> {
        self.inner.state.borrow().tcp6.clone()
    }
    fn list_fd_links(&self, pid: u32) -> Result<Vec<String>, ObserveError> {
        self.inner
            .state
            .borrow()
            .fds
            .get(&pid)
            .cloned()
            .unwrap_or(Err(ObserveError::Missing))
    }
    fn checkpoint(&self, point: ObserveCheckpoint) {
        match point {
            ObserveCheckpoint::AfterIdentity if !self.inner.identity_applied.get() => {
                if let Some(next) = self.inner.after_identity.borrow_mut().take() {
                    *self.inner.state.borrow_mut() = next;
                }
                self.inner.identity_applied.set(true);
            }
            ObserveCheckpoint::AfterSocketJoin if !self.inner.sockets_applied.get() => {
                if let Some(next) = self.inner.after_sockets.borrow_mut().take() {
                    *self.inner.state.borrow_mut() = next;
                }
                self.inner.sockets_applied.set(true);
            }
            _ => {}
        }
    }
}
