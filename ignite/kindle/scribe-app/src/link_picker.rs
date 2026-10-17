//! Read-only notebook/page picker for creating an explicit page link.
//!
//! Never a writing surface. Structural notebook edits are not reachable from this view.

use kindle_platform::FbGeometry;
use sync_protocol::{NotebookId, PageId};

use crate::home::HomeCard;
use crate::ui_gfx::{fit_text, Canvas, Rect, UiPoint, BLACK, GRAY, WHITE};

const MARGIN: u32 = 60;
const BACK_W: u32 = 330;
const BAR_H: u32 = 130;
const TOP: u32 = 40;
const DIVIDER_Y: u32 = 200;
const LIST_TOP: u32 = 240;
const ROW_H: u32 = 180;
const ROW_GAP: u32 = 20;
const PAGER_H: u32 = 150;
const COLS: usize = 2;
const THUMB_ROWS: usize = 3;
const THUMB_LABEL_H: u32 = 56;

#[derive(Clone, Debug, PartialEq)]
pub struct PendingLink {
    pub source_notebook_local: String,
    pub source_page_local: String,
    pub source_notebook_id: NotebookId,
    pub source_page_id: PageId,
    pub source_x: f32,
    pub source_y: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkPickerStage {
    Notebooks {
        page: usize,
    },
    Pages {
        notebook_local: String,
        title: String,
        page_ids: Vec<String>,
        grid_page: usize,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct LinkPickerState {
    pub pending: PendingLink,
    pub stage: LinkPickerStage,
    /// Packed 8bpp thumbnails for the currently visible page cells (`page_local` → pixels).
    pub thumbs: Vec<(String, Vec<u8>, u32, u32)>,
}

impl LinkPickerState {
    pub fn notebooks(pending: PendingLink) -> Self {
        Self {
            pending,
            stage: LinkPickerStage::Notebooks { page: 0 },
            thumbs: Vec::new(),
        }
    }

    pub fn show_notebooks(&mut self) {
        self.stage = LinkPickerStage::Notebooks { page: 0 };
        self.thumbs.clear();
    }

    pub fn show_pages(&mut self, notebook_local: String, title: String, page_ids: Vec<String>) {
        self.stage = LinkPickerStage::Pages {
            notebook_local,
            title,
            page_ids,
            grid_page: 0,
        };
        self.thumbs.clear();
    }

    pub fn is_source_page(&self, notebook_local: &str, page_local: &str) -> bool {
        self.pending.source_notebook_local == notebook_local
            && self.pending.source_page_local == page_local
    }

    pub fn visible_page_ids(&self) -> &[String] {
        match &self.stage {
            LinkPickerStage::Notebooks { .. } => &[],
            LinkPickerStage::Pages {
                page_ids,
                grid_page,
                ..
            } => {
                let start = *grid_page * COLS * THUMB_ROWS;
                let end = (start + COLS * THUMB_ROWS).min(page_ids.len());
                &page_ids[start..end]
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkPickerHit {
    None,
    Cancel,
    BackToNotebooks,
    OpenNotebook(String),
    SelectPage {
        notebook_local: String,
        page_local: String,
    },
    PagePrev,
    PageNext,
}

pub fn notebooks_per_page(geo: &FbGeometry) -> usize {
    let pager_y = geo.yres.saturating_sub(PAGER_H + 40);
    let avail = pager_y.saturating_sub(LIST_TOP);
    ((avail + ROW_GAP) / (ROW_H + ROW_GAP)).max(1) as usize
}

pub fn hit(
    state: &mut LinkPickerState,
    geo: &FbGeometry,
    p: UiPoint,
    cards: &[HomeCard],
) -> LinkPickerHit {
    let layout = layout(geo, state, cards);
    if layout.back.contains(p.x, p.y) {
        return match &state.stage {
            LinkPickerStage::Notebooks { .. } => LinkPickerHit::Cancel,
            LinkPickerStage::Pages { .. } => LinkPickerHit::BackToNotebooks,
        };
    }
    if let Some(r) = layout.prev {
        if r.contains(p.x, p.y) {
            match &mut state.stage {
                LinkPickerStage::Notebooks { page } if *page > 0 => {
                    *page -= 1;
                    return LinkPickerHit::PagePrev;
                }
                LinkPickerStage::Pages { grid_page, .. } if *grid_page > 0 => {
                    *grid_page -= 1;
                    state.thumbs.clear();
                    return LinkPickerHit::PagePrev;
                }
                _ => return LinkPickerHit::None,
            }
        }
    }
    if let Some(r) = layout.next {
        if r.contains(p.x, p.y) {
            match &mut state.stage {
                LinkPickerStage::Notebooks { page } => {
                    let per = notebooks_per_page(geo).max(1);
                    if (*page + 1) * per < cards.len() {
                        *page += 1;
                        return LinkPickerHit::PageNext;
                    }
                    return LinkPickerHit::None;
                }
                LinkPickerStage::Pages {
                    grid_page,
                    page_ids,
                    ..
                } => {
                    let per = COLS * THUMB_ROWS;
                    if (*grid_page + 1) * per < page_ids.len() {
                        *grid_page += 1;
                        state.thumbs.clear();
                        return LinkPickerHit::PageNext;
                    }
                    return LinkPickerHit::None;
                }
            }
        }
    }
    match &state.stage {
        LinkPickerStage::Notebooks { .. } => {
            for (i, r) in layout.rows.iter().enumerate() {
                if r.contains(p.x, p.y) {
                    let per = notebooks_per_page(geo);
                    let page = match state.stage {
                        LinkPickerStage::Notebooks { page } => page,
                        _ => 0,
                    };
                    let idx = page * per + i;
                    if let Some(c) = cards.get(idx) {
                        return LinkPickerHit::OpenNotebook(c.notebook_id.clone());
                    }
                }
            }
            LinkPickerHit::None
        }
        LinkPickerStage::Pages {
            notebook_local,
            page_ids,
            grid_page,
            ..
        } => {
            let start = *grid_page * COLS * THUMB_ROWS;
            for (i, r) in layout.thumbs.iter().enumerate() {
                if !r.contains(p.x, p.y) {
                    continue;
                }
                let Some(page_local) = page_ids.get(start + i) else {
                    return LinkPickerHit::None;
                };
                if state.is_source_page(notebook_local, page_local) {
                    return LinkPickerHit::None;
                }
                return LinkPickerHit::SelectPage {
                    notebook_local: notebook_local.clone(),
                    page_local: page_local.clone(),
                };
            }
            LinkPickerHit::None
        }
    }
}

impl LinkPickerState {
    pub fn hit(&mut self, geo: &FbGeometry, p: UiPoint, cards: &[HomeCard]) -> LinkPickerHit {
        hit(self, geo, p, cards)
    }
}

struct PickerLayout {
    back: Rect,
    rows: Vec<Rect>,
    thumbs: Vec<Rect>,
    prev: Option<Rect>,
    next: Option<Rect>,
}

fn layout(geo: &FbGeometry, state: &LinkPickerState, cards: &[HomeCard]) -> PickerLayout {
    let w = geo.xres.saturating_sub(2 * MARGIN);
    let back = Rect::new(MARGIN, TOP, BACK_W, BAR_H);
    let pager_y = geo.yres.saturating_sub(PAGER_H + 40);
    let prev = Rect::new(MARGIN, pager_y, 400, PAGER_H);
    let next = Rect::new(geo.xres.saturating_sub(MARGIN + 400), pager_y, 400, PAGER_H);
    match &state.stage {
        LinkPickerStage::Notebooks { page } => {
            let per = notebooks_per_page(geo);
            let start = *page * per;
            let n = cards.len().saturating_sub(start).min(per);
            let rows = (0..n)
                .map(|i| Rect::new(MARGIN, LIST_TOP + i as u32 * (ROW_H + ROW_GAP), w, ROW_H))
                .collect();
            let pages = cards.len().div_ceil(per.max(1)).max(1);
            PickerLayout {
                back,
                rows,
                thumbs: Vec::new(),
                prev: (*page > 0).then_some(prev),
                next: (*page + 1 < pages).then_some(next),
            }
        }
        LinkPickerStage::Pages {
            page_ids,
            grid_page,
            ..
        } => {
            let per = COLS * THUMB_ROWS;
            let start = *grid_page * per;
            let n = page_ids.len().saturating_sub(start).min(per);
            let cell_w = (w.saturating_sub(ROW_GAP)) / COLS as u32;
            let cell_h = (pager_y.saturating_sub(LIST_TOP + 20)) / THUMB_ROWS as u32;
            let mut thumbs = Vec::new();
            for i in 0..n {
                let col = (i % COLS) as u32;
                let row = (i / COLS) as u32;
                thumbs.push(Rect::new(
                    MARGIN + col * (cell_w + ROW_GAP),
                    LIST_TOP + row * cell_h,
                    cell_w,
                    cell_h.saturating_sub(8),
                ));
            }
            let pages = page_ids.len().div_ceil(per.max(1)).max(1);
            PickerLayout {
                back,
                rows: Vec::new(),
                thumbs,
                prev: (*grid_page > 0).then_some(prev),
                next: (*grid_page + 1 < pages).then_some(next),
            }
        }
    }
}

/// Packed thumbnail size used when blitting into a cell.
pub fn thumb_inner(cell: Rect) -> Rect {
    let label = THUMB_LABEL_H;
    let h = cell.h.saturating_sub(label + 16);
    let w = (h as u64 * 1860 / 2480) as u32;
    let w = w.min(cell.w.saturating_sub(24)).max(40);
    let x = cell.x + (cell.w.saturating_sub(w)) / 2;
    Rect::new(x, cell.y + 8, w, h)
}

pub fn render_link_picker(
    fb: &mut [u8],
    geo: &FbGeometry,
    state: &LinkPickerState,
    cards: &[HomeCard],
) -> Result<(), String> {
    let lay = layout(geo, state, cards);
    let mut c = Canvas::new(fb, geo)?;
    c.fill_all(WHITE);
    let w = geo.xres.saturating_sub(2 * MARGIN);
    c.frame(lay.back, 5, BLACK);
    let back_label = match state.stage {
        LinkPickerStage::Notebooks { .. } => "< CANCEL",
        LinkPickerStage::Pages { .. } => "< BACK",
    };
    c.text_centered(lay.back.inset(8), 7, back_label, BLACK);
    let heading = match &state.stage {
        LinkPickerStage::Notebooks { .. } => "LINK TO NOTEBOOK".to_string(),
        LinkPickerStage::Pages { title, .. } => fit_text(title, 8, w.saturating_sub(BACK_W + 80)),
    };
    c.text(lay.back.right() + 40, TOP + 36, 8, &heading, BLACK);
    c.fill(Rect::new(MARGIN, DIVIDER_Y, w, 4), BLACK);

    match &state.stage {
        LinkPickerStage::Notebooks { page } => {
            let per = notebooks_per_page(geo);
            let start = *page * per;
            for (i, r) in lay.rows.iter().enumerate() {
                let Some(card) = cards.get(start + i) else {
                    break;
                };
                c.frame(*r, 5, BLACK);
                c.text(
                    r.x + 40,
                    r.y + 50,
                    8,
                    &fit_text(&card.title, 8, r.w.saturating_sub(80)),
                    BLACK,
                );
            }
        }
        LinkPickerStage::Pages {
            notebook_local,
            page_ids,
            grid_page,
            ..
        } => {
            let start = *grid_page * COLS * THUMB_ROWS;
            for (i, cell) in lay.thumbs.iter().enumerate() {
                let Some(page_local) = page_ids.get(start + i) else {
                    break;
                };
                let disabled = state.is_source_page(notebook_local, page_local);
                let ink = if disabled { GRAY } else { BLACK };
                c.frame(*cell, 4, ink);
                let inner = thumb_inner(*cell);
                c.frame(inner, 2, ink);
                if let Some((_, pixels, tw, th)) =
                    state.thumbs.iter().find(|(id, _, _, _)| id == page_local)
                {
                    blit_thumb(&mut c, inner, pixels, *tw, *th);
                }
                let ordinal = start + i + 1;
                c.text_centered(
                    Rect::new(cell.x, inner.bottom() + 8, cell.w, THUMB_LABEL_H - 8),
                    5,
                    &format!("Page {ordinal}"),
                    ink,
                );
            }
        }
    }

    if let Some(r) = lay.prev {
        c.frame(r, 5, BLACK);
        c.text_centered(r.inset(8), 8, "< PREV", BLACK);
    }
    if let Some(r) = lay.next {
        c.frame(r, 5, BLACK);
        c.text_centered(r.inset(8), 8, "NEXT >", BLACK);
    }
    Ok(())
}

fn blit_thumb(c: &mut Canvas<'_>, dest: Rect, pixels: &[u8], tw: u32, th: u32) {
    if tw == 0 || th == 0 {
        return;
    }
    let inner = dest.inset(4);
    for y in 0..inner.h {
        let sy = y * th / inner.h.max(1);
        for x in 0..inner.w {
            let sx = x * tw / inner.w.max(1);
            let i = (sy * tw + sx) as usize;
            let v = pixels.get(i).copied().unwrap_or(WHITE);
            c.put(inner.x + x, inner.y + y, v);
        }
    }
}

/// Copy shown when a link's destination is not currently live.
pub const MISSING_LINK_LABEL: &str = "Linked page no longer exists";

/// A destination that exists in the current library and notebook manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveLinkTarget {
    pub notebook_local: String,
    pub page_local: String,
    pub title: String,
    pub ordinal: usize,
}

/// Jump-button label and whether Jump is enabled. A missing destination never
/// formats an ordinal, so the button cannot read `Page 0`.
pub fn link_jump_presentation(target: Option<&LiveLinkTarget>) -> (String, bool) {
    match target {
        Some(t) => (
            format!("Jump to \"{} - Page {}\"", t.title, t.ordinal),
            true,
        ),
        None => (MISSING_LINK_LABEL.to_string(), false),
    }
}

/// Live only when the global IDs map to a notebook that is in the library and a
/// page that is in that notebook's current manifest, so an ordinal exists.
pub fn resolve_live_target(
    library: &crate::library::LibraryRepository,
    identity: &sync_protocol::IdentityFileV1,
    notebook_id: sync_protocol::NotebookId,
    page_id: sync_protocol::PageId,
) -> Option<LiveLinkTarget> {
    let notebook_local = identity
        .notebooks
        .iter()
        .find(|(_, g)| **g == notebook_id)
        .map(|(l, _)| l.clone())?;
    if !library
        .manifest()
        .notebook_ids
        .iter()
        .any(|id| id == &notebook_local)
    {
        return None;
    }
    let page_local = identity
        .pages
        .get(&notebook_local)?
        .iter()
        .find(|(_, g)| **g == page_id)
        .map(|(l, _)| l.clone())?;
    let manifest =
        crate::library::read_notebook_manifest_read_only(&library.notebook_dir(&notebook_local))
            .ok()?;
    let index = manifest.page_ids.iter().position(|p| p == &page_local)?;
    let title = library
        .title(&notebook_local)
        .unwrap_or(&notebook_local)
        .to_string();
    Some(LiveLinkTarget {
        notebook_local,
        page_local,
        title,
        ordinal: index + 1,
    })
}

/// The picker selection is live only when that local notebook and page are in
/// the current library and notebook manifests. An identity binding is not enough.
pub fn selected_target_is_live(
    library: &crate::library::LibraryRepository,
    notebook_local: &str,
    page_local: &str,
) -> bool {
    if notebook_local.is_empty() || page_local.is_empty() {
        return false;
    }
    if !library
        .manifest()
        .notebook_ids
        .iter()
        .any(|id| id == notebook_local)
    {
        return false;
    }
    let Ok(manifest) =
        crate::library::read_notebook_manifest_read_only(&library.notebook_dir(notebook_local))
    else {
        return false;
    };
    manifest.page_ids.iter().any(|p| p == page_local)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_gfx::panel_geo;
    use sync_engine::testkit::{nb, pg};

    fn pending() -> PendingLink {
        PendingLink {
            source_notebook_local: "notebook-1".into(),
            source_page_local: "page-1".into(),
            source_notebook_id: nb(1),
            source_page_id: pg(1),
            source_x: 100.0,
            source_y: 200.0,
        }
    }

    fn cards() -> Vec<HomeCard> {
        vec![
            HomeCard {
                notebook_id: "notebook-1".into(),
                title: "A".into(),
                page_count: Some(2),
            },
            HomeCard {
                notebook_id: "notebook-2".into(),
                title: "B".into(),
                page_count: Some(3),
            },
        ]
    }

    #[test]
    fn picker_does_not_allow_page_input_and_disables_source_page() {
        assert!(!crate::home::AppView::LinkTargetPicker.allows_page_input());
        let g = panel_geo();
        let mut s = LinkPickerState::notebooks(pending());
        s.show_pages(
            "notebook-1".into(),
            "A".into(),
            vec!["page-1".into(), "page-2".into()],
        );
        let lay = layout(&g, &s, &cards());
        let src = lay.thumbs[0];
        let hit = s.hit(
            &g,
            UiPoint {
                x: src.x + 10,
                y: src.y + 10,
            },
            &cards(),
        );
        assert_eq!(hit, LinkPickerHit::None, "source page is not selectable");
        let other = lay.thumbs[1];
        let hit = s.hit(
            &g,
            UiPoint {
                x: other.x + 10,
                y: other.y + 10,
            },
            &cards(),
        );
        assert_eq!(
            hit,
            LinkPickerHit::SelectPage {
                notebook_local: "notebook-1".into(),
                page_local: "page-2".into(),
            }
        );
    }

    #[test]
    fn cancel_from_notebook_list() {
        let g = panel_geo();
        let mut s = LinkPickerState::notebooks(pending());
        let lay = layout(&g, &s, &cards());
        let hit = s.hit(
            &g,
            UiPoint {
                x: lay.back.x + 10,
                y: lay.back.y + 10,
            },
            &cards(),
        );
        assert_eq!(hit, LinkPickerHit::Cancel);
    }
}
