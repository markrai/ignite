//! In-memory grayscale framebuffer for host tests.

use crate::display::{DirtyRegion, DisplaySurface, FastInkMode, RefreshMode};
use crate::error::{PlatformError, PlatformResult};

/// Software surface: 8-bit grayscale, white background, black ink.
#[derive(Clone, Debug)]
pub struct HostDisplay {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    /// Every region actually submitted to [`DisplaySurface::refresh`], after clamping.
    pub refresh_log: Vec<(DirtyRegion, RefreshMode)>,
}

impl HostDisplay {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![255; (width as usize) * (height as usize)],
            refresh_log: Vec::new(),
        }
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn inked_pixel_count(&self) -> usize {
        self.pixels.iter().filter(|&&p| p < 128).count()
    }

    /// Write a binary PGM (P5) for visual inspection.
    pub fn write_pgm(&self, path: &std::path::Path) -> PlatformResult<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(path).map_err(|e| PlatformError::Io(e.to_string()))?;
        write!(f, "P5\n{} {}\n255\n", self.width, self.height)
            .map_err(|e| PlatformError::Io(e.to_string()))?;
        f.write_all(&self.pixels)
            .map_err(|e| PlatformError::Io(e.to_string()))?;
        Ok(())
    }

    fn set_pixel(&mut self, x: i32, y: i32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let idx = (y as usize) * (self.width as usize) + (x as usize);
        self.pixels[idx] = 0;
    }

    fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        // Bresenham
        let mut x0 = x0;
        let mut y0 = y0;
        let dx = (x1 - x0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.set_pixel(x0, y0);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }
}

impl DisplaySurface for HostDisplay {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn draw_polyline_black(
        &mut self,
        points: &[(f32, f32)],
    ) -> PlatformResult<Option<DirtyRegion>> {
        if points.is_empty() {
            return Err(PlatformError::InvalidInput(
                "need at least one point".into(),
            ));
        }
        for w in points.windows(2) {
            self.draw_line(
                w[0].0.round() as i32,
                w[0].1.round() as i32,
                w[1].0.round() as i32,
                w[1].1.round() as i32,
            );
        }
        if points.len() == 1 {
            self.set_pixel(points[0].0.round() as i32, points[0].1.round() as i32);
        }
        Ok(DirtyRegion::from_points(points, 2).and_then(|r| r.clamp_to(self.width, self.height)))
    }

    fn refresh(&mut self, region: DirtyRegion, mode: RefreshMode) -> PlatformResult<()> {
        // Mirrors what a hardware backend must do before handing a rectangle to an ioctl.
        if let Some(clamped) = region.clamp_to(self.width, self.height) {
            self.refresh_log.push((clamped, mode));
        }
        Ok(())
    }
}

/// Stub: Kindle fast-ink / A2 mode is not yet confirmed on our unit.
#[derive(Debug, Default)]
pub struct UnsupportedFastInk;

impl FastInkMode for UnsupportedFastInk {
    fn enable(&mut self) -> PlatformResult<()> {
        Err(PlatformError::Unsupported(
            "Kindle fast stylus / e-ink drawing mode not yet confirmed on our device",
        ))
    }

    fn disable(&mut self) -> PlatformResult<()> {
        Err(PlatformError::Unsupported(
            "Kindle fast stylus / e-ink drawing mode not yet confirmed on our device",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_region_from_segment() {
        let r = DirtyRegion::from_points(&[(10.0, 20.0), (40.0, 80.0)], 2).unwrap();
        assert_eq!(r.x, 8);
        assert_eq!(r.y, 18);
        assert!(r.width >= 32);
        assert!(r.height >= 62);
    }

    #[test]
    fn draw_leaves_black_pixels() {
        let mut d = HostDisplay::new(100, 100);
        let dirty = d
            .draw_polyline_black(&[(10.0, 10.0), (50.0, 10.0)])
            .unwrap()
            .expect("on-surface");
        d.refresh(dirty, RefreshMode::Partial).unwrap();
        assert!(d.inked_pixel_count() > 0);
        assert_eq!(d.refresh_log.len(), 1);
    }

    #[test]
    fn draw_returns_a_region_inside_the_surface() {
        let mut d = HostDisplay::new(64, 64);
        // Stroke running off the bottom-right corner.
        let dirty = d
            .draw_polyline_black(&[(60.0, 60.0), (120.0, 120.0)])
            .unwrap()
            .expect("partially on-surface");
        assert!(dirty.is_within(64, 64));
        assert_eq!(dirty.right(), 64);
        assert_eq!(dirty.bottom(), 64);
    }

    #[test]
    fn fully_offscreen_draw_reports_no_dirty_region() {
        let mut d = HostDisplay::new(64, 64);
        let dirty = d
            .draw_polyline_black(&[(500.0, 500.0), (600.0, 600.0)])
            .unwrap();
        assert!(dirty.is_none());
        assert_eq!(d.inked_pixel_count(), 0);
    }

    #[test]
    fn refresh_clamps_before_logging() {
        let mut d = HostDisplay::new(64, 64);
        d.refresh(DirtyRegion::new(-10, -10, 200, 200), RefreshMode::Partial)
            .unwrap();
        assert_eq!(d.refresh_log.len(), 1);
        assert_eq!(d.refresh_log[0].0, DirtyRegion::new(0, 0, 64, 64));
    }

    #[test]
    fn refresh_of_an_offscreen_region_is_not_submitted() {
        let mut d = HostDisplay::new(64, 64);
        d.refresh(DirtyRegion::new(900, 900, 10, 10), RefreshMode::Partial)
            .unwrap();
        assert!(d.refresh_log.is_empty());
    }

    #[test]
    fn empty_polyline_is_an_error() {
        let mut d = HostDisplay::new(8, 8);
        assert!(d.draw_polyline_black(&[]).is_err());
    }

    #[test]
    fn fast_ink_is_unsupported_not_faked() {
        let mut f = UnsupportedFastInk;
        assert!(matches!(f.enable(), Err(PlatformError::Unsupported(_))));
        assert!(matches!(f.disable(), Err(PlatformError::Unsupported(_))));
    }
}
