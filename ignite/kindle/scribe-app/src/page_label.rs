//! Non-durable Kindle-style page chrome (`1` / `N of M`).
//!
//! Framebuffer-only: never written into notebook journals or `background.raw`.
//!
//! F11: the runtime paints positional page state through `chrome.rs`; the label formatter and
//! blitter here remain as the sealed F8/F9 contract under test. `scrub_chrome_band` is live.
#![cfg_attr(not(test), allow(dead_code))]

use kindle_platform::FbGeometry;

/// Source glyph width (pixels before scale).
pub const GLYPH_W: usize = 5;
/// Source glyph height (pixels before scale).
pub const GLYPH_H: usize = 7;
/// Integer upscale applied to each source pixel.
pub const GLYPH_SCALE: usize = 3;
/// Source columns of empty space between adjacent glyphs (then × scale).
/// Gap=2 (6 panel px at scale 3) keeps digits separable if mild panel residue remains.
pub const GLYPH_GAP: usize = 2;
/// Panel pixels from the bottom edge to the bottom of the glyph row.
pub const BOTTOM_MARGIN_PX: u32 = 28;

/// Format HUD chrome for `page_id` using its positional index in the manifest.
///
/// Fail-closed: unknown `page_id` never silently becomes page 1.
pub fn format_label_for_page(
    manifest: &crate::notebook::PersistedNotebookManifestV1,
    page_id: &str,
) -> Result<String, String> {
    let Some(idx) = crate::notebook::page_index(manifest, page_id) else {
        return Err(format!(
            "REFUSE PAGE LABEL: manifest does not contain requested page id {page_id}"
        ));
    };
    Ok(format_page_label(idx + 1, manifest.page_ids.len()))
}

/// Format the on-panel page label.
///
/// - `page_count == 1` → `"1"`
/// - otherwise → `"{index_1based} of {page_count}"`
///
/// Panics in debug if `page_count < 1` or `index_1based` is out of range.
pub fn format_page_label(index_1based: usize, page_count: usize) -> String {
    debug_assert!(page_count >= 1, "page_count must be >= 1");
    debug_assert!(
        index_1based >= 1 && index_1based <= page_count,
        "index_1based out of range"
    );
    if page_count == 1 {
        "1".to_string()
    } else {
        format!("{index_1based} of {page_count}")
    }
}

/// Axis-aligned panel rectangle for a label (inclusive origin, exclusive of w/h extent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Pixel width of `label` at current glyph metrics (refuses unsupported chars).
pub fn label_pixel_width(label: &str) -> Result<u32, String> {
    if label.is_empty() {
        return Err("page label must not be empty".into());
    }
    let mut n = 0usize;
    for ch in label.chars() {
        glyph_bits(ch)?;
        n += 1;
    }
    let cell = GLYPH_W * GLYPH_SCALE;
    let gap = GLYPH_GAP * GLYPH_SCALE;
    let width = n * cell + n.saturating_sub(1) * gap;
    u32::try_from(width).map_err(|_| "page label width overflow".into())
}

/// Centered bottom placement for `label` inside `geo` visible panel.
pub fn label_rect(geo: &FbGeometry, label: &str) -> Result<LabelRect, String> {
    let w = label_pixel_width(label)?;
    let h = u32::try_from(GLYPH_H * GLYPH_SCALE).expect("glyph height fits u32");
    if w > geo.xres || h > geo.yres {
        return Err(format!(
            "page label {w}x{h} does not fit panel {}x{}",
            geo.xres, geo.yres
        ));
    }
    if BOTTOM_MARGIN_PX.saturating_add(h) > geo.yres {
        return Err(format!(
            "page label bottom margin {BOTTOM_MARGIN_PX} + height {h} exceeds yres {}",
            geo.yres
        ));
    }
    let x = (geo.xres - w) / 2;
    let y = geo.yres - BOTTOM_MARGIN_PX - h;
    geo.check_panel_rect(x, y, w, h)
        .map_err(|_| format!("page label rect out of bounds ({x},{y} {w}x{h})"))?;
    Ok(LabelRect { x, y, w, h })
}

/// Bottom band that may hold page chrome (glyph row + margin).
pub fn chrome_band_height_px() -> u32 {
    (GLYPH_H * GLYPH_SCALE) as u32 + BOTTOM_MARGIN_PX
}

/// Top panel Y of the chrome band.
pub fn chrome_band_top(geo: &FbGeometry) -> u32 {
    geo.yres.saturating_sub(chrome_band_height_px())
}

/// Remove page-chrome pixels from a background buffer (packed `xres*yres` or stride).
///
/// Startup FB capture and Amazon UI can bake digits into `background.raw`. Restoring that
/// background then blitting our HUD stacks labels. This fills the bottom chrome band with
/// sampled paper color. Safe for backgrounds: durable ink lives in stroke journals, not here.
///
/// Returns `true` if any byte changed.
pub fn scrub_chrome_band(bg: &mut [u8], geo: &FbGeometry) -> Result<bool, String> {
    let packed_len = (geo.xres as usize)
        .checked_mul(geo.yres as usize)
        .ok_or("geometry overflow")?;
    let stride_visible = (geo.line_length as usize)
        .checked_mul(geo.yres as usize)
        .ok_or("geometry overflow")?;
    let (layout, row_stride) = if bg.len() == packed_len {
        (BgLayout::Packed, geo.xres as usize)
    } else if bg.len() >= stride_visible {
        // Mapped fb0 may be double-height (yres_virtual); only touch the visible window.
        (BgLayout::Stride, geo.line_length as usize)
    } else {
        return Err(format!(
            "background len {} matches neither packed {packed_len} nor stride>={stride_visible}",
            bg.len()
        ));
    };
    let y0 = chrome_band_top(geo);
    let paper = sample_paper_byte(bg, geo, layout, row_stride, y0)?;
    let mut changed = false;
    for y in y0..geo.yres {
        for x in 0..geo.xres {
            let i = (y as usize) * row_stride + (x as usize);
            if bg[i] != paper {
                bg[i] = paper;
                changed = true;
            }
        }
    }
    Ok(changed)
}

fn sample_paper_byte(
    bg: &[u8],
    geo: &FbGeometry,
    layout: BgLayout,
    row_stride: usize,
    chrome_top: u32,
) -> Result<u8, String> {
    let sample_y = chrome_top.saturating_sub(8);
    let x0 = geo.xres / 4;
    let x1 = (geo.xres * 3) / 4;
    if x1 <= x0 {
        return Ok(0xFF);
    }
    let mut sum: u64 = 0;
    let mut n: u64 = 0;
    for x in x0..x1 {
        let i = match layout {
            BgLayout::Packed | BgLayout::Stride => (sample_y as usize) * row_stride + (x as usize),
        };
        let b = *bg.get(i).ok_or("paper sample OOB")?;
        sum += u64::from(b);
        n += 1;
    }
    if n == 0 {
        Ok(0xFF)
    } else {
        Ok((sum / n) as u8)
    }
}

/// Clear `rect` to paper color sampled from `background` above the chrome band, then blit.
///
/// Does **not** copy chrome pixels from `background` into the label rect (startup FB capture
/// often bakes Amazon/prior digits into `background.raw`).
pub fn blit_page_label(
    fb: &mut [u8],
    background: &[u8],
    geo: &FbGeometry,
    label: &str,
) -> Result<LabelRect, String> {
    let rect = label_rect(geo, label)?;
    let paper = paper_byte_from_background(background, geo)?;
    fill_rect(fb, geo, rect, paper)?;
    let ink = if paper >= 128 { 0x00 } else { 0xFF };
    raster_label(fb, geo, rect.x, rect.y, label, ink)?;
    Ok(rect)
}

fn paper_byte_from_background(background: &[u8], geo: &FbGeometry) -> Result<u8, String> {
    let packed_len = (geo.xres as usize)
        .checked_mul(geo.yres as usize)
        .ok_or("geometry overflow")?;
    let stride_len = (geo.line_length as usize)
        .checked_mul(geo.yres as usize)
        .ok_or("geometry overflow")?;
    let (layout, row_stride) = if background.len() == packed_len {
        (BgLayout::Packed, geo.xres as usize)
    } else if background.len() == stride_len {
        (BgLayout::Stride, geo.line_length as usize)
    } else {
        return Err(format!(
            "background len {} matches neither packed {packed_len} nor stride {stride_len}",
            background.len()
        ));
    };
    sample_paper_byte(background, geo, layout, row_stride, chrome_band_top(geo))
}

fn fill_rect(fb: &mut [u8], geo: &FbGeometry, rect: LabelRect, ink: u8) -> Result<(), String> {
    for row in 0..rect.h {
        for col in 0..rect.w {
            let i = stride_index(geo, rect.x + col, rect.y + row)?;
            fb[i] = ink;
        }
    }
    Ok(())
}

#[cfg(test)]
fn restore_rect_from_background(
    fb: &mut [u8],
    background: &[u8],
    geo: &FbGeometry,
    rect: LabelRect,
) -> Result<(), String> {
    let packed_len = (geo.xres as usize)
        .checked_mul(geo.yres as usize)
        .ok_or("geometry overflow")?;
    let stride_len = (geo.line_length as usize)
        .checked_mul(geo.yres as usize)
        .ok_or("geometry overflow")?;
    let bg_mode = if background.len() == packed_len {
        BgLayout::Packed
    } else if background.len() == stride_len {
        BgLayout::Stride
    } else {
        return Err(format!(
            "background len {} matches neither packed {packed_len} nor stride {stride_len}",
            background.len()
        ));
    };
    for row in 0..rect.h {
        for col in 0..rect.w {
            let px = rect.x + col;
            let py = rect.y + row;
            let src = match bg_mode {
                BgLayout::Packed => {
                    let i = (py as usize)
                        .checked_mul(geo.xres as usize)
                        .and_then(|b| b.checked_add(px as usize))
                        .ok_or("bg packed index overflow")?;
                    background
                        .get(i)
                        .copied()
                        .ok_or_else(|| format!("bg packed OOB at {px},{py}"))?
                }
                BgLayout::Stride => {
                    let i = (py as usize)
                        .checked_mul(geo.line_length as usize)
                        .and_then(|b| b.checked_add(px as usize))
                        .ok_or("bg stride index overflow")?;
                    background
                        .get(i)
                        .copied()
                        .ok_or_else(|| format!("bg stride OOB at {px},{py}"))?
                }
            };
            let dst = stride_index(geo, px, py)?;
            fb[dst] = src;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum BgLayout {
    Packed,
    Stride,
}

fn raster_label(
    fb: &mut [u8],
    geo: &FbGeometry,
    origin_x: u32,
    origin_y: u32,
    label: &str,
    ink: u8,
) -> Result<(), String> {
    let cell = (GLYPH_W * GLYPH_SCALE) as u32;
    let gap = (GLYPH_GAP * GLYPH_SCALE) as u32;
    let mut cursor_x = origin_x;
    for ch in label.chars() {
        let bits = glyph_bits(ch)?;
        for (gy, row) in bits.iter().enumerate() {
            for gx in 0..GLYPH_W {
                // Glyph constants are authored MSB-left (bit 4 = leftmost column).
                if (row & (1 << (GLYPH_W - 1 - gx))) == 0 {
                    continue;
                }
                for sy in 0..GLYPH_SCALE {
                    for sx in 0..GLYPH_SCALE {
                        let px = cursor_x + (gx * GLYPH_SCALE + sx) as u32;
                        let py = origin_y + (gy * GLYPH_SCALE + sy) as u32;
                        if px >= geo.xres || py >= geo.yres {
                            continue;
                        }
                        let i = stride_index(geo, px, py)?;
                        fb[i] = ink;
                    }
                }
            }
        }
        cursor_x = cursor_x.saturating_add(cell.saturating_add(gap));
    }
    Ok(())
}

fn stride_index(geo: &FbGeometry, x: u32, y: u32) -> Result<usize, String> {
    geo.check_panel_rect(x, y, 1, 1)
        .map_err(|_| format!("pixel out of bounds ({x},{y})"))?;
    let vx = geo.xoffset.checked_add(x).ok_or("fb xoffset overflow")?;
    let vy = geo.yoffset.checked_add(y).ok_or("fb yoffset overflow")?;
    let i = (vy as usize)
        .checked_mul(geo.line_length as usize)
        .and_then(|b| b.checked_add(vx as usize))
        .ok_or("fb index overflow")?;
    Ok(i)
}

/// 5×7 bitmaps; bit 4 = leftmost column of each row (MSB-left).
fn glyph_bits(ch: char) -> Result<&'static [u8; GLYPH_H], String> {
    match ch {
        '0' => Ok(&GLYPH_0),
        '1' => Ok(&GLYPH_1),
        '2' => Ok(&GLYPH_2),
        '3' => Ok(&GLYPH_3),
        '4' => Ok(&GLYPH_4),
        '5' => Ok(&GLYPH_5),
        '6' => Ok(&GLYPH_6),
        '7' => Ok(&GLYPH_7),
        '8' => Ok(&GLYPH_8),
        '9' => Ok(&GLYPH_9),
        ' ' => Ok(&GLYPH_SPACE),
        'o' => Ok(&GLYPH_O),
        'f' => Ok(&GLYPH_F),
        other => Err(format!("unsupported page-label character {other:?}")),
    }
}

// Bit patterns: each byte is one row; low 5 bits used; bit4 = leftmost column (MSB-left).
const GLYPH_SPACE: [u8; 7] = [0, 0, 0, 0, 0, 0, 0];
const GLYPH_0: [u8; 7] = [
    0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
];
const GLYPH_1: [u8; 7] = [
    0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
];
const GLYPH_2: [u8; 7] = [
    0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111,
];
const GLYPH_3: [u8; 7] = [
    0b01110, 0b10001, 0b00001, 0b00110, 0b00001, 0b10001, 0b01110,
];
const GLYPH_4: [u8; 7] = [
    0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
];
const GLYPH_5: [u8; 7] = [
    0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
];
const GLYPH_6: [u8; 7] = [
    0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
];
const GLYPH_7: [u8; 7] = [
    0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
];
const GLYPH_8: [u8; 7] = [
    0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
];
const GLYPH_9: [u8; 7] = [
    0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
];
const GLYPH_O: [u8; 7] = [
    0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110, 0b00000,
];
const GLYPH_F: [u8; 7] = [
    0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn test_geo(xres: u32, yres: u32, line_length: u32) -> FbGeometry {
        FbGeometry {
            xres,
            yres,
            xres_virtual: xres,
            yres_virtual: yres,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length,
            smem_len: line_length.saturating_mul(yres),
        }
    }

    #[test]
    fn format_examples() {
        assert_eq!(format_page_label(1, 1), "1");
        assert_eq!(format_page_label(1, 2), "1 of 2");
        assert_eq!(format_page_label(2, 2), "2 of 2");
        assert_eq!(format_page_label(7, 13), "7 of 13");
        assert_eq!(format_page_label(13, 13), "13 of 13");
    }

    #[test]
    fn format_label_for_page_fail_closed_on_missing_id() {
        use crate::notebook::{
            PersistedNotebookManifestV1, DEFAULT_NOTEBOOK_ID, NOTEBOOK_MANIFEST_VERSION,
        };
        let m = PersistedNotebookManifestV1 {
            v: NOTEBOOK_MANIFEST_VERSION,
            notebook_id: DEFAULT_NOTEBOOK_ID.into(),
            page_ids: vec!["page-1".into(), "page-3".into()],
            active_page_id: "page-1".into(),
            next_page_ordinal: 4,
        };
        assert_eq!(format_label_for_page(&m, "page-1").unwrap(), "1 of 2");
        assert_eq!(format_label_for_page(&m, "page-3").unwrap(), "2 of 2");
        let err = format_label_for_page(&m, "page-2").unwrap_err();
        assert!(
            err.contains("REFUSE PAGE LABEL") && err.contains("does not contain requested page id"),
            "{err}"
        );
        assert!(!err.contains("1 of 2") && !err.contains("1 of "), "{err}");
    }

    #[test]
    fn supported_alphabet_and_refuse_unknown() {
        for ch in "0123456789 of".chars() {
            glyph_bits(ch).unwrap();
        }
        assert!(glyph_bits('A').is_err());
        assert!(glyph_bits('O').is_err()); // uppercase not supported
        assert!(label_pixel_width("1 of 2").is_ok());
        assert!(label_pixel_width("1 Of 2").is_err());
    }

    #[test]
    fn width_deterministic_and_rect_centered() {
        let geo = test_geo(1860, 2480, 1872);
        let label = "7 of 13";
        let w = label_pixel_width(label).unwrap();
        let r = label_rect(&geo, label).unwrap();
        assert_eq!(r.w, w);
        assert_eq!(r.h, (GLYPH_H * GLYPH_SCALE) as u32);
        assert_eq!(r.x, (geo.xres - w) / 2);
        assert_eq!(r.y, geo.yres - BOTTOM_MARGIN_PX - r.h);
        assert_eq!(r.y + r.h + BOTTOM_MARGIN_PX, geo.yres);
    }

    #[test]
    fn blit_light_bg_dark_glyphs_and_dark_bg_light_glyphs() {
        let geo = test_geo(120, 80, 128);
        let packed = geo.xres as usize * geo.yres as usize;
        let light_bg = vec![0xFF; packed];
        let dark_bg = vec![0x00; packed];
        let mut fb_light = vec![0x80; geo.line_length as usize * geo.yres as usize];
        let mut fb_dark = vec![0x80; geo.line_length as usize * geo.yres as usize];
        // Seed fb with noise so restore is observable.
        for b in fb_light.iter_mut().chain(fb_dark.iter_mut()) {
            *b = 0x55;
        }
        let r_light = blit_page_label(&mut fb_light, &light_bg, &geo, "1").unwrap();
        let r_dark = blit_page_label(&mut fb_dark, &dark_bg, &geo, "1").unwrap();
        assert_eq!(r_light, r_dark);
        // Sample a stem pixel for '1' (center column of glyph is set).
        let sample_x = r_light.x + (2 * GLYPH_SCALE) as u32;
        let sample_y = r_light.y + (3 * GLYPH_SCALE) as u32;
        let i = stride_index(&geo, sample_x, sample_y).unwrap();
        assert_eq!(fb_light[i], 0x00);
        assert_eq!(fb_dark[i], 0xFF);

        // '2' top-right corner on must be set; top-left of that row must be clear
        // (guards against MSB/LSB mirroring regressions).
        let two = blit_page_label(&mut fb_light, &light_bg, &geo, "2").unwrap();
        let y1 = two.y + GLYPH_SCALE as u32; // second source row: 0b10001
        let left = stride_index(&geo, two.x, y1).unwrap();
        let right = stride_index(&geo, two.x + (4 * GLYPH_SCALE) as u32, y1).unwrap();
        assert_eq!(fb_light[left], 0x00, "2 row1 leftmost should be ink");
        assert_eq!(fb_light[right], 0x00, "2 row1 rightmost should be ink");
        let y2 = two.y + (2 * GLYPH_SCALE) as u32; // third row: 0b00001 → only right
        let left2 = stride_index(&geo, two.x, y2).unwrap();
        let right2 = stride_index(&geo, two.x + (4 * GLYPH_SCALE) as u32, y2).unwrap();
        assert_eq!(
            fb_light[left2], 0xFF,
            "2 row2 leftmost should be background"
        );
        assert_eq!(fb_light[right2], 0x00, "2 row2 rightmost should be ink");
    }

    #[test]
    fn blit_changes_only_inside_hud_rect() {
        let geo = test_geo(200, 90, 208);
        let packed = geo.xres as usize * geo.yres as usize;
        let bg = vec![0xEE; packed];
        let mut fb = vec![0x11; geo.line_length as usize * geo.yres as usize];
        let before = fb.clone();
        let rect = blit_page_label(&mut fb, &bg, &geo, "2 of 2").unwrap();
        for y in 0..geo.yres {
            for x in 0..geo.xres {
                let i = stride_index(&geo, x, y).unwrap();
                let inside =
                    x >= rect.x && x < rect.x + rect.w && y >= rect.y && y < rect.y + rect.h;
                if !inside {
                    assert_eq!(fb[i], before[i], "mutated outside HUD at {x},{y}");
                }
            }
        }
        let mut changed = false;
        for y in rect.y..rect.y + rect.h {
            for x in rect.x..rect.x + rect.w {
                let i = stride_index(&geo, x, y).unwrap();
                if fb[i] != before[i] {
                    changed = true;
                }
            }
        }
        assert!(changed);
    }

    #[test]
    fn bottom_band_stroke_outside_hud_survives_reconstruct_blit() {
        // Simulate: bg restore + stroke through bottom band (outside HUD rect) + HUD blit.
        let geo = test_geo(200, 90, 208);
        let packed = geo.xres as usize * geo.yres as usize;
        let bg = vec![0xF0; packed];
        let stride = geo.line_length as usize * geo.yres as usize;
        let mut fb = vec![0; stride];
        for y in 0..geo.yres as usize {
            for x in 0..geo.xres as usize {
                fb[y * geo.line_length as usize + x] = bg[y * geo.xres as usize + x];
            }
        }
        let hud = label_rect(&geo, "2 of 2").unwrap();
        // Paint a horizontal "stroke" in the bottom band but left of the HUD rect.
        assert!(hud.x > 4, "need room left of HUD");
        let stroke_y = hud.y + hud.h / 2;
        let stroke_x = 2u32;
        let stroke_i = stride_index(&geo, stroke_x, stroke_y).unwrap();
        fb[stroke_i] = 0x00;
        blit_page_label(&mut fb, &bg, &geo, "2 of 2").unwrap();
        assert_eq!(
            fb[stroke_i], 0x00,
            "handwriting outside HUD rect must remain after blit"
        );
    }

    #[test]
    fn reconstruct_wide_then_narrow_label_fb_level() {
        // Framebuffer-level stand-in for GC16 FULL: full bg restore between labels.
        let geo = test_geo(400, 120, 416);
        let packed = geo.xres as usize * geo.yres as usize;
        let bg = vec![0xF0; packed];
        let stride = geo.line_length as usize * geo.yres as usize;
        let mut fb = vec![0; stride];
        let restore_full = |fb: &mut [u8]| {
            for y in 0..geo.yres as usize {
                for x in 0..geo.xres as usize {
                    fb[y * geo.line_length as usize + x] = bg[y * geo.xres as usize + x];
                }
            }
        };
        restore_full(&mut fb);
        blit_page_label(&mut fb, &bg, &geo, "12 of 12").unwrap();
        let wide = label_rect(&geo, "12 of 12").unwrap();
        restore_full(&mut fb);
        blit_page_label(&mut fb, &bg, &geo, "1").unwrap();
        let narrow = label_rect(&geo, "1").unwrap();
        for y in wide.y..wide.y + wide.h {
            for x in wide.x..wide.x + wide.w {
                let inside_new = x >= narrow.x
                    && x < narrow.x + narrow.w
                    && y >= narrow.y
                    && y < narrow.y + narrow.h;
                if !inside_new {
                    let i = stride_index(&geo, x, y).unwrap();
                    assert_eq!(
                        fb[i], 0xF0,
                        "old wide-label pixel must be absent at {x},{y}"
                    );
                }
            }
        }
    }

    #[test]
    fn baked_chrome_in_background_scrubbed_before_blit() {
        // Contaminated background.raw (Amazon/prior HUD) must not stack under a new label.
        let geo = test_geo(200, 90, 208);
        let packed = geo.xres as usize * geo.yres as usize;
        let mut bg = vec![0xF0; packed];
        let y0 = chrome_band_top(&geo);
        for y in y0..geo.yres {
            for x in 40..160 {
                bg[(y as usize) * geo.xres as usize + (x as usize)] = 0x00;
            }
        }
        assert!(scrub_chrome_band(&mut bg, &geo).unwrap());
        for y in y0..geo.yres {
            for x in 0..geo.xres {
                assert_eq!(
                    bg[(y as usize) * geo.xres as usize + (x as usize)],
                    0xF0,
                    "chrome band must be paper after scrub"
                );
            }
        }
        let stride = geo.line_length as usize * geo.yres as usize;
        let mut fb = vec![0; stride];
        for y in 0..geo.yres as usize {
            for x in 0..geo.xres as usize {
                fb[y * geo.line_length as usize + x] = bg[y * geo.xres as usize + x];
            }
        }
        let rect = blit_page_label(&mut fb, &bg, &geo, "1").unwrap();
        for y in y0..geo.yres {
            for x in 0..geo.xres {
                let inside =
                    x >= rect.x && x < rect.x + rect.w && y >= rect.y && y < rect.y + rect.h;
                if !inside {
                    let i = stride_index(&geo, x, y).unwrap();
                    assert_eq!(fb[i], 0xF0, "stacked chrome at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn transient_hud_second_reconstruct_without_label_clears_chrome() {
        let geo = test_geo(200, 90, 208);
        let packed = geo.xres as usize * geo.yres as usize;
        let bg = vec![0xF0; packed];
        let stride = geo.line_length as usize * geo.yres as usize;
        let mut with_hud = vec![0; stride];
        for y in 0..geo.yres as usize {
            for x in 0..geo.xres as usize {
                with_hud[y * geo.line_length as usize + x] = bg[y * geo.xres as usize + x];
            }
        }
        blit_page_label(&mut with_hud, &bg, &geo, "1 of 2").unwrap();

        let mut without = vec![0; stride];
        for y in 0..geo.yres as usize {
            for x in 0..geo.xres as usize {
                without[y * geo.line_length as usize + x] = bg[y * geo.xres as usize + x];
            }
        }
        let rect = label_rect(&geo, "1 of 2").unwrap();
        restore_rect_from_background(&mut with_hud, &bg, &geo, rect).unwrap();
        assert_eq!(with_hud, without);
    }

    #[test]
    fn blit_does_not_mutate_background_buffer() {
        let geo = test_geo(80, 70, 80);
        let bg = vec![0xCC; geo.xres as usize * geo.yres as usize];
        let snapshot = bg.clone();
        let mut fb = vec![0; geo.line_length as usize * geo.yres as usize];
        blit_page_label(&mut fb, &bg, &geo, "1").unwrap();
        assert_eq!(bg, snapshot);
    }

    #[test]
    fn chrome_path_leaves_journal_files_untouched() {
        use std::fs;
        use std::io::Write;
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("scribe-page-label-{n}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let paths = [
            dir.join("background.raw"),
            dir.join("strokes.jsonl"),
            dir.join("edits.jsonl"),
            dir.join("page.meta"),
            dir.join("notebook.meta"),
        ];
        for (i, p) in paths.iter().enumerate() {
            let mut f = fs::File::create(p).unwrap();
            writeln!(f, "durable-seed-{i}").unwrap();
        }
        let before: Vec<Vec<u8>> = paths.iter().map(|p| fs::read(p).unwrap()).collect();

        let geo = test_geo(200, 90, 208);
        let bg = vec![0xFF; geo.xres as usize * geo.yres as usize];
        let mut fb = vec![0; geo.line_length as usize * geo.yres as usize];
        blit_page_label(&mut fb, &bg, &geo, "2 of 3").unwrap();

        for (p, was) in paths.iter().zip(before.iter()) {
            assert_eq!(&fs::read(p).unwrap(), was, "{}", p.display());
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
