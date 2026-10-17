//! Slice 18E.4B: capture continues without a server and while upload is faulted.
//! Synthetic libraries only. The historical uncaptured-erase failure is reproduced, then the
//! corrected write-ahead path is proven (exact fragment IDs, no upload from divergence).

use std::fs;
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use notebook_core::PagePoint;
use sync_protocol::snapshot_sha256;

use super::tests::{
    env, local_snapshot, nb_dir, pending_seqs, replay_local, server, stop, synced_erase,
    wait_until, write, write_config,
};
use super::*;
use crate::eraser::EraserGestureState;
use crate::library::{open_library_read_only, rename_notebook_durable, RenameNotebookHooks};
use crate::nbk_import::ImportSync;
use crate::store::{load_existing_page, PageStore, PersistedReplacementV2};
use crate::sync_local::DERIVED_FRAGMENT_PREFIX;

fn unsynced_erase(lib: &std::path::Path, nb: &str, page: &str, path: &[(f32, f32)]) {
    let (mut store, loaded) = load_existing_page(&nb_dir(lib, nb), page).unwrap();
    let mut g = EraserGestureState::Idle;
    let p0 = PagePoint::new(path[0].0, path[0].1);
    assert!(g.on_down(p0, &loaded.strokes, &loaded.root_of));
    g.process_to(p0);
    for &(x, y) in &path[1..] {
        g.process_to(PagePoint::new(x, y));
    }
    let commit = g.on_up();
    assert!(commit.dirty, "test erase must hit something");
    store.append_replace_strokes(&commit.replacements).unwrap();
}

fn derived_ids(lib: &std::path::Path, nb: &str, page: &str) -> Vec<String> {
    let (_, loaded) = load_existing_page(&nb_dir(lib, nb), page).unwrap();
    loaded
        .strokes
        .iter()
        .map(|s| s.id.as_str().to_string())
        .filter(|id| id.starts_with(DERIVED_FRAGMENT_PREFIX))
        .collect()
}

fn journal_kinds(sync_dir: &std::path::Path) -> Vec<&'static str> {
    parse_journal(&fs::read(sync_dir.join(OUTBOX_FILE)).unwrap())
        .unwrap()
        .committed
        .iter()
        .map(|o| op_kind(&o.envelope.operation))
        .collect()
}

fn retitle_outside_capture(lib: &std::path::Path) {
    let mut repo = open_library_read_only(lib).unwrap();
    rename_notebook_durable(
        &mut repo,
        "notebook-1",
        "Elsewhere",
        &RenameNotebookHooks::default(),
    )
    .unwrap();
}

/// Historical failure: local erase with random fragment IDs and no `EraseRegion` in the
/// outbox cannot be re-derived. Restart faults; nothing is rewritten.
#[test]
fn uncaptured_random_erase_faults_on_reopen_as_underivable() {
    let e = env("hist-erase");
    stop(OutboundSync::open(&e.lib, &e.sync).unwrap());
    unsynced_erase(
        &e.lib,
        "notebook-1",
        "page-1",
        &[(2000.0, 400.0), (2000.0, 4500.0)],
    );
    let (_, loaded) = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1").unwrap();
    let fragments: Vec<&str> = loaded
        .strokes
        .iter()
        .filter(|s| {
            loaded
                .root_of
                .get(s.id.as_str())
                .map(String::as_str)
                .unwrap_or(s.id.as_str())
                != s.id.as_str()
        })
        .map(|s| s.id.as_str())
        .collect();
    assert!(!fragments.is_empty(), "erase must leave fragments");
    assert!(
        fragments
            .iter()
            .all(|id| !id.starts_with(DERIVED_FRAGMENT_PREFIX)),
        "historical path used random ids, not d-<uuidv5>: {fragments:?}"
    );
    let before = local_snapshot(&e.lib);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let f = s.fault().expect("must fault");
    assert!(
        f.contains("not derivable") || f.contains("erase") || f.contains("lineage"),
        "{f}"
    );
    assert!(!s.upload_permitted());
    stop(s);
    assert_eq!(local_snapshot(&e.lib), before);
    assert!(pending_seqs(&e.sync).is_empty());
}

/// Session fault (upload blocked) must not disable write-ahead erase capture or v5 IDs.
#[test]
fn faulted_session_still_captures_erase_with_reducer_ids() {
    let e = env("fault-erase");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1000.0, 8000.0), (9000.0, 8000.0)],
    );
    wait_until("add", Duration::from_secs(5), || s.pending() == 1);
    s.fail_external("simulated reconcile fault".into());
    assert!(!s.upload_permitted());
    assert!(s.is_active());
    synced_erase(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(5000.0, 7000.0), (5000.0, 9000.0)],
    );
    wait_until("erase", Duration::from_secs(5), || s.pending() == 2);
    assert!(!s.upload_permitted(), "capture must not clear the fault");
    let derived = derived_ids(&e.lib, "notebook-1", "page-1");
    assert_eq!(derived.len(), 2, "{derived:?}");
    assert!(derived
        .iter()
        .all(|id| id.starts_with(DERIVED_FRAGMENT_PREFIX)));
    stop(s);
    assert_eq!(journal_kinds(&e.sync), ["add_stroke", "erase_region"]);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 2);
    stop(s);
}

/// Persistent divergence blocks upload; new offline edits are still captured and survive restart.
#[test]
fn divergence_blocks_upload_but_not_authoring_or_capture() {
    let srv = server("div-up");
    let e = env("div-up");
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    wait_until("genesis", Duration::from_secs(10), || {
        s.state().genesis_uploaded
    });
    let lib_id = s.state().library_id;
    stop(s);
    retitle_outside_capture(&e.lib);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().unwrap().contains("diverged"));
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1000.0, 8000.0), (9000.0, 8000.0)],
    );
    synced_erase(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(5000.0, 7000.0), (5000.0, 9000.0)],
    );
    wait_until("captured", Duration::from_secs(5), || s.pending() == 2);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        srv.head_seq(&lib_id),
        0,
        "divergent replica must not upload"
    );
    assert!(s.fault().unwrap().contains("diverged"));
    let cursor = s.server_cursor();
    let seqs = pending_seqs(&e.sync);
    stop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().unwrap().contains("diverged"));
    assert_eq!(s.pending(), 2);
    assert_eq!(s.server_cursor(), cursor);
    assert_eq!(pending_seqs(&e.sync), seqs);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(srv.head_seq(&lib_id), 0);
    stop(s);
}

/// Unreachable server (no divergence): edits including erase queue, then resume on reconnect.
#[test]
fn offline_erase_reconnects_and_matches_server_identities() {
    let e = env("off-erase");
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    write_config(&e.sync, &dead_url, &"0".repeat(64), false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1000.0, 8000.0), (9000.0, 8000.0)],
    );
    synced_erase(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(5000.0, 7000.0), (5000.0, 9000.0)],
    );
    wait_until("journal", Duration::from_secs(5), || s.pending() == 2);
    assert!(s.fault().is_none());
    let local_ids = derived_ids(&e.lib, "notebook-1", "page-1");
    stop(s);
    let srv = server("off-erase");
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    wait_until("upload", Duration::from_secs(30), || s.pending() == 0);
    assert!(s.fault().is_none(), "{:?}", s.fault());
    let lib_id = s.state().library_id;
    stop(s);
    assert_eq!(
        srv.snapshot_hash(&lib_id),
        snapshot_sha256(&local_snapshot(&e.lib))
            .unwrap()
            .to_string()
    );
    assert_eq!(derived_ids(&e.lib, "notebook-1", "page-1"), local_ids);
}

/// Several cuts of one lineage: each fragment id is d-v5 and replay-identical.
#[test]
fn repeated_offline_erase_of_one_root_keeps_reducer_parity() {
    let e = env("multi-erase");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(500.0, 8000.0), (9500.0, 8000.0)],
    );
    synced_erase(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(3000.0, 7000.0), (3000.0, 9000.0)],
    );
    synced_erase(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(7000.0, 7000.0), (7000.0, 9000.0)],
    );
    wait_until("journal", Duration::from_secs(5), || s.pending() == 3);
    stop(s);
    let derived = derived_ids(&e.lib, "notebook-1", "page-1");
    assert!(derived.len() >= 3, "{derived:?}");
    assert!(derived
        .iter()
        .all(|id| id.starts_with(DERIVED_FRAGMENT_PREFIX)));
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none());
    stop(s);
    assert_eq!(derived_ids(&e.lib, "notebook-1", "page-1"), derived);
}

/// Crash after write-ahead erase prep but before the notebook replace: no phantom edit.
#[test]
fn faulted_erase_prep_without_journal_write_is_aborted() {
    let e = env("prep-fault");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1000.0, 8000.0), (9000.0, 8000.0)],
    );
    wait_until("add", Duration::from_secs(5), || s.pending() == 1);
    s.fail_external("upload blocked".into());
    let (store, loaded) = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1").unwrap();
    let mut g = EraserGestureState::Idle;
    let p0 = PagePoint::new(5000.0, 7000.0);
    assert!(g.on_down(p0, &loaded.strokes, &loaded.root_of));
    g.process_to(p0);
    g.process_to(PagePoint::new(5000.0, 9000.0));
    let intent = g.intent().unwrap();
    let op = s.erase_op_id();
    let t = s
        .prepare_erase(
            op,
            "notebook-1",
            "page-1",
            &intent.observed_roots,
            &intent.path,
        )
        .expect("prep while faulted");
    drop(store);
    drop(t);
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 1);
    stop(s);
    assert_eq!(journal_kinds(&e.sync), ["add_stroke"]);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

/// Crash after durable replace and before commit: restart keeps the erase exactly once.
#[test]
fn erase_journal_without_commit_record_is_recovered_once() {
    let e = env("erase-commit-crash");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1000.0, 8000.0), (9000.0, 8000.0)],
    );
    wait_until("add", Duration::from_secs(5), || s.pending() == 1);
    let (mut store, loaded) = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1").unwrap();
    let mut g = EraserGestureState::Idle;
    let p0 = PagePoint::new(5000.0, 7000.0);
    assert!(g.on_down(p0, &loaded.strokes, &loaded.root_of));
    g.process_to(p0);
    g.process_to(PagePoint::new(5000.0, 9000.0));
    let intent = g.intent().unwrap();
    let op = s.erase_op_id();
    let commit = g.on_up_with(&mut |src, i| {
        s.erase_fragment_local_id(op, "notebook-1", "page-1", src, i)
            .unwrap()
    });
    let t = s
        .prepare_erase(
            op,
            "notebook-1",
            "page-1",
            &intent.observed_roots,
            &intent.path,
        )
        .unwrap();
    store.append_replace_strokes(&commit.replacements).unwrap();
    drop(t);
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 2);
    stop(s);
    assert_eq!(journal_kinds(&e.sync), ["add_stroke", "erase_region"]);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
    let once = derived_ids(&e.lib, "notebook-1", "page-1");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    stop(s);
    assert_eq!(derived_ids(&e.lib, "notebook-1", "page-1"), once);
}

/// Unpaired (no config.json): capture still runs; pairing is not invented.
#[test]
fn unpaired_library_captures_without_creating_a_server_link() {
    let e = env("unpaired");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.link_endpoint().is_none());
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (40.0, 40.0)],
    );
    wait_until("journal", Duration::from_secs(5), || s.pending() == 1);
    assert!(s.fault().is_none());
    stop(s);
    assert!(!e.sync.join(CONFIG_FILE).exists());
    assert_eq!(pending_seqs(&e.sync), [1]);
}

fn outbox_bytes(sync_dir: &std::path::Path) -> Vec<u8> {
    fs::read(sync_dir.join(OUTBOX_FILE)).unwrap_or_default()
}

fn local_undo(lib: &std::path::Path, nb: &str, page: &str, root: &str) {
    let (mut store, loaded) = load_existing_page(&nb_dir(lib, nb), page).unwrap();
    let targets: Vec<PersistedReplacementV2> = loaded
        .strokes
        .iter()
        .filter(|s| {
            loaded
                .root_of
                .get(s.id.as_str())
                .map_or(s.id.as_str(), String::as_str)
                == root
        })
        .map(|s| PersistedReplacementV2 {
            target_stroke_id: s.id.as_str().to_string(),
            fragments: Vec::new(),
        })
        .collect();
    store.append_replace_strokes(&targets).unwrap();
}

fn gated(s: &OutboundSync) -> Arc<JournalGate> {
    let gate = Arc::new(JournalGate::default());
    gate.close();
    s.set_journal_gate(Some(Arc::clone(&gate)));
    gate
}

/// Sequence gap: parsed history is not trustworthy; nothing is appended or rewritten.
#[test]
fn outbox_sequence_gap_refuses_allocation() {
    let e = env("gap");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (20.0, 20.0)],
    );
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(50.0, 50.0), (60.0, 60.0)],
    );
    wait_until("three", Duration::from_secs(5), || s.pending() == 3);
    stop(s);
    let parsed = parse_journal(&outbox_bytes(&e.sync)).unwrap();
    let mut keep = Vec::new();
    keep.extend(record_line(&parsed.committed[0].record(RecordKind::Op)));
    keep.extend(record_line(&parsed.committed[2].record(RecordKind::Op)));
    fs::write(e.sync.join(OUTBOX_FILE), &keep).unwrap();
    let before = keep.clone();
    let snap = local_snapshot(&e.lib);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let f = s.fault().unwrap();
    assert!(f.contains("seq 3 after 1") || f.contains("after"), "{f}");
    assert!(!s.capture_ready());
    assert!(!s.upload_permitted());
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(70.0, 70.0), (80.0, 80.0)],
    );
    assert!(s.prepare_rename_notebook("notebook-1", "Nope").is_none());
    assert_eq!(s.pending(), 0);
    stop(s);
    assert_eq!(outbox_bytes(&e.sync), before);
    assert_eq!(
        local_snapshot(&e.lib).notebooks[0].title,
        snap.notebooks[0].title
    );
}

/// Trailing valid prep plus inbound apply marker: no sequenced append.
#[test]
fn prep_plus_inbound_marker_refuses_sequence() {
    let e = env("prep-mark");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (20.0, 20.0)],
    );
    wait_until("add", Duration::from_secs(5), || s.pending() == 1);
    let t = s.prepare_rename_notebook("notebook-1", "Held").unwrap();
    fs::write(e.sync.join(APPLY_FILE), br#"{"v":1,"target_cursor":1}"#).unwrap();
    let before = outbox_bytes(&e.sync);
    drop(t);
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().unwrap().contains("apply marker"));
    assert!(!s.capture_ready());
    assert!(s.prepare_rename_notebook("notebook-1", "Nope").is_none());
    stop(s);
    assert_eq!(outbox_bytes(&e.sync), before);
}

/// Trailing malformed prep: parse fails; original bytes kept.
#[test]
fn trailing_malformed_prep_refuses_sequence() {
    let e = env("bad-prep");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (20.0, 20.0)],
    );
    wait_until("add", Duration::from_secs(5), || s.pending() == 1);
    stop(s);
    let mut bytes = outbox_bytes(&e.sync);
    bytes.extend(b"{\"v\":1,\"kind\":\"prep\",\"seq\":2,not-json\n");
    fs::write(e.sync.join(OUTBOX_FILE), &bytes).unwrap();
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().unwrap().contains("corrupt"));
    assert!(!s.capture_ready());
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    stop(s);
    assert_eq!(outbox_bytes(&e.sync), bytes);
}

/// Repeated restart after a malformed journal never appends or rewrites it.
#[test]
fn repeated_restart_after_malformed_journal_stays_refused() {
    let e = env("mal-loop");
    stop(OutboundSync::open(&e.lib, &e.sync).unwrap());
    fs::write(e.sync.join(OUTBOX_FILE), b"not json\n").unwrap();
    let before = outbox_bytes(&e.sync);
    for _ in 0..3 {
        let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
        assert!(!s.capture_ready());
        write(
            &s,
            &e.lib,
            "notebook-1",
            "page-1",
            &[(1.0, 1.0), (2.0, 2.0)],
        );
        assert!(s
            .prepare_erase(s.erase_op_id(), "notebook-1", "page-1", &[], &[])
            .is_none());
        stop(s);
        assert_eq!(outbox_bytes(&e.sync), before);
    }
}

/// Prep write/fsync failure: erase must not persist.
#[test]
fn prep_fsync_failure_refuses_erase() {
    let e = env("prep-fsync");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let stroke = write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1000.0, 8000.0), (9000.0, 8000.0)],
    );
    wait_until("add", Duration::from_secs(5), || s.pending() == 1);
    s.fail_next_journal_write("injected prep fsync failure");
    let before = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1")
        .unwrap()
        .1
        .strokes
        .len();
    let (store, loaded) = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1").unwrap();
    let mut g = EraserGestureState::Idle;
    let p0 = notebook_core::PagePoint::new(5000.0, 7000.0);
    assert!(g.on_down(p0, &loaded.strokes, &loaded.root_of));
    g.process_to(p0);
    g.process_to(notebook_core::PagePoint::new(5000.0, 9000.0));
    let intent = g.intent().unwrap();
    let op = s.erase_op_id();
    assert!(s
        .prepare_erase(
            op,
            "notebook-1",
            "page-1",
            &intent.observed_roots,
            &intent.path
        )
        .is_none());
    drop(store);
    assert!(!s.capture_ready());
    stop(s);
    let after = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1")
        .unwrap()
        .1;
    assert_eq!(after.strokes.len(), before);
    assert!(after.strokes.iter().any(|st| st.id == stroke.id));
    assert_eq!(journal_kinds(&e.sync), ["add_stroke"]);
}

/// Prep durable, notebook write skipped: abort reuses the sequence.
#[test]
fn prepared_erase_aborted_when_notebook_write_skipped() {
    let e = env("prep-abort-nb");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1000.0, 8000.0), (9000.0, 8000.0)],
    );
    wait_until("add", Duration::from_secs(5), || s.pending() == 1);
    let snap = local_snapshot(&e.lib);
    let (store, loaded) = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1").unwrap();
    let mut g = EraserGestureState::Idle;
    let p0 = notebook_core::PagePoint::new(5000.0, 7000.0);
    assert!(g.on_down(p0, &loaded.strokes, &loaded.root_of));
    g.process_to(p0);
    g.process_to(notebook_core::PagePoint::new(5000.0, 9000.0));
    let intent = g.intent().unwrap();
    let t = s
        .prepare_erase(
            s.erase_op_id(),
            "notebook-1",
            "page-1",
            &intent.observed_roots,
            &intent.path,
        )
        .unwrap();
    drop(store);
    s.abort(t);
    assert!(s.capture_ready());
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    wait_until("reused", Duration::from_secs(5), || s.pending() == 2);
    stop(s);
    assert_eq!(pending_seqs(&e.sync), [1, 2]);
    assert_eq!(local_snapshot(&e.lib).notebooks.len(), snap.notebooks.len());
}

/// Notebook replace succeeded but Commit append failed: restart recovers the erase once.
#[test]
fn commit_append_failure_recovers_erase_on_restart() {
    let e = env("commit-fail");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1000.0, 8000.0), (9000.0, 8000.0)],
    );
    wait_until("add", Duration::from_secs(5), || s.pending() == 1);
    let (mut store, loaded) = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1").unwrap();
    let mut g = EraserGestureState::Idle;
    let p0 = notebook_core::PagePoint::new(5000.0, 7000.0);
    assert!(g.on_down(p0, &loaded.strokes, &loaded.root_of));
    g.process_to(p0);
    g.process_to(notebook_core::PagePoint::new(5000.0, 9000.0));
    let intent = g.intent().unwrap();
    let op = s.erase_op_id();
    let commit = g.on_up_with(&mut |src, i| {
        s.erase_fragment_local_id(op, "notebook-1", "page-1", src, i)
            .unwrap()
    });
    let t = s
        .prepare_erase(
            op,
            "notebook-1",
            "page-1",
            &intent.observed_roots,
            &intent.path,
        )
        .unwrap();
    store.append_replace_strokes(&commit.replacements).unwrap();
    s.fail_next_journal_write("injected commit failure");
    s.commit(t);
    wait_until("commit fail", Duration::from_secs(5), || !s.capture_ready());
    let ids = derived_ids(&e.lib, "notebook-1", "page-1");
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert!(s.capture_ready());
    assert_eq!(s.pending(), 2);
    stop(s);
    assert_eq!(journal_kinds(&e.sync), ["add_stroke", "erase_region"]);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
    assert_eq!(derived_ids(&e.lib, "notebook-1", "page-1"), ids);
}

/// Abort record itself fails: sequence is not reused; restart aborts the unused prep.
#[test]
fn interrupted_abort_does_not_reuse_sequence() {
    let e = env("abort-fail");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (20.0, 20.0)],
    );
    wait_until("add", Duration::from_secs(5), || s.pending() == 1);
    let t = s.prepare_rename_notebook("notebook-1", "Never").unwrap();
    let seq = s.next_client_seq();
    s.fail_next_journal_write("injected abort failure");
    s.abort(t);
    assert!(!s.capture_ready());
    assert!(s.prepare_rename_notebook("notebook-1", "Nope").is_none());
    assert_eq!(s.next_client_seq(), seq);
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert!(s.capture_ready());
    assert_eq!(s.pending(), 1);
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    wait_until("after", Duration::from_secs(5), || s.pending() == 2);
    stop(s);
    assert_eq!(pending_seqs(&e.sync), [1, 2]);
}

/// Timeout is not proof the prep was unwritten: sequence is not reused; delayed write is recovered.
#[test]
fn prep_timeout_then_delayed_durable_append_is_not_reused() {
    let e = env("prep-to");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1000.0, 8000.0), (9000.0, 8000.0)],
    );
    wait_until("add", Duration::from_secs(5), || s.pending() == 1);
    let gate = gated(&s);
    let (store, loaded) = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1").unwrap();
    let mut g = EraserGestureState::Idle;
    let p0 = notebook_core::PagePoint::new(5000.0, 7000.0);
    assert!(g.on_down(p0, &loaded.strokes, &loaded.root_of));
    g.process_to(p0);
    g.process_to(notebook_core::PagePoint::new(5000.0, 9000.0));
    let intent = g.intent().unwrap();
    let op = s.erase_op_id();
    let t0 = Instant::now();
    assert!(s
        .prepare_erase(
            op,
            "notebook-1",
            "page-1",
            &intent.observed_roots,
            &intent.path
        )
        .is_none());
    assert!(
        t0.elapsed() >= Duration::from_secs(4),
        "prepare must wait out the timeout, got {:?}",
        t0.elapsed()
    );
    assert!(!s.capture_ready());
    drop(store);
    s.set_journal_gate(None);
    gate.open();
    std::thread::sleep(Duration::from_millis(200));
    assert!(s
        .prepare_erase(s.erase_op_id(), "notebook-1", "page-1", &[], &[])
        .is_none());
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert!(s.capture_ready());
    assert_eq!(s.pending(), 1);
    stop(s);
    assert_eq!(journal_kinds(&e.sync), ["add_stroke"]);
}

/// Post-commit stroke capture failure is recovered by derivation on restart.
#[test]
fn stroke_outbox_append_failure_is_rederived() {
    let e = env("stroke-fail");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    s.fail_next_journal_write("injected stroke append failure");
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (40.0, 40.0)],
    );
    wait_until("fail", Duration::from_secs(5), || !s.capture_ready());
    let snap = local_snapshot(&e.lib);
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert!(s.capture_ready());
    assert_eq!(s.pending(), 1);
    stop(s);
    assert_eq!(replay_local(&e.sync).snapshot, snap);
}

/// Post-commit undo capture failure is recovered by derivation on restart.
#[test]
fn undo_outbox_append_failure_is_rederived() {
    let e = env("undo-fail");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let stroke = write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (40.0, 40.0)],
    );
    wait_until("add", Duration::from_secs(5), || s.pending() == 1);
    local_undo(&e.lib, "notebook-1", "page-1", stroke.id.as_str());
    s.fail_next_journal_write("injected undo append failure");
    s.lineage_deleted("notebook-1", "page-1", stroke.id.as_str());
    wait_until("fail", Duration::from_secs(5), || !s.capture_ready());
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 2);
    stop(s);
    assert_eq!(
        journal_kinds(&e.sync),
        ["add_stroke", "delete_stroke_lineage"]
    );
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

/// Journal worker gone: capture send fails closed; barrier reports stopped or faulted.
#[test]
fn capture_channel_disconnect_is_explicit() {
    let e = env("jdisc");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (20.0, 20.0)],
    );
    wait_until("add", Duration::from_secs(5), || s.pending() == 1);
    s.disconnect_journal();
    wait_until("stopped", Duration::from_secs(5), || {
        s.wait_journal_durable(&AtomicBool::new(false)) == Err(DurableWaitError::Stopped)
            || !s.capture_ready()
    });
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    assert!(!s.capture_ready() || s.fault().is_some());
    stop(s);
}

/// Unreachable server / known divergence: bulk barrier still succeeds for a healthy journal.
#[test]
fn bulk_barrier_ignores_network_and_divergence() {
    let no = AtomicBool::new(false);
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    let e = env("bulk-net");
    write_config(&e.sync, &dead_url, &"0".repeat(64), false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1.0, 1.0), (2.0, 2.0)],
    );
    assert_eq!(s.wait_journal_durable(&no), Ok(()));
    assert_eq!(ImportSync::flush(&s, &no), Ok(()));
    stop(s);

    retitle_outside_capture(&e.lib);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().unwrap().contains("diverged"));
    assert!(s.capture_ready());
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(3.0, 3.0), (4.0, 4.0)],
    );
    assert_eq!(s.wait_journal_durable(&no), Ok(()));
    assert!(!s.upload_permitted());
    stop(s);
}

/// Unknown sequence authority: a barrier must not claim capture durability.
#[test]
fn bulk_barrier_fails_when_sequence_unknown() {
    let e = env("bulk-seq");
    stop(OutboundSync::open(&e.lib, &e.sync).unwrap());
    fs::write(e.sync.join(OUTBOX_FILE), b"not json\n").unwrap();
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(!s.capture_ready());
    assert!(matches!(
        s.wait_journal_durable(&AtomicBool::new(false)),
        Err(DurableWaitError::Faulted(_))
    ));
    stop(s);
}

/// Grouped writes then a barrier: the barrier covers preceding records.
#[test]
fn bulk_barrier_after_grouped_writes() {
    let e = env("bulk-grp");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    for i in 0..8 {
        let x = 10.0 + i as f32;
        write(&s, &e.lib, "notebook-1", "page-1", &[(x, 10.0), (x, 40.0)]);
    }
    assert_eq!(s.wait_journal_durable(&AtomicBool::new(false)), Ok(()));
    assert_eq!(s.pending(), 8);
    stop(s);
    assert_eq!(pending_seqs(&e.sync).len(), 8);
}
