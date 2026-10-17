//! Gate F0/E3 DU cadence policy + live app default.

use kindle_platform::DirtyRegion;

/// Historical Gate F0 / E2 baseline DU interval (not the live app default).
pub const F0_DU_INTERVAL_US: u64 = 25_000;

/// Live `scribe-app` default when `--du-interval-us` is omitted (E3-stable 8 ms).
pub const DEFAULT_LIVE_DU_INTERVAL_US: u64 = 8_000;

/// Allowed developer `--du-interval-us` overrides (exact match only).
pub const ALLOWED_DU_INTERVALS_US: &[u64] = &[25_000, 16_000, 12_000, 8_000];

/// Parse `--du-interval-us`. Omitted → live default (`8000`, `override=false`).
pub fn parse_du_interval_us(args: &[String]) -> Result<(u64, bool), String> {
    let explicit = args.iter().any(|a| a == "--du-interval-us");
    let Some(raw) = args
        .windows(2)
        .find(|w| w[0] == "--du-interval-us")
        .map(|w| w[1].as_str())
    else {
        return Ok((DEFAULT_LIVE_DU_INTERVAL_US, false));
    };
    let Ok(v) = raw.parse::<u64>() else {
        return Err(format!(
            "REFUSE DU INTERVAL: allowed values are 25000,16000,12000,8000 us (got {raw:?})"
        ));
    };
    if !ALLOWED_DU_INTERVALS_US.contains(&v) {
        return Err(format!(
            "REFUSE DU INTERVAL: allowed values are 25000,16000,12000,8000 us (got {v})"
        ));
    }
    Ok((v, explicit))
}

pub fn du_target_hz(interval_us: u64) -> f64 {
    if interval_us == 0 {
        0.0
    } else {
        1_000_000.0 / interval_us as f64
    }
}

/// Accumulates pending dirty rectangles and decides when to submit a partial DU refresh.
#[derive(Clone, Debug, Default)]
pub struct StreamRefreshPolicy {
    pub interval_us: u64,
    pending: Option<DirtyRegion>,
    last_submit_us: Option<u64>,
    pub max_pending_width: u32,
    pub max_pending_height: u32,
}

impl StreamRefreshPolicy {
    pub fn new(interval_us: u64) -> Self {
        Self {
            interval_us,
            ..Default::default()
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn f0_default() -> Self {
        Self::new(F0_DU_INTERVAL_US)
    }

    pub fn merge(&mut self, dirty: DirtyRegion) {
        self.pending = Some(match self.pending {
            Some(p) => p.union(dirty),
            None => dirty,
        });
        if let Some(p) = self.pending {
            self.max_pending_width = self.max_pending_width.max(p.width);
            self.max_pending_height = self.max_pending_height.max(p.height);
        }
    }

    pub fn should_submit(&self, now_us: u64) -> bool {
        if self.pending.is_none() {
            return false;
        }
        match self.last_submit_us {
            None => true,
            Some(t) => now_us.saturating_sub(t) >= self.interval_us,
        }
    }

    pub fn take_if_due(&mut self, now_us: u64) -> Option<DirtyRegion> {
        if !self.should_submit(now_us) {
            return None;
        }
        self.last_submit_us = Some(now_us);
        self.pending.take()
    }

    pub fn force_take(&mut self, now_us: u64) -> Option<DirtyRegion> {
        self.pending?;
        self.last_submit_us = Some(now_us);
        self.pending.take()
    }

    pub fn clear(&mut self) {
        self.pending = None;
        self.last_submit_us = None;
    }
}

pub fn dist_min_median_p90_max(samples: &[u64]) -> Option<(u64, u64, u64, u64)> {
    if samples.is_empty() {
        return None;
    }
    let mut v = samples.to_vec();
    v.sort_unstable();
    let n = v.len();
    let min = v[0];
    let max = v[n - 1];
    let median = v[n / 2];
    let p90_idx = ((n as f64 - 1.0) * 0.90).round() as usize;
    let p90 = v[p90_idx.min(n - 1)];
    Some((min, median, p90, max))
}

pub fn format_dist(name: &str, samples: &[u64]) -> String {
    match dist_min_median_p90_max(samples) {
        Some((min, med, p90, max)) => format!(
            "{name}[n={} min={} median={} p90={} max={}]",
            samples.len(),
            min,
            med,
            p90,
            max
        ),
        None => format!("{name}[n=0]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_f0_baseline_remains_25ms() {
        assert_eq!(F0_DU_INTERVAL_US, 25_000);
        assert_eq!(StreamRefreshPolicy::f0_default().interval_us, 25_000);
    }

    #[test]
    fn omitted_cadence_is_live_8ms() {
        let (v, o) = parse_du_interval_us(&[]).unwrap();
        assert_eq!(v, DEFAULT_LIVE_DU_INTERVAL_US);
        assert_eq!(v, 8_000);
        assert!(!o);
    }

    #[test]
    fn explicit_allowed_intervals() {
        for (raw, expect) in [
            ("8000", 8_000),
            ("12000", 12_000),
            ("16000", 16_000),
            ("25000", 25_000),
        ] {
            let args = vec!["--du-interval-us".into(), raw.into()];
            let (v, o) = parse_du_interval_us(&args).unwrap();
            assert_eq!(v, expect, "raw={raw}");
            assert!(o, "raw={raw}");
        }
    }

    #[test]
    fn refuse_unsupported_values() {
        for bad in ["7000", "4000", "0", "fast", ""] {
            let args = vec!["--du-interval-us".into(), bad.into()];
            assert!(parse_du_interval_us(&args).is_err(), "bad={bad:?}");
        }
    }
}
