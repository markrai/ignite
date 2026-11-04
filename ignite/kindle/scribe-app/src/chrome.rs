//! Gate F11 notebook chrome: live framebuffer controls only.
//!
//! Two reserved panel bands (top and bottom). A pen contact that *starts* inside a band is UI
//! and never begins a stroke. Stored stroke geometry is not remapped or shifted.
//!
//! Chrome is painted after background + strokes and is never part of `background.raw`,
//! `strokes.jsonl`, or `edits.jsonl`.

use std::cell::Cell;

use kindle_platform::{DirtyRegion, FbGeometry};

use crate::notebook::{page_index, PersistedNotebookManifestV1};
use crate::ui_gfx::{Canvas, Rect, UiPoint, BLACK, GRAY, WHITE};

pub const TOP_BAND_H: u32 = 130;
pub const BOTTOM_BAND_H: u32 = 120;
const RULE_H: u32 = 4;
const BUTTON_PAD: u32 = 14;
const LABEL_SCALE: u32 = 7;
const TITLE_SCALE: u32 = 8;
const PAGE_SCALE: u32 = 8;
const ERASE_W: u32 = 250;

thread_local! {
    /// Transient on-screen Erase mode (never persisted). The UI thread owns it: it is flipped by
    /// the Erase chrome button, read when chrome is painted and when a pen Down is routed.
    static ERASE_MODE: Cell<bool> = const { Cell::new(false) };
}

/// Whether the pen tip currently acts as the eraser (on-screen Erase mode).
pub fn erase_mode() -> bool {
    ERASE_MODE.with(Cell::get)
}

pub fn set_erase_mode(on: bool) {
    ERASE_MODE.with(|m| m.set(on));
}

/// Positional page state painted in the bottom band (1-based `index`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageChrome {
    pub index: usize,
    pub count: usize,
}

impl PageChrome {
    pub fn label(&self) -> String {
        format!("{} of {}", self.index, self.count)
    }

    pub fn can_previous(&self) -> bool {
        self.index > 1
    }
}

/// Fail-closed: unknown `page_id` never silently becomes page 1.
pub fn page_chrome_for(
    manifest: &PersistedNotebookManifestV1,
    page_id: &str,
) -> Result<PageChrome, String> {
    let Some(i) = page_index(manifest, page_id) else {
        return Err(format!(
            "REFUSE PAGE CHROME: manifest does not contain requested page id {page_id}"
        ));
    };
    Ok(PageChrome {
        index: i + 1,
        count: manifest.page_ids.len(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromeHit {
    Home,
    Undo,
    /// Toggle the on-screen Erase mode (pen tip erases while on).
    Erase,
    PageMenu,
    PreviousPage,
    NextPage,
    /// Inside a reserved band but not on a control: consumed, no action.
    Inert,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChromeLayout {
    pub top_band: Rect,
    pub bottom_band: Rect,
    pub home: Rect,
    pub title: Rect,
    pub undo: Rect,
    pub erase: Rect,
    pub menu: Rect,
    pub previous: Rect,
    pub next: Rect,
    pub page_label: Rect,
}

impl ChromeLayout {
    pub fn new(geo: &FbGeometry) -> Self {
        let w = geo.xres;
        let top = Rect::new(0, 0, w, TOP_BAND_H);
        let bottom_y = geo.yres.saturating_sub(BOTTOM_BAND_H);
        let bottom = Rect::new(0, bottom_y, w, BOTTOM_BAND_H);
        let bh = TOP_BAND_H - RULE_H - 2 * BUTTON_PAD;
        let home = Rect::new(20, BUTTON_PAD, 260, bh);
        let menu = Rect::new(w.saturating_sub(20 + 200), BUTTON_PAD, 200, bh);
        let undo = Rect::new(menu.x.saturating_sub(24 + 260), BUTTON_PAD, 260, bh);
        let erase = Rect::new(undo.x.saturating_sub(24 + ERASE_W), BUTTON_PAD, ERASE_W, bh);
        let title_x = home.right() + 40;
        let title = Rect::new(
            title_x,
            BUTTON_PAD,
            erase.x.saturating_sub(40 + title_x),
            bh,
        );
        let nb_h = BOTTOM_BAND_H - RULE_H - 2 * 10;
        let nb_y = bottom_y + RULE_H + 10;
        let cx = w / 2;
        let previous = Rect::new(cx.saturating_sub(250 + 300), nb_y, 300, nb_h);
        let next = Rect::new(cx + 250, nb_y, 300, nb_h);
        let page_label = Rect::new(cx - 240, nb_y, 480, nb_h);
        Self {
            top_band: top,
            bottom_band: bottom,
            home,
            title,
            undo,
            erase,
            menu,
            previous,
            next,
            page_label,
        }
    }

    /// `None` = normal page content (ink engine owns it).
    pub fn hit(&self, p: UiPoint) -> Option<ChromeHit> {
        let in_top = self.top_band.contains(p.x, p.y);
        let in_bottom = self.bottom_band.contains(p.x, p.y);
        if !in_top && !in_bottom {
            return None;
        }
        let hit = [
            (self.home, ChromeHit::Home),
            (self.undo, ChromeHit::Undo),
            (self.erase, ChromeHit::Erase),
            (self.menu, ChromeHit::PageMenu),
            (self.previous, ChromeHit::PreviousPage),
            (self.next, ChromeHit::NextPage),
        ]
        .into_iter()
        .find(|(r, _)| r.contains(p.x, p.y))
        .map(|(_, h)| h)
        .unwrap_or(ChromeHit::Inert);
        Some(hit)
    }

    /// Whether a live-ink dirty region overlaps a band (chrome needs repainting).
    pub fn damaged_by(&self, dirty: DirtyRegion) -> bool {
        if dirty.x < 0 || dirty.y < 0 {
            return true;
        }
        let r = Rect::new(dirty.x as u32, dirty.y as u32, dirty.width, dirty.height);
        r.intersects(&self.top_band) || r.intersects(&self.bottom_band)
    }
}

fn button(c: &mut Canvas<'_>, r: Rect, label: &str, ink: u8) {
    c.fill(r, WHITE);
    c.frame(r, 4, ink);
    c.text_centered(r.inset(8), LABEL_SCALE, label, ink);
}

/// Paint both chrome bands over the current framebuffer contents. Rows outside the bands are
/// never touched, so page content in the writable area is preserved exactly.
pub fn render_notebook_chrome(
    fb: &mut [u8],
    geo: &FbGeometry,
    title: &str,
    page: PageChrome,
) -> Result<ChromeLayout, String> {
    let layout = ChromeLayout::new(geo);
    let mut c = Canvas::new(fb, geo)?;
    c.fill(layout.top_band, WHITE);
    c.fill(layout.bottom_band, WHITE);
    c.fill(Rect::new(0, TOP_BAND_H - RULE_H, geo.xres, RULE_H), BLACK);
    c.fill(Rect::new(0, layout.bottom_band.y, geo.xres, RULE_H), BLACK);

    button(&mut c, layout.home, "HOME", BLACK);
    button(&mut c, layout.undo, "UNDO", BLACK);
    if erase_mode() {
        // Mode is on: inverted button so it reads at a glance.
        c.fill(layout.erase, BLACK);
        c.frame(layout.erase, 4, BLACK);
        c.text_centered(layout.erase.inset(8), LABEL_SCALE, "ERASE", WHITE);
    } else {
        button(&mut c, layout.erase, "ERASE", BLACK);
    }
    c.fill(layout.menu, WHITE);
    c.frame(layout.menu, 4, BLACK);
    c.dots(layout.menu, 18, BLACK);
    c.text_centered(layout.title, TITLE_SCALE, title, BLACK);

    let prev_ink = if page.can_previous() { BLACK } else { GRAY };
    c.frame(layout.previous, 4, prev_ink);
    c.triangle(
        Rect::new(
            layout.previous.x + 110,
            layout.previous.y + 18,
            80,
            layout.previous.h - 36,
        ),
        true,
        prev_ink,
    );
    c.frame(layout.next, 4, BLACK);
    c.triangle(
        Rect::new(
            layout.next.x + 110,
            layout.next.y + 18,
            80,
            layout.next.h - 36,
        ),
        false,
        BLACK,
    );
    c.text_centered(layout.page_label, PAGE_SCALE, &page.label(), BLACK);
    Ok(layout)
}

#[cfg(test)]
mod erase_button_tests {
    use super::*;

    fn geo() -> FbGeometry {
        FbGeometry {
            xres: 1860,
            yres: 2480,
            xres_virtual: 1872,
            yres_virtual: 4960,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 1872,
            smem_len: 1872 * 4960,
        }
    }

    fn centre(r: Rect) -> UiPoint {
        UiPoint {
            x: r.x + r.w / 2,
            y: r.y + r.h / 2,
        }
    }

    #[test]
    fn erase_button_fits_the_top_band_without_overlapping_neighbours() {
        let l = ChromeLayout::new(&geo());
        assert!(l.erase.y + l.erase.h <= TOP_BAND_H - RULE_H);
        assert!(
            l.erase.x >= l.title.x + l.title.w,
            "title must end before Erase"
        );
        assert!(
            l.erase.x + l.erase.w <= l.undo.x,
            "Erase must end before Undo"
        );
        assert!(l.home.right() <= l.title.x);
        assert!(
            l.title.w >= 500,
            "title keeps a usable width, got {}",
            l.title.w
        );
    }

    #[test]
    fn tapping_erase_hits_erase_and_neighbours_keep_their_targets() {
        let l = ChromeLayout::new(&geo());
        assert_eq!(l.hit(centre(l.erase)), Some(ChromeHit::Erase));
        assert_eq!(l.hit(centre(l.undo)), Some(ChromeHit::Undo));
        assert_eq!(l.hit(centre(l.home)), Some(ChromeHit::Home));
        assert_eq!(l.hit(centre(l.menu)), Some(ChromeHit::PageMenu));
        assert_eq!(l.hit(centre(l.title)), Some(ChromeHit::Inert));
    }

    #[test]
    fn button_is_inverted_only_while_erase_mode_is_on() {
        let g = geo();
        let l = ChromeLayout::new(&g);
        // An interior pixel away from the label and the frame.
        let (x, y) = (l.erase.x + 16, l.erase.y + 16);
        let at = |fb: &[u8]| fb[y as usize * g.line_length as usize + x as usize];
        let page = PageChrome { index: 1, count: 2 };

        set_erase_mode(false);
        let mut fb = vec![0x80u8; g.smem_len as usize];
        render_notebook_chrome(&mut fb, &g, "T", page).unwrap();
        assert_eq!(at(&fb), WHITE);

        set_erase_mode(true);
        let mut fb = vec![0x80u8; g.smem_len as usize];
        render_notebook_chrome(&mut fb, &g, "T", page).unwrap();
        assert_eq!(at(&fb), BLACK);

        set_erase_mode(false);
        assert!(!erase_mode());
    }
}
