//! Slice 19B.3D: verified baseline + shadow ledger validation (diagnostic only).
//!
//! End-to-end cases drive real full reconciles (`OutboundSync::open`) on disposable libraries;
//! the reader, baseline parser and comparison are also tested directly.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use notebook_core::{new_stroke_id, Point, Stroke};
use sync_protocol::{LibraryId, ReplicaId};
use uuid::Uuid;

use super::confirmed_tests::{apply_all_staged, stage_remote};
use super::tests::{append_stroke, env, geo, local_snapshot, nb_dir, stop, Env};
use super::*;
use crate::library::{
    create_notebook_durable, delete_notebook_durable, open_library_read_only,
    read_notebook_manifest_read_only, rename_notebook_durable, CreateNotebookHooks,
    DeleteNotebookHooks, RenameNotebookHooks,
};
use crate::notebook::{
    delete_page_durable, insert_page_after_durable, persist_active_page, reorder_pages_durable,
    write_manifest_atomic_with, DeletePageHooks, InsertPageHooks, ManifestWriteHooks,
};
use crate::page_ledger::{ledger_path, read_provenance, LedgerScope, Provenance, Reject};
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use crate::profile::RenderProfile;
use crate::store::{load_existing_page, page_dir};
use crate::sync_local::SYNC_DIR;
use crate::sync_verified::{
    install_invalidate_hooks, note_library_mutation, read_mutation_gen, CensusMap, InvalidateHooks,
    MUTATION_GEN_FILE,
};
use crate::ui_gfx::paper_background;
use crate::verified_baseline::{
    baseline_path, classify, compare, last_lines_for_test, read_baseline, ActualChange,
    BaselineReject, VerifiedBaselineV1,
};

const NB: &str = "notebook-1";

// ------------------------------------------------------------------ helpers

/// One full open + clean stop. Returns (fast path?, shadow line, baseline line).
fn cycle(e: &Env) -> (bool, String, String) {
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    let fast = s.reconcile_path().is_some_and(|r| r.is_fast());
    stop(s);
    if fast {
        return (true, String::new(), String::new());
    }
    let lines = last_lines_for_test(&e.lib);
    let shadow = lines
        .iter()
        .find(|l| l.starts_with("ledger_shadow"))
        .cloned()
        .unwrap_or_default();
    let base = lines
        .iter()
        .find(|l| l.starts_with("local_baseline"))
        .cloned()
        .unwrap_or_default();
    (false, shadow, base)
}

/// A library with a published baseline (first full reconcile done).
fn baselined(label: &str) -> Env {
    let e = env(label);
    let (fast, shadow, base) = cycle(&e);
    assert!(!fast);
    assert!(shadow.contains("reason=baseline_missing"), "{shadow}");
    assert!(base.starts_with("local_baseline written"), "{base}");
    e
}

fn field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split(' ')
        .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        .unwrap_or_else(|| panic!("{key} missing in {line}"))
}

fn n(line: &str, key: &str) -> u64 {
    field(line, key).parse().unwrap()
}

fn assert_complete(line: &str) {
    assert_eq!(field(line, "result"), "complete", "{line}");
    assert_eq!(n(line, "misses"), 0, "{line}");
}

fn stroke(lib: &Path, page: &str) {
    append_stroke(lib, NB, page, &[(30.0, 30.0), (300.0, 300.0)]);
}

fn add_page(lib: &Path) -> String {
    let d = nb_dir(lib, NB);
    let mut m = read_notebook_manifest_read_only(&d).unwrap();
    let last = m.page_ids.last().unwrap().clone();
    insert_page_after_durable(&d, &mut m, &last, &InsertPageHooks::default())
        .unwrap()
        .inserted_page_id
}

fn raw_stroke_line() -> String {
    crate::store::stroke_record_line(&Stroke {
        id: new_stroke_id(),
        points: vec![
            Point::new(40.0, 40.0).with_pressure(0.5),
            Point::new(400.0, 400.0).with_pressure(0.5),
        ],
        brush: Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
        finished: true,
    })
    .unwrap()
}

/// A writer that appends a stroke without any hook (no generation bump, no ledger record).
fn external_append(lib: &Path, page: &str) {
    fs::OpenOptions::new()
        .append(true)
        .open(page_dir(&nb_dir(lib, NB), page).join("strokes.jsonl"))
        .unwrap()
        .write_all(raw_stroke_line().as_bytes())
        .unwrap();
}

fn cur_gen(lib: &Path) -> u64 {
    read_mutation_gen(lib).unwrap()
}

fn cleanup(e: Env) {
    crate::sync_verified::clear_registry_for(&e.lib);
    let _ = fs::remove_dir_all(e.lib.parent().unwrap());
}

// ------------------------------------------------------------------ coverage (1–10, 18, 24)

#[test]
fn a_single_changed_page_is_exactly_covered() {
    let e = baselined("bl-single");
    stroke(&e.lib, "page-1");
    let (_, sh, base) = cycle(&e);
    assert_complete(&sh);
    assert_eq!(
        (
            n(&sh, "actual_pages"),
            n(&sh, "exact"),
            n(&sh, "predicted_pages")
        ),
        (1, 1, 1),
        "{sh}"
    );
    assert_eq!(n(&sh, "overcovered_pages"), 0);
    assert!(base.starts_with("local_baseline written"), "{base}");
    cleanup(e);
}

#[test]
fn several_pages_and_repeated_strokes_are_covered_page_by_page() {
    let e = baselined("bl-multi");
    let p2 = add_page(&e.lib);
    let p3 = add_page(&e.lib);
    let (_, sh, _) = cycle(&e);
    assert_complete(&sh); // page creation: notebook scope
    assert!(n(&sh, "by_notebook") >= 2, "{sh}");
    for _ in 0..3 {
        stroke(&e.lib, &p2);
    }
    stroke(&e.lib, &p3);
    let (_, sh, _) = cycle(&e);
    assert_complete(&sh);
    assert_eq!((n(&sh, "actual_pages"), n(&sh, "exact")), (2, 2), "{sh}");
    assert_eq!(
        n(&sh, "scopes"),
        2,
        "one record per page, not per stroke: {sh}"
    );
    cleanup(e);
}

#[test]
fn a_page_switch_is_not_a_change_but_a_legacy_switch_is_covered_by_its_notebook() {
    let e = baselined("bl-switch");
    let p2 = add_page(&e.lib);
    cycle(&e);
    let d = nb_dir(&e.lib, NB);
    let mut m = read_notebook_manifest_read_only(&d).unwrap();
    // 19B.3D.1: this build's switch is replica-local; the library stays verified.
    let g0 = cur_gen(&e.lib);
    persist_active_page(&d, &mut m, "page-1").unwrap();
    persist_active_page(&d, &mut m, &p2).unwrap();
    assert_eq!(cur_gen(&e.lib), g0);
    let (fast, _, _) = cycle(&e);
    assert!(fast, "browsing must keep the verified fast path");
    // An older build still rewrites notebook.meta (admitted with a notebook scope).
    m.active_page_id = "page-1".into();
    write_manifest_atomic_with(&d, &m, &ManifestWriteHooks::default()).unwrap();
    let (_, sh, _) = cycle(&e);
    assert_complete(&sh);
    assert_eq!(
        (
            n(&sh, "actual_pages"),
            n(&sh, "actual_notebook_meta"),
            n(&sh, "metadata_only_notebooks"),
            n(&sh, "by_notebook")
        ),
        (0, 1, 1, 1),
        "{sh}"
    );
    assert_eq!(
        n(&sh, "overcovered_pages"),
        2,
        "notebook scope over-covers both pages"
    );
    let _ = p2;
    cleanup(e);
}

#[test]
fn page_and_notebook_structure_changes_are_covered() {
    let e = baselined("bl-structure");
    let lib = &e.lib;
    let d = nb_dir(lib, NB);
    // page create, reorder, delete
    let p2 = add_page(lib);
    let mut m = read_notebook_manifest_read_only(&d).unwrap();
    let mut order = m.page_ids.clone();
    order.reverse();
    reorder_pages_durable(&d, &mut m, &order).unwrap();
    let (_, sh, _) = cycle(&e);
    assert_complete(&sh);
    let mut m = read_notebook_manifest_read_only(&d).unwrap();
    delete_page_durable(&d, &mut m, &p2, &DeletePageHooks::default()).unwrap();
    let (_, sh, _) = cycle(&e);
    assert_complete(&sh);
    assert!(n(&sh, "actual_pages") >= 1, "deleted page files: {sh}");
    // notebook create, rename, reorder, delete
    let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
    let g = geo();
    let mut repo = open_library_read_only(lib).unwrap();
    let created = create_notebook_durable(
        &mut repo,
        &profile,
        &g,
        &paper_background(&g),
        None,
        &CreateNotebookHooks::default(),
    )
    .unwrap();
    let nb2 = created.notebook_id.clone();
    let (_, sh, _) = cycle(&e);
    assert_complete(&sh);
    assert!(n(&sh, "actual_structure") >= 1, "{sh}");
    // Rename and delete are captured operations (offline they are not derivable by today's
    // reconcile, as is an offline notebook reorder — covered by the ledger tests and
    // `comparison_flags_each_uncovered_class`). Drive them as the runtime does.
    let s = OutboundSync::open(lib, &e.sync).unwrap();
    let t = s.prepare_rename_notebook(&nb2, "Renamed").unwrap();
    let mut repo = open_library_read_only(lib).unwrap();
    rename_notebook_durable(&mut repo, &nb2, "Renamed", &RenameNotebookHooks::default()).unwrap();
    s.commit(t);
    stop(s);
    let (_, sh, _) = cycle(&e);
    assert_complete(&sh);
    assert!(n(&sh, "actual_notebook_meta") >= 1, "{sh}");
    let s = OutboundSync::open(lib, &e.sync).unwrap();
    let t = s.prepare_delete_notebook(&nb2).unwrap();
    let mut repo = open_library_read_only(lib).unwrap();
    delete_notebook_durable(&mut repo, &nb2, &DeleteNotebookHooks::default()).unwrap();
    s.commit(t);
    stop(s);
    let (_, sh, _) = cycle(&e);
    assert_complete(&sh);
    cleanup(e);
}

#[test]
fn folder_and_identity_changes_are_structure_changes() {
    let e = baselined("bl-folders");
    let mut f = crate::folders::read_folders(&e.lib).unwrap();
    f.next_folder_ordinal += 1;
    crate::folders::write_materialized(&e.lib, &f).unwrap();
    let (_, sh, _) = cycle(&e);
    assert_complete(&sh);
    assert_eq!(
        (n(&sh, "actual_structure"), n(&sh, "exact")),
        (1, 1),
        "{sh}"
    );
    // A new page needs an identity binding (structure) written by the reconcile itself.
    add_page(&e.lib);
    let (_, sh, _) = cycle(&e);
    assert_complete(&sh);
    assert!(
        n(&sh, "actual_structure") >= 1,
        "identity.json changed: {sh}"
    );
    cleanup(e);
}

#[test]
fn inbound_materialization_is_covered() {
    let e = baselined("bl-inbound");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let replica = ReplicaId::from_uuid(Uuid::now_v7()).unwrap();
    let mut client = 0;
    stage_remote(&s, replica, &mut client, 2);
    apply_all_staged(&s);
    stop(s);
    let (fast, sh, _) = cycle(&e);
    assert!(!fast);
    assert_complete(&sh);
    assert!(n(&sh, "actual_pages") >= 1, "{sh}");
    cleanup(e);
}

#[test]
fn interrupted_journal_repair_is_covered() {
    let e = baselined("bl-repair");
    stroke(&e.lib, "page-1");
    let p = page_dir(&nb_dir(&e.lib, NB), "page-1").join("strokes.jsonl");
    fs::OpenOptions::new()
        .append(true)
        .open(&p)
        .unwrap()
        .write_all(b"{\"v\":2,\"type\":\"stroke\",\"id\":\"s-torn")
        .unwrap();
    crate::sync_verified::clear_registry_for(&e.lib);
    load_existing_page(&nb_dir(&e.lib, NB), "page-1").unwrap(); // truncates (admitted)
    let (_, sh, _) = cycle(&e);
    assert_complete(&sh);
    cleanup(e);
}

#[test]
fn an_all_scope_covers_everything() {
    let e = baselined("bl-all");
    {
        let _admit = note_library_mutation(&e.lib).unwrap(); // bulk writer (e.g. import)
        external_append(&e.lib, "page-1");
    }
    let (_, sh, _) = cycle(&e);
    assert_complete(&sh);
    assert_eq!(n(&sh, "by_all"), 1, "{sh}");
    cleanup(e);
}

#[test]
fn a_change_with_an_omitted_scope_is_a_miss_even_when_snapshots_match() {
    // An external writer adds a stroke: the reconcile re-derives it (snapshots end up equal),
    // but the census shows a page change the ledger never recorded.
    let e = baselined("bl-external");
    external_append(&e.lib, "page-1");
    let (fast, sh, _) = cycle(&e);
    assert!(!fast, "census mismatch forces the full path");
    assert_eq!(field(&sh, "result"), "miss", "{sh}");
    assert_eq!((n(&sh, "misses"), n(&sh, "miss_pages")), (1, 1), "{sh}");
    // A semantically neutral rewrite of notebook.info (snapshot identical) is still a change.
    let info = nb_dir(&e.lib, NB).join("notebook.info");
    let mut bytes = fs::read(&info).unwrap();
    bytes.push(b'\n');
    fs::write(&info, bytes).unwrap();
    let (_, sh, _) = cycle(&e);
    assert_eq!(field(&sh, "result"), "miss", "{sh}");
    assert_eq!(n(&sh, "miss_notebook_meta"), 1, "{sh}");
    cleanup(e);
}

#[test]
fn comparison_flags_each_uncovered_class() {
    let mut before = CensusMap::new();
    for p in [
        "notebooks/notebook-1/page-1/strokes.jsonl",
        "notebooks/notebook-1/notebook.meta",
        "library.meta",
        "odd/file.jsonl",
    ] {
        before.insert(p.into(), (10, "-".into()));
    }
    let mut after = before.clone();
    for v in after.values_mut() {
        v.0 = 11;
    }
    let b = VerifiedBaselineV1 {
        v: 1,
        library_id: LibraryId::from_uuid(Uuid::now_v7()).unwrap(),
        replica_id: ReplicaId::from_uuid(Uuid::now_v7()).unwrap(),
        mutation_gen: 1,
        server_cursor: 0,
        acked_client_seq: 0,
        stamp_census_sha256: String::new(),
        layout: vec![(NB.into(), vec!["page-1".into()])],
        census: before,
    };
    let none = Provenance::default();
    let c = compare(&b, &after, &b.layout, &none);
    assert_eq!(
        (
            c.misses,
            c.miss_pages,
            c.miss_notebook_meta,
            c.miss_structure,
            c.miss_unknown
        ),
        (4, 1, 1, 1, 1)
    );
    // A page scope does not cover notebook metadata; only `all` covers unknown files.
    let page = Provenance {
        scopes: vec![LedgerScope::Page {
            nb: NB.into(),
            page: "page-1".into(),
        }],
        ..Default::default()
    };
    let c = compare(&b, &after, &b.layout, &page);
    assert_eq!((c.exact, c.misses), (1, 3));
    let all = Provenance {
        scopes: vec![LedgerScope::All],
        has_all: true,
        ..Default::default()
    };
    let c = compare(&b, &after, &b.layout, &all);
    assert_eq!((c.by_all, c.misses), (4, 0));
    assert_eq!(
        classify("notebooks/notebook-1/page-2/page.meta"),
        ActualChange::Page {
            nb: NB.into(),
            page: "page-2".into()
        }
    );
    assert_eq!(
        classify(".scribe-sync/identity.json"),
        ActualChange::Structure
    );
    assert_eq!(classify("notebooks/Bad Name/x"), ActualChange::Unknown);
}

// ------------------------------------------------------------------ provenance (11, 15–17)

#[test]
fn an_older_writer_that_bumps_without_recording_makes_the_check_ineligible() {
    let e = baselined("bl-old-writer");
    let g = cur_gen(&e.lib);
    fs::write(
        e.lib.join(SYNC_DIR).join(MUTATION_GEN_FILE),
        format!("{}\n", g + 1),
    )
    .unwrap();
    external_append(&e.lib, "page-1");
    let (_, sh, _) = cycle(&e);
    assert_eq!(field(&sh, "result"), "ineligible", "{sh}");
    assert_eq!(field(&sh, "reason"), "ledger_generation_gap", "{sh}");
    cleanup(e);
}

fn write_ledger(lib: &Path, body: &str) {
    fs::create_dir_all(lib.join(SYNC_DIR)).unwrap();
    fs::write(ledger_path(lib), body).unwrap();
}

fn tmp_lib(label: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("bl-reader-{label}-{}", Uuid::now_v7()));
    fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn reader_accepts_only_a_complete_strictly_ordered_history() {
    let lib = tmp_lib("reader");
    let s = |g: u64| format!("{{\"v\":1,\"g\":{g},\"k\":\"session\"}}\n");
    let p = |g: u64, page: &str| {
        format!("{{\"v\":1,\"g\":{g},\"k\":\"page\",\"nb\":\"notebook-1\",\"page\":\"{page}\"}}\n")
    };
    let a = |g: u64| format!("{{\"v\":1,\"g\":{g},\"k\":\"all\"}}\n");
    let ok = format!(
        "{}{}{}{}{}",
        s(3),
        p(3, "page-1"),
        s(4),
        p(4, "page-2"),
        s(5)
    );
    write_ledger(&lib, &ok);
    let prov = read_provenance(&lib, 3, 5).unwrap();
    assert_eq!(prov.sessions, vec![4, 5]);
    assert_eq!(prov.scopes.len(), 1, "g=3 is the baseline's own history");
    assert!(!prov.has_all);
    let cases: Vec<(String, u64, u64, Reject)> = vec![
        (ok.clone(), 3, 6, Reject::Gap), // g=6 unrecorded
        (
            format!("{}{}{}", s(3), s(5), p(5, "page-1")),
            3,
            5,
            Reject::Gap,
        ), // g=4 missing
        (
            format!("{}{}{}", s(4), s(4), p(4, "page-1")),
            3,
            4,
            Reject::Order,
        ), // duplicate
        (format!("{}{}", s(5), s(4)), 3, 5, Reject::Order), // out of order
        (
            format!("{}{}", p(4, "page-1"), s(4)),
            3,
            4,
            Reject::ScopeOutsideSession,
        ),
        (format!("{}{}", s(4), p(3, "page-1")), 3, 4, Reject::Order), // old after new
        (format!("{}{}", s(4), s(5)), 3, 4, Reject::FutureGeneration),
        (format!("{}garbage\n", s(4)), 3, 4, Reject::Malformed),
        (
            format!("{}{{\"v\":1,\"g\":4,\"k\":\"pa", s(4)),
            3,
            4,
            Reject::TornTail,
        ),
        (
            format!("{}{{\"v\":2,\"g\":4,\"k\":\"all\"}}\n", s(4)),
            3,
            4,
            Reject::Malformed,
        ),
        (ok.clone(), 6, 5, Reject::BaselineAhead),
    ];
    for (body, b, c, want) in cases {
        write_ledger(&lib, &body);
        assert_eq!(
            read_provenance(&lib, b, c),
            Err(want.clone()),
            "{want:?}: {body}"
        );
    }
    // Garbage strictly inside older generations is tolerated; at the range boundary it is not.
    write_ledger(&lib, &format!("{}junk\n{}{}{}", s(2), s(3), s(4), a(4)));
    let prov = read_provenance(&lib, 3, 4).unwrap();
    assert_eq!((prov.old_malformed, prov.has_all), (1, true));
    write_ledger(&lib, &format!("{}{}junk\n{}", s(2), s(3), s(4)));
    assert_eq!(read_provenance(&lib, 3, 4), Err(Reject::Malformed));
    // Missing ledger: fine only when no generation advanced.
    fs::remove_file(ledger_path(&lib)).unwrap();
    assert!(read_provenance(&lib, 4, 4).is_ok());
    assert_eq!(read_provenance(&lib, 4, 5), Err(Reject::Missing));
    // Oversized.
    write_ledger(
        &lib,
        &" ".repeat(crate::page_ledger::READ_MAX_BYTES as usize + 1),
    );
    assert_eq!(read_provenance(&lib, 4, 5), Err(Reject::TooLarge));
    let _ = fs::remove_dir_all(lib);
}

#[test]
fn a_damaged_ledger_makes_the_shadow_check_ineligible_end_to_end() {
    for (label, damage) in [("missing", 0u8), ("gap", 1), ("malformed", 2), ("torn", 3)] {
        let e = baselined(&format!("bl-damaged-{label}"));
        stroke(&e.lib, "page-1");
        let lp = ledger_path(&e.lib);
        let raw = fs::read_to_string(&lp).unwrap();
        match damage {
            0 => fs::remove_file(&lp).unwrap(),
            1 => {
                // drop the newest generation's records
                let g = cur_gen(&e.lib);
                let kept: String = raw
                    .lines()
                    .filter(|l| !l.contains(&format!("\"g\":{g},")))
                    .map(|l| format!("{l}\n"))
                    .collect();
                fs::write(&lp, kept).unwrap();
            }
            2 => fs::write(&lp, format!("{raw}not json\n")).unwrap(),
            _ => fs::write(&lp, format!("{raw}{{\"v\":1,")).unwrap(),
        }
        let (_, sh, base) = cycle(&e);
        assert_eq!(field(&sh, "result"), "ineligible", "{label}: {sh}");
        let want = [
            "ledger_missing",
            "ledger_generation_gap",
            "ledger_malformed",
            "ledger_torn_tail",
        ][damage as usize];
        assert_eq!(field(&sh, "reason"), want, "{label}: {sh}");
        assert!(
            base.starts_with("local_baseline written"),
            "a new baseline is still valid"
        );
        cleanup(e);
    }
}

// ------------------------------------------------------------------ baseline (13, 14, 19)

#[test]
fn missing_stale_corrupt_unsupported_and_foreign_baselines_are_refused() {
    let e = baselined("bl-baseline-refusals");
    let bp = baseline_path(&e.sync);
    let good = fs::read(&bp).unwrap();
    let st = OutboundSync::open(&e.lib, &e.sync).unwrap().state();
    let (lid, rid) = (st.library_id, st.replica_id);
    // (cycle-free direct reads)
    assert!(read_baseline(&e.sync, lid, rid).is_ok());
    let mut v: serde_json::Value = serde_json::from_slice(&good).unwrap();
    let cases: Vec<(Vec<u8>, BaselineReject)> = vec![
        (b"{not json".to_vec(), BaselineReject::Corrupt),
        (good[..good.len() / 2].to_vec(), BaselineReject::Corrupt),
        (
            {
                let mut x = v.clone();
                x["v"] = 2.into();
                serde_json::to_vec(&x).unwrap()
            },
            BaselineReject::Unsupported,
        ),
        (
            {
                let mut x = v.clone();
                x["census"]["library.meta"][0] = 1.into();
                serde_json::to_vec(&x).unwrap()
            },
            BaselineReject::Corrupt,
        ),
        (
            {
                let mut x = v.clone();
                x["extra"] = 1.into();
                serde_json::to_vec(&x).unwrap()
            },
            BaselineReject::Corrupt,
        ),
    ];
    for (bytes, want) in cases {
        fs::write(&bp, &bytes).unwrap();
        assert_eq!(
            read_baseline(&e.sync, lid, rid),
            Err(want.clone()),
            "{want:?}"
        );
    }
    fs::write(&bp, &good).unwrap();
    let other = LibraryId::from_uuid(Uuid::now_v7()).unwrap();
    assert_eq!(
        read_baseline(&e.sync, other, rid),
        Err(BaselineReject::OtherIdentity)
    );
    // End to end: stale (ahead of the library) and missing.
    v["mutation_gen"] = (cur_gen(&e.lib) + 5).into();
    fs::write(&bp, serde_json::to_vec(&v).unwrap()).unwrap();
    stroke(&e.lib, "page-1");
    let (_, sh, _) = cycle(&e);
    assert_eq!(field(&sh, "reason"), "baseline_ahead", "{sh}");
    fs::remove_file(&bp).unwrap();
    stroke(&e.lib, "page-1");
    let (_, sh, _) = cycle(&e);
    assert_eq!(field(&sh, "reason"), "baseline_missing", "{sh}");
    cleanup(e);
}

#[test]
fn interrupted_baseline_publication_keeps_the_previous_baseline() {
    let e = baselined("bl-publish-crash");
    let bp = baseline_path(&e.sync);
    for hooks in [
        InvalidateHooks {
            torn_baseline_write: Some(40),
            ..Default::default()
        },
        InvalidateHooks {
            fail_baseline_fsync: true,
            ..Default::default()
        },
        InvalidateHooks {
            fail_baseline_rename: true,
            ..Default::default()
        },
    ] {
        let before = fs::read(&bp).unwrap();
        stroke(&e.lib, "page-1");
        let (_, sh, base) = {
            let _h = install_invalidate_hooks(hooks);
            cycle(&e)
        };
        assert_complete(&sh);
        assert!(base.starts_with("local_baseline_write_failed"), "{base}");
        assert_eq!(
            fs::read(&bp).unwrap(),
            before,
            "previous baseline untouched"
        );
    }
    // The older baseline is still a valid reference: the next check spans more generations.
    stroke(&e.lib, "page-1");
    let (_, sh, base) = cycle(&e);
    assert_complete(&sh);
    assert!(n(&sh, "sessions") >= 2, "{sh}");
    assert!(base.starts_with("local_baseline written"), "{base}");
    cleanup(e);
}

// ------------------------------------------------------------------ concurrency (20)

#[test]
fn a_mutation_during_capture_skips_the_check_and_the_publication() {
    let e = baselined("bl-concurrent");
    let bp = baseline_path(&e.sync);
    let before = fs::read(&bp).unwrap();
    stroke(&e.lib, "page-1");
    let lib = e.lib.clone();
    let (_, sh, base) = {
        let _h = install_invalidate_hooks(InvalidateHooks {
            before_baseline_publish: Some(Arc::new(move || {
                let _a = note_library_mutation(&lib).unwrap();
            })),
            ..Default::default()
        });
        cycle(&e)
    };
    assert_eq!(field(&sh, "reason"), "concurrent_mutation", "{sh}");
    assert!(base.contains("skipped"), "{base}");
    assert_eq!(fs::read(&bp).unwrap(), before);
    cleanup(e);
}

#[test]
fn navigation_during_capture_neither_skips_the_check_nor_the_publication() {
    let e = baselined("bl-nav-capture");
    let p2 = add_page(&e.lib);
    cycle(&e);
    stroke(&e.lib, "page-1");
    let d = nb_dir(&e.lib, NB);
    let (_, sh, base) = {
        let _h = install_invalidate_hooks(InvalidateHooks {
            before_baseline_publish: Some(Arc::new(move || {
                let mut m = read_notebook_manifest_read_only(&d).unwrap();
                persist_active_page(&d, &mut m, &p2).unwrap();
            })),
            ..Default::default()
        });
        cycle(&e)
    };
    assert_complete(&sh);
    assert!(base.starts_with("local_baseline written"), "{base}");
    cleanup(e);
}

// ------------------------------------------------------------------ navigation then edits (19B.3D.1)

#[test]
fn navigation_then_a_stroke_is_one_exactly_covered_page() {
    let e = baselined("bl-nav-stroke");
    let p2 = add_page(&e.lib);
    let p3 = add_page(&e.lib);
    cycle(&e);
    let d = nb_dir(&e.lib, NB);
    let g0 = cur_gen(&e.lib);
    let ledger0 = fs::read(ledger_path(&e.lib)).unwrap_or_default();
    let mut m = read_notebook_manifest_read_only(&d).unwrap();
    for p in [&p2, &p3, &"page-1".to_string(), &p2] {
        persist_active_page(&d, &mut m, p).unwrap();
    }
    stroke(&e.lib, &p2);
    assert_eq!(cur_gen(&e.lib), g0 + 1, "one edit session");
    let ledger = fs::read_to_string(ledger_path(&e.lib)).unwrap();
    let added = &ledger[ledger0.len()..];
    assert!(
        added.contains("\"k\":\"page\"") && !added.contains("\"k\":\"notebook\""),
        "{added}"
    );
    let (fast, sh, _) = cycle(&e);
    assert!(!fast);
    assert_complete(&sh);
    assert_eq!(
        (
            n(&sh, "actual_pages"),
            n(&sh, "actual_notebook_meta"),
            n(&sh, "exact"),
            n(&sh, "by_notebook"),
            n(&sh, "overcovered_pages")
        ),
        (1, 0, 1, 0, 0),
        "{sh}"
    );
    cleanup(e);
}

#[test]
fn navigation_then_a_notebook_change_is_covered_by_its_notebook_scope() {
    let e = baselined("bl-nav-structure");
    let p2 = add_page(&e.lib);
    cycle(&e);
    let d = nb_dir(&e.lib, NB);
    let mut m = read_notebook_manifest_read_only(&d).unwrap();
    persist_active_page(&d, &mut m, &p2).unwrap();
    let g0 = cur_gen(&e.lib);
    insert_page_after_durable(&d, &mut m, &p2, &InsertPageHooks::default()).unwrap();
    assert_eq!(cur_gen(&e.lib), g0 + 1);
    let (fast, sh, _) = cycle(&e);
    assert!(!fast);
    assert_complete(&sh);
    assert_eq!(n(&sh, "actual_notebook_meta"), 1, "{sh}");
    assert!(n(&sh, "by_notebook") >= 1, "{sh}");
    cleanup(e);
}

// ------------------------------------------------------------------ startup unchanged (21–23)

#[test]
fn startup_paths_and_sync_output_are_unchanged_by_the_shadow_check() {
    // Clean reopen keeps the verified fast path and does not touch the baseline.
    let e = baselined("bl-startup");
    let bp = baseline_path(&e.sync);
    let b0 = fs::read(&bp).unwrap();
    let (fast, _, _) = cycle(&e);
    assert!(fast);
    assert_eq!(fs::read(&bp).unwrap(), b0);
    // Two identical libraries, identical edits; one with a corrupt baseline and ledger.
    let f = baselined("bl-startup-twin");
    for x in [&e, &f] {
        stroke(&x.lib, "page-1");
        external_append(&x.lib, "page-1"); // also a lost capture to re-derive
    }
    fs::write(baseline_path(&f.sync), b"garbage").unwrap();
    fs::write(ledger_path(&f.lib), b"garbage\n").unwrap();
    let outs: Vec<_> = [&e, &f]
        .iter()
        .map(|x| {
            let s = OutboundSync::open(&x.lib, &x.sync).unwrap();
            let r = (
                s.reconcile_path(),
                s.fault(),
                s.pending(),
                s.state().next_client_seq,
            );
            stop(s);
            r
        })
        .collect();
    assert_eq!(
        outs[0], outs[1],
        "same path, fault state, pending ops and sequence"
    );
    assert!(!outs[0].0.unwrap().is_fast() && outs[0].1.is_none());
    assert_eq!(
        local_snapshot(&e.lib).notebooks.len(),
        local_snapshot(&f.lib).notebooks.len()
    );
    cleanup(e);
    cleanup(f);
}

// ------------------------------------------------------------------ local cost (opt-in)

/// Cost on a real-size library copy (read-only): `IGNITE_SHADOW_LIBRARY=<library dir>`,
/// run with `-- --ignored --nocapture`. Prints counts and timings only.
#[test]
#[ignore = "needs IGNITE_SHADOW_LIBRARY (a private library copy); read-only"]
fn shadow_cost_on_a_real_library_copy() {
    use std::time::Instant;
    let Ok(lib) = std::env::var("IGNITE_SHADOW_LIBRARY") else {
        return;
    };
    let lib = PathBuf::from(lib);
    let out = std::env::temp_dir().join(format!("bl-cost-{}", Uuid::now_v7()));
    fs::create_dir_all(&out).unwrap();
    let t = Instant::now();
    let (census, stats) = crate::sync_verified::library_census_map(&lib).unwrap();
    let census_us = t.elapsed().as_micros();
    let t = Instant::now();
    let layout: crate::verified_baseline::Layout =
        crate::sync_local::live_layout(&lib).unwrap().1.notebooks;
    let layout_us = t.elapsed().as_micros();
    let b = VerifiedBaselineV1 {
        v: 1,
        library_id: LibraryId::from_uuid(Uuid::now_v7()).unwrap(),
        replica_id: ReplicaId::from_uuid(Uuid::now_v7()).unwrap(),
        mutation_gen: 1,
        server_cursor: 0,
        acked_client_seq: 0,
        stamp_census_sha256: crate::sync_verified::census_map_sha256(&census),
        layout: layout.clone(),
        census: census.clone(),
    };
    let t = Instant::now();
    let bytes = crate::verified_baseline::write_baseline(&out, &b).unwrap();
    let write_us = t.elapsed().as_micros();
    let t = Instant::now();
    let r = read_baseline(&out, b.library_id, b.replica_id).unwrap();
    let read_us = t.elapsed().as_micros();
    let t = Instant::now();
    let mut changed = census.clone();
    if let Some((_, v)) = changed
        .iter_mut()
        .find(|(k, _)| k.ends_with("strokes.jsonl"))
    {
        v.0 += 1;
    }
    let c = compare(&r, &changed, &layout, &Provenance::default());
    let compare_us = t.elapsed().as_micros();
    println!(
        "shadow_cost files={} hashed={} census_us={census_us} layout_us={layout_us} baseline_bytes={bytes} write_us={write_us} read_us={read_us} compare_us={compare_us} pages_examined={} changed_files={}",
        stats.files, stats.hashed_files, c.pages_examined, c.changed_files
    );
    let _ = fs::remove_dir_all(out);
}
