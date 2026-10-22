//! Slice 19B.3C: changed-page ledger (record-only) — coverage, ordering, failure and
//! compatibility tests on disposable libraries.
//!
//! Assertions are per generation (another test's global `clear_registry()` may start a new
//! generation mid-test; that only adds records, never removes them).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notebook_core::{new_stroke_id, Point, Stroke};
use sync_protocol::ReplicaId;
use uuid::Uuid;

use super::confirmed_tests::{apply_all_staged, stage_remote};
use super::tests::{append_stroke, env, geo, nb_dir, stop};
use super::*;
use crate::library::{
    create_notebook_durable, delete_notebook_durable, open_library_read_only,
    read_notebook_manifest_read_only, rename_notebook_durable, reorder_notebooks_durable,
    CreateNotebookHooks, DeleteNotebookHooks, LibraryManifestWriteHooks, RenameNotebookHooks,
};
use crate::notebook::{
    delete_page_durable, insert_page_after_durable, move_page_durable, reorder_pages_durable,
    DeletePageHooks, InsertPageHooks, MovePageDirection, MovePageHooks,
};
use crate::page_ledger::{ledger_path, parse_line, scope_for_dir, LedgerRecordV1, LedgerScope};
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use crate::profile::RenderProfile;
use crate::store::{
    install_background, load_existing_page, page_dir, write_meta, write_page_links,
    write_page_texts, PageStore, PersistedPageHeader, PersistedReplacementV2,
};
use crate::sync_local::{identity_path, SYNC_DIR};
use crate::sync_verified::{
    clear_registry_for, install_invalidate_hooks, permit_sync_mutations, read_mutation_gen,
    InvalidateHooks, MUTATION_GEN_FILE,
};
use crate::ui_gfx::paper_background;

// ------------------------------------------------------------------ helpers

const NB: &str = "notebook-1";

fn page(nb: &str, p: &str) -> LedgerScope {
    LedgerScope::Page {
        nb: nb.into(),
        page: p.into(),
    }
}

fn notebook(nb: &str) -> LedgerScope {
    LedgerScope::Notebook { nb: nb.into() }
}

/// A new process: the per-library session (generation + ledger state) is dropped.
fn new_process(lib: &Path) {
    clear_registry_for(lib);
}

fn raw(lib: &Path) -> String {
    fs::read_to_string(ledger_path(lib)).unwrap_or_default()
}

/// Every line must be a valid record.
fn recs(lib: &Path) -> Vec<LedgerRecordV1> {
    let r = raw(lib);
    assert!(r.is_empty() || r.ends_with('\n'), "torn tail: {r:?}");
    r.lines()
        .map(|l| parse_line(l).unwrap_or_else(|e| panic!("{e}: {r}")))
        .collect()
}

/// Valid records only (torn fragments skipped).
fn valid_recs(lib: &Path) -> Vec<LedgerRecordV1> {
    raw(lib)
        .lines()
        .filter_map(|l| parse_line(l).ok())
        .collect()
}

fn scope_of(r: &LedgerRecordV1) -> Option<LedgerScope> {
    Some(match r.k.as_str() {
        "page" => page(r.nb.as_deref()?, r.page.as_deref()?),
        "notebook" => notebook(r.nb.as_deref()?),
        "structure" => LedgerScope::Structure,
        "all" => LedgerScope::All,
        _ => return None,
    })
}

/// A `session g` record precedes a record of generation `g` covering `scope`.
fn recorded_in(rs: &[LedgerRecordV1], g: u64, scope: &LedgerScope) -> bool {
    let Some(s) = rs.iter().position(|r| r.g == g && r.k == "session") else {
        return false;
    };
    rs[s + 1..]
        .iter()
        .any(|r| r.g == g && scope_of(r).is_some_and(|c| c.covers(scope)))
}

fn cur_gen(lib: &Path) -> u64 {
    read_mutation_gen(lib).unwrap()
}

fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_name() == SYNC_DIR {
                continue;
            }
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(p.strip_prefix(root).unwrap().into(), fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// In a new process run `op`; assert the first durable ledger record set landed while `watch`
/// was still untouched (write-ahead), and that `scopes` are recorded in the new generation.
fn assert_write_ahead(
    lib: &Path,
    watch: &Path,
    scopes: &[LedgerScope],
    expect_change: bool,
    op: impl FnOnce(),
) {
    new_process(lib);
    let before = tree(watch);
    let seen: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
    {
        let (seen, watch, before) = (seen.clone(), watch.to_path_buf(), before.clone());
        let _h = install_invalidate_hooks(InvalidateHooks {
            after_ledger_durable: Some(Arc::new(move || {
                let mut s = seen.lock().unwrap();
                if s.is_none() {
                    *s = Some(tree(&watch) == before);
                }
            })),
            ..Default::default()
        });
        op();
    }
    assert_eq!(
        *seen.lock().unwrap(),
        Some(true),
        "ledger record must be durable before any canonical change"
    );
    if expect_change {
        assert_ne!(tree(watch), before, "operation changed nothing");
    }
    let rs = recs(lib);
    let g = cur_gen(lib);
    for s in scopes {
        assert!(
            recorded_in(&rs, g, s),
            "{s:?} not recorded in g={g}: {rs:?}"
        );
    }
}

fn stroke(pts: &[(f32, f32)]) -> Stroke {
    Stroke {
        id: new_stroke_id(),
        points: pts
            .iter()
            .map(|(x, y)| Point::new(*x, *y).with_pressure(0.5))
            .collect(),
        brush: Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
        finished: true,
    }
}

fn header(dir: &Path) -> PersistedPageHeader {
    serde_json::from_slice(&fs::read(dir.join("page.meta")).unwrap()).unwrap()
}

fn add_page(lib: &Path) -> String {
    let d = nb_dir(lib, NB);
    let mut m = read_notebook_manifest_read_only(&d).unwrap();
    let last = m.page_ids.last().unwrap().clone();
    insert_page_after_durable(&d, &mut m, &last, &InsertPageHooks::default())
        .unwrap()
        .inserted_page_id
}

fn journal(lib: &Path, p: &str) -> Vec<u8> {
    fs::read(page_dir(&nb_dir(lib, NB), p).join("strokes.jsonl")).unwrap()
}

fn try_stroke(lib: &Path, p: &str) -> Result<(), String> {
    let (mut store, _) = load_existing_page(&nb_dir(lib, NB), p)?;
    store
        .append_committed_stroke(&stroke(&[(9.0, 9.0), (90.0, 90.0)]))
        .map(|_| ())
}

fn cleanup(lib: &Path) {
    new_process(lib);
    let _ = fs::remove_dir_all(lib.parent().unwrap());
}

// ------------------------------------------------------------------ format and scopes

#[test]
fn scopes_follow_the_library_layout_and_widen_when_unsure() {
    let root = Path::new("/lib");
    let s = |p: &str| scope_for_dir(root, &root.join(p));
    assert_eq!(
        s("notebooks/notebook-3/page-7"),
        page("notebook-3", "page-7")
    );
    assert_eq!(s("notebooks/notebook-3"), notebook("notebook-3"));
    assert_eq!(
        s("notebooks/notebook-3/.page-trash"),
        notebook("notebook-3")
    );
    assert_eq!(s(""), LedgerScope::Structure);
    assert_eq!(s(".scribe-sync"), LedgerScope::Structure);
    assert_eq!(s(".scribe-folders"), LedgerScope::Structure);
    assert_eq!(s("notebooks"), LedgerScope::All);
    assert_eq!(s("page-1"), LedgerScope::All, "legacy flat layout");
    assert_eq!(
        s("notebooks/Notebook 3"),
        LedgerScope::All,
        "unexpected name"
    );
    assert_eq!(s("notebooks/notebook-3/../x"), LedgerScope::All);
    assert_eq!(
        scope_for_dir(root, Path::new("/elsewhere/notebooks/notebook-1")),
        LedgerScope::All
    );
    assert!(LedgerScope::All.covers(&page("a", "b")));
    assert!(notebook("a").covers(&page("a", "b")));
    assert!(!notebook("a").covers(&page("c", "b")));
    assert!(!LedgerScope::Structure.covers(&notebook("a")));
}

#[test]
fn records_are_strict_versioned_json_lines() {
    for ok in [
        r#"{"v":1,"g":42,"k":"session"}"#,
        r#"{"v":1,"g":42,"k":"page","nb":"notebook-3","page":"page-7"}"#,
        r#"{"v":1,"g":42,"k":"notebook","nb":"notebook-3"}"#,
        r#"{"v":1,"g":42,"k":"structure"}"#,
        r#"{"v":1,"g":42,"k":"all"}"#,
    ] {
        assert!(parse_line(ok).is_ok(), "{ok}");
    }
    for bad in [
        r#"{"v":2,"g":42,"k":"session"}"#,
        r#"{"v":1,"g":42,"k":"session","nb":"notebook-1"}"#,
        r#"{"v":1,"g":42,"k":"page","nb":"notebook-3"}"#,
        r#"{"v":1,"g":42,"k":"page","nb":"../x","page":"page-1"}"#,
        r#"{"v":1,"g":42,"k":"pages"}"#,
        r#"{"v":1,"g":42,"k":"all","extra":1}"#,
        r#"{"v":1,"g":42,"k":"#,
        "",
    ] {
        assert!(parse_line(bad).is_err(), "{bad}");
    }
}

// ------------------------------------------------------------------ coverage (1–4)

#[test]
fn every_page_mutation_records_its_page_before_writing() {
    let e = env("ledger-page-ops");
    let lib = &e.lib;
    let dir = page_dir(&nb_dir(lib, NB), "page-1");
    let p1 = page(NB, "page-1");
    let first = |lib: &Path| load_existing_page(&nb_dir(lib, NB), "page-1").unwrap();

    assert_write_ahead(lib, &dir, std::slice::from_ref(&p1), true, || {
        append_stroke(lib, NB, "page-1", &[(1.0, 1.0), (20.0, 20.0)]);
    });
    assert_write_ahead(lib, &dir, std::slice::from_ref(&p1), true, || {
        let (mut s, _) = first(lib);
        s.append_committed_strokes(&[stroke(&[(3.0, 3.0), (30.0, 30.0)])])
            .unwrap();
    });
    assert_write_ahead(lib, &dir, std::slice::from_ref(&p1), true, || {
        let (mut s, loaded) = first(lib);
        s.append_delete_stroke(&loaded.strokes[0].id).unwrap();
    });
    assert_write_ahead(lib, &dir, std::slice::from_ref(&p1), true, || {
        let (mut s, loaded) = first(lib);
        let target = loaded.strokes.last().unwrap().id.as_str().to_string();
        s.append_replace_strokes(&[PersistedReplacementV2 {
            target_stroke_id: target,
            fragments: Vec::new(),
        }])
        .unwrap();
    });
    assert_write_ahead(lib, &dir, std::slice::from_ref(&p1), false, || {
        write_page_texts(&dir, &[]).unwrap();
    });
    assert_write_ahead(lib, &dir, std::slice::from_ref(&p1), false, || {
        write_page_links(&dir, &[]).unwrap();
    });
    assert_write_ahead(lib, &dir, std::slice::from_ref(&p1), false, || {
        write_meta(&dir, &header(&dir)).unwrap();
    });
    assert_write_ahead(lib, &dir, std::slice::from_ref(&p1), false, || {
        let px = fs::read(dir.join("background.raw")).unwrap();
        let mut h = header(&dir);
        install_background(&dir, &px, &mut h).unwrap();
    });
    cleanup(lib);
}

#[test]
fn notebook_and_structure_mutations_record_their_scope_before_writing() {
    let e = env("ledger-structure-ops");
    let lib = &e.lib;
    let nbd = nb_dir(lib, NB);
    let nb1 = notebook(NB);
    let manifest = || read_notebook_manifest_read_only(&nbd).unwrap();
    let repo = || open_library_read_only(lib).unwrap();

    assert_write_ahead(lib, &nbd, std::slice::from_ref(&nb1), true, || {
        let mut m = manifest();
        insert_page_after_durable(&nbd, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    });
    assert_write_ahead(lib, &nbd, std::slice::from_ref(&nb1), true, || {
        let mut m = manifest();
        let p = m.page_ids[1].clone();
        move_page_durable(
            &nbd,
            &mut m,
            &p,
            MovePageDirection::Left,
            &MovePageHooks::default(),
        )
        .unwrap();
    });
    assert_write_ahead(lib, &nbd, std::slice::from_ref(&nb1), true, || {
        let mut m = manifest();
        let mut order = m.page_ids.clone();
        order.reverse();
        reorder_pages_durable(&nbd, &mut m, &order).unwrap();
    });
    assert_write_ahead(lib, &nbd, std::slice::from_ref(&nb1), true, || {
        let mut m = manifest();
        let p = m.page_ids[0].clone();
        delete_page_durable(&nbd, &mut m, &p, &DeletePageHooks::default()).unwrap();
    });
    assert_write_ahead(lib, &nbd, std::slice::from_ref(&nb1), true, || {
        rename_notebook_durable(&mut repo(), NB, "Renamed", &RenameNotebookHooks::default())
            .unwrap();
    });
    let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
    let g = geo();
    assert_write_ahead(lib, lib, &[LedgerScope::Structure], true, || {
        create_notebook_durable(
            &mut repo(),
            &profile,
            &g,
            &paper_background(&g),
            None,
            &CreateNotebookHooks::default(),
        )
        .unwrap();
    });
    // The new notebook's own files are recorded too (its first page creation).
    let rs = recs(lib);
    assert!(
        recorded_in(&rs, cur_gen(lib), &notebook("notebook-2")),
        "{rs:?}"
    );
    assert_write_ahead(lib, lib, &[LedgerScope::Structure], true, || {
        let mut r = repo();
        let mut order = r.manifest().notebook_ids.clone();
        order.reverse();
        reorder_notebooks_durable(&mut r, &order).unwrap();
    });
    assert_write_ahead(
        lib,
        lib,
        &[LedgerScope::Structure, notebook("notebook-2")],
        true,
        || {
            delete_notebook_durable(&mut repo(), "notebook-2", &DeleteNotebookHooks::default())
                .unwrap();
        },
    );
    assert_write_ahead(lib, lib, &[LedgerScope::Structure], false, || {
        let f = crate::folders::read_folders(lib).unwrap();
        crate::folders::write_materialized(lib, &f).unwrap();
    });
    // Identity persistence lives under .scribe-sync (excluded from `tree`): check the scope.
    new_process(lib);
    crate::sync_local::ensure_sync_identity(lib).unwrap();
    assert!(recorded_in(
        &recs(lib),
        cur_gen(lib),
        &LedgerScope::Structure
    ));
    cleanup(lib);
}

#[test]
fn recovery_and_sync_authorized_writes_are_recorded() {
    let e = env("ledger-recovery");
    let lib = &e.lib;
    let dir = page_dir(&nb_dir(lib, NB), "page-1");
    // Interrupted-append recovery: the torn journal tail is truncated only after the record.
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(dir.join("strokes.jsonl"))
        .unwrap();
    f.write_all(b"{\"v\":2,\"type\":\"stroke\",\"id\":\"s-torn")
        .unwrap();
    drop(f);
    assert_write_ahead(lib, &dir, &[page(NB, "page-1")], true, || {
        load_existing_page(&nb_dir(lib, NB), "page-1").unwrap();
    });
    // Sync/recovery-authorized mutation (permit) records like an ordinary one.
    assert_write_ahead(lib, &dir, &[page(NB, "page-1")], false, || {
        permit_sync_mutations(|| write_page_texts(&dir, &[])).unwrap();
    });
    // Recovery's planned-identity publish.
    add_page(lib);
    new_process(lib);
    let (from, plan) = crate::sync_local::plan_sync_identity(lib).unwrap();
    crate::sync_local::persist_planned_identity(lib, from.as_ref(), &plan).unwrap();
    assert!(recorded_in(
        &recs(lib),
        cur_gen(lib),
        &LedgerScope::Structure
    ));
    cleanup(lib);
}

#[test]
fn inbound_materialization_records_the_page_it_writes() {
    let e = env("ledger-inbound");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let replica = ReplicaId::from_uuid(Uuid::now_v7()).unwrap();
    let mut client = 0;
    let before = journal(&e.lib, "page-1");
    stage_remote(&s, replica, &mut client, 2);
    apply_all_staged(&s);
    assert_ne!(
        journal(&e.lib, "page-1"),
        before,
        "inbound strokes materialized"
    );
    let g = cur_gen(&e.lib);
    stop(s);
    assert!(recorded_in(&recs(&e.lib), g, &page(NB, "page-1")));
    cleanup(&e.lib);
}

// ------------------------------------------------------------------ identity guard (5)

#[test]
fn identity_writer_holds_its_admit_until_the_file_is_written() {
    let e = env("ledger-identity-guard");
    new_process(&e.lib);
    let at_drop: Arc<Mutex<Vec<bool>>> = Arc::default();
    {
        let (at_drop, p) = (at_drop.clone(), identity_path(&e.lib));
        let _h = install_invalidate_hooks(InvalidateHooks {
            on_admit_drop: Some(Arc::new(move || at_drop.lock().unwrap().push(p.exists()))),
            ..Default::default()
        });
        assert!(!identity_path(&e.lib).exists());
        crate::sync_local::ensure_sync_identity(&e.lib).unwrap();
    }
    let v = at_drop.lock().unwrap().clone();
    assert!(
        !v.is_empty() && v.iter().all(|x| *x),
        "admit dropped early: {v:?}"
    );
    cleanup(&e.lib);
}

#[test]
fn batched_stroke_append_holds_its_admit_until_the_journal_is_written() {
    let e = env("ledger-batch-guard");
    new_process(&e.lib);
    let before = journal(&e.lib, "page-1").len();
    let at_drop: Arc<Mutex<Vec<bool>>> = Arc::default();
    {
        let at_drop = at_drop.clone();
        let p = page_dir(&nb_dir(&e.lib, NB), "page-1").join("strokes.jsonl");
        let _h = install_invalidate_hooks(InvalidateHooks {
            on_admit_drop: Some(Arc::new(move || {
                at_drop
                    .lock()
                    .unwrap()
                    .push(fs::metadata(&p).unwrap().len() as usize > before)
            })),
            ..Default::default()
        });
        let (mut s, _) = load_existing_page(&nb_dir(&e.lib, NB), "page-1").unwrap();
        s.append_committed_strokes(&[stroke(&[(5.0, 5.0), (50.0, 50.0)])])
            .unwrap();
    }
    let v = at_drop.lock().unwrap().clone();
    assert!(
        !v.is_empty() && v.iter().all(|x| *x),
        "admit dropped early: {v:?}"
    );
    cleanup(&e.lib);
}

// ------------------------------------------------------------------ dedup and concurrency (6–8)

#[test]
fn repeated_writes_to_one_page_record_it_once_per_generation() {
    let e = env("ledger-repeat");
    new_process(&e.lib);
    try_stroke(&e.lib, "page-1").unwrap();
    let len_after_first = raw(&e.lib).len();
    for _ in 0..5 {
        try_stroke(&e.lib, "page-1").unwrap();
    }
    let rs = recs(&e.lib);
    let g = cur_gen(&e.lib);
    let n = rs
        .iter()
        .filter(|r| r.g == g && scope_of(r) == Some(page(NB, "page-1")))
        .count();
    assert_eq!(n, 1, "{rs:?}");
    if rs.iter().filter(|r| r.k == "session" && r.g == g).count() == 1 {
        assert_eq!(
            raw(&e.lib).len(),
            len_after_first,
            "no further ledger writes"
        );
    }
    cleanup(&e.lib);
}

#[test]
fn first_writes_to_two_pages_record_both() {
    let e = env("ledger-two-pages");
    let p2 = add_page(&e.lib);
    new_process(&e.lib);
    try_stroke(&e.lib, "page-1").unwrap();
    try_stroke(&e.lib, &p2).unwrap();
    let rs = recs(&e.lib);
    let g = cur_gen(&e.lib);
    assert!(recorded_in(&rs, g, &page(NB, "page-1")));
    assert!(recorded_in(&rs, g, &page(NB, &p2)));
    let session = rs.iter().filter(|r| r.g == g && r.k == "session").count();
    assert_eq!(session, 1, "one session record per generation: {rs:?}");
    cleanup(&e.lib);
}

#[test]
fn concurrent_writers_record_each_scope_once_with_one_session() {
    let e = env("ledger-concurrent");
    let pages: Vec<String> = std::iter::once("page-1".to_string())
        .chain((0..3).map(|_| add_page(&e.lib)))
        .collect();
    new_process(&e.lib);
    let lib = Arc::new(e.lib.clone());
    let threads: Vec<_> = pages
        .iter()
        .cloned()
        .chain(std::iter::repeat_n("page-1".to_string(), 2))
        .map(|p| {
            let lib = lib.clone();
            std::thread::spawn(move || {
                for _ in 0..4 {
                    try_stroke(&lib, &p).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let rs = recs(&e.lib);
    let mut per: BTreeMap<(u64, String), usize> = BTreeMap::new();
    for r in &rs {
        *per.entry((r.g, format!("{r:?}"))).or_default() += 1;
    }
    assert!(per.values().all(|n| *n == 1), "duplicate record: {rs:?}");
    for p in &pages {
        assert!(
            rs.iter().any(|r| recorded_in(&rs, r.g, &page(NB, p))),
            "{p} unrecorded: {rs:?}"
        );
    }
    cleanup(&e.lib);
}

// ------------------------------------------------------------------ ordering (9)

#[test]
fn session_record_follows_the_durable_bump_and_creates_the_ledger() {
    let e = env("ledger-order");
    fs::remove_file(ledger_path(&e.lib)).unwrap();
    new_process(&e.lib);
    let g0 = cur_gen(&e.lib);
    let seen: Arc<Mutex<Option<(u64, String, usize)>>> = Arc::default();
    {
        let (probe, lib) = (seen.clone(), e.lib.clone());
        let _h = install_invalidate_hooks(InvalidateHooks {
            after_ledger_durable: Some(Arc::new(move || {
                let j = journal(&lib, "page-1").len();
                *probe.lock().unwrap() = Some((cur_gen(&lib), raw(&lib), j));
            })),
            ..Default::default()
        });
        let before = journal(&e.lib, "page-1").len();
        try_stroke(&e.lib, "page-1").unwrap();
        let (g_at, ledger_at, j_at) = seen.lock().unwrap().clone().unwrap();
        assert_eq!(g_at, g0 + 1, "generation bumped before the session record");
        assert_eq!(
            ledger_at,
            format!(
                "{{\"v\":1,\"g\":{g_at},\"k\":\"session\"}}\n\
                 {{\"v\":1,\"g\":{g_at},\"k\":\"page\",\"nb\":\"{NB}\",\"page\":\"page-1\"}}\n"
            ),
            "exact v1 bytes"
        );
        assert_eq!(j_at, before, "content not yet written");
    }
    cleanup(&e.lib);
}

// ------------------------------------------------------------------ failures (10–14)

#[test]
fn torn_session_append_refuses_the_write_and_marks_the_generation_all() {
    let e = env("ledger-torn-session");
    new_process(&e.lib);
    let g0 = cur_gen(&e.lib);
    let j0 = journal(&e.lib, "page-1");
    let before = raw(&e.lib);
    {
        let _h = install_invalidate_hooks(InvalidateHooks {
            torn_ledger_append: Some(9),
            ..Default::default()
        });
        let err = try_stroke(&e.lib, "page-1").unwrap_err();
        assert!(err.contains("REFUSE PAGE LEDGER"), "{err}");
    }
    assert_eq!(journal(&e.lib, "page-1"), j0, "canonical write refused");
    assert_eq!(cur_gen(&e.lib), g0 + 1, "bump preserved (as today)");
    assert!(!raw(&e.lib).ends_with('\n'), "torn fragment left in place");
    assert!(raw(&e.lib).starts_with(&before), "never truncated");
    // Retry in the same process: fragment isolated on its own line, then session + all.
    try_stroke(&e.lib, "page-1").unwrap();
    let r = raw(&e.lib);
    let tail: Vec<&str> = r[before.len()..].lines().collect();
    assert_eq!(tail.len(), 3, "{tail:?}");
    assert!(parse_line(tail[0]).is_err(), "fragment stays unparseable");
    let g = g0 + 1;
    assert_eq!(parse_line(tail[1]).unwrap().k, "session");
    assert_eq!(
        parse_line(tail[2]).unwrap(),
        parse_line(&format!(r#"{{"v":1,"g":{g},"k":"all"}}"#)).unwrap()
    );
    assert!(r.ends_with('\n'));
    cleanup(&e.lib);
}

#[test]
fn failed_scope_fsync_refuses_the_write_and_widens_the_generation() {
    let e = env("ledger-scope-fsync");
    let p2 = add_page(&e.lib);
    new_process(&e.lib);
    try_stroke(&e.lib, "page-1").unwrap();
    let j2 = journal(&e.lib, &p2);
    {
        let _h = install_invalidate_hooks(InvalidateHooks {
            fail_ledger_fsync: true,
            ..Default::default()
        });
        assert!(try_stroke(&e.lib, &p2).is_err());
    }
    assert_eq!(journal(&e.lib, &p2), j2, "canonical write refused");
    try_stroke(&e.lib, &p2).unwrap();
    let rs = valid_recs(&e.lib);
    let g = cur_gen(&e.lib);
    assert!(rs.iter().any(|r| r.g == g && r.k == "all"), "{rs:?}");
    assert!(recorded_in(&rs, g, &page(NB, &p2)));
    cleanup(&e.lib);
}

#[test]
fn ledger_creation_and_directory_sync_failures_refuse_the_write() {
    let e = env("ledger-create-fail");
    for hooks in [
        InvalidateHooks {
            fail_ledger_create: true,
            ..Default::default()
        },
        InvalidateHooks {
            fail_ledger_dir_sync: true,
            ..Default::default()
        },
    ] {
        let _ = fs::remove_file(ledger_path(&e.lib));
        new_process(&e.lib);
        let j0 = journal(&e.lib, "page-1");
        {
            let _h = install_invalidate_hooks(hooks);
            assert!(try_stroke(&e.lib, "page-1").is_err());
        }
        assert_eq!(journal(&e.lib, "page-1"), j0);
        try_stroke(&e.lib, "page-1").unwrap();
        let rs = valid_recs(&e.lib);
        assert!(
            recorded_in(&rs, cur_gen(&e.lib), &page(NB, "page-1")),
            "{rs:?}"
        );
    }
    cleanup(&e.lib);
}

#[test]
fn generation_and_stamp_failures_keep_todays_behaviour_and_leave_no_session_record() {
    let e = env("ledger-gen-fail");
    // Generation write fails: nothing changes, nothing recorded.
    new_process(&e.lib);
    let (g0, l0) = (cur_gen(&e.lib), raw(&e.lib));
    {
        let _h = install_invalidate_hooks(InvalidateHooks {
            fail_gen_fsync: true,
            ..Default::default()
        });
        assert!(try_stroke(&e.lib, "page-1").is_err());
    }
    assert_eq!((cur_gen(&e.lib), raw(&e.lib)), (g0, l0.clone()));
    // Bump durable, then failure before stamp clear / record: generation g0+1 has no session.
    new_process(&e.lib);
    {
        let _h = install_invalidate_hooks(InvalidateHooks {
            crash_after_gen: true,
            ..Default::default()
        });
        assert!(try_stroke(&e.lib, "page-1").is_err());
    }
    assert_eq!(cur_gen(&e.lib), g0 + 1);
    assert_eq!(raw(&e.lib), l0, "no session record for an unrecorded bump");
    // The next attempt bumps again: the abandoned generation is never reused.
    try_stroke(&e.lib, "page-1").unwrap();
    let rs = recs(&e.lib);
    assert!(!rs.iter().any(|r| r.g == g0 + 1), "{rs:?}");
    assert!(recorded_in(&rs, g0 + 2, &page(NB, "page-1")));
    // Stamp-clear failure (registered sync dir with a stamp).
    new_process(&e.lib);
    fs::create_dir_all(&e.sync).unwrap();
    crate::sync_verified::register_paths(&e.lib, &e.sync);
    let (lid, rid) = (
        sync_protocol::LibraryId::from_uuid(Uuid::now_v7()).unwrap(),
        ReplicaId::from_uuid(Uuid::now_v7()).unwrap(),
    );
    crate::sync_verified::write_stamp_after_full_reconcile(&e.lib, &e.sync, lid, rid, 0, 0)
        .unwrap();
    let (g1, l1) = (cur_gen(&e.lib), raw(&e.lib));
    {
        let _h = install_invalidate_hooks(InvalidateHooks {
            fail_stamp_delete: true,
            ..Default::default()
        });
        assert!(try_stroke(&e.lib, "page-1").is_err());
    }
    assert_eq!(cur_gen(&e.lib), g1 + 1);
    assert_eq!(raw(&e.lib), l1);
    cleanup(&e.lib);
}

#[test]
fn canonical_write_failure_after_recording_keeps_the_extra_record() {
    let e = env("ledger-content-fail");
    new_process(&e.lib);
    let info = nb_dir(&e.lib, NB).join("notebook.info");
    let before = fs::read(&info).unwrap();
    let mut repo = open_library_read_only(&e.lib).unwrap();
    let err = rename_notebook_durable(
        &mut repo,
        NB,
        "Never",
        &RenameNotebookHooks {
            info: LibraryManifestWriteHooks {
                fail_rename: true,
                ..Default::default()
            },
        },
    );
    assert!(err.is_err());
    assert_eq!(fs::read(&info).unwrap(), before);
    assert!(recorded_in(&recs(&e.lib), cur_gen(&e.lib), &notebook(NB)));
    cleanup(&e.lib);
}

#[test]
fn termination_at_each_boundary_never_leaves_content_without_a_record() {
    let e = env("ledger-crash-points");
    let lib = &e.lib;
    // (a) after the record is durable, before the content write.
    new_process(lib);
    let j0 = journal(lib, "page-1");
    let r = {
        let _h = install_invalidate_hooks(InvalidateHooks {
            after_ledger_durable: Some(Arc::new(|| panic!("process killed"))),
            ..Default::default()
        });
        catch_unwind(AssertUnwindSafe(|| try_stroke(lib, "page-1")))
    };
    assert!(r.is_err());
    assert_eq!(journal(lib, "page-1"), j0, "no content");
    let g_a = cur_gen(lib);
    assert!(
        recorded_in(&recs(lib), g_a, &page(NB, "page-1")),
        "record retained"
    );
    // (b) restart: the next process opens a new generation and keeps working.
    new_process(lib);
    try_stroke(lib, "page-1").unwrap();
    let rs = recs(lib);
    assert!(cur_gen(lib) > g_a);
    assert!(recorded_in(&rs, cur_gen(lib), &page(NB, "page-1")));
    // (c) invariant over the whole history: each generation with records starts with exactly
    // one session record, and generations only increase in file order.
    let sessions: Vec<u64> = rs
        .iter()
        .filter(|r| r.k == "session")
        .map(|r| r.g)
        .collect();
    assert!(sessions.windows(2).all(|w| w[0] < w[1]), "{sessions:?}");
    for r in &rs {
        let s = rs
            .iter()
            .position(|x| x.k == "session" && x.g == r.g)
            .unwrap();
        let i = rs.iter().position(|x| x == r).unwrap();
        assert!(s <= i, "record before its session: {rs:?}");
    }
    cleanup(lib);
}

#[test]
fn a_previous_process_torn_tail_is_isolated_not_repaired() {
    let e = env("ledger-prev-torn");
    let frag = "{\"v\":1,\"g\":";
    let before = raw(&e.lib);
    fs::OpenOptions::new()
        .append(true)
        .open(ledger_path(&e.lib))
        .unwrap()
        .write_all(frag.as_bytes())
        .unwrap();
    new_process(&e.lib);
    try_stroke(&e.lib, "page-1").unwrap();
    let r = raw(&e.lib);
    assert!(
        r.starts_with(&format!("{before}{frag}\n")),
        "fragment kept, then isolated"
    );
    let rest: Vec<LedgerRecordV1> = r[before.len() + frag.len() + 1..]
        .lines()
        .map(|l| parse_line(l).unwrap())
        .collect();
    assert_eq!(rest[0].k, "session");
    assert!(recorded_in(&rest, cur_gen(&e.lib), &page(NB, "page-1")));
    cleanup(&e.lib);
}

// ------------------------------------------------------------------ provenance (15–16)

#[test]
fn ledger_unaware_writers_leave_a_visible_generation_gap() {
    let e = env("ledger-old-binary");
    new_process(&e.lib);
    try_stroke(&e.lib, "page-1").unwrap();
    let g_new = cur_gen(&e.lib);
    // An older binary: bumps the generation (atomic file replace) and appends a stroke,
    // writing no ledger record.
    new_process(&e.lib);
    fs::write(
        e.lib.join(SYNC_DIR).join(MUTATION_GEN_FILE),
        format!("{}\n", g_new + 1),
    )
    .unwrap();
    let mut j = fs::OpenOptions::new()
        .append(true)
        .open(page_dir(&nb_dir(&e.lib, NB), "page-1").join("strokes.jsonl"))
        .unwrap();
    let line = crate::store::stroke_record_line(&stroke(&[(7.0, 7.0), (70.0, 70.0)])).unwrap();
    j.write_all(line.as_bytes()).unwrap();
    drop(j);
    // The current binary next: its own generation, its own session.
    try_stroke(&e.lib, "page-1").unwrap();
    let rs = recs(&e.lib);
    assert!(
        !rs.iter().any(|r| r.g == g_new + 1),
        "gap stays visible: {rs:?}"
    );
    assert!(recorded_in(&rs, g_new + 2, &page(NB, "page-1")));
    cleanup(&e.lib);
}

#[test]
fn the_ledger_is_invisible_to_the_census_and_to_older_readers() {
    let e = env("ledger-compat");
    new_process(&e.lib);
    try_stroke(&e.lib, "page-1").unwrap();
    let c0 = crate::sync_verified::library_census_sha256(&e.lib).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(ledger_path(&e.lib))
        .unwrap()
        .write_all(b"garbage that no reader trusts\n")
        .unwrap();
    assert_eq!(
        crate::sync_verified::library_census_sha256(&e.lib).unwrap(),
        c0
    );
    fs::remove_file(ledger_path(&e.lib)).unwrap();
    assert_eq!(
        crate::sync_verified::library_census_sha256(&e.lib).unwrap(),
        c0
    );
    open_library_read_only(&e.lib).unwrap();
    crate::sync_local::live_layout(&e.lib).unwrap();
    cleanup(&e.lib);
}

// ------------------------------------------------------------------ startup unchanged (17–18)

#[test]
fn clean_and_dirty_startup_decisions_ignore_the_ledger() {
    let e = env("ledger-startup");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(!s.reconcile_path().unwrap().is_fast(), "first open is full");
    stop(s);
    // A corrupt or missing ledger never changes the verified fast path.
    fs::OpenOptions::new()
        .append(true)
        .open(ledger_path(&e.lib))
        .unwrap()
        .write_all(b"{not json\n")
        .unwrap();
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(
        s.reconcile_path().unwrap().is_fast(),
        "{:?}",
        s.reconcile_path()
    );
    assert!(s.fault().is_none());
    stop(s);
    fs::remove_file(ledger_path(&e.lib)).unwrap();
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.reconcile_path().unwrap().is_fast());
    stop(s);
    // An edit (recorded in the ledger) still takes the full path.
    try_stroke(&e.lib, "page-1").unwrap();
    assert!(recorded_in(
        &recs(&e.lib),
        cur_gen(&e.lib),
        &page(NB, "page-1")
    ));
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(
        !s.reconcile_path().unwrap().is_fast(),
        "{:?}",
        s.reconcile_path()
    );
    assert!(s.fault().is_none());
    stop(s);
    cleanup(&e.lib);
}

// ------------------------------------------------------------------ 19B.3C.1 failure state

/// Count of durable ledger appends made by `op` (the hook fires once per successful append).
fn appends_during(op: impl FnOnce()) -> usize {
    let n = Arc::new(Mutex::new(0usize));
    {
        let c = n.clone();
        let _h = install_invalidate_hooks(InvalidateHooks {
            after_ledger_durable: Some(Arc::new(move || *c.lock().unwrap() += 1)),
            ..Default::default()
        });
        op();
    }
    let v = *n.lock().unwrap();
    v
}

#[test]
fn a_failed_record_forces_all_before_any_further_write_even_to_a_recorded_page() {
    let e = env("ledger-c31-resolve");
    let lib = &e.lib;
    let p2 = add_page(lib);
    new_process(lib);
    // 1. Page A recorded and written.
    try_stroke(lib, "page-1").unwrap();
    let g = cur_gen(lib);
    // 2. Page B's record fsync fails: its write is refused.
    let j2 = journal(lib, &p2);
    {
        let _h = install_invalidate_hooks(InvalidateHooks {
            fail_ledger_fsync: true,
            ..Default::default()
        });
        assert!(try_stroke(lib, &p2).is_err());
    }
    assert_eq!(journal(lib, &p2), j2);
    // 3. Page A again while `all` cannot be recorded: refused, not de-duplicated through.
    let j1 = journal(lib, "page-1");
    {
        let _h = install_invalidate_hooks(InvalidateHooks {
            fail_ledger_fsync: true,
            ..Default::default()
        });
        let err = try_stroke(lib, "page-1").unwrap_err();
        assert!(err.contains("REFUSE PAGE LEDGER"), "{err}");
    }
    assert_eq!(
        journal(lib, "page-1"),
        j1,
        "no write without the recovery record"
    );
    // 4. Page A retried successfully: `all` is durable before the canonical write.
    let at_record: Arc<Mutex<Option<bool>>> = Arc::default();
    {
        let (probe, lib2, j1) = (at_record.clone(), lib.clone(), j1.clone());
        let _h = install_invalidate_hooks(InvalidateHooks {
            after_ledger_durable: Some(Arc::new(move || {
                let tail = raw(&lib2);
                let all = format!("{{\"v\":1,\"g\":{g},\"k\":\"all\"}}\n");
                *probe.lock().unwrap() =
                    Some(tail.ends_with(&all) && journal(&lib2, "page-1") == j1);
            })),
            ..Default::default()
        });
        try_stroke(lib, "page-1").unwrap();
    }
    assert_eq!(
        *at_record.lock().unwrap(),
        Some(true),
        "`all` durable first"
    );
    assert_ne!(journal(lib, "page-1"), j1);
    assert_eq!(cur_gen(lib), g, "same generation");
    let rs = valid_recs(lib);
    let sessions = rs.iter().filter(|r| r.g == g && r.k == "session").count();
    assert_eq!(sessions, 1, "{rs:?}");
    // 5. Resolved: later writes to either page append nothing.
    assert_eq!(
        appends_during(|| {
            try_stroke(lib, "page-1").unwrap();
            try_stroke(lib, &p2).unwrap();
        }),
        0
    );
    cleanup(lib);
}

#[test]
fn ordinary_repeated_strokes_need_no_further_ledger_fsync() {
    let e = env("ledger-c31-dedup");
    new_process(&e.lib);
    assert_eq!(appends_during(|| try_stroke(&e.lib, "page-1").unwrap()), 1);
    assert_eq!(
        appends_during(|| {
            for _ in 0..10 {
                try_stroke(&e.lib, "page-1").unwrap();
            }
        }),
        0
    );
    cleanup(&e.lib);
}

#[test]
fn a_torn_scope_record_stays_intact_and_unparseable_before_the_recovery_record() {
    let e = env("ledger-c31-torn");
    let lib = &e.lib;
    let p2 = add_page(lib);
    new_process(lib);
    try_stroke(lib, "page-1").unwrap();
    let before = raw(lib);
    {
        let _h = install_invalidate_hooks(InvalidateHooks {
            torn_ledger_append: Some(15),
            ..Default::default()
        });
        assert!(try_stroke(lib, &p2).is_err());
    }
    let torn = raw(lib)[before.len()..].to_string();
    assert_eq!(torn.len(), 15);
    try_stroke(lib, "page-1").unwrap();
    let r = raw(lib);
    assert!(
        r.starts_with(&format!("{before}{torn}\n")),
        "fragment kept byte-for-byte"
    );
    let lines: Vec<&str> = r[before.len()..].lines().collect();
    assert!(parse_line(lines[0]).is_err());
    assert_eq!(parse_line(lines[1]).unwrap().k, "all");
    assert_eq!(lines.len(), 2, "{lines:?}");
    cleanup(lib);
}

#[test]
fn a_crash_after_a_failure_leaves_no_unrecorded_content_in_that_generation() {
    let e = env("ledger-c31-crash");
    let lib = &e.lib;
    let p2 = add_page(lib);
    new_process(lib);
    try_stroke(lib, "page-1").unwrap();
    let g = cur_gen(lib);
    let j2 = journal(lib, &p2);
    {
        let _h = install_invalidate_hooks(InvalidateHooks {
            torn_ledger_append: Some(20),
            ..Default::default()
        });
        assert!(try_stroke(lib, &p2).is_err());
    }
    // Killed here, before any recovery record.
    new_process(lib);
    let mid = raw(lib);
    assert!(!mid.ends_with('\n'), "torn tail left as is");
    let rs = valid_recs(lib);
    assert!(!rs.iter().any(|r| r.g == g && r.k == "all"));
    // Generation g: the only content written (page A) is recorded; page B never written; the
    // malformed line stays in g's range so a reader cannot treat g as clean (19B.3D rule).
    assert!(recorded_in(&rs, g, &page(NB, "page-1")));
    assert_eq!(journal(lib, &p2), j2);
    assert!(mid.lines().any(|l| parse_line(l).is_err()));
    // The next process never writes into g: new generation, fragment isolated first.
    try_stroke(lib, &p2).unwrap();
    let after = raw(lib);
    assert!(after.starts_with(&format!("{mid}\n")));
    let new: Vec<LedgerRecordV1> = after[mid.len() + 1..]
        .lines()
        .map(|l| parse_line(l).unwrap())
        .collect();
    assert!(new.iter().all(|r| r.g > g), "{new:?}");
    assert!(recorded_in(&new, cur_gen(lib), &page(NB, &p2)));
    cleanup(lib);
}

#[test]
fn a_panicking_diagnostic_leaves_admission_state_consistent() {
    let e = env("ledger-c31-diag");
    let lib = &e.lib;
    let boom = || InvalidateHooks {
        on_ledger_diag: Some(Arc::new(|_| panic!("stderr failed"))),
        ..Default::default()
    };
    // Success diagnostic (new session) panics after the record is durable.
    new_process(lib);
    let j0 = journal(lib, "page-1");
    let r = {
        let _h = install_invalidate_hooks(boom());
        catch_unwind(AssertUnwindSafe(|| try_stroke(lib, "page-1")))
    };
    assert!(r.is_err());
    assert_eq!(
        crate::sync_verified::session_health_for_test(lib),
        Some((false, 0)),
        "lock not poisoned, no leaked in-flight admission"
    );
    assert_eq!(journal(lib, "page-1"), j0, "no content written");
    assert!(recorded_in(&recs(lib), cur_gen(lib), &page(NB, "page-1")));
    // State was fully published: the retry needs no new record.
    assert_eq!(appends_during(|| try_stroke(lib, "page-1").unwrap()), 0);

    // Failure diagnostic panics: the failure state must still be published. (A new process,
    // so page B is not already covered by `add_page`'s notebook record.)
    let p2 = add_page(lib);
    new_process(lib);
    let r = {
        let _h = install_invalidate_hooks(InvalidateHooks {
            fail_ledger_fsync: true,
            ..boom()
        });
        catch_unwind(AssertUnwindSafe(|| try_stroke(lib, &p2)))
    };
    assert!(r.is_err());
    assert_eq!(
        crate::sync_verified::session_health_for_test(lib),
        Some((false, 0))
    );
    try_stroke(lib, "page-1").unwrap();
    let rs = valid_recs(lib);
    assert!(
        rs.iter().any(|x| x.g == cur_gen(lib) && x.k == "all"),
        "recovery still required after a failed diagnostic: {rs:?}"
    );
    cleanup(lib);
}
