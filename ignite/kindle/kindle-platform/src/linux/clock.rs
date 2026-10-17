//! `CLOCK_MONOTONIC` clock, matching what `EVIOCSCLOCKID` puts on the input node.

use crate::time::{ClockDomain, MonotonicClock, TimestampUs};

/// Reads `clock_gettime(CLOCK_MONOTONIC)`.
///
/// This is the only clock whose values may be subtracted from evdev kernel timestamps, and then
/// only on a node where `EVIOCSCLOCKID(CLOCK_MONOTONIC)` succeeded.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxMonotonicClock;

impl MonotonicClock for LinuxMonotonicClock {
    fn now_us(&self) -> TimestampUs {
        // Zeroed rather than a struct literal: `timespec` gains padding fields on some
        // targets, and this build must work for whichever ABI the Scribe turns out to use.
        let mut ts: libc::timespec = unsafe { core::mem::zeroed() };
        let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        if rc != 0 {
            return 0;
        }
        // tv_sec/tv_nsec are i32 on armv7hf and i64 on x86_64; cast is intentional.
        #[allow(clippy::unnecessary_cast)]
        let sec = ts.tv_sec as i64;
        #[allow(clippy::unnecessary_cast)]
        let nsec = ts.tv_nsec as i64;
        sec.saturating_mul(1_000_000)
            .saturating_add(nsec / 1_000)
            .max(0) as TimestampUs
    }

    fn domain(&self) -> ClockDomain {
        ClockDomain::LinuxMonotonic
    }
}
