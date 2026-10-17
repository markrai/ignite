//! Display / framebuffer abstractions.
//!
//! Coordinates here are **display pixels** (see `notebook_core::coords`). Page geometry is
//! converted by a `ViewportTransform` before it reaches this layer.

use crate::error::PlatformResult;

/// An axis-aligned update rectangle in display pixels.
///
/// [`DirtyRegion::from_points`] returns the true geometric bounds, which may be partly or wholly
/// outside the panel. Anything that submits a region to hardware must call
/// [`DirtyRegion::clamp_to`] first: an e-ink refresh ioctl given an out-of-bounds rectangle is at
/// best rejected and at worst undefined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl DirtyRegion {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Geometric bounds of `points`, expanded by `pad` on every side.
    ///
    /// Not clamped: the result may have negative `x`/`y` or extend past the panel.
    pub fn from_points(points: &[(f32, f32)], pad: i32) -> Option<Self> {
        let first = points.first()?;
        let mut min_x = first.0;
        let mut min_y = first.1;
        let mut max_x = first.0;
        let mut max_y = first.1;
        for &(x, y) in &points[1..] {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        let x0 = min_x.floor() as i32 - pad;
        let y0 = min_y.floor() as i32 - pad;
        let x1 = max_x.ceil() as i32 + pad;
        let y1 = max_y.ceil() as i32 + pad;
        Some(Self {
            x: x0,
            y: y0,
            width: (x1 - x0).max(1) as u32,
            height: (y1 - y0).max(1) as u32,
        })
    }

    /// Exclusive right edge.
    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    /// Exclusive bottom edge.
    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Clip to a `width` x `height` display. Returns `None` when nothing remains visible.
    pub fn clamp_to(self, width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 || self.is_empty() {
            return None;
        }
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = self.right().min(width as i32);
        let y1 = self.bottom().min(height as i32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(Self {
            x: x0,
            y: y0,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        })
    }

    /// True when the region lies entirely inside a `width` x `height` display.
    pub fn is_within(&self, width: u32, height: u32) -> bool {
        self.x >= 0 && self.y >= 0 && self.right() <= width as i32 && self.bottom() <= height as i32
    }

    pub fn union(self, other: Self) -> Self {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self.right().max(other.right());
        let y1 = self.bottom().max(other.bottom());
        Self {
            x: x0,
            y: y0,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        }
    }

    /// Inclusive-start / exclusive-end AABB overlap (empty regions never intersect).
    pub fn intersects(self, other: Self) -> bool {
        if self.is_empty() || other.is_empty() {
            return false;
        }
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshMode {
    /// Smallest practical update for ink following the pen.
    Partial,
    Full,
}

/// Fast stylus / A2 / du-style modes.
///
/// FBInk exposes a documented Scribe/MTK pen-mode path (`MXCFB_SET_STYLUS_MODE`); until that is
/// confirmed against our own unit and firmware, implementations here report `Unsupported`.
pub trait FastInkMode {
    fn enable(&mut self) -> PlatformResult<()>;
    fn disable(&mut self) -> PlatformResult<()>;
}

pub trait DisplaySurface {
    fn width(&self) -> u32;
    fn height(&self) -> u32;

    /// Draw a black 1px polyline in display pixel space.
    ///
    /// Returns the dirty region already clamped to this surface, or `None` when the polyline
    /// lies entirely off-surface.
    fn draw_polyline_black(&mut self, points: &[(f32, f32)])
        -> PlatformResult<Option<DirtyRegion>>;

    /// Issue the smallest appropriate refresh for `region`.
    ///
    /// Implementations must clamp; returning `Ok(())` means the refresh was **submitted**, not
    /// that the panel has finished changing state.
    fn refresh(&mut self, region: DirtyRegion, mode: RefreshMode) -> PlatformResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_points_is_unclamped() {
        let r = DirtyRegion::from_points(&[(-10.0, -20.0), (5.0, 5.0)], 2).unwrap();
        assert_eq!(r.x, -12);
        assert_eq!(r.y, -22);
        assert!(r.right() >= 7);
    }

    #[test]
    fn empty_input_has_no_region() {
        assert!(DirtyRegion::from_points(&[], 1).is_none());
    }

    #[test]
    fn single_point_region_is_at_least_one_pixel() {
        let r = DirtyRegion::from_points(&[(3.0, 4.0)], 0).unwrap();
        assert!(r.width >= 1);
        assert!(r.height >= 1);
    }

    #[test]
    fn clamps_past_the_right_and_bottom_edges() {
        let r = DirtyRegion::new(1850, 2470, 100, 100);
        let c = r.clamp_to(1860, 2480).unwrap();
        assert_eq!(c.x, 1850);
        assert_eq!(c.y, 2470);
        assert_eq!(c.width, 10);
        assert_eq!(c.height, 10);
        assert!(c.is_within(1860, 2480));
    }

    #[test]
    fn clamps_past_the_left_and_top_edges() {
        let r = DirtyRegion::new(-30, -40, 100, 100);
        let c = r.clamp_to(1860, 2480).unwrap();
        assert_eq!(c.x, 0);
        assert_eq!(c.y, 0);
        assert_eq!(c.width, 70);
        assert_eq!(c.height, 60);
        assert!(c.is_within(1860, 2480));
    }

    #[test]
    fn region_larger_than_the_panel_becomes_the_panel() {
        let c = DirtyRegion::new(-5, -5, 10_000, 10_000)
            .clamp_to(1860, 2480)
            .unwrap();
        assert_eq!(c, DirtyRegion::new(0, 0, 1860, 2480));
    }

    #[test]
    fn fully_offscreen_regions_clamp_to_nothing() {
        assert!(DirtyRegion::new(2000, 10, 50, 50)
            .clamp_to(1860, 2480)
            .is_none());
        assert!(DirtyRegion::new(10, 3000, 50, 50)
            .clamp_to(1860, 2480)
            .is_none());
        assert!(DirtyRegion::new(-200, 10, 50, 50)
            .clamp_to(1860, 2480)
            .is_none());
        assert!(DirtyRegion::new(10, -200, 50, 50)
            .clamp_to(1860, 2480)
            .is_none());
    }

    #[test]
    fn exact_edge_region_survives_clamping_unchanged() {
        let r = DirtyRegion::new(0, 0, 1860, 2480);
        assert_eq!(r.clamp_to(1860, 2480).unwrap(), r);
        assert!(r.is_within(1860, 2480));
    }

    #[test]
    fn one_pixel_past_the_edge_loses_exactly_one_pixel() {
        let r = DirtyRegion::new(0, 0, 1861, 2481);
        let c = r.clamp_to(1860, 2480).unwrap();
        assert_eq!(c.width, 1860);
        assert_eq!(c.height, 2480);
        assert!(!r.is_within(1860, 2480));
    }

    #[test]
    fn zero_sized_display_or_region_clamps_to_nothing() {
        assert!(DirtyRegion::new(0, 0, 10, 10).clamp_to(0, 10).is_none());
        assert!(DirtyRegion::new(0, 0, 10, 10).clamp_to(10, 0).is_none());
        assert!(DirtyRegion::new(0, 0, 0, 10).clamp_to(10, 10).is_none());
    }

    #[test]
    fn stroke_bounds_beyond_the_panel_are_clamped_before_submission() {
        // A stroke drawn at the very bottom-right corner of a Scribe-sized panel.
        let r = DirtyRegion::from_points(&[(1858.0, 2478.0), (1859.0, 2479.0)], 2).unwrap();
        assert!(!r.is_within(1860, 2480));
        let c = r.clamp_to(1860, 2480).unwrap();
        assert!(c.is_within(1860, 2480));
        assert_eq!(c.right(), 1860);
        assert_eq!(c.bottom(), 2480);
    }

    #[test]
    fn union_covers_both_regions() {
        let a = DirtyRegion::new(0, 0, 10, 10);
        let b = DirtyRegion::new(20, 5, 10, 10);
        let u = a.union(b);
        assert_eq!(u.x, 0);
        assert_eq!(u.y, 0);
        assert_eq!(u.right(), 30);
        assert_eq!(u.bottom(), 15);
    }

    /// Selection-move preview: paint previous ∪ current, then store current only.
    #[test]
    fn selection_move_preview_dirty_is_prev_union_current_not_path() {
        let boxes = [
            DirtyRegion::new(100, 100, 50, 50),
            DirtyRegion::new(150, 120, 50, 50),
            DirtyRegion::new(400, 400, 50, 50),
            DirtyRegion::new(800, 900, 50, 50),
        ];
        let mut footprint: Option<DirtyRegion> = None;
        let mut paints = Vec::new();
        for &current in &boxes {
            let paint = footprint.map_or(current, |old| old.union(current));
            paints.push(paint);
            footprint = Some(current);
        }
        assert_eq!(paints[0], boxes[0]);
        assert_eq!(paints[1], boxes[0].union(boxes[1]));
        assert_eq!(paints[2], boxes[1].union(boxes[2]));
        assert_eq!(paints[3], boxes[2].union(boxes[3]));
        assert_eq!(footprint, Some(boxes[3]));
        let path = boxes[0].union(boxes[1]).union(boxes[2]).union(boxes[3]);
        assert_ne!(paints[3], path);
        assert_ne!(footprint.unwrap(), path);
    }

    #[test]
    fn intersects_detects_overlap_and_empty() {
        let a = DirtyRegion::new(0, 0, 10, 10);
        assert!(a.intersects(DirtyRegion::new(9, 9, 5, 5)));
        assert!(!a.intersects(DirtyRegion::new(10, 0, 5, 5)));
        assert!(!a.intersects(DirtyRegion::new(0, 0, 0, 10)));
        assert!(DirtyRegion::new(-10, 0, 20, 5).intersects(DirtyRegion::new(0, 0, 5, 5)));
    }
}
