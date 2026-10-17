//! Sync S2A parity: the canonical reducer's `EraseRegion` evaluation and the Kindle's F7
//! eraser gesture produce bit-identical fragment geometry (one eraser, not two).

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use notebook_core::{PagePoint, Point, Stroke, StrokeId, ERASER_HIT_RADIUS_CANONICAL};
    use sync_engine::erase_page_strokes;
    use sync_protocol::{
        global_stroke_id, OperationId, PageId, StrokeStyle, SyncPoint, SyncStroke,
    };

    use crate::eraser::EraserGestureState;

    fn page() -> PageId {
        PageId::parse("019a8ef2-0000-7000-8000-00000000000a").unwrap()
    }

    fn local(id: &str, pts: &[(f32, f32, f32)]) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: pts
                .iter()
                .map(|(x, y, p)| Point::new(*x, *y).with_pressure(*p))
                .collect(),
            brush: Some("pressure-width-v1".into()),
            finished: true,
        }
    }

    fn to_sync(s: &Stroke) -> SyncStroke {
        let id = global_stroke_id(page(), s.id.as_str()).unwrap();
        SyncStroke {
            stroke_id: id,
            root_id: id,
            style: StrokeStyle::PressureWidthV1,
            points: s
                .points
                .iter()
                .map(|p| SyncPoint::new(p.x, p.y, p.pressure))
                .collect(),
        }
    }

    #[test]
    fn engine_erase_matches_kindle_gesture_bit_for_bit() {
        let strokes = vec![
            local(
                "s-1727000000000-1",
                &[
                    (1000.0, 3000.0, 0.2),
                    (4000.0, 3010.0, 0.6),
                    (9000.0, 2990.0, 0.9),
                ],
            ),
            local(
                "s-1727000000000-2",
                &[(2000.0, 1000.0, 0.3), (2100.0, 6000.0, 0.8)],
            ),
            local(
                "s-1727000000000-3",
                &[(12000.0, 12000.0, 0.5), (12500.0, 12600.0, 0.5)],
            ),
        ];
        // A wiggly multi-sample path that cuts strokes 1 and 2 more than once.
        let path: Vec<(f32, f32)> = vec![
            (1900.0, 2800.0),
            (2150.0, 3150.0),
            (2600.0, 3300.0),
            (5000.0, 2700.0),
            (5100.25, 3400.5),
            (2050.0, 4500.0),
        ];

        // Kindle gesture: Down, stationary first sample, then each Move (runtime order).
        let roots: HashMap<String, String> = strokes
            .iter()
            .map(|s| (s.id.as_str().to_string(), s.id.as_str().to_string()))
            .collect();
        let mut g = EraserGestureState::Idle;
        let p0 = PagePoint::new(path[0].0, path[0].1);
        assert!(g.on_down(p0, &strokes, &roots));
        g.process_to(p0);
        for (x, y) in &path[1..] {
            g.process_to(PagePoint::new(*x, *y));
        }
        let commit = g.on_up();

        // Engine.
        let sync: Vec<SyncStroke> = strokes.iter().map(to_sync).collect();
        let observed: BTreeSet<_> = sync.iter().map(|s| s.root_id).collect();
        let sync_path: Vec<SyncPoint> = path
            .iter()
            .map(|(x, y)| SyncPoint::new(*x, *y, None))
            .collect();
        let op = OperationId::parse("019a8ef2-0000-7000-8000-0000000000ff").unwrap();
        let res = erase_page_strokes(
            &sync,
            &observed,
            &sync_path,
            ERASER_HIT_RADIUS_CANONICAL,
            op,
        );

        // Same sources replaced, same fragment count and bit-identical geometry per source.
        let mut local_replaced: Vec<_> = commit
            .replacements
            .iter()
            .map(|r| global_stroke_id(page(), &r.target_stroke_id).unwrap())
            .collect();
        local_replaced.sort();
        let mut engine_replaced = res.replaced.clone();
        engine_replaced.sort();
        assert_eq!(engine_replaced, local_replaced);
        assert_eq!(
            engine_replaced.len(),
            2,
            "strokes 1 and 2 cut; stroke 3 untouched"
        );
        for rep in &commit.replacements {
            let root = global_stroke_id(page(), &rep.target_stroke_id).unwrap();
            let engine_frags: Vec<_> = res.strokes.iter().filter(|s| s.root_id == root).collect();
            assert_eq!(
                engine_frags.len(),
                rep.fragments.len(),
                "{}",
                rep.target_stroke_id
            );
            for (e, l) in engine_frags.iter().zip(&rep.fragments) {
                assert_eq!(e.points.len(), l.points.len());
                for (ep, lp) in e.points.iter().zip(&l.points) {
                    assert_eq!(ep.x.to_bits(), lp.x.to_bits());
                    assert_eq!(ep.y.to_bits(), lp.y.to_bits());
                    assert_eq!(ep.pressure.map(f32::to_bits), lp.pressure.map(f32::to_bits));
                }
            }
        }
        assert!(
            res.strokes.contains(&sync[2]),
            "unaffected stroke byte-identical"
        );
    }
}
