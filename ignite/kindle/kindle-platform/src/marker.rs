//! Asymmetric panel marker for Gate C pen→display calibration.
//!
//! Geometry is panel-space only. Callers must snapshot/restore the returned dirty rectangle.
//!
//! Shape (center = `+`):
//! ```text
//!         |
//!         |
//!    -----+---
//!         |
//!         |
//!           ##
//!           ##
//! ```

use crate::fbdev_abi::{FbGeometry, PanelRectError};

/// Half-extent of the crosshair arms from the center pixel (inclusive span = `2*half+1`).
pub const MARKER_HALF: u32 = 10;
/// Extra filled block side length in the bottom-right quadrant (orientation cue).
pub const MARKER_BLOCK: u32 = 3;
/// Offset of the block's top-left from the center toward bottom-right.
pub const MARKER_BLOCK_OFFSET: u32 = 5;

/// Axis-aligned dirty rectangle covering the full clipped marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkerDirty {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Compute the clipped dirty rectangle for a marker centered at `(cx, cy)`.
///
/// Extends far enough to cover both the crosshair (`±MARKER_HALF`) and the BR block.
pub fn marker_dirty_rect(
    cx: u32,
    cy: u32,
    panel_w: u32,
    panel_h: u32,
) -> Result<MarkerDirty, PanelRectError> {
    if cx >= panel_w {
        return Err(PanelRectError::OriginXOutOfRange);
    }
    if cy >= panel_h {
        return Err(PanelRectError::OriginYOutOfRange);
    }
    let x0 = cx.saturating_sub(MARKER_HALF);
    let y0 = cy.saturating_sub(MARKER_HALF);
    let cross_x1 = cx
        .checked_add(MARKER_HALF)
        .and_then(|v| v.checked_add(1))
        .ok_or(PanelRectError::WidthOverflow)?;
    let cross_y1 = cy
        .checked_add(MARKER_HALF)
        .and_then(|v| v.checked_add(1))
        .ok_or(PanelRectError::HeightOverflow)?;
    let block_x1 = cx
        .checked_add(MARKER_BLOCK_OFFSET)
        .and_then(|v| v.checked_add(MARKER_BLOCK))
        .unwrap_or(u32::MAX);
    let block_y1 = cy
        .checked_add(MARKER_BLOCK_OFFSET)
        .and_then(|v| v.checked_add(MARKER_BLOCK))
        .unwrap_or(u32::MAX);
    let x1 = cross_x1.max(block_x1).min(panel_w);
    let y1 = cross_y1.max(block_y1).min(panel_h);
    if x1 <= x0 || y1 <= y0 {
        return Err(PanelRectError::ExtendsPastRightEdge);
    }
    let width = x1 - x0;
    let height = y1 - y0;
    FbGeometry {
        xres: panel_w,
        yres: panel_h,
        xres_virtual: panel_w,
        yres_virtual: panel_h,
        xoffset: 0,
        yoffset: 0,
        bits_per_pixel: 8,
        line_length: panel_w,
        smem_len: 0,
    }
    .check_panel_rect(x0, y0, width, height)?;
    Ok(MarkerDirty {
        x: x0,
        y: y0,
        width,
        height,
    })
}

fn put_pixel(
    buf: &mut [u8],
    g: &FbGeometry,
    x: u32,
    y: u32,
    value: u8,
) -> Result<(), PanelRectError> {
    g.check_panel_rect(x, y, 1, 1)?;
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
        return Err(PanelRectError::ExtendsPastRightEdge);
    }
    buf[idx] = value;
    Ok(())
}

/// Paint an asymmetric crosshair+BR-block at `(cx, cy)` with `ink`.
///
/// Returns the clipped dirty rectangle that must be snapshotted / refreshed.
pub fn paint_asymmetric_marker(
    buf: &mut [u8],
    g: &FbGeometry,
    cx: u32,
    cy: u32,
    ink: u8,
) -> Result<MarkerDirty, PanelRectError> {
    let dirty = marker_dirty_rect(cx, cy, g.xres, g.yres)?;

    let x_lo = cx.saturating_sub(MARKER_HALF);
    let x_hi = cx.saturating_add(MARKER_HALF).min(g.xres.saturating_sub(1));
    for x in x_lo..=x_hi {
        put_pixel(buf, g, x, cy, ink)?;
    }
    let y_lo = cy.saturating_sub(MARKER_HALF);
    let y_hi = cy.saturating_add(MARKER_HALF).min(g.yres.saturating_sub(1));
    for y in y_lo..=y_hi {
        put_pixel(buf, g, cx, y, ink)?;
    }
    let bx0 = cx.saturating_add(MARKER_BLOCK_OFFSET);
    let by0 = cy.saturating_add(MARKER_BLOCK_OFFSET);
    for dy in 0..MARKER_BLOCK {
        for dx in 0..MARKER_BLOCK {
            let Some(x) = bx0.checked_add(dx).filter(|&v| v < g.xres) else {
                continue;
            };
            let Some(y) = by0.checked_add(dy).filter(|&v| v < g.yres) else {
                continue;
            };
            put_pixel(buf, g, x, y, ink)?;
        }
    }
    Ok(dirty)
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
    fn dirty_rect_centered_interior() {
        let d = marker_dirty_rect(100, 200, 1860, 2480).unwrap();
        assert_eq!(d.x, 90);
        assert_eq!(d.y, 190);
        // Cross ±10 plus BR block to offset+size → exclusive end max(111, 108)=111 → width 21
        assert_eq!(d.width, 21);
        assert_eq!(d.height, 21);
    }

    #[test]
    fn dirty_rect_clips_near_all_edges() {
        let tl = marker_dirty_rect(0, 0, 1860, 2480).unwrap();
        assert_eq!((tl.x, tl.y), (0, 0));
        assert!(tl.width >= 1 && tl.height >= 1);

        let tr = marker_dirty_rect(1859, 0, 1860, 2480).unwrap();
        assert_eq!(tr.y, 0);
        assert_eq!(tr.x + tr.width, 1860);

        let bl = marker_dirty_rect(0, 2479, 1860, 2480).unwrap();
        assert_eq!(bl.x, 0);
        assert_eq!(bl.y + bl.height, 2480);

        let br = marker_dirty_rect(1859, 2479, 1860, 2480).unwrap();
        assert_eq!(br.x + br.width, 1860);
        assert_eq!(br.y + br.height, 2480);
    }

    #[test]
    fn paint_sets_cross_and_br_block() {
        let geo = g(40, 40);
        let mut buf = vec![0x55u8; 40 * 40];
        let dirty = paint_asymmetric_marker(&mut buf, &geo, 20, 20, 0x00).unwrap();
        assert!(dirty.width >= 21 && dirty.height >= 21);
        assert_eq!(buf[20 * 40 + 20], 0x00);
        assert_eq!(buf[20 * 40 + 10], 0x00);
        assert_eq!(buf[10 * 40 + 20], 0x00);
        assert_eq!(buf[25 * 40 + 25], 0x00);
        assert_eq!(buf[0], 0x55);
    }

    #[test]
    fn paint_rejects_center_outside_panel() {
        let geo = g(10, 10);
        let mut buf = vec![0u8; 100];
        assert!(paint_asymmetric_marker(&mut buf, &geo, 10, 5, 0xff).is_err());
    }
}
