//! Deterministic verified-stamp fast-path tests (Slice 18C).

use std::cell::RefCell;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use super::*;
use crate::library::{
    create_notebook_durable, delete_notebook_durable, open_library_read_only,
    rename_notebook_durable, CreateNotebookHooks, DeleteNotebookHooks, RenameNotebookHooks,
};
use crate::notebook::{
    delete_page_durable, insert_page_after_durable, DeletePageHooks, InsertPageHooks,
    PersistedNotebookManifestV1,
};
use crate::profile::RenderProfile;
use crate::store::{load_existing_page, PageStore};
use crate::sync_verified::{self, FastPathReason, InvalidateHooks, LocalVerifiedV1, STAMP_FILE};
use crate::ui_gfx::paper_background;

use super::tests::{append_stroke, env, geo, local_snapshot, stop, wait_until, write};
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use notebook_core::{new_stroke_id, Point, Stroke};

/// `last_fast_path_reason_for_test` is process-global; serialize these cases.
static LOCK: Mutex<()> = Mutex::new(());

fn guard() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

thread_local! {
    static LAST: RefCell<Option<FastPathReason>> = RefCell::new(None);
}

fn open(lib: &Path, sync: &Path) -> OutboundSync {
    let s = OutboundSync::open(lib, sync).unwrap();
    LAST.with(|c| *c.borrow_mut() = s.reconcile_path());
    s
}

fn reason() -> FastPathReason {
    LAST.with(|c| (*c.borrow()).expect("reconcile evaluated a path"))
}

/// Offline edits bump gen without always clearing the stamp file (registry is empty
/// after shutdown). Any of these is a safe full-reconcile fallback.
fn assert_dirty_fallback() {
    let r = reason();
    assert!(
        matches!(
            r,
            FastPathReason::Dirty | FastPathReason::GenMismatch | FastPathReason::MissingStamp
        ),
        "expected dirty/gen/missing fallback, got {r:?}"
    );
}

#[test]
fn clean_reopen_uses_verified_fast_path() {
    let _g = guard();
    let e = env("fp-clean");
    let s = open(&e.lib, &e.sync);
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert!(
        !reason().is_fast(),
        "first open must full-reconcile: {:?}",
        reason()
    );
    assert!(
        e.sync.join(STAMP_FILE).exists(),
        "stamp written after full reconcile"
    );
    let st1 = s.state();
    let pending1 = s.pending();
    let snap1 = local_snapshot(&e.lib);
    stop(s);

    let s2 = open(&e.lib, &e.sync);
    assert!(s2.fault().is_none(), "{:?}", s2.fault());
    assert_eq!(reason(), FastPathReason::Verified);
    assert_eq!(s2.state().library_id, st1.library_id);
    assert_eq!(s2.state().replica_id, st1.replica_id);
    assert_eq!(s2.state().server_cursor, st1.server_cursor);
    assert_eq!(s2.state().acked_client_seq, st1.acked_client_seq);
    assert_eq!(s2.pending(), pending1);
    assert_eq!(local_snapshot(&e.lib), snap1);
    stop(s2);
}

#[test]
fn stroke_between_sessions_forces_full_reconcile() {
    let _g = guard();
    let e = env("fp-stroke2");
    let s = open(&e.lib, &e.sync);
    assert!(s.fault().is_none());
    stop(s);
    append_stroke(
        &e.lib,
        "notebook-1",
        "page-1",
        &[(100.0, 100.0), (200.0, 200.0)],
    );
    let s2 = open(&e.lib, &e.sync);
    assert!(s2.fault().is_none(), "{:?}", s2.fault());
    assert!(
        matches!(
            reason(),
            FastPathReason::GenMismatch | FastPathReason::Dirty | FastPathReason::MissingStamp
        ),
        "edited library must not take fast path: {:?}",
        reason()
    );
    stop(s2);
    let s3 = open(&e.lib, &e.sync);
    assert!(s3.fault().is_none());
    assert_eq!(reason(), FastPathReason::Verified);
    stop(s3);
}

#[test]
fn notebook_create_invalidates_fast_path() {
    let _g = guard();
    let e = env("fp-nb-create");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let g = geo();
    let paper = paper_background(&g);
    let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
    let mut lib = open_library_read_only(&e.lib).unwrap();
    create_notebook_durable(
        &mut lib,
        &profile,
        &g,
        &paper,
        Some("Extra"),
        &CreateNotebookHooks::default(),
    )
    .unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert!(s2.fault().is_none(), "{:?}", s2.fault());
    assert_dirty_fallback();
    stop(s2);
}

#[test]
fn text_write_invalidates_stamp() {
    use crate::store::{page_dir, write_page_texts};

    let _g = guard();
    let e = env("fp-text");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let dir = page_dir(&e.lib.join("notebooks").join("notebook-1"), "page-1");
    let text = sync_engine::testkit::text_obj(1, "hello");
    write_page_texts(&dir, std::slice::from_ref(&text)).unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert!(!reason().is_fast(), "text edit: {:?}", reason());
    // Uncaptured text may fault closed on full reconcile; the stamp must still not be trusted.
    stop(s2);
}

#[test]
fn link_rewrite_invalidates_stamp() {
    use crate::store::{page_dir, write_page_links};

    let _g = guard();
    let e = env("fp-link");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let dir = page_dir(&e.lib.join("notebooks").join("notebook-1"), "page-1");
    // Empty rewrite still crosses the durable mutation boundary (gen bump / stamp clear).
    write_page_links(&dir, &[]).unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert!(s2.fault().is_none(), "{:?}", s2.fault());
    assert!(!reason().is_fast(), "link rewrite: {:?}", reason());
    stop(s2);
}

#[test]
fn missing_stamp_falls_back() {
    let _g = guard();
    let e = env("fp-missing");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let _ = fs::remove_file(e.sync.join(STAMP_FILE));
    let s2 = open(&e.lib, &e.sync);
    assert!(s2.fault().is_none());
    assert_eq!(reason(), FastPathReason::MissingStamp);
    stop(s2);
}

#[test]
fn corrupt_stamp_falls_back() {
    let _g = guard();
    let e = env("fp-corrupt");
    let s = open(&e.lib, &e.sync);
    stop(s);
    fs::write(e.sync.join(STAMP_FILE), b"{not-json\n").unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert!(s2.fault().is_none());
    assert_eq!(reason(), FastPathReason::CorruptStamp);
    stop(s2);
}

#[test]
fn apply_marker_disables_fast_path() {
    let _g = guard();
    let e = env("fp-marker");
    let s = open(&e.lib, &e.sync);
    stop(s);
    fs::write(e.sync.join(APPLY_FILE), b"{\"v\":1,\"target_cursor\":1}\n").unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::RecoveryMarker);
    stop(s2);
}

#[test]
fn epoch_mismatch_disables_fast_path() {
    let _g = guard();
    let e = env("fp-epoch");
    let s = open(&e.lib, &e.sync);
    let st = s.state();
    stop(s);
    let mut stamp: LocalVerifiedV1 =
        serde_json::from_slice(&fs::read(e.sync.join(STAMP_FILE)).unwrap()).unwrap();
    stamp.server_cursor = st.server_cursor.saturating_add(99);
    fs::write(
        e.sync.join(STAMP_FILE),
        serde_json::to_vec_pretty(&stamp).unwrap(),
    )
    .unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert!(s2.fault().is_none());
    assert_eq!(reason(), FastPathReason::EpochMismatch);
    stop(s2);
}

#[test]
fn acked_seq_mismatch_disables_fast_path() {
    let _g = guard();
    let e = env("fp-acked");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let mut stamp: LocalVerifiedV1 =
        serde_json::from_slice(&fs::read(e.sync.join(STAMP_FILE)).unwrap()).unwrap();
    stamp.acked_client_seq = stamp.acked_client_seq.saturating_add(7);
    fs::write(
        e.sync.join(STAMP_FILE),
        serde_json::to_vec_pretty(&stamp).unwrap(),
    )
    .unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert!(s2.fault().is_none());
    assert_eq!(reason(), FastPathReason::EpochMismatch);
    stop(s2);
}

#[test]
fn gen_mismatch_disables_fast_path() {
    let _g = guard();
    let e = env("fp-gen");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let mut stamp: LocalVerifiedV1 =
        serde_json::from_slice(&fs::read(e.sync.join(STAMP_FILE)).unwrap()).unwrap();
    stamp.mutation_gen = stamp.mutation_gen.saturating_add(1);
    fs::write(
        e.sync.join(STAMP_FILE),
        serde_json::to_vec_pretty(&stamp).unwrap(),
    )
    .unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert!(s2.fault().is_none());
    assert_eq!(reason(), FastPathReason::GenMismatch);
    stop(s2);
}

#[test]
fn captured_edit_then_reopen_preserves_oracle_state() {
    let _g = guard();
    let e = env("fp-captured");
    let s = open(&e.lib, &e.sync);
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (20.0, 20.0)],
    );
    wait_until("captured edit journaled", Duration::from_secs(5), || {
        s.pending() == 1
    });
    assert!(s.fault().is_none());
    let snap = local_snapshot(&e.lib);
    let pending = s.pending();
    stop(s);
    let s2 = open(&e.lib, &e.sync);
    assert!(s2.fault().is_none(), "{:?}", s2.fault());
    assert!(!reason().is_fast(), "{:?}", reason());
    assert_eq!(local_snapshot(&e.lib), snap);
    assert_eq!(s2.pending(), pending);
    stop(s2);
    let s3 = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::Verified);
    assert_eq!(s3.pending(), pending);
    stop(s3);
}

#[test]
fn interrupted_invalidate_cannot_leave_falsely_clean_stamp() {
    // Simulate crash after gen bump but before stamp clear: stamp gen no longer matches.
    let _g = guard();
    let e = env("fp-crash-order");
    let s = open(&e.lib, &e.sync);
    stop(s);
    assert!(e.sync.join(STAMP_FILE).exists());
    let gen_path = e
        .lib
        .join(crate::sync_local::SYNC_DIR)
        .join(sync_verified::MUTATION_GEN_FILE);
    let cur: u64 = fs::read_to_string(&gen_path)
        .unwrap_or_else(|_| "0".into())
        .trim()
        .parse()
        .unwrap_or(0);
    fs::create_dir_all(gen_path.parent().unwrap()).unwrap();
    fs::write(&gen_path, format!("{}\n", cur + 1)).unwrap();
    // Stamp left in place on purpose (crash before clear_stamp_file).
    let s2 = open(&e.lib, &e.sync);
    assert!(s2.fault().is_none(), "{:?}", s2.fault());
    assert_ne!(reason(), FastPathReason::Verified);
    stop(s2);
}

#[test]
fn repeated_clean_reopen_stays_verified() {
    let _g = guard();
    let e = env("fp-repeat");
    let s = open(&e.lib, &e.sync);
    stop(s);
    for _ in 0..3 {
        let s = open(&e.lib, &e.sync);
        assert!(s.fault().is_none());
        assert_eq!(reason(), FastPathReason::Verified);
        stop(s);
    }
}

#[test]
fn divergence_still_faults_when_stamp_absent() {
    // Without a stamp, full reconcile still detects a corrupt/missing stroke binding gap
    // the same way as before — here we only assert reopen after a known-good edit path
    // does not invent a verified stamp when recovery would be required.
    let _g = guard();
    let e = env("fp-div");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let _ = fs::remove_file(e.sync.join(STAMP_FILE));
    // Directly clobber page journal into nonsense so snapshot compare must refuse.
    let edits = e
        .lib
        .join("notebooks")
        .join("notebook-1")
        .join("page-1")
        .join("edits.jsonl");
    fs::write(&edits, b"{not-a-stroke\n").unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert!(s2.fault().is_some(), "expected fault on corrupt journal");
    assert!(!reason().is_fast());
    stop(s2);
}

fn nb_manifest(lib: &Path, nb: &str) -> PersistedNotebookManifestV1 {
    serde_json::from_slice(&fs::read(lib.join("notebooks").join(nb).join("notebook.meta")).unwrap())
        .unwrap()
}

#[test]
fn notebook_delete_invalidates_fast_path() {
    let _g = guard();
    let e = env("fp-nb-del");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let g = geo();
    let paper = paper_background(&g);
    let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
    let mut lib = open_library_read_only(&e.lib).unwrap();
    let created = create_notebook_durable(
        &mut lib,
        &profile,
        &g,
        &paper,
        Some("Scratch"),
        &CreateNotebookHooks::default(),
    )
    .unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert_dirty_fallback();
    stop(s2);

    let s3 = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::Verified);
    stop(s3);
    delete_notebook_durable(
        &mut lib,
        &created.notebook_id,
        &DeleteNotebookHooks::default(),
    )
    .unwrap();
    let s4 = open(&e.lib, &e.sync);
    assert_dirty_fallback();
    stop(s4);
}

#[test]
fn notebook_rename_invalidates_fast_path() {
    let _g = guard();
    let e = env("fp-nb-ren");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let s = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::Verified);
    let mut lib = open_library_read_only(&e.lib).unwrap();
    let t = s
        .prepare_rename_notebook("notebook-1", "Renamed")
        .expect("prep durable");
    rename_notebook_durable(
        &mut lib,
        "notebook-1",
        "Renamed",
        &RenameNotebookHooks::default(),
    )
    .unwrap();
    s.commit(t);
    stop(s);
    let s2 = open(&e.lib, &e.sync);
    assert_dirty_fallback();
    stop(s2);
}

#[test]
fn page_insert_and_delete_invalidate_fast_path() {
    let _g = guard();
    let e = env("fp-page");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let nb = e.lib.join("notebooks").join("notebook-1");
    let mut m = nb_manifest(&e.lib, "notebook-1");
    let inserted =
        insert_page_after_durable(&nb, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert_dirty_fallback();
    stop(s2);

    let s3 = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::Verified);
    stop(s3);
    let mut m = nb_manifest(&e.lib, "notebook-1");
    delete_page_durable(
        &nb,
        &mut m,
        &inserted.inserted_page_id,
        &DeletePageHooks::default(),
    )
    .unwrap();
    let s4 = open(&e.lib, &e.sync);
    assert_dirty_fallback();
    stop(s4);
}

#[test]
fn pending_prep_disables_fast_path() {
    let _g = guard();
    let e = env("fp-prep");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let s = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::Verified);
    let t = s
        .prepare_rename_notebook("notebook-1", "Never")
        .expect("prep durable");
    drop(t);
    drop(s);
    let s2 = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::PendingPrep, "{:?}", reason());
    stop(s2);
}

#[test]
fn faulted_session_edits_invalidate_stamp() {
    let _g = guard();
    let e = env("fp-faulted");
    let s = open(&e.lib, &e.sync);
    stop(s);
    fs::write(e.sync.join(OUTBOX_FILE), b"{not-json\n").unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert!(s2.fault().is_some(), "{:?}", s2.fault());
    assert_eq!(reason(), FastPathReason::FaultDivergence);
    append_stroke(
        &e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    stop(s2);
    fs::write(e.sync.join(OUTBOX_FILE), b"").unwrap();
    let s3 = open(&e.lib, &e.sync);
    assert!(
        !reason().is_fast(),
        "edit while faulted must dirty stamp: {:?}",
        reason()
    );
    stop(s3);
}

#[test]
fn no_upload_before_required_recovery() {
    let _g = guard();
    let e = env("fp-noup");
    let s = open(&e.lib, &e.sync);
    assert_eq!(s.stats().uploaded, 0);
    stop(s);
    fs::write(e.sync.join(APPLY_FILE), b"{\"v\":1,\"target_cursor\":1}\n").unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::RecoveryMarker);
    wait_until(
        "uploader polled after reconcile",
        Duration::from_secs(2),
        || s2.stats().uploader_polls >= 1,
    );
    assert_eq!(
        s2.stats().uploaded,
        0,
        "uploader must not have sent before recovery"
    );
    assert_eq!(s2.stats().batches, 0);
    stop(s2);
}

#[test]
fn legacy_writer_without_gen_cannot_take_fast_path() {
    let _g = guard();
    let e = env("fp-legacy");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let journal = e
        .lib
        .join("notebooks")
        .join("notebook-1")
        .join("page-1")
        .join("strokes.jsonl");
    {
        let mut f = fs::OpenOptions::new().append(true).open(&journal).unwrap();
        f.write_all(b"{\"legacy\":true}\n").unwrap();
    }
    let s2 = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::CensusMismatch);
    assert!(!reason().is_fast());
    stop(s2);
}

#[test]
fn notebook_write_aborts_when_invalidation_fails() {
    let _g = guard();
    let e = env("fp-abort-write");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let stroke = Stroke {
        id: new_stroke_id(),
        points: vec![
            Point::new(1.0, 1.0).with_pressure(0.5),
            Point::new(2.0, 2.0).with_pressure(0.5),
        ],
        brush: Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
        finished: true,
    };
    let (mut store, _) =
        load_existing_page(&e.lib.join("notebooks").join("notebook-1"), "page-1").unwrap();
    {
        let _h = sync_verified::install_invalidate_hooks(InvalidateHooks {
            fail_gen_create: true,
            ..Default::default()
        });
        let err = store.append_committed_stroke(&stroke).unwrap_err();
        assert!(err.contains("injected: create"), "{err}");
    }
    let s2 = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::Verified);
    stop(s2);
}

#[test]
fn same_size_legacy_title_rewrite_cannot_take_fast_path() {
    let _g = guard();
    let e = env("fp-same-size");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let info = e
        .lib
        .join("notebooks")
        .join("notebook-1")
        .join("notebook.info");
    let raw = fs::read_to_string(&info).unwrap();
    let next = raw.replace("Notebook 1", "Notebook X");
    assert_ne!(next, raw, "fixture title must be replaceable");
    assert_eq!(next.len(), raw.len(), "equal-length rewrite");
    fs::write(&info, next).unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::CensusMismatch);
    assert!(!reason().is_fast());
    stop(s2);
}

#[test]
fn same_size_atomic_replace_cannot_take_fast_path() {
    let _g = guard();
    let e = env("fp-atomic-same");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let info = e
        .lib
        .join("notebooks")
        .join("notebook-1")
        .join("notebook.info");
    let raw = fs::read(&info).unwrap();
    let mut next = raw.clone();
    for b in &mut next {
        if *b == b'1' {
            *b = b'2';
            break;
        }
    }
    assert_eq!(next.len(), raw.len());
    assert_ne!(next, raw);
    let tmp = info.with_extension("info.tmp");
    fs::write(&tmp, &next).unwrap();
    #[cfg(windows)]
    let _ = fs::remove_file(&info);
    fs::rename(&tmp, &info).unwrap();
    let s2 = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::CensusMismatch);
    stop(s2);
}

#[test]
fn fast_path_reopen_matches_forced_full_reconcile() {
    let _g = guard();
    let e = env("fp-oracle");
    let s = open(&e.lib, &e.sync);
    assert!(s.fault().is_none());
    stop(s);
    let s_fast = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::Verified);
    let fast_state = s_fast.state();
    let fast_pending = s_fast.pending();
    let fast_fault = s_fast.fault().map(|x| x.to_string());
    let snap = local_snapshot(&e.lib);
    stop(s_fast);
    fs::remove_file(e.sync.join(STAMP_FILE)).unwrap();
    let s_full = open(&e.lib, &e.sync);
    assert!(!reason().is_fast(), "{:?}", reason());
    assert_eq!(s_full.state().library_id, fast_state.library_id);
    assert_eq!(s_full.state().replica_id, fast_state.replica_id);
    assert_eq!(s_full.state().server_cursor, fast_state.server_cursor);
    assert_eq!(s_full.state().acked_client_seq, fast_state.acked_client_seq);
    assert_eq!(s_full.pending(), fast_pending);
    assert_eq!(s_full.fault().map(|x| x.to_string()), fast_fault);
    assert_eq!(local_snapshot(&e.lib), snap);
    stop(s_full);
}

#[test]
fn concurrent_failed_note_blocks_journal_write() {
    let _g = guard();
    let e = env("fp-conc-fail");
    let s = open(&e.lib, &e.sync);
    stop(s);
    let holding = std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let release = std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let b_at_gate = std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let stroke = Stroke {
        id: new_stroke_id(),
        points: vec![
            Point::new(3.0, 3.0).with_pressure(0.5),
            Point::new(4.0, 4.0).with_pressure(0.5),
        ],
        brush: Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
        finished: true,
    };
    let page = e.lib.join("notebooks").join("notebook-1");
    let holding_a = std::sync::Arc::clone(&holding);
    let release_a = std::sync::Arc::clone(&release);
    let page_a = page.clone();
    let stroke_a = stroke.clone();
    let a = std::thread::spawn(move || {
        let _h = sync_verified::install_invalidate_hooks(InvalidateHooks {
            fail_gen_create: true,
            stall_invalidate: Some(std::sync::Arc::new(move || {
                *holding_a.0.lock().unwrap() = true;
                holding_a.1.notify_all();
                let mut g = release_a.0.lock().unwrap();
                while !*g {
                    g = release_a.1.wait(g).unwrap();
                }
            })),
            ..Default::default()
        });
        let (mut store, _) = load_existing_page(&page_a, "page-1").unwrap();
        store.append_committed_stroke(&stroke_a)
    });
    {
        let mut g = holding.0.lock().unwrap();
        while !*g {
            g = holding.1.wait(g).unwrap();
        }
    }
    let b_at_gate_t = std::sync::Arc::clone(&b_at_gate);
    let page_b = page.clone();
    let stroke_b = stroke.clone();
    let b = std::thread::spawn(move || {
        let _h = sync_verified::install_invalidate_hooks(InvalidateHooks {
            fail_gen_create: true,
            before_session_lock: Some(std::sync::Arc::new(move || {
                *b_at_gate_t.0.lock().unwrap() = true;
                b_at_gate_t.1.notify_all();
            })),
            ..Default::default()
        });
        let (mut store, _) = load_existing_page(&page_b, "page-1").unwrap();
        store.append_committed_stroke(&stroke_b)
    });
    {
        let mut g = b_at_gate.0.lock().unwrap();
        while !*g {
            g = b_at_gate.1.wait(g).unwrap();
        }
    }
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    assert!(a.join().unwrap().is_err());
    assert!(b.join().unwrap().is_err());
    let s2 = open(&e.lib, &e.sync);
    assert_eq!(reason(), FastPathReason::Verified);
    stop(s2);
}
