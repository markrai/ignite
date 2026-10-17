//! Shared stroke rasterization for live wet ink and restart reconstruction.
//!
//! All callers must pass the page's sealed [`RenderProfile`] so reconstruction
//! cannot silently depend on CLI defaults or compile-time globals.
//!
//! Gate F6: a stroke's `brush` field selects the raster style:
//!   * `None` or [`STYLE_LEGACY_FIXED_V1`] → fixed `profile.brush_radius`
//!     (byte-identical to pre-F6 `scribe-raster-v1`).
//!   * [`STYLE_PRESSURE_WIDTH_V1`] → per-sample width from the sealed
//!     [`PressureCurveV1`] via `raster_line_variable_radius`.

use std::collections::HashSet;

use kindle_platform::{
    raster_line, raster_line_variable_radius, raster_stamp, DirtyRegion, FbGeometry,
};
use notebook_core::{PagePoint, Point, Stroke};

use crate::pressure_curve::{
    interpolate_q, quantize_pressure, PressureCurveV1, STYLE_LEGACY_FIXED_V1,
    STYLE_PRESSURE_WIDTH_V1,
};
use crate::profile::RenderProfile;

/// Which raster style a stroke uses, resolved from its `brush` tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrokeStyle {
    /// Fixed square brush at `profile.brush_radius` (legacy `scribe-raster-v1`).
    LegacyFixed,
    /// Per-sample pressure width via the sealed curve.
    PressureWidth,
}

/// Resolve the raster style from a stroke's `brush` tag. Unknown styles refuse.
pub fn style_for_stroke(stroke: &Stroke) -> Result<StrokeStyle, String> {
    match stroke.brush.as_deref() {
        None => Ok(StrokeStyle::LegacyFixed),
        Some(STYLE_LEGACY_FIXED_V1) => Ok(StrokeStyle::LegacyFixed),
        Some(STYLE_PRESSURE_WIDTH_V1) => Ok(StrokeStyle::PressureWidth),
        Some(other) => Err(format!("REFUSE RENDER: unknown stroke brush style {other}")),
    }
}

/// Radius for a single sample under the pressure curve (used for single-point strokes
/// and live-per-sample stamps). Missing/non-finite pressure → curve fallback radius.
pub fn radius_for_pressure(curve: PressureCurveV1, pressure: Option<f32>) -> u32 {
    curve.radius_for_pressure(pressure)
}

fn merge(dirty: &mut Option<DirtyRegion>, d: Option<DirtyRegion>) {
    if let Some(d) = d {
        *dirty = Some(match *dirty {
            Some(s) => s.union(d),
            None => d,
        });
    }
}

/// Conservative max square-brush radius this profile can paint (legacy or pressure).
pub fn max_paint_radius(profile: &RenderProfile) -> u32 {
    let mut r = profile.brush_radius;
    if let Ok(c) = profile.pressure_curve() {
        r = r
            .max(c.max_radius)
            .max(c.min_radius)
            .max(c.missing_fallback_radius);
    }
    r
}

/// Panel AABB of a stroke's samples after the same clamp as [`raster_stroke_into`], expanded by
/// the profile's maximum square brush. Conservative: false positives only.
pub fn stroke_rendered_bounds(
    profile: &RenderProfile,
    stroke: &Stroke,
) -> Result<Option<DirtyRegion>, String> {
    stroke_rendered_bounds_shifted(profile, stroke, 0.0, 0.0)
}

fn stroke_rendered_bounds_shifted(
    profile: &RenderProfile,
    stroke: &Stroke,
    dx: f32,
    dy: f32,
) -> Result<Option<DirtyRegion>, String> {
    let Some((min_x, min_y, max_x, max_y)) = stroke.bounds() else {
        return Ok(None);
    };
    let xf = profile.transform()?;
    let pad = max_paint_radius(profile) as i32;
    let corners = [
        PagePoint::new(min_x + dx, min_y + dy),
        PagePoint::new(max_x + dx, min_y + dy),
        PagePoint::new(min_x + dx, max_y + dy),
        PagePoint::new(max_x + dx, max_y + dy),
    ];
    let mut pts = [(0.0f32, 0.0f32); 4];
    let mut n = 0usize;
    for p in corners {
        if let Some((x, y)) = xf.to_panel_pixel(p) {
            pts[n] = (x as f32, y as f32);
            n += 1;
        }
    }
    if n == 0 {
        return Ok(None);
    }
    Ok(DirtyRegion::from_points(&pts[..n], pad))
}

pub fn stroke_intersects_dirty(
    profile: &RenderProfile,
    stroke: &Stroke,
    dirty: DirtyRegion,
) -> Result<bool, String> {
    stroke_intersects_dirty_shifted(profile, stroke, dirty, 0.0, 0.0)
}

fn stroke_intersects_dirty_shifted(
    profile: &RenderProfile,
    stroke: &Stroke,
    dirty: DirtyRegion,
    dx: f32,
    dy: f32,
) -> Result<bool, String> {
    match stroke_rendered_bounds_shifted(profile, stroke, dx, dy)? {
        None => Ok(false),
        Some(b) => Ok(b.intersects(dirty)),
    }
}

/// Packed 8bpp geometry for a dirty rectangle (no panel stride padding).
pub fn region_geometry(w: u32, h: u32) -> Result<FbGeometry, String> {
    if w == 0 || h == 0 {
        return Err("empty region geometry".into());
    }
    let smem = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or("region geometry overflow")?;
    Ok(FbGeometry {
        xres: w,
        yres: h,
        xres_virtual: w,
        yres_virtual: h,
        xoffset: 0,
        yoffset: 0,
        bits_per_pixel: 8,
        line_length: w,
        smem_len: smem,
    })
}

/// Raster one finished canonical stroke using `profile` (transform, ink, style/radius).
pub fn raster_stroke_into(
    buf: &mut [u8],
    geo: &FbGeometry,
    profile: &RenderProfile,
    stroke: &Stroke,
) -> Result<Option<DirtyRegion>, String> {
    raster_stroke_into_at(buf, geo, profile, stroke, 0, 0)
}

/// Same as [`raster_stroke_into`], with panel `(origin_x, origin_y)` mapping to buffer `(0, 0)`.
pub fn raster_stroke_into_at(
    buf: &mut [u8],
    geo: &FbGeometry,
    profile: &RenderProfile,
    stroke: &Stroke,
    origin_x: i32,
    origin_y: i32,
) -> Result<Option<DirtyRegion>, String> {
    raster_stroke_into_at_shifted(buf, geo, profile, stroke, origin_x, origin_y, 0.0, 0.0)
}

/// Raster `stroke` with a transient canonical translation. The page model is not cloned.
#[allow(clippy::too_many_arguments)]
fn raster_stroke_into_at_shifted(
    buf: &mut [u8],
    geo: &FbGeometry,
    profile: &RenderProfile,
    stroke: &Stroke,
    origin_x: i32,
    origin_y: i32,
    dx: f32,
    dy: f32,
) -> Result<Option<DirtyRegion>, String> {
    if stroke.points.is_empty() {
        return Ok(None);
    }
    let style = style_for_stroke(stroke)?;
    let transform = profile.transform()?;
    let ink = profile.ink_byte;

    match style {
        StrokeStyle::LegacyFixed => {
            let radius = profile.brush_radius;
            let mut dirty: Option<DirtyRegion> = None;
            let mut last_px: Option<(i32, i32)> = None;
            for p in &stroke.points {
                let Some((x, y)) = transform.to_panel_pixel(PagePoint::new(p.x + dx, p.y + dy))
                else {
                    continue;
                };
                let (px, py) = (x as i32 - origin_x, y as i32 - origin_y);
                let painted = if let Some((lx, ly)) = last_px {
                    raster_line(buf, geo, lx, ly, px, py, ink, radius)
                } else {
                    raster_stamp(buf, geo, px, py, ink, radius)
                }
                .map_err(|e| format!("{e:?}"))?;
                merge(&mut dirty, painted.dirty);
                last_px = Some((px, py));
            }
            Ok(dirty)
        }
        StrokeStyle::PressureWidth => {
            let curve = profile.pressure_curve()?;
            let mut dirty: Option<DirtyRegion> = None;
            let mut last: Option<((i32, i32), &Point)> = None;
            for p in &stroke.points {
                let Some((x, y)) = transform.to_panel_pixel(PagePoint::new(p.x + dx, p.y + dy))
                else {
                    continue;
                };
                let (px, py) = (x as i32 - origin_x, y as i32 - origin_y);
                let painted = if let Some(((lx, ly), lp)) = last {
                    raster_pressure_segment_panel(
                        buf,
                        geo,
                        curve,
                        ink,
                        (lx, ly),
                        lp.pressure,
                        (px, py),
                        p.pressure,
                    )?
                } else {
                    let r = radius_for_pressure(curve, p.pressure);
                    raster_stamp(buf, geo, px, py, ink, r).map_err(|e| format!("{e:?}"))?
                };
                merge(&mut dirty, painted.dirty);
                last = Some(((px, py), p));
            }
            Ok(dirty)
        }
    }
}

/// Paint one pressure-width segment (panel coords) with per-step interpolated radius.
///
/// Shared by restart reconstruction ([`raster_stroke_into`]) and live wet ink
/// ([`crate::runtime::KindleTransport`]) so both are byte-identical.
///
/// When both endpoints carry finite pressure, the quantized pressure is interpolated
/// deterministically per Bresenham step and mapped through the curve. If either endpoint
/// is missing pressure, the whole segment uses the curve's fixed fallback radius (matching
/// [`PressureCurveV1::radius_for_pressure`] for `None`).
#[allow(clippy::too_many_arguments)]
pub fn raster_pressure_segment_panel(
    buf: &mut [u8],
    geo: &FbGeometry,
    curve: PressureCurveV1,
    ink: u8,
    from: (i32, i32),
    from_p: Option<f32>,
    to: (i32, i32),
    to_p: Option<f32>,
) -> Result<kindle_platform::RasterResult, String> {
    let finite = |p: Option<f32>| p.map(|v| v.is_finite()).unwrap_or(false);
    let (x0, y0) = from;
    let (x1, y1) = to;
    if finite(from_p) && finite(to_p) {
        let q0 = quantize_pressure(from_p.unwrap());
        let q1 = quantize_pressure(to_p.unwrap());
        raster_line_variable_radius(buf, geo, x0, y0, x1, y1, ink, |step, steps| {
            curve.radius_for_q(interpolate_q(q0, q1, step, steps))
        })
        .map_err(|e| format!("{e:?}"))
    } else {
        let r = curve.missing_fallback_radius;
        raster_line(buf, geo, x0, y0, x1, y1, ink, r).map_err(|e| format!("{e:?}"))
    }
}

/// Raster many committed strokes in journal order; returns union dirty.
pub fn raster_strokes_into(
    buf: &mut [u8],
    geo: &FbGeometry,
    profile: &RenderProfile,
    strokes: &[Stroke],
) -> Result<Option<DirtyRegion>, String> {
    let mut dirty: Option<DirtyRegion> = None;
    for s in strokes {
        merge(&mut dirty, raster_stroke_into(buf, geo, profile, s)?);
    }
    Ok(dirty)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionReplayStats {
    pub considered: usize,
    pub replayed: usize,
}

/// Raster only strokes whose conservative rendered AABB intersects `dirty`.
///
/// `region` is packed 8bpp with origin at panel `(origin_x, origin_y)`. Journal order preserved.
pub fn raster_intersecting_strokes_into_region(
    region: &mut [u8],
    region_geo: &FbGeometry,
    origin_x: i32,
    origin_y: i32,
    dirty: DirtyRegion,
    profile: &RenderProfile,
    strokes: &[Stroke],
) -> Result<RegionReplayStats, String> {
    let mut stats = RegionReplayStats {
        considered: strokes.len(),
        replayed: 0,
    };
    for s in strokes {
        if !stroke_intersects_dirty(profile, s, dirty)? {
            continue;
        }
        raster_stroke_into_at(region, region_geo, profile, s, origin_x, origin_y)?;
        stats.replayed += 1;
    }
    Ok(stats)
}

/// Regional replay with a transient canonical translation for selected visible stroke IDs.
/// Translation is applied at raster time; selected strokes are not cloned.
#[allow(clippy::too_many_arguments)]
pub fn raster_intersecting_strokes_into_region_translated(
    region: &mut [u8],
    region_geo: &FbGeometry,
    origin_x: i32,
    origin_y: i32,
    dirty: DirtyRegion,
    profile: &RenderProfile,
    strokes: &[Stroke],
    moved_ids: &HashSet<String>,
    dx: f32,
    dy: f32,
) -> Result<RegionReplayStats, String> {
    let mut stats = RegionReplayStats {
        considered: strokes.len(),
        replayed: 0,
    };
    for stroke in strokes {
        let (ox, oy) = if moved_ids.contains(stroke.id.as_str()) {
            (dx, dy)
        } else {
            (0.0, 0.0)
        };
        if !stroke_intersects_dirty_shifted(profile, stroke, dirty, ox, oy)? {
            continue;
        }
        raster_stroke_into_at_shifted(
            region, region_geo, profile, stroke, origin_x, origin_y, ox, oy,
        )?;
        stats.replayed += 1;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::RenderProfile;
    use notebook_core::{new_stroke_id, Point};

    fn geo() -> FbGeometry {
        FbGeometry {
            xres: 100,
            yres: 100,
            xres_virtual: 100,
            yres_virtual: 100,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 100,
            smem_len: 10_000,
        }
    }

    fn sample_stroke() -> Stroke {
        Stroke {
            id: new_stroke_id(),
            points: vec![
                Point::new(840.0, 840.0),
                Point::new(1680.0, 1680.0),
                Point::new(2520.0, 840.0),
            ],
            brush: None,
            finished: true,
        }
    }

    #[test]
    fn raster_writes_pixels() {
        let mut buf = vec![0xffu8; 10_000];
        let profile = RenderProfile::sealed_v1(0x00).unwrap();
        let dirty = raster_stroke_into(&mut buf, &geo(), &profile, &sample_stroke()).unwrap();
        assert!(dirty.is_some());
        assert!(buf.contains(&0x00));
    }

    #[test]
    fn notebook_ink_stays_logical_then_encodes_per_mode() {
        let g = geo();
        let mut logical = vec![0xffu8; g.smem_len as usize];
        let profile = RenderProfile::sealed_v1(0x00).unwrap();
        raster_stroke_into(&mut logical, &g, &profile, &sample_stroke()).unwrap();
        assert!(logical.contains(&0x00));

        let mut g2 = vec![0x33u8; logical.len()];
        kindle_platform::copy_logical_rect_to_hw(
            &logical,
            &mut g2,
            g.line_length,
            0,
            0,
            g.xres,
            g.yres,
            false,
        )
        .unwrap();
        assert_eq!(g2, logical);

        let mut g1 = vec![0x33u8; logical.len()];
        kindle_platform::copy_logical_rect_to_hw(
            &logical,
            &mut g1,
            g.line_length,
            0,
            0,
            g.xres,
            g.yres,
            true,
        )
        .unwrap();
        for (h, l) in g1.iter().zip(logical.iter()) {
            assert_eq!(*h, !*l);
        }
        assert!(g1.contains(&0xFF));
        assert!(!g1.contains(&0x00) || logical.iter().any(|&b| b == 0xFF));
    }

    #[test]
    fn deterministic_live_vs_reconstruction_ink_00() {
        assert_byte_identical(0x00);
    }

    #[test]
    fn deterministic_live_vs_reconstruction_ink_ff() {
        assert_byte_identical(0xff);
    }

    fn assert_byte_identical(ink: u8) {
        let g = geo();
        let stroke = sample_stroke();
        let profile = RenderProfile::sealed_v1(ink).unwrap();

        // Live-profile path.
        let mut live = vec![0xAAu8; g.smem_len as usize];
        raster_stroke_into(&mut live, &g, &profile, &stroke).unwrap();

        // Persist → load profile → reconstruction path (same sealed profile object).
        let persisted = profile.to_persisted_v1().unwrap();
        let loaded = crate::profile::validate_persisted_render_profile_v1(&persisted).unwrap();
        let mut recon = vec![0xAAu8; g.smem_len as usize];
        raster_stroke_into(&mut recon, &g, &loaded, &stroke).unwrap();

        assert_eq!(live, recon, "ink={ink:#04x} byte-for-byte mismatch");
    }

    #[test]
    fn pressure_stroke_under_v1_profile_refuses() {
        let g = geo();
        let profile = RenderProfile::sealed_v1(0x00).unwrap();
        let mut s = sample_stroke();
        s.brush = Some(STYLE_PRESSURE_WIDTH_V1.into());
        let err =
            raster_stroke_into(&mut vec![0u8; g.smem_len as usize], &g, &profile, &s).unwrap_err();
        assert!(
            err.contains("pressure-capable") || err.contains("REFUSE RENDER"),
            "err={err}"
        );
    }

    #[test]
    fn legacy_style_matches_none_brush() {
        let g = geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let mut s_none = sample_stroke();
        s_none.brush = None;
        let mut s_legacy = sample_stroke();
        s_legacy.brush = Some(STYLE_LEGACY_FIXED_V1.into());

        let mut a = vec![0xAAu8; g.smem_len as usize];
        raster_stroke_into(&mut a, &g, &profile, &s_none).unwrap();
        let mut b = vec![0xAAu8; g.smem_len as usize];
        raster_stroke_into(&mut b, &g, &profile, &s_legacy).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn unknown_brush_refuses() {
        let g = geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let mut s = sample_stroke();
        s.brush = Some("future-brush".into());
        assert!(raster_stroke_into(&mut vec![0u8; g.smem_len as usize], &g, &profile, &s).is_err());
    }

    #[test]
    fn pressure_stroke_writes_varying_width() {
        use notebook_core::{PANEL_HEIGHT_PX, PANEL_WIDTH_PX};
        // Full panel geometry: the sealed transform maps into real panel pixel space.
        let w = PANEL_WIDTH_PX as u32;
        let h = PANEL_HEIGHT_PX as u32;
        let g = FbGeometry {
            xres: w,
            yres: h,
            xres_virtual: w,
            yres_virtual: h,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: w,
            smem_len: w * h,
        };
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        // Horizontal line across the page, light left → firm right.
        let stroke = Stroke {
            id: new_stroke_id(),
            points: vec![
                Point::new(2000.0, 10000.0).with_pressure(0.02),
                Point::new(13000.0, 10000.0).with_pressure(1.0),
            ],
            brush: Some(STYLE_PRESSURE_WIDTH_V1.into()),
            finished: true,
        };
        let mut buf = vec![0xffu8; g.smem_len as usize];
        let dirty = raster_stroke_into(&mut buf, &g, &profile, &stroke).unwrap();
        assert!(dirty.is_some());
        // Firm end must paint a taller column than the light end.
        let count_col = |buf: &[u8], col: usize| -> usize {
            (0..g.yres as usize)
                .filter(|&row| buf[row * g.line_length as usize + col] == 0x00)
                .count()
        };
        let left = count_col(&buf, 260); // near light (left) end
        let right = count_col(&buf, 1540); // near firm (right) end
        assert!(
            right > left,
            "firm end {right} should be wider than light end {left}"
        );
    }

    fn crop(buf: &[u8], geo: &FbGeometry, dirty: DirtyRegion) -> Vec<u8> {
        let c = dirty.clamp_to(geo.xres, geo.yres).unwrap();
        let mut out = Vec::new();
        for y in c.y as u32..(c.y as u32 + c.height) {
            for x in c.x as u32..(c.x as u32 + c.width) {
                let off = geo.byte_offset_visible_8bpp(x, y).unwrap();
                out.push(buf[off]);
            }
        }
        out
    }

    #[test]
    fn region_raster_matches_full_composition_crop() {
        let g = geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let crossing = Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(168.0, 168.0), Point::new(4000.0, 168.0)],
            brush: None,
            finished: true,
        };
        let mut far = Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(14000.0, 18000.0), Point::new(14500.0, 18500.0)],
            brush: None,
            finished: true,
        };
        far.points[0].pressure = Some(0.9);
        far.points[1].pressure = Some(0.9);
        far.brush = Some(STYLE_PRESSURE_WIDTH_V1.into());
        let strokes = [crossing, far];
        let mut full = vec![0xffu8; g.smem_len as usize];
        raster_strokes_into(&mut full, &g, &profile, &strokes).unwrap();
        let dirty = DirtyRegion::new(10, 10, 30, 20);
        let c = dirty.clamp_to(g.xres, g.yres).unwrap();
        let rg = region_geometry(c.width, c.height).unwrap();
        let mut region = vec![0xffu8; (c.width * c.height) as usize];
        let stats = raster_intersecting_strokes_into_region(
            &mut region,
            &rg,
            c.x,
            c.y,
            c,
            &profile,
            &strokes,
        )
        .unwrap();
        assert_eq!(stats.considered, 2);
        assert_eq!(stats.replayed, 1);
        assert_eq!(region, crop(&full, &g, dirty));
    }

    #[test]
    fn dense_page_replays_only_intersecting_strokes() {
        let w = 1860u32;
        let h = 2480u32;
        let g = FbGeometry {
            xres: w,
            yres: h,
            xres_virtual: w,
            yres_virtual: h,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: w,
            smem_len: w * h,
        };
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let mut strokes = Vec::with_capacity(1200);
        for i in 0..1200u32 {
            let col = i % 40;
            let row = i / 40;
            let x = 200.0 + col as f32 * 380.0;
            let y = 200.0 + row as f32 * 680.0;
            strokes.push(Stroke {
                id: new_stroke_id(),
                points: vec![Point::new(x, y), Point::new(x + 80.0, y + 40.0)],
                brush: None,
                finished: true,
            });
        }
        let target = strokes[0].clone();
        let Some(tb) = stroke_rendered_bounds(&profile, &target).unwrap() else {
            panic!("target bounds");
        };
        let dirty = DirtyRegion::new(tb.x, tb.y, tb.width.max(1), tb.height.max(1));
        let c = dirty.clamp_to(g.xres, g.yres).unwrap();
        let rg = region_geometry(c.width, c.height).unwrap();
        let mut region = vec![0xffu8; (c.width as usize) * (c.height as usize)];
        let stats = raster_intersecting_strokes_into_region(
            &mut region,
            &rg,
            c.x,
            c.y,
            c,
            &profile,
            &strokes,
        )
        .unwrap();
        assert_eq!(stats.considered, 1200);
        let old_replayed = 1200usize;
        let old_scratch = (g.xres as usize) * (g.yres as usize);
        let new_replayed = stats.replayed;
        let new_scratch = (c.width as usize) * (c.height as usize);
        assert!(
            new_replayed > 0 && new_replayed < 40,
            "regional replay {new_replayed} of {old_replayed} (old path rasterized all {old_replayed})"
        );
        assert!(
            new_scratch < old_scratch / 8,
            "scratch {new_scratch} vs old full-frame {old_scratch}"
        );
    }

    #[test]
    fn translated_replay_matches_cloned_points_without_mutating_source() {
        let g = geo();
        let profile = RenderProfile::sealed_v1(0x00).unwrap();
        let stroke = sample_stroke();
        let original = stroke.points.clone();
        let dx = 840.0;
        let dy = 0.0;
        let mut cloned = stroke.clone();
        for p in &mut cloned.points {
            p.x += dx;
            p.y += dy;
        }
        let dirty = DirtyRegion::new(0, 0, 100, 100);
        let mut a = vec![0xffu8; g.smem_len as usize];
        let mut b = vec![0xffu8; g.smem_len as usize];
        let mut ids = HashSet::new();
        ids.insert(stroke.id.as_str().to_string());
        raster_intersecting_strokes_into_region(
            &mut a,
            &g,
            0,
            0,
            dirty,
            &profile,
            std::slice::from_ref(&cloned),
        )
        .unwrap();
        raster_intersecting_strokes_into_region_translated(
            &mut b,
            &g,
            0,
            0,
            dirty,
            &profile,
            std::slice::from_ref(&stroke),
            &ids,
            dx,
            dy,
        )
        .unwrap();
        assert_eq!(a, b);
        assert_eq!(stroke.points, original);
    }
}
