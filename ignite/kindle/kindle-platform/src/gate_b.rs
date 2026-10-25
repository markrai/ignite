//! Display Gate B framebuffer preconditions for Scribe `hwtcon_v2` 8-bit packed layout.
//!
//! Geometry / stride / memory constants are the physical snapshot from Scribe 1st Gen
//! FW 5.18.6 (`docs/KINDLE_PROBE.md`). The application renderer accepts that layout
//! only. `grayscale` is *not* used for mmap addressing. Output polarity is applied
//! at the mmap copy boundary (`fb_polarity`): grayscale=2 stores logical bytes
//! unchanged; grayscale=1 stores their complement.
//!
//! `fb_var_screeninfo.grayscale` in Linux `fb.h`: 0 = color, 1 = grayscale, >1 = FOURCC.
//! Lab126/MTK public Kindle headers also name `GRAYSCALE_8BIT = 1` and
//! `GRAYSCALE_8BIT_INVERTED = 2`. Value 2 is not a plausible FOURCC (those are packed
//! ASCII, e.g. `0x32315659`). Accept **1 or 2** only when every other Gate B field
//! matches the proven packed 8-bit `hwtcon_v2` layout. Reject color (`0`) and other
//! FOURCC / depth / stride combinations.

use crate::fbdev_abi::{FbFixScreeninfo, FbVarScreeninfo};

pub const EXPECTED_ID: &str = "hwtcon_v2";
pub const EXPECTED_XRES: u32 = 1860;
pub const EXPECTED_YRES: u32 = 2480;
pub const EXPECTED_XRES_VIRTUAL: u32 = 1872;
pub const EXPECTED_YRES_VIRTUAL: u32 = 4960;
pub const EXPECTED_XOFFSET: u32 = 0;
pub const EXPECTED_YOFFSET: u32 = 0;
pub const EXPECTED_BPP: u32 = 8;
pub const EXPECTED_LINE_LENGTH: u32 = 1872;
pub const EXPECTED_SMEM_LEN: u32 = 9_285_120;
/// Proven Scribe 1st Gen / 5.18.6 snapshot (`GRAYSCALE_8BIT_INVERTED` in Lab126 naming).
pub const EXPECTED_GRAYSCALE: u32 = 2;
/// Linux `fb.h` grayscale flag / Lab126 `GRAYSCALE_8BIT`. Same 8-bit packed layout.
pub const ACCEPTED_GRAYSCALE_LINUX_GRAY: u32 = 1;
pub const EXPECTED_NONSTD: u32 = 1;
/// Rotation from the proven 5.18.6 snapshot. A 5.19.6 diagnostic reports 1
/// with the same portrait geometry; screen orientation still needs hardware confirmation.
pub const EXPECTED_ROTATE: u32 = 3;
pub const ACCEPTED_ROTATE_CW: u32 = 1;

/// Probe-reported hardware black/white observation (`ink-probe` Gate B).
/// Compositor polarity lives in `fb_polarity` (`LOGICAL_BLACK` / `LOGICAL_WHITE`).
pub const BLACK_BYTE: Option<u8> = None;
pub const WHITE_BYTE: Option<u8> = None;

/// `grayscale` values that share the proven 8-bit packed gray mmap layout.
pub fn grayscale_is_accepted_8bpp_gray(got: u32) -> bool {
    got == ACCEPTED_GRAYSCALE_LINUX_GRAY || got == EXPECTED_GRAYSCALE
}

/// Compare live fbdev info against the Gate-B layout. Returns every mismatch.
///
/// Does not mmap, refresh, or write pixels. Callers must not open a writable
/// mapping until this returns `Ok`.
pub fn check_preconditions(
    fix: &FbFixScreeninfo,
    var: &FbVarScreeninfo,
) -> Result<(), Vec<String>> {
    let mut mismatches = Vec::new();
    let id = fix.id_str();
    if id != EXPECTED_ID {
        mismatches.push(format!("id: got {id:?}, expected {EXPECTED_ID:?}"));
    }
    check_u32("xres", var.xres, EXPECTED_XRES, &mut mismatches);
    check_u32("yres", var.yres, EXPECTED_YRES, &mut mismatches);
    check_u32(
        "xres_virtual",
        var.xres_virtual,
        EXPECTED_XRES_VIRTUAL,
        &mut mismatches,
    );
    check_u32(
        "yres_virtual",
        var.yres_virtual,
        EXPECTED_YRES_VIRTUAL,
        &mut mismatches,
    );
    check_u32("xoffset", var.xoffset, EXPECTED_XOFFSET, &mut mismatches);
    check_u32("yoffset", var.yoffset, EXPECTED_YOFFSET, &mut mismatches);
    check_u32(
        "bits_per_pixel",
        var.bits_per_pixel,
        EXPECTED_BPP,
        &mut mismatches,
    );
    check_u32(
        "line_length",
        fix.line_length,
        EXPECTED_LINE_LENGTH,
        &mut mismatches,
    );
    check_u32("smem_len", fix.smem_len, EXPECTED_SMEM_LEN, &mut mismatches);
    if !grayscale_is_accepted_8bpp_gray(var.grayscale) {
        mismatches.push(format!(
            "grayscale: got {}, expected {} (Linux grayscale / Lab126 GRAYSCALE_8BIT) or {} (Lab126 GRAYSCALE_8BIT_INVERTED; proven Scribe 5.18.6)",
            var.grayscale, ACCEPTED_GRAYSCALE_LINUX_GRAY, EXPECTED_GRAYSCALE
        ));
    }
    check_u32("nonstd", var.nonstd, EXPECTED_NONSTD, &mut mismatches);
    if var.rotate != EXPECTED_ROTATE && var.rotate != ACCEPTED_ROTATE_CW {
        mismatches.push(format!(
            "rotate: got {}, expected {} (or {} observed in the 5.19.6 diagnostic)",
            var.rotate, EXPECTED_ROTATE, ACCEPTED_ROTATE_CW
        ));
    }

    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(mismatches)
    }
}

fn check_u32(name: &str, got: u32, expected: u32, out: &mut Vec<String>) {
    if got != expected {
        out.push(format!("{name}: got {got}, expected {expected}"));
    }
}

fn bitfield_line(name: &str, bf: crate::fbdev_abi::FbBitfield) -> String {
    format!(
        "{name} offset={} length={} msb_right={}",
        bf.offset, bf.length, bf.msb_right
    )
}

/// Read-only dump of `FBIOGET_FSCREENINFO` / `FBIOGET_VSCREENINFO` plus Gate B result.
pub fn format_fb_diagnose(fix: &FbFixScreeninfo, var: &FbVarScreeninfo) -> String {
    let mut out = String::new();
    out.push_str("# fixed (FBIOGET_FSCREENINFO)\n");
    out.push_str(&format!("id={}\n", fix.id_str()));
    out.push_str(&format!("smem_start={:#x}\n", fix.smem_start as u64));
    out.push_str(&format!("smem_len={}\n", fix.smem_len));
    out.push_str(&format!("type={}\n", fix.type_));
    out.push_str(&format!("type_aux={}\n", fix.type_aux));
    out.push_str(&format!("visual={}\n", fix.visual));
    out.push_str(&format!(
        "xpanstep={} ypanstep={} ywrapstep={}\n",
        fix.xpanstep, fix.ypanstep, fix.ywrapstep
    ));
    out.push_str(&format!("line_length={}\n", fix.line_length));
    out.push_str(&format!("mmio_start={:#x}\n", fix.mmio_start as u64));
    out.push_str(&format!("mmio_len={}\n", fix.mmio_len));
    out.push_str(&format!("accel={}\n", fix.accel));
    out.push_str(&format!("capabilities={}\n", fix.capabilities));
    out.push_str("# variable (FBIOGET_VSCREENINFO)\n");
    out.push_str(&format!("xres={} yres={}\n", var.xres, var.yres));
    out.push_str(&format!(
        "xres_virtual={} yres_virtual={}\n",
        var.xres_virtual, var.yres_virtual
    ));
    out.push_str(&format!(
        "xoffset={} yoffset={}\n",
        var.xoffset, var.yoffset
    ));
    out.push_str(&format!("bits_per_pixel={}\n", var.bits_per_pixel));
    out.push_str(&format!("grayscale={}\n", var.grayscale));
    out.push_str(&format!(
        "{}\n",
        crate::fb_polarity::diagnose_line(var.grayscale)
    ));
    out.push_str(&format!("nonstd={}\n", var.nonstd));
    out.push_str(&format!("rotate={}\n", var.rotate));
    out.push_str(&format!("activate={}\n", var.activate));
    out.push_str(&format!("colorspace={}\n", var.colorspace));
    out.push_str(&format!("{}\n", bitfield_line("red", var.red)));
    out.push_str(&format!("{}\n", bitfield_line("green", var.green)));
    out.push_str(&format!("{}\n", bitfield_line("blue", var.blue)));
    out.push_str(&format!("{}\n", bitfield_line("transp", var.transp)));
    match check_preconditions(fix, var) {
        Ok(()) => out.push_str("gate_b=ok\n"),
        Err(mismatches) => {
            out.push_str("gate_b=REFUSE\n");
            for m in mismatches {
                out.push_str(&format!("mismatch: {m}\n"));
            }
        }
    }
    out.push_str("note=FBIOGET only; no mmap write, no refresh, no notebook open\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fbdev_abi::{FbFixScreeninfo, FbVarScreeninfo};

    fn matching_pair() -> (FbFixScreeninfo, FbVarScreeninfo) {
        let mut fix = FbFixScreeninfo {
            line_length: EXPECTED_LINE_LENGTH,
            smem_len: EXPECTED_SMEM_LEN,
            ..Default::default()
        };
        fix.id[..EXPECTED_ID.len()].copy_from_slice(EXPECTED_ID.as_bytes());
        let var = FbVarScreeninfo {
            xres: EXPECTED_XRES,
            yres: EXPECTED_YRES,
            xres_virtual: EXPECTED_XRES_VIRTUAL,
            yres_virtual: EXPECTED_YRES_VIRTUAL,
            xoffset: EXPECTED_XOFFSET,
            yoffset: EXPECTED_YOFFSET,
            bits_per_pixel: EXPECTED_BPP,
            grayscale: EXPECTED_GRAYSCALE,
            nonstd: EXPECTED_NONSTD,
            rotate: EXPECTED_ROTATE,
            ..Default::default()
        };
        (fix, var)
    }

    #[test]
    fn proven_grayscale_2_layout_passes() {
        let (fix, var) = matching_pair();
        assert_eq!(var.grayscale, 2);
        assert!(check_preconditions(&fix, &var).is_ok());
    }

    #[test]
    fn tester_grayscale_1_same_layout_passes() {
        let (fix, mut var) = matching_pair();
        var.grayscale = ACCEPTED_GRAYSCALE_LINUX_GRAY;
        assert!(check_preconditions(&fix, &var).is_ok());
    }

    #[test]
    fn color_grayscale_0_is_rejected() {
        let (fix, mut var) = matching_pair();
        var.grayscale = 0;
        let err = check_preconditions(&fix, &var).unwrap_err();
        assert!(err.iter().any(|m| m.starts_with("grayscale:")));
    }

    #[test]
    fn fourcc_like_grayscale_is_rejected() {
        let (fix, mut var) = matching_pair();
        var.grayscale = u32::from_le_bytes(*b"YVYU");
        let err = check_preconditions(&fix, &var).unwrap_err();
        assert!(err.iter().any(|m| m.starts_with("grayscale:")));
        assert!(check_preconditions(&fix, &matching_pair().1).is_ok());
    }

    #[test]
    fn unsupported_bit_depth_is_rejected() {
        let (fix, mut var) = matching_pair();
        var.bits_per_pixel = 16;
        var.grayscale = ACCEPTED_GRAYSCALE_LINUX_GRAY;
        let err = check_preconditions(&fix, &var).unwrap_err();
        assert!(err.iter().any(|m| m.starts_with("bits_per_pixel:")));
    }

    #[test]
    fn unsupported_stride_is_rejected() {
        let (mut fix, var) = matching_pair();
        fix.line_length = EXPECTED_XRES;
        let err = check_preconditions(&fix, &var).unwrap_err();
        assert!(err.iter().any(|m| m.starts_with("line_length:")));
    }

    #[test]
    fn grayscale_1_does_not_bypass_wrong_geometry() {
        let (fix, mut var) = matching_pair();
        var.grayscale = ACCEPTED_GRAYSCALE_LINUX_GRAY;
        var.xres = 1072;
        let err = check_preconditions(&fix, &var).unwrap_err();
        assert!(err.iter().any(|m| m.starts_with("xres:")));
        assert!(!err.iter().any(|m| m.starts_with("grayscale:")));
    }

    #[test]
    fn reports_every_mismatch() {
        let (mut fix, mut var) = matching_pair();
        fix.line_length = 1;
        var.xres = 2;
        var.yoffset = 9;
        let err = check_preconditions(&fix, &var).unwrap_err();
        assert!(err.iter().any(|m| m.starts_with("line_length:")));
        assert!(err.iter().any(|m| m.starts_with("xres:")));
        assert!(err.iter().any(|m| m.starts_with("yoffset:")));
    }

    #[test]
    fn diagnose_reports_ok_and_refuse() {
        let (fix, var) = matching_pair();
        let ok = format_fb_diagnose(&fix, &var);
        assert!(ok.contains("grayscale=2"));
        assert!(ok.contains("polarity=identity"));
        assert!(ok.contains("xres=1860 yres=2480"));
        assert!(ok.contains("line_length=1872"));
        assert!(ok.contains("visual="));
        assert!(ok.contains("capabilities="));
        assert!(ok.contains("gate_b=ok"));
        assert!(ok.contains("no mmap write"));

        let (fix, mut bad) = matching_pair();
        bad.bits_per_pixel = 4;
        let refuse = format_fb_diagnose(&fix, &bad);
        assert!(refuse.contains("gate_b=REFUSE"));
        assert!(refuse.contains("mismatch: bits_per_pixel:"));
    }

    #[test]
    fn polarity_probe_constants_remain_unset() {
        assert!(BLACK_BYTE.is_none());
        assert!(WHITE_BYTE.is_none());
    }

    #[test]
    fn diagnose_grayscale_1_reports_complement_polarity() {
        let (fix, mut var) = matching_pair();
        var.grayscale = ACCEPTED_GRAYSCALE_LINUX_GRAY;
        let text = format_fb_diagnose(&fix, &var);
        assert!(text.contains("grayscale=1"));
        assert!(text.contains("polarity=complement"));
        assert!(text.contains("gate_b=ok"));
    }

    #[test]
    fn check_preconditions_is_info_only_no_buffer_required() {
        let (fix, var) = matching_pair();
        let _ = check_preconditions(&fix, &var);
    }
}
