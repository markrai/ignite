//! Slice 18E.9C: application-level recovery maintenance fence.

use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use kindle_platform::FbGeometry;
use notebook_core::{new_stroke_id, Point, Stroke};
use sync_engine::testkit::{replica, text_obj};
use sync_engine::LibraryState;

use crate::ink::InkRequest;
use crate::library::{
    create_notebook_durable, open_library_read_only, open_or_create_library,
    read_notebook_manifest_read_only, rename_notebook_durable, CreateNotebookHooks,
    RenameNotebookHooks,
};
use crate::notebook::{insert_page_after_durable, InsertPageHooks};
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use crate::profile::RenderProfile;
use crate::store::{load_existing_page, write_page_links, write_page_texts, PageStore};
use crate::sync_local::ensure_sync_identity;
use crate::sync_verified::{
    clear_registry, hold_authoring_for_maintenance, install_invalidate_hooks,
    note_library_mutation, read_mutation_gen, InvalidateHooks,
};
use crate::ui_gfx::paper_background;

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("scribe-fence-{label}-{n}"))
}

fn geo() -> FbGeometry {
    FbGeometry {
        xres: 200,
        yres: 160,
        xres_virtual: 208,
        yres_virtual: 160,
        xoffset: 0,
        yoffset: 0,
        bits_per_pixel: 8,
        line_length: 208,
        smem_len: 208 * 160,
    }
}

fn sample_stroke() -> Stroke {
    Stroke {
        id: new_stroke_id(),
        points: vec![
            Point::new(500.0, 500.0).with_pressure(0.5),
            Point::new(4000.0, 4000.0).with_pressure(0.5),
        ],
        brush: Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
        finished: true,
    }
}

fn lib_pair(label: &str) -> (PathBuf, PathBuf, LibraryState) {
    let lib = tmp(label).join("library");
    let sync = tmp(&format!("{label}-sync"));
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    ensure_sync_identity(&lib).unwrap();
    let repo = open_library_read_only(&lib).unwrap();
    let dir = repo.notebook_dir("notebook-1");
    let mut m = read_notebook_manifest_read_only(&dir).unwrap();
    insert_page_after_durable(&dir, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    fs::create_dir_all(&sync).unwrap();
    let identity = crate::sync_local::read_identity(&lib).unwrap().unwrap();
    let bound = crate::sync_local::build_snapshot_bound_only(&lib, &identity).unwrap();
    let cp = LibraryState::genesis(bound).unwrap();
    (lib, sync, cp)
}

fn spec<'a>(lib: &'a Path, sync: &'a Path, cp: &'a LibraryState) -> PrepareSpec<'a> {
    PrepareSpec {
        library_root: lib,
        sync_dir: sync,
        checkpoint: cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        next_client_seq: 1,
        replica_id: replica(1),
        genesis_sha256: "g".into(),
        server_tail: &[],
        require_disposable: false,
    }
}

fn page1(lib: &Path) -> PathBuf {
    lib.join("notebooks").join("notebook-1").join("page-1")
}

fn try_append(lib: &Path) -> Result<(), String> {
    let (mut store, _) = load_existing_page(&lib.join("notebooks").join("notebook-1"), "page-1")?;
    store.append_committed_stroke(&sample_stroke())?;
    Ok(())
}

fn journal_len(lib: &Path) -> u64 {
    fs::metadata(page1(lib).join("strokes.jsonl"))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn is_held(err: &str) -> bool {
    err.contains("RECOVERY MAINTENANCE")
}

#[test]
fn writer_finishes_before_maintenance_is_in_snapshot() {
    let (lib, sync, cp) = lib_pair("before");
    let park = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let at = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let park_w = Arc::clone(&park);
    let at_w = Arc::clone(&at);
    let lib_w = lib.clone();
    let th = std::thread::spawn(move || {
        let _h = install_invalidate_hooks(InvalidateHooks {
            after_ordinary_admit: Some(Arc::new(move || {
                *at_w.0.lock().unwrap() = true;
                at_w.1.notify_all();
                let mut g = park_w.0.lock().unwrap();
                while !*g {
                    g = park_w.1.wait(g).unwrap();
                }
            })),
            ..Default::default()
        });
        try_append(&lib_w)
    });
    {
        let mut g = at.0.lock().unwrap();
        while !*g {
            g = at.1.wait(g).unwrap();
        }
    }
    let lib_p = lib.clone();
    let sync_p = sync.clone();
    let prepared = std::thread::spawn(move || {
        let spec = spec(&lib_p, &sync_p, &cp);
        prepare_recovery(&spec, &Default::default())
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        prepared.is_finished() == false,
        "prepare must wait for admitted writer"
    );
    *park.0.lock().unwrap() = true;
    park.1.notify_all();
    th.join().unwrap().unwrap();
    let intent = prepared.join().unwrap().unwrap();
    let identity = crate::sync_local::read_identity(&lib).unwrap().unwrap();
    let snap = crate::sync_local::build_snapshot_read_only(&lib, &identity).unwrap();
    let sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    assert_eq!(sha, intent.local_snapshot_sha256);
    assert!(journal_len(&lib) > 0);
}

#[test]
fn writer_after_maintenance_refuses_before_journal_change() {
    let (lib, sync, cp) = lib_pair("after");
    prepare_recovery(&spec(&lib, &sync, &cp), &Default::default()).unwrap();
    let before = journal_len(&lib);
    let gen = read_mutation_gen(&lib).unwrap();
    let err = try_append(&lib).unwrap_err();
    assert!(is_held(&err), "{err}");
    assert_eq!(journal_len(&lib), before);
    assert_eq!(read_mutation_gen(&lib).unwrap(), gen);
}

#[test]
fn concurrent_writer_and_prepare_have_one_legal_outcome() {
    let (lib, sync, cp) = lib_pair("race");
    let bar = Arc::new(Barrier::new(3));
    let lib_w = lib.clone();
    let bar_w = Arc::clone(&bar);
    let writer = std::thread::spawn(move || {
        bar_w.wait();
        try_append(&lib_w)
    });
    let lib_p = lib.clone();
    let sync_p = sync.clone();
    let bar_p = Arc::clone(&bar);
    let prep = std::thread::spawn(move || {
        bar_p.wait();
        prepare_recovery(&spec(&lib_p, &sync_p, &cp), &Default::default())
    });
    bar.wait();
    let w = writer.join().unwrap();
    let p = prep.join().unwrap();
    let intent = p.unwrap();
    match w {
        Ok(()) => {
            let identity = crate::sync_local::read_identity(&lib).unwrap().unwrap();
            let snap = crate::sync_local::build_snapshot_read_only(&lib, &identity).unwrap();
            let sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
            assert_eq!(sha, intent.local_snapshot_sha256);
            assert!(journal_len(&lib) > 0);
        }
        Err(e) => {
            assert!(is_held(&e), "{e}");
            assert_eq!(journal_len(&lib), 0);
        }
    }
}

#[test]
fn multiple_ordinary_writers_refuse_without_partial_writes() {
    let (lib, sync, cp) = lib_pair("multi");
    prepare_recovery(&spec(&lib, &sync, &cp), &Default::default()).unwrap();
    let before = journal_len(&lib);
    let g = geo();
    let mut repo = open_library_read_only(&lib).unwrap();
    let profile =
        RenderProfile::sealed_v2_pressure(crate::ink::ink_for_new_page(InkRequest::Default))
            .unwrap();
    let paper = paper_background(&g);
    let stroke_err = try_append(&lib).unwrap_err();
    let page_dir_nb = repo.notebook_dir("notebook-1");
    let mut m = read_notebook_manifest_read_only(&page_dir_nb).unwrap();
    let page_err =
        insert_page_after_durable(&page_dir_nb, &mut m, "page-1", &InsertPageHooks::default())
            .unwrap_err();
    let nb_err = create_notebook_durable(
        &mut repo,
        &profile,
        &g,
        &paper,
        Some("held"),
        &CreateNotebookHooks::default(),
    )
    .unwrap_err();
    let text_err = write_page_texts(&page1(&lib), &[text_obj(1, "no")]).unwrap_err();
    assert!(is_held(&stroke_err) && is_held(&page_err) && is_held(&nb_err) && is_held(&text_err));
    assert_eq!(journal_len(&lib), before);
    assert!(!page1(&lib).join("texts.jsonl").exists());
}

#[test]
fn restart_with_intent_keeps_fence_without_network() {
    let (lib, sync, cp) = lib_pair("restart");
    prepare_recovery(&spec(&lib, &sync, &cp), &Default::default()).unwrap();
    clear_registry();
    let err = try_append(&lib).unwrap_err();
    assert!(is_held(&err), "{err}");
    assert!(read_intent(&sync).unwrap().is_some());
}

#[test]
fn halted_recovery_stays_fenced() {
    let (lib, sync, cp) = lib_pair("halt");
    let mut intent = prepare_recovery(&spec(&lib, &sync, &cp), &Default::default()).unwrap();
    intent.phase = RecoveryPhase::Halted;
    intent.halt_reason = Some("test".into());
    write_intent(&sync, &intent, &Default::default()).unwrap();
    let err = try_append(&lib).unwrap_err();
    assert!(is_held(&err), "{err}");
}

#[test]
fn inbound_permit_still_writes_during_maintenance() {
    let (lib, sync, cp) = lib_pair("permit");
    prepare_recovery(&spec(&lib, &sync, &cp), &Default::default()).unwrap();
    crate::sync_verified::permit_sync_mutations(|| try_append(&lib)).unwrap();
    assert!(journal_len(&lib) > 0);
    let ordinary = try_append(&lib).unwrap_err();
    assert!(is_held(&ordinary), "{ordinary}");
}

#[test]
fn mutation_coverage_refuses_supported_entry_points() {
    let (lib, sync, cp) = lib_pair("cover");
    prepare_recovery(&spec(&lib, &sync, &cp), &Default::default()).unwrap();
    let g = geo();
    let mut repo = open_library_read_only(&lib).unwrap();
    let profile =
        RenderProfile::sealed_v2_pressure(crate::ink::ink_for_new_page(InkRequest::Default))
            .unwrap();
    let paper = paper_background(&g);
    let nb = lib.join("notebooks").join("notebook-1");
    assert!(is_held(&try_append(&lib).unwrap_err()));
    assert!(is_held(
        &write_page_texts(&page1(&lib), &[text_obj(1, "x")]).unwrap_err()
    ));
    assert!(is_held(&write_page_links(&page1(&lib), &[]).unwrap_err()));
    let mut m = read_notebook_manifest_read_only(&nb).unwrap();
    assert!(is_held(
        &insert_page_after_durable(&nb, &mut m, "page-1", &InsertPageHooks::default()).unwrap_err()
    ));
    assert!(is_held(
        &create_notebook_durable(
            &mut repo,
            &profile,
            &g,
            &paper,
            Some("blocked"),
            &CreateNotebookHooks::default(),
        )
        .unwrap_err()
    ));
    assert!(is_held(
        &rename_notebook_durable(
            &mut repo,
            "notebook-1",
            "Blocked title",
            &RenameNotebookHooks::default(),
        )
        .unwrap_err()
    ));
    assert!(is_held(
        &crate::store::write_background_atomic(&page1(&lib), &[1, 2, 3]).unwrap_err()
    ));
}

#[test]
fn hold_waits_for_inflight_then_refuses_new_writers() {
    let (lib, _sync, _cp) = lib_pair("holdwait");
    let park = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let at = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let park_w = Arc::clone(&park);
    let at_w = Arc::clone(&at);
    let lib_w = lib.clone();
    let th = std::thread::spawn(move || {
        let _h = install_invalidate_hooks(InvalidateHooks {
            after_ordinary_admit: Some(Arc::new(move || {
                *at_w.0.lock().unwrap() = true;
                at_w.1.notify_all();
                let mut g = park_w.0.lock().unwrap();
                while !*g {
                    g = park_w.1.wait(g).unwrap();
                }
            })),
            ..Default::default()
        });
        try_append(&lib_w)
    });
    {
        let mut g = at.0.lock().unwrap();
        while !*g {
            g = at.1.wait(g).unwrap();
        }
    }
    let lib_h = lib.clone();
    let holder = std::thread::spawn(move || hold_authoring_for_maintenance(&lib_h));
    std::thread::sleep(std::time::Duration::from_millis(40));
    assert!(!holder.is_finished());
    *park.0.lock().unwrap() = true;
    park.1.notify_all();
    th.join().unwrap().unwrap();
    let hold = holder.join().unwrap().unwrap();
    drop(hold);
    try_append(&lib).unwrap();
}

#[test]
fn two_prepares_serialize_to_one_transaction() {
    let (lib, sync, cp) = lib_pair("twoprep");
    let a = prepare_recovery(&spec(&lib, &sync, &cp), &Default::default()).unwrap();
    let b = prepare_recovery(&spec(&lib, &sync, &cp), &Default::default()).unwrap();
    assert_eq!(a.transaction_id, b.transaction_id);
    assert_eq!(a.ops, b.ops);
}

#[test]
fn ordinary_append_overhead_stays_in_note_plus_fsync_band() {
    let (lib, _sync, _cp) = lib_pair("perf");
    let (mut store, _) =
        load_existing_page(&lib.join("notebooks").join("notebook-1"), "page-1").unwrap();
    let t0 = Instant::now();
    for _ in 0..20 {
        store.append_committed_stroke(&sample_stroke()).unwrap();
    }
    let ms = t0.elapsed().as_millis();
    // Host-only characterization: fence adds a metadata check, not a census.
    assert!(
        ms < 5_000,
        "20 stroke appends took {ms}ms on host (sanity, not Kindle timing)"
    );
}

#[test]
fn malformed_intent_flag_fails_closed() {
    let (lib, sync, cp) = lib_pair("corrupt");
    prepare_recovery(&spec(&lib, &sync, &cp), &Default::default()).unwrap();
    let flag = lib
        .join(crate::sync_local::SYNC_DIR)
        .join(crate::sync_verified::RECOVERY_MAINTENANCE_FILE);
    fs::write(&flag, b"").unwrap();
    let err = try_append(&lib).unwrap_err();
    assert!(is_held(&err), "{err}");
}

#[test]
fn note_without_recovery_does_not_create_intent() {
    let (lib, sync, _cp) = lib_pair("none");
    let _ = note_library_mutation(&lib).unwrap();
    assert!(read_intent(&sync).unwrap().is_none());
    try_append(&lib).unwrap();
}
