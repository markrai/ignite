//! Device ABS axis ranges.
//!
//! Ranges are a property of the *discovered device* (`EVIOCGABS`), never a constant. In
//! particular there is no assumed pressure maximum: a synthetic value such as `4096` is only
//! ever acceptable in a host fixture, and [`StylusAxes::synthetic`] exists to make that explicit
//! at the call site.
//!
//! Confirmed WacomDigitizer ranges on Scribe 1st Gen 5.18.6 (for documentation; still queried
//! live): X 0..15624, Y 0..20832, PRESSURE 0..4095, DISTANCE 0..255, TILT ±9000.

use notebook_core::{DeviceCalibration, Orientation, PageGeometry};

/// One ABS axis range as reported by the kernel (`struct input_absinfo`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxisRange {
    pub min: i32,
    pub max: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

impl AxisRange {
    pub fn new(min: i32, max: i32) -> Self {
        Self {
            min,
            max,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        }
    }

    pub fn span(&self) -> i32 {
        self.max.saturating_sub(self.min)
    }

    /// Map a raw axis value to `0.0..=1.0`. A degenerate range yields `0.0`.
    pub fn normalize(&self, value: i32) -> f32 {
        let span = self.span();
        if span <= 0 {
            return 0.0;
        }
        (((value - self.min) as f32) / (span as f32)).clamp(0.0, 1.0)
    }

    /// Map a signed axis (e.g. tilt) to `-1.0..=1.0` with the midpoint at `0.0`.
    pub fn normalize_bipolar(&self, value: i32) -> f32 {
        let half = self.span() as f32 / 2.0;
        if half <= 0.0 {
            return 0.0;
        }
        let mid = (self.min as f32 + self.max as f32) / 2.0;
        ((value as f32 - mid) / half).clamp(-1.0, 1.0)
    }
}

/// The axis ranges a stylus pipeline needs.
///
/// Optional axes (`pressure`, `distance`, `tilt_*`) are `None` when the device does not report
/// them; samples then carry `None` rather than invented values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StylusAxes {
    pub x: AxisRange,
    pub y: AxisRange,
    pub pressure: Option<AxisRange>,
    pub distance: Option<AxisRange>,
    pub tilt_x: Option<AxisRange>,
    pub tilt_y: Option<AxisRange>,
}

impl StylusAxes {
    pub fn new(x: AxisRange, y: AxisRange, pressure: Option<AxisRange>) -> Self {
        Self {
            x,
            y,
            pressure,
            distance: None,
            tilt_x: None,
            tilt_y: None,
        }
    }

    /// Explicitly synthetic ranges for host fixtures and the `--demo` path.
    ///
    /// Never use this for a physical device: query the device instead.
    pub fn synthetic(x_max: i32, y_max: i32, pressure_max: Option<i32>) -> Self {
        Self::new(
            AxisRange::new(0, x_max),
            AxisRange::new(0, y_max),
            pressure_max.map(|m| AxisRange::new(0, m)),
        )
    }

    /// Confirmed WacomDigitizer ranges on Scribe 1st Gen — for fixtures / docs, not a live shortcut.
    pub fn scribe_wacom_confirmed() -> Self {
        Self {
            x: AxisRange::new(0, 15_624),
            y: AxisRange::new(0, 20_832),
            pressure: Some(AxisRange::new(0, 4095)),
            distance: Some(AxisRange::new(0, 255)),
            tilt_x: Some(AxisRange::new(-9000, 9000)),
            tilt_y: Some(AxisRange::new(-9000, 9000)),
        }
    }

    /// Build the device -> canonical page calibration implied by these axes.
    pub fn calibration(&self, orientation: Orientation, page: PageGeometry) -> DeviceCalibration {
        DeviceCalibration {
            device_min_x: self.x.min as f32,
            device_max_x: self.x.max as f32,
            device_min_y: self.y.min as f32,
            device_max_y: self.y.max as f32,
            orientation,
            page,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notebook_core::DevicePoint;

    #[test]
    fn normalizes_within_range() {
        let a = AxisRange::new(0, 4095);
        assert_eq!(a.normalize(0), 0.0);
        assert_eq!(a.normalize(4095), 1.0);
        assert!((a.normalize(2047) - 0.5).abs() < 0.001);
    }

    #[test]
    fn normalizes_non_zero_minimum() {
        let a = AxisRange::new(100, 300);
        assert_eq!(a.normalize(100), 0.0);
        assert_eq!(a.normalize(300), 1.0);
        assert_eq!(a.normalize(200), 0.5);
    }

    #[test]
    fn clamps_out_of_range_values() {
        let a = AxisRange::new(0, 100);
        assert_eq!(a.normalize(-50), 0.0);
        assert_eq!(a.normalize(5000), 1.0);
    }

    #[test]
    fn degenerate_range_is_zero_not_nan() {
        let a = AxisRange::new(7, 7);
        assert_eq!(a.normalize(7), 0.0);
        assert!(!a.normalize(9).is_nan());
    }

    #[test]
    fn bipolar_tilt_maps_zero_to_midpoint() {
        let a = AxisRange::new(-9000, 9000);
        assert!((a.normalize_bipolar(0)).abs() < 1e-6);
        assert!((a.normalize_bipolar(9000) - 1.0).abs() < 1e-6);
        assert!((a.normalize_bipolar(-9000) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn calibration_uses_device_reported_ranges() {
        let axes = StylusAxes::new(
            AxisRange::new(0, 20966),
            AxisRange::new(0, 15725),
            Some(AxisRange::new(0, 4095)),
        );
        let cal = axes.calibration(Orientation::Portrait0, PageGeometry::new(1000.0, 1400.0));
        assert_eq!(cal.device_max_x, 20966.0);
        assert_eq!(cal.device_max_y, 15725.0);
        let p = cal.to_page(DevicePoint::new(20966.0, 15725.0));
        assert_eq!(p.x, 1000.0);
        assert_eq!(p.y, 1400.0);
    }

    #[test]
    fn pressure_may_be_absent() {
        let axes = StylusAxes::synthetic(100, 100, None);
        assert!(axes.pressure.is_none());
    }

    #[test]
    fn scribe_wacom_confirmed_matches_hardware() {
        let a = StylusAxes::scribe_wacom_confirmed();
        assert_eq!(a.x.max, 15_624);
        assert_eq!(a.y.max, 20_832);
        assert_eq!(a.pressure.unwrap().max, 4095);
        assert_eq!(a.distance.unwrap().max, 255);
    }
}
