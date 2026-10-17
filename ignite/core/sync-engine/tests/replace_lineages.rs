//! Production reducer proofs for `ReplaceStrokeLineages` (Slice 18E.2).
//! Synthetic identities only. Mirrors the 18E.1 Review 80 shapes.

use sync_engine::testkit::{line, nb, pg, render, replica, sk, Author};
use sync_engine::{replay, LibraryState, Outcome, SemanticErrorCode as C};
use sync_protocol::{
    BackgroundRef, LineageReplacement, NotebookId, PageId, PageSpace, PageSpec, SnapshotNotebook,
    SnapshotPage, StrokeId, SyncEnvelopeV1, SyncOperationV1 as Op, SyncSnapshotV1, SyncStroke,
};

fn blank_page(id: PageId, strokes: Vec<SyncStroke>) -> SnapshotPage {
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

fn snapshot(notebooks: Vec<SnapshotNotebook>) -> SyncSnapshotV1 {
    SyncSnapshotV1 {
        v: 1,
        library_id: sync_engine::testkit::library(),
        page_space: PageSpace::CANONICAL,
        notebooks,
        folders: Vec::new(),
    }
}

fn page_spec_of(id: PageId) -> PageSpec {
    PageSpec {
        page_id: id,
        background: BackgroundRef::Blank,
        render: render(false),
    }
}

fn add(author: &mut Author, nb: NotebookId, page: PageId, stroke: SyncStroke) -> SyncEnvelopeV1 {
    author.env(Op::AddStroke {
        notebook_id: nb,
        page_id: page,
        stroke,
    })
}

fn create_page(
    author: &mut Author,
    nb: NotebookId,
    page: PageId,
    after: Option<PageId>,
) -> SyncEnvelopeV1 {
    author.env(Op::CreatePage {
        notebook_id: nb,
        page: page_spec_of(page),
        after_page_id: after,
    })
}

fn repl(root: StrokeId, expected: Vec<SyncStroke>, after: Vec<SyncStroke>) -> LineageReplacement {
    LineageReplacement {
        root_id: root,
        expected,
        after,
    }
}

fn sorted(mut v: Vec<LineageReplacement>) -> Vec<LineageReplacement> {
    v.sort_by_key(|r| r.root_id);
    v
}

fn replace_env(
    author: &mut Author,
    notebook: NotebookId,
    page: PageId,
    replacements: Vec<LineageReplacement>,
) -> SyncEnvelopeV1 {
    author.env(Op::ReplaceStrokeLineages {
        notebook_id: notebook,
        page_id: page,
        replacements: sorted(replacements),
    })
}

fn apply(s: &LibraryState, env: &SyncEnvelopeV1) -> (LibraryState, Outcome) {
    let before = s.clone();
    let mut in_place = s.clone();
    let r = s.apply(env).unwrap();
    let ip = in_place.apply_in_place(env).unwrap();
    assert_eq!(r.1, ip);
    assert_eq!(r.0, in_place);
    assert_eq!(*s, before);
    r
}

fn refuse(s: &LibraryState, env: &SyncEnvelopeV1) -> C {
    let before = s.clone();
    let e = s.apply(env).unwrap_err();
    assert_eq!(*s, before, "refused op must not mutate");
    let mut in_place = s.clone();
    let ip = in_place.apply_in_place(env).unwrap_err();
    assert_eq!(e, ip);
    e.code
}

#[test]
fn fixture_a_eight_originals_use_add_stroke() {
    let expected_strokes: Vec<_> = (1..=4).map(orig).collect();
    let extras: Vec<_> = (101..=108).map(orig).collect();
    let expected = LibraryState::genesis(snapshot(vec![notebook(
        nb(1),
        "FX-A",
        vec![blank_page(pg(1), expected_strokes.clone())],
    )]))
    .unwrap();
    let mut actual_strokes = expected_strokes;
    actual_strokes.extend(extras.iter().cloned());
    let actual = LibraryState::genesis(snapshot(vec![notebook(
        nb(1),
        "FX-A",
        vec![blank_page(pg(1), actual_strokes)],
    )]))
    .unwrap();
    let mut author = Author::new(replica(1));
    let mut recovered = expected;
    let mut envs = Vec::new();
    for s in extras {
        let env = add(&mut author, nb(1), pg(1), s);
        recovered = apply(&recovered, &env).0;
        envs.push(env);
    }
    assert_eq!(recovered.snapshot, actual.snapshot);
    let replayed = replay(
        snapshot(vec![notebook(
            nb(1),
            "FX-A",
            vec![blank_page(pg(1), (1..=4).map(orig).collect())],
        )]),
        &envs,
    )
    .unwrap();
    assert_eq!(replayed.snapshot, actual.snapshot);
}

#[test]
fn fixture_b_prefix_extras_and_one_fragment() {
    let prefix: Vec<_> = (1..=5).map(orig).collect();
    let extras: Vec<_> = (200..=295).map(orig).collect();
    let piece = frag(400, 296, 900.0);
    let expected = LibraryState::genesis(snapshot(vec![notebook(
        nb(2),
        "FX-B",
        vec![blank_page(pg(2), prefix.clone())],
    )]))
    .unwrap();
    let mut actual_strokes = prefix.clone();
    actual_strokes.extend(extras.iter().cloned());
    actual_strokes.push(piece.clone());
    let actual = LibraryState::genesis(snapshot(vec![notebook(
        nb(2),
        "FX-B",
        vec![blank_page(pg(2), actual_strokes)],
    )]))
    .unwrap();
    let mut author = Author::new(replica(1));
    let mut recovered = expected;
    for s in extras {
        recovered = apply(&recovered, &add(&mut author, nb(2), pg(2), s)).0;
    }
    let env = replace_env(
        &mut author,
        nb(2),
        pg(2),
        vec![repl(orig(296).root_id, vec![], vec![piece.clone()])],
    );
    recovered = apply(&recovered, &env).0;
    assert_eq!(recovered.snapshot, actual.snapshot);
    let kept = recovered.snapshot.notebooks[0].pages[0]
        .strokes
        .iter()
        .find(|s| s.root_id != s.stroke_id)
        .unwrap();
    assert_eq!(kept.stroke_id, piece.stroke_id);
    assert_eq!(kept.root_id, piece.root_id);
    let (again, out) = apply(&recovered, &env);
    assert_eq!(out, Outcome::NoOp("already applied"));
    assert_eq!(again, recovered);
}

#[test]
fn fixture_c_two_missing_originals_three_fragments_ten_extras() {
    let keep: Vec<_> = (1..=6).map(orig).collect();
    let gone1 = orig(7);
    let gone2 = orig(8);
    let tail: Vec<_> = (9..=12).map(orig).collect();
    let mut expected_strokes = keep.clone();
    expected_strokes.push(gone1.clone());
    expected_strokes.push(gone2.clone());
    expected_strokes.extend(tail.clone());
    let f1 = frag(501, 7, 1100.0);
    let f2 = frag(502, 7, 1200.0);
    let f3 = frag(503, 8, 1300.0);
    let extras: Vec<_> = (600..=609).map(orig).collect();
    let mut actual_strokes = keep;
    actual_strokes.push(f1.clone());
    actual_strokes.push(f2.clone());
    actual_strokes.push(f3.clone());
    actual_strokes.extend(tail);
    actual_strokes.extend(extras.iter().cloned());
    let expected = LibraryState::genesis(snapshot(vec![notebook(
        nb(3),
        "FX-C",
        vec![blank_page(pg(3), expected_strokes)],
    )]))
    .unwrap();
    let actual = LibraryState::genesis(snapshot(vec![notebook(
        nb(3),
        "FX-C",
        vec![blank_page(pg(3), actual_strokes)],
    )]))
    .unwrap();
    let mut author = Author::new(replica(1));
    let env = replace_env(
        &mut author,
        nb(3),
        pg(3),
        vec![
            repl(gone1.root_id, vec![gone1], vec![f1.clone(), f2.clone()]),
            repl(gone2.root_id, vec![gone2], vec![f3.clone()]),
        ],
    );
    let mut recovered = apply(&expected, &env).0;
    for s in extras {
        recovered = apply(&recovered, &add(&mut author, nb(3), pg(3), s)).0;
    }
    assert_eq!(recovered.snapshot, actual.snapshot);
    let ids: Vec<_> = recovered.snapshot.notebooks[0].pages[0]
        .strokes
        .iter()
        .filter(|s| s.root_id != s.stroke_id)
        .map(|s| s.stroke_id)
        .collect();
    assert_eq!(ids, vec![sk(501), sk(502), sk(503)]);
}

#[test]
fn fixture_d_two_new_pages() {
    let expected = LibraryState::genesis(snapshot(vec![notebook(
        nb(4),
        "FX-D",
        vec![blank_page(pg(40), vec![orig(1)])],
    )]))
    .unwrap();
    let actual = LibraryState::genesis(snapshot(vec![notebook(
        nb(4),
        "FX-D",
        vec![
            blank_page(pg(40), vec![orig(1)]),
            blank_page(pg(41), vec![]),
            blank_page(pg(42), vec![orig(701), orig(702)]),
        ],
    )]))
    .unwrap();
    let mut author = Author::new(replica(1));
    let mut recovered = expected;
    recovered = apply(
        &recovered,
        &create_page(&mut author, nb(4), pg(41), Some(pg(40))),
    )
    .0;
    recovered = apply(
        &recovered,
        &create_page(&mut author, nb(4), pg(42), Some(pg(41))),
    )
    .0;
    recovered = apply(&recovered, &add(&mut author, nb(4), pg(42), orig(701))).0;
    recovered = apply(&recovered, &add(&mut author, nb(4), pg(42), orig(702))).0;
    assert_eq!(recovered.snapshot, actual.snapshot);
}

#[test]
fn fixture_e_combined_review80_shape() {
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
    assert_eq!(p1_extra.len() + p2_extra.len() + p3_extra.len(), 114);

    let genesis_snap = snapshot(vec![
        notebook(nb(1), "E1", vec![blank_page(pg(1), p1_exp.clone())]),
        notebook(nb(2), "E2", vec![blank_page(pg(2), p2_exp.clone())]),
        notebook(
            nb(3),
            "E3",
            vec![blank_page(pg(3), {
                let mut v = p3_keep.clone();
                v.push(p3_g1.clone());
                v.push(p3_g2.clone());
                v
            })],
        ),
        notebook(nb(4), "E4", vec![blank_page(pg(4), vec![p4_home.clone()])]),
    ]);
    let expected = LibraryState::genesis(genesis_snap.clone()).unwrap();
    let mut a1 = p1_exp;
    a1.extend(p1_extra.iter().cloned());
    let mut a2 = p2_exp;
    a2.extend(p2_extra.iter().cloned());
    a2.push(p2_frag.clone());
    let mut a3 = p3_keep;
    a3.push(p3_f1.clone());
    a3.push(p3_f2.clone());
    a3.push(p3_f3.clone());
    a3.extend(p3_extra.iter().cloned());
    let actual = LibraryState::genesis(snapshot(vec![
        notebook(nb(1), "E1", vec![blank_page(pg(1), a1)]),
        notebook(nb(2), "E2", vec![blank_page(pg(2), a2)]),
        notebook(nb(3), "E3", vec![blank_page(pg(3), a3)]),
        notebook(
            nb(4),
            "E4",
            vec![
                blank_page(pg(4), vec![p4_home]),
                blank_page(pg(5), vec![]),
                blank_page(pg(6), vec![orig(41), orig(42)]),
            ],
        ),
    ]))
    .unwrap();

    let mut author = Author::new(replica(1));
    let mut recovered = expected;
    let mut envs = Vec::new();
    for s in p1_extra {
        let e = add(&mut author, nb(1), pg(1), s);
        recovered = apply(&recovered, &e).0;
        envs.push(e);
    }
    for s in p2_extra {
        let e = add(&mut author, nb(2), pg(2), s);
        recovered = apply(&recovered, &e).0;
        envs.push(e);
    }
    let e = replace_env(
        &mut author,
        nb(2),
        pg(2),
        vec![repl(sk(896), vec![], vec![p2_frag.clone()])],
    );
    recovered = apply(&recovered, &e).0;
    envs.push(e);
    let e = replace_env(
        &mut author,
        nb(3),
        pg(3),
        vec![
            repl(
                p3_g1.root_id,
                vec![p3_g1],
                vec![p3_f1.clone(), p3_f2.clone()],
            ),
            repl(p3_g2.root_id, vec![p3_g2], vec![p3_f3.clone()]),
        ],
    );
    recovered = apply(&recovered, &e).0;
    envs.push(e);
    for s in p3_extra {
        let env = add(&mut author, nb(3), pg(3), s);
        recovered = apply(&recovered, &env).0;
        envs.push(env);
    }
    for env in [
        create_page(&mut author, nb(4), pg(5), Some(pg(4))),
        create_page(&mut author, nb(4), pg(6), Some(pg(5))),
        add(&mut author, nb(4), pg(6), orig(41)),
        add(&mut author, nb(4), pg(6), orig(42)),
    ] {
        recovered = apply(&recovered, &env).0;
        envs.push(env);
    }
    assert_eq!(recovered.snapshot, actual.snapshot);
    let frags: Vec<_> = recovered
        .snapshot
        .notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .flat_map(|p| &p.strokes)
        .filter(|s| s.root_id != s.stroke_id)
        .map(|s| s.stroke_id)
        .collect();
    assert_eq!(frags.len(), 4);
    assert_eq!(frags, vec![sk(900), sk(910), sk(911), sk(912)]);
    let replayed = replay(genesis_snap, &envs).unwrap();
    assert_eq!(replayed.snapshot, actual.snapshot);
}

#[test]
fn unrelated_remote_stroke_is_preserved() {
    let r1 = orig(1);
    let r2 = orig(2);
    let expected = LibraryState::genesis(snapshot(vec![notebook(
        nb(1),
        "UX",
        vec![blank_page(pg(1), vec![r1.clone(), r2.clone()])],
    )]))
    .unwrap();
    let mut remote = Author::new(replica(2));
    let extra = orig(50);
    let expected = apply(&expected, &add(&mut remote, nb(1), pg(1), extra.clone())).0;
    let mut local = Author::new(replica(1));
    let env = replace_env(
        &mut local,
        nb(1),
        pg(1),
        vec![repl(r1.root_id, vec![r1], vec![frag(60, 1, 300.0)])],
    );
    let got = apply(&expected, &env).0;
    let ids: Vec<_> = got.snapshot.notebooks[0].pages[0]
        .strokes
        .iter()
        .map(|s| s.stroke_id)
        .collect();
    assert!(ids.contains(&extra.stroke_id));
    assert!(ids.contains(&r2.stroke_id));
}

#[test]
fn same_lineage_remote_change_conflicts_and_does_not_mutate() {
    let r1 = orig(1);
    let expected = LibraryState::genesis(snapshot(vec![notebook(
        nb(1),
        "CX",
        vec![blank_page(pg(1), vec![r1.clone()])],
    )]))
    .unwrap();
    let mut remote = Author::new(replica(2));
    let del = remote.env(Op::DeleteStrokeLineage {
        notebook_id: nb(1),
        page_id: pg(1),
        root_id: r1.root_id,
    });
    let expected = apply(&expected, &del).0;
    let mut local = Author::new(replica(1));
    let env = replace_env(
        &mut local,
        nb(1),
        pg(1),
        vec![repl(r1.root_id, vec![r1], vec![frag(60, 1, 300.0)])],
    );
    assert_eq!(refuse(&expected, &env), C::ConflictSameLineage);
}

#[test]
fn deleted_target_page_is_noop_so_history_can_replay() {
    // Production reducer: concurrent DeletePage wins as NoOp (EraseRegion rule).
    // Recovery planning must detect this before minting (18E.4); refusing here would
    // stick accepted-log replay if DeletePage is ordered first.
    let expected = LibraryState::genesis(snapshot(vec![notebook(
        nb(1),
        "DX",
        vec![
            blank_page(pg(1), vec![orig(1)]),
            blank_page(pg(2), vec![orig(2)]),
        ],
    )]))
    .unwrap();
    let mut remote = Author::new(replica(2));
    let del = remote.env(Op::DeletePage {
        notebook_id: nb(1),
        page_id: pg(1),
    });
    let expected = apply(&expected, &del).0;
    let mut local = Author::new(replica(1));
    let env = replace_env(
        &mut local,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![orig(1)], vec![frag(9, 1, 1.0)])],
    );
    let (got, out) = apply(&expected, &env);
    assert_eq!(out, Outcome::NoOp("page or notebook deleted"));
    assert_eq!(got.snapshot, expected.snapshot);
}

#[test]
fn identity_collision_with_unrelated_stroke_is_refused() {
    let a = orig(1);
    let b = orig(2);
    let expected = LibraryState::genesis(snapshot(vec![notebook(
        nb(1),
        "IX",
        vec![blank_page(pg(1), vec![a.clone(), b.clone()])],
    )]))
    .unwrap();
    let mut colliding = frag(3, 1, 10.0);
    colliding.stroke_id = b.stroke_id;
    colliding.root_id = a.root_id;
    let mut author = Author::new(replica(1));
    let env = replace_env(
        &mut author,
        nb(1),
        pg(1),
        vec![repl(a.root_id, vec![a], vec![colliding])],
    );
    assert_eq!(refuse(&expected, &env), C::DuplicateStroke);
}

#[test]
fn multi_root_conflict_is_atomic() {
    let a = orig(1);
    let b = orig(2);
    let expected = LibraryState::genesis(snapshot(vec![notebook(
        nb(1),
        "MX",
        vec![blank_page(pg(1), vec![a.clone(), b.clone()])],
    )]))
    .unwrap();
    let mut remote = Author::new(replica(2));
    let del = remote.env(Op::DeleteStrokeLineage {
        notebook_id: nb(1),
        page_id: pg(1),
        root_id: b.root_id,
    });
    let expected = apply(&expected, &del).0;
    let mut local = Author::new(replica(1));
    let env = replace_env(
        &mut local,
        nb(1),
        pg(1),
        vec![
            repl(a.root_id, vec![a.clone()], vec![frag(10, 1, 1.0)]),
            repl(b.root_id, vec![b], vec![frag(11, 2, 2.0)]),
        ],
    );
    assert_eq!(refuse(&expected, &env), C::ConflictSameLineage);
}

#[test]
fn empty_after_is_a_cas_lineage_clear() {
    let a = orig(1);
    let b = orig(2);
    let expected = LibraryState::genesis(snapshot(vec![notebook(
        nb(1),
        "EX",
        vec![blank_page(pg(1), vec![a.clone(), b.clone()])],
    )]))
    .unwrap();
    let mut author = Author::new(replica(1));
    let env = replace_env(
        &mut author,
        nb(1),
        pg(1),
        vec![repl(a.root_id, vec![a.clone()], vec![])],
    );
    let got = apply(&expected, &env).0;
    let ids: Vec<_> = got.snapshot.notebooks[0].pages[0]
        .strokes
        .iter()
        .map(|s| s.stroke_id)
        .collect();
    assert_eq!(ids, vec![b.stroke_id]);
    assert!(got.history.deleted_strokes.contains(&a.stroke_id));
    assert!(got.history.known_roots.contains(&a.root_id));
}

#[test]
fn different_operation_id_with_matching_after_is_noop_not_a_seq_bypass() {
    // Reducer NoOp is not authorization: a second envelope still needs its own
    // OperationId/client_seq at the ledger. Here we only prove state is unchanged.
    let a = orig(1);
    let expected = LibraryState::genesis(snapshot(vec![notebook(
        nb(1),
        "ID",
        vec![blank_page(pg(1), vec![a.clone()])],
    )]))
    .unwrap();
    let f = frag(8, 1, 40.0);
    let mut author = Author::new(replica(1));
    let first = replace_env(
        &mut author,
        nb(1),
        pg(1),
        vec![repl(a.root_id, vec![a.clone()], vec![f.clone()])],
    );
    let applied = apply(&expected, &first).0;
    let second = replace_env(
        &mut author,
        nb(1),
        pg(1),
        vec![repl(a.root_id, vec![a], vec![f])],
    );
    assert_ne!(first.operation_id, second.operation_id);
    let (again, out) = apply(&applied, &second);
    assert_eq!(out, Outcome::NoOp("already applied"));
    assert_eq!(again.snapshot, applied.snapshot);
}

#[test]
fn add_stroke_still_refuses_non_root() {
    let expected = LibraryState::genesis(snapshot(vec![notebook(
        nb(1),
        "NR",
        vec![blank_page(pg(1), vec![orig(1)])],
    )]))
    .unwrap();
    let mut author = Author::new(replica(1));
    let env = add(&mut author, nb(1), pg(1), frag(3, 1, 1.0));
    assert_eq!(refuse(&expected, &env), C::InvalidOperation);
}

#[test]
fn unknown_page_is_refused() {
    let expected = LibraryState::genesis(snapshot(vec![notebook(
        nb(1),
        "UP",
        vec![blank_page(pg(1), vec![orig(1)])],
    )]))
    .unwrap();
    let mut author = Author::new(replica(1));
    let env = replace_env(
        &mut author,
        nb(1),
        pg(99),
        vec![repl(sk(1), vec![orig(1)], vec![frag(2, 1, 1.0)])],
    );
    assert_eq!(refuse(&expected, &env), C::UnknownPage);
}
