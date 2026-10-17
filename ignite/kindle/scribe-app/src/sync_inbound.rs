//! Sync Phase S2C: inbound (server → Kindle) half of the one sync engine.
//!
//! State model (all durable under `<sync-dir>`):
//!
//! ```text
//! ServerBaseState    base-state.json   canonical reducer state through server_cursor
//! accepted bridge    accepted.jsonl    own ops ACKed by the server (exact bytes + server_seq),
//!                                      kept until server_cursor crosses their server_seq
//! outbox             outbox.jsonl      own ops durable locally, not yet ACKed
//! LocalOverlay       = accepted(server_seq > cursor) ∪ outbox, replayed in client_seq order
//! ExpectedLocalState = ServerBaseState + LocalOverlay        (sync-engine is the authority)
//! inbox              inbox.jsonl       validated server tail items > server_cursor (exact bytes)
//! apply marker       apply.json        a materialization transaction is in flight
//! ```
//!
//! `server_cursor` = the highest contiguous `server_seq` whose canonical effect is durably in
//! `ServerBaseState` **and** reconciled into the Kindle files (verified by snapshot equality).
//!
//! Apply transaction (runtime thread, idle safe point only):
//! inbox items → reducer (every item, own echoes included, **in place**) → new ServerBaseState
//! → new ExpectedLocalState (rebased overlay) → `apply.json` → LocalMaterializer (the touched
//! pages + structure) → verify (the same scope) → append the items to the confirmed log
//! (`sync_confirmed.rs`; one durability barrier) → sync-state.json (cursor) → compact
//! accepted/inbox → remove `apply.json`. A crash anywhere leaves either the old consistent
//! state or a marker whose recovery re-runs the idempotent materialization; the cursor never
//! skips an unapplied item. Work is proportional to the applied operations, never to the
//! library: the full ServerBaseState is only written by the background checkpoint.

use std::collections::HashSet;

use serde_json::value::RawValue;

use super::*;
use crate::sync_materialize::{DeviceGeometry, LocalMaterializer, MaterializeReport};

pub const HISTORY_FILE: &str = "accepted.jsonl";
pub const INBOX_FILE: &str = "inbox.jsonl";
pub const APPLY_FILE: &str = "apply.json";
/// Tail page size requested from the server (bounded; never the whole ledger).
pub const TAIL_PAGE: u64 = 100;
/// Backpressure: stop fetching while this many items are staged but unapplied.
pub const INBOX_MAX: usize = 500;
/// Most inbox items materialized per apply transaction.
pub const APPLY_BATCH: usize = 200;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InboxItem {
    pub server_seq: u64,
    pub envelope: SyncEnvelopeV1,
    pub bytes: Vec<u8>,
}

/// An own operation the server acknowledged, retained (exact bytes) until inbound consumption.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AcceptedOp {
    pub client_seq: u64,
    pub server_seq: u64,
    pub envelope: SyncEnvelopeV1,
    pub bytes: Vec<u8>,
}

impl AcceptedOp {
    pub fn as_pending(&self) -> PendingOp {
        PendingOp {
            seq: self.client_seq,
            envelope: self.envelope.clone(),
            bytes: self.bytes.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxRecordV1 {
    v: u32,
    server_seq: u64,
    envelope: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptedRecordV1 {
    v: u32,
    client_seq: u64,
    server_seq: u64,
    operation_id: OperationId,
    envelope: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyMarkerV1 {
    v: u32,
    target_cursor: u64,
}

/// Summary of one completed apply transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundApplied {
    pub from_cursor: u64,
    pub through: u64,
    pub ops: usize,
    pub own_echoes: usize,
    pub report: MaterializeReport,
    /// Wall-clock phase timings (µs) of this transaction on the calling thread.
    pub timings: ApplyTimings,
}

/// Phase timings (µs) of one apply transaction, for latency attribution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplyTimings {
    pub barrier_us: u64,
    pub reducer_us: u64,
    pub rebase_us: u64,
    pub materialize_us: u64,
    pub verify_us: u64,
    pub persist_us: u64,
    pub total_us: u64,
}

fn us_since(t: std::time::Instant) -> u64 {
    t.elapsed().as_micros() as u64
}

/// Strict canonical envelope: exact canonical bytes, valid, of this library.
pub(crate) fn canonical_envelope(
    bytes: &[u8],
    library: LibraryId,
) -> Result<SyncEnvelopeV1, String> {
    let env: SyncEnvelopeV1 = from_json(bytes).map_err(|e| format!("invalid envelope: {e}"))?;
    let canon = env
        .to_canonical_bytes()
        .map_err(|e| format!("invalid envelope: {e}"))?;
    if canon != bytes {
        return Err("envelope bytes are not canonical".into());
    }
    if env.library_id != library {
        return Err(format!("envelope of foreign library {}", env.library_id));
    }
    Ok(env)
}

/// Complete lines of a JSONL file (a torn final line — crash mid-append — is ignored).
fn lines(path: &Path) -> Result<Vec<Vec<u8>>, String> {
    let raw = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..raw.len() {
        if raw[i] == b'\n' {
            if i > start {
                out.push(raw[start..i].to_vec());
            }
            start = i + 1;
        }
    }
    Ok(out)
}

fn inbox_line(i: &InboxItem) -> Vec<u8> {
    let mut v = serde_json::to_vec(&InboxRecordV1 {
        v: 1,
        server_seq: i.server_seq,
        envelope: String::from_utf8(i.bytes.clone()).expect("canonical JSON is UTF-8"),
    })
    .expect("inbox record serializes");
    v.push(b'\n');
    v
}

pub(crate) fn accepted_line(a: &AcceptedOp) -> Vec<u8> {
    let mut v = serde_json::to_vec(&AcceptedRecordV1 {
        v: 1,
        client_seq: a.client_seq,
        server_seq: a.server_seq,
        operation_id: a.envelope.operation_id,
        envelope: String::from_utf8(a.bytes.clone()).expect("canonical JSON is UTF-8"),
    })
    .expect("accepted record serializes");
    v.push(b'\n');
    v
}

fn append_fsync(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() {
        return Ok(());
    }
    let path = dir.join(name);
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    f.write_all(bytes)
        .and_then(|_| f.sync_data())
        .map_err(|e| format!("append {}: {e}", path.display()))?;
    sync_dir_fsync(dir);
    Ok(())
}

/// Staged inbox items after `cursor`: strictly contiguous, identical duplicates tolerated.
pub(crate) fn load_inbox(
    dir: &Path,
    library: LibraryId,
    cursor: u64,
) -> Result<Vec<InboxItem>, String> {
    let mut out: Vec<InboxItem> = Vec::new();
    for l in lines(&dir.join(INBOX_FILE))? {
        let r: InboxRecordV1 =
            serde_json::from_slice(&l).map_err(|e| format!("corrupt inbox record: {e}"))?;
        if r.v != 1 {
            return Err(format!("unsupported inbox record version {}", r.v));
        }
        if r.server_seq <= cursor {
            continue;
        }
        let bytes = r.envelope.into_bytes();
        let envelope = canonical_envelope(&bytes, library)?;
        let want = out.last().map_or(cursor + 1, |i| i.server_seq + 1);
        if let Some(prev) = out.iter().find(|i| i.server_seq == r.server_seq) {
            if prev.bytes != bytes {
                return Err(format!(
                    "inbox: conflicting items for server_seq {}",
                    r.server_seq
                ));
            }
            continue;
        }
        if r.server_seq != want {
            return Err(format!(
                "inbox: server_seq {} after {}",
                r.server_seq,
                want - 1
            ));
        }
        out.push(InboxItem {
            server_seq: r.server_seq,
            envelope,
            bytes,
        });
    }
    Ok(out)
}

/// Accepted bridge records not yet consumed (`server_seq > cursor`), in client_seq order.
///
/// `accepted.jsonl` is authoritative durable state, so **every** record is validated before
/// any duplicate or cursor decision: canonical envelope bytes of this library, authored by this
/// replica, whose `operation_id`/`client_seq` match the record. Then the bridge must be a
/// consistent function of the ledger:
///
/// * a byte-for-byte identical repeat (same client_seq, server_seq, OperationId, bytes — e.g.
///   an ACK re-recorded after a crash) is an idempotent duplicate and is tolerated;
/// * any other repeat is corruption and is refused: a client_seq with a different server_seq,
///   OperationId or bytes; an OperationId under two client_seqs; a server_seq assigned to two
///   different own operations. Never first-wins or last-wins.
/// * Ordering invariant: for one replica, increasing client_seq ⇒ strictly increasing
///   server_seq. The S2A ledger guarantees it (inside its single serialized write transaction a
///   replica's next op must be exactly `last_client_seq + 1` and receives `head + 1`); other
///   replicas' ops may sit between, so gaps are normal. A violation is refused.
///
/// Consumed records (`server_seq <= cursor`) are validated the same way and then dropped, so a
/// corrupt record can never hide behind cursor filtering or an earlier valid record.
pub(crate) fn load_accepted(
    dir: &Path,
    library: LibraryId,
    replica: ReplicaId,
    cursor: u64,
) -> Result<Vec<AcceptedOp>, String> {
    use std::collections::{BTreeMap, HashMap};
    let mut by_client: BTreeMap<u64, AcceptedOp> = BTreeMap::new();
    let mut client_of_op: HashMap<OperationId, u64> = HashMap::new();
    let mut client_of_server: HashMap<u64, u64> = HashMap::new();
    for (line_no, l) in lines(&dir.join(HISTORY_FILE))?.into_iter().enumerate() {
        let at = |m: String| format!("ACCEPTED_HISTORY_CORRUPT (record {}): {m}", line_no + 1);
        let r: AcceptedRecordV1 =
            serde_json::from_slice(&l).map_err(|e| at(format!("unparsable: {e}")))?;
        if r.v != 1 {
            return Err(at(format!("unsupported record version {}", r.v)));
        }
        let bytes = r.envelope.into_bytes();
        let envelope = canonical_envelope(&bytes, library).map_err(at)?;
        if envelope.operation_id != r.operation_id {
            return Err(at(format!(
                "operation_id {} but envelope carries {}",
                r.operation_id, envelope.operation_id
            )));
        }
        if envelope.client_seq != r.client_seq {
            return Err(at(format!(
                "client_seq {} but envelope carries {}",
                r.client_seq, envelope.client_seq
            )));
        }
        if envelope.replica_id != replica {
            return Err(at(format!(
                "operation {} was authored by replica {}, not this replica",
                r.operation_id, envelope.replica_id
            )));
        }
        let rec = AcceptedOp {
            client_seq: r.client_seq,
            server_seq: r.server_seq,
            envelope,
            bytes,
        };
        if let Some(prev) = by_client.get(&rec.client_seq) {
            if *prev == rec {
                continue; // exact idempotent duplicate
            }
            let what = if prev.server_seq != rec.server_seq {
                "server_seq"
            } else if prev.envelope.operation_id != rec.envelope.operation_id {
                "OperationId"
            } else {
                "envelope bytes"
            };
            return Err(at(format!(
                "client_seq {} recorded twice with a different {what}",
                rec.client_seq
            )));
        }
        if let Some(c) = client_of_op.get(&rec.envelope.operation_id) {
            return Err(at(format!(
                "OperationId {} recorded under client_seq {c} and {}",
                rec.envelope.operation_id, rec.client_seq
            )));
        }
        if let Some(c) = client_of_server.get(&rec.server_seq) {
            return Err(at(format!(
                "server_seq {} assigned to own client_seq {c} and {}",
                rec.server_seq, rec.client_seq
            )));
        }
        client_of_op.insert(rec.envelope.operation_id, rec.client_seq);
        client_of_server.insert(rec.server_seq, rec.client_seq);
        by_client.insert(rec.client_seq, rec);
    }
    // Ordering invariant over the whole (validated) bridge, consumed records included.
    let mut prev: Option<&AcceptedOp> = None;
    for a in by_client.values() {
        if let Some(p) = prev {
            if a.server_seq <= p.server_seq {
                return Err(format!(
                    "ACCEPTED_HISTORY_CORRUPT: client_seq {} has server_seq {} but earlier client_seq {} has server_seq {}",
                    a.client_seq, a.server_seq, p.client_seq, p.server_seq
                ));
            }
        }
        prev = Some(a);
    }
    Ok(by_client
        .into_values()
        .filter(|a| a.server_seq > cursor)
        .collect())
}

pub(crate) fn append_accepted(dir: &Path, ops: &[AcceptedOp]) -> Result<(), String> {
    let mut bytes = Vec::new();
    for a in ops {
        bytes.extend(accepted_line(a));
    }
    append_fsync(dir, HISTORY_FILE, &bytes)
}

pub(crate) fn read_marker(dir: &Path) -> Result<Option<u64>, String> {
    let p = dir.join(APPLY_FILE);
    if !p.exists() {
        return Ok(None);
    }
    let m: ApplyMarkerV1 = read_json(&p)?;
    Ok(Some(m.target_cursor))
}

/// Keep only records beyond `cursor` in the accepted/inbox files (re-read from disk under the
/// io lock: the coordinator may have appended meanwhile), then drop the apply marker.
/// Rewrite the bridge/inbox files only while at most this many live records remain. Consumed
/// records (server_seq ≤ cursor) are skipped by the loaders, so leaving them is always correct;
/// rewriting a large live set after every transaction (thousands of acknowledged ops after a
/// big import) stalled the runtime thread for seconds per transaction on the Kindle.
pub const BRIDGE_COMPACT_MAX_LIVE: usize = 500;

/// End of an apply transaction (cursor already durable): reclaim consumed bridge/inbox records
/// when that is cheap — written from the in-memory live sets, which equal the files' live
/// records because every append updates memory under the same `io` lock — then drop the marker.
fn finish_apply(g: &mut Shared) -> Result<(), String> {
    if g.accepted.len() + g.inbox.len() <= BRIDGE_COMPACT_MAX_LIVE {
        let mut b = Vec::new();
        for i in &g.inbox {
            b.extend(inbox_line(i));
        }
        write_atomic(&g.dir, INBOX_FILE, &b)?;
        let mut b = Vec::new();
        for a in &g.accepted {
            b.extend(accepted_line(a));
        }
        write_atomic(&g.dir, HISTORY_FILE, &b)?;
        g.pub_stats.bridge_compactions += 1;
    }
    match fs::remove_file(g.dir.join(APPLY_FILE)) {
        Ok(()) => sync_dir_fsync(&g.dir),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("remove apply marker: {e}")),
    }
    Ok(())
}

pub(crate) fn compact_after(
    dir: &Path,
    library: LibraryId,
    replica: ReplicaId,
    cursor: u64,
) -> Result<(), String> {
    let inbox = load_inbox(dir, library, cursor)?;
    let mut b = Vec::new();
    for i in &inbox {
        b.extend(inbox_line(i));
    }
    write_atomic(dir, INBOX_FILE, &b)?;
    let acc = load_accepted(dir, library, replica, cursor)?;
    let mut b = Vec::new();
    for a in &acc {
        b.extend(accepted_line(a));
    }
    write_atomic(dir, HISTORY_FILE, &b)?;
    match fs::remove_file(dir.join(APPLY_FILE)) {
        Ok(()) => sync_dir_fsync(dir),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("remove apply marker: {e}")),
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// Tail fetch (coordinator thread)

/// Parse + strictly validate one tail page: `library_id` matches, items are exactly
/// `after+1, after+2, …` (no gap, no duplicate), every envelope is canonical and of this library.
pub(crate) fn parse_tail(
    body: &[u8],
    library: LibraryId,
    after: u64,
) -> Result<(u64, Vec<InboxItem>), String> {
    #[derive(Deserialize)]
    struct E<'a> {
        server_seq: u64,
        #[serde(borrow)]
        envelope: &'a RawValue,
    }
    #[derive(Deserialize)]
    struct T<'a> {
        library_id: String,
        head_server_seq: u64,
        #[serde(borrow)]
        operations: Vec<E<'a>>,
        #[allow(dead_code)]
        next_after: u64,
    }
    let t: T<'_> = serde_json::from_slice(body).map_err(|e| format!("malformed tail: {e}"))?;
    if t.library_id != library.to_string() {
        return Err(format!("tail of foreign library {}", t.library_id));
    }
    let mut out = Vec::with_capacity(t.operations.len());
    for (want, e) in (after + 1..).zip(t.operations) {
        if e.server_seq != want {
            return Err(format!(
                "tail gap/duplicate: expected server_seq {want}, got {}",
                e.server_seq
            ));
        }
        let bytes = e.envelope.get().as_bytes().to_vec();
        let envelope = canonical_envelope(&bytes, library)
            .map_err(|x| format!("server_seq {}: {x}", e.server_seq))?;
        out.push(InboxItem {
            server_seq: e.server_seq,
            envelope,
            bytes,
        });
    }
    // `head_server_seq` is advisory (read separately from the rows); contiguity is the guarantee.
    Ok((t.head_server_seq, out))
}

/// Make a background blob referenced by a staged op available locally (verified).
fn ensure_blob(
    agent: &ureq::Agent,
    cfg: &SyncConfigV1,
    dir: &Path,
    bg: &BackgroundRef,
) -> Result<(), PushError> {
    let BackgroundRef::Blob {
        sha256,
        byte_len,
        format,
    } = bg
    else {
        return Ok(());
    };
    let path = dir.join(BLOB_DIR).join(sha256.as_str());
    if fs::read(&path).is_ok_and(|b| sha256_hex(&b) == *sha256) {
        return Ok(());
    }
    if format.width == 0
        || format.height == 0
        || format.stride < format.width
        || u64::from(format.stride) * u64::from(format.height) != *byte_len
    {
        return Err(PushError::Fatal(format!(
            "blob {sha256}: insane format {format:?}"
        )));
    }
    let (status, bytes) = send(agent, cfg, "GET", &format!("/v1/blobs/{sha256}"), b"")?;
    if status != 200 {
        return Err(classify(status, &bytes, &format!("blob {sha256}")));
    }
    if bytes.len() as u64 != *byte_len || sha256_hex(&bytes) != *sha256 {
        return Err(PushError::Fatal(format!(
            "blob {sha256} failed verification"
        )));
    }
    store_blob(dir, &bytes).map_err(PushError::Retry)?;
    Ok(())
}

/// Fetch one bounded tail page after the staged/applied position and stage it durably.
/// `Ok(true)` = items staged.
pub(crate) fn pull_once(
    shared: &Arc<Mutex<Shared>>,
    agent: &ureq::Agent,
    cfg: &SyncConfigV1,
) -> Result<bool, PushError> {
    let (dir, lib, uploaded, after, backlog, io, crash) = {
        let g = shared.lock().unwrap();
        let after = g
            .inbox
            .last()
            .map_or(g.state.server_cursor, |i| i.server_seq)
            .max(g.state.server_cursor);
        (
            g.dir.clone(),
            g.state.library_id,
            g.state.genesis_uploaded,
            after,
            g.inbox.len(),
            Arc::clone(&g.io),
            g.crash_at.clone(),
        )
    };
    if !uploaded || backlog >= INBOX_MAX {
        return Ok(false);
    }
    let path = format!("/v1/libraries/{lib}/operations?after={after}&limit={TAIL_PAGE}");
    let (status, body) = send(agent, cfg, "GET", &path, b"")?;
    if status != 200 {
        return Err(classify(status, &body, "operations tail"));
    }
    let (_head, items) = parse_tail(&body, lib, after)
        .map_err(|e| PushError::Fatal(format!("TAIL_REFUSED: {e}")))?;
    if items.is_empty() {
        return Ok(false);
    }
    for it in &items {
        match &it.envelope.operation {
            SyncOperationV1::CreateNotebook { first_page: p, .. }
            | SyncOperationV1::CreatePage { page: p, .. } => {
                ensure_blob(agent, cfg, &dir, &p.background)?
            }
            _ => {}
        }
    }
    // `io` is held until the in-memory inbox matches the file (lock order io → shared): an
    // apply transaction compacts the inbox from memory and must never see one without the other.
    let io_guard = io.lock().unwrap();
    let mut b = Vec::new();
    for it in &items {
        b.extend(inbox_line(it));
    }
    append_fsync(&dir, INBOX_FILE, &b).map_err(PushError::Retry)?;
    if crash.as_deref() == Some("after_inbox_fsync") {
        return Err(PushError::Fatal(
            "SIMULATED CRASH at after_inbox_fsync".into(),
        ));
    }
    let mut g = shared.lock().unwrap();
    let n = items.len();
    let (first, last) = (items[0].server_seq, items[n - 1].server_seq);
    g.inbox.extend(items);
    g.pub_stats.staged += n as u64;
    drop(g);
    drop(io_guard);
    log(format!("staged inbound server_seq {first}..{last}"));
    Ok(true)
}

// ------------------------------------------------------------------------------------------
// Apply transaction (runtime thread at a safe point, or startup recovery)

impl Shared {
    fn hook(&self, name: &str) -> Result<(), String> {
        if self.crash_at.as_deref() == Some(name) {
            return Err(format!("SIMULATED CRASH at {name}"));
        }
        Ok(())
    }

    fn device(&self) -> Result<DeviceGeometry, String> {
        self.device
            .ok_or_else(|| "no device geometry (library has no page)".into())
    }
}

/// Apply up to `limit` staged items (optionally only through `target`). See module docs for
/// the crash ordering. Holds the caller's `Shared` borrow throughout (no network).
///
/// **The caller holds `Shared::io`** (lock order everywhere: `io` → `shared`), so the uploader
/// can persist ACK state under `io` without ever holding `shared` across an fsync.
///
/// Own-echo fast path: when every item is one of our own operations and together they are, in
/// order, the front of the local overlay (accepted ∪ outbox by client_seq), the reducer consumes
/// exactly the op sequence the overlay already describes: `base + echoes + rest == base +
/// overlay`. ExpectedLocalState is therefore unchanged and the files need no materialization or
/// re-verification (startup reconciliation remains the authority). Only the base, cursor and
/// compaction advance.
pub(crate) fn apply_staged(
    g: &mut Shared,
    limit: usize,
    target: Option<u64>,
) -> Result<Option<InboundApplied>, String> {
    // The ServerBaseState is advanced in place (never cloned). On `Err` it may be partially
    // advanced: every caller then faults sync, and a faulted state is never persisted — the
    // durable checkpoint + confirmed log remain the truth for the next start.
    let placeholder = LibraryState {
        snapshot: SyncSnapshotV1 {
            v: g.base.snapshot.v,
            library_id: g.base.snapshot.library_id,
            page_space: g.base.snapshot.page_space,
            notebooks: Vec::new(),
            folders: Vec::new(),
        },
        history: HistoryV1::empty(),
    };
    let mut base = std::mem::replace(&mut g.base, placeholder);
    let r = apply_staged_on(g, &mut base, limit, target);
    g.base = base;
    r
}

fn apply_staged_on(
    g: &mut Shared,
    base: &mut LibraryState,
    limit: usize,
    target: Option<u64>,
) -> Result<Option<InboundApplied>, String> {
    let from = g.state.server_cursor;
    let items: Vec<InboxItem> = g
        .inbox
        .iter()
        .filter(|i| i.server_seq > from && target.is_none_or(|t| i.server_seq <= t))
        .take(limit)
        .cloned()
        .collect();
    if items.is_empty() {
        return Ok(None);
    }
    for (k, it) in items.iter().enumerate() {
        if it.server_seq != from + 1 + k as u64 {
            return Err(format!("inbox gap at server_seq {}", it.server_seq));
        }
    }
    let through = items.last().unwrap().server_seq;
    let t_start = std::time::Instant::now();
    let mut timings = ApplyTimings::default();

    // 1. Every server operation (own echoes included) through the reducer, in server order.
    let mut consumed: HashSet<OperationId> = HashSet::new();
    let mut echoed_pending_max = 0u64;
    for it in &items {
        let outcome = base
            .apply_in_place(&it.envelope)
            .map_err(|e| format!("reducer refused server_seq {}: {e}", it.server_seq))?;
        if it.envelope.replica_id != g.state.replica_id {
            continue;
        }
        if let sync_engine::Outcome::NoOp(why) = outcome {
            // Our authored change had a local effect but is void in canonical order (e.g. its
            // notebook was deleted first). Converging would silently drop local work: stop.
            return Err(format!(
                "LOCAL_REBASE_CONFLICT: own client_seq {} ({}) became a no-op in canonical order: {why}",
                it.envelope.client_seq,
                op_kind(&it.envelope.operation)
            ));
        }
        let id = it.envelope.operation_id;
        if let Some(a) = g.accepted.iter().find(|a| a.envelope.operation_id == id) {
            if a.bytes != it.bytes || a.server_seq != it.server_seq {
                return Err(format!(
                    "own echo {id} differs from its acknowledged record"
                ));
            }
        } else if let Some(p) = g.committed.iter().find(|p| p.envelope.operation_id == id) {
            // Accepted by the server but its ACK never reached us (lost ACK).
            if p.bytes != it.bytes {
                return Err(format!("own echo {id} differs from its outbox record"));
            }
            echoed_pending_max = echoed_pending_max.max(p.seq);
        } else {
            return Err(format!(
                "own operation {id} (client_seq {}) is in the server tail but not in local history",
                it.envelope.client_seq
            ));
        }
        consumed.insert(id);
    }

    timings.reducer_us = us_since(t_start);

    let touched: HashSet<PageId> = items
        .iter()
        .flat_map(|it| confirmed::touched_pages(&it.envelope.operation))
        .collect();
    let echo_prefix = is_own_echo_prefix(g, &items);
    let report = if echo_prefix {
        MaterializeReport::default()
    } else {
        materialize_rebased(g, base, &consumed, through, &touched, &mut timings)?
    };

    // 4. Persist: the items to the confirmed log (one barrier), then cursor, then compact.
    let t = std::time::Instant::now();
    let records: Vec<(u64, &[u8])> = items
        .iter()
        .map(|it| (it.server_seq, it.bytes.as_slice()))
        .collect();
    let appended = g
        .log
        .as_mut()
        .ok_or("confirmed log not open")?
        .append(&records)?;
    g.dirty_pages.extend(touched.iter().copied());
    g.pub_stats.confirmed_log_bytes += appended;
    g.hook("after_base_write")?;
    g.hook("before_cursor_write")?;
    g.state.server_cursor = through;
    g.state.acked_client_seq = g.state.acked_client_seq.max(echoed_pending_max);
    g.state.next_client_seq = g.next_seq;
    write_json(&g.dir, STATE_FILE, &g.state)?;
    g.hook("after_cursor_write")?;
    g.accepted
        .retain(|a| a.server_seq > through && !consumed.contains(&a.envelope.operation_id));
    g.inbox.retain(|i| i.server_seq > through);
    finish_apply(g)?;
    timings.persist_us = us_since(t);
    timings.total_us = us_since(t_start);
    g.last_inbound = std::time::Instant::now();
    let acked = g.state.acked_client_seq;
    while g.committed.front().is_some_and(|o| o.seq <= acked) {
        g.committed.pop_front();
    }
    let own_echoes = consumed.len();
    g.pub_stats.applied += items.len() as u64;
    g.pub_stats.own_echoes += own_echoes as u64;
    if echo_prefix {
        g.pub_stats.echo_fast_path += 1;
    }
    log(format!(
        "inbound applied server_seq {}..{through} ops={} own_echoes={own_echoes} changed={}          echo_fast_path={echo_prefix} reducer_ms={} rebase_ms={} materialize_ms={} verify_ms={}          persist_ms={} total_ms={}",
        from + 1,
        items.len(),
        report.changed(),
        timings.reducer_us / 1000,
        timings.rebase_us / 1000,
        timings.materialize_us / 1000,
        timings.verify_us / 1000,
        timings.persist_us / 1000,
        timings.total_us / 1000
    ));
    Ok(Some(InboundApplied {
        from_cursor: from,
        through,
        ops: items.len(),
        own_echoes,
        report,
        timings,
    }))
}

/// True when `items` are all our own operations and are, in order, the front of the local
/// overlay (accepted bridge ∪ outbox, by client_seq). See [`apply_staged`].
fn is_own_echo_prefix(g: &Shared, items: &[InboxItem]) -> bool {
    if items
        .iter()
        .any(|it| it.envelope.replica_id != g.state.replica_id)
    {
        return false;
    }
    let mut overlay: Vec<(u64, OperationId)> = g
        .accepted
        .iter()
        .map(|a| (a.client_seq, a.envelope.operation_id))
        .chain(g.committed.iter().map(|p| (p.seq, p.envelope.operation_id)))
        .collect();
    overlay.sort_by_key(|o| o.0);
    overlay.len() >= items.len()
        && items.iter().zip(&overlay).all(|(it, (seq, id))| {
            it.envelope.client_seq == *seq && it.envelope.operation_id == *id
        })
}

/// Steps 2–3 of the apply transaction (general path): rebase the overlay on the new canonical
/// base, write the apply marker, materialize the Kindle files and verify them.
fn materialize_rebased(
    g: &mut Shared,
    base: &LibraryState,
    consumed: &HashSet<OperationId>,
    through: u64,
    touched: &HashSet<PageId>,
    timings: &mut ApplyTimings,
) -> Result<MaterializeReport, String> {
    // 2. Rebase the local overlay on the new canonical base (client_seq order).
    let t = std::time::Instant::now();
    let mut overlay: Vec<PendingOp> = g
        .accepted
        .iter()
        .filter(|a| !consumed.contains(&a.envelope.operation_id))
        .map(AcceptedOp::as_pending)
        .collect();
    overlay.extend(
        g.committed
            .iter()
            .filter(|p| !consumed.contains(&p.envelope.operation_id))
            .cloned(),
    );
    overlay.sort_by_key(|o| o.seq);
    // Nothing pending locally ⇒ ExpectedLocalState is the base itself (no copy).
    let expected: std::borrow::Cow<'_, LibraryState> = if overlay.is_empty() {
        std::borrow::Cow::Borrowed(base)
    } else {
        std::borrow::Cow::Owned(rebase_overlay(base, &overlay)?)
    };
    timings.rebase_us = us_since(t);

    // 3. Marker, materialize, verify.
    let t = std::time::Instant::now();
    write_json(
        &g.dir,
        APPLY_FILE,
        &ApplyMarkerV1 {
            v: 1,
            target_cursor: through,
        },
    )?;
    g.hook("after_marker")?;
    let crash = g.crash_at.clone();
    let hook = move |name: &str| -> Result<(), String> {
        if crash.as_deref() == Some(name) {
            Err(format!("SIMULATED CRASH at {name}"))
        } else {
            Ok(())
        }
    };
    let blob_dir = g.dir.join(BLOB_DIR);
    crate::store::reset_background_io_stats();
    let report = LocalMaterializer {
        library_root: &g.library_root,
        device: g.device()?,
        blob_dir: &blob_dir,
        hook: &hook,
        scope: Some(touched),
    }
    .reconcile(&expected.snapshot)?;
    timings.materialize_us = us_since(t);
    let mat_bg = crate::store::take_background_io_stats();
    let t = std::time::Instant::now();
    crate::store::reset_background_io_stats();
    let identity = read_identity(&g.library_root)?.ok_or("identity missing")?;
    crate::sync_local::verify_scoped(&g.library_root, &identity, &expected.snapshot, touched)?;
    g.identity = identity;
    timings.verify_us = us_since(t);
    let ver_bg = crate::store::take_background_io_stats();
    log(format!(
        "inbound_bg materialize_{} verify_{} touched_pages={}",
        mat_bg.log_suffix(timings.materialize_us),
        ver_bg.log_suffix(timings.verify_us),
        touched.len()
    ));
    g.hook("after_verify")?;
    Ok(report)
}

/// Own echoes are not consumed while more than this many local operations still await upload.
/// Consuming them persists the whole ServerBaseState, and the uploader's ACK waits for that
/// write — during a large backlog uploads would advance one batch per base write (seconds each
/// on the Kindle, growing with the library). The echoes are consumed after the backlog drains.
pub const DEFER_OWN_ECHOES_WHILE_PENDING: usize = 200;

/// Every staged item is our own operation's echo.
fn staged_all_own(g: &Shared) -> bool {
    g.inbox
        .iter()
        .filter(|i| i.server_seq > g.state.server_cursor)
        .all(|i| i.envelope.replica_id == g.state.replica_id)
}

/// Staged work is only our own echoes and a large upload backlog remains: defer it. Foreign
/// operations (another replica's edits) are never deferred.
fn own_echoes_deferred(g: &Shared) -> bool {
    g.committed.len() > DEFER_OWN_ECHOES_WHILE_PENDING && staged_all_own(g)
}

/// ExpectedLocalState = `base` + overlay. Each pending local op must still *apply* (not be
/// refused, not become a no-op) on the canonical base; otherwise LOCAL_REBASE_CONFLICT.
pub(crate) fn rebase_overlay(
    base: &LibraryState,
    overlay: &[PendingOp],
) -> Result<LibraryState, String> {
    let mut s = base.clone();
    for op in overlay {
        let outcome = s.apply_in_place(&op.envelope).map_err(|e| {
            format!(
                "LOCAL_REBASE_CONFLICT: pending client_seq {} refused on canonical state: {e}",
                op.seq
            )
        })?;
        if let sync_engine::Outcome::NoOp(why) = outcome {
            return Err(format!(
                "LOCAL_REBASE_CONFLICT: pending client_seq {} ({}) became a no-op on canonical state: {why}",
                op.seq,
                op_kind(&op.envelope.operation)
            ));
        }
    }
    Ok(s)
}

impl OutboundSync {
    /// Staged inbound work exists and sync is healthy (cheap; the runtime polls it at idle).
    pub fn inbound_ready(&self) -> bool {
        let g = self.shared.lock().unwrap();
        g.fault.is_none()
            && g.inbox.iter().any(|i| i.server_seq > g.state.server_cursor)
            && !own_echoes_deferred(&g)
    }

    /// Run one apply transaction. **Runtime thread, idle safe point only** (no gesture, no
    /// open write-ahead record). Never authors local operations. `Err` faults sync (the
    /// notebook is unaffected); `Ok(None)` = nothing to do.
    pub fn apply_inbound(&self) -> Result<Option<InboundApplied>, String> {
        if self.fault().is_some() {
            return Ok(None);
        }
        // Every local capture queued so far is durable in the outbox before we rebase on it.
        let t_barrier = std::time::Instant::now();
        let (tx, rx) = mpsc::sync_channel(1);
        if self.journal_tx.send(JournalCmd::Barrier(tx)).is_err()
            || rx.recv_timeout(PREP_TIMEOUT).is_err()
        {
            self.fail("journal barrier failed".into());
            return Ok(None);
        }
        let barrier_us = us_since(t_barrier);
        // Lock order io → shared (see `apply_staged`).
        let io = Arc::clone(&self.shared.lock().unwrap().io);
        let _io = io.lock().unwrap();
        let mut g = self.shared.lock().unwrap();
        if own_echoes_deferred(&g) {
            return Ok(None);
        }
        // A run of our own echoes is consumed in one transaction (one base write for the whole
        // staged window); anything foreign keeps the normal batch.
        let limit = if staged_all_own(&g) {
            INBOX_MAX
        } else {
            APPLY_BATCH
        };
        match apply_staged(&mut g, limit, None) {
            Ok(mut r) => {
                if let Some(a) = r.as_mut() {
                    a.timings.barrier_us = barrier_us;
                }
                let acked = g.state.acked_client_seq;
                drop(g);
                if r.is_some() {
                    let _ = self.journal_tx.send(JournalCmd::Compact(acked));
                }
                Ok(r)
            }
            Err(e) => {
                log(format!(
                    "FAULT (inbound halted; notebook, outbox and inbox preserved): {e}"
                ));
                g.fault = Some(e.clone());
                Err(e)
            }
        }
    }

    pub fn server_cursor(&self) -> u64 {
        self.shared.lock().unwrap().state.server_cursor
    }

    pub fn inbox_len(&self) -> usize {
        self.shared.lock().unwrap().inbox.len()
    }

    pub fn accepted_len(&self) -> usize {
        self.shared.lock().unwrap().accepted.len()
    }

    pub fn next_client_seq(&self) -> u64 {
        self.shared.lock().unwrap().next_seq
    }

    /// The runtime's real framebuffer geometry: remotely created pages must match it exactly
    /// (the page-derived default is only a headless fallback).
    pub fn set_device(&self, device: DeviceGeometry) {
        self.shared.lock().unwrap().device = Some(device);
    }

    /// Test/diagnostic crash point (see `apply_staged` / materializer hook names).
    pub fn set_crash_point(&self, point: Option<&str>) {
        self.shared.lock().unwrap().crash_at = point.map(str::to_string);
    }
}
