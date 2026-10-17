//! Settings chrome: peer tabs `TOOLS` and `PREFERENCES` (Home → tools icon).
//!
//! Tools still owns Import Kindle notebooks. Preferences owns replica-local options
//! (side-button action). Neither view is a writing surface.

use kindle_platform::FbGeometry;

use crate::prefs::SideButtonAction;
use crate::ui_gfx::{fit_text, text_width, wrap_text, Canvas, Rect, BLACK, GLYPH_H, GRAY, WHITE};

const MARGIN: u32 = 60;
const BACK_W: u32 = 330;
const BAR_H: u32 = 130;
const TOP: u32 = 40;
const TAB_GAP: u32 = 20;
pub const DIVIDER_Y: u32 = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsTab {
    Tools,
    Preferences,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsChromeHit {
    Back,
    ToolsTab,
    PreferencesTab,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsChrome {
    pub back: Rect,
    pub tools_tab: Rect,
    pub prefs_tab: Rect,
    xres: u32,
    yres: u32,
}

impl SettingsChrome {
    pub fn new(geo: &FbGeometry) -> Self {
        let w = geo.xres.saturating_sub(2 * MARGIN);
        let back = Rect::new(MARGIN, TOP, BACK_W, BAR_H);
        let tabs_x = back.right() + TAB_GAP;
        let tabs_w = w.saturating_sub(BACK_W + TAB_GAP);
        let tab_w = tabs_w.saturating_sub(TAB_GAP) / 2;
        Self {
            back,
            tools_tab: Rect::new(tabs_x, TOP, tab_w, BAR_H),
            prefs_tab: Rect::new(tabs_x + tab_w + TAB_GAP, TOP, tab_w, BAR_H),
            xres: geo.xres,
            yres: geo.yres,
        }
    }

    pub fn hit(&self, x: u32, y: u32, back_enabled: bool) -> Option<SettingsChromeHit> {
        if self.back.contains(x, y) {
            return back_enabled.then_some(SettingsChromeHit::Back);
        }
        if self.tools_tab.contains(x, y) {
            return Some(SettingsChromeHit::ToolsTab);
        }
        if self.prefs_tab.contains(x, y) {
            return Some(SettingsChromeHit::PreferencesTab);
        }
        None
    }
}

/// Product version shown on Settings chrome. Sourced from the repo-root `VERSION` file.
pub fn product_version_label() -> String {
    format!("v{}", include_str!("../../../VERSION").trim())
}

const VERSION_SCALE: u32 = 5;

pub fn paint_settings_chrome(
    c: &mut Canvas<'_>,
    chrome: &SettingsChrome,
    selected: SettingsTab,
    back_enabled: bool,
) {
    let back_ink = if back_enabled { BLACK } else { GRAY };
    c.frame(chrome.back, 5, back_ink);
    c.text_centered(chrome.back.inset(8), 8, "< BACK", back_ink);
    paint_tab(c, chrome.tools_tab, "TOOLS", selected == SettingsTab::Tools);
    paint_tab(
        c,
        chrome.prefs_tab,
        "PREFERENCES",
        selected == SettingsTab::Preferences,
    );
    let label = product_version_label();
    let w = text_width(&label, VERSION_SCALE);
    let h = GLYPH_H * VERSION_SCALE;
    let x = chrome.xres.saturating_sub(MARGIN).saturating_sub(w);
    let y = chrome.yres.saturating_sub(MARGIN).saturating_sub(h);
    c.text(x, y, VERSION_SCALE, &label, GRAY);
}

fn paint_tab(c: &mut Canvas<'_>, r: Rect, label: &str, selected: bool) {
    if selected {
        c.fill(r, BLACK);
        c.text_centered(r.inset(8), 7, label, WHITE);
    } else {
        c.frame(r, 5, BLACK);
        c.text_centered(r.inset(8), 7, label, BLACK);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreferencesHit {
    Chrome(SettingsChromeHit),
    NavigationUndo,
    CreateLink,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreferencesLayout {
    pub chrome: SettingsChrome,
    pub nav_undo: Rect,
    pub create_link: Rect,
}

const SECTION_Y: u32 = 260;
const BODY_Y: u32 = 360;
const CHOICE_Y: u32 = 720;
const CHOICE_H: u32 = 200;
const CHOICE_GAP: u32 = 40;

impl PreferencesLayout {
    pub fn new(geo: &FbGeometry) -> Self {
        let w = geo.xres.saturating_sub(2 * MARGIN);
        Self {
            chrome: SettingsChrome::new(geo),
            nav_undo: Rect::new(MARGIN, CHOICE_Y, w, CHOICE_H),
            create_link: Rect::new(MARGIN, CHOICE_Y + CHOICE_H + CHOICE_GAP, w, CHOICE_H),
        }
    }

    pub fn hit(&self, x: u32, y: u32) -> Option<PreferencesHit> {
        if let Some(h) = self.chrome.hit(x, y, true) {
            return Some(PreferencesHit::Chrome(h));
        }
        if self.nav_undo.contains(x, y) {
            return Some(PreferencesHit::NavigationUndo);
        }
        if self.create_link.contains(x, y) {
            return Some(PreferencesHit::CreateLink);
        }
        None
    }
}

const SIDE_BUTTON_BLURB: &str = "Choose what the pen side button does while you are on a page.";

pub fn render_preferences(
    fb: &mut [u8],
    geo: &FbGeometry,
    action: SideButtonAction,
) -> Result<(), String> {
    let layout = PreferencesLayout::new(geo);
    let mut c = Canvas::new(fb, geo)?;
    c.fill_all(WHITE);
    let w = geo.xres.saturating_sub(2 * MARGIN);
    paint_settings_chrome(&mut c, &layout.chrome, SettingsTab::Preferences, true);
    c.fill(Rect::new(MARGIN, DIVIDER_Y, w, 4), BLACK);
    c.text(MARGIN, SECTION_Y, 9, "SIDE BUTTON", BLACK);
    for (i, line) in wrap_text(SIDE_BUTTON_BLURB, 6, w, 5).iter().enumerate() {
        c.text(MARGIN, BODY_Y + i as u32 * 64, 6, line, BLACK);
    }
    paint_choice(
        &mut c,
        layout.nav_undo,
        "NAVIGATION + UNDO",
        "Page edges turn pages. Center undoes the last stroke.",
        action == SideButtonAction::NavigationUndo,
    );
    paint_choice(
        &mut c,
        layout.create_link,
        "CREATE LINK",
        "A side-button tap starts a link at that spot.",
        action == SideButtonAction::CreateLink,
    );
    Ok(())
}

fn paint_choice(c: &mut Canvas<'_>, r: Rect, title: &str, detail: &str, selected: bool) {
    if selected {
        c.fill(r, BLACK);
        c.text(r.x + 40, r.y + 40, 8, title, WHITE);
        c.text(
            r.x + 40,
            r.y + 120,
            5,
            &fit_text(detail, 5, r.w.saturating_sub(80)),
            WHITE,
        );
    } else {
        c.frame(r, 5, BLACK);
        c.text(r.x + 40, r.y + 40, 8, title, BLACK);
        c.text(
            r.x + 40,
            r.y + 120,
            5,
            &fit_text(detail, 5, r.w.saturating_sub(80)),
            BLACK,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_gfx::panel_geo;

    fn center(r: Rect) -> (u32, u32) {
        (r.x + r.w / 2, r.y + r.h / 2)
    }

    #[test]
    fn tabs_are_peers_and_choices_hit() {
        let g = panel_geo();
        let l = PreferencesLayout::new(&g);
        let (x, y) = center(l.chrome.tools_tab);
        assert_eq!(
            l.hit(x, y),
            Some(PreferencesHit::Chrome(SettingsChromeHit::ToolsTab))
        );
        let (x, y) = center(l.nav_undo);
        assert_eq!(l.hit(x, y), Some(PreferencesHit::NavigationUndo));
        let (x, y) = center(l.create_link);
        assert_eq!(l.hit(x, y), Some(PreferencesHit::CreateLink));
        let mut fb = vec![0x55; g.smem_len as usize];
        render_preferences(&mut fb, &g, SideButtonAction::NavigationUndo).unwrap();
        render_preferences(&mut fb, &g, SideButtonAction::CreateLink).unwrap();
    }

    #[test]
    fn version_label_matches_root_version_file() {
        let raw = include_str!("../../../VERSION").trim();
        assert_eq!(product_version_label(), format!("v{raw}"));
        assert!(!raw.is_empty());
        assert!(raw.chars().all(|c| c.is_ascii_digit() || c == '.'));
    }

    #[test]
    fn version_text_is_not_a_hit_target() {
        let g = panel_geo();
        let chrome = SettingsChrome::new(&g);
        assert_eq!(chrome.hit(g.xres - 10, g.yres - 10, true), None);
        assert_eq!(
            chrome.hit(
                g.xres.saturating_sub(MARGIN),
                g.yres.saturating_sub(MARGIN),
                true
            ),
            None
        );
    }
}
