//! Kindle Scribe 1st Gen page profile (firmware 5.18.6, confirmed on-device).
//!
//! The Wacom digitizer raw range matches Amazon NBK canvas coordinates. That space is the
//! **canonical notebook page** for this device. The visible panel (1860×2480) is an 8.4×
//! display scale applied only through [`ScribeDisplayTransform`] / [`ViewportTransform`].

use crate::coords::{DisplayPoint, PageGeometry, PagePoint};
use crate::transform::{DeviceCalibration, Orientation, ViewportTransform};

/// Canonical page width in Wacom / NBK units (confirmed = `ABS_X` max on `WacomDigitizer`).
pub const CANONICAL_WIDTH: f32 = 15_624.0;
/// Canonical page height in Wacom / NBK units (confirmed = `ABS_Y` max on `WacomDigitizer`).
pub const CANONICAL_HEIGHT: f32 = 20_832.0;

/// Visible e-ink panel width in pixels. Display/viewport only — not notebook geometry.
pub const PANEL_WIDTH_PX: f32 = 1_860.0;
/// Visible e-ink panel height in pixels. Display/viewport only — not notebook geometry.
pub const PANEL_HEIGHT_PX: f32 = 2_480.0;

/// Confirmed scale factor panel → canonical (`15624/1860 == 20832/2480 == 8.4`).
pub const PANEL_TO_CANONICAL_SCALE: f32 = 8.4;

/// Gate C candidate: canonical Scribe page → visible panel pixels.
///
/// Does **not** consult `fb_var.rotate`. Physical evidence may later require flip/rotate
/// variants; do not invent them without observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ScribeDisplayTransform {
    /// `display = canonical * panel / page` with origin top-left, no flip.
    #[default]
    PortraitIdentity,
}

impl ScribeDisplayTransform {
    /// Viewport for this candidate (ratio-derived scales, zero offset).
    pub fn viewport(self) -> ViewportTransform {
        match self {
            Self::PortraitIdentity => ViewportTransform {
                scale_x: PANEL_WIDTH_PX / CANONICAL_WIDTH,
                scale_y: PANEL_HEIGHT_PX / CANONICAL_HEIGHT,
                offset_x: 0.0,
                offset_y: 0.0,
            },
        }
    }

    pub fn to_display(self, page: PagePoint) -> DisplayPoint {
        self.viewport().to_display(page)
    }

    /// Continuous display → final panel pixel index in `0..1860` × `0..2480`.
    ///
    /// Floors continuous coordinates, then clamps into the valid pixel range. The far
    /// canonical corner maps to continuous `(1860, 2480)`, which becomes pixel `(1859, 2479)`.
    pub fn to_panel_pixel(self, page: PagePoint) -> Option<(u32, u32)> {
        let d = self.to_display(page);
        if !d.x.is_finite() || !d.y.is_finite() {
            return None;
        }
        let max_x = (PANEL_WIDTH_PX as u32).saturating_sub(1);
        let max_y = (PANEL_HEIGHT_PX as u32).saturating_sub(1);
        let xi = (d.x.floor() as i64).clamp(0, max_x as i64) as u32;
        let yi = (d.y.floor() as i64).clamp(0, max_y as i64) as u32;
        Some((xi, yi))
    }
}

/// Canonical page geometry for a freeform Scribe notebook.
pub fn page_geometry() -> PageGeometry {
    PageGeometry::new(CANONICAL_WIDTH, CANONICAL_HEIGHT)
}

/// Portrait calibration: raw Wacom `ABS_X/Y` map 1:1 into canonical page coordinates.
pub fn portrait_wacom_calibration() -> DeviceCalibration {
    DeviceCalibration {
        device_min_x: 0.0,
        device_max_x: CANONICAL_WIDTH,
        device_min_y: 0.0,
        device_max_y: CANONICAL_HEIGHT,
        orientation: Orientation::Portrait0,
        page: page_geometry(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coords::DevicePoint;

    #[test]
    fn canonical_geometry_matches_wacom_and_nbk() {
        let g = page_geometry();
        assert_eq!(g.width, 15_624.0);
        assert_eq!(g.height, 20_832.0);
    }

    #[test]
    fn panel_scale_is_exactly_eight_point_four() {
        assert!((CANONICAL_WIDTH / PANEL_WIDTH_PX - PANEL_TO_CANONICAL_SCALE).abs() < 1e-6);
        assert!((CANONICAL_HEIGHT / PANEL_HEIGHT_PX - PANEL_TO_CANONICAL_SCALE).abs() < 1e-6);
    }

    #[test]
    fn portrait_identity_preserves_corners_and_midpoint() {
        let c = portrait_wacom_calibration();
        assert_eq!(
            c.to_page(DevicePoint::new(0.0, 0.0)),
            crate::PagePoint::new(0.0, 0.0)
        );
        assert_eq!(
            c.to_page(DevicePoint::new(CANONICAL_WIDTH, CANONICAL_HEIGHT)),
            crate::PagePoint::new(CANONICAL_WIDTH, CANONICAL_HEIGHT)
        );
        let mid = c.to_page(DevicePoint::new(
            CANONICAL_WIDTH / 2.0,
            CANONICAL_HEIGHT / 2.0,
        ));
        assert!((mid.x - CANONICAL_WIDTH / 2.0).abs() < 1e-3);
        assert!((mid.y - CANONICAL_HEIGHT / 2.0).abs() < 1e-3);
    }

    #[test]
    fn portrait_identity_display_corners_and_center() {
        let t = ScribeDisplayTransform::PortraitIdentity;
        let o = t.to_display(PagePoint::new(0.0, 0.0));
        assert!((o.x - 0.0).abs() < 1e-4 && (o.y - 0.0).abs() < 1e-4);

        let far = t.to_display(PagePoint::new(CANONICAL_WIDTH, CANONICAL_HEIGHT));
        assert!((far.x - PANEL_WIDTH_PX).abs() < 1e-3);
        assert!((far.y - PANEL_HEIGHT_PX).abs() < 1e-3);

        let mid = t.to_display(PagePoint::new(
            CANONICAL_WIDTH / 2.0,
            CANONICAL_HEIGHT / 2.0,
        ));
        assert!((mid.x - 930.0).abs() < 1e-2);
        assert!((mid.y - 1240.0).abs() < 1e-2);
    }

    #[test]
    fn portrait_identity_panel_pixels_are_valid_indices() {
        let t = ScribeDisplayTransform::PortraitIdentity;
        assert_eq!(t.to_panel_pixel(PagePoint::new(0.0, 0.0)), Some((0, 0)));
        assert_eq!(
            t.to_panel_pixel(PagePoint::new(CANONICAL_WIDTH, CANONICAL_HEIGHT)),
            Some((1859, 2479))
        );
        assert_eq!(
            t.to_panel_pixel(PagePoint::new(7812.0, 10416.0)),
            Some((930, 1240))
        );
        assert_eq!(
            t.to_panel_pixel(PagePoint::new(
                CANONICAL_WIDTH - PANEL_TO_CANONICAL_SCALE,
                CANONICAL_HEIGHT - PANEL_TO_CANONICAL_SCALE
            )),
            Some((1859, 2479))
        );
    }

    #[test]
    fn portrait_identity_edge_interior_pixels() {
        let t = ScribeDisplayTransform::PortraitIdentity;
        // Points immediately inside the four edges → valid indices, not boundary endpoints.
        let left = t
            .to_panel_pixel(PagePoint::new(50.0, CANONICAL_HEIGHT / 2.0))
            .unwrap();
        assert!(left.0 >= 1 && left.0 < 20, "left={left:?}");
        assert_eq!(left.1, 1240);

        let top = t
            .to_panel_pixel(PagePoint::new(CANONICAL_WIDTH / 2.0, 50.0))
            .unwrap();
        assert_eq!(top.0, 930);
        assert!(top.1 >= 1 && top.1 < 20, "top={top:?}");

        let right = t
            .to_panel_pixel(PagePoint::new(
                CANONICAL_WIDTH - 50.0,
                CANONICAL_HEIGHT / 2.0,
            ))
            .unwrap();
        assert!(right.0 > 1840 && right.0 < 1860, "right={right:?}");
        assert_eq!(right.1, 1240);

        let bottom = t
            .to_panel_pixel(PagePoint::new(
                CANONICAL_WIDTH / 2.0,
                CANONICAL_HEIGHT - 50.0,
            ))
            .unwrap();
        assert_eq!(bottom.0, 930);
        assert!(bottom.1 > 2460 && bottom.1 < 2480, "bottom={bottom:?}");
    }

    #[test]
    fn panel_pixel_indices_always_in_range() {
        let t = ScribeDisplayTransform::PortraitIdentity;
        for &(cx, cy) in &[
            (-100.0, -100.0),
            (0.0, 0.0),
            (7812.0, 10416.0),
            (15624.0, 20832.0),
            (20_000.0, 30_000.0),
        ] {
            let (x, y) = t.to_panel_pixel(PagePoint::new(cx, cy)).unwrap();
            assert!(x < 1860, "x={x}");
            assert!(y < 2480, "y={y}");
        }
    }

    #[test]
    fn portrait_identity_is_monotonic() {
        let t = ScribeDisplayTransform::PortraitIdentity;
        let mut prev_x = -1.0f32;
        let mut x = 0.0f32;
        while x <= CANONICAL_WIDTH {
            let d = t.to_display(PagePoint::new(x, 0.0));
            assert!(d.x + 1e-5 >= prev_x);
            prev_x = d.x;
            x += 137.0;
        }
        let mut prev_y = -1.0f32;
        let mut y = 0.0f32;
        while y <= CANONICAL_HEIGHT {
            let d = t.to_display(PagePoint::new(0.0, y));
            assert!(d.y + 1e-5 >= prev_y);
            prev_y = d.y;
            y += 181.0;
        }
    }

    #[test]
    fn scales_are_ratio_derived_not_magic_alone() {
        let v = ScribeDisplayTransform::PortraitIdentity.viewport();
        assert!((v.scale_x - PANEL_WIDTH_PX / CANONICAL_WIDTH).abs() < 1e-9);
        assert!((v.scale_y - PANEL_HEIGHT_PX / CANONICAL_HEIGHT).abs() < 1e-9);
        assert_eq!(v.offset_x, 0.0);
        assert_eq!(v.offset_y, 0.0);
    }
}
