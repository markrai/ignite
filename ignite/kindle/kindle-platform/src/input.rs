//! Normalized stylus events, source polling semantics, and discovery traits.

use crate::capabilities::InputDeviceInfo;
use crate::error::PlatformResult;
use crate::evdev_abi::KernelTimestamp;
use crate::time::{ClockDomain, TimestampUs};
use notebook_core::DevicePoint;

/// What the pen did, as opposed to what the tool is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StylusEventKind {
    /// Tool is in proximity and moving, but the nib is **not** touching. Never draws.
    Hover,
    /// Nib made contact (`BTN_TOUCH` 0 -> 1). Starts a stroke.
    Down,
    /// Nib moved while in contact. Extends a stroke.
    Move,
    /// Nib left the surface (`BTN_TOUCH` 1 -> 0). Ends a stroke.
    Up,
}

/// Which end of the stylus is in proximity (`BTN_TOOL_*`).
///
/// This is tool identity, not contact: an `Eraser` tool hovering draws nothing, exactly like a
/// hovering `Pen`. `Eraser` is driven by kernel `BTN_TOOL_RUBBER`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StylusTool {
    /// No `BTN_TOOL_*` currently asserted, or none seen yet.
    #[default]
    None,
    Pen,
    /// Eraser end (`BTN_TOOL_RUBBER`).
    Eraser,
}

/// State of the stylus barrel / side buttons.
///
/// On Scribe 1st Gen 5.18.6 the physical side button produces `BTN_STYLUS` (including while
/// hovering). `BTN_STYLUS2` is advertised but was not observed in capture — still reported when
/// the kernel asserts it, with no invented product semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct BarrelButtons {
    /// `BTN_STYLUS` (0x14b) — physical side button on the confirmed Wacom stream.
    pub primary: bool,
    /// `BTN_STYLUS2` (0x14c) — advertised; not observed on our unit.
    pub secondary: bool,
}

impl BarrelButtons {
    pub fn any(&self) -> bool {
        self.primary || self.secondary
    }
}

/// Device-axis stylus sample, before device calibration into page space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StylusEvent {
    pub kind: StylusEventKind,
    /// Raw device axis units. Calibrate with [`notebook_core::DeviceCalibration`].
    pub device: DevicePoint,
    /// Normalized pressure `0.0..=1.0`, or `None` when the device reports no pressure axis.
    pub pressure: Option<f32>,
    /// Normalized hover distance `0.0..=1.0` when `ABS_DISTANCE` is present.
    pub distance: Option<f32>,
    /// Normalized tilt `ABS_TILT_X` in `-1.0..=1.0` when present.
    pub tilt_x: Option<f32>,
    /// Normalized tilt `ABS_TILT_Y` in `-1.0..=1.0` when present.
    pub tilt_y: Option<f32>,
    /// Tool in proximity when this event was produced.
    pub tool: StylusTool,
    pub buttons: BarrelButtons,
    /// Whether the tool is in proximity at all.
    pub in_proximity: bool,
    /// Kernel event timestamp of the frame, when the source provides one.
    pub kernel_time: Option<KernelTimestamp>,
    /// Clock domain of [`StylusEvent::kernel_time`]. `Unknown` means "do not subtract this".
    pub kernel_clock: ClockDomain,
}

impl StylusEvent {
    /// Minimal constructor for fixtures: no pressure, pen in proximity, no kernel timestamp.
    pub fn new(kind: StylusEventKind, x: f32, y: f32) -> Self {
        Self {
            kind,
            device: DevicePoint::new(x, y),
            pressure: None,
            distance: None,
            tilt_x: None,
            tilt_y: None,
            tool: StylusTool::Pen,
            buttons: BarrelButtons::default(),
            in_proximity: true,
            kernel_time: None,
            kernel_clock: ClockDomain::Unknown,
        }
    }

    pub fn with_pressure(mut self, pressure: f32) -> Self {
        self.pressure = Some(pressure.clamp(0.0, 1.0));
        self
    }

    pub fn with_distance(mut self, distance: f32) -> Self {
        self.distance = Some(distance.clamp(0.0, 1.0));
        self
    }

    pub fn with_tilt(mut self, tilt_x: f32, tilt_y: f32) -> Self {
        self.tilt_x = Some(tilt_x.clamp(-1.0, 1.0));
        self.tilt_y = Some(tilt_y.clamp(-1.0, 1.0));
        self
    }

    pub fn with_tool(mut self, tool: StylusTool) -> Self {
        self.tool = tool;
        self
    }

    pub fn with_buttons(mut self, buttons: BarrelButtons) -> Self {
        self.buttons = buttons;
        self
    }

    /// Attach a kernel timestamp together with the clock domain it was taken in.
    pub fn with_kernel_time(mut self, t_us: TimestampUs, domain: ClockDomain) -> Self {
        self.kernel_time = Some(KernelTimestamp {
            tv_sec: (t_us / 1_000_000) as i64,
            tv_usec: (t_us % 1_000_000) as i64,
        });
        self.kernel_clock = domain;
        self
    }

    /// Kernel timestamp in microseconds, if one was supplied.
    pub fn kernel_t_us(&self) -> Option<TimestampUs> {
        self.kernel_time.map(KernelTimestamp::to_micros)
    }
}

/// Result of polling a [`StylusSource`].
///
/// The cases are deliberately distinct. Using `Option` for this collapses "nothing right
/// now" and "this source is finished", which makes a live probe exit the instant the user pauses
/// between pen strokes. [`SourcePoll::StreamGap`] is separate from stylus events so a buffer
/// overrun cannot be mistaken for a pen Up/Down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SourcePoll {
    /// An event is available now.
    Event(StylusEvent),
    /// Evdev `SYN_DROPPED` (or equivalent): stream state became untrustworthy.
    /// The source has entered fail-closed gap handling but has not recovered yet.
    /// Cancel any in-progress stroke; do not invent geometry.
    StreamGap { had_active_contact: bool },
    /// Nothing available at this instant. The source is still live; poll again later.
    WouldBlock,
    /// The source is permanently exhausted (script consumed, device node closed / EOF).
    EndOfStream,
}

/// Whether a source can ever return [`SourcePoll::WouldBlock`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceMode {
    /// `poll_event` blocks until an event arrives or the stream ends. Never `WouldBlock`.
    Blocking,
    /// `poll_event` returns immediately, possibly with `WouldBlock`.
    NonBlocking,
}

pub trait StylusSource {
    fn poll_event(&mut self) -> PlatformResult<SourcePoll>;

    /// Declared blocking behavior, so a caller knows whether a busy-poll loop is required.
    fn mode(&self) -> SourceMode {
        SourceMode::NonBlocking
    }

    /// Current `BTN_TOOL_PEN` / `BTN_TOOL_RUBBER` proximity, if the source tracks it.
    ///
    /// Read-only observation of already-parsed tool state: the parser emits no event when a
    /// tool leaves proximity, so consumers that need the exit edge (touch palm arbitration)
    /// sample this instead. `None` = unknown (scripted sources, desynchronized stream).
    fn tool_in_proximity(&self) -> Option<bool> {
        None
    }
}

pub trait InputDiscovery {
    fn list_devices(&self) -> PlatformResult<Vec<InputDeviceInfo>>;

    /// Best stylus candidate by capability and name evidence. Never an assumed `eventN`.
    ///
    /// Prefer a usable [`crate::PREFERRED_STYLUS_PATH`] entry when present in `list_devices`.
    /// Platform backends that can probe the symlink directly (Linux) should override this.
    fn find_stylus(&self) -> PlatformResult<InputDeviceInfo> {
        let devices = self.list_devices()?;
        let preferred = devices
            .iter()
            .find(|d| d.path == crate::PREFERRED_STYLUS_PATH)
            .cloned();
        crate::select_stylus_device(&devices, preferred.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_constructor_defaults_to_hovering_pen() {
        let e = StylusEvent::new(StylusEventKind::Hover, 1.0, 2.0);
        assert_eq!(e.tool, StylusTool::Pen);
        assert!(e.in_proximity);
        assert!(e.pressure.is_none());
        assert_eq!(e.kernel_clock, ClockDomain::Unknown);
        assert!(e.kernel_t_us().is_none());
    }

    #[test]
    fn kernel_time_round_trips_with_its_domain() {
        let e = StylusEvent::new(StylusEventKind::Down, 0.0, 0.0)
            .with_kernel_time(1_234_567, ClockDomain::Synthetic);
        assert_eq!(e.kernel_t_us(), Some(1_234_567));
        assert_eq!(e.kernel_clock, ClockDomain::Synthetic);
    }

    #[test]
    fn barrel_buttons_default_to_released() {
        let b = BarrelButtons::default();
        assert!(!b.any());
    }
}
