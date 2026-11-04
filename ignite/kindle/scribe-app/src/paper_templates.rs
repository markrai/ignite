//! Deterministic Gray8 page-background templates (Slice 1).
//!
//! Pure packed `width * height` buffers for eventual use as `background.raw`.
//! No framebuffer, filesystem, sync, or UI coupling.
//!
//! ## Geometry
//!
//! Panel pixel coordinates (Kindle visible surface), not canonical stroke space.
//!
//! ## Chrome-band contract
//!
//! [`crate::page_label::scrub_chrome_band`] fills the bottom chrome band from a
//! paper sample taken eight rows above that band. Generated templates keep the
//! entire bottom `chrome_band_height_px() + 8` rows white (capped by page height)
//! so scrub is a no-op on every supported geometry.
//!
//! ## Mark constants
//!
//! Spacing and gray levels are fixed initial values for E Ink — not
//! hardware-validated. Tune only by changing these constants; do not rewrite
//! already-persisted page backgrounds.

use crate::page_label::chrome_band_height_px;

/// Paper / unmarked Gray8 white (matches `ui_gfx::WHITE` and `BackgroundIdentityV1::Blank`).
pub const PAPER_WHITE: u8 = 0xFF;

/// Grid/dot pigment (matched to physical E Ink tuning of ruled lines).
pub const MARK_GRAY: u8 = 0x80;

/// Ruled line pigment (same intensity as [`MARK_GRAY`] after physical E Ink tuning).
pub const RULED_MARK_GRAY: u8 = 0x80;

/// Vertical pitch between ruled horizontal lines (panel px).
pub const RULED_SPACING_PX: u32 = 80;

/// Pitch for grid lines (panel px).
pub const GRID_SPACING_PX: u32 = 40;

/// Pitch for dotted lattice (panel px).
pub const DOTTED_SPACING_PX: u32 = 40;

/// Side length of each filled square at a dotted lattice point (panel px).
/// Single-pixel marks lack enough area to register on E Ink; spacing is unchanged.
pub const DOTTED_MARK_SIZE_PX: u32 = 3;

/// Rows above the chrome band sampled by `scrub_chrome_band` (`sample_y = chrome_top - 8`).
const CHROME_SAMPLE_MARGIN_PX: u32 = 8;

/// Verified Scribe 1st Gen panel (firmware 5.18.6 path).
pub const SCRIBE_PANEL_WIDTH: u32 = 1_860;
pub const SCRIBE_PANEL_HEIGHT: u32 = 2_480;

/// Page paper style for durable background generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PageTemplate {
    Blank,
    Ruled,
    Grid,
    Dotted,
}

impl PageTemplate {
    /// Fixed Kindle dialog labels for the paper picker (uppercase, matching other menus).
    pub const fn dialog_label(self) -> &'static str {
        match self {
            Self::Blank => "BLANK",
            Self::Ruled => "RULED",
            Self::Grid => "GRID",
            Self::Dotted => "DOTTED",
        }
    }

    /// Choices offered by the notebook paper picker, in display order.
    pub const fn picker_choices() -> [Self; 4] {
        [Self::Blank, Self::Ruled, Self::Grid, Self::Dotted]
    }
}

/// How many bottom rows must stay white so scrub sampling + chrome fill are no-ops.
///
/// Equals `min(height, chrome_band_height_px() + 8)`. On short fixtures the whole
/// page may be reserved (output is all white for every template).
pub fn reserved_bottom_rows(height: u32) -> u32 {
    let need = chrome_band_height_px().saturating_add(CHROME_SAMPLE_MARGIN_PX);
    need.min(height)
}

/// Top of the drawable (non-reserved) region: `0..usable_height`.
pub fn usable_height(height: u32) -> u32 {
    height.saturating_sub(reserved_bottom_rows(height))
}

/// Packed Gray8 background: exactly `width * height` bytes, row-major, no stride padding.
pub fn generate_page_background(
    template: PageTemplate,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err(format!(
            "REFUSE TEMPLATE: invalid dimensions {width}x{height}"
        ));
    }
    // Checked u64 product first; then try_reserve_exact so hosts where the product
    // fits in usize but not in addressable memory (e.g. u32::MAX² on 64-bit) refuse
    // cleanly instead of panicking inside `vec![_; len]`.
    let len_u64 = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| format!("REFUSE TEMPLATE: dimension overflow {width}x{height}"))?;
    let len = usize::try_from(len_u64).map_err(|_| {
        format!("REFUSE TEMPLATE: dimension overflow {width}x{height}")
    })?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).map_err(|e| {
        format!("REFUSE TEMPLATE: cannot allocate {len} bytes for {width}x{height}: {e}")
    })?;
    buf.resize(len, PAPER_WHITE);
    let usable = usable_height(height);
    if usable == 0 || matches!(template, PageTemplate::Blank) {
        return Ok(buf);
    }
    match template {
        PageTemplate::Blank => unreachable!(),
        PageTemplate::Ruled => draw_ruled(&mut buf, width, usable),
        PageTemplate::Grid => draw_grid(&mut buf, width, usable),
        PageTemplate::Dotted => draw_dotted(&mut buf, width, usable),
    }
    debug_assert_eq!(buf.len(), len);
    debug_assert!(
        reserved_region_is_white(&buf, width, height),
        "generator violated chrome reserved region"
    );
    Ok(buf)
}

fn index(width: u32, x: u32, y: u32) -> usize {
    (y as usize)
        .saturating_mul(width as usize)
        .saturating_add(x as usize)
}

fn put(buf: &mut [u8], width: u32, x: u32, y: u32, value: u8) {
    debug_assert!(x < width);
    let i = index(width, x, y);
    debug_assert!(i < buf.len());
    buf[i] = value;
}

fn draw_hline(buf: &mut [u8], width: u32, y: u32, usable: u32, gray: u8) {
    if y >= usable {
        return;
    }
    for x in 0..width {
        put(buf, width, x, y, gray);
    }
}

fn draw_vline(buf: &mut [u8], width: u32, x: u32, usable: u32, gray: u8) {
    if x >= width {
        return;
    }
    for y in 0..usable {
        put(buf, width, x, y, gray);
    }
}

fn draw_ruled(buf: &mut [u8], width: u32, usable: u32) {
    let mut y = RULED_SPACING_PX;
    while y < usable {
        draw_hline(buf, width, y, usable, RULED_MARK_GRAY);
        match y.checked_add(RULED_SPACING_PX) {
            Some(n) => y = n,
            None => break,
        }
    }
}

fn draw_grid(buf: &mut [u8], width: u32, usable: u32) {
    let mut y = GRID_SPACING_PX;
    while y < usable {
        draw_hline(buf, width, y, usable, MARK_GRAY);
        match y.checked_add(GRID_SPACING_PX) {
            Some(n) => y = n,
            None => break,
        }
    }
    let mut x = GRID_SPACING_PX;
    while x < width {
        draw_vline(buf, width, x, usable, MARK_GRAY);
        match x.checked_add(GRID_SPACING_PX) {
            Some(n) => x = n,
            None => break,
        }
    }
}

fn draw_dot(buf: &mut [u8], width: u32, usable: u32, cx: u32, cy: u32) {
    // Odd sizes are centered on (cx, cy); even sizes bias toward +x/+y.
    let half = DOTTED_MARK_SIZE_PX / 2;
    let x0 = cx.saturating_sub(half);
    let y0 = cy.saturating_sub(half);
    for dy in 0..DOTTED_MARK_SIZE_PX {
        let y = match y0.checked_add(dy) {
            Some(v) if v < usable => v,
            _ => continue,
        };
        for dx in 0..DOTTED_MARK_SIZE_PX {
            let x = match x0.checked_add(dx) {
                Some(v) if v < width => v,
                _ => continue,
            };
            put(buf, width, x, y, MARK_GRAY);
        }
    }
}

fn draw_dotted(buf: &mut [u8], width: u32, usable: u32) {
    let mut y = DOTTED_SPACING_PX;
    while y < usable {
        let mut x = DOTTED_SPACING_PX;
        while x < width {
            draw_dot(buf, width, usable, x, y);
            match x.checked_add(DOTTED_SPACING_PX) {
                Some(n) => x = n,
                None => break,
            }
        }
        match y.checked_add(DOTTED_SPACING_PX) {
            Some(n) => y = n,
            None => break,
        }
    }
}

fn reserved_region_is_white(buf: &[u8], width: u32, height: u32) -> bool {
    let y0 = usable_height(height);
    for y in y0..height {
        for x in 0..width {
            if buf[index(width, x, y)] != PAPER_WHITE {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_label::scrub_chrome_band;
    use crate::store::BackgroundIdentityV1;
    use kindle_platform::FbGeometry;

    fn packed_geo(width: u32, height: u32) -> FbGeometry {
        FbGeometry {
            xres: width,
            yres: height,
            xres_virtual: width,
            yres_virtual: height,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: width,
            smem_len: width.saturating_mul(height),
        }
    }

    fn scrub_is_noop(bg: &mut [u8], width: u32, height: u32) -> Result<bool, String> {
        let before = bg.to_vec();
        let changed = scrub_chrome_band(bg, &packed_geo(width, height))?;
        Ok(!changed && bg == before.as_slice())
    }

    fn count_marks(buf: &[u8]) -> usize {
        buf.iter().filter(|&&b| b != PAPER_WHITE).count()
    }

    #[test]
    fn blank_is_exact_all_white_packed() {
        let bg = generate_page_background(PageTemplate::Blank, 40, 100).unwrap();
        assert_eq!(bg.len(), 40 * 100);
        assert!(bg.iter().all(|&b| b == PAPER_WHITE));
        assert!(matches!(
            BackgroundIdentityV1::from_pixels(&bg),
            BackgroundIdentityV1::Blank { .. }
        ));
    }

    #[test]
    fn dimensions_and_layout_are_packed_width_times_height() {
        for &(w, h) in &[(1, 1), (16, 64), (186, 248)] {
            let bg = generate_page_background(PageTemplate::Ruled, w, h).unwrap();
            assert_eq!(bg.len(), (w * h) as usize);
        }
    }

    #[test]
    fn nonblank_styles_mark_usable_region() {
        let w = 120u32;
        let h = 200u32;
        let usable = usable_height(h);
        assert!(usable > RULED_SPACING_PX, "fixture must fit at least one rule");
        for style in [
            PageTemplate::Ruled,
            PageTemplate::Grid,
            PageTemplate::Dotted,
        ] {
            let bg = generate_page_background(style, w, h).unwrap();
            assert!(
                count_marks(&bg) > 0,
                "{style:?} must place marks in usable region"
            );
            assert!(
                reserved_region_is_white(&bg, w, h),
                "{style:?} reserved region must stay white"
            );
            // No marks at or below usable_height.
            for y in usable..h {
                for x in 0..w {
                    assert_eq!(bg[index(w, x, y)], PAPER_WHITE);
                }
            }
        }
    }

    #[test]
    fn same_input_is_byte_identical() {
        for style in [
            PageTemplate::Blank,
            PageTemplate::Ruled,
            PageTemplate::Grid,
            PageTemplate::Dotted,
        ] {
            let a = generate_page_background(style, 90, 160).unwrap();
            let b = generate_page_background(style, 90, 160).unwrap();
            assert_eq!(a, b, "{style:?}");
        }
    }

    #[test]
    fn styles_produce_distinct_buffers() {
        let w = 100u32;
        let h = 180u32;
        let blank = generate_page_background(PageTemplate::Blank, w, h).unwrap();
        let ruled = generate_page_background(PageTemplate::Ruled, w, h).unwrap();
        let grid = generate_page_background(PageTemplate::Grid, w, h).unwrap();
        let dotted = generate_page_background(PageTemplate::Dotted, w, h).unwrap();
        assert_ne!(blank, ruled);
        assert_ne!(blank, grid);
        assert_ne!(blank, dotted);
        assert_ne!(ruled, grid);
        assert_ne!(ruled, dotted);
        assert_ne!(grid, dotted);
    }

    #[test]
    fn ruled_spacing_is_deterministic_integer_placement() {
        let w = 50u32;
        let h = 220u32;
        let usable = usable_height(h);
        let bg = generate_page_background(PageTemplate::Ruled, w, h).unwrap();
        let mut expected_y = RULED_SPACING_PX;
        while expected_y < usable {
            assert_eq!(
                bg[index(w, 0, expected_y)],
                RULED_MARK_GRAY,
                "missing rule at y={expected_y}"
            );
            assert_eq!(bg[index(w, w - 1, expected_y)], RULED_MARK_GRAY);
            // Midway between rules stays paper (when spacing allows).
            let mid = expected_y + RULED_SPACING_PX / 2;
            if mid < usable {
                assert_eq!(bg[index(w, 0, mid)], PAPER_WHITE);
            }
            expected_y += RULED_SPACING_PX;
        }
    }

    #[test]
    fn grid_and_dotted_placement_follow_spacing_constants() {
        let w = 100u32;
        let h = 200u32;
        let usable = usable_height(h);
        let grid = generate_page_background(PageTemplate::Grid, w, h).unwrap();
        assert_eq!(grid[index(w, GRID_SPACING_PX, 0)], MARK_GRAY);
        assert_eq!(grid[index(w, 0, GRID_SPACING_PX)], MARK_GRAY);
        assert_eq!(
            grid[index(w, GRID_SPACING_PX, GRID_SPACING_PX)],
            MARK_GRAY
        );

        let dotted = generate_page_background(PageTemplate::Dotted, w, h).unwrap();
        let cx = DOTTED_SPACING_PX;
        let cy = DOTTED_SPACING_PX;
        assert_eq!(dotted[index(w, cx, cy)], MARK_GRAY);
        // 3×3 footprint includes the immediate neighbors of the lattice point.
        assert_eq!(dotted[index(w, cx + 1, cy)], MARK_GRAY);
        assert_eq!(dotted[index(w, cx, cy + 1)], MARK_GRAY);
        assert_eq!(dotted[index(w, 0, 0)], PAPER_WHITE);
        assert_eq!(dotted[index(w, cx, 0)], PAPER_WHITE);
        let outside = cy + DOTTED_MARK_SIZE_PX;
        if outside < usable {
            assert_eq!(dotted[index(w, cx, outside)], PAPER_WHITE);
        }
    }

    #[test]
    fn invalid_and_overflow_dimensions_are_refused() {
        assert!(generate_page_background(PageTemplate::Blank, 0, 10).is_err());
        assert!(generate_page_background(PageTemplate::Ruled, 10, 0).is_err());
        assert!(generate_page_background(PageTemplate::Grid, 0, 0).is_err());
        // On 64-bit hosts u32::MAX² fits in usize but is not allocatable; refuse via
        // try_reserve_exact (do not panic). On 32-bit the product overflows usize first.
        let err = generate_page_background(PageTemplate::Blank, u32::MAX, u32::MAX);
        assert!(err.is_err(), "u32::MAX x u32::MAX must refuse: {err:?}");
        let msg = err.unwrap_err();
        assert!(
            msg.contains("cannot allocate") || msg.contains("overflow"),
            "unexpected refusal wording: {msg}"
        );
    }

    #[test]
    fn short_fixtures_keep_scrub_stable_even_if_all_reserved() {
        // Height below chrome+sample: every row is reserved → all white for all styles.
        let h = chrome_band_height_px(); // 49; sample margin makes reserved = 49
        assert!(h < chrome_band_height_px() + CHROME_SAMPLE_MARGIN_PX);
        let w = 32u32;
        for style in [
            PageTemplate::Blank,
            PageTemplate::Ruled,
            PageTemplate::Grid,
            PageTemplate::Dotted,
        ] {
            let mut bg = generate_page_background(style, w, h).unwrap();
            assert!(bg.iter().all(|&b| b == PAPER_WHITE), "{style:?}");
            assert!(scrub_is_noop(&mut bg, w, h).unwrap(), "{style:?}");
        }
    }

    #[test]
    fn scrub_chrome_band_is_noop_on_generated_small_geometry() {
        let w = 200u32;
        let h = 160u32;
        assert!(usable_height(h) > GRID_SPACING_PX);
        for style in [
            PageTemplate::Blank,
            PageTemplate::Ruled,
            PageTemplate::Grid,
            PageTemplate::Dotted,
        ] {
            let mut bg = generate_page_background(style, w, h).unwrap();
            let before = bg.clone();
            let changed = scrub_chrome_band(&mut bg, &packed_geo(w, h)).unwrap();
            assert!(!changed, "{style:?} first scrub must be no-op");
            assert_eq!(bg, before, "{style:?}");
            let changed2 = scrub_chrome_band(&mut bg, &packed_geo(w, h)).unwrap();
            assert!(!changed2, "{style:?} second scrub must be no-op");
            assert_eq!(bg, before, "{style:?}");
        }
    }

    #[test]
    fn scrub_chrome_band_is_noop_on_scribe_panel_geometry() {
        // One allocation of the real panel (~4.4 MiB) covering all four styles via reuse.
        let w = SCRIBE_PANEL_WIDTH;
        let h = SCRIBE_PANEL_HEIGHT;
        assert_eq!(reserved_bottom_rows(h), 57);
        assert_eq!(usable_height(h), h - 57);
        for style in [
            PageTemplate::Blank,
            PageTemplate::Ruled,
            PageTemplate::Grid,
            PageTemplate::Dotted,
        ] {
            let mut bg = generate_page_background(style, w, h).unwrap();
            assert_eq!(bg.len(), (w * h) as usize);
            // Spot-check reserved white band.
            for y in (h - 57)..h {
                assert_eq!(bg[index(w, 0, y)], PAPER_WHITE);
                assert_eq!(bg[index(w, w / 2, y)], PAPER_WHITE);
            }
            let before = bg.clone();
            assert!(
                !scrub_chrome_band(&mut bg, &packed_geo(w, h)).unwrap(),
                "{style:?}"
            );
            assert_eq!(bg, before, "{style:?}");
            assert!(
                !scrub_chrome_band(&mut bg, &packed_geo(w, h)).unwrap(),
                "{style:?} second"
            );
        }
    }

    #[test]
    fn background_identity_blank_vs_blob() {
        // Height must clear reserved chrome+sample (57) plus at least one ruled line (80).
        let blank = generate_page_background(PageTemplate::Blank, 64, 200).unwrap();
        assert!(matches!(
            BackgroundIdentityV1::from_pixels(&blank),
            BackgroundIdentityV1::Blank { .. }
        ));
        for style in [
            PageTemplate::Ruled,
            PageTemplate::Grid,
            PageTemplate::Dotted,
        ] {
            let bg = generate_page_background(style, 64, 200).unwrap();
            assert!(
                matches!(
                    BackgroundIdentityV1::from_pixels(&bg),
                    BackgroundIdentityV1::Blob { .. }
                ),
                "{style:?}"
            );
        }
    }

    #[test]
    fn matches_existing_paper_background_for_blank_scribe_size() {
        // Blank must match `ui_gfx::paper_background` semantics: packed all 0xFF.
        let bg = generate_page_background(
            PageTemplate::Blank,
            SCRIBE_PANEL_WIDTH,
            SCRIBE_PANEL_HEIGHT,
        )
        .unwrap();
        assert_eq!(bg.len(), (SCRIBE_PANEL_WIDTH * SCRIBE_PANEL_HEIGHT) as usize);
        assert!(bg.iter().all(|&b| b == PAPER_WHITE));
    }
}
