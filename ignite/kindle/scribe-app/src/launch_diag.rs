//! Bounded launch-window diagnostics. No pump-thread I/O. Main-loop lines are budgeted.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use kindle_platform::{FbGridSample, FlushCopyStats, TouchFrame, TouchLift};

use crate::launch_clock;
use crate::touch::{FingerAction, FingerClassifyDiag, FingerTracker};

static LIVE: OnceLock<Mutex<LaunchDiag>> = OnceLock::new();

/// Process-wide diag used by the unix runtime. Tests construct [`LaunchDiag`] directly.
pub fn live() -> MutexGuard<'static, LaunchDiag> {
    LIVE.get_or_init(|| Mutex::new(LaunchDiag::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Per-frame / per-SEND budget inside the launch window. Transitions may still
/// emit after the window until this cap is hit.
pub const TOUCH_LINE_BUDGET: u32 = 48;
pub const FB_LINE_BUDGET: u32 = 24;
pub const FINGER_CLOCK_BUDGET: u32 = 24;
pub const WINDOW_MS: u128 = 15_000;

#[derive(Clone, Copy, Debug, Default)]
struct GestureClock {
    down_at: Option<Instant>,
    down_slot: Option<usize>,
    down_tid: Option<i32>,
    up_at: Option<Instant>,
    up_slot: Option<usize>,
    up_tid: Option<i32>,
    ambiguous: bool,
}

#[derive(Clone, Debug, Default)]
pub struct LaunchDiag {
    last_n: usize,
    last_empty: bool,
    primed: bool,
    touch_emitted: u32,
    touch_dropped: u32,
    touch_move_suppressed: u32,
    fb_emitted: u32,
    fb_dropped: u32,
    finger_clock_emitted: u32,
    finger_clock_dropped: u32,
    stats_emitted: bool,
    gesture: GestureClock,
}

impl LaunchDiag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dropped_touch(&self) -> u32 {
        self.touch_dropped
    }

    pub fn dropped_fb(&self) -> u32 {
        self.fb_dropped
    }

    pub fn move_suppressed(&self) -> u32 {
        self.touch_move_suppressed
    }

    pub fn touch_emitted(&self) -> u32 {
        self.touch_emitted
    }

    pub fn finger_clock_emitted(&self) -> u32 {
        self.finger_clock_emitted
    }

    pub fn finger_clock_dropped(&self) -> u32 {
        self.finger_clock_dropped
    }

    pub fn stats_emitted(&self) -> bool {
        self.stats_emitted
    }

    fn in_window(launch_ms: u128) -> bool {
        launch_ms <= WINDOW_MS
    }

    fn take_finger_clock_budget(&mut self, launch_ms: u128) -> bool {
        if !Self::in_window(launch_ms) || self.finger_clock_emitted >= FINGER_CLOCK_BUDGET {
            self.finger_clock_dropped = self.finger_clock_dropped.saturating_add(1);
            return false;
        }
        self.finger_clock_emitted = self.finger_clock_emitted.saturating_add(1);
        true
    }

    /// Bind pump Instants to slot/tid when the frame is a single contact or single lift.
    pub fn record_gesture(&mut self, at: Instant, frame: &TouchFrame) {
        if !frame.contacts.is_empty() {
            let new_down = self.gesture.down_at.is_none() || self.gesture.up_at.is_some();
            if new_down {
                let c = &frame.contacts[0];
                self.gesture = GestureClock {
                    down_at: Some(at),
                    down_slot: Some(c.slot),
                    down_tid: Some(c.tracking_id),
                    up_at: None,
                    up_slot: None,
                    up_tid: None,
                    ambiguous: frame.contacts.len() != 1,
                };
            } else if frame.contacts.len() != 1 {
                self.gesture.ambiguous = true;
            }
        } else if !frame.lifts.is_empty() {
            self.gesture.up_at = Some(at);
            if frame.lifts.len() == 1 {
                self.gesture.up_slot = Some(frame.lifts[0].slot);
                self.gesture.up_tid = Some(frame.lifts[0].tracking_id);
            } else {
                self.gesture.ambiguous = true;
            }
            if self.gesture.down_tid != self.gesture.up_tid
                || self.gesture.down_slot != self.gesture.up_slot
            {
                self.gesture.ambiguous = true;
            }
        }
    }

    fn correlate_label(&self) -> &'static str {
        if self.gesture.down_at.is_none() || self.gesture.up_at.is_none() || self.gesture.ambiguous
        {
            "ambiguous"
        } else {
            "ok"
        }
    }

    fn take_touch_budget(&mut self, launch_ms: u128, transition: bool) -> bool {
        if !transition {
            if Self::in_window(launch_ms) {
                self.touch_move_suppressed = self.touch_move_suppressed.saturating_add(1);
            }
            return false;
        }
        if self.touch_emitted >= TOUCH_LINE_BUDGET {
            self.touch_dropped = self.touch_dropped.saturating_add(1);
            return false;
        }
        self.touch_emitted = self.touch_emitted.saturating_add(1);
        true
    }

    fn take_fb_budget(&mut self) -> bool {
        if self.fb_emitted >= FB_LINE_BUDGET {
            self.fb_dropped = self.fb_dropped.saturating_add(1);
            false
        } else {
            self.fb_emitted = self.fb_emitted.saturating_add(1);
            true
        }
    }

    /// Contact-count / lift / empty transitions only. Moves are counted, not printed.
    pub fn note_frame(&mut self, frame: &TouchFrame) -> bool {
        let n = frame.contacts.len();
        let empty = n == 0;
        let transition =
            !self.primed || n != self.last_n || empty != self.last_empty || !frame.lifts.is_empty();
        self.primed = true;
        self.last_n = n;
        self.last_empty = empty;
        transition
    }

    #[allow(clippy::too_many_arguments)]
    pub fn emit_touch_frame(
        &mut self,
        pump_at: Instant,
        dispatch_at: Instant,
        frame: &TouchFrame,
        fifo: usize,
        produced: u64,
        consumed: u64,
        ui_x: Option<u32>,
        ui_y: Option<u32>,
    ) {
        let launch_ms = launch_clock::elapsed_ms();
        self.record_gesture(pump_at, frame);
        let transition = self.note_frame(frame);
        if !self.take_touch_budget(launch_ms, transition) {
            return;
        }
        let pump_ms = launch_clock::ms_at(pump_at);
        let dispatch_ms = launch_clock::ms_at(dispatch_at);
        let queue_us = dispatch_at.saturating_duration_since(pump_at).as_micros() as u64;
        let (slot, tid, raw_x, raw_y) = primary_contact(frame);
        let lift_tid = primary_lift_tid(&frame.lifts);
        let k = frame.kernel_time;
        println!(
            "touch_frame launch_ms={launch_ms} pump_ms={pump_ms} dispatch_ms={dispatch_ms} \
             queue_us={queue_us} k_sec={} k_usec={} clock=realtime n={} slot={} tid={} \
             lifts={} lift_tid={} raw_x={} raw_y={} ui_x={} ui_y={} empty={} fifo={fifo} \
             produced={produced} consumed={consumed} kernel_unread=unknown \
             note=fifo_empty_is_not_kernel_idle",
            k.tv_sec,
            k.tv_usec,
            frame.contacts.len(),
            fmt_opt_usize(slot),
            fmt_opt_i32(tid),
            frame.lifts.len(),
            fmt_opt_i32(lift_tid),
            fmt_opt_i32(raw_x),
            fmt_opt_i32(raw_y),
            fmt_opt_u32(ui_x),
            fmt_opt_u32(ui_y),
            u8::from(frame.contacts.is_empty()),
        );
    }

    /// Attach classify diag only to Tap / page-swipe lifts. Always consumes the snapshot.
    pub fn classify_for_action(
        tracker: &mut FingerTracker,
        action: FingerAction,
    ) -> Option<FingerClassifyDiag> {
        let taken = tracker.take_last_classify();
        match action {
            FingerAction::Tap(_) | FingerAction::NextPage | FingerAction::PreviousPage => taken,
            _ => None,
        }
    }

    /// Format classify fields; absent → `-`, never a fabricated duration.
    pub fn format_classify_fields(classify: Option<FingerClassifyDiag>) -> String {
        match classify {
            Some(c) => format!(
                "tracker_start_now_ms={} tracker_up_now_ms={} tracker_dur_ms={} \
                 tracker_travel={} tracker_tap_max_ms={} classified_tap={}",
                c.start_now_ms,
                c.up_now_ms,
                c.dur_ms,
                c.max_travel,
                c.tap_max_ms,
                u8::from(c.classified_as_tap)
            ),
            None => "tracker_start_now_ms=- tracker_up_now_ms=- tracker_dur_ms=- tracker_travel=- tracker_tap_max_ms=- classified_tap=-"
                .into(),
        }
    }

    pub fn emit_finger_action(
        &mut self,
        session_start: Instant,
        action: &str,
        view: &str,
        modal: bool,
        command: &str,
        classify: Option<FingerClassifyDiag>,
    ) {
        let launch_ms = launch_clock::elapsed_ms();
        if !self.take_finger_clock_budget(launch_ms) {
            return;
        }
        let pump_down_ms = self.gesture.down_at.map(launch_clock::ms_at);
        let pump_up_ms = self.gesture.up_at.map(launch_clock::ms_at);
        let sat = self
            .gesture
            .down_at
            .map(|d| launch_clock::is_before(d, session_start))
            .unwrap_or(false)
            || self
                .gesture
                .up_at
                .map(|u| launch_clock::is_before(u, session_start))
                .unwrap_or(false);
        let fields = Self::format_classify_fields(classify);
        println!(
            "finger_clock launch_ms={launch_ms} action={action} view={view} modal={modal} \
             command={command} {fields} tracker_saturated={} pump_down_launch_ms={} \
             pump_up_launch_ms={} pump_down_slot={} pump_down_tid={} pump_up_slot={} \
             pump_up_tid={} correlate={} note=classified_tap_is_not_physical_duration",
            u8::from(sat),
            fmt_opt_u64(pump_down_ms),
            fmt_opt_u64(pump_up_ms),
            fmt_opt_usize(self.gesture.down_slot),
            fmt_opt_i32(self.gesture.down_tid),
            fmt_opt_usize(self.gesture.up_slot),
            fmt_opt_i32(self.gesture.up_tid),
            self.correlate_label(),
        );
    }

    pub fn emit_ui_ready_fifo(&mut self, fifo: usize, produced: u64, consumed: u64) {
        let launch_ms = launch_clock::elapsed_ms();
        println!(
            "touch_diag launch_ms={launch_ms} fifo={fifo} produced={produced} consumed={consumed} \
             kernel_unread=unknown note=fifo_empty_is_not_kernel_idle"
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn emit_fb_full(
        &mut self,
        kind: &str,
        marker: u32,
        collision: u32,
        send_us: u64,
        wait_us: u64,
        left: u32,
        top: u32,
        w: u32,
        h: u32,
        flush: Option<FlushCopyStats>,
        sample: Option<&FbGridSample>,
    ) {
        if !self.take_fb_budget() {
            return;
        }
        let launch_ms = launch_clock::elapsed_ms();
        let f = flush.unwrap_or_default();
        println!(
            "fb_flush launch_ms={launch_ms} kind={kind} vx={} vy={} w={} h={} bytes={} \
             copy_us={} complement={}",
            f.vx,
            f.vy,
            f.w,
            f.h,
            f.bytes,
            f.copy_us,
            u8::from(f.complement)
        );
        println!(
            "fb_send launch_ms={launch_ms} kind={kind} marker={marker} left={left} top={top} \
             w={w} h={h} waveform=gc16 full=1 send_us={send_us}"
        );
        println!(
            "fb_wait launch_ms={launch_ms} kind={kind} marker={marker} wait_us={wait_us} \
             collision={collision} note=wait_ioctl_is_not_physical_eink_done"
        );
        if let Some(s) = sample {
            if self.take_fb_budget() {
                println!("{}", s.format_line(launch_ms, marker, "wait_complete"));
            }
        }
    }

    fn emit_stats_line(&mut self, launch_ms: u128, reason: &str) {
        self.stats_emitted = true;
        println!(
            "launch_diag_stats launch_ms={launch_ms} reason={reason} touch_emitted={} \
             touch_dropped={} touch_move_suppressed={} fb_emitted={} fb_dropped={} \
             finger_clock_emitted={} finger_clock_dropped={}",
            self.touch_emitted,
            self.touch_dropped,
            self.touch_move_suppressed,
            self.fb_emitted,
            self.fb_dropped,
            self.finger_clock_emitted,
            self.finger_clock_dropped
        );
    }

    /// Once after the diagnostic window, even if no further touch frames arrive.
    pub fn maybe_emit_stats(&mut self) {
        self.maybe_emit_stats_at(launch_clock::elapsed_ms());
    }

    pub fn maybe_emit_stats_at(&mut self, launch_ms: u128) {
        if self.stats_emitted || launch_ms <= WINDOW_MS {
            return;
        }
        self.emit_stats_line(launch_ms, "window");
    }

    /// Once at process shutdown if the window line was never printed.
    pub fn emit_stats_at_shutdown(&mut self) {
        if self.stats_emitted {
            return;
        }
        self.emit_stats_line(launch_clock::elapsed_ms(), "shutdown");
    }
}

fn primary_contact(frame: &TouchFrame) -> (Option<usize>, Option<i32>, Option<i32>, Option<i32>) {
    match frame.contacts.first() {
        Some(c) => (Some(c.slot), Some(c.tracking_id), c.x, c.y),
        None => (None, None, None, None),
    }
}

fn primary_lift_tid(lifts: &[TouchLift]) -> Option<i32> {
    lifts.first().map(|l| l.tracking_id)
}

fn fmt_opt_usize(v: Option<usize>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".into())
}
fn fmt_opt_i32(v: Option<i32>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".into())
}
fn fmt_opt_u32(v: Option<u32>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".into())
}
fn fmt_opt_u64(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".into())
}

/// Host-test helper: kernel timestamp formatting does not mix clock domains.
pub fn kernel_time_is_realtime_label() -> &'static str {
    "clock=realtime"
}

/// Reconstruct tracker `now_ms` the same way the runtime does (`session_start` origin).
pub fn tracker_now_ms(pump_at: Instant, session_start: Instant) -> u64 {
    launch_clock::saturating_ms_since(pump_at, session_start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kindle_platform::{KernelTimestamp, TouchContact, TouchFrame, TouchLift};
    use std::time::Duration;

    fn frame_n(n: usize) -> TouchFrame {
        TouchFrame {
            kernel_time: KernelTimestamp {
                tv_sec: 1,
                tv_usec: 2,
            },
            contacts: (0..n)
                .map(|i| TouchContact {
                    slot: i,
                    tracking_id: i as i32,
                    x: Some(1),
                    y: Some(2),
                    palm: false,
                })
                .collect(),
            lifts: Vec::new(),
        }
    }

    #[test]
    fn only_transitions_consume_touch_budget() {
        let mut d = LaunchDiag::new();
        assert!(d.note_frame(&frame_n(1)));
        assert!(!d.note_frame(&frame_n(1)), "same n is not a transition");
        assert!(d.note_frame(&frame_n(0)));
        let mut lift = frame_n(0);
        lift.lifts = vec![TouchLift {
            slot: 0,
            tracking_id: 7,
            x: Some(1),
            y: Some(2),
        }];
        assert!(d.note_frame(&lift));
        assert_ne!(lift.lifts[0].tracking_id, -1);
    }

    #[test]
    fn budget_drop_increments_counter() {
        let mut d = LaunchDiag::new();
        for _ in 0..TOUCH_LINE_BUDGET {
            assert!(d.take_touch_budget(0, true));
        }
        assert!(!d.take_touch_budget(0, true));
        assert_eq!(d.dropped_touch(), 1);
        assert!(!d.take_touch_budget(0, false));
        assert_eq!(d.move_suppressed(), 1);
    }

    #[test]
    fn tracker_now_ms_matches_runtime_saturation() {
        let session = Instant::now() + Duration::from_millis(800);
        let down = session - Duration::from_millis(200);
        assert_eq!(tracker_now_ms(down, session), 0);
        let up = session + Duration::from_millis(50);
        assert_eq!(tracker_now_ms(up, session), 50);
    }

    #[test]
    fn kernel_label_does_not_claim_monotonic() {
        assert_eq!(kernel_time_is_realtime_label(), "clock=realtime");
    }

    #[test]
    fn finger_clock_budget_is_time_and_count_capped() {
        let mut d = LaunchDiag::new();
        for _ in 0..FINGER_CLOCK_BUDGET {
            assert!(d.take_finger_clock_budget(0));
        }
        assert!(!d.take_finger_clock_budget(0));
        assert_eq!(d.finger_clock_dropped(), 1);
        let mut d2 = LaunchDiag::new();
        assert!(!d2.take_finger_clock_budget(WINDOW_MS + 1));
        assert_eq!(d2.finger_clock_emitted(), 0);
        assert_eq!(d2.finger_clock_dropped(), 1);
    }

    #[test]
    fn absent_classify_does_not_fabricate_durations() {
        let s = LaunchDiag::format_classify_fields(None);
        assert!(s.contains("tracker_dur_ms=-"));
        assert!(s.contains("classified_tap=-"));
        assert!(!s.contains("tracker_dur_ms=0"));
        let some = LaunchDiag::format_classify_fields(Some(FingerClassifyDiag {
            start_now_ms: 10,
            up_now_ms: 90,
            dur_ms: 80,
            max_travel: 3,
            classified_as_tap: true,
            tap_max_ms: 700,
        }));
        assert!(some.contains("tracker_dur_ms=80"));
        assert!(some.contains("classified_tap=1"));
    }

    #[test]
    fn classify_for_action_consumes_snapshot_and_ignores_stale_for_non_lift() {
        use crate::app_ui::UiState;
        use crate::touch::TouchMapping;
        use crate::ui_gfx::{panel_geo, UiPoint};

        let g = panel_geo();
        let m = TouchMapping::new((0, 1859), (0, 2479), &g).unwrap();
        let ui = UiState::launch_home(0, 9);
        let p = UiPoint { x: 400, y: 400 };
        let contact = kindle_platform::TouchContact {
            x: Some(p.x as i32),
            y: Some(p.y as i32),
            ..kindle_platform::TouchContact::default()
        };
        let mut t = FingerTracker::default();
        t.on_frame(&[contact], &m, 10, false, &ui, &g);
        let act = t.on_frame(&[], &m, 90, false, &ui, &g).unwrap();
        assert!(matches!(act, FingerAction::Tap(_)));
        let first = LaunchDiag::classify_for_action(&mut t, act);
        assert_eq!(first.unwrap().dur_ms, 80);
        assert!(t.last_classify().is_none());
        let second = LaunchDiag::classify_for_action(&mut t, act);
        assert!(second.is_none(), "second take must not reuse");
        t.on_frame(&[contact], &m, 10, false, &ui, &g);
        t.on_frame(&[], &m, 90, false, &ui, &g);
        let stale = LaunchDiag::classify_for_action(&mut t, FingerAction::LassoCancelled);
        assert!(stale.is_none());
        assert!(t.last_classify().is_none());
    }

    #[test]
    fn gesture_clock_ok_vs_ambiguous() {
        let t0 = Instant::now();
        let mut d = LaunchDiag::new();
        let down = frame_n(1);
        d.record_gesture(t0, &down);
        let mut up = frame_n(0);
        up.lifts = vec![TouchLift {
            slot: 0,
            tracking_id: 0,
            x: Some(1),
            y: Some(2),
        }];
        d.record_gesture(t0 + Duration::from_millis(50), &up);
        assert_eq!(d.correlate_label(), "ok");

        let mut d2 = LaunchDiag::new();
        d2.record_gesture(t0, &frame_n(2));
        let mut up2 = frame_n(0);
        up2.lifts = vec![
            TouchLift {
                slot: 0,
                tracking_id: 0,
                x: None,
                y: None,
            },
            TouchLift {
                slot: 1,
                tracking_id: 1,
                x: None,
                y: None,
            },
        ];
        d2.record_gesture(t0 + Duration::from_millis(10), &up2);
        assert_eq!(d2.correlate_label(), "ambiguous");
    }

    #[test]
    fn stats_emit_once_after_window_or_shutdown() {
        let mut d = LaunchDiag::new();
        d.maybe_emit_stats_at(WINDOW_MS);
        assert!(!d.stats_emitted());
        d.maybe_emit_stats_at(WINDOW_MS + 1);
        assert!(d.stats_emitted());
        d.maybe_emit_stats_at(WINDOW_MS + 50);
        d.emit_stats_at_shutdown();
        assert!(d.stats_emitted());

        let mut d2 = LaunchDiag::new();
        d2.emit_stats_at_shutdown();
        assert!(d2.stats_emitted());
        d2.emit_stats_at_shutdown();
    }
}
