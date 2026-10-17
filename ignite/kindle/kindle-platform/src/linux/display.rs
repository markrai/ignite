//! Framebuffer / e-ink refresh.
//!
//! `FBIOGET_*` characterization, mmap, and HWTCON send-update / wait (from Amazon `eips`
//! evidence in [`crate::hwtcon_abi`]). Gate E1 adds a narrow `MXCFB_SET_STYLUS_MODE`
//! wrapper; callers forbid ordinary GC16 SEND while armed and allow stock WetInk DU SEND
//! only while armed (Gate E1.2).
//! See `docs/KINDLE_PROBE.md`.
//!
//! Ordinary Rust error paths after the first modified framebuffer byte should restore via
//! callers (see `ink-probe --fb-test --confirm-write` RAII guard). SIGKILL / power loss /
//! kernel crash can bypass userspace restoration. The same caveat applies to stylus-mode
//! disarm via [`StylusModeGuard`].

use std::ptr;

use crate::display::{DirtyRegion, DisplaySurface, FastInkMode, RefreshMode};
use crate::error::{PlatformError, PlatformResult};
use crate::fb_polarity::{copy_logical_rect_to_hw, hardware_complement};
use crate::fb_sample::{sample_visible_grid, FbGridSample, FlushCopyStats};
use crate::fbdev_abi::{self, FbFixScreeninfo, FbGeometry, FbVarScreeninfo, PanelRectError};
use crate::hwtcon_abi::{
    MxcfbRect, MxcfbUpdateDataMtk, MxcfbUpdateMarkerData, MXCFB_SEND_UPDATE_MTK,
    MXCFB_SET_STYLUS_MODE, MXCFB_WAIT_FOR_UPDATE_COMPLETE,
};
use crate::linux::clock::LinuxMonotonicClock;
use crate::linux::fd::OwnedFd;
use crate::stylus_mode_ctrl::validate_e1_stylus_mode;
use crate::time::{MonotonicClock, TimestampUs};

const DEFAULT_FB: &str = "/dev/fb0";

/// Timing + marker result of one send + wait pair.
///
/// `send_ioctl_us` / `wait_ioctl_us` are syscall wall times on `CLOCK_MONOTONIC`.
/// They are **not** display / pigment / pen latency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshIoctlResult {
    pub marker: u32,
    pub collision_test: u32,
    pub send_ioctl_us: TimestampUs,
    pub wait_ioctl_us: TimestampUs,
}

/// Result of [`MappedFramebuffer::send_update`] (no wait).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendUpdateResult {
    pub marker: u32,
    pub send_ioctl_us: TimestampUs,
}

/// Result of [`MappedFramebuffer::wait_for_update`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitUpdateResult {
    pub marker: u32,
    pub collision_test: u32,
    pub wait_ioctl_us: TimestampUs,
}

/// Result of [`MappedFramebuffer::set_stylus_mode`] (syscall duration only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StylusModeIoctlResult {
    pub mode: u32,
    pub ioctl_rc: i32,
    pub ioctl_errno: i32,
    pub set_stylus_mode_ioctl_us: TimestampUs,
}

/// Open `/dev/fb0` and query fixed + variable screen info (no mmap, no refresh).
pub fn query_fb_info(path: &str) -> PlatformResult<(FbFixScreeninfo, FbVarScreeninfo)> {
    let fd = OwnedFd::open_read(path, false)?;
    let fix = get_fix_screeninfo(&fd)?;
    let var = get_var_screeninfo(&fd)?;
    Ok((fix, var))
}

fn get_fix_screeninfo(fd: &OwnedFd) -> PlatformResult<FbFixScreeninfo> {
    let mut info = FbFixScreeninfo::default();
    let rc = unsafe {
        libc::ioctl(
            fd.raw(),
            fbdev_abi::FBIOGET_FSCREENINFO as _,
            &mut info as *mut FbFixScreeninfo as *mut libc::c_void,
        )
    };
    if rc < 0 {
        return Err(PlatformError::Io(format!(
            "FBIOGET_FSCREENINFO: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(info)
}

fn get_var_screeninfo(fd: &OwnedFd) -> PlatformResult<FbVarScreeninfo> {
    let mut info = FbVarScreeninfo::default();
    let rc = unsafe {
        libc::ioctl(
            fd.raw(),
            fbdev_abi::FBIOGET_VSCREENINFO as _,
            &mut info as *mut FbVarScreeninfo as *mut libc::c_void,
        )
    };
    if rc < 0 {
        return Err(PlatformError::Io(format!(
            "FBIOGET_VSCREENINFO: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(info)
}

fn panel_rect_err(e: PanelRectError) -> PlatformError {
    PlatformError::Io(format!("panel rect rejected: {e:?}"))
}

/// Framebuffer geometry only (no mmap).
#[derive(Debug)]
pub struct LinuxFramebuffer {
    pub path: String,
    pub fix: FbFixScreeninfo,
    pub var: FbVarScreeninfo,
    pub geometry: FbGeometry,
}

impl LinuxFramebuffer {
    /// Open default `/dev/fb0` and read geometry only (no mmap).
    pub fn open_info_only() -> PlatformResult<Self> {
        Self::open_info_only_path(DEFAULT_FB)
    }

    pub fn open_info_only_path(path: impl Into<String>) -> PlatformResult<Self> {
        let path = path.into();
        let (fix, var) = query_fb_info(&path)?;
        let geometry = FbGeometry::from_infos(&fix, &var);
        Ok(Self {
            path,
            fix,
            var,
            geometry,
        })
    }

    /// Clip a region to the panel before it could reach a refresh ioctl.
    pub fn clip(&self, region: DirtyRegion) -> Option<DirtyRegion> {
        region.clamp_to(self.geometry.xres, self.geometry.yres)
    }
}

impl DisplaySurface for LinuxFramebuffer {
    fn width(&self) -> u32 {
        self.geometry.xres
    }

    fn height(&self) -> u32 {
        self.geometry.yres
    }

    fn draw_polyline_black(
        &mut self,
        _points: &[(f32, f32)],
    ) -> PlatformResult<Option<DirtyRegion>> {
        Err(PlatformError::Unsupported(
            "use MappedFramebuffer for pixel writes; LinuxFramebuffer is info-only",
        ))
    }

    fn refresh(&mut self, _region: DirtyRegion, _mode: RefreshMode) -> PlatformResult<()> {
        Err(PlatformError::Unsupported(
            "use MappedFramebuffer::send_update_wait for HWTCON refresh",
        ))
    }
}

/// Memory-mapped `/dev/fb0` with HWTCON send-update / wait.
///
/// Intentionally **not** `Send`/`Sync`: the mmap pointer stays thread-confined.
/// Does not change `yoffset` / pan. Stylus mode is opt-in via [`Self::set_stylus_mode`] /
/// [`StylusModeGuard`] (Gate E1); Drop never issues framebuffer refreshes (ordinary or wet).
pub struct MappedFramebuffer {
    fd: OwnedFd,
    pub path: String,
    pub fix: FbFixScreeninfo,
    pub var: FbVarScreeninfo,
    pub geometry: FbGeometry,
    map: *mut u8,
    map_len: usize,
    next_marker: u32,
    /// Canonical 0x00-black compositor buffer (same stride/length as mmap).
    logical: Vec<u8>,
    /// `true` only for Lab126 `GRAYSCALE_8BIT` (grayscale=1).
    complement: bool,
    last_flush: Option<FlushCopyStats>,
    last_send: Option<MxcfbRect>,
}

impl MappedFramebuffer {
    pub fn open_default() -> PlatformResult<Self> {
        Self::open_path(DEFAULT_FB)
    }

    /// Open RDWR + mmap. Caller must already have validated Gate-B preconditions when used
    /// from the diagnostic probe.
    pub fn open_path(path: impl Into<String>) -> PlatformResult<Self> {
        let path = path.into();
        let fd = OwnedFd::open_read_write(&path, false)?;
        let fix = get_fix_screeninfo(&fd)?;
        let var = get_var_screeninfo(&fd)?;
        let geometry = FbGeometry::from_infos(&fix, &var);

        if geometry.bits_per_pixel != 8 {
            return Err(PlatformError::Unsupported(
                "MappedFramebuffer requires 8bpp",
            ));
        }

        let map_len = fix.smem_len as usize;
        if map_len == 0 {
            return Err(PlatformError::Io("smem_len is 0".into()));
        }
        let map = unsafe {
            libc::mmap(
                ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.raw(),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(PlatformError::Io(format!(
                "mmap: {}",
                std::io::Error::last_os_error()
            )));
        }

        Ok(Self {
            fd,
            path,
            fix,
            var,
            geometry,
            map: map as *mut u8,
            map_len,
            next_marker: 1,
            logical: vec![0xFF; map_len],
            complement: hardware_complement(var.grayscale),
            last_flush: None,
            last_send: None,
        })
    }

    pub fn map_len(&self) -> usize {
        self.map_len
    }

    /// Canonical logical framebuffer (0x00 black / 0xFF white), including stride padding.
    /// Hardware mmap is updated from this buffer at fill/restore/SEND time.
    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        &mut self.logical
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.logical
    }

    /// Copy a virtual-space rectangle from the logical buffer into mmap, encoded.
    fn flush_virtual_rect(&mut self, vx: u32, vy: u32, w: u32, h: u32) -> PlatformResult<()> {
        let stride = self.geometry.line_length;
        let complement = self.complement;
        let map = unsafe { std::slice::from_raw_parts_mut(self.map, self.map_len) };
        let t0 = std::time::Instant::now();
        copy_logical_rect_to_hw(&self.logical, map, stride, vx, vy, w, h, complement)
            .ok_or_else(|| PlatformError::Io("polarity copy to mmap failed".into()))?;
        let copy_us = t0.elapsed().as_micros() as u64;
        self.last_flush = Some(FlushCopyStats {
            vx,
            vy,
            w,
            h,
            bytes: w.saturating_mul(h),
            copy_us,
            complement,
        });
        Ok(())
    }

    pub fn last_flush_copy(&self) -> Option<FlushCopyStats> {
        self.last_flush
    }

    pub fn last_send_region(&self) -> Option<MxcfbRect> {
        self.last_send
    }

    /// Partial visible-grid agreement after a WAIT. Read-only; never writes mmap or logical.
    pub fn sample_visible_agreement(&self) -> FbGridSample {
        let hw = unsafe { std::slice::from_raw_parts(self.map, self.map_len) };
        sample_visible_grid(
            &self.logical,
            hw,
            crate::fb_sample::SampleLayout {
                stride: self.geometry.line_length,
                xoffset: self.geometry.xoffset,
                yoffset: self.geometry.yoffset,
                xres: self.geometry.xres,
                yres: self.geometry.yres,
            },
            self.complement,
        )
    }

    fn virtual_xy(&self, x: u32, y: u32) -> PlatformResult<(u32, u32)> {
        let vx = self
            .geometry
            .xoffset
            .checked_add(x)
            .ok_or_else(|| PlatformError::Io("xoffset+x overflows".into()))?;
        let vy = self
            .geometry
            .yoffset
            .checked_add(y)
            .ok_or_else(|| PlatformError::Io("yoffset+y overflows".into()))?;
        Ok((vx, vy))
    }

    /// Snapshot visible panel-space pixels (no stride / off-visible bytes in the Vec).
    pub fn extract_rect(&self, x: u32, y: u32, w: u32, h: u32) -> PlatformResult<Vec<u8>> {
        self.geometry
            .check_panel_rect(x, y, w, h)
            .map_err(panel_rect_err)?;
        let (vx, vy) = self.virtual_xy(x, y)?;
        fbdev_abi::rect8::extract(&self.logical, self.geometry.line_length, vx, vy, w, h)
            .ok_or_else(|| PlatformError::Io("extract_rect failed".into()))
    }

    pub fn restore_rect(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        pixels: &[u8],
    ) -> PlatformResult<()> {
        self.geometry
            .check_panel_rect(x, y, w, h)
            .map_err(panel_rect_err)?;
        let (vx, vy) = self.virtual_xy(x, y)?;
        let g = self.geometry;
        fbdev_abi::rect8::restore(&mut self.logical, g.line_length, vx, vy, w, h, pixels)
            .ok_or_else(|| PlatformError::Io("restore_rect failed".into()))?;
        self.flush_virtual_rect(vx, vy, w, h)
    }

    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, value: u8) -> PlatformResult<()> {
        self.geometry
            .check_panel_rect(x, y, w, h)
            .map_err(panel_rect_err)?;
        let (vx, vy) = self.virtual_xy(x, y)?;
        let g = self.geometry;
        fbdev_abi::rect8::fill(&mut self.logical, g.line_length, vx, vy, w, h, value)
            .ok_or_else(|| PlatformError::Io("fill_rect failed".into()))?;
        self.flush_virtual_rect(vx, vy, w, h)
    }

    /// Submit a conservative GC16 partial update **without** waiting for completion.
    ///
    /// Used by Gate D streaming so pen input is not serialized against panel settle.
    /// Prefer [`Self::send_update_gc16`] at new call sites for an explicit waveform name.
    pub fn send_update(&mut self, region: MxcfbRect) -> PlatformResult<SendUpdateResult> {
        self.send_update_gc16(region)
    }

    /// Ordinary GC16 / PARTIAL / ambient SEND (Gate B/C/D). Does not wait.
    pub fn send_update_gc16(&mut self, region: MxcfbRect) -> PlatformResult<SendUpdateResult> {
        self.geometry
            .check_panel_rect(region.left, region.top, region.width, region.height)
            .map_err(panel_rect_err)?;
        let marker = self.next_marker;
        self.next_marker = self.next_marker.wrapping_add(1).max(1);
        let update = MxcfbUpdateDataMtk::conservative_partial(region, marker);
        self.send_raw_update(update)
    }

    /// GC16 / FULL / ambient SEND — clears prior-panel residue (page reconstruct).
    pub fn send_update_gc16_full(&mut self, region: MxcfbRect) -> PlatformResult<SendUpdateResult> {
        self.geometry
            .check_panel_rect(region.left, region.top, region.width, region.height)
            .map_err(panel_rect_err)?;
        let marker = self.next_marker;
        self.next_marker = self.next_marker.wrapping_add(1).max(1);
        let update = MxcfbUpdateDataMtk::conservative_full(region, marker);
        self.send_raw_update(update)
    }

    /// Stock WetInk DU / PARTIAL / ambient SEND (Gate E1.1 / E1.2). Does not wait.
    ///
    /// Intended only while `MXCFB_SET_STYLUS_MODE(1)` is active (caller-enforced).
    pub fn send_update_wetink_du(&mut self, region: MxcfbRect) -> PlatformResult<SendUpdateResult> {
        self.geometry
            .check_panel_rect(region.left, region.top, region.width, region.height)
            .map_err(panel_rect_err)?;
        let marker = self.next_marker;
        self.next_marker = self.next_marker.wrapping_add(1).max(1);
        let update = MxcfbUpdateDataMtk::wetink_du_partial(region, marker);
        self.send_raw_update(update)
    }

    fn send_raw_update(&mut self, update: MxcfbUpdateDataMtk) -> PlatformResult<SendUpdateResult> {
        let region = update.update_region;
        let (vx, vy) = self.virtual_xy(region.left, region.top)?;
        self.flush_virtual_rect(vx, vy, region.width, region.height)?;
        self.last_send = Some(region);
        let clock = LinuxMonotonicClock;
        let t0 = clock.now_us();
        let rc = unsafe {
            libc::ioctl(
                self.fd.raw(),
                MXCFB_SEND_UPDATE_MTK as _,
                &update as *const MxcfbUpdateDataMtk as *const libc::c_void,
            )
        };
        let t1 = clock.now_us();
        if rc < 0 {
            return Err(PlatformError::Io(format!(
                "MXCFB_SEND_UPDATE_MTK: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(SendUpdateResult {
            marker: update.update_marker,
            send_ioctl_us: t1.saturating_sub(t0),
        })
    }

    /// Wait for a previously submitted update marker to complete.
    pub fn wait_for_update(&mut self, marker: u32) -> PlatformResult<WaitUpdateResult> {
        let mut wait = MxcfbUpdateMarkerData {
            update_marker: marker,
            collision_test: 0,
        };
        let clock = LinuxMonotonicClock;
        let t0 = clock.now_us();
        let rc = unsafe {
            libc::ioctl(
                self.fd.raw(),
                MXCFB_WAIT_FOR_UPDATE_COMPLETE as _,
                &mut wait as *mut MxcfbUpdateMarkerData as *mut libc::c_void,
            )
        };
        let t1 = clock.now_us();
        if rc < 0 {
            return Err(PlatformError::Io(format!(
                "MXCFB_WAIT_FOR_UPDATE_COMPLETE: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(WaitUpdateResult {
            marker: wait.update_marker,
            collision_test: wait.collision_test,
            wait_ioctl_us: t1.saturating_sub(t0),
        })
    }

    /// Submit a conservative GC16 partial update and wait for completion.
    pub fn send_update_wait(&mut self, region: MxcfbRect) -> PlatformResult<RefreshIoctlResult> {
        let sent = self.send_update_gc16(region)?;
        let waited = self.wait_for_update(sent.marker)?;
        Ok(RefreshIoctlResult {
            marker: waited.marker,
            collision_test: waited.collision_test,
            send_ioctl_us: sent.send_ioctl_us,
            wait_ioctl_us: waited.wait_ioctl_us,
        })
    }

    /// Full-refresh GC16 + wait (page reconstruct / HUD; clears PARTIAL residue).
    pub fn send_update_wait_full(
        &mut self,
        region: MxcfbRect,
    ) -> PlatformResult<RefreshIoctlResult> {
        let sent = self.send_update_gc16_full(region)?;
        let waited = self.wait_for_update(sent.marker)?;
        Ok(RefreshIoctlResult {
            marker: waited.marker,
            collision_test: waited.collision_test,
            send_ioctl_us: sent.send_ioctl_us,
            wait_ioctl_us: waited.wait_ioctl_us,
        })
    }

    /// Restore panel bytes then GC16 partial + wait (best-effort recovery helper).
    pub fn restore_and_refresh(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        pixels: &[u8],
    ) -> PlatformResult<RefreshIoctlResult> {
        self.restore_rect(x, y, w, h, pixels)?;
        self.send_update_wait(MxcfbRect {
            top: y,
            left: x,
            width: w,
            height: h,
        })
    }

    /// `MXCFB_SET_STYLUS_MODE` on this framebuffer fd (Gate E1).
    ///
    /// Accepts only modes `0` (DISABLED) and `1` (WITH_NO_TPS). Does **not** issue
    /// `SEND_UPDATE`. Callers enforce update class: ordinary GC16 only when disarmed;
    /// stock WetInk DU SEND only while mode `1` is active (Gate E1.2).
    pub fn set_stylus_mode(&mut self, mode: u32) -> PlatformResult<StylusModeIoctlResult> {
        validate_e1_stylus_mode(mode)?;
        let mut arg = mode;
        let clock = LinuxMonotonicClock;
        let t0 = clock.now_us();
        let rc = unsafe {
            libc::ioctl(
                self.fd.raw(),
                MXCFB_SET_STYLUS_MODE as _,
                &mut arg as *mut u32 as *mut libc::c_void,
            )
        };
        let t1 = clock.now_us();
        let errno = if rc < 0 {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        } else {
            0
        };
        let result = StylusModeIoctlResult {
            mode,
            ioctl_rc: rc,
            ioctl_errno: errno,
            set_stylus_mode_ioctl_us: t1.saturating_sub(t0),
        };
        if rc < 0 {
            return Err(PlatformError::Io(format!(
                "MXCFB_SET_STYLUS_MODE({mode}): rc={rc} errno={errno} ({})",
                std::io::Error::from_raw_os_error(errno)
            )));
        }
        Ok(result)
    }
}

/// RAII arm/disarm for stock stylus mode on an open [`MappedFramebuffer`].
///
/// After a successful mode=`1`, Drop best-effort issues mode=`0` and **never** issues
/// framebuffer refreshes (ordinary or wet). Do not ordinary-refresh until disarm succeeds.
/// SIGKILL / power loss / kernel failure may bypass userspace disarm.
pub struct StylusModeGuard<'a> {
    fb: &'a mut MappedFramebuffer,
    armed: bool,
}

impl<'a> StylusModeGuard<'a> {
    /// Issue mode=`1` (`WITH_NO_TPS`). On success the guard is armed.
    pub fn arm(fb: &'a mut MappedFramebuffer) -> PlatformResult<(Self, StylusModeIoctlResult)> {
        let result = fb.set_stylus_mode(crate::hwtcon_abi::stylus_mode::WITH_NO_TPS)?;
        Ok((Self { fb, armed: true }, result))
    }

    /// Explicit disarm (mode=`0`). Consumes the guard so Drop is a no-op.
    pub fn disarm(mut self) -> PlatformResult<StylusModeIoctlResult> {
        let result = self
            .fb
            .set_stylus_mode(crate::hwtcon_abi::stylus_mode::DISABLED)?;
        self.armed = false;
        Ok(result)
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    pub fn fb(&mut self) -> &mut MappedFramebuffer {
        self.fb
    }
}

impl Drop for StylusModeGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort only. Never SEND_UPDATE while (or after failed) disarm attempt.
            let _ = self
                .fb
                .set_stylus_mode(crate::hwtcon_abi::stylus_mode::DISABLED);
            self.armed = false;
        }
    }
}

impl Drop for MappedFramebuffer {
    fn drop(&mut self) {
        if !self.map.is_null() && self.map_len != 0 {
            unsafe {
                libc::munmap(self.map as *mut libc::c_void, self.map_len);
            }
            self.map = ptr::null_mut();
        }
    }
}

impl std::fmt::Debug for MappedFramebuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedFramebuffer")
            .field("path", &self.path)
            .field("map_len", &self.map_len)
            .field("geometry", &self.geometry)
            .field("complement", &self.complement)
            .finish()
    }
}

#[derive(Debug, Default)]
pub struct LinuxFastInk;

impl FastInkMode for LinuxFastInk {
    fn enable(&mut self) -> PlatformResult<()> {
        Err(PlatformError::Unsupported(
            "stylus / fast-ink mode out of scope (no MXCFB_SET_STYLUS_MODE)",
        ))
    }

    fn disable(&mut self) -> PlatformResult<()> {
        Err(PlatformError::Unsupported(
            "stylus / fast-ink mode out of scope (no MXCFB_SET_STYLUS_MODE)",
        ))
    }
}
