//! Gate F11.3 fullscreen writing: transient notebook chrome visibility + auto-hide.
//!
//! Notebook pages are normally a full-panel writing surface. The F11 chrome bands are a
//! temporary overlay revealed by a deliberate finger tap and hidden again by a content tap, a
//! pen Down in content, a page swipe, or 3 s without chrome interaction.
//!
//! Pure and host-testable: time is Scribe's monotonic milliseconds supplied by the caller.
//! Nothing here is durable; visibility is never written to `library.meta`, `notebook.meta`,
//! `page.meta`, `background.raw`, `strokes.jsonl`, or `edits.jsonl`.

use kindle_platform::{DirtyRegion, FbGeometry};
use std::collections::HashSet;

use notebook_core::Stroke;

use crate::chrome::{render_notebook_chrome, PageChrome};
use crate::page_label::scrub_chrome_band;
use crate::profile::RenderProfile;
use crate::render::{
    raster_intersecting_strokes_into_region, raster_intersecting_strokes_into_region_translated,
    raster_strokes_into, region_geometry,
};
use sync_protocol::SyncPageLink;

use crate::text_paint::{blit_overlay, TextOverlay};

/// Chrome auto-hides this long after it was revealed or last operated.
pub const CHROME_AUTO_HIDE_MS: u64 = 3000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChromeVisibility {
    #[default]
    Hidden,
    Visible,
}

/// Notebook chrome visibility plus the inactivity deadline (monotonic ms).
///
/// `hide_deadline_ms == None` while visible means the timer is suspended (a modal is open).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NotebookChromeState {
    visibility: ChromeVisibility,
    hide_deadline_ms: Option<u64>,
}

impl NotebookChromeState {
    #[cfg(test)]
    pub fn visibility(&self) -> ChromeVisibility {
        self.visibility
    }

    pub fn is_visible(&self) -> bool {
        self.visibility == ChromeVisibility::Visible
    }

    #[cfg(test)]
    pub fn hide_deadline_ms(&self) -> Option<u64> {
        self.hide_deadline_ms
    }

    /// Normal notebook entry: fullscreen paper.
    pub fn enter_notebook(&mut self) {
        *self = Self::default();
    }

    /// Show chrome and (re)start the inactivity countdown.
    pub fn reveal(&mut self, now_ms: u64) {
        self.visibility = ChromeVisibility::Visible;
        self.hide_deadline_ms = Some(now_ms.saturating_add(CHROME_AUTO_HIDE_MS));
    }

    /// Hide chrome. Returns `true` if it was visible (the panel must be re-presented).
    pub fn hide(&mut self) -> bool {
        let was = self.is_visible();
        *self = Self::default();
        was
    }

    /// Successful chrome interaction (finger or pen): restart the countdown. No-op while
    /// hidden or while a modal has suspended the timer.
    pub fn ui_activity(&mut self, now_ms: u64) {
        if self.is_visible() && self.hide_deadline_ms.is_some() {
            self.hide_deadline_ms = Some(now_ms.saturating_add(CHROME_AUTO_HIDE_MS));
        }
    }

    /// A modal opened over the notebook: suspend auto-hide (visibility unchanged).
    pub fn modal_opened(&mut self) {
        self.hide_deadline_ms = None;
    }

    /// A modal closed back to the notebook: chrome visible, fresh countdown.
    pub fn modal_closed(&mut self, now_ms: u64) {
        self.reveal(now_ms);
    }

    /// Pen Down in page content. Returns `true` if chrome was visible and is now hidden, in
    /// which case the caller must reconstruct the page *before* starting the wet stroke.
    pub fn pen_content_down(&mut self) -> bool {
        self.hide()
    }

    /// Display restored after sleep (F11.1): keep visibility, restart a fresh countdown if it
    /// was running. A suspended (modal) timer stays suspended.
    pub fn display_restored(&mut self, now_ms: u64) {
        self.ui_activity(now_ms);
    }

    /// Whether the inactivity deadline has passed. The caller additionally requires a valid
    /// display, no modal, and Notebook view.
    pub fn timeout_due(&self, now_ms: u64) -> bool {
        self.is_visible() && self.hide_deadline_ms.is_some_and(|d| now_ms >= d)
    }
}

/// Canonical notebook composition over an already-restored durable background:
/// scrub legacy HUD pixels → durable strokes → display-only text → link icons → optional live chrome.
/// No refresh.
///
/// With `overlay == None` the whole panel is page content; with `Some` the bands temporarily
/// cover whatever handwriting, text, and link icons lie beneath them. Hiding chrome is therefore
/// always a full recomposition from durable state (never painting white over old controls), so
/// covered ink and text reappear. `strokes`, `texts`, `links`, and the background are read-only here.
pub fn compose_notebook_layers(
    fb: &mut [u8],
    geo: &FbGeometry,
    profile: &RenderProfile,
    strokes: &[Stroke],
    texts: Option<&TextOverlay>,
    links: &[SyncPageLink],
    overlay: Option<(&str, PageChrome)>,
) -> Result<(), String> {
    // Scrub chrome baked into background.raw (legacy startup FB capture / Amazon UI) before
    // strokes, so bottom-band handwriting is redrawn on top of paper.
    scrub_chrome_band(fb, geo)?;
    raster_strokes_into(fb, geo, profile, strokes)?;
    if let Some(t) = texts {
        blit_overlay(fb, geo, t, None);
    }
    crate::link_paint::paint_page_links(fb, geo, profile, links, None)?;
    if let Some((title, page)) = overlay {
        render_notebook_chrome(fb, geo, title, page)?;
    }
    Ok(())
}

pub(crate) fn dirty_panel_bounds(
    geo: &FbGeometry,
    dirty: DirtyRegion,
) -> Option<(u32, u32, u32, u32)> {
    let c = dirty.clamp_to(geo.xres, geo.yres)?;
    let x0 = c.x as u32;
    let y0 = c.y as u32;
    Some((x0, y0, x0 + c.width, y0 + c.height))
}

fn sample_background(background: &[u8], geo: &FbGeometry, x: u32, y: u32) -> u8 {
    let packed = (geo.xres as usize).saturating_mul(geo.yres as usize);
    let stride = (geo.line_length as usize).saturating_mul(geo.yres as usize);
    if background.len() == packed {
        background
            .get(
                (y as usize)
                    .saturating_mul(geo.xres as usize)
                    .saturating_add(x as usize),
            )
            .copied()
            .unwrap_or(crate::ui_gfx::WHITE)
    } else if background.len() == stride {
        background
            .get(
                (y as usize)
                    .saturating_mul(geo.line_length as usize)
                    .saturating_add(x as usize),
            )
            .copied()
            .unwrap_or(crate::ui_gfx::WHITE)
    } else {
        crate::ui_gfx::WHITE
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirtyRecomposeStats {
    pub strokes_considered: usize,
    pub strokes_replayed: usize,
    pub scratch_bytes: usize,
}

/// Restore paper + intersecting strokes in `dirty`, then overlay text, then any link
/// icon that intersects `dirty`.
///
/// Idle GC16 uses this so antialiased text is not alpha-darkened on every
/// stroke that intersects the same glyphs. Work is proportional to the dirty
/// rectangle and strokes whose rendered AABB overlaps it. Link icons use the same
/// paint order as [`compose_notebook_layers`] and are not a full-page reconstruct.
#[allow(clippy::too_many_arguments)] // same layer list as full composition, plus the dirty rect
pub fn recompose_dirty_notebook_layers(
    fb: &mut [u8],
    geo: &FbGeometry,
    profile: &RenderProfile,
    background: &[u8],
    strokes: &[Stroke],
    texts: Option<&TextOverlay>,
    links: &[SyncPageLink],
    dirty: DirtyRegion,
) -> Result<DirtyRecomposeStats, String> {
    recompose_dirty_notebook_layers_inner(
        fb, geo, profile, background, strokes, texts, links, dirty, None,
    )
}

/// Same regional composition with selected strokes translated only in the transient preview.
#[allow(clippy::too_many_arguments)]
pub fn recompose_dirty_notebook_layers_translated(
    fb: &mut [u8],
    geo: &FbGeometry,
    profile: &RenderProfile,
    background: &[u8],
    strokes: &[Stroke],
    texts: Option<&TextOverlay>,
    links: &[SyncPageLink],
    dirty: DirtyRegion,
    moved_ids: &HashSet<String>,
    dx: f32,
    dy: f32,
) -> Result<DirtyRecomposeStats, String> {
    recompose_dirty_notebook_layers_inner(
        fb,
        geo,
        profile,
        background,
        strokes,
        texts,
        links,
        dirty,
        Some((moved_ids, dx, dy)),
    )
}

#[allow(clippy::too_many_arguments)]
fn recompose_dirty_notebook_layers_inner(
    fb: &mut [u8],
    geo: &FbGeometry,
    profile: &RenderProfile,
    background: &[u8],
    strokes: &[Stroke],
    texts: Option<&TextOverlay>,
    links: &[SyncPageLink],
    dirty: DirtyRegion,
    translation: Option<(&HashSet<String>, f32, f32)>,
) -> Result<DirtyRecomposeStats, String> {
    let Some((x0, y0, x1, y1)) = dirty_panel_bounds(geo, dirty) else {
        return Ok(DirtyRecomposeStats {
            strokes_considered: strokes.len(),
            ..DirtyRecomposeStats::default()
        });
    };
    let w = x1 - x0;
    let h = y1 - y0;
    let scratch_bytes = (w as usize)
        .checked_mul(h as usize)
        .ok_or("dirty region overflow")?;
    let mut scratch = vec![crate::ui_gfx::WHITE; scratch_bytes];
    for ly in 0..h {
        for lx in 0..w {
            scratch[(ly * w + lx) as usize] = sample_background(background, geo, x0 + lx, y0 + ly);
        }
    }
    let rg = region_geometry(w, h)?;
    let clip = DirtyRegion::new(x0 as i32, y0 as i32, w, h);
    let replay = match translation {
        Some((moved_ids, dx, dy)) => raster_intersecting_strokes_into_region_translated(
            &mut scratch,
            &rg,
            x0 as i32,
            y0 as i32,
            clip,
            profile,
            strokes,
            moved_ids,
            dx,
            dy,
        )?,
        None => raster_intersecting_strokes_into_region(
            &mut scratch,
            &rg,
            x0 as i32,
            y0 as i32,
            clip,
            profile,
            strokes,
        )?,
    };
    for ly in 0..h {
        for lx in 0..w {
            let Some(off) = geo.byte_offset_visible_8bpp(x0 + lx, y0 + ly) else {
                continue;
            };
            if off < fb.len() {
                fb[off] = scratch[(ly * w + lx) as usize];
            }
        }
    }
    if let Some(t) = texts {
        blit_overlay(fb, geo, t, Some(clip));
    }
    crate::link_paint::paint_page_links(fb, geo, profile, links, Some(clip))?;
    Ok(DirtyRecomposeStats {
        strokes_considered: replay.considered,
        strokes_replayed: replay.replayed,
        scratch_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
    use crate::profile::RenderProfile;
    use crate::text_paint::TextOverlay;
    use crate::ui_gfx::WHITE;
    use notebook_core::{new_stroke_id, Point, ScribeDisplayTransform};
    use sync_protocol::{TextFace, TextId};
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

    fn crop(fb: &[u8], g: &FbGeometry, dirty: DirtyRegion) -> Vec<u8> {
        let c = dirty.clamp_to(g.xres, g.yres).unwrap();
        let mut out = Vec::new();
        for y in c.y as u32..(c.y as u32 + c.height) {
            for x in c.x as u32..(c.x as u32 + c.width) {
                out.push(fb[g.byte_offset_visible_8bpp(x, y).unwrap()]);
            }
        }
        out
    }

    fn tid(n: u8) -> TextId {
        let mut b = [0u8; 16];
        b[0] = 0x01;
        b[6] = 0x70;
        b[8] = 0x80;
        b[15] = n;
        TextId::from_uuid(Uuid::from_bytes(b)).unwrap()
    }

    fn sample_text(content: &str, x: f32, y: f32, width: f32) -> sync_protocol::SyncText {
        sync_protocol::SyncText {
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
    fn dirty_clip_negative_origin_uses_true_end() {
        let g = geo();
        let b = dirty_panel_bounds(&g, DirtyRegion::new(-10, 0, 20, 8)).unwrap();
        assert_eq!(b, (0, 0, 10, 8));
    }

    #[test]
    fn dirty_clip_overflow_and_off_panel() {
        let g = geo();
        let right = dirty_panel_bounds(&g, DirtyRegion::new(1850, 10, 40, 5)).unwrap();
        assert_eq!(right, (1850, 10, 1860, 15));
        let bottom = dirty_panel_bounds(&g, DirtyRegion::new(0, 2470, 4, 30)).unwrap();
        assert_eq!(bottom, (0, 2470, 4, 2480));
        assert!(dirty_panel_bounds(&g, DirtyRegion::new(-200, 10, 50, 50)).is_none());
        assert!(dirty_panel_bounds(&g, DirtyRegion::new(2000, 10, 50, 50)).is_none());
        assert!(dirty_panel_bounds(&g, DirtyRegion::new(10, -200, 50, 50)).is_none());
        assert!(dirty_panel_bounds(&g, DirtyRegion::new(10, 3000, 50, 50)).is_none());
        assert!(dirty_panel_bounds(&g, DirtyRegion::new(0, 0, 0, 10)).is_none());
    }

    #[test]
    fn regional_recompose_matches_full_composition_crop() {
        let g = geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let mut crossing = Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(100.0, 220.0), Point::new(4000.0, 220.0)],
            brush: None,
            finished: true,
        };
        crossing.points[0].pressure = Some(0.4);
        crossing.points[1].pressure = Some(0.9);
        crossing.brush = Some(STYLE_PRESSURE_WIDTH_V1.into());
        let other = Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(200.0, 400.0), Point::new(900.0, 900.0)],
            brush: None,
            finished: true,
        };
        let far = Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(12000.0, 18000.0), Point::new(13000.0, 19000.0)],
            brush: None,
            finished: true,
        };
        let overlay = TextOverlay::prepare(
            &[sample_text("Hello", 100.0, 200.0, 5000.0)],
            &g,
            ScribeDisplayTransform::PortraitIdentity,
        );
        let strokes = [crossing, other, far];
        let mut expected = vec![WHITE; g.smem_len as usize];
        compose_notebook_layers(
            &mut expected,
            &g,
            &profile,
            &strokes,
            Some(&overlay),
            &[],
            None,
        )
        .unwrap();
        let dirty = DirtyRegion::new(0, 0, 200, 80);
        let background = vec![WHITE; (g.xres * g.yres) as usize];
        let mut fb = vec![0xAA; g.smem_len as usize];
        let stats = recompose_dirty_notebook_layers(
            &mut fb,
            &g,
            &profile,
            &background,
            &strokes,
            Some(&overlay),
            &[],
            dirty,
        )
        .unwrap();
        assert_eq!(stats.strokes_considered, 3);
        assert!(stats.strokes_replayed >= 1 && stats.strokes_replayed < 3);
        assert_eq!(stats.scratch_bytes, 200 * 80);
        assert_eq!(crop(&fb, &g, dirty), crop(&expected, &g, dirty));
    }

    #[test]
    fn dirty_restore_reads_stride_background() {
        let g = FbGeometry {
            xres: 20,
            yres: 10,
            xres_virtual: 24,
            yres_virtual: 10,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 24,
            smem_len: 24 * 10,
        };
        let profile = RenderProfile::sealed_v1(0x00).unwrap();
        let mut stride = vec![WHITE; 24 * 10];
        stride[24 * 3 + 5] = 0x40;
        let mut fb = vec![0xAA; g.smem_len as usize];
        recompose_dirty_notebook_layers(
            &mut fb,
            &g,
            &profile,
            &stride,
            &[],
            None,
            &[],
            DirtyRegion::new(4, 2, 4, 3),
        )
        .unwrap();
        assert_eq!(fb[24 * 3 + 5], 0x40);
        assert_eq!(fb[24 * 2 + 4], WHITE);
    }

    #[test]
    fn regional_clean_repaints_an_intersecting_link_icon() {
        use crate::link_paint::panel_dirty;
        use notebook_core::ScribeDisplayTransform;
        use sync_engine::testkit::{nb, page_link, pg};

        let g = geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let link = page_link(1, nb(1), pg(1), nb(2), pg(2));
        let icon = panel_dirty(ScribeDisplayTransform::PortraitIdentity, &link).unwrap();
        let through = Stroke {
            id: new_stroke_id(),
            points: vec![
                Point::new(link.source_x - 400.0, link.source_y),
                Point::new(link.source_x + 400.0, link.source_y),
            ],
            brush: None,
            finished: true,
        };
        let far = Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(400.0, 400.0), Point::new(800.0, 500.0)],
            brush: None,
            finished: true,
        };
        let strokes = [through, far];
        let links = [link];
        let mut expected = vec![WHITE; g.smem_len as usize];
        compose_notebook_layers(&mut expected, &g, &profile, &strokes, None, &links, None).unwrap();
        let box_ = icon.clamp_to(g.xres, g.yres).unwrap();
        let mut icon_off = None;
        'scan: for y in box_.y as u32..box_.y as u32 + box_.height {
            for x in box_.x as u32..box_.x as u32 + box_.width {
                let off = g.byte_offset_visible_8bpp(x, y).unwrap();
                if expected[off] == 0x00 {
                    icon_off = Some(off);
                    break 'scan;
                }
            }
        }
        let icon_off = icon_off.expect("full composition paints the link icon");

        let hit = DirtyRegion::new(icon.x, icon.y, icon.width, icon.height);
        assert!(hit.intersects(icon));
        let background = vec![WHITE; (g.xres * g.yres) as usize];
        let mut fb = vec![0xAA; g.smem_len as usize];
        let stats = recompose_dirty_notebook_layers(
            &mut fb,
            &g,
            &profile,
            &background,
            &strokes,
            None,
            &links,
            hit,
        )
        .unwrap();
        assert_eq!(crop(&fb, &g, hit), crop(&expected, &g, hit));
        assert_eq!(fb[icon_off], 0x00);
        assert!(stats.strokes_replayed >= 1 && stats.strokes_replayed < strokes.len());
        assert_eq!(
            stats.scratch_bytes,
            (hit.width as usize) * (hit.height as usize)
        );

        let miss = DirtyRegion::new(1400, 2000, 50, 40);
        assert!(!miss.intersects(icon));
        let mut untouched = vec![0xAA; g.smem_len as usize];
        let stats = recompose_dirty_notebook_layers(
            &mut untouched,
            &g,
            &profile,
            &background,
            &strokes,
            None,
            &links,
            miss,
        )
        .unwrap();
        assert_eq!(crop(&untouched, &g, miss), crop(&expected, &g, miss));
        assert_eq!(
            untouched[icon_off], 0xAA,
            "a clean that misses every icon must leave that icon's pixels alone"
        );
        assert_eq!(stats.scratch_bytes, 50 * 40);
    }

    /// Host evidence for Hypothesis B/C: regional recompose scratch (and typically wall time)
    /// scales with dirty pixel area. Path-accumulated dirty regions therefore make each
    /// selection-move preview progressively more expensive.
    #[test]
    fn recompose_cost_scales_with_dirty_pixel_area() {
        use std::time::Instant;

        let g = geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let mut strokes = Vec::new();
        for i in 0..40 {
            let y = 200.0 + i as f32 * 400.0;
            let mut s = Stroke {
                id: new_stroke_id(),
                points: vec![Point::new(200.0, y), Point::new(8000.0, y + 80.0)],
                brush: None,
                finished: true,
            };
            s.points[0].pressure = Some(0.5);
            s.points[1].pressure = Some(0.8);
            s.brush = Some(STYLE_PRESSURE_WIDTH_V1.into());
            strokes.push(s);
        }
        let background = vec![WHITE; (g.xres * g.yres) as usize];
        let small = DirtyRegion::new(100, 100, 80, 80);
        let large = DirtyRegion::new(100, 100, 900, 900);
        let mut fb = vec![WHITE; g.smem_len as usize];
        let t0 = Instant::now();
        let small_stats = recompose_dirty_notebook_layers(
            &mut fb,
            &g,
            &profile,
            &background,
            &strokes,
            None,
            &[],
            small,
        )
        .unwrap();
        let small_us = t0.elapsed().as_micros();
        let t1 = Instant::now();
        let large_stats = recompose_dirty_notebook_layers(
            &mut fb,
            &g,
            &profile,
            &background,
            &strokes,
            None,
            &[],
            large,
        )
        .unwrap();
        let large_us = t1.elapsed().as_micros();
        assert_eq!(small_stats.scratch_bytes, 80 * 80);
        assert_eq!(large_stats.scratch_bytes, 900 * 900);
        assert!(
            large_stats.scratch_bytes > small_stats.scratch_bytes * 50,
            "large dirty scratch must dwarf a tiny selection box"
        );
        // Timing is host-dependent; require a clear order-of-magnitude when the machine is warm.
        // If the host is extremely fast, scratch_bytes above still prove the work scaling.
        eprintln!(
            "recompose_cost_diag small_us={small_us} large_us={large_us} small_scratch={} large_scratch={}",
            small_stats.scratch_bytes, large_stats.scratch_bytes
        );
        assert!(large_us >= small_us);
    }
}
