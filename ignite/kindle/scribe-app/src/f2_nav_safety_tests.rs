//! Nav safety regressions: 5-zone routing, guard no-ops, trailing pristine create guard.

#[cfg(test)]
mod tests {
    use kindle_platform::ClockDomain;
    use notebook_core::{PagePoint, Point, Stroke, StrokeId, CANONICAL_WIDTH};

    use crate::nav::{
        classify_nav_down, may_create_page_on_right_nav, nav_left_end_x, nav_left_guard_end_x,
        nav_right_guard_end_x, nav_undo_end_x, NavDownAction, NavGestureState,
    };
    use crate::orch::{FakeTransport, StrokeController, TransportStep, UndoResult};
    use crate::store::{
        MemoryPageStore, PageStore, PersistedFragmentV2, PersistedPointV1, PersistedReplacementV2,
    };

    fn pt(x: f32) -> PagePoint {
        PagePoint::new(x, 100.0)
    }

    fn stroke(id: &str, pts: &[(f32, f32)]) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: pts.iter().map(|(x, y)| Point::new(*x, *y)).collect(),
            brush: None,
            finished: true,
        }
    }

    fn ctrl_seeded(strokes: Vec<Stroke>) -> StrokeController<FakeTransport> {
        let mut store = MemoryPageStore::new();
        for s in &strokes {
            store.append_committed_stroke(s).unwrap();
        }
        let mut c = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        c.session.load_committed_strokes(strokes).unwrap();
        c
    }

    fn ids(c: &StrokeController<FakeTransport>) -> Vec<&str> {
        c.session
            .committed_strokes()
            .iter()
            .map(|s| s.id.as_str())
            .collect()
    }

    fn assert_no_wet(steps: &[TransportStep]) {
        assert!(!steps.contains(&TransportStep::Arm));
        assert!(!steps.contains(&TransportStep::DuSend));
        assert!(!steps.contains(&TransportStep::Disarm));
        assert!(!steps.contains(&TransportStep::Stamp));
        assert!(!steps.contains(&TransportStep::Segment));
    }

    #[test]
    fn observed_physical_coords_classify_as_nav() {
        assert_eq!(
            classify_nav_down(true, pt(1484.0)),
            Some(NavDownAction::Prev)
        );
        assert_eq!(
            classify_nav_down(true, pt(1872.0)),
            Some(NavDownAction::Prev)
        );
        assert_eq!(
            classify_nav_down(true, pt(13830.0)),
            Some(NavDownAction::NextOrCreate)
        );
        assert_eq!(
            classify_nav_down(true, pt(14034.0)),
            Some(NavDownAction::NextOrCreate)
        );
    }

    #[test]
    fn guard_bands_are_ignore_no_ink() {
        let left_guard = (nav_left_end_x() + nav_left_guard_end_x()) * 0.5;
        let right_guard = (nav_undo_end_x() + nav_right_guard_end_x()) * 0.5;
        assert_eq!(
            classify_nav_down(true, pt(left_guard)),
            Some(NavDownAction::IgnoreNoInk)
        );
        assert_eq!(
            classify_nav_down(true, pt(right_guard)),
            Some(NavDownAction::IgnoreNoInk)
        );
    }

    #[test]
    fn guard_tap_does_not_mutate_strokes_or_wet_ink() {
        let mut c = ctrl_seeded(vec![
            stroke("A", &[(0.0, 0.0), (10.0, 0.0)]),
            stroke("B", &[(0.0, 20.0), (10.0, 20.0)]),
            stroke("C", &[(0.0, 40.0), (10.0, 40.0)]),
        ]);
        let undo_hits_before = c.totals.undo_hits;
        let replace_before = c.store().replace_append_count;
        let before_ids: Vec<String> = ids(&c).iter().map(|s| (*s).to_string()).collect();
        c.transport_mut().steps.clear();

        let left_guard = (nav_left_end_x() + nav_left_guard_end_x()) * 0.5;
        let right_guard = (nav_undo_end_x() + nav_right_guard_end_x()) * 0.5;
        for x in [left_guard, right_guard] {
            let act = classify_nav_down(true, pt(x)).unwrap();
            assert_eq!(act, NavDownAction::IgnoreNoInk);
            let mut nav = NavGestureState::Idle;
            nav.on_down(act);
            // Runtime performs no undo/navigate/persist for IgnoreNoInk.
            nav.on_up();
        }

        assert_eq!(
            ids(&c),
            before_ids.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert_eq!(c.totals.undo_hits, undo_hits_before);
        assert_eq!(c.store().replace_append_count, replace_before);
        assert_eq!(c.session.strokes_started(), 0);
        assert!(c.is_idle());
        assert_no_wet(&c.transport().steps);
    }

    #[test]
    fn center_still_undoes_newest_lineage() {
        assert_eq!(
            classify_nav_down(true, pt(CANONICAL_WIDTH / 2.0)),
            Some(NavDownAction::UndoLastStroke)
        );
        let mut c = ctrl_seeded(vec![
            stroke("A", &[(0.0, 0.0), (10.0, 0.0)]),
            stroke("B", &[(0.0, 20.0), (10.0, 20.0)]),
            stroke("C", &[(0.0, 40.0), (10.0, 40.0)]),
        ]);
        c.transport_mut().steps.clear();
        match c.undo_last_visible_stroke().unwrap() {
            UndoResult::Deleted { stroke_id, .. } => {
                assert_eq!(stroke_id.as_str(), "C");
            }
            other => panic!("expected Deleted, got {other:?}"),
        }
        assert_eq!(ids(&c), vec!["A", "B"]);
        assert_no_wet(&c.transport().steps);
    }

    #[test]
    fn split_lineage_center_undo_removes_all_descendants() {
        let mut c = ctrl_seeded(vec![stroke(
            "C",
            &[
                (0.0, 0.0),
                (100.0, 0.0),
                (200.0, 0.0),
                (300.0, 0.0),
                (400.0, 0.0),
            ],
        )]);
        // Split C into three fragments sharing root C.
        let reps = vec![PersistedReplacementV2 {
            target_stroke_id: "C".into(),
            fragments: vec![
                PersistedFragmentV2 {
                    id: "C1".into(),
                    style: None,
                    points: vec![
                        PersistedPointV1 {
                            x: 0.0,
                            y: 0.0,
                            pressure: None,
                        },
                        PersistedPointV1 {
                            x: 50.0,
                            y: 0.0,
                            pressure: None,
                        },
                    ],
                },
                PersistedFragmentV2 {
                    id: "C2".into(),
                    style: None,
                    points: vec![
                        PersistedPointV1 {
                            x: 150.0,
                            y: 0.0,
                            pressure: None,
                        },
                        PersistedPointV1 {
                            x: 200.0,
                            y: 0.0,
                            pressure: None,
                        },
                    ],
                },
                PersistedFragmentV2 {
                    id: "C3".into(),
                    style: None,
                    points: vec![
                        PersistedPointV1 {
                            x: 300.0,
                            y: 0.0,
                            pressure: None,
                        },
                        PersistedPointV1 {
                            x: 400.0,
                            y: 0.0,
                            pressure: None,
                        },
                    ],
                },
            ],
        }];
        c.durable_replace_strokes(&reps).unwrap();
        assert_eq!(ids(&c), vec!["C1", "C2", "C3"]);
        assert_eq!(c.session.root_id_of(&StrokeId::new("C1")), "C");
        match c.undo_last_visible_stroke().unwrap() {
            UndoResult::Deleted { .. } => {}
            other => panic!("expected Deleted, got {other:?}"),
        }
        assert!(c.session.committed_strokes().is_empty());
    }

    #[test]
    fn trailing_pristine_page_refuses_create() {
        let pristine = MemoryPageStore::new();
        assert!(!pristine.has_authored_history());
        assert!(!may_create_page_on_right_nav(
            pristine.has_authored_history()
        ));
    }

    #[test]
    fn authored_final_page_allows_one_create_then_pristine_blocks() {
        let mut authored = MemoryPageStore::new();
        authored
            .append_committed_stroke(&stroke("s1", &[(1.0, 1.0), (2.0, 2.0)]))
            .unwrap();
        assert!(authored.has_authored_history());
        assert!(may_create_page_on_right_nav(
            authored.has_authored_history()
        ));

        // After create, the new page is pristine — further RightNav must not create.
        let trailing = MemoryPageStore::new();
        assert!(!may_create_page_on_right_nav(
            trailing.has_authored_history()
        ));
        assert!(!may_create_page_on_right_nav(
            trailing.has_authored_history()
        ));
        assert!(!may_create_page_on_right_nav(
            trailing.has_authored_history()
        ));
    }

    #[test]
    fn write_on_trailing_page_then_right_nav_may_create_again() {
        let mut trailing = MemoryPageStore::new();
        assert!(!may_create_page_on_right_nav(
            trailing.has_authored_history()
        ));
        trailing
            .append_committed_stroke(&stroke("s2", &[(3.0, 3.0), (4.0, 4.0)]))
            .unwrap();
        assert!(may_create_page_on_right_nav(
            trailing.has_authored_history()
        ));
    }

    #[test]
    fn authored_but_now_empty_still_has_history() {
        let mut c = ctrl_seeded(vec![stroke("A", &[(0.0, 0.0), (10.0, 0.0)])]);
        assert!(c.store().has_authored_history());
        match c.undo_last_visible_stroke().unwrap() {
            UndoResult::Deleted { .. } => {}
            other => panic!("expected Deleted, got {other:?}"),
        }
        assert!(c.session.committed_strokes().is_empty());
        assert!(c.store().has_authored_history());
        assert!(may_create_page_on_right_nav(
            c.store().has_authored_history()
        ));
    }

    #[test]
    fn stream_gap_clears_ignore_no_ink_without_second_fire() {
        let mut nav = NavGestureState::Idle;
        nav.on_down(NavDownAction::IgnoreNoInk);
        assert!(nav.is_active());
        nav.clear_on_stream_gap();
        assert!(!nav.is_active());
    }
}
