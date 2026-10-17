//! Linux backends for a jailbroken Kindle (and any Linux host).
//!
//! Discovery scans `/sys/class/input` and then asks each node directly via `EVIOCGBIT` /
//! `EVIOCGABS`. Framebuffer path: `FBIOGET_*`, mmap, and HWTCON send-update / wait.

mod clock;
mod discovery;
mod display;
mod fd;
mod stylus;
mod touch;

pub use clock::LinuxMonotonicClock;
pub use discovery::LinuxInputDiscovery;
pub use display::{
    query_fb_info, LinuxFastInk, LinuxFramebuffer, MappedFramebuffer, RefreshIoctlResult,
    SendUpdateResult, StylusModeGuard, StylusModeIoctlResult, WaitUpdateResult,
};
pub use stylus::LinuxEvdevStylus;
pub use touch::{LinuxEvdevTouch, TouchAxes, TouchPoll, TouchPump};
