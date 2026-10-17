//! Slice 18E.6D: disposable identity-preparation proofs. Synthetic libraries only
//! for ordinary runs.

use super::*;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kindle_platform::FbGeometry;
use sync_engine::testkit::{pg, replica};
use sync_engine::LibraryState;
use sync_protocol::{PageId, SyncOperationV1};

use crate::ink::InkRequest;
use crate::library::{
    open_library_read_only, open_or_create_library, read_notebook_manifest_read_only,
};
use crate::notebook::{insert_page_after_durable, InsertPageHooks};
use crate::sync_local::{
    build_snapshot_read_only, ensure_sync_identity, identity_path, read_identity, SyncWriteHooks,
};
use crate::sync_recovery_compare::inspect_identity_read_only;
use crate::sync_recovery_extract::{apply_candidates, extract_read_only};
use crate::sync_recovery_plan::{
    plan_recovery, OutboxInput, PlanResult, RecoveryInputs, SequenceAuthority,
};
use crate::ui_gfx::paper_background;

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("scribe-bind-{label}-{n}"))
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

fn lib_with_two_unbound() -> PathBuf {
    let lib = tmp("two-unbound").join("library");
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    ensure_sync_identity(&lib).unwrap();
    let repo = open_library_read_only(&lib).unwrap();
    let dir = repo.notebook_dir("notebook-1");
    let mut m = read_notebook_manifest_read_only(&dir).unwrap();
    insert_page_after_durable(&dir, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    let after = m.page_ids.last().unwrap().clone();
    insert_page_after_durable(&dir, &mut m, &after, &InsertPageHooks::default()).unwrap();
    lib
}

#[test]
fn protected_paths_are_refused() {
    let err = require_disposable_bind_root(Path::new(
        r"C:\Users\example\Ignite-Backups-PRIVATE\Ignite-CurrentState-18E6C\acq\scribe-data",
    ))
    .unwrap_err();
    assert!(err.contains("protected"));
    let err = require_disposable_bind_root(Path::new(r"C:\tmp\not-the-work-tree")).unwrap_err();
    assert!(err.contains("18E6D"));
    require_disposable_bind_root(Path::new(
        r"C:\Users\example\Ignite-Backups-PRIVATE\Ignite-Divergence-Analysis-18D3\work\18E6D\copy",
    ))
    .unwrap();
    require_disposable_bind_root(Path::new(
        r"C:\Users\example\Ignite-Backups-PRIVATE\Ignite-Divergence-Analysis-18D3\work\18E9A\copy",
    ))
    .unwrap();
    require_disposable_bind_root(Path::new(
        r"C:\Users\example\Ignite-Backups-PRIVATE\Ignite-Divergence-Analysis-18D3\work\18E9B\copy",
    ))
    .unwrap();
    require_disposable_bind_root(Path::new(
        r"C:\Users\example\Ignite-Backups-PRIVATE\Ignite-Divergence-Analysis-18D3\work\18E9C\copy",
    ))
    .unwrap();
}

#[test]
fn allocates_exactly_two_pages_and_retries_reuse_ids() {
    let lib = lib_with_two_unbound();
    let before = read_identity(&lib).unwrap().unwrap();
    let bound = before.pages["notebook-1"].len();
    assert_eq!(bound, 1);
    let content_before = notebook_content_census(&lib).unwrap();
    let report =
        prepare_unbound_page_identities(&lib, &BTreeSet::new(), false, &Default::default())
            .unwrap();
    assert_eq!(report.unbound_before, 2);
    assert_eq!(report.allocated_pages, 2);
    assert!(report.existing_page_bindings_preserved);
    assert!(report.retired_preserved);
    assert!(report.library_preserved);
    assert!(report.notebooks_preserved);
    let after = read_identity(&lib).unwrap().unwrap();
    let new = new_page_ids(&before, &after);
    assert_eq!(new.len(), 2);
    assert!(inspect_identity_read_only(&lib).unwrap().complete());
    let again = prepare_unbound_page_identities(&lib, &BTreeSet::new(), false, &Default::default())
        .unwrap();
    assert_eq!(again.allocated_pages, 0);
    assert_eq!(again.unbound_before, 0);
    let after2 = read_identity(&lib).unwrap().unwrap();
    assert_eq!(after, after2);
    let content_after = notebook_content_census(&lib).unwrap();
    assert_eq!(content_before, content_after);
    let snap = build_snapshot_read_only(&lib, &after).unwrap();
    assert_eq!(snap.notebooks[0].pages.len(), 3);
}

#[test]
fn interrupted_write_leaves_original_identity() {
    let lib = lib_with_two_unbound();
    let original = std::fs::read(identity_path(&lib)).unwrap();
    for hooks in [
        SyncWriteHooks {
            fail_write: true,
            ..Default::default()
        },
        SyncWriteHooks {
            fail_sync: true,
            ..Default::default()
        },
        SyncWriteHooks {
            fail_rename: true,
            ..Default::default()
        },
    ] {
        let err =
            prepare_unbound_page_identities(&lib, &BTreeSet::new(), false, &hooks).unwrap_err();
        assert!(err.contains("injected"), "{err}");
        assert_eq!(std::fs::read(identity_path(&lib)).unwrap(), original);
        assert!(!inspect_identity_read_only(&lib).unwrap().complete());
        assert!(!identity_path(&lib)
            .parent()
            .unwrap()
            .join("identity.json.tmp")
            .exists());
    }
    prepare_unbound_page_identities(&lib, &BTreeSet::new(), false, &Default::default()).unwrap();
    assert!(inspect_identity_read_only(&lib).unwrap().complete());
}

#[test]
fn dirsync_failure_after_rename_is_ambiguous_but_ids_are_durable() {
    let lib = lib_with_two_unbound();
    let err = prepare_unbound_page_identities(
        &lib,
        &BTreeSet::new(),
        false,
        &SyncWriteHooks {
            fail_dirsync: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("dirsync"));
    let after = read_identity(&lib).unwrap().unwrap();
    assert!(inspect_identity_read_only(&lib).unwrap().complete());
    let ids = after.pages["notebook-1"].clone();
    let again = prepare_unbound_page_identities(&lib, &BTreeSet::new(), false, &Default::default())
        .unwrap();
    assert_eq!(again.allocated_pages, 0);
    assert_eq!(
        read_identity(&lib).unwrap().unwrap().pages["notebook-1"],
        ids
    );
}

#[test]
fn lost_commit_confirmation_reuses_durable_ids() {
    let lib = lib_with_two_unbound();
    let err = prepare_unbound_page_identities(
        &lib,
        &BTreeSet::new(),
        false,
        &SyncWriteHooks {
            fail_after_commit: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("after-commit"));
    let after = read_identity(&lib).unwrap().unwrap();
    assert!(inspect_identity_read_only(&lib).unwrap().complete());
    let again = prepare_unbound_page_identities(&lib, &BTreeSet::new(), false, &Default::default())
        .unwrap();
    assert_eq!(again.allocated_pages, 0);
    assert_eq!(read_identity(&lib).unwrap().unwrap(), after);
}

#[test]
fn persist_cas_refuses_stale_plan() {
    let lib = lib_with_two_unbound();
    let (from, plan) = crate::sync_local::plan_sync_identity(&lib).unwrap();
    crate::sync_local::ensure_sync_identity(&lib).unwrap();
    let err = crate::sync_local::persist_planned_identity(&lib, from.as_ref(), &plan).unwrap_err();
    assert!(err.contains("changed while recovery was planned"));
    let durable = read_identity(&lib).unwrap().unwrap();
    assert!(inspect_identity_read_only(&lib).unwrap().complete());
    assert_ne!(durable, plan.identity);
}

#[test]
fn mint_collision_with_known_id_is_refused() {
    use sync_protocol::reconcile_identity;
    let lib = lib_with_two_unbound();
    let existing = read_identity(&lib).unwrap().unwrap();
    let known = *existing.pages["notebook-1"].values().next().unwrap();
    let (_repo, layout) = crate::sync_local::live_layout(&lib).unwrap();
    let err = reconcile_identity(Some(&existing), &layout, &mut || known.as_uuid()).unwrap_err();
    assert!(err.to_string().contains("collides"));
}

#[test]
fn torn_identity_json_is_refused() {
    let lib = lib_with_two_unbound();
    std::fs::write(identity_path(&lib), b"{").unwrap();
    let err = read_identity(&lib).unwrap_err();
    assert!(err.contains("REFUSE SYNC IDENTITY"));
}

#[test]
fn missing_binding_blocks_complete_planner() {
    let lib = lib_with_two_unbound();
    let id = read_identity(&lib).unwrap().unwrap();
    let local = crate::sync_local::build_snapshot_bound_only(&lib, &id).unwrap();
    let cp = LibraryState::genesis(local.clone()).unwrap();
    let insp = inspect_identity_read_only(&lib).unwrap();
    let err = crate::sync_recovery_compare::plan_if_identity_complete(
        local.library_id,
        replica(1),
        &cp,
        37928,
        0,
        &local,
        &[],
        &[],
        SequenceAuthority::Known { next_client_seq: 1 },
        &insp,
    )
    .unwrap_err();
    assert!(err.contains("identity_binding_prerequisite"));
}

#[test]
fn stale_authoritative_tail_cursor_is_refused() {
    let lib = lib_with_two_unbound();
    prepare_unbound_page_identities(&lib, &BTreeSet::new(), false, &Default::default()).unwrap();
    let id = read_identity(&lib).unwrap().unwrap();
    let local = build_snapshot_read_only(&lib, &id).unwrap();
    let cp = LibraryState::genesis(local.clone()).unwrap();
    let insp = inspect_identity_read_only(&lib).unwrap();
    let err = crate::sync_recovery_compare::plan_if_identity_complete(
        local.library_id,
        replica(1),
        &cp,
        1,
        0,
        &local,
        &[],
        &[],
        SequenceAuthority::Known { next_client_seq: 1 },
        &insp,
    )
    .unwrap_err();
    assert!(err.contains("stale-boundary"));
}

#[test]
fn checkpoint_collision_is_refused() {
    let lib = lib_with_two_unbound();
    let (from, plan) = crate::sync_local::plan_sync_identity(&lib).unwrap();
    let _ = from;
    let mut forbidden = BTreeSet::new();
    forbidden.insert(plan.allocated_pages[0].sync_id);
    let err = checkpoint_collision(&plan, &forbidden).unwrap_err();
    assert!(err.contains("collides with checkpoint"));
    assert!(!inspect_identity_read_only(&lib).unwrap().complete());
}

#[test]
fn retired_page_id_collision_is_refused() {
    let lib = lib_with_two_unbound();
    let (_from, mut plan) = crate::sync_local::plan_sync_identity(&lib).unwrap();
    let pid = plan.allocated_pages[0].sync_id;
    let nb = *plan.identity.notebooks.values().next().unwrap();
    plan.identity.retired_pages.insert(
        pid,
        sync_protocol::binding::RetiredPage {
            notebook_id: nb,
            local_id: "gone".into(),
        },
    );
    let err = checkpoint_collision(&plan, &BTreeSet::new()).unwrap_err();
    assert!(err.contains("retired"));
}

#[test]
fn real_checkpoint_namespace_is_used_for_two_unbound_pages() {
    let lib = lib_with_two_unbound();
    let id = read_identity(&lib).unwrap().unwrap();
    let bound = crate::sync_local::build_snapshot_bound_only(&lib, &id).unwrap();
    let cp = LibraryState::genesis(bound).unwrap();
    let ns = crate::sync_recovery_exec::checkpoint_namespace(&cp, id.library_id, &[]).unwrap();
    assert!(!ns.is_empty());
    for pages in id.pages.values() {
        for pid in pages.values() {
            assert!(
                ns.contains(pid),
                "live binding missing from checkpoint namespace"
            );
        }
    }
    let report = prepare_unbound_page_identities(&lib, &ns, false, &Default::default()).unwrap();
    assert_eq!(report.unbound_before, 2);
    assert_eq!(report.allocated_pages, 2);
    assert!(inspect_identity_read_only(&lib).unwrap().complete());
    let retry = prepare_unbound_page_identities(&lib, &ns, false, &Default::default()).unwrap();
    assert_eq!(retry.allocated_pages, 0);
}

#[test]
fn planner_create_page_uses_durable_ids() {
    let lib = lib_with_two_unbound();
    let before = read_identity(&lib).unwrap().unwrap();
    prepare_unbound_page_identities(&lib, &BTreeSet::new(), false, &Default::default()).unwrap();
    let after = read_identity(&lib).unwrap().unwrap();
    let new = new_page_ids(&before, &after);
    let local = build_snapshot_read_only(&lib, &after).unwrap();
    let mut cp_snap = local.clone();
    cp_snap.notebooks[0].pages.truncate(1);
    let cp = LibraryState::genesis(cp_snap).unwrap();
    let result = plan_recovery(RecoveryInputs {
        library_id: local.library_id,
        replica_id: replica(1),
        checkpoint: &cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        accepted: &[],
        outbox: OutboxInput::Valid { envelopes: &[] },
        local: &local,
        server_tail: &[],
        sequence_authority: SequenceAuthority::Known { next_client_seq: 1 },
    });
    let PlanResult::Feasible(plan) = result else {
        panic!("{result:?}");
    };
    assert!(!plan.upload_authorized);
    let created: BTreeSet<PageId> = plan
        .candidates
        .iter()
        .filter_map(|c| match &c.operation {
            SyncOperationV1::CreatePage { page, .. } => Some(page.page_id),
            _ => None,
        })
        .collect();
    assert_eq!(created, new);
    let replayed = apply_candidates(&cp, local.library_id, replica(1), &plan.candidates).unwrap();
    assert_eq!(replayed.snapshot, local);

    let mut wrong_id = local.clone();
    wrong_id.notebooks[0].pages.last_mut().unwrap().page_id = pg(99);
    assert_ne!(replayed.snapshot, wrong_id);

    let mut dup = local.clone();
    let first = dup.notebooks[0].pages[0].page_id;
    dup.notebooks[0].pages.last_mut().unwrap().page_id = first;
    assert_ne!(replayed.snapshot, dup);

    let mut geom = local.clone();
    geom.notebooks[0].title.push('x');
    assert_ne!(replayed.snapshot, geom);

    let mut order = local.clone();
    order.notebooks[0].pages.swap(0, 1);
    assert_ne!(replayed.snapshot, order);

    let mut missing = local.clone();
    missing.notebooks[0].pages.pop();
    assert_ne!(replayed.snapshot, missing);

    let mut bad = plan.candidates.clone();
    let mut flipped = false;
    for c in &mut bad {
        if let SyncOperationV1::CreatePage { page, .. } = &mut c.operation {
            page.page_id = pg(99);
            flipped = true;
            break;
        }
    }
    assert!(flipped);
    match apply_candidates(&cp, local.library_id, replica(1), &bad) {
        Ok(wrong_replay) => assert_ne!(wrong_replay.snapshot, local),
        Err(_) => {}
    }
}

#[test]
fn stale_cursor_still_gates_review_94_tail() {
    assert!(!crate::sync_recovery_compare::review_94_tail_applicable(1));
    assert!(crate::sync_recovery_compare::review_94_tail_applicable(
        37928
    ));
}

#[test]
#[ignore = "manual: set SCRIBE_BIND_LIB to a disposable 18E6D copy"]
fn disposable_bind_from_env() {
    use std::str::FromStr;
    use std::time::Instant;

    use crate::sync_outbound::SyncStateV1;
    use crate::sync_recovery_compare::{bound_snapshot_read_only, review_94_tail_applicable};
    use crate::sync_recovery_extract::file_sha256;
    use crate::sync_recovery_plan::CandidateOp;
    use crate::sync_recovery_rehearse::plan_with_tail;
    use sync_protocol::{snapshot_sha256, LibraryId, ReplicaId};

    let lib = PathBuf::from(std::env::var("SCRIBE_BIND_LIB").unwrap());
    let sync = PathBuf::from(std::env::var("SCRIBE_BIND_SYNC").unwrap());
    let out = PathBuf::from(std::env::var("SCRIBE_BIND_OUT").unwrap());
    require_disposable_bind_root(&lib).unwrap();
    require_disposable_bind_root(&sync).unwrap();
    require_disposable_bind_root(&out).unwrap();

    let t0 = Instant::now();
    let insp = inspect_identity_read_only(&lib).unwrap();
    let inspect_ms = t0.elapsed().as_millis();
    assert_eq!(insp.unbound_pages.len(), 2);
    assert_eq!(insp.live_pages, 73);

    let hist_lib = PathBuf::from(std::env::var("SCRIBE_COMPARE_HIST_LIB").unwrap());
    let protected_curr = PathBuf::from(std::env::var("SCRIBE_COMPARE_CURR_LIB").unwrap());
    require_disposable_bind_root(&protected_curr).unwrap_err();
    let mut empty_pages = 0usize;
    let mut written_pages = 0usize;
    for (nb, page) in &insp.unbound_pages {
        let (n, is_empty) = complete_page_record_match(&hist_lib, &lib, nb, page).unwrap();
        let (n_p, empty_p) = complete_page_record_match(&protected_curr, &lib, nb, page).unwrap();
        assert_eq!(n, n_p);
        assert_eq!(is_empty, empty_p);
        if is_empty {
            assert_eq!(n, 0);
            empty_pages += 1;
        } else {
            assert_eq!(n, 2);
            written_pages += 1;
        }
    }
    assert_eq!((empty_pages, written_pages), (1, 1));

    let (before_insp, before_snap) = bound_snapshot_read_only(&lib).unwrap();
    let before_snap = before_snap.unwrap();
    assert_eq!(before_insp.bound_pages, 71);
    let before_id = read_identity(&lib).unwrap().unwrap();
    let content_before = notebook_content_census(&lib).unwrap();
    let hist_id = file_sha256(&identity_path(&hist_lib)).unwrap();
    let protected_id = file_sha256(&identity_path(&protected_curr)).unwrap();
    let sync_before = file_tree_census(&sync).unwrap();
    let retired_n_before = before_id.retired_notebooks.len();
    let retired_p_before = before_id.retired_pages.len();
    let bound_before = before_id.pages.values().map(|m| m.len()).sum::<usize>();
    assert_eq!(bound_before, 71);

    let st: SyncStateV1 = serde_json::from_slice(
        &std::fs::read(sync.join(crate::sync_outbound::STATE_FILE)).unwrap(),
    )
    .unwrap();
    let _ = st.server_cursor;
    let mut checkpoint_pages = BTreeSet::new();
    let (cp_state, _, _) = crate::sync_outbound::confirmed::load_checkpoint(&sync)
        .unwrap()
        .expect("checkpoint");
    for n in &cp_state.snapshot.notebooks {
        for p in &n.pages {
            checkpoint_pages.insert(p.page_id);
        }
    }

    let t1 = Instant::now();
    let report =
        prepare_unbound_page_identities(&lib, &checkpoint_pages, true, &Default::default())
            .unwrap();
    let persist_ms = t1.elapsed().as_millis();
    assert_eq!(report.allocated_pages, 2);
    assert!(report.existing_page_bindings_preserved);

    let after = read_identity(&lib).unwrap().unwrap();
    let new = new_page_ids(&before_id, &after);
    assert_eq!(new.len(), 2);
    let t2 = Instant::now();
    let full = build_snapshot_read_only(&lib, &after).unwrap();
    let snap_ms = t2.elapsed().as_millis();
    assert_eq!(full.notebooks.len(), 24);
    assert_eq!(
        full.notebooks.iter().map(|n| n.pages.len()).sum::<usize>(),
        73
    );
    snapshots_equal_after_binding(&before_snap, &full, &new).unwrap();
    let bound_diff = crate::sync_recovery_compare::compare_snapshots(&before_snap, &full);
    assert!(bound_diff.fragment_ids_match);
    assert_eq!(bound_diff.added_pages, 2);
    assert_eq!(notebook_content_census(&lib).unwrap(), content_before);
    assert_eq!(file_sha256(&identity_path(&hist_lib)).unwrap(), hist_id);
    assert_eq!(
        file_sha256(&identity_path(&protected_curr)).unwrap(),
        protected_id
    );
    assert_eq!(file_tree_census(&sync).unwrap(), sync_before);
    assert_eq!(after.retired_notebooks.len(), retired_n_before);
    assert_eq!(after.retired_pages.len(), retired_p_before);
    let bound_after = after.pages.values().map(|m| m.len()).sum::<usize>();
    assert_eq!(bound_after, 73);

    let retry = prepare_unbound_page_identities(&lib, &checkpoint_pages, true, &Default::default())
        .unwrap();
    assert_eq!(retry.allocated_pages, 0);
    assert_eq!(read_identity(&lib).unwrap().unwrap(), after);

    let mut new_stroke_counts = Vec::new();
    for (nb, page) in &insp.unbound_pages {
        let pid = after.page_id(nb, page).expect("bound after persist");
        let found = full
            .notebooks
            .iter()
            .flat_map(|n| n.pages.iter())
            .find(|p| p.page_id == pid)
            .expect("page in snapshot");
        new_stroke_counts.push(found.strokes.len());
        assert!(found.strokes.iter().all(|s| s.root_id == s.stroke_id));
    }
    new_stroke_counts.sort();
    assert_eq!(new_stroke_counts, vec![0, 2]);

    let h = extract_read_only(&lib, &sync).expect("extract after bind");
    assert_eq!(h.identity_allocated_pages, 0);
    assert_eq!(
        h.local
            .notebooks
            .iter()
            .map(|n| n.pages.len())
            .sum::<usize>(),
        73
    );
    assert!(review_94_tail_applicable(h.checkpoint_cursor));
    let t3 = Instant::now();
    let p1 = plan_with_tail(&h, &[]);
    let plan_ms = t3.elapsed().as_millis();
    let PlanResult::Feasible(plan) = &p1 else {
        panic!("{p1:?}");
    };
    assert!(!plan.upload_authorized);
    assert_eq!(plan.counts.extra_originals, 116);
    assert_eq!(plan.counts.lineage_replacements, 3);
    assert_eq!(plan.counts.extra_pages, 2);
    assert_eq!(plan.candidates.len(), 120);
    let created: BTreeSet<PageId> = plan
        .candidates
        .iter()
        .filter_map(|c| match &c.operation {
            SyncOperationV1::CreatePage { page, .. } => Some(page.page_id),
            _ => None,
        })
        .collect();
    assert_eq!(created, new);
    let t4 = Instant::now();
    let replayed =
        apply_candidates(&h.checkpoint, h.library_id, h.replica_id, &plan.candidates).unwrap();
    let replay_ms = t4.elapsed().as_millis();
    assert_eq!(replayed.snapshot, h.local);

    let insp_after = inspect_identity_read_only(&lib).unwrap();
    let gated = crate::sync_recovery_compare::plan_if_identity_complete(
        h.library_id,
        h.replica_id,
        &h.checkpoint,
        h.checkpoint_cursor,
        h.acked_client_seq,
        &h.local,
        &h.pending,
        &[],
        h.sequence_authority.clone(),
        &insp_after,
    )
    .unwrap();
    let PlanResult::Feasible(gated_plan) = &gated else {
        panic!("{gated:?}");
    };
    assert!(!gated_plan.upload_authorized);
    assert_eq!(gated_plan.candidates.len(), 120);

    let h2 = extract_read_only(&lib, &sync).expect("reopen extract");
    let p2 = plan_with_tail(&h2, &[]);
    let PlanResult::Feasible(plan2) = &p2 else {
        panic!("{p2:?}");
    };
    assert_eq!(plan.fingerprint, plan2.fingerprint);
    assert_eq!(plan.candidates, plan2.candidates);

    let kinds: Vec<&str> = plan
        .candidates
        .iter()
        .map(|c: &CandidateOp| match &c.operation {
            SyncOperationV1::AddStroke { .. } => "AddStroke",
            SyncOperationV1::ReplaceStrokeLineages { .. } => "ReplaceStrokeLineages",
            SyncOperationV1::CreatePage { .. } => "CreatePage",
            _ => "other",
        })
        .collect();
    let add = kinds.iter().filter(|k| **k == "AddStroke").count();
    let repl = kinds
        .iter()
        .filter(|k| **k == "ReplaceStrokeLineages")
        .count();
    let create = kinds.iter().filter(|k| **k == "CreatePage").count();
    assert_eq!((add, repl, create), (116, 2, 2));

    let _ = (LibraryId::from_str, ReplicaId::from_str, snapshot_sha256);
    let json = serde_json::json!({
        "unbound_before": 2,
        "allocated": 2,
        "live_pages": 73,
        "notebooks": 24,
        "candidate_envelopes": 120,
        "add_stroke": 116,
        "replace_ops": 2,
        "replace_items": 3,
        "create_page": 2,
        "upload_authorized": false,
        "exact_replay": true,
        "inspect_ms": inspect_ms,
        "persist_ms": persist_ms,
        "snapshot_ms": snap_ms,
        "plan_ms": plan_ms,
        "replay_ms": replay_ms,
        "tail_label": crate::sync_recovery_compare::REVIEW_94_TAIL_LABEL,
        "empty_unbound_pages": empty_pages,
        "written_unbound_pages": written_pages,
        "retry_allocated": retry.allocated_pages,
        "identity_allocated_on_reopen": h.identity_allocated_pages,
        "fragment_ids_match": bound_diff.fragment_ids_match,
    });
    std::fs::write(
        out.join("bind-rehearsal.json"),
        serde_json::to_vec_pretty(&json).unwrap(),
    )
    .unwrap();
}
