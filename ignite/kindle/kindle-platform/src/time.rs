//! Monotonic timestamps for latency diagnostics.
//!
//! Latency numbers are only meaningful when the two timestamps being subtracted come from the
//! same clock. Every clock therefore advertises a [`ClockDomain`], and callers must refuse to
//! subtract across domains rather than silently producing a number.

use std::time::Instant;

pub type TimestampUs = u64;

/// Which clock a timestamp came from.
///
/// `Unknown` is the honest default for kernel timestamps we have not proven anything about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockDomain {
    /// Provenance unproven. Never subtract from anything.
    Unknown,
    /// `std::time::Instant` on this process. Monotonic but with an arbitrary, private epoch.
    HostInstant,
    /// Linux `CLOCK_MONOTONIC`. Shared with evdev timestamps only after a successful
    /// `EVIOCSCLOCKID(CLOCK_MONOTONIC)` on the input node.
    LinuxMonotonic,
    /// Linux `CLOCK_REALTIME`. The evdev default; wall-clock and steppable.
    LinuxRealtime,
    /// Synthetic clock driven by a test.
    Synthetic,
}

impl ClockDomain {
    /// True when a difference between two timestamps from `self` and `other` is meaningful.
    pub fn comparable_with(self, other: ClockDomain) -> bool {
        self != ClockDomain::Unknown && self == other
    }

    /// Whether a kernel/event timestamp from this domain may populate
    /// [`notebook_core::Point::t_us`], which is documented as monotonic.
    ///
    /// - [`Self::LinuxMonotonic`][] / [`Self::Synthetic`][]: yes
    /// - [`Self::LinuxRealtime`][]: no (wall-clock / steppable — must not be stored as monotonic)
    /// - [`Self::Unknown`][] / [`Self::HostInstant`][]: no
    pub fn may_populate_point_timestamp(self) -> bool {
        matches!(self, ClockDomain::LinuxMonotonic | ClockDomain::Synthetic)
    }
}

pub trait MonotonicClock {
    fn now_us(&self) -> TimestampUs;

    /// The domain of the values returned by [`MonotonicClock::now_us`].
    fn domain(&self) -> ClockDomain;
}

/// Host clock based on [`Instant`]. Monotonic, but its epoch is private to this process, so it
/// is not comparable with kernel event timestamps.
#[derive(Clone, Debug)]
pub struct HostClock {
    start: Instant,
}

impl HostClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for HostClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock for HostClock {
    fn now_us(&self) -> TimestampUs {
        self.start.elapsed().as_micros() as TimestampUs
    }

    fn domain(&self) -> ClockDomain {
        ClockDomain::HostInstant
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_domain_is_never_comparable() {
        assert!(!ClockDomain::Unknown.comparable_with(ClockDomain::Unknown));
        assert!(!ClockDomain::Unknown.comparable_with(ClockDomain::LinuxMonotonic));
        assert!(!ClockDomain::LinuxMonotonic.comparable_with(ClockDomain::Unknown));
    }

    #[test]
    fn distinct_domains_are_not_comparable() {
        assert!(!ClockDomain::LinuxMonotonic.comparable_with(ClockDomain::LinuxRealtime));
        assert!(!ClockDomain::HostInstant.comparable_with(ClockDomain::LinuxMonotonic));
    }

    #[test]
    fn same_known_domain_is_comparable() {
        assert!(ClockDomain::LinuxMonotonic.comparable_with(ClockDomain::LinuxMonotonic));
        assert!(ClockDomain::Synthetic.comparable_with(ClockDomain::Synthetic));
    }

    #[test]
    fn only_monotonic_domains_may_populate_point_timestamps() {
        assert!(ClockDomain::LinuxMonotonic.may_populate_point_timestamp());
        assert!(ClockDomain::Synthetic.may_populate_point_timestamp());
        assert!(!ClockDomain::LinuxRealtime.may_populate_point_timestamp());
        assert!(!ClockDomain::Unknown.may_populate_point_timestamp());
        assert!(!ClockDomain::HostInstant.may_populate_point_timestamp());
    }

    #[test]
    fn host_clock_advertises_its_private_epoch() {
        assert_eq!(HostClock::new().domain(), ClockDomain::HostInstant);
    }
}
