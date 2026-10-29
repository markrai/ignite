//! Disposable crash-safe recovery execution coordinator (Slices 18E.7–18E.9A).
//!
//! Freezes operation identities and client sequences in a durable recovery-intent file,
//! submits them through the production authenticated ledger, reconstructs accepted prefixes
//! from authoritative tail evidence, and never remints an OperationId that may already have
//! been accepted. Ordinary upload stays blocked while the intent file exists.
//!
//! Not invoked from Kindle startup. Not a live-recovery path.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sync_engine::LibraryState;
use sync_protocol::{
    from_json, snapshot_sha256, to_canonical_json, LibraryId, OperationId, PageId, ReplicaId,
    SyncEnvelopeV1, SyncOperationV1, SyncSnapshotV1, ENVELOPE_VERSION,
};
use uuid::Uuid;

use crate::sync_local::{build_snapshot_read_only, identity_path, read_identity};
use crate::sync_outbound::{
    append_journal, confirmed, parse_journal, PendingOp, SyncStateV1, APPLY_FILE, OUTBOX_FILE,
    STATE_FILE,
};
use crate::sync_recovery_bind::{prepare_unbound_page_identities, require_disposable_bind_root};
use crate::sync_recovery_compare::inspect_identity_read_only;
use crate::sync_recovery_extract::file_sha256;
use crate::sync_recovery_observe::DisposableTunnelHttpGuard;
use crate::sync_recovery_plan::{
    plan_recovery, OutboxInput, PlanResult, RecoveryInputs, SequenceAuthority, ServerTailItem,
};
#[cfg(test)]
pub use crate::sync_recovery_policy::require_loopback_url;
use crate::sync_recovery_policy::{
    authorize_recovery_origin, evaluate_recovery_policy, AuthorizedOrigin, CredentialSource,
    RecoveryGate, RecoveryPolicyInput, RecoveryTransport, LIVE_NON_LOOPBACK_POST_ENABLED,
};
#[cfg(test)]
use crate::sync_recovery_stream::{recovery_http1_request, RecoveryStreamPlan};
#[cfg(test)]
use crate::sync_recovery_supervise::{
    DisposableStreamPermit, StreamTransportOutcome, SupervisedRecoveryStream,
};
#[cfg(test)]
use crate::sync_recovery_tunnel::{HostKeyTrustInput, DROPBEAR_CLIENT_APPLET};
pub use crate::sync_verified::RECOVERY_INTENT_FILE;
use crate::sync_verified::{
    clear_recovery_maintenance, hold_authoring_for_maintenance, publish_recovery_maintenance,
    read_mutation_gen, register_paths,
};

/// Match ordinary Kindle outbound client (see `sync_outbound`). Recovery never waits forever.
const RECOVERY_HTTP_CONNECT: Duration = Duration::from_secs(5);
const RECOVERY_HTTP_IO: Duration = Duration::from_secs(20);
#[cfg(not(test))]
const RECOVERY_HTTP_DEADLINE: Duration = Duration::from_secs(60);
#[cfg(test)]
const RECOVERY_HTTP_DEADLINE: Duration = Duration::from_secs(3);

const ALLOWED_RECOVERY_OP_TYPES: &[&str] = &["AddStroke", "ReplaceStrokeLineages", "CreatePage"];

pub const RECOVERY_INTENT_VERSION: u32 = 1;
pub const RECOVERY_COMPLETE_FILE: &str = "recovery-complete-v1.json";
pub const RECOVERY_COMPLETE_VERSION: u32 = 1;
pub const DISPOSABLE_EXEC_MARK: &str = "18E7";
const PREPARE_CLAIM_FILE: &str = "recovery-intent-v1.claim";

/// Ordinary push/capture is blocked whenever a recovery-intent file is present, including after
/// disposable finalization (`upload_authorized` stays false).
pub fn recovery_blocks_ordinary_push(sync_dir: &Path) -> bool {
    sync_dir.join(RECOVERY_INTENT_FILE).is_file()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RecoveryExecHooks {
    pub fail_before_identity: bool,
    pub fail_after_identity: bool,
    pub fail_during_intent_write: bool,
    pub fail_after_intent_before_upload: bool,
    /// Stop after this many *new* (non-duplicate) accepts in this process.
    pub halt_after_new_accepts: Option<usize>,
    /// Treat the next HTTP success as a lost response (still consult the tail).
    pub drop_next_ack: bool,
    /// Test-only: run after the initial pre-submit revalidate (before tail GET / POST).
    pub after_revalidate: Option<fn()>,
    /// Test-only: run after accepted-tail reconstruction, before the first POST.
    pub after_tail: Option<fn()>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    #[default]
    IdentitiesBound,
    PlanFrozen,
    SequencesAssigned,
    Submitting,
    PartiallyAccepted,
    FullyAccepted,
    ConfirmedReconciled,
    Verified,
    Finalized,
    Halted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenOp {
    pub client_seq: u64,
    pub operation_id: OperationId,
    pub envelope: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryIntentV1 {
    pub v: u32,
    pub transaction_id: String,
    pub phase: RecoveryPhase,
    pub library_id: LibraryId,
    pub replica_id: ReplicaId,
    pub genesis_sha256: String,
    pub checkpoint_cursor: u64,
    pub expected_server_head: u64,
    pub acked_client_seq: u64,
    pub next_client_seq_after: u64,
    pub local_snapshot_sha256: String,
    pub identity_sha256: String,
    pub plan_fingerprint: String,
    pub ops: Vec<FrozenOp>,
    pub accepted_prefix: usize,
    /// Always false in this slice.
    pub upload_authorized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub halt_reason: Option<String>,
    /// Library `local-mutation-gen` after identity bind, captured with the frozen snapshot.
    #[serde(default)]
    pub mutation_gen: u64,
    /// Set durably before the first POST attempt. Once true, abort must not clean up.
    #[serde(default)]
    pub submit_attempted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbortReport {
    pub intent_retained: bool,
    /// Sequence rewind / outbox deletion is never supported.
    pub cleanup_supported: bool,
    pub accepted_prefix: usize,
    pub submit_attempted: bool,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ReleaseHooks {
    pub fail_after_tail_verify: bool,
    pub fail_after_proof_before_release: bool,
}

/// In-memory OutboundSync health the durable files cannot see.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReleaseHealth {
    pub journal_error: Option<String>,
    pub seq_ready: bool,
    pub seq_block: Option<String>,
    pub sync_fault: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryCompleteV1 {
    pub v: u32,
    pub transaction_id: String,
    pub library_id: LibraryId,
    pub replica_id: ReplicaId,
    pub accepted_ops: usize,
    pub server_cursor: u64,
    pub acked_client_seq: u64,
    pub local_snapshot_sha256: String,
    pub identity_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseReport {
    pub intent_removed: bool,
    pub already_released: bool,
    pub server_cursor: u64,
    pub acked_client_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedAcceptance {
    pub accepted_prefix: usize,
    pub through_server_seq: u64,
}

#[derive(Clone)]
pub struct SubmitTarget {
    pub base_url: String,
    /// Bearer secret. Never logged; [`Debug`] redacts it.
    pub token: String,
}

impl std::fmt::Debug for SubmitTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubmitTarget")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct PrepareSpec<'a> {
    pub library_root: &'a Path,
    pub sync_dir: &'a Path,
    pub checkpoint: &'a LibraryState,
    pub checkpoint_cursor: u64,
    pub acked_client_seq: u64,
    pub next_client_seq: u64,
    pub replica_id: ReplicaId,
    pub genesis_sha256: String,
    pub server_tail: &'a [ServerTailItem],
    pub require_disposable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecOutcome {
    Progress {
        accepted_prefix: usize,
        total: usize,
    },
    Complete {
        accepted_prefix: usize,
    },
    Halted {
        accepted_prefix: usize,
        reason: String,
    },
}

pub fn require_disposable_exec_root(path: &Path) -> Result<(), String> {
    let s = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    // Refuse this crate / workspace checkout wherever it is cloned (portable).
    let manifest = env!("CARGO_MANIFEST_DIR")
        .replace('/', "\\")
        .to_ascii_lowercase();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .map(|p| p.to_string_lossy().replace('/', "\\").to_ascii_lowercase())
        .unwrap_or_default();
    if s.contains("ignite-divergence-analysis-18d3\\reference")
        || s.contains("ignite-currentstate-18e6c")
        || s.contains("ignite-divergence-analysis-18d3\\work\\18e6d")
        || s.contains("ignite-livetail-18e6b")
        || s.contains("c:\\dev\\project\\ignite\\")
        || s.contains("c:\\dev\\project\\scribe\\")
        || (!manifest.is_empty() && s.contains(&manifest))
        || (!workspace.is_empty() && (s == workspace || s.starts_with(&(workspace.clone() + "\\"))))
    {
        return Err(format!(
            "REFUSE RECOVERY EXEC: {} is a protected or product path",
            path.display()
        ));
    }
    if !s.contains("18e7") && !s.contains("18e9a") && !s.contains("18e9b") && !s.contains("18e9c") {
        return Err(format!(
            "REFUSE RECOVERY EXEC: {} is not under a disposable {DISPOSABLE_EXEC_MARK}, 18E9A, 18E9B, or 18E9C tree",
            path.display()
        ));
    }
    Ok(())
}

/// Sealed transport kind. There is no `skip_attestation` variant.
enum RecoveryHttpKind {
    /// Existing disposable loopback executor (Reviews 97–103). Origin policy only.
    DisposableLoopback,
    /// Explicit collected-tunnel path. Recollects before every HTTP call.
    CollectedTunnel(DisposableTunnelHttpGuard),
    /// Test-only supervised SSH stdin/stdout. Not constructed by [`submit_recovery`].
    #[cfg(test)]
    DisposableDirectStream(DisposableDirectStreamPlan),
}

/// Owned disposable SSH/HTTP plan. Bearer stays on [`RecoveryHttp::token`], never here.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct DisposableDirectStreamPlan {
    pub executable: String,
    pub identity_file: String,
    pub known_hosts_home: String,
    pub known_hosts_text: String,
    pub expected_key_b64: String,
    pub pinned_host_key_sha256: String,
    pub ssh_host: String,
    pub ssh_user: String,
    pub ssh_port: u16,
    pub remote_forward_host: String,
    pub remote_forward_port: u16,
    pub library_id: String,
    pub replica_id: String,
    pub genesis_sha256: String,
    pub transaction_id: String,
}

#[cfg(test)]
impl DisposableDirectStreamPlan {
    fn as_stream_plan(&self) -> RecoveryStreamPlan<'_> {
        RecoveryStreamPlan {
            executable: &self.executable,
            applet: DROPBEAR_CLIENT_APPLET,
            ssh_user: &self.ssh_user,
            ssh_host: &self.ssh_host,
            ssh_port: self.ssh_port,
            remote_forward_host: &self.remote_forward_host,
            remote_forward_port: self.remote_forward_port,
            host_key_algorithm: "ssh-ed25519",
            pinned_host_key_sha256: &self.pinned_host_key_sha256,
            identity_file: &self.identity_file,
            known_hosts_home: &self.known_hosts_home,
            library_id: &self.library_id,
            replica_id: &self.replica_id,
            genesis_sha256: &self.genesis_sha256,
            transaction_id: &self.transaction_id,
        }
    }

    fn host_key(&self) -> HostKeyTrustInput<'_> {
        HostKeyTrustInput {
            known_hosts_text: Some(&self.known_hosts_text),
            expected_hostname: &self.ssh_host,
            expected_ssh_port: self.ssh_port,
            planned_ssh_port: self.ssh_port,
            expected_algorithm: "ssh-ed25519",
            expected_key_b64: &self.expected_key_b64,
            expected_fingerprint_sha256: &self.pinned_host_key_sha256,
            tofu_permitted: false,
            accept_unknown_host: false,
            host_key_verification_disabled: false,
        }
    }
}

/// Immutable recovery HTTP target: origin is validated before any socket and never rewritten
/// from redirects, environment proxies, or a later mutation of [`SubmitTarget`].
struct RecoveryHttp {
    origin: AuthorizedOrigin,
    token: String,
    kind: RecoveryHttpKind,
}

impl std::fmt::Debug for RecoveryHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryHttp")
            .field("origin", &self.origin.as_str())
            .field("token", &"<redacted>")
            .finish()
    }
}

impl RecoveryHttp {
    fn from_target(target: &SubmitTarget) -> Result<Self, String> {
        if LIVE_NON_LOOPBACK_POST_ENABLED {
            return Err("REFUSE RECOVERY EXEC: non-loopback recovery POST is not enabled".into());
        }
        Ok(Self {
            origin: authorize_recovery_origin(&target.base_url)?,
            token: target.token.clone(),
            kind: RecoveryHttpKind::DisposableLoopback,
        })
    }

    fn from_target_collected_tunnel(
        target: &SubmitTarget,
        guard: DisposableTunnelHttpGuard,
    ) -> Result<Self, String> {
        if LIVE_NON_LOOPBACK_POST_ENABLED {
            return Err("REFUSE RECOVERY EXEC: non-loopback recovery POST is not enabled".into());
        }
        Ok(Self {
            origin: authorize_recovery_origin(&target.base_url)?,
            token: target.token.clone(),
            kind: RecoveryHttpKind::CollectedTunnel(guard),
        })
    }

    #[cfg(test)]
    fn from_target_direct_stream(
        target: &SubmitTarget,
        plan: DisposableDirectStreamPlan,
    ) -> Result<Self, String> {
        if LIVE_NON_LOOPBACK_POST_ENABLED {
            return Err("REFUSE RECOVERY EXEC: non-loopback recovery POST is not enabled".into());
        }
        Ok(Self {
            origin: authorize_recovery_origin(&target.base_url)?,
            token: target.token.clone(),
            kind: RecoveryHttpKind::DisposableDirectStream(plan),
        })
    }
}

fn is_tunnel_guard_refusal(err: &str) -> bool {
    err.contains("REFUSE TUNNEL OBSERVE")
        || err.contains("REFUSE TUNNEL ATTEST")
        || err.contains("REFUSE HOST KEY")
}

fn is_halt_transport_refusal(err: &str) -> bool {
    is_tunnel_guard_refusal(err)
        || err.contains("REFUSE STREAM PLAN")
        || err.contains("REFUSE HOST KEY")
}

fn format_guard_reasons(token: &str, reasons: Vec<String>) -> String {
    redact_secret(&reasons.join("; "), token)
}

fn redact_secret(text: &str, token: &str) -> String {
    let mut s = text.to_string();
    if !token.is_empty() {
        s = s.replace(token, "<redacted>");
    }
    if s.to_ascii_lowercase().contains("authorization") {
        s = s.replace("Authorization", "<redacted-header>");
        s = s.replace("authorization", "<redacted-header>");
    }
    s
}

fn recovery_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(RECOVERY_HTTP_CONNECT)
        .timeout_read(RECOVERY_HTTP_IO)
        .timeout_write(RECOVERY_HTTP_IO)
        .timeout(RECOVERY_HTTP_DEADLINE)
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .redirects(0)
        .try_proxy_from_env(false)
        .build()
}

fn validate_recovery_path(method: &str, path: &str) -> Result<(), String> {
    if !path.starts_with("/v1/libraries/") {
        return Err("REFUSE RECOVERY TRANSPORT: unexpected path".into());
    }
    if path.contains("://") || path.contains('@') || path.contains('#') {
        return Err("REFUSE RECOVERY TRANSPORT: malformed path".into());
    }
    match method {
        "POST" => {
            if path.contains('?') {
                return Err("REFUSE RECOVERY TRANSPORT: POST path must not contain a query".into());
            }
            if !path.ends_with("/operations") {
                return Err("REFUSE RECOVERY TRANSPORT: unexpected POST path".into());
            }
        }
        "GET" => {
            let Some((base, query)) = path.split_once('?') else {
                return Err("REFUSE RECOVERY TRANSPORT: GET tail path requires a query".into());
            };
            if !base.ends_with("/operations") {
                return Err("REFUSE RECOVERY TRANSPORT: unexpected GET path".into());
            }
            let mut after_ok = false;
            let mut limit_ok = false;
            for part in query.split('&') {
                if let Some(v) = part.strip_prefix("after=") {
                    if v.parse::<i64>().is_err() {
                        return Err("REFUSE RECOVERY TRANSPORT: invalid after=".into());
                    }
                    after_ok = true;
                } else if let Some(v) = part.strip_prefix("limit=") {
                    if v.parse::<u64>().is_err() {
                        return Err("REFUSE RECOVERY TRANSPORT: invalid limit=".into());
                    }
                    limit_ok = true;
                } else {
                    return Err("REFUSE RECOVERY TRANSPORT: unexpected query parameter".into());
                }
            }
            if !after_ok || !limit_ok {
                return Err("REFUSE RECOVERY TRANSPORT: GET query must be after and limit".into());
            }
        }
        _ => {
            return Err(format!(
                "REFUSE RECOVERY TRANSPORT: unsupported method {method}"
            ))
        }
    }
    Ok(())
}

/// Page IDs that must not be minted: checkpoint live + retired/deleted, plus tail-referenced pages.
/// Never returns an empty stand-in for a failed or mismatched checkpoint.
pub fn checkpoint_namespace(
    checkpoint: &LibraryState,
    library_id: LibraryId,
    tail: &[ServerTailItem],
) -> Result<BTreeSet<PageId>, String> {
    if let Err(e) = checkpoint.validate() {
        return Err(format!("REFUSE RECOVERY EXEC: invalid checkpoint: {e}"));
    }
    if checkpoint.snapshot.library_id != library_id {
        return Err("REFUSE RECOVERY EXEC: checkpoint library id is incompatible".into());
    }
    let mut ids = BTreeSet::new();
    for n in &checkpoint.snapshot.notebooks {
        for p in &n.pages {
            ids.insert(p.page_id);
        }
    }
    ids.extend(checkpoint.history.deleted_pages.iter().copied());
    for item in tail {
        ids.extend(page_ids_in_operation(&item.envelope.operation));
    }
    let live_pages: usize = checkpoint
        .snapshot
        .notebooks
        .iter()
        .map(|n| n.pages.len())
        .sum();
    if live_pages > 0 && ids.is_empty() {
        return Err("REFUSE RECOVERY EXEC: checkpoint has pages but no page identities".into());
    }
    Ok(ids)
}

fn page_ids_in_operation(op: &SyncOperationV1) -> Vec<PageId> {
    match op {
        SyncOperationV1::CreateNotebook { first_page, .. } => vec![first_page.page_id],
        SyncOperationV1::CreatePage {
            page,
            after_page_id,
            ..
        } => {
            let mut v = vec![page.page_id];
            if let Some(a) = after_page_id {
                v.push(*a);
            }
            v
        }
        SyncOperationV1::MovePage {
            page_id,
            after_page_id,
            ..
        } => {
            let mut v = vec![*page_id];
            if let Some(a) = after_page_id {
                v.push(*a);
            }
            v
        }
        SyncOperationV1::DeletePage { page_id, .. }
        | SyncOperationV1::AddStroke { page_id, .. }
        | SyncOperationV1::EraseRegion { page_id, .. }
        | SyncOperationV1::DeleteStrokeLineage { page_id, .. }
        | SyncOperationV1::ReplaceStrokeLineages { page_id, .. }
        | SyncOperationV1::MoveStrokeLineages { page_id, .. }
        | SyncOperationV1::AddText { page_id, .. }
        | SyncOperationV1::UpdateText { page_id, .. }
        | SyncOperationV1::DeleteText { page_id, .. }
        | SyncOperationV1::AddPageLink { page_id, .. }
        | SyncOperationV1::DeletePageLink { page_id, .. } => vec![*page_id],
        _ => Vec::new(),
    }
}

struct PrepareClaim(PathBuf);

impl Drop for PrepareClaim {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn acquire_prepare_claim(sync_dir: &Path) -> Result<PrepareClaim, String> {
    fs::create_dir_all(sync_dir).map_err(|e| format!("create {}: {e}", sync_dir.display()))?;
    let p = sync_dir.join(PREPARE_CLAIM_FILE);
    match OpenOptions::new().write(true).create_new(true).open(&p) {
        Ok(_) => Ok(PrepareClaim(p)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if read_intent(sync_dir)?.is_some() {
                return Err("REFUSE RECOVERY EXEC: recovery intent already exists".into());
            }
            Err("REFUSE RECOVERY EXEC: concurrent or interrupted prepare".into())
        }
        Err(e) => Err(format!("prepare claim {}: {e}", p.display())),
    }
}

pub fn intent_path(sync_dir: &Path) -> PathBuf {
    sync_dir.join(RECOVERY_INTENT_FILE)
}

pub fn read_intent(sync_dir: &Path) -> Result<Option<RecoveryIntentV1>, String> {
    let p = intent_path(sync_dir);
    match fs::read(&p) {
        Ok(bytes) => {
            let i: RecoveryIntentV1 =
                from_json(&bytes).map_err(|e| format!("REFUSE RECOVERY INTENT: {e}"))?;
            if i.v != RECOVERY_INTENT_VERSION {
                return Err(format!(
                    "REFUSE RECOVERY INTENT: unsupported version {}",
                    i.v
                ));
            }
            if i.upload_authorized {
                return Err("REFUSE RECOVERY INTENT: upload_authorized must be false".into());
            }
            Ok(Some(i))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {}: {e}", p.display())),
    }
}

fn write_intent(
    sync_dir: &Path,
    intent: &RecoveryIntentV1,
    hooks: &RecoveryExecHooks,
) -> Result<(), String> {
    if hooks.fail_during_intent_write {
        return Err("injected: recovery-intent write failed".into());
    }
    fs::create_dir_all(sync_dir).map_err(|e| format!("create {}: {e}", sync_dir.display()))?;
    let bytes = to_canonical_json(intent).map_err(|e| e.to_string())?;
    let tmp = sync_dir.join(format!("{RECOVERY_INTENT_FILE}.tmp"));
    {
        let mut f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(&bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.write_all(b"\n")
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, intent_path(sync_dir)).map_err(|e| format!("rename intent: {e}"))?;
    Ok(())
}

fn pending_from_frozen(op: &FrozenOp) -> Result<PendingOp, String> {
    let bytes = op.envelope.as_bytes().to_vec();
    let envelope: SyncEnvelopeV1 = from_json(&bytes).map_err(|e| e.to_string())?;
    if envelope.client_seq != op.client_seq || envelope.operation_id != op.operation_id {
        return Err("frozen envelope does not match assigned identity".into());
    }
    Ok(PendingOp {
        seq: op.client_seq,
        envelope,
        bytes,
    })
}

fn ensure_outbox(sync_dir: &Path, ops: &[FrozenOp]) -> Result<(), String> {
    let raw = fs::read(sync_dir.join(OUTBOX_FILE)).unwrap_or_default();
    let parsed = parse_journal(&raw)?;
    if parsed.torn_tail || parsed.trailing_prep.is_some() {
        return Err("REFUSE RECOVERY EXEC: outbox is not a valid committed journal".into());
    }
    let have: BTreeSet<u64> = parsed.committed.iter().map(|p| p.seq).collect();
    let mut missing = Vec::new();
    for op in ops {
        if have.contains(&op.client_seq) {
            let existing = parsed
                .committed
                .iter()
                .find(|p| p.seq == op.client_seq)
                .unwrap();
            if existing.bytes != op.envelope.as_bytes() {
                return Err(format!(
                    "REFUSE RECOVERY EXEC: outbox seq {} does not match frozen envelope",
                    op.client_seq
                ));
            }
        } else {
            missing.push(pending_from_frozen(op)?);
        }
    }
    if !missing.is_empty() {
        append_journal(sync_dir, &missing)?;
    }
    Ok(())
}

/// Bind identities if needed, freeze the production plan with durable OperationIds and sequences.
pub fn prepare_recovery(
    spec: &PrepareSpec<'_>,
    hooks: &RecoveryExecHooks,
) -> Result<RecoveryIntentV1, String> {
    if spec.require_disposable {
        require_disposable_exec_root(spec.library_root)?;
        require_disposable_exec_root(spec.sync_dir)?;
        require_disposable_bind_root(spec.library_root)?;
    }
    if let Some(existing) = read_intent(spec.sync_dir)? {
        register_paths(spec.library_root, spec.sync_dir);
        publish_recovery_maintenance(spec.library_root)?;
        return Ok(existing);
    }
    let _claim = acquire_prepare_claim(spec.sync_dir)?;
    if let Some(existing) = read_intent(spec.sync_dir)? {
        register_paths(spec.library_root, spec.sync_dir);
        publish_recovery_maintenance(spec.library_root)?;
        return Ok(existing);
    }
    let identity_for_lib = read_identity(spec.library_root)?
        .ok_or_else(|| "REFUSE RECOVERY EXEC: missing identity.json".to_string())?;
    let checkpoint_pages = checkpoint_namespace(
        spec.checkpoint,
        identity_for_lib.library_id,
        spec.server_tail,
    )?;
    if hooks.fail_before_identity {
        return Err("injected: before identity persistence".into());
    }
    let insp = inspect_identity_read_only(spec.library_root)?;
    if !insp.unbound_pages.is_empty() {
        prepare_unbound_page_identities(
            spec.library_root,
            &checkpoint_pages,
            spec.require_disposable,
            &Default::default(),
        )?;
    }
    if hooks.fail_after_identity {
        return Err("injected: after identity persistence".into());
    }
    register_paths(spec.library_root, spec.sync_dir);
    let _hold = hold_authoring_for_maintenance(spec.library_root)?;
    let identity = read_identity(spec.library_root)?
        .ok_or_else(|| "REFUSE RECOVERY EXEC: missing identity.json".to_string())?;
    let local = build_snapshot_read_only(spec.library_root, &identity)?;
    if local.library_id != spec.checkpoint.snapshot.library_id {
        return Err("REFUSE RECOVERY EXEC: library id mismatch".into());
    }
    let pending_raw = fs::read(spec.sync_dir.join(OUTBOX_FILE)).unwrap_or_default();
    let parsed = parse_journal(&pending_raw)?;
    if parsed.torn_tail || parsed.trailing_prep.is_some() {
        return Err("REFUSE RECOVERY EXEC: invalid outbox".into());
    }
    let pending: Vec<_> = parsed.committed.into_iter().map(|p| p.envelope).collect();
    let result = plan_recovery(RecoveryInputs {
        library_id: local.library_id,
        replica_id: spec.replica_id,
        checkpoint: spec.checkpoint,
        checkpoint_cursor: spec.checkpoint_cursor,
        acked_client_seq: spec.acked_client_seq,
        accepted: &[],
        outbox: OutboxInput::Valid {
            envelopes: &pending,
        },
        local: &local,
        server_tail: spec.server_tail,
        sequence_authority: SequenceAuthority::Known {
            next_client_seq: spec.next_client_seq,
        },
    });
    let PlanResult::Feasible(plan) = result else {
        return Err(format!("REFUSE RECOVERY EXEC: planner blocked: {result:?}"));
    };
    if !plan.sequencing.legal_submission_order {
        return Err("REFUSE RECOVERY EXEC: no legal client-sequence order".into());
    }
    let mut seq = spec.next_client_seq;
    let mut ops = Vec::with_capacity(plan.candidates.len());
    for c in &plan.candidates {
        let env = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: OperationId::from_uuid(Uuid::now_v7()).map_err(|e| e.to_string())?,
            library_id: local.library_id,
            replica_id: spec.replica_id,
            client_seq: seq,
            operation: c.operation.clone(),
        };
        let bytes = env.to_canonical_bytes().map_err(|e| e.to_string())?;
        ops.push(FrozenOp {
            client_seq: seq,
            operation_id: env.operation_id,
            envelope: String::from_utf8(bytes).map_err(|e| e.to_string())?,
        });
        seq = seq
            .checked_add(1)
            .ok_or_else(|| "client_seq exhausted".to_string())?;
    }
    let identity_sha = file_sha256(&identity_path(spec.library_root))?
        .ok_or_else(|| "missing identity.json".to_string())?;
    let local_sha = snapshot_sha256(&local)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())?;
    let intent = RecoveryIntentV1 {
        v: RECOVERY_INTENT_VERSION,
        transaction_id: Uuid::now_v7().hyphenated().to_string(),
        phase: RecoveryPhase::SequencesAssigned,
        library_id: local.library_id,
        replica_id: spec.replica_id,
        genesis_sha256: spec.genesis_sha256.clone(),
        checkpoint_cursor: spec.checkpoint_cursor,
        expected_server_head: spec.checkpoint_cursor,
        acked_client_seq: spec.acked_client_seq,
        next_client_seq_after: seq,
        local_snapshot_sha256: local_sha,
        identity_sha256: identity_sha,
        plan_fingerprint: plan.fingerprint,
        ops,
        accepted_prefix: 0,
        upload_authorized: false,
        halt_reason: None,
        mutation_gen: read_mutation_gen(spec.library_root)?,
        submit_attempted: false,
    };
    write_intent(spec.sync_dir, &intent, hooks)?;
    publish_recovery_maintenance(spec.library_root)?;
    ensure_outbox(spec.sync_dir, &intent.ops)?;
    if let Some(mut st) = read_sync_state(spec.sync_dir)? {
        st.next_client_seq = intent.next_client_seq_after;
        write_sync_state(spec.sync_dir, &st)?;
    }
    if hooks.fail_after_intent_before_upload {
        return Err("injected: after intent, before upload".into());
    }
    Ok(intent)
}

fn read_sync_state(sync_dir: &Path) -> Result<Option<SyncStateV1>, String> {
    let p = sync_dir.join(STATE_FILE);
    match fs::read(&p) {
        Ok(b) => Ok(Some(
            serde_json::from_slice(&b).map_err(|e| format!("sync-state: {e}"))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn write_sync_state(sync_dir: &Path, st: &SyncStateV1) -> Result<(), String> {
    let mut bytes = to_canonical_json(st).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let tmp = sync_dir.join(format!("{STATE_FILE}.tmp"));
    {
        let mut f = File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(&bytes).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, sync_dir.join(STATE_FILE)).map_err(|e| e.to_string())
}

pub fn reconstruct_prefix(
    intent: &RecoveryIntentV1,
    tail: &[(i64, SyncEnvelopeV1)],
) -> Result<usize, String> {
    let mut n = 0usize;
    for op in &intent.ops {
        if let Some((_, found)) = tail.iter().find(|(_, e)| e.operation_id == op.operation_id) {
            if found.client_seq != op.client_seq {
                return Err(format!(
                    "identity/sequence conflict: OperationId {} reused with different client_seq",
                    op.operation_id
                ));
            }
            let frozen: SyncEnvelopeV1 =
                from_json(op.envelope.as_bytes()).map_err(|e| e.to_string())?;
            if found != &frozen {
                return Err(
                    "identity/sequence conflict: same OperationId with different payload".into(),
                );
            }
            n += 1;
        } else if tail
            .iter()
            .any(|(_, e)| e.replica_id == intent.replica_id && e.client_seq == op.client_seq)
        {
            return Err(format!(
                "identity/sequence conflict: client_seq {} reused with different OperationId",
                op.client_seq
            ));
        } else {
            break;
        }
    }
    Ok(n)
}

/// Recovery GET/POST. Validates origin and path before constructing the request URL.
/// Redirects are not followed. Timeouts are finite. Environment proxies are ignored.
/// Disposable loopback path: origin policy only (existing tests).
pub(crate) fn recovery_http_exchange(
    target: &SubmitTarget,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<(u16, Vec<u8>), String> {
    let http = RecoveryHttp::from_target(target)?;
    http_call(&http, method, path, body)
}

/// Same HTTP helper with a collected-tunnel guard. Recollects before Authorization.
pub(crate) fn recovery_http_exchange_guarded(
    target: &SubmitTarget,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    guard: DisposableTunnelHttpGuard,
) -> Result<(u16, Vec<u8>), String> {
    let http = RecoveryHttp::from_target_collected_tunnel(target, guard)?;
    http_call(&http, method, path, body)
}

fn http_call(
    http: &RecoveryHttp,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<(u16, Vec<u8>), String> {
    validate_recovery_path(method, path)?;
    #[cfg(test)]
    if let RecoveryHttpKind::DisposableDirectStream(plan) = &http.kind {
        return direct_stream_http_call(http, plan, method, path, body);
    }
    if let RecoveryHttpKind::CollectedTunnel(guard) = &http.kind {
        if let Err(reasons) = guard.require_fresh_before_http(http.origin.as_str()) {
            return Err(format_guard_reasons(&http.token, reasons));
        }
    }
    let url = format!("{}{path}", http.origin.as_str());
    let mut req = recovery_agent().request(method, &url);
    if !http.token.is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", http.token));
    }
    let res = match body {
        Some(b) => req.send_bytes(b),
        None => req.call(),
    };
    let resp = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => {
            return Err(format!(
                "transport: {}",
                redact_secret(&e.to_string(), &http.token)
            ))
        }
    };
    let status = resp.status();
    if (300..400).contains(&status) {
        return Err(format!(
            "REFUSE RECOVERY TRANSPORT: redirect {status} not followed"
        ));
    }
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut out)
        .map_err(|e| format!("transport: {}", redact_secret(&e.to_string(), &http.token)))?;
    Ok((status, out))
}

#[cfg(test)]
fn direct_stream_http_call(
    http: &RecoveryHttp,
    plan: &DisposableDirectStreamPlan,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<(u16, Vec<u8>), String> {
    let stream_plan = plan.as_stream_plan();
    let host_key = plan.host_key();
    let permit = DisposableStreamPermit::for_synthetic_fixture();
    let mut session =
        SupervisedRecoveryStream::open(permit, &stream_plan, &host_key).map_err(|outcome| {
            match outcome {
                StreamTransportOutcome::PreWriteRefuse { reason }
                | StreamTransportOutcome::WriteStartedAmbiguous { reason } => {
                    redact_secret(&reason, &http.token)
                }
                StreamTransportOutcome::Authoritative { .. } => {
                    "REFUSE RECOVERY STREAM: unexpected spawn success payload".into()
                }
            }
        })?;
    let req = recovery_http1_request(&stream_plan, method, path, &http.token, body)
        .map_err(|e| redact_secret(&e, &http.token))?;
    match session.exchange(&stream_plan, &req) {
        StreamTransportOutcome::Authoritative { status, body } => {
            if (300..400).contains(&status) {
                return Err(format!(
                    "REFUSE RECOVERY TRANSPORT: redirect {status} not followed"
                ));
            }
            Ok((status, body))
        }
        StreamTransportOutcome::WriteStartedAmbiguous { reason }
        | StreamTransportOutcome::PreWriteRefuse { reason } => {
            Err(redact_secret(&reason, &http.token))
        }
    }
}

pub fn fetch_tail(
    target: &SubmitTarget,
    library: LibraryId,
    after: u64,
) -> Result<Vec<(i64, SyncEnvelopeV1)>, String> {
    let http = RecoveryHttp::from_target(target)?;
    fetch_tail_http(&http, library, after)
}

pub(crate) fn fetch_tail_guarded(
    target: &SubmitTarget,
    library: LibraryId,
    after: u64,
    guard: DisposableTunnelHttpGuard,
) -> Result<Vec<(i64, SyncEnvelopeV1)>, String> {
    let http = RecoveryHttp::from_target_collected_tunnel(target, guard)?;
    fetch_tail_http(&http, library, after)
}

fn fetch_tail_http(
    http: &RecoveryHttp,
    library: LibraryId,
    after: u64,
) -> Result<Vec<(i64, SyncEnvelopeV1)>, String> {
    let mut after = after as i64;
    let mut out = Vec::new();
    loop {
        let (s, body) = http_call(
            http,
            "GET",
            &format!("/v1/libraries/{library}/operations?after={after}&limit=50"),
            None,
        )?;
        if s == 401 || s == 403 {
            return Err(format!(
                "authentication refused ({s}): {}",
                redact_secret(&String::from_utf8_lossy(&body), &http.token)
            ));
        }
        if s != 200 {
            return Err(format!(
                "tail GET {s}: {}",
                redact_secret(&String::from_utf8_lossy(&body), &http.token)
            ));
        }
        let v: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| format!("tail json: {e}"))?;
        let ops = v["operations"].as_array().cloned().unwrap_or_default();
        if ops.is_empty() {
            return Ok(out);
        }
        for item in ops {
            let seq = item["server_seq"].as_i64().unwrap_or(0);
            let env: SyncEnvelopeV1 =
                serde_json::from_value(item["envelope"].clone()).map_err(|e| e.to_string())?;
            out.push((seq, env));
            after = after.max(seq);
        }
    }
}

fn submit_one(
    http: &RecoveryHttp,
    library: LibraryId,
    op: &FrozenOp,
) -> Result<(u16, Vec<u8>), String> {
    let mut body = b"{\"envelopes\":[".to_vec();
    body.extend_from_slice(op.envelope.as_bytes());
    body.extend_from_slice(b"]}");
    http_call(
        http,
        "POST",
        &format!("/v1/libraries/{library}/operations"),
        Some(&body),
    )
}

fn persist_prefix(
    sync_dir: &Path,
    intent: &mut RecoveryIntentV1,
    prefix: usize,
    hooks: &RecoveryExecHooks,
) -> Result<(), String> {
    let phase = if prefix == 0 {
        RecoveryPhase::SequencesAssigned
    } else if prefix < intent.ops.len() {
        RecoveryPhase::PartiallyAccepted
    } else {
        RecoveryPhase::FullyAccepted
    };
    if intent.accepted_prefix == prefix && intent.phase == phase {
        return Ok(());
    }
    intent.accepted_prefix = prefix;
    intent.phase = phase;
    write_intent(sync_dir, intent, hooks)
}

fn halt_durable(
    sync_dir: &Path,
    intent: &mut RecoveryIntentV1,
    prefix: usize,
    reason: String,
    hooks: &RecoveryExecHooks,
) -> Result<ExecOutcome, String> {
    intent.accepted_prefix = prefix;
    intent.halt_reason = Some(reason.clone());
    intent.phase = RecoveryPhase::Halted;
    intent.upload_authorized = false;
    write_intent(sync_dir, intent, hooks)?;
    Ok(ExecOutcome::Halted {
        accepted_prefix: prefix,
        reason,
    })
}

fn prefix_from_tail(
    intent: &RecoveryIntentV1,
    http: &RecoveryHttp,
) -> Result<Result<usize, String>, String> {
    let tail = fetch_tail_http(http, intent.library_id, intent.checkpoint_cursor)?;
    Ok(reconstruct_prefix(intent, &tail))
}

fn frozen_op_policy_name(op: &SyncOperationV1) -> &'static str {
    match op {
        SyncOperationV1::AddStroke { .. } => "AddStroke",
        SyncOperationV1::ReplaceStrokeLineages { .. } => "ReplaceStrokeLineages",
        SyncOperationV1::CreatePage { .. } => "CreatePage",
        _ => "unsupported",
    }
}

fn reject_protected_recovery_path(path: &Path) -> Result<(), String> {
    let s = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    // Match require_disposable_exec_root: Windows fixtures plus portable
    // checkout roots (Linux CI does not use c:\dev\project\...).
    let manifest = env!("CARGO_MANIFEST_DIR")
        .replace('/', "\\")
        .to_ascii_lowercase();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .map(|p| p.to_string_lossy().replace('/', "\\").to_ascii_lowercase())
        .unwrap_or_default();
    if s.contains("ignite-divergence-analysis-18d3\\reference")
        || s.contains("ignite-currentstate-18e6c")
        || s.contains("ignite-divergence-analysis-18d3\\work\\18e6d")
        || s.contains("ignite-livetail-18e6b")
        || s.contains("c:\\dev\\project\\ignite\\")
        || s.contains("c:\\dev\\project\\scribe\\")
        || (!manifest.is_empty() && s.contains(&manifest))
        || (!workspace.is_empty() && (s == workspace || s.starts_with(&(workspace.clone() + "\\"))))
    {
        return Err(format!(
            "REFUSE RECOVERY EXEC: {} is a protected or product path",
            path.display()
        ));
    }
    Ok(())
}

fn enforce_exec_policy(
    http: &RecoveryHttp,
    intent: &RecoveryIntentV1,
    library_root: Option<&Path>,
    sync_dir: &Path,
) -> Result<(), String> {
    reject_protected_recovery_path(sync_dir)?;
    if let Some(root) = library_root {
        reject_protected_recovery_path(root)?;
    }
    let lib = intent.library_id.to_string();
    let replica = intent.replica_id.to_string();
    let genesis = intent.genesis_sha256.clone();
    let mut expected_lib = lib.clone();
    let mut expected_replica = replica.clone();
    let expected_genesis = genesis.clone();
    if let Some(st) = read_sync_state(sync_dir)? {
        expected_lib = st.library_id.to_string();
        expected_replica = st.replica_id.to_string();
    }
    if let Some(root) = library_root {
        if let Some(id) = read_identity(root)? {
            expected_lib = id.library_id.to_string();
        }
    }
    let mut proposed: Vec<&str> = Vec::new();
    for op in &intent.ops {
        let env: SyncEnvelopeV1 =
            from_json(op.envelope.as_bytes()).map_err(|e| format!("frozen envelope: {e}"))?;
        proposed.push(frozen_op_policy_name(&env.operation));
    }
    let fence_active = recovery_blocks_ordinary_push(sync_dir);
    let input = RecoveryPolicyInput {
        gate: RecoveryGate::AFreshEvidence,
        proposed_base_url: http.origin.as_str(),
        transport: RecoveryTransport::LoopbackHttpViaAuthenticatedTunnel,
        tunnel_origin_host: "",
        library_id: &lib,
        replica_id: &replica,
        genesis_sha256: &genesis,
        expected_library_id: &expected_lib,
        expected_replica_id: &expected_replica,
        expected_genesis_sha256: &expected_genesis,
        expected_history_cursor: intent.expected_server_head,
        observed_history_cursor: Some(intent.checkpoint_cursor),
        intent_present: true,
        fence_active,
        completion_proof_present: false,
        accepted_prefix_complete: intent.accepted_prefix == intent.ops.len()
            && !intent.ops.is_empty(),
        server_accept_replace: false,
        client_decodes_replace: true,
        participating_clients_compatible: true,
        credential_source: CredentialSource::MemoryOnly,
        allowed_operation_types: ALLOWED_RECOVERY_OP_TYPES,
        proposed_operation_types: &proposed,
        local_library_approved: true,
        local_sync_approved: true,
        redirects_permitted: false,
        request_non_loopback_post: false,
        tunnel_identity_verified: false,
    };
    evaluate_recovery_policy(&input).map_err(|reasons| reasons.join("; "))?;
    Ok(())
}

fn identity_page_ids(identity: &sync_protocol::IdentityFileV1) -> BTreeSet<PageId> {
    identity
        .pages
        .values()
        .flat_map(|m| m.values().copied())
        .collect()
}

/// Outbox and sequence/snapshot assumptions that must still hold before any new POST.
pub fn revalidate_prepared(
    sync_dir: &Path,
    library_root: Option<&Path>,
    intent: &RecoveryIntentV1,
) -> Result<(), String> {
    if intent.upload_authorized {
        return Err("REFUSE RECOVERY EXEC: upload_authorized must be false".into());
    }
    if intent.ops.is_empty() && intent.accepted_prefix != 0 {
        return Err("REFUSE RECOVERY EXEC: empty plan with nonzero prefix".into());
    }
    let mut seen_seq = BTreeSet::new();
    let mut seen_id = BTreeSet::new();
    for op in &intent.ops {
        if !seen_seq.insert(op.client_seq) {
            return Err("REFUSE RECOVERY EXEC: duplicate frozen client_seq".into());
        }
        if !seen_id.insert(op.operation_id) {
            return Err("REFUSE RECOVERY EXEC: duplicate frozen OperationId".into());
        }
        let env: SyncEnvelopeV1 =
            from_json(op.envelope.as_bytes()).map_err(|e| format!("frozen envelope: {e}"))?;
        if env.client_seq != op.client_seq || env.operation_id != op.operation_id {
            return Err("REFUSE RECOVERY EXEC: frozen envelope identity mismatch".into());
        }
        if env.library_id != intent.library_id || env.replica_id != intent.replica_id {
            return Err("REFUSE RECOVERY EXEC: frozen envelope library/replica mismatch".into());
        }
        let bytes = env.to_canonical_bytes().map_err(|e| e.to_string())?;
        if bytes != op.envelope.as_bytes() {
            return Err("REFUSE RECOVERY EXEC: frozen envelope bytes are not canonical".into());
        }
    }
    let raw = fs::read(sync_dir.join(OUTBOX_FILE)).unwrap_or_default();
    let parsed = parse_journal(&raw)?;
    if parsed.torn_tail || parsed.trailing_prep.is_some() {
        return Err("REFUSE RECOVERY EXEC: outbox is not a valid committed journal".into());
    }
    let first_frozen = intent.ops.first().map(|o| o.client_seq);
    let reserved: BTreeSet<u64> = intent.ops.iter().map(|o| o.client_seq).collect();
    for p in &parsed.committed {
        if let Some(first) = first_frozen {
            if p.seq >= first && !reserved.contains(&p.seq) {
                return Err(format!(
                    "REFUSE RECOVERY EXEC: outbox seq {} is outside the frozen plan",
                    p.seq
                ));
            }
        }
        if reserved.contains(&p.seq) {
            let frozen = intent.ops.iter().find(|o| o.client_seq == p.seq).unwrap();
            if p.bytes != frozen.envelope.as_bytes() {
                return Err(format!(
                    "REFUSE RECOVERY EXEC: outbox seq {} does not match frozen envelope",
                    p.seq
                ));
            }
        }
    }
    if let Some(st) = read_sync_state(sync_dir)? {
        if st.library_id != intent.library_id || st.replica_id != intent.replica_id {
            return Err("REFUSE RECOVERY EXEC: sync-state identity mismatch".into());
        }
        if st.acked_client_seq != intent.acked_client_seq {
            return Err("REFUSE RECOVERY EXEC: acknowledged client sequence changed".into());
        }
        if st.next_client_seq != intent.next_client_seq_after {
            return Err("REFUSE RECOVERY EXEC: sequence authority changed".into());
        }
        if st.server_cursor != intent.checkpoint_cursor {
            return Err("REFUSE RECOVERY EXEC: confirmed cursor changed".into());
        }
    }
    let Some(root) = library_root else {
        return Ok(());
    };
    let identity = read_identity(root)?
        .ok_or_else(|| "REFUSE RECOVERY EXEC: missing identity.json".to_string())?;
    let identity_sha =
        file_sha256(&identity_path(root))?.ok_or_else(|| "missing identity.json".to_string())?;
    if identity_sha != intent.identity_sha256 {
        return Err("stale-local-state: identity bindings changed".into());
    }
    let live = identity_page_ids(&identity);
    for op in &intent.ops {
        let env: SyncEnvelopeV1 = from_json(op.envelope.as_bytes()).map_err(|e| e.to_string())?;
        for pid in page_ids_in_operation(&env.operation) {
            if !live.contains(&pid) && !matches!(env.operation, SyncOperationV1::CreatePage { .. })
            {
                return Err("stale-local-state: frozen page binding is missing".into());
            }
        }
        if let SyncOperationV1::CreatePage { page, .. } = &env.operation {
            if !live.contains(&page.page_id) {
                return Err("stale-local-state: CreatePage identity is not durable".into());
            }
        }
    }
    let local = build_snapshot_read_only(root, &identity)?;
    let sha = snapshot_sha256(&local)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())?;
    if sha != intent.local_snapshot_sha256 {
        return Err("stale-local-state: local snapshot fingerprint changed".into());
    }
    let gen = read_mutation_gen(root)?;
    if gen != intent.mutation_gen {
        return Err("stale-local-state: library mutation generation changed".into());
    }
    Ok(())
}

/// Operator abort. Never rewinds `next_client_seq` or deletes the outbox. Intent is retained.
pub fn abort_prepared_recovery(sync_dir: &Path) -> Result<AbortReport, String> {
    let mut intent = read_intent(sync_dir)?.ok_or("REFUSE ABORT: missing intent")?;
    let reason = if intent.accepted_prefix > 0 || intent.submit_attempted {
        "abort refused to clean up: submission may have reached the server; intent retained"
            .to_string()
    } else {
        "operator abort before proven submission; reservations retained (cleanup not supported)"
            .to_string()
    };
    intent.phase = RecoveryPhase::Halted;
    intent.halt_reason = Some(reason.clone());
    intent.upload_authorized = false;
    write_intent(sync_dir, &intent, &RecoveryExecHooks::default())?;
    Ok(AbortReport {
        intent_retained: true,
        cleanup_supported: false,
        accepted_prefix: intent.accepted_prefix,
        submit_attempted: intent.submit_attempted,
        reason,
    })
}

/// Submit remaining frozen operations. Reconstructs the accepted prefix from the tail first.
///
/// `library_root` is required when the intent was produced by [`prepare_recovery`] so the
/// post-binding snapshot fingerprint can be rechecked. Synthetic freeze-only tests may pass `None`.
pub fn submit_recovery(
    sync_dir: &Path,
    library_root: Option<&Path>,
    target: &SubmitTarget,
    hooks: &RecoveryExecHooks,
) -> Result<ExecOutcome, String> {
    let http = RecoveryHttp::from_target(target)?;
    submit_recovery_http(sync_dir, library_root, hooks, http)
}

/// Submit through the disposable supervised direct stream. Not reachable from
/// [`submit_recovery`] or live recovery config.
#[cfg(test)]
pub(crate) fn submit_recovery_with_direct_stream(
    sync_dir: &Path,
    library_root: Option<&Path>,
    target: &SubmitTarget,
    hooks: &RecoveryExecHooks,
    plan: DisposableDirectStreamPlan,
) -> Result<ExecOutcome, String> {
    let http = RecoveryHttp::from_target_direct_stream(target, plan)?;
    submit_recovery_http(sync_dir, library_root, hooks, http)
}

/// Submit through the collected-tunnel guard. Not reachable from live recovery config.
pub(crate) fn submit_recovery_with_tunnel_guard(
    sync_dir: &Path,
    library_root: Option<&Path>,
    target: &SubmitTarget,
    hooks: &RecoveryExecHooks,
    guard: DisposableTunnelHttpGuard,
) -> Result<ExecOutcome, String> {
    let http = RecoveryHttp::from_target_collected_tunnel(target, guard)?;
    submit_recovery_http(sync_dir, library_root, hooks, http)
}

fn submit_recovery_http(
    sync_dir: &Path,
    library_root: Option<&Path>,
    hooks: &RecoveryExecHooks,
    http: RecoveryHttp,
) -> Result<ExecOutcome, String> {
    let mut intent = read_intent(sync_dir)?.ok_or("REFUSE RECOVERY EXEC: missing intent")?;
    if matches!(
        intent.phase,
        RecoveryPhase::Finalized | RecoveryPhase::Verified | RecoveryPhase::ConfirmedReconciled
    ) {
        return Ok(ExecOutcome::Complete {
            accepted_prefix: intent.ops.len(),
        });
    }
    if matches!(intent.phase, RecoveryPhase::Halted) {
        return Ok(ExecOutcome::Halted {
            accepted_prefix: intent.accepted_prefix,
            reason: intent
                .halt_reason
                .clone()
                .unwrap_or_else(|| "halted".into()),
        });
    }
    if let Err(e) = enforce_exec_policy(&http, &intent, library_root, sync_dir) {
        let kept = intent.accepted_prefix;
        return halt_durable(sync_dir, &mut intent, kept, e, hooks);
    }
    if let Err(e) = revalidate_prepared(sync_dir, library_root, &intent) {
        if e.contains("stale-local-state") {
            intent.halt_reason = Some(e.clone());
            write_intent(sync_dir, &intent, hooks)?;
            return Ok(ExecOutcome::Halted {
                accepted_prefix: intent.accepted_prefix,
                reason: e,
            });
        }
        let kept = intent.accepted_prefix;
        return halt_durable(sync_dir, &mut intent, kept, e, hooks);
    }
    if let Some(hook) = hooks.after_revalidate {
        hook();
    }
    let prefix = match prefix_from_tail(&intent, &http) {
        Ok(Ok(p)) => p,
        Ok(Err(reason)) => {
            let kept = intent.accepted_prefix;
            return halt_durable(sync_dir, &mut intent, kept, reason, hooks);
        }
        Err(e) if e.contains("authentication refused") || is_halt_transport_refusal(&e) => {
            let kept = intent.accepted_prefix;
            return halt_durable(sync_dir, &mut intent, kept, e, hooks);
        }
        Err(e) => return Err(e),
    };
    persist_prefix(sync_dir, &mut intent, prefix, hooks)?;
    ensure_outbox(sync_dir, &intent.ops)?;
    if let Some(hook) = hooks.after_tail {
        hook();
    }
    if prefix == intent.ops.len() {
        return Ok(ExecOutcome::Complete {
            accepted_prefix: prefix,
        });
    }
    if !intent.submit_attempted {
        intent.submit_attempted = true;
        write_intent(sync_dir, &intent, hooks)?;
    }
    intent.phase = RecoveryPhase::Submitting;
    let mut new_accepts = 0usize;
    let mut drop_ack = hooks.drop_next_ack;
    let ops = intent.ops.clone();
    let total = ops.len();
    for (i, op) in ops.iter().enumerate().skip(prefix) {
        if let Some(limit) = hooks.halt_after_new_accepts {
            if new_accepts >= limit {
                persist_prefix(sync_dir, &mut intent, i, hooks)?;
                return Ok(ExecOutcome::Progress {
                    accepted_prefix: i,
                    total,
                });
            }
        }
        if let Err(e) = revalidate_prepared(sync_dir, library_root, &intent) {
            if e.contains("stale-local-state") {
                intent.halt_reason = Some(e.clone());
                write_intent(sync_dir, &intent, hooks)?;
                return Ok(ExecOutcome::Halted {
                    accepted_prefix: intent.accepted_prefix,
                    reason: e,
                });
            }
            let kept = intent.accepted_prefix;
            return halt_durable(sync_dir, &mut intent, kept, e, hooks);
        }
        let (status, body) = match submit_one(&http, intent.library_id, op) {
            Ok(v) => v,
            Err(e) if is_halt_transport_refusal(&e) => {
                let kept = intent.accepted_prefix;
                return halt_durable(sync_dir, &mut intent, kept, e, hooks);
            }
            Err(e) => {
                let p = match prefix_from_tail(&intent, &http) {
                    Ok(Ok(p)) => p,
                    Ok(Err(reason)) => {
                        return halt_durable(sync_dir, &mut intent, i, reason, hooks)
                    }
                    Err(te) if te.contains("authentication refused") => {
                        return halt_durable(sync_dir, &mut intent, i, te, hooks)
                    }
                    Err(_te) => {
                        persist_prefix(sync_dir, &mut intent, i, hooks)?;
                        return Ok(ExecOutcome::Progress {
                            accepted_prefix: i,
                            total,
                        });
                    }
                };
                persist_prefix(sync_dir, &mut intent, p, hooks)?;
                if p > i {
                    new_accepts += 1;
                    continue;
                }
                let _ = e;
                return Ok(ExecOutcome::Progress {
                    accepted_prefix: p,
                    total,
                });
            }
        };
        if status == 200 && !drop_ack {
            new_accepts += 1;
            persist_prefix(sync_dir, &mut intent, i + 1, hooks)?;
            continue;
        }
        if status == 200 && drop_ack {
            drop_ack = false;
        }
        let p = match prefix_from_tail(&intent, &http) {
            Ok(Ok(p)) => p,
            Ok(Err(reason)) => return halt_durable(sync_dir, &mut intent, i, reason, hooks),
            Err(e) if e.contains("authentication refused") => {
                return halt_durable(sync_dir, &mut intent, i, e, hooks)
            }
            Err(_) => {
                persist_prefix(sync_dir, &mut intent, i, hooks)?;
                return Ok(ExecOutcome::Progress {
                    accepted_prefix: i,
                    total,
                });
            }
        };
        if p > i {
            persist_prefix(sync_dir, &mut intent, p, hooks)?;
            new_accepts += 1;
            continue;
        }
        let text = redact_secret(&String::from_utf8_lossy(&body), &http.token);
        let reason = if status == 401 || status == 403 {
            format!("authentication refused ({status})")
        } else if text.contains("unsupported_operation") {
            "ReplaceStrokeLineages gate disabled".into()
        } else if text.contains("semantic_refusal") || status == 422 {
            format!("CAS or semantic refusal: {text}")
        } else if status == 409 {
            format!("identity/sequence conflict: {text}")
        } else if status >= 500 {
            persist_prefix(sync_dir, &mut intent, p, hooks)?;
            return Ok(ExecOutcome::Progress {
                accepted_prefix: p,
                total,
            });
        } else {
            format!("HTTP {status}: {text}")
        };
        return halt_durable(sync_dir, &mut intent, p, reason, hooks);
    }
    persist_prefix(sync_dir, &mut intent, total, hooks)?;
    Ok(ExecOutcome::Complete {
        accepted_prefix: total,
    })
}

/// Confirm that accepted recovery operations already describe local notebook content.
/// Does not re-draw strokes or create pages a second time.
pub fn reconcile_confirmed(
    sync_dir: &Path,
    checkpoint: &LibraryState,
    local: &SyncSnapshotV1,
) -> Result<RecoveryIntentV1, String> {
    let mut intent = read_intent(sync_dir)?.ok_or("missing intent")?;
    if intent.accepted_prefix != intent.ops.len() {
        return Err("REFUSE RECONCILE: accepted prefix is incomplete".into());
    }
    let replayed = replay_frozen(checkpoint, &intent)?;
    if replayed.snapshot != *local {
        return Err(
            "REFUSE RECONCILE: local notebook does not match accepted recovery replay".into(),
        );
    }
    intent.phase = RecoveryPhase::ConfirmedReconciled;
    intent.upload_authorized = false;
    write_intent(sync_dir, &intent, &RecoveryExecHooks::default())?;
    Ok(intent)
}

pub fn mark_verified(sync_dir: &Path) -> Result<RecoveryIntentV1, String> {
    let mut intent = read_intent(sync_dir)?.ok_or("missing intent")?;
    if intent.accepted_prefix != intent.ops.len() {
        return Err("REFUSE FINALIZE: accepted prefix is incomplete".into());
    }
    if !matches!(
        intent.phase,
        RecoveryPhase::FullyAccepted | RecoveryPhase::ConfirmedReconciled | RecoveryPhase::Verified
    ) {
        return Err("REFUSE FINALIZE: accepted history is not fully confirmed".into());
    }
    intent.phase = RecoveryPhase::Verified;
    intent.upload_authorized = false;
    write_intent(sync_dir, &intent, &RecoveryExecHooks::default())?;
    Ok(intent)
}

pub fn mark_finalized(sync_dir: &Path) -> Result<RecoveryIntentV1, String> {
    let mut intent = read_intent(sync_dir)?.ok_or("missing intent")?;
    if intent.phase != RecoveryPhase::Verified && intent.accepted_prefix != intent.ops.len() {
        return Err("REFUSE FINALIZE: recovery is incomplete".into());
    }
    intent.phase = RecoveryPhase::Finalized;
    intent.upload_authorized = false;
    write_intent(sync_dir, &intent, &RecoveryExecHooks::default())?;
    Ok(intent)
}

fn complete_path(sync_dir: &Path) -> PathBuf {
    sync_dir.join(RECOVERY_COMPLETE_FILE)
}

pub fn read_completion(sync_dir: &Path) -> Result<Option<RecoveryCompleteV1>, String> {
    let p = complete_path(sync_dir);
    match fs::read(&p) {
        Ok(bytes) => {
            let c: RecoveryCompleteV1 =
                from_json(&bytes).map_err(|e| format!("REFUSE RECOVERY COMPLETE: {e}"))?;
            if c.v != RECOVERY_COMPLETE_VERSION {
                return Err(format!(
                    "REFUSE RECOVERY COMPLETE: unsupported version {}",
                    c.v
                ));
            }
            Ok(Some(c))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {}: {e}", p.display())),
    }
}

fn write_completion(sync_dir: &Path, proof: &RecoveryCompleteV1) -> Result<(), String> {
    let bytes = to_canonical_json(proof).map_err(|e| e.to_string())?;
    let tmp = sync_dir.join(format!("{RECOVERY_COMPLETE_FILE}.tmp"));
    {
        let mut f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(&bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.write_all(b"\n")
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, complete_path(sync_dir)).map_err(|e| format!("rename completion: {e}"))
}

fn load_confirmed_state(
    sync_dir: &Path,
    library_id: LibraryId,
) -> Result<(LibraryState, u64), String> {
    let (mut state, through, _) = confirmed::load_checkpoint(sync_dir)?
        .ok_or_else(|| "REFUSE RELEASE: missing confirmed checkpoint".to_string())?;
    let decode = |b: &[u8]| -> Result<SyncEnvelopeV1, String> {
        let env: SyncEnvelopeV1 = from_json(b).map_err(|e| e.to_string())?;
        if env.library_id != library_id {
            return Err("confirmed log envelope library mismatch".into());
        }
        let canon = env.to_canonical_bytes().map_err(|e| e.to_string())?;
        if canon != b {
            return Err("confirmed log envelope is not canonical".into());
        }
        Ok(env)
    };
    let (_scan, last, _) = confirmed::replay_log(sync_dir, &mut state, through, &decode)?;
    Ok((state, last))
}

/// Authoritative GET tail must contain every frozen envelope, exact bytes, in a contiguous
/// server-seq history after the checkpoint cursor.
pub fn verify_accepted_history(
    intent: &RecoveryIntentV1,
    tail: &[(i64, SyncEnvelopeV1)],
) -> Result<VerifiedAcceptance, String> {
    if intent.ops.is_empty() {
        return Err("REFUSE RELEASE: empty recovery plan".into());
    }
    let mut prev = intent.checkpoint_cursor as i64;
    for (seq, env) in tail {
        if *seq != prev + 1 {
            return Err(format!(
                "REFUSE RELEASE: accepted-history gap: expected {} got {seq}",
                prev + 1
            ));
        }
        if env.library_id != intent.library_id {
            return Err("REFUSE RELEASE: tail library mismatch".into());
        }
        prev = *seq;
    }
    let prefix = reconstruct_prefix(intent, tail)?;
    if prefix != intent.ops.len() {
        return Err(format!(
            "REFUSE RELEASE: accepted prefix {prefix} of {} is incomplete",
            intent.ops.len()
        ));
    }
    for op in &intent.ops {
        let Some((_, found)) = tail.iter().find(|(_, e)| e.operation_id == op.operation_id) else {
            return Err("REFUSE RELEASE: frozen operation missing from authoritative tail".into());
        };
        if found.replica_id != intent.replica_id {
            return Err("REFUSE RELEASE: frozen replica mismatch".into());
        }
        if found.client_seq != op.client_seq {
            return Err("REFUSE RELEASE: frozen client_seq mismatch".into());
        }
        let bytes = found.to_canonical_bytes().map_err(|e| e.to_string())?;
        if bytes != op.envelope.as_bytes() {
            return Err(
                "REFUSE RELEASE: frozen envelope bytes differ from accepted history".into(),
            );
        }
    }
    Ok(VerifiedAcceptance {
        accepted_prefix: prefix,
        through_server_seq: prev as u64,
    })
}

fn frozen_seqs_still_in_outbox(
    sync_dir: &Path,
    intent: &RecoveryIntentV1,
    acked: u64,
) -> Result<bool, String> {
    let raw = fs::read(sync_dir.join(OUTBOX_FILE)).unwrap_or_default();
    let parsed = parse_journal(&raw)?;
    if parsed.torn_tail || parsed.trailing_prep.is_some() {
        return Err("REFUSE RELEASE: outbox is not a valid committed journal".into());
    }
    let reserved: BTreeSet<u64> = intent.ops.iter().map(|o| o.client_seq).collect();
    for p in &parsed.committed {
        if reserved.contains(&p.seq) && p.seq <= acked {
            return Ok(true);
        }
        if p.seq <= acked {
            return Err(format!(
                "REFUSE RELEASE: outbox still holds acknowledged seq {}",
                p.seq
            ));
        }
    }
    Ok(false)
}

/// Release the recovery hold only after accepted history, confirmed state, notebooks,
/// identities, and sequence authority all agree. Intent removal is the commit boundary.
pub fn release_recovery_hold(
    sync_dir: &Path,
    library_root: &Path,
    target: &SubmitTarget,
    health: &ReleaseHealth,
    hooks: &ReleaseHooks,
) -> Result<ReleaseReport, String> {
    let http = RecoveryHttp::from_target(target)?;
    release_recovery_hold_http(sync_dir, library_root, health, hooks, http)
}

#[cfg(test)]
pub(crate) fn release_recovery_hold_with_direct_stream(
    sync_dir: &Path,
    library_root: &Path,
    target: &SubmitTarget,
    health: &ReleaseHealth,
    hooks: &ReleaseHooks,
    plan: DisposableDirectStreamPlan,
) -> Result<ReleaseReport, String> {
    let http = RecoveryHttp::from_target_direct_stream(target, plan)?;
    release_recovery_hold_http(sync_dir, library_root, health, hooks, http)
}

pub(crate) fn release_recovery_hold_with_tunnel_guard(
    sync_dir: &Path,
    library_root: &Path,
    target: &SubmitTarget,
    health: &ReleaseHealth,
    hooks: &ReleaseHooks,
    guard: DisposableTunnelHttpGuard,
) -> Result<ReleaseReport, String> {
    let http = RecoveryHttp::from_target_collected_tunnel(target, guard)?;
    release_recovery_hold_http(sync_dir, library_root, health, hooks, http)
}

fn release_recovery_hold_http(
    sync_dir: &Path,
    library_root: &Path,
    health: &ReleaseHealth,
    hooks: &ReleaseHooks,
    http: RecoveryHttp,
) -> Result<ReleaseReport, String> {
    if let Some(existing) = read_completion(sync_dir)? {
        if read_intent(sync_dir)?.is_none() {
            clear_recovery_maintenance(library_root)?;
            return Ok(ReleaseReport {
                intent_removed: true,
                already_released: true,
                server_cursor: existing.server_cursor,
                acked_client_seq: existing.acked_client_seq,
            });
        }
    }
    let intent = read_intent(sync_dir)?.ok_or("REFUSE RELEASE: missing recovery intent")?;
    if matches!(intent.phase, RecoveryPhase::Halted) {
        return Err("REFUSE RELEASE: recovery is halted".into());
    }
    if intent.accepted_prefix != intent.ops.len() {
        return Err("REFUSE RELEASE: accepted history is incomplete".into());
    }
    if intent.ops.is_empty() {
        return Err("REFUSE RELEASE: empty recovery plan".into());
    }
    if let Err(e) = enforce_exec_policy(&http, &intent, Some(library_root), sync_dir) {
        return Err(e);
    }
    let tail = fetch_tail_http(&http, intent.library_id, intent.checkpoint_cursor)?;
    let verified = verify_accepted_history(&intent, &tail)?;
    if hooks.fail_after_tail_verify {
        return Err("injected: after accepted-tail verification".into());
    }
    if health.journal_error.is_some() {
        return Err("REFUSE RELEASE: unresolved journal error".into());
    }
    if let Some(b) = &health.seq_block {
        return Err(format!("REFUSE RELEASE: sequence authority blocked: {b}"));
    }
    if !health.seq_ready {
        return Err("REFUSE RELEASE: sequence authority is not ready".into());
    }
    if let Some(f) = &health.sync_fault {
        return Err(format!("REFUSE RELEASE: unrelated sync fault remains: {f}"));
    }
    if sync_dir.join(APPLY_FILE).exists() {
        return Err("REFUSE RELEASE: unresolved apply marker".into());
    }
    let st = read_sync_state(sync_dir)?
        .ok_or_else(|| "REFUSE RELEASE: missing sync-state".to_string())?;
    if st.library_id != intent.library_id || st.replica_id != intent.replica_id {
        return Err("REFUSE RELEASE: sync-state identity mismatch".into());
    }
    if st.server_cursor != verified.through_server_seq {
        return Err("REFUSE RELEASE: confirmed cursor has not caught the accepted tail".into());
    }
    let last_frozen = intent.ops.last().map(|o| o.client_seq).unwrap();
    if st.acked_client_seq < last_frozen {
        return Err("REFUSE RELEASE: acknowledged client sequence has not caught recovery".into());
    }
    if st.next_client_seq != intent.next_client_seq_after {
        return Err(
            "REFUSE RELEASE: next_client_seq does not match frozen sequence authority".into(),
        );
    }
    if frozen_seqs_still_in_outbox(sync_dir, &intent, st.acked_client_seq)? {
        return Err("REFUSE RELEASE: accepted recovery records remain in the outbox".into());
    }
    let identity = read_identity(library_root)?
        .ok_or_else(|| "REFUSE RELEASE: missing identity.json".to_string())?;
    let identity_sha = file_sha256(&identity_path(library_root))?
        .ok_or_else(|| "missing identity.json".to_string())?;
    if identity_sha != intent.identity_sha256 {
        return Err("REFUSE RELEASE: identity bindings changed".into());
    }
    let (confirmed, applied_through) = load_confirmed_state(sync_dir, intent.library_id)?;
    if applied_through != st.server_cursor {
        return Err("REFUSE RELEASE: confirmed log does not match server_cursor".into());
    }
    let local = build_snapshot_read_only(library_root, &identity)?;
    if local != confirmed.snapshot {
        return Err("REFUSE RELEASE: local notebook does not match confirmed history".into());
    }
    let local_sha = snapshot_sha256(&local)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())?;
    let proof = RecoveryCompleteV1 {
        v: RECOVERY_COMPLETE_VERSION,
        transaction_id: intent.transaction_id.clone(),
        library_id: intent.library_id,
        replica_id: intent.replica_id,
        accepted_ops: intent.ops.len(),
        server_cursor: st.server_cursor,
        acked_client_seq: st.acked_client_seq,
        local_snapshot_sha256: local_sha,
        identity_sha256: identity_sha,
    };
    write_completion(sync_dir, &proof)?;
    if hooks.fail_after_proof_before_release {
        return Err("injected: after completion proof, before hold removal".into());
    }
    fs::remove_file(intent_path(sync_dir)).map_err(|e| format!("remove intent: {e}"))?;
    clear_recovery_maintenance(library_root)?;
    Ok(ReleaseReport {
        intent_removed: true,
        already_released: false,
        server_cursor: proof.server_cursor,
        acked_client_seq: proof.acked_client_seq,
    })
}

/// Replay frozen envelopes through the production reducer (exact-state check).
pub fn replay_frozen(
    start: &LibraryState,
    intent: &RecoveryIntentV1,
) -> Result<LibraryState, String> {
    let mut state = start.clone();
    for op in &intent.ops {
        let env: SyncEnvelopeV1 = from_json(op.envelope.as_bytes()).map_err(|e| e.to_string())?;
        match state.apply(&env) {
            Ok((next, _)) => state = next,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(state)
}

#[cfg(test)]
pub(crate) fn recovery_http_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
#[path = "sync_recovery_fence_tests.rs"]
mod fence_tests;
#[cfg(test)]
#[path = "sync_recovery_guard_tests.rs"]
mod guard_tests;
#[cfg(test)]
#[path = "sync_recovery_http_tests.rs"]
mod http_tests;
#[cfg(test)]
#[path = "sync_recovery_release_tests.rs"]
mod release_tests;
#[cfg(test)]
#[path = "sync_recovery_stream_exec_tests.rs"]
mod stream_exec_tests;
#[cfg(test)]
#[path = "sync_recovery_exec_tests.rs"]
mod tests;
