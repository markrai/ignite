//! Panel gray polarity at the mmap output boundary.
//!
//! Logical / compositor convention (Home, text, ink, `background.raw`, overlays):
//! `0x00` = black, `0xFF` = white, intermediates are linear 8-bit gray.
//!
//! Physical evidence:
//! - Scribe 2022 / FW 5.18.6 / `grayscale=2` (`GRAYSCALE_8BIT_INVERTED`): those
//!   logical bytes display correctly when stored unchanged in mmap.
//! - Scribe 2024 / FW 5.19.6 / `grayscale=1` (`GRAYSCALE_8BIT`): the same bytes
//!   display with reversed polarity. Hardware bytes are the bitwise complement.
//!
//! Addressing, stride, and Gate B geometry are unchanged. This module does not
//! issue `FBIOPUT_VSCREENINFO` or alter night mode. Encode always from the
//! logical source; never invert hardware bytes in place (that would double-invert
//! on the next dirty copy).

use std::io::Read;
use std::path::Path;

use crate::gate_b::{ACCEPTED_GRAYSCALE_LINUX_GRAY, EXPECTED_GRAYSCALE};

/// Canonical logical black / white (also the grayscale=2 hardware bytes).
pub const LOGICAL_BLACK: u8 = 0x00;
pub const LOGICAL_WHITE: u8 = 0xFF;

/// Maximum accepted size of `/mnt/us/scribe-app/fb-polarity` (bytes). Larger files are
/// ignored so a runaway write cannot force an unbounded read at framebuffer open.
pub const POLARITY_OVERRIDE_MAX_BYTES: usize = 128;

/// `true` when mmap must store `!logical` (`GRAYSCALE_8BIT`).
///
/// Unknown grayscale values are not complemented (Gate B refuses them before
/// writable mmap in the app). Identity keeps grayscale=2 byte-for-byte.
pub fn hardware_complement(grayscale: u32) -> bool {
    grayscale == ACCEPTED_GRAYSCALE_LINUX_GRAY
}

/// Optional developer/tester override of the polarity decision, for firmware whose reported
/// `grayscale` does not match its real panel polarity (observed: a 1st-gen Scribe on 5.18.3
/// reports `grayscale=1` yet displays complemented ink as white-on-dark).
///
/// `text` is the content of the override file: `identity` (never complement) or `complement`
/// (always complement), surrounding whitespace ignored. Anything else, or no file, falls back
/// to [`hardware_complement`], so devices without the file behave exactly as before.
pub fn resolve_complement(grayscale: u32, override_text: Option<&str>) -> bool {
    match override_text.map(str::trim) {
        Some("identity") => false,
        Some("complement") => true,
        _ => hardware_complement(grayscale),
    }
}

/// Decode override-file bytes. Returns [`None`] when the payload is oversized or not UTF-8 so
/// callers fall back to [`hardware_complement`]. Does not interpret the keyword itself.
pub fn override_text_from_bytes(bytes: &[u8]) -> Option<&str> {
    if bytes.len() > POLARITY_OVERRIDE_MAX_BYTES {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

/// Bounded read of the optional polarity override file.
///
/// Returns [`None`] (automatic polarity) when the path is missing, unreadable, larger than
/// [`POLARITY_OVERRIDE_MAX_BYTES`], or not valid UTF-8. Reads until EOF or `MAX + 1` bytes so a
/// short `read()` cannot miss an oversized file.
pub fn load_override_text(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    load_override_text_from_reader(file)
}

fn load_override_text_from_reader(reader: impl Read) -> Option<String> {
    let mut buf = Vec::with_capacity(POLARITY_OVERRIDE_MAX_BYTES + 1);
    reader
        .take((POLARITY_OVERRIDE_MAX_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .ok()?;
    override_text_from_bytes(&buf).map(str::to_owned)
}

/// Map one logical gray byte to the hardware mmap byte for this `grayscale` field.
pub fn encode_panel_gray(logical: u8, grayscale: u32) -> u8 {
    encode_panel_gray_complement(logical, hardware_complement(grayscale))
}

pub fn encode_panel_gray_complement(logical: u8, complement: bool) -> u8 {
    if complement {
        !logical
    } else {
        logical
    }
}

/// Decode a hardware mmap byte back to the logical convention.
pub fn decode_panel_gray(hardware: u8, grayscale: u32) -> u8 {
    encode_panel_gray(hardware, grayscale)
}

/// One-line diagnose field for the live `grayscale` value.
pub fn diagnose_line(grayscale: u32) -> String {
    if grayscale == EXPECTED_GRAYSCALE {
        "polarity=identity (GRAYSCALE_8BIT_INVERTED; logical byte = hardware byte)".to_string()
    } else if grayscale == ACCEPTED_GRAYSCALE_LINUX_GRAY {
        "polarity=complement (GRAYSCALE_8BIT; hardware = !logical)".to_string()
    } else {
        format!("polarity=unsupported (grayscale={grayscale})")
    }
}

/// Copy a rectangle from a strided logical buffer into a strided hardware buffer.
///
/// `logical` and `hardware` share `stride` (`line_length`). Bytes outside the
/// rectangle — including row padding — are not touched. Always reads `logical`;
/// never inverts `hardware` in place.
#[allow(clippy::too_many_arguments)]
pub fn copy_logical_rect_to_hw(
    logical: &[u8],
    hardware: &mut [u8],
    stride: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    complement: bool,
) -> Option<()> {
    let ll = stride as usize;
    let w = w as usize;
    let h = h as usize;
    let x = x as usize;
    let y = y as usize;
    if w == 0 || h == 0 || x.checked_add(w)? > ll {
        return None;
    }
    if hardware.len() != logical.len() {
        return None;
    }
    for row in 0..h {
        let start = y.checked_add(row)?.checked_mul(ll)?.checked_add(x)?;
        let end = start.checked_add(w)?;
        if end > logical.len() || end > hardware.len() {
            return None;
        }
        let src = &logical[start..end];
        let dst = &mut hardware[start..end];
        if complement {
            for (d, s) in dst.iter_mut().zip(src.iter()) {
                *d = !*s;
            }
        } else {
            dst.copy_from_slice(src);
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate_b::{EXPECTED_LINE_LENGTH, EXPECTED_XRES};

    fn buf(len: usize, fill: u8) -> Vec<u8> {
        vec![fill; len]
    }

    #[test]
    fn logical_black_white_and_mid_gray_both_modes() {
        assert_eq!(encode_panel_gray(LOGICAL_BLACK, 2), 0x00);
        assert_eq!(encode_panel_gray(LOGICAL_WHITE, 2), 0xFF);
        assert_eq!(encode_panel_gray(0x7F, 2), 0x7F);
        assert_eq!(encode_panel_gray(0x80, 2), 0x80);

        assert_eq!(encode_panel_gray(LOGICAL_BLACK, 1), 0xFF);
        assert_eq!(encode_panel_gray(LOGICAL_WHITE, 1), 0x00);
        assert_eq!(encode_panel_gray(0x7F, 1), 0x80);
        assert_eq!(encode_panel_gray(0x80, 1), 0x7F);
    }

    #[test]
    fn override_wins_and_default_is_unchanged() {
        // No file / unknown text: exactly the old behavior.
        for g in [1u32, 2] {
            assert_eq!(resolve_complement(g, None), hardware_complement(g));
            assert_eq!(resolve_complement(g, Some("")), hardware_complement(g));
            assert_eq!(resolve_complement(g, Some("bogus")), hardware_complement(g));
        }
        // Explicit override in both directions, whitespace tolerated.
        assert!(!resolve_complement(1, Some("identity\n")));
        assert!(resolve_complement(2, Some("  complement ")));
        assert!(!resolve_complement(2, Some("identity")));
        assert!(resolve_complement(1, Some("complement")));
    }

    #[test]
    fn override_text_from_bytes_accepts_valid_utf8_within_limit() {
        assert_eq!(override_text_from_bytes(b"identity"), Some("identity"));
        assert_eq!(
            override_text_from_bytes(b"complement\n"),
            Some("complement\n")
        );
        let exact = vec![b'a'; POLARITY_OVERRIDE_MAX_BYTES];
        assert_eq!(
            override_text_from_bytes(&exact).map(|s| s.len()),
            Some(POLARITY_OVERRIDE_MAX_BYTES)
        );
    }

    #[test]
    fn override_text_from_bytes_rejects_oversized_and_invalid_utf8() {
        let oversized = vec![b'x'; POLARITY_OVERRIDE_MAX_BYTES + 1];
        assert!(override_text_from_bytes(&oversized).is_none());
        assert!(override_text_from_bytes(&[0xff, 0xfe, 0xfd]).is_none());
    }

    #[test]
    fn load_override_text_covers_missing_valid_invalid_and_oversized() {
        let root = std::env::temp_dir().join(format!(
            "ignite-fb-polarity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("fb-polarity");

        assert!(load_override_text(&path).is_none(), "missing file");

        std::fs::write(&path, b"  identity \n").unwrap();
        assert_eq!(load_override_text(&path).as_deref(), Some("  identity \n"));
        assert!(!resolve_complement(1, load_override_text(&path).as_deref()));

        std::fs::write(&path, b"complement").unwrap();
        assert!(resolve_complement(2, load_override_text(&path).as_deref()));

        std::fs::write(&path, b"not-a-keyword").unwrap();
        assert_eq!(
            resolve_complement(1, load_override_text(&path).as_deref()),
            hardware_complement(1)
        );

        std::fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
        assert!(load_override_text(&path).is_none(), "invalid UTF-8");

        let oversized = vec![b'z'; POLARITY_OVERRIDE_MAX_BYTES + 1];
        std::fs::write(&path, &oversized).unwrap();
        assert!(load_override_text(&path).is_none(), "oversized");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Deliberately returns only a few bytes per `read`, like a short-read filesystem.
    struct ShortRead<'a> {
        data: &'a [u8],
        chunk: usize,
    }

    impl Read for ShortRead<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.data.is_empty() || buf.is_empty() {
                return Ok(0);
            }
            let n = self.chunk.min(self.data.len()).min(buf.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    #[test]
    fn short_reads_still_reject_oversized_override_files() {
        let oversized = vec![b'z'; POLARITY_OVERRIDE_MAX_BYTES + 1];
        let reader = ShortRead {
            data: &oversized,
            chunk: 3,
        };
        assert!(
            load_override_text_from_reader(reader).is_none(),
            "short reads must not let an oversized file pass the bound"
        );

        let valid = b"  complement \n";
        let reader = ShortRead {
            data: valid,
            chunk: 2,
        };
        assert_eq!(
            load_override_text_from_reader(reader).as_deref(),
            Some("  complement \n")
        );
        assert!(resolve_complement(
            2,
            load_override_text_from_reader(ShortRead {
                data: valid,
                chunk: 1,
            })
            .as_deref()
        ));
    }

    #[test]
    fn grayscale_2_is_byte_identical() {
        assert!(!hardware_complement(2));
        for b in 0u8..=255 {
            assert_eq!(encode_panel_gray(b, 2), b);
        }
    }

    #[test]
    fn grayscale_1_is_complement_of_logical() {
        assert!(hardware_complement(1));
        for b in 0u8..=255 {
            assert_eq!(encode_panel_gray(b, 1), !b);
            assert_eq!(decode_panel_gray(encode_panel_gray(b, 1), 1), b);
        }
    }

    #[test]
    fn unknown_grayscale_is_not_complemented() {
        assert!(!hardware_complement(0));
        assert!(!hardware_complement(3));
        assert_eq!(encode_panel_gray(0x00, 0), 0x00);
    }

    #[test]
    fn identity_copy_is_byte_for_byte() {
        let stride = 8u32;
        let mut logical = buf(32, 0xFF);
        logical[8 + 2] = 0x00;
        logical[8 + 3] = 0x80;
        let mut hw = buf(32, 0x11);
        copy_logical_rect_to_hw(&logical, &mut hw, stride, 1, 1, 4, 1, false).unwrap();
        assert_eq!(&hw[8 + 1..8 + 5], &logical[8 + 1..8 + 5]);
        assert_eq!(hw[8], 0x11);
        assert_eq!(hw[8 + 5], 0x11);
        assert_eq!(hw[0], 0x11);
    }

    #[test]
    fn complement_copy_inverts_only_the_rect() {
        let stride = 8u32;
        let logical = {
            let mut v = buf(32, 0xFF);
            v[8 + 2] = 0x00;
            v[8 + 3] = 0x80;
            v
        };
        let mut hw = buf(32, 0x11);
        copy_logical_rect_to_hw(&logical, &mut hw, stride, 1, 1, 4, 1, true).unwrap();
        assert_eq!(hw[8 + 1], 0x00); // !0xFF
        assert_eq!(hw[8 + 2], 0xFF); // !0x00 ink
        assert_eq!(hw[8 + 3], 0x7F); // !0x80
        assert_eq!(hw[8 + 4], 0x00);
        assert_eq!(hw[8], 0x11);
        assert_eq!(hw[8 + 5], 0x11);
    }

    #[test]
    fn padded_stride_is_untouched() {
        let stride = EXPECTED_LINE_LENGTH;
        let xres = EXPECTED_XRES;
        let rows = 2u32;
        let len = stride as usize * rows as usize;
        let logical = buf(len, LOGICAL_WHITE);
        let mut hw = buf(len, 0xAA);
        copy_logical_rect_to_hw(&logical, &mut hw, stride, 0, 0, xres, rows, true).unwrap();
        for y in 0..rows as usize {
            let row = y * stride as usize;
            assert!(hw[row..row + xres as usize]
                .iter()
                .all(|&b| b == encode_panel_gray(LOGICAL_WHITE, 1)));
            assert!(hw[row + xres as usize..row + stride as usize]
                .iter()
                .all(|&b| b == 0xAA));
        }
    }

    #[test]
    fn full_and_incremental_copies_match() {
        let stride = 10u32;
        let mut logical = buf(40, LOGICAL_WHITE);
        logical[0] = LOGICAL_BLACK;
        logical[11] = 0x40;
        logical[22] = 0xC0;

        let mut full = buf(40, 0x00);
        copy_logical_rect_to_hw(&logical, &mut full, stride, 0, 0, 8, 4, true).unwrap();

        let mut incr = buf(40, 0x00);
        copy_logical_rect_to_hw(&logical, &mut incr, stride, 0, 0, 8, 2, true).unwrap();
        copy_logical_rect_to_hw(&logical, &mut incr, stride, 0, 2, 8, 2, true).unwrap();
        assert_eq!(full, incr);
    }

    #[test]
    fn subsequent_flush_does_not_double_invert() {
        let stride = 4u32;
        let logical = vec![LOGICAL_BLACK, LOGICAL_WHITE, 0x10, 0xEF];
        let mut hw = buf(4, 0x33);
        copy_logical_rect_to_hw(&logical, &mut hw, stride, 0, 0, 4, 1, true).unwrap();
        let once = hw.clone();
        copy_logical_rect_to_hw(&logical, &mut hw, stride, 0, 0, 4, 1, true).unwrap();
        assert_eq!(hw, once);
        assert_eq!(hw, vec![0xFF, 0x00, 0xEF, 0x10]);
        // In-place invert of hardware would have restored logical on the second pass.
        assert_ne!(hw, logical);
    }

    #[test]
    fn home_text_and_ink_logical_buffers_encode_as_expected() {
        // Mini composed frame: paper, HUD glyph, notebook ink, text gray.
        let stride = 8u32;
        let mut logical = buf(24, LOGICAL_WHITE);
        logical[1] = LOGICAL_BLACK; // Home/chrome glyph
        logical[9] = LOGICAL_BLACK; // notebook ink
        logical[17] = 0x40; // text darken toward ink

        let mut g2 = buf(24, 0x22);
        copy_logical_rect_to_hw(&logical, &mut g2, stride, 0, 0, 6, 3, false).unwrap();
        assert_eq!(&g2[0..6], &logical[0..6]);
        assert_eq!(&g2[8..14], &logical[8..14]);
        assert_eq!(g2[6], 0x22, "dirty width must not smear into padding");

        let mut g1 = buf(24, 0x22);
        copy_logical_rect_to_hw(&logical, &mut g1, stride, 0, 0, 6, 3, true).unwrap();
        assert_eq!(g1[1], 0xFF);
        assert_eq!(g1[9], 0xFF);
        assert_eq!(g1[17], !0x40);
        assert_eq!(g1[0], 0x00);
        assert_eq!(g1[6], 0x22);
    }

    #[test]
    fn diagnose_names_both_supported_modes() {
        assert!(diagnose_line(2).starts_with("polarity=identity"));
        assert!(diagnose_line(1).starts_with("polarity=complement"));
        assert!(diagnose_line(0).contains("unsupported"));
    }

    /// Host stand-in for `MappedFramebuffer`: logical compositor + encoded mmap twin.
    struct Twin {
        logical: Vec<u8>,
        hardware: Vec<u8>,
        stride: u32,
        xres: u32,
        yres: u32,
        complement: bool,
    }

    impl Twin {
        fn new(xres: u32, yres: u32, stride: u32, complement: bool, hw_fill: u8) -> Self {
            let len = stride as usize * yres as usize;
            Self {
                logical: buf(len, LOGICAL_WHITE),
                hardware: buf(len, hw_fill),
                stride,
                xres,
                yres,
                complement,
            }
        }

        fn flush(&mut self, x: u32, y: u32, w: u32, h: u32) {
            copy_logical_rect_to_hw(
                &self.logical,
                &mut self.hardware,
                self.stride,
                x,
                y,
                w,
                h,
                self.complement,
            )
            .unwrap();
        }

        fn send_full(&mut self) {
            self.flush(0, 0, self.xres, self.yres);
        }

        fn extract(&self, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
            crate::fbdev_abi::rect8::extract(&self.logical, self.stride, x, y, w, h).unwrap()
        }

        fn restore(&mut self, x: u32, y: u32, w: u32, h: u32, pixels: &[u8]) {
            crate::fbdev_abi::rect8::restore(&mut self.logical, self.stride, x, y, w, h, pixels)
                .unwrap();
            self.flush(x, y, w, h);
        }
    }

    #[test]
    fn initial_full_paint_lands_before_partial_updates() {
        for complement in [false, true] {
            let amazon = 0x77u8;
            let mut p = Twin::new(8, 4, 10, complement, amazon);
            assert!(
                p.hardware.iter().all(|&b| b == amazon),
                "mmap stays untouched until the first full flush"
            );

            crate::fbdev_abi::rect8::fill(&mut p.logical, p.stride, 0, 0, 8, 4, LOGICAL_WHITE)
                .unwrap();
            p.logical[1] = LOGICAL_BLACK; // splash / Home glyph
            p.send_full();
            let paper = encode_panel_gray_complement(LOGICAL_WHITE, complement);
            let ink = encode_panel_gray_complement(LOGICAL_BLACK, complement);
            assert_eq!(p.hardware[1], ink);
            assert_eq!(p.hardware[0], paper);
            assert_eq!(
                p.hardware[8], amazon,
                "stride pad not part of the first paint"
            );

            // Wet-ink dirty rect: only (2,1) 3x1.
            let before = p.hardware.clone();
            p.logical[p.stride as usize + 2] = LOGICAL_BLACK;
            p.flush(2, 1, 3, 1);
            assert_eq!(p.hardware[p.stride as usize + 2], ink);
            let lo = p.stride as usize + 2;
            let hi = p.stride as usize + 5;
            for (i, (got, old)) in p.hardware.iter().zip(before.iter()).enumerate() {
                if !(i >= lo && i < hi) {
                    assert_eq!(got, old, "partial must not rewrite the frame");
                }
            }
        }
    }

    #[test]
    fn extract_restore_keeps_logical_overlays_then_encodes() {
        for complement in [false, true] {
            let mut p = Twin::new(8, 3, 10, complement, 0x11);
            crate::fbdev_abi::rect8::fill(&mut p.logical, p.stride, 0, 0, 8, 3, LOGICAL_WHITE)
                .unwrap();
            crate::fbdev_abi::rect8::fill(&mut p.logical, p.stride, 2, 1, 3, 1, LOGICAL_BLACK)
                .unwrap();
            p.send_full();

            let snap = p.extract(2, 1, 3, 1);
            assert_eq!(snap, vec![LOGICAL_BLACK, LOGICAL_BLACK, LOGICAL_BLACK]);

            crate::fbdev_abi::rect8::fill(&mut p.logical, p.stride, 2, 1, 3, 1, LOGICAL_WHITE)
                .unwrap();
            p.logical[p.stride as usize + 2] = 0x40; // subsequent overlay
            p.flush(2, 1, 3, 1);
            assert_eq!(p.extract(2, 1, 3, 1)[0], 0x40);

            p.restore(2, 1, 3, 1, &snap);
            assert_eq!(p.extract(2, 1, 3, 1), snap);
            let ink = encode_panel_gray_complement(LOGICAL_BLACK, complement);
            assert_eq!(
                &p.hardware[p.stride as usize + 2..p.stride as usize + 5],
                &[ink, ink, ink]
            );
            assert_eq!(p.hardware[9], 0x11);
        }
    }

    #[test]
    fn wet_dirty_copy_is_bounded_not_a_full_frame() {
        let stride = EXPECTED_LINE_LENGTH;
        let xres = EXPECTED_XRES;
        let yres = 8u32;
        let len = stride as usize * yres as usize;
        let logical = buf(len, LOGICAL_WHITE);
        let mut hw = buf(len, 0x5A);
        let dirty_w = 24u32;
        let dirty_y = 1u32;
        let dirty_h = 2u32;
        copy_logical_rect_to_hw(
            &logical, &mut hw, stride, 40, dirty_y, dirty_w, dirty_h, true,
        )
        .unwrap();
        let copied = (dirty_w * dirty_h) as usize;
        assert!(
            copied < len / 4,
            "pen dirty copy must not be a full mmap blit"
        );
        let mut changed = 0usize;
        for (i, b) in hw.iter().enumerate() {
            if *b != 0x5A {
                changed += 1;
            }
            let col = i % stride as usize;
            let row = i / stride as usize;
            let inside = row >= dirty_y as usize
                && row < dirty_y as usize + dirty_h as usize
                && col >= 40
                && col < 40 + dirty_w as usize;
            if !inside {
                assert_eq!(*b, 0x5A);
            }
        }
        assert_eq!(changed, copied);
        assert!(xres as usize > 40 + dirty_w as usize);
    }
}
