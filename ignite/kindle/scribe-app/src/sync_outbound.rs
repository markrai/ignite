//! Sync Phases S2B + S2C: the Kindle's one bidirectional sync engine.
//!
//! ```text
//! local durable mutation ──▶ outbox.jsonl (journal thread) ──▶ coordinator ──push──▶ /v1
//! /v1 tail ──pull──▶ coordinator ──▶ inbox.jsonl ──runtime safe point──▶ LocalMaterializer
//! ```
//!
//! **The Kindle never waits for the server.** Authoring completes locally first; replication is
//! background work. Inbound state model, cursor meaning and crash ordering: see
//! `sync_inbound.rs` (S2C). One coordinator thread owns network push/pull; the runtime thread
//! owns every library-file mutation (local authoring and remote materialization alike).
//!
//! # Durable state (`<sync-dir>`, outside the library, never in notebook files)
//!
//! | file | role |
//! |---|---|
//! | `config.json` | `{v, server_base_url, token}` — developer-provisioned; never logged |
//! | `replica.json` | S1 replica identity |
//! | `sync-state.json` | `{v:2, library_id, replica_id, genesis_sha256, genesis_uploaded, acked_client_seq, next_client_seq, server_base_url, server_cursor, server_id?}` |
//! | `genesis.json` | canonical `SyncSnapshotV1` at the sync epoch (exact bytes the server gets) |
//! | `base-state.json` | **ServerBaseState**: canonical reducer state through `server_cursor` |
//! | `outbox.jsonl` | append-only operation journal (records below), compacted after ACK |
//! | `accepted.jsonl` | S2C bridge: ACKed own ops (exact bytes + `server_seq`) until consumed inbound |
//! | `inbox.jsonl` | S2C: validated server tail items not yet applied |
//! | `apply.json` | S2C: an inbound materialization transaction is in flight |
//! | `blobs/<sha256>` | background bytes referenced by genesis / create ops |
//! | `sync.lock` | exclusive process lock for this sync dir |
//!
//! # Crash safety between the notebook and the outbox
//!
//! * **Write-ahead** (`EraseRegion`, `MoveStrokeLineages`, `AddPageLink`, `DeletePageLink`, all structural ops): a
//!   `prep` record holding the exact canonical envelope is fsynced *before* the local mutation,
//!   then `commit` (or `abort` if the local operation is refused; the `client_seq` is then
//!   reused). A crash before that `commit`/`abort` is settled at startup by which outcome
//!   matches the local snapshot. Link differences are not re-derived.
//! * **Post-commit** (`AddStroke`, `DeleteStrokeLineage`): captured after the local durable
//!   commit, persisted off the pen thread by the journal thread.
//! * **Startup reconciliation** (the authority): `base-state + overlay` (accepted bridge +
//!   committed outbox, client_seq order) is replayed with
//!   the S2A reducer and must equal the Kindle's actual canonical snapshot. A trailing `prep` is
//!   resolved by which outcome (with/without) matches. A stroke/undo lost in the post-commit
//!   window is re-derived exactly from the local journals. Any other divergence **faults upload**
//!   (never fatal-to-notebook; nothing local is ever rewritten). Capture of new local edits
//!   continues **only when the parsed outbox established a trustworthy next `client_seq`**;
//!   a malformed or gapped journal refuses sequenced allocation rather than minting a conflict.
//!   The uploader stays idle until reconcile succeeds. Fault is never auto-cleared.
//!
//! Envelopes are immutable: retries resend the journaled bytes; `OperationId`/`client_seq` are
//! never re-minted for an envelope that may have reached the server.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use control_protocol::ServerId;
use serde::{Deserialize, Serialize};
use sync_engine::{HistoryV1, LibraryState};
use sync_protocol::{
    from_json, sha256_hex, to_canonical_json, BackgroundRef, FolderId, IdentityFileV1, LibraryId,
    LinkId, NotebookId, OperationId, PageId, PageSpec, ReplicaId, Sha256Hex, SnapshotNotebook,
    SnapshotPage, StrokeId, StrokeStyle, SyncEnvelopeV1, SyncOperationV1, SyncPageLink, SyncPoint,
    SyncSnapshotV1, SyncStroke, ENVELOPE_VERSION,
};
use uuid::Uuid;

use notebook_core::{PagePoint, Stroke, ERASER_HIT_RADIUS_CANONICAL};

use crate::pressure_curve::{STYLE_LEGACY_FIXED_V1, STYLE_PRESSURE_WIDTH_V1};
use crate::sync_local::{
    bind_identities, build_snapshot, ensure_replica_state, ensure_sync_identity,
    kindle_global_stroke_id, read_identity,
};

pub const CONFIG_FILE: &str = "config.json";
pub const STATE_FILE: &str = "sync-state.json";
pub const GENESIS_FILE: &str = "genesis.json";
pub const BASE_FILE: &str = "base-state.json";
pub const OUTBOX_FILE: &str = "outbox.jsonl";
pub const BLOB_DIR: &str = "blobs";
pub const LOCK_FILE: &str = "sync.lock";
/// `sync-state.json` version written by S2C (S2B wrote 1).
pub const SYNC_STATE_VERSION: u32 = 2;

#[path = "sync_confirmed.rs"]
pub(crate) mod confirmed;
#[path = "sync_inbound.rs"]
mod inbound;
pub use crate::sync_materialize::DeviceGeometry;
use confirmed::{CheckpointCapture, CheckpointPolicy, ConfirmedLog, CONFIRMED_LOG_FILE};
pub(crate) use inbound::parse_tail;
use inbound::{load_accepted, load_inbox, AcceptedOp, InboxItem};
pub use inbound::{InboundApplied, APPLY_FILE, HISTORY_FILE, INBOX_FILE, TAIL_PAGE};

/// Most operations per upload batch.
pub const MAX_BATCH_OPS: usize = 50;
/// Upper bound on a batch's JSON body.
pub const MAX_BATCH_BYTES: usize = 1024 * 1024;
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(120);
/// Idle re-check interval without a server config (or while faulted).
const IDLE_POLL: Duration = Duration::from_secs(30);
/// Push/pull poll interval while online and caught up (S2C).
const PULL_POLL: Duration = Duration::from_millis(2500);
/// Longest the runtime waits for a write-ahead record to become durable.
const PREP_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_IO_TIMEOUT: Duration = Duration::from_secs(20);
/// Hard bound on one whole HTTP exchange (connect, send, status line, headers, body). `ureq`
/// clears a pooled connection's socket timeouts when it returns to the pool, so without an
/// overall deadline a reused connection whose peer vanished (Kindle suspended mid-request, Wi-Fi
/// down) blocks reading the reply forever — observed on the device 2026-09-27.
#[cfg(not(test))]
const HTTP_REQUEST_DEADLINE: Duration = Duration::from_secs(60);
#[cfg(test)]
const HTTP_REQUEST_DEADLINE: Duration = Duration::from_secs(3);

fn log(msg: impl AsRef<str>) {
    println!("sync: {}", msg.as_ref());
}

// ------------------------------------------------------------------------------------------
// DTOs

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncConfigV1 {
    pub v: u32,
    pub server_base_url: String,
    /// Bearer token (secret). Never logged, never stored in the library or protocol content.
    pub token: String,
    /// Developer hook for the lost-ACK acceptance test: drop the first successful ACK.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub debug_drop_first_ack: bool,
    /// Developer hook for the S2C restart acceptance test: halt inbound at this crash point
    /// (e.g. `after_stroke_journal`), leaving the inbox + marker for recovery on restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_crash_point: Option<String>,
    /// D1 product mode: the paired server's identity (from the server profile). An epoch bound
    /// to a server (`SyncStateV1::server_id`) only uses links that name that server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<ServerId>,
}

impl std::fmt::Debug for SyncConfigV1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncConfigV1")
            .field("server_base_url", &self.server_base_url)
            .field("token", &"<redacted>")
            .field("server_id", &self.server_id)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncStateV1 {
    pub v: u32,
    pub library_id: LibraryId,
    pub replica_id: ReplicaId,
    pub genesis_sha256: Sha256Hex,
    pub genesis_uploaded: bool,
    /// Highest `client_seq` the server has provably accepted (0 = none).
    pub acked_client_seq: u64,
    /// Next `client_seq` to assign (informational; recomputed from the journal on open).
    pub next_client_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_base_url: Option<String>,
    /// S2C: highest contiguous `server_seq` whose canonical effect is durably in the
    /// ServerBaseState **and** reconciled into the Kindle files (see `sync_inbound`).
    pub server_cursor: u64,
    /// The server this epoch belongs to (set by library adoption, `sync_adopt`). Absent for
    /// epochs established by uploading a genesis, which stay usable with any link as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<ServerId>,
}

/// Whether `link` may carry this epoch's traffic: an epoch bound to a server refuses a link to
/// any other (or an unidentified) server.
fn link_permitted(state: &SyncStateV1, link: &SyncConfigV1) -> bool {
    state.server_id.is_none_or(|id| link.server_id == Some(id))
}

/// S2C ServerBaseState: canonical reducer state through `server_cursor`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaseStateV2 {
    v: u32,
    through_server_seq: u64,
    snapshot: SyncSnapshotV1,
    history: HistoryV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordKind {
    Op,
    Prep,
    Commit,
    Abort,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutboxRecordV1 {
    v: u32,
    kind: RecordKind,
    seq: u64,
    /// Exact canonical envelope bytes (UTF-8 JSON text) for `op` / `prep`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    envelope: Option<String>,
}

/// A committed, not yet acknowledged operation.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingOp {
    pub seq: u64,
    pub envelope: SyncEnvelopeV1,
    /// Exact bytes sent on every attempt.
    pub bytes: Vec<u8>,
}

impl PendingOp {
    fn from_record(rec: &OutboxRecordV1) -> Result<Self, String> {
        let text = rec
            .envelope
            .as_ref()
            .ok_or_else(|| format!("outbox seq {} has no envelope", rec.seq))?;
        let bytes = text.as_bytes().to_vec();
        let envelope: SyncEnvelopeV1 = from_json(&bytes).map_err(|e| e.to_string())?;
        if to_canonical_json(&envelope).map_err(|e| e.to_string())? != bytes {
            return Err(format!(
                "outbox seq {}: envelope bytes are not canonical",
                rec.seq
            ));
        }
        if envelope.client_seq != rec.seq {
            return Err(format!(
                "outbox seq {} holds client_seq {}",
                rec.seq, envelope.client_seq
            ));
        }
        Ok(Self {
            seq: rec.seq,
            envelope,
            bytes,
        })
    }

    fn record(&self, kind: RecordKind) -> OutboxRecordV1 {
        OutboxRecordV1 {
            v: 1,
            kind,
            seq: self.seq,
            envelope: Some(String::from_utf8(self.bytes.clone()).expect("canonical JSON is UTF-8")),
        }
    }
}

fn record_line(rec: &OutboxRecordV1) -> Vec<u8> {
    let mut v = serde_json::to_vec(rec).expect("outbox record serializes");
    v.push(b'\n');
    v
}

/// Parsed journal: committed ops in order, plus an unresolved trailing `prep`, if any.
#[derive(Debug, Default)]
pub(crate) struct ParsedJournal {
    pub committed: Vec<PendingOp>,
    pub trailing_prep: Option<PendingOp>,
    pub torn_tail: bool,
}

pub(crate) fn parse_journal(raw: &[u8]) -> Result<ParsedJournal, String> {
    parse_journal_from(raw)
}

/// Streamed [`parse_journal`]: one record in memory at a time (a large backlog is never held
/// twice, as raw bytes and parsed).
fn parse_journal_from(mut r: impl std::io::BufRead) -> Result<ParsedJournal, String> {
    let mut out = ParsedJournal::default();
    let mut open_prep: Option<PendingOp> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = r
            .read_until(b'\n', &mut line)
            .map_err(|e| format!("read outbox: {e}"))?;
        if n == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            // Last line without its newline: never acknowledged as durable.
            out.torn_tail = true;
            break;
        }
        line.pop();
        if line.is_empty() {
            continue;
        }
        let rec: OutboxRecordV1 =
            serde_json::from_slice(&line).map_err(|e| format!("corrupt outbox record: {e}"))?;
        if rec.v != 1 {
            return Err(format!("unsupported outbox record version {}", rec.v));
        }
        match rec.kind {
            RecordKind::Op | RecordKind::Prep => {
                if let Some(p) = &open_prep {
                    return Err(format!(
                        "outbox: seq {} follows unresolved prep {}",
                        rec.seq, p.seq
                    ));
                }
                let op = PendingOp::from_record(&rec)?;
                if let Some(last) = out.committed.last() {
                    if op.seq != last.seq + 1 {
                        return Err(format!("outbox: seq {} after {}", op.seq, last.seq));
                    }
                }
                if rec.kind == RecordKind::Op {
                    out.committed.push(op);
                } else {
                    open_prep = Some(op);
                }
            }
            RecordKind::Commit | RecordKind::Abort => {
                let Some(p) = open_prep.take() else {
                    return Err(format!(
                        "outbox: {:?} without prep (seq {})",
                        rec.kind, rec.seq
                    ));
                };
                if p.seq != rec.seq {
                    return Err(format!(
                        "outbox: {:?} {} closes prep {}",
                        rec.kind, rec.seq, p.seq
                    ));
                }
                if rec.kind == RecordKind::Commit {
                    out.committed.push(p);
                }
            }
        }
    }
    out.trailing_prep = open_prep;
    Ok(out)
}

// ------------------------------------------------------------------------------------------
// Durable helpers

pub(crate) fn sync_dir_fsync(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

pub(crate) fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let tmp = dir.join(format!("{name}.tmp"));
    {
        let mut f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, dir.join(name)).map_err(|e| format!("rename {}: {e}", tmp.display()))?;
    sync_dir_fsync(dir);
    Ok(())
}

pub(crate) fn write_json<T: Serialize>(dir: &Path, name: &str, v: &T) -> Result<(), String> {
    let mut bytes = to_canonical_json(v).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    write_atomic(dir, name, &bytes)
}

/// Borrowed view of [`BaseStateV2`]: same fields, same order, so the same canonical bytes —
/// without cloning the whole library to serialize it.
#[derive(Serialize)]
struct BaseStateRef<'a> {
    v: u32,
    through_server_seq: u64,
    snapshot: &'a SyncSnapshotV1,
    history: &'a HistoryV1,
}

/// Atomically write `v` as canonical JSON + newline (exactly [`write_json`]'s bytes), streamed
/// to a temp file: memory stays flat however large the value is.
fn write_json_streamed<T: Serialize>(dir: &Path, name: &str, v: &T) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let tmp = dir.join(format!("{name}.tmp"));
    {
        let f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
        serde_json::to_writer(&mut w, v).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        w.write_all(b"\n")
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        let f = w
            .into_inner()
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, dir.join(name)).map_err(|e| format!("rename {}: {e}", tmp.display()))?;
    sync_dir_fsync(dir);
    Ok(())
}

/// Persist the ServerBaseState (streamed; no whole-library clone or JSON buffer).
fn write_base_state(
    dir: &Path,
    through_server_seq: u64,
    base: &LibraryState,
) -> Result<(), String> {
    write_json_streamed(
        dir,
        BASE_FILE,
        &BaseStateRef {
            v: 2,
            through_server_seq,
            snapshot: &base.snapshot,
            history: &base.history,
        },
    )
}

/// Read the ServerBaseState streamed (no whole-file buffer next to the parsed state).
fn read_base_state(path: &Path) -> Result<BaseStateV2, String> {
    let f = File::open(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_reader(std::io::BufReader::with_capacity(1 << 20, f))
        .map_err(|e| format!("{}: parse: {e}", path.display()))
}

pub(crate) fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    from_json(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Atomically replace the outbox with `ops` (as `op` records), streamed: temp file, fsync,
/// rename, directory fsync. Never holds the whole journal in memory.
fn rewrite_journal(dir: &Path, ops: &[PendingOp]) -> Result<(), String> {
    let tmp = dir.join(format!("{OUTBOX_FILE}.tmp"));
    {
        let f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
        for op in ops {
            w.write_all(&record_line(&op.record(RecordKind::Op)))
                .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        }
        let f = w
            .into_inner()
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, dir.join(OUTBOX_FILE))
        .map_err(|e| format!("rename {}: {e}", tmp.display()))?;
    sync_dir_fsync(dir);
    Ok(())
}

/// Durably append `ops` (as `op` records) to an intact outbox (no torn tail, no open prep).
pub(crate) fn append_journal(dir: &Path, ops: &[PendingOp]) -> Result<(), String> {
    let path = dir.join(OUTBOX_FILE);
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
    for op in ops {
        w.write_all(&record_line(&op.record(RecordKind::Op)))
            .map_err(|e| format!("append {}: {e}", path.display()))?;
    }
    let f = w
        .into_inner()
        .map_err(|e| format!("append {}: {e}", path.display()))?;
    f.sync_data()
        .map_err(|e| format!("fsync {}: {e}", path.display()))
}

fn is_within(child: &Path, parent: &Path) -> bool {
    match (fs::canonicalize(child), fs::canonicalize(parent)) {
        (Ok(c), Ok(p)) => c.starts_with(p),
        _ => false,
    }
}

fn store_blob(dir: &Path, bytes: &[u8]) -> Result<Sha256Hex, String> {
    let sha = sha256_hex(bytes);
    let blobs = dir.join(BLOB_DIR);
    if !blobs.join(sha.as_str()).exists() {
        write_atomic(&blobs, sha.as_str(), bytes)?;
    }
    Ok(sha)
}

// ------------------------------------------------------------------------------------------
// Reconciliation (pure over snapshots, testable)

/// Derive the operations that turn `expected` (base + everything journaled) into `actual` (the
/// Kindle files) when the only possible cause is capture that never reached the journal — a
/// post-commit crash window, or history made while sync was faulted:
///
/// * **Notebook unknown to `expected`**: `CreateNotebook` (first page) + `CreatePage` for the
///   rest + `AddStroke` for every stroke, in library/page order. Only if every stroke is an
///   original (`root == stroke`); an erase fragment or other lineage cannot be re-authored.
/// * **Known notebook**: new pages (`CreatePage` + `AddStroke`), appended strokes, undone
///   lineages (`DeleteStrokeLineage`), and removed pages (`DeletePage`) — the latter **only**
///   with durable evidence that the page was deleted locally (`trashed`: the F8 quarantine
///   holds it). Creates precede deletes (a notebook never passes through zero pages).
///
/// Anything else — notebook deletion/reorder/retitle, page reorder, background/profile change,
/// rewritten or erased strokes — is refused with a reason naming the notebook. The caller still
/// proves the result: base + journal + derived ops must reproduce `actual` exactly.
fn derive_lost_ops(
    expected: &SyncSnapshotV1,
    actual: &SyncSnapshotV1,
    trashed: &dyn Fn(NotebookId, PageId) -> bool,
) -> Result<Vec<SyncOperationV1>, String> {
    let known: HashSet<NotebookId> = expected.notebooks.iter().map(|n| n.notebook_id).collect();
    let survivors: Vec<NotebookId> = actual
        .notebooks
        .iter()
        .map(|n| n.notebook_id)
        .filter(|n| known.contains(n))
        .collect();
    if survivors
        != expected
            .notebooks
            .iter()
            .map(|n| n.notebook_id)
            .collect::<Vec<_>>()
    {
        let gone: Vec<String> = expected
            .notebooks
            .iter()
            .filter(|e| !survivors.contains(&e.notebook_id))
            .map(|e| format!("{} {:?}", e.notebook_id, e.title))
            .collect();
        return Err(if gone.is_empty() {
            "notebooks were reordered (not derivable)".into()
        } else {
            format!(
                "notebook(s) deleted locally (not derivable): {}",
                gone.join(", ")
            )
        });
    }
    let original = |nb: &SnapshotNotebook, p: &SnapshotPage| -> Result<(), String> {
        match p.strokes.iter().find(|s| s.root_id != s.stroke_id) {
            Some(s) => Err(format!(
                "notebook {} {:?}: page {} holds stroke {} of lineage {} (erase/lineage history is not derivable)",
                nb.notebook_id, nb.title, p.page_id, s.stroke_id, s.root_id
            )),
            None => Ok(()),
        }
    };
    let add_all = |ops: &mut Vec<SyncOperationV1>, nb: NotebookId, p: &SnapshotPage| {
        for s in &p.strokes {
            ops.push(SyncOperationV1::AddStroke {
                notebook_id: nb,
                page_id: p.page_id,
                stroke: s.clone(),
            });
        }
    };
    let mut ops = Vec::new();
    let mut prev_nb: Option<NotebookId> = None;
    for an in &actual.notebooks {
        let Some(en) = expected
            .notebooks
            .iter()
            .find(|e| e.notebook_id == an.notebook_id)
        else {
            let Some((first, rest)) = an.pages.split_first() else {
                return Err(format!(
                    "notebook {} {:?} has no pages",
                    an.notebook_id, an.title
                ));
            };
            for p in &an.pages {
                original(an, p)?;
            }
            ops.push(SyncOperationV1::CreateNotebook {
                notebook_id: an.notebook_id,
                title: an.title.clone(),
                after_notebook_id: prev_nb,
                first_page: spec_of(first),
            });
            add_all(&mut ops, an.notebook_id, first);
            let mut prev_page = first.page_id;
            for p in rest {
                ops.push(SyncOperationV1::CreatePage {
                    notebook_id: an.notebook_id,
                    page: spec_of(p),
                    after_page_id: Some(prev_page),
                });
                add_all(&mut ops, an.notebook_id, p);
                prev_page = p.page_id;
            }
            prev_nb = Some(an.notebook_id);
            continue;
        };
        prev_nb = Some(an.notebook_id);
        let what = |m: &str| format!("notebook {} {:?}: {m}", an.notebook_id, an.title);
        if en.title != an.title {
            return Err(what("title changed (not derivable)"));
        }
        let live: HashSet<PageId> = an.pages.iter().map(|p| p.page_id).collect();
        let removed: Vec<PageId> = en
            .pages
            .iter()
            .map(|p| p.page_id)
            .filter(|p| !live.contains(p))
            .collect();
        if let Some(p) = removed.iter().find(|p| !trashed(an.notebook_id, **p)) {
            return Err(what(&format!(
                "page {p} is gone without deletion evidence in the page trash"
            )));
        }
        let known_pages: HashSet<PageId> = en.pages.iter().map(|p| p.page_id).collect();
        let kept: Vec<PageId> = an
            .pages
            .iter()
            .map(|p| p.page_id)
            .filter(|p| known_pages.contains(p))
            .collect();
        let expected_kept: Vec<PageId> = en
            .pages
            .iter()
            .map(|p| p.page_id)
            .filter(|p| live.contains(p))
            .collect();
        if kept != expected_kept {
            return Err(what("pages were reordered (not derivable)"));
        }
        let mut prev_page: Option<PageId> = None;
        for ap in &an.pages {
            let Some(ep) = en.pages.iter().find(|p| p.page_id == ap.page_id) else {
                original(an, ap)?;
                ops.push(SyncOperationV1::CreatePage {
                    notebook_id: an.notebook_id,
                    page: spec_of(ap),
                    after_page_id: prev_page,
                });
                add_all(&mut ops, an.notebook_id, ap);
                prev_page = Some(ap.page_id);
                continue;
            };
            prev_page = Some(ap.page_id);
            if ep.background != ap.background || ep.render != ap.render {
                return Err(what(&format!(
                    "page {} background/render profile changed (not derivable)",
                    ap.page_id
                )));
            }
            // Page links are write-ahead, not a post-commit gap. A mismatch rejects this
            // outcome so startup can try the other (prep committed vs aborted).
            if ep.links != ap.links {
                return Err(what(&format!(
                    "page {} links differ (not derivable)",
                    ap.page_id
                )));
            }
            if ep.strokes == ap.strokes {
                continue;
            }
            // Deleted lineages: roots present in expected, entirely absent in actual.
            let actual_roots: HashSet<StrokeId> = ap.strokes.iter().map(|s| s.root_id).collect();
            let gone: BTreeSet<StrokeId> = ep
                .strokes
                .iter()
                .map(|s| s.root_id)
                .filter(|r| !actual_roots.contains(r))
                .collect();
            let kept: Vec<&SyncStroke> = ep
                .strokes
                .iter()
                .filter(|s| !gone.contains(&s.root_id))
                .collect();
            if ap.strokes.len() < kept.len() || kept.iter().zip(&ap.strokes).any(|(k, a)| *k != a) {
                return Err(what(&format!(
                    "page {} strokes were rewritten, not appended/undone (not derivable)",
                    ap.page_id
                )));
            }
            for r in &gone {
                ops.push(SyncOperationV1::DeleteStrokeLineage {
                    notebook_id: an.notebook_id,
                    page_id: ap.page_id,
                    root_id: *r,
                });
            }
            for s in &ap.strokes[kept.len()..] {
                if s.root_id != s.stroke_id {
                    return Err(what(&format!(
                        "page {} new stroke {} belongs to lineage {} (not derivable)",
                        ap.page_id, s.stroke_id, s.root_id
                    )));
                }
                ops.push(SyncOperationV1::AddStroke {
                    notebook_id: an.notebook_id,
                    page_id: ap.page_id,
                    stroke: s.clone(),
                });
            }
        }
        for p in removed {
            ops.push(SyncOperationV1::DeletePage {
                notebook_id: an.notebook_id,
                page_id: p,
            });
        }
    }
    derive_folder_ops(expected, actual, &mut ops)?;
    Ok(ops)
}

/// Folder changes are state, so they are always derivable: deleted, created (appended, as the
/// reducer places them), renamed, and every notebook whose folder differs once those apply.
fn derive_folder_ops(
    expected: &SyncSnapshotV1,
    actual: &SyncSnapshotV1,
    ops: &mut Vec<SyncOperationV1>,
) -> Result<(), String> {
    let exp: HashSet<FolderId> = expected.folders.iter().map(|f| f.folder_id).collect();
    let act: HashSet<FolderId> = actual.folders.iter().map(|f| f.folder_id).collect();
    let kept_in_actual: Vec<FolderId> = actual
        .folders
        .iter()
        .map(|f| f.folder_id)
        .filter(|f| exp.contains(f))
        .collect();
    let kept_in_expected: Vec<FolderId> = expected
        .folders
        .iter()
        .map(|f| f.folder_id)
        .filter(|f| act.contains(f))
        .collect();
    if kept_in_actual != kept_in_expected {
        return Err("folders were reordered (not derivable)".into());
    }
    if actual.folders[kept_in_actual.len()..]
        .iter()
        .any(|f| exp.contains(&f.folder_id))
    {
        return Err("a new folder precedes an existing one (not derivable)".into());
    }
    for e in expected
        .folders
        .iter()
        .filter(|e| !act.contains(&e.folder_id))
    {
        ops.push(SyncOperationV1::DeleteFolder {
            folder_id: e.folder_id,
        });
    }
    for a in &actual.folders {
        match expected.folders.iter().find(|e| e.folder_id == a.folder_id) {
            None => ops.push(SyncOperationV1::CreateFolder {
                folder_id: a.folder_id,
                title: a.title.clone(),
            }),
            Some(e) if e.title != a.title => ops.push(SyncOperationV1::RenameFolder {
                folder_id: a.folder_id,
                title: a.title.clone(),
            }),
            Some(_) => {}
        }
    }
    for an in &actual.notebooks {
        let before = expected
            .notebooks
            .iter()
            .find(|e| e.notebook_id == an.notebook_id)
            .and_then(|e| e.folder_id)
            .filter(|f| act.contains(f));
        if before != an.folder_id {
            ops.push(SyncOperationV1::SetNotebookFolder {
                notebook_id: an.notebook_id,
                folder_id: an.folder_id,
            });
        }
    }
    Ok(())
}

fn spec_of(p: &SnapshotPage) -> PageSpec {
    PageSpec {
        page_id: p.page_id,
        background: p.background.clone(),
        render: p.render.clone(),
    }
}

fn apply_all(state: &LibraryState, ops: &[PendingOp]) -> Result<LibraryState, String> {
    apply_seq(state, ops.iter().map(|o| (o.seq, &o.envelope)))
}

/// `state` + envelopes in order: one copy, then in place (a whole-library clone per operation
/// made large backlogs quadratic), borrowing the envelopes (no overlay copy).
fn apply_seq<'a>(
    state: &LibraryState,
    ops: impl IntoIterator<Item = (u64, &'a SyncEnvelopeV1)>,
) -> Result<LibraryState, String> {
    let mut s = state.clone();
    for (seq, env) in ops {
        s.apply_in_place(env)
            .map_err(|e| format!("reducer refused outbox client_seq {seq}: {e}"))?;
    }
    Ok(s)
}

// ------------------------------------------------------------------------------------------
// Shared runtime state

struct Shared {
    library_root: PathBuf,
    dir: PathBuf,
    config: Option<SyncConfigV1>,
    state: SyncStateV1,
    /// Committed, durable, not yet acknowledged ops (ascending, contiguous from acked+1).
    committed: VecDeque<PendingOp>,
    /// Next `client_seq` to assign. Meaningful only when `seq_ready`.
    next_seq: u64,
    /// Parsed outbox history validated a safe next sequence (including snapshot-divergence
    /// with an intact journal). Independent of upload `fault`.
    seq_ready: bool,
    /// Why sequenced capture is refused (`seq_ready` false).
    seq_block: Option<String>,
    /// Local journal write/fsync failure. Independent of reconcile/upload `fault`.
    journal_error: Option<String>,
    identity: IdentityFileV1,
    /// ServerBaseState (canonical through `state.server_cursor`). `None` only while a deferred
    /// (Slice 19B.2B) load is in progress or after it faulted: nothing may read a placeholder.
    base: Option<LibraryState>,
    /// Own ACKed ops not yet consumed inbound (by client_seq).
    accepted: Vec<AcceptedOp>,
    /// Staged server tail items beyond the cursor.
    inbox: Vec<InboxItem>,
    device: Option<DeviceGeometry>,
    /// Deterministic crash point for tests (`None` in production).
    crash_at: Option<String>,
    /// Serializes sync-state/accepted/inbox file writes between threads. Lock order: `Shared`
    /// before `io`; never hold `io` while waiting for `Shared`.
    io: Arc<Mutex<()>>,
    /// Test-only: stretch ACK persistence to prove the pen path never waits for it.
    #[cfg(test)]
    ack_persist_delay: Duration,
    #[cfg(test)]
    journal_gate: Option<Arc<JournalGate>>,
    /// Test-only: fail the next journal durability barrier with this message.
    #[cfg(test)]
    journal_fail: Option<String>,
    /// Test-only: hold a checkpoint between its capture and its write.
    #[cfg(test)]
    checkpoint_gate: Option<Arc<JournalGate>>,
    /// Why this open chose fast path vs full reconcile (tests).
    #[cfg(test)]
    recon_path: Option<crate::sync_verified::FastPathReason>,
    fault: Option<String>,
    /// D1: link supplied by the paired-server manager (`set_link`), not `config.json`.
    product_mode: bool,
    /// D1 product mode: the server rejected the paired credential (401).
    auth_revoked: bool,
    uploaded_blobs: HashSet<String>,
    dropped_ack_once: bool,
    /// Test/diagnostic counters.
    pub_stats: SyncStats,
    /// ServerBaseState persistence (see `sync_confirmed.rs`): the confirmed log holds every
    /// applied server op after `checkpoint_through`.
    log: Option<ConfirmedLog>,
    checkpoint_through: u64,
    /// Content address of every page in the promoted checkpoint.
    page_hashes: HashMap<PageId, Sha256Hex>,
    /// Pages the base changed since the last checkpoint capture.
    dirty_pages: HashSet<PageId>,
    checkpoint_in_flight: bool,
    last_checkpoint: Instant,
    last_inbound: Instant,
    checkpoint_policy: CheckpointPolicy,
}

/// Counters for logs and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub captured: u64,
    pub uploaded: u64,
    pub duplicate_acks: u64,
    pub batches: u64,
    pub transport_errors: u64,
    /// S2C: server tail items staged durably / applied / own echoes consumed.
    pub staged: u64,
    pub applied: u64,
    pub own_echoes: u64,
    /// Apply transactions that took the own-echo fast path (no materialize/verify).
    pub echo_fast_path: u64,
    /// Journal records made durable, and the durability barriers (fsyncs) that covered them.
    pub journal_records_durable: u64,
    pub journal_syncs: u64,
    /// Outbox rewrites that dropped an acknowledged prefix.
    pub journal_compactions: u64,
    /// Bytes appended to the confirmed log (inbound persistence).
    pub confirmed_log_bytes: u64,
    /// Promoted checkpoints, and the last one's duration / capture lock hold.
    pub checkpoints: u64,
    pub checkpoint_last_ms: u64,
    pub checkpoint_capture_max_us: u64,
    /// Largest captured-but-not-yet-durable journal backlog seen (records).
    pub journal_backlog_max: u64,
    /// Bridge/inbox rewrites after apply transactions (only when few records remain live).
    pub bridge_compactions: u64,
    /// Uploader loop iterations (proves the thread ran after reconcile).
    pub uploader_polls: u64,
}

/// Why a bulk durability wait ([`OutboundSync::wait_journal_durable`]) did not complete.
/// Never success: a stopped sync subsystem is not a durable journal. Reconcile/upload
/// faults are not this error — only local capture-journal problems.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DurableWaitError {
    /// The local capture journal could not prove durability (write/fsync failure, or
    /// sequenced capture is not authorized). Not used for server/reconcile faults.
    Faulted(String),
    /// The caller cancelled (e.g. the app is shutting down).
    Cancelled,
    /// The journal thread is gone (sync shut down).
    Stopped,
}

impl std::fmt::Display for DurableWaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Faulted(e) => write!(f, "capture journal: {e}"),
            Self::Cancelled => f.write_str("durability wait cancelled"),
            Self::Stopped => f.write_str("sync journal stopped"),
        }
    }
}

/// Most records one journal durability barrier covers. Only records already queued are
/// grouped (the writer never waits for more), so a lone interactive stroke is synced at once.
const JOURNAL_GROUP_MAX_RECORDS: usize = 1024;
const JOURNAL_GROUP_MAX_BYTES: usize = 8 * 1024 * 1024;
/// The outbox is compacted (acknowledged prefix dropped) only once at most this many
/// unacknowledged operations remain, so a compaction is always a small rewrite.
const COMPACT_MAX_REMAINING: usize = 500;
/// Poll interval / progress-log interval of a bulk durability wait.
const BULK_WAIT_POLL: Duration = Duration::from_millis(200);
const BULK_WAIT_LOG_EVERY: Duration = Duration::from_secs(5);

/// Test-only: hold the journal writer just before its durability barrier.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct JournalGate {
    /// (closed, permits, waiting)
    state: Mutex<(bool, u64, u64)>,
    cv: Condvar,
}

#[cfg(test)]
impl JournalGate {
    pub(crate) fn close(&self) {
        self.state.lock().unwrap().0 = true;
    }
    pub(crate) fn open(&self) {
        self.state.lock().unwrap().0 = false;
        self.cv.notify_all();
    }
    /// Let exactly one held barrier complete.
    pub(crate) fn release_one(&self) {
        self.state.lock().unwrap().1 += 1;
        self.cv.notify_all();
    }
    /// Block until the writer is held at a barrier.
    pub(crate) fn wait_held(&self) {
        let mut g = self.state.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while g.2 == 0 {
            assert!(
                Instant::now() < deadline,
                "journal never reached its barrier"
            );
            g = self
                .cv
                .wait_timeout(g, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }
    fn pass(&self) {
        let mut g = self.state.lock().unwrap();
        if !g.0 {
            return;
        }
        g.2 += 1;
        self.cv.notify_all();
        while g.0 && g.1 == 0 {
            g = self.cv.wait(g).unwrap();
        }
        if g.1 > 0 {
            g.1 -= 1;
        }
        g.2 -= 1;
    }
}

enum JournalCmd {
    Append(OutboxRecordV1, Option<SyncSender<Result<(), String>>>),
    Compact(u64),
    /// Replies once every earlier command is durable.
    Barrier(SyncSender<()>),
    Stop,
}

struct Wake {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Wake {
    fn notify(&self) {
        *self.flag.lock().unwrap() = true;
        self.cv.notify_all();
    }
    /// Wait up to `d` or until notified/stopped.
    fn wait(&self, d: Duration, stop: &AtomicBool) {
        let mut g = self.flag.lock().unwrap();
        let deadline = Instant::now() + d;
        while !*g && !stop.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            g = self.cv.wait_timeout(g, deadline - now).unwrap().0;
        }
        *g = false;
    }
}

/// Handle for a write-ahead operation between `prepare` and `commit`/`abort`.
#[derive(Debug)]
pub struct PrepTicket {
    op: PendingOp,
}

impl PrepTicket {
    pub fn operation_id(&self) -> OperationId {
        self.op.envelope.operation_id
    }
}

/// Kindle outbound replication for one library. Always safe to use: configuration refusals
/// leave sync off; a reconcile or protocol fault prohibits **upload** but capture of new local
/// edits continues (the notebook never depends on the server).
pub struct OutboundSync {
    shared: Arc<Mutex<Shared>>,
    journal_tx: Sender<JournalCmd>,
    stop: Arc<AtomicBool>,
    wake: Arc<Wake>,
    journal_thread: Option<JoinHandle<()>>,
    uploader_done: Option<Receiver<()>>,
    checkpoint_done: Option<Receiver<()>>,
    /// Slice 19B.2B: the deferred confirmed-base loader (verified fast-path starts only).
    base_loader_done: Option<Receiver<()>>,
    _lock: File,
}

/// Slice 19B.2B rollback switch for the Kindle runtime: `false` makes every start load the
/// confirmed base synchronously (exactly the pre-19B.2B behaviour). No persisted state depends
/// on it, so it can be flipped in either direction between launches.
pub const DEFER_CONFIRMED_BASE: bool = true;

/// How [`OutboundSync::open_with`] initializes sync.
#[derive(Clone, Debug, Default)]
pub struct SyncOpenOptions {
    /// Panel geometry the caller already knows (see [`OutboundSync::open_with_device`]).
    pub device: Option<DeviceGeometry>,
    /// On a proven verified-fast-path start only (see `deferral_preflight`), load the confirmed
    /// base (checkpoint pages + confirmed-log replay) on a background thread once capture is
    /// ready. Upload, pull, inbound apply and checkpointing wait for it. Every other start is
    /// fully synchronous, exactly as without this option.
    pub defer_confirmed_base: bool,
    #[cfg(test)]
    pub(crate) loader_gate: Option<Arc<LoaderGate>>,
}

/// Test-only: pauses the deferred loader mid-load (after the checkpoint pages, before the log
/// replay; no lock held) and reports whether it published.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct LoaderGate {
    state: Mutex<(bool, bool, Option<bool>)>, // (paused, released, finished: published?)
    cv: Condvar,
    /// Panic inside materialization, right after the pause (contained at that boundary).
    pub(crate) panic_mid_load: AtomicBool,
    /// Panic after materialization, before publication (caught by the thread's outer guard).
    pub(crate) panic_before_publish: AtomicBool,
}

#[cfg(test)]
impl LoaderGate {
    fn pause(&self) {
        let mut g = self.state.lock().unwrap();
        g.0 = true;
        self.cv.notify_all();
        while !g.1 {
            g = self.cv.wait(g).unwrap();
        }
    }
    fn finish(&self, published: bool) {
        self.state.lock().unwrap().2 = Some(published);
        self.cv.notify_all();
    }
    /// Block until the loader is paused at the gate.
    pub(crate) fn wait_paused(&self) {
        let mut g = self.state.lock().unwrap();
        while !g.0 {
            g = self.cv.wait(g).unwrap();
        }
    }
    pub(crate) fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.cv.notify_all();
    }
    /// Block until the loader finished; `true` = it published the base.
    pub(crate) fn wait_finished(&self) -> bool {
        let mut g = self.state.lock().unwrap();
        while g.2.is_none() {
            g = self.cv.wait(g).unwrap();
        }
        g.2.unwrap()
    }
}

impl OutboundSync {
    /// Open (or establish the epoch for) outbound sync of `library_root` with state in
    /// `sync_dir`. Runs startup reconciliation synchronously, then starts the journal and
    /// uploader threads. `Err` only for configuration refusals (sync stays off); divergence is
    /// reported as a fault (upload halted, capture continues, notebook unaffected).
    pub fn open(library_root: &Path, sync_dir: &Path) -> Result<Self, String> {
        Self::open_with_device(library_root, sync_dir, None)
    }

    /// [`Self::open`] with the panel geometry the caller already knows (the Kindle runtime's
    /// framebuffer). It is installed before startup reconciliation, so completing an interrupted
    /// inbound apply materializes with it, and the first-page probe is skipped. `None`, or a
    /// geometry that is not plausible (zero size, stride below width), keeps the probe.
    pub fn open_with_device(
        library_root: &Path,
        sync_dir: &Path,
        device: Option<DeviceGeometry>,
    ) -> Result<Self, String> {
        Self::open_with(
            library_root,
            sync_dir,
            SyncOpenOptions {
                device,
                ..Default::default()
            },
        )
    }

    /// [`Self::open_with_device`] with explicit [`SyncOpenOptions`]. With
    /// `defer_confirmed_base`, a start that [`deferral_preflight`] proves to be a verified fast
    /// path returns once local capture is ready (sequence authority published, journal running)
    /// and loads the confirmed base in the background; until it is published, upload, pull,
    /// inbound apply and checkpointing are refused at their execution boundaries. A deferred load
    /// failure faults sync (capture continues). Every other start is unchanged and synchronous.
    pub fn open_with(
        library_root: &Path,
        sync_dir: &Path,
        opts: SyncOpenOptions,
    ) -> Result<Self, String> {
        let device = opts.device;
        // Slice 19A: one `sync_startup_breakdown` line per attempt, failures included.
        let diag = crate::sync_startup_diag::begin();
        crate::sync_startup_diag::enter("pre");
        fs::create_dir_all(sync_dir).map_err(|e| format!("create {}: {e}", sync_dir.display()))?;
        if is_within(sync_dir, library_root) {
            return Err(format!(
                "REFUSE SYNC: sync dir {} is inside the library; sync state must live outside notebook data",
                sync_dir.display()
            ));
        }
        crate::sync_verified::register_paths(library_root, sync_dir);
        let lock_path = sync_dir.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| format!("open {}: {e}", lock_path.display()))?;
        lock.try_lock()
            .map_err(|e| format!("REFUSE SYNC: {} is in use ({e})", sync_dir.display()))?;
        // Checked under the lock: an adoption publishes only after taking it.
        if crate::sync_adopt::adoption_pending(sync_dir) {
            return Err(format!(
                "REFUSE SYNC: a library adoption into {} is pending; recover it first",
                sync_dir.display()
            ));
        }
        let config: Option<SyncConfigV1> =
            match read_json::<SyncConfigV1>(&sync_dir.join(CONFIG_FILE)) {
                Ok(c) if c.v == 1 => Some(c),
                Ok(c) => return Err(format!("REFUSE SYNC: unsupported config version {}", c.v)),
                Err(_) if !sync_dir.join(CONFIG_FILE).exists() => None,
                Err(e) => return Err(format!("REFUSE SYNC: bad config: {e}")),
            };
        crate::sync_startup_diag::enter("replica");
        let replica = ensure_replica_state(sync_dir, Some(library_root))?;
        crate::store::reset_background_io_stats();
        let t0 = Instant::now();
        // Slice 19B.2B: read-only proof that this start may defer the confirmed base. A refusal
        // keeps today's synchronous path (reusing the manifest it already read, if any).
        let mut deferral: Option<Deferral> = None;
        let mut sync_head: Option<confirmed::CheckpointHead> = None;
        if opts.defer_confirmed_base {
            crate::sync_startup_diag::enter("defer_preflight");
            match deferral_preflight(library_root, sync_dir, replica.replica_id) {
                Ok(d) => {
                    crate::sync_startup_diag::defer("selected");
                    deferral = Some(d);
                }
                Err(refusal) => {
                    crate::sync_startup_diag::defer(refusal.reason);
                    log(format!(
                        "confirmed state: loading synchronously (deferral refused: {})",
                        refusal.reason
                    ));
                    sync_head = refusal.head.map(|h| *h);
                }
            }
        } else {
            crate::sync_startup_diag::defer("not_requested");
        }
        crate::launch_clock::mark("sync_epoch_begin");
        let (state, store) = establish_epoch(
            library_root,
            sync_dir,
            replica.replica_id,
            &config,
            deferral.as_ref().map(|d| d.applied_through),
            sync_head,
        )?;
        crate::launch_clock::mark("sync_epoch_end");
        // A base that was loaded after all (it never is when the preflight held) cancels the
        // deferral: there is exactly one source of the ServerBaseState.
        if store.is_some() {
            deferral = None;
        }
        crate::sync_startup_diag::mem("epoch");
        let config = config.filter(|c| {
            let ok = link_permitted(&state, c);
            if !ok {
                log(
                    "REFUSE LINK: this sync epoch belongs to another server (offline capture only)",
                );
            }
            ok
        });
        crate::sync_startup_diag::enter("device_geom");
        let device = match device {
            Some(d) if plausible_geometry(d) => {
                crate::sync_startup_diag::count("device_geom_probe", 0);
                Some(d)
            }
            supplied => {
                if let Some(d) = supplied {
                    log(format!(
                        "ignoring implausible device geometry {d:?}; probing the library"
                    ));
                    crate::sync_startup_diag::count("device_geom_rejected", 1);
                }
                crate::sync_startup_diag::count("device_geom_probe", 1);
                device_geometry(library_root)
            }
        };
        crate::sync_startup_diag::enter("accepted_open");
        let accepted = load_accepted(
            sync_dir,
            state.library_id,
            state.replica_id,
            state.server_cursor,
        )?;
        crate::sync_startup_diag::count_once("accepted_pending", accepted.len() as u64);
        crate::sync_startup_diag::enter("inbox");
        let inbox = load_inbox(sync_dir, state.library_id, state.server_cursor)?;
        crate::sync_startup_diag::count_once("inbox_pending", inbox.len() as u64);
        crate::sync_startup_diag::enter("identity_open");
        let crash_at = config.as_ref().and_then(|c| c.debug_crash_point.clone());
        let (base, confirmed_log, checkpoint_through, page_hashes, dirty_pages) = match store {
            Some(s) => (
                Some(s.state),
                Some(s.log),
                s.checkpoint_through,
                s.page_hashes,
                s.dirty,
            ),
            // Deferred: nothing of the base exists until the loader publishes all of it at once.
            None => (None, None, 0, HashMap::new(), HashSet::new()),
        };
        let mut shared = Shared {
            library_root: library_root.to_path_buf(),
            dir: sync_dir.to_path_buf(),
            config,
            next_seq: state.acked_client_seq + 1,
            seq_ready: false,
            seq_block: None,
            journal_error: None,
            state,
            committed: VecDeque::new(),
            identity: read_identity(library_root)?.ok_or("identity missing after epoch")?,
            base,
            accepted,
            inbox,
            device,
            crash_at,
            io: Arc::new(Mutex::new(())),
            #[cfg(test)]
            ack_persist_delay: Duration::ZERO,
            #[cfg(test)]
            journal_gate: None,
            #[cfg(test)]
            journal_fail: None,
            #[cfg(test)]
            checkpoint_gate: None,
            #[cfg(test)]
            recon_path: None,
            fault: None,
            product_mode: false,
            auth_revoked: false,
            uploaded_blobs: HashSet::new(),
            dropped_ack_once: false,
            pub_stats: SyncStats::default(),
            log: confirmed_log,
            checkpoint_through,
            page_hashes,
            dirty_pages,
            checkpoint_in_flight: false,
            last_checkpoint: Instant::now(),
            last_inbound: Instant::now(),
            checkpoint_policy: CheckpointPolicy::default(),
        };
        let preparsed = deferral.as_mut().and_then(|d| d.outbox.take());
        if let Err(e) = reconcile(&mut shared, preparsed) {
            diag.fault();
            crate::sync_startup_diag::path("fault_divergence_fallback");
            shared.fault = Some(e.clone());
            log(format!("FAULT (sync halted, notebook unaffected): {e}"));
            log("reconcile_path=fault_divergence_fallback");
            crate::sync_verified::note_reconcile_fault();
            #[cfg(test)]
            {
                if shared
                    .recon_path
                    .map(crate::sync_verified::FastPathReason::is_fast)
                    .unwrap_or(true)
                {
                    shared.recon_path = Some(crate::sync_verified::FastPathReason::FaultDivergence);
                }
            }
        }
        crate::sync_startup_diag::enter("post");
        let reconcile_us = t0.elapsed().as_micros() as u64;
        let bg = crate::store::take_background_io_stats();
        log(format!(
            "reconcile_io {} total_ms={}",
            bg.log_suffix(reconcile_us),
            reconcile_us / 1000
        ));
        log(format!(
            "opened library={} replica={} acked={} pending={} accepted={} server_cursor={} inbox={} next_client_seq={} genesis_uploaded={} reconcile_ms={} server={}",
            shared.state.library_id,
            shared.state.replica_id,
            shared.state.acked_client_seq,
            shared.committed.len(),
            shared.accepted.len(),
            shared.state.server_cursor,
            shared.inbox.len(),
            shared.next_seq,
            shared.state.genesis_uploaded,
            reconcile_us / 1000,
            shared.config.as_ref().map_or("<none: offline capture only>", |c| c.server_base_url.as_str()),
        ));
        crate::sync_startup_diag::enter("threads");
        let shared = Arc::new(Mutex::new(shared));
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Wake {
            flag: Mutex::new(false),
            cv: Condvar::new(),
        });
        let (journal_tx, journal_rx) = mpsc::channel();
        let journal_thread = {
            let shared = Arc::clone(&shared);
            let wake = Arc::clone(&wake);
            let dir = sync_dir.to_path_buf();
            std::thread::Builder::new()
                .name("sync-journal".into())
                .spawn(move || journal_loop(&dir, journal_rx, &shared, &wake))
                .map_err(|e| e.to_string())?
        };
        let (done_tx, done_rx) = mpsc::channel();
        {
            let shared = Arc::clone(&shared);
            let wake = Arc::clone(&wake);
            let stop = Arc::clone(&stop);
            let jtx = journal_tx.clone();
            std::thread::Builder::new()
                .name("sync-upload".into())
                .spawn(move || {
                    uploader_loop(&shared, &wake, &stop, &jtx);
                    let _ = done_tx.send(());
                })
                .map_err(|e| e.to_string())?;
        }
        let (ckpt_tx, ckpt_rx) = mpsc::channel();
        {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("sync-checkpoint".into())
                .spawn(move || {
                    checkpoint_loop(&shared, &stop);
                    let _ = ckpt_tx.send(());
                })
                .map_err(|e| e.to_string())?;
        }
        // Slice 19B.2B: capture is ready now (sequence published, journal running). A faulted
        // start never loads the base: everything that needs it is halted by the fault anyway.
        let base_loader_done = match deferral {
            Some(d) if shared.lock().unwrap().fault.is_none() => {
                #[cfg(test)]
                let d = Deferral {
                    gate: opts.loader_gate.clone(),
                    ..d
                };
                spawn_base_loader(&shared, &stop, &wake, sync_dir, d)
            }
            _ => None,
        };
        diag.finish();
        Ok(Self {
            shared,
            journal_tx,
            stop,
            wake,
            journal_thread: Some(journal_thread),
            uploader_done: Some(done_rx),
            checkpoint_done: Some(ckpt_rx),
            base_loader_done,
            _lock: lock,
        })
    }

    /// D1 product mode: the paired-server manager supplies (or withdraws) the endpoint +
    /// per-device credential. Never logs the credential.
    pub fn set_link(&self, link: Option<SyncConfigV1>) {
        {
            let mut g = self.shared.lock().unwrap();
            let link = link.filter(|c| {
                let ok = link_permitted(&g.state, c);
                if !ok {
                    log("REFUSE LINK: this sync epoch belongs to another server");
                }
                ok
            });
            g.product_mode = true;
            if link.is_some() {
                g.auth_revoked = false;
            }
            g.config = link;
        }
        self.kick();
    }

    /// D1 product mode: the server answered 401 to the paired credential.
    pub fn auth_revoked(&self) -> bool {
        self.shared.lock().unwrap().auth_revoked
    }

    /// The endpoint currently in use (never the credential).
    pub fn link_endpoint(&self) -> Option<String> {
        self.shared
            .lock()
            .unwrap()
            .config
            .as_ref()
            .map(|c| c.server_base_url.clone())
    }

    pub fn fault(&self) -> Option<String> {
        self.shared.lock().unwrap().fault.clone()
    }

    /// Slice 19B.2B: the confirmed base (ServerBaseState) is loaded and published. Always true
    /// after a synchronous start; after a deferred one, once the loader has published.
    pub fn base_ready(&self) -> bool {
        self.shared.lock().unwrap().base.is_some()
    }

    /// In-memory sequence authority after reconcile (independent of the recovery-intent hold).
    pub fn seq_ready(&self) -> bool {
        self.shared.lock().unwrap().seq_ready
    }

    pub fn seq_block(&self) -> Option<String> {
        self.shared.lock().unwrap().seq_block.clone()
    }

    pub fn journal_error(&self) -> Option<String> {
        self.shared.lock().unwrap().journal_error.clone()
    }

    #[cfg(test)]
    pub fn reconcile_path(&self) -> Option<crate::sync_verified::FastPathReason> {
        self.shared.lock().unwrap().recon_path
    }

    /// Write a checkpoint now if anything is uncheckpointed (diagnostics/tests; the background
    /// thread normally decides). `Ok(true)` = a checkpoint was promoted.
    pub fn checkpoint_now(&self) -> Result<bool, String> {
        run_checkpoint(&self.shared, true).map(|r| r.is_some())
    }

    /// Replace the checkpoint policy (tests / tuning).
    pub fn set_checkpoint_policy(&self, policy: CheckpointPolicy) {
        self.shared.lock().unwrap().checkpoint_policy = policy;
    }

    /// (checkpoint_through, confirmed-log records after it).
    pub fn confirmed_state(&self) -> (u64, u64) {
        let g = self.shared.lock().unwrap();
        let after = g
            .log
            .as_ref()
            .map_or(0, |l| l.records_after(g.checkpoint_through));
        (g.checkpoint_through, after)
    }

    pub fn stats(&self) -> SyncStats {
        self.shared.lock().unwrap().pub_stats
    }

    pub fn state(&self) -> SyncStateV1 {
        self.shared.lock().unwrap().state.clone()
    }

    pub fn pending(&self) -> usize {
        self.shared.lock().unwrap().committed.len()
    }

    /// Ask the uploader to try now (e.g. after new work).
    pub fn kick(&self) {
        self.wake.notify();
    }

    // ------------------------------------------------------------------ id helpers

    fn page_gid(&self, notebook_local: &str, page_local: &str) -> Option<(NotebookId, PageId)> {
        let g = self.shared.lock().unwrap();
        Some((
            g.identity.notebook_id(notebook_local)?,
            g.identity.page_id(notebook_local, page_local)?,
        ))
    }

    fn notebook_gid(&self, notebook_local: &str) -> Option<NotebookId> {
        self.shared
            .lock()
            .unwrap()
            .identity
            .notebook_id(notebook_local)
    }

    /// Local operation capture is available (OutboundSync is installed). Independent of
    /// whether upload is permitted: a reconcile fault must not disable authoring capture.
    pub fn is_active(&self) -> bool {
        true
    }

    /// Sequenced outbox allocation is safe: the parsed journal established `next_seq` and the
    /// local writer is healthy. Independent of server reachability and of upload permission.
    pub fn capture_ready(&self) -> bool {
        let g = self.shared.lock().unwrap();
        g.seq_ready
            && g.journal_error.is_none()
            && !crate::sync_recovery_exec::recovery_blocks_ordinary_push(&g.dir)
    }

    /// Uploader may send: reconcile is healthy. Missing config still returns true here; the
    /// uploader idles separately until a pairing/developer endpoint exists.
    ///
    /// A durable recovery-intent file blocks ordinary push even after a successful disposable
    /// recovery (upload stays unauthorized until a later, separately approved live step).
    pub fn upload_permitted(&self) -> bool {
        let g = self.shared.lock().unwrap();
        g.fault.is_none() && !crate::sync_recovery_exec::recovery_blocks_ordinary_push(&g.dir)
    }

    /// Halt sync for a reason found by the caller (the notebook is unaffected).
    pub fn fail_external(&self, why: String) {
        self.fail(why)
    }

    fn fail(&self, why: String) {
        let mut g = self.shared.lock().unwrap();
        if g.fault.is_none() {
            log(format!("FAULT (sync halted, notebook unaffected): {why}"));
            g.fault = Some(why);
        }
    }

    fn envelope(
        &self,
        seq: u64,
        op_id: OperationId,
        operation: SyncOperationV1,
    ) -> Result<PendingOp, String> {
        let g = self.shared.lock().unwrap();
        let env = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: op_id,
            library_id: g.state.library_id,
            replica_id: g.state.replica_id,
            client_seq: seq,
            operation,
        };
        let bytes = env.to_canonical_bytes().map_err(|e| e.to_string())?;
        Ok(PendingOp {
            seq,
            envelope: env,
            bytes,
        })
    }

    fn take_seq(&self) -> Option<u64> {
        let mut g = self.shared.lock().unwrap();
        if !g.seq_ready
            || g.journal_error.is_some()
            || crate::sync_recovery_exec::recovery_blocks_ordinary_push(&g.dir)
        {
            return None;
        }
        let s = g.next_seq;
        match s.checked_add(1) {
            Some(n) => g.next_seq = n,
            None => {
                g.fault = Some("client_seq exhausted".into());
                g.seq_ready = false;
                g.seq_block = Some("client_seq exhausted".into());
                return None;
            }
        }
        Some(s)
    }

    fn note_journal_failure(&self, why: String) {
        let mut g = self.shared.lock().unwrap();
        g.seq_ready = false;
        if g.seq_block.is_none() {
            g.seq_block = Some(why.clone());
        }
        if g.journal_error.is_none() {
            g.journal_error = Some(why.clone());
        }
        if g.fault.is_none() {
            log(format!("FAULT (sync halted, notebook unaffected): {why}"));
            g.fault = Some(why);
        }
    }

    fn invalidate_sequence(&self, why: String) {
        let mut g = self.shared.lock().unwrap();
        g.seq_ready = false;
        if g.seq_block.is_none() {
            g.seq_block = Some(why.clone());
        }
        if g.fault.is_none() {
            log(format!("FAULT (sync halted, notebook unaffected): {why}"));
            g.fault = Some(why);
        }
    }

    // ------------------------------------------------------------------ post-commit capture

    fn capture(&self, op_id: OperationId, operation: SyncOperationV1) {
        let Some(seq) = self.take_seq() else { return };
        match self.envelope(seq, op_id, operation) {
            Ok(op) => {
                self.shared.lock().unwrap().pub_stats.captured += 1;
                if self
                    .journal_tx
                    .send(JournalCmd::Append(op.record(RecordKind::Op), None))
                    .is_err()
                {
                    self.note_journal_failure("journal thread gone".into());
                }
            }
            Err(e) => self.fail(format!("capture client_seq {seq}: {e}")),
        }
    }

    /// A stroke was durably committed locally (post-commit capture).
    pub fn stroke_committed(&self, notebook_local: &str, page_local: &str, stroke: &Stroke) {
        let Some((nb, pg)) = self.page_gid(notebook_local, page_local) else {
            return self.fail(format!(
                "no sync identity for {notebook_local}/{page_local}"
            ));
        };
        let result = (|| -> Result<SyncOperationV1, String> {
            let id = kindle_global_stroke_id(pg, stroke.id.as_str())?;
            let style = match stroke.brush.as_deref() {
                None | Some(STYLE_LEGACY_FIXED_V1) => StrokeStyle::LegacyFixedV1,
                Some(STYLE_PRESSURE_WIDTH_V1) => StrokeStyle::PressureWidthV1,
                Some(o) => return Err(format!("unknown style {o}")),
            };
            Ok(SyncOperationV1::AddStroke {
                notebook_id: nb,
                page_id: pg,
                stroke: SyncStroke {
                    stroke_id: id,
                    root_id: id,
                    style,
                    points: stroke
                        .points
                        .iter()
                        .map(|p| SyncPoint::new(p.x, p.y, p.pressure))
                        .collect(),
                },
            })
        })();
        match result {
            Ok(op) => self.capture(OperationId::from_uuid(Uuid::now_v7()).unwrap(), op),
            Err(e) => self.fail(format!("stroke capture: {e}")),
        }
    }

    /// The newest lineage `root_local` was durably removed by undo (post-commit capture).
    pub fn lineage_deleted(&self, notebook_local: &str, page_local: &str, root_local: &str) {
        let Some((nb, pg)) = self.page_gid(notebook_local, page_local) else {
            return self.fail(format!(
                "no sync identity for {notebook_local}/{page_local}"
            ));
        };
        match kindle_global_stroke_id(pg, root_local) {
            Ok(root) => self.capture(
                OperationId::from_uuid(Uuid::now_v7()).unwrap(),
                SyncOperationV1::DeleteStrokeLineage {
                    notebook_id: nb,
                    page_id: pg,
                    root_id: root,
                },
            ),
            Err(e) => self.fail(format!("undo capture: {e}")),
        }
    }

    // ------------------------------------------------------------------ write-ahead capture

    /// Durably journal `operation` *before* the local mutation. `None` = the required capture
    /// intent is not durable. Callers of operations that cannot be reconstructed (erase, move,
    /// links, structural edits) must refuse the local mutation rather than report success.
    /// Capture is attempted even when upload is already faulted, provided sequence authority
    /// is known and the local journal is healthy. A timeout is not proof the prep was unwritten:
    /// the sequence is not reused.
    fn prepare_with(&self, op_id: OperationId, operation: SyncOperationV1) -> Option<PrepTicket> {
        let seq = self.take_seq()?;
        let op = match self.envelope(seq, op_id, operation) {
            Ok(op) => op,
            Err(e) => {
                self.fail(format!("prepare client_seq {seq}: {e}"));
                return None;
            }
        };
        let (tx, rx) = mpsc::sync_channel(1);
        if self
            .journal_tx
            .send(JournalCmd::Append(op.record(RecordKind::Prep), Some(tx)))
            .is_err()
        {
            self.note_journal_failure("journal thread gone".into());
            return None;
        }
        match rx.recv_timeout(PREP_TIMEOUT) {
            Ok(Ok(())) => {
                self.shared.lock().unwrap().pub_stats.captured += 1;
                Some(PrepTicket { op })
            }
            Ok(Err(e)) => {
                self.note_journal_failure(format!("prepare client_seq {seq}: {e}"));
                None
            }
            Err(_) => {
                // The write may still become durable; do not reuse this client_seq.
                self.invalidate_sequence(format!(
                    "prepare client_seq {seq}: journal not durable within {PREP_TIMEOUT:?}"
                ));
                None
            }
        }
    }

    fn journal_barrier_error(&self) -> Option<DurableWaitError> {
        let g = self.shared.lock().unwrap();
        if let Some(e) = &g.journal_error {
            return Some(DurableWaitError::Faulted(e.clone()));
        }
        if !g.seq_ready {
            return Some(DurableWaitError::Faulted(
                g.seq_block
                    .clone()
                    .unwrap_or_else(|| "sequence authority unknown".into()),
            ));
        }
        None
    }

    /// Bulk backpressure (NBK import): block until every capture queued so far is covered by a
    /// completed durability barrier. Not bounded by `PREP_TIMEOUT` — a large backlog is waited
    /// out — but it returns as soon as the **local journal** cannot prove durability, the
    /// journal stops, or `cancel` is set. Never used on the pen path.
    ///
    /// `Ok` means preceding local capture records completed their durable writes. It does **not**
    /// mean the server is reachable or that reconcile succeeded. Upload permission is unchanged.
    pub fn wait_journal_durable(&self, cancel: &AtomicBool) -> Result<(), DurableWaitError> {
        if let Some(e) = self.journal_barrier_error() {
            return Err(e);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        if self.journal_tx.send(JournalCmd::Barrier(tx)).is_err() {
            return Err(DurableWaitError::Stopped);
        }
        let t0 = Instant::now();
        let mut next_log = BULK_WAIT_LOG_EVERY;
        let result = loop {
            match rx.recv_timeout(BULK_WAIT_POLL) {
                Ok(()) => break self.journal_barrier_error().map_or(Ok(()), Err),
                Err(mpsc::RecvTimeoutError::Disconnected) => break Err(DurableWaitError::Stopped),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(e) = self.journal_barrier_error() {
                        break Err(e);
                    }
                    if cancel.load(Ordering::SeqCst) {
                        break Err(DurableWaitError::Cancelled);
                    }
                    if t0.elapsed() >= next_log {
                        next_log += BULK_WAIT_LOG_EVERY;
                        let s = self.stats();
                        log(format!(
                            "journal: waiting for durability waited_ms={} captured={} durable={} syncs={}",
                            t0.elapsed().as_millis(),
                            s.captured,
                            s.journal_records_durable,
                            s.journal_syncs
                        ));
                    }
                }
            }
        };
        match &result {
            Ok(()) if t0.elapsed() >= Duration::from_secs(1) => log(format!(
                "journal: durable after waited_ms={}",
                t0.elapsed().as_millis()
            )),
            Ok(()) => {}
            Err(e) => log(format!(
                "journal: durability wait ended without success after waited_ms={}: {e}",
                t0.elapsed().as_millis()
            )),
        }
        result
    }

    /// Test-only: hold the journal writer at its durability barrier.
    #[cfg(test)]
    pub(crate) fn set_journal_gate(&self, gate: Option<Arc<JournalGate>>) {
        self.shared.lock().unwrap().journal_gate = gate;
    }

    /// Test-only: the next journal write/fsync fails with `why`.
    #[cfg(test)]
    pub(crate) fn fail_next_journal_write(&self, why: &str) {
        self.shared.lock().unwrap().journal_fail = Some(why.to_string());
    }

    /// Test-only: stop the journal worker so later capture sends fail.
    #[cfg(test)]
    pub(crate) fn disconnect_journal(&self) {
        let _ = self.journal_tx.send(JournalCmd::Stop);
    }

    /// Test-only: fault sync from another thread (as a stalled journal would).
    #[cfg(test)]
    pub(crate) fn fault_injector(&self) -> impl Fn(&str) + Send + 'static {
        let shared = Arc::clone(&self.shared);
        move |why: &str| {
            let mut g = shared.lock().unwrap();
            if g.fault.is_none() {
                log(format!("FAULT (sync halted, notebook unaffected): {why}"));
                g.fault = Some(why.to_string());
            }
        }
    }

    /// Test-only: inject a local journal write failure from another thread.
    #[cfg(test)]
    pub(crate) fn journal_fault_injector(&self) -> impl Fn(&str) + Send + 'static {
        let shared = Arc::clone(&self.shared);
        move |why: &str| {
            let mut g = shared.lock().unwrap();
            g.seq_ready = false;
            if g.seq_block.is_none() {
                g.seq_block = Some(why.to_string());
            }
            if g.journal_error.is_none() {
                g.journal_error = Some(why.to_string());
            }
            if g.fault.is_none() {
                log(format!("FAULT (sync halted, notebook unaffected): {why}"));
                g.fault = Some(why.to_string());
            }
        }
    }

    /// The local mutation became durable.
    pub fn commit(&self, ticket: PrepTicket) {
        if self
            .journal_tx
            .send(JournalCmd::Append(
                ticket.op.record(RecordKind::Commit),
                None,
            ))
            .is_err()
        {
            self.note_journal_failure("journal thread gone".into());
        }
    }

    /// The local mutation was refused (nothing durable happened): the seq is reused once the
    /// abort record itself is durable. A timeout is not treated as proof the abort (or the
    /// original prep) was unwritten, so the sequence is not reused in that case.
    pub fn abort(&self, ticket: PrepTicket) {
        let seq = ticket.op.seq;
        let rec = OutboxRecordV1 {
            v: 1,
            kind: RecordKind::Abort,
            seq,
            envelope: None,
        };
        let (tx, rx) = mpsc::sync_channel(1);
        if self
            .journal_tx
            .send(JournalCmd::Append(rec, Some(tx)))
            .is_err()
        {
            self.note_journal_failure("journal thread gone".into());
            return;
        }
        match rx.recv_timeout(PREP_TIMEOUT) {
            Ok(Ok(())) => {
                let mut g = self.shared.lock().unwrap();
                if g.seq_ready && g.next_seq == seq + 1 {
                    g.next_seq = seq;
                }
                g.pub_stats.captured = g.pub_stats.captured.saturating_sub(1);
            }
            Ok(Err(e)) => self.note_journal_failure(format!("abort client_seq {seq}: {e}")),
            Err(_) => self.invalidate_sequence(format!(
                "abort client_seq {seq}: journal not durable within {PREP_TIMEOUT:?}"
            )),
        }
    }

    /// Fresh operation ID for a synced erase (needed before the local commit, because local
    /// fragment IDs derive from it).
    pub fn erase_op_id(&self) -> OperationId {
        OperationId::from_uuid(Uuid::now_v7()).unwrap()
    }

    /// Deterministic local fragment ID for a synced erase (`d-<UUIDv5>`).
    pub fn erase_fragment_local_id(
        &self,
        op_id: OperationId,
        notebook_local: &str,
        page_local: &str,
        source_local: &str,
        index: usize,
    ) -> Result<String, String> {
        let (_, pg) = self
            .page_gid(notebook_local, page_local)
            .ok_or("no sync identity for page")?;
        let src = kindle_global_stroke_id(pg, source_local)?;
        Ok(crate::sync_local::derived_fragment_local_id(
            op_id, src, index,
        ))
    }

    pub fn prepare_erase(
        &self,
        op_id: OperationId,
        notebook_local: &str,
        page_local: &str,
        observed_root_locals: &[String],
        path: &[PagePoint],
    ) -> Option<PrepTicket> {
        let Some((nb, pg)) = self.page_gid(notebook_local, page_local) else {
            self.fail(format!(
                "no sync identity for {notebook_local}/{page_local}"
            ));
            return None;
        };
        let mut roots = Vec::new();
        for r in observed_root_locals {
            match kindle_global_stroke_id(pg, r) {
                Ok(g) => roots.push(g),
                Err(e) => {
                    self.fail(format!("erase capture: {e}"));
                    return None;
                }
            }
        }
        roots.sort();
        roots.dedup();
        let op = SyncOperationV1::EraseRegion {
            notebook_id: nb,
            page_id: pg,
            observed_roots: roots,
            path: path
                .iter()
                .map(|p| SyncPoint::new(p.x, p.y, None))
                .collect(),
            radius: ERASER_HIT_RADIUS_CANONICAL,
        };
        self.prepare_with(op_id, op)
    }

    /// Write-ahead capture for one committed finger-selection translation. Local root IDs map
    /// to the same global lineage roots used by erase and undo; selection state is not serialized.
    pub fn prepare_move_stroke_lineages(
        &self,
        notebook_local: &str,
        page_local: &str,
        root_locals: &[String],
        dx: f32,
        dy: f32,
    ) -> Option<PrepTicket> {
        let Some((nb, pg)) = self.page_gid(notebook_local, page_local) else {
            self.fail(format!(
                "no sync identity for {notebook_local}/{page_local}"
            ));
            return None;
        };
        let mut roots = Vec::with_capacity(root_locals.len());
        for root in root_locals {
            match kindle_global_stroke_id(pg, root) {
                Ok(global) => roots.push(global),
                Err(e) => {
                    self.fail(format!("move capture: {e}"));
                    return None;
                }
            }
        }
        roots.sort();
        roots.dedup();
        self.prepare_with(
            OperationId::from_uuid(Uuid::now_v7()).unwrap(),
            SyncOperationV1::MoveStrokeLineages {
                notebook_id: nb,
                page_id: pg,
                root_ids: roots,
                dx,
                dy,
            },
        )
    }

    /// A page was durably created (post-commit): mint its `PageId`, bind it, capture
    /// `CreatePage` with the page's actual background/render profile.
    pub fn page_created(
        &self,
        notebook_dir: &Path,
        notebook_local: &str,
        page_local: &str,
        after_page_local: Option<&str>,
    ) {
        let Some(nb) = self.notebook_gid(notebook_local) else {
            return self.fail(format!("no sync identity for {notebook_local}"));
        };
        let after = match after_page_local {
            None => None,
            Some(p) => match self.page_gid(notebook_local, p) {
                Some((_, g)) => Some(g),
                None => return self.fail(format!("no sync identity for {notebook_local}/{p}")),
            },
        };
        let result = (|| -> Result<SyncOperationV1, String> {
            let page = self.new_page_spec(notebook_dir, notebook_local, page_local)?;
            Ok(SyncOperationV1::CreatePage {
                notebook_id: nb,
                page,
                after_page_id: after,
            })
        })();
        match result {
            Ok(op) => self.capture(OperationId::from_uuid(Uuid::now_v7()).unwrap(), op),
            Err(e) => self.fail(format!("create page capture: {e}")),
        }
    }

    /// A notebook was durably created (post-commit) with its first page.
    pub fn notebook_created(
        &self,
        notebook_dir: &Path,
        notebook_local: &str,
        title: &str,
        after_notebook_local: Option<&str>,
    ) {
        let after = match after_notebook_local {
            None => None,
            Some(n) => match self.notebook_gid(n) {
                Some(g) => Some(g),
                None => return self.fail(format!("no sync identity for {n}")),
            },
        };
        let result = (|| -> Result<SyncOperationV1, String> {
            let m = crate::library::read_notebook_manifest_read_only(notebook_dir)?;
            let [first] = m.page_ids.as_slice() else {
                return Err(format!(
                    "new notebook has {} pages, expected 1",
                    m.page_ids.len()
                ));
            };
            let nb = NotebookId::from_uuid(Uuid::now_v7()).map_err(|e| e.to_string())?;
            let root = self.shared.lock().unwrap().library_root.clone();
            bind_identities(&root, &[(notebook_local.to_string(), nb)], &[])?;
            self.refresh_identity()?;
            let first_page = self.new_page_spec(notebook_dir, notebook_local, first)?;
            Ok(SyncOperationV1::CreateNotebook {
                notebook_id: nb,
                title: title.to_string(),
                after_notebook_id: after,
                first_page,
            })
        })();
        match result {
            Ok(op) => self.capture(OperationId::from_uuid(Uuid::now_v7()).unwrap(), op),
            Err(e) => self.fail(format!("create notebook capture: {e}")),
        }
    }

    fn refresh_identity(&self) -> Result<(), String> {
        let root = self.shared.lock().unwrap().library_root.clone();
        let fresh = read_identity(&root)?.ok_or("identity missing")?;
        self.shared.lock().unwrap().identity = fresh;
        Ok(())
    }

    /// Spec of a durably created page: mint + bind its ID; blob kept in sync state.
    fn new_page_spec(
        &self,
        notebook_dir: &Path,
        notebook_local: &str,
        page_local: &str,
    ) -> Result<PageSpec, String> {
        let (root, dir) = {
            let g = self.shared.lock().unwrap();
            (g.library_root.clone(), g.dir.clone())
        };
        let page = crate::store::load_page_read_only(notebook_dir, page_local)?;
        let background = crate::sync_local::background_ref_for(
            &page.background,
            page.header.panel_width,
            page.header.panel_height,
            page.header.line_length,
        )?;
        if matches!(background, BackgroundRef::Blob { .. }) {
            store_blob(&dir, &page.background)?;
        }
        let page_id = PageId::from_uuid(Uuid::now_v7()).map_err(|e| e.to_string())?;
        bind_identities(
            &root,
            &[],
            &[(notebook_local.to_string(), page_local.to_string(), page_id)],
        )?;
        self.refresh_identity()?;
        Ok(PageSpec {
            page_id,
            background,
            render: crate::sync_local::render_ref_for(&page.render_profile),
        })
    }

    pub fn prepare_move_page(
        &self,
        notebook_local: &str,
        page_local: &str,
        after_page_local: Option<&str>,
    ) -> Option<PrepTicket> {
        let Some((nb, pg)) = self.page_gid(notebook_local, page_local) else {
            self.fail(format!(
                "no sync identity for {notebook_local}/{page_local}"
            ));
            return None;
        };
        let after = after_page_local.and_then(|p| self.page_gid(notebook_local, p).map(|x| x.1));
        self.prepare_with(
            OperationId::from_uuid(Uuid::now_v7()).unwrap(),
            SyncOperationV1::MovePage {
                notebook_id: nb,
                page_id: pg,
                after_page_id: after,
            },
        )
    }

    pub fn prepare_delete_page(
        &self,
        notebook_local: &str,
        page_local: &str,
    ) -> Option<PrepTicket> {
        let Some((nb, pg)) = self.page_gid(notebook_local, page_local) else {
            self.fail(format!(
                "no sync identity for {notebook_local}/{page_local}"
            ));
            return None;
        };
        self.prepare_with(
            OperationId::from_uuid(Uuid::now_v7()).unwrap(),
            SyncOperationV1::DeletePage {
                notebook_id: nb,
                page_id: pg,
            },
        )
    }

    /// The library this sync state replicates (folder identities derive from it).
    pub fn library_id(&self) -> LibraryId {
        self.shared.lock().unwrap().state.library_id
    }

    /// A folder was created, renamed or deleted locally (post-commit capture; a lost capture
    /// is re-derived at startup, folder state being fully derivable).
    pub fn folder_changed(&self, operation: SyncOperationV1) {
        debug_assert!(matches!(
            operation,
            SyncOperationV1::CreateFolder { .. }
                | SyncOperationV1::RenameFolder { .. }
                | SyncOperationV1::DeleteFolder { .. }
        ));
        self.capture(OperationId::from_uuid(Uuid::now_v7()).unwrap(), operation);
    }

    /// A notebook was filed in `folder` (or moved back to the top level) locally.
    pub fn notebook_folder_set(&self, notebook_local: &str, folder: Option<FolderId>) {
        let Some(nb) = self.notebook_gid(notebook_local) else {
            return self.fail(format!("no sync identity for {notebook_local}"));
        };
        self.capture(
            OperationId::from_uuid(Uuid::now_v7()).unwrap(),
            SyncOperationV1::SetNotebookFolder {
                notebook_id: nb,
                folder_id: folder,
            },
        );
    }

    pub fn prepare_rename_notebook(&self, notebook_local: &str, title: &str) -> Option<PrepTicket> {
        let Some(nb) = self.notebook_gid(notebook_local) else {
            self.fail(format!("no sync identity for {notebook_local}"));
            return None;
        };
        self.prepare_with(
            OperationId::from_uuid(Uuid::now_v7()).unwrap(),
            SyncOperationV1::RenameNotebook {
                notebook_id: nb,
                title: title.to_string(),
            },
        )
    }

    pub fn prepare_add_page_link(
        &self,
        notebook_local: &str,
        page_local: &str,
        link: &SyncPageLink,
    ) -> Option<PrepTicket> {
        let Some((nb, pg)) = self.page_gid(notebook_local, page_local) else {
            self.fail(format!(
                "no sync identity for {notebook_local}/{page_local}"
            ));
            return None;
        };
        self.prepare_with(
            OperationId::from_uuid(Uuid::now_v7()).unwrap(),
            SyncOperationV1::AddPageLink {
                notebook_id: nb,
                page_id: pg,
                link: link.clone(),
            },
        )
    }

    pub fn prepare_delete_page_link(
        &self,
        notebook_local: &str,
        page_local: &str,
        link_id: LinkId,
    ) -> Option<PrepTicket> {
        let Some((nb, pg)) = self.page_gid(notebook_local, page_local) else {
            self.fail(format!(
                "no sync identity for {notebook_local}/{page_local}"
            ));
            return None;
        };
        self.prepare_with(
            OperationId::from_uuid(Uuid::now_v7()).unwrap(),
            SyncOperationV1::DeletePageLink {
                notebook_id: nb,
                page_id: pg,
                link_id,
            },
        )
    }

    pub fn prepare_delete_notebook(&self, notebook_local: &str) -> Option<PrepTicket> {
        let Some(nb) = self.notebook_gid(notebook_local) else {
            self.fail(format!("no sync identity for {notebook_local}"));
            return None;
        };
        self.prepare_with(
            OperationId::from_uuid(Uuid::now_v7()).unwrap(),
            SyncOperationV1::DeleteNotebook { notebook_id: nb },
        )
    }

    // ------------------------------------------------------------------ lifecycle

    /// Stop background work within `budget`; the durable outbox stays intact. Never blocks
    /// the controlled exit beyond `budget` (an in-flight HTTP request is abandoned).
    pub fn shutdown(mut self, budget: Duration) {
        let root = self.shared.lock().unwrap().library_root.clone();
        self.stop_threads(budget);
        crate::sync_verified::clear_registry_for(&root);
    }

    fn stop_threads(&mut self, budget: Duration) {
        let t0 = Instant::now();
        self.stop.store(true, Ordering::SeqCst);
        // Barrier: a deferred base publication in progress (it runs under this lock) completes
        // first; from here on the loader observes `stop` and publishes nothing.
        drop(self.shared.lock().unwrap_or_else(|p| p.into_inner()));
        self.wake.notify();
        let _ = self.journal_tx.send(JournalCmd::Stop);
        if let Some(j) = self.journal_thread.take() {
            // Local fsync only: bounded by the filesystem, not the network.
            let _ = j.join();
        }
        if let Some(rx) = self.uploader_done.take() {
            let left = budget.saturating_sub(t0.elapsed());
            if rx.recv_timeout(left).is_err() {
                log("shutdown: uploader still in a network call; abandoned (outbox intact)");
            }
        }
        if let Some(rx) = self.checkpoint_done.take() {
            let left = budget.saturating_sub(t0.elapsed());
            if rx.recv_timeout(left).is_err() {
                // Crash-safe by construction: nothing is retired before promotion.
                log("shutdown: checkpoint still writing; abandoned (previous checkpoint + log intact)");
            }
        }
        if let Some(rx) = self.base_loader_done.take() {
            let left = budget.saturating_sub(t0.elapsed());
            if rx.recv_timeout(left).is_err() {
                // Read-only by construction (see `deferral_preflight`): nothing to interrupt.
                log("shutdown: confirmed-state loader still reading; abandoned (read-only, nothing published)");
            }
        }
        log(format!("shutdown in {}ms", t0.elapsed().as_millis()));
    }
}

/// Write-ahead `AddPageLink`, then replace `links.jsonl`, then `commit`.
///
/// `still_live` runs before the prep and again immediately before the local write.
/// `Ok(None)` means the target was not live: any prep is aborted and nothing is written.
/// A failed durable write leaves the prep unresolved so startup recovery can abort it.
pub fn author_add_page_link(
    sync: Option<&OutboundSync>,
    notebook_local: &str,
    page_local: &str,
    dir: &Path,
    link: &SyncPageLink,
    mut still_live: impl FnMut() -> bool,
) -> Result<Option<Vec<SyncPageLink>>, String> {
    if !still_live() {
        return Ok(None);
    }
    let ticket = if let Some(s) = sync {
        Some(
            s.prepare_add_page_link(notebook_local, page_local, link)
                .ok_or_else(|| "local capture journal could not record this link".to_string())?,
        )
    } else {
        None
    };
    if !still_live() {
        if let (Some(s), Some(t)) = (sync, ticket) {
            s.abort(t);
        }
        return Ok(None);
    }
    let mut links = crate::store::load_page_links(dir)?;
    links.push(link.clone());
    match crate::store::write_page_links(dir, &links) {
        Ok(()) => {
            if let (Some(s), Some(t)) = (sync, ticket) {
                s.commit(t);
            }
            Ok(Some(links))
        }
        Err(e) => Err(e),
    }
}

/// Write-ahead `DeletePageLink` for a link that is actually in `links.jsonl`.
///
/// Missing links prepare nothing. A failed durable write leaves the prep unresolved.
pub fn author_delete_page_link(
    sync: Option<&OutboundSync>,
    notebook_local: &str,
    page_local: &str,
    dir: &Path,
    link_id: LinkId,
) -> Result<Option<Vec<SyncPageLink>>, String> {
    let mut links = crate::store::load_page_links(dir)?;
    let before = links.len();
    links.retain(|l| l.link_id != link_id);
    if links.len() == before {
        return Ok(None);
    }
    let ticket = if let Some(s) = sync {
        Some(
            s.prepare_delete_page_link(notebook_local, page_local, link_id)
                .ok_or_else(|| {
                    "local capture journal could not record this link delete".to_string()
                })?,
        )
    } else {
        None
    };
    match crate::store::write_page_links(dir, &links) {
        Ok(()) => {
            if let (Some(s), Some(t)) = (sync, ticket) {
                s.commit(t);
            }
            Ok(Some(links))
        }
        Err(e) => Err(e),
    }
}

// ------------------------------------------------------------------------------------------
// Process-wide capture point (the runtime's mutation paths call into this; `None` = sync off).

static GLOBAL: Mutex<Option<OutboundSync>> = Mutex::new(None);

fn global() -> std::sync::MutexGuard<'static, Option<OutboundSync>> {
    GLOBAL.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn install_global(sync: OutboundSync) {
    *global() = Some(sync);
}

/// Run `f` against the installed sync, if any.
pub fn with_global<R>(f: impl FnOnce(&OutboundSync) -> R) -> Option<R> {
    global().as_ref().map(f)
}

/// Bounded stop of background sync (no-op when sync is off). The outbox stays durable.
pub fn shutdown_global(budget: Duration) {
    let taken = global().take();
    if let Some(s) = taken {
        s.shutdown(budget);
    }
}

impl Drop for OutboundSync {
    fn drop(&mut self) {
        if self.journal_thread.is_some() {
            self.stop_threads(Duration::from_millis(1500));
        }
    }
}

// ------------------------------------------------------------------------------------------
// Epoch + reconciliation

/// A caller-supplied geometry the materializer can use: non-empty, stride at least the width.
fn plausible_geometry(d: DeviceGeometry) -> bool {
    d.width > 0 && d.height > 0 && d.line_length >= d.width
}

/// This Kindle's panel geometry, from its first live page (used for remotely created pages).
fn device_geometry(library_root: &Path) -> Option<DeviceGeometry> {
    let (repo, layout) = crate::sync_local::live_layout(library_root).ok()?;
    let (nb, pages) = layout.notebooks.first()?;
    let page = crate::store::load_page_read_only(&repo.notebook_dir(nb), pages.first()?).ok()?;
    Some(DeviceGeometry {
        width: page.header.panel_width,
        height: page.header.panel_height,
        line_length: page.header.line_length,
    })
}

/// The ServerBaseState as loaded: checkpoint + replayed confirmed log.
struct BaseStore {
    state: LibraryState,
    checkpoint_through: u64,
    page_hashes: HashMap<PageId, Sha256Hex>,
    log: ConfirmedLog,
    /// Last server_seq applied (checkpoint + log).
    applied_through: u64,
    /// Pages changed by the replayed log records (not yet in any checkpoint file).
    dirty: HashSet<PageId>,
}

fn no_hook(_: &str) -> Result<(), String> {
    Ok(())
}

/// A fresh checkpoint of `state` through `through` and an empty confirmed log (new epoch).
fn initial_base_store(dir: &Path, state: LibraryState, through: u64) -> Result<BaseStore, String> {
    let _ = fs::remove_dir_all(dir.join(confirmed::CHECKPOINT_DIR));
    let w = confirmed::write_checkpoint(
        dir,
        &CheckpointCapture::borrow_all(&state, through),
        &no_hook,
    )?;
    write_atomic(dir, CONFIRMED_LOG_FILE, b"")?;
    let log = ConfirmedLog::open(&dir.join(CONFIRMED_LOG_FILE), &Default::default())?;
    Ok(BaseStore {
        state,
        checkpoint_through: through,
        page_hashes: w.page_hashes,
        log,
        applied_through: through,
        dirty: HashSet::new(),
    })
}

/// Load the ServerBaseState: the promoted checkpoint (or the legacy single-file
/// `base-state.json`) plus a sequential replay of the confirmed log. A legacy file is converted
/// to a checkpoint here, before any sync thread runs, and removed only once that checkpoint is
/// promoted.
fn load_base_store(
    dir: &Path,
    library: LibraryId,
    head: Option<confirmed::CheckpointHead>,
) -> Result<BaseStore, String> {
    let t0 = Instant::now();
    crate::sync_startup_diag::enter("checkpoint");
    let _ = fs::remove_file(dir.join(format!("{CONFIRMED_LOG_FILE}.tmp")));
    // A manifest the 19B.2B preflight already read (it refuses when the `.tmp` above exists,
    // so nothing has changed since) is materialized exactly as `load_checkpoint` would.
    let loaded = match head {
        Some(head) => Some(confirmed::materialize_checkpoint(dir, head)?),
        None => confirmed::load_checkpoint(dir)?,
    };
    let (mut state, through, hashes, legacy) = match loaded {
        Some((s, t, h)) => (s, t, h, false),
        None => {
            crate::sync_startup_diag::count("ckpt_legacy", 1);
            let b = read_base_state(&dir.join(BASE_FILE))?;
            if b.v != 2 {
                return Err(format!("unsupported base-state version {}", b.v));
            }
            let s = LibraryState {
                snapshot: b.snapshot,
                history: b.history,
            };
            s.validate()
                .map_err(|e| format!("base-state invalid: {e}"))?;
            (s, b.through_server_seq, HashMap::new(), true)
        }
    };
    let t_load = t0.elapsed();
    crate::sync_startup_diag::enter("replay");
    let decode = |b: &[u8]| inbound::canonical_envelope(b, library);
    let (scan, last, dirty) = confirmed::replay_log(dir, &mut state, through, &decode)?;
    crate::sync_startup_diag::enter("log_open");
    if scan.torn_tail {
        log(format!(
            "recovery: discarded torn confirmed-log tail after byte {} (never durable)",
            scan.valid_len
        ));
    }
    let replayed = last - through;
    let log_ = ConfirmedLog::open(&dir.join(CONFIRMED_LOG_FILE), &scan)?;
    let mut store = BaseStore {
        state,
        checkpoint_through: through,
        page_hashes: hashes,
        log: log_,
        applied_through: last,
        dirty,
    };
    log(format!(
        "confirmed state: checkpoint through {through} (load_ms={}) + {replayed} log records (replay_ms={}) = server_seq {last}",
        t_load.as_millis(),
        (t0.elapsed() - t_load).as_millis()
    ));
    if legacy {
        crate::sync_startup_diag::enter("legacy_convert");
        let t = Instant::now();
        let w = confirmed::write_checkpoint(
            dir,
            &CheckpointCapture::borrow_all(&store.state, last),
            &no_hook,
        )?;
        store.log.compact_through(last, &no_hook)?;
        store.checkpoint_through = last;
        store.page_hashes = w.page_hashes;
        store.dirty.clear();
        match fs::remove_file(dir.join(BASE_FILE)) {
            Ok(()) => sync_dir_fsync(dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("remove legacy base-state: {e}")),
        }
        log(format!(
            "converted legacy base-state.json to a checkpoint through {last}: pages={} bytes={} ms={}",
            w.pages_written,
            w.bytes_written,
            t.elapsed().as_millis()
        ));
    }
    crate::sync_startup_diag::enter("ckpt_gc");
    let removed = confirmed::collect_unreferenced_pages(dir, &store.page_hashes)?;
    if removed > 0 {
        log(format!(
            "recovery: removed {removed} unreferenced checkpoint page file(s)"
        ));
    }
    Ok(store)
}

/// The ServerBaseState as durably recorded (checkpoint or legacy file + confirmed log),
/// read-only: nothing is truncated, converted or collected. Diagnostics and tests.
pub(crate) fn read_confirmed_state(
    dir: &Path,
    library: LibraryId,
) -> Result<(LibraryState, u64), String> {
    let (mut state, through) = match confirmed::load_checkpoint(dir)? {
        Some((s, t, _)) => (s, t),
        None => {
            let b = read_base_state(&dir.join(BASE_FILE))?;
            (
                LibraryState {
                    snapshot: b.snapshot,
                    history: b.history,
                },
                b.through_server_seq,
            )
        }
    };
    let decode = |b: &[u8]| inbound::canonical_envelope(b, library);
    let (_, last, _) = confirmed::replay_log(dir, &mut state, through, &decode)?;
    Ok((state, last))
}

/// `deferred_through`: the confirmed position [`deferral_preflight`] proved from headers; when it
/// equals the cursor the base is not loaded here (`None` is returned) and no cursor recovery is
/// needed. `head`: a manifest a refused preflight already read.
fn establish_epoch(
    library_root: &Path,
    dir: &Path,
    replica_id: ReplicaId,
    config: &Option<SyncConfigV1>,
    deferred_through: Option<u64>,
    head: Option<confirmed::CheckpointHead>,
) -> Result<(SyncStateV1, Option<BaseStore>), String> {
    let state_path = dir.join(STATE_FILE);
    crate::sync_startup_diag::enter("epoch_state");
    if state_path.exists() {
        let state: SyncStateV1 = read_json(&state_path)?;
        if state.v != 1 && state.v != SYNC_STATE_VERSION {
            return Err(format!(
                "REFUSE SYNC: unsupported sync-state version {}",
                state.v
            ));
        }
        let identity = read_identity(library_root)?.ok_or(
            "REFUSE SYNC: library has no identity.json but sync state exists (different library?)",
        )?;
        if identity.library_id != state.library_id {
            return Err(format!(
                "REFUSE SYNC: sync dir belongs to library {} but this library is {}",
                state.library_id, identity.library_id
            ));
        }
        if state.replica_id != replica_id {
            return Err("REFUSE SYNC: replica identity changed under existing sync state".into());
        }
        let mut state = state;
        if state.v == 1 {
            // S2B epoch. Its base folded ACKed own ops without their server_seq, so it can only
            // be upgraded when nothing was acknowledged yet (base == genesis).
            crate::sync_startup_diag::enter("epoch_upgrade");
            if state.acked_client_seq > 0 || state.server_cursor > 0 {
                return Err(
                    "REFUSE SYNC: S2B (v1) sync state with acknowledged operations cannot become an S2C replica in place; start a new S2C epoch in a new sync dir"
                        .into(),
                );
            }
            let genesis = SyncSnapshotV1::parse(
                &fs::read(dir.join(GENESIS_FILE)).map_err(|e| format!("read genesis: {e}"))?,
            )
            .map_err(|e| e.to_string())?;
            let base = LibraryState::genesis(genesis).map_err(|e| e.to_string())?;
            let store = initial_base_store(dir, base, 0)?;
            state.v = SYNC_STATE_VERSION;
            write_json(dir, STATE_FILE, &state)?;
            log("upgraded S2B sync state (nothing acknowledged) to S2C v2");
            return Ok((state, Some(store)));
        }
        let store = match deferred_through {
            // Proven from the log headers: the confirmed log ends exactly at the cursor, so no
            // cursor recovery is needed and the base can be loaded once capture is ready.
            Some(t) if t == state.server_cursor => None,
            _ => Some(load_base_store(dir, state.library_id, head)?),
        };
        crate::sync_startup_diag::enter("cursor");
        if let Some(store) = &store {
            if store.applied_through > state.server_cursor {
                // Crash between persisting confirmed ops and the cursor: they are only persisted
                // after verified materialization, so their position is the truth.
                log(format!(
                    "recovery: confirmed state through server_seq {} persisted before the cursor; adopting it",
                    store.applied_through
                ));
                state.server_cursor = store.applied_through;
                write_json(dir, STATE_FILE, &state)?;
            } else if store.applied_through < state.server_cursor {
                return Err(format!(
                    "sync state inconsistent: confirmed state through {} but cursor {}",
                    store.applied_through, state.server_cursor
                ));
            }
        }
        // ACKs appended to the bridge before the state write (crash window) are authoritative.
        crate::sync_startup_diag::enter("accepted_epoch");
        let accepted = load_accepted(dir, state.library_id, state.replica_id, state.server_cursor)?;
        if let Some(max) = accepted.iter().map(|a| a.client_seq).max() {
            if max > state.acked_client_seq {
                log(format!(
                    "recovery: ACK through client_seq {max} found in accepted history; adopting it"
                ));
                state.acked_client_seq = max;
                write_json(dir, STATE_FILE, &state)?;
            }
        }
        return Ok((state, store));
    }
    // New epoch: the current canonical snapshot IS the genesis (no historical ops).
    crate::sync_startup_diag::enter("epoch_new");
    let identity = ensure_sync_identity(library_root)?.identity;
    let snap = build_snapshot(library_root, &identity)?;
    let genesis = snap.to_canonical_bytes().map_err(|e| e.to_string())?;
    // Copy referenced background blobs into sync state (content-addressed).
    let (repo, layout) = crate::sync_local::live_layout(library_root)?;
    for (nb_local, pages) in &layout.notebooks {
        for page_local in pages {
            let page = crate::store::load_page_read_only(&repo.notebook_dir(nb_local), page_local)?;
            let bg = crate::sync_local::background_ref_for(
                &page.background,
                page.header.panel_width,
                page.header.panel_height,
                page.header.line_length,
            )?;
            if matches!(bg, BackgroundRef::Blob { .. }) {
                store_blob(dir, &page.background)?;
            }
        }
    }
    let base_state = LibraryState::genesis(snap.clone()).map_err(|e| e.to_string())?;
    write_atomic(dir, GENESIS_FILE, &genesis)?;
    let store = initial_base_store(dir, base_state, 0)?;
    write_atomic(dir, OUTBOX_FILE, b"")?;
    write_atomic(dir, HISTORY_FILE, b"")?;
    write_atomic(dir, INBOX_FILE, b"")?;
    let state = SyncStateV1 {
        v: SYNC_STATE_VERSION,
        library_id: snap.library_id,
        replica_id,
        genesis_sha256: sha256_hex(&genesis),
        genesis_uploaded: false,
        acked_client_seq: 0,
        next_client_seq: 1,
        server_base_url: config.as_ref().map(|c| c.server_base_url.clone()),
        server_cursor: 0,
        server_id: None,
    };
    // The state file is written last: its existence marks the epoch as established.
    write_json(dir, STATE_FILE, &state)?;
    log(format!(
        "epoch established library={} genesis_sha256={} (current snapshot is the genesis)",
        state.library_id, state.genesis_sha256
    ));
    Ok((state, Some(store)))
}

/// Library adoption (`sync_adopt`): establish an epoch in the empty `dir` for a library that
/// already exists on `server_id`, whose genesis is `genesis_bytes` (verified canonical bytes of
/// `genesis`). Unlike a new epoch nothing is uploaded: the genesis is marked uploaded, the
/// ServerBaseState is the genesis at server_seq 0 and ordinary inbound sync pulls the tail.
/// Client sequencing starts at 1 under `replica_id`. The state file is written last.
pub(crate) fn establish_adopted_epoch(
    dir: &Path,
    replica_id: ReplicaId,
    server_id: ServerId,
    genesis_bytes: &[u8],
    genesis: &SyncSnapshotV1,
    blobs: &[&[u8]],
) -> Result<SyncStateV1, String> {
    if dir.join(STATE_FILE).exists() {
        return Err(format!(
            "REFUSE ADOPT: {} already holds a sync epoch",
            dir.display()
        ));
    }
    for b in blobs {
        store_blob(dir, b)?;
    }
    let base = LibraryState::genesis(genesis.clone()).map_err(|e| e.to_string())?;
    write_atomic(dir, GENESIS_FILE, genesis_bytes)?;
    initial_base_store(dir, base, 0)?;
    write_atomic(dir, OUTBOX_FILE, b"")?;
    write_atomic(dir, HISTORY_FILE, b"")?;
    write_atomic(dir, INBOX_FILE, b"")?;
    let state = SyncStateV1 {
        v: SYNC_STATE_VERSION,
        library_id: genesis.library_id,
        replica_id,
        genesis_sha256: sha256_hex(genesis_bytes),
        genesis_uploaded: true,
        acked_client_seq: 0,
        next_client_seq: 1,
        server_base_url: None,
        server_cursor: 0,
        server_id: Some(server_id),
    };
    write_json(dir, STATE_FILE, &state)?;
    Ok(state)
}

/// Install a trustworthy next `client_seq` from a valid parsed outbox (no unresolved prep).
fn publish_sequence(shared: &mut Shared, committed: Vec<PendingOp>, acked: u64) {
    shared.committed = committed.into();
    shared.next_seq = shared.committed.back().map_or(acked + 1, |o| o.seq + 1);
    shared.state.next_client_seq = shared.next_seq;
    shared.seq_ready = true;
    shared.seq_block = None;
}

/// Startup reconciliation (see module docs). Fills `shared.committed` and `next_seq`.
///
/// `preverified`: the outbox [`deferral_preflight`] already parsed; it also proved the start a
/// verified fast path (census included), so the stamp is not evaluated again. The cheap
/// conditions are re-checked and any contradiction faults (fail-safe: never a full path
/// without a base).
fn reconcile(shared: &mut Shared, preverified: Option<ParsedJournal>) -> Result<(), String> {
    use crate::sync_startup_diag as diag;
    diag::enter("outbox");
    let path = shared.dir.join(OUTBOX_FILE);
    let is_preverified = preverified.is_some();
    let parsed = match preverified {
        Some(p) => p,
        None => match File::open(&path) {
            Ok(f) => {
                // Size from the already-open handle (one fstat), not a second read.
                if let Ok(m) = f.metadata() {
                    diag::count_once("outbox_file_bytes", m.len());
                }
                match parse_journal_from(std::io::BufReader::with_capacity(1 << 20, f)) {
                    Ok(p) => p,
                    Err(e) => {
                        shared.seq_block = Some(e.clone());
                        return Err(e);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ParsedJournal::default(),
            Err(e) => {
                let why = format!("read outbox: {e}");
                shared.seq_block = Some(why.clone());
                return Err(why);
            }
        },
    };
    let torn_tail = parsed.torn_tail;
    diag::count("outbox_records", parsed.committed.len() as u64);
    diag::count("outbox_torn_tail", u64::from(torn_tail));
    if parsed.torn_tail {
        log("recovery: discarded torn outbox tail (never acknowledged as durable)");
    }
    // Drop entries already acknowledged (crash after ACK, before compaction).
    let acked = shared.state.acked_client_seq;
    let journaled = parsed.committed.len();
    let mut committed: Vec<PendingOp> = parsed
        .committed
        .into_iter()
        .filter(|o| o.seq > acked)
        .collect();
    let acked_prefix = journaled > committed.len();
    let trailing = parsed.trailing_prep.filter(|p| p.seq > acked);
    if let Some(first) = committed.first().or(trailing.as_ref()) {
        if first.seq != acked + 1 {
            let why = format!(
                "outbox gap: first pending client_seq {} but acked {acked}",
                first.seq
            );
            shared.seq_block = Some(why.clone());
            return Err(why);
        }
    }
    diag::count("outbox_pending", committed.len() as u64);
    diag::count("outbox_trailing_prep", u64::from(trailing.is_some()));
    diag::enter("marker");
    let apply_marker = inbound::read_marker(&shared.dir)?;
    diag::enter("validate");
    crate::launch_clock::mark("sync_validate_begin");
    let (fast_reason, fast_ms) = if is_preverified {
        if apply_marker.is_some() || trailing.is_some() || torn_tail || shared.base.is_some() {
            return Err(
                "startup state changed during initialization (deferred confirmed state refused)"
                    .into(),
            );
        }
        (crate::sync_verified::FastPathReason::Verified, 0)
    } else {
        crate::sync_verified::evaluate_fast_path(
            &shared.library_root,
            &shared.dir,
            shared.state.library_id,
            shared.state.replica_id,
            shared.state.server_cursor,
            shared.state.acked_client_seq,
            apply_marker.is_some(),
            trailing.is_some() || torn_tail,
        )
    };
    crate::launch_clock::mark("sync_validate_end");
    log(format!(
        "reconcile_path={} validate_ms={}",
        fast_reason.as_str(),
        fast_ms
    ));
    diag::path(fast_reason.as_str());
    #[cfg(test)]
    {
        shared.recon_path = Some(fast_reason);
    }
    if fast_reason.is_fast() {
        diag::enter("fast_publish");
        // Trusted: prior full reconcile + unchanged library mutation generation.
        publish_sequence(shared, committed, acked);
        crate::launch_clock::mark("sync_snapshot_skipped");
        return Ok(());
    }
    crate::launch_clock::mark("sync_full_reconcile_begin");
    let t_full = Instant::now();
    // S2C: an interrupted inbound materialization is completed first (idempotent; the marker
    // proves the runtime was at a safe point, so no local capture can be missing).
    if let Some(target) = apply_marker {
        diag::enter("inbound_recovery");
        if trailing.is_some() {
            shared.seq_block =
                Some("apply marker together with an unresolved write-ahead record".into());
            return Err("apply marker together with an unresolved write-ahead record".into());
        }
        publish_sequence(shared, committed.clone(), acked);
        let io = Arc::clone(&shared.io);
        let io_guard = io.lock().unwrap();
        let applied = target > shared.state.server_cursor
            && inbound::apply_staged(shared, usize::MAX, Some(target))?.is_some();
        drop(io_guard);
        if applied {
            log(format!(
                "recovery: completed interrupted inbound apply through server_seq {target}"
            ));
            rewrite_journal(
                &shared.dir,
                &shared.committed.iter().cloned().collect::<Vec<_>>(),
            )?;
            shared.state.next_client_seq = shared.next_seq;
            crate::launch_clock::mark("sync_full_reconcile_end");
            log(format!(
                "full_reconcile_ms={}",
                t_full.elapsed().as_millis()
            ));
            return Ok(());
        }
        let _io = shared.io.lock().unwrap();
        inbound::compact_after(
            &shared.dir,
            shared.state.library_id,
            shared.state.replica_id,
            shared.state.server_cursor,
        )?;
    }
    // ExpectedLocalState = ServerBaseState + LocalOverlay (accepted bridge, then outbox).
    diag::enter("expected");
    let Some(base) = shared.base.as_ref() else {
        return Err("full reconciliation without a confirmed base".into());
    };
    let expected = match apply_seq(
        base,
        shared
            .accepted
            .iter()
            .map(|a| (a.client_seq, &a.envelope))
            .chain(committed.iter().map(|o| (o.seq, &o.envelope))),
    ) {
        Ok(s) => s,
        Err(e) => {
            shared.seq_ready = false;
            shared.seq_block = Some(e.clone());
            return Err(e);
        }
    };
    // Live entities without a binding can only come from a create whose capture was lost:
    // they get fresh IDs (never sent anywhere before); retired entities are dropped. The map
    // is planned in memory and published only after the exact-replay proof below.
    diag::enter("identity_plan");
    crate::launch_clock::mark("sync_identity_begin");
    let (identity_base, identity_plan) =
        crate::sync_local::plan_sync_identity(&shared.library_root)?;
    crate::launch_clock::mark("sync_identity_end");
    let identity = identity_plan.identity.clone();
    diag::enter("snapshot");
    crate::launch_clock::mark("sync_snapshot_begin");
    let actual = build_snapshot(&shared.library_root, &identity)
        .map_err(|e| format!("cannot build local snapshot: {e}"))?;
    crate::launch_clock::mark("sync_snapshot_end");
    diag::mem("snapshot");
    diag::enter("derive");
    crate::launch_clock::mark("sync_compare_begin");
    let trashed = |nb: NotebookId, page: PageId| {
        page_deleted_locally(&shared.library_root, &identity, nb, page)
    };

    let mut outcome: Option<(LibraryState, Vec<SyncOperationV1>)> = None;
    let mut refusal = String::new();
    let had_trailing = trailing.is_some();
    // Held as an Option so it is released as soon as the outcome no longer needs it (one full
    // library state less at the startup memory peak).
    let mut expected = Some(expected);
    if let Some(prep) = trailing {
        // Outcome A: the local mutation became durable.
        let with = expected.as_ref().and_then(|x| x.apply(&prep.envelope).ok());
        if let Some((with, _)) = with {
            match derive_lost_ops(&with.snapshot, &actual, &trashed) {
                Ok(ops) => {
                    log(format!(
                        "recovery: write-ahead client_seq {} ({}) committed locally",
                        prep.seq,
                        op_kind(&prep.envelope.operation)
                    ));
                    committed.push(prep.clone());
                    outcome = Some((with, ops));
                }
                Err(e) => refusal = format!("with client_seq {}: {e}; ", prep.seq),
            }
        }
        // Outcome B: it never became durable locally → aborted (seq reused).
        if outcome.is_none() {
            let expected = expected.take().expect("still held");
            match derive_lost_ops(&expected.snapshot, &actual, &trashed) {
                Ok(ops) => {
                    log(format!(
                        "recovery: write-ahead client_seq {} ({}) never committed locally; aborted",
                        prep.seq,
                        op_kind(&prep.envelope.operation)
                    ));
                    outcome = Some((expected, ops));
                }
                Err(e) => refusal.push_str(&format!("without client_seq {}: {e}", prep.seq)),
            }
        }
    } else {
        let expected = expected.take().expect("still held");
        match derive_lost_ops(&expected.snapshot, &actual, &trashed) {
            Ok(ops) => outcome = Some((expected, ops)),
            Err(e) => refusal = e,
        }
    }
    drop(expected);
    let Some((mut resolved, new_ops)) = outcome else {
        if had_trailing {
            // The file still holds an unresolved prep occupying this sequence. Do not append.
            let prep_seq = committed.last().map_or(acked + 1, |o| o.seq + 1);
            shared.committed = committed.into();
            shared.next_seq = prep_seq.saturating_add(1);
            shared.state.next_client_seq = shared.next_seq;
            shared.seq_ready = false;
            shared.seq_block = Some(format!("unresolved write-ahead client_seq {prep_seq}"));
        } else {
            // Valid parsed overlay: capture continues from the next client_seq even though
            // upload stays blocked. Do not rewrite the outbox.
            publish_sequence(shared, committed, acked);
        }
        return Err(format!(
            "local notebook state diverged from base + outbox (not recoverable; manual recovery needed): {refusal}"
        ));
    };
    // Existing journaled envelopes are kept byte-for-byte; derived ops only fill the gap after
    // them, with fresh OperationIds and the next client_seqs (never sent before).
    diag::enter("envelopes");
    let overlay = committed.clone();
    let mut next = committed.last().map_or(acked + 1, |o| o.seq + 1);
    let first_derived = next;
    let mut kinds: std::collections::BTreeMap<&'static str, u64> = Default::default();
    let mut blobs: Vec<PageSpec> = Vec::new();
    for operation in new_ops {
        if let SyncOperationV1::CreateNotebook {
            first_page: page, ..
        }
        | SyncOperationV1::CreatePage { page, .. } = &operation
        {
            blobs.push(page.clone());
        }
        let env = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: OperationId::from_uuid(Uuid::now_v7()).map_err(|e| e.to_string())?,
            library_id: shared.state.library_id,
            replica_id: shared.state.replica_id,
            client_seq: next,
            operation,
        };
        let bytes = env.to_canonical_bytes().map_err(|e| e.to_string())?;
        resolved
            .apply_in_place(&env)
            .map_err(|e| format!("derived op client_seq {next} refused: {e}"))?;
        *kinds.entry(op_kind(&env.operation)).or_default() += 1;
        committed.push(PendingOp {
            seq: next,
            envelope: env,
            bytes,
        });
        next = next.checked_add(1).ok_or("client_seq exhausted")?;
    }
    diag::count("derived_ops", next - first_derived);
    // Final proof: base + all pending ops == actual local state. Nothing is written before it.
    diag::enter("proof");
    if resolved.snapshot != actual {
        // Derived ops were not proven; keep only the parsed overlay for sequencing.
        publish_sequence(shared, overlay, acked);
        return Err(
            "reconciliation could not reproduce local state exactly (manual recovery needed)"
                .into(),
        );
    }
    // Publish: identities first (a crash before the journal rewrite re-derives with the same
    // IDs), then background blobs, then the journal.
    diag::enter("persist_identity");
    crate::sync_local::persist_planned_identity(
        &shared.library_root,
        identity_base.as_ref(),
        &identity_plan,
    )?;
    diag::enter("blobs");
    for page in &blobs {
        keep_blob_for_page(&shared.library_root, &shared.dir, &identity, page)?;
    }
    diag::enter("journal");
    if next != first_derived {
        log(format!(
            "recovery: re-derived {} lost capture(s) as client_seq {first_derived}..={} {kinds:?}",
            next - first_derived,
            next - 1
        ));
    }
    // The journal changes only as needed: a resolved write-ahead record or a torn tail is
    // normalized by an atomic (streamed) rewrite, as is a small remainder behind an
    // acknowledged prefix (compaction; a large backlog keeps its prefix until it drains — see
    // `COMPACT_MAX_REMAINING`); otherwise derived ops are appended; an intact journal with
    // nothing derived is left untouched.
    if had_trailing || torn_tail || (acked_prefix && committed.len() <= COMPACT_MAX_REMAINING) {
        rewrite_journal(&shared.dir, &committed)?;
    } else if next != first_derived {
        let from =
            committed.len() - usize::try_from(next - first_derived).map_err(|e| e.to_string())?;
        append_journal(&shared.dir, &committed[from..])?;
    }
    shared.identity = identity;
    shared.next_seq = next;
    shared.state.next_client_seq = next;
    shared.committed = committed.into();
    shared.seq_ready = true;
    shared.seq_block = None;
    crate::launch_clock::mark("sync_compare_end");
    crate::launch_clock::mark("sync_full_reconcile_end");
    log(format!(
        "full_reconcile_ms={}",
        t_full.elapsed().as_millis()
    ));
    // Only a successful full reconcile may establish a verified stamp.
    diag::enter("stamp");
    let admissions_before = crate::sync_verified::admission_count(&shared.library_root);
    match crate::sync_verified::write_stamp_after_full_reconcile(
        &shared.library_root,
        &shared.dir,
        shared.state.library_id,
        shared.state.replica_id,
        shared.state.server_cursor,
        shared.state.acked_client_seq,
    ) {
        Err(e) => log(format!("verified_stamp_write_failed detail={e}")),
        Ok(written) => {
            log("verified_stamp=written");
            // Slice 19B.3D (diagnostic only): shadow-check the page ledger against the previous
            // baseline, then publish this capture as the new baseline. Never fails the reconcile.
            for line in crate::verified_baseline::after_verified_reconcile(
                &shared.library_root,
                &shared.dir,
                &written,
                admissions_before,
            ) {
                log(line);
            }
        }
    }
    Ok(())
}

/// Slice 19B.2B: what [`deferral_preflight`] proved (read-only) for a verified fast-path start.
struct Deferral {
    /// The promoted manifest (claims only; its pages are verified by the loader).
    head: confirmed::CheckpointHead,
    /// The header-only confirmed-log scan the deferred replay must reproduce exactly.
    scan: confirmed::LogScan,
    /// Last confirmed server_seq from the headers (== the cursor).
    applied_through: u64,
    library_id: LibraryId,
    /// The parsed outbox, handed to `reconcile` so it is parsed once.
    outbox: Option<ParsedJournal>,
    #[cfg(test)]
    gate: Option<Arc<LoaderGate>>,
}

/// Why a start keeps the synchronous base load (a short code for the breakdown and log; no
/// paths or identifiers), plus a manifest already read so it is not read twice.
struct Refusal {
    reason: &'static str,
    head: Option<Box<confirmed::CheckpointHead>>,
}

/// Slice 19B.2B: prove, **read-only**, that this start is a verified fast path whose confirmed
/// base can be loaded after capture is ready. Every condition today's synchronous path would
/// act on must be absent, so the deferred loader never writes:
///
/// * a v2 sync state of this library and replica (no S2B upgrade, no new epoch);
/// * no legacy `base-state.json`, no `confirmed.log.tmp`, a parseable v3 checkpoint manifest;
/// * a confirmed log that exists, has no torn tail (nothing to truncate), continues the
///   checkpoint without a gap, and ends **exactly at the cursor** (no cursor recovery);
/// * no unreferenced checkpoint page files (nothing to collect);
/// * accepted history that is valid and needs no ACK recovery;
/// * a valid outbox with no gap, no unresolved write-ahead record and no torn tail;
/// * no inbound apply marker;
/// * `evaluate_fast_path == Verified` (stamp, generation and census).
///
/// Checkpoint **page** integrity and the log payloads are not inferred from any of this: the
/// loader verifies them, and a failure faults sync.
fn deferral_preflight(
    library_root: &Path,
    dir: &Path,
    replica_id: ReplicaId,
) -> Result<Deferral, Refusal> {
    let refuse = |reason| Refusal { reason, head: None };
    let state: SyncStateV1 = read_json(&dir.join(STATE_FILE)).map_err(|_| refuse("sync_state"))?;
    if state.v != SYNC_STATE_VERSION {
        return Err(refuse("sync_state_version"));
    }
    if state.replica_id != replica_id {
        return Err(refuse("replica"));
    }
    match read_identity(library_root) {
        Ok(Some(i)) if i.library_id == state.library_id => {}
        _ => return Err(refuse("identity")),
    }
    if dir.join(BASE_FILE).exists() {
        return Err(refuse("legacy_base"));
    }
    if dir.join(format!("{CONFIRMED_LOG_FILE}.tmp")).exists() {
        return Err(refuse("confirmed_log_tmp"));
    }
    crate::sync_startup_diag::enter("checkpoint");
    let head = confirmed::read_checkpoint_head(dir);
    crate::sync_startup_diag::enter("defer_preflight");
    let Ok(Some(head)) = head else {
        return Err(refuse("checkpoint_manifest"));
    };
    match preflight_after_head(library_root, dir, &state, &head) {
        Ok((scan, applied_through, outbox)) => Ok(Deferral {
            head,
            scan,
            applied_through,
            library_id: state.library_id,
            outbox: Some(outbox),
            #[cfg(test)]
            gate: None,
        }),
        Err(reason) => Err(Refusal {
            reason,
            head: Some(Box::new(head)),
        }),
    }
}

fn preflight_after_head(
    library_root: &Path,
    dir: &Path,
    state: &SyncStateV1,
    head: &confirmed::CheckpointHead,
) -> Result<(confirmed::LogScan, u64, ParsedJournal), &'static str> {
    let log_path = dir.join(CONFIRMED_LOG_FILE);
    if !log_path.exists() {
        return Err("confirmed_log_missing");
    }
    let scan = confirmed::scan_log(&log_path, |_, _| Ok(())).map_err(|_| "confirmed_log")?;
    if scan.torn_tail {
        return Err("confirmed_log_torn_tail");
    }
    let through = head.through();
    // `replay_log`'s position rule from the headers: records <= through are skipped, the first
    // later one must be through + 1 (scan_log already enforces contiguity after that).
    let applied = match scan.index.iter().find(|(seq, _)| *seq > through) {
        None => through,
        Some(&(first, _)) if first == through + 1 => scan.index.last().map_or(through, |x| x.0),
        Some(_) => return Err("confirmed_log_gap"),
    };
    if applied != state.server_cursor {
        return Err("confirmed_log_cursor");
    }
    if head
        .unreferenced_pages_exist(dir)
        .map_err(|_| "checkpoint_pages_dir")?
    {
        return Err("unreferenced_pages");
    }
    let acked = state.acked_client_seq;
    let accepted = load_accepted(dir, state.library_id, state.replica_id, state.server_cursor)
        .map_err(|_| "accepted")?;
    if accepted.iter().any(|a| a.client_seq > acked) {
        return Err("ack_recovery");
    }
    let parsed = match File::open(dir.join(OUTBOX_FILE)) {
        Ok(f) => {
            if let Ok(m) = f.metadata() {
                crate::sync_startup_diag::count_once("outbox_file_bytes", m.len());
            }
            parse_journal_from(std::io::BufReader::with_capacity(1 << 20, f))
                .map_err(|_| "outbox")?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ParsedJournal::default(),
        Err(_) => return Err("outbox"),
    };
    if parsed.torn_tail {
        return Err("outbox_torn_tail");
    }
    if parsed.trailing_prep.as_ref().is_some_and(|p| p.seq > acked) {
        return Err("pending_prep");
    }
    if let Some(first) = parsed.committed.iter().find(|o| o.seq > acked) {
        if first.seq != acked + 1 {
            return Err("outbox_gap");
        }
    }
    if inbound::read_marker(dir)
        .map_err(|_| "apply_marker")?
        .is_some()
    {
        return Err("apply_marker");
    }
    // Accounted as `validate` (its census is `validate_census_*`), exactly as in `reconcile`.
    crate::sync_startup_diag::enter("validate");
    let (reason, _) = crate::sync_verified::evaluate_fast_path(
        library_root,
        dir,
        state.library_id,
        state.replica_id,
        state.server_cursor,
        acked,
        false,
        false,
    );
    crate::sync_startup_diag::enter("defer_preflight");
    if !reason.is_fast() {
        return Err(reason.as_str());
    }
    Ok((scan, applied, parsed))
}

/// Start the deferred confirmed-base loader. `None` (with a fault) if the thread cannot start.
fn spawn_base_loader(
    shared: &Arc<Mutex<Shared>>,
    stop: &Arc<AtomicBool>,
    wake: &Arc<Wake>,
    dir: &Path,
    d: Deferral,
) -> Option<Receiver<()>> {
    let (tx, rx) = mpsc::channel();
    let (s, st, w, dir2) = (
        Arc::clone(shared),
        Arc::clone(stop),
        Arc::clone(wake),
        dir.to_path_buf(),
    );
    match std::thread::Builder::new()
        .name("sync-base-loader".into())
        .spawn(move || {
            #[cfg(test)]
            let gate = d.gate.clone();
            // Outer fail-closed guard: whatever escapes (materialization panics are already
            // contained inside), the instance never stays silently in BaseLoading.
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                load_deferred_base(&s, &st, &w, &dir2, d)
            }));
            if let Err(p) = r {
                let why = format!(
                    "deferred confirmed state: loader terminated unexpectedly ({})",
                    panic_text(&*p)
                );
                fail_deferred_base(&s, &st, &w, why);
                #[cfg(test)]
                if let Some(g) = gate {
                    g.finish(false);
                }
            }
            let _ = tx.send(());
        }) {
        Ok(_) => Some(rx),
        Err(e) => {
            let why = format!("deferred confirmed state: loader could not start: {e}");
            log(format!("FAULT (sync halted, notebook unaffected): {why}"));
            let mut g = shared.lock().unwrap();
            if g.fault.is_none() {
                g.fault = Some(why);
            }
            None
        }
    }
}

/// A panic payload as text (never panics itself).
fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-text panic payload".to_string()
    }
}

/// Fault sync for a deferred-load failure — unless shutdown began or the base is (somehow)
/// already published — and wake every worker waiting for readiness. Poison-tolerant; the locked
/// section only assigns (no logging, no allocation-heavy work), so it cannot panic itself.
/// Returns whether the fault was recorded.
fn fail_deferred_base(shared: &Mutex<Shared>, stop: &AtomicBool, wake: &Wake, why: String) -> bool {
    let recorded = {
        let mut g = shared.lock().unwrap_or_else(|p| p.into_inner());
        if stop.load(Ordering::SeqCst) || g.base.is_some() {
            false
        } else {
            if g.fault.is_none() {
                g.fault = Some(why.clone());
            }
            true
        }
    };
    if recorded {
        log(format!("FAULT (sync halted, notebook unaffected): {why}"));
        crate::launch_clock::mark("sync_base_fault");
    }
    wake.notify();
    recorded
}

/// The deferred loader thread: materialize and verify the base without holding `shared`, then
/// publish all of it at once — unless shutdown began. A failure (including a panic during
/// materialization) faults sync: capture continues, nothing unverified is published, waiting
/// workers are woken. Never writes a file (see [`materialize_deferred`]).
fn load_deferred_base(
    shared: &Arc<Mutex<Shared>>,
    stop: &AtomicBool,
    wake: &Wake,
    dir: &Path,
    d: Deferral,
) {
    let diag = crate::sync_startup_diag::begin_deferred();
    crate::sync_startup_diag::path("deferred");
    #[cfg(test)]
    let gate = d.gate.clone();
    let t0 = Instant::now();
    // Narrowest boundary: materialization owns all of its data and touches no shared state, so
    // a panic inside it leaves nothing half-updated; it becomes an ordinary load failure.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        materialize_deferred(dir, d, stop)
    }))
    .unwrap_or_else(|p| Err(format!("loader panicked: {}", panic_text(&*p))));
    #[cfg(test)]
    if gate
        .as_ref()
        .is_some_and(|g| g.panic_before_publish.load(Ordering::SeqCst))
    {
        panic!("injected deferred-loader panic before publication");
    }
    // Read only by the test gate.
    #[cfg_attr(not(test), allow(unused_variables))]
    let published = match r {
        Ok(store) => {
            let (through, applied) = (store.checkpoint_through, store.applied_through);
            // The locked section only moves values in (cannot panic, so cannot poison).
            let published = {
                let mut g = shared.lock().unwrap_or_else(|p| p.into_inner());
                if stop.load(Ordering::SeqCst) || g.fault.is_some() {
                    false
                } else {
                    g.base = Some(store.state);
                    g.log = Some(store.log);
                    g.checkpoint_through = store.checkpoint_through;
                    g.page_hashes = store.page_hashes;
                    g.dirty_pages.extend(store.dirty);
                    true
                }
            };
            if published {
                log(format!(
                    "confirmed state (deferred): checkpoint through {through} = server_seq {applied} ready in {}ms",
                    t0.elapsed().as_millis()
                ));
                crate::launch_clock::mark("sync_base_ready");
                wake.notify();
            }
            published
        }
        Err(e) => {
            if fail_deferred_base(shared, stop, wake, format!("deferred confirmed state: {e}")) {
                diag.fault();
            }
            false
        }
    };
    #[cfg(test)]
    if let Some(g) = gate {
        g.finish(published);
    }
    diag.finish();
}

/// Read-only: the checkpoint pages (SHA-256-verified, parsed, validated) plus the confirmed-log
/// replay, which must reproduce the preflight's header scan exactly. Writes nothing, even if it
/// outlives its instance: every file is opened for reading except the confirmed log, which is
/// opened with [`ConfirmedLog::open_existing`] (no create, no truncate) and only if shutdown has
/// not begun.
fn materialize_deferred(dir: &Path, d: Deferral, stop: &AtomicBool) -> Result<BaseStore, String> {
    crate::sync_startup_diag::enter("checkpoint");
    let (mut state, through, hashes) = confirmed::materialize_checkpoint(dir, d.head)?;
    #[cfg(test)]
    if let Some(g) = &d.gate {
        g.pause();
        if g.panic_mid_load.load(Ordering::SeqCst) {
            panic!("injected deferred-loader panic mid-load");
        }
    }
    crate::sync_startup_diag::enter("replay");
    let decode = |b: &[u8]| inbound::canonical_envelope(b, d.library_id);
    let (scan, last, dirty) = confirmed::replay_log(dir, &mut state, through, &decode)?;
    crate::sync_startup_diag::enter("log_open");
    if scan.torn_tail || scan.valid_len != d.scan.valid_len || last != d.applied_through {
        return Err(format!(
            "confirmed log changed during the deferred load (expected server_seq {} in {} bytes, found {last} in {})",
            d.applied_through, d.scan.valid_len, scan.valid_len
        ));
    }
    if stop.load(Ordering::SeqCst) {
        return Err("shutdown began during the deferred load".into());
    }
    let log = ConfirmedLog::open_existing(&dir.join(CONFIRMED_LOG_FILE), &scan)?;
    Ok(BaseStore {
        state,
        checkpoint_through: through,
        page_hashes: hashes,
        log,
        applied_through: last,
        dirty,
    })
}

/// Durable evidence that `page` of live notebook `nb` was deleted on this Kindle: its local
/// directory sits in the notebook's F8 page trash (`.page-trash/<local>__<n>`).
fn page_deleted_locally(
    library_root: &Path,
    identity: &IdentityFileV1,
    nb: NotebookId,
    page: PageId,
) -> bool {
    let Some(nb_local) = identity
        .notebooks
        .iter()
        .find(|(_, g)| **g == nb)
        .map(|(l, _)| l.clone())
    else {
        return false;
    };
    let Some(retired) = identity.retired_pages.get(&page) else {
        return false;
    };
    if retired.notebook_id != nb {
        return false;
    }
    let Ok((repo, _)) = crate::sync_local::live_layout(library_root) else {
        return false;
    };
    let prefix = format!("{}__", retired.local_id);
    fs::read_dir(
        repo.notebook_dir(&nb_local)
            .join(crate::notebook::PAGE_TRASH_DIR),
    )
    .map(|rd| {
        rd.flatten().any(|e| {
            e.file_type().is_ok_and(|t| t.is_dir())
                && e.file_name().to_string_lossy().starts_with(&prefix)
        })
    })
    .unwrap_or(false)
}

/// Copy a live page's background blob into sync state (derived create ops).
fn keep_blob_for_page(
    library_root: &Path,
    dir: &Path,
    identity: &IdentityFileV1,
    page: &PageSpec,
) -> Result<(), String> {
    if !matches!(page.background, BackgroundRef::Blob { .. }) {
        return Ok(());
    }
    for (nb_local, pages) in &identity.pages {
        for (page_local, id) in pages {
            if *id == page.page_id {
                let (repo, _) = crate::sync_local::live_layout(library_root)?;
                let p =
                    crate::store::load_page_read_only(&repo.notebook_dir(nb_local), page_local)?;
                store_blob(dir, &p.background)?;
                return Ok(());
            }
        }
    }
    Err(format!("no local page bound to {}", page.page_id))
}

fn op_kind(op: &SyncOperationV1) -> &'static str {
    match op {
        SyncOperationV1::CreateNotebook { .. } => "create_notebook",
        SyncOperationV1::RenameNotebook { .. } => "rename_notebook",
        SyncOperationV1::DeleteNotebook { .. } => "delete_notebook",
        SyncOperationV1::CreatePage { .. } => "create_page",
        SyncOperationV1::MovePage { .. } => "move_page",
        SyncOperationV1::DeletePage { .. } => "delete_page",
        SyncOperationV1::AddStroke { .. } => "add_stroke",
        SyncOperationV1::EraseRegion { .. } => "erase_region",
        SyncOperationV1::DeleteStrokeLineage { .. } => "delete_stroke_lineage",
        SyncOperationV1::ReplaceStrokeLineages { .. } => "replace_stroke_lineages",
        SyncOperationV1::MoveStrokeLineages { .. } => "move_stroke_lineages",
        SyncOperationV1::AddText { .. } => "add_text",
        SyncOperationV1::UpdateText { .. } => "update_text",
        SyncOperationV1::DeleteText { .. } => "delete_text",
        SyncOperationV1::AddPageLink { .. } => "add_page_link",
        SyncOperationV1::DeletePageLink { .. } => "delete_page_link",
        SyncOperationV1::CreateFolder { .. } => "create_folder",
        SyncOperationV1::RenameFolder { .. } => "rename_folder",
        SyncOperationV1::DeleteFolder { .. } => "delete_folder",
        SyncOperationV1::SetNotebookFolder { .. } => "set_notebook_folder",
    }
}

// ------------------------------------------------------------------------------------------
// Checkpoint thread

const CHECKPOINT_POLL: Duration = Duration::from_secs(5);

fn checkpoint_loop(shared: &Arc<Mutex<Shared>>, stop: &AtomicBool) {
    let mut failures = 0u32;
    let mut last_logged: Option<SyncStats> = None;
    while !stop.load(Ordering::SeqCst) {
        // Observability: one line whenever the counters moved since the last one.
        let (st, cursor, acked, next, ckpt, log_records, log_bytes) = {
            let g = shared.lock().unwrap();
            let (records, bytes) = g.log.as_ref().map_or((0, 0), |l| {
                (
                    l.records_after(g.checkpoint_through),
                    l.bytes_after(g.checkpoint_through),
                )
            });
            (
                g.pub_stats,
                g.state.server_cursor,
                g.state.acked_client_seq,
                g.next_seq,
                g.checkpoint_through,
                records,
                bytes,
            )
        };
        if last_logged != Some(st) {
            log(format!(
                "stats captured={} journal_durable={} journal_syncs={} journal_backlog_max={} uploaded={} batches={} acked={acked} next_client_seq={next} server_cursor={cursor} confirmed_log_records={log_records} confirmed_log_bytes={log_bytes} confirmed_log_appended={} checkpoint_through={ckpt} checkpoints={} checkpoint_last_ms={} checkpoint_capture_max_us={} transport_errors={}",
                st.captured,
                st.journal_records_durable,
                st.journal_syncs,
                st.journal_backlog_max,
                st.uploaded,
                st.batches,
                st.confirmed_log_bytes,
                st.checkpoints,
                st.checkpoint_last_ms,
                st.checkpoint_capture_max_us,
                st.transport_errors
            ));
            last_logged = Some(st);
        }
        // Poll the policy; back off after failures (history stays in the log meanwhile).
        let wait = CHECKPOINT_POLL * (1 << failures.min(6));
        let t = Instant::now();
        while t.elapsed() < wait && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match run_checkpoint(shared, false) {
            Ok(_) => failures = 0,
            Err(e) => {
                failures += 1;
                log(format!(
                    "checkpoint failed (will retry; checkpoint + log intact): {e}"
                ));
            }
        }
    }
}

/// One checkpoint, if due (`force`: whenever anything is uncheckpointed).
///
/// 1. Under the sync lock (brief): pick the boundary N = server_cursor and capture structure,
///    history and **only the pages changed since the last checkpoint** (unchanged pages are
///    referenced by the hash of the file already on disk — nothing is copied for them).
/// 2. Without any lock: write the changed pages, then the manifest, fsync, promote (rename).
///    Inbound/outbound sync, captures and write-ahead preps continue meanwhile; confirmed ops
///    after N keep appending to the log.
/// 3. Under the lock (brief): retire log records ≤ N (only records after N are copied), adopt
///    the new page hashes; then drop page files no longer referenced.
fn run_checkpoint(shared: &Arc<Mutex<Shared>>, force: bool) -> Result<Option<u64>, String> {
    let t0 = Instant::now();
    let (cap, dirty, dir, crash, reason) = {
        let mut g = shared.lock().unwrap();
        // 19B.2B: never before the (deferred) confirmed base is published.
        if g.fault.is_some() || g.checkpoint_in_flight || g.base.is_none() {
            return Ok(None);
        }
        let Some(log) = g.log.as_ref() else {
            return Ok(None);
        };
        let through = g.state.server_cursor;
        let ops = through.saturating_sub(g.checkpoint_through);
        let bytes = log.bytes_after(g.checkpoint_through);
        let reason = if force {
            (ops > 0).then_some("forced")
        } else {
            g.checkpoint_policy.due(
                ops,
                bytes,
                g.last_checkpoint.elapsed(),
                g.last_inbound.elapsed(),
            )
        };
        let Some(reason) = reason else {
            return Ok(None);
        };
        let t = Instant::now();
        let dirty = std::mem::take(&mut g.dirty_pages);
        let base = g.base.as_ref().expect("checked above");
        let cap = CheckpointCapture::capture(base, through, &g.page_hashes, &dirty);
        let hold = t.elapsed().as_micros() as u64;
        g.pub_stats.checkpoint_capture_max_us = g.pub_stats.checkpoint_capture_max_us.max(hold);
        g.checkpoint_in_flight = true;
        (
            cap,
            dirty,
            g.dir.clone(),
            g.crash_at.clone(),
            format!("{reason} ops={ops} log_bytes={bytes} capture_lock_us={hold}"),
        )
    };
    #[cfg(test)]
    {
        let gate = shared.lock().unwrap().checkpoint_gate.clone();
        if let Some(gate) = gate {
            gate.pass();
        }
    }
    let hook = |name: &str| -> Result<(), String> {
        if crash.as_deref() == Some(name) {
            Err(format!("SIMULATED CRASH at {name}"))
        } else {
            Ok(())
        }
    };
    let through = cap.through_server_seq;
    let changed = cap.changed.len();
    let written = confirmed::write_checkpoint(&dir, &cap, &hook);
    drop(cap);
    let written = match written {
        Ok(w) => w,
        Err(e) => {
            let mut g = shared.lock().unwrap();
            g.dirty_pages.extend(dirty);
            g.checkpoint_in_flight = false;
            return Err(e);
        }
    };
    // Promoted. From here a crash only leaves log records ≤ N, which replay skips.
    let finish = (|| -> Result<(u64, HashMap<PageId, Sha256Hex>), String> {
        hook("checkpoint_after_promotion")?;
        let mut g = shared.lock().unwrap();
        let retired = g
            .log
            .as_mut()
            .ok_or("confirmed log not open")?
            .compact_through(through, &hook)?;
        g.checkpoint_through = through;
        g.page_hashes = written.page_hashes.clone();
        Ok((retired, g.page_hashes.clone()))
    })();
    let result = match finish {
        Ok((retired, hashes)) => {
            let removed = confirmed::collect_unreferenced_pages(&dir, &hashes);
            let _ = fs::remove_file(dir.join(BASE_FILE));
            let ms = t0.elapsed().as_millis() as u64;
            let mut g = shared.lock().unwrap();
            g.last_checkpoint = Instant::now();
            g.pub_stats.checkpoints += 1;
            g.pub_stats.checkpoint_last_ms = ms;
            log(format!(
                "checkpoint through server_seq {through} promoted ({reason}): changed_pages={changed} pages_written={} bytes_written={} log_bytes_retired={retired} unreferenced_removed={} ms={ms}",
                written.pages_written,
                written.bytes_written,
                removed.unwrap_or(0)
            ));
            Ok(Some(through))
        }
        Err(e) => Err(e),
    };
    let mut g = shared.lock().unwrap();
    if result.is_err() {
        // Promotion happened; only bookkeeping/compaction failed. Pages written stay valid; the
        // next checkpoint re-captures anything not recorded here.
        g.dirty_pages.extend(dirty);
    }
    g.checkpoint_in_flight = false;
    result
}

// ------------------------------------------------------------------------------------------
// Journal thread

fn journal_loop(dir: &Path, rx: Receiver<JournalCmd>, shared: &Arc<Mutex<Shared>>, wake: &Wake) {
    let path = dir.join(OUTBOX_FILE);
    let open = || OpenOptions::new().create(true).append(true).open(&path);
    let mut file = match open() {
        Ok(f) => f,
        Err(e) => {
            let mut g = shared.lock().unwrap();
            let why = format!("open outbox: {e}");
            g.seq_ready = false;
            g.seq_block = Some(why.clone());
            g.journal_error = Some(why.clone());
            g.fault = Some(why);
            return;
        }
    };
    // Prep records waiting for their commit (only ever one; the runtime is sequential).
    let mut open_prep: Option<PendingOp> = None;
    let mut held: Option<JournalCmd> = None;
    loop {
        let cmd = match held.take() {
            Some(c) => c,
            None => match rx.recv() {
                Ok(c) => c,
                Err(_) => break,
            },
        };
        let (rec, reply) = match cmd {
            JournalCmd::Stop => break,
            JournalCmd::Barrier(reply) => {
                // Every earlier append was synced (and published) before this point.
                let _ = reply.send(());
                continue;
            }
            JournalCmd::Compact(acked) => {
                let keep: Vec<PendingOp> = {
                    let g = shared.lock().unwrap();
                    let remaining = g.committed.iter().filter(|o| o.seq > acked).count();
                    if remaining > COMPACT_MAX_REMAINING {
                        // Deferred: the acknowledged prefix may stay in the file (startup skips
                        // it). Rewriting a large backlog after every batch would stall this
                        // thread — and every write-ahead prep behind it — for seconds.
                        continue;
                    }
                    g.committed
                        .iter()
                        .filter(|o| o.seq > acked)
                        .cloned()
                        .collect()
                };
                let mut bytes = Vec::new();
                for op in &keep {
                    bytes.extend(record_line(&op.record(RecordKind::Op)));
                }
                if let Some(p) = &open_prep {
                    bytes.extend(record_line(&p.record(RecordKind::Prep)));
                }
                let res = write_atomic(dir, OUTBOX_FILE, &bytes)
                    .and_then(|_| open().map_err(|e| e.to_string()));
                match res {
                    Ok(f) => {
                        file = f;
                        shared.lock().unwrap().pub_stats.journal_compactions += 1;
                    }
                    Err(e) => {
                        let mut g = shared.lock().unwrap();
                        let why = format!("compact outbox: {e}");
                        g.seq_ready = false;
                        if g.seq_block.is_none() {
                            g.seq_block = Some(why.clone());
                        }
                        if g.journal_error.is_none() {
                            g.journal_error = Some(why.clone());
                        }
                        g.fault = Some(why);
                    }
                }
                continue;
            }
            JournalCmd::Append(rec, reply) => (rec, reply),
        };
        // Group commit: this append plus every append *already queued* behind it (never wait
        // for more) share one durability barrier. A non-append command ends the group and is
        // handled after it, so barrier/compact order is preserved.
        let mut group = vec![(rec, reply)];
        let mut bytes = record_line(&group[0].0);
        while group.len() < JOURNAL_GROUP_MAX_RECORDS && bytes.len() < JOURNAL_GROUP_MAX_BYTES {
            match rx.try_recv() {
                Ok(JournalCmd::Append(rec, reply)) => {
                    bytes.extend(record_line(&rec));
                    group.push((rec, reply));
                }
                Ok(other) => {
                    held = Some(other);
                    break;
                }
                Err(_) => break,
            }
        }
        {
            let mut g = shared.lock().unwrap();
            let backlog = g
                .pub_stats
                .captured
                .saturating_sub(g.pub_stats.journal_records_durable);
            g.pub_stats.journal_backlog_max = g.pub_stats.journal_backlog_max.max(backlog);
        }
        let res = file.write_all(&bytes).and_then(|_| {
            #[cfg(test)]
            {
                let fail = shared.lock().unwrap().journal_fail.take();
                if let Some(e) = fail {
                    return Err(std::io::Error::new(std::io::ErrorKind::Other, e));
                }
                let gate = shared.lock().unwrap().journal_gate.clone();
                if let Some(g) = gate {
                    g.pass();
                }
            }
            file.sync_data()
        });
        let res = res.map_err(|e| format!("outbox append: {e}"));
        // Only now — after the barrier covering the whole group completed — do its operations
        // become eligible for transmission and its write-ahead callers hear back.
        match &res {
            Ok(()) => {
                let mut published = false;
                {
                    let mut g = shared.lock().unwrap();
                    g.pub_stats.journal_syncs += 1;
                    g.pub_stats.journal_records_durable += group.len() as u64;
                    for (rec, _) in &group {
                        match rec.kind {
                            RecordKind::Op => {
                                if let Ok(op) = PendingOp::from_record(rec) {
                                    g.committed.push_back(op);
                                    published = true;
                                }
                            }
                            RecordKind::Prep => open_prep = PendingOp::from_record(rec).ok(),
                            RecordKind::Commit => {
                                if let Some(op) = open_prep.take() {
                                    g.committed.push_back(op);
                                    published = true;
                                }
                            }
                            RecordKind::Abort => open_prep = None,
                        }
                    }
                }
                if published {
                    wake.notify();
                }
            }
            Err(e) => {
                let mut g = shared.lock().unwrap();
                g.seq_ready = false;
                if g.seq_block.is_none() {
                    g.seq_block = Some(e.clone());
                }
                if g.journal_error.is_none() {
                    g.journal_error = Some(e.clone());
                }
                if g.fault.is_none() {
                    log(format!("FAULT (sync halted, notebook unaffected): {e}"));
                    g.fault = Some(e.clone());
                }
            }
        }
        for (_, reply) in group {
            if let Some(tx) = reply {
                let _ = tx.send(res.clone());
            }
        }
    }
}

// ------------------------------------------------------------------------------------------
// Uploader thread

enum PushError {
    /// Transport / 5xx: retry the same bytes after backoff.
    Retry(String),
    /// Protocol/semantic/auth refusal: fatal-to-sync.
    Fatal(String),
    /// HTTP 401. Developer mode: fatal like any refusal. D1 product mode: the paired credential
    /// was revoked, so sync pauses (not a fault) until the device pairs again.
    Unauthorized(String),
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(HTTP_CONNECT_TIMEOUT)
        .timeout_read(HTTP_IO_TIMEOUT)
        .timeout_write(HTTP_IO_TIMEOUT)
        // Every read and write is bounded by the request deadline, pooled or not…
        .timeout(HTTP_REQUEST_DEADLINE)
        // …and no idle connection is kept to go stale across a suspend: each exchange opens a
        // fresh one (a LAN handshake per batch) with all socket timeouts set.
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .build()
}

fn send(
    agent: &ureq::Agent,
    cfg: &SyncConfigV1,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<(u16, Vec<u8>), PushError> {
    let url = format!("{}{path}", cfg.server_base_url.trim_end_matches('/'));
    let req = agent
        .request(method, &url)
        .set("Authorization", &format!("Bearer {}", cfg.token));
    let res = if body.is_empty() && method == "GET" {
        req.call()
    } else {
        req.send_bytes(body)
    };
    let resp = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => {
            return Err(PushError::Retry(format!(
                "transport: {}",
                redact(&e.to_string(), cfg)
            )))
        }
    };
    let status = resp.status();
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut out)
        .map_err(|e| PushError::Retry(format!("read response: {e}")))?;
    Ok((status, out))
}

fn redact(s: &str, cfg: &SyncConfigV1) -> String {
    if cfg.token.is_empty() {
        s.to_string()
    } else {
        s.replace(&cfg.token, "<redacted>")
    }
}

fn classify(status: u16, body: &[u8], what: &str) -> PushError {
    let text = String::from_utf8_lossy(body);
    if status == 401 {
        return PushError::Unauthorized(format!("{what}: server refused 401: {text}"));
    }
    if status >= 500 || status == 408 || status == 429 {
        PushError::Retry(format!("{what}: server {status}: {text}"))
    } else {
        PushError::Fatal(format!("{what}: server refused {status}: {text}"))
    }
}

fn jitter(d: Duration) -> Duration {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(Instant::now().elapsed().as_nanos());
    let frac = (h.finish() % 1000) as f64 / 1000.0; // 0..1
    d.mul_f64(0.5 + frac * 0.5)
}

fn uploader_loop(
    shared: &Arc<Mutex<Shared>>,
    wake: &Wake,
    stop: &AtomicBool,
    jtx: &Sender<JournalCmd>,
) {
    let agent = agent();
    let mut backoff = BACKOFF_MIN;
    while !stop.load(Ordering::SeqCst) {
        shared.lock().unwrap().pub_stats.uploader_polls += 1;
        // 19B.2B: no push and no pull before the (deferred) confirmed base is published; the
        // loader wakes this thread when it is.
        let (cfg, faulted) = {
            let g = shared.lock().unwrap();
            (g.config.clone(), g.fault.is_some() || g.base.is_none())
        };
        let Some(cfg) = cfg.filter(|_| !faulted) else {
            wake.wait(IDLE_POLL, stop);
            continue;
        };
        let push_blocked = {
            let g = shared.lock().unwrap();
            crate::sync_recovery_exec::recovery_blocks_ordinary_push(&g.dir)
        };
        let cycle = if push_blocked {
            inbound::pull_once(shared, &agent, &cfg)
        } else {
            push_once(shared, &agent, &cfg, jtx).and_then(|pushed| {
                inbound::pull_once(shared, &agent, &cfg).map(|pulled| pushed || pulled)
            })
        };
        match cycle {
            Ok(true) => backoff = BACKOFF_MIN, // progress: loop immediately
            Ok(false) => wake.wait(PULL_POLL, stop), // caught up: poll the tail again soon
            Err(PushError::Retry(e)) => {
                shared.lock().unwrap().pub_stats.transport_errors += 1;
                let d = jitter(backoff);
                log(format!("retrying in {}ms after {e}", d.as_millis()));
                wake.wait(d, stop);
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
            Err(PushError::Unauthorized(e)) if shared.lock().unwrap().product_mode => {
                let mut g = shared.lock().unwrap();
                log("authentication revoked (sync paused, outbox preserved, notebook unaffected)");
                g.auth_revoked = true;
                g.config = None;
                let _ = e;
            }
            Err(PushError::Fatal(e) | PushError::Unauthorized(e)) => {
                let mut g = shared.lock().unwrap();
                log(format!(
                    "FAULT (sync halted, outbox preserved, notebook unaffected): {e}"
                ));
                g.fault = Some(e);
            }
        }
    }
}

/// One unit of upload work. `Ok(true)` = progress made, `Ok(false)` = nothing to do.
fn push_once(
    shared: &Arc<Mutex<Shared>>,
    agent: &ureq::Agent,
    cfg: &SyncConfigV1,
    jtx: &Sender<JournalCmd>,
) -> Result<bool, PushError> {
    let (dir, genesis_uploaded, library_id) = {
        let g = shared.lock().unwrap();
        (g.dir.clone(), g.state.genesis_uploaded, g.state.library_id)
    };
    if !genesis_uploaded {
        let genesis = fs::read(dir.join(GENESIS_FILE))
            .map_err(|e| PushError::Fatal(format!("read genesis: {e}")))?;
        let snap = SyncSnapshotV1::parse(&genesis).map_err(|e| PushError::Fatal(e.to_string()))?;
        for page in snap.notebooks.iter().flat_map(|n| &n.pages) {
            put_blob_for(shared, agent, cfg, &dir, page)?;
        }
        let (status, body) = send(agent, cfg, "POST", "/v1/libraries", &genesis)?;
        match status {
            200 | 201 => {
                let mut state = shared.lock().unwrap().state.clone();
                state.genesis_uploaded = true;
                write_json(&dir, STATE_FILE, &state).map_err(PushError::Retry)?;
                shared.lock().unwrap().state.genesis_uploaded = true;
                log(if status == 201 {
                    "genesis established on server".to_string()
                } else {
                    "genesis already present (identical)".to_string()
                });
                return Ok(true);
            }
            _ => return Err(classify(status, &body, "genesis")),
        }
    }
    // Contiguous batch from acked+1.
    let batch: Vec<PendingOp> = {
        let g = shared.lock().unwrap();
        let mut size = 0;
        let mut out = Vec::new();
        for (want, op) in (g.state.acked_client_seq + 1..).zip(g.committed.iter()) {
            if op.seq != want
                || out.len() >= MAX_BATCH_OPS
                || size + op.bytes.len() > MAX_BATCH_BYTES
            {
                break;
            }
            size += op.bytes.len() + 1;
            out.push(op.clone());
        }
        out
    };
    if batch.is_empty() {
        return Ok(false);
    }
    for op in &batch {
        if let SyncOperationV1::CreateNotebook {
            first_page: page, ..
        }
        | SyncOperationV1::CreatePage { page, .. } = &op.envelope.operation
        {
            put_blob_ref(shared, agent, cfg, &dir, &page.background)?;
        }
    }
    let mut body = b"{\"envelopes\":[".to_vec();
    for (i, op) in batch.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        body.extend_from_slice(&op.bytes);
    }
    body.extend_from_slice(b"]}");
    let (status, resp) = send(
        agent,
        cfg,
        "POST",
        &format!("/v1/libraries/{library_id}/operations"),
        &body,
    )?;
    if status != 200 {
        return Err(classify(
            status,
            &resp,
            &format!("operations {}..{}", batch[0].seq, batch.last().unwrap().seq),
        ));
    }
    {
        let mut g = shared.lock().unwrap();
        if cfg.debug_drop_first_ack && !g.dropped_ack_once {
            g.dropped_ack_once = true;
            log(format!(
                "DEBUG: dropping ACK for {}..{} (lost-ACK test); will resend identical bytes",
                batch[0].seq,
                batch.last().unwrap().seq
            ));
            return Err(PushError::Retry("debug: simulated lost ACK".into()));
        }
    }
    // Prove acceptance of exactly these OperationIds.
    let v: serde_json::Value =
        serde_json::from_slice(&resp).map_err(|e| PushError::Retry(format!("bad ACK: {e}")))?;
    let results = v["results"]
        .as_array()
        .ok_or(PushError::Retry("ACK without results".into()))?;
    if results.len() != batch.len() {
        return Err(PushError::Fatal(format!(
            "ACK covers {} ops, sent {}",
            results.len(),
            batch.len()
        )));
    }
    let mut dup = 0;
    for (r, op) in results.iter().zip(&batch) {
        if r["operation_id"].as_str() != Some(&op.envelope.operation_id.to_string())
            || r["server_seq"].as_i64().is_none()
        {
            return Err(PushError::Fatal(format!(
                "ACK mismatch at client_seq {}",
                op.seq
            )));
        }
        if r["duplicate"].as_bool() == Some(true) {
            dup += 1;
        }
    }
    let last = batch.last().unwrap().seq;
    let mut records = Vec::with_capacity(batch.len());
    for (r, op) in results.iter().zip(&batch) {
        let server_seq = r["server_seq"]
            .as_u64()
            .ok_or_else(|| PushError::Fatal(format!("ACK without server_seq at {}", op.seq)))?;
        records.push(AcceptedOp {
            client_seq: op.seq,
            server_seq,
            envelope: op.envelope.clone(),
            bytes: op.bytes.clone(),
        });
    }
    // S2C: the ACK moves ops from the outbox into the accepted bridge (with their server_seq);
    // the canonical base only advances when the inbound cursor consumes them. The accepted
    // records are durable before the ACK state, which is durable before outbox compaction.
    //
    // Lock order io → shared. `io` serializes sync-state persistence (an apply transaction
    // cannot interleave), while `shared` is only held for short in-memory sections — never
    // across these fsyncs — so the runtime's post-commit capture never waits for them.
    let io = Arc::clone(&shared.lock().unwrap().io);
    let io_guard = io.lock().unwrap();
    #[cfg(test)]
    let delay = shared.lock().unwrap().ack_persist_delay;
    let (fresh, persisted) = {
        let g = shared.lock().unwrap();
        let (acked, cursor) = (g.state.acked_client_seq, g.state.server_cursor);
        // An inbound echo may already have consumed some (lost-ACK race): never re-add those.
        let fresh: Vec<AcceptedOp> = records
            .into_iter()
            .filter(|a| a.client_seq > acked && a.server_seq > cursor)
            .collect();
        let mut persisted = g.state.clone();
        persisted.acked_client_seq = persisted.acked_client_seq.max(last);
        persisted.next_client_seq = g.next_seq;
        (fresh, persisted)
    };
    let t_persist = Instant::now();
    #[cfg(test)]
    std::thread::sleep(delay);
    inbound::append_accepted(&dir, &fresh).map_err(PushError::Retry)?;
    write_json(&dir, STATE_FILE, &persisted).map_err(PushError::Retry)?;
    let ack_persist_ms = t_persist.elapsed().as_millis();
    let mut g = shared.lock().unwrap();
    g.state.acked_client_seq = g.state.acked_client_seq.max(persisted.acked_client_seq);
    g.state.next_client_seq = persisted.next_client_seq;
    g.accepted.extend(fresh);
    g.accepted.sort_by_key(|a| a.client_seq);
    let acked_now = g.state.acked_client_seq;
    while g.committed.front().is_some_and(|o| o.seq <= acked_now) {
        g.committed.pop_front();
    }
    g.pub_stats.uploaded += batch.len() as u64;
    g.pub_stats.duplicate_acks += dup;
    g.pub_stats.batches += 1;
    drop(g);
    drop(io_guard);
    let _ = jtx.send(JournalCmd::Compact(acked_now));
    log(format!(
        "uploaded operations {}..{} (server_seq {}..{}, duplicates {dup}) ack_persist_ms={ack_persist_ms} (shared lock not held)",
        batch[0].seq,
        last,
        results[0]["server_seq"],
        results.last().unwrap()["server_seq"]
    ));
    Ok(true)
}

fn put_blob_for(
    shared: &Arc<Mutex<Shared>>,
    agent: &ureq::Agent,
    cfg: &SyncConfigV1,
    dir: &Path,
    page: &SnapshotPage,
) -> Result<(), PushError> {
    put_blob_ref(shared, agent, cfg, dir, &page.background)
}

fn put_blob_ref(
    shared: &Arc<Mutex<Shared>>,
    agent: &ureq::Agent,
    cfg: &SyncConfigV1,
    dir: &Path,
    bg: &BackgroundRef,
) -> Result<(), PushError> {
    let BackgroundRef::Blob { sha256, .. } = bg else {
        return Ok(());
    };
    if shared
        .lock()
        .unwrap()
        .uploaded_blobs
        .contains(sha256.as_str())
    {
        return Ok(());
    }
    let bytes = fs::read(dir.join(BLOB_DIR).join(sha256.as_str()))
        .map_err(|e| PushError::Fatal(format!("blob {sha256} missing from sync state: {e}")))?;
    let (status, body) = send(agent, cfg, "PUT", &format!("/v1/blobs/{sha256}"), &bytes)?;
    if status != 200 && status != 201 {
        return Err(classify(status, &body, &format!("blob {sha256}")));
    }
    shared
        .lock()
        .unwrap()
        .uploaded_blobs
        .insert(sha256.as_str().to_string());
    log(format!("blob {sha256} present on server"));
    Ok(())
}

/// One authenticated request with the Kindle's blocking client (developer tools). Transport
/// errors are token-redacted; the token never appears in the returned strings.
pub fn http_request(
    cfg: &SyncConfigV1,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<(u16, Vec<u8>), String> {
    send(&agent(), cfg, method, path, body).map_err(|e| match e {
        PushError::Retry(m) | PushError::Fatal(m) | PushError::Unauthorized(m) => m,
    })
}

/// Read the developer-provisioned sync config (`<sync-dir>/config.json`). The token inside is
/// secret: callers must never print it (its `Debug` is redacted).
pub fn read_config(sync_dir: &Path) -> Result<SyncConfigV1, String> {
    let c: SyncConfigV1 = read_json(&sync_dir.join(CONFIG_FILE))?;
    if c.v != 1 {
        return Err(format!("unsupported config version {}", c.v));
    }
    Ok(c)
}

/// The server's current materialized snapshot of `library_id`: `(through_server_seq,
/// snapshot_sha256)`. Diagnostic only (the comparison tool); the Kindle never pulls in S2B.
pub fn fetch_server_snapshot_hash(
    cfg: &SyncConfigV1,
    library_id: &LibraryId,
) -> Result<(u64, String), String> {
    let path = format!("/v1/libraries/{library_id}/snapshot");
    let (status, body) = match send(&agent(), cfg, "GET", &path, b"") {
        Ok(r) => r,
        Err(PushError::Retry(e) | PushError::Fatal(e) | PushError::Unauthorized(e)) => {
            return Err(e)
        }
    };
    if status != 200 {
        return Err(format!(
            "server {status}: {}",
            redact(&String::from_utf8_lossy(&body), cfg)
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    let seq = v["through_server_seq"]
        .as_u64()
        .ok_or("snapshot response without through_server_seq")?;
    let sha = v["snapshot_sha256"]
        .as_str()
        .ok_or("snapshot response without snapshot_sha256")?;
    Ok((seq, sha.to_string()))
}

#[cfg(test)]
#[path = "s2b_outbound_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "sync_offline_capture_tests.rs"]
mod offline_capture_tests;

#[cfg(test)]
#[path = "s2c_inbound_tests.rs"]
mod s2c_tests;

#[cfg(test)]
#[path = "sync_recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
#[path = "sync_confirmed_tests.rs"]
mod confirmed_tests;

#[cfg(test)]
#[path = "sync_fast_path_tests.rs"]
mod fast_path_tests;

#[cfg(test)]
#[path = "sync_startup_diag_tests.rs"]
mod startup_diag_tests;

#[cfg(test)]
#[path = "sync_deferred_base_tests.rs"]
mod deferred_base_tests;

#[cfg(test)]
#[path = "page_ledger_tests.rs"]
mod page_ledger_tests;

#[cfg(test)]
#[path = "verified_baseline_tests.rs"]
mod verified_baseline_tests;

#[cfg(test)]
#[path = "nav_state_tests.rs"]
mod nav_state_tests;
