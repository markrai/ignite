//! Latency metrics that mean what they say.
//!
//! # What is and is not measurable from this process
//!
//! | Metric | Measured between | Honest meaning |
//! |--------|------------------|----------------|
//! | `dispatch` | kernel event timestamp -> our first instruction for that event | how long the sample waited in the kernel and our read loop. **Only recorded when the kernel timestamps and our clock are provably the same [`ClockDomain`]** |
//! | `draw` | before -> after the rasterisation call | CPU time spent turning the segment into pixels |
//! | `refresh_submit` | before -> after the refresh call returns | time to hand the region to the display backend (on hardware: the ioctl round trip) |
//! | `draw_plus_submit` | before draw -> after refresh returns | total in-process work per sample |
//!
//! There is deliberately **no** end-to-end "pen to pigment" metric. A refresh ioctl returning
//! says the request was accepted, not that the panel has finished changing state; measuring
//! actual ink appearance needs external evidence (high-speed camera, or a kernel/driver
//! completion signal we have not characterised). Claiming otherwise would be a fabricated number.

use kindle_platform::ClockDomain;

/// Min / mean / max over one metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetricSummary {
    pub count: usize,
    pub min_us: u64,
    pub max_us: u64,
    pub mean_us: u64,
}

/// One named latency series.
#[derive(Clone, Debug, Default)]
pub struct Metric {
    pub samples: Vec<u64>,
}

impl Metric {
    pub fn record(&mut self, us: u64) {
        self.samples.push(us);
    }

    pub fn summary(&self) -> Option<MetricSummary> {
        if self.samples.is_empty() {
            return None;
        }
        let sum: u128 = self.samples.iter().map(|&v| v as u128).sum();
        Some(MetricSummary {
            count: self.samples.len(),
            min_us: *self.samples.iter().min().unwrap(),
            max_us: *self.samples.iter().max().unwrap(),
            mean_us: (sum / self.samples.len() as u128) as u64,
        })
    }

    fn render(&self, name: &str) -> String {
        match self.summary() {
            Some(s) => format!(
                "{name}[n={} min={} mean={} max={}]",
                s.count, s.min_us, s.mean_us, s.max_us
            ),
            None => format!("{name}[n=0]"),
        }
    }
}

/// Why dispatch latency could not be measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchAvailability {
    /// No event carrying a kernel timestamp has been seen yet.
    NoKernelTimestamps,
    /// Kernel timestamps exist but are in a different clock domain than our clock.
    ClockDomainMismatch {
        kernel: ClockDomain,
        local: ClockDomain,
    },
    /// Measured.
    Available,
}

/// Counters plus the four latency series.
#[derive(Clone, Debug, Default)]
pub struct LatencyStats {
    pub events: u64,
    pub hovers_ignored: u64,
    pub moves_dropped_without_contact: u64,
    pub stream_gaps: u64,
    pub strokes_finished: u64,
    pub strokes_cancelled: u64,
    pub refreshes: u64,
    /// Kernel event timestamp -> start of our processing for that event.
    pub dispatch: Metric,
    /// Rasterisation call duration.
    pub draw: Metric,
    /// Refresh call duration (submission only).
    pub refresh_submit: Metric,
    /// Draw start -> refresh call returned.
    pub draw_plus_submit: Metric,
    dispatch_state: Option<DispatchAvailability>,
}

impl LatencyStats {
    pub fn note_dispatch_unavailable(&mut self, why: DispatchAvailability) {
        // Only the first reason is kept; it does not change within a run.
        if self.dispatch_state.is_none() {
            self.dispatch_state = Some(why);
        }
    }

    pub fn note_dispatch_available(&mut self) {
        self.dispatch_state = Some(DispatchAvailability::Available);
    }

    pub fn dispatch_availability(&self) -> DispatchAvailability {
        self.dispatch_state
            .unwrap_or(DispatchAvailability::NoKernelTimestamps)
    }

    /// Human-readable summary. Unavailable metrics say so instead of showing a zero.
    pub fn summary_line(&self) -> String {
        let dispatch = match (self.dispatch.summary(), self.dispatch_availability()) {
            (Some(s), _) => format!(
                "dispatch[n={} min={} mean={} max={}]",
                s.count, s.min_us, s.mean_us, s.max_us
            ),
            (None, DispatchAvailability::ClockDomainMismatch { kernel, local }) => {
                format!("dispatch[unavailable: kernel clock {kernel:?} != local clock {local:?}]")
            }
            (None, _) => "dispatch[unavailable: no kernel timestamps]".to_string(),
        };
        format!(
            "events={} hover_ignored={} move_dropped={} stream_gaps={} strokes={} cancelled={} refreshes={} \
             {dispatch} {} {} {} (refresh = submitted, not pigment settled)",
            self.events,
            self.hovers_ignored,
            self.moves_dropped_without_contact,
            self.stream_gaps,
            self.strokes_finished,
            self.strokes_cancelled,
            self.refreshes,
            self.draw.render("draw_us"),
            self.refresh_submit.render("refresh_submit_us"),
            self.draw_plus_submit.render("draw_plus_submit_us"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_metric_has_no_summary() {
        assert!(Metric::default().summary().is_none());
    }

    #[test]
    fn metric_summarises_min_mean_max() {
        let mut m = Metric::default();
        m.record(10);
        m.record(20);
        m.record(30);
        let s = m.summary().unwrap();
        assert_eq!(s.count, 3);
        assert_eq!(s.min_us, 10);
        assert_eq!(s.mean_us, 20);
        assert_eq!(s.max_us, 30);
    }

    #[test]
    fn summary_line_says_unavailable_rather_than_zero() {
        let mut s = LatencyStats::default();
        s.note_dispatch_unavailable(DispatchAvailability::ClockDomainMismatch {
            kernel: ClockDomain::LinuxRealtime,
            local: ClockDomain::LinuxMonotonic,
        });
        let line = s.summary_line();
        assert!(line.contains("dispatch[unavailable"));
        assert!(line.contains("LinuxRealtime"));
        assert!(!line.contains("dispatch[n=0 min=0"));
    }

    #[test]
    fn summary_line_never_claims_pigment_completion() {
        let line = LatencyStats::default().summary_line();
        assert!(line.contains("submitted, not pigment settled"));
    }

    #[test]
    fn first_unavailability_reason_is_kept() {
        let mut s = LatencyStats::default();
        s.note_dispatch_unavailable(DispatchAvailability::NoKernelTimestamps);
        s.note_dispatch_unavailable(DispatchAvailability::ClockDomainMismatch {
            kernel: ClockDomain::Unknown,
            local: ClockDomain::HostInstant,
        });
        assert_eq!(
            s.dispatch_availability(),
            DispatchAvailability::NoKernelTimestamps
        );
    }
}
