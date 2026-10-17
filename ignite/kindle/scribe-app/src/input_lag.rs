//! Low-overhead handwriting-latency instrumentation (no per-sample logging).
//!
//! Every stylus event carries its kernel `CLOCK_MONOTONIC` stamp; the runtime measures the
//! delay until the main loop dispatches it. A *stall* is a run of events whose dispatch delay is
//! at least [`STALL_US`]; only the first event of each run is reported, together with the most
//! recent main-thread blocking operation, so a stall can be attributed without flooding the log.

/// Dispatch delay at which an event counts as stalled (well above one DU frame).
pub const STALL_US: u64 = 50_000;

/// Inbound sync materialization runs on the runtime thread, so it waits for a real pause in
/// writing: no stylus event for this long.
pub const INBOUND_QUIET_US: u64 = 1_500_000;

/// May the runtime start an inbound apply transaction now? Never while the pen is in range (a
/// hover always precedes a pen Down), never with queued input, and only after a quiet period.
/// Deferral is always safe: staged items stay durable in the inbox and own operations stay in
/// the accepted bridge until the cursor consumes them.
pub fn inbound_may_run(pen_in_range: Option<bool>, us_since_stylus: u64, backlog: usize) -> bool {
    pen_in_range != Some(true) && backlog == 0 && us_since_stylus >= INBOUND_QUIET_US
}

/// A main-thread operation that could delay input dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    pub what: &'static str,
    pub dur_us: u64,
    pub end_us: u64,
}

#[derive(Debug, Default)]
pub struct InputLag {
    pub max_delay_us: u64,
    pub stalls: u64,
    in_stall: bool,
    pub last_block: Option<Block>,
    pub max_block: Option<Block>,
}

/// First event of a new stall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stall {
    pub delay_us: u64,
    pub last_block: Option<Block>,
}

impl InputLag {
    /// Observe one dispatched event. Returns `Some` only for the first event of a stall run.
    pub fn observe(&mut self, kernel_us: u64, now_us: u64) -> Option<Stall> {
        let d = now_us.saturating_sub(kernel_us);
        self.max_delay_us = self.max_delay_us.max(d);
        if d < STALL_US {
            self.in_stall = false;
            return None;
        }
        if self.in_stall {
            return None;
        }
        self.in_stall = true;
        self.stalls += 1;
        Some(Stall {
            delay_us: d,
            last_block: self.last_block,
        })
    }

    /// Record a main-thread blocking operation that just finished at `end_us`.
    pub fn blocked(&mut self, what: &'static str, dur_us: u64, end_us: u64) {
        let b = Block {
            what,
            dur_us,
            end_us,
        };
        self.last_block = Some(b);
        if self.max_block.is_none_or(|m| dur_us > m.dur_us) {
            self.max_block = Some(b);
        }
    }
}

impl Stall {
    pub fn log_line(&self, kind: &str, now_us: u64) -> String {
        match self.last_block {
            Some(b) => format!(
                "input_stall delay_ms={} kind={kind} last_block={} block_ms={} block_end_ago_ms={}",
                self.delay_us / 1000,
                b.what,
                b.dur_us / 1000,
                now_us.saturating_sub(b.end_us) / 1000
            ),
            None => format!(
                "input_stall delay_ms={} kind={kind} last_block=none",
                self.delay_us / 1000
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_waits_for_pen_out_of_range_empty_backlog_and_quiet() {
        let q = INBOUND_QUIET_US;
        assert!(inbound_may_run(Some(false), q, 0));
        assert!(inbound_may_run(None, q, 0));
        assert!(
            !inbound_may_run(Some(true), q * 100, 0),
            "hovering pen = about to write"
        );
        assert!(!inbound_may_run(Some(false), q - 1, 0), "still writing");
        assert!(!inbound_may_run(Some(false), q, 3), "queued input first");
    }

    #[test]
    fn a_stall_run_is_reported_once_and_attributed() {
        let mut l = InputLag::default();
        assert_eq!(l.observe(1_000, 2_000), None);
        l.blocked("inbound_apply", 400_000, 500_000);
        let s = l.observe(100_000, 520_000).expect("stall starts");
        assert_eq!(s.delay_us, 420_000);
        assert_eq!(s.last_block.unwrap().what, "inbound_apply");
        // The backlog behind it is the same stall.
        assert_eq!(l.observe(110_000, 521_000), None);
        // Caught up, then a new stall.
        assert_eq!(l.observe(600_000, 601_000), None);
        assert!(l.observe(700_000, 800_000).is_some());
        assert_eq!(l.stalls, 2);
        assert_eq!(l.max_delay_us, 420_000);
        assert_eq!(l.max_block.unwrap().dur_us, 400_000);
        assert!(s.log_line("Down", 520_000).contains("block_end_ago_ms=20"));
    }
}
