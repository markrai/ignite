//! Ink sample in **canonical page space**.

use crate::coords::PagePoint;

/// One stylus sample expressed in canonical page coordinates (see [`crate::coords`]).
///
/// Deliberately *not* display pixels: the same `Point` renders on a Kindle framebuffer, in a
/// browser or on a desktop by changing only the [`crate::ViewportTransform`].
/// Pressure is normalized to `0.0..=1.0` when known.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    /// Canonical page X (page units).
    pub x: f32,
    /// Canonical page Y (page units).
    pub y: f32,
    pub pressure: Option<f32>,
    /// Monotonic timestamp in microseconds, in the capturing process's clock domain.
    pub t_us: Option<u64>,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Self {
        Self {
            x,
            y,
            pressure: None,
            t_us: None,
        }
    }

    pub fn at(p: PagePoint) -> Self {
        Self::new(p.x, p.y)
    }

    pub fn page_point(&self) -> PagePoint {
        PagePoint::new(self.x, self.y)
    }

    /// True when two samples occupy the same page location (ignores pressure / time).
    pub fn same_location(&self, other: &Point) -> bool {
        self.x == other.x && self.y == other.y
    }

    pub fn with_pressure(mut self, pressure: f32) -> Self {
        self.pressure = Some(pressure.clamp(0.0, 1.0));
        self
    }

    pub fn with_time(mut self, t_us: u64) -> Self {
        self.t_us = Some(t_us);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_location_ignores_pressure_and_time() {
        let a = Point::new(1.0, 2.0).with_pressure(0.2).with_time(10);
        let b = Point::new(1.0, 2.0).with_pressure(0.9).with_time(99);
        assert!(a.same_location(&b));
        assert!(!a.same_location(&Point::new(1.0, 2.5)));
    }

    #[test]
    fn round_trips_page_point() {
        let p = PagePoint::new(4.0, 5.0);
        assert_eq!(Point::at(p).page_point(), p);
    }
}
