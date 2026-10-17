//! Gate F2 navigation + switching proofs (host).

#[cfg(test)]
mod tests {
    use crate::nav::{classify_nav_down, NavDownAction, NavGestureState};
    use crate::orch::{FakeTransport, StrokeController};
    use crate::store::MemoryPageStore;
    use kindle_platform::ClockDomain;
    use notebook_core::{PagePoint, Point, Stroke, StrokeId, CANONICAL_WIDTH};

    fn pt(x: f32, y: f32) -> PagePoint {
        PagePoint::new(x, y)
    }

    #[test]
    fn navigation_down_never_begins_stroke() {
        let c = StrokeController::new("page-1", FakeTransport::new(), 25_000);
        let mut nav = NavGestureState::Idle;
        // Side + left: classify as Prev — do not call on_pen_down.
        let action = classify_nav_down(true, pt(10.0, 100.0)).unwrap();
        assert_eq!(action, NavDownAction::Prev);
        nav.on_down(action);
        assert!(c.is_idle());
        assert_eq!(c.session.strokes_started(), 0);
        assert!(!c.life().is_armed());
        nav.on_up();
        assert_eq!(c.session.strokes_started(), 0);
    }

    #[test]
    fn normal_edge_without_side_still_writes() {
        assert!(classify_nav_down(false, pt(10.0, 0.0)).is_none());
        let mut c = StrokeController::new("page-1", FakeTransport::new(), 25_000);
        c.on_pen_down(pt(10.0, 100.0), None).unwrap();
        assert_eq!(c.session.strokes_started(), 1);
        c.on_pen_up(pt(20.0, 100.0), None).unwrap();
    }

    #[test]
    fn side_center_is_undo_last_stroke() {
        assert_eq!(
            classify_nav_down(true, pt(CANONICAL_WIDTH / 2.0, 0.0)),
            Some(NavDownAction::UndoLastStroke)
        );
        // Without side button, center is ordinary handwriting (not classified as nav).
        assert!(classify_nav_down(false, pt(CANONICAL_WIDTH / 2.0, 0.0)).is_none());
    }

    #[test]
    fn side_guard_is_ignore_no_ink_not_undo() {
        use crate::nav::{nav_left_end_x, nav_left_guard_end_x};
        let x = (nav_left_end_x() + nav_left_guard_end_x()) * 0.5;
        assert_eq!(
            classify_nav_down(true, pt(x, 0.0)),
            Some(NavDownAction::IgnoreNoInk)
        );
    }

    #[test]
    fn stream_gap_clears_nav_without_inventing_switch() {
        let mut nav = NavGestureState::Idle;
        nav.on_down(NavDownAction::NextOrCreate);
        assert!(nav.is_active());
        nav.clear_on_stream_gap();
        assert!(!nav.is_active());
    }

    #[test]
    fn stream_gap_clears_undo_gesture_without_inventing_undo() {
        let mut nav = NavGestureState::Idle;
        nav.on_down(NavDownAction::UndoLastStroke);
        assert!(nav.is_active());
        nav.clear_on_stream_gap();
        assert!(!nav.is_active());
    }

    #[test]
    fn stream_gap_clears_ignore_no_ink() {
        let mut nav = NavGestureState::Idle;
        nav.on_down(NavDownAction::IgnoreNoInk);
        assert!(nav.is_active());
        nav.clear_on_stream_gap();
        assert!(!nav.is_active());
    }

    #[test]
    fn bind_isolates_pages_no_cross_leak() {
        let mut c = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            25_000,
            ClockDomain::LinuxMonotonic,
            MemoryPageStore::new(),
        );
        let a = Stroke {
            id: StrokeId::new("s-a"),
            points: vec![Point::new(1.0, 1.0)],
            brush: None,
            finished: true,
        };
        let b = Stroke {
            id: StrokeId::new("s-b"),
            points: vec![Point::new(2.0, 2.0)],
            brush: None,
            finished: true,
        };
        c.bind_page("page-1", vec![a.clone()], MemoryPageStore::new())
            .unwrap();
        c.bind_page("page-2", vec![b.clone()], MemoryPageStore::new())
            .unwrap();
        assert_eq!(c.session.committed_strokes()[0].id.as_str(), "s-b");
        c.bind_page("page-1", vec![a], MemoryPageStore::new())
            .unwrap();
        assert_eq!(c.session.committed_strokes()[0].id.as_str(), "s-a");
        assert_eq!(c.session.committed_count(), 1);
    }
}
