//! Panel-space line rasterization for Gate D streaming ink (host-testable).
//!
//! Integer Bresenham centers + a fixed square brush. No antialiasing, no pressure.

use crate::display::DirtyRegion;
use crate::fbdev_abi::{FbGeometry, PanelRectError};

/// Half-extent of the Gate-D brush square (`radius=2` → ~5×5 stamp).
pub const STREAM_BRUSH_RADIUS: u32 = 2;

/// Result of painting a segment (or single stamp).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RasterResult {
    /// Clipped dirty rectangle covering all written pixels, if any landed on-panel.
    pub dirty: Option<DirtyRegion>,
    /// Number of framebuffer bytes assigned (including overwrites of the same pixel).
    pub pixels_written: u32,
}

fn put_pixel(
    buf: &mut [u8],
    g: &FbGeometry,
    x: u32,
    y: u32,
    ink: u8,
) -> Result<bool, PanelRectError> {
    if g.check_panel_rect(x, y, 1, 1).is_err() {
        return Ok(false);
    }
    let vx = g
        .xoffset
        .checked_add(x)
        .ok_or(PanelRectError::WidthOverflow)?;
    let vy = g
        .yoffset
        .checked_add(y)
        .ok_or(PanelRectError::HeightOverflow)?;
    let off = (vy as u64)
        .checked_mul(g.line_length as u64)
        .and_then(|r| r.checked_add(vx as u64))
        .ok_or(PanelRectError::WidthOverflow)?;
    let idx = usize::try_from(off).map_err(|_| PanelRectError::WidthOverflow)?;
    if idx >= buf.len() {
        return Ok(false);
    }
    buf[idx] = ink;
    Ok(true)
}

fn stamp_brush(
    buf: &mut [u8],
    g: &FbGeometry,
    cx: i32,
    cy: i32,
    radius: u32,
    ink: u8,
) -> Result<(Option<DirtyRegion>, u32), PanelRectError> {
    let r = radius as i32;
    let mut written = 0u32;
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for dy in -r..=r {
        for dx in -r..=r {
            let x = cx + dx;
            let y = cy + dy;
            if x < 0 || y < 0 {
                continue;
            }
            let xu = x as u32;
            let yu = y as u32;
            if put_pixel(buf, g, xu, yu, ink)? {
                written = written.saturating_add(1);
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if written == 0 {
        return Ok((None, 0));
    }
    let dirty = DirtyRegion::new(
        min_x,
        min_y,
        (max_x - min_x + 1) as u32,
        (max_y - min_y + 1) as u32,
    )
    .clamp_to(g.xres, g.yres);
    Ok((dirty, written))
}

fn merge_dirty(a: Option<DirtyRegion>, b: Option<DirtyRegion>) -> Option<DirtyRegion> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b)) => Some(a.union(b)),
    }
}

/// Paint a single brush stamp at panel integer `(x, y)`.
pub fn raster_stamp(
    buf: &mut [u8],
    g: &FbGeometry,
    x: i32,
    y: i32,
    ink: u8,
    radius: u32,
) -> Result<RasterResult, PanelRectError> {
    let (dirty, pixels_written) = stamp_brush(buf, g, x, y, radius, ink)?;
    Ok(RasterResult {
        dirty,
        pixels_written,
    })
}

/// Bresenham line from `(x0,y0)` to `(x1,y1)` with a square brush of `radius`.
///
/// Endpoints are inclusive. A zero-length segment paints one stamp.
#[allow(clippy::too_many_arguments)]
pub fn raster_line(
    buf: &mut [u8],
    g: &FbGeometry,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    ink: u8,
    radius: u32,
) -> Result<RasterResult, PanelRectError> {
    let mut dirty = None;
    let mut pixels_written = 0u32;

    let mut x = x0;
    let mut y = y0;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;

    loop {
        let (d, w) = stamp_brush(buf, g, x, y, radius, ink)?;
        dirty = merge_dirty(dirty, d);
        pixels_written = pixels_written.saturating_add(w);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }

    Ok(RasterResult {
        dirty,
        pixels_written,
    })
}

/// Bresenham dashed line from `(x0,y0)` to `(x1,y1)` with a square brush of `radius`.
///
/// `phase` is the distance along the dash cycle (`0..dash_on+dash_off`) and is advanced by
/// one per inclusive Bresenham pixel so successive short segments keep one polyline pattern.
/// Stamp when `*phase < dash_on`. If `skip_start` is set and the segment has length, the
/// first pixel is omitted (already consumed by the previous segment's inclusive endpoint).
///
/// `dash_on + dash_off == 0` paints solid and does not mutate `phase`.
#[allow(clippy::too_many_arguments)]
pub fn raster_dashed_line(
    buf: &mut [u8],
    g: &FbGeometry,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    ink: u8,
    radius: u32,
    dash_on: u32,
    dash_off: u32,
    skip_start: bool,
    phase: &mut u32,
) -> Result<RasterResult, PanelRectError> {
    let period = dash_on.saturating_add(dash_off);
    let mut dirty = None;
    let mut pixels_written = 0u32;

    let mut x = x0;
    let mut y = y0;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut is_start = true;

    loop {
        let omit = skip_start && is_start;
        is_start = false;
        if !omit {
            let stamp = period == 0 || *phase < dash_on;
            if stamp {
                let (d, w) = stamp_brush(buf, g, x, y, radius, ink)?;
                dirty = merge_dirty(dirty, d);
                pixels_written = pixels_written.saturating_add(w);
            }
            if period > 0 {
                *phase = (*phase + 1) % period;
            }
        }
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }

    Ok(RasterResult {
        dirty,
        pixels_written,
    })
}

/// Bresenham line with a **per-step** brush radius (Gate F6 pressure width).
///
/// `radius_at(step, total_steps)` is called for each inclusive pixel. `total_steps` is the
/// number of Bresenham iterations from start to end (0 for a single-point stamp).
#[allow(clippy::too_many_arguments)]
pub fn raster_line_variable_radius<F>(
    buf: &mut [u8],
    g: &FbGeometry,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    ink: u8,
    mut radius_at: F,
) -> Result<RasterResult, PanelRectError>
where
    F: FnMut(u32, u32) -> u32,
{
    let total_steps = bresenham_step_count(x0, y0, x1, y1);
    let mut dirty = None;
    let mut pixels_written = 0u32;

    let mut x = x0;
    let mut y = y0;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut step = 0u32;

    loop {
        let radius = radius_at(step, total_steps);
        let (d, w) = stamp_brush(buf, g, x, y, radius, ink)?;
        dirty = merge_dirty(dirty, d);
        pixels_written = pixels_written.saturating_add(w);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
        step = step.saturating_add(1);
    }

    Ok(RasterResult {
        dirty,
        pixels_written,
    })
}

fn bresenham_step_count(x0: i32, y0: i32, x1: i32, y1: i32) -> u32 {
    if x0 == x1 && y0 == y1 {
        return 0;
    }
    let mut x = x0;
    let mut y = y0;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut steps = 0u32;
    loop {
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
        steps = steps.saturating_add(1);
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(w: u32, h: u32) -> FbGeometry {
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
    fn horizontal_segment_writes_row() {
        let geo = g(40, 40);
        let mut buf = vec![0x55u8; 1600];
        let r = raster_line(&mut buf, &geo, 5, 10, 15, 10, 0x00, 0).unwrap();
        assert!(r.pixels_written >= 11);
        for x in 5..=15 {
            assert_eq!(buf[10 * 40 + x], 0x00);
        }
        let d = r.dirty.unwrap();
        assert_eq!(d.y, 10);
        assert_eq!(d.x, 5);
        assert_eq!(d.width, 11);
    }

    #[test]
    fn vertical_segment_writes_column() {
        let geo = g(40, 40);
        let mut buf = vec![0x55u8; 1600];
        let r = raster_line(&mut buf, &geo, 8, 2, 8, 12, 0xff, 0).unwrap();
        for y in 2..=12 {
            assert_eq!(buf[y * 40 + 8], 0xff);
        }
        assert_eq!(r.dirty.unwrap().height, 11);
    }

    #[test]
    fn diagonal_and_reverse_cover_endpoints() {
        let geo = g(50, 50);
        let mut buf = vec![0u8; 2500];
        let a = raster_line(&mut buf, &geo, 1, 1, 10, 10, 0xaa, 0).unwrap();
        assert_eq!(buf[50 + 1], 0xaa);
        assert_eq!(buf[10 * 50 + 10], 0xaa);
        assert!(a.pixels_written >= 10);

        let mut buf2 = vec![0u8; 2500];
        let b = raster_line(&mut buf2, &geo, 10, 10, 1, 1, 0xaa, 0).unwrap();
        assert_eq!(buf2[50 + 1], 0xaa);
        assert_eq!(buf2[10 * 50 + 10], 0xaa);
        assert_eq!(a.pixels_written, b.pixels_written);
    }

    #[test]
    fn single_point_stroke_is_one_stamp() {
        let geo = g(20, 20);
        let mut buf = vec![0x11u8; 400];
        let r = raster_line(&mut buf, &geo, 7, 9, 7, 9, 0x00, 0).unwrap();
        assert_eq!(buf[9 * 20 + 7], 0x00);
        assert_eq!(r.pixels_written, 1);
    }

    #[test]
    fn brush_radius_two_is_about_five_pixels() {
        let geo = g(30, 30);
        let mut buf = vec![0u8; 900];
        let r = raster_stamp(&mut buf, &geo, 15, 15, 0xff, STREAM_BRUSH_RADIUS).unwrap();
        assert_eq!(r.pixels_written, 25); // 5×5 square
        let d = r.dirty.unwrap();
        assert_eq!(d.width, 5);
        assert_eq!(d.height, 5);
        assert_eq!(buf[15 * 30 + 15], 0xff);
        assert_eq!(buf[13 * 30 + 13], 0xff);
        assert_eq!(buf[12 * 30 + 15], 0); // outside radius
    }

    #[test]
    fn dirty_clips_near_all_edges() {
        let geo = g(20, 20);
        let mut buf = vec![0u8; 400];
        let tl = raster_stamp(&mut buf, &geo, 0, 0, 0xff, STREAM_BRUSH_RADIUS)
            .unwrap()
            .dirty
            .unwrap();
        assert_eq!((tl.x, tl.y), (0, 0));
        assert!(tl.width <= 3 && tl.height <= 3);

        let br = raster_stamp(&mut buf, &geo, 19, 19, 0xff, STREAM_BRUSH_RADIUS)
            .unwrap()
            .dirty
            .unwrap();
        assert_eq!(br.right(), 20);
        assert_eq!(br.bottom(), 20);

        let tr = raster_stamp(&mut buf, &geo, 19, 0, 0xff, STREAM_BRUSH_RADIUS)
            .unwrap()
            .dirty
            .unwrap();
        assert_eq!(tr.y, 0);
        assert_eq!(tr.right(), 20);

        let bl = raster_stamp(&mut buf, &geo, 0, 19, 0xff, STREAM_BRUSH_RADIUS)
            .unwrap()
            .dirty
            .unwrap();
        assert_eq!(bl.x, 0);
        assert_eq!(bl.bottom(), 20);
    }

    #[test]
    fn dirty_union_of_segments() {
        let geo = g(40, 40);
        let mut buf = vec![0u8; 1600];
        let a = raster_line(&mut buf, &geo, 5, 5, 10, 5, 0x00, 0)
            .unwrap()
            .dirty
            .unwrap();
        let b = raster_line(&mut buf, &geo, 20, 20, 25, 20, 0x00, 0)
            .unwrap()
            .dirty
            .unwrap();
        let u = a.union(b);
        assert_eq!(u.x, 5);
        assert_eq!(u.y, 5);
        assert_eq!(u.right(), 26);
        assert_eq!(u.bottom(), 21);
    }

    fn dash_on_at(x: i32, dash_on: u32, dash_off: u32) -> bool {
        let period = dash_on + dash_off;
        (x as u32) % period < dash_on
    }

    #[test]
    fn dashed_horizontal_line_has_gaps() {
        let geo = g(40, 40);
        let mut buf = vec![0x55u8; 1600];
        let mut phase = 0u32;
        let r = raster_dashed_line(
            &mut buf, &geo, 0, 10, 39, 10, 0x00, 0, 3, 5, false, &mut phase,
        )
        .unwrap();
        assert!(r.pixels_written > 0);
        assert!(r.pixels_written < 40);
        let mut on = 0u32;
        let mut off = 0u32;
        for x in 0..=39 {
            if dash_on_at(x, 3, 5) {
                assert_eq!(buf[10 * 40 + x as usize], 0x00, "dash-on pixel {x}");
                on += 1;
            } else {
                assert_eq!(buf[10 * 40 + x as usize], 0x55, "dash-off pixel {x}");
                off += 1;
            }
        }
        assert_eq!(on, 15);
        assert_eq!(off, 25);
        assert_eq!(phase, 0);
    }

    #[test]
    fn dashed_segment_continues_phase_across_join() {
        let geo = g(40, 40);
        let mut buf = vec![0x55u8; 1600];
        let mut phase = 0u32;
        raster_dashed_line(
            &mut buf, &geo, 0, 10, 10, 10, 0x00, 0, 3, 5, false, &mut phase,
        )
        .unwrap();
        assert_eq!(phase, 3);
        raster_dashed_line(
            &mut buf, &geo, 10, 10, 20, 10, 0x00, 0, 3, 5, true, &mut phase,
        )
        .unwrap();
        for x in 0..=20 {
            if dash_on_at(x, 3, 5) {
                assert_eq!(buf[10 * 40 + x as usize], 0x00, "continued dash-on {x}");
            } else {
                assert_eq!(buf[10 * 40 + x as usize], 0x55, "continued dash-off {x}");
            }
        }
        let mut restarted = vec![0x55u8; 1600];
        let mut restart_phase = 0u32;
        raster_dashed_line(
            &mut restarted,
            &geo,
            10,
            10,
            20,
            10,
            0x00,
            0,
            3,
            5,
            false,
            &mut restart_phase,
        )
        .unwrap();
        assert_eq!(restarted[10 * 40 + 11], 0x00);
        assert_eq!(buf[10 * 40 + 11], 0x55);
    }
}
