//! Kindle Scribe platform capabilities.
//!
//! Design rules:
//!
//! * Select devices by **name and capabilities**, never by a hardcoded `/dev/input/eventN`.
//! * Never assume a kernel ABI: the `input_event` layout is derived from the target word size
//!   (see [`evdev_abi`]) and axis ranges come from the device (see [`axes`]).
//! * Where Kindle behavior is unconfirmed on our own unit, return
//!   [`PlatformError::Unsupported`] rather than inventing a kernel API.
//! * Never subtract timestamps across [`ClockDomain`]s.

#![deny(unsafe_code)]

pub mod evdev_abi;
pub mod evdev_ioctl;
pub mod fb_polarity;
pub mod fb_sample;
pub mod fbdev_abi;
pub mod gate_b;
pub mod hwtcon_abi;
pub mod marker;
pub mod raster;
pub mod stylus_mode_ctrl;
pub mod sysfs_bitmap;

mod axes;
mod capabilities;
mod display;
mod error;
mod evdev_state;
mod input;
mod input_pump;
mod orientation;
mod select;
mod time;
mod touch_mt;

pub mod host;

#[cfg(unix)]
#[allow(unsafe_code)]
pub mod linux;

pub use axes::{AxisRange, StylusAxes};
pub use capabilities::{
    CapabilitySource, DeviceKind, InputCapability, InputDeviceInfo, KNOWN_SCRIBE_STYLUS_NAMES,
};
pub use display::{DirtyRegion, DisplaySurface, FastInkMode, RefreshMode};
pub use error::{PlatformError, PlatformResult};
pub use evdev_abi::{
    decode_native, decode_with_word_size, InputEvent, KernelTimestamp, RawInputEvent,
    INPUT_EVENT_SIZE, KERNEL_LONG_BYTES,
};
pub use evdev_state::{DeviceSnapshot, EvdevStateMachine, FeedResult, StreamIntegrity};
pub use fb_polarity::{
    copy_logical_rect_to_hw, decode_panel_gray, encode_panel_gray, hardware_complement,
    LOGICAL_BLACK, LOGICAL_WHITE,
};
pub use fb_sample::{
    sample_visible_grid, visible_grid_coords, FbGridSample, FbSamplePoint, FlushCopyStats,
    SampleLayout,
};
pub use fbdev_abi::{
    FbBitfield, FbFixScreeninfo, FbGeometry, FbGeometryReport, FbVarScreeninfo, PanelRectError,
    FBIOGET_FSCREENINFO, FBIOGET_VSCREENINFO,
};
pub use gate_b::{
    check_preconditions as check_gate_b_preconditions, format_fb_diagnose,
    grayscale_is_accepted_8bpp_gray, BLACK_BYTE, WHITE_BYTE,
};
pub use hwtcon_abi::{
    stylus_mode, MxcfbRect, MxcfbUpdateDataMtk, MxcfbUpdateMarkerData, MXCFB_SEND_UPDATE_MTK,
    MXCFB_SET_STYLUS_MODE, MXCFB_WAIT_FOR_UPDATE_COMPLETE, TEMP_USE_AMBIENT, UPDATE_MODE_FULL,
    UPDATE_MODE_PARTIAL,
};
pub use input::{
    BarrelButtons, InputDiscovery, SourceMode, SourcePoll, StylusEvent, StylusEventKind,
    StylusSource, StylusTool,
};
pub use input_pump::InputPump;
pub use marker::{marker_dirty_rect, paint_asymmetric_marker, MarkerDirty};
pub use orientation::{DeviceOrientation, OrientationSource};
pub use raster::{
    raster_dashed_line, raster_line, raster_line_variable_radius, raster_stamp, RasterResult,
    STREAM_BRUSH_RADIUS,
};
pub use touch_mt::{MtTouchDecoder, TouchContact, TouchFeed, TouchFrame, TouchLift, MAX_SLOTS};

pub use select::{
    is_stylus_custom_name, is_wacom_digitizer_name, preferred_path_is_usable, select_stylus_device,
    PREFERRED_STYLUS_PATH,
};
pub use stylus_mode_ctrl::{
    rect_checksum, validate_e1_stylus_mode, StylusModeLifecycle, StylusModePhase, UpdateClass,
};
pub use sysfs_bitmap::{bit_evidence, BitEvidence};
pub use time::{ClockDomain, HostClock, MonotonicClock, TimestampUs};

/// Raw evdev code constants (`EV_*`, `ABS_*`, `BTN_*`, `SYN_*`) as defined by Linux.
pub mod codes {
    pub use crate::evdev_abi::{abs, btn, ev, syn};
}
