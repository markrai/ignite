//! Device orientation reporting (sensor wiring TBD on hardware).

use notebook_core::Orientation;

/// Wrapper so platform code can evolve independently of notebook-core.
pub type DeviceOrientation = Orientation;

pub trait OrientationSource {
    fn current(&self) -> crate::error::PlatformResult<DeviceOrientation>;
}
