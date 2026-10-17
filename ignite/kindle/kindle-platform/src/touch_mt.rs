//! Multitouch protocol-B decoder for the Scribe touchscreen (`pt_mt`, `/dev/input/touch`).
//!
//! Observed on FW 5.18.6 (read-only trace, never grabbed):
//!
//! * Only `ABS_MT_*` axes; no `BTN_TOUCH`, no single-touch `ABS_X/Y`.
//! * `ABS_MT_TRACKING_ID` is **not** unique per contact: every lone contact reuses id 0 and a
//!   second simultaneous finger gets id 1 in slot 1. Contact lifetime therefore comes from
//!   tracking-id transitions (`>= 0` = down, `-1` = lift) per slot, never from id values.
//! * `ABS_MT_TOOL_TYPE` is emitted only on change; `MT_TOOL_PALM` (2) appears when a large
//!   contact is reclassified shortly before lift.
//! * Event timestamps are `CLOCK_REALTIME`; consumers time gestures with their own monotonic
//!   clock at read time.
//!
//! The decoder is host-testable and has no notion of UI. It emits one [`TouchFrame`] per
//! `SYN_REPORT` describing every active contact.

use crate::evdev_abi::{abs, ev, mt_tool, syn, KernelTimestamp, RawInputEvent};

/// Slots tracked. The panel reports far fewer; extra slots are ignored.
pub const MAX_SLOTS: usize = 10;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Slot {
    active: bool,
    tracking_id: i32,
    x: Option<i32>,
    y: Option<i32>,
    palm: bool,
}

/// One active contact after a `SYN_REPORT`. Position may be unknown for its first frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TouchContact {
    pub slot: usize,
    /// Current `ABS_MT_TRACKING_ID` for this slot. Ids are reused (see module docs).
    pub tracking_id: i32,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub palm: bool,
}

/// A slot that lifted in the events leading to this `SYN_REPORT`.
///
/// `tracking_id` is the **preceding active** id, not `-1`. Classification must not
/// treat this as a contact that is still down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TouchLift {
    pub slot: usize,
    pub tracking_id: i32,
    pub x: Option<i32>,
    pub y: Option<i32>,
}

/// State of all contacts at one `SYN_REPORT`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TouchFrame {
    /// Kernel `struct timeval` from the `SYN_REPORT` (touch nodes: `CLOCK_REALTIME`).
    pub kernel_time: KernelTimestamp,
    pub contacts: Vec<TouchContact>,
    pub lifts: Vec<TouchLift>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TouchFeed {
    None,
    Frame(TouchFrame),
    /// `SYN_DROPPED`: contact state is unknown until every contact lifts.
    Gap,
}

#[derive(Clone, Debug, Default)]
pub struct MtTouchDecoder {
    slots: [Slot; MAX_SLOTS],
    slot: usize,
    desynchronized: bool,
    pending_lifts: Vec<TouchLift>,
}

impl MtTouchDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    fn current(&mut self) -> Option<&mut Slot> {
        self.slots.get_mut(self.slot)
    }

    pub fn feed(&mut self, e: RawInputEvent) -> TouchFeed {
        match (e.ev_type, e.code) {
            (ev::SYN, syn::DROPPED) => {
                self.desynchronized = true;
                self.slots = [Slot::default(); MAX_SLOTS];
                self.pending_lifts.clear();
                TouchFeed::Gap
            }
            (ev::SYN, syn::REPORT) => {
                if self.desynchronized {
                    // Discard the partial post-drop frame; slots were reset at the drop, so
                    // any finger still down stays invisible until it lifts and re-touches.
                    self.desynchronized = false;
                    self.slots = [Slot::default(); MAX_SLOTS];
                    self.pending_lifts.clear();
                    return TouchFeed::None;
                }
                let mut f = self.frame();
                f.kernel_time = e.time;
                f.lifts = std::mem::take(&mut self.pending_lifts);
                TouchFeed::Frame(f)
            }
            _ if self.desynchronized => TouchFeed::None,
            (ev::ABS, abs::MT_SLOT) => {
                // Out-of-range slots are tracked as "no slot": their events are ignored.
                self.slot = usize::try_from(e.value).unwrap_or(usize::MAX);
                TouchFeed::None
            }
            (ev::ABS, abs::MT_TRACKING_ID) => {
                let slot_idx = self.slot;
                let lift = {
                    let Some(s) = self.current() else {
                        return TouchFeed::None;
                    };
                    if e.value >= 0 {
                        // New contact in this slot (id values are not unique; see module doc).
                        *s = Slot {
                            active: true,
                            tracking_id: e.value,
                            ..Slot::default()
                        };
                        None
                    } else if s.active {
                        let lift = TouchLift {
                            slot: slot_idx,
                            tracking_id: s.tracking_id,
                            x: s.x,
                            y: s.y,
                        };
                        *s = Slot::default();
                        Some(lift)
                    } else {
                        *s = Slot::default();
                        None
                    }
                };
                if let Some(lift) = lift {
                    self.pending_lifts.push(lift);
                }
                TouchFeed::None
            }
            (ev::ABS, abs::MT_POSITION_X) => {
                if let Some(s) = self.current() {
                    s.x = Some(e.value);
                }
                TouchFeed::None
            }
            (ev::ABS, abs::MT_POSITION_Y) => {
                if let Some(s) = self.current() {
                    s.y = Some(e.value);
                }
                TouchFeed::None
            }
            (ev::ABS, abs::MT_TOOL_TYPE) => {
                if let Some(s) = self.current() {
                    // Sticky: once a contact is reported as a palm it stays one until lift.
                    s.palm |= e.value == mt_tool::PALM;
                }
                TouchFeed::None
            }
            _ => TouchFeed::None,
        }
    }

    fn frame(&self) -> TouchFrame {
        TouchFrame {
            contacts: self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.active)
                .map(|(slot, s)| TouchContact {
                    slot,
                    tracking_id: s.tracking_id,
                    x: s.x,
                    y: s.y,
                    palm: s.palm,
                })
                .collect(),
            ..TouchFrame::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evdev_abi::KernelTimestamp;

    fn e(ev_type: u16, code: u16, value: i32) -> RawInputEvent {
        RawInputEvent {
            time: KernelTimestamp::default(),
            ev_type,
            code,
            value,
        }
    }
    fn a(code: u16, v: i32) -> RawInputEvent {
        e(ev::ABS, code, v)
    }
    fn report(d: &mut MtTouchDecoder) -> TouchFeed {
        d.feed(e(ev::SYN, syn::REPORT, 0))
    }

    #[test]
    fn single_contact_down_move_up_like_fw_5_18_6_trace() {
        let mut d = MtTouchDecoder::new();
        // Recorded shape: TRACKING_ID 0, X, Y, PRESSURE, TOUCH_MAJOR, SYN (no MT_SLOT first).
        for ev in [
            a(abs::MT_TRACKING_ID, 0),
            a(abs::MT_POSITION_X, 1664),
            a(abs::MT_POSITION_Y, 1794),
            a(abs::MT_PRESSURE, 90),
            a(abs::MT_TOUCH_MAJOR, 107),
        ] {
            assert_eq!(d.feed(ev), TouchFeed::None);
        }
        let TouchFeed::Frame(f) = report(&mut d) else {
            panic!()
        };
        assert_eq!(
            f.contacts,
            vec![TouchContact {
                slot: 0,
                tracking_id: 0,
                x: Some(1664),
                y: Some(1794),
                palm: false
            }]
        );
        d.feed(a(abs::MT_POSITION_X, 1646));
        let TouchFeed::Frame(f) = report(&mut d) else {
            panic!()
        };
        assert_eq!(f.contacts[0].x, Some(1646));
        assert_eq!(
            f.contacts[0].y,
            Some(1794),
            "unchanged axis keeps last value"
        );
        d.feed(a(abs::MT_TRACKING_ID, -1));
        let TouchFeed::Frame(up) = report(&mut d) else {
            panic!()
        };
        assert!(up.contacts.is_empty());
        assert_eq!(
            up.lifts,
            vec![TouchLift {
                slot: 0,
                tracking_id: 0,
                x: Some(1646),
                y: Some(1794),
            }]
        );
        // Reused id 0 is a fresh contact with no stale position.
        d.feed(a(abs::MT_TRACKING_ID, 0));
        let TouchFeed::Frame(f) = report(&mut d) else {
            panic!()
        };
        assert_eq!(f.contacts[0].x, None);
    }

    #[test]
    fn second_slot_is_reported_and_palm_is_sticky() {
        let mut d = MtTouchDecoder::new();
        d.feed(a(abs::MT_TRACKING_ID, 0));
        d.feed(a(abs::MT_POSITION_X, 500));
        d.feed(a(abs::MT_POSITION_Y, 1000));
        d.feed(a(abs::MT_SLOT, 1));
        d.feed(a(abs::MT_TRACKING_ID, 1));
        d.feed(a(abs::MT_POSITION_X, 300));
        d.feed(a(abs::MT_POSITION_Y, 1200));
        let TouchFeed::Frame(f) = report(&mut d) else {
            panic!()
        };
        assert_eq!(f.contacts.len(), 2);
        assert_eq!(f.contacts[1].slot, 1);
        d.feed(a(abs::MT_SLOT, 0));
        d.feed(a(abs::MT_TOOL_TYPE, mt_tool::PALM));
        d.feed(a(abs::MT_TOOL_TYPE, mt_tool::FINGER));
        let TouchFeed::Frame(f) = report(&mut d) else {
            panic!()
        };
        assert!(f.contacts[0].palm);
        assert!(!f.contacts[1].palm);
        // Slot beyond MAX_SLOTS is ignored, never panics.
        d.feed(a(abs::MT_SLOT, 99));
        d.feed(a(abs::MT_TRACKING_ID, 5));
        d.feed(a(abs::MT_SLOT, -3));
        d.feed(a(abs::MT_POSITION_X, 1));
        let TouchFeed::Frame(f) = report(&mut d) else {
            panic!()
        };
        assert_eq!(f.contacts.len(), 2);
    }

    #[test]
    fn syn_dropped_forgets_contacts_until_resync() {
        let mut d = MtTouchDecoder::new();
        d.feed(a(abs::MT_TRACKING_ID, 0));
        d.feed(a(abs::MT_POSITION_X, 10));
        report(&mut d);
        assert_eq!(d.feed(e(ev::SYN, syn::DROPPED, 0)), TouchFeed::Gap);
        d.feed(a(abs::MT_POSITION_X, 20));
        assert_eq!(report(&mut d), TouchFeed::None);
        assert_eq!(report(&mut d), TouchFeed::Frame(TouchFrame::default()));
    }

    #[test]
    fn syn_report_preserves_kernel_timestamp() {
        let mut d = MtTouchDecoder::new();
        d.feed(a(abs::MT_TRACKING_ID, 0));
        let syn = RawInputEvent {
            time: KernelTimestamp {
                tv_sec: 12,
                tv_usec: 345678,
            },
            ev_type: ev::SYN,
            code: syn::REPORT,
            value: 0,
        };
        let TouchFeed::Frame(f) = d.feed(syn) else {
            panic!()
        };
        assert_eq!(f.kernel_time.tv_sec, 12);
        assert_eq!(f.kernel_time.tv_usec, 345678);
    }

    #[test]
    fn lift_preserves_preceding_tracking_id_not_minus_one() {
        let mut d = MtTouchDecoder::new();
        d.feed(a(abs::MT_TRACKING_ID, 7));
        d.feed(a(abs::MT_POSITION_X, 10));
        d.feed(a(abs::MT_POSITION_Y, 20));
        report(&mut d);
        d.feed(a(abs::MT_TRACKING_ID, -1));
        let TouchFeed::Frame(f) = report(&mut d) else {
            panic!()
        };
        assert!(f.contacts.is_empty());
        assert_eq!(f.lifts[0].tracking_id, 7);
        assert_ne!(f.lifts[0].tracking_id, -1);
    }
}
