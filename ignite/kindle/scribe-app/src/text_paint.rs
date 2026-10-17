//! Display-only raster of [`sync_protocol::SyncText`] onto the Kindle panel.
//!
//! Layout is computed in **canonical page units** from the embedded `default-sans-v1`
//! font, with monochrome Noto Emoji as a cmap fallback. Glyphs are then mapped with
//! the same [`ScribeDisplayTransform`] as ink. Parsed fonts are cached process-wide;
//! glyph bitmaps are produced while building a [`TextOverlay`] (page open / text
//! change), never on the pen path.

use std::path::Path;
use std::sync::OnceLock;

use fontdue::{Font, FontSettings, Metrics};
use kindle_platform::{DirtyRegion, FbGeometry};
use notebook_core::{PagePoint, ScribeDisplayTransform, CANONICAL_HEIGHT, CANONICAL_WIDTH};
use sync_protocol::{SyncText, TextFace};

/// Canonical face bytes — the repository file, not a Kindle/system font.
const DEFAULT_SANS_V1_TTF: &[u8] = include_bytes!("../../../assets/typography/default-sans-v1.ttf");
/// Official Google Fonts monochrome Noto Emoji (variable; default Regular instance).
const NOTO_EMOJI_TTF: &[u8] =
    include_bytes!("../../../assets/typography/noto-emoji/NotoEmoji[wght].ttf");
const _: &str = include_str!("../../../licenses/noto-emoji/OFL.txt");

/// Line height as a multiple of `font_size` (CSS `line-height: 1.2`).
pub const LINE_HEIGHT_EM: f32 = 1.2;

/// Reference px used only to read linear advances (`advance / REF * font_size`).
const METRIC_PX: f32 = 100.0;

static PRIMARY: OnceLock<Font> = OnceLock::new();
static EMOJI: OnceLock<Font> = OnceLock::new();

fn primary_face() -> &'static Font {
    PRIMARY.get_or_init(|| {
        Font::from_bytes(DEFAULT_SANS_V1_TTF, FontSettings::default())
            .expect("default-sans-v1.ttf must parse")
    })
}

fn emoji_face() -> &'static Font {
    EMOJI.get_or_init(|| {
        Font::from_bytes(NOTO_EMOJI_TTF, FontSettings::default())
            .expect("NotoEmoji[wght].ttf must parse")
    })
}

fn face() -> &'static Font {
    primary_face()
}

fn is_variation_selector(ch: char) -> bool {
    matches!(ch as u32, 0xFE00..=0xFE0F | 0xE0100..=0xE01EF)
}

struct GlyphChoice {
    font: &'static Font,
    index: u16,
}

fn cmap_index(font: &Font, ch: char) -> Option<u16> {
    let idx = font.lookup_glyph_index(ch);
    if idx != 0 || ch == '\0' {
        Some(idx)
    } else {
        None
    }
}

/// Pick the face and glyph for one scalar. `None` = skip (zero advance, no paint).
///
/// `next_is_emoji_vs` is true when the following scalar is U+FE0F: prefer Noto Emoji
/// for the base so `❤️` is one heart, not a text heart plus a `.notdef` box.
fn resolve_scalar(ch: char, next_is_emoji_vs: bool) -> Option<GlyphChoice> {
    if is_variation_selector(ch) {
        return None;
    }
    let primary = primary_face();
    let emoji = emoji_face();
    if next_is_emoji_vs {
        if let Some(index) = cmap_index(emoji, ch) {
            return Some(GlyphChoice { font: emoji, index });
        }
        return Some(GlyphChoice {
            font: primary,
            index: cmap_index(primary, ch).unwrap_or(0),
        });
    }
    if let Some(index) = cmap_index(primary, ch) {
        return Some(GlyphChoice {
            font: primary,
            index,
        });
    }
    if let Some(index) = cmap_index(emoji, ch) {
        return Some(GlyphChoice { font: emoji, index });
    }
    Some(GlyphChoice {
        font: primary,
        index: 0,
    })
}

fn next_is_emoji_vs(chars: &[char], i: usize) -> bool {
    chars.get(i + 1).copied() == Some('\u{FE0F}')
}

/// Coverage plane for the visible panel (`xres * yres`, no stride). 0 = transparent.
#[derive(Clone, Debug)]
pub struct TextOverlay {
    pub xres: u32,
    pub yres: u32,
    pub coverage: Vec<u8>,
}

impl TextOverlay {
    pub fn empty() -> Self {
        Self {
            xres: 0,
            yres: 0,
            coverage: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.coverage.iter().all(|&c| c == 0) || self.xres == 0
    }

    /// Layout + raster every object in list order (later objects overwrite).
    pub fn prepare(
        texts: &[SyncText],
        geo: &FbGeometry,
        transform: ScribeDisplayTransform,
    ) -> Self {
        overlay_from_texts(texts, geo, transform)
    }
}

/// Display cache derived from retained texts. Bind and inbound must assign the overlay
/// from the same slice stored on the controller — never leave a previous page's raster.
pub fn overlay_from_texts(
    texts: &[SyncText],
    geo: &FbGeometry,
    transform: ScribeDisplayTransform,
) -> TextOverlay {
    let n = (geo.xres as usize).saturating_mul(geo.yres as usize);
    let mut overlay = TextOverlay {
        xres: geo.xres,
        yres: geo.yres,
        coverage: vec![0u8; n],
    };
    for t in texts {
        paint_text(&mut overlay, t, transform);
    }
    overlay
}

/// Load `texts.jsonl` and derive the overlay from that vec. The pair is the Phase 1 contract.
pub fn load_texts_and_overlay(
    page_dir: &Path,
    geo: &FbGeometry,
    transform: ScribeDisplayTransform,
) -> Result<(Vec<SyncText>, TextOverlay), String> {
    let texts = crate::store::load_page_texts(page_dir)?;
    let overlay = overlay_from_texts(&texts, geo, transform);
    Ok((texts, overlay))
}

fn glyph_advance(choice: &GlyphChoice, font_size: f32) -> f32 {
    let m = choice.font.metrics_indexed(choice.index, METRIC_PX);
    (m.advance_width / METRIC_PX) * font_size
}

fn advance_canonical(ch: char, next_is_emoji_vs: bool, font_size: f32) -> f32 {
    match resolve_scalar(ch, next_is_emoji_vs) {
        None => 0.0,
        Some(choice) => glyph_advance(&choice, font_size),
    }
}

fn line_metrics_px(font: &Font, px: f32) -> (f32, f32) {
    match font.horizontal_line_metrics(px) {
        Some(lm) => (lm.ascent, LINE_HEIGHT_EM * px),
        None => (0.8 * px, LINE_HEIGHT_EM * px),
    }
}

/// Canonical layout-box height using the same wrap as Kindle paint.
///
/// Height is `line_count * LINE_HEIGHT_EM * font_size` with at least one line.
/// This is the text *box*, not the ink AABB of individual glyphs (descenders may
/// extend slightly past the box; the paint path still clips to the page).
pub fn text_layout_height(text: &SyncText) -> f32 {
    let n = wrap_lines(&text.content, text.font_size, text.width)
        .len()
        .max(1);
    n as f32 * LINE_HEIGHT_EM * text.font_size
}

/// True when `(x, y)` lies in the canonical layout box (`x..x+width`, `y..y+height`).
pub fn point_in_text_layout(text: &SyncText, x: f32, y: f32) -> bool {
    let h = text_layout_height(text);
    x >= text.x && x < text.x + text.width && y >= text.y && y < text.y + h
}

/// Wrap `content` into lines that each measure `<= width` in canonical units.
pub fn wrap_lines(content: &str, font_size: f32, width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for para in content.split('\n') {
        wrap_paragraph(para, font_size, width, &mut lines);
    }
    lines
}

fn wrap_paragraph(para: &str, font_size: f32, width: f32, out: &mut Vec<String>) {
    if para.is_empty() {
        out.push(String::new());
        return;
    }
    let chars: Vec<char> = para.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let mut end = i;
        let mut last_break: Option<usize> = None;
        let mut w = 0.0f32;
        while end < chars.len() {
            let ch = chars[end];
            let adv = advance_canonical(ch, next_is_emoji_vs(&chars, end), font_size);
            if w + adv > width && end > i {
                break;
            }
            w += adv;
            end += 1;
            if ch == ' ' {
                last_break = Some(end);
            }
        }
        if end == i {
            // Single scalar wider than the box: still take it (paint will clip).
            end = i + 1;
        } else if end < chars.len() {
            if let Some(b) = last_break {
                if b > i {
                    end = b;
                }
            }
        }
        out.push(chars[i..end].iter().collect());
        i = end;
    }
}

fn paint_text(overlay: &mut TextOverlay, text: &SyncText, transform: ScribeDisplayTransform) {
    if text.face != TextFace::DefaultSansV1 {
        return;
    }
    let font = face();
    let vp = transform.viewport();
    let px = (text.font_size * vp.scale_x).max(1.0);
    let (ascent_px, line_height_px) = line_metrics_px(font, px);
    let half_leading = (line_height_px - px) * 0.5;
    let lines = wrap_lines(&text.content, text.font_size, text.width);
    let clip_left = transform
        .to_display(PagePoint::new(text.x, text.y))
        .x
        .floor() as i32;
    let clip_right = transform
        .to_display(PagePoint::new(
            (text.x + text.width).min(CANONICAL_WIDTH),
            text.y,
        ))
        .x
        .floor() as i32;
    let page_bottom = overlay.yres as i32;

    for (li, line) in lines.iter().enumerate() {
        let line_top_c = text.y + (li as f32) * (LINE_HEIGHT_EM * text.font_size);
        if line_top_c >= CANONICAL_HEIGHT {
            break;
        }
        // CSS half-leading + font ascent, converted canonical→panel with the ink scale.
        let baseline_c = line_top_c + (half_leading + ascent_px) / vp.scale_x;
        let mut pen_x = text.x;
        let line_chars: Vec<char> = line.chars().collect();
        for (ci, &ch) in line_chars.iter().enumerate() {
            let Some(choice) = resolve_scalar(ch, next_is_emoji_vs(&line_chars, ci)) else {
                continue;
            };
            let (metrics, bitmap) = choice.font.rasterize_indexed(choice.index, px);
            blit_glyph(
                overlay,
                transform,
                pen_x,
                baseline_c,
                &metrics,
                &bitmap,
                GlyphClip {
                    left: clip_left,
                    right: clip_right.max(clip_left + 1),
                    bottom: page_bottom,
                },
            );
            pen_x += glyph_advance(&choice, text.font_size);
        }
    }
}

struct GlyphClip {
    left: i32,
    right: i32,
    bottom: i32,
}

fn blit_glyph(
    overlay: &mut TextOverlay,
    transform: ScribeDisplayTransform,
    pen_x: f32,
    baseline_c: f32,
    metrics: &Metrics,
    bitmap: &[u8],
    clip: GlyphClip,
) {
    let origin = transform.to_display(PagePoint::new(pen_x, baseline_c));
    if !origin.x.is_finite() || !origin.y.is_finite() {
        return;
    }
    let dest_x0 = origin.x.floor() as i32 + metrics.xmin;
    // `ymin` is the y-up bottom of the outline; the panel is +Y down, matching
    // fontdue's PositiveYDown layout: top = -height - ymin.
    let dest_y0 =
        origin.y.floor() as i32 + (-metrics.bounds.height - metrics.bounds.ymin).floor() as i32;
    let w = metrics.width as i32;
    let h = metrics.height as i32;
    for gy in 0..h {
        let py = dest_y0 + gy;
        if py < 0 || py >= clip.bottom || py >= overlay.yres as i32 {
            continue;
        }
        for gx in 0..w {
            let px = dest_x0 + gx;
            if px < clip.left || px >= clip.right || px < 0 || px >= overlay.xres as i32 {
                continue;
            }
            let cov = bitmap[(gy as usize) * (metrics.width) + gx as usize];
            if cov == 0 {
                continue;
            }
            let i = (py as u32 * overlay.xres + px as u32) as usize;
            if i < overlay.coverage.len() {
                overlay.coverage[i] = overlay.coverage[i].max(cov);
            }
        }
    }
}

/// Composite overlay coverage onto an 8bpp framebuffer (darken toward ink 0x00).
/// `clip` limits the work to a dirty rectangle (idle GC16 / wet-region restore).
pub fn blit_overlay(
    fb: &mut [u8],
    geo: &FbGeometry,
    overlay: &TextOverlay,
    clip: Option<DirtyRegion>,
) {
    if overlay.xres != geo.xres || overlay.yres != geo.yres {
        return;
    }
    let (x0, y0, x1, y1) = match clip {
        None => (0u32, 0u32, geo.xres, geo.yres),
        Some(d) => match d.clamp_to(geo.xres, geo.yres) {
            None => return,
            Some(c) => (
                c.x as u32,
                c.y as u32,
                c.x as u32 + c.width,
                c.y as u32 + c.height,
            ),
        },
    };
    for y in y0..y1 {
        for x in x0..x1 {
            let cov = overlay.coverage[(y * overlay.xres + x) as usize];
            if cov == 0 {
                continue;
            }
            let Some(off) = geo.byte_offset_visible_8bpp(x, y) else {
                continue;
            };
            if off >= fb.len() {
                continue;
            }
            let ink = 0u16;
            let paper = fb[off] as u16;
            fb[off] = ((paper * (255 - cov as u16) + ink * cov as u16) / 255) as u8;
        }
    }
}

/// Raster durable texts into a packed 8bpp buffer at `dest_w`×`dest_h`.
///
/// Wrap, line height, and baseline match [`paint_text`]. Glyph size is
/// `font_size` scaled by `dest_w / canonical width` (thumbnail scale when the
/// buffer is a page thumbnail, panel scale when it is the full page).
pub fn paint_texts_8bpp(dest: &mut [u8], dest_w: u32, dest_h: u32, texts: &[SyncText]) {
    let need = (dest_w as usize).saturating_mul(dest_h as usize);
    if dest_w == 0 || dest_h == 0 || dest.len() < need || texts.is_empty() {
        return;
    }
    let scale_x = dest_w as f32 / CANONICAL_WIDTH;
    let scale_y = dest_h as f32 / CANONICAL_HEIGHT;
    if scale_x <= 0.0 || scale_y <= 0.0 {
        return;
    }
    let font = face();
    for text in texts {
        if text.face != TextFace::DefaultSansV1 {
            continue;
        }
        let px = (text.font_size * scale_x).max(1.0);
        let (ascent_px, line_height_px) = line_metrics_px(font, px);
        let half_leading = (line_height_px - px) * 0.5;
        let lines = wrap_lines(&text.content, text.font_size, text.width);
        let clip_left = (text.x * scale_x).floor() as i32;
        let clip_right = ((text.x + text.width).min(CANONICAL_WIDTH) * scale_x).floor() as i32;
        let place = ScaledText {
            dest_w,
            dest_h,
            scale_x,
            scale_y,
            clip_left,
            clip_right: clip_right.max(clip_left + 1),
        };
        for (li, line) in lines.iter().enumerate() {
            let line_top_c = text.y + (li as f32) * (LINE_HEIGHT_EM * text.font_size);
            if line_top_c >= CANONICAL_HEIGHT {
                break;
            }
            let baseline_c = line_top_c + (half_leading + ascent_px) / scale_x;
            let mut pen_x = text.x;
            let line_chars: Vec<char> = line.chars().collect();
            for (ci, &ch) in line_chars.iter().enumerate() {
                let Some(choice) = resolve_scalar(ch, next_is_emoji_vs(&line_chars, ci)) else {
                    continue;
                };
                let (metrics, bitmap) = choice.font.rasterize_indexed(choice.index, px);
                blit_glyph_8bpp(dest, &place, pen_x, baseline_c, &metrics, &bitmap);
                pen_x += glyph_advance(&choice, text.font_size);
            }
        }
    }
}

struct ScaledText {
    dest_w: u32,
    dest_h: u32,
    scale_x: f32,
    scale_y: f32,
    clip_left: i32,
    clip_right: i32,
}

fn blit_glyph_8bpp(
    dest: &mut [u8],
    place: &ScaledText,
    pen_x: f32,
    baseline_c: f32,
    metrics: &Metrics,
    bitmap: &[u8],
) {
    let origin_x = pen_x * place.scale_x;
    let origin_y = baseline_c * place.scale_y;
    if !origin_x.is_finite() || !origin_y.is_finite() {
        return;
    }
    let dest_x0 = origin_x.floor() as i32 + metrics.xmin;
    let dest_y0 =
        origin_y.floor() as i32 + (-metrics.bounds.height - metrics.bounds.ymin).floor() as i32;
    let w = metrics.width as i32;
    let h = metrics.height as i32;
    for gy in 0..h {
        let py = dest_y0 + gy;
        if py < 0 || py >= place.dest_h as i32 {
            continue;
        }
        for gx in 0..w {
            let px = dest_x0 + gx;
            if px < place.clip_left || px >= place.clip_right || px < 0 || px >= place.dest_w as i32
            {
                continue;
            }
            let cov = bitmap[(gy as usize) * metrics.width + gx as usize];
            if cov == 0 {
                continue;
            }
            let i = (py as u32 * place.dest_w + px as u32) as usize;
            if i < dest.len() {
                let paper = dest[i] as u16;
                dest[i] = ((paper * (255 - cov as u16)) / 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fullscreen::{compose_notebook_layers, recompose_dirty_notebook_layers};
    use crate::profile::RenderProfile;
    use crate::render::raster_strokes_into;
    use crate::ui_gfx::WHITE;
    use notebook_core::{new_stroke_id, Point, Stroke};
    use sync_protocol::{sha256_hex, TextFace, TextId};
    use uuid::Uuid;

    fn geo() -> FbGeometry {
        FbGeometry {
            xres: 1860,
            yres: 2480,
            xres_virtual: 1860,
            yres_virtual: 2480,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 1860,
            smem_len: 1860 * 2480,
        }
    }

    fn tid(n: u8) -> TextId {
        let mut b = [0u8; 16];
        b[0] = 0x01;
        b[6] = 0x70;
        b[8] = 0x80;
        b[15] = n;
        TextId::from_uuid(Uuid::from_bytes(b)).unwrap()
    }

    fn sample(content: &str, x: f32, y: f32, width: f32) -> SyncText {
        SyncText {
            text_id: tid(1),
            x,
            y,
            width,
            font_size: 340.0,
            face: TextFace::DefaultSansV1,
            content: content.into(),
        }
    }

    #[test]
    fn font_parses_once() {
        let a = face() as *const Font;
        let b = face() as *const Font;
        assert_eq!(a, b);
        assert!(face().lookup_glyph_index('A') != 0);
        let e = emoji_face() as *const Font;
        assert_eq!(e, emoji_face() as *const Font);
        assert!(emoji_face().lookup_glyph_index('😅') != 0);
    }

    #[test]
    fn wrap_preserves_spaces_and_newlines() {
        let lines = wrap_lines("  hi  \nthere", 340.0, 5000.0);
        assert_eq!(lines, ["  hi  ", "there"]);
    }

    #[test]
    fn wrap_breaks_overlong_run() {
        let lines = wrap_lines("MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM", 340.0, 800.0);
        assert!(lines.len() > 1, "expected a break, got {lines:?}");
        for l in &lines {
            let chars: Vec<char> = l.chars().collect();
            let w: f32 = chars
                .iter()
                .enumerate()
                .map(|(i, &c)| advance_canonical(c, next_is_emoji_vs(&chars, i), 340.0))
                .sum();
            assert!(
                w <= 800.0 + 1.0 || l.chars().count() == 1,
                "line {l:?} w={w}"
            );
        }
    }

    #[test]
    fn wrap_at_spaces() {
        let lines = wrap_lines("hello world friends", 340.0, 1400.0);
        assert!(lines.len() >= 2);
        assert!(lines.iter().any(|l| l.contains("hello")));
    }

    #[test]
    fn wrap_single_line_fits() {
        assert_eq!(wrap_lines("Hello", 340.0, 5000.0), ["Hello"]);
    }

    #[test]
    fn prepare_overlay_does_not_mutate_text_objects() {
        let t = sample("Hello", 100.0, 200.0, 5000.0);
        let original = t.clone();
        let g = geo();
        let _overlay = TextOverlay::prepare(
            std::slice::from_ref(&t),
            &g,
            ScribeDisplayTransform::PortraitIdentity,
        );
        assert_eq!(t, original);
        assert_eq!(t.text_id, original.text_id);
        assert_eq!(t.content, original.content);
        assert_eq!(t.x, original.x);
        assert_eq!(t.y, original.y);
        assert_eq!(t.width, original.width);
        assert_eq!(t.font_size, original.font_size);
        assert_eq!(t.face, original.face);
    }

    #[test]
    fn overlay_from_updated_texts_is_not_the_stale_raster() {
        let g = geo();
        let xf = ScribeDisplayTransform::PortraitIdentity;
        let old = sample("OldCopy", 100.0, 200.0, 5000.0);
        let new = sample("NewCopy", 100.0, 200.0, 5000.0);
        let stale = overlay_from_texts(std::slice::from_ref(&old), &g, xf);
        let retained = vec![new.clone()];
        let live = overlay_from_texts(&retained, &g, xf);
        assert_ne!(stale.coverage, live.coverage);
        assert_eq!(
            live.coverage,
            overlay_from_texts(std::slice::from_ref(&new), &g, xf).coverage
        );
    }

    #[test]
    fn load_texts_and_overlay_pairs_file_bytes_with_derived_cache() {
        let dir = std::env::temp_dir().join(format!(
            "scribe-text-overlay-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let old = sample("OldCopy", 100.0, 200.0, 5000.0);
        let new = sample("NewCopy", 100.0, 200.0, 5000.0);
        crate::store::write_page_texts(&dir, std::slice::from_ref(&old)).unwrap();
        let g = geo();
        let xf = ScribeDisplayTransform::PortraitIdentity;
        let (first, overlay_old) = load_texts_and_overlay(&dir, &g, xf).unwrap();
        assert_eq!(first, vec![old.clone()]);
        crate::store::write_page_texts(&dir, std::slice::from_ref(&new)).unwrap();
        let (retained, overlay_new) = load_texts_and_overlay(&dir, &g, xf).unwrap();
        assert_eq!(retained, vec![new.clone()]);
        assert_eq!(
            overlay_new.coverage,
            overlay_from_texts(&retained, &g, xf).coverage
        );
        assert_ne!(
            overlay_old.coverage, overlay_new.coverage,
            "inbound-style file replace must not leave the previous overlay"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn layout_height_matches_wrap_line_count() {
        let t = sample("Hello", 100.0, 200.0, 5000.0);
        assert_eq!(wrap_lines(&t.content, t.font_size, t.width).len(), 1);
        assert_eq!(text_layout_height(&t), LINE_HEIGHT_EM * t.font_size);
        assert!(point_in_text_layout(&t, t.x, t.y));
        assert!(!point_in_text_layout(&t, t.x - 1.0, t.y));
    }

    #[test]
    fn missing_cmap_uses_notdef_not_skip() {
        let g = geo();
        let t = ScribeDisplayTransform::PortraitIdentity;
        let overlay = TextOverlay::prepare(&[sample("\u{FFFF}", 100.0, 200.0, 5000.0)], &g, t);
        assert!(
            overlay.coverage.iter().any(|&c| c > 0),
            "unsupported scalar must paint canonical .notdef"
        );
    }

    fn last_ink_row(overlay: &TextOverlay) -> Option<u32> {
        overlay
            .coverage
            .iter()
            .enumerate()
            .rev()
            .find(|(_, &c)| c > 32)
            .map(|(i, _)| i as u32 / overlay.xres)
    }

    #[test]
    fn latin_glyphs_share_a_baseline() {
        let g = geo();
        let t = ScribeDisplayTransform::PortraitIdentity;
        let h = TextOverlay::prepare(&[sample("H", 100.0, 200.0, 5000.0)], &g, t);
        let e = TextOverlay::prepare(&[sample("e", 100.0, 200.0, 5000.0)], &g, t);
        let gy = TextOverlay::prepare(&[sample("g", 100.0, 200.0, 5000.0)], &g, t);
        let h_bot = last_ink_row(&h).expect("H");
        let e_bot = last_ink_row(&e).expect("e");
        let g_bot = last_ink_row(&gy).expect("g");
        assert!(
            (h_bot as i32 - e_bot as i32).abs() <= 2,
            "H and e must share a baseline; H bottom={h_bot} e bottom={e_bot}"
        );
        assert!(
            g_bot > h_bot + 3,
            "g descender must sit below H; H bottom={h_bot} g bottom={g_bot}"
        );
    }

    #[test]
    fn fixture_hello_overlay_hash() {
        let g = geo();
        let t = ScribeDisplayTransform::PortraitIdentity;
        let overlay = TextOverlay::prepare(&[sample("Hello", 100.0, 200.0, 5000.0)], &g, t);
        let hex = sha256_hex(&overlay.coverage).to_string();
        assert_eq!(
            hex, "47817e65740e69d1d6203b4452caa6c24b243b2da6d8dc6faebd0686f51b4ba6",
            "layout/raster drifted; if intentional, update this pin. got {hex}"
        );
    }

    #[test]
    fn right_edge_does_not_paint_past_width() {
        let g = geo();
        let t = ScribeDisplayTransform::PortraitIdentity;
        let text = sample("HelloWorldHelloWorld", 15000.0, 100.0, 200.0);
        let overlay = TextOverlay::prepare(std::slice::from_ref(&text), &g, t);
        let right = t
            .to_display(PagePoint::new(text.x + text.width, text.y))
            .x
            .floor() as u32;
        for y in 0..g.yres {
            for x in right..g.xres {
                assert_eq!(
                    overlay.coverage[(y * g.xres + x) as usize],
                    0,
                    "painted past wrap width at {x},{y}"
                );
            }
        }
    }

    #[test]
    fn bottom_edge_clips_to_page() {
        let g = geo();
        let t = ScribeDisplayTransform::PortraitIdentity;
        let overlay = TextOverlay::prepare(
            &[sample(
                "Hello\nWorld\nMore\nLines\nHere",
                100.0,
                20000.0,
                5000.0,
            )],
            &g,
            t,
        );
        // Last rows of the panel may have pixels, but we must not have written past yres
        // (vec length is the assertion). Some coverage near the bottom is expected.
        assert_eq!(overlay.coverage.len(), (g.xres * g.yres) as usize);
        let any = overlay.coverage.iter().any(|&c| c > 0);
        assert!(any);
    }

    #[test]
    fn later_object_paints_over_earlier() {
        let g = geo();
        let t = ScribeDisplayTransform::PortraitIdentity;
        let a = sample("A", 100.0, 200.0, 5000.0);
        let mut b = sample("B", 100.0, 200.0, 5000.0);
        b.text_id = tid(2);
        let one = TextOverlay::prepare(std::slice::from_ref(&a), &g, t);
        let two = TextOverlay::prepare(&[a, b], &g, t);
        assert_ne!(one.coverage, two.coverage);
    }

    #[test]
    fn text_wins_over_stroke_after_recompose() {
        let g = geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let mut stroke = Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(100.0, 220.0), Point::new(800.0, 220.0)],
            brush: None,
            finished: true,
        };
        stroke.points[0].pressure = Some(0.5);
        stroke.points[1].pressure = Some(0.5);
        let mut fb = vec![0xFFu8; g.smem_len as usize];
        raster_strokes_into(&mut fb, &g, &profile, std::slice::from_ref(&stroke)).unwrap();
        let overlay = TextOverlay::prepare(
            &[sample("Hello", 100.0, 200.0, 5000.0)],
            &g,
            ScribeDisplayTransform::PortraitIdentity,
        );
        blit_overlay(&mut fb, &g, &overlay, None);
        let mut text_on_stroke = 0u32;
        for (i, &c) in overlay.coverage.iter().enumerate() {
            if c == 0 {
                continue;
            }
            let x = (i as u32) % g.xres;
            let y = (i as u32) / g.xres;
            let off = g.byte_offset_visible_8bpp(x, y).unwrap();
            if fb[off] < 0x80 {
                text_on_stroke += 1;
            }
        }
        assert!(
            text_on_stroke > 10,
            "text should darken pixels over the stroke"
        );

        let mut g2 = vec![0u8; fb.len()];
        kindle_platform::copy_logical_rect_to_hw(
            &fb,
            &mut g2,
            g.line_length,
            0,
            0,
            g.xres,
            g.yres,
            false,
        )
        .unwrap();
        assert_eq!(g2, fb);
        let mut g1 = vec![0u8; fb.len()];
        kindle_platform::copy_logical_rect_to_hw(
            &fb,
            &mut g1,
            g.line_length,
            0,
            0,
            g.xres,
            g.yres,
            true,
        )
        .unwrap();
        for (h, l) in g1.iter().zip(fb.iter()) {
            assert_eq!(*h, !*l);
        }
    }

    #[test]
    fn dirty_region_blit_restores_text_after_ink() {
        let g = geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let overlay = TextOverlay::prepare(
            &[sample("Hello", 100.0, 200.0, 5000.0)],
            &g,
            ScribeDisplayTransform::PortraitIdentity,
        );
        let mut fb = vec![0xFFu8; g.smem_len as usize];
        blit_overlay(&mut fb, &g, &overlay, None);
        let mut covered = Vec::new();
        for (i, &c) in overlay.coverage.iter().enumerate() {
            if c > 40 {
                let x = (i as u32) % g.xres;
                let y = (i as u32) / g.xres;
                covered.push((x, y));
            }
        }
        assert!(!covered.is_empty());
        let (tx, ty) = covered[covered.len() / 2];
        if let Some(off) = g.byte_offset_visible_8bpp(tx, ty) {
            fb[off] = 0x00; // wet ink stamped over a text pixel
        }
        let stroke = Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(100.0, 220.0), Point::new(400.0, 220.0)],
            brush: None,
            finished: true,
        };
        raster_strokes_into(&mut fb, &g, &profile, &[stroke]).unwrap();
        blit_overlay(
            &mut fb,
            &g,
            &overlay,
            Some(DirtyRegion::new(0, 0, g.xres, g.yres)),
        );
        let off = g.byte_offset_visible_8bpp(tx, ty).unwrap();
        assert!(
            fb[off] < 0xE0,
            "text coverage must be restored after ink dirty blit, pixel={}",
            fb[off]
        );
    }

    #[test]
    fn compose_layers_paints_text_after_strokes() {
        let g = geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let overlay = TextOverlay::prepare(
            &[sample("Hello", 100.0, 200.0, 5000.0)],
            &g,
            ScribeDisplayTransform::PortraitIdentity,
        );
        let mut stroke = Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(100.0, 220.0), Point::new(800.0, 220.0)],
            brush: None,
            finished: true,
        };
        stroke.points[0].pressure = Some(0.5);
        stroke.points[1].pressure = Some(0.5);
        let mut fb = vec![WHITE; g.smem_len as usize];
        compose_notebook_layers(
            &mut fb,
            &g,
            &profile,
            std::slice::from_ref(&stroke),
            Some(&overlay),
            &[],
            None,
        )
        .unwrap();
        let mut hits = 0u32;
        for (i, &c) in overlay.coverage.iter().enumerate() {
            if c == 0 {
                continue;
            }
            let x = (i as u32) % g.xres;
            let y = (i as u32) / g.xres;
            let off = g.byte_offset_visible_8bpp(x, y).unwrap();
            if fb[off] < 0xE0 {
                hits += 1;
            }
        }
        assert!(hits > 10);
    }

    #[test]
    fn erase_restore_dirty_region_keeps_text() {
        let g = geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let overlay = TextOverlay::prepare(
            &[sample("Hello", 100.0, 200.0, 5000.0)],
            &g,
            ScribeDisplayTransform::PortraitIdentity,
        );
        let mut stroke = Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(100.0, 220.0), Point::new(800.0, 220.0)],
            brush: None,
            finished: true,
        };
        stroke.points[0].pressure = Some(0.5);
        stroke.points[1].pressure = Some(0.5);
        let mut fb = vec![WHITE; g.smem_len as usize];
        compose_notebook_layers(
            &mut fb,
            &g,
            &profile,
            std::slice::from_ref(&stroke),
            Some(&overlay),
            &[],
            None,
        )
        .unwrap();
        // Simulate an eraser restore of a dirty rectangle: paper + remaining strokes (none),
        // then the same prepared overlay — text must not vanish.
        let dirty = DirtyRegion::new(0, 0, 400, 200);
        for y in dirty.y.max(0) as u32..(dirty.y as u32).saturating_add(dirty.height).min(g.yres) {
            for x in dirty.x.max(0) as u32..(dirty.x as u32).saturating_add(dirty.width).min(g.xres)
            {
                if let Some(off) = g.byte_offset_visible_8bpp(x, y) {
                    fb[off] = WHITE;
                }
            }
        }
        blit_overlay(&mut fb, &g, &overlay, Some(dirty));
        let mut restored = 0u32;
        for y in dirty.y.max(0) as u32..(dirty.y as u32).saturating_add(dirty.height).min(g.yres) {
            for x in dirty.x.max(0) as u32..(dirty.x as u32).saturating_add(dirty.width).min(g.xres)
            {
                let i = (y * overlay.xres + x) as usize;
                if overlay.coverage[i] > 40 {
                    let off = g.byte_offset_visible_8bpp(x, y).unwrap();
                    if fb[off] < 0xE0 {
                        restored += 1;
                    }
                }
            }
        }
        assert!(
            restored > 0,
            "text pixels inside the eraser dirty rect must be restored"
        );
    }

    fn coverage_dirty(overlay: &TextOverlay) -> DirtyRegion {
        let mut min_x = overlay.xres;
        let mut min_y = overlay.yres;
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        for (i, &c) in overlay.coverage.iter().enumerate() {
            if c == 0 {
                continue;
            }
            let x = (i as u32) % overlay.xres;
            let y = (i as u32) / overlay.xres;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        assert!(max_x >= min_x, "overlay must contain coverage");
        DirtyRegion::new(
            min_x as i32,
            min_y as i32,
            max_x - min_x + 1,
            max_y - min_y + 1,
        )
    }

    fn region_bytes(fb: &[u8], geo: &FbGeometry, dirty: DirtyRegion) -> Vec<u8> {
        let x0 = dirty.x.max(0) as u32;
        let y0 = dirty.y.max(0) as u32;
        let x1 = x0.saturating_add(dirty.width).min(geo.xres);
        let y1 = y0.saturating_add(dirty.height).min(geo.yres);
        let mut out = Vec::new();
        for y in y0..y1 {
            for x in x0..x1 {
                let off = geo.byte_offset_visible_8bpp(x, y).unwrap();
                out.push(fb[off]);
            }
        }
        out
    }

    #[test]
    fn repeated_dirty_gc16_commits_leave_text_pixel_stable() {
        let g = geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let mut stroke = Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(100.0, 220.0), Point::new(800.0, 220.0)],
            brush: None,
            finished: true,
        };
        stroke.points[0].pressure = Some(0.5);
        stroke.points[1].pressure = Some(0.5);
        let overlay = TextOverlay::prepare(
            &[sample("Hello", 100.0, 200.0, 5000.0)],
            &g,
            ScribeDisplayTransform::PortraitIdentity,
        );
        let mut expected = vec![WHITE; g.smem_len as usize];
        compose_notebook_layers(
            &mut expected,
            &g,
            &profile,
            std::slice::from_ref(&stroke),
            Some(&overlay),
            &[],
            None,
        )
        .unwrap();
        let dirty = coverage_dirty(&overlay);
        let want = region_bytes(&expected, &g, dirty);
        let background = vec![WHITE; (g.xres * g.yres) as usize];
        let mut fb = expected.clone();
        for pass in 1..=5 {
            // Simulate wet ink through the same glyphs, then the idle GC16 path.
            for y in
                dirty.y.max(0) as u32..(dirty.y as u32).saturating_add(dirty.height).min(g.yres)
            {
                for x in
                    dirty.x.max(0) as u32..(dirty.x as u32).saturating_add(dirty.width).min(g.xres)
                {
                    if overlay.coverage[(y * overlay.xres + x) as usize] > 0 {
                        if let Some(off) = g.byte_offset_visible_8bpp(x, y) {
                            fb[off] = 0x00;
                        }
                    }
                }
            }
            recompose_dirty_notebook_layers(
                &mut fb,
                &g,
                &profile,
                &background,
                std::slice::from_ref(&stroke),
                Some(&overlay),
                &[],
                dirty,
            )
            .unwrap();
            let got = region_bytes(&fb, &g, dirty);
            assert_eq!(
                got, want,
                "idle GC16 commit {pass} drifted from canonical composition"
            );
        }
    }

    fn resolved_kind(ch: char, next_fe0f: bool) -> &'static str {
        match resolve_scalar(ch, next_fe0f) {
            None => "skip",
            Some(g) if g.index == 0 && ch != '\0' => "notdef",
            Some(g) if std::ptr::eq(g.font, emoji_face()) => "emoji",
            Some(_) => "primary",
        }
    }

    fn line_width(s: &str, font_size: f32) -> f32 {
        let chars: Vec<char> = s.chars().collect();
        chars
            .iter()
            .enumerate()
            .map(|(i, &c)| advance_canonical(c, next_is_emoji_vs(&chars, i), font_size))
            .sum()
    }

    fn ink_count(overlay: &TextOverlay) -> usize {
        overlay.coverage.iter().filter(|&&c| c > 32).count()
    }

    #[test]
    fn noto_emoji_parses_and_covers_grin() {
        assert!(emoji_face().lookup_glyph_index('😅') != 0);
        assert_eq!(resolved_kind('😅', false), "emoji");
    }

    #[test]
    fn ordinary_text_stays_on_primary() {
        for ch in "Plain ASCII text Café".chars() {
            assert_eq!(
                resolved_kind(ch, false),
                "primary",
                "{ch:?} must use default-sans-v1"
            );
        }
        assert_eq!(resolved_kind('→', false), "primary");
        assert_eq!(resolved_kind('✓', false), "primary");
        for ch in "★∞".chars() {
            let kind = resolved_kind(ch, false);
            assert!(
                kind == "primary" || kind == "emoji" || kind == "notdef",
                "{ch:?} resolved to {kind}"
            );
        }
    }

    #[test]
    fn grin_overlay_is_not_the_notdef_box() {
        let g = geo();
        let t = ScribeDisplayTransform::PortraitIdentity;
        let emoji = TextOverlay::prepare(&[sample("😅", 100.0, 200.0, 5000.0)], &g, t);
        let tofu = TextOverlay::prepare(&[sample("\u{FFFF}", 100.0, 200.0, 5000.0)], &g, t);
        assert!(ink_count(&emoji) > 0);
        assert!(ink_count(&tofu) > 0);
        assert_ne!(
            emoji.coverage, tofu.coverage,
            "😅 must not paint the primary .notdef"
        );
        assert!(
            ink_count(&emoji) > ink_count(&tofu),
            "emoji glyph should cover more than a missing-glyph box"
        );
    }

    #[test]
    fn mixed_hello_grin_world_layout_uses_emoji_advance() {
        let mixed = "Hello 😅 world";
        assert_eq!(wrap_lines(mixed, 340.0, 20000.0), [mixed]);
        let with = line_width(mixed, 340.0);
        let without = line_width("Hello  world", 340.0);
        let emoji_adv = advance_canonical('😅', false, 340.0);
        let notdef_adv = advance_canonical('\u{FFFF}', false, 340.0);
        assert!(emoji_adv > 0.0);
        assert_ne!(emoji_adv, notdef_adv);
        assert!((with - without - emoji_adv).abs() < 0.5);
        let t = sample(mixed, 100.0, 200.0, 5000.0);
        let original = t.clone();
        let _ = TextOverlay::prepare(
            std::slice::from_ref(&t),
            &geo(),
            ScribeDisplayTransform::PortraitIdentity,
        );
        assert_eq!(t.content, original.content);
        assert_eq!(t, original);
    }

    #[test]
    fn emoji_presentation_vs_is_skipped_not_tofu() {
        assert_eq!(resolved_kind('\u{FE0F}', false), "skip");
        assert_eq!(advance_canonical('\u{FE0F}', false, 340.0), 0.0);
        assert_eq!(resolved_kind('\u{2764}', true), "emoji");
        let g = geo();
        let xf = ScribeDisplayTransform::PortraitIdentity;
        let vs_only = TextOverlay::prepare(&[sample("\u{FE0F}", 100.0, 200.0, 5000.0)], &g, xf);
        assert_eq!(ink_count(&vs_only), 0, "variation selector must not paint");
        let heart =
            TextOverlay::prepare(&[sample("\u{2764}\u{FE0F}", 100.0, 200.0, 5000.0)], &g, xf);
        assert!(ink_count(&heart) > 0);
        let w = line_width("\u{2764}\u{FE0F}", 340.0);
        assert_eq!(w, advance_canonical('\u{2764}', true, 340.0));
    }

    #[test]
    fn fallback_corpus_is_not_missing() {
        for ch in "😀😂🙂😢😭👍👎👌🙏🔥⭐✨✅❌📌📅📝💡🚀☕🎉😅".chars()
        {
            let kind = resolved_kind(ch, false);
            assert!(
                kind == "primary" || kind == "emoji",
                "{ch} resolved to {kind}, expected a real glyph"
            );
        }
        assert_eq!(resolved_kind('😅', false), "emoji");
        assert_eq!(resolved_kind('⚠', true), "emoji");
    }

    #[test]
    fn license_and_font_assets_are_present() {
        // From ignite/kindle/scribe-app, ../.. is the inner ignite/ product root
        // (assets/, licenses/, VERSION) — same depth as the include_bytes!/include_str! embeds.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for rel in [
            "assets/typography/default-sans-v1.ttf",
            "assets/typography/noto-emoji/NotoEmoji[wght].ttf",
            "assets/typography/noto-emoji/README.md",
            "licenses/noto-emoji/OFL.txt",
            "licenses/noto-emoji/DESCRIPTION.en_us.html",
            "licenses/noto-emoji/upstream_info.md",
            "VERSION",
        ] {
            let p = root.join(rel);
            assert!(p.is_file(), "missing {rel} at {}", p.display());
        }
        let ofl = std::fs::read_to_string(root.join("licenses/noto-emoji/OFL.txt")).unwrap();
        assert!(ofl.contains("SIL Open Font License"));
        assert!(ofl.contains("Copyright 2013 Google LLC"));
        // Kindle launcher deploy scripts are not migrated in Slice 10. When present
        // (legacy monorepo layout), keep the packaging-path assertions.
        let deploy = root.join("tools/kindle_launcher/deploy.sh");
        if deploy.is_file() {
            let deploy = std::fs::read_to_string(&deploy).unwrap();
            assert!(deploy.contains("licenses/noto-emoji"));
            assert!(deploy.contains("/mnt/us/scribe-app/assets/licenses/noto-emoji"));
        }
    }
}
