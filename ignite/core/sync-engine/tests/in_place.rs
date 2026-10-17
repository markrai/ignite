//! `LibraryState::apply_in_place` ≡ `LibraryState::apply`: every envelope of long random
//! sequences (all operation kinds, valid and refused) gives the same state + outcome or the
//! identical error, and a chain advanced only in place ends bit-identical to the clone path.

use sync_engine::testkit::{fd, genesis, line, nb, page_spec, pg, replica, sk, Author};
use sync_engine::LibraryState;
use sync_protocol::{StrokeId, SyncOperationV1 as Op, SyncPoint};

/// Deterministic xorshift (no RNG dependency; reproducible failures).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
    fn coord(&mut self) -> f32 {
        (self.next() % 15_000) as f32 + 300.0
    }
}

fn random_op(r: &mut Rng, s: &LibraryState, fresh: &mut u64) -> Op {
    let nbs = &s.snapshot.notebooks;
    let n = &nbs[r.below(nbs.len())];
    let p = &n.pages[r.below(n.pages.len())];
    let mut new_id = || {
        *fresh += 1;
        *fresh
    };
    // Occasionally aim at something unknown or already used.
    let nb_id = if r.chance(5) {
        nb(900 + r.below(3) as u64)
    } else {
        n.notebook_id
    };
    let pg_id = if r.chance(5) {
        pg(900 + r.below(3) as u64)
    } else {
        p.page_id
    };
    let existing_stroke: Option<StrokeId> =
        (!p.strokes.is_empty()).then(|| p.strokes[r.below(p.strokes.len())].stroke_id);
    // Folders: a live one, a fresh one, or (rarely) one that never existed.
    let folder = |r: &mut Rng, fresh: u64| {
        let live = &s.snapshot.folders;
        if !live.is_empty() && r.chance(60) {
            live[r.below(live.len())].folder_id
        } else if r.chance(10) {
            fd(990)
        } else {
            fd(1 + fresh % 6)
        }
    };
    if r.chance(15) {
        let salt = r.next();
        let f = folder(r, salt);
        return match r.below(4) {
            0 => Op::CreateFolder {
                folder_id: if r.chance(80) { fd(new_id()) } else { f },
                title: if r.chance(5) {
                    " ".into()
                } else {
                    format!("F{}", r.below(9))
                },
            },
            1 => Op::RenameFolder {
                folder_id: f,
                title: format!("R{}", r.below(9)),
            },
            2 => Op::DeleteFolder { folder_id: f },
            _ => Op::SetNotebookFolder {
                notebook_id: nb_id,
                folder_id: r.chance(75).then_some(f),
            },
        };
    }
    match r.below(10) {
        0..=3 => {
            let id = match existing_stroke {
                Some(e) if r.chance(5) => e,
                _ => sk(new_id()),
            };
            let (x, y) = (r.coord(), r.coord());
            Op::AddStroke {
                notebook_id: nb_id,
                page_id: pg_id,
                stroke: line(
                    id,
                    id,
                    (x, y),
                    (x + 3000.0, y),
                    2 + r.below(20),
                    r.chance(70),
                ),
            }
        }
        4 => {
            let mut roots: Vec<StrokeId> = p.strokes.iter().map(|s| s.root_id).collect();
            roots.sort();
            roots.dedup();
            if roots.is_empty() || r.chance(5) {
                roots = vec![sk(777_777)];
            } else {
                let keep = 1 + r.below(roots.len());
                roots.truncate(keep);
            }
            let x = r.coord();
            Op::EraseRegion {
                notebook_id: nb_id,
                page_id: pg_id,
                observed_roots: roots,
                path: vec![
                    SyncPoint::new(x, 0.0, None),
                    SyncPoint::new(x, 20_000.0, None),
                ],
                radius: 240.0,
            }
        }
        5 => Op::DeleteStrokeLineage {
            notebook_id: nb_id,
            page_id: pg_id,
            root_id: p
                .strokes
                .get(r.below(p.strokes.len().max(1)))
                .map_or(sk(888_888), |s| s.root_id),
        },
        6 => Op::CreatePage {
            notebook_id: nb_id,
            page: page_spec(if r.chance(5) { p.page_id } else { pg(new_id()) }),
            after_page_id: match r.below(3) {
                0 => None,
                1 => Some(p.page_id),
                _ => Some(pg(950)),
            },
        },
        7 => {
            if r.chance(50) {
                Op::DeletePage {
                    notebook_id: nb_id,
                    page_id: pg_id,
                }
            } else {
                Op::MovePage {
                    notebook_id: nb_id,
                    page_id: pg_id,
                    after_page_id: n.pages.get(r.below(n.pages.len())).map(|q| q.page_id),
                }
            }
        }
        8 => {
            if r.chance(50) {
                Op::CreateNotebook {
                    notebook_id: if r.chance(5) {
                        n.notebook_id
                    } else {
                        nb(new_id())
                    },
                    title: format!("N{}", r.below(99)),
                    after_notebook_id: Some(n.notebook_id),
                    first_page: page_spec(pg(new_id())),
                }
            } else {
                Op::RenameNotebook {
                    notebook_id: nb_id,
                    title: if r.chance(10) {
                        String::new()
                    } else {
                        format!("T{}", r.below(9))
                    },
                }
            }
        }
        _ => Op::DeleteNotebook { notebook_id: nb_id },
    }
}

#[test]
fn in_place_matches_clone_path_for_random_sequences() {
    for seed in 1..=6u64 {
        let mut r = Rng(0x9E37_79B9_7F4A_7C15 ^ seed);
        let mut a = Author::new(replica(seed));
        let mut clone_path = LibraryState::genesis(genesis()).unwrap();
        let mut chain = clone_path.clone(); // advanced only in place
        let mut fresh = 10_000 * seed;
        let (mut ok, mut refused) = (0, 0);
        for step in 0..1500 {
            let env = a.env(random_op(&mut r, &clone_path, &mut fresh));
            let mut probe = clone_path.clone();
            match (clone_path.apply(&env), probe.apply_in_place(&env)) {
                (Ok((next, o1)), Ok(o2)) => {
                    assert_eq!(o1, o2, "seed {seed} step {step}");
                    assert_eq!(next, probe, "seed {seed} step {step}: {env:?}");
                    assert_eq!(chain.apply_in_place(&env).unwrap(), o1);
                    clone_path = next;
                    ok += 1;
                }
                (Err(e1), Err(e2)) => {
                    assert_eq!(e1, e2, "seed {seed} step {step}: {env:?}");
                    refused += 1;
                }
                (x, y) => panic!("seed {seed} step {step}: apply {x:?} vs in place {y:?}"),
            }
        }
        assert_eq!(chain, clone_path, "seed {seed}: accumulated in-place chain");
        assert_eq!(
            chain.snapshot.to_canonical_bytes().unwrap(),
            clone_path.snapshot.to_canonical_bytes().unwrap()
        );
        assert!(
            ok > 500 && refused > 50,
            "seed {seed}: ok={ok} refused={refused}"
        );
    }
}
