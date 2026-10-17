//! Gate F3 host tests: erase sequencing, isolation, no WetInk on eraser.

#[cfg(test)]
mod tests {
    use kindle_platform::ClockDomain;
    use notebook_core::{
        hit_test_topmost_stroke, PagePoint, Point, Stroke, StrokeId, ERASER_HIT_RADIUS_CANONICAL,
    };

    use crate::orch::{FakeTransport, StrokeController, TransportStep};
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

    #[test]
    fn durable_delete_ordering_no_wet_ink() {
        let mut store = MemoryPageStore::new();
        let a = stroke("s-a", &[(0.0, 0.0), (100.0, 0.0)]);
        let b = stroke("s-b", &[(0.0, 50.0), (100.0, 50.0)]);
        store.append_committed_stroke(&a).unwrap();
        store.append_committed_stroke(&b).unwrap();
        let mut c = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            25_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        c.session
            .load_committed_strokes(vec![a.clone(), b.clone()])
            .unwrap();
        assert!(c.is_idle());
        assert!(!c.is_armed());
        let before_steps = c.transport().steps.len();
        c.durable_delete_stroke(&StrokeId::new("s-b")).unwrap();
        assert_eq!(c.session.committed_count(), 1);
        assert_eq!(c.session.committed_strokes()[0].id.as_str(), "s-a");
        assert_eq!(c.totals.strokes_deleted, 1);
        // No arm / DU / BeginStroke path.
        assert_eq!(c.transport().steps.len(), before_steps);
        assert!(!c.transport().steps.contains(&TransportStep::Arm));
        assert!(!c.is_armed());
        assert!(c.is_idle());
    }

    #[test]
    fn fail_delete_leaves_model_visible() {
        let mut store = MemoryPageStore::new();
        let a = stroke("s-a", &[(0.0, 0.0), (10.0, 10.0)]);
        store.append_committed_stroke(&a).unwrap();
        store.fail_delete = true;
        let mut c = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            25_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        c.session.load_committed_strokes(vec![a]).unwrap();
        assert!(c.durable_delete_stroke(&StrokeId::new("s-a")).is_err());
        assert_eq!(c.session.committed_count(), 1);
        assert_eq!(c.totals.delete_persistence_errors, 1);
        assert!(!c.is_armed());
    }

    #[test]
    fn fail_delete_fsync_leaves_model_visible() {
        let mut store = MemoryPageStore::new();
        let a = stroke("s-a", &[(0.0, 0.0), (10.0, 10.0)]);
        store.append_committed_stroke(&a).unwrap();
        store.fail_delete_fsync = true;
        let mut c = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            25_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        c.session.load_committed_strokes(vec![a]).unwrap();
        assert!(c.durable_delete_stroke(&StrokeId::new("s-a")).is_err());
        assert_eq!(c.session.committed_count(), 1);
    }

    #[test]
    fn page_isolation_delete_does_not_cross_pages() {
        let a = stroke("s-a", &[(0.0, 0.0), (1.0, 1.0)]);
        let b = stroke("s-b", &[(2.0, 2.0), (3.0, 3.0)]);
        let mut store2 = MemoryPageStore::new();
        store2.append_committed_stroke(&b).unwrap();
        let mut c = StrokeController::with_store(
            "page-2",
            FakeTransport::new(),
            25_000,
            ClockDomain::LinuxMonotonic,
            store2,
        );
        c.session.load_committed_strokes(vec![b.clone()]).unwrap();
        c.durable_delete_stroke(&StrokeId::new("s-b")).unwrap();
        assert_eq!(c.session.committed_count(), 0);

        let mut store1 = MemoryPageStore::new();
        store1.append_committed_stroke(&a).unwrap();
        c.bind_page("page-1", vec![a.clone()], store1).unwrap();
        assert_eq!(c.session.committed_count(), 1);
        assert_eq!(c.session.committed_strokes()[0].id.as_str(), "s-a");
    }

    #[test]
    fn session_delete_idle_only_preserves_order() {
        let mut s = PageSession::new("page-1");
        s.load_committed_strokes(vec![
            stroke("s1", &[(0.0, 0.0)]),
            stroke("s2", &[(1.0, 1.0)]),
            stroke("s3", &[(2.0, 2.0)]),
        ])
        .unwrap();
        s.delete_committed_stroke(&StrokeId::new("s2")).unwrap();
        assert_eq!(
            s.committed_strokes()
                .iter()
                .map(|x| x.id.as_str())
                .collect::<Vec<_>>(),
            vec!["s1", "s3"]
        );
        let _ = s.on_pen_down(PagePoint::new(0.0, 0.0), None);
        assert!(s.delete_committed_stroke(&StrokeId::new("s1")).is_err());
    }

    #[test]
    fn topmost_hit_after_delete_skips_tombstone() {
        let older = stroke("old", &[(0.0, 0.0), (1000.0, 0.0)]);
        let visible = vec![older];
        let id = hit_test_topmost_stroke(
            &visible,
            Point::new(500.0, 0.0),
            ERASER_HIT_RADIUS_CANONICAL,
        );
        assert_eq!(id.map(|x| x.as_str()), Some("old"));
    }
}
