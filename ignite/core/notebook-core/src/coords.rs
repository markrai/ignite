//! Explicit coordinate spaces.
//!
//! The notebook model is deliberately **not** expressed in framebuffer pixels. The pipeline is:
//!
//! ```text
//! raw stylus/device coordinates      DevicePoint   (device axis units, orientation-naive)
//!         ↓ device calibration/orientation         [DeviceCalibration]
//! canonical page coordinates         PagePoint     (page units, origin top-left)
//!         ↓ viewport/display transform             [ViewportTransform]
//! framebuffer pixels                 DisplayPoint  (pixels of whatever surface renders)
//! ```
//!
//! Only the last stage knows about a concrete display. A Kindle framebuffer, a browser canvas
//! and a desktop window differ only in their [`ViewportTransform`]; the stored notebook geometry
//! ([`crate::Point`], [`crate::Stroke`]) is always in canonical page coordinates.

/// Raw digitizer sample, in the device's own ABS axis units, before orientation is applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DevicePoint {
    pub x: f32,
    pub y: f32,
}

impl DevicePoint {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// Canonical page coordinate: origin top-left, units defined by [`PageGeometry`].
///
/// This is the only space persisted in the notebook model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PagePoint {
    pub x: f32,
    pub y: f32,
}

impl PagePoint {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// A pixel on some concrete output surface (framebuffer, canvas, window).
///
/// Never stored in the notebook model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayPoint {
    pub x: f32,
    pub y: f32,
}

impl DisplayPoint {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// Extent of one canonical page, in page units.
///
/// Page units are display-independent. The probe happens to pick a geometry numerically equal
/// to its host surface so the viewport transform is 1:1, but nothing in the model requires that.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageGeometry {
    pub width: f32,
    pub height: f32,
}

impl PageGeometry {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width: width.max(f32::EPSILON),
            height: height.max(f32::EPSILON),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_geometry_rejects_zero_extent() {
        let g = PageGeometry::new(0.0, -5.0);
        assert!(g.width > 0.0);
        assert!(g.height > 0.0);
    }
}
