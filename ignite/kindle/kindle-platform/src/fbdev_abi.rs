//! Linux fbdev UAPI: `fb_fix_screeninfo` / `fb_var_screeninfo` and get-info ioctls.
//!
//! Layout and ioctl numbers come from Linux `include/uapi/linux/fb.h` (kernel UAPI).
//! `unsigned long` fields are target-width-dependent (`KernelLong` / pointer width).
//!
//! Pure layout arithmetic is host-testable; the ioctl calls live under `cfg(unix)`.

use crate::evdev_abi::KernelLong;

/// `FBIOGET_VSCREENINFO` — literal `0x4600` from UAPI (`'F' << 8`).
pub const FBIOGET_VSCREENINFO: u32 = 0x4600;
/// `FBIOPUT_VSCREENINFO` — not used in this slice (we do not reconfigure the panel).
pub const FBIOPUT_VSCREENINFO: u32 = 0x4601;
/// `FBIOGET_FSCREENINFO` — literal `0x4602` from UAPI.
pub const FBIOGET_FSCREENINFO: u32 = 0x4602;

/// `FB_TYPE_PACKED_PIXELS`
pub const FB_TYPE_PACKED_PIXELS: u32 = 0;
/// `FB_VISUAL_MONO01` — 1 = black, 0 = white
pub const FB_VISUAL_MONO01: u32 = 0;
/// `FB_VISUAL_MONO10` — 1 = white, 0 = black
pub const FB_VISUAL_MONO10: u32 = 1;
/// `FB_VISUAL_TRUECOLOR`
pub const FB_VISUAL_TRUECOLOR: u32 = 2;
/// `FB_VISUAL_PSEUDOCOLOR`
pub const FB_VISUAL_PSEUDOCOLOR: u32 = 3;
/// `FB_VISUAL_STATIC_PSEUDOCOLOR`
pub const FB_VISUAL_STATIC_PSEUDOCOLOR: u32 = 5;
/// `FB_ROTATE_CCW` — common reported rotate value on this Scribe class
pub const FB_ROTATE_CCW: u32 = 3;

/// `struct fb_bitfield` from Linux UAPI.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FbBitfield {
    pub offset: u32,
    pub length: u32,
    pub msb_right: u32,
}

/// `struct fb_fix_screeninfo` — fixed screen information.
///
/// `smem_start` / `mmio_start` are `__kernel_ulong_t` / `unsigned long` sized.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FbFixScreeninfo {
    pub id: [u8; 16],
    pub smem_start: KernelLong,
    pub smem_len: u32,
    pub type_: u32,
    pub type_aux: u32,
    pub visual: u32,
    pub xpanstep: u16,
    pub ypanstep: u16,
    pub ywrapstep: u16,
    pub line_length: u32,
    pub mmio_start: KernelLong,
    pub mmio_len: u32,
    pub accel: u32,
    pub capabilities: u16,
    pub reserved: [u16; 2],
}

impl FbFixScreeninfo {
    pub fn id_str(&self) -> &str {
        let end = self
            .id
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.id.len());
        std::str::from_utf8(&self.id[..end]).unwrap_or("")
    }
}

/// `struct fb_var_screeninfo` — variable screen information.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FbVarScreeninfo {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub grayscale: u32,
    pub red: FbBitfield,
    pub green: FbBitfield,
    pub blue: FbBitfield,
    pub transp: FbBitfield,
    pub nonstd: u32,
    pub activate: u32,
    pub height: u32,
    pub width: u32,
    pub accel_flags: u32,
    pub pixclock: u32,
    pub left_margin: u32,
    pub right_margin: u32,
    pub upper_margin: u32,
    pub lower_margin: u32,
    pub hsync_len: u32,
    pub vsync_len: u32,
    pub sync: u32,
    pub vmode: u32,
    pub rotate: u32,
    pub colorspace: u32,
    pub reserved: [u32; 4],
}

/// Derived, overflow-checked geometry helpers. Pure arithmetic for host tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FbGeometry {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub line_length: u32,
    pub smem_len: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FbGeometryReport {
    pub visible_row_bytes: u32,
    /// `xres_virtual * (bpp/8)` when bpp is byte-aligned; else 0.
    pub virtual_row_pixel_bytes: u32,
    /// Bytes in each row that are virtual pixels beyond the visible width.
    pub off_visible_virtual_bytes: u32,
    /// `line_length - virtual_row_pixel_bytes` when line_length is at least that wide.
    pub stride_padding_bytes: u32,
    pub visible_page_bytes: u64,
    pub virtual_minimum_bytes: u64,
    pub line_length_times_yres: u64,
    pub line_length_times_yres_virtual: u64,
    pub notes: Vec<&'static str>,
}

/// Why a panel-space rectangle was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelRectError {
    ZeroWidth,
    ZeroHeight,
    OriginXOutOfRange,
    OriginYOutOfRange,
    WidthOverflow,
    HeightOverflow,
    ExtendsPastRightEdge,
    ExtendsPastBottomEdge,
}

impl FbGeometry {
    pub fn from_infos(fix: &FbFixScreeninfo, var: &FbVarScreeninfo) -> Self {
        Self {
            xres: var.xres,
            yres: var.yres,
            xres_virtual: var.xres_virtual,
            yres_virtual: var.yres_virtual,
            xoffset: var.xoffset,
            yoffset: var.yoffset,
            bits_per_pixel: var.bits_per_pixel,
            line_length: fix.line_length,
            smem_len: fix.smem_len,
        }
    }

    /// Bytes per visible row from bpp (not `line_length`).
    pub fn visible_row_bytes(&self) -> Option<u32> {
        self.row_bytes_for_width(self.xres)
    }

    /// Bytes per virtual-width row from bpp (not `line_length`).
    pub fn virtual_row_pixel_bytes(&self) -> Option<u32> {
        self.row_bytes_for_width(self.xres_virtual)
    }

    fn row_bytes_for_width(&self, width_px: u32) -> Option<u32> {
        if self.bits_per_pixel == 0 || !self.bits_per_pixel.is_multiple_of(8) {
            return None;
        }
        let bpp_bytes = self.bits_per_pixel / 8;
        width_px.checked_mul(bpp_bytes)
    }

    /// Panel-space rectangle must lie entirely in `0..xres` × `0..yres` (checked math).
    pub fn check_panel_rect(&self, x: u32, y: u32, w: u32, h: u32) -> Result<(), PanelRectError> {
        if w == 0 {
            return Err(PanelRectError::ZeroWidth);
        }
        if h == 0 {
            return Err(PanelRectError::ZeroHeight);
        }
        if x >= self.xres {
            return Err(PanelRectError::OriginXOutOfRange);
        }
        if y >= self.yres {
            return Err(PanelRectError::OriginYOutOfRange);
        }
        let x_end = x.checked_add(w).ok_or(PanelRectError::WidthOverflow)?;
        let y_end = y.checked_add(h).ok_or(PanelRectError::HeightOverflow)?;
        if x_end > self.xres {
            return Err(PanelRectError::ExtendsPastRightEdge);
        }
        if y_end > self.yres {
            return Err(PanelRectError::ExtendsPastBottomEdge);
        }
        Ok(())
    }

    pub fn report(&self) -> FbGeometryReport {
        let mut notes = Vec::new();
        let visible_row_bytes = self.visible_row_bytes().unwrap_or(0);
        let virtual_row_pixel_bytes = self.virtual_row_pixel_bytes().unwrap_or(0);

        let (off_visible_virtual_bytes, stride_padding_bytes) = if visible_row_bytes == 0
            || virtual_row_pixel_bytes == 0
        {
            notes.push("row byte widths unavailable (bpp not byte-aligned or zero)");
            (0, 0)
        } else {
            let off_vis = virtual_row_pixel_bytes.saturating_sub(visible_row_bytes);
            let stride_pad = self.line_length.saturating_sub(virtual_row_pixel_bytes);
            if self.line_length < virtual_row_pixel_bytes {
                notes.push("line_length < virtual_row_pixel_bytes (inconsistent)");
            }
            if off_vis > 0 {
                notes.push("virtual width extends past visible xres (off-visible virtual pixels)");
            }
            if stride_pad > 0 {
                notes.push("line_length has stride padding beyond virtual pixel row");
            }
            (off_vis, stride_pad)
        };

        let line_length_times_yres = (self.line_length as u64).saturating_mul(self.yres as u64);
        let line_length_times_yres_virtual =
            (self.line_length as u64).saturating_mul(self.yres_virtual as u64);
        let visible_page_bytes = line_length_times_yres;
        let virtual_minimum_bytes = line_length_times_yres_virtual;

        if self.smem_len as u64 != 0 && (self.smem_len as u64) < virtual_minimum_bytes {
            notes.push("smem_len < line_length * yres_virtual");
        }
        if self.smem_len as u64 != 0 && (self.smem_len as u64) < visible_page_bytes {
            notes.push("smem_len < line_length * yres (visible page does not fit)");
        }
        if self.xoffset != 0 {
            notes.push("nonzero xoffset: visible window is shifted in virtual X");
        }
        if self.yoffset != 0 {
            notes.push("nonzero yoffset: visible window is shifted in virtual Y");
        }
        if self.yres.checked_mul(2) == Some(self.yres_virtual) {
            notes.push(
                "yres_virtual == 2*yres (suggests double buffering; not proven without yoffset flips)",
            );
        }
        if let Some(span) = self.xoffset.checked_add(self.xres) {
            if self.xres_virtual != 0 && span > self.xres_virtual {
                notes.push("xoffset+xres exceeds xres_virtual");
            }
        } else {
            notes.push("xoffset+xres overflows u32");
        }
        if let Some(span) = self.yoffset.checked_add(self.yres) {
            if self.yres_virtual != 0 && span > self.yres_virtual {
                notes.push("yoffset+yres exceeds yres_virtual");
            }
        } else {
            notes.push("yoffset+yres overflows u32");
        }

        FbGeometryReport {
            visible_row_bytes,
            virtual_row_pixel_bytes,
            off_visible_virtual_bytes,
            stride_padding_bytes,
            visible_page_bytes,
            virtual_minimum_bytes,
            line_length_times_yres,
            line_length_times_yres_virtual,
            notes,
        }
    }

    /// Byte offset of pixel `(x,y)` in the **visible** page, accounting for xoffset/yoffset
    /// into the virtual framebuffer and using `line_length` stride.
    ///
    /// Coordinates are relative to the visible panel (`0..xres`, `0..yres`).
    pub fn byte_offset_visible_8bpp(&self, x: u32, y: u32) -> Option<usize> {
        if self.bits_per_pixel != 8 {
            return None;
        }
        self.check_panel_rect(x, y, 1, 1).ok()?;
        let vx = self.xoffset.checked_add(x)?;
        let vy = self.yoffset.checked_add(y)?;
        if self.xres_virtual != 0 && vx >= self.xres_virtual {
            return None;
        }
        if self.yres_virtual != 0 && vy >= self.yres_virtual {
            return None;
        }
        let row = (vy as u64).checked_mul(self.line_length as u64)?;
        let off = row.checked_add(vx as u64)?;
        usize::try_from(off).ok()
    }

    /// Mapping length candidate: prefer `smem_len` when it covers the virtual minimum.
    pub fn mapping_len_candidates(&self) -> (u64, u64) {
        (
            self.smem_len as u64,
            (self.line_length as u64).saturating_mul(self.yres_virtual as u64),
        )
    }
}

/// Snapshot / restore helpers for a rectangle in an 8bpp buffer with stride.
pub mod rect8 {
    /// Extract `w*h` visible pixels (no padding) from a stride buffer.
    pub fn extract(
        buf: &[u8],
        line_length: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Option<Vec<u8>> {
        let ll = line_length as usize;
        let w = w as usize;
        let h = h as usize;
        let x = x as usize;
        let y = y as usize;
        if w == 0 || h == 0 || x.checked_add(w)? > ll {
            return None;
        }
        let mut out = Vec::with_capacity(w.checked_mul(h)?);
        for row in 0..h {
            let start = y.checked_add(row)?.checked_mul(ll)?.checked_add(x)?;
            let end = start.checked_add(w)?;
            if end > buf.len() {
                return None;
            }
            out.extend_from_slice(&buf[start..end]);
        }
        Some(out)
    }

    /// Write snapshot bytes back (visible pixels only; does not touch padding).
    pub fn restore(
        buf: &mut [u8],
        line_length: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        pixels: &[u8],
    ) -> Option<()> {
        let ll = line_length as usize;
        let w = w as usize;
        let h = h as usize;
        let x = x as usize;
        let y = y as usize;
        if pixels.len() != w.checked_mul(h)? {
            return None;
        }
        if w == 0 || h == 0 || x.checked_add(w)? > ll {
            return None;
        }
        for row in 0..h {
            let start = y.checked_add(row)?.checked_mul(ll)?.checked_add(x)?;
            let end = start.checked_add(w)?;
            if end > buf.len() {
                return None;
            }
            buf[start..end].copy_from_slice(&pixels[row * w..(row + 1) * w]);
        }
        Some(())
    }

    /// Fill a rectangle with a solid byte (visible pixels only).
    pub fn fill(
        buf: &mut [u8],
        line_length: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        value: u8,
    ) -> Option<()> {
        let ll = line_length as usize;
        let w = w as usize;
        let h = h as usize;
        let x = x as usize;
        let y = y as usize;
        if w == 0 || h == 0 || x.checked_add(w)? > ll {
            return None;
        }
        for row in 0..h {
            let start = y.checked_add(row)?.checked_mul(ll)?.checked_add(x)?;
            let end = start.checked_add(w)?;
            if end > buf.len() {
                return None;
            }
            for b in &mut buf[start..end] {
                *b = value;
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_linux_uapi() {
        assert_eq!(FBIOGET_VSCREENINFO, 0x4600);
        assert_eq!(FBIOPUT_VSCREENINFO, 0x4601);
        assert_eq!(FBIOGET_FSCREENINFO, 0x4602);
    }

    #[test]
    fn fix_struct_size_matches_target_word() {
        // id[16] + ulong + 3*u32 + 3*u16 + pad? + u32 + ulong + 2*u32 + u16 + u16[2]
        // On 32-bit: 16+4+12+6+2(pad to 4)+4+4+8+2+4 = carefully check with size_of.
        let size = core::mem::size_of::<FbFixScreeninfo>();
        if cfg!(target_pointer_width = "32") {
            // Common 32-bit layout is 68 bytes with natural alignment.
            assert_eq!(size, 68, "unexpected 32-bit fb_fix_screeninfo size {size}");
        } else if cfg!(target_pointer_width = "64") {
            assert_eq!(size, 80, "unexpected 64-bit fb_fix_screeninfo size {size}");
        }
    }

    #[test]
    fn var_struct_size_is_stable() {
        // All u32 / bitfields — 160 bytes on every ABI we care about.
        assert_eq!(core::mem::size_of::<FbVarScreeninfo>(), 160);
    }

    #[test]
    fn stride_addressing_8bpp_with_padding() {
        let g = FbGeometry {
            xres: 10,
            yres: 4,
            xres_virtual: 12,
            yres_virtual: 8,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 12,
            smem_len: 96,
        };
        assert_eq!(g.visible_row_bytes(), Some(10));
        assert_eq!(g.byte_offset_visible_8bpp(0, 0), Some(0));
        assert_eq!(g.byte_offset_visible_8bpp(9, 0), Some(9));
        assert_eq!(g.byte_offset_visible_8bpp(0, 1), Some(12));
        assert_eq!(g.byte_offset_visible_8bpp(10, 0), None);
        assert_eq!(g.byte_offset_visible_8bpp(0, 4), None);
    }

    #[test]
    fn yoffset_shifts_visible_page() {
        let g = FbGeometry {
            xres: 4,
            yres: 2,
            xres_virtual: 4,
            yres_virtual: 4,
            xoffset: 0,
            yoffset: 2,
            bits_per_pixel: 8,
            line_length: 4,
            smem_len: 16,
        };
        assert_eq!(g.byte_offset_visible_8bpp(1, 0), Some(2 * 4 + 1));
        let r = g.report();
        assert!(r.notes.iter().any(|n| n.contains("yoffset")));
    }

    #[test]
    fn rejects_non_8bpp_and_oob() {
        let g = FbGeometry {
            xres: 10,
            yres: 10,
            xres_virtual: 10,
            yres_virtual: 10,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 16,
            line_length: 20,
            smem_len: 200,
        };
        assert_eq!(g.byte_offset_visible_8bpp(0, 0), None);
        let g8 = FbGeometry {
            bits_per_pixel: 8,
            line_length: 10,
            ..g
        };
        assert_eq!(g8.byte_offset_visible_8bpp(10, 0), None);
        assert_eq!(g8.byte_offset_visible_8bpp(0, 10), None);
    }

    #[test]
    fn rect_extract_restore_round_trip() {
        let mut buf = vec![0u8; 12 * 4];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = i as u8;
        }
        let snap = rect8::extract(&buf, 12, 2, 1, 3, 2).unwrap();
        assert_eq!(snap, vec![14, 15, 16, 26, 27, 28]);
        let mut other = vec![0xffu8; buf.len()];
        rect8::restore(&mut other, 12, 2, 1, 3, 2, &snap).unwrap();
        assert_eq!(&other[14..17], &[14, 15, 16]);
        assert_eq!(&other[26..29], &[26, 27, 28]);
        // padding untouched
        assert_eq!(other[12], 0xff);
    }

    #[test]
    fn rect_fill_does_not_touch_padding() {
        let mut buf = vec![0x55u8; 8 * 2];
        rect8::fill(&mut buf, 8, 1, 0, 2, 1, 0x00).unwrap();
        assert_eq!(buf[0], 0x55);
        assert_eq!(buf[1], 0x00);
        assert_eq!(buf[2], 0x00);
        assert_eq!(buf[3], 0x55);
    }

    #[test]
    fn double_buffer_suggestion_is_note_not_fact() {
        let g = FbGeometry {
            xres: 1860,
            yres: 2480,
            xres_virtual: 1872,
            yres_virtual: 4960,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 1872,
            smem_len: 1872 * 4960,
        };
        let r = g.report();
        assert_eq!(r.visible_row_bytes, 1860);
        assert_eq!(r.virtual_row_pixel_bytes, 1872);
        assert_eq!(r.off_visible_virtual_bytes, 12);
        assert_eq!(r.stride_padding_bytes, 0);
        assert_eq!(r.line_length_times_yres, 1872 * 2480);
        assert_eq!(r.line_length_times_yres_virtual, 1872 * 4960);
        assert!(r
            .notes
            .iter()
            .any(|n| n.contains("suggests double buffering")));
        assert!(r.notes.iter().any(|n| n.contains("off-visible virtual")));
    }

    #[test]
    fn panel_rect_rejects_off_visible_virtual_x() {
        let g = FbGeometry {
            xres: 1860,
            yres: 2480,
            xres_virtual: 1872,
            yres_virtual: 4960,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 1872,
            smem_len: 1872 * 4960,
        };
        // Virtual pixels 1860..1871 exist in the row but are not panel-visible.
        assert_eq!(
            g.check_panel_rect(1860, 0, 1, 1),
            Err(PanelRectError::OriginXOutOfRange)
        );
        assert_eq!(
            g.check_panel_rect(1859, 0, 2, 1),
            Err(PanelRectError::ExtendsPastRightEdge)
        );
        assert_eq!(g.check_panel_rect(1859, 0, 1, 1), Ok(()));
        assert_eq!(g.byte_offset_visible_8bpp(1860, 0), None);
        assert_eq!(g.byte_offset_visible_8bpp(1859, 0), Some(1859));
    }

    #[test]
    fn panel_rect_edge_and_overflow_cases() {
        let g = FbGeometry {
            xres: 100,
            yres: 50,
            xres_virtual: 128,
            yres_virtual: 100,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 128,
            smem_len: 128 * 100,
        };
        assert_eq!(
            g.check_panel_rect(0, 0, 0, 1),
            Err(PanelRectError::ZeroWidth)
        );
        assert_eq!(
            g.check_panel_rect(0, 0, 1, 0),
            Err(PanelRectError::ZeroHeight)
        );
        assert_eq!(g.check_panel_rect(99, 49, 1, 1), Ok(())); // exact bottom-right
        assert_eq!(
            g.check_panel_rect(99, 49, 1, 2),
            Err(PanelRectError::ExtendsPastBottomEdge)
        );
        assert_eq!(
            g.check_panel_rect(u32::MAX, 0, 1, 1),
            Err(PanelRectError::OriginXOutOfRange)
        );
        assert_eq!(
            g.check_panel_rect(1, 0, u32::MAX, 1),
            Err(PanelRectError::WidthOverflow)
        );
        assert_eq!(
            g.check_panel_rect(0, 1, 1, u32::MAX),
            Err(PanelRectError::HeightOverflow)
        );

        let shifted = FbGeometry {
            xoffset: 10,
            yoffset: 20,
            ..g
        };
        assert_eq!(shifted.check_panel_rect(0, 0, 100, 50), Ok(()));
        assert_eq!(shifted.byte_offset_visible_8bpp(0, 0), Some(20 * 128 + 10));
        assert_eq!(
            shifted.check_panel_rect(0, 0, 101, 1),
            Err(PanelRectError::ExtendsPastRightEdge)
        );
    }
}
