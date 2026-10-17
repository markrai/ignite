//! Cold-launch phase timestamps (`launch_ms phase=... ms=N` from process entry).

use std::sync::OnceLock;
use std::time::Instant;

static T0: OnceLock<Instant> = OnceLock::new();

/// Mark a launch phase. The first call (typically process entry) is `ms=0`.
pub fn mark(phase: &str) {
    let t0 = T0.get_or_init(Instant::now);
    println!("launch_ms phase={phase} ms={}", t0.elapsed().as_millis());
}

/// Milliseconds since the first [`mark`] (or now, if none yet).
#[allow(dead_code)]
pub fn elapsed_ms() -> u128 {
    T0.get_or_init(Instant::now).elapsed().as_millis()
}

/// Process-entry origin used for launch-relative timestamps.
pub fn origin() -> Instant {
    *T0.get_or_init(Instant::now)
}

/// Launch-relative ms of `at` (0 if `at` precedes process origin).
pub fn ms_at(at: Instant) -> u64 {
    saturating_ms_since(at, origin())
}

/// `at.saturating_duration_since(origin)` in whole milliseconds.
pub fn saturating_ms_since(at: Instant, origin: Instant) -> u64 {
    at.saturating_duration_since(origin).as_millis() as u64
}

/// True when `at` is strictly before `origin` (the FingerTracker saturation case).
pub fn is_before(at: Instant, origin: Instant) -> bool {
    at < origin
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn pre_session_start_saturates_tracker_now_ms_to_zero() {
        let process = Instant::now();
        let pump_down = process + Duration::from_millis(400);
        let session_start = process + Duration::from_millis(870);
        assert!(is_before(pump_down, session_start));
        assert_eq!(saturating_ms_since(pump_down, session_start), 0);
        assert_eq!(saturating_ms_since(pump_down, process), 400);
        let pump_up = process + Duration::from_millis(600);
        assert_eq!(saturating_ms_since(pump_up, session_start), 0);
        let dur = saturating_ms_since(pump_up, session_start)
            .saturating_sub(saturating_ms_since(pump_down, session_start));
        assert_eq!(dur, 0, "saturated Down and Up yield dur_ms=0");
    }

    #[test]
    fn post_session_start_is_not_saturated() {
        let process = Instant::now();
        let session_start = process + Duration::from_millis(870);
        let pump_up = process + Duration::from_millis(4550);
        assert!(!is_before(pump_up, session_start));
        assert_eq!(saturating_ms_since(pump_up, session_start), 3680);
        assert_eq!(saturating_ms_since(pump_up, process), 4550);
    }
}
