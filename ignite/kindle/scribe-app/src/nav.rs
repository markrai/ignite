//! Gate F2/F4 pen-native page navigation + last-visible-stroke undo (BTN_STYLUS + zones).
//!
//! Horizontal zones (canonical X, half-open except the final zone which includes the right edge):
//!
//! ```text
//! [0%, 15%)   LeftNav      → Prev
//! [15%, 25%)  LeftGuard    → IgnoreNoInk
//! [25%, 75%)  UndoCenter   → UndoLastStroke
//! [75%, 85%)  RightGuard   → IgnoreNoInk
//! [85%, 100%] RightNav     → NextOrCreate
//! ```

use crate::prefs::SideButtonAction;
use notebook_core::{PagePoint, CANONICAL_WIDTH};

/// Outer fraction reserved for page navigation (each side).
pub const NAV_OUTER_FRACTION: f32 = 0.15;
/// Guard band after each nav edge (fraction of width).
pub const NAV_GUARD_FRACTION: f32 = 0.10;
/// Undo occupies the center half: from 25% to 75%.
pub const NAV_UNDO_START_FRACTION: f32 = NAV_OUTER_FRACTION + NAV_GUARD_FRACTION; // 0.25
pub const NAV_UNDO_END_FRACTION: f32 = 1.0 - NAV_UNDO_START_FRACTION; // 0.75
pub const NAV_RIGHT_GUARD_END_FRACTION: f32 = 1.0 - NAV_OUTER_FRACTION; // 0.85

/// Exclusive end of LeftNav / start of LeftGuard: `15% * CANONICAL_WIDTH`.
pub fn nav_left_end_x() -> f32 {
    NAV_OUTER_FRACTION * CANONICAL_WIDTH
}

/// Exclusive end of LeftGuard / start of UndoCenter: `25% * CANONICAL_WIDTH`.
pub fn nav_left_guard_end_x() -> f32 {
    NAV_UNDO_START_FRACTION * CANONICAL_WIDTH
}

/// Exclusive end of UndoCenter / start of RightGuard: `75% * CANONICAL_WIDTH`.
pub fn nav_undo_end_x() -> f32 {
    NAV_UNDO_END_FRACTION * CANONICAL_WIDTH
}

/// Exclusive end of RightGuard / start of RightNav: `85% * CANONICAL_WIDTH`.
pub fn nav_right_guard_end_x() -> f32 {
    NAV_RIGHT_GUARD_END_FRACTION * CANONICAL_WIDTH
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavZone {
    LeftNav,
    LeftGuard,
    UndoCenter,
    RightGuard,
    RightNav,
}

pub fn classify_nav_zone(page: PagePoint) -> NavZone {
    let x = page.x;
    if x < nav_left_end_x() {
        NavZone::LeftNav
    } else if x < nav_left_guard_end_x() {
        NavZone::LeftGuard
    } else if x < nav_undo_end_x() {
        NavZone::UndoCenter
    } else if x < nav_right_guard_end_x() {
        NavZone::RightGuard
    } else {
        NavZone::RightNav
    }
}

/// BTN_STYLUS (primary barrel) held + Pen Down intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavDownAction {
    /// Switch to previous page (no-op if already first).
    Prev,
    /// Switch to next page, or create if on last *and* current page has authored history.
    NextOrCreate,
    /// Side button + center: durable undo of newest visible stroke lineage (F4/F7).
    UndoLastStroke,
    /// Guard / dead zone: consume contact, no ink, no undo, no page switch, no persistence.
    IgnoreNoInk,
}

/// Classify a Pen Down when the side button is held.
///
/// Returns `Some(action)` whenever the side button is held so the contact is always consumed
/// as a nav gesture (never falls through to handwriting). Guard bands yield [`IgnoreNoInk`].
pub fn classify_nav_down(side_button_held: bool, page: PagePoint) -> Option<NavDownAction> {
    if !side_button_held {
        return None;
    }
    Some(match classify_nav_zone(page) {
        NavZone::LeftNav => NavDownAction::Prev,
        NavZone::LeftGuard | NavZone::RightGuard => NavDownAction::IgnoreNoInk,
        NavZone::UndoCenter => NavDownAction::UndoLastStroke,
        NavZone::RightNav => NavDownAction::NextOrCreate,
    })
}

/// Resolved side-button intent. [`classify_nav_down`] is unchanged for the default preference.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SideButtonDispatch {
    Nav(NavDownAction),
    CreateLink { page: PagePoint },
}

/// Route an already-decoded BTN_STYLUS + Pen Down into the configured action.
///
/// `CreateLink` consumes the whole page (no leftover edge/center nav). Hardware parsing stays
/// F2/F4: callers pass `buttons.primary` and the canonical `PagePoint`.
pub fn dispatch_side_button(
    pref: SideButtonAction,
    side_button_held: bool,
    page: PagePoint,
) -> Option<SideButtonDispatch> {
    if !side_button_held {
        return None;
    }
    match pref {
        SideButtonAction::NavigationUndo => {
            classify_nav_down(true, page).map(SideButtonDispatch::Nav)
        }
        SideButtonAction::CreateLink => Some(SideButtonDispatch::CreateLink { page }),
    }
}

/// Whether RightNav on the final page may create a new trailing page.
///
/// Pristine (never-authored) trailing pages refuse create. A page that once had writing and is
/// now visually empty still counts as authored history.
pub fn may_create_page_on_right_nav(current_has_authored_history: bool) -> bool {
    current_has_authored_history
}

/// In-progress navigation/undo/no-op tap: consume until Up; never create ink.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NavGestureState {
    #[default]
    Idle,
    /// Navigation/undo/no-op Down accepted; waiting for Up (Moves ignored).
    Active { action: NavDownAction },
}

impl NavGestureState {
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// Clear on StreamGap — do not invent a switch/undo from recovered button state.
    pub fn clear_on_stream_gap(&mut self) {
        *self = Self::Idle;
    }

    /// Begin a nav/undo/no-op gesture from Down. Returns the action to execute once (at Down).
    pub fn on_down(&mut self, action: NavDownAction) -> NavDownAction {
        *self = Self::Active { action };
        action
    }

    /// Matching Up ends the gesture.
    pub fn on_up(&mut self) {
        *self = Self::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone_at(x: f32) -> NavZone {
        classify_nav_zone(PagePoint::new(x, 0.0))
    }

    fn action_at(x: f32) -> NavDownAction {
        classify_nav_down(true, PagePoint::new(x, 0.0)).unwrap()
    }

    #[test]
    fn zone_fraction_constants() {
        assert!((nav_left_end_x() - 0.15 * CANONICAL_WIDTH).abs() < 1e-3);
        assert!((nav_left_guard_end_x() - 0.25 * CANONICAL_WIDTH).abs() < 1e-3);
        assert!((nav_undo_end_x() - 0.75 * CANONICAL_WIDTH).abs() < 1e-3);
        assert!((nav_right_guard_end_x() - 0.85 * CANONICAL_WIDTH).abs() < 1e-3);
    }

    #[test]
    fn physical_test_h_near_miss_coords_are_nav_not_undo() {
        // Observed on-device intended-navigation taps that wrongly became Undo under 8% edges.
        assert_eq!(action_at(1484.0), NavDownAction::Prev);
        assert_eq!(action_at(1872.0), NavDownAction::Prev);
        assert_eq!(action_at(13830.0), NavDownAction::NextOrCreate);
        assert_eq!(action_at(14034.0), NavDownAction::NextOrCreate);
        assert_ne!(action_at(1484.0), NavDownAction::UndoLastStroke);
        assert_ne!(action_at(1872.0), NavDownAction::UndoLastStroke);
        assert_ne!(action_at(13830.0), NavDownAction::UndoLastStroke);
        assert_ne!(action_at(14034.0), NavDownAction::UndoLastStroke);
    }

    #[test]
    fn zones_exhaustive_non_overlapping_boundaries() {
        let left_end = nav_left_end_x();
        let left_guard_end = nav_left_guard_end_x();
        let undo_end = nav_undo_end_x();
        let right_guard_end = nav_right_guard_end_x();

        assert_eq!(zone_at(0.0), NavZone::LeftNav);
        assert_eq!(zone_at(left_end * 0.5), NavZone::LeftNav);
        assert_eq!(zone_at(left_end - 1.0), NavZone::LeftNav);

        assert_eq!(zone_at(left_end), NavZone::LeftGuard);
        assert_eq!(
            zone_at((left_end + left_guard_end) * 0.5),
            NavZone::LeftGuard
        );
        assert_eq!(zone_at(left_guard_end - 1.0), NavZone::LeftGuard);

        assert_eq!(zone_at(left_guard_end), NavZone::UndoCenter);
        assert_eq!(zone_at(CANONICAL_WIDTH * 0.5), NavZone::UndoCenter);
        assert_eq!(zone_at(undo_end - 1.0), NavZone::UndoCenter);

        assert_eq!(zone_at(undo_end), NavZone::RightGuard);
        assert_eq!(
            zone_at((undo_end + right_guard_end) * 0.5),
            NavZone::RightGuard
        );
        assert_eq!(zone_at(right_guard_end - 1.0), NavZone::RightGuard);

        assert_eq!(zone_at(right_guard_end), NavZone::RightNav);
        assert_eq!(
            zone_at((right_guard_end + CANONICAL_WIDTH) * 0.5),
            NavZone::RightNav
        );
        assert_eq!(zone_at(CANONICAL_WIDTH), NavZone::RightNav);
    }

    #[test]
    fn side_plus_zones_map_to_actions() {
        assert_eq!(action_at(10.0), NavDownAction::Prev);
        assert_eq!(
            action_at((nav_left_end_x() + nav_left_guard_end_x()) * 0.5),
            NavDownAction::IgnoreNoInk
        );
        assert_eq!(
            action_at(CANONICAL_WIDTH / 2.0),
            NavDownAction::UndoLastStroke
        );
        assert_eq!(
            action_at((nav_undo_end_x() + nav_right_guard_end_x()) * 0.5),
            NavDownAction::IgnoreNoInk
        );
        assert_eq!(
            action_at(CANONICAL_WIDTH - 10.0),
            NavDownAction::NextOrCreate
        );
        assert_eq!(classify_nav_down(false, PagePoint::new(10.0, 0.0)), None);
        assert_eq!(
            classify_nav_down(false, PagePoint::new(CANONICAL_WIDTH / 2.0, 0.0)),
            None
        );
    }

    #[test]
    fn may_create_requires_authored_history() {
        assert!(may_create_page_on_right_nav(true));
        assert!(!may_create_page_on_right_nav(false));
    }

    #[test]
    fn gesture_clears_on_gap_for_every_action() {
        for action in [
            NavDownAction::Prev,
            NavDownAction::NextOrCreate,
            NavDownAction::UndoLastStroke,
            NavDownAction::IgnoreNoInk,
        ] {
            let mut g = NavGestureState::Idle;
            assert_eq!(g.on_down(action), action);
            assert!(g.is_active());
            g.clear_on_stream_gap();
            assert!(!g.is_active());
        }
    }

    #[test]
    fn one_contact_fires_action_once() {
        let mut g = NavGestureState::Idle;
        let act = g.on_down(NavDownAction::NextOrCreate);
        assert_eq!(act, NavDownAction::NextOrCreate);
        // Move while active does not re-classify on_down.
        assert!(g.is_active());
        g.on_up();
        assert!(!g.is_active());
    }

    #[test]
    fn dispatch_default_matches_classify_nav_down() {
        let p = PagePoint::new(CANONICAL_WIDTH / 2.0, 100.0);
        assert_eq!(
            dispatch_side_button(SideButtonAction::NavigationUndo, true, p),
            Some(SideButtonDispatch::Nav(NavDownAction::UndoLastStroke))
        );
        assert_eq!(
            dispatch_side_button(SideButtonAction::NavigationUndo, false, p),
            None
        );
        assert_eq!(
            dispatch_side_button(SideButtonAction::CreateLink, true, p),
            Some(SideButtonDispatch::CreateLink { page: p })
        );
        let edge = PagePoint::new(10.0, 0.0);
        assert_eq!(
            dispatch_side_button(SideButtonAction::CreateLink, true, edge),
            Some(SideButtonDispatch::CreateLink { page: edge })
        );
        assert_ne!(
            dispatch_side_button(SideButtonAction::CreateLink, true, edge).unwrap(),
            SideButtonDispatch::Nav(NavDownAction::Prev)
        );
    }
}
