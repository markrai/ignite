//! D1 Sync screen (Home → SYNC): Scribe Server discovery, pairing and connection state.
//!
//! Framebuffer-native like Home; one GC16 FULL refresh per change. Never shown while writing:
//! it is its own view, reached only from Home. Restrained states: Not paired · Searching for
//! Scribe Server… · Pairing… (comparison code) · Connected · Offline (changes will sync when
//! Scribe Server is available) · Authentication revoked (Pair again).
//!
//! 1H-D2 **Restore from Server** (`LinkView::restore`): choose server → (provisional) pairing →
//! libraries → one confirmation → restoring → done (restart) or failed. CANCEL exists only
//! until the replacement commits; nothing on these screens shows IDs or hashes.

use control_protocol::ServerId;
use kindle_platform::FbGeometry;

use crate::server_link::{LibraryChoice, LinkPhase, LinkView, RestorePhase};
use crate::ui_gfx::{fit_text, wrap_text, Canvas, Rect, BLACK, GRAY, WHITE};
use sync_protocol::LibraryId;

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
    /// Normal screen: start Restore from Server.
    Restore,
    RestoreServer(ServerId),
    RestoreLibrary(LibraryId),
    RestoreConfirm,
    RestoreCancel,
    /// Acknowledge a failed restore.
    RestoreOk,
}

/// One library row of the restore flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryRow {
    pub rect: Rect,
    pub select: Rect,
    pub library_id: LibraryId,
    pub title: String,
    pub subtitle: String,
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
    /// "< HOME" (normal screen) or the restore flow's CANCEL; absent when leaving is not allowed.
    pub back: Option<Rect>,
    pub rows: Vec<Row>,
    pub cancel: Option<Rect>,
    pub manual: Option<Rect>,
    /// Normal screen: "Restore from server".
    pub restore: Option<Rect>,
    pub libraries: Vec<LibraryRow>,
    /// Confirmation: RESTORE.
    pub confirm: Option<Rect>,
    /// Failure: OK.
    pub ok: Option<Rect>,
}

/// "24 notebooks - 71 pages".
pub fn counts(l: &LibraryChoice) -> String {
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    format!(
        "{} - {}",
        plural(l.notebooks, "notebook", "notebooks"),
        plural(l.pages, "page", "pages")
    )
}

/// Whether this build offers Restore from Server: the `restore_from_server` feature, which the
/// official ARM release build enables (and host tests).
pub const RESTORE_FROM_SERVER: bool = cfg!(any(test, feature = "restore_from_server"));

pub const RESTORE_QUESTION: &str = "Restore this library?";
pub const RESTORE_WARNING: &str = "Your notebooks on this Kindle will be replaced with the selected server's copy. Changes not on the server will be discarded.";

/// Headline + detail lines of a restore phase.
pub fn restore_lines(r: &RestorePhase) -> (String, Vec<String>) {
    match r {
        RestorePhase::ChooseServer => (
            "Restore from server".into(),
            vec!["Choose the server that holds your library.".into()],
        ),
        RestorePhase::Pairing { server_name, .. } => {
            ("Pairing...".into(), vec![server_name.clone()])
        }
        RestorePhase::Loading { server_name } => (
            "Restore from server".into(),
            vec![server_name.clone(), "Looking for libraries...".into()],
        ),
        RestorePhase::Libraries { server_name, .. } => {
            ("Choose a library".into(), vec![server_name.clone()])
        }
        RestorePhase::Confirm { server_name, .. } => {
            (RESTORE_QUESTION.into(), vec![server_name.clone()])
        }
        RestorePhase::Working { status, .. } => (
            "Restoring...".into(),
            vec![status.clone(), "Keep Ignite open.".into()],
        ),
        RestorePhase::Failed { message } => {
            ("Restore did not complete".into(), vec![message.clone()])
        }
        RestorePhase::Done => (
            "Library restored".into(),
            vec!["Ignite is restarting...".into()],
        ),
    }
}

fn library_rows(geo: &FbGeometry, libs: &[LibraryChoice]) -> Vec<LibraryRow> {
    let width = geo.xres.saturating_sub(2 * MARGIN);
    libs.iter()
        .take(MAX_ROWS)
        .enumerate()
        .map(|(i, l)| {
            let y = LIST_Y + 110 + i as u32 * (ROW_H + ROW_GAP);
            let rect = Rect::new(MARGIN, y, width, ROW_H);
            LibraryRow {
                rect,
                select: Rect::new(
                    rect.right() - PAIR_W - 30,
                    y + (ROW_H - 150) / 2,
                    PAIR_W,
                    150,
                ),
                library_id: l.library_id,
                title: l.label.clone(),
                subtitle: if l.on_this_kindle {
                    format!("{} - on this Kindle", counts(l))
                } else {
                    counts(l)
                },
            }
        })
        .collect()
}

/// Bottom-centred button (pairing CANCEL, restore CANCEL / OK).
fn bottom_button(geo: &FbGeometry) -> Rect {
    Rect::new((geo.xres - 600) / 2, geo.yres.saturating_sub(520), 600, 170)
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
    fn empty(back: Option<Rect>) -> Self {
        Self {
            back,
            rows: Vec::new(),
            cancel: None,
            manual: None,
            restore: None,
            libraries: Vec::new(),
            confirm: None,
            ok: None,
        }
    }

    fn for_restore(geo: &FbGeometry, v: &LinkView, r: &RestorePhase) -> Self {
        let width = geo.xres.saturating_sub(2 * MARGIN);
        let top = Rect::new(MARGIN, TOP, BACK_W, BACK_H);
        match r {
            RestorePhase::ChooseServer => {
                let mut l = Self::server_list(geo, v, Some(top), "SELECT");
                l.restore = None;
                l
            }
            RestorePhase::Pairing { .. } => Self {
                cancel: Some(bottom_button(geo)),
                ..Self::empty(Some(top))
            },
            RestorePhase::Loading { .. } => Self::empty(Some(top)),
            RestorePhase::Libraries { libraries, .. } => Self {
                libraries: library_rows(geo, libraries),
                ..Self::empty(Some(top))
            },
            RestorePhase::Confirm { library, .. } => {
                let y = geo.yres.saturating_sub(520);
                let half = (width - 60) / 2;
                Self {
                    cancel: Some(Rect::new(MARGIN, y, half, 170)),
                    confirm: (!library.on_this_kindle)
                        .then(|| Rect::new(MARGIN + half + 60, y, half, 170)),
                    ..Self::empty(Some(top))
                }
            }
            RestorePhase::Working { cancellable, .. } => Self {
                cancel: cancellable.then(|| bottom_button(geo)),
                ..Self::empty(None)
            },
            RestorePhase::Failed { .. } => Self {
                ok: Some(bottom_button(geo)),
                ..Self::empty(None)
            },
            RestorePhase::Done => Self::empty(None),
        }
    }

    pub fn new(geo: &FbGeometry, v: &LinkView) -> Self {
        if let Some(r) = &v.restore {
            return Self::for_restore(geo, v, r);
        }
        let back = Rect::new(MARGIN, TOP, BACK_W, BACK_H);
        if let LinkPhase::Pairing { .. } = v.phase {
            return Self {
                cancel: Some(bottom_button(geo)),
                ..Self::empty(Some(back))
            };
        }
        Self::server_list(geo, v, Some(back), "")
    }

    /// The server list (normal screen, or the restore flow's server choice with `select`).
    fn server_list(
        geo: &FbGeometry,
        v: &LinkView,
        back: Option<Rect>,
        select: &'static str,
    ) -> Self {
        let width = geo.xres.saturating_sub(2 * MARGIN);
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
                    _ if !select.is_empty() => select,
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
        // Bottom line: manual address, and (normal screen) Restore from server beside it.
        let y = geo.yres.saturating_sub(MARGIN + MANUAL_H);
        let (manual, restore) = if select.is_empty() && RESTORE_FROM_SERVER {
            let half = (width - 40) / 2;
            (
                Rect::new(MARGIN, y, half, MANUAL_H),
                Some(Rect::new(MARGIN + half + 40, y, half, MANUAL_H)),
            )
        } else {
            (Rect::new(MARGIN, y, width, MANUAL_H), None)
        };
        Self {
            rows,
            manual: Some(manual),
            restore,
            ..Self::empty(back)
        }
    }

    pub fn hit(&self, x: u32, y: u32) -> Option<SyncHit> {
        self.hit_in(None, x, y)
    }

    /// Hit-test against what was drawn for `v` (restore-flow buttons map to restore actions).
    pub fn hit_view(&self, v: &LinkView, x: u32, y: u32) -> Option<SyncHit> {
        self.hit_in(v.restore.as_ref(), x, y)
    }

    fn hit_in(&self, restore: Option<&RestorePhase>, x: u32, y: u32) -> Option<SyncHit> {
        let inside = |r: Option<Rect>| r.is_some_and(|r| r.contains(x, y));
        if inside(self.back) {
            return Some(if restore.is_some() {
                SyncHit::RestoreCancel
            } else {
                SyncHit::Back
            });
        }
        if inside(self.cancel) {
            return Some(if restore.is_some() {
                SyncHit::RestoreCancel
            } else {
                SyncHit::CancelPairing
            });
        }
        if inside(self.confirm) {
            return Some(SyncHit::RestoreConfirm);
        }
        if inside(self.ok) {
            return Some(SyncHit::RestoreOk);
        }
        for r in &self.rows {
            if inside(r.pair) {
                return Some(if restore.is_some() {
                    SyncHit::RestoreServer(r.server_id)
                } else {
                    SyncHit::Pair(r.server_id)
                });
            }
        }
        for l in &self.libraries {
            if l.select.contains(x, y) {
                return Some(SyncHit::RestoreLibrary(l.library_id));
            }
        }
        if inside(self.manual) {
            return Some(SyncHit::ManualUrl);
        }
        if inside(self.restore) {
            return Some(SyncHit::Restore);
        }
        None
    }
}

pub fn render_sync(fb: &mut [u8], geo: &FbGeometry, v: &LinkView) -> Result<usize, String> {
    let layout = SyncLayout::new(geo, v);
    let mut c = Canvas::new(fb, geo)?;
    c.fill_all(WHITE);
    if let Some(back) = layout.back {
        c.frame(back, 6, BLACK);
        let label = if v.restore.is_some() {
            "CANCEL"
        } else {
            "< HOME"
        };
        c.text_centered(back.inset(10), 9, label, BLACK);
    }
    let title = if v.restore.is_some() {
        "RESTORE"
    } else {
        "SYNC"
    };
    c.text(MARGIN + BACK_W + 80, TOP + 40, 14, title, BLACK);
    let width = geo.xres.saturating_sub(2 * MARGIN);
    if let Some(r) = &v.restore {
        return render_restore(&mut c, geo, v, r, &layout);
    }

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
    draw_server_rows(&mut c, &layout);
    if let Some(r) = layout.restore {
        c.frame(r, 4, BLACK);
        c.text_centered(r.inset(10), 7, "Restore from server", BLACK);
    }
    Ok(layout.rows.len())
}

fn draw_server_rows(c: &mut Canvas<'_>, layout: &SyncLayout) {
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
}

fn render_restore(
    c: &mut Canvas<'_>,
    geo: &FbGeometry,
    v: &LinkView,
    r: &RestorePhase,
    layout: &SyncLayout,
) -> Result<usize, String> {
    let width = geo.xres.saturating_sub(2 * MARGIN);
    let (headline, details) = restore_lines(r);
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
    let button = |c: &mut Canvas<'_>, rect: Option<Rect>, label: &str, filled: bool| {
        if let Some(b) = rect {
            if filled {
                c.fill(b, BLACK);
                c.text_centered(b.inset(10), 9, label, WHITE);
            } else {
                c.frame(b, 6, BLACK);
                c.text_centered(b.inset(10), 9, label, BLACK);
            }
        }
    };
    match r {
        RestorePhase::ChooseServer => {
            c.text(MARGIN, LIST_Y, 8, "Available servers", BLACK);
            c.fill(Rect::new(MARGIN, LIST_Y + 80, width, 4), BLACK);
            if layout.rows.is_empty() {
                c.text(MARGIN, LIST_Y + 140, 7, "Looking on this network...", GRAY);
            }
            draw_server_rows(c, layout);
        }
        RestorePhase::Pairing { server_name, code } => {
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
        }
        RestorePhase::Libraries { .. } => {
            c.text(MARGIN, LIST_Y, 8, "Libraries on this server", BLACK);
            c.fill(Rect::new(MARGIN, LIST_Y + 80, width, 4), BLACK);
            for l in &layout.libraries {
                c.frame(l.rect, 4, BLACK);
                let text_w = l.select.x.saturating_sub(l.rect.x + 70);
                c.text(
                    l.rect.x + 40,
                    l.rect.y + 50,
                    9,
                    &fit_text(&l.title, 9, text_w),
                    BLACK,
                );
                c.text(
                    l.rect.x + 40,
                    l.rect.y + 140,
                    6,
                    &fit_text(&l.subtitle, 6, text_w),
                    BLACK,
                );
                c.fill(l.select, BLACK);
                c.text_centered(l.select.inset(10), 8, "SELECT", WHITE);
            }
        }
        RestorePhase::Confirm { library, .. } => {
            let panel = Rect::new(MARGIN, 700, width, 900);
            c.frame(panel, 8, BLACK);
            c.text(
                panel.x + 50,
                panel.y + 60,
                9,
                &fit_text(
                    &format!("{} - {}", library.label, counts(library)),
                    9,
                    panel.w - 100,
                ),
                BLACK,
            );
            let text = if library.on_this_kindle {
                "This library is already on this Kindle."
            } else {
                RESTORE_WARNING
            };
            for (i, line) in wrap_text(text, 7, panel.w - 100, 6).iter().enumerate() {
                c.text(panel.x + 50, panel.y + 240 + i as u32 * 90, 7, line, BLACK);
            }
        }
        _ => {}
    }
    let cancel_label = match r {
        RestorePhase::Confirm { library, .. } if library.on_this_kindle => "BACK",
        _ => "CANCEL",
    };
    button(c, layout.cancel, cancel_label, false);
    button(c, layout.confirm, "RESTORE", true);
    button(c, layout.ok, "OK", true);
    Ok(layout.rows.len() + layout.libraries.len())
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
            restore: None,
            restart: false,
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
        let b = l.back.unwrap();
        assert_eq!(l.hit(b.x + 5, b.y + 5), Some(SyncHit::Back));
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

    fn lib(n: u8, notebooks: usize, pages: usize, here: bool) -> LibraryChoice {
        LibraryChoice {
            library_id: LibraryId::from_uuid(uuid::Uuid::from_bytes([
                0x01, 0x90, 0, 0, 0, 0, 0x70, n, 0x80, 0, 0, 0, 0, 0, 0, n,
            ]))
            .unwrap(),
            label: format!("Library {n}"),
            notebooks,
            pages,
            on_this_kindle: here,
        }
    }

    fn restore_view(r: RestorePhase) -> LinkView {
        LinkView {
            restore: Some(r),
            ..view(
                LinkPhase::Connected {
                    server_name: "Vega".into(),
                },
                &[(1, "Vega"), (2, "Recovery")],
                Some(1),
            )
        }
    }

    fn tap(l: &SyncLayout, v: &LinkView, r: Option<Rect>) -> Option<SyncHit> {
        let r = r.unwrap();
        l.hit_view(v, r.x + 5, r.y + 5)
    }

    #[test]
    fn the_sync_screen_offers_restore_beside_the_manual_address() {
        let g = panel_geo();
        let v = view(
            LinkPhase::Connected {
                server_name: "Vega".into(),
            },
            &[(1, "Vega")],
            Some(1),
        );
        let l = SyncLayout::new(&g, &v);
        assert_eq!(tap(&l, &v, l.restore), Some(SyncHit::Restore));
        assert_eq!(tap(&l, &v, l.manual), Some(SyncHit::ManualUrl));
        assert!(!l.restore.unwrap().intersects(&l.manual.unwrap()));
        for r in &l.rows {
            assert!(!r.rect.intersects(&l.restore.unwrap()));
        }
        // Not while an ordinary pairing shows its code.
        let p = view(
            LinkPhase::Pairing {
                server_name: "Vega".into(),
                code: "123 456".into(),
            },
            &[],
            None,
        );
        assert_eq!(SyncLayout::new(&g, &p).restore, None);
    }

    #[test]
    fn restore_screens_map_taps_to_restore_actions_only() {
        let g = panel_geo();
        // Server choice: SELECT on every row (the paired one too); the top button cancels.
        let v = restore_view(RestorePhase::ChooseServer);
        let l = SyncLayout::new(&g, &v);
        assert_eq!(l.rows.len(), 2);
        assert!(l.rows.iter().all(|r| r.action == "SELECT"));
        assert_eq!(
            tap(&l, &v, l.rows[1].pair),
            Some(SyncHit::RestoreServer(sid(2)))
        );
        assert_eq!(tap(&l, &v, l.back), Some(SyncHit::RestoreCancel));
        assert_eq!(tap(&l, &v, l.manual), Some(SyncHit::ManualUrl));
        assert_eq!(l.restore, None);

        let v = restore_view(RestorePhase::Pairing {
            server_name: "Recovery".into(),
            code: "1 2".into(),
        });
        let l = SyncLayout::new(&g, &v);
        assert_eq!(tap(&l, &v, l.cancel), Some(SyncHit::RestoreCancel));

        let libs = vec![lib(1, 24, 71, false), lib(2, 3, 9, true)];
        let v = restore_view(RestorePhase::Libraries {
            server_name: "Recovery".into(),
            libraries: libs.clone(),
        });
        let l = SyncLayout::new(&g, &v);
        assert_eq!(l.libraries.len(), 2);
        assert_eq!(l.libraries[0].subtitle, "24 notebooks - 71 pages");
        assert_eq!(
            l.libraries[1].subtitle,
            "3 notebooks - 9 pages - on this Kindle"
        );
        assert_eq!(
            l.hit_view(&v, l.libraries[1].select.x + 5, l.libraries[1].select.y + 5),
            Some(SyncHit::RestoreLibrary(libs[1].library_id))
        );

        let v = restore_view(RestorePhase::Confirm {
            server_name: "Recovery".into(),
            library: libs[0].clone(),
        });
        let l = SyncLayout::new(&g, &v);
        assert_eq!(tap(&l, &v, l.confirm), Some(SyncHit::RestoreConfirm));
        assert_eq!(tap(&l, &v, l.cancel), Some(SyncHit::RestoreCancel));
        // A library already on this Kindle cannot be confirmed.
        let v = restore_view(RestorePhase::Confirm {
            server_name: "Recovery".into(),
            library: libs[1].clone(),
        });
        assert_eq!(SyncLayout::new(&g, &v).confirm, None);

        // While restoring: CANCEL only until the commit; never a way back to Home.
        let v = restore_view(RestorePhase::Working {
            status: "Preparing library...".into(),
            cancellable: true,
        });
        let l = SyncLayout::new(&g, &v);
        assert_eq!(
            (l.back, tap(&l, &v, l.cancel)),
            (None, Some(SyncHit::RestoreCancel))
        );
        let v = restore_view(RestorePhase::Working {
            status: "Finishing...".into(),
            cancellable: false,
        });
        let l = SyncLayout::new(&g, &v);
        assert_eq!(
            (l.back, l.cancel, l.confirm, l.ok),
            (None, None, None, None)
        );
        assert!(l.rows.is_empty() && l.manual.is_none());
        let v = restore_view(RestorePhase::Done);
        let l = SyncLayout::new(&g, &v);
        assert_eq!((l.back, l.cancel, l.ok), (None, None, None));
        let v = restore_view(RestorePhase::Failed {
            message: "Could not reach the server. Nothing was changed.".into(),
        });
        let l = SyncLayout::new(&g, &v);
        assert_eq!(
            (l.back, tap(&l, &v, l.ok)),
            (None, Some(SyncHit::RestoreOk))
        );
    }

    #[test]
    fn every_restore_screen_renders_without_ids() {
        let g = panel_geo();
        let libs = vec![lib(1, 24, 71, false)];
        for r in [
            RestorePhase::ChooseServer,
            RestorePhase::Pairing {
                server_name: "Recovery".into(),
                code: "123 456".into(),
            },
            RestorePhase::Loading {
                server_name: "Recovery".into(),
            },
            RestorePhase::Libraries {
                server_name: "Recovery".into(),
                libraries: libs.clone(),
            },
            RestorePhase::Confirm {
                server_name: "Recovery".into(),
                library: libs[0].clone(),
            },
            RestorePhase::Working {
                status: "Downloading library...".into(),
                cancellable: true,
            },
            RestorePhase::Failed {
                message: "Not enough free space on this Kindle.".into(),
            },
            RestorePhase::Done,
        ] {
            let (head, lines) = restore_lines(&r);
            for t in std::iter::once(&head).chain(&lines) {
                assert!(!t.contains("0190") && !t.contains("sha"), "{t}");
            }
            let mut fb = vec![0x55; g.smem_len as usize];
            render_sync(&mut fb, &g, &restore_view(r)).unwrap();
        }
        assert_eq!(RESTORE_QUESTION, "Restore this library?");
        assert!(wrap_text(RESTORE_WARNING, 7, g.xres - 2 * MARGIN - 100, 6).len() <= 6);
    }
}
