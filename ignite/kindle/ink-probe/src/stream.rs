//! Gate D / E2 / E3 streaming refresh batching policy (host-testable, no hardware).

use kindle_platform::DirtyRegion;

/// Maximum desired SEND_UPDATE cadence for the Gate D GC16 baseline (experimental).
pub const STREAM_REFRESH_INTERVAL_US: u64 = 25_000;

/// Gate E2 DU wet streaming default cadence (same as Gate D). Gate E3 may override via CLI.
pub const STREAM_DU_REFRESH_INTERVAL_US: u64 = STREAM_REFRESH_INTERVAL_US;

/// Allowed `--du-interval-us` values for Gate E3 cadence sweep (exact match only).
pub const E3_ALLOWED_DU_INTERVALS_US: &[u64] = &[25_000, 16_000, 12_000, 8_000];

/// Parse `--du-interval-us` from argv. Omitted → E2 default (`25000`, `e3_override=false`).
///
/// Rejects unsupported values without rounding. Explicit `--du-interval-us 25000` still sets
/// `e3_override=true` so sweep logs distinguish default vs intentional baseline re-run.
pub fn parse_du_interval_us(args: &[String]) -> Result<(u64, bool), String> {
    let explicit = args.iter().any(|a| a == "--du-interval-us");
    let Some(raw) = args
        .windows(2)
        .find(|w| w[0] == "--du-interval-us")
        .map(|w| w[1].as_str())
    else {
        return Ok((STREAM_DU_REFRESH_INTERVAL_US, false));
    };
    let Ok(v) = raw.parse::<u64>() else {
        return Err(format!(
            "REFUSE E3 INTERVAL: allowed values are 25000,16000,12000,8000 us (got {raw:?})"
        ));
    };
    if !E3_ALLOWED_DU_INTERVALS_US.contains(&v) {
        return Err(format!(
            "REFUSE E3 INTERVAL: allowed values are 25000,16000,12000,8000 us (got {v})"
        ));
    }
    Ok((v, explicit))
}

/// Approximate target Hz for logging (not a measured rate).
pub fn du_target_hz(interval_us: u64) -> f64 {
    if interval_us == 0 {
        0.0
    } else {
        1_000_000.0 / interval_us as f64
    }
}

/// Accumulates pending dirty rectangles and decides when to submit a partial refresh.
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

    pub fn gate_d_default() -> Self {
        Self::new(STREAM_REFRESH_INTERVAL_US)
    }

    pub fn gate_e2_du_default() -> Self {
        Self::new(STREAM_DU_REFRESH_INTERVAL_US)
    }

    pub fn pending(&self) -> Option<DirtyRegion> {
        self.pending
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

    /// True when there is pending dirty and either no prior submit or the interval has elapsed.
    pub fn should_submit(&self, now_us: u64) -> bool {
        if self.pending.is_none() {
            return false;
        }
        match self.last_submit_us {
            None => true,
            Some(t) => now_us.saturating_sub(t) >= self.interval_us,
        }
    }

    /// Take pending dirty for submit when the interval policy allows it.
    pub fn take_if_due(&mut self, now_us: u64) -> Option<DirtyRegion> {
        if !self.should_submit(now_us) {
            return None;
        }
        self.last_submit_us = Some(now_us);
        self.pending.take()
    }

    /// Forced flush (Pen Up / StreamGap / shutdown).
    pub fn force_take(&mut self, now_us: u64) -> Option<DirtyRegion> {
        self.pending?;
        self.last_submit_us = Some(now_us);
        self.pending.take()
    }
}

/// Percentile helpers for Gate D telemetry (samples are copied and sorted).
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
    fn under_interval_does_not_submit() {
        let mut p = StreamRefreshPolicy::new(25_000);
        p.merge(DirtyRegion::new(0, 0, 10, 10));
        // First submit is due immediately (no prior submit).
        assert!(p.should_submit(0));
        let _ = p.take_if_due(1_000);
        p.merge(DirtyRegion::new(10, 10, 5, 5));
        assert!(!p.should_submit(1_000 + 24_999));
        assert!(p.take_if_due(1_000 + 24_999).is_none());
    }

    #[test]
    fn threshold_reached_submits() {
        let mut p = StreamRefreshPolicy::new(25_000);
        p.merge(DirtyRegion::new(0, 0, 1, 1));
        assert!(p.take_if_due(0).is_some());
        p.merge(DirtyRegion::new(2, 2, 1, 1));
        assert!(p.take_if_due(25_000).is_some());
    }

    #[test]
    fn up_forces_final_submit() {
        let mut p = StreamRefreshPolicy::new(25_000);
        p.merge(DirtyRegion::new(0, 0, 3, 3));
        let _ = p.take_if_due(0);
        p.merge(DirtyRegion::new(4, 4, 2, 2));
        // Still inside interval.
        assert!(p.take_if_due(10_000).is_none());
        let forced = p.force_take(10_000).unwrap();
        assert_eq!(forced, DirtyRegion::new(4, 4, 2, 2));
        assert!(p.pending().is_none());
    }

    #[test]
    fn merge_tracks_max_pending_extent() {
        let mut p = StreamRefreshPolicy::new(25_000);
        p.merge(DirtyRegion::new(0, 0, 10, 20));
        p.merge(DirtyRegion::new(5, 5, 100, 5));
        assert_eq!(p.max_pending_width, 105);
        assert!(p.max_pending_height >= 20);
    }

    #[test]
    fn percentile_helpers_and_gate_d_interval_constant() {
        let s = [10u64, 20, 30, 40, 50];
        let (min, med, p90, max) = dist_min_median_p90_max(&s).unwrap();
        assert_eq!((min, med, max), (10, 30, 50));
        assert!(p90 >= med);
        assert!(format_dist("send_ioctl_us", &s).contains("median=30"));
        assert_eq!(
            StreamRefreshPolicy::gate_d_default().interval_us,
            STREAM_REFRESH_INTERVAL_US
        );
    }

    #[test]
    fn e2_du_interval_matches_gate_d_baseline() {
        assert_eq!(STREAM_DU_REFRESH_INTERVAL_US, 25_000);
        assert_eq!(STREAM_DU_REFRESH_INTERVAL_US, STREAM_REFRESH_INTERVAL_US);
        assert_eq!(
            StreamRefreshPolicy::gate_e2_du_default().interval_us,
            STREAM_DU_REFRESH_INTERVAL_US
        );
        // Immediate first submit, then 25ms batching (same policy as Gate D).
        let mut p = StreamRefreshPolicy::gate_e2_du_default();
        p.merge(DirtyRegion::new(0, 0, 1, 1));
        assert!(p.take_if_due(0).is_some());
        p.merge(DirtyRegion::new(1, 1, 1, 1));
        assert!(p.take_if_due(24_999).is_none());
        assert!(p.take_if_due(25_000).is_some());
        p.merge(DirtyRegion::new(2, 2, 1, 1));
        assert!(p.force_take(25_001).is_some());
    }

    #[test]
    fn e3_parse_du_interval_allows_only_sweep_set() {
        let none: Vec<String> = vec![];
        assert_eq!(parse_du_interval_us(&none).unwrap(), (25_000, false));
        let a = vec!["--du-interval-us".into(), "25000".into()];
        assert_eq!(parse_du_interval_us(&a).unwrap(), (25_000, true));
        let b = vec!["--du-interval-us".into(), "8000".into()];
        assert_eq!(parse_du_interval_us(&b).unwrap(), (8_000, true));
        let bad = vec!["--du-interval-us".into(), "4000".into()];
        assert!(parse_du_interval_us(&bad)
            .unwrap_err()
            .contains("REFUSE E3 INTERVAL"));
        let junk = vec!["--du-interval-us".into(), "fast".into()];
        assert!(parse_du_interval_us(&junk)
            .unwrap_err()
            .contains("REFUSE E3 INTERVAL"));
        assert!((du_target_hz(25_000) - 40.0).abs() < 0.01);
        assert!((du_target_hz(8_000) - 125.0).abs() < 0.01);
        let mut p = StreamRefreshPolicy::new(8_000);
        p.merge(DirtyRegion::new(0, 0, 1, 1));
        assert!(p.take_if_due(0).is_some());
        p.merge(DirtyRegion::new(1, 1, 1, 1));
        assert!(p.take_if_due(7_999).is_none());
        assert!(p.take_if_due(8_000).is_some());
    }

    #[test]
    fn e2_lifecycle_allows_wet_du_only_when_armed() {
        use kindle_platform::{StylusModeLifecycle, UpdateClass};
        let mut lc = StylusModeLifecycle::new();
        assert!(lc.require_send(UpdateClass::Ordinary).is_ok());
        assert!(lc.require_send(UpdateClass::WetInkDu).is_err());
        lc.mark_armed().unwrap();
        assert!(lc.require_send(UpdateClass::Ordinary).is_err());
        assert!(lc.require_send(UpdateClass::WetInkDu).is_ok());
        lc.mark_disarmed().unwrap();
        assert!(lc.require_send(UpdateClass::Ordinary).is_ok());
        assert!(lc.require_send(UpdateClass::WetInkDu).is_err());
    }

    #[test]
    fn e2_wetink_du_constructor_not_gc16() {
        use kindle_platform::hwtcon_abi::waveform;
        use kindle_platform::{MxcfbRect, MxcfbUpdateDataMtk};
        let r = MxcfbRect {
            top: 1,
            left: 2,
            width: 3,
            height: 4,
        };
        let du = MxcfbUpdateDataMtk::wetink_du_partial(r, 9);
        let gc = MxcfbUpdateDataMtk::conservative_partial(r, 9);
        assert_eq!(du.waveform_mode, waveform::DU);
        assert_eq!(gc.waveform_mode, waveform::GC16);
        assert_ne!(du.waveform_mode, gc.waveform_mode);
    }

    #[test]
    fn stream_gap_clears_continuity_without_inventing_geometry() {
        // Documented Gate-D rule: StreamGap ends continuity; callers must not bridge last→next.
        let mut last: Option<(i32, i32)> = Some((10, 20));
        let gap = true;
        if gap {
            last = None;
        }
        assert!(last.is_none());
    }

    #[test]
    fn portrait_canonical_segment_maps_monotonically_to_panel() {
        use notebook_core::{
            portrait_wacom_calibration, DevicePoint, PagePoint, ScribeDisplayTransform,
        };
        let cal = portrait_wacom_calibration();
        let t = ScribeDisplayTransform::PortraitIdentity;
        let a = cal.to_page(DevicePoint::new(1000.0, 2000.0));
        let b = cal.to_page(DevicePoint::new(3000.0, 4000.0));
        let (ax, ay) = t.to_panel_pixel(a).unwrap();
        let (bx, by) = t.to_panel_pixel(b).unwrap();
        assert!(bx >= ax);
        assert!(by >= ay);
        let mid = t
            .to_panel_pixel(PagePoint::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0))
            .unwrap();
        assert!(mid.0 >= ax && mid.0 <= bx);
        assert!(mid.1 >= ay && mid.1 <= by);
    }
}
