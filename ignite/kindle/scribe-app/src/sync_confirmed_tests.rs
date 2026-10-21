//! Checkpoint + confirmed log: framing, torn tails, fail-closed corruption, sequence order,
//! every crash window of a checkpoint, concurrent arrivals, and idempotent startup replay.
//! Inbound work is injected as staged server items from a foreign replica and applied through
//! the real apply path (`apply_inbound`).

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::confirmed::*;
use super::tests::{env, stop, Env};
use super::*;

/// Stage `n` foreign AddStroke items after the cursor (as the coordinator would).
pub(super) fn stage_remote(s: &OutboundSync, replica: ReplicaId, next_client: &mut u64, n: usize) {
    let mut g = s.shared.lock().unwrap();
    let (library_id, nb, page) = {
        let nbk = &g.base.as_ref().unwrap().snapshot.notebooks[0];
        (g.state.library_id, nbk.notebook_id, nbk.pages[0].page_id)
    };
    let mut seq = g
        .inbox
        .last()
        .map_or(g.state.server_cursor, |i| i.server_seq);
    for _ in 0..n {
        seq += 1;
        *next_client += 1;
        let id = StrokeId::from_uuid(Uuid::now_v7()).unwrap();
        let y = 200.0 + 13.0 * (*next_client % 1000) as f32;
        let env = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: OperationId::from_uuid(Uuid::now_v7()).unwrap(),
            library_id,
            replica_id: replica,
            client_seq: *next_client,
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

pub(super) fn apply_all_staged(s: &OutboundSync) {
    while s.inbound_ready() {
        s.apply_inbound().unwrap();
    }
    assert!(s.fault().is_none(), "{:?}", s.fault());
}

struct Rig {
    e: Env,
    replica: ReplicaId,
    client: u64,
}

impl Rig {
    fn new(label: &str) -> (Self, OutboundSync) {
        let e = env(label);
        let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
        // No automatic checkpoints unless a test asks for one.
        s.set_checkpoint_policy(CheckpointPolicy {
            max_ops: u64::MAX,
            max_log_bytes: u64::MAX,
            max_age: Duration::from_secs(3600),
            idle: Duration::from_secs(3600),
        });
        (
            Self {
                e,
                replica: ReplicaId::from_uuid(Uuid::now_v7()).unwrap(),
                client: 0,
            },
            s,
        )
    }
    fn remote(&mut self, s: &OutboundSync, n: usize) {
        stage_remote(s, self.replica, &mut self.client, n);
        apply_all_staged(s);
    }
    fn reopen(&self, s: OutboundSync) -> OutboundSync {
        stop(s);
        let s = OutboundSync::open(&self.e.lib, &self.e.sync).unwrap();
        s.set_checkpoint_policy(CheckpointPolicy {
            max_ops: u64::MAX,
            max_log_bytes: u64::MAX,
            max_age: Duration::from_secs(3600),
            idle: Duration::from_secs(3600),
        });
        s
    }
    fn log(&self) -> std::path::PathBuf {
        self.e.sync.join(CONFIRMED_LOG_FILE)
    }
}

/// In-memory base == durable (checkpoint + log) == the Kindle files' view of it.
fn assert_consistent(s: &OutboundSync, sync_dir: &Path) {
    // Startup reconciliation compares the whole library with this state: a fault means the
    // durable confirmed state and the Kindle files disagree.
    assert!(s.fault().is_none(), "fault: {:?}", s.fault());
    let g = s.shared.lock().unwrap();
    let (durable, through) = read_confirmed_state(sync_dir, g.state.library_id).unwrap();
    assert_eq!(through, g.state.server_cursor, "durable confirmed position");
    assert!(
        g.base.as_ref() == Some(&durable),
        "durable confirmed state == in-memory ServerBaseState"
    );
}

#[test]
fn no_one_and_many_delta_records_replay_exactly_and_idempotently() {
    let (mut r, s) = Rig::new("ckpt-replay");
    // No delta records: the epoch checkpoint alone.
    assert_eq!(s.confirmed_state(), (0, 0));
    assert_consistent(&s, &r.e.sync);
    let s = r.reopen(s);
    assert_consistent(&s, &r.e.sync);
    // One record.
    r.remote(&s, 1);
    assert_eq!(s.confirmed_state(), (0, 1));
    let s = r.reopen(s);
    assert_consistent(&s, &r.e.sync);
    // A large log.
    r.remote(&s, 2500);
    assert_eq!(s.confirmed_state(), (0, 2501));
    let before = s.shared.lock().unwrap().base.clone();
    let s = r.reopen(s);
    assert!(s.shared.lock().unwrap().base == before);
    // Repeated startup changes nothing.
    let log = fs::read(r.log()).unwrap();
    let s = r.reopen(s);
    assert_eq!(fs::read(r.log()).unwrap(), log);
    assert!(s.shared.lock().unwrap().base == before);
    assert_consistent(&s, &r.e.sync);
    stop(s);
}

#[test]
fn a_torn_final_record_is_ignored_and_truncated() {
    for cut in ["short_header", "short_payload", "bad_checksum_at_end"] {
        let (mut r, s) = Rig::new("ckpt-torn");
        r.remote(&s, 5);
        stop(s);
        // Simulate a power cut while appending record 6.
        let good = fs::read(r.log()).unwrap();
        let rec = frame(6, b"{\"not\":\"durable\"}");
        let mut bytes = good.clone();
        match cut {
            "short_header" => bytes.extend_from_slice(&rec[..7]),
            "short_payload" => bytes.extend_from_slice(&rec[..rec.len() - 3]),
            _ => {
                let mut bad = rec.clone();
                *bad.last_mut().unwrap() ^= 0xFF;
                bytes.extend_from_slice(&bad);
            }
        }
        fs::write(r.log(), &bytes).unwrap();
        let s = OutboundSync::open(&r.e.lib, &r.e.sync).unwrap();
        assert!(s.fault().is_none(), "{cut}: {:?}", s.fault());
        assert_eq!(s.state().server_cursor, 5, "{cut}");
        assert_eq!(
            fs::read(r.log()).unwrap(),
            good,
            "{cut}: only the torn tail removed"
        );
        // …and appending continues cleanly after it.
        r.remote(&s, 2);
        let s = r.reopen(s);
        assert_eq!(s.state().server_cursor, 7);
        assert_consistent(&s, &r.e.sync);
        stop(s);
    }
}

#[test]
fn corruption_inside_durable_history_fails_closed() {
    let (mut r, s) = Rig::new("ckpt-corrupt");
    r.remote(&s, 6);
    stop(s);
    let good = fs::read(r.log()).unwrap();
    // Flip one payload byte of the 2nd record (records follow it).
    let mut bytes = good.clone();
    let first_len = 20 + u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    bytes[first_len + 20 + 10] ^= 0x01;
    fs::write(r.log(), &bytes).unwrap();
    let err = OutboundSync::open(&r.e.lib, &r.e.sync)
        .err()
        .expect("refused");
    assert!(err.contains("CONFIRMED_LOG_CORRUPT"), "{err}");
    // A bad magic mid-file too.
    let mut bytes = good.clone();
    bytes[first_len] = b'X';
    fs::write(r.log(), &bytes).unwrap();
    let err = OutboundSync::open(&r.e.lib, &r.e.sync)
        .err()
        .expect("refused");
    assert!(err.contains("CONFIRMED_LOG_CORRUPT"), "{err}");
    // Nothing was silently repaired.
    assert_eq!(fs::read(r.log()).unwrap(), bytes);
}

#[test]
fn server_sequence_order_is_enforced_on_replay() {
    let (mut r, s) = Rig::new("ckpt-order");
    r.remote(&s, 4);
    stop(s);
    let good = fs::read(r.log()).unwrap();
    // Re-frame the records with a gap (1,2,4,5) and with a duplicate (1,2,2,3).
    let mut payloads = Vec::new();
    scan_log(&r.log(), |_, p| {
        payloads.push(p.to_vec());
        Ok(())
    })
    .unwrap();
    for seqs in [[1u64, 2, 4, 5], [1, 2, 2, 3]] {
        let mut bytes = Vec::new();
        for (seq, p) in seqs.iter().zip(&payloads) {
            bytes.extend(frame(*seq, p));
        }
        fs::write(r.log(), &bytes).unwrap();
        let err = OutboundSync::open(&r.e.lib, &r.e.sync)
            .err()
            .expect("refused");
        assert!(err.contains("CONFIRMED_LOG_CORRUPT"), "{seqs:?}: {err}");
    }
    fs::write(r.log(), good).unwrap();
    let s = OutboundSync::open(&r.e.lib, &r.e.sync).unwrap();
    assert!(s.fault().is_none());
    stop(s);
}

#[test]
fn a_checkpoint_retires_only_what_it_covers_and_old_checkpoint_plus_new_log_replays() {
    let (mut r, s) = Rig::new("ckpt-basic");
    r.remote(&s, 40);
    assert!(s.checkpoint_now().unwrap());
    assert_eq!(s.confirmed_state(), (40, 0));
    assert_eq!(
        fs::metadata(r.log()).unwrap().len(),
        0,
        "log retired through 40"
    );
    r.remote(&s, 10);
    assert_eq!(s.confirmed_state(), (40, 10));
    let before = s.shared.lock().unwrap().base.clone();
    // Old checkpoint (through 40) + new log (41..=50).
    let s = r.reopen(s);
    assert!(s.shared.lock().unwrap().base == before);
    assert_consistent(&s, &r.e.sync);
    // Only changed pages are written by the next checkpoint.
    let pages_before = fs::read_dir(r.e.sync.join(CHECKPOINT_DIR).join(CHECKPOINT_PAGES))
        .unwrap()
        .count();
    assert!(s.checkpoint_now().unwrap());
    let pages_after = fs::read_dir(r.e.sync.join(CHECKPOINT_DIR).join(CHECKPOINT_PAGES))
        .unwrap()
        .count();
    assert_eq!(
        pages_after, pages_before,
        "one page replaced, old file collected"
    );
    stop(s);
}

/// Every crash window of checkpoint promotion: the next start must reproduce exactly the state
/// before the crash, from the previous (or the new) checkpoint + the log.
#[test]
fn every_checkpoint_crash_window_recovers_exactly() {
    for point in [
        "checkpoint_after_pages", // serialization done, manifest not written
        "checkpoint_before_manifest_fsync", // manifest temp written, not fsynced
        "checkpoint_before_promotion", // fsynced, not renamed
        "checkpoint_after_promotion", // promoted, log not compacted
        "confirmed_log_compaction_written", // compaction temp written, not renamed
    ] {
        let (mut r, s) = Rig::new("ckpt-crash");
        r.remote(&s, 30);
        assert!(s.checkpoint_now().unwrap(), "{point}: first checkpoint");
        r.remote(&s, 25);
        let before = s.shared.lock().unwrap().base.clone();
        s.shared.lock().unwrap().crash_at = Some(point.to_string());
        let res = s.checkpoint_now();
        assert!(res.is_err(), "{point}: simulated crash");
        // "Power loss": no cleanup, straight to the next start.
        drop(s);
        let s = OutboundSync::open(&r.e.lib, &r.e.sync).unwrap();
        assert!(s.fault().is_none(), "{point}: {:?}", s.fault());
        assert_eq!(s.state().server_cursor, 55, "{point}");
        assert!(
            s.shared.lock().unwrap().base == before,
            "{point}: exact state"
        );
        let promoted = matches!(
            point,
            "checkpoint_after_promotion" | "confirmed_log_compaction_written"
        );
        assert_eq!(
            s.confirmed_state().0,
            if promoted { 55 } else { 30 },
            "{point}: checkpoint in use"
        );
        // Recovery is idempotent and sync continues.
        r.remote(&s, 3);
        let s = r.reopen(s);
        assert_eq!(s.state().server_cursor, 58);
        assert_consistent(&s, &r.e.sync);
        assert!(s.checkpoint_now().unwrap());
        let s = r.reopen(s);
        assert_consistent(&s, &r.e.sync);
        stop(s);
    }
}

/// Confirmed ops keep arriving (and are applied) while a checkpoint is being written; the
/// checkpoint installs exactly its boundary and compaction keeps everything after it.
#[test]
fn arrivals_continue_while_a_checkpoint_is_written() {
    let (mut r, s) = Rig::new("ckpt-concurrent");
    r.remote(&s, 200);
    let gate = Arc::new(JournalGate::default());
    gate.close();
    s.shared.lock().unwrap().checkpoint_gate = Some(Arc::clone(&gate));
    let shared = Arc::clone(&s.shared);
    let writer = std::thread::spawn(move || run_checkpoint(&shared, true));
    gate.wait_held(); // captured through 200; now "writing"
                      // Inbound sync is not blocked by the checkpoint: 150 more ops apply meanwhile.
    let t = Instant::now();
    r.remote(&s, 150);
    let during = t.elapsed();
    assert_eq!(s.state().server_cursor, 350);
    gate.open();
    assert_eq!(writer.join().unwrap().unwrap(), Some(200));
    s.shared.lock().unwrap().checkpoint_gate = None;
    assert_eq!(s.confirmed_state(), (200, 150), "201..=350 stay in the log");
    println!(
        "150 ops applied during a held checkpoint in {} ms",
        during.as_millis()
    );
    let before = s.shared.lock().unwrap().base.clone();
    let s = r.reopen(s);
    assert!(s.shared.lock().unwrap().base == before);
    assert_consistent(&s, &r.e.sync);
    stop(s);
}

#[test]
fn the_policy_triggers_on_the_first_threshold_reached() {
    let p = CheckpointPolicy {
        max_ops: 100,
        max_log_bytes: 1000,
        max_age: Duration::from_secs(600),
        idle: Duration::from_secs(90),
    };
    let z = Duration::ZERO;
    assert_eq!(
        p.due(
            0,
            10_000,
            Duration::from_secs(9999),
            Duration::from_secs(9999)
        ),
        None
    );
    assert_eq!(p.due(100, 0, z, z), Some("ops"));
    assert_eq!(p.due(1, 1000, z, z), Some("log_bytes"));
    assert_eq!(p.due(1, 0, Duration::from_secs(600), z), Some("age"));
    assert_eq!(p.due(1, 0, z, Duration::from_secs(90)), Some("idle"));
    assert_eq!(p.due(1, 0, z, z), None);
    // The background thread honours it.
    let (mut r, s) = Rig::new("ckpt-policy");
    s.set_checkpoint_policy(CheckpointPolicy { max_ops: 20, ..p });
    r.remote(&s, 25);
    let t = Instant::now();
    while s.confirmed_state().0 == 0 {
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "no automatic checkpoint"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(s.confirmed_state(), (25, 0));
    stop(s);
}

#[test]
fn a_legacy_base_state_is_converted_once_and_removed_only_after_promotion() {
    let (mut r, s) = Rig::new("ckpt-legacy");
    r.remote(&s, 12);
    let before = s.shared.lock().unwrap().base.clone();
    let cursor = s.state().server_cursor;
    stop(s);
    // Rewind the sync dir to the pre-checkpoint layout: base-state.json only.
    write_json(
        &r.e.sync,
        BASE_FILE,
        &BaseStateV2 {
            v: 2,
            through_server_seq: cursor,
            snapshot: before.as_ref().unwrap().snapshot.clone(),
            history: before.as_ref().unwrap().history.clone(),
        },
    )
    .unwrap();
    fs::remove_dir_all(r.e.sync.join(CHECKPOINT_DIR)).unwrap();
    fs::write(r.log(), b"").unwrap();
    let s = OutboundSync::open(&r.e.lib, &r.e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert!(s.shared.lock().unwrap().base == before);
    assert!(
        !r.e.sync.join(BASE_FILE).exists(),
        "legacy file retired after promotion"
    );
    assert_eq!(s.confirmed_state(), (12, 0));
    assert_consistent(&s, &r.e.sync);
    stop(s);
}

/// Receiving remote strokes writes work proportional to them: the log grows by the records,
/// no full-state file is rewritten.
#[test]
fn an_inbound_transaction_writes_only_its_records() {
    let (mut r, s) = Rig::new("ckpt-bytes");
    r.remote(&s, 300);
    assert!(s.checkpoint_now().unwrap());
    let manifest = r.e.sync.join(CHECKPOINT_DIR).join(CHECKPOINT_MANIFEST);
    let m0 = fs::metadata(&manifest).unwrap().modified().unwrap();
    let log0 = fs::metadata(r.log()).unwrap().len();
    let b0 = s.stats().confirmed_log_bytes;
    r.remote(&s, 1);
    let one = s.stats().confirmed_log_bytes - b0;
    assert_eq!(fs::metadata(r.log()).unwrap().len() - log0, one);
    assert!(one < 4096, "one stroke appended {one} bytes");
    assert_eq!(
        fs::metadata(&manifest).unwrap().modified().unwrap(),
        m0,
        "no checkpoint rewrite"
    );
    stop(s);
}

/// The device bug of 2026-09-27: log records replayed at startup changed a page, the next
/// checkpoint (in that process) treated the page as unchanged and referenced its old file,
/// then retired the records — losing them from the confirmed state. Startup reconciliation
/// caught it (fail closed). Replayed pages must be rewritten by the next checkpoint.
#[test]
fn a_checkpoint_after_startup_replay_keeps_the_replayed_changes() {
    let (mut r, s) = Rig::new("ckpt-replay-dirty");
    r.remote(&s, 20);
    assert!(s.checkpoint_now().unwrap()); // checkpoint through 20
    r.remote(&s, 10); // log: 21..=30, applied live
    let before = s.shared.lock().unwrap().base.clone();
    let s = r.reopen(s); // replays 21..=30 at startup
    assert_eq!(s.confirmed_state(), (20, 10));
    assert!(
        s.checkpoint_now().unwrap(),
        "checkpoint in the replaying process"
    );
    assert_eq!(s.confirmed_state(), (30, 0));
    let s = r.reopen(s); // checkpoint only, no log
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert!(
        s.shared.lock().unwrap().base == before,
        "replayed strokes kept"
    );
    assert_consistent(&s, &r.e.sync);
    stop(s);
}

/// One-off repair tool (manual, never in product code paths): rewrite checkpoint pages that
/// differ from the server's canonical snapshot **at exactly the checkpoint's server_seq**.
/// `SCRIBE_REPAIR_SYNC=<sync dir copy>` `SCRIBE_REPAIR_SERVER=<file: {head}\n@@SNAPSHOT@@\n{snapshot}>`.
/// Refuses unless: server head == checkpoint through, structure identical, only page content
/// differs, and the repaired state validates. Then dry-runs startup on the copy's library.
#[test]
#[ignore = "manual repair tool: needs SCRIBE_REPAIR_SYNC + SCRIBE_REPAIR_SERVER"]
fn repair_checkpoint_pages_from_server_snapshot() {
    let dir = std::path::PathBuf::from(std::env::var("SCRIBE_REPAIR_SYNC").unwrap());
    let raw = fs::read_to_string(std::env::var("SCRIBE_REPAIR_SERVER").unwrap()).unwrap();
    let (head, snap) = raw.split_once("@@SNAPSHOT@@").unwrap();
    let head: serde_json::Value = serde_json::from_str(head.trim()).unwrap();
    let v: serde_json::Value = serde_json::from_str(snap.trim()).unwrap();
    let server: SyncSnapshotV1 =
        serde_json::from_value(v.get("snapshot").cloned().unwrap_or(v)).unwrap();
    let (mut state, through, hashes) = load_checkpoint(&dir).unwrap().unwrap();
    assert_eq!(
        head["head_server_seq"].as_u64(),
        Some(through),
        "server head != checkpoint"
    );
    assert_eq!(
        sync_protocol::snapshot_sha256(&server).unwrap().as_str(),
        head["snapshot_sha256"].as_str().unwrap(),
        "server snapshot bytes do not match the head's hash"
    );
    assert_eq!(
        structure_of(&state.snapshot),
        structure_of(&server),
        "structure differs"
    );
    let mut dirty = HashSet::new();
    for (n, sn) in state.snapshot.notebooks.iter_mut().zip(&server.notebooks) {
        for (p, sp) in n.pages.iter_mut().zip(&sn.pages) {
            if p != sp {
                println!(
                    "repair page {}: {} -> {} strokes",
                    p.page_id,
                    p.strokes.len(),
                    sp.strokes.len()
                );
                *p = sp.clone();
                dirty.insert(p.page_id);
            }
        }
    }
    assert!(state.snapshot == server);
    state
        .validate()
        .expect("repaired state validates with the checkpoint history");
    let cap = CheckpointCapture::capture(&state, through, &hashes, &dirty);
    let w = write_checkpoint(&dir, &cap, &|_: &str| Ok(())).unwrap();
    collect_unreferenced_pages(&dir, &w.page_hashes).unwrap();
    println!(
        "repaired checkpoint through {through}: pages_written={} bytes={}",
        w.pages_written, w.bytes_written
    );
    if let Ok(lib) = std::env::var("SCRIBE_REPAIR_LIBRARY") {
        let s = OutboundSync::open(std::path::Path::new(&lib), &dir).unwrap();
        println!(
            "dry-run startup fault={:?} pending={}",
            s.fault(),
            s.pending()
        );
        assert!(s.fault().is_none());
        assert_eq!(s.pending(), 0, "no derived operations");
        stop(s);
    }
}

#[test]
fn checkpoints_keep_folders_and_memberships() {
    use sync_engine::testkit::{fd, genesis, nb, replica, Author};
    let mut a = Author::new(replica(3));
    let mut state = LibraryState::genesis(genesis()).unwrap();
    for op in [
        SyncOperationV1::CreateFolder {
            folder_id: fd(1),
            title: "Journals".into(),
        },
        SyncOperationV1::CreateFolder {
            folder_id: fd(2),
            title: "Gone".into(),
        },
        SyncOperationV1::SetNotebookFolder {
            notebook_id: nb(2),
            folder_id: Some(fd(1)),
        },
        SyncOperationV1::DeleteFolder { folder_id: fd(2) },
    ] {
        state = state.apply(&a.env(op)).unwrap().0;
    }
    let dir = std::env::temp_dir().join(format!("scribe-ckpt-folders-{}", Uuid::now_v7().simple()));
    fs::create_dir_all(&dir).unwrap();
    let cap = CheckpointCapture::borrow_all(&state, 4);
    write_checkpoint(&dir, &cap, &|_: &str| Ok(())).unwrap();
    let (loaded, through, _) = load_checkpoint(&dir).unwrap().unwrap();
    assert_eq!(through, 4);
    assert_eq!(
        loaded, state,
        "folders, memberships and deleted folders round-trip"
    );

    // A library without folders keeps the pre-folder manifest bytes (no new keys).
    let plain = LibraryState::genesis(genesis()).unwrap();
    let dir2 = dir.join("plain");
    fs::create_dir_all(&dir2).unwrap();
    write_checkpoint(
        &dir2,
        &CheckpointCapture::borrow_all(&plain, 1),
        &|_: &str| Ok(()),
    )
    .unwrap();
    let manifest = fs::read_dir(dir2.join(CHECKPOINT_DIR))
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.path().is_file())
        .map(|e| fs::read_to_string(e.path()).unwrap())
        .unwrap();
    assert!(!manifest.contains("folder"), "{manifest}");
    let _ = fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ Slice 19B.2A: head / pages seam

/// A rig whose promoted checkpoint holds real pages (genesis + remote work), stopped so its
/// sync dir can be inspected and damaged freely.
fn checkpointed_rig(label: &str) -> Rig {
    let (mut rig, s) = Rig::new(label);
    rig.remote(&s, 3);
    assert!(s.checkpoint_now().unwrap(), "a checkpoint is promoted");
    stop(s);
    rig
}

fn page_files(sync_dir: &Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = fs::read_dir(sync_dir.join(CHECKPOINT_DIR).join(CHECKPOINT_PAGES))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    v.sort();
    v
}

#[test]
fn head_then_materialize_is_exactly_load_checkpoint() {
    let rig = checkpointed_rig("head-equiv");
    let sync = &rig.e.sync;
    let (state, through, hashes) = load_checkpoint(sync).unwrap().unwrap();
    let head = read_checkpoint_head(sync).unwrap().unwrap();
    assert_eq!(head.through(), through);
    let (state2, through2, hashes2) = materialize_checkpoint(sync, head).unwrap();
    assert!(state2 == state, "same ServerBaseState");
    assert_eq!(through2, through);
    assert_eq!(hashes2, hashes);
    assert_eq!(hashes.len(), page_files(sync).len());
}

#[test]
fn no_checkpoint_has_no_head() {
    let dir = std::env::temp_dir().join(format!("scribe-head-none-{}", Uuid::now_v7()));
    fs::create_dir_all(&dir).unwrap();
    assert!(read_checkpoint_head(&dir).unwrap().is_none());
    assert!(load_checkpoint(&dir).unwrap().is_none());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_valid_head_is_no_proof_of_page_contents() {
    // Corrupt page bytes (address mismatch) and a missing page file both pass the manifest
    // phase and fail only when the pages are materialized — with load_checkpoint's error.
    for damage in ["flip", "remove"] {
        let rig = checkpointed_rig(&format!("head-{damage}"));
        let sync = &rig.e.sync;
        let victim = page_files(sync).into_iter().next().unwrap();
        match damage {
            "flip" => {
                let mut b = fs::read(&victim).unwrap();
                let last = b.len() - 1;
                b[last] ^= 0x01;
                fs::write(&victim, b).unwrap();
            }
            _ => fs::remove_file(&victim).unwrap(),
        }
        let head = read_checkpoint_head(sync)
            .unwrap()
            .expect("manifest itself is intact");
        let err = materialize_checkpoint(sync, head)
            .err()
            .expect("pages are refused");
        assert!(err.contains("CHECKPOINT_CORRUPT page"), "{damage}: {err}");
        assert_eq!(
            load_checkpoint(sync).err().unwrap(),
            err,
            "{damage}: same error"
        );
        // Unchanged startup behaviour: open refuses the corrupt confirmed state.
        let open_err = OutboundSync::open(&rig.e.lib, sync)
            .err()
            .expect("open refuses");
        assert!(
            open_err.contains("CHECKPOINT_CORRUPT page"),
            "{damage}: {open_err}"
        );
    }
}

#[test]
fn a_corrupt_manifest_fails_at_the_head() {
    let rig = checkpointed_rig("head-manifest");
    let sync = &rig.e.sync;
    fs::write(
        sync.join(CHECKPOINT_DIR).join(CHECKPOINT_MANIFEST),
        b"{ not json",
    )
    .unwrap();
    let err = read_checkpoint_head(sync).err().expect("refused");
    assert!(err.starts_with("CHECKPOINT_CORRUPT"), "{err}");
    assert_eq!(load_checkpoint(sync).err().unwrap(), err);
}
