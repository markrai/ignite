//! Slice 19A: startup sync breakdown (measurement only; no behaviour depends on it).
//!
//! One `sync_startup_breakdown` line per [`crate::sync_outbound::OutboundSync::open`] attempt:
//! on success (`outcome=ok`), after a reconcile fault (`outcome=fault`, `fault_phase=`), and —
//! through the guard's `Drop` — when `open` returns `Err` or unwinds (`outcome=err`,
//! `failed_phase=`). Whatever was measured before the failure is still reported.
//!
//! Accounting: phases are **exclusive** and strictly sequential — entering a phase closes the
//! previous one — so the `<phase>_ms` fields sum to `attributed_ms`, and `residual_ms` is the
//! remainder of `total_ms` (time before the first phase / after the last; ~0). `diag_ms` is this
//! module's own memory sampling, excluded from every other phase. Fields named `ckpt_*`,
//! `replay_*`, `validate_census_*` and `stamp_census_*` are **inclusive** sub-components of the
//! `checkpoint`, `replay`, `validate` and `stamp` phases and are not part of the sum.
//! `ckpt_other_ms` / `replay_other_ms` are what those phases spent outside their named
//! sub-timers (bookkeeping, path joins, error mapping; for replay also log framing, CRC, file
//! I/O and page bookkeeping) — including the sub-timers' own clock reads.
//!
//! What `total_ms` does **not** cover: the call into `open` before [`begin`], building this
//! line (`emit_format_ms`, reported after `total_ms` is taken) and writing it to stdout (not
//! measured), and whatever the caller does around `open` (the runtime's `set_device`,
//! `install_global` and `sync_enabled` print). Those lie inside the runtime's
//! `sync_reconcile_begin/end` interval: outer − `total_ms` bounds them.
//!
//! Overhead: the instrumentation's clock reads run inside the phases they measure and are part
//! of their times. `clock_read_ns` is a per-read cost calibrated at [`begin`] and
//! `instr_clock_reads` the number of reads made (exact for phase/memory boundaries, derived
//! from page/record counts for the loaders), so `instr_overhead_est_ms` estimates the timer
//! cost contained in `total_ms`. Counting loops over in-memory lines (accepted/inbox bytes)
//! are not included in that estimate.
//!
//! Values: a number (`0` is a measured zero); `-` = not reached, or not applicable on this
//! path; `na` = the measurement is unavailable on this platform (e.g. no `/proc/self/status`).
//! `rss_kb_*` is the instantaneous resident set; `hwm_kb_*` is the **process** high-water mark
//! so far (it never decreases and is not the memory of any single phase).
//!
//! Slice 19B.2B: `defer=` records the deferred confirmed-base decision (`selected`,
//! `not_requested`, or a refusal code such as `dirty_fallback` / `confirmed_log_cursor`). When
//! selected, the foreground line has no page or replay phases; the loader thread emits its own
//! `sync_deferred_base_breakdown` line (same fields, `path=deferred`), and `launch_ms
//! phase=sync_base_ready` / `sync_base_fault` mark when it finished. Never add the two lines'
//! totals: they run concurrently.
//!
//! State is thread-local and exists only between [`begin`] and the guard's end, so the recording
//! helpers are no-ops on other threads and outside startup (inbound apply, checkpoints, recovery
//! tools). Nothing here reads or writes the library or the sync directory.

use std::cell::RefCell;
use std::time::{Duration, Instant};

/// Exclusive phases, in startup order (the order fields are printed in).
const PHASES: &[&str] = &[
    "pre",
    "replica",
    "defer_preflight",
    "epoch_state",
    "epoch_upgrade",
    "epoch_new",
    "checkpoint",
    "replay",
    "log_open",
    "legacy_convert",
    "ckpt_gc",
    "cursor",
    "accepted_epoch",
    "device_geom",
    "accepted_open",
    "inbox",
    "identity_open",
    "outbox",
    "marker",
    "validate",
    "fast_publish",
    "inbound_recovery",
    "expected",
    "identity_plan",
    "snapshot",
    "derive",
    "envelopes",
    "proof",
    "persist_identity",
    "blobs",
    "journal",
    "stamp",
    "post",
    "threads",
    "diag",
];

/// Inclusive sub-component durations (printed as `_ms`).
const TIMERS: &[&str] = &[
    "ckpt_manifest_read",
    "ckpt_manifest_parse",
    "ckpt_read",
    "ckpt_sha",
    "ckpt_parse",
    "ckpt_validate",
    "replay_scan",
    "replay_decode",
    "replay_apply",
    "validate_census",
    "stamp_census",
    "snapshot_page_read",
    "snapshot_page_parse",
];

const COUNTS: &[&str] = &[
    "ckpt_present",
    "ckpt_legacy",
    "ckpt_manifest_bytes",
    "ckpt_pages",
    "ckpt_page_bytes",
    "replay_records",
    "replay_applied",
    "replay_payload_bytes",
    "accepted_records",
    "accepted_line_bytes",
    "accepted_pending",
    "inbox_records",
    "inbox_line_bytes",
    "inbox_pending",
    "device_geom_probe",
    "device_geom_rejected",
    "outbox_file_bytes",
    "outbox_records",
    "outbox_pending",
    "outbox_trailing_prep",
    "outbox_torn_tail",
    "validate_census_files",
    "validate_census_hashed",
    "validate_census_size_only",
    "validate_census_bytes_hashed",
    "stamp_census_files",
    "stamp_census_hashed",
    "stamp_census_size_only",
    "stamp_census_bytes_hashed",
    "derived_ops",
    "snapshot_pages",
    "snapshot_journal_bytes",
    "snapshot_journal_bytes_max",
];

/// Memory sample points, in order.
const MEM_TAGS: &[&str] = &["begin", "epoch", "snapshot", "end"];

struct Diag {
    t0: Instant,
    /// Mean cost of one `Instant::now()`, calibrated at `begin`.
    clock_read: Duration,
    /// Clock reads made by `enter`/`mem`/`emit` (loaders' reads are derived from their counts).
    reads: u64,
    current: Option<(&'static str, Instant)>,
    last_completed: Option<&'static str>,
    /// `last_completed` before the most recent close (the last phase that really completed
    /// when the scope ends with a phase still in progress).
    phases_before_last: Option<&'static str>,
    phases: Vec<(&'static str, Duration)>,
    timers: Vec<(&'static str, Duration)>,
    counts: Vec<(&'static str, u64)>,
    mem: Vec<(&'static str, Option<u64>, Option<u64>)>,
    /// Slice 19B.3B: one entry per page `build_snapshot` processed (total duration only; no
    /// page identity is kept).
    page_samples: Vec<Duration>,
    /// The page currently between [`page_begin`] and [`page_end`]: (read, parse/replay, bytes).
    page_open: Option<(Duration, Duration, u64)>,
    path: Option<&'static str>,
    /// Slice 19B.2B: deferred confirmed-base decision (`selected`, `not_requested`, or the
    /// refusal reason code).
    defer: Option<&'static str>,
    /// Line prefix: `sync_startup_breakdown` (open) or `sync_deferred_base_breakdown` (loader).
    prefix: &'static str,
    fault_phase: Option<&'static str>,
}

thread_local! {
    static CUR: RefCell<Option<Diag>> = const { RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    static LAST_LINE: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn with(f: impl FnOnce(&mut Diag)) {
    let _ = CUR.try_with(|c| {
        if let Some(d) = c.borrow_mut().as_mut() {
            f(d);
        }
    });
}

fn bump<T: Copy + std::ops::Add<Output = T>>(
    v: &mut Vec<(&'static str, T)>,
    k: &'static str,
    x: T,
) {
    match v.iter_mut().find(|(n, _)| *n == k) {
        Some((_, y)) => *y = *y + x,
        None => v.push((k, x)),
    }
}

fn get<T: Copy>(v: &[(&'static str, T)], k: &str) -> Option<T> {
    v.iter().find(|(n, _)| *n == k).map(|(_, x)| *x)
}

impl Diag {
    /// Clock reads made inside the instrumented loaders, from what they processed.
    fn loader_reads(&self) -> u64 {
        let c = |k| get(&self.counts, k).unwrap_or(0);
        // load_checkpoint: 2 (manifest read) + 2 (parse) + 4 per page + 2 (validate); a missing
        // manifest stops after its first read.
        let ckpt = match get(&self.counts, "ckpt_present") {
            Some(0) => 1,
            Some(_) => 6 + 4 * c("ckpt_pages"),
            None => 0,
        };
        // replay_log: 2 around the scan + 3 per applied record.
        let replay = if get(&self.timers, "replay_scan").is_some() {
            2 + 3 * c("replay_applied")
        } else {
            0
        };
        // Each census wrapper: 2.
        let census = 2
            * (u64::from(get(&self.timers, "validate_census").is_some())
                + u64::from(get(&self.timers, "stamp_census").is_some()));
        ckpt + replay + census
    }

    fn close(&mut self, now: Instant) {
        if let Some((name, start)) = self.current.take() {
            bump(&mut self.phases, name, now.saturating_duration_since(start));
            self.phases_before_last = self.last_completed;
            self.last_completed = Some(name);
        }
    }

    fn line(
        &self,
        outcome: &str,
        failed_phase: Option<&str>,
        last_phase: Option<&str>,
        total: Duration,
    ) -> String {
        fn ms(d: Duration) -> String {
            format!("{:.1}", d.as_secs_f64() * 1000.0)
        }
        fn opt<T: ToString>(v: Option<T>) -> String {
            v.map_or_else(|| "-".to_string(), |x| x.to_string())
        }
        let attributed: Duration = self.phases.iter().map(|(_, d)| *d).sum();
        let mut s = format!(
            "{} outcome={outcome} path={} defer={} failed_phase={} last_phase={} fault_phase={} total_ms={} attributed_ms={} residual_ms={}",
            self.prefix,
            self.path.unwrap_or("-"),
            self.defer.unwrap_or("-"),
            failed_phase.unwrap_or("-"),
            last_phase.unwrap_or("-"),
            self.fault_phase.unwrap_or("-"),
            ms(total),
            ms(attributed),
            ms(total.saturating_sub(attributed)),
        );
        let mut push = |k: &str, v: String| {
            s.push(' ');
            s.push_str(k);
            s.push('=');
            s.push_str(&v);
        };
        for p in PHASES {
            push(&format!("{p}_ms"), opt(get(&self.phases, p).map(ms)));
        }
        for t in TIMERS {
            push(&format!("{t}_ms"), opt(get(&self.timers, t).map(ms)));
        }
        // Derived exclusive remainders of the two instrumented loaders.
        let ckpt_other = match (
            get(&self.phases, "checkpoint"),
            get(&self.counts, "ckpt_present"),
            get(&self.counts, "ckpt_legacy"),
        ) {
            (Some(total), Some(1), None) => {
                let sub: Duration = TIMERS
                    .iter()
                    .filter(|t| t.starts_with("ckpt_"))
                    .filter_map(|t| get(&self.timers, t))
                    .sum();
                Some(ms(total.saturating_sub(sub)))
            }
            _ => None,
        };
        push("ckpt_other_ms", opt(ckpt_other));
        let other = get(&self.timers, "replay_scan").map(|scan| {
            let cb = get(&self.timers, "replay_decode").unwrap_or_default()
                + get(&self.timers, "replay_apply").unwrap_or_default();
            ms(scan.saturating_sub(cb))
        });
        push("replay_other_ms", opt(other));
        // Slice 19B.3B: per-page `page_snapshot` distribution (nearest-rank percentiles).
        let mut pages = self.page_samples.clone();
        pages.sort();
        let pick = |q: f64| -> Option<String> {
            if pages.is_empty() {
                return None;
            }
            let rank = ((q * pages.len() as f64).ceil() as usize).clamp(1, pages.len());
            Some(ms(pages[rank - 1]))
        };
        push("snapshot_page_ms_min", opt(pages.first().map(|d| ms(*d))));
        push("snapshot_page_ms_median", opt(pick(0.5)));
        push("snapshot_page_ms_p90", opt(pick(0.9)));
        push("snapshot_page_ms_max", opt(pages.last().map(|d| ms(*d))));
        for c in COUNTS {
            push(c, opt(get(&self.counts, c)));
        }
        let reads = self.reads + self.loader_reads();
        push("clock_read_ns", self.clock_read.as_nanos().to_string());
        push("instr_clock_reads", reads.to_string());
        push(
            "instr_overhead_est_ms",
            format!(
                "{:.3}",
                self.clock_read.as_secs_f64() * 1000.0 * reads as f64
            ),
        );
        for tag in MEM_TAGS {
            let m = self.mem.iter().find(|(t, _, _)| t == tag);
            let fmt = |v: Option<u64>| v.map_or_else(|| "na".to_string(), |x| x.to_string());
            push(
                &format!("rss_kb_{tag}"),
                m.map_or_else(|| "-".into(), |(_, r, _)| fmt(*r)),
            );
            push(
                &format!("hwm_kb_{tag}"),
                m.map_or_else(|| "-".into(), |(_, _, h)| fmt(*h)),
            );
        }
        s
    }
}

/// Scope of one `open` attempt. Emits the breakdown exactly once: [`Startup::finish`] or `Drop`.
pub(crate) struct Startup {
    done: bool,
}

/// Start collecting for this thread (replaces any stale collector) and take the `begin` memory
/// sample. The first phase must be entered explicitly.
pub(crate) fn begin() -> Startup {
    begin_as("sync_startup_breakdown")
}

/// Slice 19B.2B: like [`begin`], for the deferred confirmed-base loader thread. Its line
/// (`sync_deferred_base_breakdown`) is separate: background time is never added to the
/// foreground startup accounting.
pub(crate) fn begin_deferred() -> Startup {
    begin_as("sync_deferred_base_breakdown")
}

fn begin_as(prefix: &'static str) -> Startup {
    // Calibrate the cost of one clock read (32 back-to-back reads; a few µs even uncached).
    const N: u32 = 32;
    let c0 = Instant::now();
    let mut last = c0;
    for _ in 0..N {
        last = Instant::now();
    }
    let clock_read = last.saturating_duration_since(c0) / N;
    let _ = CUR.try_with(|c| {
        *c.borrow_mut() = Some(Diag {
            t0: Instant::now(),
            clock_read,
            reads: 0,
            current: None,
            last_completed: None,
            phases_before_last: None,
            phases: Vec::new(),
            timers: Vec::new(),
            counts: Vec::new(),
            mem: Vec::new(),
            page_samples: Vec::new(),
            page_open: None,
            path: None,
            defer: None,
            prefix,
            fault_phase: None,
        });
    });
    mem("begin");
    Startup { done: false }
}

/// Close the current phase (if any) and start `phase`.
pub(crate) fn enter(phase: &'static str) {
    with(|d| {
        let now = Instant::now();
        d.reads += 1;
        d.close(now);
        d.current = Some((phase, now));
    });
}

/// Add `d` to the inclusive sub-component timer `key`.
pub(crate) fn time(key: &'static str, d: Duration) {
    with(|g| bump(&mut g.timers, key, d));
}

/// Add `n` to counter `key`.
pub(crate) fn count(key: &'static str, n: u64) {
    with(|d| bump(&mut d.counts, key, n));
}

/// Set counter `key` unless already set (repeated loads of the same input report it once).
pub(crate) fn count_once(key: &'static str, n: u64) {
    with(|d| {
        if get(&d.counts, key).is_none() {
            d.counts.push((key, n));
        }
    });
}

/// Record the Slice 19B.2B deferral decision (`selected`, `not_requested`, or a refusal code).
pub(crate) fn defer(why: &'static str) {
    with(|d| d.defer = Some(why));
}

/// Record the reconcile path (`FastPathReason::as_str`).
pub(crate) fn path(p: &'static str) {
    with(|d| d.path = Some(p));
}

/// Slice 19B.3B: `build_snapshot` starts one page (the journal loader then reports into it).
pub(crate) fn page_begin() {
    with(|d| d.page_open = Some((Duration::ZERO, Duration::ZERO, 0)));
}

/// Journal I/O of the page in progress: file read time, parse + edit-replay time, bytes.
/// A no-op outside [`page_begin`]..[`page_end`] (other callers of the loader are not counted).
pub(crate) fn page_io(read: Duration, parse: Duration, bytes: u64) {
    with(|d| {
        if let Some(p) = d.page_open.as_mut() {
            p.0 += read;
            p.1 += parse;
            p.2 += bytes;
        }
    });
}

/// `build_snapshot` finished one page in `total` (inclusive of its journal I/O).
pub(crate) fn page_end(total: Duration) {
    with(|d| {
        let (read, parse, bytes) = d.page_open.take().unwrap_or_default();
        d.page_samples.push(total);
        bump(&mut d.timers, "snapshot_page_read", read);
        bump(&mut d.timers, "snapshot_page_parse", parse);
        bump(&mut d.counts, "snapshot_pages", 1);
        bump(&mut d.counts, "snapshot_journal_bytes", bytes);
        match d
            .counts
            .iter_mut()
            .find(|(n, _)| *n == "snapshot_journal_bytes_max")
        {
            Some((_, m)) => *m = (*m).max(bytes),
            None => d.counts.push(("snapshot_journal_bytes_max", bytes)),
        }
    });
}

/// Record one census walk. `stamp` = the walk inside the stamp write, else the fast-path check.
pub(crate) fn census(stamp: bool, s: &crate::sync_verified::CensusStats, elapsed: Duration) {
    let (t, f, h, z, b) = if stamp {
        (
            "stamp_census",
            "stamp_census_files",
            "stamp_census_hashed",
            "stamp_census_size_only",
            "stamp_census_bytes_hashed",
        )
    } else {
        (
            "validate_census",
            "validate_census_files",
            "validate_census_hashed",
            "validate_census_size_only",
            "validate_census_bytes_hashed",
        )
    };
    time(t, elapsed);
    count(f, s.files as u64);
    count(h, s.hashed_files as u64);
    count(z, s.size_only_files as u64);
    count(b, s.bytes_hashed);
}

/// Sample `VmRSS`/`VmHWM` at `tag` (one small procfs read). Its cost is charged to `diag`, not
/// to the phase in progress.
pub(crate) fn mem(tag: &'static str) {
    let mut active = false;
    with(|_| active = true);
    if !active {
        return;
    }
    let t = Instant::now();
    let (rss, hwm) = crate::launch_clock::mem_status_kb();
    let dt = t.elapsed();
    with(|d| {
        d.reads += 2;
        d.mem.push((tag, rss, hwm));
        bump(&mut d.phases, "diag", dt);
        if let Some((_, start)) = d.current.as_mut() {
            *start += dt;
        }
    });
}

impl Startup {
    /// Startup reconcile faulted in the current phase (upload halted, capture continues).
    pub(crate) fn fault(&self) {
        with(|d| d.fault_phase = d.current.map(|(p, _)| p));
    }

    /// `open` succeeded (possibly with a recorded fault): emit the breakdown.
    pub(crate) fn finish(mut self) {
        self.done = true;
        emit(None);
    }
}

impl Drop for Startup {
    fn drop(&mut self) {
        if !self.done {
            emit(Some(()));
        }
    }
}

fn emit(failed: Option<()>) {
    if failed.is_none() {
        mem("end");
    }
    let line = CUR
        .try_with(|c| {
            let mut d = c.borrow_mut().take()?;
            let now = Instant::now();
            d.reads += 1;
            let failed_phase = failed.and(d.current.map(|(p, _)| p));
            d.close(now);
            // On failure the phase in progress did not complete.
            let last_phase = match failed_phase {
                Some(_) => d.phases_before_last,
                None => d.last_completed,
            };
            let outcome = if failed.is_some() {
                "err"
            } else if d.fault_phase.is_some() {
                "fault"
            } else {
                "ok"
            };
            let t_fmt = Instant::now();
            let mut line = d.line(
                outcome,
                failed_phase,
                last_phase,
                now.saturating_duration_since(d.t0),
            );
            // Measured after `total_ms`; the stdout write itself is not measured.
            line.push_str(&format!(
                " emit_format_ms={:.3}",
                t_fmt.elapsed().as_secs_f64() * 1000.0
            ));
            Some(line)
        })
        .ok()
        .flatten();
    if let Some(line) = line {
        println!("{line}");
        #[cfg(test)]
        LAST_LINE.with(|l| *l.borrow_mut() = Some(line));
    }
}

/// Test-only: the last breakdown line emitted on this thread.
#[cfg(test)]
pub(crate) fn take_last_line() -> Option<String> {
    LAST_LINE.with(|l| l.borrow_mut().take())
}

/// Test-only: the value of `key=` in a breakdown line.
#[cfg(test)]
pub(crate) fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split(' ')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_are_noops_without_a_collector() {
        enter("pre");
        count("ckpt_pages", 3);
        mem("begin");
        assert!(take_last_line().is_none());
    }

    #[test]
    fn exclusive_phases_sum_and_unrun_fields_are_dashes() {
        let s = begin();
        enter("pre");
        enter("replica");
        count("ckpt_pages", 2);
        count("ckpt_pages", 3);
        count_once("accepted_records", 7);
        count_once("accepted_records", 9);
        path("verified_fast_path");
        s.finish();
        let l = take_last_line().unwrap();
        assert_eq!(field(&l, "outcome"), Some("ok"));
        assert_eq!(field(&l, "path"), Some("verified_fast_path"));
        assert_eq!(field(&l, "last_phase"), Some("replica"));
        assert_eq!(field(&l, "ckpt_pages"), Some("5"));
        assert_eq!(field(&l, "accepted_records"), Some("7"));
        assert_eq!(field(&l, "snapshot_ms"), Some("-"));
        assert_eq!(field(&l, "ckpt_other_ms"), Some("-"));
        assert_ne!(field(&l, "pre_ms"), Some("-"));
        let total: f64 = field(&l, "total_ms").unwrap().parse().unwrap();
        let attr: f64 = field(&l, "attributed_ms").unwrap().parse().unwrap();
        let resid: f64 = field(&l, "residual_ms").unwrap().parse().unwrap();
        assert!(
            attr <= total + 0.1 && (total - attr - resid).abs() <= 0.11,
            "{l}"
        );
        // Memory: reached but maybe unavailable (`na` off Linux); never `-` once sampled.
        assert_ne!(field(&l, "rss_kb_begin"), Some("-"));
        assert_ne!(field(&l, "hwm_kb_end"), Some("-"));
        assert_eq!(field(&l, "rss_kb_snapshot"), Some("-"));
        // begin's mem sample (2) + 2 enters + emit (1) + end mem sample (2); no loaders ran.
        assert_eq!(field(&l, "instr_clock_reads"), Some("7"));
        assert!(field(&l, "clock_read_ns").unwrap().parse::<u64>().is_ok());
        assert!(field(&l, "emit_format_ms").unwrap().parse::<f64>().is_ok());
        assert!(l.ends_with(&format!(
            "emit_format_ms={}",
            field(&l, "emit_format_ms").unwrap()
        )));
    }

    #[test]
    fn page_samples_aggregate_without_identity() {
        let s = begin();
        enter("snapshot");
        page_io(Duration::from_millis(1), Duration::from_millis(2), 10); // outside a page: ignored
        for (i, ms_) in [5u64, 1, 9, 3, 7].iter().enumerate() {
            page_begin();
            page_io(
                Duration::from_millis(1),
                Duration::from_millis(*ms_),
                100 + i as u64,
            );
            page_end(Duration::from_millis(*ms_ + 1));
        }
        s.finish();
        let l = take_last_line().unwrap();
        assert_eq!(field(&l, "snapshot_pages"), Some("5"));
        assert_eq!(field(&l, "snapshot_journal_bytes"), Some("510"));
        assert_eq!(field(&l, "snapshot_journal_bytes_max"), Some("104"));
        assert_eq!(field(&l, "snapshot_page_ms_min"), Some("2.0"));
        assert_eq!(field(&l, "snapshot_page_ms_median"), Some("6.0"));
        assert_eq!(field(&l, "snapshot_page_ms_p90"), Some("10.0"));
        assert_eq!(field(&l, "snapshot_page_ms_max"), Some("10.0"));
        assert_eq!(field(&l, "snapshot_page_read_ms"), Some("5.0"));
        assert_eq!(field(&l, "snapshot_page_parse_ms"), Some("25.0"));
    }

    #[test]
    fn a_dropped_scope_reports_the_failed_phase() {
        {
            let _s = begin();
            enter("pre");
            enter("checkpoint");
            count("ckpt_pages", 1);
        }
        let l = take_last_line().unwrap();
        assert_eq!(field(&l, "outcome"), Some("err"));
        assert_eq!(field(&l, "failed_phase"), Some("checkpoint"));
        assert_eq!(field(&l, "last_phase"), Some("pre"));
        assert_eq!(field(&l, "ckpt_pages"), Some("1"));
        assert_eq!(field(&l, "rss_kb_end"), Some("-"));
    }

    #[test]
    fn a_fault_names_its_phase_and_the_scope_still_finishes() {
        let s = begin();
        enter("snapshot");
        s.fault();
        enter("post");
        s.finish();
        let l = take_last_line().unwrap();
        assert_eq!(field(&l, "outcome"), Some("fault"));
        assert_eq!(field(&l, "fault_phase"), Some("snapshot"));
        assert_eq!(field(&l, "failed_phase"), Some("-"));
    }
}
