//! Slice 18E.4C: production-engine recovery planner proofs. Synthetic identities only.

use super::*;
use sync_engine::testkit::{library, line, nb, pg, render, replica, sk, Author};
use sync_engine::{LibraryState, Outcome};
use sync_protocol::{
    BackgroundRef, LineageReplacement, NotebookId, PageId, PageSpace, SnapshotNotebook,
    SnapshotPage, StrokeId, StrokeStyle, SyncEnvelopeV1, SyncOperationV1, SyncSnapshotV1,
    SyncStroke,
};

fn orig(n: u64) -> SyncStroke {
    line(
        sk(n),
        sk(n),
        (200.0, 200.0 + n as f32),
        (800.0, 200.0 + n as f32),
        4,
        false,
    )
}

fn frag(id: u64, root: u64, y: f32) -> SyncStroke {
    line(sk(id), sk(root), (200.0, y), (400.0, y), 3, false)
}

fn blank(id: PageId, strokes: Vec<SyncStroke>) -> SnapshotPage {
    SnapshotPage {
        page_id: id,
        background: BackgroundRef::Blank,
        render: render(false),
        strokes,
        texts: Vec::new(),
        links: Vec::new(),
    }
}

fn notebook(id: NotebookId, title: &str, pages: Vec<SnapshotPage>) -> SnapshotNotebook {
    SnapshotNotebook {
        notebook_id: id,
        title: title.into(),
        pages,
        folder_id: None,
    }
}

fn snap(notebooks: Vec<SnapshotNotebook>) -> SyncSnapshotV1 {
    SyncSnapshotV1 {
        v: 1,
        library_id: library(),
        page_space: PageSpace::CANONICAL,
        notebooks,
        folders: Vec::new(),
    }
}

fn genesis(notebooks: Vec<SnapshotNotebook>) -> LibraryState {
    LibraryState::genesis(snap(notebooks)).unwrap()
}

fn authority_for(acked: u64, pending: &[SyncEnvelopeV1]) -> SequenceAuthority {
    let next = pending
        .last()
        .map(|e| e.client_seq + 1)
        .unwrap_or(acked.saturating_add(1).max(1));
    let next = if acked == 0 && pending.is_empty() {
        1
    } else {
        next
    };
    SequenceAuthority::Known {
        next_client_seq: next,
    }
}

fn run(
    checkpoint: &LibraryState,
    local: &SyncSnapshotV1,
    accepted: &[SyncEnvelopeV1],
    pending: &[SyncEnvelopeV1],
    tail: &[ServerTailItem],
) -> PlanResult {
    plan_recovery(RecoveryInputs {
        library_id: library(),
        replica_id: replica(1),
        checkpoint,
        checkpoint_cursor: tail.first().map(|t| t.server_seq - 1).unwrap_or(0),
        acked_client_seq: if pending.is_empty() && accepted.is_empty() {
            0
        } else if accepted.is_empty() {
            pending.first().map(|e| e.client_seq - 1).unwrap_or(0)
        } else {
            accepted.last().map(|e| e.client_seq).unwrap_or(0)
        },
        accepted,
        outbox: OutboxInput::Valid { envelopes: pending },
        local,
        server_tail: tail,
        sequence_authority: authority_for(
            if accepted.is_empty() {
                pending
                    .first()
                    .map(|e| e.client_seq.saturating_sub(1))
                    .unwrap_or(0)
            } else {
                accepted.last().unwrap().client_seq
            },
            pending,
        ),
    })
}

fn prove(result: PlanResult, checkpoint: &LibraryState, local: &SyncSnapshotV1) -> RecoveryPlan {
    let PlanResult::Feasible(plan) = result else {
        panic!("expected feasible plan, got {result:?}");
    };
    assert!(!plan.upload_authorized);
    assert!(plan.exact_state_proven);
    let mut state = checkpoint.clone();
    for (i, c) in plan.candidates.iter().enumerate() {
        let env = sim_env(library(), replica(1), i as u64 + 1, c.operation.clone());
        let (next, out) = state.apply(&env).unwrap();
        assert!(
            matches!(out, Outcome::Applied | Outcome::NoOp(_)),
            "{out:?}"
        );
        state = next;
    }
    assert_eq!(state.snapshot, *local);
    plan
}

fn fragments(local: &SyncSnapshotV1) -> Vec<StrokeId> {
    local
        .notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .flat_map(|p| &p.strokes)
        .filter(|s| s.root_id != s.stroke_id)
        .map(|s| s.stroke_id)
        .collect()
}

#[test]
fn fixture_a_originals_only_uses_add_stroke() {
    let expected: Vec<_> = (1..=4).map(orig).collect();
    let extras: Vec<_> = (101..=108).map(orig).collect();
    let mut actual = expected.clone();
    actual.extend(extras.iter().cloned());
    let cp = genesis(vec![notebook(nb(1), "FX-A", vec![blank(pg(1), expected)])]);
    let local = snap(vec![notebook(nb(1), "FX-A", vec![blank(pg(1), actual)])]);
    let before = cp.clone();
    let plan = prove(run(&cp, &local, &[], &[], &[]), &cp, &local);
    assert_eq!(plan.counts.extra_originals, 8);
    assert_eq!(plan.counts.lineage_replacements, 0);
    assert!(plan
        .candidates
        .iter()
        .all(|c| matches!(c.operation, SyncOperationV1::AddStroke { .. })));
    assert_eq!(cp.snapshot, before.snapshot);
}

#[test]
fn interleaved_unseen_fragment_among_new_originals() {
    let cp = genesis(vec![notebook(nb(1), "INT", vec![blank(pg(1), vec![])])]);
    let actual = vec![orig(1), orig(2), frag(50, 40, 300.0), orig(3), orig(4)];
    let local = snap(vec![notebook(nb(1), "INT", vec![blank(pg(1), actual)])]);
    let plan = prove(run(&cp, &local, &[], &[], &[]), &cp, &local);
    assert_eq!(plan.counts.extra_originals, 4);
    assert_eq!(plan.counts.lineage_replacements, 1);
    assert_eq!(fragments(&local), vec![sk(50)]);
}

#[test]
fn fixture_b_prefix_plus_fragment_keeps_uuid() {
    let prefix: Vec<_> = (1..=5).map(orig).collect();
    let extras: Vec<_> = (200..=295).map(orig).collect();
    let piece = frag(400, 296, 900.0);
    let mut actual = prefix.clone();
    actual.extend(extras.iter().cloned());
    actual.push(piece.clone());
    let cp = genesis(vec![notebook(nb(2), "FX-B", vec![blank(pg(2), prefix)])]);
    let local = snap(vec![notebook(nb(2), "FX-B", vec![blank(pg(2), actual)])]);
    let plan = prove(run(&cp, &local, &[], &[], &[]), &cp, &local);
    assert_eq!(plan.counts.extra_originals, 96);
    assert_eq!(plan.counts.lineage_replacements, 1);
    assert_eq!(fragments(&local), vec![sk(400)]);
    assert!(plan
        .candidates
        .iter()
        .any(|c| matches!(c.operation, SyncOperationV1::ReplaceStrokeLineages { .. })));
}

#[test]
fn fixture_c_non_prefix_replacement_is_exact() {
    let keep: Vec<_> = (1..=6).map(orig).collect();
    let gone1 = orig(7);
    let gone2 = orig(8);
    let tail: Vec<_> = (9..=12).map(orig).collect();
    let mut expected = keep.clone();
    expected.push(gone1);
    expected.push(gone2);
    expected.extend(tail.clone());
    let f1 = frag(501, 7, 1100.0);
    let f2 = frag(502, 7, 1200.0);
    let f3 = frag(503, 8, 1300.0);
    let extras: Vec<_> = (600..=609).map(orig).collect();
    let mut actual = keep;
    actual.push(f1);
    actual.push(f2);
    actual.push(f3);
    actual.extend(tail);
    actual.extend(extras);
    let cp = genesis(vec![notebook(nb(3), "FX-C", vec![blank(pg(3), expected)])]);
    let local = snap(vec![notebook(nb(3), "FX-C", vec![blank(pg(3), actual)])]);
    let plan = prove(run(&cp, &local, &[], &[], &[]), &cp, &local);
    assert_eq!(plan.counts.extra_originals, 10);
    assert_eq!(plan.counts.lineage_replacements, 2);
    assert_eq!(fragments(&local), vec![sk(501), sk(502), sk(503)]);
}

#[test]
fn fixture_d_additional_pages_keep_identities() {
    let cp = genesis(vec![notebook(
        nb(4),
        "FX-D",
        vec![blank(pg(40), vec![orig(1)])],
    )]);
    let local = snap(vec![notebook(
        nb(4),
        "FX-D",
        vec![
            blank(pg(40), vec![orig(1)]),
            blank(pg(41), vec![]),
            blank(pg(42), vec![orig(701), orig(702)]),
        ],
    )]);
    let plan = prove(run(&cp, &local, &[], &[], &[]), &cp, &local);
    assert_eq!(plan.counts.extra_pages, 2);
    assert_eq!(plan.counts.extra_originals, 2);
    let pages: Vec<PageId> = local.notebooks[0].pages.iter().map(|p| p.page_id).collect();
    assert_eq!(pages, vec![pg(40), pg(41), pg(42)]);
}

#[test]
fn fixture_e_combined_review_80_shape() {
    let p1_exp: Vec<_> = (1..=3).map(orig).collect();
    let p1_extra: Vec<_> = (110..=117).map(orig).collect();
    let p2_exp: Vec<_> = (20..=22).map(orig).collect();
    let p2_extra: Vec<_> = (800..=895).map(orig).collect();
    let p2_frag = frag(900, 896, 400.0);
    let p3_keep: Vec<_> = (30..=33).map(orig).collect();
    let p3_g1 = orig(34);
    let p3_g2 = orig(35);
    let p3_f1 = frag(910, 34, 500.0);
    let p3_f2 = frag(911, 34, 510.0);
    let p3_f3 = frag(912, 35, 520.0);
    let p3_extra: Vec<_> = (920..=929).map(orig).collect();
    let p4_home = orig(40);

    let mut p3_exp = p3_keep.clone();
    p3_exp.push(p3_g1);
    p3_exp.push(p3_g2);
    let cp = genesis(vec![
        notebook(nb(1), "E1", vec![blank(pg(1), p1_exp.clone())]),
        notebook(nb(2), "E2", vec![blank(pg(2), p2_exp.clone())]),
        notebook(nb(3), "E3", vec![blank(pg(3), p3_exp)]),
        notebook(nb(4), "E4", vec![blank(pg(4), vec![p4_home.clone()])]),
    ]);
    let mut a1 = p1_exp;
    a1.extend(p1_extra.iter().cloned());
    let mut a2 = p2_exp;
    a2.extend(p2_extra.iter().cloned());
    a2.push(p2_frag);
    let mut a3 = p3_keep;
    a3.push(p3_f1);
    a3.push(p3_f2);
    a3.push(p3_f3);
    a3.extend(p3_extra.iter().cloned());
    let local = snap(vec![
        notebook(nb(1), "E1", vec![blank(pg(1), a1)]),
        notebook(nb(2), "E2", vec![blank(pg(2), a2)]),
        notebook(nb(3), "E3", vec![blank(pg(3), a3)]),
        notebook(
            nb(4),
            "E4",
            vec![
                blank(pg(4), vec![p4_home]),
                blank(pg(5), vec![]),
                blank(pg(6), vec![orig(41), orig(42)]),
            ],
        ),
    ]);
    assert_eq!(p1_extra.len() + p2_extra.len() + p3_extra.len(), 114);
    let plan = prove(run(&cp, &local, &[], &[], &[]), &cp, &local);
    assert_eq!(
        plan.counts.extra_originals, 116,
        "114 on existing pages + 2 on extra page"
    );
    assert_eq!(plan.counts.lineage_replacements, 3);
    assert_eq!(plan.counts.extra_pages, 2);
    assert_eq!(fragments(&local).len(), 4);
    assert!(!plan.sequencing.future_execution_protocol_required);
}

#[test]
fn pending_captured_ops_are_not_duplicated() {
    let cp = genesis(vec![notebook(
        nb(1),
        "P",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let extra = orig(50);
    let mut author = Author::new(replica(1));
    let pending = vec![author.env(SyncOperationV1::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: extra.clone(),
    })];
    let local = snap(vec![notebook(
        nb(1),
        "P",
        vec![blank(pg(1), vec![orig(1), extra.clone(), orig(51)])],
    )]);
    let PlanResult::Feasible(plan) = run(&cp, &local, &[], &pending, &[]) else {
        panic!("expected feasible");
    };
    assert!(!plan.upload_authorized);
    let mut state = cp.clone();
    state = state.apply(&pending[0]).unwrap().0;
    for (i, c) in plan.candidates.iter().enumerate() {
        state = state
            .apply(&sim_env(
                library(),
                replica(1),
                i as u64 + 1,
                c.operation.clone(),
            ))
            .unwrap()
            .0;
    }
    assert_eq!(state.snapshot, local);
    let adds: Vec<StrokeId> = plan
        .candidates
        .iter()
        .filter_map(|c| match &c.operation {
            SyncOperationV1::AddStroke { stroke, .. } => Some(stroke.stroke_id),
            _ => None,
        })
        .collect();
    assert_eq!(adds, vec![sk(51)]);
    assert_eq!(plan.counts.pending_preserved, 1);
}

#[test]
fn later_captured_cas_depending_on_fragment_blocks_sequence() {
    let root = orig(1);
    let f = frag(2, 1, 400.0);
    let f2 = frag(3, 1, 500.0);
    let cp = genesis(vec![notebook(
        nb(1),
        "DEP",
        vec![blank(pg(1), vec![root.clone()])],
    )]);
    let mut author = Author::new(replica(1));
    let pending = vec![author.env(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb(1),
        page_id: pg(1),
        replacements: vec![LineageReplacement {
            root_id: sk(1),
            expected: vec![f.clone()],
            after: vec![f2.clone()],
        }],
    })];
    let local = snap(vec![notebook(nb(1), "DEP", vec![blank(pg(1), vec![f2])])]);
    let result = run(&cp, &local, &[], &pending, &[]);
    assert!(
        matches!(
            result,
            PlanResult::Blocked(RecoveryBlock::NoLegalClientSequenceOrdering { .. })
                | PlanResult::Blocked(RecoveryBlock::PendingOperationDependency { .. })
        ),
        "{result:?}"
    );
    assert!(!result.upload_authorized());
}

#[test]
fn unrelated_remote_stroke_is_preserved() {
    let cp = genesis(vec![notebook(
        nb(1),
        "R",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let y = orig(70);
    let env = remote.env(SyncOperationV1::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: y.clone(),
    });
    let tail = [ServerTailItem {
        server_seq: 1,
        envelope: env,
    }];
    let extras: Vec<_> = (101..=108).map(orig).collect();
    let mut actual = vec![orig(1), y.clone()];
    actual.extend(extras);
    let local = snap(vec![notebook(nb(1), "R", vec![blank(pg(1), actual)])]);
    let plan = match run(&cp, &local, &[], &[], &tail) {
        PlanResult::Feasible(p) => p,
        other => panic!("{other:?}"),
    };
    assert!(!plan.upload_authorized);
    let mut state = cp.clone();
    state = state.apply(&tail[0].envelope).unwrap().0;
    for (i, c) in plan.candidates.iter().enumerate() {
        state = state
            .apply(&sim_env(
                library(),
                replica(1),
                i as u64 + 1,
                c.operation.clone(),
            ))
            .unwrap()
            .0;
    }
    assert_eq!(state.snapshot, local);
    assert!(state.snapshot.notebooks[0].pages[0]
        .strokes
        .iter()
        .any(|s| s.stroke_id == y.stroke_id));
}

#[test]
fn same_lineage_remote_change_is_refused() {
    let cp = genesis(vec![notebook(
        nb(1),
        "C",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let env = remote.env(SyncOperationV1::DeleteStrokeLineage {
        notebook_id: nb(1),
        page_id: pg(1),
        root_id: sk(1),
    });
    let tail = [ServerTailItem {
        server_seq: 1,
        envelope: env,
    }];
    let local = snap(vec![notebook(
        nb(1),
        "C",
        vec![blank(pg(1), vec![frag(2, 1, 400.0)])],
    )]);
    let result = run(&cp, &local, &[], &[], &tail);
    assert!(
        matches!(
            result,
            PlanResult::Blocked(RecoveryBlock::SameLineageConflict { .. })
        ),
        "{result:?}"
    );
}

#[test]
fn deleted_target_page_is_refused() {
    let cp = genesis(vec![notebook(
        nb(1),
        "DEL",
        vec![blank(pg(1), vec![orig(1)]), blank(pg(2), vec![orig(2)])],
    )]);
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let env = remote.env(SyncOperationV1::DeletePage {
        notebook_id: nb(1),
        page_id: pg(2),
    });
    let tail = [ServerTailItem {
        server_seq: 1,
        envelope: env,
    }];
    let local = snap(vec![notebook(
        nb(1),
        "DEL",
        vec![
            blank(pg(1), vec![orig(1)]),
            blank(pg(2), vec![orig(2), orig(3)]),
        ],
    )]);
    let result = run(&cp, &local, &[], &[], &tail);
    assert!(
        matches!(
            result,
            PlanResult::Blocked(RecoveryBlock::DeletedTarget { .. })
        ),
        "{result:?}"
    );
}

#[test]
fn stroke_id_collision_is_refused() {
    let cp = genesis(vec![notebook(
        nb(1),
        "ID",
        vec![blank(pg(1), vec![orig(1)]), blank(pg(2), vec![orig(9)])],
    )]);
    let local = snap(vec![notebook(
        nb(1),
        "ID",
        vec![
            blank(pg(1), vec![orig(1), orig(9)]),
            blank(pg(2), vec![orig(9)]),
        ],
    )]);
    let result = run(&cp, &local, &[], &[], &[]);
    assert!(
        matches!(
            result,
            PlanResult::Blocked(RecoveryBlock::IdentityCollision { .. })
        ),
        "{result:?}"
    );
}

#[test]
fn invalid_outbox_is_refused() {
    let cp = genesis(vec![notebook(
        nb(1),
        "X",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let local = cp.snapshot.clone();
    let result = plan_recovery(RecoveryInputs {
        library_id: library(),
        replica_id: replica(1),
        checkpoint: &cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        accepted: &[],
        outbox: OutboxInput::Invalid {
            detail: "corrupt outbox record",
        },
        local: &local,
        server_tail: &[],
        sequence_authority: SequenceAuthority::Known { next_client_seq: 1 },
    });
    assert!(matches!(
        result,
        PlanResult::Blocked(RecoveryBlock::InvalidOutbox { .. })
    ));
}

#[test]
fn unknown_sequence_authority_is_refused() {
    let cp = genesis(vec![notebook(
        nb(1),
        "X",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let local = cp.snapshot.clone();
    let result = plan_recovery(RecoveryInputs {
        library_id: library(),
        replica_id: replica(1),
        checkpoint: &cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        accepted: &[],
        outbox: OutboxInput::Valid { envelopes: &[] },
        local: &local,
        server_tail: &[],
        sequence_authority: SequenceAuthority::Unknown,
    });
    assert!(matches!(
        result,
        PlanResult::Blocked(RecoveryBlock::UnknownSequenceAuthority)
    ));
}

#[test]
fn sequence_gap_is_refused() {
    let cp = genesis(vec![notebook(
        nb(1),
        "X",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let mut author = Author::new(replica(1));
    author.next_seq = 3;
    let pending = vec![author.env(SyncOperationV1::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: orig(2),
    })];
    let local = cp.snapshot.clone();
    let result = plan_recovery(RecoveryInputs {
        library_id: library(),
        replica_id: replica(1),
        checkpoint: &cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        accepted: &[],
        outbox: OutboxInput::Valid {
            envelopes: &pending,
        },
        local: &local,
        server_tail: &[],
        sequence_authority: SequenceAuthority::Known { next_client_seq: 4 },
    });
    assert!(matches!(
        result,
        PlanResult::Blocked(RecoveryBlock::InconsistentAcceptedHistory { .. })
    ));
}

#[test]
fn duplicate_sequence_is_refused() {
    let cp = genesis(vec![notebook(
        nb(1),
        "X",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let mut author = Author::new(replica(1));
    let a = author.env(SyncOperationV1::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: orig(2),
    });
    let mut b = author.env(SyncOperationV1::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: orig(3),
    });
    b.client_seq = a.client_seq;
    let pending = vec![a, b];
    let local = cp.snapshot.clone();
    let result = plan_recovery(RecoveryInputs {
        library_id: library(),
        replica_id: replica(1),
        checkpoint: &cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        accepted: &[],
        outbox: OutboxInput::Valid {
            envelopes: &pending,
        },
        local: &local,
        server_tail: &[],
        sequence_authority: SequenceAuthority::Known { next_client_seq: 3 },
    });
    assert!(matches!(
        result,
        PlanResult::Blocked(RecoveryBlock::InconsistentAcceptedHistory { .. })
    ));
}

#[test]
fn unresolved_preparation_is_refused() {
    let cp = genesis(vec![notebook(
        nb(1),
        "X",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let local = cp.snapshot.clone();
    let result = plan_recovery(RecoveryInputs {
        library_id: library(),
        replica_id: replica(1),
        checkpoint: &cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        accepted: &[],
        outbox: OutboxInput::Invalid {
            detail: "unresolved prepare/commit pair",
        },
        local: &local,
        server_tail: &[],
        sequence_authority: SequenceAuthority::Known { next_client_seq: 1 },
    });
    assert!(matches!(
        result,
        PlanResult::Blocked(RecoveryBlock::InvalidOutbox { .. })
    ));
}

#[test]
fn conflicting_operation_identity_is_refused() {
    let cp = genesis(vec![notebook(
        nb(1),
        "X",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let mut local_author = Author::new(replica(1));
    let a = local_author.env(SyncOperationV1::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: orig(2),
    });
    let mut b = local_author.env(SyncOperationV1::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: orig(3),
    });
    b.operation_id = a.operation_id;
    let pending = vec![a, b];
    let local = cp.snapshot.clone();
    let result = plan_recovery(RecoveryInputs {
        library_id: library(),
        replica_id: replica(1),
        checkpoint: &cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        accepted: &[],
        outbox: OutboxInput::Valid {
            envelopes: &pending,
        },
        local: &local,
        server_tail: &[],
        sequence_authority: SequenceAuthority::Known { next_client_seq: 3 },
    });
    assert!(
        matches!(
            result,
            PlanResult::Blocked(RecoveryBlock::IdentityCollision { .. })
        ),
        "{result:?}"
    );
}

#[test]
fn deleted_target_notebook_is_refused() {
    let cp = genesis(vec![
        notebook(nb(1), "DELNB", vec![blank(pg(1), vec![orig(1)])]),
        notebook(nb(2), "KEEP", vec![blank(pg(2), vec![orig(9)])]),
    ]);
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let env = remote.env(SyncOperationV1::DeleteNotebook { notebook_id: nb(1) });
    let tail = [ServerTailItem {
        server_seq: 1,
        envelope: env,
    }];
    let local = snap(vec![
        notebook(nb(1), "DELNB", vec![blank(pg(1), vec![orig(1), orig(2)])]),
        notebook(nb(2), "KEEP", vec![blank(pg(2), vec![orig(9)])]),
    ]);
    let result = run(&cp, &local, &[], &[], &tail);
    assert!(
        matches!(
            result,
            PlanResult::Blocked(RecoveryBlock::DeletedTarget { .. })
        ),
        "{result:?}"
    );
}

#[test]
fn extra_page_id_collision_is_refused() {
    let cp = genesis(vec![
        notebook(nb(1), "A", vec![blank(pg(1), vec![orig(1)])]),
        notebook(nb(2), "B", vec![blank(pg(99), vec![orig(2)])]),
    ]);
    let local = snap(vec![
        notebook(
            nb(1),
            "A",
            vec![blank(pg(1), vec![orig(1)]), blank(pg(99), vec![])],
        ),
        notebook(nb(2), "B", vec![blank(pg(99), vec![orig(2)])]),
    ]);
    let result = run(&cp, &local, &[], &[], &[]);
    assert!(
        matches!(
            result,
            PlanResult::Blocked(RecoveryBlock::IdentityCollision { .. })
        ),
        "{result:?}"
    );
}

#[test]
fn unrelated_remote_erase_is_preserved() {
    let cp = genesis(vec![notebook(
        nb(1),
        "ER",
        vec![blank(pg(1), vec![orig(1), orig(2)])],
    )]);
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let env = remote.env(SyncOperationV1::DeleteStrokeLineage {
        notebook_id: nb(1),
        page_id: pg(1),
        root_id: sk(2),
    });
    let tail = [ServerTailItem {
        server_seq: 1,
        envelope: env,
    }];
    let extras: Vec<_> = (101..=104).map(orig).collect();
    let mut actual = vec![orig(1)];
    actual.extend(extras);
    let local = snap(vec![notebook(nb(1), "ER", vec![blank(pg(1), actual)])]);
    let plan = match run(&cp, &local, &[], &[], &tail) {
        PlanResult::Feasible(p) => p,
        other => panic!("{other:?}"),
    };
    let mut state = cp.clone();
    state = state.apply(&tail[0].envelope).unwrap().0;
    for (i, c) in plan.candidates.iter().enumerate() {
        state = state
            .apply(&sim_env(
                library(),
                replica(1),
                i as u64 + 1,
                c.operation.clone(),
            ))
            .unwrap()
            .0;
    }
    assert_eq!(state.snapshot, local);
    assert!(state.snapshot.notebooks[0].pages[0]
        .strokes
        .iter()
        .all(|s| s.stroke_id != sk(2)));
}

#[test]
fn discontinuous_server_tail_is_refused() {
    let cp = genesis(vec![notebook(
        nb(1),
        "X",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let env = remote.env(SyncOperationV1::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: orig(2),
    });
    let tail = [ServerTailItem {
        server_seq: 3,
        envelope: env,
    }];
    let local = cp.snapshot.clone();
    let result = plan_recovery(RecoveryInputs {
        library_id: library(),
        replica_id: replica(1),
        checkpoint: &cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        accepted: &[],
        outbox: OutboxInput::Valid { envelopes: &[] },
        local: &local,
        server_tail: &tail,
        sequence_authority: SequenceAuthority::Known { next_client_seq: 1 },
    });
    assert!(matches!(
        result,
        PlanResult::Blocked(RecoveryBlock::StaleAuthoritativeTail { .. })
    ));
}

#[test]
fn planning_is_deterministic() {
    let expected: Vec<_> = (1..=3).map(orig).collect();
    let extras: Vec<_> = (110..=117).map(orig).collect();
    let mut actual = expected.clone();
    actual.extend(extras);
    let cp = genesis(vec![notebook(nb(1), "D", vec![blank(pg(1), expected)])]);
    let local = snap(vec![notebook(nb(1), "D", vec![blank(pg(1), actual)])]);
    let a = run(&cp, &local, &[], &[], &[]);
    let b = run(&cp, &local, &[], &[], &[]);
    match (a, b) {
        (PlanResult::Feasible(x), PlanResult::Feasible(y)) => {
            assert_eq!(x.fingerprint, y.fingerprint);
            assert_eq!(x.candidates, y.candidates);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn noop_already_applied_is_not_ledger_identity() {
    let cp = genesis(vec![notebook(
        nb(1),
        "N",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let local = cp.snapshot.clone();
    let plan = prove(run(&cp, &local, &[], &[], &[]), &cp, &local);
    assert!(plan.candidates.is_empty());
    assert!(!plan.upload_authorized);
}

#[test]
fn same_lineage_remote_replacement_is_refused() {
    let cp = genesis(vec![notebook(
        nb(1),
        "RR",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let env = remote.env(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb(1),
        page_id: pg(1),
        replacements: vec![LineageReplacement {
            root_id: sk(1),
            expected: vec![orig(1)],
            after: vec![frag(9, 1, 400.0)],
        }],
    });
    let tail = [ServerTailItem {
        server_seq: 1,
        envelope: env,
    }];
    let local = snap(vec![notebook(
        nb(1),
        "RR",
        vec![blank(pg(1), vec![frag(2, 1, 350.0)])],
    )]);
    let result = run(&cp, &local, &[], &[], &tail);
    assert!(
        matches!(
            result,
            PlanResult::Blocked(RecoveryBlock::SameLineageConflict { .. })
        ),
        "{result:?}"
    );
}

#[test]
fn negative_wrong_fragment_id_does_not_match_true_local() {
    let prefix: Vec<_> = (1..=3).map(orig).collect();
    let piece = frag(400, 1, 900.0);
    let mut actual = prefix.clone();
    actual.push(piece);
    let cp = genesis(vec![notebook(nb(1), "NEG", vec![blank(pg(1), prefix)])]);
    let true_local = snap(vec![notebook(nb(1), "NEG", vec![blank(pg(1), actual)])]);
    let mut wrong = true_local.clone();
    wrong.notebooks[0].pages[0]
        .strokes
        .last_mut()
        .unwrap()
        .stroke_id = sk(401);
    match run(&cp, &wrong, &[], &[], &[]) {
        PlanResult::Blocked(_) => {}
        PlanResult::Feasible(plan) => {
            let mut state = cp.clone();
            for (i, c) in plan.candidates.iter().enumerate() {
                state = state
                    .apply(&sim_env(
                        library(),
                        replica(1),
                        i as u64 + 1,
                        c.operation.clone(),
                    ))
                    .unwrap()
                    .0;
            }
            assert_ne!(state.snapshot, true_local);
        }
    }
}

#[test]
fn negative_missing_extra_page_is_detected() {
    let cp = genesis(vec![notebook(
        nb(1),
        "NP",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let full = snap(vec![notebook(
        nb(1),
        "NP",
        vec![
            blank(pg(1), vec![orig(1)]),
            blank(pg(2), vec![]),
            blank(pg(3), vec![orig(4)]),
        ],
    )]);
    let missing = snap(vec![notebook(
        nb(1),
        "NP",
        vec![blank(pg(1), vec![orig(1)]), blank(pg(2), vec![])],
    )]);
    let plan = prove(run(&cp, &missing, &[], &[], &[]), &cp, &missing);
    let mut state = cp.clone();
    for (i, c) in plan.candidates.iter().enumerate() {
        state = state
            .apply(&sim_env(
                library(),
                replica(1),
                i as u64 + 1,
                c.operation.clone(),
            ))
            .unwrap()
            .0;
    }
    assert_ne!(state.snapshot, full);
}

#[test]
fn negative_wrong_render_order_does_not_equal() {
    let cp = genesis(vec![notebook(
        nb(1),
        "ORD",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let local = snap(vec![notebook(
        nb(1),
        "ORD",
        vec![blank(pg(1), vec![orig(1), orig(2), orig(3)])],
    )]);
    let mut swapped = local.clone();
    swapped.notebooks[0].pages[0].strokes.swap(1, 2);
    let plan = prove(run(&cp, &local, &[], &[], &[]), &cp, &local);
    let mut state = cp.clone();
    for (i, c) in plan.candidates.iter().enumerate() {
        state = state
            .apply(&sim_env(
                library(),
                replica(1),
                i as u64 + 1,
                c.operation.clone(),
            ))
            .unwrap()
            .0;
    }
    assert_ne!(state.snapshot, swapped);
    assert_eq!(state.snapshot, local);
}

#[test]
fn negative_changed_stroke_attribute_is_detected() {
    let cp = genesis(vec![notebook(
        nb(1),
        "ATTR",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let local = snap(vec![notebook(
        nb(1),
        "ATTR",
        vec![blank(pg(1), vec![orig(1), orig(2)])],
    )]);
    let plan = prove(run(&cp, &local, &[], &[], &[]), &cp, &local);
    let mut mutated = local.clone();
    mutated.notebooks[0].pages[0].strokes[1].style = StrokeStyle::PressureWidthV1;
    let mut state = cp.clone();
    for (i, c) in plan.candidates.iter().enumerate() {
        state = state
            .apply(&sim_env(
                library(),
                replica(1),
                i as u64 + 1,
                c.operation.clone(),
            ))
            .unwrap()
            .0;
    }
    assert_ne!(state.snapshot, mutated);
    assert_eq!(state.snapshot, local);
}

#[test]
fn negative_duplicate_original_is_detected() {
    let cp = genesis(vec![notebook(
        nb(1),
        "DUP",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let local = snap(vec![notebook(
        nb(1),
        "DUP",
        vec![blank(pg(1), vec![orig(1), orig(2)])],
    )]);
    let plan = prove(run(&cp, &local, &[], &[], &[]), &cp, &local);
    let mut dup = local.clone();
    dup.notebooks[0].pages[0].strokes.push(orig(1));
    let mut state = cp.clone();
    for (i, c) in plan.candidates.iter().enumerate() {
        state = state
            .apply(&sim_env(
                library(),
                replica(1),
                i as u64 + 1,
                c.operation.clone(),
            ))
            .unwrap()
            .0;
    }
    assert_ne!(state.snapshot, dup);
    assert_eq!(state.snapshot, local);
}

#[test]
fn negative_changed_lineage_root_does_not_match() {
    let prefix: Vec<_> = (1..=3).map(orig).collect();
    let piece = frag(400, 1, 900.0);
    let mut actual = prefix.clone();
    actual.push(piece);
    let cp = genesis(vec![notebook(nb(1), "ROOT", vec![blank(pg(1), prefix)])]);
    let true_local = snap(vec![notebook(nb(1), "ROOT", vec![blank(pg(1), actual)])]);
    let mut wrong = true_local.clone();
    wrong.notebooks[0].pages[0]
        .strokes
        .last_mut()
        .unwrap()
        .root_id = sk(2);
    match run(&cp, &wrong, &[], &[], &[]) {
        PlanResult::Blocked(_) => {}
        PlanResult::Feasible(plan) => {
            let mut state = cp.clone();
            for (i, c) in plan.candidates.iter().enumerate() {
                state = state
                    .apply(&sim_env(
                        library(),
                        replica(1),
                        i as u64 + 1,
                        c.operation.clone(),
                    ))
                    .unwrap()
                    .0;
            }
            assert_ne!(state.snapshot, true_local);
        }
    }
}

#[test]
fn later_captured_unrelated_erase_is_not_duplicated() {
    let cp = genesis(vec![notebook(
        nb(1),
        "PE",
        vec![blank(pg(1), vec![orig(1), orig(2)])],
    )]);
    let mut author = Author::new(replica(1));
    let pending = vec![author.env(SyncOperationV1::DeleteStrokeLineage {
        notebook_id: nb(1),
        page_id: pg(1),
        root_id: sk(2),
    })];
    let local = snap(vec![notebook(
        nb(1),
        "PE",
        vec![blank(pg(1), vec![orig(1), orig(50)])],
    )]);
    let PlanResult::Feasible(plan) = run(&cp, &local, &[], &pending, &[]) else {
        panic!("expected feasible");
    };
    assert_eq!(plan.counts.pending_preserved, 1);
    assert_eq!(plan.counts.extra_originals, 1);
    let mut state = cp.clone();
    state = state.apply(&pending[0]).unwrap().0;
    for (i, c) in plan.candidates.iter().enumerate() {
        state = state
            .apply(&sim_env(
                library(),
                replica(1),
                i as u64 + 1,
                c.operation.clone(),
            ))
            .unwrap()
            .0;
    }
    assert_eq!(state.snapshot, local);
    assert!(!plan.candidates.iter().any(|c| {
        matches!(
            &c.operation,
            SyncOperationV1::DeleteStrokeLineage { root_id, .. } if *root_id == sk(2)
        )
    }));
}

#[test]
fn multiple_pending_contiguous_sequences_are_preserved() {
    let cp = genesis(vec![notebook(
        nb(1),
        "MP",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let mut author = Author::new(replica(1));
    let pending = vec![
        author.env(SyncOperationV1::AddStroke {
            notebook_id: nb(1),
            page_id: pg(1),
            stroke: orig(50),
        }),
        author.env(SyncOperationV1::AddStroke {
            notebook_id: nb(1),
            page_id: pg(1),
            stroke: orig(51),
        }),
    ];
    let local = snap(vec![notebook(
        nb(1),
        "MP",
        vec![blank(pg(1), vec![orig(1), orig(50), orig(51), orig(52)])],
    )]);
    let PlanResult::Feasible(plan) = run(&cp, &local, &[], &pending, &[]) else {
        panic!("expected feasible");
    };
    assert_eq!(plan.counts.pending_preserved, 2);
    let adds: Vec<StrokeId> = plan
        .candidates
        .iter()
        .filter_map(|c| match &c.operation {
            SyncOperationV1::AddStroke { stroke, .. } => Some(stroke.stroke_id),
            _ => None,
        })
        .collect();
    assert_eq!(adds, vec![sk(52)]);
    let mut state = cp.clone();
    for env in &pending {
        state = state.apply(env).unwrap().0;
    }
    for (i, c) in plan.candidates.iter().enumerate() {
        state = state
            .apply(&sim_env(
                library(),
                replica(1),
                i as u64 + 1,
                c.operation.clone(),
            ))
            .unwrap()
            .0;
    }
    assert_eq!(state.snapshot, local);
}

#[test]
fn later_captured_touching_recovered_lineage_blocks() {
    let cp = genesis(vec![notebook(
        nb(1),
        "TOUCH",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let mut author = Author::new(replica(1));
    let pending = vec![author.env(SyncOperationV1::DeleteStrokeLineage {
        notebook_id: nb(1),
        page_id: pg(1),
        root_id: sk(1),
    })];
    let local = snap(vec![notebook(
        nb(1),
        "TOUCH",
        vec![blank(pg(1), vec![frag(2, 1, 400.0)])],
    )]);
    let result = run(&cp, &local, &[], &pending, &[]);
    assert!(
        matches!(
            result,
            PlanResult::Blocked(RecoveryBlock::NoLegalClientSequenceOrdering { .. })
                | PlanResult::Blocked(RecoveryBlock::PendingOperationDependency { .. })
                | PlanResult::Blocked(RecoveryBlock::SameLineageConflict { .. })
                | PlanResult::Blocked(RecoveryBlock::ExactStateReproductionFailure { .. })
        ),
        "{result:?}"
    );
    assert!(!result.upload_authorized());
}
