//! Gate F4 host tests: last-visible-stroke undo (durable delete reuse, no WetInk).

#[cfg(test)]
mod tests {
    use kindle_platform::ClockDomain;
    use notebook_core::{PagePoint, Point, Stroke, StrokeId, CANONICAL_WIDTH};

    use crate::nav::{classify_nav_down, NavDownAction, NavGestureState};
    use crate::orch::{FakeTransport, StrokeController, TransportStep, UndoResult};
    use crate::session::PageSession;
    use crate::store::{MemoryPageStore, PageStore};

    fn stroke(id: &str, pts: &[(f32, f32)]) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: pts.iter().map(|(x, y)| Point::new(*x, *y)).collect(),
            brush: None,
            finished: true,
        }
    }

    fn ctrl_with(store: MemoryPageStore, strokes: Vec<Stroke>) -> StrokeController<FakeTransport> {
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

    fn assert_no_wet_ink(steps: &[TransportStep]) {
        assert!(!steps.contains(&TransportStep::Arm));
        assert!(!steps.contains(&TransportStep::DuSend));
        assert!(!steps.contains(&TransportStep::Wait));
        assert!(!steps.contains(&TransportStep::Disarm));
        assert!(!steps.contains(&TransportStep::Stamp));
        assert!(!steps.contains(&TransportStep::Segment));
    }

    #[test]
    fn nav_classification_side_center_undo_before_stroke_builder() {
        assert_eq!(
            classify_nav_down(true, PagePoint::new(10.0, 0.0)),
            Some(NavDownAction::Prev)
        );
        assert_eq!(
            classify_nav_down(true, PagePoint::new(CANONICAL_WIDTH - 10.0, 0.0)),
            Some(NavDownAction::NextOrCreate)
        );
        assert_eq!(
            classify_nav_down(true, PagePoint::new(CANONICAL_WIDTH / 2.0, 100.0)),
            Some(NavDownAction::UndoLastStroke)
        );
        assert!(classify_nav_down(false, PagePoint::new(CANONICAL_WIDTH / 2.0, 100.0)).is_none());
        // Classification alone never begins a stroke.
        let c = StrokeController::new("page-1", FakeTransport::new(), 8_000);
        let mut nav = NavGestureState::Idle;
        let act = classify_nav_down(true, PagePoint::new(CANONICAL_WIDTH / 2.0, 0.0)).unwrap();
        nav.on_down(act);
        assert_eq!(c.session.strokes_started(), 0);
        assert!(c.is_idle());
        assert!(!c.is_armed());
    }

    #[test]
    fn undo_no_wet_ink_sequencing() {
        let a = stroke("s-a", &[(0.0, 0.0), (10.0, 0.0)]);
        let b = stroke("s-b", &[(0.0, 50.0), (10.0, 50.0)]);
        let mut store = MemoryPageStore::new();
        store.append_committed_stroke(&a).unwrap();
        store.append_committed_stroke(&b).unwrap();
        let mut c = ctrl_with(store, vec![a, b]);
        let before = c.transport().steps.clone();
        match c.undo_last_visible_stroke().unwrap() {
            UndoResult::Deleted { stroke_id, .. } => {
                assert_eq!(stroke_id.as_str(), "s-b");
            }
            UndoResult::Empty => panic!("expected delete"),
        }
        assert_eq!(ids(&c), vec!["s-a"]);
        assert_eq!(c.totals.undo_hits, 1);
        assert_eq!(c.totals.eraser_hits, 0);
        assert_eq!(c.totals.eraser_gestures, 0);
        assert_eq!(c.totals.strokes_deleted, 1);
        assert_eq!(c.transport().steps, before);
        assert_no_wet_ink(&c.transport().steps);
        assert!(!c.is_armed());
        assert!(c.is_idle());
    }

    #[test]
    fn undo_empty_no_store_mutation() {
        let store = MemoryPageStore::new();
        let mut c = ctrl_with(store, vec![]);
        let deletes_before = c.store().deleted_ids.len();
        let steps_before = c.transport().steps.len();
        match c.undo_last_visible_stroke().unwrap() {
            UndoResult::Empty => {}
            UndoResult::Deleted { .. } => panic!("expected empty"),
        }
        assert_eq!(c.store().deleted_ids.len(), deletes_before);
        assert_eq!(c.session.committed_count(), 0);
        assert_eq!(c.totals.undo_empty, 1);
        assert_eq!(c.totals.undo_hits, 0);
        assert_eq!(c.totals.strokes_deleted, 0);
        assert_eq!(c.transport().steps.len(), steps_before);
        assert!(c.is_idle());
        assert!(c.store().is_writable());
    }

    #[test]
    fn repeated_undo_then_reload_empty() {
        let a = stroke("s-a", &[(0.0, 0.0), (1.0, 1.0)]);
        let b = stroke("s-b", &[(2.0, 2.0), (3.0, 3.0)]);
        let c0 = stroke("s-c", &[(4.0, 4.0), (5.0, 5.0)]);
        let mut store = MemoryPageStore::new();
        store.append_committed_stroke(&a).unwrap();
        store.append_committed_stroke(&b).unwrap();
        store.append_committed_stroke(&c0).unwrap();
        let mut c = ctrl_with(store, vec![a.clone(), b.clone(), c0.clone()]);

        assert!(matches!(
            c.undo_last_visible_stroke().unwrap(),
            UndoResult::Deleted { .. }
        ));
        assert_eq!(c.session.committed_count(), 2);
        assert!(matches!(
            c.undo_last_visible_stroke().unwrap(),
            UndoResult::Deleted { .. }
        ));
        assert_eq!(c.session.committed_count(), 1);
        assert!(matches!(
            c.undo_last_visible_stroke().unwrap(),
            UndoResult::Deleted { .. }
        ));
        assert_eq!(c.session.committed_count(), 0);
        assert!(matches!(
            c.undo_last_visible_stroke().unwrap(),
            UndoResult::Empty
        ));

        // Reload from store visible set.
        let visible = c.store().strokes.clone();
        let mut s = PageSession::new("page-1");
        s.load_committed_strokes(visible).unwrap();
        assert_eq!(s.committed_count(), 0);
    }

    #[test]
    fn eraser_then_undo_does_not_resurrect() {
        let a = stroke("s-a", &[(0.0, 0.0), (1.0, 1.0)]);
        let b = stroke("s-b", &[(2.0, 2.0), (3.0, 3.0)]);
        let c0 = stroke("s-c", &[(4.0, 4.0), (5.0, 5.0)]);
        let mut store = MemoryPageStore::new();
        store.append_committed_stroke(&a).unwrap();
        store.append_committed_stroke(&b).unwrap();
        store.append_committed_stroke(&c0).unwrap();
        let mut c = ctrl_with(store, vec![a.clone(), b.clone(), c0.clone()]);

        // F3 eraser deleted B.
        c.durable_delete_stroke(&StrokeId::new("s-b")).unwrap();
        assert_eq!(ids(&c), vec!["s-a", "s-c"]);
        assert_eq!(c.totals.eraser_hits, 0); // API path; runtime would bump eraser_hits

        // F4 undo removes newest visible (C), not resurrect B.
        match c.undo_last_visible_stroke().unwrap() {
            UndoResult::Deleted { stroke_id, .. } => assert_eq!(stroke_id.as_str(), "s-c"),
            UndoResult::Empty => panic!("expected C"),
        }
        assert_eq!(ids(&c), vec!["s-a"]);
        assert_eq!(c.totals.undo_hits, 1);

        let visible = c.store().strokes.clone();
        let mut s = PageSession::new("page-1");
        s.load_committed_strokes(visible).unwrap();
        assert_eq!(
            s.committed_strokes()
                .iter()
                .map(|x| x.id.as_str())
                .collect::<Vec<_>>(),
            vec!["s-a"]
        );
    }

    #[test]
    fn undo_write_undo_visible_order() {
        let a = stroke("s-a", &[(0.0, 0.0), (1.0, 1.0)]);
        let b = stroke("s-b", &[(2.0, 2.0), (3.0, 3.0)]);
        let c0 = stroke("s-c", &[(4.0, 4.0), (5.0, 5.0)]);
        let mut store = MemoryPageStore::new();
        store.append_committed_stroke(&a).unwrap();
        store.append_committed_stroke(&b).unwrap();
        store.append_committed_stroke(&c0).unwrap();
        let mut c = ctrl_with(store, vec![a, b, c0]);

        c.undo_last_visible_stroke().unwrap(); // delete C
        assert_eq!(ids(&c), vec!["s-a", "s-b"]);

        // Write D via live path.
        c.on_pen_down(PagePoint::new(10.0, 10.0), None).unwrap();
        let d_id = c
            .on_pen_up(PagePoint::new(20.0, 20.0), None)
            .unwrap()
            .as_str()
            .to_string();
        assert_eq!(c.session.committed_count(), 3);
        assert_eq!(c.session.last_committed_stroke_id().unwrap().as_str(), d_id);

        match c.undo_last_visible_stroke().unwrap() {
            UndoResult::Deleted { stroke_id, .. } => assert_eq!(stroke_id.as_str(), d_id),
            UndoResult::Empty => panic!("expected D"),
        }
        assert_eq!(ids(&c), vec!["s-a", "s-b"]);

        match c.undo_last_visible_stroke().unwrap() {
            UndoResult::Deleted { stroke_id, .. } => assert_eq!(stroke_id.as_str(), "s-b"),
            UndoResult::Empty => panic!("expected B"),
        }
        assert_eq!(ids(&c), vec!["s-a"]);

        let visible = c.store().strokes.clone();
        let mut s = PageSession::new("page-1");
        s.load_committed_strokes(visible).unwrap();
        assert_eq!(
            s.committed_strokes()
                .iter()
                .map(|x| x.id.as_str())
                .collect::<Vec<_>>(),
            vec!["s-a"]
        );
    }

    #[test]
    fn undo_is_page_local() {
        let a = stroke("s-a", &[(0.0, 0.0), (1.0, 1.0)]);
        let b = stroke("s-b", &[(2.0, 2.0), (3.0, 3.0)]);
        let c0 = stroke("s-c", &[(4.0, 4.0), (5.0, 5.0)]);
        let d = stroke("s-d", &[(6.0, 6.0), (7.0, 7.0)]);

        let mut store2 = MemoryPageStore::new();
        store2.append_committed_stroke(&c0).unwrap();
        store2.append_committed_stroke(&d).unwrap();
        let mut ctrl = StrokeController::with_store(
            "page-2",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store2,
        );
        ctrl.session
            .load_committed_strokes(vec![c0.clone(), d.clone()])
            .unwrap();
        ctrl.undo_last_visible_stroke().unwrap(); // delete D on page-2
        assert_eq!(
            ctrl.session
                .committed_strokes()
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["s-c"]
        );

        let mut store1 = MemoryPageStore::new();
        store1.append_committed_stroke(&a).unwrap();
        store1.append_committed_stroke(&b).unwrap();
        ctrl.bind_page("page-1", vec![a.clone(), b.clone()], store1)
            .unwrap();
        assert_eq!(
            ctrl.session
                .committed_strokes()
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["s-a", "s-b"]
        );

        // Simulate restart: page-1 unchanged, page-2 has C only.
        let mut s1 = PageSession::new("page-1");
        s1.load_committed_strokes(vec![a, b]).unwrap();
        assert_eq!(s1.committed_count(), 2);
        let mut s2 = PageSession::new("page-2");
        s2.load_committed_strokes(vec![c0]).unwrap();
        assert_eq!(s2.committed_count(), 1);
        assert_eq!(s2.committed_strokes()[0].id.as_str(), "s-c");
    }

    #[test]
    fn last_committed_stroke_id_follows_visible_order() {
        let mut s = PageSession::new("page-1");
        s.load_committed_strokes(vec![
            stroke("s-a", &[(0.0, 0.0)]),
            stroke("s-b", &[(1.0, 1.0)]),
            stroke("s-c", &[(2.0, 2.0)]),
        ])
        .unwrap();
        assert_eq!(s.last_committed_stroke_id().unwrap().as_str(), "s-c");
        s.delete_committed_stroke(&StrokeId::new("s-b")).unwrap();
        assert_eq!(s.last_committed_stroke_id().unwrap().as_str(), "s-c");
        s.delete_committed_stroke(&StrokeId::new("s-c")).unwrap();
        assert_eq!(s.last_committed_stroke_id().unwrap().as_str(), "s-a");
    }

    #[test]
    fn undo_persist_fail_leaves_visible_and_counts_undo_error() {
        let a = stroke("s-a", &[(0.0, 0.0), (1.0, 1.0)]);
        let mut store = MemoryPageStore::new();
        store.append_committed_stroke(&a).unwrap();
        store.fail_delete = true;
        let mut c = ctrl_with(store, vec![a]);
        assert!(c.undo_last_visible_stroke().is_err());
        assert_eq!(c.session.committed_count(), 1);
        assert_eq!(c.totals.undo_persistence_errors, 1);
        assert_eq!(c.totals.persistence_errors, 1);
        assert_eq!(c.totals.undo_hits, 0);
        assert!(!c.is_armed());
    }

    #[test]
    fn undo_refused_when_not_idle() {
        let a = stroke("s-a", &[(0.0, 0.0), (1.0, 1.0)]);
        let mut store = MemoryPageStore::new();
        store.append_committed_stroke(&a).unwrap();
        let mut c = ctrl_with(store, vec![a]);
        c.on_pen_down(PagePoint::new(0.0, 0.0), None).unwrap();
        assert!(c.undo_last_visible_stroke().is_err());
        assert_eq!(c.totals.undo_hits, 0);
        assert_eq!(c.totals.undo_empty, 0);
    }
}
