//! Gate F7: historical F6 v4 render-profile migration compatibility
//! (`pressure_missing_fallback_radius` absent → synthesize 2 for migration only).

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use kindle_platform::FbGeometry;
    use notebook_core::{Point, Stroke, StrokeId, PANEL_HEIGHT_PX, PANEL_WIDTH_PX};

    use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
    use crate::profile::{
        RenderProfile, LEGACY_FIXED_RADIUS, RASTERIZER_ID_V2_PRESSURE, RENDER_PROFILE_VERSION,
        TRANSFORM_ID_V1,
    };
    use crate::render::{radius_for_pressure, raster_stroke_into};
    use crate::store::{
        create_new_page, load_existing_page, migrate_all_pages, migrate_page_v4_to_v5_if_needed,
        normalize_historical_v4_render_for_migration, page_dir, parse_header_render_for_test,
        write_meta, PersistedPageHeader, EDIT_JOURNAL_VERSION, EDIT_JOURNAL_VERSION_V1,
        FORMAT_VERSION, FORMAT_VERSION_V4,
    };

    fn tmp_dir() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scribe-f7-v4-compat-{n}"));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn profile00() -> RenderProfile {
        RenderProfile::sealed_v2_pressure(0x00).unwrap()
    }

    /// Exact on-device historical F6 v4 shape: sealed pressure fields, fallback ABSENT.
    fn historical_v4_render_missing_fallback(ink: u8) -> serde_json::Value {
        let sealed = RenderProfile::sealed_v2_pressure(ink)
            .unwrap()
            .to_persisted_v2()
            .unwrap();
        let mut v = serde_json::to_value(&sealed).unwrap();
        v.as_object_mut()
            .unwrap()
            .remove("pressure_missing_fallback_radius");
        assert!(v.get("pressure_missing_fallback_radius").is_none());
        v
    }

    fn forge_historical_v4_page(dir: &std::path::Path, ink: u8) {
        let meta_p = dir.join("page.meta");
        let mut header: PersistedPageHeader =
            serde_json::from_str(&fs::read_to_string(&meta_p).unwrap()).unwrap();
        header.v = FORMAT_VERSION_V4;
        header.edit_journal_version = EDIT_JOURNAL_VERSION_V1;
        header.render = historical_v4_render_missing_fallback(ink);
        write_meta(dir, &header).unwrap();
    }

    fn forge_v4_with_render(dir: &std::path::Path, render: serde_json::Value) {
        let meta_p = dir.join("page.meta");
        let mut header: PersistedPageHeader =
            serde_json::from_str(&fs::read_to_string(&meta_p).unwrap()).unwrap();
        header.v = FORMAT_VERSION_V4;
        header.edit_journal_version = EDIT_JOURNAL_VERSION_V1;
        header.render = render;
        write_meta(dir, &header).unwrap();
    }

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

    #[test]
    fn historical_v4_missing_fallback_migrates_and_seals_explicit_2() {
        let root = tmp_dir();
        let bg = vec![0x11u8; 40 * 8];
        let (_s, _) = create_new_page(&root, "page-1", 40, 8, 40, &bg, &profile00()).unwrap();
        let dir = page_dir(&root, "page-1");
        forge_historical_v4_page(&dir, 0x00);

        let bg_before = fs::read(dir.join("background.raw")).unwrap();
        let strokes_before = fs::read(dir.join("strokes.jsonl")).unwrap();
        let edits_before = fs::read(dir.join("edits.jsonl")).unwrap();

        assert!(migrate_page_v4_to_v5_if_needed(&root, "page-1").unwrap());

        assert_eq!(fs::read(dir.join("background.raw")).unwrap(), bg_before);
        assert_eq!(fs::read(dir.join("strokes.jsonl")).unwrap(), strokes_before);
        assert_eq!(fs::read(dir.join("edits.jsonl")).unwrap(), edits_before);

        let header: PersistedPageHeader =
            serde_json::from_str(&fs::read_to_string(dir.join("page.meta")).unwrap()).unwrap();
        assert_eq!(header.v, FORMAT_VERSION);
        assert_eq!(header.edit_journal_version, EDIT_JOURNAL_VERSION);
        assert_eq!(
            header
                .render
                .get("pressure_missing_fallback_radius")
                .and_then(|v| v.as_u64()),
            Some(LEGACY_FIXED_RADIUS as u64)
        );
        // Strict current parser accepts resulting v5 header.
        let profile = parse_header_render_for_test(&header.render).unwrap();
        assert!(profile.pressure.is_some());
        assert_eq!(
            profile.pressure.unwrap().missing_fallback_radius,
            LEGACY_FIXED_RADIUS
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn historical_v4_explicit_fallback_2_migrates() {
        let root = tmp_dir();
        let bg = vec![0x22u8; 40 * 8];
        let (_s, _) = create_new_page(&root, "page-1", 40, 8, 40, &bg, &profile00()).unwrap();
        let dir = page_dir(&root, "page-1");
        // create_new_page already wrote sealed v2 with fallback=2; forge to v4 keeping it.
        let mut header: PersistedPageHeader =
            serde_json::from_str(&fs::read_to_string(dir.join("page.meta")).unwrap()).unwrap();
        header.v = FORMAT_VERSION_V4;
        header.edit_journal_version = EDIT_JOURNAL_VERSION_V1;
        write_meta(&dir, &header).unwrap();
        assert!(migrate_page_v4_to_v5_if_needed(&root, "page-1").unwrap());
        let h: PersistedPageHeader =
            serde_json::from_str(&fs::read_to_string(dir.join("page.meta")).unwrap()).unwrap();
        assert_eq!(h.v, FORMAT_VERSION);
        assert_eq!(
            h.render
                .get("pressure_missing_fallback_radius")
                .and_then(|v| v.as_u64()),
            Some(2)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn historical_v4_fallback_3_refuses_without_rewrite() {
        let root = tmp_dir();
        let bg = vec![0x33u8; 40 * 8];
        let (_s, _) = create_new_page(&root, "page-1", 40, 8, 40, &bg, &profile00()).unwrap();
        let dir = page_dir(&root, "page-1");
        let mut render = historical_v4_render_missing_fallback(0x00);
        render.as_object_mut().unwrap().insert(
            "pressure_missing_fallback_radius".into(),
            serde_json::json!(3),
        );
        forge_v4_with_render(&dir, render);
        let meta_before = fs::read(dir.join("page.meta")).unwrap();
        let err = migrate_page_v4_to_v5_if_needed(&root, "page-1").unwrap_err();
        assert!(
            err.contains("pressure") || err.contains("threshold") || err.contains("sealed"),
            "{err}"
        );
        assert_eq!(fs::read(dir.join("page.meta")).unwrap(), meta_before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn historical_v4_other_missing_field_refuses() {
        let root = tmp_dir();
        let bg = vec![0x44u8; 40 * 8];
        let (_s, _) = create_new_page(&root, "page-1", 40, 8, 40, &bg, &profile00()).unwrap();
        let dir = page_dir(&root, "page-1");
        let mut render = historical_v4_render_missing_fallback(0x00);
        // Missing another required field (and still missing fallback).
        render.as_object_mut().unwrap().remove("pressure_curve");
        forge_v4_with_render(&dir, render);
        let meta_before = fs::read(dir.join("page.meta")).unwrap();
        let err = migrate_page_v4_to_v5_if_needed(&root, "page-1").unwrap_err();
        assert!(
            err.contains("bad render profile") || err.contains("missing"),
            "{err}"
        );
        assert_eq!(fs::read(dir.join("page.meta")).unwrap(), meta_before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn historical_v4_bad_thresholds_refuses() {
        let root = tmp_dir();
        let bg = vec![0x55u8; 40 * 8];
        let (_s, _) = create_new_page(&root, "page-1", 40, 8, 40, &bg, &profile00()).unwrap();
        let dir = page_dir(&root, "page-1");
        let mut render = historical_v4_render_missing_fallback(0x00);
        render
            .as_object_mut()
            .unwrap()
            .insert("pressure_q_r1_max".into(), serde_json::json!(1));
        forge_v4_with_render(&dir, render);
        let meta_before = fs::read(dir.join("page.meta")).unwrap();
        let err = migrate_page_v4_to_v5_if_needed(&root, "page-1").unwrap_err();
        assert!(
            err.contains("threshold") || err.contains("sealed") || err.contains("pressure"),
            "{err}"
        );
        assert_eq!(fs::read(dir.join("page.meta")).unwrap(), meta_before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn historical_v4_bad_rasterizer_refuses() {
        let root = tmp_dir();
        let bg = vec![0x66u8; 40 * 8];
        let (_s, _) = create_new_page(&root, "page-1", 40, 8, 40, &bg, &profile00()).unwrap();
        let dir = page_dir(&root, "page-1");
        let mut render = historical_v4_render_missing_fallback(0x00);
        render
            .as_object_mut()
            .unwrap()
            .insert("rasterizer".into(), serde_json::json!("not-a-rasterizer"));
        forge_v4_with_render(&dir, render);
        let meta_before = fs::read(dir.join("page.meta")).unwrap();
        let err = migrate_page_v4_to_v5_if_needed(&root, "page-1").unwrap_err();
        assert!(err.contains("rasterizer"), "{err}");
        assert_eq!(fs::read(dir.join("page.meta")).unwrap(), meta_before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn v5_missing_fallback_still_refuses_strict_parser() {
        let sealed = RenderProfile::sealed_v2_pressure(0x00)
            .unwrap()
            .to_persisted_v2()
            .unwrap();
        let mut v = serde_json::to_value(&sealed).unwrap();
        v.as_object_mut()
            .unwrap()
            .remove("pressure_missing_fallback_radius");
        let err = parse_header_render_for_test(&v).unwrap_err();
        assert!(
            err.contains("pressure_missing_fallback_radius") || err.contains("missing field"),
            "{err}"
        );
    }

    #[test]
    fn current_page_corrupted_missing_fallback_refuses_open() {
        let root = tmp_dir();
        let bg = vec![0x77u8; 40 * 8];
        let (_s, _) = create_new_page(&root, "page-1", 40, 8, 40, &bg, &profile00()).unwrap();
        let dir = page_dir(&root, "page-1");
        let mut header: PersistedPageHeader =
            serde_json::from_str(&fs::read_to_string(dir.join("page.meta")).unwrap()).unwrap();
        assert_eq!(header.v, FORMAT_VERSION);
        header
            .render
            .as_object_mut()
            .unwrap()
            .remove("pressure_missing_fallback_radius");
        write_meta(&dir, &header).unwrap();
        match load_existing_page(&root, "page-1") {
            Ok(_) => panic!("expected refuse on missing fallback"),
            Err(err) => {
                assert!(
                    err.contains("pressure_missing_fallback_radius")
                        || err.contains("missing field"),
                    "{err}"
                );
            }
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn normalize_matches_sealed_v2_semantics_and_missing_pressure_pixels() {
        let historical = historical_v4_render_missing_fallback(0x00);
        let normalized = normalize_historical_v4_render_for_migration(&historical).unwrap();
        let from_hist = parse_header_render_for_test(&normalized).unwrap();
        let sealed = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        assert_eq!(from_hist, sealed);

        let curve_h = from_hist.pressure.unwrap();
        let curve_s = sealed.pressure.unwrap();
        assert_eq!(radius_for_pressure(curve_h, None), LEGACY_FIXED_RADIUS);
        assert_eq!(radius_for_pressure(curve_s, None), LEGACY_FIXED_RADIUS);

        let stroke = Stroke {
            id: StrokeId::new("s-miss"),
            points: vec![
                Point::new(100.0, 100.0).with_pressure(0.5),
                Point::new(200.0, 150.0), // pressure=None
                Point::new(300.0, 200.0).with_pressure(0.8),
            ],
            brush: Some(STYLE_PRESSURE_WIDTH_V1.into()),
            finished: true,
        };
        let geo = panel_geo();
        let mut a = vec![0xffu8; (geo.line_length * geo.yres) as usize];
        let mut b = vec![0xffu8; a.len()];
        raster_stroke_into(&mut a, &geo, &from_hist, &stroke).unwrap();
        raster_stroke_into(&mut b, &geo, &sealed, &stroke).unwrap();
        assert_eq!(a, b);
        let _ = (
            RASTERIZER_ID_V2_PRESSURE,
            TRANSFORM_ID_V1,
            RENDER_PROFILE_VERSION,
        );
    }

    #[test]
    fn multi_page_mixed_historical_and_explicit_migrates() {
        let root = tmp_dir();
        let bg = vec![0x88u8; 30 * 6];
        for id in ["page-1", "page-2", "page-3"] {
            let (_s, _) = create_new_page(&root, id, 30, 6, 30, &bg, &profile00()).unwrap();
        }
        forge_historical_v4_page(&page_dir(&root, "page-1"), 0x00);
        // page-2: explicit fallback=2
        {
            let dir = page_dir(&root, "page-2");
            let mut header: PersistedPageHeader =
                serde_json::from_str(&fs::read_to_string(dir.join("page.meta")).unwrap()).unwrap();
            header.v = FORMAT_VERSION_V4;
            header.edit_journal_version = EDIT_JOURNAL_VERSION_V1;
            write_meta(&dir, &header).unwrap();
        }
        forge_historical_v4_page(&page_dir(&root, "page-3"), 0x00);

        let snaps: Vec<_> = ["page-1", "page-2", "page-3"]
            .iter()
            .map(|id| {
                let d = page_dir(&root, id);
                (
                    fs::read(d.join("background.raw")).unwrap(),
                    fs::read(d.join("strokes.jsonl")).unwrap(),
                    fs::read(d.join("edits.jsonl")).unwrap(),
                )
            })
            .collect();

        let ids = vec!["page-1".into(), "page-2".into(), "page-3".into()];
        migrate_all_pages(&root, &ids).unwrap();
        for (i, id) in ["page-1", "page-2", "page-3"].iter().enumerate() {
            let d = page_dir(&root, id);
            let h: PersistedPageHeader =
                serde_json::from_str(&fs::read_to_string(d.join("page.meta")).unwrap()).unwrap();
            assert_eq!(h.v, FORMAT_VERSION);
            assert_eq!(h.edit_journal_version, EDIT_JOURNAL_VERSION);
            assert_eq!(
                h.render
                    .get("pressure_missing_fallback_radius")
                    .and_then(|v| v.as_u64()),
                Some(2)
            );
            assert_eq!(fs::read(d.join("background.raw")).unwrap(), snaps[i].0);
            assert_eq!(fs::read(d.join("strokes.jsonl")).unwrap(), snaps[i].1);
            assert_eq!(fs::read(d.join("edits.jsonl")).unwrap(), snaps[i].2);
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn multi_page_refuses_when_one_page_has_fallback_3() {
        let root = tmp_dir();
        let bg = vec![0x99u8; 30 * 6];
        for id in ["page-1", "page-2", "page-3"] {
            let (_s, _) = create_new_page(&root, id, 30, 6, 30, &bg, &profile00()).unwrap();
            forge_historical_v4_page(&page_dir(&root, id), 0x00);
        }
        // Poison page-2 with fallback=3.
        let dir2 = page_dir(&root, "page-2");
        let mut render = historical_v4_render_missing_fallback(0x00);
        render.as_object_mut().unwrap().insert(
            "pressure_missing_fallback_radius".into(),
            serde_json::json!(3),
        );
        forge_v4_with_render(&dir2, render);
        let meta2 = fs::read(dir2.join("page.meta")).unwrap();

        let ids = vec!["page-1".into(), "page-2".into(), "page-3".into()];
        let err = migrate_all_pages(&root, &ids).unwrap_err();
        assert!(err.contains("page-2"), "{err}");
        assert_eq!(fs::read(dir2.join("page.meta")).unwrap(), meta2);
        // page-1 may already be v5 (per-page atomic).
        let h1: PersistedPageHeader = serde_json::from_str(
            &fs::read_to_string(page_dir(&root, "page-1").join("page.meta")).unwrap(),
        )
        .unwrap();
        assert_eq!(h1.v, FORMAT_VERSION);
        let _ = fs::remove_dir_all(root);
    }
}
