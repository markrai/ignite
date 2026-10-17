//! Reducer semantics, refusals, the observed-roots erase rule, and determinism.

use std::collections::BTreeSet;

use sync_engine::testkit::{
    self, fd, genesis, line, lk, nb, page_link, page_spec, pg, replica, sk, text_obj, tx, Author,
};
use sync_engine::{fragment_id, replay, LibraryState, Outcome, SemanticErrorCode as C};
use sync_protocol::{
    global_stroke_id, snapshot_sha256, BackgroundRef, StrokeId, SyncOperationV1 as Op, SyncPoint,
};

fn st() -> LibraryState {
    LibraryState::genesis(genesis()).unwrap()
}

fn apply(s: &LibraryState, a: &mut Author, op: Op) -> (LibraryState, Outcome) {
    let env = a.env(op);
    let r = s.apply(&env).unwrap();
    assert_in_place_parity(s, &env);
    r
}

fn refuse(s: &LibraryState, a: &mut Author, op: Op) -> C {
    let before = s.clone();
    let env = a.env(op);
    let e = s.apply(&env).unwrap_err();
    assert_eq!(*s, before);
    assert_in_place_parity(s, &env);
    e.code
}

/// `apply_in_place` must agree with `apply` exactly: state + outcome, or the identical error.
fn assert_in_place_parity(s: &LibraryState, env: &sync_protocol::SyncEnvelopeV1) {
    let mut in_place = s.clone();
    match (s.apply(env), in_place.apply_in_place(env)) {
        (Ok((next, o1)), Ok(o2)) => {
            assert_eq!(o1, o2);
            assert_eq!(next, in_place);
        }
        (Err(e1), Err(e2)) => assert_eq!(e1, e2),
        (a, b) => panic!("apply {a:?} vs apply_in_place {b:?}"),
    }
}

fn page_ids(s: &LibraryState, n: usize) -> Vec<sync_protocol::PageId> {
    s.snapshot.notebooks[n]
        .pages
        .iter()
        .map(|p| p.page_id)
        .collect()
}

fn strokes(s: &LibraryState, n: usize, p: usize) -> &Vec<sync_protocol::SyncStroke> {
    &s.snapshot.notebooks[n].pages[p].strokes
}

// ------------------------------------------------------------------ structure

#[test]
fn notebook_lifecycle_and_placement() {
    let mut a = Author::new(replica(1));
    let s = st();
    // None → first; Some(existing) → right after; Some(unknown) → end.
    let (s, o) = apply(
        &s,
        &mut a,
        Op::CreateNotebook {
            notebook_id: nb(10),
            title: "First".into(),
            after_notebook_id: None,
            first_page: page_spec(pg(10)),
        },
    );
    assert_eq!(o, Outcome::Applied);
    assert_eq!(s.snapshot.notebooks[0].notebook_id, nb(10));
    let (s, _) = apply(
        &s,
        &mut a,
        Op::CreateNotebook {
            notebook_id: nb(11),
            title: "After Work".into(),
            after_notebook_id: Some(nb(2)),
            first_page: page_spec(pg(11)),
        },
    );
    let order: Vec<_> = s.snapshot.notebooks.iter().map(|n| n.notebook_id).collect();
    assert_eq!(order, vec![nb(10), nb(1), nb(2), nb(11), nb(3)]);
    let (s, _) = apply(
        &s,
        &mut a,
        Op::CreateNotebook {
            notebook_id: nb(12),
            title: "Orphan anchor".into(),
            after_notebook_id: Some(nb(999)),
            first_page: page_spec(pg(12)),
        },
    );
    assert_eq!(s.snapshot.notebooks.last().unwrap().notebook_id, nb(12));
    assert_eq!(
        s.snapshot.notebooks[3].pages.len(),
        1,
        "created with its first page"
    );

    let (s, _) = apply(
        &s,
        &mut a,
        Op::RenameNotebook {
            notebook_id: nb(1),
            title: "Renamed".into(),
        },
    );
    assert_eq!(s.snapshot.notebooks[1].title, "Renamed");

    let (s, o) = apply(&s, &mut a, Op::DeleteNotebook { notebook_id: nb(2) });
    assert_eq!(o, Outcome::Applied);
    assert!(s.history.deleted_notebooks.contains(&nb(2)));
    assert!(s.history.deleted_pages.contains(&pg(3)));
    assert!(s.history.deleted_strokes.contains(&sk(2)));
    // Deleted target → deterministic no-op (delete wins), state unchanged.
    for op in [
        Op::DeleteNotebook { notebook_id: nb(2) },
        Op::RenameNotebook {
            notebook_id: nb(2),
            title: "Late".into(),
        },
        Op::CreatePage {
            notebook_id: nb(2),
            page: page_spec(pg(50)),
            after_page_id: None,
        },
        Op::AddStroke {
            notebook_id: nb(2),
            page_id: pg(3),
            stroke: line(sk(50), sk(50), (1.0, 1.0), (2.0, 2.0), 2, false),
        },
    ] {
        let (s2, o) = apply(&s, &mut a, op);
        assert!(matches!(o, Outcome::NoOp(_)), "{o:?}");
        assert_eq!(s2, s);
    }
    // IDs are never reused.
    assert_eq!(
        refuse(
            &s,
            &mut a,
            Op::CreateNotebook {
                notebook_id: nb(2),
                title: "Again".into(),
                after_notebook_id: None,
                first_page: page_spec(pg(60)),
            }
        ),
        C::DuplicateNotebook
    );
    assert_eq!(
        refuse(
            &s,
            &mut a,
            Op::CreateNotebook {
                notebook_id: nb(61),
                title: "Reuse page".into(),
                after_notebook_id: None,
                first_page: page_spec(pg(3)),
            }
        ),
        C::DuplicatePage
    );
}

#[test]
fn page_create_move_delete_and_sole_page_rule() {
    let mut a = Author::new(replica(1));
    let s = st();
    let (s, _) = apply(
        &s,
        &mut a,
        Op::CreatePage {
            notebook_id: nb(1),
            page: page_spec(pg(20)),
            after_page_id: Some(pg(1)),
        },
    );
    assert_eq!(page_ids(&s, 0), vec![pg(1), pg(20), pg(2)]);
    let (s, _) = apply(
        &s,
        &mut a,
        Op::CreatePage {
            notebook_id: nb(1),
            page: page_spec(pg(21)),
            after_page_id: None,
        },
    );
    assert_eq!(page_ids(&s, 0), vec![pg(21), pg(1), pg(20), pg(2)]);
    let (s, _) = apply(
        &s,
        &mut a,
        Op::MovePage {
            notebook_id: nb(1),
            page_id: pg(21),
            after_page_id: Some(pg(2)),
        },
    );
    assert_eq!(page_ids(&s, 0), vec![pg(1), pg(20), pg(2), pg(21)]);
    let (s, _) = apply(
        &s,
        &mut a,
        Op::MovePage {
            notebook_id: nb(1),
            page_id: pg(2),
            after_page_id: None,
        },
    );
    assert_eq!(page_ids(&s, 0), vec![pg(2), pg(1), pg(20), pg(21)]);
    // Anchor elsewhere / unknown → end.
    let (s, _) = apply(
        &s,
        &mut a,
        Op::MovePage {
            notebook_id: nb(1),
            page_id: pg(2),
            after_page_id: Some(pg(4)),
        },
    );
    assert_eq!(page_ids(&s, 0), vec![pg(1), pg(20), pg(21), pg(2)]);
    let (s, _) = apply(
        &s,
        &mut a,
        Op::DeletePage {
            notebook_id: nb(1),
            page_id: pg(1),
        },
    );
    assert_eq!(page_ids(&s, 0), vec![pg(20), pg(21), pg(2)]);
    let legacy = global_stroke_id(pg(1), "s-1727000000000-1").unwrap();
    assert!(s.history.deleted_strokes.contains(&legacy));
    // Sole page / sole notebook are refused.
    assert_eq!(
        refuse(
            &s,
            &mut a,
            Op::DeletePage {
                notebook_id: nb(3),
                page_id: pg(4)
            }
        ),
        C::SolePage
    );
    let (s, _) = apply(&s, &mut a, Op::DeleteNotebook { notebook_id: nb(2) });
    let (s, _) = apply(&s, &mut a, Op::DeleteNotebook { notebook_id: nb(3) });
    assert_eq!(
        refuse(&s, &mut a, Op::DeleteNotebook { notebook_id: nb(1) }),
        C::SoleNotebook
    );
    // Deleted page → no-op for page-scoped ops.
    let (s2, o) = apply(
        &s,
        &mut a,
        Op::MovePage {
            notebook_id: nb(1),
            page_id: pg(1),
            after_page_id: None,
        },
    );
    assert!(matches!(o, Outcome::NoOp(_)));
    assert_eq!(s2, s);
}

// ------------------------------------------------------------------ strokes

#[test]
fn add_stroke_and_delete_lineage() {
    let mut a = Author::new(replica(1));
    let s = st();
    let new = line(sk(30), sk(30), (100.0, 100.0), (900.0, 150.0), 5, true);
    let (s, _) = apply(
        &s,
        &mut a,
        Op::AddStroke {
            notebook_id: nb(1),
            page_id: pg(1),
            stroke: new.clone(),
        },
    );
    assert_eq!(
        strokes(&s, 0, 0).last().unwrap(),
        &new,
        "exact geometry, top of render order"
    );
    assert_eq!(
        refuse(
            &s,
            &mut a,
            Op::AddStroke {
                notebook_id: nb(1),
                page_id: pg(2),
                stroke: new.clone()
            }
        ),
        C::DuplicateStroke
    );
    // Whole-lineage delete removes both fragments of the erased legacy root, nothing else.
    let root = global_stroke_id(pg(1), "s-1727000000000-2").unwrap();
    let before: Vec<_> = strokes(&s, 0, 0).clone();
    let (s, o) = apply(
        &s,
        &mut a,
        Op::DeleteStrokeLineage {
            notebook_id: nb(1),
            page_id: pg(1),
            root_id: root,
        },
    );
    assert_eq!(o, Outcome::Applied);
    let after = strokes(&s, 0, 0);
    assert_eq!(after.len(), before.len() - 2);
    assert!(after.iter().all(|x| x.root_id != root));
    let kept: Vec<_> = before
        .iter()
        .filter(|x| x.root_id != root)
        .cloned()
        .collect();
    assert_eq!(after, &kept, "unrelated geometry untouched and in order");
    // Again → no visible fragments → no-op; unknown root → refusal.
    let (_, o) = apply(
        &s,
        &mut a,
        Op::DeleteStrokeLineage {
            notebook_id: nb(1),
            page_id: pg(1),
            root_id: root,
        },
    );
    assert!(matches!(o, Outcome::NoOp(_)));
    assert_eq!(
        refuse(
            &s,
            &mut a,
            Op::DeleteStrokeLineage {
                notebook_id: nb(1),
                page_id: pg(1),
                root_id: sk(999)
            }
        ),
        C::UnknownLineage
    );
}

#[test]
fn move_lineages_preserves_identity_replays_and_inverts() {
    let mut author = Author::new(replica(1));
    let base = genesis();
    let state = LibraryState::genesis(base.clone()).unwrap();
    let added = line(sk(30), sk(30), (100.0, 100.0), (900.0, 150.0), 5, true);
    let add = author.env(Op::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: added.clone(),
    });
    let (state, _) = state.apply(&add).unwrap();
    let movement = author.env(Op::MoveStrokeLineages {
        notebook_id: nb(1),
        page_id: pg(1),
        root_ids: vec![sk(30)],
        dx: 50.0,
        dy: 70.0,
    });
    let (moved, outcome) = state.apply(&movement).unwrap();
    assert_eq!(outcome, Outcome::Applied);
    let got = strokes(&moved, 0, 0)
        .iter()
        .find(|stroke| stroke.stroke_id == sk(30))
        .unwrap();
    assert_eq!(got.stroke_id, added.stroke_id);
    assert_eq!(got.root_id, added.root_id);
    assert_eq!(got.points[0].x, added.points[0].x + 50.0);
    assert_eq!(got.points[0].y, added.points[0].y + 70.0);

    let replayed = replay(base, [&add, &movement]).unwrap();
    assert_eq!(replayed, moved, "fresh reconstruction reproduces the move");

    let inverse = author.env(Op::MoveStrokeLineages {
        notebook_id: nb(1),
        page_id: pg(1),
        root_ids: vec![sk(30)],
        dx: -50.0,
        dy: -70.0,
    });
    let (restored, _) = moved.apply(&inverse).unwrap();
    assert_eq!(
        restored.snapshot.notebooks[0].pages[0]
            .strokes
            .iter()
            .find(|stroke| stroke.stroke_id == sk(30))
            .unwrap(),
        &added,
        "inverse translation restores exact canonical geometry"
    );

    assert_eq!(
        refuse(
            &state,
            &mut author,
            Op::MoveStrokeLineages {
                notebook_id: nb(1),
                page_id: pg(1),
                root_ids: vec![sk(999)],
                dx: 1.0,
                dy: 0.0,
            }
        ),
        C::UnknownLineage
    );
    assert_eq!(
        refuse(
            &state,
            &mut author,
            Op::MoveStrokeLineages {
                notebook_id: nb(1),
                page_id: pg(1),
                root_ids: vec![sk(30)],
                dx: -1000.0,
                dy: 0.0,
            }
        ),
        C::InvalidResult
    );
}

fn erase(observed: Vec<StrokeId>, path: Vec<SyncPoint>) -> Op {
    let mut observed = observed;
    observed.sort();
    Op::EraseRegion {
        notebook_id: nb(1),
        page_id: pg(1),
        observed_roots: observed,
        path,
        radius: 240.0,
    }
}

#[test]
fn erase_region_splits_observed_lineage_with_derived_ids_and_pressure() {
    let mut a = Author::new(replica(1));
    let s = st();
    // Vertical sweep through the middle of sk(1) (horizontal, y=3000, x 1000..9000).
    let op = erase(
        vec![sk(1)],
        vec![
            SyncPoint::new(5000.0, 2500.0, None),
            SyncPoint::new(5000.0, 3500.0, None),
        ],
    );
    let env = a.env(op);
    let (s2, o) = s.apply(&env).unwrap();
    assert_in_place_parity(&s, &env);
    assert_eq!(o, Outcome::Applied);
    let page = strokes(&s2, 0, 0);
    let frags: Vec<_> = page.iter().filter(|x| x.root_id == sk(1)).collect();
    assert_eq!(frags.len(), 2);
    for (i, f) in frags.iter().enumerate() {
        assert_eq!(f.stroke_id, fragment_id(env.operation_id, sk(1), i));
        assert_eq!(
            f.stroke_id.as_uuid(),
            uuid::Uuid::new_v5(
                &env.operation_id.as_uuid(),
                format!("{}/{i}", sk(1)).as_bytes()
            )
        );
        assert_eq!(f.style, sync_protocol::StrokeStyle::PressureWidthV1);
    }
    // Render slot: fragments replace the source in place.
    assert_eq!(page[1].stroke_id, frags[0].stroke_id);
    assert_eq!(page[2].stroke_id, frags[1].stroke_id);
    // Boundaries ~ x=4760 / x=5240; interpolated pressure is 12-bit quantized.
    let entry = frags[0].points.last().unwrap();
    assert!((entry.x - 4760.0).abs() < 0.05, "{entry:?}");
    let q = entry.pressure.unwrap() * 4095.0;
    assert!((q - q.round()).abs() < 1e-3, "quantized pressure {q}");
    assert!(s2.history.deleted_strokes.contains(&sk(1)));
    // Other strokes byte-identical.
    for x in strokes(&s, 0, 0).iter().filter(|x| x.root_id != sk(1)) {
        assert!(page.contains(x));
    }
    // A miss is a no-op.
    let miss = erase(vec![sk(1)], vec![SyncPoint::new(100.0, 15000.0, None)]);
    let (s3, o) = apply(&s2, &mut a, miss);
    assert!(matches!(o, Outcome::NoOp(_)));
    assert_eq!(s3, s2);
}

/// Pinned S1 observed-roots rule: an erase only cuts lineages its author observed.
#[test]
fn erase_never_cuts_an_unobserved_concurrent_stroke() {
    let mut a = Author::new(replica(1));
    let mut b = Author::new(replica(2));
    let s = st();
    // B adds a stroke crossing exactly where A (who never saw it) will erase.
    let b_stroke = line(sk(40), sk(40), (4000.0, 2800.0), (6000.0, 3200.0), 9, true);
    let (s, _) = apply(
        &s,
        &mut b,
        Op::AddStroke {
            notebook_id: nb(1),
            page_id: pg(1),
            stroke: b_stroke.clone(),
        },
    );
    let path = vec![
        SyncPoint::new(5000.0, 2000.0, None),
        SyncPoint::new(5000.0, 4000.0, None),
    ];
    let (s, o) = apply(&s, &mut a, erase(vec![sk(1)], path.clone()));
    assert_eq!(o, Outcome::Applied);
    assert!(
        strokes(&s, 0, 0).contains(&b_stroke),
        "B's unseen stroke survives byte-for-byte"
    );
    assert!(!strokes(&s, 0, 0).iter().any(|x| x.stroke_id == sk(1)));
    // Had A observed it, it would have been cut.
    let (s2, _) = apply(&s, &mut b, erase(vec![sk(40)], path));
    assert!(!strokes(&s2, 0, 0).contains(&b_stroke));
    assert_eq!(
        strokes(&s2, 0, 0)
            .iter()
            .filter(|x| x.root_id == sk(40))
            .count(),
        2
    );
}

#[test]
fn erase_result_is_pinned_bit_for_bit() {
    // Cross-platform determinism pin: Windows, Linux and wasm32 must all produce this hash.
    let mut a = Author::new(replica(7));
    let path = vec![
        SyncPoint::new(1500.0, 800.0, Some(0.5)),
        SyncPoint::new(1700.0, 1200.0, Some(0.5)),
        SyncPoint::new(5000.0, 2900.0, None),
        SyncPoint::new(5003.5, 3100.25, None),
    ];
    let legacy = global_stroke_id(pg(1), "s-1727000000000-1").unwrap();
    let (s, o) = apply(&st(), &mut a, erase(vec![legacy, sk(1)], path));
    assert_eq!(o, Outcome::Applied);
    let h = snapshot_sha256(&s.snapshot).unwrap();
    assert_eq!(h.as_str(), PINNED_ERASE_SHA256, "erase output changed");
}

const PINNED_ERASE_SHA256: &str =
    "0d1c54bd937e1b6a8017df897fc6c66cd3ea04f98c89f2b0e1052fe6aea5d28c";

// ------------------------------------------------------------------ refusals

#[test]
fn refusals_are_deterministic_and_leave_state_untouched() {
    let mut a = Author::new(replica(1));
    let s = st();
    let good = line(sk(70), sk(70), (1.0, 1.0), (2.0, 2.0), 2, true);
    let cases: Vec<(Op, C)> = vec![
        (
            Op::RenameNotebook {
                notebook_id: nb(99),
                title: "x".into(),
            },
            C::UnknownNotebook,
        ),
        (
            Op::AddStroke {
                notebook_id: nb(1),
                page_id: pg(99),
                stroke: good.clone(),
            },
            C::UnknownPage,
        ),
        (
            Op::AddStroke {
                notebook_id: nb(1),
                page_id: pg(3),
                stroke: good.clone(),
            },
            C::PageNotInNotebook,
        ),
        (
            Op::CreateNotebook {
                notebook_id: nb(1),
                title: "dup".into(),
                after_notebook_id: None,
                first_page: page_spec(pg(80)),
            },
            C::DuplicateNotebook,
        ),
        (
            Op::CreatePage {
                notebook_id: nb(1),
                page: page_spec(pg(2)),
                after_page_id: None,
            },
            C::DuplicatePage,
        ),
        (
            Op::AddStroke {
                notebook_id: nb(1),
                page_id: pg(1),
                stroke: line(sk(1), sk(1), (1.0, 1.0), (2.0, 2.0), 2, true),
            },
            C::DuplicateStroke,
        ),
        (
            Op::AddStroke {
                notebook_id: nb(1),
                page_id: pg(1),
                stroke: line(sk(71), sk(1), (1.0, 1.0), (2.0, 2.0), 2, true),
            },
            C::InvalidOperation, // invalid lineage: a new stroke must be its own root
        ),
        (
            Op::AddStroke {
                notebook_id: nb(1),
                page_id: pg(1),
                stroke: line(sk(72), sk(72), (f32::NAN, 1.0), (2.0, 2.0), 2, true),
            },
            C::InvalidOperation,
        ),
        (
            Op::AddStroke {
                notebook_id: nb(1),
                page_id: pg(1),
                stroke: {
                    let mut x = line(sk(73), sk(73), (1.0, 1.0), (2.0, 2.0), 2, true);
                    x.points[0].pressure = Some(1.5);
                    x
                },
            },
            C::InvalidOperation,
        ),
        (
            Op::AddStroke {
                notebook_id: nb(2),
                page_id: pg(3),
                stroke: line(sk(74), sk(74), (1.0, 1.0), (2.0, 2.0), 2, true),
            },
            C::InvalidResult, // pressure stroke on a page without a pressure curve
        ),
        (
            Op::DeletePage {
                notebook_id: nb(3),
                page_id: pg(4),
            },
            C::SolePage,
        ),
        (
            Op::MovePage {
                notebook_id: nb(1),
                page_id: pg(1),
                after_page_id: Some(pg(1)),
            },
            C::InvalidOperation, // self-placement
        ),
        (
            erase(vec![], vec![SyncPoint::new(1.0, 1.0, None)]),
            C::InvalidOperation,
        ),
        (
            Op::EraseRegion {
                notebook_id: nb(1),
                page_id: pg(1),
                observed_roots: vec![sk(1)],
                path: vec![SyncPoint::new(1.0, 1.0, None)],
                radius: -1.0,
            },
            C::InvalidOperation,
        ),
        (
            erase(vec![sk(555)], vec![SyncPoint::new(1.0, 1.0, None)]),
            C::UnknownLineage,
        ),
    ];
    for (op, want) in cases {
        let got = refuse(&s, &mut a, op.clone());
        assert_eq!(got, want, "{op:?}");
    }
    // Wrong library.
    let mut env = a.env(Op::DeleteNotebook { notebook_id: nb(3) });
    env.library_id = sync_protocol::LibraryId::from_uuid(testkit::uuid7(1, 2)).unwrap();
    assert_eq!(s.apply(&env).unwrap_err().code, C::WrongLibrary);
    assert_in_place_parity(&s, &env);
    // Blob backgrounds stay valid references through creation.
    let (bytes, blob) = testkit::blob_background();
    assert!(!bytes.is_empty());
    let mut spec = page_spec(pg(90));
    spec.background = blob;
    let (s2, _) = apply(
        &s,
        &mut a,
        Op::CreatePage {
            notebook_id: nb(3),
            page: spec,
            after_page_id: None,
        },
    );
    assert!(matches!(
        s2.snapshot.notebooks[2].pages[0].background,
        BackgroundRef::Blob { .. }
    ));
}

// ------------------------------------------------------------------ replay determinism

#[test]
fn replay_equals_stepwise_application_and_history_round_trips() {
    let mut a = Author::new(replica(1));
    let envs = vec![
        a.env(Op::AddStroke {
            notebook_id: nb(1),
            page_id: pg(2),
            stroke: line(sk(80), sk(80), (100.0, 100.0), (4000.0, 100.0), 7, true),
        }),
        a.env(erase(
            vec![sk(80)],
            vec![
                SyncPoint::new(2000.0, 0.0, None),
                SyncPoint::new(2000.0, 300.0, None),
            ],
        )),
    ];
    let mut step = st();
    for e in &envs {
        step = step.apply(e).unwrap().0;
    }
    let fresh = replay(genesis(), &envs).unwrap();
    assert_eq!(fresh, step);
    assert_eq!(
        fresh.snapshot.to_canonical_bytes().unwrap(),
        step.snapshot.to_canonical_bytes().unwrap()
    );
    let hb = fresh.history.to_canonical_bytes().unwrap();
    assert_eq!(sync_engine::HistoryV1::parse(&hb).unwrap(), fresh.history);
    let roots: BTreeSet<_> = fresh.history.known_roots.clone();
    assert!(roots.contains(&sk(80)));
}

// ------------------------------------------------------------------ folders

#[test]
fn folders_create_rename_file_delete_and_history() {
    let mut a = Author::new(replica(1));
    let s = st();
    // No folders: the canonical form is unchanged by the folder fields.
    let bytes_before = s.snapshot.to_canonical_bytes().unwrap();
    assert!(!String::from_utf8(bytes_before).unwrap().contains("folder"));
    assert!(!String::from_utf8(s.history.to_canonical_bytes().unwrap())
        .unwrap()
        .contains("folder"));

    let (s, o) = apply(
        &s,
        &mut a,
        Op::CreateFolder {
            folder_id: fd(1),
            title: "Journals".into(),
        },
    );
    assert_eq!(o, Outcome::Applied);
    let (s, _) = apply(
        &s,
        &mut a,
        Op::CreateFolder {
            folder_id: fd(2),
            title: "Work".into(),
        },
    );
    assert_eq!(
        s.snapshot
            .folders
            .iter()
            .map(|f| f.title.as_str())
            .collect::<Vec<_>>(),
        ["Journals", "Work"],
        "creation order"
    );
    let (s, _) = apply(
        &s,
        &mut a,
        Op::SetNotebookFolder {
            notebook_id: nb(1),
            folder_id: Some(fd(1)),
        },
    );
    let (s, _) = apply(
        &s,
        &mut a,
        Op::SetNotebookFolder {
            notebook_id: nb(2),
            folder_id: Some(fd(1)),
        },
    );
    let (s, _) = apply(
        &s,
        &mut a,
        Op::RenameFolder {
            folder_id: fd(1),
            title: "Diaries".into(),
        },
    );
    assert_eq!(s.snapshot.folders[0].title, "Diaries");
    // Change folder, then back to the top level.
    let (s, _) = apply(
        &s,
        &mut a,
        Op::SetNotebookFolder {
            notebook_id: nb(2),
            folder_id: Some(fd(2)),
        },
    );
    assert_eq!(s.snapshot.notebooks[1].folder_id, Some(fd(2)));
    let (s, _) = apply(
        &s,
        &mut a,
        Op::SetNotebookFolder {
            notebook_id: nb(2),
            folder_id: None,
        },
    );
    assert_eq!(s.snapshot.notebooks[1].folder_id, None);

    // Delete: members return to the top level; nothing else changes.
    let notebooks = s.snapshot.notebooks.len();
    let (s, _) = apply(&s, &mut a, Op::DeleteFolder { folder_id: fd(1) });
    assert_eq!(s.snapshot.notebooks.len(), notebooks);
    assert_eq!(s.snapshot.notebooks[0].folder_id, None);
    assert!(s.history.deleted_folders.contains(&fd(1)));

    // Deleted → NoOp; never-existed → refused; IDs are never reused.
    let (_, o) = apply(
        &s,
        &mut a,
        Op::RenameFolder {
            folder_id: fd(1),
            title: "x".into(),
        },
    );
    assert!(matches!(o, Outcome::NoOp(_)));
    let (_, o) = apply(
        &s,
        &mut a,
        Op::SetNotebookFolder {
            notebook_id: nb(1),
            folder_id: Some(fd(1)),
        },
    );
    assert!(matches!(o, Outcome::NoOp(_)));
    let (_, o) = apply(&s, &mut a, Op::DeleteFolder { folder_id: fd(1) });
    assert!(matches!(o, Outcome::NoOp(_)));
    assert_eq!(
        refuse(&s, &mut a, Op::DeleteFolder { folder_id: fd(9) }),
        C::UnknownFolder
    );
    assert_eq!(
        refuse(
            &s,
            &mut a,
            Op::SetNotebookFolder {
                notebook_id: nb(1),
                folder_id: Some(fd(9)),
            }
        ),
        C::UnknownFolder
    );
    assert_eq!(
        refuse(
            &s,
            &mut a,
            Op::CreateFolder {
                folder_id: fd(1),
                title: "again".into(),
            }
        ),
        C::DuplicateFolder
    );
    assert_eq!(
        refuse(
            &s,
            &mut a,
            Op::SetNotebookFolder {
                notebook_id: nb(99),
                folder_id: None,
            }
        ),
        C::UnknownNotebook
    );
    assert_eq!(
        refuse(
            &s,
            &mut a,
            Op::CreateFolder {
                folder_id: fd(5),
                title: "  ".into(),
            }
        ),
        C::InvalidOperation
    );

    // Deleting a filed notebook drops its membership with it.
    let (s, _) = apply(
        &s,
        &mut a,
        Op::SetNotebookFolder {
            notebook_id: nb(3),
            folder_id: Some(fd(2)),
        },
    );
    let (s, _) = apply(&s, &mut a, Op::DeleteNotebook { notebook_id: nb(3) });
    assert!(s.snapshot.notebooks.iter().all(|n| n.folder_id.is_none()));
    let (_, o) = apply(
        &s,
        &mut a,
        Op::SetNotebookFolder {
            notebook_id: nb(3),
            folder_id: Some(fd(2)),
        },
    );
    assert!(matches!(o, Outcome::NoOp(_)));

    // Canonical round trips with folders present.
    let bytes = s.snapshot.to_canonical_bytes().unwrap();
    assert_eq!(
        sync_protocol::SyncSnapshotV1::parse(&bytes).unwrap(),
        s.snapshot
    );
    let h = s.history.to_canonical_bytes().unwrap();
    assert_eq!(sync_engine::HistoryV1::parse(&h).unwrap(), s.history);
}

#[test]
fn folder_projection_matches_the_reducer() {
    let mut a = Author::new(replica(2));
    let s = st();
    let ops = vec![
        Op::CreateFolder {
            folder_id: fd(1),
            title: "A".into(),
        },
        Op::SetNotebookFolder {
            notebook_id: nb(1),
            folder_id: Some(fd(1)),
        },
        Op::CreateFolder {
            folder_id: fd(2),
            title: "B".into(),
        },
        Op::SetNotebookFolder {
            notebook_id: nb(2),
            folder_id: Some(fd(2)),
        },
        Op::RenameFolder {
            folder_id: fd(2),
            title: "B2".into(),
        },
        Op::DeleteFolder { folder_id: fd(1) },
    ];
    let mut reduced = s.clone();
    for op in &ops {
        reduced = reduced.apply(&a.env(op.clone())).unwrap().0;
    }
    let mut p = sync_engine::FolderProjection {
        folders: s.snapshot.folders.clone(),
        notebooks: s
            .snapshot
            .notebooks
            .iter()
            .map(|n| (n.notebook_id, n.folder_id))
            .collect(),
        deleted_folders: BTreeSet::new(),
    };
    let changed = p.project(&ops);
    assert!(changed.iter().all(|c| *c));
    assert_eq!(p.folders, reduced.snapshot.folders);
    assert_eq!(
        p.notebooks,
        reduced
            .snapshot
            .notebooks
            .iter()
            .map(|n| (n.notebook_id, n.folder_id))
            .collect::<Vec<_>>()
    );
    // A refused op is skipped by the projection (state untouched), never an error.
    let before = p.clone();
    assert_eq!(
        p.project(&[Op::RenameFolder {
            folder_id: fd(77),
            title: "x".into()
        }]),
        [false]
    );
    assert_eq!(p, before);
}

// ------------------------------------------------------------------ text

fn page_texts(s: &LibraryState) -> Vec<(sync_protocol::TextId, String)> {
    s.snapshot.notebooks[0].pages[0]
        .texts
        .iter()
        .map(|t| (t.text_id, t.content.clone()))
        .collect()
}

#[test]
fn text_add_update_delete_and_lww() {
    let mut a = Author::new(replica(1));
    let s = st();
    let (s, o) = apply(
        &s,
        &mut a,
        Op::AddText {
            notebook_id: nb(1),
            page_id: pg(1),
            text: text_obj(1, "The cat"),
        },
    );
    assert_eq!(o, Outcome::Applied);
    assert_eq!(page_texts(&s), vec![(tx(1), "The cat".into())]);

    let (s, o) = apply(
        &s,
        &mut a,
        Op::UpdateText {
            notebook_id: nb(1),
            page_id: pg(1),
            text: text_obj(1, "The dog"),
        },
    );
    assert_eq!(o, Outcome::Applied);
    assert_eq!(page_texts(&s), vec![(tx(1), "The dog".into())]);

    let mut later = text_obj(1, "The bird");
    later.x = 400.0;
    let (s, _) = apply(
        &s,
        &mut a,
        Op::UpdateText {
            notebook_id: nb(1),
            page_id: pg(1),
            text: later,
        },
    );
    assert_eq!(
        s.snapshot.notebooks[0].pages[0].texts[0].content,
        "The bird"
    );
    assert_eq!(s.snapshot.notebooks[0].pages[0].texts[0].x, 400.0);

    let (s, o) = apply(
        &s,
        &mut a,
        Op::DeleteText {
            notebook_id: nb(1),
            page_id: pg(1),
            text_id: tx(1),
        },
    );
    assert_eq!(o, Outcome::Applied);
    assert!(s.snapshot.notebooks[0].pages[0].texts.is_empty());
    assert!(s.history.deleted_texts.contains(&tx(1)));

    let (s, o) = apply(
        &s,
        &mut a,
        Op::DeleteText {
            notebook_id: nb(1),
            page_id: pg(1),
            text_id: tx(1),
        },
    );
    assert_eq!(o, Outcome::NoOp("text already gone"));
    assert_eq!(
        refuse(
            &s,
            &mut a,
            Op::UpdateText {
                notebook_id: nb(1),
                page_id: pg(1),
                text: text_obj(1, "after delete"),
            },
        ),
        C::UnknownText
    );
    assert_eq!(
        refuse(
            &s,
            &mut a,
            Op::AddText {
                notebook_id: nb(1),
                page_id: pg(1),
                text: text_obj(1, "reuse"),
            },
        ),
        C::DuplicateText
    );
}

#[test]
fn text_unknown_update_and_delete_refuse() {
    let mut a = Author::new(replica(1));
    let s = st();
    assert_eq!(
        refuse(
            &s,
            &mut a,
            Op::UpdateText {
                notebook_id: nb(1),
                page_id: pg(1),
                text: text_obj(9, "nope"),
            },
        ),
        C::UnknownText
    );
    assert_eq!(
        refuse(
            &s,
            &mut a,
            Op::DeleteText {
                notebook_id: nb(1),
                page_id: pg(1),
                text_id: tx(9),
            },
        ),
        C::UnknownText
    );
}

#[test]
fn text_survives_on_deleted_page_as_noop() {
    let mut a = Author::new(replica(1));
    let s = st();
    let (s, _) = apply(
        &s,
        &mut a,
        Op::AddText {
            notebook_id: nb(1),
            page_id: pg(1),
            text: text_obj(2, "keep"),
        },
    );
    let (s, _) = apply(
        &s,
        &mut a,
        Op::DeletePage {
            notebook_id: nb(1),
            page_id: pg(1),
        },
    );
    assert!(s.history.deleted_texts.contains(&tx(2)));
    let (s, o) = apply(
        &s,
        &mut a,
        Op::AddText {
            notebook_id: nb(1),
            page_id: pg(1),
            text: text_obj(3, "late"),
        },
    );
    assert_eq!(o, Outcome::NoOp("page or notebook deleted"));
    assert!(!s.history.known_texts.contains(&tx(3)));
}

// ------------------------------------------------------------------ page links

fn page_links(s: &LibraryState) -> Vec<sync_protocol::LinkId> {
    s.snapshot.notebooks[0].pages[0]
        .links
        .iter()
        .map(|l| l.link_id)
        .collect()
}

#[test]
fn page_link_add_delete_same_notebook_and_cross_notebook() {
    let mut a = Author::new(replica(1));
    let s = st();
    let same_nb = page_link(1, nb(1), pg(1), nb(1), pg(2));
    let (s, o) = apply(
        &s,
        &mut a,
        Op::AddPageLink {
            notebook_id: nb(1),
            page_id: pg(1),
            link: same_nb.clone(),
        },
    );
    assert_eq!(o, Outcome::Applied);
    assert_eq!(page_links(&s), vec![lk(1)]);

    let cross = page_link(2, nb(1), pg(1), nb(2), pg(3));
    let (s, o) = apply(
        &s,
        &mut a,
        Op::AddPageLink {
            notebook_id: nb(1),
            page_id: pg(1),
            link: cross,
        },
    );
    assert_eq!(o, Outcome::Applied);
    assert_eq!(page_links(&s).len(), 2);

    let (s, o) = apply(
        &s,
        &mut a,
        Op::DeletePageLink {
            notebook_id: nb(1),
            page_id: pg(1),
            link_id: lk(1),
        },
    );
    assert_eq!(o, Outcome::Applied);
    assert_eq!(page_links(&s), vec![lk(2)]);
    assert!(s.history.deleted_links.contains(&lk(1)));

    let (s, o) = apply(
        &s,
        &mut a,
        Op::DeletePageLink {
            notebook_id: nb(1),
            page_id: pg(1),
            link_id: lk(1),
        },
    );
    assert_eq!(o, Outcome::NoOp("link already gone"));
    let _ = s;
}

#[test]
fn page_link_self_target_is_invalid_operation() {
    let mut a = Author::new(replica(1));
    let s = st();
    let mut link = page_link(1, nb(1), pg(1), nb(1), pg(1));
    link.source_page_id = pg(1);
    link.target_page_id = pg(1);
    let code = refuse(
        &s,
        &mut a,
        Op::AddPageLink {
            notebook_id: nb(1),
            page_id: pg(1),
            link,
        },
    );
    assert_eq!(code, C::InvalidOperation);
}

#[test]
fn page_link_duplicate_id_refused_and_deleted_target_is_noop() {
    let mut a = Author::new(replica(1));
    let s = st();
    let link = page_link(1, nb(1), pg(1), nb(1), pg(2));
    let (s, _) = apply(
        &s,
        &mut a,
        Op::AddPageLink {
            notebook_id: nb(1),
            page_id: pg(1),
            link: link.clone(),
        },
    );
    let code = refuse(
        &s,
        &mut a,
        Op::AddPageLink {
            notebook_id: nb(1),
            page_id: pg(1),
            link,
        },
    );
    assert_eq!(code, C::DuplicateLink);

    let (s, _) = apply(&s, &mut a, Op::DeleteNotebook { notebook_id: nb(2) });
    // Existing link to a still-live page is unchanged; new link to the deleted page is a NoOp.
    assert_eq!(page_links(&s).len(), 1);
    let (s, o) = apply(
        &s,
        &mut a,
        Op::AddPageLink {
            notebook_id: nb(1),
            page_id: pg(1),
            link: page_link(9, nb(1), pg(1), nb(2), pg(3)),
        },
    );
    assert_eq!(o, Outcome::NoOp("target page or notebook deleted"));
    assert!(!s.history.known_links.contains(&lk(9)));
}

#[test]
fn page_link_survives_notebook_rename_and_source_delete_retires_id() {
    let mut a = Author::new(replica(1));
    let s = st();
    let (s, _) = apply(
        &s,
        &mut a,
        Op::AddPageLink {
            notebook_id: nb(1),
            page_id: pg(1),
            link: page_link(1, nb(1), pg(1), nb(2), pg(3)),
        },
    );
    let (s, _) = apply(
        &s,
        &mut a,
        Op::RenameNotebook {
            notebook_id: nb(2),
            title: "Projects".into(),
        },
    );
    assert_eq!(page_links(&s)[0], lk(1));
    assert_eq!(s.snapshot.notebooks[1].title, "Projects");
    let (s, _) = apply(
        &s,
        &mut a,
        Op::DeletePage {
            notebook_id: nb(1),
            page_id: pg(1),
        },
    );
    assert!(s.history.deleted_links.contains(&lk(1)));
}
