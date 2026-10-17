//! A clock a test drives by hand, so latency assertions are deterministic.

use std::cell::Cell;

use crate::time::{ClockDomain, MonotonicClock, TimestampUs};

/// Manually advanced clock in the [`ClockDomain::Synthetic`] domain.
///
/// Events carrying `ClockDomain::Synthetic` kernel timestamps are comparable with it, which lets
/// host tests exercise the dispatch-latency path that on hardware requires
/// `EVIOCSCLOCKID(CLOCK_MONOTONIC)`.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: Cell<TimestampUs>,
    /// Added to `now` on every read, to model time passing inside an operation.
    auto_advance: Cell<TimestampUs>,
}

impl ManualClock {
    pub fn new(start_us: TimestampUs) -> Self {
        Self {
            now: Cell::new(start_us),
            auto_advance: Cell::new(0),
        }
    }

    /// Make every subsequent `now_us()` read advance the clock by `step_us` afterwards.
    pub fn with_auto_advance(self, step_us: TimestampUs) -> Self {
        self.auto_advance.set(step_us);
        self
    }

    pub fn set(&self, t_us: TimestampUs) {
        self.now.set(t_us);
    }

    pub fn advance(&self, delta_us: TimestampUs) {
        self.now.set(self.now.get().saturating_add(delta_us));
    }
}

impl MonotonicClock for ManualClock {
    fn now_us(&self) -> TimestampUs {
        let t = self.now.get();
        let step = self.auto_advance.get();
        if step > 0 {
            self.now.set(t.saturating_add(step));
        }
        t
    }

    fn domain(&self) -> ClockDomain {
        ClockDomain::Synthetic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_only_moves_when_told() {
        let c = ManualClock::new(100);
        assert_eq!(c.now_us(), 100);
        assert_eq!(c.now_us(), 100);
        c.advance(50);
        assert_eq!(c.now_us(), 150);
        c.set(7);
        assert_eq!(c.now_us(), 7);
    }

    #[test]
    fn auto_advance_models_elapsed_work() {
        let c = ManualClock::new(0).with_auto_advance(10);
        assert_eq!(c.now_us(), 0);
        assert_eq!(c.now_us(), 10);
        assert_eq!(c.now_us(), 20);
    }

    #[test]
    fn manual_clock_is_comparable_with_synthetic_kernel_timestamps() {
        let c = ManualClock::new(0);
        assert!(c.domain().comparable_with(ClockDomain::Synthetic));
        assert!(!c.domain().comparable_with(ClockDomain::Unknown));
    }
}
