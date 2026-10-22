//! Slice 19B.3D.1: page navigation is replica-local state, not a library mutation.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::tests::{append_stroke, env, geo, local_snapshot, nb_dir, stop, Env};
use super::*;
use crate::ink::InkRequest;
use crate::library::{
    create_notebook_durable, delete_notebook_durable, open_active_notebook, open_library_read_only,
    open_or_create_library, read_library_manifest_for_test, read_notebook_manifest_read_only,
    rename_notebook_durable, reorder_notebooks_durable, switch_notebook_durable,
    write_library_manifest_for_test, CreateNotebookHooks, DeleteNotebookHooks,
    LibraryManifestWriteHooks, LibraryRepository, RenameNotebookHooks, SwitchNotebookHooks,
};
use crate::nav_state::{
    active_notebook_path, nav_path, remember, selected_page, ACTIVE_NOTEBOOK_FILE, NAV_FILE,
};
use crate::notebook::{
    allocate_page_id, commit_new_page_to_manifest, create_page_files_from_template,
    delete_page_durable, insert_page_after_durable, load_page_for_switch, open_or_create_notebook,
    persist_active_page, remove_page_durable, reorder_pages_durable, write_manifest_atomic_with,
    DeletePageHooks, InsertPageHooks, ManifestWriteHooks, PersistedNotebookManifestV1,
};
use crate::page_ledger::ledger_path;
use crate::profile::RenderProfile;
use crate::sync_local::SYNC_DIR;
use crate::sync_verified::{library_census_map, read_mutation_gen, FastPathReason, STAMP_FILE};
use crate::ui_gfx::paper_background;

const NB: &str = "notebook-1";

fn manifest(lib: &Path, nb: &str) -> PersistedNotebookManifestV1 {
    read_notebook_manifest_read_only(&nb_dir(lib, nb)).unwrap()
}

fn meta_bytes(lib: &Path, nb: &str) -> Vec<u8> {
    fs::read(nb_dir(lib, nb).join("notebook.meta")).unwrap()
}

/// Insert pages until `nb` has `count` (each insert is a genuine canonical change).
fn grow(lib: &Path, nb: &str, count: usize) {
    let d = nb_dir(lib, nb);
    let mut m = manifest(lib, nb);
    while m.page_ids.len() < count {
        let last = m.page_ids.last().unwrap().clone();
        insert_page_after_durable(&d, &mut m, &last, &InsertPageHooks::default()).unwrap();
    }
}

/// The runtime's page switch: load and bind the target, then remember it.
fn navigate(lib: &Path, nb: &str, m: &mut PersistedNotebookManifestV1, page: &str) {
    let d = nb_dir(lib, nb);
    let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
    load_page_for_switch(&d, page, &profile, &geo()).unwrap();
    persist_active_page(&d, m, page).unwrap();
}

/// The page a fresh process would show for `nb`.
fn reopened_page(lib: &Path, nb: &str) -> String {
    let g = geo();
    open_or_create_notebook(
        &nb_dir(lib, nb),
        InkRequest::Default,
        &g,
        &paper_background(&g),
    )
    .unwrap()
    .page_id
}

fn read_opt(p: &Path) -> Option<Vec<u8>> {
    fs::read(p).ok()
}

fn second_notebook(lib: &Path) -> String {
    let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
    let g = geo();
    let mut repo = open_library_read_only(lib).unwrap();
    create_notebook_durable(
        &mut repo,
        &profile,
        &g,
        &paper_background(&g),
        None,
        &CreateNotebookHooks::default(),
    )
    .unwrap()
    .notebook_id
}

fn cleanup(e: Env) {
    crate::sync_verified::clear_registry_for(&e.lib);
    let _ = fs::remove_dir_all(e.lib.parent().unwrap());
}

/// A notebook directory outside any library (dev / single-notebook mode).
fn bare_notebook(label: &str) -> PathBuf {
    let d = super::tests::tmp(label).join("bare");
    let g = geo();
    open_or_create_notebook(&d, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    d
}

// ------------------------------------------------------------------ principal acceptance

#[test]
fn browse_only_navigation_keeps_the_verified_fast_path() {
    let e = env("nav-browse");
    grow(&e.lib, NB, 4);
    // Clean, verified library.
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    stop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert_eq!(s.reconcile_path(), Some(FastPathReason::Verified));
    let gen0 = read_mutation_gen(&e.lib).unwrap();
    let stamp0 = fs::read(e.sync.join(STAMP_FILE)).unwrap();
    let (census0, _) = library_census_map(&e.lib).unwrap();
    let meta0 = meta_bytes(&e.lib, NB);
    let ledger0 = read_opt(&ledger_path(&e.lib));
    let snap0 = local_snapshot(&e.lib);
    let (state0, pending0) = (s.state(), s.pending());

    // Open the notebook and browse while sync runs, as the runtime does.
    let g = geo();
    let lib =
        open_or_create_library(&e.lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    let mut m = lib.notebook.manifest.clone();
    drop(lib);
    for p in ["page-2", "page-3", "page-1", "page-4", "page-3"] {
        navigate(&e.lib, NB, &mut m, p);
    }
    assert_eq!(s.pending(), pending0, "navigation queued sync work");
    stop(s);

    // Nothing canonical moved.
    assert_eq!(read_mutation_gen(&e.lib).unwrap(), gen0);
    assert_eq!(fs::read(e.sync.join(STAMP_FILE)).unwrap(), stamp0);
    assert_eq!(library_census_map(&e.lib).unwrap().0, census0);
    assert_eq!(meta_bytes(&e.lib, NB), meta0);
    assert_eq!(read_opt(&ledger_path(&e.lib)), ledger0, "ledger appended");
    assert!(nav_path(&nb_dir(&e.lib, NB)).unwrap().is_file());

    // Restart: same page, verified fast path, identical sync state.
    assert_eq!(reopened_page(&e.lib, NB), "page-3");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.reconcile_path(), Some(FastPathReason::Verified));
    let st = s.state();
    assert_eq!(st.server_cursor, state0.server_cursor);
    assert_eq!(st.acked_client_seq, state0.acked_client_seq);
    assert_eq!(s.pending(), pending0);
    assert_eq!(local_snapshot(&e.lib), snap0);
    stop(s);
    cleanup(e);
}

// ------------------------------------------------------------------ preference behaviour

#[test]
fn each_notebook_keeps_its_own_page_across_restarts() {
    let e = env("nav-two");
    let nb2 = second_notebook(&e.lib);
    grow(&e.lib, NB, 3);
    grow(&e.lib, &nb2, 3);
    let mut m1 = manifest(&e.lib, NB);
    let mut m2 = manifest(&e.lib, &nb2);
    navigate(&e.lib, NB, &mut m1, "page-2");
    navigate(&e.lib, &nb2, &mut m2, "page-1");
    navigate(&e.lib, NB, &mut m1, "page-1");
    navigate(&e.lib, NB, &mut m1, "page-2");
    assert_eq!(reopened_page(&e.lib, NB), "page-2");
    assert_eq!(reopened_page(&e.lib, &nb2), "page-1");
    // Reopening twice is stable and writes nothing.
    let nav = fs::read(nav_path(&nb_dir(&e.lib, NB)).unwrap()).unwrap();
    assert_eq!(reopened_page(&e.lib, NB), "page-2");
    assert_eq!(
        fs::read(nav_path(&nb_dir(&e.lib, NB)).unwrap()).unwrap(),
        nav
    );
    // No secrets or content: ids only.
    let text = String::from_utf8(nav).unwrap();
    assert!(text.starts_with("{\"v\":1,\"notebooks\":{"), "{text}");
    cleanup(e);
}

#[test]
fn missing_malformed_oversized_and_unknown_state_fall_back_to_notebook_meta() {
    let e = env("nav-bad");
    grow(&e.lib, NB, 3);
    let d = nb_dir(&e.lib, NB);
    let m = manifest(&e.lib, NB);
    let path = nav_path(&d).unwrap();
    assert!(!path.exists());
    assert_eq!(selected_page(&d, &m), "page-3", "missing → active_page_id");
    let meta = meta_bytes(&e.lib, NB);
    let big = format!(
        "{{\"v\":1,\"notebooks\":{{\"x\":\"{}\"}}}}",
        "a".repeat(300 * 1024)
    );
    for bad in [
        &b""[..],
        b"{",
        b"\x00\xff",
        b"[]",
        b"{\"v\":2,\"notebooks\":{}}",
        b"{\"v\":1,\"notebooks\":{\"notebook-1\":{\"page\":\"page-1\",\"base\":\"page-3\",\"x\":1}}}",
        b"{\"v\":1,\"notebooks\":{\"notebook-1\":{\"page\":\"page-1\"}}}",
        big.as_bytes(),
    ] {
        fs::write(&path, bad).unwrap();
        assert_eq!(selected_page(&d, &m), "page-3");
        assert_eq!(reopened_page(&e.lib, NB), "page-3");
    }
    // The next save replaces the bad file; notebook.meta was never rewritten.
    remember(&d, &m, "page-1").unwrap();
    assert_eq!(reopened_page(&e.lib, NB), "page-1");
    assert_eq!(meta_bytes(&e.lib, NB), meta);
    cleanup(e);
}

#[test]
fn an_interrupted_save_keeps_the_previous_selection() {
    let e = env("nav-torn");
    grow(&e.lib, NB, 3);
    let d = nb_dir(&e.lib, NB);
    let mut m = manifest(&e.lib, NB);
    navigate(&e.lib, NB, &mut m, "page-2");
    let path = nav_path(&d).unwrap();
    // Crash after the temp write, before the rename: a torn temp file is left behind.
    let tmp = path.with_file_name(format!("{NAV_FILE}.tmp"));
    fs::write(&tmp, b"{\"v\":1,\"noteb").unwrap();
    assert_eq!(reopened_page(&e.lib, NB), "page-2");
    // A torn final file (no fsync on a convenience write) falls back to notebook.meta.
    let good = fs::read(&path).unwrap();
    fs::write(&path, &good[..good.len() / 2]).unwrap();
    assert_eq!(reopened_page(&e.lib, NB), "page-3");
    // The next save recovers both.
    navigate(&e.lib, NB, &mut m, "page-1");
    assert_eq!(reopened_page(&e.lib, NB), "page-1");
    assert!(!tmp.exists());
    cleanup(e);
}

#[test]
fn a_failed_save_does_not_fail_navigation_or_touch_the_notebook() {
    let e = env("nav-fail");
    grow(&e.lib, NB, 2);
    let d = nb_dir(&e.lib, NB);
    let mut m = manifest(&e.lib, NB);
    let meta = meta_bytes(&e.lib, NB);
    let gen0 = read_mutation_gen(&e.lib).unwrap();
    // A directory where the file should be makes every save fail.
    fs::create_dir_all(nav_path(&d).unwrap()).unwrap();
    persist_active_page(&d, &mut m, "page-1").unwrap();
    assert_eq!(meta_bytes(&e.lib, NB), meta);
    assert_eq!(read_mutation_gen(&e.lib).unwrap(), gen0);
    assert_eq!(reopened_page(&e.lib, NB), "page-2");
    // An unknown page is still refused.
    assert!(persist_active_page(&d, &mut m, "page-9").is_err());
    cleanup(e);
}

#[test]
fn stale_selections_fall_back_without_rewriting_the_notebook() {
    let e = env("nav-stale");
    grow(&e.lib, NB, 4);
    let d = nb_dir(&e.lib, NB);
    let mut m = manifest(&e.lib, NB);
    // canonical active stays page-4; the user browses to page-2.
    navigate(&e.lib, NB, &mut m, "page-2");
    // A remote deletion of page-2 (the inbound primitive keeps the canonical active page).
    remove_page_durable(&d, &mut m, "page-2").unwrap();
    assert_eq!(m.active_page_id, "page-4");
    let meta = meta_bytes(&e.lib, NB);
    let generation = read_mutation_gen(&e.lib).unwrap();
    assert_eq!(reopened_page(&e.lib, NB), "page-4");
    assert_eq!(
        meta_bytes(&e.lib, NB),
        meta,
        "open must not correct the notebook"
    );
    assert_eq!(read_mutation_gen(&e.lib).unwrap(), generation);
    // A remote removal of the canonical active page moves it; that newer choice wins.
    navigate(&e.lib, NB, &mut m, "page-1");
    remove_page_durable(&d, &mut m, "page-4").unwrap();
    assert_ne!(m.active_page_id, "page-4");
    assert_eq!(reopened_page(&e.lib, NB), m.active_page_id);
    cleanup(e);
}

#[test]
fn page_create_insert_delete_and_reorder_keep_the_shown_page() {
    let e = env("nav-structure");
    grow(&e.lib, NB, 3);
    let d = nb_dir(&e.lib, NB);
    let mut m = manifest(&e.lib, NB);
    navigate(&e.lib, NB, &mut m, "page-1");
    // Insert after the shown page: the new page is shown (canonical active), as before.
    let ins = insert_page_after_durable(&d, &mut m, "page-1", &InsertPageHooks::default())
        .unwrap()
        .inserted_page_id;
    assert_eq!(reopened_page(&e.lib, NB), ins);
    navigate(&e.lib, NB, &mut m, "page-2");
    // Reorder: no change of shown page.
    let mut order = m.page_ids.clone();
    order.reverse();
    reorder_pages_durable(&d, &mut m, &order).unwrap();
    assert_eq!(reopened_page(&e.lib, NB), "page-2");
    // Deleting the shown page shows its successor.
    let del = delete_page_durable(&d, &mut m, "page-2", &DeletePageHooks::default()).unwrap();
    assert_eq!(reopened_page(&e.lib, NB), del.successor_page_id);
    // "Next" past the last page creates one and shows it.
    navigate(&e.lib, NB, &mut m, "page-1");
    let new_id = allocate_page_id(&m);
    let last = m.page_ids.last().unwrap().clone();
    create_page_files_from_template(&d, &new_id, &last).unwrap();
    commit_new_page_to_manifest(&d, &mut m, &new_id).unwrap();
    assert_eq!(reopened_page(&e.lib, NB), new_id);
    cleanup(e);
}

#[test]
fn deleted_notebooks_are_pruned_on_the_next_save() {
    let e = env("nav-prune");
    let nb2 = second_notebook(&e.lib);
    grow(&e.lib, NB, 2);
    grow(&e.lib, &nb2, 2);
    let mut m2 = manifest(&e.lib, &nb2);
    navigate(&e.lib, &nb2, &mut m2, "page-1");
    let path = nav_path(&nb_dir(&e.lib, NB)).unwrap();
    assert!(fs::read_to_string(&path).unwrap().contains(&nb2));
    let mut repo = open_library_read_only(&e.lib).unwrap();
    delete_notebook_durable(&mut repo, &nb2, &DeleteNotebookHooks::default()).unwrap();
    let mut m1 = manifest(&e.lib, NB);
    navigate(&e.lib, NB, &mut m1, "page-1");
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains(&nb2), "{text}");
    assert!(text.contains(NB));
    cleanup(e);
}

#[test]
fn concurrent_saves_keep_every_notebook() {
    let e = env("nav-concurrent");
    let nbs: Vec<String> = (0..3).map(|_| second_notebook(&e.lib)).collect();
    for nb in &nbs {
        grow(&e.lib, nb, 3);
    }
    let lib = Arc::new(e.lib.clone());
    let handles: Vec<_> = nbs
        .iter()
        .cloned()
        .map(|nb| {
            let lib = Arc::clone(&lib);
            std::thread::spawn(move || {
                let d = nb_dir(&lib, &nb);
                let mut m = manifest(&lib, &nb);
                for i in 0..30 {
                    let p = format!("page-{}", 1 + i % 3);
                    persist_active_page(&d, &mut m, &p).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    for nb in &nbs {
        assert_eq!(reopened_page(&e.lib, nb), "page-3", "{nb}");
    }
    cleanup(e);
}

// ------------------------------------------------------------------ version skew

/// What an older build does on a page switch: rewrite `notebook.meta` (admitted mutation).
fn legacy_switch(lib: &Path, nb: &str, page: &str) {
    let mut m = manifest(lib, nb);
    m.active_page_id = page.to_string();
    write_manifest_atomic_with(&nb_dir(lib, nb), &m, &ManifestWriteHooks::default()).unwrap();
}

#[test]
fn upgrade_downgrade_and_re_upgrade_show_the_latest_choice() {
    let e = env("nav-skew");
    grow(&e.lib, NB, 4);
    let d = nb_dir(&e.lib, NB);
    // Upgrade: no local state yet → notebook.meta's page (written by the old build).
    legacy_switch(&e.lib, NB, "page-2");
    assert_eq!(reopened_page(&e.lib, NB), "page-2");
    // New build browses to page-3; notebook.meta still names page-2.
    let mut m = manifest(&e.lib, NB);
    navigate(&e.lib, NB, &mut m, "page-3");
    assert_eq!(manifest(&e.lib, NB).active_page_id, "page-2");
    // Downgrade: the old build opens page-2 (an older position, contents untouched) and
    // switches to page-4.
    legacy_switch(&e.lib, NB, "page-4");
    // Re-upgrade: the old build's newer choice wins over the remembered page-3.
    assert_eq!(reopened_page(&e.lib, NB), "page-4");
    let mut m = manifest(&e.lib, NB);
    navigate(&e.lib, NB, &mut m, "page-1");
    assert_eq!(reopened_page(&e.lib, NB), "page-1");
    assert_eq!(selected_page(&d, &manifest(&e.lib, NB)), "page-1");
    cleanup(e);
}

#[test]
fn a_notebook_outside_a_library_keeps_the_legacy_canonical_write() {
    let d = bare_notebook("nav-bare");
    let g = geo();
    let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
    let mut m: PersistedNotebookManifestV1 =
        serde_json::from_slice(&fs::read(d.join("notebook.meta")).unwrap()).unwrap();
    insert_page_after_durable(&d, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    load_page_for_switch(&d, "page-1", &profile, &g).unwrap();
    persist_active_page(&d, &mut m, "page-1").unwrap();
    assert!(nav_path(&d).is_none());
    let on_disk: PersistedNotebookManifestV1 =
        serde_json::from_slice(&fs::read(d.join("notebook.meta")).unwrap()).unwrap();
    assert_eq!(on_disk.active_page_id, "page-1");
    assert!(!d.join(SYNC_DIR).exists());
    let _ = fs::remove_dir_all(d.parent().unwrap());
}

// ------------------------------------------------------------------ notebook selection (19B.3D.2)

/// Home → open `nb`, as `open_notebook_from_ui` does: switch, then open the selected notebook.
fn open_from_home(repo: &mut LibraryRepository, nb: &str) -> (PersistedNotebookManifestV1, String) {
    switch_notebook_durable(repo, nb, &SwitchNotebookHooks::default()).unwrap();
    assert_eq!(repo.active_notebook_id(), nb);
    let g = geo();
    let (_, opened) =
        open_active_notebook(repo, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    assert_eq!(opened.manifest.notebook_id, nb);
    (opened.manifest, opened.page_id)
}

fn library_meta(lib: &Path) -> Vec<u8> {
    fs::read(lib.join("library.meta")).unwrap()
}

fn disk_active(lib: &Path) -> String {
    read_library_manifest_for_test(lib)
        .unwrap()
        .active_notebook_id
}

/// What a fresh process selects: (notebook, page in that notebook).
fn restart_selection(lib: &Path) -> (String, String) {
    let g = geo();
    let o = open_or_create_library(lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    (
        o.repository.active_notebook_id().to_string(),
        o.notebook.page_id,
    )
}

/// An older build's Home switch: rewrite `library.meta` (admitted mutation).
fn legacy_notebook_switch(lib: &Path, nb: &str) {
    let mut m = read_library_manifest_for_test(lib).unwrap();
    m.active_notebook_id = nb.to_string();
    write_library_manifest_for_test(lib, &m).unwrap();
}

/// Library with notebook A (`notebook-1`, 3 pages) and B (3 pages); `library.meta` names B.
fn two_notebooks(label: &str) -> (Env, String) {
    let e = env(label);
    let b = second_notebook(&e.lib);
    grow(&e.lib, NB, 3);
    grow(&e.lib, &b, 3);
    assert_eq!(disk_active(&e.lib), b);
    (e, b)
}

#[test]
fn browsing_across_notebooks_keeps_the_verified_fast_path() {
    let (e, b) = two_notebooks("nav-notebooks");
    let a = NB.to_string();
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    stop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert_eq!(s.reconcile_path(), Some(FastPathReason::Verified));
    let gen0 = read_mutation_gen(&e.lib).unwrap();
    let stamp0 = fs::read(e.sync.join(STAMP_FILE)).unwrap();
    let (census0, _) = library_census_map(&e.lib).unwrap();
    let lib_meta0 = library_meta(&e.lib);
    let metas0 = (meta_bytes(&e.lib, &a), meta_bytes(&e.lib, &b));
    let ledger0 = read_opt(&ledger_path(&e.lib));
    let snap0 = local_snapshot(&e.lib);
    let (state0, pending0) = (s.state(), s.pending());

    let mut repo = open_library_read_only(&e.lib).unwrap();
    // 1–3: open A from Home, browse, back to Home.
    let (mut ma, _) = open_from_home(&mut repo, &a);
    for p in ["page-2", "page-3", "page-1", "page-2"] {
        navigate(&e.lib, &a, &mut ma, p);
    }
    // 4–5: open B, browse.
    let (mut mb, _) = open_from_home(&mut repo, &b);
    for p in ["page-1", "page-3"] {
        navigate(&e.lib, &b, &mut mb, p);
    }
    // 6: back to A: its page is restored.
    let (_, page) = open_from_home(&mut repo, &a);
    assert_eq!(page, "page-2");
    assert_eq!(s.pending(), pending0, "browsing queued sync work");
    stop(s);

    // Nothing canonical moved.
    assert_eq!(read_mutation_gen(&e.lib).unwrap(), gen0);
    assert_eq!(fs::read(e.sync.join(STAMP_FILE)).unwrap(), stamp0);
    assert_eq!(library_census_map(&e.lib).unwrap().0, census0);
    assert_eq!(library_meta(&e.lib), lib_meta0);
    assert_eq!((meta_bytes(&e.lib, &a), meta_bytes(&e.lib, &b)), metas0);
    assert_eq!(read_opt(&ledger_path(&e.lib)), ledger0, "ledger appended");

    // 7: restart: A on page-2 (B keeps page-3), verified fast path, same sync state.
    assert_eq!(restart_selection(&e.lib), (a.clone(), "page-2".to_string()));
    assert_eq!(reopened_page(&e.lib, &b), "page-3");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.reconcile_path(), Some(FastPathReason::Verified));
    let st = s.state();
    assert_eq!(st.server_cursor, state0.server_cursor);
    assert_eq!(st.acked_client_seq, state0.acked_client_seq);
    assert_eq!(s.pending(), pending0);
    assert_eq!(local_snapshot(&e.lib), snap0);
    stop(s);
    cleanup(e);
}

#[test]
fn genuine_library_changes_after_browsing_keep_their_mutations() {
    let (e, b) = two_notebooks("nav-nb-genuine");
    let a = NB.to_string();
    let mut repo = open_library_read_only(&e.lib).unwrap();
    open_from_home(&mut repo, &a);
    let ledger_len = || fs::read(ledger_path(&e.lib)).map_or(0, |v| v.len());
    let added_since = |from: usize| {
        String::from_utf8(fs::read(ledger_path(&e.lib)).unwrap()[from..].to_vec()).unwrap()
    };

    // A stroke after browsing: one page scope, no structure scope.
    crate::sync_verified::clear_registry_for(&e.lib);
    let (g0, l0) = (read_mutation_gen(&e.lib).unwrap(), ledger_len());
    append_stroke(&e.lib, &a, "page-1", &[(30.0, 30.0), (300.0, 300.0)]);
    assert_eq!(read_mutation_gen(&e.lib).unwrap(), g0 + 1);
    let added = added_since(l0);
    assert!(added.contains("\"k\":\"page\""), "{added}");
    assert!(!added.contains("\"k\":\"structure\""), "{added}");
    assert!(!added.contains("\"k\":\"notebook\""), "{added}");

    // Rename: notebook.info only; selection and library.meta unchanged.
    crate::sync_verified::clear_registry_for(&e.lib);
    let (g0, meta) = (read_mutation_gen(&e.lib).unwrap(), library_meta(&e.lib));
    rename_notebook_durable(&mut repo, &a, "Renamed", &RenameNotebookHooks::default()).unwrap();
    assert_eq!(read_mutation_gen(&e.lib).unwrap(), g0 + 1);
    assert_eq!(library_meta(&e.lib), meta);
    assert_eq!(repo.active_notebook_id(), a);

    // Reorder: a structural write; it persists the current selection as is.
    crate::sync_verified::clear_registry_for(&e.lib);
    let (g0, l0) = (read_mutation_gen(&e.lib).unwrap(), ledger_len());
    let mut order = repo.manifest().notebook_ids.clone();
    order.reverse();
    reorder_notebooks_durable(&mut repo, &order).unwrap();
    assert_eq!(read_mutation_gen(&e.lib).unwrap(), g0 + 1);
    assert!(added_since(l0).contains("\"k\":\"structure\""));
    assert_eq!(disk_active(&e.lib), a);
    assert_eq!(restart_selection(&e.lib).0, a);

    // Create: the new notebook is selected and canonical, as before.
    crate::sync_verified::clear_registry_for(&e.lib);
    let g0 = read_mutation_gen(&e.lib).unwrap();
    let c = second_notebook_in(&mut repo);
    assert_eq!(read_mutation_gen(&e.lib).unwrap(), g0 + 1);
    assert_eq!(repo.active_notebook_id(), c);
    assert_eq!(disk_active(&e.lib), c);
    assert_eq!(restart_selection(&e.lib).0, c);

    // Delete the selected notebook: its successor is selected and written.
    open_from_home(&mut repo, &b);
    assert_eq!(disk_active(&e.lib), c);
    crate::sync_verified::clear_registry_for(&e.lib);
    let g0 = read_mutation_gen(&e.lib).unwrap();
    let del = delete_notebook_durable(&mut repo, &b, &DeleteNotebookHooks::default()).unwrap();
    assert_eq!(read_mutation_gen(&e.lib).unwrap(), g0 + 1);
    assert_eq!(disk_active(&e.lib), del.active_notebook_id);
    assert_eq!(restart_selection(&e.lib).0, del.active_notebook_id);
    cleanup(e);
}

#[test]
fn offline_edits_after_browsing_still_reach_sync() {
    let (e, _b) = two_notebooks("nav-nb-sync");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let pending0 = s.pending();
    stop(s);
    let mut repo = open_library_read_only(&e.lib).unwrap();
    open_from_home(&mut repo, NB);
    append_stroke(&e.lib, NB, "page-2", &[(30.0, 30.0), (300.0, 300.0)]);
    let c = second_notebook_in(&mut repo);
    crate::sync_verified::clear_registry_for(&e.lib);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert!(!s.reconcile_path().unwrap().is_fast());
    assert!(
        s.pending() >= pending0 + 2,
        "stroke + create must be queued"
    );
    stop(s);
    assert_eq!(restart_selection(&e.lib).0, c);
    cleanup(e);
}

#[test]
fn missing_corrupt_and_stale_notebook_selection_falls_back_to_library_meta() {
    let (e, b) = two_notebooks("nav-nb-bad");
    let path = active_notebook_path(&e.lib);
    let meta = library_meta(&e.lib);
    assert!(!path.exists());
    assert_eq!(restart_selection(&e.lib).0, b, "missing → library.meta");
    for bad in [
        &b""[..],
        b"{",
        b"[]",
        b"{\"v\":2,\"notebook\":\"notebook-1\",\"base\":\"notebook-2\"}",
        b"{\"v\":1,\"notebook\":\"notebook-1\"}",
        b"{\"v\":1,\"notebook\":\"notebook-1\",\"base\":\"notebook-2\",\"x\":1}",
        b"{\"v\":1,\"notebook\":\"notebook-99\",\"base\":\"notebook-2\"}",
    ] {
        fs::write(&path, bad).unwrap();
        assert_eq!(restart_selection(&e.lib).0, b);
    }
    // A torn temp file from an interrupted save is ignored.
    let mut repo = open_library_read_only(&e.lib).unwrap();
    open_from_home(&mut repo, NB);
    fs::write(
        path.with_file_name(format!("{ACTIVE_NOTEBOOK_FILE}.tmp")),
        b"{\"v\"",
    )
    .unwrap();
    assert_eq!(restart_selection(&e.lib).0, NB);
    // A failed save does not fail the switch and never writes library.meta.
    let hooks = SwitchNotebookHooks {
        manifest: LibraryManifestWriteHooks {
            fail_write: true,
            ..Default::default()
        },
    };
    switch_notebook_durable(&mut repo, &b, &hooks).unwrap();
    assert_eq!(repo.active_notebook_id(), b);
    assert_eq!(restart_selection(&e.lib).0, NB, "previous selection kept");
    assert_eq!(library_meta(&e.lib), meta);
    cleanup(e);
}

#[test]
fn inbound_removal_keeps_or_moves_the_selection() {
    let (e, b) = two_notebooks("nav-nb-inbound");
    let c = second_notebook(&e.lib); // library.meta names C
    let mut repo = open_library_read_only(&e.lib).unwrap();
    open_from_home(&mut repo, NB);
    // Inbound deletes C (the canonical notebook, not the selected one): materialization opens
    // its own repository, so the write carries the selection.
    let mut inbound = open_library_read_only(&e.lib).unwrap();
    assert_eq!(inbound.active_notebook_id(), NB);
    delete_notebook_durable(&mut inbound, &c, &DeleteNotebookHooks::default()).unwrap();
    assert_eq!(restart_selection(&e.lib).0, NB);
    // Inbound deletes the selected notebook: a successor is selected.
    let mut inbound = open_library_read_only(&e.lib).unwrap();
    delete_notebook_durable(&mut inbound, NB, &DeleteNotebookHooks::default()).unwrap();
    assert_eq!(restart_selection(&e.lib).0, b);
    cleanup(e);
}

#[test]
fn notebook_selection_upgrade_downgrade_and_re_upgrade() {
    let (e, b) = two_notebooks("nav-nb-skew");
    let a = NB.to_string();
    let page_file = nav_path(&nb_dir(&e.lib, &a)).unwrap();
    // Upgrade: no local state → library.meta (B, from the old build).
    assert_eq!(restart_selection(&e.lib).0, b);
    // New build opens A and browses: library.meta still names B; page file untouched by the
    // notebook selection (19B.3D.1 readers keep their entries).
    let mut repo = open_library_read_only(&e.lib).unwrap();
    let (mut ma, _) = open_from_home(&mut repo, &a);
    navigate(&e.lib, &a, &mut ma, "page-2");
    let pages = fs::read(&page_file).unwrap();
    open_from_home(&mut repo, &b);
    open_from_home(&mut repo, &a);
    assert_eq!(fs::read(&page_file).unwrap(), pages);
    assert_eq!(disk_active(&e.lib), b);
    // Downgrade: the old build opens B (library.meta), then switches to A and back to B.
    legacy_notebook_switch(&e.lib, &a);
    legacy_notebook_switch(&e.lib, &b);
    // Re-upgrade: library.meta's B equals the saved base, so A (the new build's last choice)
    // is shown; the old build's A → B round trip is indistinguishable from no change.
    assert_eq!(restart_selection(&e.lib).0, a);
    // An old-build switch that leaves a different value wins.
    let c = second_notebook(&e.lib);
    legacy_notebook_switch(&e.lib, &a);
    legacy_notebook_switch(&e.lib, &c);
    assert_eq!(restart_selection(&e.lib), (c.clone(), "page-1".to_string()));
    // Next new-build switch re-bases.
    let mut repo = open_library_read_only(&e.lib).unwrap();
    open_from_home(&mut repo, &b);
    assert_eq!(restart_selection(&e.lib).0, b);
    assert_eq!(disk_active(&e.lib), c);
    cleanup(e);
}

/// Create a notebook through an existing repository (as the Home UI does).
fn second_notebook_in(repo: &mut LibraryRepository) -> String {
    let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
    let g = geo();
    create_notebook_durable(
        repo,
        &profile,
        &g,
        &paper_background(&g),
        None,
        &CreateNotebookHooks::default(),
    )
    .unwrap()
    .notebook_id
}
