//! Evdev `EV_KEY` / `EV_ABS` / `EV_SYN` -> [`StylusEvent`] state machine.
//!
//! # Semantics
//!
//! Linux pen digitizers report three independent things, and conflating them is the classic
//! stylus bug:
//!
//! | Codes | Meaning | Draws? |
//! |-------|---------|--------|
//! | `BTN_TOOL_PEN` (0x140), `BTN_TOOL_RUBBER` (0x141) | which tool is **in proximity** | no |
//! | `BTN_TOUCH` (0x14a) | nib is **in contact** with the surface | yes |
//! | `BTN_STYLUS` (0x14b), `BTN_STYLUS2` (0x14c) | barrel / side buttons | no |
//! | `ABS_X/Y/PRESSURE/DISTANCE/TILT_*` | sample values | only as the payload of the above |
//!
//! Only a `BTN_TOUCH` 0 -> 1 transition produces [`StylusEventKind::Down`]. A tool entering
//! proximity produces hover at most.
//!
//! # Framing
//!
//! Events are emitted on `SYN_REPORT`, which is the kernel's own frame boundary. Everything
//! between two `SYN_REPORT`s is one atomic frame, so the machine is insensitive to the order in
//! which a frame reports `BTN_TOUCH`, `ABS_X` and `ABS_PRESSURE`. At most one stylus event is
//! produced per frame, with priority Down > Up > Move > Hover.
//!
//! # `SYN_DROPPED` (fail-closed recovery — not a full evdev resync)
//!
//! When the kernel signals `SYN_DROPPED`, the client event queue lost records. This pure state
//! machine **does not** claim to reconstruct authoritative device state: it has no file
//! descriptor and must not invent Down/Up edges across the gap.
//!
//! Lifecycle (the Linux reader owns ioctl queries):
//!
//! ```text
//! SYN_DROPPED
//!   → FeedResult::StreamGap  (stay Desynchronized; do NOT recover yet)
//!   → discard streamed KEY/ABS/… until the next SYN_REPORT
//! SYN_REPORT while Desynchronized
//!   → FeedResult::RecoveryBoundary  (still Desynchronized)
//!   → caller queries EVIOCGKEY/EVIOCGABS, then recover_*()
//! ```
//!
//! `recover_from_snapshot` / `recover_fail_closed` load axes/tools/buttons for subsequent frames
//! but refuse to inherit contact into drawing state: a fresh contact edge is required before the
//! next Down. That is recovery, not "resync."

use crate::axes::StylusAxes;
use crate::evdev_abi::{abs, btn, ev, syn, KernelTimestamp, RawInputEvent};
use crate::input::{BarrelButtons, StylusEvent, StylusEventKind, StylusTool};
use crate::time::ClockDomain;

/// Whether the machine trusts streamed KEY/ABS state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StreamIntegrity {
    /// Ordinary frames may update state and emit stylus events.
    #[default]
    Synchronized,
    /// After `SYN_DROPPED`, until the following [`FeedResult::RecoveryBoundary`] is handled
    /// via an explicit [`EvdevStateMachine::recover_from_snapshot`] /
    /// [`EvdevStateMachine::recover_fail_closed`] call. Streamed KEY/ABS are ignored.
    Desynchronized,
}

/// Result of feeding one raw evdev record.
#[derive(Clone, Debug, PartialEq)]
pub enum FeedResult {
    /// No stylus event and no gap/recovery signal.
    None,
    /// A normal stylus event at a `SYN_REPORT` boundary.
    Event(StylusEvent),
    /// Kernel buffer overrun just observed. Stay [`StreamIntegrity::Desynchronized`].
    /// Do **not** recover yet — discard until [`FeedResult::RecoveryBoundary`].
    /// No Down/Up is invented. Delivered at most once per gap to cancel an active stroke.
    StreamGap { had_active_contact: bool },
    /// First `SYN_REPORT` after `SYN_DROPPED`. State is still `Desynchronized`.
    /// The Linux reader should query current device state and call `recover_*` now.
    RecoveryBoundary,
}

/// Authoritative device sample obtained outside this pure machine (e.g. via evdev ioctls).
///
/// Built by the Linux reader; never fabricated inside [`EvdevStateMachine`].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DeviceSnapshot {
    pub tool_pen: bool,
    pub tool_rubber: bool,
    pub contact: bool,
    pub buttons: BarrelButtons,
    /// Raw ABS_X value, if known.
    pub x: Option<i32>,
    /// Raw ABS_Y value, if known.
    pub y: Option<i32>,
    /// Raw ABS_PRESSURE value, if known.
    pub pressure: Option<i32>,
}

/// Accumulates one evdev frame, then decides what the pen did.
#[derive(Debug, Clone)]
pub struct EvdevStateMachine {
    axes: StylusAxes,
    kernel_clock: ClockDomain,
    integrity: StreamIntegrity,

    // Committed state (survives across frames). Untrusted while Desynchronized.
    tool_pen: bool,
    tool_rubber: bool,
    contact: bool,
    buttons: BarrelButtons,
    x: Option<f32>,
    y: Option<f32>,
    pressure: Option<f32>,
    distance: Option<f32>,
    tilt_x: Option<f32>,
    tilt_y: Option<f32>,

    // Pending state for the frame currently being accumulated.
    pending_tool_pen: Option<bool>,
    pending_tool_rubber: Option<bool>,
    pending_touch: Option<bool>,
    frame_moved: bool,
    frame_time: Option<KernelTimestamp>,

    /// After snapshot / fail-closed recovery, if contact may still be held, wait for a release
    /// before accepting a new Down (avoids inventing a Down from ioctl or ambiguous state).
    awaiting_touch_release: bool,
    /// Whether the gap interrupted an active contact (remembered for fail-closed recovery).
    gap_had_active_contact: bool,
    /// A Down has been emitted for the current contact; cleared on Up / desync.
    down_emitted: bool,
}

impl EvdevStateMachine {
    /// Axis ranges must come from the device (`EVIOCGABS`) or, for host fixtures, from
    /// [`StylusAxes::synthetic`]. There is deliberately no `Default`: an assumed pressure
    /// maximum is exactly the kind of guess this probe must not make.
    pub fn new(axes: StylusAxes) -> Self {
        Self {
            axes,
            kernel_clock: ClockDomain::Unknown,
            integrity: StreamIntegrity::Synchronized,
            tool_pen: false,
            tool_rubber: false,
            contact: false,
            buttons: BarrelButtons::default(),
            x: None,
            y: None,
            pressure: None,
            distance: None,
            tilt_x: None,
            tilt_y: None,
            pending_tool_pen: None,
            pending_tool_rubber: None,
            pending_touch: None,
            frame_moved: false,
            frame_time: None,
            awaiting_touch_release: false,
            gap_had_active_contact: false,
            down_emitted: false,
        }
    }

    /// Declare which clock the kernel timestamps on this node belong to.
    pub fn with_kernel_clock(mut self, domain: ClockDomain) -> Self {
        self.kernel_clock = domain;
        self
    }

    pub fn axes(&self) -> &StylusAxes {
        &self.axes
    }

    pub fn integrity(&self) -> StreamIntegrity {
        self.integrity
    }

    /// Whether a tool (pen or rubber) is currently in proximity.
    pub fn in_proximity(&self) -> bool {
        self.tool_pen || self.tool_rubber
    }

    /// Whether the nib is currently in contact.
    pub fn in_contact(&self) -> bool {
        self.contact
    }

    pub fn tool(&self) -> StylusTool {
        if self.tool_rubber {
            StylusTool::Eraser
        } else if self.tool_pen {
            StylusTool::Pen
        } else {
            StylusTool::None
        }
    }

    pub fn buttons(&self) -> BarrelButtons {
        self.buttons
    }

    /// Feed one decoded kernel record.
    pub fn feed(&mut self, raw: RawInputEvent) -> FeedResult {
        self.feed_parts(raw.ev_type, raw.code, raw.value, Some(raw.time))
    }

    /// Feed a raw `(type, code, value)` triple without a kernel timestamp (fixture-friendly).
    pub fn feed_raw(&mut self, ev_type: u16, code: u16, value: i32) -> FeedResult {
        self.feed_parts(ev_type, code, value, None)
    }

    /// Fail-closed recovery without device queries: return to synchronized idle.
    ///
    /// Does not invent Down or Up. When `require_touch_release` is true (gap interrupted an
    /// active contact, or the caller cannot prove the nib is up), a release must be observed
    /// before the next Down is accepted.
    pub fn recover_fail_closed(&mut self, require_touch_release: bool) {
        self.clear_pending();
        self.clear_committed_drawing_state();
        self.awaiting_touch_release = require_touch_release;
        self.gap_had_active_contact = false;
        self.down_emitted = false;
        self.integrity = StreamIntegrity::Synchronized;
    }

    /// Convenience: fail-closed using whether this gap interrupted active contact.
    pub fn recover_fail_closed_after_gap(&mut self) {
        let require = self.gap_had_active_contact;
        self.recover_fail_closed(require);
    }

    /// Apply an ioctl-sourced snapshot, then return to synchronized mode.
    ///
    /// Loads axes / tool / buttons for subsequent frames. Contact is **not** inherited into
    /// drawing state: if the snapshot says the nib is down, or the gap interrupted contact, we
    /// wait for a touch release before the next Down. No stylus event is emitted here.
    pub fn recover_from_snapshot(&mut self, snap: DeviceSnapshot) {
        self.clear_pending();
        self.tool_pen = snap.tool_pen;
        self.tool_rubber = snap.tool_rubber;
        self.buttons = snap.buttons;
        self.x = snap.x.map(|v| v as f32);
        self.y = snap.y.map(|v| v as f32);
        self.pressure = snap
            .pressure
            .and_then(|v| self.axes.pressure.map(|r| r.normalize(v)));
        self.contact = false;
        self.awaiting_touch_release = snap.contact || self.gap_had_active_contact;
        self.gap_had_active_contact = false;
        self.down_emitted = false;
        self.integrity = StreamIntegrity::Synchronized;
    }

    /// Whether the outstanding gap interrupted active contact (valid while Desynchronized).
    pub fn gap_had_active_contact(&self) -> bool {
        self.gap_had_active_contact
    }

    fn feed_parts(
        &mut self,
        ev_type: u16,
        code: u16,
        value: i32,
        time: Option<KernelTimestamp>,
    ) -> FeedResult {
        if self.integrity == StreamIntegrity::Desynchronized {
            // Discard untrustworthy stream until the next SYN_REPORT, then signal recovery.
            // Do not emit a second StreamGap for another DROPPED while already desynchronized.
            if ev_type == ev::SYN && code == syn::REPORT {
                return FeedResult::RecoveryBoundary;
            }
            return FeedResult::None;
        }

        if let Some(t) = time {
            self.frame_time = Some(t);
        }
        match ev_type {
            ev::KEY => {
                self.handle_key(code, value);
                FeedResult::None
            }
            ev::ABS => {
                self.handle_abs(code, value);
                FeedResult::None
            }
            ev::SYN => match code {
                syn::REPORT => self.end_frame(),
                syn::DROPPED => {
                    let had = self.contact;
                    self.enter_desynchronized(had);
                    FeedResult::StreamGap {
                        had_active_contact: had,
                    }
                }
                _ => FeedResult::None,
            },
            _ => FeedResult::None,
        }
    }

    fn enter_desynchronized(&mut self, had_active_contact: bool) {
        self.clear_pending();
        self.clear_committed_drawing_state();
        self.awaiting_touch_release = false;
        self.gap_had_active_contact = had_active_contact;
        self.down_emitted = false;
        self.integrity = StreamIntegrity::Desynchronized;
    }

    fn clear_pending(&mut self) {
        self.pending_tool_pen = None;
        self.pending_tool_rubber = None;
        self.pending_touch = None;
        self.frame_moved = false;
        self.frame_time = None;
    }

    fn clear_committed_drawing_state(&mut self) {
        self.tool_pen = false;
        self.tool_rubber = false;
        self.contact = false;
        self.buttons = BarrelButtons::default();
        self.x = None;
        self.y = None;
        self.pressure = None;
        self.distance = None;
        self.tilt_x = None;
        self.tilt_y = None;
        self.down_emitted = false;
    }

    fn handle_key(&mut self, code: u16, value: i32) {
        let on = value > 0;
        match code {
            btn::TOOL_PEN => self.pending_tool_pen = Some(on),
            btn::TOOL_RUBBER => self.pending_tool_rubber = Some(on),
            btn::TOUCH => self.pending_touch = Some(on),
            btn::STYLUS => self.buttons.primary = on,
            btn::STYLUS2 => self.buttons.secondary = on,
            _ => {}
        }
    }

    fn handle_abs(&mut self, code: u16, value: i32) {
        match code {
            abs::X => {
                let v = value as f32;
                if self.x != Some(v) {
                    self.frame_moved = true;
                }
                self.x = Some(v);
            }
            abs::Y => {
                let v = value as f32;
                if self.y != Some(v) {
                    self.frame_moved = true;
                }
                self.y = Some(v);
            }
            abs::PRESSURE => {
                self.pressure = self.axes.pressure.map(|r| r.normalize(value));
            }
            abs::DISTANCE => {
                self.distance = self.axes.distance.map(|r| r.normalize(value));
            }
            abs::TILT_X => {
                self.tilt_x = self.axes.tilt_x.map(|r| r.normalize_bipolar(value));
            }
            abs::TILT_Y => {
                self.tilt_y = self.axes.tilt_y.map(|r| r.normalize_bipolar(value));
            }
            _ => {}
        }
    }

    fn end_frame(&mut self) -> FeedResult {
        let was_in_proximity = self.in_proximity();
        let was_in_contact = self.contact;

        if let Some(v) = self.pending_tool_pen.take() {
            self.tool_pen = v;
        }
        if let Some(v) = self.pending_tool_rubber.take() {
            self.tool_rubber = v;
        }
        if let Some(v) = self.pending_touch.take() {
            if self.awaiting_touch_release {
                if !v {
                    self.awaiting_touch_release = false;
                    self.contact = false;
                }
                // Touch still held after a gap: do not invent a Down from inherited contact.
            } else {
                self.contact = v;
            }
        }

        let forced_release = was_in_contact && !self.in_proximity() && self.contact;
        if forced_release {
            self.contact = false;
        }

        let moved = self.frame_moved;
        self.frame_moved = false;
        let have_pos = self.x.is_some() && self.y.is_some();

        // Hardware-confirmed: Down is BTN_TOUCH rising with a known position — never pressure
        // alone, never BTN_TOOL_* alone.
        let kind = if self.contact && have_pos && !self.down_emitted {
            self.down_emitted = true;
            StylusEventKind::Down
        } else if !self.contact && was_in_contact {
            if self.down_emitted {
                self.down_emitted = false;
                StylusEventKind::Up
            } else {
                // Contact never produced a Down (no coordinates); invent nothing.
                return FeedResult::None;
            }
        } else if self.contact && self.down_emitted {
            if moved {
                StylusEventKind::Move
            } else {
                return FeedResult::None;
            }
        } else if self.in_proximity() && (moved || (!was_in_proximity && self.x.is_some())) {
            StylusEventKind::Hover
        } else {
            return FeedResult::None;
        };

        match self.emit(kind) {
            Some(ev) => FeedResult::Event(ev),
            None => FeedResult::None,
        }
    }

    fn emit(&self, kind: StylusEventKind) -> Option<StylusEvent> {
        Some(StylusEvent {
            kind,
            device: notebook_core::DevicePoint::new(
                self.x.unwrap_or(self.axes.x.min as f32),
                self.y.unwrap_or(self.axes.y.min as f32),
            ),
            pressure: self.pressure,
            distance: self.distance,
            tilt_x: self.tilt_x,
            tilt_y: self.tilt_y,
            tool: self.tool(),
            buttons: self.buttons,
            in_proximity: self.in_proximity(),
            kernel_time: self.frame_time,
            kernel_clock: self.kernel_clock,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evdev_abi::{ev, KernelTimestamp, RawInputEvent};

    fn machine() -> EvdevStateMachine {
        EvdevStateMachine::new(StylusAxes::synthetic(1000, 1000, Some(1000)))
    }

    fn key(sm: &mut EvdevStateMachine, code: u16, value: i32) -> FeedResult {
        sm.feed_raw(ev::KEY, code, value)
    }

    fn abs_(sm: &mut EvdevStateMachine, code: u16, value: i32) -> FeedResult {
        sm.feed_raw(ev::ABS, code, value)
    }

    fn syn_(sm: &mut EvdevStateMachine) -> FeedResult {
        sm.feed_raw(ev::SYN, syn::REPORT, 0)
    }

    fn dropped(sm: &mut EvdevStateMachine) -> FeedResult {
        sm.feed_raw(ev::SYN, syn::DROPPED, 0)
    }

    fn xy(sm: &mut EvdevStateMachine, x: i32, y: i32) {
        abs_(sm, abs::X, x);
        abs_(sm, abs::Y, y);
    }

    fn expect_event(r: FeedResult) -> StylusEvent {
        match r {
            FeedResult::Event(e) => e,
            other => panic!("expected Event, got {other:?}"),
        }
    }

    fn expect_none(r: FeedResult) {
        assert!(matches!(r, FeedResult::None), "expected None, got {r:?}");
    }

    #[test]
    fn hover_does_not_draw() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 100, 200);
        let ev = expect_event(syn_(&mut sm));
        assert_eq!(ev.kind, StylusEventKind::Hover);
        assert!(ev.in_proximity);
        assert!(!sm.in_contact());

        xy(&mut sm, 110, 205);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Hover);
        assert!(!sm.in_contact());
    }

    #[test]
    fn tool_pen_without_btn_touch_produces_no_down() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        let first = syn_(&mut sm);
        assert!(
            matches!(first, FeedResult::None)
                || matches!(first, FeedResult::Event(ref e) if e.kind != StylusEventKind::Down)
        );
        for _ in 0..5 {
            xy(&mut sm, 10, 10);
            let ev = syn_(&mut sm);
            assert!(
                matches!(ev, FeedResult::None)
                    || matches!(ev, FeedResult::Event(ref e) if e.kind != StylusEventKind::Down)
            );
        }
        assert!(!sm.in_contact());
    }

    #[test]
    fn btn_touch_begins_contact() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 100, 200);
        syn_(&mut sm);

        key(&mut sm, btn::TOUCH, 1);
        abs_(&mut sm, abs::PRESSURE, 500);
        let down = expect_event(syn_(&mut sm));
        assert_eq!(down.kind, StylusEventKind::Down);
        assert_eq!(down.device.x, 100.0);
        assert_eq!(down.device.y, 200.0);
        assert_eq!(down.pressure, Some(0.5));
        assert!(sm.in_contact());
    }

    #[test]
    fn move_while_in_contact_emits_move() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 100, 200);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        syn_(&mut sm);

        xy(&mut sm, 110, 210);
        let mv = expect_event(syn_(&mut sm));
        assert_eq!(mv.kind, StylusEventKind::Move);
        assert_eq!(mv.device.x, 110.0);
        assert_eq!(mv.device.y, 210.0);
    }

    #[test]
    fn frame_without_motion_while_in_contact_emits_nothing() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 5, 5);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        syn_(&mut sm);
        abs_(&mut sm, abs::PRESSURE, 900);
        expect_none(syn_(&mut sm));
    }

    #[test]
    fn btn_touch_release_emits_up() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 100, 200);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        syn_(&mut sm);
        xy(&mut sm, 120, 220);
        syn_(&mut sm);

        key(&mut sm, btn::TOUCH, 0);
        let up = expect_event(syn_(&mut sm));
        assert_eq!(up.kind, StylusEventKind::Up);
        assert_eq!(up.device.x, 120.0);
        assert!(!sm.in_contact());
        assert!(sm.in_proximity());
    }

    #[test]
    fn leaving_proximity_after_up_does_not_emit_a_second_up() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 1, 1);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 0);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Up);

        key(&mut sm, btn::TOOL_PEN, 0);
        expect_none(syn_(&mut sm));
        assert!(!sm.in_proximity());
        assert_eq!(sm.tool(), StylusTool::None);
    }

    #[test]
    fn full_reference_sequence_produces_exactly_one_stroke() {
        let mut sm = machine();
        let mut kinds = Vec::new();

        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 10, 10);
        if let FeedResult::Event(e) = syn_(&mut sm) {
            kinds.push(e.kind);
        }

        key(&mut sm, btn::TOUCH, 1);
        if let FeedResult::Event(e) = syn_(&mut sm) {
            kinds.push(e.kind);
        }

        for i in 1..4 {
            xy(&mut sm, 10 + i * 5, 10 + i * 5);
            if let FeedResult::Event(e) = syn_(&mut sm) {
                kinds.push(e.kind);
            }
        }

        key(&mut sm, btn::TOUCH, 0);
        if let FeedResult::Event(e) = syn_(&mut sm) {
            kinds.push(e.kind);
        }

        key(&mut sm, btn::TOOL_PEN, 0);
        if let FeedResult::Event(e) = syn_(&mut sm) {
            kinds.push(e.kind);
        }

        use StylusEventKind::*;
        assert_eq!(kinds, vec![Hover, Down, Move, Move, Move, Up]);
    }

    #[test]
    fn rubber_tool_in_proximity_is_not_contact() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_RUBBER, 1);
        xy(&mut sm, 40, 50);
        let ev = expect_event(syn_(&mut sm));
        assert_eq!(ev.kind, StylusEventKind::Hover);
        assert_eq!(ev.tool, StylusTool::Eraser);
        assert!(!sm.in_contact());

        for i in 1..=5 {
            xy(&mut sm, 40 + i, 50 + i);
            assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Hover);
        }
        assert!(!sm.in_contact());
    }

    #[test]
    fn rubber_contact_reports_rubber_tool_on_down() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_RUBBER, 1);
        xy(&mut sm, 7, 8);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        let down = expect_event(syn_(&mut sm));
        assert_eq!(down.kind, StylusEventKind::Down);
        assert_eq!(down.tool, StylusTool::Eraser);
    }

    #[test]
    fn side_buttons_do_not_affect_contact() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 3, 3);
        syn_(&mut sm);

        key(&mut sm, btn::STYLUS, 1);
        let ev = syn_(&mut sm);
        assert!(
            matches!(ev, FeedResult::None)
                || matches!(ev, FeedResult::Event(ref e) if e.kind != StylusEventKind::Down)
        );
        assert!(!sm.in_contact());
        assert!(sm.buttons().primary);

        key(&mut sm, btn::STYLUS2, 1);
        syn_(&mut sm);
        assert!(sm.buttons().secondary);
        assert!(!sm.in_contact());

        key(&mut sm, btn::STYLUS, 0);
        key(&mut sm, btn::STYLUS2, 0);
        syn_(&mut sm);
        assert!(!sm.buttons().any());
    }

    #[test]
    fn button_state_rides_along_on_stroke_events() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        key(&mut sm, btn::STYLUS, 1);
        xy(&mut sm, 1, 1);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        let down = expect_event(syn_(&mut sm));
        assert!(down.buttons.primary);
        assert!(!down.buttons.secondary);
    }

    #[test]
    fn frame_ordering_does_not_matter_within_a_frame() {
        let mut a = machine();
        key(&mut a, btn::TOOL_PEN, 1);
        xy(&mut a, 0, 0);
        syn_(&mut a);
        key(&mut a, btn::TOUCH, 1);
        abs_(&mut a, abs::X, 50);
        abs_(&mut a, abs::Y, 60);
        let ev_a = expect_event(syn_(&mut a));

        let mut b = machine();
        key(&mut b, btn::TOOL_PEN, 1);
        xy(&mut b, 0, 0);
        syn_(&mut b);
        abs_(&mut b, abs::X, 50);
        abs_(&mut b, abs::Y, 60);
        key(&mut b, btn::TOUCH, 1);
        let ev_b = expect_event(syn_(&mut b));

        assert_eq!(ev_a.kind, StylusEventKind::Down);
        assert_eq!(ev_a.kind, ev_b.kind);
        assert_eq!(ev_a.device, ev_b.device);
    }

    #[test]
    fn touch_and_tool_in_the_same_frame_still_yields_down_then_up() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        key(&mut sm, btn::TOUCH, 1);
        xy(&mut sm, 9, 9);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Down);

        key(&mut sm, btn::TOUCH, 0);
        key(&mut sm, btn::TOOL_PEN, 0);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Up);
        expect_none(syn_(&mut sm));
    }

    #[test]
    fn tool_leaving_proximity_while_still_in_contact_forces_one_up() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 2, 2);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        syn_(&mut sm);

        key(&mut sm, btn::TOOL_PEN, 0);
        let up = expect_event(syn_(&mut sm));
        assert_eq!(up.kind, StylusEventKind::Up);
        assert!(!sm.in_contact());
        expect_none(syn_(&mut sm));
    }

    #[test]
    fn pressure_is_none_when_device_reports_no_pressure_axis() {
        let mut sm = EvdevStateMachine::new(StylusAxes::synthetic(100, 100, None));
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 1, 1);
        syn_(&mut sm);
        abs_(&mut sm, abs::PRESSURE, 4096);
        key(&mut sm, btn::TOUCH, 1);
        let down = expect_event(syn_(&mut sm));
        assert!(down.pressure.is_none());
    }

    #[test]
    fn pressure_uses_device_reported_range_not_a_constant() {
        let mut sm = EvdevStateMachine::new(StylusAxes::synthetic(100, 100, Some(255)));
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 1, 1);
        syn_(&mut sm);
        abs_(&mut sm, abs::PRESSURE, 255);
        key(&mut sm, btn::TOUCH, 1);
        assert_eq!(expect_event(syn_(&mut sm)).pressure, Some(1.0));
    }

    #[test]
    fn syn_dropped_discards_partial_frame_and_enters_desynchronized() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 1, 1);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        syn_(&mut sm);
        assert!(sm.in_contact());

        match dropped(&mut sm) {
            FeedResult::StreamGap {
                had_active_contact: true,
            } => {}
            other => panic!("expected StreamGap(contact), got {other:?}"),
        }
        assert_eq!(sm.integrity(), StreamIntegrity::Desynchronized);
        assert!(sm.gap_had_active_contact());
        assert!(!sm.in_contact());
        assert!(!sm.in_proximity());
    }

    #[test]
    fn events_between_dropped_and_report_are_discarded() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 10, 10);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Down);
        xy(&mut sm, 20, 20);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Move);

        assert!(matches!(
            dropped(&mut sm),
            FeedResult::StreamGap {
                had_active_contact: true
            }
        ));
        assert_eq!(sm.integrity(), StreamIntegrity::Desynchronized);

        // Untrustworthy stream between DROPPED and REPORT.
        xy(&mut sm, 999, 888);
        key(&mut sm, btn::TOUCH, 1);
        abs_(&mut sm, abs::PRESSURE, 1000);
        expect_none(sm.feed_raw(ev::MSC, 0x05, 1));

        // Still desynchronized — recovery has not happened.
        assert_eq!(sm.integrity(), StreamIntegrity::Desynchronized);
        assert!(!sm.in_contact());
        assert!(sm.x.is_none());

        match syn_(&mut sm) {
            FeedResult::RecoveryBoundary => {}
            other => panic!("expected RecoveryBoundary, got {other:?}"),
        }
        // Boundary alone does not recover; caller must call recover_*.
        assert_eq!(sm.integrity(), StreamIntegrity::Desynchronized);
    }

    #[test]
    fn recovery_does_not_happen_at_syn_dropped() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 1, 1);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        syn_(&mut sm);
        dropped(&mut sm);
        assert_eq!(sm.integrity(), StreamIntegrity::Desynchronized);

        // Intermediate traffic discarded; boundary does not auto-recover.
        xy(&mut sm, 5, 5);
        assert!(matches!(syn_(&mut sm), FeedResult::RecoveryBoundary));
        assert_eq!(sm.integrity(), StreamIntegrity::Desynchronized);
    }

    #[test]
    fn recovery_happens_only_after_explicit_call_at_boundary() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 1, 1);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        syn_(&mut sm);
        dropped(&mut sm);
        xy(&mut sm, 9, 9);
        assert!(matches!(syn_(&mut sm), FeedResult::RecoveryBoundary));

        sm.recover_from_snapshot(DeviceSnapshot {
            tool_pen: true,
            contact: false,
            x: Some(40),
            y: Some(50),
            ..DeviceSnapshot::default()
        });
        assert_eq!(sm.integrity(), StreamIntegrity::Synchronized);
        assert!(!sm.in_contact());
        assert_eq!(sm.x, Some(40.0));
        // Snapshot application invents no stylus event — feed returned RecoveryBoundary earlier.
    }

    #[test]
    fn pen_still_down_across_gap_requires_release_before_down() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 1, 1);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        syn_(&mut sm);
        dropped(&mut sm);
        assert!(matches!(syn_(&mut sm), FeedResult::RecoveryBoundary));

        sm.recover_from_snapshot(DeviceSnapshot {
            tool_pen: true,
            contact: true,
            x: Some(40),
            y: Some(50),
            pressure: Some(500),
            ..DeviceSnapshot::default()
        });
        assert_eq!(sm.integrity(), StreamIntegrity::Synchronized);
        assert!(!sm.in_contact());

        xy(&mut sm, 41, 51);
        match syn_(&mut sm) {
            FeedResult::None => {}
            FeedResult::Event(e) => {
                assert_eq!(e.kind, StylusEventKind::Hover);
                assert!(!sm.in_contact());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(!sm.in_contact());

        key(&mut sm, btn::TOUCH, 0);
        expect_none(syn_(&mut sm));
        assert!(!sm.in_contact());

        key(&mut sm, btn::TOUCH, 1);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Down);
    }

    #[test]
    fn fail_closed_after_active_contact_requires_release() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 1, 1);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        syn_(&mut sm);
        dropped(&mut sm);
        assert!(matches!(syn_(&mut sm), FeedResult::RecoveryBoundary));

        sm.recover_fail_closed_after_gap();
        assert_eq!(sm.integrity(), StreamIntegrity::Synchronized);
        assert!(!sm.in_contact());

        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 20, 30);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Hover);

        // Ambiguous held contact: no immediate Down.
        key(&mut sm, btn::TOUCH, 1);
        expect_none(syn_(&mut sm));
        assert!(!sm.in_contact());

        key(&mut sm, btn::TOUCH, 0);
        expect_none(syn_(&mut sm));
        key(&mut sm, btn::TOUCH, 1);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Down);
        xy(&mut sm, 25, 35);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Move);
        key(&mut sm, btn::TOUCH, 0);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Up);
    }

    #[test]
    fn gap_while_idle_then_clean_contact_works() {
        let mut sm = machine();
        assert!(matches!(
            dropped(&mut sm),
            FeedResult::StreamGap {
                had_active_contact: false
            }
        ));
        assert!(!sm.gap_had_active_contact());
        assert!(matches!(syn_(&mut sm), FeedResult::RecoveryBoundary));
        sm.recover_fail_closed_after_gap();

        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 20, 30);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Hover);
        key(&mut sm, btn::TOUCH, 1);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Down);
        key(&mut sm, btn::TOUCH, 0);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Up);
    }

    #[test]
    fn btn_touch_without_coordinates_does_not_emit_down() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        syn_(&mut sm);
        key(&mut sm, btn::TOUCH, 1);
        expect_none(syn_(&mut sm));
        assert!(sm.in_contact());
        // Coordinates arrive on a later frame → Down (not Move).
        xy(&mut sm, 50, 60);
        assert_eq!(expect_event(syn_(&mut sm)).kind, StylusEventKind::Down);
    }

    #[test]
    fn pressure_alone_never_creates_down() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 1, 1);
        syn_(&mut sm);
        abs_(&mut sm, abs::PRESSURE, 4000);
        expect_none(syn_(&mut sm));
        assert!(!sm.in_contact());
    }

    #[test]
    fn side_button_survives_hover_and_contact() {
        let mut sm = machine();
        key(&mut sm, btn::TOOL_PEN, 1);
        key(&mut sm, btn::STYLUS, 1);
        xy(&mut sm, 10, 10);
        let hover = expect_event(syn_(&mut sm));
        assert_eq!(hover.kind, StylusEventKind::Hover);
        assert!(hover.buttons.primary);

        key(&mut sm, btn::TOUCH, 1);
        let down = expect_event(syn_(&mut sm));
        assert!(down.buttons.primary);
        key(&mut sm, btn::STYLUS, 0);
        xy(&mut sm, 11, 11);
        let mv = expect_event(syn_(&mut sm));
        assert!(!mv.buttons.primary);
    }

    #[test]
    fn distance_and_tilt_are_optional_and_do_not_affect_contact() {
        let mut sm = EvdevStateMachine::new(StylusAxes::scribe_wacom_confirmed());
        key(&mut sm, btn::TOOL_PEN, 1);
        xy(&mut sm, 100, 200);
        abs_(&mut sm, abs::DISTANCE, 128);
        abs_(&mut sm, abs::TILT_X, 0);
        abs_(&mut sm, abs::TILT_Y, 9000);
        let hover = expect_event(syn_(&mut sm));
        assert_eq!(hover.kind, StylusEventKind::Hover);
        assert!((hover.distance.unwrap() - 0.5).abs() < 0.01);
        assert!(hover.tilt_x.unwrap().abs() < 0.01);
        assert!((hover.tilt_y.unwrap() - 1.0).abs() < 0.01);
        assert!(!sm.in_contact());
    }

    #[test]
    fn kernel_timestamp_and_domain_ride_along() {
        let mut sm = machine().with_kernel_clock(ClockDomain::LinuxMonotonic);
        sm.feed(RawInputEvent {
            time: KernelTimestamp {
                tv_sec: 12,
                tv_usec: 500_000,
            },
            ev_type: ev::KEY,
            code: btn::TOOL_PEN,
            value: 1,
        });
        sm.feed(RawInputEvent {
            time: KernelTimestamp {
                tv_sec: 12,
                tv_usec: 500_000,
            },
            ev_type: ev::ABS,
            code: abs::X,
            value: 33,
        });
        let ev = expect_event(sm.feed(RawInputEvent {
            time: KernelTimestamp {
                tv_sec: 12,
                tv_usec: 500_000,
            },
            ev_type: ev::SYN,
            code: syn::REPORT,
            value: 0,
        }));
        assert_eq!(ev.kernel_t_us(), Some(12_500_000));
        assert_eq!(ev.kernel_clock, ClockDomain::LinuxMonotonic);
    }

    #[test]
    fn unknown_event_types_are_ignored() {
        let mut sm = machine();
        expect_none(sm.feed_raw(ev::MSC, 0x05, 999));
        expect_none(sm.feed_raw(0x15, 0, 1));
    }
}
