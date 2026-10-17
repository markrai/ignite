//! Per-stroke pressure sample summary (Gate F6 characterization / `--trace-pressure`).

use notebook_core::Stroke;

/// Compact pressure stats for one completed stroke. No per-Move logging.
#[derive(Clone, Debug)]
pub struct PressureStrokeSummary {
    pub samples: u64,
    pub missing: u64,
    pub min: Option<f32>,
    pub max: Option<f32>,
    pub mean: Option<f32>,
    pub p50: Option<f32>,
    pub p95: Option<f32>,
}

impl PressureStrokeSummary {
    pub fn from_stroke(stroke: &Stroke) -> Self {
        let mut present: Vec<f32> = Vec::new();
        let mut missing = 0u64;
        for p in &stroke.points {
            match p.pressure {
                Some(v) if v.is_finite() => present.push(v),
                Some(_) | None => missing += 1,
            }
        }
        let samples = present.len() as u64;
        if present.is_empty() {
            return Self {
                samples: 0,
                missing,
                min: None,
                max: None,
                mean: None,
                p50: None,
                p95: None,
            };
        }
        present.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let min = present[0];
        let max = *present.last().unwrap();
        let sum: f32 = present.iter().sum();
        let mean = sum / present.len() as f32;
        let p50 = percentile_sorted(&present, 0.50);
        let p95 = percentile_sorted(&present, 0.95);
        Self {
            samples,
            missing,
            min: Some(min),
            max: Some(max),
            mean: Some(mean),
            p50: Some(p50),
            p95: Some(p95),
        }
    }

    pub fn log_line(&self, stroke_id: &str) -> String {
        match (self.min, self.max, self.mean, self.p50, self.p95) {
            (Some(min), Some(max), Some(mean), Some(p50), Some(p95)) => format!(
                "pressure_stroke id={stroke_id} pressure_samples={} pressure_missing={} \
                 pressure_min={min:.4} pressure_max={max:.4} pressure_mean={mean:.4} \
                 pressure_p50={p50:.4} pressure_p95={p95:.4}",
                self.samples, self.missing
            ),
            _ => format!(
                "pressure_stroke id={stroke_id} pressure_samples=0 pressure_missing={} \
                 pressure_min=none pressure_max=none pressure_mean=none pressure_p50=none pressure_p95=none",
                self.missing
            ),
        }
    }
}

fn percentile_sorted(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let idx = ((sorted.len() as f32 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use notebook_core::{Point, StrokeId};

    #[test]
    fn summarizes_present_pressures() {
        let stroke = Stroke {
            id: StrokeId::new("s-a"),
            points: vec![
                Point::new(0.0, 0.0).with_pressure(0.1),
                Point::new(1.0, 0.0).with_pressure(0.5),
                Point::new(2.0, 0.0).with_pressure(0.9),
                Point::new(3.0, 0.0), // missing
            ],
            brush: None,
            finished: true,
        };
        let s = PressureStrokeSummary::from_stroke(&stroke);
        assert_eq!(s.samples, 3);
        assert_eq!(s.missing, 1);
        assert!((s.min.unwrap() - 0.1).abs() < 1e-5);
        assert!((s.max.unwrap() - 0.9).abs() < 1e-5);
    }
}
