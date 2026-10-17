//! Gate F11 framebuffer-native UI primitives: rectangles, a clipped 8bpp canvas, and the
//! conservative 5x7 bitmap font shared by Home, modals, the title keyboard, and notebook chrome.
//!
//! Everything here paints the live framebuffer only. Nothing in this module reads or writes
//! notebook backgrounds or journals.

use kindle_platform::FbGeometry;
use notebook_core::{PagePoint, ScribeDisplayTransform};

pub const WHITE: u8 = 0xFF;
pub const BLACK: u8 = 0x00;
/// Disabled controls. Mid-gray renders reliably under GC16.
pub const GRAY: u8 = 0x99;

pub const GLYPH_W: u32 = 5;
pub const GLYPH_H: u32 = 7;

/// Panel-pixel rectangle (origin top-left, half-open extent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> u32 {
        self.x.saturating_add(self.w)
    }

    pub fn bottom(&self) -> u32 {
        self.y.saturating_add(self.h)
    }

    pub fn contains(&self, px: u32, py: u32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    pub fn inset(&self, d: u32) -> Rect {
        let d2 = d.saturating_mul(2);
        Rect::new(
            self.x.saturating_add(d),
            self.y.saturating_add(d),
            self.w.saturating_sub(d2),
            self.h.saturating_sub(d2),
        )
    }

    /// Clamp to the visible panel; `None` when nothing remains.
    pub fn clamp_to(&self, geo: &FbGeometry) -> Option<Rect> {
        if self.x >= geo.xres || self.y >= geo.yres {
            return None;
        }
        let w = self.w.min(geo.xres - self.x);
        let h = self.h.min(geo.yres - self.y);
        (w > 0 && h > 0).then_some(Rect::new(self.x, self.y, w, h))
    }
}

/// A pen position in panel pixels, derived through the same canonical → panel transform the
/// ink renderer uses. UI hit-testing never touches stored canonical coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiPoint {
    pub x: u32,
    pub y: u32,
}

impl UiPoint {
    #[cfg(test)]
    pub fn new(x: u32, y: u32) -> Self {
        Self { x, y }
    }

    pub fn from_page(page: PagePoint) -> Option<Self> {
        ScribeDisplayTransform::PortraitIdentity
            .to_panel_pixel(page)
            .map(|(x, y)| Self { x, y })
    }
}

/// Plain paper background for newly created notebooks/pages: packed `xres*yres`, all white.
///
/// F11 never derives a durable background from the live framebuffer, which may contain Home,
/// modals, notebook chrome, or Amazon UI.
pub fn paper_background(geo: &FbGeometry) -> Vec<u8> {
    vec![WHITE; (geo.xres as usize) * (geo.yres as usize)]
}

pub struct Canvas<'a> {
    fb: &'a mut [u8],
    geo: FbGeometry,
}

impl<'a> Canvas<'a> {
    pub fn new(fb: &'a mut [u8], geo: &FbGeometry) -> Result<Self, String> {
        if geo.bits_per_pixel != 8 {
            return Err(format!(
                "REFUSE UI RENDER: expected 8bpp framebuffer, got {}",
                geo.bits_per_pixel
            ));
        }
        let rows = geo
            .yoffset
            .checked_add(geo.yres)
            .ok_or("REFUSE UI RENDER: framebuffer size overflow")?;
        let needed = (geo.line_length as usize)
            .checked_mul(rows as usize)
            .ok_or("REFUSE UI RENDER: framebuffer size overflow")?;
        if fb.len() < needed || geo.xoffset.saturating_add(geo.xres) > geo.line_length {
            return Err(format!(
                "REFUSE UI RENDER: framebuffer has {} bytes, need {needed}",
                fb.len()
            ));
        }
        Ok(Self { fb, geo: *geo })
    }

    fn row_start(&self, y: u32) -> usize {
        ((self.geo.yoffset + y) as usize) * (self.geo.line_length as usize)
            + self.geo.xoffset as usize
    }

    pub fn put(&mut self, x: u32, y: u32, value: u8) {
        if x >= self.geo.xres || y >= self.geo.yres {
            return;
        }
        let i = self.row_start(y) + x as usize;
        if i < self.fb.len() {
            self.fb[i] = value;
        }
    }

    pub fn fill(&mut self, rect: Rect, value: u8) {
        let Some(r) = rect.clamp_to(&self.geo) else {
            return;
        };
        for y in r.y..r.bottom() {
            let start = self.row_start(y) + r.x as usize;
            self.fb[start..start + r.w as usize].fill(value);
        }
    }

    pub fn fill_all(&mut self, value: u8) {
        let full = Rect::new(0, 0, self.geo.xres, self.geo.yres);
        self.fill(full, value);
    }

    /// Outline of `thickness` pixels drawn inside `rect`.
    pub fn frame(&mut self, rect: Rect, thickness: u32, value: u8) {
        let t = thickness.min(rect.w / 2).min(rect.h / 2).max(1);
        self.fill(Rect::new(rect.x, rect.y, rect.w, t), value);
        self.fill(Rect::new(rect.x, rect.bottom() - t, rect.w, t), value);
        self.fill(Rect::new(rect.x, rect.y, t, rect.h), value);
        self.fill(Rect::new(rect.right() - t, rect.y, t, rect.h), value);
    }

    /// Draw `text` with its top-left glyph cell at `(x, y)`. Clipped at the panel edge.
    pub fn text(&mut self, x: u32, y: u32, scale: u32, text: &str, value: u8) {
        let scale = scale.max(1);
        let mut cursor = x;
        for ch in text.chars() {
            let bits = glyph_bits(ch);
            for (row, pattern) in bits.iter().enumerate() {
                for col in 0..GLYPH_W {
                    if pattern & (1 << (GLYPH_W - 1 - col)) == 0 {
                        continue;
                    }
                    let px = cursor + col * scale;
                    let py = y + row as u32 * scale;
                    self.fill(Rect::new(px, py, scale, scale), value);
                }
            }
            cursor = cursor.saturating_add(glyph_advance(scale));
            if cursor >= self.geo.xres {
                break;
            }
        }
    }

    /// Center `text` inside `rect` (clipped with an ellipsis when too wide).
    pub fn text_centered(&mut self, rect: Rect, scale: u32, text: &str, value: u8) {
        let fitted = fit_text(text, scale, rect.w);
        let w = text_width(&fitted, scale);
        let h = GLYPH_H * scale;
        let x = rect.x + rect.w.saturating_sub(w) / 2;
        let y = rect.y + rect.h.saturating_sub(h) / 2;
        self.text(x, y, scale, &fitted, value);
    }

    /// Filled isosceles triangle pointing left (`left == true`) or right inside `rect`.
    pub fn triangle(&mut self, rect: Rect, left: bool, value: u8) {
        let half = rect.h / 2;
        if half == 0 || rect.w == 0 {
            return;
        }
        for dy in 0..rect.h {
            let dist = dy.abs_diff(half);
            let span = rect.w.saturating_mul(half.saturating_sub(dist)) / half;
            if span == 0 {
                continue;
            }
            let x = if left { rect.right() - span } else { rect.x };
            self.fill(Rect::new(x, rect.y + dy, span, 1), value);
        }
    }

    /// Three square dots: the "more actions" affordance.
    pub fn dots(&mut self, rect: Rect, dot: u32, value: u8) {
        let gap = dot;
        let total = dot * 3 + gap * 2;
        let x0 = rect.x + rect.w.saturating_sub(total) / 2;
        let y0 = rect.y + rect.h.saturating_sub(dot) / 2;
        for i in 0..3 {
            self.fill(Rect::new(x0 + i * (dot + gap), y0, dot, dot), value);
        }
    }
}

pub fn glyph_advance(scale: u32) -> u32 {
    (GLYPH_W + 1).saturating_mul(scale.max(1))
}

/// Rendered width of `text` (no trailing inter-glyph gap).
pub fn text_width(text: &str, scale: u32) -> u32 {
    let n = text.chars().count() as u32;
    if n == 0 {
        return 0;
    }
    n.saturating_mul(glyph_advance(scale))
        .saturating_sub(scale.max(1))
}

pub fn max_chars(scale: u32, width: u32) -> usize {
    // n glyphs need n*adv - scale pixels.
    ((width.saturating_add(scale.max(1))) / glyph_advance(scale)) as usize
}

/// Largest integer scale in `[min_scale, max_scale]` whose rendered width fits `width`.
/// Returns `min_scale` when even that overflows (caller should `fit_text`).
pub fn scale_to_fit(text: &str, max_scale: u32, min_scale: u32, width: u32) -> u32 {
    let max_scale = max_scale.max(1);
    let min_scale = min_scale.max(1).min(max_scale);
    if text.is_empty() {
        return max_scale;
    }
    for scale in (min_scale..=max_scale).rev() {
        if text_width(text, scale) <= width {
            return scale;
        }
    }
    min_scale
}

/// Clip `text` to `width`, replacing the tail with `...` when it does not fit.
pub fn fit_text(text: &str, scale: u32, width: u32) -> String {
    let limit = max_chars(scale, width);
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    if limit <= 3 {
        return ".".repeat(limit);
    }
    let mut out: String = text.chars().take(limit - 3).collect();
    out.push_str("...");
    out
}

/// Keep the *end* of `text` visible (used for the title being edited: cursor is at the end).
pub fn fit_text_tail(text: &str, scale: u32, width: u32) -> String {
    let limit = max_chars(scale, width);
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    if limit <= 3 {
        return ".".repeat(limit);
    }
    let keep = limit - 3;
    let mut out = String::from("...");
    out.extend(text.chars().skip(count - keep));
    out
}

/// Greedy word wrap to `width`; overlong words are hard-broken. At most `max_lines` lines,
/// the last one ellipsized when content remains.
pub fn wrap_text(text: &str, scale: u32, width: u32, max_lines: usize) -> Vec<String> {
    let limit = max_chars(scale, width).max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        let mut word: String = word.to_string();
        loop {
            let cur_len = current.chars().count();
            let word_len = word.chars().count();
            let needed = if cur_len == 0 {
                word_len
            } else {
                cur_len + 1 + word_len
            };
            if needed <= limit {
                if cur_len > 0 {
                    current.push(' ');
                }
                current.push_str(&word);
                break;
            }
            if cur_len > 0 {
                lines.push(std::mem::take(&mut current));
                continue;
            }
            // Word alone is too long: hard break.
            let head: String = word.chars().take(limit).collect();
            let tail: String = word.chars().skip(limit).collect();
            lines.push(head);
            word = tail;
            if word.is_empty() {
                break;
            }
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    if max_lines > 0 && lines.len() > max_lines {
        lines.truncate(max_lines);
        let last = &mut lines[max_lines - 1];
        let mut s: String = last.chars().take(limit.saturating_sub(3)).collect();
        s.push_str("...");
        *last = s;
    }
    lines
}

/// Whether the font has a dedicated glyph (others render as `?`; stored text is unchanged).
#[cfg(test)]
pub fn has_glyph(ch: char) -> bool {
    ch == '?' || glyph_bits(ch) != &REPLACEMENT
}

const REPLACEMENT: [u8; 7] = [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0, 0b00100];

/// Conservative 5x7 font: ASCII letters (both cases), digits, and the title symbol set.
/// Anything else renders as a deterministic replacement glyph and never affects storage.
pub fn glyph_bits(ch: char) -> &'static [u8; 7] {
    match ch {
        'A' => &[
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => &[
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => &[
            0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111,
        ],
        'D' => &[
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => &[
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => &[
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => &[
            0b01111, 0b10000, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111,
        ],
        'H' => &[
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => &[
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111,
        ],
        'J' => &[
            0b00001, 0b00001, 0b00001, 0b00001, 0b10001, 0b10001, 0b01110,
        ],
        'K' => &[
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => &[
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => &[
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => &[
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => &[
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => &[
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => &[
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => &[
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => &[
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => &[
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => &[
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => &[
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => &[
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010,
        ],
        'X' => &[
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => &[
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => &[
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        'a' => &[0, 0, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111],
        'b' => &[
            0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b11110,
        ],
        'c' => &[0, 0, 0b01110, 0b10000, 0b10000, 0b10001, 0b01110],
        'd' => &[
            0b00001, 0b00001, 0b01101, 0b10011, 0b10001, 0b10001, 0b01111,
        ],
        'e' => &[0, 0, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110],
        'f' => &[
            0b00110, 0b01001, 0b01000, 0b11100, 0b01000, 0b01000, 0b01000,
        ],
        'g' => &[0, 0b01111, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110],
        'h' => &[
            0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001,
        ],
        'i' => &[0b00100, 0, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110],
        'j' => &[0b00010, 0, 0b00110, 0b00010, 0b00010, 0b10010, 0b01100],
        'k' => &[
            0b10000, 0b10000, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010,
        ],
        'l' => &[
            0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'm' => &[0, 0, 0b11010, 0b10101, 0b10101, 0b10001, 0b10001],
        'n' => &[0, 0, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001],
        'o' => &[0, 0, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110],
        'p' => &[0, 0, 0b11110, 0b10001, 0b11110, 0b10000, 0b10000],
        'q' => &[0, 0, 0b01101, 0b10011, 0b01111, 0b00001, 0b00001],
        'r' => &[0, 0, 0b10110, 0b11001, 0b10000, 0b10000, 0b10000],
        's' => &[0, 0, 0b01110, 0b10000, 0b01110, 0b00001, 0b11110],
        't' => &[
            0b01000, 0b01000, 0b11100, 0b01000, 0b01000, 0b01001, 0b00110,
        ],
        'u' => &[0, 0, 0b10001, 0b10001, 0b10001, 0b10011, 0b01101],
        'v' => &[0, 0, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'w' => &[0, 0, 0b10001, 0b10001, 0b10101, 0b10101, 0b01010],
        'x' => &[0, 0, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001],
        'y' => &[0, 0, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110],
        'z' => &[0, 0, 0b11111, 0b00010, 0b00100, 0b01000, 0b11111],
        '0' => &[
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => &[
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => &[
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => &[
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => &[
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => &[
            0b11111, 0b10000, 0b10000, 0b11110, 0b00001, 0b00001, 0b11110,
        ],
        '6' => &[
            0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => &[
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => &[
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => &[
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
        ],
        ' ' => &[0, 0, 0, 0, 0, 0, 0],
        '+' => &[0, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0],
        '-' => &[0, 0, 0, 0b11111, 0, 0, 0],
        '_' => &[0, 0, 0, 0, 0, 0, 0b11111],
        '.' => &[0, 0, 0, 0, 0, 0b01100, 0b01100],
        ',' => &[0, 0, 0, 0, 0b01100, 0b00100, 0b01000],
        ':' => &[0, 0b01100, 0b01100, 0, 0b01100, 0b01100, 0],
        ';' => &[0, 0b01100, 0b01100, 0, 0b01100, 0b00100, 0b01000],
        '\'' => &[0b00100, 0b00100, 0b01000, 0, 0, 0, 0],
        '"' => &[0b01010, 0b01010, 0b01010, 0, 0, 0, 0],
        '&' => &[
            0b01100, 0b10010, 0b10100, 0b01000, 0b10101, 0b10010, 0b01101,
        ],
        '!' => &[0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0, 0b00100],
        '(' => &[
            0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010,
        ],
        ')' => &[
            0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000,
        ],
        '/' => &[
            0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000,
        ],
        '#' => &[
            0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010,
        ],
        '<' => &[
            0b00010, 0b00100, 0b01000, 0b10000, 0b01000, 0b00100, 0b00010,
        ],
        '>' => &[
            0b10000, 0b01000, 0b00100, 0b00010, 0b00100, 0b01000, 0b10000,
        ],
        '|' => &[
            0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        _ => &REPLACEMENT,
    }
}

/// Kindle Scribe panel geometry for host UI tests (1860x2480, stride 1872).
#[cfg(test)]
pub(crate) fn panel_geo() -> FbGeometry {
    FbGeometry {
        xres: 1860,
        yres: 2480,
        xres_virtual: 1872,
        yres_virtual: 2480,
        xoffset: 0,
        yoffset: 0,
        bits_per_pixel: 8,
        line_length: 1872,
        smem_len: 1872 * 2480,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_clips_and_never_touches_stride_padding() {
        let g = panel_geo();
        let mut fb = vec![0x55; g.smem_len as usize];
        {
            let mut c = Canvas::new(&mut fb, &g).unwrap();
            c.fill(Rect::new(1800, 2400, 500, 500), BLACK);
            c.text(1840, 10, 10, "WWWW", BLACK);
        }
        for y in 0..g.yres as usize {
            let row = &fb[y * 1872..(y + 1) * 1872];
            assert!(row[1860..].iter().all(|b| *b == 0x55));
        }
        assert_eq!(fb[2479 * 1872 + 1859], BLACK);
    }

    #[test]
    fn text_fitting_and_wrapping() {
        assert_eq!(text_width("", 3), 0);
        assert_eq!(text_width("A", 3), 15);
        assert_eq!(text_width("AB", 3), 33);
        let w = text_width("ABCDEFGHIJ", 4);
        assert_eq!(fit_text("ABCDEFGHIJ", 4, w), "ABCDEFGHIJ");
        let clipped = fit_text("ABCDEFGHIJ", 4, w - 1);
        assert!(clipped.ends_with("..."));
        assert!(text_width(&clipped, 4) < w);
        let tail = fit_text_tail("ABCDEFGHIJ", 4, w - 1);
        assert!(tail.starts_with("...") && tail.ends_with("HIJ"));
        let lines = wrap_text("The notebook will be removed from the library.", 7, 600, 4);
        assert!(lines.len() > 1);
        for l in &lines {
            assert!(text_width(l, 7) <= 600, "{l}");
        }
        let limited = wrap_text("aaa bbb ccc ddd eee fff ggg hhh", 7, 200, 2);
        assert_eq!(limited.len(), 2);
        assert!(limited[1].ends_with("..."));

        assert_eq!(scale_to_fit("A", 7, 3, 1000), 7);
        assert_eq!(scale_to_fit("", 7, 3, 0), 7);
        let mid_w = text_width("ABCDEFGHIJ", 4);
        assert_eq!(scale_to_fit("ABCDEFGHIJ", 7, 3, mid_w), 4);
        let oversized = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        assert_eq!(scale_to_fit(oversized, 7, 3, 100), 3);
        let clipped_min = fit_text(oversized, 3, 100);
        assert!(clipped_min.ends_with("..."));
        assert!(text_width(&clipped_min, 3) <= 100);
    }

    #[test]
    fn font_covers_title_keyboard_and_replaces_unknown() {
        for ch in ('a'..='z').chain('A'..='Z').chain('0'..='9') {
            assert!(has_glyph(ch), "{ch}");
        }
        for ch in "-_.'&!?,():;/#+\" ".chars() {
            assert!(has_glyph(ch), "{ch:?}");
        }
        assert!(!has_glyph('é'));
        assert_eq!(glyph_bits('é'), glyph_bits('?'));
        assert_ne!(glyph_bits('a'), glyph_bits('A'));
    }

    #[test]
    fn ui_point_uses_canonical_display_transform() {
        let p = UiPoint::from_page(PagePoint::new(8.4 * 100.5, 8.4 * 200.5)).unwrap();
        assert_eq!(p, UiPoint::new(100, 200));
        let far = UiPoint::from_page(PagePoint::new(15_624.0, 20_832.0)).unwrap();
        assert_eq!(far, UiPoint::new(1859, 2479));
    }

    #[test]
    fn paper_background_is_packed_white() {
        let g = panel_geo();
        let bg = paper_background(&g);
        assert_eq!(bg.len(), 1860 * 2480);
        assert!(bg.iter().all(|b| *b == WHITE));
    }
}
