//! Gate F7 host tests: fragment render has a real gap; live vs restart reconstruction identity.

#[cfg(test)]
mod tests {
    use kindle_platform::FbGeometry;
    use notebook_core::{
        clip_stroke_against_eraser_sweep, PagePoint, Point, ScribeDisplayTransform, Stroke,
        StrokeId, ERASER_HIT_RADIUS_CANONICAL, PANEL_HEIGHT_PX, PANEL_WIDTH_PX,
    };

    use crate::profile::RenderProfile;
    use crate::render::raster_stroke_into;
    use crate::store::{
        apply_replace_strokes, PageStore, PersistedFragmentV2, PersistedPointV1,
        PersistedReplacementV2, VisibleModel,
    };

    fn panel_geo() -> FbGeometry {
        let w = PANEL_WIDTH_PX as u32;
        let h = PANEL_HEIGHT_PX as u32;
        FbGeometry {
            xres: w,
            yres: h,
            xres_virtual: w,
            yres_virtual: h,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: w,
            smem_len: w * h,
        }
    }

    fn stroke(id: &str, pts: &[(f32, f32)]) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: pts.iter().map(|(x, y)| Point::new(*x, *y)).collect(),
            brush: None,
            finished: true,
        }
    }

    fn to_frag(id: &str, s: &Stroke) -> PersistedFragmentV2 {
        PersistedFragmentV2 {
            id: id.into(),
            style: None,
            points: s
                .points
                .iter()
                .map(|p| PersistedPointV1 {
                    x: p.x,
                    y: p.y,
                    pressure: p.pressure,
                })
                .collect(),
        }
    }

    #[test]
    fn two_fragment_render_has_blank_gap() {
        let s = stroke("s-a", &[(1000.0, 5000.0), (5000.0, 5000.0)]);
        let frags = clip_stroke_against_eraser_sweep(
            &s,
            Point::new(3000.0, 4500.0),
            Point::new(3000.0, 5500.0),
            ERASER_HIT_RADIUS_CANONICAL,
        );
        assert_eq!(frags.len(), 2);

        let geo = panel_geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let mut full = vec![0xffu8; geo.smem_len as usize];
        raster_stroke_into(&mut full, &geo, &profile, &s).unwrap();

        let mut split = vec![0xffu8; geo.smem_len as usize];
        for f in &frags {
            raster_stroke_into(&mut split, &geo, &profile, f).unwrap();
        }

        let (mx, my) = ScribeDisplayTransform::PortraitIdentity
            .to_panel_pixel(PagePoint::new(3000.0, 5000.0))
            .unwrap();
        let idx = (my * geo.line_length + mx) as usize;
        assert_eq!(full[idx], 0x00, "original should ink the middle");
        assert_eq!(split[idx], 0xff, "gap must remain blank — no bridge");
    }

    #[test]
    fn replace_replay_reconstruction_matches_live_model() {
        let base = stroke("s-a", &[(0.0, 0.0), (2000.0, 0.0)]);
        let frags = clip_stroke_against_eraser_sweep(
            &base,
            Point::new(1000.0, -200.0),
            Point::new(1000.0, 200.0),
            ERASER_HIT_RADIUS_CANONICAL,
        );
        assert_eq!(frags.len(), 2);

        let mut live = VisibleModel::from_base_strokes(vec![base.clone()]);
        apply_replace_strokes(
            &mut live,
            vec![PersistedReplacementV2 {
                target_stroke_id: "s-a".into(),
                fragments: vec![to_frag("f1", &frags[0]), to_frag("f2", &frags[1])],
            }],
        )
        .unwrap();

        let mut restart = VisibleModel::from_base_strokes(vec![base]);
        apply_replace_strokes(
            &mut restart,
            vec![PersistedReplacementV2 {
                target_stroke_id: "s-a".into(),
                fragments: vec![to_frag("f1", &frags[0]), to_frag("f2", &frags[1])],
            }],
        )
        .unwrap();

        assert_eq!(live.strokes.len(), restart.strokes.len());
        for (a, b) in live.strokes.iter().zip(restart.strokes.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.points, b.points);
            assert_eq!(a.brush, b.brush);
        }

        let geo = panel_geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let mut buf_live = vec![0xffu8; geo.smem_len as usize];
        let mut buf_restart = vec![0xffu8; geo.smem_len as usize];
        for s in &live.strokes {
            raster_stroke_into(&mut buf_live, &geo, &profile, s).unwrap();
        }
        for s in &restart.strokes {
            raster_stroke_into(&mut buf_restart, &geo, &profile, s).unwrap();
        }
        assert_eq!(buf_live, buf_restart);
    }

    #[test]
    fn prepare_before_append_invalid_target_no_store_write() {
        use crate::orch::{FakeTransport, StrokeController};
        use crate::store::MemoryPageStore;
        use kindle_platform::ClockDomain;

        let a = stroke("s-a", &[(0.0, 0.0), (10.0, 0.0)]);
        let mut store = MemoryPageStore::new();
        store.append_committed_stroke(&a).unwrap();
        let mut ctrl = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        ctrl.session.load_committed_strokes(vec![a]).unwrap();
        let before = ctrl.session.committed_count();
        let err = ctrl
            .durable_replace_strokes(&[PersistedReplacementV2 {
                target_stroke_id: "missing".into(),
                fragments: vec![],
            }])
            .unwrap_err();
        let _ = err;
        assert_eq!(ctrl.session.committed_count(), before);
        assert_eq!(ctrl.store().replace_append_count, 0);
        assert_eq!(ctrl.totals.persistence_errors, 0);
    }

    #[test]
    fn prepare_before_append_collision_no_store_write() {
        use crate::orch::{FakeTransport, StrokeController};
        use crate::store::MemoryPageStore;
        use kindle_platform::ClockDomain;

        let a = stroke("s-a", &[(0.0, 0.0), (10.0, 0.0)]);
        let b = stroke("s-b", &[(0.0, 20.0), (10.0, 20.0)]);
        let mut store = MemoryPageStore::new();
        store.append_committed_stroke(&a).unwrap();
        store.append_committed_stroke(&b).unwrap();
        let mut ctrl = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        ctrl.session.load_committed_strokes(vec![a, b]).unwrap();
        assert!(ctrl
            .durable_replace_strokes(&[PersistedReplacementV2 {
                target_stroke_id: "s-a".into(),
                fragments: vec![to_frag("s-b", &stroke("s-b", &[(0.0, 0.0)]))],
            }])
            .is_err());
        assert_eq!(ctrl.store().replace_append_count, 0);
        assert_eq!(ctrl.session.committed_count(), 2);
    }

    #[test]
    fn valid_replace_one_append_installs_prepared() {
        use crate::orch::{FakeTransport, StrokeController};
        use crate::store::MemoryPageStore;
        use kindle_platform::ClockDomain;

        let a = stroke("s-a", &[(0.0, 0.0), (100.0, 0.0)]);
        let mut store = MemoryPageStore::new();
        store.append_committed_stroke(&a).unwrap();
        let mut ctrl = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            store,
        );
        ctrl.session.load_committed_strokes(vec![a]).unwrap();
        ctrl.durable_replace_strokes(&[PersistedReplacementV2 {
            target_stroke_id: "s-a".into(),
            fragments: vec![
                to_frag("f1", &stroke("f1", &[(0.0, 0.0), (40.0, 0.0)])),
                to_frag("f2", &stroke("f2", &[(60.0, 0.0), (100.0, 0.0)])),
            ],
        }])
        .unwrap();
        assert_eq!(ctrl.store().replace_append_count, 1);
        assert_eq!(ctrl.session.committed_count(), 2);
        assert_eq!(ctrl.session.committed_strokes()[0].id.as_str(), "f1");
        assert_eq!(ctrl.session.committed_strokes()[1].id.as_str(), "f2");
        assert_eq!(ctrl.session.root_id_of(&StrokeId::new("f1")), "s-a");
    }
}
