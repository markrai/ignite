//! Finger input: UI taps, notebook page swipes, and long-press lasso ownership (never ink).
//!
//! Pure, host-testable policy over [`kindle_platform::TouchFrame`]s from the read-only
//! touchscreen observer. Finger input resolves to the *same* `UiState::on_tap` hit targets as
//! the pen, to the existing `UiCommand::NextPage` / `PreviousPage`, or to explicit transient
//! lasso/selection actions handled by the runtime. It never enters the pen or eraser state.
//!
//! Priority: display validity → stylus activity suppression → modal → notebook chrome →
//! Home → notebook content swipe.
//!
//! F11.3 palm rule: while the stylus is in proximity (`BTN_TOOL_PEN`/`BTN_TOOL_RUBBER`) or
//! touching, every finger action is suppressed, and any touch contact that overlapped that
//! state at any point stays suppressed until *all* contacts lift ([`TouchArbiter`]).

use kindle_platform::{FbGeometry, StylusEventKind, TouchContact};

use crate::app_ui::UiState;
use crate::ui_gfx::{Rect, UiPoint};

/// Max distance (panel px) from the Down point for a contact to remain a tap. FW 5.18.6 taps
/// travelled ≤ 15 px (one 30 px) in the recorded trace.
pub const TAP_MAX_TRAVEL_PX: u32 = 50;
/// Stationary hold required before a content touch is claimed for lasso.
pub const LASSO_LONG_PRESS_MS: u64 = 450;
/// Recorded taps lasted 12–140 ms.
pub const TAP_MAX_MS: u64 = 700;
/// Recorded deliberate swipes covered 520–900 px horizontally.
pub const SWIPE_MIN_DX_PX: u32 = 350;
/// Recorded slow swipe: ~960 ms.
pub const SWIPE_MAX_MS: u64 = 1200;
/// Horizontal edge zones reserved for Kindle: contacts starting here never page-swipe.
/// 8% (149 px): recorded natural right-to-left swipes started at x = 1547–1688.
pub const EDGE_FRACTION_PERCENT: u32 = 8;
/// Touch stays suppressed this long after the last stylus event. The stylus parser reports
/// proximity while the pen is in range (Hover) but emits no event when it leaves, so recent
/// stylus activity is the proximity signal.
pub const STYLUS_QUIET_MS: u64 = 700;
/// Fail-safe for a missing proximity exit edge: a proximity report older than this (no stylus
/// event since) stops suppressing on its own. Contacts that overlapped it stay poisoned.
pub const PROXIMITY_STALE_MS: u64 = 10_000;

/// Device MT position range → panel pixels. Fails closed on degenerate or out-of-range input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TouchMapping {
    x: (i32, i32),
    y: (i32, i32),
    panel_w: u32,
    panel_h: u32,
}

impl TouchMapping {
    pub fn new(x: (i32, i32), y: (i32, i32), geo: &FbGeometry) -> Result<Self, String> {
        if x.1 <= x.0 || y.1 <= y.0 || geo.xres == 0 || geo.yres == 0 {
            return Err(format!(
                "REFUSE TOUCH: degenerate mapping x={x:?} y={y:?} panel={}x{}",
                geo.xres, geo.yres
            ));
        }
        Ok(Self {
            x,
            y,
            panel_w: geo.xres,
            panel_h: geo.yres,
        })
    }

    fn axis(v: i32, (lo, hi): (i32, i32), out: u32) -> Option<u32> {
        if v < lo || v > hi {
            return None;
        }
        let num = i64::from(v - lo) * i64::from(out - 1);
        u32::try_from(num / i64::from(hi - lo)).ok()
    }

    /// `None` for a coordinate outside the device-reported range (malformed report).
    pub fn map(&self, x: i32, y: i32) -> Option<UiPoint> {
        Some(UiPoint {
            x: Self::axis(x, self.x, self.panel_w)?,
            y: Self::axis(y, self.y, self.panel_h)?,
        })
    }
}

/// Palm/stylus arbitration: stylus writing safety always wins over finger input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StylusActivity {
    contact: bool,
    last_ms: Option<u64>,
    /// Explicit tool proximity (`BTN_TOOL_*`), when the source reports it.
    in_proximity: bool,
}

impl StylusActivity {
    /// Latest sampled tool proximity (`None` = unknown; leaves the flag unchanged). Leaving
    /// proximity starts the [`STYLUS_QUIET_MS`] window, like any other stylus activity.
    pub fn on_proximity(&mut self, sample: Option<bool>, now_ms: u64) {
        let Some(p) = sample else {
            return;
        };
        if p != self.in_proximity {
            self.last_ms = Some(now_ms);
        }
        self.in_proximity = p;
    }

    #[cfg(test)]
    pub fn in_proximity(&self) -> bool {
        self.in_proximity
    }

    pub fn on_stylus_event(&mut self, kind: StylusEventKind, now_ms: u64) {
        match kind {
            StylusEventKind::Down | StylusEventKind::Move => self.contact = true,
            StylusEventKind::Up => self.contact = false,
            StylusEventKind::Hover => {}
        }
        self.last_ms = Some(now_ms);
    }

    pub fn on_stream_gap(&mut self, now_ms: u64) {
        self.contact = false;
        self.last_ms = Some(now_ms);
    }

    pub fn suppresses_touch(&self, now_ms: u64) -> bool {
        let since = self.last_ms.map(|t| now_ms.saturating_sub(t));
        self.contact
            || since.is_some_and(|d| d < STYLUS_QUIET_MS)
            || (self.in_proximity && since.is_none_or(|d| d < PROXIMITY_STALE_MS))
    }
}

/// Where a finger contact started; decides which gestures it may produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FingerOrigin {
    /// Modal, Home, or notebook chrome: tap semantics only.
    Ui,
    /// Notebook writing area: tap (no-op) or page swipe.
    Content,
    /// Notebook left/right edge zone: reserved for Kindle; never a page swipe.
    Edge,
}

pub fn classify_origin(ui: &UiState, geo: &FbGeometry, p: UiPoint) -> FingerOrigin {
    // Hidden chrome has no hit targets: its bands are page content (F11.3).
    // Every non-notebook view (Home, Sync, Tools) is UI: taps only, never page swipes.
    if ui.modal_open() || !ui.view.allows_page_input() || ui.chrome_hit(geo, p).is_some() {
        return FingerOrigin::Ui;
    }
    let edge = geo.xres * EDGE_FRACTION_PERCENT / 100;
    if p.x < edge || p.x >= geo.xres.saturating_sub(edge) {
        FingerOrigin::Edge
    } else {
        FingerOrigin::Content
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FingerAction {
    /// Resolve through `UiState::on_tap` exactly like a pen tap.
    Tap(UiPoint),
    NextPage,
    PreviousPage,
    /// Long-press claimed this content touch; the point is the original Down location.
    LassoStarted(UiPoint),
    LassoPoint(UiPoint),
    LassoFinished(UiPoint),
    LassoCancelled,
    /// A selected bounding box owns this touch immediately (no second long-press).
    SelectionMoveStarted(UiPoint),
    SelectionMovePoint(UiPoint),
    SelectionMoveFinished(UiPoint),
    SelectionMoveCancelled,
    /// Down landed outside an active selection. The same contact cannot start another lasso.
    SelectionCleared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Tracking {
        slot: usize,
        origin: FingerOrigin,
        start: UiPoint,
        last: UiPoint,
        start_ms: u64,
        max_travel: u32,
        lasso_candidate: bool,
        suppress_tap: bool,
    },
    LassoDrawing {
        slot: usize,
        last: UiPoint,
    },
    MovingSelection {
        slot: usize,
        last: UiPoint,
    },
    /// Multi-touch, palm, blocked, or malformed: ignore everything until all contacts lift.
    CancelledUntilAllUp,
}

/// Exact `now_ms` / duration values FingerTracker used on the last lift classify.
///
/// `classified_as_tap` follows `dur_ms <= TAP_MAX_MS`. If `start_now_ms` and
/// `up_now_ms` were both saturated to 0, that flag does **not** prove a physically
/// short contact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FingerClassifyDiag {
    pub start_now_ms: u64,
    pub up_now_ms: u64,
    pub dur_ms: u64,
    pub max_travel: u32,
    pub classified_as_tap: bool,
    pub tap_max_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FingerTracker {
    state: State,
    /// Any contact currently down (including one whose position has not arrived yet).
    contact_down: bool,
    last_classify: Option<FingerClassifyDiag>,
}

impl Default for FingerTracker {
    fn default() -> Self {
        Self {
            state: State::Idle,
            contact_down: false,
            last_classify: None,
        }
    }
}

fn distance(a: UiPoint, b: UiPoint) -> u32 {
    let dx = f64::from(a.x) - f64::from(b.x);
    let dy = f64::from(a.y) - f64::from(b.y);
    (dx * dx + dy * dy).sqrt() as u32
}

impl FingerTracker {
    #[cfg(test)]
    pub fn is_idle(&self) -> bool {
        self.state == State::Idle
    }

    /// Last lift-classify inputs (diagnostic). Does not affect subsequent gestures.
    pub fn last_classify(&self) -> Option<FingerClassifyDiag> {
        self.last_classify
    }

    /// Take the classify snapshot for the gesture that just lifted. Clears so a later
    /// action cannot reuse it.
    pub fn take_last_classify(&mut self) -> Option<FingerClassifyDiag> {
        self.last_classify.take()
    }

    /// Input discontinuity (touch `SYN_DROPPED`, sleep): forget the gesture, no action.
    pub fn cancel(&mut self) {
        let _ = self.cancel_with_action();
    }

    fn cancel_with_action(&mut self) -> Option<FingerAction> {
        let action = self.cancel_action();
        self.state = State::CancelledUntilAllUp;
        action
    }

    /// Stylus suppression observed: every contact currently down is poisoned until all lift,
    /// even if the stylus later leaves. No-op with no finger down.
    pub fn poison_active(&mut self) {
        let _ = self.poison_active_with_action();
    }

    fn poison_active_with_action(&mut self) -> Option<FingerAction> {
        if self.contact_down || self.state != State::Idle {
            let action = self.cancel_action();
            self.state = State::CancelledUntilAllUp;
            return action;
        }
        None
    }

    #[cfg(test)]
    pub fn is_suppressed(&self) -> bool {
        self.state == State::CancelledUntilAllUp
    }

    /// Feed one touch frame. `blocked` = display invalid or stylus active; a gesture touched by
    /// a blocked frame at any point is cancelled. Returns at most one transition/frame action.
    pub fn on_frame(
        &mut self,
        contacts: &[TouchContact],
        mapping: &TouchMapping,
        now_ms: u64,
        blocked: bool,
        ui: &UiState,
        geo: &FbGeometry,
    ) -> Option<FingerAction> {
        self.on_frame_with_selection(contacts, mapping, now_ms, blocked, ui, geo, None)
    }

    /// Feed one frame with the current transient selection bounds in panel coordinates.
    /// The bounds are UI state only; they never become document geometry.
    #[allow(clippy::too_many_arguments)]
    pub fn on_frame_with_selection(
        &mut self,
        contacts: &[TouchContact],
        mapping: &TouchMapping,
        now_ms: u64,
        blocked: bool,
        ui: &UiState,
        geo: &FbGeometry,
        selection_bounds: Option<Rect>,
    ) -> Option<FingerAction> {
        self.contact_down = !contacts.is_empty();
        if contacts.is_empty() {
            let ended = std::mem::replace(&mut self.state, State::Idle);
            return match ended {
                State::Tracking {
                    origin,
                    start,
                    last,
                    start_ms,
                    max_travel,
                    suppress_tap,
                    ..
                } if !blocked && !suppress_tap => {
                    let (action, diag) =
                        classify(origin, start, last, start_ms, max_travel, now_ms, ui);
                    self.last_classify = Some(diag);
                    action
                }
                State::LassoDrawing { last, .. } if !blocked => {
                    Some(FingerAction::LassoFinished(last))
                }
                State::MovingSelection { last, .. } if !blocked => {
                    Some(FingerAction::SelectionMoveFinished(last))
                }
                _ => None,
            };
        }
        // Second finger / stylus / display-invalid always abort. Same-slot palm is handled
        // per-state: Tracking/Idle cancel, but an established move/lasso keeps the last good
        // point. The Scribe digitizer reclassifies large contacts as MT_TOOL_PALM shortly
        // before lift; treating that as Cancel snaps a live selection back to the drag origin.
        if blocked || contacts.len() > 1 {
            let action = self.cancel_action();
            self.state = State::CancelledUntilAllUp;
            return action;
        }
        let c = contacts[0];
        let established = matches!(
            self.state,
            State::MovingSelection { .. } | State::LassoDrawing { .. }
        );
        if c.palm && !established {
            let action = self.cancel_action();
            self.state = State::CancelledUntilAllUp;
            return action;
        }
        let (Some(x), Some(y)) = (c.x, c.y) else {
            // First frame of a contact may precede its position; wait for coordinates.
            return None;
        };
        let Some(p) = mapping.map(x, y) else {
            if established {
                return None;
            }
            self.state = State::CancelledUntilAllUp;
            return None;
        };
        match &mut self.state {
            State::Idle => {
                let origin = classify_origin(ui, geo, p);
                if origin == FingerOrigin::Content {
                    if let Some(bounds) = selection_bounds {
                        if bounds.contains(p.x, p.y) {
                            self.state = State::MovingSelection {
                                slot: c.slot,
                                last: p,
                            };
                            return Some(FingerAction::SelectionMoveStarted(p));
                        }
                        self.state = State::Tracking {
                            slot: c.slot,
                            origin,
                            start: p,
                            last: p,
                            start_ms: now_ms,
                            max_travel: 0,
                            lasso_candidate: false,
                            suppress_tap: true,
                        };
                        return Some(FingerAction::SelectionCleared);
                    }
                }
                self.state = State::Tracking {
                    slot: c.slot,
                    origin,
                    start: p,
                    last: p,
                    start_ms: now_ms,
                    max_travel: 0,
                    lasso_candidate: origin == FingerOrigin::Content,
                    suppress_tap: false,
                };
            }
            State::Tracking {
                slot,
                start,
                last,
                max_travel,
                lasso_candidate,
                ..
            } => {
                if *slot != c.slot {
                    self.state = State::CancelledUntilAllUp;
                    return None;
                }
                *last = p;
                *max_travel = (*max_travel).max(distance(*start, p));
                if *max_travel > TAP_MAX_TRAVEL_PX {
                    *lasso_candidate = false;
                }
            }
            State::LassoDrawing { slot, last } => {
                if *slot != c.slot {
                    self.state = State::CancelledUntilAllUp;
                    return Some(FingerAction::LassoCancelled);
                }
                if c.palm {
                    return None;
                }
                *last = p;
                return Some(FingerAction::LassoPoint(p));
            }
            State::MovingSelection { slot, last } => {
                if *slot != c.slot {
                    self.state = State::CancelledUntilAllUp;
                    return Some(FingerAction::SelectionMoveCancelled);
                }
                if c.palm {
                    return None;
                }
                *last = p;
                return Some(FingerAction::SelectionMovePoint(p));
            }
            State::CancelledUntilAllUp => {}
        }
        None
    }

    /// Timer edge for a stationary content contact. The runtime calls this even when evdev has
    /// produced no new frame, so activation occurs at the threshold rather than on first drag.
    pub fn on_tick(&mut self, now_ms: u64, blocked: bool) -> Option<FingerAction> {
        if blocked {
            return self.poison_active_with_action();
        }
        let State::Tracking {
            slot,
            origin,
            start,
            start_ms,
            max_travel,
            lasso_candidate,
            ..
        } = self.state
        else {
            return None;
        };
        if origin != FingerOrigin::Content
            || !lasso_candidate
            || max_travel > TAP_MAX_TRAVEL_PX
            || now_ms.saturating_sub(start_ms) < LASSO_LONG_PRESS_MS
        {
            return None;
        }
        self.state = State::LassoDrawing { slot, last: start };
        Some(FingerAction::LassoStarted(start))
    }

    fn cancel_action(&self) -> Option<FingerAction> {
        match self.state {
            State::LassoDrawing { .. } => Some(FingerAction::LassoCancelled),
            State::MovingSelection { .. } => Some(FingerAction::SelectionMoveCancelled),
            _ => None,
        }
    }
}

fn classify(
    origin: FingerOrigin,
    start: UiPoint,
    last: UiPoint,
    start_ms: u64,
    max_travel: u32,
    now_ms: u64,
    ui: &UiState,
) -> (Option<FingerAction>, FingerClassifyDiag) {
    let dur_ms = now_ms.saturating_sub(start_ms);
    let classified_as_tap = max_travel <= TAP_MAX_TRAVEL_PX && dur_ms <= TAP_MAX_MS;
    let diag = FingerClassifyDiag {
        start_now_ms: start_ms,
        up_now_ms: now_ms,
        dur_ms,
        max_travel,
        classified_as_tap,
        tap_max_ms: TAP_MAX_MS,
    };
    if classified_as_tap {
        // Fired at lift, using the Down position (same hit point semantics as the pen).
        return (Some(FingerAction::Tap(start)), diag);
    }
    let dx = i64::from(last.x) - i64::from(start.x);
    let dy = i64::from(last.y) - i64::from(start.y);
    let horizontal = dx.unsigned_abs() >= u64::from(SWIPE_MIN_DX_PX)
        && dx.unsigned_abs() >= 2 * dy.unsigned_abs()
        && dur_ms <= SWIPE_MAX_MS;
    if origin == FingerOrigin::Content && horizontal && ui.allows_page_input() {
        // Page pushed aside: swipe left → next, swipe right → previous.
        return (
            Some(if dx < 0 {
                FingerAction::NextPage
            } else {
                FingerAction::PreviousPage
            }),
            diag,
        );
    }
    // Drag from UI beyond tap tolerance, edge-origin, vertical/diagonal/short: no action.
    (None, diag)
}

/// Finger tracker + stylus arbitration as one host-testable unit (the runtime's only touch
/// policy entry point). Implements the F11.3 palm rule:
///
/// ```text
/// stylus in proximity / touching / quiet window  → no finger action at all
/// contact down at any moment of that state       → SuppressedByStylus until all contacts lift
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TouchArbiter {
    pub tracker: FingerTracker,
    pub stylus: StylusActivity,
    pending_cancel: Option<FingerAction>,
}

impl TouchArbiter {
    fn poison_if_suppressed(&mut self, now_ms: u64) {
        if self.stylus.suppresses_touch(now_ms) {
            self.pending_cancel = self
                .pending_cancel
                .or_else(|| self.tracker.poison_active_with_action());
        }
    }

    pub fn on_stylus_event(&mut self, kind: StylusEventKind, in_proximity: bool, now_ms: u64) {
        self.stylus.on_stylus_event(kind, now_ms);
        self.stylus.on_proximity(Some(in_proximity), now_ms);
        // Any stylus event means the pen is near: poison contacts already down.
        self.pending_cancel = self
            .pending_cancel
            .or_else(|| self.tracker.poison_active_with_action());
    }

    pub fn on_stylus_gap(&mut self, now_ms: u64) {
        self.stylus.on_stream_gap(now_ms);
        self.pending_cancel = self
            .pending_cancel
            .or_else(|| self.tracker.poison_active_with_action());
    }

    /// Periodic proximity sample (the parser emits no event when the pen leaves range).
    pub fn on_proximity(&mut self, sample: Option<bool>, now_ms: u64) {
        self.stylus.on_proximity(sample, now_ms);
        self.poison_if_suppressed(now_ms);
    }

    #[cfg(test)]
    pub fn suppresses_touch(&self, now_ms: u64) -> bool {
        self.stylus.suppresses_touch(now_ms)
    }

    pub fn cancel(&mut self) {
        self.pending_cancel = self
            .pending_cancel
            .or_else(|| self.tracker.cancel_with_action());
    }

    pub fn take_cancelled_action(&mut self) -> Option<FingerAction> {
        self.pending_cancel.take()
    }

    /// Feed one touch frame. `display_valid == false` (F11.1) blocks like stylus suppression.
    #[allow(clippy::too_many_arguments)]
    pub fn on_frame(
        &mut self,
        contacts: &[TouchContact],
        mapping: &TouchMapping,
        now_ms: u64,
        display_valid: bool,
        ui: &UiState,
        geo: &FbGeometry,
    ) -> Option<FingerAction> {
        if let Some(action) = self.pending_cancel.take() {
            return Some(action);
        }
        let blocked = !display_valid || self.stylus.suppresses_touch(now_ms);
        self.tracker
            .on_frame(contacts, mapping, now_ms, blocked, ui, geo)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_frame_with_selection(
        &mut self,
        contacts: &[TouchContact],
        mapping: &TouchMapping,
        now_ms: u64,
        display_valid: bool,
        ui: &UiState,
        geo: &FbGeometry,
        selection_bounds: Option<Rect>,
    ) -> Option<FingerAction> {
        if let Some(action) = self.pending_cancel.take() {
            return Some(action);
        }
        let blocked = !display_valid || self.stylus.suppresses_touch(now_ms);
        self.tracker.on_frame_with_selection(
            contacts,
            mapping,
            now_ms,
            blocked,
            ui,
            geo,
            selection_bounds,
        )
    }

    pub fn on_tick(&mut self, now_ms: u64, display_valid: bool) -> Option<FingerAction> {
        if let Some(action) = self.pending_cancel.take() {
            return Some(action);
        }
        let blocked = !display_valid || self.stylus.suppresses_touch(now_ms);
        self.tracker.on_tick(now_ms, blocked)
    }
}

/// Presentation-layer coalescing for an in-progress selection move.
///
/// Raw [`TouchFrame`]s still all flow through [`TouchArbiter`]. This only defers
/// `SelectionMovePoint` preview paint so Finish/Cancel are never queued behind stale DU work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MovePreviewCoalesce {
    pending: Option<UiPoint>,
    pub moves_seen: u32,
    pub coalesced: u32,
    pub previews_rendered: u32,
}

/// What the runtime should do with one arbiter action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoalesceDecision {
    /// Keep draining. Apply no preview paint yet.
    Hold,
    /// Finish/Cancel: pending preview discarded; handle immediately.
    Supersede(FingerAction),
    /// Handle now. Caller must [`MovePreviewCoalesce::take_pending_preview`] first.
    Handle(FingerAction),
}

impl MovePreviewCoalesce {
    pub fn ingest(&mut self, action: FingerAction) -> CoalesceDecision {
        match action {
            FingerAction::SelectionMovePoint(p) => {
                if self.pending.is_some() {
                    self.coalesced = self.coalesced.saturating_add(1);
                }
                self.moves_seen = self.moves_seen.saturating_add(1);
                self.pending = Some(p);
                CoalesceDecision::Hold
            }
            FingerAction::SelectionMoveFinished(_) | FingerAction::SelectionMoveCancelled => {
                if self.pending.take().is_some() {
                    self.coalesced = self.coalesced.saturating_add(1);
                }
                CoalesceDecision::Supersede(action)
            }
            FingerAction::SelectionMoveStarted(_) => {
                *self = Self::default();
                CoalesceDecision::Handle(action)
            }
            other => CoalesceDecision::Handle(other),
        }
    }

    /// Latest deferred move, if any. Counts as one rendered preview.
    pub fn take_pending_preview(&mut self) -> Option<UiPoint> {
        let p = self.pending.take()?;
        self.previews_rendered = self.previews_rendered.saturating_add(1);
        Some(p)
    }

    /// Drop a pending preview without painting (pen takeover / end of gesture).
    pub fn discard_pending(&mut self) -> bool {
        if self.pending.take().is_some() {
            self.coalesced = self.coalesced.saturating_add(1);
            true
        } else {
            false
        }
    }

    pub fn has_pending(self) -> bool {
        self.pending.is_some()
    }
}
