//! Gate F6 persistence-contract regressions (fail-closed durable semantics).
//!
//! Does not retune pressure thresholds/curve/radii or change valid-stroke raster bytes.

#[cfg(test)]
mod tests {
    use notebook_core::{Point, Stroke, StrokeId};

    use crate::pressure_curve::{STYLE_LEGACY_FIXED_V1, STYLE_PRESSURE_WIDTH_V1};
    use crate::profile::{
        validate_persisted_render_profile_v1, validate_persisted_render_profile_v2, RenderProfile,
        LEGACY_FIXED_RADIUS, RASTERIZER_ID_V1, RASTERIZER_ID_V2_PRESSURE,
    };
    use crate::render::{radius_for_pressure, raster_stroke_into};
    use crate::store::{
        parse_header_render_for_test, parse_stroke_journal, stroke_record_line,
        validate_persisted_pressure_v2, PersistedStrokeV1, PersistedStrokeV2,
    };
    use kindle_platform::FbGeometry;

    fn tiny_geo() -> FbGeometry {
        FbGeometry {
            xres: 64,
            yres: 64,
            xres_virtual: 64,
            yres_virtual: 64,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 64,
            smem_len: 64 * 64,
        }
    }

    fn pressure_pts(pressures: &[Option<f32>]) -> Stroke {
        Stroke {
            id: StrokeId::new("s-p"),
            points: pressures
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let mut pt = Point::new(i as f32 * 10.0, 0.0);
                    if let Some(v) = *p {
                        pt = pt.with_pressure(v);
                    }
                    pt
                })
                .collect(),
            brush: Some(STYLE_PRESSURE_WIDTH_V1.into()),
            finished: true,
        }
    }

    fn legacy_pts() -> Stroke {
        Stroke {
            id: StrokeId::new("s-l"),
            points: vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0)],
            brush: None,
            finished: true,
        }
    }

    #[test]
    fn unknown_brush_persistence_refuses() {
        for bad in ["future-brush", "", "pressure-width-v2", "typo"] {
            let mut s = legacy_pts();
            s.brush = Some(bad.into());
            let err = stroke_record_line(&s).unwrap_err();
            assert!(
                err.contains("unknown") || err.contains("REFUSE"),
                "brush={bad:?} err={err}"
            );
        }
    }

    #[test]
    fn explicit_legacy_fixed_serializes_as_v1() {
        let mut s = legacy_pts();
        s.brush = Some(STYLE_LEGACY_FIXED_V1.into());
        let line = stroke_record_line(&s).unwrap();
        assert!(line.contains("\"v\":1"), "{line}");
        assert!(!line.contains("style"), "{line}");
        let (loaded, _) = parse_stroke_journal(line.as_bytes()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].brush.is_none());
    }

    #[test]
    fn pressure_width_serializes_as_v2() {
        let s = pressure_pts(&[Some(0.5)]);
        let line = stroke_record_line(&s).unwrap();
        assert!(line.contains("\"v\":2"), "{line}");
        assert!(line.contains(STYLE_PRESSURE_WIDTH_V1), "{line}");
    }

    #[test]
    fn v2_pressure_out_of_range_and_non_finite_refuse() {
        for bad in [-0.01f32, 1.01, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(
                validate_persisted_pressure_v2(Some(bad)).is_err(),
                "expected refuse for {bad:?}"
            );
            // Bypass Point::with_pressure (live clamp) — durable path must still refuse.
            let s = Stroke {
                id: StrokeId::new("s-bad"),
                points: vec![Point {
                    x: 0.0,
                    y: 0.0,
                    pressure: Some(bad),
                    t_us: None,
                }],
                brush: Some(STYLE_PRESSURE_WIDTH_V1.into()),
                finished: true,
            };
            assert!(PersistedStrokeV2::from_stroke(&s).is_err(), "from {bad:?}");
            let rec = PersistedStrokeV2 {
                v: 2,
                record_type: "stroke".into(),
                id: "s-bad".into(),
                finished: true,
                style: STYLE_PRESSURE_WIDTH_V1.into(),
                points: vec![crate::store::PersistedPointV1 {
                    x: 0.0,
                    y: 0.0,
                    pressure: Some(bad),
                }],
            };
            assert!(rec.to_stroke().is_err(), "to_stroke {bad:?}");
        }
    }

    #[test]
    fn none_pressure_persists_reloads_and_renders_fallback_r2() {
        assert!(validate_persisted_pressure_v2(None).is_ok());
        let s = pressure_pts(&[None, None]);
        let line = stroke_record_line(&s).unwrap();
        let (loaded, trunc) = parse_stroke_journal(line.as_bytes()).unwrap();
        assert!(!trunc);
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].points.iter().all(|p| p.pressure.is_none()));

        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let curve = profile.pressure_curve().unwrap();
        assert_eq!(radius_for_pressure(curve, None), LEGACY_FIXED_RADIUS);

        let g = tiny_geo();
        let mut a = vec![0xAAu8; g.smem_len as usize];
        raster_stroke_into(&mut a, &g, &profile, &loaded[0]).unwrap();
        // Same geometry with explicit pressure that maps to r2 (~0.5 → q≈2048 → r2).
        let mut explicit = loaded[0].clone();
        for p in &mut explicit.points {
            *p = Point::new(p.x, p.y).with_pressure(0.5);
        }
        let mut b = vec![0xAAu8; g.smem_len as usize];
        raster_stroke_into(&mut b, &g, &profile, &explicit).unwrap();
        assert_eq!(
            a, b,
            "None pressure must match sealed fallback r2 rendering"
        );
    }

    #[test]
    fn pressure_stroke_plus_v1_profile_refuses() {
        let g = tiny_geo();
        let v1 = RenderProfile::sealed_v1(0x00).unwrap();
        let s = pressure_pts(&[Some(0.8)]);
        let err = raster_stroke_into(&mut vec![0u8; g.smem_len as usize], &g, &v1, &s).unwrap_err();
        assert!(err.contains("pressure-capable") || err.contains("REFUSE RENDER"));
    }

    #[test]
    fn legacy_stroke_works_under_v1_and_v2_profiles() {
        let g = tiny_geo();
        let legacy = legacy_pts();
        let v1 = RenderProfile::sealed_v1(0x00).unwrap();
        let v2 = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let mut a = vec![0xAAu8; g.smem_len as usize];
        raster_stroke_into(&mut a, &g, &v1, &legacy).unwrap();
        let mut b = vec![0xAAu8; g.smem_len as usize];
        raster_stroke_into(&mut b, &g, &v2, &legacy).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn profile_version_dispatch_is_deterministic() {
        let v1 = RenderProfile::sealed_v1(0x00).unwrap();
        let v2 = RenderProfile::sealed_v2_pressure(0xff).unwrap();
        let v1j = serde_json::to_value(v1.to_persisted_v1().unwrap()).unwrap();
        let v2j = serde_json::to_value(v2.to_persisted_v2().unwrap()).unwrap();
        assert_eq!(
            parse_header_render_for_test(&v1j).unwrap().rasterizer_id,
            RASTERIZER_ID_V1
        );
        assert_eq!(
            parse_header_render_for_test(&v2j).unwrap().rasterizer_id,
            RASTERIZER_ID_V2_PRESSURE
        );

        // Invalid v2 thresholds → v2 error (never masquerades as v1).
        let mut bad_thresh = v2.to_persisted_v2().unwrap();
        bad_thresh.pressure_q_r1_max = 1;
        let bad_j = serde_json::to_value(&bad_thresh).unwrap();
        let err = parse_header_render_for_test(&bad_j).unwrap_err();
        assert!(
            err.contains("pressure curve") || err.contains("thresholds") || err.contains("v2"),
            "{err}"
        );

        // Invalid v2 rasterizer → v2 error.
        let mut bad_rast = v2.to_persisted_v2().unwrap();
        bad_rast.rasterizer = "nope".into();
        let err =
            parse_header_render_for_test(&serde_json::to_value(&bad_rast).unwrap()).unwrap_err();
        assert!(err.contains("rasterizer"), "{err}");

        // v99 refuses.
        let mut v99 = v1j.clone();
        v99.as_object_mut()
            .unwrap()
            .insert("profile_version".into(), serde_json::json!(99));
        let err = parse_header_render_for_test(&v99).unwrap_err();
        assert!(err.contains("profile_version"), "{err}");

        // Missing profile_version refuses.
        let mut missing = v1j.clone();
        missing.as_object_mut().unwrap().remove("profile_version");
        let err = parse_header_render_for_test(&missing).unwrap_err();
        assert!(err.contains("missing profile_version"), "{err}");
    }

    #[test]
    fn persisted_missing_fallback_mismatch_refuses() {
        let mut p = RenderProfile::sealed_v2_pressure(0x00)
            .unwrap()
            .to_persisted_v2()
            .unwrap();
        assert_eq!(p.pressure_missing_fallback_radius, LEGACY_FIXED_RADIUS);
        p.pressure_missing_fallback_radius = 3;
        let err = validate_persisted_render_profile_v2(&p).unwrap_err();
        assert!(
            err.contains("pressure curve")
                || err.contains("thresholds")
                || err.contains("fallback"),
            "{err}"
        );
        // Valid sealed value still loads.
        p.pressure_missing_fallback_radius = LEGACY_FIXED_RADIUS;
        assert!(validate_persisted_render_profile_v2(&p).is_ok());
        assert!(validate_persisted_render_profile_v1(
            &RenderProfile::sealed_v1(0x00)
                .unwrap()
                .to_persisted_v1()
                .unwrap()
        )
        .is_ok());
    }

    #[test]
    fn persisted_stroke_v1_refuses_unknown_brush_directly() {
        let mut s = legacy_pts();
        s.brush = Some("future-brush".into());
        assert!(PersistedStrokeV1::from_stroke(&s).is_err());
    }
}
