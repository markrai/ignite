//! Gate F6 host tests: pressure→width mapping, live-vs-reconstruction byte identity for
//! pressure-width strokes, mixed v1/v2 journal load, and migration pixel identity for legacy
//! strokes (old fixed profile vs post-v4 legacy style).

#[cfg(test)]
mod tests {
    use kindle_platform::{raster_line, raster_stamp, FbGeometry};
    use notebook_core::{
        new_stroke_id, Point, ScribeDisplayTransform, Stroke, PANEL_HEIGHT_PX, PANEL_WIDTH_PX,
    };

    use crate::pressure_curve::{PressureCurveV1, STYLE_LEGACY_FIXED_V1, STYLE_PRESSURE_WIDTH_V1};
    use crate::profile::RenderProfile;
    use crate::render::{radius_for_pressure, raster_pressure_segment_panel, raster_stroke_into};
    use crate::store::{parse_stroke_journal, stroke_record_line};

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

    fn panel(page: &Point) -> (i32, i32) {
        let (x, y) = ScribeDisplayTransform::PortraitIdentity
            .to_panel_pixel(page.page_point())
            .unwrap();
        (x as i32, y as i32)
    }

    /// Simulate the exact live wet-ink path used by `KindleTransport` for a pressure profile:
    /// per-point `stamp` on down, per-segment `segment` on move/up, sharing the same render
    /// helpers as reconstruction.
    fn render_live_pressure(
        buf: &mut [u8],
        geo: &FbGeometry,
        curve: PressureCurveV1,
        ink: u8,
        stroke: &Stroke,
    ) {
        let mut last: Option<(i32, i32, Option<f32>)> = None;
        for p in &stroke.points {
            let (px, py) = panel(p);
            match last {
                None => {
                    let r = radius_for_pressure(curve, p.pressure);
                    raster_stamp(buf, geo, px, py, ink, r).unwrap();
                }
                Some((lx, ly, lp)) => {
                    raster_pressure_segment_panel(
                        buf,
                        geo,
                        curve,
                        ink,
                        (lx, ly),
                        lp,
                        (px, py),
                        p.pressure,
                    )
                    .unwrap();
                }
            }
            last = Some((px, py, p.pressure));
        }
    }

    /// Simulate the legacy live wet-ink path (fixed radius) as pre-F6 `KindleTransport` did.
    fn render_live_fixed(buf: &mut [u8], geo: &FbGeometry, radius: u32, ink: u8, stroke: &Stroke) {
        let mut last: Option<(i32, i32)> = None;
        for p in &stroke.points {
            let (px, py) = panel(p);
            match last {
                None => {
                    raster_stamp(buf, geo, px, py, ink, radius).unwrap();
                }
                Some((lx, ly)) => {
                    raster_line(buf, geo, lx, ly, px, py, ink, radius).unwrap();
                }
            }
            last = Some((px, py));
        }
    }

    fn pressure_stroke() -> Stroke {
        // Long diagonal, light → firm → light.
        Stroke {
            id: new_stroke_id(),
            points: vec![
                Point::new(1000.0, 1000.0).with_pressure(0.02),
                Point::new(4000.0, 4000.0).with_pressure(0.62),
                Point::new(8000.0, 8000.0).with_pressure(0.90),
                Point::new(12000.0, 12000.0).with_pressure(0.99),
                Point::new(15000.0, 15000.0).with_pressure(0.03),
            ],
            brush: Some(STYLE_PRESSURE_WIDTH_V1.into()),
            finished: true,
        }
    }

    // ---- mapping ----

    #[test]
    fn mapping_light_normal_firm_bands() {
        let c = PressureCurveV1::sealed_v1();
        assert_eq!(c.radius_for_pressure(Some(0.02)), 1);
        assert_eq!(c.radius_for_pressure(Some(0.06)), 1);
        assert_eq!(c.radius_for_pressure(Some(0.60)), 2);
        assert_eq!(c.radius_for_pressure(Some(0.67)), 2);
        assert_eq!(c.radius_for_pressure(Some(0.85)), 3);
        assert_eq!(c.radius_for_pressure(Some(0.92)), 4);
        assert_eq!(c.radius_for_pressure(Some(1.0)), 4);
        // Missing → fallback (legacy fixed radius 2).
        assert_eq!(c.radius_for_pressure(None), 2);
    }

    // ---- live vs reconstruction byte identity (pressure) ----

    #[test]
    fn live_vs_recon_pressure_byte_identical_ink_00() {
        assert_pressure_identity(0x00);
    }

    #[test]
    fn live_vs_recon_pressure_byte_identical_ink_ff() {
        assert_pressure_identity(0xff);
    }

    fn assert_pressure_identity(ink: u8) {
        let g = panel_geo();
        let profile = RenderProfile::sealed_v2_pressure(ink).unwrap();
        let curve = profile.pressure.unwrap();
        let stroke = pressure_stroke();

        let mut live = vec![0xAAu8; g.smem_len as usize];
        render_live_pressure(&mut live, &g, curve, ink, &stroke);

        let mut recon = vec![0xAAu8; g.smem_len as usize];
        raster_stroke_into(&mut recon, &g, &profile, &stroke).unwrap();

        assert_eq!(
            live, recon,
            "pressure live vs recon mismatch ink={ink:#04x}"
        );
    }

    #[test]
    fn single_point_pressure_stamp_matches_recon() {
        let g = panel_geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let curve = profile.pressure.unwrap();
        let stroke = Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(5000.0, 6000.0).with_pressure(0.9)],
            brush: Some(STYLE_PRESSURE_WIDTH_V1.into()),
            finished: true,
        };
        let mut live = vec![0x00u8; g.smem_len as usize];
        render_live_pressure(&mut live, &g, curve, 0xff, &stroke);
        let mut recon = vec![0x00u8; g.smem_len as usize];
        // Match ink for the live sim above.
        let profile = RenderProfile::sealed_v2_pressure(0xff).unwrap();
        raster_stroke_into(&mut recon, &g, &profile, &stroke).unwrap();
        assert_eq!(live, recon);
    }

    // ---- mixed journal ----

    #[test]
    fn mixed_journal_v1_and_v2_load_with_correct_styles() {
        let legacy = Stroke {
            id: notebook_core::StrokeId::new("s-legacy"),
            points: vec![Point::new(1.0, 1.0), Point::new(2.0, 2.0)],
            brush: None,
            finished: true,
        };
        let mut pressure = Stroke {
            id: notebook_core::StrokeId::new("s-pressure"),
            points: vec![
                Point::new(3.0, 3.0).with_pressure(0.5),
                Point::new(4.0, 4.0).with_pressure(0.9),
            ],
            brush: Some(STYLE_PRESSURE_WIDTH_V1.into()),
            finished: true,
        };
        pressure.finished = true;

        let mut raw = String::new();
        raw.push_str(&stroke_record_line(&legacy).unwrap());
        raw.push_str(&stroke_record_line(&pressure).unwrap());
        // Sanity: the two records used different schema versions.
        assert!(raw.contains("\"v\":1"));
        assert!(raw.contains("\"v\":2"));
        assert!(raw.contains(STYLE_PRESSURE_WIDTH_V1));

        let (strokes, trunc) = parse_stroke_journal(raw.as_bytes()).unwrap();
        assert!(!trunc);
        assert_eq!(strokes.len(), 2);
        assert!(strokes[0].brush.is_none());
        assert_eq!(strokes[1].brush.as_deref(), Some(STYLE_PRESSURE_WIDTH_V1));
    }

    // ---- migration pixel identity for legacy strokes ----

    /// A legacy stroke (no brush) must render byte-identically under the pre-F6 fixed profile
    /// (`sealed_v1`) and under the post-migration pressure profile (`sealed_v2_pressure`), since
    /// legacy strokes keep the fixed `legacy_fixed_radius`. Also identical to an explicit
    /// `legacy-fixed-v1` style.
    #[test]
    fn legacy_stroke_pixel_identity_across_migration() {
        let g = panel_geo();
        let legacy = Stroke {
            id: new_stroke_id(),
            points: vec![
                Point::new(1000.0, 1000.0),
                Point::new(4000.0, 6000.0),
                Point::new(9000.0, 3000.0),
            ],
            brush: None,
            finished: true,
        };

        for ink in [0x00u8, 0xff] {
            let old = RenderProfile::sealed_v1(ink).unwrap();
            let new = RenderProfile::sealed_v2_pressure(ink).unwrap();

            let mut a = vec![0xAAu8; g.smem_len as usize];
            raster_stroke_into(&mut a, &g, &old, &legacy).unwrap();

            let mut b = vec![0xAAu8; g.smem_len as usize];
            raster_stroke_into(&mut b, &g, &new, &legacy).unwrap();

            assert_eq!(a, b, "legacy stroke migration mismatch ink={ink:#04x}");

            // Explicit legacy style renders the same as no-brush.
            let mut explicit = legacy.clone();
            explicit.brush = Some(STYLE_LEGACY_FIXED_V1.into());
            let mut c = vec![0xAAu8; g.smem_len as usize];
            raster_stroke_into(&mut c, &g, &new, &explicit).unwrap();
            assert_eq!(a, c, "explicit legacy style mismatch ink={ink:#04x}");
        }
    }

    /// Legacy live fixed path (pre-F6) equals reconstruction of the same legacy stroke under the
    /// migrated pressure profile — the visual bytes on-panel do not change across the v4 upgrade.
    #[test]
    fn legacy_live_fixed_matches_migrated_recon() {
        let g = panel_geo();
        let legacy = Stroke {
            id: new_stroke_id(),
            points: vec![
                Point::new(2000.0, 2000.0),
                Point::new(6000.0, 9000.0),
                Point::new(11000.0, 5000.0),
            ],
            brush: None,
            finished: true,
        };
        let migrated = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let radius = migrated.brush_radius;

        let mut live = vec![0xAAu8; g.smem_len as usize];
        render_live_fixed(&mut live, &g, radius, migrated.ink_byte, &legacy);

        let mut recon = vec![0xAAu8; g.smem_len as usize];
        raster_stroke_into(&mut recon, &g, &migrated, &legacy).unwrap();

        assert_eq!(live, recon);
    }
}
