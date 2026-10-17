//! Sleep screen: painted when the Kindle goes to sleep (powerd `goingToScreenSaver`), so the
//! panel never keeps showing a stale notebook/Home frame while asleep. Centered 1-bit Ignite
//! mark (flame + wordmark sampled from `ignite.png`) on white. Wake (`exitingScreenSaver`)
//! re-presents the unchanged UI through the existing F11.1 restore path.
//!
//! FW 5.18.6 evidence (`f111-probe`): blanket paints its own screensaver into `/dev/fb0` in two
//! passes ~1.2–4 s AFTER `goingToScreenSaver`, and suspend comes >= 60 s later. The first paint
//! alone therefore always loses; a [`SleepGuard`] watches a framebuffer fingerprint for a short
//! window and re-asserts the sleep screen whenever something else painted over it.

use std::time::{Duration, Instant};

use kindle_platform::FbGeometry;

use crate::ui_gfx::{text_width, Canvas, Rect, BLACK, GLYPH_H, WHITE};

const MARGIN: u32 = 60;

/// Tight 1-bit Ignite mark (MSB-first packed rows). Width is 71; each row is 9 bytes (72 bits).
const LOGO_W: u32 = 71;
const LOGO_H: u32 = 24;
const LOGO_ROW_BYTES: u32 = 9;
const LOGO: &[u8] = &[
    0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // spark
    0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // flame
    0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x43, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x43, 0x80, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, //
    0x07, 0xC8, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, //
    0x0F, 0x89, 0xF8, 0x00, 0x00, 0x07, 0x0F, 0x00, 0x00, //
    0x0F, 0xC1, 0xF8, 0x00, 0x00, 0x00, 0x0F, 0x00, 0x00, //
    0x1F, 0xE0, 0x70, 0x00, 0x00, 0x00, 0x0F, 0x00, 0x00, //
    0x3C, 0xF0, 0x70, 0xFE, 0x3F, 0xC7, 0x3F, 0xC3, 0xF8, // Ignite
    0x7D, 0xF0, 0x70, 0xFE, 0x3F, 0xC7, 0x3F, 0xC3, 0xFC, //
    0xF8, 0xF8, 0x73, 0xFF, 0xBF, 0xF7, 0x3F, 0xCF, 0xFE, //
    0xF0, 0x7C, 0x73, 0xC7, 0xB8, 0xF7, 0x0F, 0x0F, 0x1E, //
    0xF0, 0x7C, 0x73, 0xC7, 0xB8, 0xF7, 0x0F, 0x0F, 0xFE, //
    0xE2, 0x1C, 0x73, 0xC7, 0xB8, 0xF7, 0x0F, 0x0F, 0xFE, //
    0xE6, 0x1C, 0x73, 0xFF, 0xB8, 0xF7, 0x0F, 0x0F, 0x00, //
    0x67, 0x38, 0x70, 0xFF, 0xB8, 0xF7, 0x0F, 0x0F, 0x08, //
    0x77, 0x39, 0xF8, 0xFF, 0xB8, 0xF7, 0x0F, 0xE7, 0xF8, //
    0x3F, 0xF1, 0xF8, 0x07, 0xB8, 0xF7, 0x07, 0xE3, 0xF8, //
    0x1F, 0xE0, 0x01, 0xFF, 0x80, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x01, 0xFE, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x01, 0xFE, 0x00, 0x00, 0x00, 0x00, 0x00, //
];

fn logo_on(x: u32, y: u32) -> bool {
    if x >= LOGO_W || y >= LOGO_H {
        return false;
    }
    let byte = LOGO[(y * LOGO_ROW_BYTES + x / 8) as usize];
    (byte >> (7 - (x % 8))) & 1 == 1
}

/// Integer scale and top-left of the centered Ignite mark (sleep screen: logo only).
pub fn sleep_layout(geo: &FbGeometry) -> (u32, u32, u32) {
    ignite_layout(geo, None)
}

/// Integer scale and top-left of the Ignite mark, optionally leaving room for a caption.
fn ignite_layout(geo: &FbGeometry, caption: Option<&str>) -> (u32, u32, u32) {
    let usable_w = geo.xres.saturating_sub(2 * MARGIN);
    let usable_h = geo.yres.saturating_sub(2 * MARGIN);
    let scale = (usable_w / LOGO_W).min(usable_h / LOGO_H).max(1);
    let x = geo.xres.saturating_sub(LOGO_W * scale) / 2;
    let logo_h = LOGO_H * scale;
    let block_h = match caption {
        Some(_) => logo_h.saturating_add(scale.saturating_add(GLYPH_H * STARTUP_CAPTION_SCALE)),
        None => logo_h,
    };
    let y = geo.yres.saturating_sub(block_h) / 2;
    (scale, x, y)
}

const STARTUP_CAPTION_SCALE: u32 = 6;

fn paint_ignite(c: &mut Canvas<'_>, scale: u32, x0: u32, y0: u32, ink: u8) {
    for y in 0..LOGO_H {
        for x in 0..LOGO_W {
            if logo_on(x, y) {
                c.fill(Rect::new(x0 + x * scale, y0 + y * scale, scale, scale), ink);
            }
        }
    }
}

/// Caption under the launch splash. ASCII `...` so the 5x7 font actually draws the dots.
pub const LOADING_CAPTION: &str = "Loading...";

/// Paint the sleep screen into the framebuffer (caller submits one GC16 FULL refresh).
/// White paper, black Ignite mark — this is the screensaver, not the launch splash.
pub fn render_sleep(fb: &mut [u8], geo: &FbGeometry) -> Result<(), String> {
    render_ignite(fb, geo, WHITE, BLACK, None)
}

/// Dark launch splash: black field, light Ignite mark, [`LOADING_CAPTION`]. One paint, no animation.
/// Does not read notebooks, pages, backgrounds, or sync state.
pub fn render_startup(fb: &mut [u8], geo: &FbGeometry) -> Result<(), String> {
    render_ignite(fb, geo, BLACK, WHITE, Some(LOADING_CAPTION))
}

fn render_ignite(
    fb: &mut [u8],
    geo: &FbGeometry,
    paper: u8,
    ink: u8,
    caption: Option<&str>,
) -> Result<(), String> {
    let (scale, x0, y0) = ignite_layout(geo, caption);
    let mut c = Canvas::new(fb, geo)?;
    c.fill_all(paper);
    paint_ignite(&mut c, scale, x0, y0, ink);
    if let Some(text) = caption {
        let gap = scale;
        let y = y0 + LOGO_H * scale + gap;
        let w = text_width(text, STARTUP_CAPTION_SCALE);
        let x = geo.xres.saturating_sub(w) / 2;
        c.text(x, y, STARTUP_CAPTION_SCALE, text, ink);
    }
    Ok(())
}

/// How long after `goingToScreenSaver` the sleep screen is defended (blanket finishes by ~4 s).
pub const GUARD_WINDOW: Duration = Duration::from_secs(12);
/// Framebuffer check cadence inside the window.
pub const GUARD_CHECK_EVERY: Duration = Duration::from_millis(200);
/// Hard cap so a screensaver that keeps repainting cannot make us refresh forever.
pub const GUARD_MAX_REASSERTS: u32 = 8;

/// Cheap fingerprint of the visible panel (every 8th row, every 4th byte; FNV-1a).
pub fn fingerprint(fb: &[u8], geo: &FbGeometry) -> u64 {
    let stride = geo.line_length as usize;
    let w = geo.xres as usize;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for y in (0..geo.yres as usize).step_by(8) {
        let Some(row) = fb.get(y * stride..y * stride + w) else {
            break;
        };
        for b in row.iter().step_by(4) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// Defends the painted sleep screen for [`GUARD_WINDOW`] after it was first painted.
#[derive(Debug)]
pub struct SleepGuard {
    expected: u64,
    until: Instant,
    next_check: Instant,
    pub reasserts: u32,
}

/// What the main loop should do with the guard this iteration.
#[derive(Debug, PartialEq, Eq)]
pub enum GuardStep {
    /// Not time to look yet.
    Wait,
    /// Compare the framebuffer fingerprint; repaint if it changed.
    Check,
    /// Window over or cap reached: drop the guard.
    Done,
}

impl SleepGuard {
    pub fn new(expected: u64, now: Instant) -> Self {
        Self {
            expected,
            until: now + GUARD_WINDOW,
            next_check: now + GUARD_CHECK_EVERY,
            reasserts: 0,
        }
    }

    pub fn step(&mut self, now: Instant) -> GuardStep {
        if now >= self.until || self.reasserts >= GUARD_MAX_REASSERTS {
            GuardStep::Done
        } else if now >= self.next_check {
            self.next_check = now + GUARD_CHECK_EVERY;
            GuardStep::Check
        } else {
            GuardStep::Wait
        }
    }

    /// True when the panel no longer shows what we painted.
    pub fn overwritten(&self, current: u64) -> bool {
        current != self.expected
    }

    pub fn reasserted(&mut self, expected: u64) {
        self.expected = expected;
        self.reasserts += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_gfx::panel_geo;

    #[test]
    fn ignite_mark_is_centered_large_and_inside_the_panel() {
        let g = panel_geo();
        let (scale, x, y) = sleep_layout(&g);
        assert!(
            scale >= 20,
            "large mark on the Scribe panel (scale {scale})"
        );
        let w = LOGO_W * scale;
        let h = LOGO_H * scale;
        assert!(x >= MARGIN && x + w <= g.xres - MARGIN, "fits horizontally");
        assert!(y >= MARGIN && y + h <= g.yres - MARGIN, "fits vertically");
        assert_eq!(x, (g.xres - w) / 2);
        assert_eq!(y, (g.yres - h) / 2);
        assert!(logo_on(8, 0) && logo_on(9, 0), "top spark");
        assert!(logo_on(0, 13), "flame body");
        assert!(
            !logo_on(70, 0) && !logo_on(0, 0),
            "corners of the mark stay empty"
        );
    }

    #[test]
    fn guard_detects_overpaint_and_stops_after_window_or_cap() {
        let g = panel_geo();
        let mut fb = vec![0u8; g.smem_len as usize];
        render_sleep(&mut fb, &g).unwrap();
        let t0 = Instant::now();
        let mut guard = SleepGuard::new(fingerprint(&fb, &g), t0);
        assert_eq!(guard.step(t0), GuardStep::Wait);
        assert_eq!(guard.step(t0 + GUARD_CHECK_EVERY), GuardStep::Check);
        assert!(!guard.overwritten(fingerprint(&fb, &g)), "intact");
        // Something else (blanket) paints over the top half.
        let stride = g.line_length as usize;
        for y in 0..g.yres as usize / 2 {
            fb[y * stride..y * stride + g.xres as usize].fill(0x80);
        }
        assert!(guard.overwritten(fingerprint(&fb, &g)));
        render_sleep(&mut fb, &g).unwrap();
        guard.reasserted(fingerprint(&fb, &g));
        assert_eq!(guard.reasserts, 1);
        assert!(!guard.overwritten(fingerprint(&fb, &g)));
        assert_eq!(guard.step(t0 + GUARD_WINDOW), GuardStep::Done);

        let mut capped = SleepGuard::new(0, t0);
        for _ in 0..GUARD_MAX_REASSERTS {
            capped.reasserted(0);
        }
        assert_eq!(capped.step(t0 + GUARD_CHECK_EVERY), GuardStep::Done);
    }

    #[test]
    fn renders_black_mark_on_white_without_touching_stride_padding() {
        let g = panel_geo();
        let mut fb = vec![0x55; g.smem_len as usize];
        render_sleep(&mut fb, &g).unwrap();
        let stride = g.line_length as usize;
        let visible: Vec<u8> = (0..g.yres as usize)
            .flat_map(|y| fb[y * stride..y * stride + g.xres as usize].to_vec())
            .collect();
        assert!(visible.contains(&BLACK) && visible.contains(&WHITE));
        let (scale, x0, y0) = sleep_layout(&g);
        let spark = ((y0 * stride as u32) + x0 + 8 * scale) as usize;
        assert_eq!(fb[spark], BLACK, "spark pixel is ink");
        assert_eq!(fb[0], WHITE, "margin stays paper");
        for y in 0..g.yres as usize {
            assert!(fb[y * stride + g.xres as usize..(y + 1) * stride]
                .iter()
                .all(|b| *b == 0x55));
        }
    }

    #[test]
    fn startup_splash_is_dark_with_light_mark_and_loading_caption() {
        let g = panel_geo();
        let mut fb = vec![0x55; g.smem_len as usize];
        render_startup(&mut fb, &g).unwrap();
        let (scale, x0, y0) = ignite_layout(&g, Some(LOADING_CAPTION));
        let stride = g.line_length as usize;
        let spark = ((y0 * stride as u32) + x0 + 8 * scale) as usize;
        assert_eq!(fb[spark], WHITE, "Ignite spark is light on the dark field");
        assert_eq!(fb[0], BLACK, "field is black, never white paper");
        let cap_y = y0 + LOGO_H * scale + scale;
        let cap_row = &fb[(cap_y as usize) * stride..(cap_y as usize) * stride + g.xres as usize];
        assert!(
            cap_row.contains(&WHITE),
            "Loading caption sits beneath the mark"
        );
        let visible_white = (0..g.yres as usize)
            .flat_map(|y| fb[y * stride..y * stride + g.xres as usize].iter().copied())
            .filter(|&b| b == WHITE)
            .count();
        let visible = (g.xres as usize) * (g.yres as usize);
        assert!(
            visible_white > 0 && visible_white < visible / 4,
            "light pixels are the mark+caption, not a white flash (white={visible_white} of {visible})"
        );
        assert_ne!(
            sleep_layout(&g).2,
            y0,
            "caption shifts the mark up so the pair stays centered"
        );
    }
}
