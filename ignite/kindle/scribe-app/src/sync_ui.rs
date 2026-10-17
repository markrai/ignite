//! D1 Sync screen (Home → SYNC): Scribe Server discovery, pairing and connection state.
//!
//! Framebuffer-native like Home; one GC16 FULL refresh per change. Never shown while writing:
//! it is its own view, reached only from Home. Restrained states: Not paired · Searching for
//! Scribe Server… · Pairing… (comparison code) · Connected · Offline (changes will sync when
//! Scribe Server is available) · Authentication revoked (Pair again).

use control_protocol::ServerId;
use kindle_platform::FbGeometry;

use crate::server_link::{LinkPhase, LinkView};
use crate::ui_gfx::{fit_text, Canvas, Rect, BLACK, GRAY, WHITE};

const MARGIN: u32 = 60;
const BACK_W: u32 = 360;
const BACK_H: u32 = 150;
const TOP: u32 = 40;
const STATUS_Y: u32 = 260;
const LIST_Y: u32 = 720;
const ROW_H: u32 = 230;
const ROW_GAP: u32 = 24;
const PAIR_W: u32 = 380;
const MAX_ROWS: usize = 5;
const MANUAL_H: u32 = 160;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncHit {
    Back,
    Pair(ServerId),
    CancelPairing,
    ManualUrl,
}

/// One server row as drawn (and hit-tested).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub rect: Rect,
    pub pair: Option<Rect>,
    pub server_id: ServerId,
    pub name: String,
    pub subtitle: String,
    pub action: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncLayout {
    pub back: Rect,
    pub rows: Vec<Row>,
    pub cancel: Option<Rect>,
    pub manual: Option<Rect>,
}

fn short_id(id: ServerId) -> String {
    let s = id.to_string();
    format!("ID {}...{}", &s[..8], &s[s.len() - 4..])
}

/// Headline + detail lines for the current state.
pub fn status_lines(v: &LinkView) -> (String, Vec<String>) {
    match &v.phase {
        LinkPhase::NotPaired if v.searching => (
            "Searching for Scribe Server...".into(),
            vec!["Not paired".into()],
        ),
        LinkPhase::NotPaired => ("Not paired".into(), vec![]),
        LinkPhase::Pairing { server_name, .. } => ("Pairing...".into(), vec![server_name.clone()]),
        LinkPhase::Connected { server_name } => ("Connected".into(), vec![server_name.clone()]),
        LinkPhase::Offline { server_name } => (
            "Offline".into(),
            vec![
                server_name.clone(),
                "Changes will sync when Scribe Server is available.".into(),
            ],
        ),
        LinkPhase::Revoked { server_name } => (
            "Authentication revoked".into(),
            vec![server_name.clone(), "Pair again to resume syncing.".into()],
        ),
    }
}

impl SyncLayout {
    pub fn new(geo: &FbGeometry, v: &LinkView) -> Self {
        let width = geo.xres.saturating_sub(2 * MARGIN);
        let back = Rect::new(MARGIN, TOP, BACK_W, BACK_H);
        if let LinkPhase::Pairing { .. } = v.phase {
            let cancel = Rect::new((geo.xres - 600) / 2, geo.yres.saturating_sub(520), 600, 170);
            return Self {
                back,
                rows: Vec::new(),
                cancel: Some(cancel),
                manual: None,
            };
        }
        let revoked = matches!(v.phase, LinkPhase::Revoked { .. });
        let rows = v
            .servers
            .iter()
            .take(MAX_ROWS)
            .enumerate()
            .map(|(i, s)| {
                let y = LIST_Y + 110 + i as u32 * (ROW_H + ROW_GAP);
                let rect = Rect::new(MARGIN, y, width, ROW_H);
                let paired = v.paired_server == Some(s.server_id);
                let action = match (paired, revoked) {
                    (true, true) => "PAIR AGAIN",
                    (true, false) => "",
                    (false, _) => "PAIR",
                };
                let pair = (!action.is_empty()).then(|| {
                    Rect::new(
                        rect.right() - PAIR_W - 30,
                        y + (ROW_H - 150) / 2,
                        PAIR_W,
                        150,
                    )
                });
                Row {
                    rect,
                    pair,
                    server_id: s.server_id,
                    name: s.name.clone(),
                    subtitle: if paired && !revoked {
                        format!("Paired - {}", short_id(s.server_id))
                    } else {
                        format!("Available - {}", short_id(s.server_id))
                    },
                    action,
                }
            })
            .collect();
        let manual = Rect::new(
            MARGIN,
            geo.yres.saturating_sub(MARGIN + MANUAL_H),
            width,
            MANUAL_H,
        );
        Self {
            back,
            rows,
            cancel: None,
            manual: Some(manual),
        }
    }

    pub fn hit(&self, x: u32, y: u32) -> Option<SyncHit> {
        if self.back.contains(x, y) {
            return Some(SyncHit::Back);
        }
        if self.cancel.is_some_and(|c| c.contains(x, y)) {
            return Some(SyncHit::CancelPairing);
        }
        for r in &self.rows {
            if r.pair.is_some_and(|p| p.contains(x, y)) {
                return Some(SyncHit::Pair(r.server_id));
            }
        }
        if self.manual.is_some_and(|m| m.contains(x, y)) {
            return Some(SyncHit::ManualUrl);
        }
        None
    }
}

pub fn render_sync(fb: &mut [u8], geo: &FbGeometry, v: &LinkView) -> Result<usize, String> {
    let layout = SyncLayout::new(geo, v);
    let mut c = Canvas::new(fb, geo)?;
    c.fill_all(WHITE);
    c.frame(layout.back, 6, BLACK);
    c.text_centered(layout.back.inset(10), 9, "< HOME", BLACK);
    c.text(MARGIN + BACK_W + 80, TOP + 40, 14, "SYNC", BLACK);
    let width = geo.xres.saturating_sub(2 * MARGIN);

    let (headline, details) = status_lines(v);
    c.text(MARGIN, STATUS_Y, 11, &fit_text(&headline, 11, width), BLACK);
    for (i, d) in details.iter().enumerate() {
        c.text(
            MARGIN,
            STATUS_Y + 120 + i as u32 * 80,
            7,
            &fit_text(d, 7, width),
            BLACK,
        );
    }
    if let Some(n) = &v.notice {
        c.text(MARGIN, STATUS_Y + 300, 7, &fit_text(n, 7, width), BLACK);
    }

    if let LinkPhase::Pairing { server_name, code } = &v.phase {
        let panel = Rect::new(MARGIN, 760, width, 900);
        c.frame(panel, 8, BLACK);
        c.text_centered(
            Rect::new(panel.x, panel.y + 60, panel.w, 100),
            8,
            "Approve this code in Ignite",
            BLACK,
        );
        c.text_centered(
            Rect::new(panel.x, panel.y + 200, panel.w, 100),
            7,
            &fit_text(server_name, 7, panel.w - 80),
            BLACK,
        );
        c.text_centered(
            Rect::new(panel.x, panel.y + 400, panel.w, 300),
            30,
            code,
            BLACK,
        );
        if let Some(cancel) = layout.cancel {
            c.frame(cancel, 6, BLACK);
            c.text_centered(cancel.inset(10), 9, "CANCEL", BLACK);
        }
        return Ok(0);
    }

    c.text(MARGIN, LIST_Y, 8, "Available servers", BLACK);
    c.fill(Rect::new(MARGIN, LIST_Y + 80, width, 4), BLACK);
    if layout.rows.is_empty() {
        let msg = if v.searching {
            "Looking on this network..."
        } else {
            "No servers found."
        };
        c.text(MARGIN, LIST_Y + 140, 7, msg, GRAY);
    }
    for r in &layout.rows {
        c.frame(r.rect, 4, BLACK);
        let text_w = r
            .pair
            .map_or(r.rect.w - 80, |p| p.x.saturating_sub(r.rect.x + 70));
        c.text(
            r.rect.x + 40,
            r.rect.y + 50,
            9,
            &fit_text(&r.name, 9, text_w),
            BLACK,
        );
        c.text(
            r.rect.x + 40,
            r.rect.y + 140,
            6,
            &fit_text(&r.subtitle, 6, text_w),
            BLACK,
        );
        if let Some(p) = r.pair {
            c.fill(p, BLACK);
            c.text_centered(p.inset(10), 8, r.action, WHITE);
        }
    }
    if let Some(m) = layout.manual {
        c.frame(m, 4, BLACK);
        c.text_centered(m.inset(10), 7, "Add server manually", BLACK);
    }
    Ok(layout.rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::DiscoveredServer;
    use crate::ui_gfx::panel_geo;

    fn sid(n: u8) -> ServerId {
        ServerId::from_parts(1_790_000_000_000, &[n; 10])
    }

    fn view(phase: LinkPhase, servers: &[(u8, &str)], paired: Option<u8>) -> LinkView {
        LinkView {
            phase,
            searching: true,
            servers: servers
                .iter()
                .map(|(n, name)| DiscoveredServer {
                    server_id: sid(*n),
                    name: (*name).into(),
                    endpoint: "http://x".into(),
                    instance: "i".into(),
                })
                .collect(),
            paired_server: paired.map(sid),
            notice: None,
            version: 1,
        }
    }

    #[test]
    fn rows_pair_buttons_and_hits_map_to_server_ids() {
        let g = panel_geo();
        let v = view(
            LinkPhase::NotPaired,
            &[(1, "Test Server B"), (2, "Test Server B")],
            None,
        );
        let l = SyncLayout::new(&g, &v);
        assert_eq!(l.rows.len(), 2);
        let p = l.rows[1].pair.unwrap();
        assert_eq!(l.hit(p.x + 5, p.y + 5), Some(SyncHit::Pair(sid(2))));
        assert_eq!(l.hit(l.back.x + 5, l.back.y + 5), Some(SyncHit::Back));
        let m = l.manual.unwrap();
        assert_eq!(l.hit(m.x + 5, m.y + 5), Some(SyncHit::ManualUrl));
        // Two servers with the same name are told apart by ServerId.
        assert_ne!(l.rows[0].subtitle, l.rows[1].subtitle);
        let mut fb = vec![0x55; g.smem_len as usize];
        assert_eq!(render_sync(&mut fb, &g, &v).unwrap(), 2);
    }

    #[test]
    fn states_are_restrained_and_pairing_shows_the_code() {
        let g = panel_geo();
        let connected = view(
            LinkPhase::Connected {
                server_name: "Test Server B".into(),
            },
            &[(1, "Test Server B")],
            Some(1),
        );
        let l = SyncLayout::new(&g, &connected);
        assert!(
            l.rows[0].pair.is_none(),
            "the paired server has no Pair button"
        );
        let revoked = view(
            LinkPhase::Revoked {
                server_name: "Test Server B".into(),
            },
            &[(1, "Test Server B")],
            Some(1),
        );
        assert_eq!(SyncLayout::new(&g, &revoked).rows[0].action, "PAIR AGAIN");
        assert_eq!(status_lines(&revoked).0, "Authentication revoked");
        let offline = view(
            LinkPhase::Offline {
                server_name: "S".into(),
            },
            &[],
            Some(1),
        );
        assert!(status_lines(&offline)
            .1
            .iter()
            .any(|l| l.contains("Changes will sync")));
        let pairing = view(
            LinkPhase::Pairing {
                server_name: "Test Server B".into(),
                code: "482 731".into(),
            },
            &[(1, "x")],
            None,
        );
        let l = SyncLayout::new(&g, &pairing);
        assert!(l.rows.is_empty() && l.cancel.is_some());
        let c = l.cancel.unwrap();
        assert_eq!(l.hit(c.x + 1, c.y + 1), Some(SyncHit::CancelPairing));
        let mut fb = vec![0x55; g.smem_len as usize];
        render_sync(&mut fb, &g, &pairing).unwrap();
        assert!(fb.contains(&0x00));
    }
}
