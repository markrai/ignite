//! Host-testable mocks, no Kindle hardware required.

mod clock;
mod display;
mod input;

pub use clock::ManualClock;
pub use display::{HostDisplay, UnsupportedFastInk};
pub use input::{
    HostDiscovery, LiveScriptedStylus, RecordingStylus, ScriptedPolls, ScriptedStylus,
    UnsupportedDiscovery,
};
