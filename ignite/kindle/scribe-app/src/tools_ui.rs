//! Tools screen (Home → tools icon). First tool: **Import Kindle notebooks** — a one-way copy of
//! Amazon notebooks into Scribe (`nbk_import`). Framebuffer-native like Sync: its own view, never
//! a writing surface, one GC16 FULL per change.

use kindle_platform::FbGeometry;

use crate::settings_ui::{
    paint_settings_chrome, SettingsChrome, SettingsChromeHit, SettingsTab, DIVIDER_Y,
};
use crate::ui_gfx::{fit_text, wrap_text, Canvas, Rect, BLACK, WHITE};

const MARGIN: u32 = 60;
const SECTION_Y: u32 = 260;
const BODY_Y: u32 = 400;
const STATUS_Y: u32 = 760;
const BUTTON_Y: u32 = 1560;
const BUTTON_H: u32 = 180;
const SECONDARY_Y: u32 = 1780;
const SECONDARY_H: u32 = 150;
const MAX_FAILURE_LINES: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportFailureLine {
    pub title: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolsPhase {
    /// Nothing scanned yet.
    Intro,
    Scanned {
        total: usize,
        eligible: usize,
        already_imported: usize,
    },
    Importing {
        done: usize,
        total: usize,
        current: String,
    },
    Finished {
        imported: usize,
        skipped: usize,
        failed: usize,
        failures: Vec<ImportFailureLine>,
    },
    /// Scan could not run (message for the user; details are logged).
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolsState {
    pub phase: ToolsPhase,
}

impl Default for ToolsState {
    fn default() -> Self {
        Self {
            phase: ToolsPhase::Intro,
        }
    }
}

impl ToolsState {
    pub fn busy(&self) -> bool {
        matches!(self.phase, ToolsPhase::Importing { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolsHit {
    Chrome(SettingsChromeHit),
    Scan,
    ImportAll,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolsLayout {
    pub chrome: SettingsChrome,
    /// `(rect, label, action)`; absent while importing.
    pub primary: Option<(Rect, String, ToolsHit)>,
    pub secondary: Option<(Rect, String, ToolsHit)>,
}

impl ToolsLayout {
    pub fn new(geo: &FbGeometry, state: &ToolsState) -> Self {
        let w = geo.xres.saturating_sub(2 * MARGIN);
        let primary_rect = Rect::new(MARGIN, BUTTON_Y, w, BUTTON_H);
        let secondary_rect = Rect::new(MARGIN, SECONDARY_Y, w, SECONDARY_H);
        let scan_again = || (secondary_rect, "SCAN AGAIN".to_string(), ToolsHit::Scan);
        let (primary, secondary) = match &state.phase {
            ToolsPhase::Intro => (Some((primary_rect, "SCAN".into(), ToolsHit::Scan)), None),
            ToolsPhase::Scanned { eligible, .. } if *eligible > 0 => (
                Some((
                    primary_rect,
                    if *eligible == 1 {
                        "IMPORT 1 NOTEBOOK".into()
                    } else {
                        format!("IMPORT {eligible} NOTEBOOKS")
                    },
                    ToolsHit::ImportAll,
                )),
                Some(scan_again()),
            ),
            ToolsPhase::Importing { .. } => (None, None),
            ToolsPhase::Scanned { .. } | ToolsPhase::Finished { .. } | ToolsPhase::Error(_) => (
                Some((primary_rect, "SCAN AGAIN".into(), ToolsHit::Scan)),
                None,
            ),
        };
        Self {
            chrome: SettingsChrome::new(geo),
            primary,
            secondary,
        }
    }

    /// Taps resolve against what is drawn; Back is inert while an import runs.
    pub fn hit(&self, x: u32, y: u32, state: &ToolsState) -> Option<ToolsHit> {
        if let Some(h) = self.chrome.hit(x, y, !state.busy()) {
            return Some(ToolsHit::Chrome(h));
        }
        [&self.primary, &self.secondary]
            .into_iter()
            .flatten()
            .find(|(r, _, _)| r.contains(x, y))
            .map(|(_, _, h)| *h)
    }
}

pub const EXPLANATION: &str = "Copies your existing Kindle notebooks into Ignite. \
    The original Kindle notebooks are never changed. Each Kindle notebook is imported only once.";

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Headline + detail lines for the current phase.
pub fn status_lines(state: &ToolsState) -> (String, Vec<String>) {
    match &state.phase {
        ToolsPhase::Intro => (
            "Scan to find Kindle notebooks.".into(),
            vec!["Nothing is copied until you choose Import.".into()],
        ),
        ToolsPhase::Scanned {
            total,
            eligible,
            already_imported,
        } => (
            format!(
                "Found {}.",
                plural(*total, "Kindle notebook", "Kindle notebooks")
            ),
            vec![
                format!("{eligible} ready to import"),
                format!("{already_imported} already imported"),
            ],
        ),
        ToolsPhase::Importing {
            done,
            total,
            current,
        } => (
            format!("Importing {} of {total}...", done + 1),
            vec![current.clone(), "Please wait.".into()],
        ),
        ToolsPhase::Finished {
            imported,
            skipped,
            failed,
            failures,
        } => {
            let mut lines = vec![
                format!("Imported: {imported}"),
                format!("Skipped (already imported): {skipped}"),
                format!("Failed: {failed}"),
            ];
            for f in failures.iter().take(MAX_FAILURE_LINES) {
                lines.push(format!("{}: {}", f.title, f.reason));
            }
            if failures.len() > MAX_FAILURE_LINES {
                lines.push(format!(
                    "...and {} more",
                    failures.len() - MAX_FAILURE_LINES
                ));
            }
            ("Import finished.".into(), lines)
        }
        ToolsPhase::Error(msg) => ("Scan failed.".into(), vec![msg.clone()]),
    }
}

/// Short user-facing reason for an import failure code (details stay in the log).
pub fn failure_reason(detail: &str) -> String {
    if detail.starts_with("sync halted") {
        return "sync stopped; import paused".into();
    }
    if detail.starts_with("import cancelled") {
        return "interrupted; try again".into();
    }
    let code = detail.split(':').next().unwrap_or_default();
    match code {
        "NBK_PENDING_ACTIONS" => "open it on the Kindle, then try again".into(),
        "SOURCE_BUSY" | "SOURCE_CHANGED_DURING_READ" => "Kindle was saving it; try again".into(),
        "NBK_CONTAINER" | "NBK_UNSUPPORTED" => "format not supported".into(),
        _ => "could not be imported".into(),
    }
}

pub fn render_tools(fb: &mut [u8], geo: &FbGeometry, state: &ToolsState) -> Result<(), String> {
    let layout = ToolsLayout::new(geo, state);
    let mut c = Canvas::new(fb, geo)?;
    c.fill_all(WHITE);
    let w = geo.xres.saturating_sub(2 * MARGIN);

    paint_settings_chrome(&mut c, &layout.chrome, SettingsTab::Tools, !state.busy());
    c.fill(Rect::new(MARGIN, DIVIDER_Y, w, 4), BLACK);

    c.text(MARGIN, SECTION_Y, 9, "IMPORT KINDLE NOTEBOOKS", BLACK);
    for (i, line) in wrap_text(EXPLANATION, 6, w, 5).iter().enumerate() {
        c.text(MARGIN, BODY_Y + i as u32 * 64, 6, line, BLACK);
    }

    let status = Rect::new(MARGIN, STATUS_Y, w, BUTTON_Y - STATUS_Y - 60);
    c.frame(status, 4, BLACK);
    let (headline, details) = status_lines(state);
    c.text(
        status.x + 40,
        status.y + 40,
        8,
        &fit_text(&headline, 8, status.w - 80),
        BLACK,
    );
    for (i, line) in details.iter().enumerate() {
        let y = status.y + 150 + i as u32 * 70;
        if y + 50 > status.bottom() {
            break;
        }
        c.text(
            status.x + 40,
            y,
            6,
            &fit_text(line, 6, status.w - 80),
            BLACK,
        );
    }

    if let Some((r, label, _)) = &layout.primary {
        c.fill(*r, BLACK);
        c.text_centered(r.inset(10), 9, label, WHITE);
    }
    if let Some((r, label, _)) = &layout.secondary {
        c.frame(*r, 5, BLACK);
        c.text_centered(r.inset(10), 8, label, BLACK);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_gfx::panel_geo;

    fn center(r: Rect) -> (u32, u32) {
        (r.x + r.w / 2, r.y + r.h / 2)
    }

    #[test]
    fn each_phase_offers_the_right_action_and_back_is_inert_while_importing() {
        let g = panel_geo();
        let intro = ToolsState::default();
        let l = ToolsLayout::new(&g, &intro);
        let (x, y) = center(l.primary.as_ref().unwrap().0);
        assert_eq!(l.hit(x, y, &intro), Some(ToolsHit::Scan));
        let (bx, by) = center(l.chrome.back);
        assert_eq!(
            l.hit(bx, by, &intro),
            Some(ToolsHit::Chrome(SettingsChromeHit::Back))
        );

        let scanned = ToolsState {
            phase: ToolsPhase::Scanned {
                total: 3,
                eligible: 2,
                already_imported: 1,
            },
        };
        let l = ToolsLayout::new(&g, &scanned);
        let (label, hit) = (
            &l.primary.as_ref().unwrap().1,
            l.primary.as_ref().unwrap().2,
        );
        assert_eq!(
            (label.as_str(), hit),
            ("IMPORT 2 NOTEBOOKS", ToolsHit::ImportAll)
        );
        let (x, y) = center(l.secondary.as_ref().unwrap().0);
        assert_eq!(l.hit(x, y, &scanned), Some(ToolsHit::Scan));

        let busy = ToolsState {
            phase: ToolsPhase::Importing {
                done: 0,
                total: 2,
                current: "Journal".into(),
            },
        };
        let l = ToolsLayout::new(&g, &busy);
        assert!(l.primary.is_none() && l.secondary.is_none());
        assert_eq!(l.hit(bx, by, &busy), None);
    }

    #[test]
    fn renders_every_phase_without_touching_stride_padding() {
        let g = panel_geo();
        for phase in [
            ToolsPhase::Intro,
            ToolsPhase::Scanned {
                total: 21,
                eligible: 21,
                already_imported: 0,
            },
            ToolsPhase::Importing {
                done: 3,
                total: 21,
                current: "Rumi Scratch Pad".into(),
            },
            ToolsPhase::Finished {
                imported: 19,
                skipped: 1,
                failed: 8,
                failures: (0..8)
                    .map(|i| ImportFailureLine {
                        title: format!("Notebook {i}"),
                        reason: failure_reason("NBK_PENDING_ACTIONS: x"),
                    })
                    .collect(),
            },
            ToolsPhase::Error("Kindle notebook catalog unreadable".into()),
        ] {
            let mut fb = vec![0x55; g.smem_len as usize];
            render_tools(&mut fb, &g, &ToolsState { phase }).unwrap();
            for y in 0..g.yres as usize {
                let pad = &fb[y * g.line_length as usize + g.xres as usize
                    ..(y + 1) * g.line_length as usize];
                assert!(pad.iter().all(|b| *b == 0x55));
            }
        }
        assert_eq!(
            status_lines(&ToolsState {
                phase: ToolsPhase::Scanned {
                    total: 1,
                    eligible: 1,
                    already_imported: 0
                }
            })
            .0,
            "Found 1 Kindle notebook."
        );
    }
}
