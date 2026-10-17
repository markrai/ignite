//! MediaTek Kindle `hwtcon_v2` refresh ioctl ABI (userspace argument layouts).
//!
//! # Evidence (Scribe 1st Gen, firmware 5.18.6)
//!
//! Independently recovered from Amazon's on-device `/usr/sbin/eips` (copied for static
//! analysis; originals unmodified):
//!
//! * Opens `/dev/fb0` and calls `ioctl` from libc (PLT).
//! * Embeds request `0x4060462e` = `_IOW('F', 0x2E, 96)` and submits a 96-byte buffer.
//! * Embeds request `0xc008462f` = `_IOWR('F', 0x2F, 8)` as a separate completion wait.
//! * Fills the 96-byte buffer at fixed offsets matching the field map below
//!   (disassembly of `update_to_display` @ `0x12420` and callers).
//! * Waveform name → integer table in the same binary (`du`→1, `gc16`→2, …).
//!
//! Public PW6 / `hwtcon_v2` headers (FBInk `mtk-kindle.h`, origin Lab126 kernel for FW
//! 5.17.1) corroborate names and numbers only — they are **not** vendored here.
//!
//! This module defines layouts and constants for host/unit tests. It does **not** issue
//! refresh ioctls or write framebuffer memory.

#[cfg(test)]
use core::mem::{align_of, offset_of, size_of};

/// `_IOC` direction bits (Linux UAPI).
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

/// Encode a Linux ioctl number the same way the kernel/`_IOW`/`_IOWR` macros do on ARM.
pub const fn ioc(dir: u32, type_: u8, nr: u8, size: u32) -> u32 {
    (dir << 30) | (size << 16) | ((type_ as u32) << 8) | (nr as u32)
}

/// Magic used by Amazon MTK Kindle e-ink ioctls (`'F'`), shared with classic mxcfb.
pub const HWTCON_IOCTL_MAGIC: u8 = b'F';

/// `MXCFB_SEND_UPDATE_MTK` — submit one e-ink update. Size encodes 96-byte arg.
///
/// Observed literally in Amazon `eips` as `0x4060462e`.
pub const MXCFB_SEND_UPDATE_MTK: u32 = ioc(IOC_WRITE, HWTCON_IOCTL_MAGIC, 0x2E, 96);

/// `MXCFB_WAIT_FOR_UPDATE_COMPLETE` — wait on a marker. Size encodes 8-byte arg.
///
/// Observed literally in Amazon `eips` as `0xc008462f`.
pub const MXCFB_WAIT_FOR_UPDATE_COMPLETE: u32 =
    ioc(IOC_READ | IOC_WRITE, HWTCON_IOCTL_MAGIC, 0x2F, 8);

/// Also observed in `eips`: `MXCFB_GET_TEMPERATURE` `_IOR('F', 0x38, 4)` = `0x80044638`.
pub const MXCFB_GET_TEMPERATURE: u32 = ioc(IOC_READ, HWTCON_IOCTL_MAGIC, 0x38, 4);

/// Also observed in `eips`: `MXCFB_GET_WAVEFORM_TYPE` `_IOR('F', 0x39, 4)` = `0x80044639`.
pub const MXCFB_GET_WAVEFORM_TYPE: u32 = ioc(IOC_READ, HWTCON_IOCTL_MAGIC, 0x39, 4);

/// `MXCFB_SET_STYLUS_MODE` — `_IOW('F', 0x4C, u32)` = `0x4004464c`.
///
/// # Evidence (Scribe 1st Gen / FW 5.18.6 Gate E0)
///
/// Independently recovered from Amazon `/app/bin/WetInkHandler` (static analysis only;
/// **this crate does not issue the ioctl**):
///
/// * Literal `0x4004464c` at file offset `0x20d30` (VA `0x30d30`).
/// * Wrapper at VA `0x30cf4`: `ioctl(*(obj+0), MXCFB_SET_STYLUS_MODE, &mode)` via `ioctl@plt`.
/// * `*(obj+0)` is the same fd slot WetInk opens/closes for `/dev/fb0`.
/// * Direct callers pass **mode `1`** (enable) and **mode `0`** (disable) only.
///
/// Public UAPI names `EPDC_STYLUS_MODE_*` corroborate 0=disabled / 1=no TPS; modes 2/3
/// (black/white TPS) were **not** observed at WetInk call sites on this firmware.
pub const MXCFB_SET_STYLUS_MODE: u32 = ioc(IOC_WRITE, HWTCON_IOCTL_MAGIC, 0x4C, 4);

/// Stock WetInk `MXCFB_SET_STYLUS_MODE` argument values observed on FW 5.18.6.
pub mod stylus_mode {
    /// `EPDC_STYLUS_MODE_DISABLED` — WetInk disable call site (`movs r1, #0`).
    pub const DISABLED: u32 = 0;
    /// `EPDC_STYLUS_MODE_WITH_NO_TPS` — WetInk enable call site (`movs r6, #1` → `r1`).
    pub const WITH_NO_TPS: u32 = 1;
    /// Public UAPI value; **not** observed in WetInk enable/disable call sites on 5.18.6.
    pub const WITH_BLACK_TPS: u32 = 2;
    /// Public UAPI value; **not** observed in WetInk enable/disable call sites on 5.18.6.
    pub const WITH_WHITE_TPS: u32 = 3;
}

/// Partial update (Amazon `eips` prints `PARTIAL` when this is 0).
pub const UPDATE_MODE_PARTIAL: u32 = 0;
/// Full update (Amazon `eips` prints `FULL` when this is 1).
pub const UPDATE_MODE_FULL: u32 = 1;

/// Use panel temperature sensor (Amazon `eips` stores `0x1000` at temp offset).
pub const TEMP_USE_AMBIENT: i32 = 0x1000;

/// Waveform mode integers from Amazon `eips` string→int table (same values as MTK UAPI).
pub mod waveform {
    pub const INIT: u32 = 0;
    pub const DU: u32 = 1;
    pub const GC16: u32 = 2;
    pub const GL16: u32 = 3;
    pub const REAGL: u32 = 4;
    pub const REAGLD: u32 = 5;
    pub const A2: u32 = 6;
    pub const GCK16: u32 = 8;
    pub const GLKW16: u32 = 9;
    /// `nmdu` in `eips` (`DUNM`).
    pub const DUNM: u32 = 12;
    /// Default / auto when name does not match (`0x101`).
    pub const AUTO: u32 = 257;
}

/// Conservative defaults for a first static rectangle (no swipe, no alt buffer).
pub const CONSERVATIVE_WAVEFORM: u32 = waveform::GC16;
pub const CONSERVATIVE_UPDATE_MODE: u32 = UPDATE_MODE_PARTIAL;

/// Update rectangle. Amazon `eips` stores `top` at +0, `left` at +4, then `width`/`height`.
/// Callers pass screen `(x,y)` as `(left, top)` into the update helper.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MxcfbRect {
    pub top: u32,
    pub left: u32,
    pub width: u32,
    pub height: u32,
}

/// Marker payload for `MXCFB_WAIT_FOR_UPDATE_COMPLETE` (8 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MxcfbUpdateMarkerData {
    pub update_marker: u32,
    pub collision_test: u32,
}

/// Alternate buffer descriptor (unused unless a USE_ALT_BUFFER flag is set).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MxcfbAltBufferData {
    pub phys_addr: u32,
    pub width: u32,
    pub height: u32,
    pub alt_update_region: MxcfbRect,
}

/// Swipe / animate payload (Amazon `eips` writes direction @ +72, steps @ +76).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MxcfbSwipeData {
    pub direction: u32,
    pub steps: u32,
}

/// 96-byte send-update argument used with `MXCFB_SEND_UPDATE_MTK`.
///
/// Offsets verified against Amazon `eips` stores into the ioctl buffer:
/// `+0` rect, `+16` waveform, `+20` update_mode, `+24` marker, `+28` temp (`0x1000`),
/// `+32` flags, `+36` dither (`0`), `+72`/`+76` swipe, `+80`/`+84` hist waveforms.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MxcfbUpdateDataMtk {
    pub update_region: MxcfbRect,
    pub waveform_mode: u32,
    pub update_mode: u32,
    pub update_marker: u32,
    pub temp: i32,
    pub flags: u32,
    pub dither_mode: i32,
    pub quant_bit: i32,
    pub alt_buffer_data: MxcfbAltBufferData,
    pub swipe_data: MxcfbSwipeData,
    pub hist_bw_waveform_mode: u32,
    pub hist_gray_waveform_mode: u32,
    pub ts_pxp: u32,
    pub ts_epdc: u32,
}

impl MxcfbUpdateDataMtk {
    /// Build a zeroed partial GC16 update for a visible-panel rectangle.
    ///
    /// Does not submit an ioctl. Coordinates are panel pixels (`0..xres`, `0..yres`) as used
    /// by Amazon `eips` (printed next to `xres`/`yres`). Interaction with nonzero `yoffset`
    /// is not established on this unit (current visible window is `yoffset=0`).
    pub fn conservative_partial(region: MxcfbRect, update_marker: u32) -> Self {
        Self {
            update_region: region,
            waveform_mode: CONSERVATIVE_WAVEFORM,
            update_mode: CONSERVATIVE_UPDATE_MODE,
            update_marker,
            temp: TEMP_USE_AMBIENT,
            hist_bw_waveform_mode: waveform::DU,
            hist_gray_waveform_mode: waveform::GC16,
            ..Default::default()
        }
    }

    /// Full-refresh GC16 (clears e-ink residue; used for page reconstruct / HUD chrome).
    pub fn conservative_full(region: MxcfbRect, update_marker: u32) -> Self {
        Self {
            update_region: region,
            waveform_mode: CONSERVATIVE_WAVEFORM,
            update_mode: UPDATE_MODE_FULL,
            update_marker,
            temp: TEMP_USE_AMBIENT,
            hist_bw_waveform_mode: waveform::DU,
            hist_gray_waveform_mode: waveform::GC16,
            ..Default::default()
        }
    }

    /// Stock WetInkHandler wet-ink update (FW 5.18.6 Gate E1.1).
    ///
    /// Independently recovered from Amazon `DrawToFramebufferUtility` init `@ VA 0x36728`
    /// and SEND wrapper `@ VA 0x366c8`:
    ///
    /// * `waveform_mode = 1` (DU)
    /// * `update_mode = 0` (PARTIAL; memset then never overridden)
    /// * `temp = 0x1000` (TEMP_USE_AMBIENT)
    /// * `hist_bw_waveform_mode = 1`, `hist_gray_waveform_mode = 2` (stock init stores)
    /// * remaining fields zero (stock `memset` of the 96-byte buffer)
    ///
    /// Does **not** submit an ioctl. Distinct from [`Self::conservative_partial`] (GC16).
    pub fn wetink_du_partial(region: MxcfbRect, update_marker: u32) -> Self {
        Self {
            update_region: region,
            waveform_mode: waveform::DU,
            update_mode: UPDATE_MODE_PARTIAL,
            update_marker,
            temp: TEMP_USE_AMBIENT,
            hist_bw_waveform_mode: waveform::DU,
            hist_gray_waveform_mode: waveform::GC16,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_update_ioctl_matches_amazon_eips_literal() {
        assert_eq!(MXCFB_SEND_UPDATE_MTK, 0x4060_462e);
        assert_eq!(size_of::<MxcfbUpdateDataMtk>(), 96);
    }

    #[test]
    fn wait_complete_ioctl_matches_amazon_eips_literal() {
        assert_eq!(MXCFB_WAIT_FOR_UPDATE_COMPLETE, 0xc008_462f);
        assert_eq!(size_of::<MxcfbUpdateMarkerData>(), 8);
    }

    #[test]
    fn aux_ioctls_match_amazon_eips_literals() {
        assert_eq!(MXCFB_GET_TEMPERATURE, 0x8004_4638);
        assert_eq!(MXCFB_GET_WAVEFORM_TYPE, 0x8004_4639);
    }

    #[test]
    fn set_stylus_mode_matches_wetinkhandler_literal() {
        assert_eq!(MXCFB_SET_STYLUS_MODE, 0x4004_464c);
        assert_eq!(stylus_mode::DISABLED, 0);
        assert_eq!(stylus_mode::WITH_NO_TPS, 1);
        // Encoding cross-check: _IOW('F', 0x4C, 4).
        assert_eq!(
            MXCFB_SET_STYLUS_MODE,
            ioc(IOC_WRITE, HWTCON_IOCTL_MAGIC, 0x4C, 4)
        );
    }

    #[test]
    fn armv7_compatible_field_offsets() {
        // All fields are 32-bit scalars / nested 32-bit structs — identical on ARMv7hf and
        // 64-bit hosts. Offsets match Amazon `eips` store sites.
        assert_eq!(align_of::<MxcfbUpdateDataMtk>(), 4);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, update_region), 0);
        assert_eq!(offset_of!(MxcfbRect, top), 0);
        assert_eq!(offset_of!(MxcfbRect, left), 4);
        assert_eq!(offset_of!(MxcfbRect, width), 8);
        assert_eq!(offset_of!(MxcfbRect, height), 12);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, waveform_mode), 16);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, update_mode), 20);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, update_marker), 24);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, temp), 28);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, flags), 32);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, dither_mode), 36);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, quant_bit), 40);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, alt_buffer_data), 44);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, swipe_data), 72);
        assert_eq!(offset_of!(MxcfbSwipeData, direction), 0);
        assert_eq!(offset_of!(MxcfbSwipeData, steps), 4);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, hist_bw_waveform_mode), 80);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, hist_gray_waveform_mode), 84);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, ts_pxp), 88);
        assert_eq!(offset_of!(MxcfbUpdateDataMtk, ts_epdc), 92);
    }

    #[test]
    fn rectangle_field_mapping_matches_eips_xy_convention() {
        // eips passes (x,y,w,h) as (r0,r1,r2,r3) then stores top=r1, left=r0.
        let r = MxcfbRect {
            top: 100,
            left: 40,
            width: 16,
            height: 8,
        };
        let u = MxcfbUpdateDataMtk::conservative_partial(r, 42);
        assert_eq!(u.update_region.left, 40);
        assert_eq!(u.update_region.top, 100);
        assert_eq!(u.waveform_mode, waveform::GC16);
        assert_eq!(u.update_mode, UPDATE_MODE_PARTIAL);
        assert_eq!(u.temp, TEMP_USE_AMBIENT);
        assert_eq!(u.flags, 0);
        assert_eq!(u.swipe_data, MxcfbSwipeData::default());
    }

    #[test]
    fn wetink_du_partial_matches_stock_wetinkhandler_init() {
        let r = MxcfbRect {
            top: 180,
            left: 120,
            width: 64,
            height: 64,
        };
        let u = MxcfbUpdateDataMtk::wetink_du_partial(r, 7);
        assert_eq!(u.update_region, r);
        assert_eq!(u.waveform_mode, waveform::DU);
        assert_eq!(u.waveform_mode, 1);
        assert_eq!(u.update_mode, UPDATE_MODE_PARTIAL);
        assert_eq!(u.update_mode, 0);
        assert_eq!(u.update_marker, 7);
        assert_eq!(u.temp, TEMP_USE_AMBIENT);
        assert_eq!(u.temp, 0x1000);
        assert_eq!(u.flags, 0);
        assert_eq!(u.dither_mode, 0);
        assert_eq!(u.quant_bit, 0);
        assert_eq!(u.alt_buffer_data, MxcfbAltBufferData::default());
        assert_eq!(u.swipe_data, MxcfbSwipeData::default());
        assert_eq!(u.hist_bw_waveform_mode, waveform::DU);
        assert_eq!(u.hist_gray_waveform_mode, waveform::GC16);
        assert_eq!(u.ts_pxp, 0);
        assert_eq!(u.ts_epdc, 0);
        // Distinct from ordinary GC16 path.
        let gc = MxcfbUpdateDataMtk::conservative_partial(r, 7);
        assert_ne!(u.waveform_mode, gc.waveform_mode);
    }

    #[test]
    fn waveform_constants_match_eips_name_table() {
        assert_eq!(waveform::DU, 1);
        assert_eq!(waveform::GC16, 2);
        assert_eq!(waveform::GL16, 3);
        assert_eq!(waveform::REAGL, 4);
        assert_eq!(waveform::REAGLD, 5);
        assert_eq!(waveform::A2, 6);
        assert_eq!(waveform::GCK16, 8);
        assert_eq!(waveform::GLKW16, 9);
        assert_eq!(waveform::DUNM, 12);
        assert_eq!(waveform::AUTO, 257);
    }
}
