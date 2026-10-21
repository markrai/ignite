//! Slice 19B.2B: deferred confirmed-base loading on proven verified-fast-path starts only.
//! Eligibility (synchronous for everything else), capture while loading, the upload / inbound /
//! checkpoint gates, faults, shutdown, and equivalence with a synchronous start.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::confirmed::{self, ConfirmedLog, CHECKPOINT_DIR, CHECKPOINT_PAGES};
use super::tests::{env, server, stop, wait_until, write, write_config, Env};
use super::*;
use crate::sync_startup_diag::{field, take_last_line};

const T: Duration = Duration::from_secs(10);

fn opts(gate: Option<Arc<LoaderGate>>) -> SyncOpenOptions {
    SyncOpenOptions {
        device: None,
        defer_confirmed_base: true,
        loader_gate: gate,
    }
}

/// Open with deferral requested; returns the instance and its startup breakdown line.
fn open_deferred(e: &Env, gate: Option<Arc<LoaderGate>>) -> (OutboundSync, String) {
    let s = OutboundSync::open_with(&e.lib, &e.sync, opts(gate)).unwrap();
    (s, take_last_line().expect("breakdown"))
}

fn released_gate() -> Arc<LoaderGate> {
    let g = Arc::new(LoaderGate::default());
    g.release();
    g
}

/// Ids of the fixture's first notebook/page (read while a base exists).
fn first_ids(s: &OutboundSync) -> (LibraryId, NotebookId, PageId) {
    let g = s.shared.lock().unwrap();
    let nbk = &g.base.as_ref().unwrap().snapshot.notebooks[0];
    (g.state.library_id, nbk.notebook_id, nbk.pages[0].page_id)
}

/// Stage `n` foreign AddStroke items after the cursor (as the coordinator would), without
/// touching the base (so it works while the base is still loading).
fn stage_foreign(
    s: &OutboundSync,
    ids: (LibraryId, NotebookId, PageId),
    replica: ReplicaId,
    client: &mut u64,
    n: usize,
) {
    let (library_id, nb, page) = ids;
    let mut g = s.shared.lock().unwrap();
    let mut seq = g
        .inbox
        .last()
        .map_or(g.state.server_cursor, |i| i.server_seq);
    for _ in 0..n {
        seq += 1;
        *client += 1;
        let id = StrokeId::from_uuid(Uuid::now_v7()).unwrap();
        let y = 200.0 + 13.0 * (*client % 1000) as f32;
        let env = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: OperationId::from_uuid(Uuid::now_v7()).unwrap(),
            library_id,
            replica_id: replica,
            client_seq: *client,
            operation: SyncOperationV1::AddStroke {
                notebook_id: nb,
                page_id: page,
                stroke: SyncStroke {
                    stroke_id: id,
                    root_id: id,
                    style: StrokeStyle::LegacyFixedV1,
                    points: (0..8)
                        .map(|j| SyncPoint::new(300.0 + 500.0 * j as f32, y, None))
                        .collect(),
                },
            },
        };
        let bytes = env.to_canonical_bytes().unwrap();
        g.inbox.push(inbound::InboxItem {
            server_seq: seq,
            envelope: env,
            bytes,
        });
    }
}

fn apply_all(s: &OutboundSync) {
    while s.inbound_ready() {
        s.apply_inbound().unwrap();
    }
    assert!(s.fault().is_none(), "{:?}", s.fault());
}

struct Fixture {
    e: Env,
    ids: (LibraryId, NotebookId, PageId),
    /// Confirmed-log records after the checkpoint (0 = genesis checkpoint only).
    records: usize,
}

/// A library whose next start is a verified fast path. With `records > 0` the confirmed log
/// holds that many foreign operations after the checkpoint (so the deferred replay and its
/// dirty-page set are exercised). Never checkpoints automatically.
fn verified_fixture(label: &str, records: usize) -> Fixture {
    let e = env(label);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    s.set_checkpoint_policy(CheckpointPolicy {
        max_ops: u64::MAX,
        max_log_bytes: u64::MAX,
        max_age: Duration::from_secs(3600),
        idle: Duration::from_secs(3600),
    });
    let ids = first_ids(&s);
    if records > 0 {
        let replica = ReplicaId::from_uuid(Uuid::now_v7()).unwrap();
        let mut client = 0;
        stage_foreign(&s, ids, replica, &mut client, records);
        apply_all(&s);
        stop(s);
        // The foreign materialization invalidated the stamp: one full start re-establishes it.
        let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
        assert!(s.fault().is_none(), "{:?}", s.fault());
        stop(s);
    } else {
        stop(s);
    }
    let _ = take_last_line();
    Fixture { e, ids, records }
}

/// Everything a start establishes that the rest of sync depends on.
#[derive(Debug, PartialEq)]
struct Established {
    base: LibraryState,
    checkpoint_through: u64,
    page_hashes: HashMap<PageId, Sha256Hex>,
    dirty_pages: std::collections::BTreeSet<PageId>,
    log_records_after: u64,
    next_seq: u64,
    seq_ready: bool,
    cursor: u64,
    acked: u64,
}

fn established(s: &OutboundSync) -> Established {
    let g = s.shared.lock().unwrap();
    Established {
        base: g.base.clone().expect("base published"),
        checkpoint_through: g.checkpoint_through,
        page_hashes: g.page_hashes.clone(),
        dirty_pages: g.dirty_pages.iter().copied().collect(),
        log_records_after: g
            .log
            .as_ref()
            .expect("log open")
            .records_after(g.checkpoint_through),
        next_seq: g.next_seq,
        seq_ready: g.seq_ready,
        cursor: g.state.server_cursor,
        acked: g.state.acked_client_seq,
    }
}

fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else if p.file_name().is_some_and(|n| n == LOCK_FILE) {
                // The exclusive lock file has no content, and a live owner's lock makes it
                // unreadable on Windows.
                continue;
            } else {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.insert(rel, fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn outbox_committed(sync: &Path) -> usize {
    let raw = fs::read(sync.join(OUTBOX_FILE)).unwrap_or_default();
    parse_journal(&raw).unwrap().committed.len()
}

fn first_page_file(sync: &Path) -> std::path::PathBuf {
    let mut v: Vec<_> = fs::read_dir(sync.join(CHECKPOINT_DIR).join(CHECKPOINT_PAGES))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    v.sort();
    v.remove(0)
}

fn assert_synchronous(line: &str, s: &OutboundSync, reason: &str) {
    assert_eq!(field(line, "defer"), Some(reason), "{line}");
    assert!(
        s.base_ready(),
        "a synchronous start returns with the base loaded"
    );
    assert_ne!(field(line, "checkpoint_ms"), Some("-"), "{line}");
}

// ------------------------------------------------------------------ selection + equivalence

#[test]
fn verified_start_defers_and_publishes_exactly_the_synchronous_base() {
    let f = verified_fixture("defer-equiv", 3);
    let gate = released_gate();
    let (s, line) = open_deferred(&f.e, Some(gate.clone()));
    assert_eq!(field(&line, "path"), Some("verified_fast_path"), "{line}");
    assert_eq!(field(&line, "defer"), Some("selected"), "{line}");
    // The page phase did not run in the foreground; only the manifest was read.
    assert_eq!(field(&line, "ckpt_pages"), Some("-"), "{line}");
    assert_eq!(field(&line, "replay_ms"), Some("-"), "{line}");
    assert!(s.capture_ready(), "capture is ready when open returns");
    assert!(gate.wait_finished(), "the loader published");
    assert!(s.base_ready());
    assert!(s.fault().is_none(), "{:?}", s.fault());
    let deferred = established(&s);
    assert_eq!(deferred.log_records_after, f.records as u64);
    assert!(!deferred.dirty_pages.is_empty(), "replayed pages are dirty");
    stop(s);

    let s = OutboundSync::open(&f.e.lib, &f.e.sync).unwrap();
    let line = take_last_line().unwrap();
    assert_eq!(field(&line, "path"), Some("verified_fast_path"));
    assert_eq!(field(&line, "defer"), Some("not_requested"));
    let synchronous = established(&s);
    stop(s);
    assert_eq!(deferred, synchronous);
}

#[test]
fn the_rollback_switch_keeps_every_start_synchronous() {
    let f = verified_fixture("defer-off", 1);
    let s = OutboundSync::open_with(
        &f.e.lib,
        &f.e.sync,
        SyncOpenOptions {
            defer_confirmed_base: false,
            ..Default::default()
        },
    )
    .unwrap();
    let line = take_last_line().unwrap();
    assert_synchronous(&line, &s, "not_requested");
    stop(s);
}

// ------------------------------------------------------------------ ineligible starts

#[test]
fn dirty_start_stays_synchronous() {
    let f = verified_fixture("defer-dirty", 0);
    super::tests::append_stroke(&f.e.lib, "notebook-1", "page-1", &[(5.0, 5.0), (9.0, 9.0)]);
    let (s, line) = open_deferred(&f.e, None);
    let reason = field(&line, "defer").unwrap().to_string();
    assert!(
        reason.ends_with("_fallback"),
        "refused by the stamp check: {line}"
    );
    assert_synchronous(&line, &s, &reason);
    stop(s);
}

#[test]
fn missing_or_invalid_stamp_stays_synchronous() {
    let f = verified_fixture("defer-stamp", 0);
    let stamp = f.e.sync.join(crate::sync_verified::STAMP_FILE);
    fs::write(&stamp, b"{ not a stamp").unwrap();
    let (s, line) = open_deferred(&f.e, None);
    assert_synchronous(&line, &s, "corrupt_stamp_fallback");
    stop(s); // the full start rewrote a valid stamp
    fs::remove_file(&stamp).unwrap();
    let (s, line) = open_deferred(&f.e, None);
    assert_synchronous(&line, &s, "missing_stamp_fallback");
    stop(s);
}

#[test]
fn legacy_base_state_stays_synchronous() {
    let f = verified_fixture("defer-legacy", 0);
    fs::write(f.e.sync.join(BASE_FILE), b"{}").unwrap();
    let (s, line) = open_deferred(&f.e, None);
    assert_synchronous(&line, &s, "legacy_base");
    assert!(s.fault().is_none(), "{:?}", s.fault());
    stop(s);
}

#[test]
fn pending_apply_marker_stays_synchronous_and_recovers() {
    let f = verified_fixture("defer-marker", 0);
    let s = OutboundSync::open(&f.e.lib, &f.e.sync).unwrap();
    let replica = ReplicaId::from_uuid(Uuid::now_v7()).unwrap();
    let mut client = 0;
    stage_foreign(&s, f.ids, replica, &mut client, 1);
    s.set_crash_point(Some("after_marker"));
    let _ = s.apply_inbound();
    assert!(
        s.fault().unwrap().contains("after_marker"),
        "{:?}",
        s.fault()
    );
    drop(s); // process death mid-apply
    assert!(inbound::read_marker(&f.e.sync).unwrap().is_some());
    let (s, line) = open_deferred(&f.e, None);
    assert_synchronous(&line, &s, "apply_marker");
    assert_eq!(
        field(&line, "path"),
        Some("recovery_marker_fallback"),
        "{line}"
    );
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert!(
        inbound::read_marker(&f.e.sync).unwrap().is_none(),
        "recovered"
    );
    stop(s);
}

#[test]
fn unresolved_write_ahead_stays_synchronous() {
    let f = verified_fixture("defer-prep", 0);
    let s = OutboundSync::open(&f.e.lib, &f.e.sync).unwrap();
    let t = s.prepare_rename_notebook("notebook-1", "Never").unwrap();
    drop(t); // crash before the local rename
    drop(s);
    let (s, line) = open_deferred(&f.e, None);
    assert_synchronous(&line, &s, "pending_prep");
    assert_eq!(
        field(&line, "path"),
        Some("pending_prep_fallback"),
        "{line}"
    );
    assert!(s.fault().is_none(), "{:?}", s.fault());
    stop(s);
}

#[test]
fn confirmed_log_ahead_of_the_cursor_recovers_synchronously() {
    let f = verified_fixture("defer-cursor", 2);
    // Disposable fixture: the crash window "log persisted, cursor not yet".
    let state_path = f.e.sync.join(STATE_FILE);
    let mut st: SyncStateV1 = read_json(&state_path).unwrap();
    let cursor = st.server_cursor;
    st.server_cursor -= 1;
    write_json(&f.e.sync, STATE_FILE, &st).unwrap();
    let (s, line) = open_deferred(&f.e, None);
    assert_synchronous(&line, &s, "confirmed_log_cursor");
    assert_eq!(
        s.state().server_cursor,
        cursor,
        "cursor adopted from the log"
    );
    stop(s);
}

// ------------------------------------------------------------------ while the base loads

#[test]
fn capture_write_ahead_and_journal_durability_work_while_loading() {
    let f = verified_fixture("defer-capture", 2);
    let gate = Arc::new(LoaderGate::default());
    let (s, line) = open_deferred(&f.e, Some(gate.clone()));
    assert_eq!(field(&line, "defer"), Some("selected"), "{line}");
    gate.wait_paused();
    assert!(!s.base_ready());
    assert!(s.capture_ready());
    // Post-commit capture: durable in the outbox while the base is still loading.
    write(
        &s,
        &f.e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    wait_until("stroke journaled", T, || outbox_committed(&f.e.sync) == 1);
    // Write-ahead: the intent record is accepted (sequence authority is ready) and resolves.
    let t = s
        .prepare_rename_notebook("notebook-1", "Never")
        .expect("write-ahead accepted while loading");
    s.abort(t);
    write(
        &s,
        &f.e.lib,
        "notebook-1",
        "page-1",
        &[(50.0, 50.0), (60.0, 60.0)],
    );
    wait_until("second stroke journaled", T, || {
        outbox_committed(&f.e.sync) == 2
    });
    assert!(!s.base_ready(), "still loading");
    gate.release();
    assert!(gate.wait_finished());
    assert!(s.fault().is_none(), "{:?}", s.fault());
    // The journal thread queues a capture in memory only after it is durable on disk, so the
    // file can briefly be ahead of `pending()`.
    wait_until("both captures queued", T, || s.pending() == 2);
    stop(s);
    // The next (full) start proves both captures match the library exactly.
    let s = OutboundSync::open(&f.e.lib, &f.e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 2);
    stop(s);
}

#[test]
fn no_partial_base_is_visible_while_loading() {
    let f = verified_fixture("defer-partial", 2);
    let gate = Arc::new(LoaderGate::default());
    let (s, _) = open_deferred(&f.e, Some(gate.clone()));
    gate.wait_paused(); // pages already materialized inside the loader, replay pending
    {
        let g = s.shared.lock().unwrap(); // the loader holds no lock while paused mid-load
        assert!(g.base.is_none());
        assert!(g.log.is_none());
        assert_eq!(g.checkpoint_through, 0);
        assert!(g.page_hashes.is_empty());
        assert!(g.dirty_pages.is_empty());
    }
    gate.release();
    assert!(gate.wait_finished());
    let g = s.shared.lock().unwrap();
    assert!(g.base.is_some() && g.log.is_some() && !g.page_hashes.is_empty());
    drop(g);
    stop(s);
}

#[test]
fn inbound_apply_is_refused_until_ready() {
    let f = verified_fixture("defer-inbound", 0);
    let gate = Arc::new(LoaderGate::default());
    let (s, _) = open_deferred(&f.e, Some(gate.clone()));
    gate.wait_paused();
    let replica = ReplicaId::from_uuid(Uuid::now_v7()).unwrap();
    let mut client = 0;
    let cursor = s.state().server_cursor;
    stage_foreign(&s, f.ids, replica, &mut client, 2);
    assert!(!s.inbound_ready(), "staged, but not appliable yet");
    assert!(
        s.apply_inbound().unwrap().is_none(),
        "the execution boundary refuses"
    );
    assert_eq!(s.state().server_cursor, cursor);
    assert!(s.fault().is_none(), "{:?}", s.fault());
    gate.release();
    assert!(gate.wait_finished());
    assert!(s.inbound_ready());
    apply_all(&s);
    assert_eq!(s.state().server_cursor, cursor + 2);
    stop(s);
}

#[test]
fn checkpoint_maintenance_is_refused_until_ready() {
    let f = verified_fixture("defer-ckpt", 2);
    let gate = Arc::new(LoaderGate::default());
    let (s, _) = open_deferred(&f.e, Some(gate.clone()));
    gate.wait_paused();
    assert!(
        !s.checkpoint_now().unwrap(),
        "no checkpoint before the base exists"
    );
    gate.release();
    assert!(gate.wait_finished());
    assert!(
        s.checkpoint_now().unwrap(),
        "the replayed records are checkpointed"
    );
    stop(s);
}

#[test]
fn upload_is_refused_until_ready() {
    let srv = server("defer-upload");
    let e = env("defer-upload");
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    wait_until("genesis", T, || s.state().genesis_uploaded);
    stop(s);
    let _ = take_last_line();
    let gate = Arc::new(LoaderGate::default());
    let (s, line) = open_deferred(&e, Some(gate.clone()));
    assert_eq!(field(&line, "defer"), Some("selected"), "{line}");
    gate.wait_paused();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    wait_until("journaled", T, || outbox_committed(&e.sync) == 1);
    s.kick();
    // Negative check (bounded): the uploader stays idle while the base loads.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        s.state().acked_client_seq,
        0,
        "nothing uploaded before Ready"
    );
    assert_eq!(s.stats().uploaded, 0);
    gate.release();
    assert!(gate.wait_finished());
    wait_until("uploaded after Ready", T, || {
        s.state().acked_client_seq == 1
    });
    stop(s);
}

// ------------------------------------------------------------------ faults

#[test]
fn a_corrupt_checkpoint_page_faults_and_capture_continues() {
    let f = verified_fixture("defer-corrupt", 1);
    let victim = first_page_file(&f.e.sync);
    let mut b = fs::read(&victim).unwrap();
    let last = b.len() - 1;
    b[last] ^= 0x01;
    fs::write(&victim, b).unwrap();
    let gate = released_gate();
    let (s, line) = open_deferred(&f.e, Some(gate.clone()));
    assert_eq!(
        field(&line, "defer"),
        Some("selected"),
        "manifest is fine: {line}"
    );
    assert!(!gate.wait_finished(), "nothing published");
    let fault = s.fault().expect("faulted");
    assert!(fault.contains("deferred confirmed state"), "{fault}");
    assert!(fault.contains("CHECKPOINT_CORRUPT page"), "{fault}");
    assert!(!s.base_ready(), "no unverified base");
    assert!(!s.inbound_ready());
    assert!(!s.checkpoint_now().unwrap());
    // Capture continues and is durable; the fault is not cleared by activity.
    write(
        &s,
        &f.e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    wait_until("journaled despite the fault", T, || {
        outbox_committed(&f.e.sync) == 1
    });
    assert!(s.fault().is_some(), "persistent");
    stop(s);
    // Unchanged fail-closed synchronous behaviour for the same files.
    let err = OutboundSync::open(&f.e.lib, &f.e.sync)
        .err()
        .expect("refused");
    assert!(err.contains("CHECKPOINT_CORRUPT page"), "{err}");
}

#[test]
fn a_confirmed_log_replay_failure_faults() {
    let f = verified_fixture("defer-replay", 2);
    // Disposable fixture: replace the last record with a validly framed, undecodable one.
    let path = f.e.sync.join(CONFIRMED_LOG_FILE);
    let scan = confirmed::scan_log(&path, |_, _| Ok(())).unwrap();
    let (last_seq, _) = *scan.index.last().unwrap();
    let keep = scan.index[scan.index.len() - 2].1;
    let f_ = fs::OpenOptions::new().write(true).open(&path).unwrap();
    f_.set_len(keep).unwrap();
    drop(f_);
    let scan = confirmed::scan_log(&path, |_, _| Ok(())).unwrap();
    let mut log = ConfirmedLog::open(&path, &scan).unwrap();
    log.append(&[(last_seq, b"{\"not\":\"an envelope\"}".as_slice())])
        .unwrap();
    drop(log);
    let gate = released_gate();
    let (s, line) = open_deferred(&f.e, Some(gate.clone()));
    assert_eq!(
        field(&line, "defer"),
        Some("selected"),
        "headers are valid: {line}"
    );
    assert!(!gate.wait_finished());
    let fault = s.fault().expect("faulted");
    assert!(fault.contains("deferred confirmed state"), "{fault}");
    assert!(!s.base_ready());
    stop(s);
}

// ------------------------------------------------------------------ shutdown

#[test]
fn shutdown_while_loading_is_bounded_and_publishes_nothing() {
    let f = verified_fixture("defer-shutdown", 2);
    let (lib0, sync0) = (tree(&f.e.lib), tree(&f.e.sync));
    let gate = Arc::new(LoaderGate::default());
    let (s, _) = open_deferred(&f.e, Some(gate.clone()));
    gate.wait_paused();
    let t = Instant::now();
    s.shutdown(Duration::from_millis(300));
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "bounded by the budget"
    );
    gate.release();
    assert!(!gate.wait_finished(), "no publication after shutdown began");
    assert_eq!(tree(&f.e.lib), lib0, "library untouched");
    assert_eq!(
        tree(&f.e.sync),
        sync0,
        "sync state untouched (the loader never writes)"
    );
    // The sync lock was released with the instance; a new start works normally.
    let (s, line) = open_deferred(&f.e, Some(released_gate()));
    assert_eq!(field(&line, "defer"), Some("selected"), "{line}");
    wait_until("ready", T, || s.base_ready());
    stop(s);
}

// ------------------------------------------------------------------ 19B.2B.1 hardening

/// A configured fixture (real server) whose next start is a verified fast path.
/// Notebook/page ids of `notebook-1/page-1` from the identity map (no base needed).
fn ids_from_identity(e: &Env, s: &OutboundSync) -> (LibraryId, NotebookId, PageId) {
    let identity = crate::sync_local::read_identity(&e.lib).unwrap().unwrap();
    let nb = identity.notebooks["notebook-1"];
    let page = identity.pages["notebook-1"]["page-1"];
    (s.state().library_id, nb, page)
}

fn verified_online_fixture(label: &str) -> (Env, super::tests::Server) {
    let srv = server(label);
    let e = env(label);
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    wait_until("genesis", T, || s.state().genesis_uploaded);
    stop(s);
    let _ = take_last_line();
    (e, srv)
}

#[test]
fn a_loader_panic_mid_load_faults_instead_of_staying_loading() {
    let (e, _srv) = verified_online_fixture("defer-panic-mid");
    let gate = Arc::new(LoaderGate::default());
    gate.panic_mid_load
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (s, line) = open_deferred(&e, Some(gate.clone()));
    assert_eq!(field(&line, "defer"), Some("selected"), "{line}");
    gate.wait_paused();
    gate.release();
    assert!(!gate.wait_finished(), "nothing published after a panic");
    let fault = s.fault().expect("faulted, not stuck loading");
    assert!(fault.contains("loader panicked"), "{fault}");
    assert!(
        fault.contains("injected deferred-loader panic mid-load"),
        "{fault}"
    );
    {
        let g = s.shared.lock().unwrap(); // not poisoned
        assert!(g.base.is_none() && g.log.is_none() && g.page_hashes.is_empty());
    }
    // Inbound apply and checkpointing stay refused.
    let ids = ids_from_identity(&e, &s);
    let replica = ReplicaId::from_uuid(Uuid::now_v7()).unwrap();
    let mut client = 0;
    stage_foreign(&s, ids, replica, &mut client, 1);
    assert!(!s.inbound_ready());
    assert!(s.apply_inbound().unwrap().is_none());
    assert!(!s.checkpoint_now().unwrap());
    // Durable local capture continues; upload stays prohibited (bounded negative check).
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    wait_until("journaled after the fault", T, || {
        outbox_committed(&e.sync) == 1
    });
    s.kick();
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        s.state().acked_client_seq,
        0,
        "nothing uploaded while faulted"
    );
    assert!(s.fault().is_some(), "persistent");
    let t = Instant::now();
    s.shutdown(Duration::from_secs(2));
    assert!(
        t.elapsed() < Duration::from_secs(4),
        "shutdown bounded after a panic"
    );
    // The next start verifies from disk again and works normally.
    let (s, _) = open_deferred(&e, Some(released_gate()));
    wait_until("ready", T, || s.base_ready());
    assert!(s.fault().is_none(), "{:?}", s.fault());
    stop(s);
}

#[test]
fn a_loader_panic_outside_materialization_is_caught_by_the_thread_guard() {
    let f = verified_fixture("defer-panic-outer", 1);
    let gate = released_gate();
    gate.panic_before_publish
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (s, _) = open_deferred(&f.e, Some(gate.clone()));
    assert!(!gate.wait_finished());
    let fault = s.fault().expect("faulted");
    assert!(fault.contains("loader terminated unexpectedly"), "{fault}");
    assert!(!s.base_ready());
    assert!(!s.inbound_ready());
    write(
        &s,
        &f.e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    wait_until("journaled", T, || outbox_committed(&f.e.sync) == 1);
    stop(s);
}

#[test]
fn a_detached_loader_never_recreates_or_writes_sync_files() {
    // Empty confirmed log: if the file disappears under an abandoned loader, nothing in the
    // replay notices — only a non-creating open (and the stop check) keep the loader read-only.
    let f = verified_fixture("defer-detached", 0);
    let gate = Arc::new(LoaderGate::default());
    let (s, _) = open_deferred(&f.e, Some(gate.clone()));
    gate.wait_paused();
    s.shutdown(Duration::from_millis(100)); // abandons the paused loader
    let log = f.e.sync.join(CONFIRMED_LOG_FILE);
    fs::remove_file(&log).unwrap(); // a later owner replaced/removed it
    let sync0 = tree(&f.e.sync);
    gate.release();
    assert!(!gate.wait_finished(), "a stopped loader never publishes");
    assert!(
        !log.exists(),
        "the detached loader did not recreate the log"
    );
    assert_eq!(tree(&f.e.sync), sync0, "no sync file changed");
}

#[test]
fn a_new_instance_starts_while_an_abandoned_loader_is_still_reading() {
    let f = verified_fixture("defer-successor", 2);
    let old_gate = Arc::new(LoaderGate::default());
    let (old, _) = open_deferred(&f.e, Some(old_gate.clone()));
    old_gate.wait_paused();
    old.shutdown(Duration::from_millis(100)); // lock released, loader still paused
    let new_gate = released_gate();
    let (s, line) = open_deferred(&f.e, Some(new_gate.clone()));
    assert_eq!(field(&line, "defer"), Some("selected"), "{line}");
    assert!(new_gate.wait_finished());
    let before = established(&s);
    let sync0 = tree(&f.e.sync);
    old_gate.release();
    assert!(
        !old_gate.wait_finished(),
        "the abandoned loader publishes nothing"
    );
    assert_eq!(established(&s), before, "the successor is unaffected");
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(tree(&f.e.sync), sync0, "the abandoned loader wrote nothing");
    stop(s);
}
