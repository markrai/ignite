//! Gate F11 framebuffer-native Home/library view.
//!
//! Cards with large pen targets, a visible New control, and deterministic viewport paging
//! ([`HomeViewport`] rows per page). The viewport is transient UI state; it never
//! changes `library.meta`. This renderer paints only the live framebuffer; callers submit
//! one GC16 FULL refresh afterward (or a region refresh while search editing).
//!
//! Folders (Kindle-local, `folders.rs`): the main list shows folder tiles first, three per row,
//! then the notebooks in no folder; rows page together, so folders are never repeated on later
//! pages. Opening a folder shows its notebooks in the same stacked layout, with a back arrow.

use kindle_platform::FbGeometry;

use crate::keyboard::{docked_keys_height, SEARCH_KEYBOARD_GAP};
use crate::library::LibraryRepository;
use crate::ui_gfx::{
    fit_text, fit_text_tail, scale_to_fit, text_width, Canvas, Rect, UiPoint, BLACK, GLYPH_H,
    GLYPH_W, GRAY, WHITE,
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
const TILE_META_MIN_SCALE: u32 = 3;
/// Normal folder tile inset and icon (comfortable Home rows).
const TILE_INSET: u32 = 30;
const TILE_ICON_W: u32 = 90;
const TILE_ICON_H: u32 = 70;
const TILE_ICON_TITLE_GAP: u32 = 24;
const TILE_TITLE_META_GAP: u32 = 8;
const TILE_META_BOTTOM_PAD: u32 = 26;
pub const SEARCH_PLACEHOLDER: &str = "Search notebooks...";
/// Clear-search control: same minimum pen target as other Home chrome (≥ 130 px).
const CLEAR_SEARCH_W: u32 = BAR_H;

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

/// Case-insensitive title substring match for Home search (Unicode lowercase).
pub fn notebook_title_matches(title: &str, query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return false;
    }
    title.to_lowercase().contains(&q.to_lowercase())
}

/// Case-insensitive folder-title substring match (independent of child notebooks).
pub fn folder_title_matches(title: &str, query: &str) -> bool {
    notebook_title_matches(title, query)
}

/// How many notebooks in `folder` match `query` (original membership, library order).
/// Used for in-folder search; global search folder tiles show full membership instead.
pub fn folder_match_count(folder: &FolderInfo, cards: &[HomeCard], query: &str) -> usize {
    folder
        .notebook_ids
        .iter()
        .filter_map(|id| cards.iter().find(|c| c.notebook_id == *id))
        .filter(|c| notebook_title_matches(&c.title, query))
        .count()
}

/// Rows of the current view.
///
/// - Main list + `query`: matching notebooks (library order, including foldered), then
///   folder tiles whose *titles* match (stable folder order). A folder is not included
///   merely because a child notebook matches.
/// - Open folder + `query`: matching notebooks in that folder only (in-folder search).
/// - Open folder, no query: that folder's notebooks in library order.
/// - Main list, no query: folder rows first, then unfiled notebooks.
///
/// Global search parks the live query while a folder opened from results is browsed, so
/// the open-folder path sees `query == None` (full membership) until Back restores it.
pub fn home_rows(
    cards: &[HomeCard],
    folders: &[FolderInfo],
    open: Option<&str>,
    query: Option<&str>,
) -> Vec<HomeRow> {
    if let Some(q) = query {
        if let Some(f) = open.and_then(|id| folders.iter().find(|f| f.id == id)) {
            return cards
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    f.notebook_ids.contains(&c.notebook_id) && notebook_title_matches(&c.title, q)
                })
                .map(|(i, _)| HomeRow::Notebook(i))
                .collect();
        }
        let matching_folders: Vec<usize> = folders
            .iter()
            .enumerate()
            .filter(|(_, f)| folder_title_matches(&f.title, q))
            .map(|(i, _)| i)
            .collect();
        return cards
            .iter()
            .enumerate()
            .filter(|(_, c)| notebook_title_matches(&c.title, q))
            .map(|(i, _)| HomeRow::Notebook(i))
            .chain(
                matching_folders
                    .chunks(FOLDERS_PER_ROW)
                    .map(|c| HomeRow::Folders(c.to_vec())),
            )
            .collect();
    }
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

/// Shared Home paging capacity used by state, layout, hit-testing, and paint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HomeViewport {
    pub rows_per_page: usize,
    /// Absolute Y at the bottom of the Home content band (pager sits above this).
    pub content_bottom: u32,
}

impl HomeViewport {
    /// Paging-only capacity (layout Y unused). Prefer [`Self::standard`] when painting.
    pub fn standard_paging() -> Self {
        Self {
            rows_per_page: ROWS_PER_PAGE,
            content_bottom: 0,
        }
    }

    /// Ordinary Home: full panel below the chrome, [`ROWS_PER_PAGE`] rows.
    pub fn standard(geo: &FbGeometry) -> Self {
        Self {
            rows_per_page: ROWS_PER_PAGE,
            content_bottom: geo.yres.saturating_sub(PAGER_BOTTOM_MARGIN),
        }
    }

    /// Home with a bottom-docked search keyboard: derive rows from remaining height.
    pub fn for_search_editing(geo: &FbGeometry) -> Self {
        let content_bottom = geo
            .yres
            .saturating_sub(docked_keys_height().saturating_add(SEARCH_KEYBOARD_GAP));
        let pager_y = content_bottom.saturating_sub(PAGER_H);
        let cards_bottom = pager_y.saturating_sub(CARD_GAP * 2);
        let avail = cards_bottom.saturating_sub(CARDS_TOP);
        // Target ~3 rows on Scribe; clamp so cards stay finger-sized on other panels.
        const MIN_CARD_H: u32 = 200;
        let fitting = if avail < MIN_CARD_H {
            1
        } else {
            ((avail + CARD_GAP) / (MIN_CARD_H + CARD_GAP)).max(1)
        };
        Self {
            rows_per_page: (fitting as usize).clamp(1, ROWS_PER_PAGE),
            content_bottom,
        }
    }

    pub fn page_count(self, total: usize) -> usize {
        total.div_ceil(self.rows_per_page.max(1)).max(1)
    }
}

pub fn viewport_page_count(total: usize) -> usize {
    HomeViewport {
        rows_per_page: ROWS_PER_PAGE,
        content_bottom: 0,
    }
    .page_count(total)
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
        let vp = HomeViewport::standard_paging();
        s.normalize(total, vp);
        s.page = s.selected_index / vp.rows_per_page.max(1);
        s
    }

    /// Keep selection and viewport inside the library. Empty viewport pages are impossible.
    pub fn normalize(&mut self, total: usize, vp: HomeViewport) {
        self.selected_index = self.selected_index.min(total.saturating_sub(1));
        self.page = self.page.min(vp.page_count(total) - 1);
    }

    pub fn ensure_selected_visible(&mut self, total: usize) {
        let vp = HomeViewport::standard_paging();
        self.normalize(total, vp);
        self.page = self.selected_index / vp.rows_per_page.max(1);
        self.reveal = true;
    }

    /// Fit the viewport to the laid-out `rows`. Selection stays a library index; a pending
    /// reveal moves the viewport to the row showing the selected notebook (or its folder tile).
    pub fn settle(
        &mut self,
        rows: &[HomeRow],
        cards: &[HomeCard],
        folders: &[FolderInfo],
        vp: HomeViewport,
    ) {
        let rpp = vp.rows_per_page.max(1);
        self.selected_index = self.selected_index.min(cards.len().saturating_sub(1));
        if std::mem::take(&mut self.reveal) {
            self.page =
                row_of_card(rows, cards, folders, self.selected_index).map_or(0, |row| row / rpp);
        }
        self.page = self.page.min(vp.page_count(rows.len()) - 1);
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

    /// Row indices shown on the current viewport page.
    pub fn visible_range(&self, total: usize, vp: HomeViewport) -> std::ops::Range<usize> {
        let rpp = vp.rows_per_page.max(1);
        let start = (self.page * rpp).min(total);
        start..(start + rpp).min(total)
    }

    pub fn can_page_previous(&self) -> bool {
        self.page > 0
    }

    pub fn can_page_next(&self, total: usize, vp: HomeViewport) -> bool {
        self.page + 1 < vp.page_count(total)
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
    /// Open the search keyboard (main list only).
    Search,
    /// Clear the active title filter (main list, search active).
    ClearSearch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HomeLayout {
    /// Exit icon: exactly one glyph cell (scale `EXIT_SCALE`), centred in `exit_hit`.
    pub exit_icon: Rect,
    /// Square touch target around `exit_icon`.
    pub exit_hit: Rect,
    /// Flexible-width search field (main list). Inside a folder this rect holds the folder title.
    pub search: Rect,
    /// Clear-search control (≥ 130×130) when a filter is active on the main list.
    pub clear_search: Option<Rect>,
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
    /// Viewport capacity used for this layout (must match settle/hit).
    pub viewport: HomeViewport,
    /// Band covering chrome + cards + pager (for region refresh while searching).
    pub content_band: Rect,
    pub pager_previous: Rect,
    pub pager_next: Rect,
    pub pager_label: Rect,
}

impl HomeLayout {
    /// Notebooks only (no folders): one row per library notebook.
    pub fn new(geo: &FbGeometry, state: &HomeState, total: usize) -> Self {
        Self::for_rows(
            geo,
            state,
            &plain_rows(total),
            false,
            false,
            HomeViewport::standard(geo),
        )
    }

    pub fn for_rows(
        geo: &FbGeometry,
        state: &HomeState,
        all: &[HomeRow],
        in_folder: bool,
        search_active: bool,
        vp: HomeViewport,
    ) -> Self {
        let width = geo.xres.saturating_sub(2 * MARGIN);
        let pager_y = vp.content_bottom.saturating_sub(PAGER_H);
        let cards_bottom = pager_y.saturating_sub(CARD_GAP * 2);
        let rows = vp.rows_per_page.max(1) as u32;
        let card_h = (cards_bottom.saturating_sub(CARDS_TOP + (rows - 1) * CARD_GAP)) / rows;
        let per = FOLDERS_PER_ROW as u32;
        let tile_w = width.saturating_sub((per - 1) * TILE_GAP) / per;
        let mut cards = Vec::new();
        let mut folders = Vec::new();
        for (row, index) in state.visible_range(all.len(), vp).enumerate() {
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
        let (search, clear_search) = if search_active && !in_folder {
            let clear_x = search_right.saturating_sub(CLEAR_SEARCH_W);
            let clear_search = Rect::new(clear_x, BAR_Y, CLEAR_SEARCH_W, BAR_H);
            let search_w = clear_x.saturating_sub(BAR_GAP).saturating_sub(search_left);
            (
                Rect::new(search_left, BAR_Y, search_w, BAR_H),
                Some(clear_search),
            )
        } else {
            (
                Rect::new(
                    search_left,
                    BAR_Y,
                    search_right.saturating_sub(search_left),
                    BAR_H,
                ),
                None,
            )
        };
        Self {
            exit_icon,
            exit_hit,
            search,
            clear_search,
            new_button,
            sync_button,
            tools_button,
            cards,
            folders,
            rows_len: all.len(),
            in_folder,
            viewport: vp,
            content_band: Rect::new(0, 0, geo.xres, vp.content_bottom),
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
        // Main list only: clear has priority over the search field. Inside a folder the
        // search rect shows the folder title (and filter hint) and is not a search opener.
        if !self.in_folder {
            if let Some(clear) = self.clear_search {
                if clear.contains(p.x, p.y) {
                    return Some(HomeHit::ClearSearch);
                }
            }
            if self.search.contains(p.x, p.y) {
                return Some(HomeHit::Search);
            }
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
        if self.pager_next.contains(p.x, p.y) && state.can_page_next(total, self.viewport) {
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
    /// Chrome + cards + pager band (for search region refresh).
    pub content_band: Rect,
}

fn pages_label(count: Option<usize>) -> String {
    match count {
        Some(1) => "1 page".into(),
        Some(n) => format!("{n} pages"),
        None => "? pages".into(),
    }
}

/// Notebook-card secondary line: page count, plus parent folder on the main list.
pub fn notebook_meta_label(count: Option<usize>, folder_title: Option<&str>) -> String {
    let pages = pages_label(count);
    match folder_title {
        Some(folder) if !folder.is_empty() => format!("{pages} · {folder}"),
        _ => pages,
    }
}

fn parent_folder_title<'a>(folders: &'a [FolderInfo], notebook_id: &str) -> Option<&'a str> {
    folders
        .iter()
        .find(|f| f.notebook_ids.iter().any(|id| id == notebook_id))
        .map(|f| f.title.as_str())
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
    let tab_h = (r.h / 5).max(1);
    c.fill(Rect::new(r.x, r.y, r.w * 2 / 5, tab_h + 4), BLACK);
    c.frame(
        Rect::new(r.x, r.y + tab_h, r.w, r.h.saturating_sub(tab_h)),
        6,
        BLACK,
    );
}

fn notebooks_label(n: usize) -> String {
    if n == 1 {
        "1 notebook".into()
    } else {
        format!("{n} notebooks")
    }
}

fn notebooks_label_short(n: usize) -> String {
    if n == 1 {
        "1 nb".into()
    } else {
        format!("{n} nbs")
    }
}

/// Folder-tile notebook count text that fits in `width`, preserving the exact numeric count.
///
/// Prefers the full label at the largest scale that fits, then a short "N nb(s)" form, then the
/// bare number. Never truncates digits.
pub fn folder_count_label(n: usize, width: u32) -> (String, u32) {
    let candidates = [notebooks_label(n), notebooks_label_short(n), n.to_string()];
    for label in candidates {
        let scale = scale_to_fit(&label, TILE_META_SCALE, TILE_META_MIN_SCALE, width);
        if text_width(&label, scale) <= width {
            return (label, scale);
        }
    }
    // Extremely narrow tiles: still show the full number at the minimum scale.
    let num = n.to_string();
    (
        num.clone(),
        scale_to_fit(&num, TILE_META_SCALE, TILE_META_MIN_SCALE, width),
    )
}

/// Painted folder-tile content (icon / title / notebook-count). Hit targets stay on the tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FolderTileContent {
    pub icon: Rect,
    pub title: Rect,
    pub meta: Rect,
    pub title_scale: u32,
    pub compact: bool,
}

/// Lay out folder icon, title, and count inside `tile` (text ends before `menu_x`).
///
/// Uses the normal stacked arrangement when height allows; otherwise packs a compact stack
/// from the tile's own height and glyph sizes so title and count cannot overlap.
pub fn folder_tile_content(tile: Rect, menu_x: u32, title: &str) -> FolderTileContent {
    let meta_h = GLYPH_H * TILE_META_SCALE;
    let text_x_normal = tile.x.saturating_add(TILE_INSET);
    let text_w_normal = menu_x
        .saturating_sub(text_x_normal.saturating_add(8))
        .max(1);
    let mut title_scale =
        scale_to_fit(title, TILE_TITLE_SCALE, TILE_TITLE_MIN_SCALE, text_w_normal);
    let normal_need = TILE_INSET
        .saturating_add(TILE_ICON_H)
        .saturating_add(TILE_ICON_TITLE_GAP)
        .saturating_add(GLYPH_H * title_scale)
        .saturating_add(TILE_TITLE_META_GAP)
        .saturating_add(meta_h)
        .saturating_add(TILE_META_BOTTOM_PAD);

    let (icon, title_y, meta_y, text_x, text_w, compact) = if tile.h >= normal_need {
        let icon = Rect::new(
            tile.x.saturating_add(TILE_INSET),
            tile.y.saturating_add(TILE_INSET),
            TILE_ICON_W,
            TILE_ICON_H,
        );
        (
            icon,
            icon.bottom().saturating_add(TILE_ICON_TITLE_GAP),
            tile.bottom()
                .saturating_sub(meta_h.saturating_add(TILE_META_BOTTOM_PAD)),
            text_x_normal,
            text_w_normal,
            false,
        )
    } else {
        // Compact: pad/gap from tile height; place meta then title then icon so they cannot overlap.
        let pad = (tile.h / 12).clamp(4, TILE_INSET);
        let gap = (tile.h / 20).clamp(2, TILE_ICON_TITLE_GAP / 2);
        let text_x = tile.x.saturating_add(pad);
        let text_w = menu_x.saturating_sub(text_x.saturating_add(8)).max(1);
        title_scale = scale_to_fit(title, TILE_TITLE_SCALE, TILE_TITLE_MIN_SCALE, text_w);
        let min_icon_h = (GLYPH_H * 3).min(TILE_ICON_H);
        while title_scale > TILE_TITLE_MIN_SCALE {
            let need = pad
                .saturating_mul(2)
                .saturating_add(min_icon_h)
                .saturating_add(gap.saturating_mul(2))
                .saturating_add(GLYPH_H * title_scale)
                .saturating_add(meta_h);
            if need <= tile.h {
                break;
            }
            title_scale -= 1;
        }
        let title_h = GLYPH_H * title_scale;
        let meta_y = tile.bottom().saturating_sub(pad.saturating_add(meta_h));
        let title_y = meta_y.saturating_sub(gap.saturating_add(title_h));
        let icon_top = tile.y.saturating_add(pad);
        let icon_bottom = title_y.saturating_sub(gap).max(icon_top.saturating_add(1));
        let icon_h = icon_bottom.saturating_sub(icon_top).min(TILE_ICON_H).max(1);
        let icon_w = icon_h
            .saturating_mul(TILE_ICON_W)
            .checked_div(TILE_ICON_H.max(1))
            .unwrap_or(icon_h)
            .clamp(1, TILE_ICON_W);
        let icon = Rect::new(text_x, icon_top, icon_w, icon_h);
        (icon, title_y, meta_y, text_x, text_w, true)
    };

    let shown = fit_text(title, title_scale, text_w);
    let title_h = GLYPH_H * title_scale;
    let title_rect = Rect::new(
        text_x,
        title_y,
        text_width(&shown, title_scale).min(text_w).max(1),
        title_h,
    );
    let meta_rect = Rect::new(text_x, meta_y, text_w, meta_h);
    FolderTileContent {
        icon,
        title: title_rect,
        meta: meta_rect,
        title_scale,
        compact,
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
    render_home_view(fb, geo, cards, &[], None, None, state)
}

/// Options for painting Home (standard or search-editing composer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HomePaintOpts<'a> {
    pub search: Option<&'a str>,
    /// Live draft shown in the search field while the docked keyboard is open.
    pub search_draft: Option<&'a str>,
    pub viewport: HomeViewport,
    /// Draw a caret in the search field (search editing).
    pub search_caret: bool,
}

impl<'a> HomePaintOpts<'a> {
    pub fn standard(geo: &FbGeometry, search: Option<&'a str>) -> Self {
        Self {
            search,
            search_draft: None,
            viewport: HomeViewport::standard(geo),
            search_caret: false,
        }
    }
}

/// Home with folders: the main list (`open == None`) or one open folder.
///
/// On the main list, `search` matches notebook titles and folder titles independently.
/// Inside an open folder, `search` filters that folder's notebooks (in-folder search).
/// Folder chrome shows the folder title plus a filter hint when an in-folder filter is active.
pub fn render_home_view(
    fb: &mut [u8],
    geo: &FbGeometry,
    cards: &[HomeCard],
    folders: &[FolderInfo],
    open: Option<&str>,
    search: Option<&str>,
    state: &HomeState,
) -> Result<HomeRenderSummary, String> {
    render_home_view_with(
        fb,
        geo,
        cards,
        folders,
        open,
        state,
        HomePaintOpts::standard(geo, search),
    )
}

pub fn render_home_view_with(
    fb: &mut [u8],
    geo: &FbGeometry,
    cards: &[HomeCard],
    folders: &[FolderInfo],
    open: Option<&str>,
    state: &HomeState,
    opts: HomePaintOpts<'_>,
) -> Result<HomeRenderSummary, String> {
    let open_folder = open.and_then(|id| folders.iter().find(|f| f.id == id));
    let query = opts.search;
    let rows = home_rows(cards, folders, open_folder.map(|f| f.id.as_str()), query);
    let total = rows.len();
    let mut state = *state;
    let vp = opts.viewport;
    state.settle(&rows, cards, folders, vp);
    let search_active = query.is_some() || opts.search_draft.is_some();
    let layout = HomeLayout::for_rows(
        geo,
        &state,
        &rows,
        open_folder.is_some(),
        search_active && open_folder.is_none(),
        vp,
    );
    let mut c = Canvas::new(fb, geo)?;
    // Full white clear: no stale notebook page, modal, or previous Home frame survives.
    c.fill_all(WHITE);
    let text_y = layout.search.y + (BAR_H - GLYPH_H * 7) / 2;
    if let Some(f) = open_folder {
        draw_back_icon(&mut c, layout.exit_icon);
        let title = if query.is_some() {
            format!("{} (filtered)", f.title)
        } else {
            f.title.clone()
        };
        let name = fit_text(&title, 8, layout.search.w.saturating_sub(40));
        c.text(
            layout.search.x + 20,
            layout.search.y + (BAR_H - GLYPH_H * 8) / 2,
            8,
            &name,
            BLACK,
        );
    } else {
        draw_exit_icon(&mut c, layout.exit_icon);
        c.frame(layout.search, 3, GRAY);
        let label = opts
            .search_draft
            .or(opts.search)
            .unwrap_or(SEARCH_PLACEHOLDER);
        let ink = if opts.search_draft.is_some() || opts.search.is_some() {
            BLACK
        } else {
            GRAY
        };
        let inner_w = layout.search.w.saturating_sub(60);
        let search_text = if opts.search_caret {
            fit_text_tail(label, 7, inner_w.saturating_sub(16))
        } else {
            fit_text(label, 7, inner_w)
        };
        c.text(layout.search.x + 30, text_y, 7, &search_text, ink);
        if opts.search_caret {
            let cursor_x = layout.search.x + 30 + crate::ui_gfx::text_width(&search_text, 7) + 8;
            c.fill(
                Rect::new(cursor_x, text_y.saturating_sub(4), 8, GLYPH_H * 7 + 8),
                BLACK,
            );
        }
        if let Some(clear) = layout.clear_search {
            c.frame(clear, 3, BLACK);
            c.text_centered(clear.inset(8), 8, "X", BLACK);
        }
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
        let content = folder_tile_content(*tile, menu.x, &f.title);
        draw_folder_icon(&mut c, content.icon);
        let text_w = menu.x.saturating_sub(content.title.x.saturating_add(8));
        let title = fit_text(&f.title, content.title_scale, text_w);
        c.text(
            content.title.x,
            content.title.y,
            content.title_scale,
            &title,
            BLACK,
        );
        // Search tiles that matched by folder title show full membership, not hit count.
        let count = f.notebook_ids.len();
        let (meta, meta_scale) = folder_count_label(count, content.meta.w);
        let meta_y = content.meta.y + content.meta.h.saturating_sub(GLYPH_H * meta_scale) / 2;
        c.text(content.meta.x, meta_y, meta_scale, &meta, BLACK);
    }

    if rows.is_empty() {
        let r = Rect::new(
            MARGIN,
            CARDS_TOP + 120,
            geo.xres.saturating_sub(2 * MARGIN),
            200,
        );
        let empty = if query.is_some() {
            "No notebooks found"
        } else if open_folder.is_some() {
            "This folder is empty"
        } else {
            ""
        };
        if !empty.is_empty() {
            c.text_centered(r, 8, empty, GRAY);
        }
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
        let folder_title = if open_folder.is_none() {
            parent_folder_title(folders, &entry.notebook_id)
        } else {
            None
        };
        let meta = fit_text(
            &notebook_meta_label(entry.page_count, folder_title),
            META_SCALE,
            text_w,
        );
        c.text(text_x, card.y + card.h / 2 + 24, META_SCALE, &meta, BLACK);
    }

    let pages = vp.page_count(total);
    let prev_ink = if state.can_page_previous() {
        BLACK
    } else {
        GRAY
    };
    let next_ink = if state.can_page_next(total, vp) {
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
        content_band: layout.content_band,
    })
}

/// Paint Home results with a bottom-docked search keyboard (query lives in the search field).
pub fn render_home_search_composer(
    fb: &mut [u8],
    geo: &FbGeometry,
    cards: &[HomeCard],
    folders: &[FolderInfo],
    open: Option<&str>,
    state: &HomeState,
    editor: &crate::keyboard::TitleEditor,
) -> Result<(HomeRenderSummary, crate::keyboard::KeyboardLayout), String> {
    let draft = editor.text();
    let query = {
        let t = draft.trim();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    };
    let summary = render_home_view_with(
        fb,
        geo,
        cards,
        folders,
        open,
        state,
        HomePaintOpts {
            search: query,
            search_draft: Some(draft),
            viewport: HomeViewport::for_search_editing(geo),
            search_caret: true,
        },
    )?;
    let keyboard = crate::keyboard::KeyboardLayout::docked(geo, editor.symbols());
    let mut c = Canvas::new(fb, geo)?;
    crate::keyboard::paint_docked_search_keys(&mut c, &keyboard, editor);
    Ok((summary, keyboard))
}

/// Shared Home + docked-keyboard geometry for search editing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HomeSearchGeometry {
    pub viewport: HomeViewport,
    pub home: HomeLayout,
    pub keyboard: crate::keyboard::KeyboardLayout,
}

impl HomeSearchGeometry {
    pub fn new(
        geo: &FbGeometry,
        state: &HomeState,
        rows: &[HomeRow],
        in_folder: bool,
        search_active: bool,
        symbols: bool,
    ) -> Self {
        let viewport = HomeViewport::for_search_editing(geo);
        let home = HomeLayout::for_rows(geo, state, rows, in_folder, search_active, viewport);
        let keyboard = crate::keyboard::KeyboardLayout::docked(geo, symbols);
        Self {
            viewport,
            home,
            keyboard,
        }
    }

    /// Dirty band covering chrome, results, pager, and (optionally) keys.
    pub fn dirty_region(&self, include_keys: bool) -> Rect {
        if include_keys {
            Rect::new(
                0,
                0,
                self.home.content_band.w.max(self.keyboard.keys_area.w),
                self.keyboard.keys_area.bottom(),
            )
        } else {
            self.home.content_band
        }
    }
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
        assert_eq!(l.hit(c(l.search), &st, 3), Some(HomeHit::Search));
        assert_eq!(l.hit(c(l.sync_button), &st, 3), Some(HomeHit::Sync));
        // Without sync the search field takes the freed space.
        st.sync = false;
        let l2 = HomeLayout::new(&g, &st, 3);
        assert!(l2.search.w > l.search.w);
        assert_eq!(l2.hit(c(l.sync_button), &st, 3), Some(HomeHit::Search));
    }

    #[test]
    fn notebook_title_matches_is_case_insensitive_substring() {
        assert!(notebook_title_matches("Project Alpha", "alpha"));
        assert!(notebook_title_matches("Café Notes", "café"));
        assert!(!notebook_title_matches("Work", "play"));
        assert!(!notebook_title_matches("Work", "   "));
        assert!(folder_title_matches("Personal Projects", "per"));
        assert!(!folder_title_matches("Journals", "per"));
    }

    fn search_cards() -> Vec<HomeCard> {
        vec![
            HomeCard {
                notebook_id: "notebook-1".into(),
                title: "Alpha".into(),
                page_count: Some(1),
            },
            HomeCard {
                notebook_id: "notebook-2".into(),
                title: "Beta Work".into(),
                page_count: Some(2),
            },
            HomeCard {
                notebook_id: "notebook-3".into(),
                title: "alpha draft".into(),
                page_count: Some(3),
            },
        ]
    }

    #[test]
    fn search_rows_notebook_only_excludes_parent_folder() {
        let cards = search_cards();
        let folders = vec![FolderInfo {
            id: "folder-1".into(),
            title: "Projects".into(),
            notebook_ids: vec!["notebook-3".into()],
        }];
        let rows = home_rows(&cards, &folders, None, Some("ALPHA"));
        assert_eq!(
            rows,
            vec![HomeRow::Notebook(0), HomeRow::Notebook(2)],
            "matching notebooks first (library order, including foldered); parent folder excluded"
        );
        assert!(home_rows(&cards, &folders, None, Some("zzz")).is_empty());
        assert_eq!(
            home_rows(&cards, &folders, Some("folder-1"), Some("ALPHA")),
            vec![HomeRow::Notebook(2)],
            "in-folder search keeps only that folder's matches"
        );
        assert_eq!(
            home_rows(&cards, &folders, Some("folder-1"), None),
            vec![HomeRow::Notebook(2)],
            "open folder without a live filter shows full membership"
        );
        assert_eq!(
            home_rows(&cards, &folders, None, None),
            vec![
                HomeRow::Folders(vec![0]),
                HomeRow::Notebook(0),
                HomeRow::Notebook(1)
            ],
            "unfiltered Home stays folders first, then unfiled notebooks"
        );
    }

    #[test]
    fn search_rows_folder_only_match_by_title_including_empty() {
        let cards = search_cards();
        let folders = vec![
            FolderInfo {
                id: "folder-1".into(),
                title: "Alpha Archive".into(),
                notebook_ids: vec!["notebook-2".into(), "notebook-3".into()],
            },
            FolderInfo {
                id: "folder-2".into(),
                title: "alpha empty".into(),
                notebook_ids: vec![],
            },
        ];
        assert_eq!(
            home_rows(&cards, &folders, None, Some("alpha")),
            vec![
                HomeRow::Notebook(0),
                HomeRow::Notebook(2),
                HomeRow::Folders(vec![0, 1]),
            ],
            "notebooks by title, then folders by title (including empty)"
        );
        assert_eq!(folders[0].notebook_ids.len(), 2);
        assert_eq!(folder_match_count(&folders[0], &cards, "alpha"), 1);
        assert_eq!(
            home_rows(&cards, &folders, None, Some("Archive")),
            vec![HomeRow::Folders(vec![0])],
            "folder-only title match with no notebook hits"
        );
    }

    #[test]
    fn search_rows_keep_title_matched_folders_in_order_and_chunked() {
        let cards: Vec<HomeCard> = (1..=5)
            .map(|i| HomeCard {
                notebook_id: format!("notebook-{i}"),
                title: format!("Note {i}"),
                page_count: Some(i),
            })
            .collect();
        let folders: Vec<FolderInfo> = (1..=4)
            .map(|n| FolderInfo {
                id: format!("folder-{n}"),
                title: format!("Alpha Folder {n}"),
                notebook_ids: vec![format!("notebook-{n}")],
            })
            .collect();
        assert_eq!(
            home_rows(&cards, &folders, None, Some("alpha")),
            vec![HomeRow::Folders(vec![0, 1, 2]), HomeRow::Folders(vec![3]),],
            "folder-title matches only, stable order, three per row"
        );
    }

    #[test]
    fn notebook_meta_label_keeps_page_count_and_adds_folder() {
        assert_eq!(notebook_meta_label(Some(1), None), "1 page");
        assert_eq!(notebook_meta_label(Some(3), None), "3 pages");
        assert_eq!(notebook_meta_label(None, None), "? pages");
        assert_eq!(
            notebook_meta_label(Some(3), Some("Projects")),
            "3 pages · Projects"
        );
        assert_eq!(
            notebook_meta_label(Some(1), Some("Projects")),
            "1 page · Projects"
        );
        assert_eq!(notebook_meta_label(Some(2), Some("")), "2 pages");
    }

    #[test]
    fn notebook_meta_label_truncates_long_folder_names_to_card_width() {
        let label = notebook_meta_label(
            Some(12),
            Some("A very long research folder name that will overflow a card"),
        );
        assert!(label.starts_with("12 pages · "));
        let fitted = fit_text(&label, META_SCALE, 500);
        assert!(fitted.ends_with("..."), "{fitted:?}");
        assert!(fitted.starts_with("12 pages"), "{fitted:?}");
        assert!(text_width(&fitted, META_SCALE) <= 500, "{fitted:?}");
        assert_ne!(fitted, label);
    }

    #[test]
    fn search_rows_on_a_large_library_keep_notebooks_first() {
        let n = 2000usize;
        let folder_count = 80usize;
        let per_folder = n / folder_count;
        let cards: Vec<HomeCard> = (0..n)
            .map(|i| HomeCard {
                notebook_id: format!("notebook-{i}"),
                title: format!("Note {i:04} alpha"),
                page_count: Some(1),
            })
            .collect();
        let folders: Vec<FolderInfo> = (0..folder_count)
            .map(|f| {
                let start = f * per_folder;
                FolderInfo {
                    id: format!("folder-{f}"),
                    // Titles do not match "alpha"; child notebooks do.
                    title: format!("Folder {f}"),
                    notebook_ids: (start..start + per_folder)
                        .map(|i| format!("notebook-{i}"))
                        .collect(),
                }
            })
            .collect();

        let rows = home_rows(&cards, &folders, None, Some("alpha"));
        let expected: Vec<HomeRow> = (0..n).map(HomeRow::Notebook).collect();
        assert_eq!(
            rows, expected,
            "notebook-only matches; parent folders excluded without title hits"
        );
    }

    #[test]
    fn clear_search_hit_is_at_least_130_and_outranks_search_field() {
        let g = panel_geo();
        let st = HomeState::selecting(0, 3);
        let rows = plain_rows(3);
        let l = HomeLayout::for_rows(&g, &st, &rows, false, true, HomeViewport::standard(&g));
        let clear = l.clear_search.expect("clear control when search active");
        assert!(clear.w >= 130 && clear.h >= 130, "{clear:?}");
        assert!(clear.x >= l.search.right());
        let corner = UiPoint {
            x: clear.x + 1,
            y: clear.y + 1,
        };
        assert_eq!(l.hit(corner, &st, 3), Some(HomeHit::ClearSearch));
        let mid_search = UiPoint {
            x: l.search.x + l.search.w / 2,
            y: l.search.y + l.search.h / 2,
        };
        assert_eq!(l.hit(mid_search, &st, 3), Some(HomeHit::Search));
        // Inside a folder the title area stays inert even if search_active is set.
        let folder_l = HomeLayout::for_rows(&g, &st, &rows, true, true, HomeViewport::standard(&g));
        assert!(folder_l.clear_search.is_none());
        let folder_mid = UiPoint {
            x: folder_l.search.x + folder_l.search.w / 2,
            y: folder_l.search.y + folder_l.search.h / 2,
        };
        assert_eq!(folder_l.hit(folder_mid, &st, 3), None);
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
