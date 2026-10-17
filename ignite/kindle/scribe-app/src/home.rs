//! Gate F11 framebuffer-native Home/library view.
//!
//! Cards with large pen targets, a visible New control, and deterministic viewport paging
//! (`ROWS_PER_PAGE` rows per Home page). The viewport is transient UI state; it never
//! changes `library.meta`. This renderer paints only the live framebuffer; callers submit
//! one GC16 FULL refresh afterward.
//!
//! Folders (Kindle-local, `folders.rs`): the main list shows folder tiles first, three per row,
//! then the notebooks in no folder; rows page together, so folders are never repeated on later
//! pages. Opening a folder shows its notebooks in the same stacked layout, with a back arrow.

use kindle_platform::FbGeometry;

use crate::library::LibraryRepository;
use crate::ui_gfx::{
    fit_text, scale_to_fit, Canvas, Rect, UiPoint, BLACK, GLYPH_H, GLYPH_W, GRAY, WHITE,
};

pub use crate::folders::FolderInfo;

pub const ROWS_PER_PAGE: usize = 6;

const MARGIN: u32 = 60;
/// Compact top bar: [exit] [search placeholder] [sync] [+ new] [tools]. Every control keeps a
/// >= 130 px (about 11 mm) pen/finger target.
const BAR_Y: u32 = 36;
const BAR_H: u32 = 130;
const BAR_GAP: u32 = 20;
/// Exit pictogram drawn in one glyph cell at this scale (50x70), centred in a BAR_H square.
const EXIT_SCALE: u32 = 10;
const NEW_W: u32 = 280;
/// D1: the Sync entry point (product mode only), left of "+ NEW".
const SYNC_W: u32 = 230;
const TOOLS_W: u32 = BAR_H;
const DIVIDER_Y: u32 = 196;
const CARDS_TOP: u32 = 232;
const CARD_GAP: u32 = 24;
const PAGER_H: u32 = 170;
const PAGER_BOTTOM_MARGIN: u32 = 50;
const PAGER_BUTTON_W: u32 = 300;
const MENU_W: u32 = 230;
const TITLE_SCALE: u32 = 10;
const META_SCALE: u32 = 7;
/// Folder tiles: three per row.
pub const FOLDERS_PER_ROW: usize = 3;
const TILE_GAP: u32 = 24;
const TILE_MENU_W: u32 = 140;
const TILE_TITLE_SCALE: u32 = 7;
const TILE_TITLE_MIN_SCALE: u32 = 3;
const TILE_META_SCALE: u32 = 5;
pub const SEARCH_PLACEHOLDER: &str = "Search notebooks...";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppView {
    Notebook,
    Home,
    /// D1: server discovery / pairing / sync status. Never a writing surface.
    Sync,
    /// Tools (Import Kindle notebooks). Never a writing surface.
    Tools,
    /// Replica-local preferences (side-button action). Never a writing surface.
    Preferences,
    /// Read-only picker for choosing a page-link target. Never a writing surface.
    LinkTargetPicker,
}

impl AppView {
    /// The single routing gate used before any page gesture state machine sees input.
    pub fn allows_page_input(self) -> bool {
        matches!(self, Self::Notebook)
    }
}

/// One notebook as Home shows it. `page_count` is `None` when its manifest is unreadable
/// (rendered conservatively; never repaired from Home).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HomeCard {
    pub notebook_id: String,
    pub title: String,
    pub page_count: Option<usize>,
}

/// Read-only snapshot of the library for Home rendering and hit-testing.
pub fn home_cards(library: &LibraryRepository) -> Vec<HomeCard> {
    library
        .manifest()
        .notebook_ids
        .iter()
        .map(|id| {
            let summary = library.home_summaries().get(id);
            HomeCard {
                notebook_id: id.clone(),
                title: summary
                    .map(|s| s.title.clone())
                    .unwrap_or_else(|| "?".into()),
                page_count: summary.and_then(|s| s.page_count),
            }
        })
        .collect()
}

/// One Home row: a row of up to three folder tiles (indices into the folder list), or one
/// notebook card (index into the library-order card list).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HomeRow {
    Folders(Vec<usize>),
    Notebook(usize),
}

/// Rows of the current view. Main list (`open == None`): folder rows first, then every notebook
/// in no folder, in library order. Inside a folder: that folder's notebooks, in library order.
pub fn home_rows(cards: &[HomeCard], folders: &[FolderInfo], open: Option<&str>) -> Vec<HomeRow> {
    match open.and_then(|id| folders.iter().find(|f| f.id == id)) {
        Some(f) => cards
            .iter()
            .enumerate()
            .filter(|(_, c)| f.notebook_ids.contains(&c.notebook_id))
            .map(|(i, _)| HomeRow::Notebook(i))
            .collect(),
        None => {
            let foldered: std::collections::HashSet<&str> = folders
                .iter()
                .flat_map(|f| f.notebook_ids.iter().map(String::as_str))
                .collect();
            let idx: Vec<usize> = (0..folders.len()).collect();
            idx.chunks(FOLDERS_PER_ROW)
                .map(|c| HomeRow::Folders(c.to_vec()))
                .chain(
                    cards
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| !foldered.contains(c.notebook_id.as_str()))
                        .map(|(i, _)| HomeRow::Notebook(i)),
                )
                .collect()
        }
    }
}

/// The row showing card `card` in this view: its own row, or (main list) its folder's tile row.
pub fn row_of_card(
    rows: &[HomeRow],
    cards: &[HomeCard],
    folders: &[FolderInfo],
    card: usize,
) -> Option<usize> {
    let id = &cards.get(card)?.notebook_id;
    let folder = folders.iter().position(|f| f.notebook_ids.contains(id));
    rows.iter().position(|r| match r {
        HomeRow::Notebook(i) => *i == card,
        HomeRow::Folders(v) => folder.is_some_and(|f| v.contains(&f)),
    })
}

fn plain_rows(total: usize) -> Vec<HomeRow> {
    (0..total).map(HomeRow::Notebook).collect()
}

pub fn viewport_page_count(total: usize) -> usize {
    total.div_ceil(ROWS_PER_PAGE).max(1)
}

/// Transient Home selection + viewport page. Selection starts on the active notebook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HomeState {
    pub selected_index: usize,
    pub page: usize,
    /// D1: show the SYNC button (product mode with a sync directory).
    pub sync: bool,
    /// Move the viewport to the selected notebook's row at the next [`HomeState::settle`]
    /// (with folders, rows are not library indices).
    pub reveal: bool,
}

impl HomeState {
    pub fn for_active(library: &LibraryRepository) -> Self {
        let ids = &library.manifest().notebook_ids;
        let index = ids
            .iter()
            .position(|id| id == library.active_notebook_id())
            .unwrap_or(0);
        Self::selecting(index, ids.len())
    }

    pub fn selecting(index: usize, total: usize) -> Self {
        let mut s = Self {
            selected_index: index,
            page: 0,
            sync: false,
            reveal: true,
        };
        s.normalize(total);
        s.page = s.selected_index / ROWS_PER_PAGE;
        s
    }

    /// Keep selection and viewport inside the library. Empty viewport pages are impossible.
    pub fn normalize(&mut self, total: usize) {
        self.selected_index = self.selected_index.min(total.saturating_sub(1));
        self.page = self.page.min(viewport_page_count(total) - 1);
    }

    pub fn ensure_selected_visible(&mut self, total: usize) {
        self.normalize(total);
        self.page = self.selected_index / ROWS_PER_PAGE;
        self.reveal = true;
    }

    /// Fit the viewport to the laid-out `rows`. Selection stays a library index; a pending
    /// reveal moves the viewport to the row showing the selected notebook (or its folder tile).
    pub fn settle(&mut self, rows: &[HomeRow], cards: &[HomeCard], folders: &[FolderInfo]) {
        self.selected_index = self.selected_index.min(cards.len().saturating_sub(1));
        if std::mem::take(&mut self.reveal) {
            self.page = row_of_card(rows, cards, folders, self.selected_index)
                .map_or(0, |row| row / ROWS_PER_PAGE);
        }
        self.page = self.page.min(viewport_page_count(rows.len()) - 1);
    }

    /// F10 developer cursor controls: wrap through the full manifest order.
    pub fn select_next(&mut self, total: usize) {
        if total > 0 {
            self.selected_index = (self.selected_index + 1) % total;
            self.ensure_selected_visible(total);
        }
    }

    pub fn select_previous(&mut self, total: usize) {
        if total > 0 {
            self.selected_index = (self.selected_index + total - 1) % total;
            self.ensure_selected_visible(total);
        }
    }

    /// Library indices shown on the current viewport page.
    pub fn visible_range(&self, total: usize) -> std::ops::Range<usize> {
        let start = (self.page * ROWS_PER_PAGE).min(total);
        start..(start + ROWS_PER_PAGE).min(total)
    }

    pub fn can_page_previous(&self) -> bool {
        self.page > 0
    }

    pub fn can_page_next(&self, total: usize) -> bool {
        self.page + 1 < viewport_page_count(total)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeHit {
    /// Doorway icon (top-left): leave the app cleanly.
    Exit,
    /// Inside a folder the top-left control goes back to the main list.
    Back,
    /// Folder tile (index into the folder list): open the folder.
    OpenFolder(usize),
    /// Folder tile menu (index into the folder list).
    FolderMenu(usize),
    New,
    /// Library index (manifest order), not viewport row.
    OpenCard(usize),
    CardMenu(usize),
    PagePrevious,
    PageNext,
    /// D1: open the Sync screen.
    Sync,
    /// Tools screen (Import Kindle notebooks, ...).
    Tools,
    /// Search placeholder: consumed, does nothing (search is not implemented yet).
    SearchPlaceholder,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HomeLayout {
    /// Exit icon: exactly one glyph cell (scale `EXIT_SCALE`), centred in `exit_hit`.
    pub exit_icon: Rect,
    /// Square touch target around `exit_icon`.
    pub exit_hit: Rect,
    /// Flexible-width, inert search placeholder.
    pub search: Rect,
    pub new_button: Rect,
    /// D1: hit/drawn only when `HomeState::sync`.
    pub sync_button: Rect,
    pub tools_button: Rect,
    /// `(library index, card rect, menu rect)` for the visible viewport page.
    pub cards: Vec<(usize, Rect, Rect)>,
    /// `(folder index, tile rect, menu rect)` for the visible viewport page.
    pub folders: Vec<(usize, Rect, Rect)>,
    /// Rows in the current view (paging bound).
    pub rows_len: usize,
    /// Inside a folder: top-left is Back, the search area shows the folder title.
    pub in_folder: bool,
    pub pager_previous: Rect,
    pub pager_next: Rect,
    pub pager_label: Rect,
}

impl HomeLayout {
    /// Notebooks only (no folders): one row per library notebook.
    pub fn new(geo: &FbGeometry, state: &HomeState, total: usize) -> Self {
        Self::for_rows(geo, state, &plain_rows(total), false)
    }

    pub fn for_rows(geo: &FbGeometry, state: &HomeState, all: &[HomeRow], in_folder: bool) -> Self {
        let width = geo.xres.saturating_sub(2 * MARGIN);
        let pager_y = geo.yres.saturating_sub(PAGER_H + PAGER_BOTTOM_MARGIN);
        let cards_bottom = pager_y.saturating_sub(CARD_GAP * 2);
        let rows = ROWS_PER_PAGE as u32;
        let card_h = (cards_bottom.saturating_sub(CARDS_TOP + (rows - 1) * CARD_GAP)) / rows;
        let per = FOLDERS_PER_ROW as u32;
        let tile_w = width.saturating_sub((per - 1) * TILE_GAP) / per;
        let mut cards = Vec::new();
        let mut folders = Vec::new();
        for (row, index) in state.visible_range(all.len()).enumerate() {
            let y = CARDS_TOP + row as u32 * (card_h + CARD_GAP);
            match &all[index] {
                HomeRow::Notebook(i) => {
                    let card = Rect::new(MARGIN, y, width, card_h);
                    let menu = Rect::new(card.right() - MENU_W, y, MENU_W, card_h);
                    cards.push((*i, card, menu));
                }
                HomeRow::Folders(v) => {
                    for (col, f) in v.iter().enumerate() {
                        let x = MARGIN + col as u32 * (tile_w + TILE_GAP);
                        let tile = Rect::new(x, y, tile_w, card_h);
                        let menu = Rect::new(tile.right() - TILE_MENU_W, y, TILE_MENU_W, card_h);
                        folders.push((*f, tile, menu));
                    }
                }
            }
        }
        let cx = geo.xres / 2;
        let exit_hit = Rect::new(MARGIN, BAR_Y, BAR_H, BAR_H);
        let (icon_w, icon_h) = (GLYPH_W * EXIT_SCALE, GLYPH_H * EXIT_SCALE);
        let exit_icon = Rect::new(
            exit_hit.x + (BAR_H - icon_w) / 2,
            exit_hit.y + (BAR_H - icon_h) / 2,
            icon_w,
            icon_h,
        );
        let right = geo.xres.saturating_sub(MARGIN);
        let tools_button = Rect::new(right.saturating_sub(TOOLS_W), BAR_Y, TOOLS_W, BAR_H);
        let new_button = Rect::new(
            tools_button.x.saturating_sub(BAR_GAP + NEW_W),
            BAR_Y,
            NEW_W,
            BAR_H,
        );
        let sync_button = Rect::new(
            new_button.x.saturating_sub(BAR_GAP + SYNC_W),
            BAR_Y,
            SYNC_W,
            BAR_H,
        );
        let search_left = exit_hit.right() + BAR_GAP;
        let search_right = if state.sync {
            sync_button.x
        } else {
            new_button.x
        }
        .saturating_sub(BAR_GAP);
        let search = Rect::new(
            search_left,
            BAR_Y,
            search_right.saturating_sub(search_left),
            BAR_H,
        );
        Self {
            exit_icon,
            exit_hit,
            search,
            new_button,
            sync_button,
            tools_button,
            cards,
            folders,
            rows_len: all.len(),
            in_folder,
            pager_previous: Rect::new(MARGIN, pager_y, PAGER_BUTTON_W, PAGER_H),
            pager_next: Rect::new(
                geo.xres.saturating_sub(MARGIN + PAGER_BUTTON_W),
                pager_y,
                PAGER_BUTTON_W,
                PAGER_H,
            ),
            pager_label: Rect::new(cx - 400, pager_y, 800, PAGER_H),
        }
    }

    /// `_total` is kept for callers; paging is bounded by the laid-out rows.
    pub fn hit(&self, p: UiPoint, state: &HomeState, _total: usize) -> Option<HomeHit> {
        let total = self.rows_len;
        if self.exit_hit.contains(p.x, p.y) {
            return Some(if self.in_folder {
                HomeHit::Back
            } else {
                HomeHit::Exit
            });
        }
        if self.new_button.contains(p.x, p.y) {
            return Some(HomeHit::New);
        }
        if state.sync && self.sync_button.contains(p.x, p.y) {
            return Some(HomeHit::Sync);
        }
        if self.tools_button.contains(p.x, p.y) {
            return Some(HomeHit::Tools);
        }
        if self.search.contains(p.x, p.y) {
            return Some(HomeHit::SearchPlaceholder);
        }
        for (index, tile, menu) in &self.folders {
            if menu.contains(p.x, p.y) {
                return Some(HomeHit::FolderMenu(*index));
            }
            if tile.contains(p.x, p.y) {
                return Some(HomeHit::OpenFolder(*index));
            }
        }
        for (index, card, menu) in &self.cards {
            if menu.contains(p.x, p.y) {
                return Some(HomeHit::CardMenu(*index));
            }
            if card.contains(p.x, p.y) {
                return Some(HomeHit::OpenCard(*index));
            }
        }
        if self.pager_previous.contains(p.x, p.y) && state.can_page_previous() {
            return Some(HomeHit::PagePrevious);
        }
        if self.pager_next.contains(p.x, p.y) && state.can_page_next(total) {
            return Some(HomeHit::PageNext);
        }
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HomeRenderSummary {
    pub visible_entries: usize,
    pub total_entries: usize,
    pub viewport_page: usize,
    pub viewport_pages: usize,
}

fn pages_label(count: Option<usize>) -> String {
    match count {
        Some(1) => "1 page".into(),
        Some(n) => format!("{n} pages"),
        None => "? pages".into(),
    }
}

/// Doorway-exit pictogram inside one glyph cell (50x70 at scale 10): a door frame open on its
/// right side with an arrow leaving through the gap, drawn in font units.
fn draw_exit_icon(c: &mut Canvas<'_>, r: Rect) {
    // One font "pixel"; strokes are thinner than the font's so the icon does not read as a letter.
    let u = (r.w / GLYPH_W).max(1);
    let stroke = (u / 2).max(2);
    let frame_w = u * 3;
    // Frame: left post, lintel, sill, and short right jambs leaving a doorway gap.
    c.fill(Rect::new(r.x, r.y, stroke, r.h), BLACK);
    c.fill(Rect::new(r.x, r.y, frame_w, stroke), BLACK);
    c.fill(Rect::new(r.x, r.bottom() - stroke, frame_w, stroke), BLACK);
    let jamb_h = u * 2;
    c.fill(
        Rect::new(r.x + frame_w - stroke, r.y, stroke, jamb_h),
        BLACK,
    );
    c.fill(
        Rect::new(r.x + frame_w - stroke, r.bottom() - jamb_h, stroke, jamb_h),
        BLACK,
    );
    // Arrow out through the gap: shaft + right-pointing head reaching the cell edge.
    let mid = r.y + r.h / 2;
    let head_w = u + u / 2;
    let head_h = u * 2 + u / 2;
    let shaft_x = r.x + u;
    c.fill(
        Rect::new(
            shaft_x,
            mid - stroke / 2,
            r.right() - head_w - shaft_x,
            stroke,
        ),
        BLACK,
    );
    c.triangle(
        Rect::new(r.right() - head_w, mid - head_h / 2, head_w, head_h),
        false,
        BLACK,
    );
}

/// Back pictogram (inside a folder): a left-pointing arrow in the exit icon's cell.
fn draw_back_icon(c: &mut Canvas<'_>, r: Rect) {
    let u = (r.w / GLYPH_W).max(1);
    let stroke = u.max(4);
    let mid = r.y + r.h / 2;
    let head_w = u * 2;
    let head_h = u * 4;
    c.triangle(
        Rect::new(r.x, mid - head_h / 2, head_w, head_h),
        true,
        BLACK,
    );
    c.fill(
        Rect::new(
            r.x + head_w,
            mid - stroke / 2,
            r.w.saturating_sub(head_w),
            stroke,
        ),
        BLACK,
    );
}

/// Folder pictogram: a tab over a body outline.
fn draw_folder_icon(c: &mut Canvas<'_>, r: Rect) {
    let tab_h = r.h / 5;
    c.fill(Rect::new(r.x, r.y, r.w * 2 / 5, tab_h + 4), BLACK);
    c.frame(Rect::new(r.x, r.y + tab_h, r.w, r.h - tab_h), 6, BLACK);
}

fn notebooks_label(n: usize) -> String {
    if n == 1 {
        "1 notebook".into()
    } else {
        format!("{n} notebooks")
    }
}

/// Gear pictogram (tools/settings) from filled rectangles: a square ring with eight teeth.
fn draw_tools_icon(c: &mut Canvas<'_>, r: Rect) {
    let side = r.w.min(r.h);
    let u = (side / 10).max(2);
    let cx = r.x + side / 2;
    let cy = r.y + side / 2;
    let half = u * 3;
    // Teeth: N/S/E/W and four corner nubs.
    c.fill(Rect::new(cx - u, cy - half - 2 * u, 2 * u, 2 * u), BLACK);
    c.fill(Rect::new(cx - u, cy + half, 2 * u, 2 * u), BLACK);
    c.fill(Rect::new(cx - half - 2 * u, cy - u, 2 * u, 2 * u), BLACK);
    c.fill(Rect::new(cx + half, cy - u, 2 * u, 2 * u), BLACK);
    for (x, y) in [
        (cx - half - u, cy - half - u),
        (cx + half, cy - half - u),
        (cx - half - u, cy + half),
        (cx + half, cy + half),
    ] {
        c.fill(Rect::new(x, y, u, u), BLACK);
    }
    // Body ring with a white hub.
    c.frame(
        Rect::new(cx - half, cy - half, 2 * half, 2 * half),
        u + u / 2,
        BLACK,
    );
}

/// Notebooks only (no folders).
pub fn render_home(
    fb: &mut [u8],
    geo: &FbGeometry,
    cards: &[HomeCard],
    state: &HomeState,
) -> Result<HomeRenderSummary, String> {
    render_home_view(fb, geo, cards, &[], None, state)
}

/// Home with folders: the main list (`open == None`) or one open folder.
pub fn render_home_view(
    fb: &mut [u8],
    geo: &FbGeometry,
    cards: &[HomeCard],
    folders: &[FolderInfo],
    open: Option<&str>,
    state: &HomeState,
) -> Result<HomeRenderSummary, String> {
    let open_folder = open.and_then(|id| folders.iter().find(|f| f.id == id));
    let rows = home_rows(cards, folders, open_folder.map(|f| f.id.as_str()));
    let total = rows.len();
    let mut state = *state;
    state.settle(&rows, cards, folders);
    let layout = HomeLayout::for_rows(geo, &state, &rows, open_folder.is_some());
    let mut c = Canvas::new(fb, geo)?;
    // Full white clear: no stale notebook page, modal, or previous Home frame survives.
    c.fill_all(WHITE);
    let text_y = layout.search.y + (BAR_H - GLYPH_H * 7) / 2;
    if let Some(f) = open_folder {
        draw_back_icon(&mut c, layout.exit_icon);
        // The folder's name where search sits on the main list (inert).
        let name = fit_text(&f.title, 8, layout.search.w.saturating_sub(40));
        c.text(
            layout.search.x + 20,
            layout.search.y + (BAR_H - GLYPH_H * 8) / 2,
            8,
            &name,
            BLACK,
        );
    } else {
        draw_exit_icon(&mut c, layout.exit_icon);
        // Inert search placeholder (flexible width).
        c.frame(layout.search, 3, GRAY);
        let search_text = fit_text(SEARCH_PLACEHOLDER, 7, layout.search.w.saturating_sub(60));
        c.text(layout.search.x + 30, text_y, 7, &search_text, GRAY);
    }
    if state.sync {
        c.frame(layout.sync_button, 4, BLACK);
        c.text_centered(layout.sync_button.inset(8), 8, "SYNC", BLACK);
    }
    c.fill(layout.new_button, BLACK);
    c.text_centered(layout.new_button.inset(8), 8, "+ NEW", WHITE);
    c.frame(layout.tools_button, 4, BLACK);
    draw_tools_icon(&mut c, layout.tools_button.inset(28));
    c.fill(
        Rect::new(MARGIN, DIVIDER_Y, geo.xres.saturating_sub(2 * MARGIN), 4),
        BLACK,
    );

    for (index, tile, menu) in &layout.folders {
        let f = &folders[*index];
        c.frame(*tile, 4, BLACK);
        c.fill(
            Rect::new(menu.x, menu.y + 30, 4, menu.h.saturating_sub(60)),
            BLACK,
        );
        c.dots(Rect::new(menu.x + 4, menu.y, menu.w - 4, menu.h), 18, BLACK);
        let icon = Rect::new(tile.x + 30, tile.y + 30, 90, 70);
        draw_folder_icon(&mut c, icon);
        let text_x = tile.x + 30;
        let text_w = menu.x.saturating_sub(text_x + 20);
        let scale = scale_to_fit(&f.title, TILE_TITLE_SCALE, TILE_TITLE_MIN_SCALE, text_w);
        let title = fit_text(&f.title, scale, text_w);
        c.text(text_x, icon.bottom() + 24, scale, &title, BLACK);
        c.text(
            text_x,
            tile.bottom().saturating_sub(GLYPH_H * TILE_META_SCALE + 26),
            TILE_META_SCALE,
            &notebooks_label(f.notebook_ids.len()),
            BLACK,
        );
    }

    if open_folder.is_some() && rows.is_empty() {
        let r = Rect::new(
            MARGIN,
            CARDS_TOP + 120,
            geo.xres.saturating_sub(2 * MARGIN),
            200,
        );
        c.text_centered(r, 8, "This folder is empty", GRAY);
    }

    for (index, card, menu) in &layout.cards {
        let entry = &cards[*index];
        let selected = *index == state.selected_index;
        c.frame(*card, if selected { 12 } else { 4 }, BLACK);
        if selected {
            c.fill(Rect::new(card.x, card.y, 28, card.h), BLACK);
        }
        // Menu affordance, separated from the open target by a rule.
        c.fill(
            Rect::new(menu.x, menu.y + 30, 4, menu.h.saturating_sub(60)),
            BLACK,
        );
        c.dots(Rect::new(menu.x + 4, menu.y, menu.w - 4, menu.h), 24, BLACK);

        let text_x = card.x + 70;
        let text_w = menu.x.saturating_sub(text_x + 30);
        let title_y = card.y + card.h / 2 - 7 * TITLE_SCALE - 12;
        let title = fit_text(&entry.title, TITLE_SCALE, text_w);
        c.text(text_x, title_y, TITLE_SCALE, &title, BLACK);
        c.text(
            text_x,
            card.y + card.h / 2 + 24,
            META_SCALE,
            &pages_label(entry.page_count),
            BLACK,
        );
    }

    let pages = viewport_page_count(total);
    let prev_ink = if state.can_page_previous() {
        BLACK
    } else {
        GRAY
    };
    let next_ink = if state.can_page_next(total) {
        BLACK
    } else {
        GRAY
    };
    if pages > 1 {
        c.frame(layout.pager_previous, 5, prev_ink);
        c.triangle(
            Rect::new(
                layout.pager_previous.x + PAGER_BUTTON_W / 2 - 40,
                layout.pager_previous.y + 35,
                80,
                PAGER_H - 70,
            ),
            true,
            prev_ink,
        );
        c.frame(layout.pager_next, 5, next_ink);
        c.triangle(
            Rect::new(
                layout.pager_next.x + PAGER_BUTTON_W / 2 - 40,
                layout.pager_next.y + 35,
                80,
                PAGER_H - 70,
            ),
            false,
            next_ink,
        );
    }
    c.text_centered(
        layout.pager_label,
        9,
        &format!("{} of {}", state.page + 1, pages),
        BLACK,
    );

    Ok(HomeRenderSummary {
        visible_entries: layout.cards.len() + layout.folders.len(),
        total_entries: total,
        viewport_page: state.page,
        viewport_pages: pages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_gfx::panel_geo;

    fn cards(n: usize) -> Vec<HomeCard> {
        (1..=n)
            .map(|i| HomeCard {
                notebook_id: format!("notebook-{i}"),
                title: format!("Notebook {i}"),
                page_count: Some(i),
            })
            .collect()
    }

    #[test]
    fn renders_cards_and_replacement_glyph_without_touching_stride_padding() {
        let g = panel_geo();
        let mut fb = vec![0x55; g.smem_len as usize];
        let mut entries = cards(3);
        entries[1].title = "Café".into();
        let summary = render_home(&mut fb, &g, &entries, &HomeState::selecting(0, 3)).unwrap();
        assert_eq!(summary.total_entries, 3);
        assert_eq!(summary.visible_entries, 3);
        assert!(fb.contains(&0x00));
        for y in 0..g.yres as usize {
            let padding =
                &fb[y * g.line_length as usize + g.xres as usize..(y + 1) * g.line_length as usize];
            assert!(padding.iter().all(|b| *b == 0x55));
        }

        let mut g2 = vec![0x55u8; fb.len()];
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

        let mut g1 = vec![0x55u8; fb.len()];
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
        for y in 0..g.yres as usize {
            let row = y * g.line_length as usize;
            for x in 0..g.xres as usize {
                assert_eq!(g1[row + x], !fb[row + x]);
            }
            assert!(g1[row + g.xres as usize..row + g.line_length as usize]
                .iter()
                .all(|b| *b == 0x55));
        }
    }

    #[test]
    fn compact_bar_orders_controls_and_keeps_comfortable_targets() {
        let g = panel_geo();
        let mut st = HomeState::selecting(0, 3);
        st.sync = true;
        let l = HomeLayout::new(&g, &st, 3);
        for r in [
            l.exit_hit,
            l.search,
            l.sync_button,
            l.new_button,
            l.tools_button,
        ] {
            assert!(r.w >= 130 && r.h >= 130, "{r:?}");
        }
        // [exit] [search] [sync] [+ new] [tools], left to right, no overlap.
        assert!(l.exit_hit.right() <= l.search.x);
        assert!(l.search.right() <= l.sync_button.x);
        assert!(l.sync_button.right() <= l.new_button.x);
        assert!(l.new_button.right() <= l.tools_button.x);
        assert!(l.tools_button.right() <= g.xres - MARGIN);
        assert!(l.tools_button.bottom() < DIVIDER_Y && DIVIDER_Y < CARDS_TOP);
        let c = |r: Rect| UiPoint {
            x: r.x + r.w / 2,
            y: r.y + r.h / 2,
        };
        assert_eq!(l.hit(c(l.tools_button), &st, 3), Some(HomeHit::Tools));
        assert_eq!(l.hit(c(l.search), &st, 3), Some(HomeHit::SearchPlaceholder));
        assert_eq!(l.hit(c(l.sync_button), &st, 3), Some(HomeHit::Sync));
        // Without sync the search placeholder takes the freed space.
        st.sync = false;
        let l2 = HomeLayout::new(&g, &st, 3);
        assert!(l2.search.w > l.search.w);
        assert_eq!(
            l2.hit(c(l.sync_button), &st, 3),
            Some(HomeHit::SearchPlaceholder)
        );
    }

    #[test]
    fn home_rejects_all_page_input_routing() {
        assert!(AppView::Notebook.allows_page_input());
        assert!(!AppView::Home.allows_page_input());
        assert!(!AppView::Tools.allows_page_input());
        assert!(!AppView::Preferences.allows_page_input());
        assert!(!AppView::LinkTargetPicker.allows_page_input());
        assert!(!AppView::Sync.allows_page_input());
    }
}
