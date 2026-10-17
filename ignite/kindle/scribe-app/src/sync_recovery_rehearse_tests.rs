//! Off-device rehearsal proofs. Synthetic cases always run. The historical acquisition
//! test is `#[ignore]` and runs only when `SCRIBE_REHEARSAL_LIB` / `SCRIBE_REHEARSAL_SYNC` are set.

use super::*;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sync_engine::testkit::{line, replica, sk, Author};
use sync_protocol::{
    LineageReplacement, NotebookId, PageId, StrokeId, SyncOperationV1, SyncSnapshotV1, SyncStroke,
};

use crate::sync_recovery_extract::{extract_read_only, file_sha256, ExtractedHistory};
use crate::sync_recovery_plan::{plan_recovery, RecoveryBlock, SequenceAuthority, ServerTailItem};

fn orig_at(n: u64, y: f32) -> SyncStroke {
    line(sk(n), sk(n), (200.0, y), (800.0, y), 4, false)
}

fn first_unrelated(h: &ExtractedHistory) -> Option<(NotebookId, PageId, StrokeId)> {
    for cn in &h.checkpoint.snapshot.notebooks {
        let Some(ln) = h
            .local
            .notebooks
            .iter()
            .find(|n| n.notebook_id == cn.notebook_id)
        else {
            continue;
        };
        for cp in &cn.pages {
            let Some(lp) = ln.pages.iter().find(|p| p.page_id == cp.page_id) else {
                continue;
            };
            if cp.strokes != lp.strokes {
                continue;
            }
            if let Some(s) = cp.strokes.iter().find(|s| s.root_id == s.stroke_id) {
                return Some((cn.notebook_id, cp.page_id, s.stroke_id));
            }
        }
    }
    None
}

fn first_replace_target(
    plan: &crate::sync_recovery_plan::RecoveryPlan,
) -> Option<(NotebookId, PageId, StrokeId, Vec<SyncStroke>)> {
    for c in &plan.candidates {
        if let SyncOperationV1::ReplaceStrokeLineages {
            notebook_id,
            page_id,
            replacements,
        } = &c.operation
        {
            let r = replacements.first()?;
            return Some((*notebook_id, *page_id, r.root_id, r.expected.clone()));
        }
    }
    None
}

fn first_create_page(
    plan: &crate::sync_recovery_plan::RecoveryPlan,
) -> Option<(NotebookId, PageId)> {
    for c in &plan.candidates {
        if let SyncOperationV1::CreatePage {
            notebook_id, page, ..
        } = &c.operation
        {
            return Some((*notebook_id, page.page_id));
        }
    }
    None
}

fn first_extra_original(
    plan: &crate::sync_recovery_plan::RecoveryPlan,
) -> Option<(NotebookId, PageId, SyncStroke)> {
    for c in &plan.candidates {
        if let SyncOperationV1::AddStroke {
            notebook_id,
            page_id,
            stroke,
        } = &c.operation
        {
            return Some((*notebook_id, *page_id, stroke.clone()));
        }
    }
    None
}

fn remote_author(h: &ExtractedHistory) -> Author {
    let mut a = Author::new(replica(2));
    a.library = h.library_id;
    a
}

fn remove_root(local: &SyncSnapshotV1, page: PageId, root: StrokeId) -> SyncSnapshotV1 {
    let mut s = local.clone();
    for n in &mut s.notebooks {
        for p in &mut n.pages {
            if p.page_id == page {
                p.strokes.retain(|x| x.root_id != root);
            }
        }
    }
    s
}

fn page_role_profiles(h: &ExtractedHistory) -> String {
    let mut out = String::new();
    for (ni, ln) in h.local.notebooks.iter().enumerate() {
        let cn = h
            .checkpoint
            .snapshot
            .notebooks
            .iter()
            .find(|n| n.notebook_id == ln.notebook_id);
        for (pi, lp) in ln.pages.iter().enumerate() {
            let cp = cn.and_then(|n| n.pages.iter().find(|p| p.page_id == lp.page_id));
            let exp_ids: std::collections::BTreeSet<_> = cp
                .map(|p| p.strokes.iter().map(|s| s.stroke_id).collect())
                .unwrap_or_default();
            let mut roles = String::new();
            for s in &lp.strokes {
                let ch = if exp_ids.contains(&s.stroke_id) {
                    'E'
                } else if s.root_id == s.stroke_id {
                    'N'
                } else {
                    'F'
                };
                roles.push(ch);
            }
            let missing = cp
                .map(|p| {
                    p.strokes
                        .iter()
                        .filter(|s| lp.strokes.iter().all(|a| a.stroke_id != s.stroke_id))
                        .count()
                })
                .unwrap_or(0);
            let extra = lp.page_id;
            let _ = extra;
            out.push_str(&format!(
                "nb{ni}p{pi} extra_page={} roles={roles} missing={missing} n={}\n",
                cp.is_none(),
                lp.strokes.len()
            ));
        }
    }
    out
}

fn insert_stroke(local: &SyncSnapshotV1, page: PageId, stroke: SyncStroke) -> SyncSnapshotV1 {
    let mut s = local.clone();
    for n in &mut s.notebooks {
        for p in &mut n.pages {
            if p.page_id == page {
                p.strokes.push(stroke);
                return s;
            }
        }
    }
    s
}

#[derive(Serialize)]
struct SentinelHashes {
    manifest: Option<String>,
    outbox: Option<String>,
    state: Option<String>,
    identity: Option<String>,
    replica: Option<String>,
    confirmed: Option<String>,
}

fn sentinels(lib: &Path, sync: &Path) -> SentinelHashes {
    SentinelHashes {
        manifest: file_sha256(&sync.join("checkpoint").join("manifest.json")).unwrap(),
        outbox: file_sha256(&sync.join("outbox.jsonl")).unwrap(),
        state: file_sha256(&sync.join("sync-state.json")).unwrap(),
        identity: file_sha256(&lib.join(".scribe-sync").join("identity.json")).unwrap(),
        replica: file_sha256(&sync.join("replica.json")).unwrap(),
        confirmed: file_sha256(&sync.join("confirmed.log")).unwrap(),
    }
}

/// Historical acquisition rehearsal. Not run in ordinary `cargo test` (no private paths).
#[test]
#[ignore = "manual: set SCRIBE_REHEARSAL_LIB and SCRIBE_REHEARSAL_SYNC"]
fn off_device_historical_rehearsal_from_env() {
    let lib = PathBuf::from(
        std::env::var("SCRIBE_REHEARSAL_LIB")
            .expect("SCRIBE_REHEARSAL_LIB must point at the reference library root"),
    );
    let sync = PathBuf::from(
        std::env::var("SCRIBE_REHEARSAL_SYNC")
            .expect("SCRIBE_REHEARSAL_SYNC must point at the reference sync-state directory"),
    );
    let out = PathBuf::from(std::env::var("SCRIBE_REHEARSAL_OUT").unwrap_or_else(|_| {
        r"C:\Users\example\Ignite-Backups-PRIVATE\Ignite-Divergence-Analysis-18D3\work\18E5".into()
    }));
    fs::create_dir_all(&out).unwrap();
    let before = sentinels(&lib, &sync);

    let t_extract = std::time::Instant::now();
    let h = extract_read_only(&lib, &sync).expect("read-only extract");
    let extract_ms = t_extract.elapsed().as_millis();

    let proof = prove_checkpoint_only(&h);
    assert!(
        !proof.upload_authorized,
        "planning must never authorize upload"
    );
    assert_eq!(h.outbox_bytes, 0);
    assert!(h.pending.is_empty());
    assert!(!h.pairing_present);
    assert_eq!(h.confirmed_log_bytes, 0);

    let PlanResult::Feasible(plan) = plan_with_tail(&h, &[]) else {
        let profiles = page_role_profiles(&h);
        fs::write(out.join("page-role-profiles.txt"), profiles).unwrap();
        panic!(
            "checkpoint-only plan blocked: {:?} (profiles written)",
            proof.blocked_kind
        );
    };
    assert!(proof.exact_canonical_equality, "canonical equality failed");
    assert!(proof.fragment_ids_match);

    let mut remote = remote_author(&h);
    let unrelated = first_unrelated(&h).expect("unaffected page with an original");
    let y = orig_at(70_001, 50.0);
    let tail_b = [ServerTailItem {
        server_seq: h.checkpoint_cursor + 1,
        envelope: remote.env(SyncOperationV1::AddStroke {
            notebook_id: unrelated.0,
            page_id: unrelated.1,
            stroke: y.clone(),
        }),
    }];
    let local_b = insert_stroke(&h.local, unrelated.1, y.clone());
    let result_b = plan_with_local_and_tail(
        &h,
        &local_b,
        &[],
        &tail_b,
        SequenceAuthority::Known {
            next_client_seq: h.next_client_seq,
        },
    );
    let PlanResult::Feasible(plan_b) = &result_b else {
        panic!("scenario B blocked: {result_b:?}");
    };
    let merged_b = replay_tail_then_candidates(
        &h.checkpoint,
        h.library_id,
        h.replica_id,
        &tail_b,
        &plan_b.candidates,
    )
    .unwrap();
    assert_eq!(merged_b.snapshot, local_b);
    assert!(merged_b.snapshot.notebooks.iter().any(|n| n
        .pages
        .iter()
        .any(|p| p.strokes.iter().any(|s| s.stroke_id == y.stroke_id))));

    let mut remote = remote_author(&h);
    let tail_c = [ServerTailItem {
        server_seq: h.checkpoint_cursor + 1,
        envelope: remote.env(SyncOperationV1::DeleteStrokeLineage {
            notebook_id: unrelated.0,
            page_id: unrelated.1,
            root_id: unrelated.2,
        }),
    }];
    let local_c = remove_root(&h.local, unrelated.1, unrelated.2);
    let result_c = plan_with_local_and_tail(
        &h,
        &local_c,
        &[],
        &tail_c,
        SequenceAuthority::Known {
            next_client_seq: h.next_client_seq,
        },
    );
    let PlanResult::Feasible(plan_c) = &result_c else {
        panic!("scenario C blocked: {result_c:?}");
    };
    let merged_c = replay_tail_then_candidates(
        &h.checkpoint,
        h.library_id,
        h.replica_id,
        &tail_c,
        &plan_c.candidates,
    )
    .unwrap();
    assert_eq!(merged_c.snapshot, local_c);

    let target = first_replace_target(&plan).expect("historical fragments need a replacement");
    let mut remote = remote_author(&h);
    let tail_d = [ServerTailItem {
        server_seq: h.checkpoint_cursor + 1,
        envelope: remote.env(SyncOperationV1::DeleteStrokeLineage {
            notebook_id: target.0,
            page_id: target.1,
            root_id: target.2,
        }),
    }];
    let result_d = plan_with_tail(&h, &tail_d);
    assert!(
        matches!(
            result_d,
            PlanResult::Blocked(RecoveryBlock::SameLineageConflict { .. })
        ),
        "{result_d:?}"
    );

    let mut remote = remote_author(&h);
    let syn_frag = line(sk(70_002), target.2, (10.0, 10.0), (20.0, 20.0), 3, false);
    let tail_e = [ServerTailItem {
        server_seq: h.checkpoint_cursor + 1,
        envelope: remote.env(SyncOperationV1::ReplaceStrokeLineages {
            notebook_id: target.0,
            page_id: target.1,
            replacements: vec![LineageReplacement {
                root_id: target.2,
                expected: target.3.clone(),
                after: vec![syn_frag],
            }],
        }),
    }];
    let result_e = plan_with_tail(&h, &tail_e);
    assert!(
        matches!(
            result_e,
            PlanResult::Blocked(RecoveryBlock::SameLineageConflict { .. })
        ),
        "{result_e:?}"
    );

    let mut remote = remote_author(&h);
    let tail_f = [ServerTailItem {
        server_seq: h.checkpoint_cursor + 1,
        envelope: remote.env(SyncOperationV1::DeletePage {
            notebook_id: target.0,
            page_id: target.1,
        }),
    }];
    let result_f = plan_with_tail(&h, &tail_f);
    assert!(
        matches!(
            result_f,
            PlanResult::Blocked(RecoveryBlock::DeletedTarget { .. })
        ),
        "{result_f:?}"
    );

    let mut remote = remote_author(&h);
    let tail_g = [ServerTailItem {
        server_seq: h.checkpoint_cursor + 1,
        envelope: remote.env(SyncOperationV1::DeleteNotebook {
            notebook_id: target.0,
        }),
    }];
    let result_g = plan_with_tail(&h, &tail_g);
    assert!(
        matches!(
            result_g,
            PlanResult::Blocked(RecoveryBlock::DeletedTarget { .. })
        ),
        "{result_g:?}"
    );

    let extra = first_extra_original(&plan).expect("historical extras");
    let other_page = first_unrelated(&h).map(|t| t.1).unwrap_or(unrelated.1);
    let mut remote = remote_author(&h);
    let tail_h = [ServerTailItem {
        server_seq: h.checkpoint_cursor + 1,
        envelope: remote.env(SyncOperationV1::AddStroke {
            notebook_id: unrelated.0,
            page_id: other_page,
            stroke: extra.2.clone(),
        }),
    }];
    let result_h = plan_with_tail(&h, &tail_h);
    assert!(
        matches!(
            result_h,
            PlanResult::Blocked(RecoveryBlock::IdentityCollision { .. })
                | PlanResult::Blocked(RecoveryBlock::SameLineageConflict { .. })
                | PlanResult::Blocked(RecoveryBlock::ExactStateReproductionFailure { .. })
        ),
        "{result_h:?}"
    );

    let mut remote = remote_author(&h);
    let tail_i = [ServerTailItem {
        server_seq: h.checkpoint_cursor.saturating_add(3),
        envelope: remote.env(SyncOperationV1::AddStroke {
            notebook_id: unrelated.0,
            page_id: unrelated.1,
            stroke: orig_at(70_003, 60.0),
        }),
    }];
    let result_i = plan_with_tail(&h, &tail_i);
    assert!(
        matches!(
            result_i,
            PlanResult::Blocked(RecoveryBlock::StaleAuthoritativeTail { .. })
        ),
        "{result_i:?}"
    );

    let mut later = Author::new(h.replica_id);
    later.library = h.library_id;
    later.next_seq = h.next_client_seq;
    let later_stroke = orig_at(70_010, 70.0);
    let pending_ok = vec![later.env(SyncOperationV1::AddStroke {
        notebook_id: unrelated.0,
        page_id: unrelated.1,
        stroke: later_stroke.clone(),
    })];
    let local_p = insert_stroke(&h.local, unrelated.1, later_stroke);
    let result_p = plan_with_local_and_tail(
        &h,
        &local_p,
        &pending_ok,
        &[],
        SequenceAuthority::Known {
            next_client_seq: h.next_client_seq + 1,
        },
    );
    let PlanResult::Feasible(plan_p) = &result_p else {
        panic!("later captured original blocked: {result_p:?}");
    };
    assert!(!plan_p
        .candidates
        .iter()
        .any(|c| matches!(&c.operation, SyncOperationV1::AddStroke { stroke, .. } if stroke.stroke_id == sk(70_010))));

    let mut later = Author::new(h.replica_id);
    later.library = h.library_id;
    later.next_seq = h.next_client_seq;
    let pending_dep = vec![later.env(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: target.0,
        page_id: target.1,
        replacements: vec![LineageReplacement {
            root_id: target.2,
            expected: {
                let page = h
                    .local
                    .notebooks
                    .iter()
                    .find(|n| n.notebook_id == target.0)
                    .unwrap()
                    .pages
                    .iter()
                    .find(|p| p.page_id == target.1)
                    .unwrap();
                page.strokes
                    .iter()
                    .filter(|s| s.root_id == target.2)
                    .cloned()
                    .collect()
            },
            after: vec![line(sk(70_020), target.2, (1.0, 1.0), (2.0, 2.0), 3, false)],
        }],
    })];
    let result_dep = plan_with_local_and_tail(
        &h,
        &h.local,
        &pending_dep,
        &[],
        SequenceAuthority::Known {
            next_client_seq: h.next_client_seq + 1,
        },
    );
    assert!(
        matches!(
            result_dep,
            PlanResult::Blocked(RecoveryBlock::NoLegalClientSequenceOrdering { .. })
                | PlanResult::Blocked(RecoveryBlock::PendingOperationDependency { .. })
                | PlanResult::Blocked(RecoveryBlock::ExactStateReproductionFailure { .. })
        ),
        "{result_dep:?}"
    );

    let invalid = plan_recovery(crate::sync_recovery_plan::RecoveryInputs {
        library_id: h.library_id,
        replica_id: h.replica_id,
        checkpoint: &h.checkpoint,
        checkpoint_cursor: h.checkpoint_cursor,
        acked_client_seq: h.acked_client_seq,
        accepted: &[],
        outbox: crate::sync_recovery_plan::OutboxInput::Invalid {
            detail: "unresolved prepare/commit pair",
        },
        local: &h.local,
        server_tail: &[],
        sequence_authority: SequenceAuthority::Known {
            next_client_seq: h.next_client_seq,
        },
    });
    assert!(matches!(
        invalid,
        PlanResult::Blocked(RecoveryBlock::InvalidOutbox { .. })
    ));

    let after = sentinels(&lib, &sync);
    let before_s = serde_json::to_string(&before).unwrap();
    let after_s = serde_json::to_string(&after).unwrap();
    assert_eq!(before_s, after_s, "reference sentinel hashes changed");

    #[derive(Serialize)]
    struct Report {
        extract_ms: u128,
        checkpoint_cursor: u64,
        acked_client_seq: u64,
        next_client_seq: u64,
        proof: CheckpointOnlyProof,
        kinds: Vec<&'static str>,
        scenarios: Vec<ScenarioOutcome>,
        sentinels_unchanged: bool,
        upload_authorized: bool,
        live_server_verified: bool,
        live_kindle_verified: bool,
        live_execution_authorized: bool,
    }
    let report = Report {
        extract_ms,
        checkpoint_cursor: h.checkpoint_cursor,
        acked_client_seq: h.acked_client_seq,
        next_client_seq: h.next_client_seq,
        proof,
        kinds: op_kinds(&plan.candidates),
        scenarios: vec![
            outcome_from_plan("A_empty_tail", &PlanResult::Feasible(plan.clone())),
            outcome_from_plan("B_unrelated_remote_original", &result_b),
            outcome_from_plan("C_unrelated_remote_erase", &result_c),
            outcome_from_plan("D_same_lineage_remote_erase", &result_d),
            outcome_from_plan("E_remote_lineage_replacement", &result_e),
            outcome_from_plan("F_remote_page_deletion", &result_f),
            outcome_from_plan("G_remote_notebook_deletion", &result_g),
            outcome_from_plan("H_identity_collision", &result_h),
            outcome_from_plan("I_invalid_tail", &result_i),
            outcome_from_plan("pending_later_original", &result_p),
            outcome_from_plan("pending_depends_on_fragment", &result_dep),
            outcome_from_plan("invalid_outbox", &invalid),
        ],
        sentinels_unchanged: true,
        upload_authorized: false,
        live_server_verified: false,
        live_kindle_verified: false,
        live_execution_authorized: false,
    };
    let _ = first_create_page(&plan);
    fs::write(
        out.join("rehearsal.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
}
