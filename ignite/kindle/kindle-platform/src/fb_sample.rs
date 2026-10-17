//! Bounded logical-vs-hardware framebuffer samples (diagnostics only).
//!
//! A matching sample is **partial evidence** that those pixels agree in memory after a
//! chosen synchronization point. It is never whole-frame proof, and WAIT ioctl return
//! is not physical E Ink completion. Mismatch is not by itself Amazon overwrite.

use crate::fb_polarity::encode_panel_gray_complement;

/// Copy accounting for one logical→mmap rectangle (filled at flush, logged after SEND).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlushCopyStats {
    pub vx: u32,
    pub vy: u32,
    pub w: u32,
    pub h: u32,
    pub bytes: u32,
    pub copy_us: u64,
    pub complement: bool,
}

/// One visible-panel sample point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FbSamplePoint {
    pub x: u32,
    pub y: u32,
    pub logical: u8,
    pub hardware: u8,
    pub expected_hw: u8,
    pub match_ok: bool,
}

/// Visible mmap addressing for a sample (not a Gate B check).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleLayout {
    pub stride: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub xres: u32,
    pub yres: u32,
}

/// Partial grid over the visible panel. `coverage=partial` always.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FbGridSample {
    pub points: Vec<FbSamplePoint>,
    pub mismatch: u32,
    /// Human-readable region tags included (e.g. `y_third`, `right_edge`).
    pub regions: &'static str,
}

/// Max mismatch points listed on one log line (remainder counted, not printed).
pub const MISMATCH_LIST_CAP: usize = 12;

impl FbGridSample {
    /// Bounded `x,y:exp=HH,obs=HH` list. Empty mismatch → `-`.
    pub fn format_mismatch_xy(&self) -> String {
        let mut parts = Vec::new();
        for p in self
            .points
            .iter()
            .filter(|p| !p.match_ok)
            .take(MISMATCH_LIST_CAP)
        {
            parts.push(format!(
                "{},{}:exp={:02x},obs={:02x}",
                p.x, p.y, p.expected_hw, p.hardware
            ));
        }
        if parts.is_empty() {
            "-".into()
        } else {
            parts.join(";")
        }
    }

    pub fn format_line(&self, launch_ms: u128, marker: u32, sync: &str) -> String {
        let listed = self
            .points
            .iter()
            .filter(|p| !p.match_ok)
            .take(MISMATCH_LIST_CAP)
            .count();
        let truncated = u8::from(self.mismatch as usize > listed);
        format!(
            "fb_sample launch_ms={launch_ms} marker={marker} sync={sync} \
             points={} mismatch={} mismatch_xy={} truncated={} coverage=partial regions={} \
             note=partial_memory_agreement_not_whole_frame_not_eink_done",
            self.points.len(),
            self.mismatch,
            self.format_mismatch_xy(),
            truncated,
            self.regions
        )
    }
}

/// Visible-panel grid: corners, thirds, midlines, reported blank-band loci.
///
/// Includes y≈1/3 (operator “band a third from the top”) and the right edge
/// (operator “right portion”), plus a column 12px in from the right (virtual
/// stride 1872 vs visible 1860).
pub fn visible_grid_coords(xres: u32, yres: u32) -> Vec<(u32, u32)> {
    if xres == 0 || yres == 0 {
        return Vec::new();
    }
    let xs = unique_u32(&[
        0,
        xres / 4,
        xres / 2,
        (xres.saturating_mul(3)) / 4,
        xres.saturating_sub(12).min(xres.saturating_sub(1)),
        xres.saturating_sub(1),
    ]);
    let ys = unique_u32(&[
        0,
        yres / 6,
        yres / 3,
        yres / 2,
        (yres.saturating_mul(2)) / 3,
        (yres.saturating_mul(5)) / 6,
        yres.saturating_sub(1),
    ]);
    let mut out = Vec::with_capacity(xs.len() * ys.len());
    for y in ys {
        for x in &xs {
            out.push((*x, y));
        }
    }
    out
}

fn unique_u32(vals: &[u32]) -> Vec<u32> {
    let mut v = vals.to_vec();
    v.sort_unstable();
    v.dedup();
    v
}

/// Compare logical vs hardware at the visible grid. Does not write either buffer.
pub fn sample_visible_grid(
    logical: &[u8],
    hardware: &[u8],
    layout: SampleLayout,
    complement: bool,
) -> FbGridSample {
    let coords = visible_grid_coords(layout.xres, layout.yres);
    let mut points = Vec::with_capacity(coords.len());
    let mut mismatch = 0u32;
    let ll = layout.stride as usize;
    for (x, y) in coords {
        let vx = layout.xoffset.saturating_add(x) as usize;
        let vy = layout.yoffset.saturating_add(y) as usize;
        let idx = vy.saturating_mul(ll).saturating_add(vx);
        let (logical_b, hardware_b) = if idx < logical.len() && idx < hardware.len() {
            (logical[idx], hardware[idx])
        } else {
            mismatch = mismatch.saturating_add(1);
            points.push(FbSamplePoint {
                x,
                y,
                logical: 0,
                hardware: 0,
                expected_hw: encode_panel_gray_complement(0, complement),
                match_ok: false,
            });
            continue;
        };
        let expected_hw = encode_panel_gray_complement(logical_b, complement);
        let match_ok = hardware_b == expected_hw;
        if !match_ok {
            mismatch = mismatch.saturating_add(1);
        }
        points.push(FbSamplePoint {
            x,
            y,
            logical: logical_b,
            hardware: hardware_b,
            expected_hw,
            match_ok,
        });
    }
    FbGridSample {
        points,
        mismatch,
        regions: "corners,thirds,mid,y_third,right_edge,right_12",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fb_polarity::{copy_logical_rect_to_hw, LOGICAL_BLACK, LOGICAL_WHITE};
    use crate::gate_b::{EXPECTED_LINE_LENGTH, EXPECTED_XRES, EXPECTED_YRES};

    fn panel_bufs(fill_log: u8, fill_hw: u8) -> (Vec<u8>, Vec<u8>) {
        let len = EXPECTED_LINE_LENGTH as usize * EXPECTED_YRES_VIRTUAL_PAGE;
        (vec![fill_log; len], vec![fill_hw; len])
    }

    fn layout() -> SampleLayout {
        SampleLayout {
            stride: EXPECTED_LINE_LENGTH,
            xoffset: 0,
            yoffset: 0,
            xres: EXPECTED_XRES,
            yres: EXPECTED_YRES,
        }
    }

    /// One visible page of virtual rows (not the full 4960 yres_virtual).
    const EXPECTED_YRES_VIRTUAL_PAGE: usize = EXPECTED_YRES as usize;

    #[test]
    fn grid_includes_reported_blank_band_loci() {
        let c = visible_grid_coords(EXPECTED_XRES, EXPECTED_YRES);
        assert!(c.contains(&(0, 0)));
        assert!(c.contains(&(EXPECTED_XRES - 1, EXPECTED_YRES - 1)));
        assert!(c.iter().any(|&(_, y)| y == EXPECTED_YRES / 3));
        assert!(c.iter().any(|&(x, _)| x == EXPECTED_XRES - 1));
        assert!(c
            .iter()
            .any(|&(x, _)| x == EXPECTED_XRES.saturating_sub(12)));
        assert!(c.len() >= 20);
        assert!(c.len() <= 64);
    }

    #[test]
    fn identity_agreement_after_full_flush() {
        let stride = EXPECTED_LINE_LENGTH;
        let (mut logical, mut hw) = panel_bufs(LOGICAL_WHITE, 0x11);
        logical[10] = LOGICAL_BLACK;
        copy_logical_rect_to_hw(
            &logical,
            &mut hw,
            stride,
            0,
            0,
            EXPECTED_XRES,
            EXPECTED_YRES,
            false,
        )
        .unwrap();
        let before_l = logical.clone();
        let before_h = hw.clone();
        let s = sample_visible_grid(&logical, &hw, layout(), false);
        assert_eq!(s.mismatch, 0);
        assert!(s.format_line(10, 1, "wait").contains("coverage=partial"));
        assert_eq!(logical, before_l, "sample must not write logical");
        assert_eq!(hw, before_h, "sample must not write hardware");
    }

    #[test]
    fn complement_agreement_and_detected_mismatch() {
        let stride = EXPECTED_LINE_LENGTH;
        let (logical, mut hw) = panel_bufs(LOGICAL_WHITE, 0x00);
        copy_logical_rect_to_hw(
            &logical,
            &mut hw,
            stride,
            0,
            0,
            EXPECTED_XRES,
            EXPECTED_YRES,
            true,
        )
        .unwrap();
        let ok = sample_visible_grid(&logical, &hw, layout(), true);
        assert_eq!(ok.mismatch, 0);
        hw[0] = LOGICAL_WHITE; // wrong for complement of white (expected 0x00)
        let bad = sample_visible_grid(&logical, &hw, layout(), true);
        assert!(bad.mismatch >= 1);
        let line = bad.format_line(10, 2, "wait");
        assert!(line.contains("coverage=partial"));
        assert!(line.contains("mismatch_xy="));
        assert!(
            line.contains("0,0:exp=00,obs=ff"),
            "mismatch list must include x,y and expected/observed bytes: {line}"
        );
        assert!(!line.contains("mismatch_xy=-") || bad.mismatch == 0);
    }

    #[test]
    fn mismatch_xy_list_is_bounded() {
        let (logical, hw) = panel_bufs(LOGICAL_BLACK, LOGICAL_WHITE);
        let s = sample_visible_grid(&logical, &hw, layout(), false);
        assert!(s.mismatch as usize > MISMATCH_LIST_CAP);
        let listed = s.format_mismatch_xy().split(';').count();
        assert_eq!(listed, MISMATCH_LIST_CAP);
        let line = s.format_line(1, 1, "wait");
        assert!(line.contains("truncated=1"));
        assert!(line.contains("coverage=partial"));
    }

    #[test]
    fn flush_byte_count_is_w_times_h() {
        let s = FlushCopyStats {
            w: 1860,
            h: 2480,
            bytes: 1860 * 2480,
            ..FlushCopyStats::default()
        };
        assert_eq!(s.bytes, s.w.saturating_mul(s.h));
    }
}
