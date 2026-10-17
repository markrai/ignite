//! Gate F6 sealed pressure → brush-radius mapping.
//!
//! Quantized domain: `pressure_q` integer `0..=4095` from semantic `0.0..=1.0`.
//! Thresholds from physical characterization (`f6-pressure-characterize`):
//!   light mean ≈ 0.02–0.06, normal ≈ 0.59–0.67, firm ≈ 0.84–0.92.

/// Quantization span (matches Wacom ABS_PRESSURE max on Scribe Gen1).
pub const PRESSURE_Q_MAX: u16 = 4095;

/// Curve semantic id sealed into RenderProfile.
pub const PRESSURE_CURVE_ID_V1: &str = "scribe-pressure-q4095-r1-4-v1";

/// Stroke style brush id for new F6 pressure-width strokes.
pub const STYLE_PRESSURE_WIDTH_V1: &str = "pressure-width-v1";

/// Legacy fixed-width style (loaded v1 stroke records / explicit).
pub const STYLE_LEGACY_FIXED_V1: &str = "legacy-fixed-v1";

/// Sealed integer thresholds (inclusive max for each radius band).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PressureCurveV1 {
    /// q in `0..=q_r1_max` → radius 1
    pub q_r1_max: u16,
    /// q in `(q_r1_max+1)..=q_r2_max` → radius 2
    pub q_r2_max: u16,
    /// q in `(q_r2_max+1)..=q_r3_max` → radius 3
    /// above → radius 4
    pub q_r3_max: u16,
    pub min_radius: u32,
    pub max_radius: u32,
    pub missing_fallback_radius: u32,
}

impl PressureCurveV1 {
    /// Physical-characterization-derived sealed curve.
    pub const fn sealed_v1() -> Self {
        Self {
            // ~0.20 — light strokes (char mean ≤0.06) stay thin
            q_r1_max: 820,
            // ~0.70 — normal handwriting (char mean ~0.59–0.67)
            q_r2_max: 2866,
            // ~0.90 — firm (char mean ~0.84–0.92); above → r4
            q_r3_max: 3685,
            min_radius: 1,
            max_radius: 4,
            missing_fallback_radius: 2,
        }
    }

    pub fn radius_for_q(self, q: u16) -> u32 {
        if q <= self.q_r1_max {
            1
        } else if q <= self.q_r2_max {
            2
        } else if q <= self.q_r3_max {
            3
        } else {
            4
        }
    }

    pub fn radius_for_pressure(self, pressure: Option<f32>) -> u32 {
        match pressure {
            Some(p) if p.is_finite() => self.radius_for_q(quantize_pressure(p)),
            _ => self.missing_fallback_radius,
        }
    }
}

/// Clamp/round semantic pressure `0.0..=1.0` into `0..=4095`.
/// Non-finite → treated as missing by callers (this returns 0 only for finite-after-clamp path).
pub fn quantize_pressure(p: f32) -> u16 {
    if !p.is_finite() {
        return 0;
    }
    let c = p.clamp(0.0, 1.0);
    ((c * f32::from(PRESSURE_Q_MAX)).round() as u32).min(u32::from(PRESSURE_Q_MAX)) as u16
}

/// Deterministic integer lerp of quantized pressure along a Bresenham step index.
pub fn interpolate_q(q0: u16, q1: u16, step: u32, steps: u32) -> u16 {
    if steps == 0 {
        return q1;
    }
    let a = i32::from(q0);
    let b = i32::from(q1);
    let t = step as i32;
    let n = steps as i32;
    let v = a + ((b - a) * t) / n;
    v.clamp(0, i32::from(PRESSURE_Q_MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_light_normal_firm() {
        let c = PressureCurveV1::sealed_v1();
        assert_eq!(c.radius_for_pressure(Some(0.05)), 1); // light
        assert_eq!(c.radius_for_pressure(Some(0.60)), 2); // normal
        assert_eq!(c.radius_for_pressure(Some(0.85)), 3); // firm
        assert_eq!(c.radius_for_pressure(Some(1.0)), 4); // very firm
        assert_eq!(c.radius_for_pressure(None), 2);
    }

    #[test]
    fn increasing_pressure_never_decreases_radius() {
        let c = PressureCurveV1::sealed_v1();
        let mut prev = 0u32;
        for q in 0..=PRESSURE_Q_MAX {
            let r = c.radius_for_q(q);
            assert!(r >= prev);
            prev = r;
        }
    }

    #[test]
    fn quantize_boundaries() {
        assert_eq!(quantize_pressure(0.0), 0);
        assert_eq!(quantize_pressure(1.0), PRESSURE_Q_MAX);
        assert_eq!(quantize_pressure(-1.0), 0);
        assert_eq!(quantize_pressure(2.0), PRESSURE_Q_MAX);
    }

    #[test]
    fn interpolate_endpoints() {
        assert_eq!(interpolate_q(0, 4095, 0, 10), 0);
        assert_eq!(interpolate_q(0, 4095, 10, 10), 4095);
    }
}
