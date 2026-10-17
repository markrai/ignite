//! Scripted / recording stylus sources for tests and `--demo`.

use crate::capabilities::{CapabilitySource, DeviceKind, InputCapability, InputDeviceInfo};
use crate::error::{PlatformError, PlatformResult};
use crate::input::{InputDiscovery, SourceMode, SourcePoll, StylusEvent, StylusSource};

#[derive(Debug, Default)]
pub struct HostDiscovery {
    pub devices: Vec<InputDeviceInfo>,
}

impl HostDiscovery {
    pub fn with_stylus(path: &str, name: &str) -> Self {
        Self {
            devices: vec![InputDeviceInfo::new(
                path,
                name,
                DeviceKind::Stylus,
                vec![
                    InputCapability::AbsX,
                    InputCapability::AbsY,
                    InputCapability::AbsPressure,
                    InputCapability::BtnToolPen,
                    InputCapability::BtnTouch,
                ],
                CapabilitySource::Synthetic,
            )],
        }
    }

    pub fn with_devices(devices: Vec<InputDeviceInfo>) -> Self {
        Self { devices }
    }
}

impl InputDiscovery for HostDiscovery {
    fn list_devices(&self) -> PlatformResult<Vec<InputDeviceInfo>> {
        Ok(self.devices.clone())
    }
}

/// A finite, pre-scripted sequence of stylus events.
///
/// Models a recorded fixture: once the script is consumed the source really is finished, so it
/// reports [`SourcePoll::EndOfStream`].
#[derive(Debug, Default)]
pub struct ScriptedStylus {
    events: Vec<StylusEvent>,
    index: usize,
}

impl ScriptedStylus {
    pub fn new(events: Vec<StylusEvent>) -> Self {
        Self { events, index: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.events.len().saturating_sub(self.index)
    }
}

impl StylusSource for ScriptedStylus {
    fn poll_event(&mut self) -> PlatformResult<SourcePoll> {
        if self.index >= self.events.len() {
            return Ok(SourcePoll::EndOfStream);
        }
        let ev = self.events[self.index];
        self.index += 1;
        Ok(SourcePoll::Event(ev))
    }

    fn mode(&self) -> SourceMode {
        SourceMode::NonBlocking
    }
}

/// Scripted [`SourcePoll`] sequence for gap / recovery fixtures.
#[derive(Debug, Default)]
pub struct ScriptedPolls {
    items: Vec<SourcePoll>,
    index: usize,
}

impl ScriptedPolls {
    pub fn new(items: Vec<SourcePoll>) -> Self {
        Self { items, index: 0 }
    }
}

impl StylusSource for ScriptedPolls {
    fn poll_event(&mut self) -> PlatformResult<SourcePoll> {
        if self.index >= self.items.len() {
            return Ok(SourcePoll::EndOfStream);
        }
        let item = self.items[self.index];
        self.index += 1;
        Ok(item)
    }

    fn mode(&self) -> SourceMode {
        SourceMode::NonBlocking
    }
}

/// A **live** host source: it runs dry, but never ends.
///
/// This is the host stand-in for a real digitizer. Running out of queued events means the user
/// paused between strokes, which is [`SourcePoll::WouldBlock`] and must never be mistaken for
/// end of stream. More events can be pushed at any time.
#[derive(Debug, Default)]
pub struct LiveScriptedStylus {
    queued: std::collections::VecDeque<StylusEvent>,
    closed: bool,
}

impl LiveScriptedStylus {
    pub fn new(events: Vec<StylusEvent>) -> Self {
        Self {
            queued: events.into(),
            closed: false,
        }
    }

    /// Queue more events, as a real device would when the user puts the pen down again.
    pub fn push(&mut self, event: StylusEvent) {
        self.queued.push_back(event);
    }

    pub fn extend(&mut self, events: impl IntoIterator<Item = StylusEvent>) {
        self.queued.extend(events);
    }

    /// Simulate the device node going away (unplug / close). Only now is the stream over.
    pub fn close(&mut self) {
        self.closed = true;
    }

    pub fn pending(&self) -> usize {
        self.queued.len()
    }
}

impl StylusSource for LiveScriptedStylus {
    fn poll_event(&mut self) -> PlatformResult<SourcePoll> {
        match self.queued.pop_front() {
            Some(ev) => Ok(SourcePoll::Event(ev)),
            None if self.closed => Ok(SourcePoll::EndOfStream),
            None => Ok(SourcePoll::WouldBlock),
        }
    }

    fn mode(&self) -> SourceMode {
        SourceMode::NonBlocking
    }
}

/// Records events for later replay as fixtures.
#[derive(Debug, Default)]
pub struct RecordingStylus {
    pub recorded: Vec<StylusEvent>,
    inner: ScriptedStylus,
}

impl RecordingStylus {
    pub fn from_script(events: Vec<StylusEvent>) -> Self {
        Self {
            recorded: Vec::new(),
            inner: ScriptedStylus::new(events),
        }
    }
}

impl StylusSource for RecordingStylus {
    fn poll_event(&mut self) -> PlatformResult<SourcePoll> {
        let poll = self.inner.poll_event()?;
        if let SourcePoll::Event(ref e) = poll {
            self.recorded.push(*e);
        }
        Ok(poll)
    }

    fn mode(&self) -> SourceMode {
        self.inner.mode()
    }
}

/// Always-unsupported discovery (documents the Kindle gap on Windows hosts).
#[derive(Debug, Default)]
pub struct UnsupportedDiscovery;

impl InputDiscovery for UnsupportedDiscovery {
    fn list_devices(&self) -> PlatformResult<Vec<InputDeviceInfo>> {
        Err(PlatformError::Unsupported(
            "live input discovery requires Linux /sys and /dev/input",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::StylusEventKind;

    fn ev(kind: StylusEventKind) -> StylusEvent {
        StylusEvent::new(kind, 1.0, 1.0)
    }

    #[test]
    fn scripted_source_ends_after_its_script() {
        let mut s = ScriptedStylus::new(vec![ev(StylusEventKind::Down)]);
        assert!(matches!(s.poll_event().unwrap(), SourcePoll::Event(_)));
        assert_eq!(s.poll_event().unwrap(), SourcePoll::EndOfStream);
        assert_eq!(s.poll_event().unwrap(), SourcePoll::EndOfStream);
    }

    #[test]
    fn live_source_idles_instead_of_ending() {
        let mut s = LiveScriptedStylus::new(vec![ev(StylusEventKind::Down)]);
        assert!(matches!(s.poll_event().unwrap(), SourcePoll::Event(_)));
        // Pen lifted from the digitizer: nothing now, but the device is still there.
        assert_eq!(s.poll_event().unwrap(), SourcePoll::WouldBlock);
        assert_eq!(s.poll_event().unwrap(), SourcePoll::WouldBlock);
        // The user writes again.
        s.push(ev(StylusEventKind::Move));
        assert!(matches!(s.poll_event().unwrap(), SourcePoll::Event(_)));
        assert_eq!(s.poll_event().unwrap(), SourcePoll::WouldBlock);
    }

    #[test]
    fn live_source_ends_only_when_closed() {
        let mut s = LiveScriptedStylus::new(vec![]);
        assert_eq!(s.poll_event().unwrap(), SourcePoll::WouldBlock);
        s.close();
        assert_eq!(s.poll_event().unwrap(), SourcePoll::EndOfStream);
    }

    #[test]
    fn closed_live_source_still_drains_queued_events_first() {
        let mut s = LiveScriptedStylus::new(vec![ev(StylusEventKind::Up)]);
        s.close();
        assert!(matches!(s.poll_event().unwrap(), SourcePoll::Event(_)));
        assert_eq!(s.poll_event().unwrap(), SourcePoll::EndOfStream);
    }

    #[test]
    fn recording_source_captures_what_it_yields() {
        let mut s =
            RecordingStylus::from_script(vec![ev(StylusEventKind::Down), ev(StylusEventKind::Up)]);
        while let SourcePoll::Event(_) = s.poll_event().unwrap() {}
        assert_eq!(s.recorded.len(), 2);
    }

    #[test]
    fn discovery_picks_the_highest_scoring_device() {
        let d = HostDiscovery::with_devices(vec![
            InputDeviceInfo::new(
                "/dev/input/event3",
                "mtk-tpd",
                DeviceKind::Touch,
                vec![
                    InputCapability::AbsX,
                    InputCapability::AbsY,
                    InputCapability::BtnTouch,
                ],
                CapabilitySource::Synthetic,
            ),
            InputDeviceInfo::new(
                "/dev/input/event7",
                "WacomDigitizer",
                DeviceKind::Stylus,
                vec![
                    InputCapability::AbsX,
                    InputCapability::AbsY,
                    InputCapability::AbsPressure,
                    InputCapability::BtnToolPen,
                    InputCapability::BtnTouch,
                ],
                CapabilitySource::Synthetic,
            ),
        ]);
        assert_eq!(d.find_stylus().unwrap().name, "WacomDigitizer");
    }

    #[test]
    fn discovery_reports_not_found_when_nothing_qualifies() {
        let d = HostDiscovery::with_devices(vec![InputDeviceInfo::new(
            "/dev/input/event0",
            "gpio-keys",
            DeviceKind::Button,
            vec![],
            CapabilitySource::Synthetic,
        )]);
        assert!(d.find_stylus().is_err());
    }
}
