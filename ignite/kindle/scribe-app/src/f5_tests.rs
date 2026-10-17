//! Gate F7 host tests: durable partial-stroke eraser (supersedes F5 whole-stroke erase).

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use kindle_platform::ClockDomain;
    use notebook_core::{
        stroke_intersects_eraser_sweep, PagePoint, Point, Stroke, StrokeId,
        ERASER_HIT_RADIUS_CANONICAL,
    };

    use crate::eraser::EraserGestureState;
    use crate::orch::{FakeTransport, StrokeController, TransportStep};
    use crate::store::{MemoryPageStore, PageStore};

    /// Spacing well above 2× eraser radius so neighboring lines are independent.
    const GAP: f32 = 1000.0;

    fn stroke(id: &str, pts: &[(f32, f32)]) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: pts.iter().map(|(x, y)| Point::new(*x, *y)).collect(),
            brush: None,
            finished: true,
        }
    }

    fn vertical(id: &str, x: f32) -> Stroke {
        stroke(id, &[(x, 0.0), (x, 2000.0)])
    }

    fn horizontal(id: &str, y: f32) -> Stroke {
        stroke(id, &[(0.0, y), (2000.0, y)])
    }

    fn identity_roots(strokes: &[Stroke]) -> HashMap<String, String> {
        strokes
            .iter()
            .map(|s| {
                let id = s.id.as_str().to_string();
                (id.clone(), id)
            })
            .collect()
    }

    /// Provisional erase gesture → durable replace on Up (one reconstruct when dirty).
    fn run_partial_erase(
        c: &mut StrokeController<FakeTransport, MemoryPageStore>,
        g: &mut EraserGestureState,
        points: &[(f32, f32)],
    ) -> (usize, u64, u64) {
        assert!(!points.is_empty());
        let visible = c.session.committed_strokes().to_vec();
        let roots = c.session.root_of_map().clone();
        assert!(g.on_down(PagePoint::new(points[0].0, points[0].1), &visible, &roots));
        for &(x, y) in points {
            g.process_to(PagePoint::new(x, y));
        }
        let commit = g.on_up();
        let before_steps = c.transport().steps.len();
        let targets = commit.replacements.len();
        let frags: u64 = commit
            .replacements
            .iter()
            .map(|r| r.fragments.len() as u64)
            .sum();
        if commit.dirty {
            c.durable_replace_strokes(&commit.replacements).unwrap();
            assert_eq!(
                c.transport().steps.len(),
                before_steps,
                "eraser must never arm WetInk / DU"
            );
            assert!(!c.transport().steps.contains(&TransportStep::Arm));
        }
        let reconstructs = if commit.dirty { 1 } else { 0 };
        (targets, frags, reconstructs)
    }

    #[test]
    fn middle_erase_splits_one_stroke_two_fragments() {
        let mut store = MemoryPageStore::new();
        let a = horizontal("s-a", 1000.0);
        store.append_committed_stroke(&a).unwrap();
        let mut ctrl = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        ctrl.session.load_committed_strokes(vec![a]).unwrap();

        let mut g = EraserGestureState::Idle;
        // Vertical sweep through middle of horizontal stroke at y=1000.
        let (targets, frags, reconstructs) =
            run_partial_erase(&mut ctrl, &mut g, &[(1000.0, 500.0), (1000.0, 1500.0)]);
        assert_eq!(targets, 1);
        assert_eq!(frags, 2);
        assert_eq!(reconstructs, 1);
        assert_eq!(ctrl.session.committed_count(), 2);
        assert!(!ctrl.is_armed());
        assert!(ctrl.is_idle());
        // Lineage preserved.
        for s in ctrl.session.committed_strokes() {
            assert_eq!(ctrl.session.root_id_of(&s.id), "s-a");
        }
    }

    #[test]
    fn full_coverage_deletes_entire_stroke() {
        let mut store = MemoryPageStore::new();
        let a = vertical("s-a", 2.0 * GAP);
        store.append_committed_stroke(&a).unwrap();
        let mut ctrl = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        ctrl.session.load_committed_strokes(vec![a]).unwrap();
        let mut g = EraserGestureState::Idle;
        let (targets, frags, reconstructs) = run_partial_erase(
            &mut ctrl,
            &mut g,
            &[(2.0 * GAP - 10.0, 0.0), (2.0 * GAP + 10.0, 2000.0)],
        );
        assert_eq!(targets, 1);
        assert_eq!(frags, 0);
        assert_eq!(reconstructs, 1);
        assert_eq!(ctrl.session.committed_count(), 0);
    }

    #[test]
    fn blank_gesture_no_edits_no_reconstruct() {
        let mut store = MemoryPageStore::new();
        let a = vertical("s-a", 1.0 * GAP);
        store.append_committed_stroke(&a).unwrap();
        let mut ctrl = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        ctrl.session.load_committed_strokes(vec![a]).unwrap();
        let mut g = EraserGestureState::Idle;
        let (targets, _, reconstructs) = run_partial_erase(
            &mut ctrl,
            &mut g,
            &[(8000.0, 8000.0), (9000.0, 8000.0), (10000.0, 8000.0)],
        );
        assert_eq!(targets, 0);
        assert_eq!(reconstructs, 0);
        assert_eq!(ctrl.session.committed_count(), 1);
    }

    #[test]
    fn neighboring_strokes_untouched() {
        let mut store = MemoryPageStore::new();
        let a = vertical("s-a", 1.0 * GAP);
        let b = vertical("s-b", 2.0 * GAP);
        let c = vertical("s-c", 3.0 * GAP);
        for s in [&a, &b, &c] {
            store.append_committed_stroke(s).unwrap();
        }
        let mut ctrl = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        ctrl.session.load_committed_strokes(vec![a, b, c]).unwrap();
        let mut g = EraserGestureState::Idle;
        // Cross only B.
        let _ = run_partial_erase(
            &mut ctrl,
            &mut g,
            &[(2.0 * GAP - 10.0, 800.0), (2.0 * GAP + 10.0, 1200.0)],
        );
        let ids: Vec<_> = ctrl
            .session
            .committed_strokes()
            .iter()
            .map(|s| s.id.as_str().to_string())
            .collect();
        assert!(ids.iter().any(|id| id == "s-a"));
        assert!(ids.iter().any(|id| id == "s-c"));
        assert!(!ids.iter().any(|id| id == "s-b"));
        // B replaced by fragments (or fully deleted if sweep covered it).
        assert!(ctrl.session.committed_count() >= 2);
    }

    #[test]
    fn undo_after_split_removes_entire_lineage() {
        let mut store = MemoryPageStore::new();
        let strokes = vec![
            horizontal("s-a", 500.0),
            horizontal("s-b", 1500.0),
            horizontal("s-c", 2500.0),
        ];
        for s in &strokes {
            store.append_committed_stroke(s).unwrap();
        }
        let mut ctrl = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        ctrl.session.load_committed_strokes(strokes).unwrap();
        let mut g = EraserGestureState::Idle;
        let _ = run_partial_erase(&mut ctrl, &mut g, &[(1000.0, 2000.0), (1000.0, 3000.0)]);
        // C at y=2500 should be split into fragments.
        assert!(ctrl.session.committed_count() >= 4);
        ctrl.undo_last_visible_stroke().unwrap();
        let ids: Vec<_> = ctrl
            .session
            .committed_strokes()
            .iter()
            .map(|s| ctrl.session.root_id_of(&s.id))
            .collect();
        assert!(!ids.iter().any(|r| r == "s-c"));
        assert!(ids.iter().any(|r| r == "s-a"));
        assert!(ids.iter().any(|r| r == "s-b"));
    }

    #[test]
    fn render_slot_preserved_after_split() {
        let mut store = MemoryPageStore::new();
        let a = horizontal("s-a", 500.0);
        let b = horizontal("s-b", 1500.0);
        let c = horizontal("s-c", 2500.0);
        for s in [&a, &b, &c] {
            store.append_committed_stroke(s).unwrap();
        }
        let mut ctrl = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        ctrl.session.load_committed_strokes(vec![a, b, c]).unwrap();
        let mut g = EraserGestureState::Idle;
        let _ = run_partial_erase(&mut ctrl, &mut g, &[(1000.0, 1000.0), (1000.0, 2000.0)]);
        let roots: Vec<_> = ctrl
            .session
            .committed_strokes()
            .iter()
            .map(|s| ctrl.session.root_id_of(&s.id))
            .collect();
        // Order: A, B-frags..., C
        assert_eq!(roots.first().map(String::as_str), Some("s-a"));
        assert_eq!(roots.last().map(String::as_str), Some("s-c"));
        assert!(roots.iter().filter(|r| r.as_str() == "s-b").count() >= 1);
    }

    #[test]
    fn stream_gap_discards_provisional() {
        let strokes = vec![horizontal("s-a", 1000.0)];
        let roots = identity_roots(&strokes);
        let mut g = EraserGestureState::Idle;
        assert!(g.on_down(PagePoint::new(1000.0, 500.0), &strokes, &roots));
        g.process_to(PagePoint::new(1000.0, 1500.0));
        let _ = g.clear_on_stream_gap();
        assert!(!g.is_active());
    }

    #[test]
    fn sweep_geometry_stationary_and_miss() {
        let s = stroke("h", &[(0.0, 0.0), (2000.0, 0.0)]);
        assert!(stroke_intersects_eraser_sweep(
            &s,
            Point::new(1000.0, 0.0),
            Point::new(1000.0, 0.0),
            ERASER_HIT_RADIUS_CANONICAL,
        ));
        assert!(!stroke_intersects_eraser_sweep(
            &s,
            Point::new(0.0, ERASER_HIT_RADIUS_CANONICAL + 50.0),
            Point::new(2000.0, ERASER_HIT_RADIUS_CANONICAL + 50.0),
            ERASER_HIT_RADIUS_CANONICAL,
        ));
    }

    #[test]
    fn page_isolation_partial_erase() {
        let mut store2 = MemoryPageStore::new();
        let b = horizontal("s-b", 1500.0);
        store2.append_committed_stroke(&b).unwrap();
        let mut ctrl = StrokeController::with_store(
            "page-2",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store2,
        );
        ctrl.session.load_committed_strokes(vec![b]).unwrap();
        let mut g = EraserGestureState::Idle;
        let _ = run_partial_erase(&mut ctrl, &mut g, &[(1000.0, 1000.0), (1000.0, 2000.0)]);
        assert!(ctrl.session.committed_count() >= 1);

        let mut store1 = MemoryPageStore::new();
        let a = horizontal("s-a", 500.0);
        store1.append_committed_stroke(&a).unwrap();
        ctrl.bind_page("page-1", vec![a.clone()], store1).unwrap();
        assert_eq!(ctrl.session.committed_count(), 1);
        assert_eq!(ctrl.session.committed_strokes()[0].id.as_str(), "s-a");
    }
}
