//! The two transform stages between raw device samples and output pixels.
//!
//! See [`crate::coords`] for the coordinate spaces themselves.

use crate::coords::{DevicePoint, DisplayPoint, PageGeometry, PagePoint};

/// Panel / device orientation. Rotation is clockwise from the natural portrait panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Orientation {
    #[default]
    Portrait0,
    Landscape90,
    Portrait180,
    Landscape270,
}

/// Stage 1: raw device axis units → canonical page coordinates.
///
/// Owns the device-specific facts (axis ranges, mounting orientation). Axis ranges must come
/// from the device when a real digitizer is used; host fixtures may supply synthetic ranges.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceCalibration {
    pub device_min_x: f32,
    pub device_max_x: f32,
    pub device_min_y: f32,
    pub device_max_y: f32,
    pub orientation: Orientation,
    /// Canonical page extent the device surface maps onto.
    pub page: PageGeometry,
}

impl DeviceCalibration {
    /// Calibration whose device axes already are page units, unrotated.
    pub fn identity(page: PageGeometry) -> Self {
        Self {
            device_min_x: 0.0,
            device_max_x: page.width,
            device_min_y: 0.0,
            device_max_y: page.height,
            orientation: Orientation::Portrait0,
            page,
        }
    }

    pub fn to_page(&self, p: DevicePoint) -> PagePoint {
        let nx = normalize(p.x, self.device_min_x, self.device_max_x);
        let ny = normalize(p.y, self.device_min_y, self.device_max_y);
        let (ux, uy) = match self.orientation {
            Orientation::Portrait0 => (nx, ny),
            Orientation::Landscape90 => (ny, 1.0 - nx),
            Orientation::Portrait180 => (1.0 - nx, 1.0 - ny),
            Orientation::Landscape270 => (1.0 - ny, nx),
        };
        PagePoint::new(ux * self.page.width, uy * self.page.height)
    }
}

/// Stage 2: canonical page coordinates → output pixels.
///
/// Intentionally minimal: uniform-or-anisotropic scale plus translation. This is **not** a
/// layout/viewport system; it exists so `notebook-core` never learns a panel resolution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewportTransform {
    pub scale_x: f32,
    pub scale_y: f32,
    pub offset_x: f32,
    pub offset_y: f32,
}

impl ViewportTransform {
    /// 1:1 page-unit-to-pixel mapping. Used by the latency probe.
    pub fn identity() -> Self {
        Self {
            scale_x: 1.0,
            scale_y: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }
    }

    /// Uniform scale that fits `page` inside `display_width` × `display_height`, centred.
    pub fn fit(page: PageGeometry, display_width: f32, display_height: f32) -> Self {
        let s = (display_width / page.width).min(display_height / page.height);
        Self {
            scale_x: s,
            scale_y: s,
            offset_x: (display_width - page.width * s) * 0.5,
            offset_y: (display_height - page.height * s) * 0.5,
        }
    }

    pub fn to_display(&self, p: PagePoint) -> DisplayPoint {
        DisplayPoint::new(
            p.x * self.scale_x + self.offset_x,
            p.y * self.scale_y + self.offset_y,
        )
    }

    /// Inverse mapping (e.g. hit-testing a tap in pixels). Returns `None` for a degenerate scale.
    pub fn to_page(&self, p: DisplayPoint) -> Option<PagePoint> {
        if self.scale_x.abs() < f32::EPSILON || self.scale_y.abs() < f32::EPSILON {
            return None;
        }
        Some(PagePoint::new(
            (p.x - self.offset_x) / self.scale_x,
            (p.y - self.offset_y) / self.scale_y,
        ))
    }
}

impl Default for ViewportTransform {
    fn default() -> Self {
        Self::identity()
    }
}

fn normalize(v: f32, min: f32, max: f32) -> f32 {
    if (max - min).abs() < f32::EPSILON {
        return 0.0;
    }
    ((v - min) / (max - min)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal(orientation: Orientation, page: PageGeometry) -> DeviceCalibration {
        DeviceCalibration {
            device_min_x: 0.0,
            device_max_x: 100.0,
            device_min_y: 0.0,
            device_max_y: 200.0,
            orientation,
            page,
        }
    }

    #[test]
    fn scales_device_axes_to_page_units() {
        let c = cal(Orientation::Portrait0, PageGeometry::new(1000.0, 2000.0));
        assert_eq!(
            c.to_page(DevicePoint::new(0.0, 0.0)),
            PagePoint::new(0.0, 0.0)
        );
        assert_eq!(
            c.to_page(DevicePoint::new(100.0, 200.0)),
            PagePoint::new(1000.0, 2000.0)
        );
        assert_eq!(
            c.to_page(DevicePoint::new(50.0, 100.0)),
            PagePoint::new(500.0, 1000.0)
        );
    }

    #[test]
    fn rotates_90_clockwise() {
        let c = DeviceCalibration {
            device_max_x: 1.0,
            device_max_y: 1.0,
            ..cal(Orientation::Landscape90, PageGeometry::new(100.0, 200.0))
        };
        assert_eq!(
            c.to_page(DevicePoint::new(0.0, 0.0)),
            PagePoint::new(0.0, 200.0)
        );
        assert_eq!(
            c.to_page(DevicePoint::new(1.0, 0.0)),
            PagePoint::new(0.0, 0.0)
        );
        assert_eq!(
            c.to_page(DevicePoint::new(0.0, 1.0)),
            PagePoint::new(100.0, 200.0)
        );
    }

    #[test]
    fn rotates_180() {
        let c = DeviceCalibration {
            device_max_x: 10.0,
            device_max_y: 10.0,
            ..cal(Orientation::Portrait180, PageGeometry::new(10.0, 10.0))
        };
        assert_eq!(
            c.to_page(DevicePoint::new(0.0, 0.0)),
            PagePoint::new(10.0, 10.0)
        );
        assert_eq!(
            c.to_page(DevicePoint::new(10.0, 10.0)),
            PagePoint::new(0.0, 0.0)
        );
    }

    #[test]
    fn rotates_270() {
        let c = DeviceCalibration {
            device_max_x: 1.0,
            device_max_y: 1.0,
            ..cal(Orientation::Landscape270, PageGeometry::new(100.0, 200.0))
        };
        assert_eq!(
            c.to_page(DevicePoint::new(0.0, 0.0)),
            PagePoint::new(100.0, 0.0)
        );
    }

    #[test]
    fn identity_viewport_is_one_to_one() {
        let v = ViewportTransform::identity();
        assert_eq!(
            v.to_display(PagePoint::new(3.5, 9.0)),
            DisplayPoint::new(3.5, 9.0)
        );
    }

    #[test]
    fn fit_viewport_centres_and_scales_uniformly() {
        // Page 100×200 into a 400×400 display → scale 2, centred horizontally.
        let v = ViewportTransform::fit(PageGeometry::new(100.0, 200.0), 400.0, 400.0);
        assert_eq!(v.scale_x, 2.0);
        assert_eq!(v.scale_y, 2.0);
        assert_eq!(
            v.to_display(PagePoint::new(0.0, 0.0)),
            DisplayPoint::new(100.0, 0.0)
        );
        assert_eq!(
            v.to_display(PagePoint::new(100.0, 200.0)),
            DisplayPoint::new(300.0, 400.0)
        );
    }

    #[test]
    fn viewport_inverse_round_trips() {
        let v = ViewportTransform::fit(PageGeometry::new(100.0, 200.0), 400.0, 400.0);
        let p = PagePoint::new(42.0, 77.0);
        let back = v.to_page(v.to_display(p)).unwrap();
        assert!((back.x - p.x).abs() < 1e-3);
        assert!((back.y - p.y).abs() < 1e-3);
    }

    #[test]
    fn page_geometry_is_independent_of_any_panel_resolution() {
        // Same canonical page rendered onto two very different surfaces.
        let page = PageGeometry::new(1000.0, 1400.0);
        let c = DeviceCalibration::identity(page);
        let pt = c.to_page(DevicePoint::new(500.0, 700.0));
        let kindle = ViewportTransform::fit(page, 1860.0, 2480.0);
        let browser = ViewportTransform::fit(page, 500.0, 700.0);
        assert_ne!(kindle.to_display(pt), browser.to_display(pt));
        // …but the stored page coordinate is identical in both cases.
        assert_eq!(pt, PagePoint::new(500.0, 700.0));
    }
}
