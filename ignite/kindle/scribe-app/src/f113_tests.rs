//! Gate F11.3 host tests: fullscreen writing, finger reveal/hide, 3 s auto-hide, pen dismiss
//! + write in one contact, stylus-proximity palm poisoning, ink under chrome, sleep/wake.

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use kindle_platform::{FbGeometry, StylusEventKind, StylusTool, TouchContact};
    use notebook_core::{PagePoint, Point, Stroke, StrokeId};

    use crate::app_ui::{DownRoute, NotebookUiContext, PenRouter, Redraw, UiCommand, UiState};
    use crate::chrome::{ChromeLayout, PageChrome, BOTTOM_BAND_H, TOP_BAND_H};
    use crate::display_events::DisplayEvents;
    use crate::fullscreen::{
        compose_notebook_layers, ChromeVisibility, NotebookChromeState, CHROME_AUTO_HIDE_MS,
    };
    use crate::home::{AppView, HomeCard, HomeLayout};
    use crate::ink::InkRequest;
    use crate::modal::Modal;
    use crate::notebook::open_or_create_notebook;
    use crate::orch::{FakeTransport, StrokeController};
    use crate::profile::RenderProfile;
    use crate::store::{load_existing_page, page_dir, PageStore};
    use crate::touch::{FingerAction, TouchArbiter, TouchMapping, STYLUS_QUIET_MS};
    use crate::ui_gfx::{panel_geo, paper_background, Rect, UiPoint, WHITE};

    const PAGE: PageChrome = PageChrome { index: 2, count: 3 };

    // ---------------------------------------------------------------- fixtures

    fn cards(n: usize) -> Vec<HomeCard> {
        (1..=n)
            .map(|i| HomeCard {
                notebook_id: format!("notebook-{i}"),
                title: format!("Notebook {i}"),
                page_count: Some(3),
            })
            .collect()
    }

    fn center(r: Rect) -> UiPoint {
        UiPoint {
            x: r.x + r.w / 2,
            y: r.y + r.h / 2,
        }
    }

    fn pt(x: u32, y: u32) -> UiPoint {
        UiPoint { x, y }
    }

    fn identity() -> TouchMapping {
        TouchMapping::new((0, 1859), (0, 2479), &panel_geo()).unwrap()
    }

    fn contact(p: UiPoint) -> TouchContact {
        TouchContact {
            x: Some(p.x as i32),
            y: Some(p.y as i32),
            ..TouchContact::default()
        }
    }

    /// Normal notebook entry: fullscreen (chrome hidden).
    fn notebook() -> UiState {
        let mut ui = UiState::launch_home(0, 1);
        ui.enter_notebook();
        ui
    }

    fn ctx() -> Option<NotebookUiContext> {
        Some(NotebookUiContext { page: PAGE })
    }

    fn line(from: (u32, u32), to: (u32, u32), steps: u32) -> Vec<UiPoint> {
        (0..=steps)
            .map(|i| UiPoint {
                x: (from.0 as i64 + (to.0 as i64 - from.0 as i64) * i as i64 / steps as i64) as u32,
                y: (from.1 as i64 + (to.1 as i64 - from.1 as i64) * i as i64 / steps as i64) as u32,
            })
            .collect()
    }

    /// One finger contact through the runtime's touch policy entry point (`TouchArbiter`),
    /// resolved through `UiState::on_finger` exactly as the runtime does. Returns every
    /// non-`None` command (at most one is allowed) and the number of raw finger actions.
    fn finger(
        ui: &mut UiState,
        arb: &mut TouchArbiter,
        path: &[UiPoint],
        t0: u64,
        dur_ms: u64,
    ) -> (Vec<UiCommand>, usize) {
        let g = panel_geo();
        let m = identity();
        let n = path.len().max(2) as u64 - 1;
        let mut actions = Vec::new();
        for (i, p) in path.iter().enumerate() {
            let ms = t0 + dur_ms * i as u64 / n;
            actions.extend(arb.on_frame(&[contact(*p)], &m, ms, true, ui, &g));
        }
        actions.extend(arb.on_frame(&[], &m, t0 + dur_ms, true, ui, &g));
        assert!(actions.len() <= 1, "more than one action: {actions:?}");
        let mut cmds = Vec::new();
        for a in &actions {
            let cmd = ui.on_finger(&g, *a, &cards(2), "Notebook 3", ctx(), t0 + dur_ms);
            if cmd != UiCommand::None {
                cmds.push(cmd);
            }
        }
        (cmds, actions.len())
    }

    fn tap(ui: &mut UiState, p: UiPoint, t0: u64) -> Vec<UiCommand> {
        let mut arb = TouchArbiter::default();
        finger(ui, &mut arb, &[p, pt(p.x + 3, p.y + 2)], t0, 80).0
    }

    // ---------------------------------------------------------------- 1/2 state + full panel

    #[test]
    fn notebook_entry_is_fullscreen_and_state_is_transient() {
        let ui = notebook();
        assert_eq!(ui.view, AppView::Notebook);
        assert_eq!(ui.chrome.visibility(), ChromeVisibility::Hidden);
        assert!(!ui.chrome_overlay_visible());
        // Home has no hidden-chrome mode and never paints notebook chrome.
        let mut home = UiState::launch_home(0, 3);
        home.chrome.reveal(0);
        assert!(!home.chrome_overlay_visible());
        // Re-entering a notebook always starts fullscreen.
        let mut ui = notebook();
        ui.chrome.reveal(0);
        ui.enter_home(0, 1);
        ui.enter_notebook();
        assert!(!ui.chrome.is_visible());
    }

    #[test]
    fn hidden_chrome_makes_the_entire_panel_writable() {
        let g = panel_geo();
        let ui = notebook();
        let mut router = PenRouter::default();
        let layout = ChromeLayout::new(&g);
        // Every former chrome control and both band extremes are page content for the pen.
        for p in [
            center(layout.home),
            center(layout.undo),
            center(layout.menu),
            center(layout.previous),
            center(layout.next),
            pt(0, 0),
            pt(1859, 0),
            pt(0, 2479),
            pt(1859, 2479),
            pt(930, TOP_BAND_H - 1),
            pt(930, g.yres - BOTTOM_BAND_H),
        ] {
            assert_eq!(
                router.route_down(&ui, &g, StylusTool::Pen, Some(p)),
                DownRoute::Page,
                "{p:?}"
            );
            assert!(ui.chrome_hit(&g, p).is_none());
        }
    }

    #[test]
    fn pen_writes_into_both_band_regions_while_hidden() {
        let mut ui = notebook();
        let mut router = PenRouter::default();
        let mut c: StrokeController<FakeTransport> =
            StrokeController::new("page-1", FakeTransport::new(), 8_000);
        for y in [5.0, 60.0, 1200.0, 2420.0, 2475.0] {
            let n = pen_contact(&mut ui, &mut router, &mut c, (400.0, y), (900.0, y));
            assert_eq!(n.ui_taps, 0);
        }
        assert_eq!(c.session.committed_count(), 5);
        assert!(!ui.chrome.is_visible(), "pen writing never reveals chrome");
    }

    // ---------------------------------------------------------------- 3/18 finger reveal

    #[test]
    fn finger_tap_while_hidden_reveals_chrome_and_does_nothing_else() {
        let g = panel_geo();
        for p in [
            pt(930, 1200),                      // content
            center(ChromeLayout::new(&g).home), // where HOME would be
            center(ChromeLayout::new(&g).next), // where Next would be
            pt(40, 1200),                       // edge zone
        ] {
            let mut ui = notebook();
            let cmds = tap(&mut ui, p, 1000);
            assert!(
                cmds.is_empty(),
                "{p:?}: no notebook/page command, got {cmds:?}"
            );
            assert!(ui.chrome.is_visible(), "{p:?}");
            assert!(ui.modal.is_none());
            assert_eq!(
                ui.chrome.hide_deadline_ms(),
                Some(1080 + CHROME_AUTO_HIDE_MS)
            );
        }
    }

    #[test]
    fn finger_down_and_movement_never_reveal_before_classification() {
        let g = panel_geo();
        let m = identity();
        let mut ui = notebook();
        let mut arb = TouchArbiter::default();
        // Down + moves: no action, chrome untouched until lift.
        for (i, p) in line((1500, 1200), (600, 1210), 10).iter().enumerate() {
            assert_eq!(
                arb.on_frame(&[contact(*p)], &m, i as u64 * 20, true, &ui, &g),
                None
            );
            assert!(!ui.chrome.is_visible());
        }
        let a = arb.on_frame(&[], &m, 220, true, &ui, &g).unwrap();
        assert_eq!(a, FingerAction::NextPage);
        let cmd = ui.on_finger(&g, a, &cards(1), "x", ctx(), 220);
        assert_eq!(cmd, UiCommand::NextPage);
        assert!(
            !ui.chrome.is_visible(),
            "a swipe never flashes the controls"
        );
    }

    // ---------------------------------------------------------------- 4/19 swipes

    #[test]
    fn finger_swipe_while_hidden_turns_exactly_one_page_and_stays_hidden() {
        let mut ui = notebook();
        let mut arb = TouchArbiter::default();
        let (cmds, n) = finger(
            &mut ui,
            &mut arb,
            &line((1500, 1200), (700, 1205), 30),
            0,
            200,
        );
        assert_eq!((cmds, n), (vec![UiCommand::NextPage], 1));
        assert!(!ui.chrome.is_visible());
        let (cmds, _) = finger(
            &mut ui,
            &mut arb,
            &line((400, 1200), (1300, 1180), 30),
            500,
            200,
        );
        assert_eq!(cmds, vec![UiCommand::PreviousPage]);
        assert!(!ui.chrome.is_visible());
        // Hidden bands are content: a swipe starting in the former top band also turns.
        let (cmds, _) = finger(
            &mut ui,
            &mut arb,
            &line((1500, 60), (600, 70), 30),
            900,
            200,
        );
        assert_eq!(cmds, vec![UiCommand::NextPage]);
        // F11.2 edge exclusion preserved.
        let (cmds, n) = finger(
            &mut ui,
            &mut arb,
            &line((20, 1200), (900, 1200), 30),
            1300,
            200,
        );
        assert_eq!((cmds.len(), n), (0, 0));
        assert!(!ui.chrome.is_visible());
    }

    // ---------------------------------------------------------------- 9 finger while visible

    #[test]
    fn finger_with_chrome_visible_operates_hides_or_swipes() {
        let g = panel_geo();
        let c = ChromeLayout::new(&g);
        // Controls keep F11/F11.2 semantics.
        for (r, want) in [
            (c.home, UiCommand::GoHome),
            (c.undo, UiCommand::Undo),
            (c.next, UiCommand::NextPage),
            (c.previous, UiCommand::PreviousPage),
        ] {
            let mut ui = notebook();
            ui.chrome.reveal(0);
            assert_eq!(tap(&mut ui, center(r), 1000), vec![want]);
        }
        let mut ui = notebook();
        ui.chrome.reveal(0);
        assert_eq!(
            tap(&mut ui, center(c.menu), 1000),
            vec![UiCommand::Redraw(Redraw::Full)]
        );
        assert!(matches!(ui.modal, Some(Modal::PageMenu { .. })));

        // Content tap: hide immediately, no command, no ink.
        let mut ui = notebook();
        ui.chrome.reveal(0);
        assert!(tap(&mut ui, pt(930, 1200), 1000).is_empty());
        assert!(!ui.chrome.is_visible());

        // Swipe from content while visible: page command, chrome hidden, not a tap.
        let mut ui = notebook();
        ui.chrome.reveal(0);
        let mut arb = TouchArbiter::default();
        let (cmds, _) = finger(
            &mut ui,
            &mut arb,
            &line((1500, 1200), (600, 1200), 20),
            100,
            200,
        );
        assert_eq!(cmds, vec![UiCommand::NextPage]);
        assert!(!ui.chrome.is_visible());

        // Drag starting on a visible control: no swipe, no button, chrome stays.
        let mut ui = notebook();
        ui.chrome.reveal(0);
        let s = center(c.next);
        let (cmds, n) = finger(
            &mut ui,
            &mut arb,
            &line((s.x, s.y), (s.x - 500, s.y), 20),
            100,
            250,
        );
        assert_eq!((cmds.len(), n), (0, 0));
        assert!(ui.chrome.is_visible());
    }

    // ---------------------------------------------------------------- 10/22 timer

    #[test]
    fn auto_hide_after_three_seconds_of_inactivity() {
        let mut s = NotebookChromeState::default();
        s.reveal(0);
        assert!(!s.timeout_due(2_900));
        assert!(s.timeout_due(3_000));
        assert!(s.timeout_due(3_500));
        assert!(s.hide());
        assert!(!s.is_visible());
        assert!(!s.timeout_due(10_000));
    }

    #[test]
    fn chrome_interaction_resets_the_deadline() {
        let g = panel_geo();
        let c = ChromeLayout::new(&g);
        let mut ui = notebook();
        assert!(tap(&mut ui, pt(930, 1200), 0).is_empty()); // reveal (lift at t=80)
        assert_eq!(ui.chrome.hide_deadline_ms(), Some(80 + CHROME_AUTO_HIDE_MS));
        // Finger UI tap (Next) at t≈2 s resets the countdown.
        assert_eq!(
            tap(&mut ui, center(c.next), 1920),
            vec![UiCommand::NextPage]
        );
        assert!(ui.chrome.is_visible());
        assert!(!ui.chrome.timeout_due(4_000));
        assert!(!ui.chrome.timeout_due(4_999));
        assert!(ui.chrome.timeout_due(5_000));

        // Pen tap on a chrome button also resets it.
        let mut ui = notebook();
        ui.chrome.reveal(0);
        let cmd = ui.on_ui_tap(&g, center(c.undo), &cards(1), "x", ctx(), 2_000);
        assert_eq!(cmd, UiCommand::Undo);
        assert!(!ui.chrome.timeout_due(4_900));
        assert!(ui.chrome.timeout_due(5_000));
    }

    #[test]
    fn rejected_and_suppressed_touches_do_not_reset_the_deadline() {
        let g = panel_geo();
        let m = identity();
        let mut ui = notebook();
        ui.chrome.reveal(0);
        let mut arb = TouchArbiter::default();
        // Palm on HOME while the pen hovers at t=2 s: nothing, deadline unchanged.
        arb.on_stylus_event(StylusEventKind::Hover, true, 2_000);
        let home = center(ChromeLayout::new(&g).home);
        assert_eq!(
            arb.on_frame(&[contact(home)], &m, 2_010, true, &ui, &g),
            None
        );
        assert_eq!(arb.on_frame(&[], &m, 2_050, true, &ui, &g), None);
        // Cancelled drag from a control.
        let mut arb = TouchArbiter::default();
        let s = center(ChromeLayout::new(&g).next);
        let (cmds, n) = finger(
            &mut ui,
            &mut arb,
            &line((s.x, s.y), (s.x - 400, s.y), 10),
            2_100,
            200,
        );
        assert_eq!((cmds.len(), n), (0, 0));
        // Display-invalid input.
        let mut arb = TouchArbiter::default();
        assert_eq!(
            arb.on_frame(&[contact(home)], &m, 2_400, false, &ui, &g),
            None
        );
        assert_eq!(arb.on_frame(&[], &m, 2_450, false, &ui, &g), None);
        assert_eq!(ui.chrome.hide_deadline_ms(), Some(CHROME_AUTO_HIDE_MS));
        assert!(ui.chrome.timeout_due(3_000));
    }

    // ---------------------------------------------------------------- 11 modal

    #[test]
    fn modal_suspends_auto_hide_and_closing_restarts_it() {
        let g = panel_geo();
        let c = ChromeLayout::new(&g);
        let mut ui = notebook();
        ui.chrome.reveal(0);
        // Open the page menu from chrome at t=1 s.
        assert_eq!(
            ui.on_ui_tap(&g, center(c.menu), &cards(1), "x", ctx(), 1_000),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.modal_open());
        assert!(!ui.allows_page_input());
        assert!(!ui.chrome.timeout_due(11_000), "modal is never auto-hidden");
        // Menu buttons (dialog) are the only targets; underlying page ink is not actionable.
        let mut router = PenRouter::default();
        assert!(matches!(
            router.route_down(&ui, &g, StylusTool::Pen, Some(pt(930, 1900))),
            DownRoute::UiTap(_)
        ));
        router.on_up();
        // Close (tap outside buttons is swallowed; Cancel closes) at t=11 s.
        let menu = ui.modal.clone().unwrap();
        let spec = crate::modal::dialog_spec(&menu).unwrap();
        let (_, cancel) = crate::modal::DialogLayout::new(&g, &spec)
            .buttons
            .into_iter()
            .find(|(b, _)| b.label == "CANCEL")
            .unwrap();
        assert_eq!(
            ui.on_ui_tap(&g, center(cancel), &cards(1), "x", ctx(), 11_000),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(!ui.modal_open());
        assert!(ui.chrome.is_visible());
        assert!(!ui.chrome.timeout_due(13_900));
        assert!(ui.chrome.timeout_due(14_000));

        // A menu action that stays in the notebook also returns to visible chrome + timer.
        let mut ui = notebook();
        ui.chrome.reveal(0);
        ui.on_ui_tap(&g, center(c.menu), &cards(1), "x", ctx(), 100);
        let menu = ui.modal.clone().unwrap();
        let spec = crate::modal::dialog_spec(&menu).unwrap();
        let (_, ins) = crate::modal::DialogLayout::new(&g, &spec)
            .buttons
            .into_iter()
            .find(|(b, _)| b.label == "INSERT PAGE AFTER")
            .unwrap();
        assert_eq!(
            ui.on_ui_tap(&g, center(ins), &cards(1), "x", ctx(), 500),
            UiCommand::InsertPageAfter
        );
        assert_eq!(
            ui.chrome.hide_deadline_ms(),
            Some(500 + CHROME_AUTO_HIDE_MS)
        );

        // Executor-raised error dialog (e.g. refused op) suspends; OK restarts.
        let mut ui = notebook();
        ui.chrome.reveal(0);
        ui.show_error("The page couldn't be moved.");
        ui.note_modal_transition(false, 200);
        assert!(!ui.chrome.timeout_due(60_000));
        let spec = crate::modal::dialog_spec(ui.modal.as_ref().unwrap()).unwrap();
        let (_, ok) = crate::modal::DialogLayout::new(&g, &spec)
            .buttons
            .into_iter()
            .next()
            .unwrap();
        ui.on_ui_tap(&g, center(ok), &cards(1), "x", ctx(), 60_000);
        assert!(!ui.modal_open());
        assert_eq!(
            ui.chrome.hide_deadline_ms(),
            Some(60_000 + CHROME_AUTO_HIDE_MS)
        );
    }

    // ---------------------------------------------------------------- 5/6/20 palm

    #[test]
    fn palm_already_present_stays_suppressed_after_pen_leaves() {
        let g = panel_geo();
        let m = identity();
        let mut ui = notebook();
        let mut arb = TouchArbiter::default();
        let palm = pt(1200, 1800);
        // Palm down first (no stylus yet).
        assert_eq!(arb.on_frame(&[contact(palm)], &m, 0, true, &ui, &g), None);
        // Pen enters proximity, writes, leaves proximity.
        arb.on_stylus_event(StylusEventKind::Hover, true, 100);
        arb.on_stylus_event(StylusEventKind::Down, true, 150);
        for t in (160..600).step_by(10) {
            arb.on_stylus_event(StylusEventKind::Move, true, t);
        }
        arb.on_stylus_event(StylusEventKind::Up, true, 600);
        arb.on_proximity(Some(false), 700);
        assert!(!arb.stylus.in_proximity());
        // Long after the quiet window, the palm is still down and still poisoned.
        let later = 700 + STYLUS_QUIET_MS * 5;
        assert!(!arb.suppresses_touch(later));
        assert!(arb.tracker.is_suppressed());
        assert_eq!(
            arb.on_frame(&[contact(pt(1205, 1802))], &m, later, true, &ui, &g),
            None
        );
        // Palm lifts: zero commands, chrome untouched.
        assert_eq!(arb.on_frame(&[], &m, later + 30, true, &ui, &g), None);
        assert!(!ui.chrome.is_visible());
        // Next NEW finger contact is eligible.
        let (cmds, n) = finger(
            &mut ui,
            &mut arb,
            &[pt(930, 1200), pt(932, 1201)],
            later + 200,
            60,
        );
        assert_eq!((cmds.len(), n), (0, 1));
        assert!(
            ui.chrome.is_visible(),
            "a deliberate tap after the palm lifts reveals chrome"
        );
    }

    #[test]
    fn resting_palm_without_touch_frames_is_poisoned_by_proximity_samples() {
        // A motionless palm produces no touch frames while the pen hovers; the proximity
        // sample alone must poison it.
        let g = panel_geo();
        let m = identity();
        let ui = notebook();
        let mut arb = TouchArbiter::default();
        assert_eq!(
            arb.on_frame(&[contact(pt(900, 1500))], &m, 0, true, &ui, &g),
            None
        );
        arb.on_proximity(Some(true), 50);
        arb.on_proximity(Some(false), 90);
        assert_eq!(
            arb.on_frame(&[], &m, 90 + STYLUS_QUIET_MS * 3, true, &ui, &g),
            None,
            "short contact overlapping proximity must never become a tap"
        );
    }

    #[test]
    fn palm_arriving_while_pen_is_near_is_inert() {
        let g = panel_geo();
        let c = ChromeLayout::new(&g);
        for visible in [false, true] {
            let mut ui = notebook();
            if visible {
                ui.chrome.reveal(0);
            }
            let mut arb = TouchArbiter::default();
            arb.on_proximity(Some(true), 0);
            // Tap-shaped, swipe-shaped, and HOME/UNDO/menu-shaped contacts: all inert.
            let targets = [
                vec![pt(930, 1200), pt(931, 1201)],
                line((1500, 1200), (400, 1200), 20),
                vec![center(c.home), center(c.home)],
                vec![center(c.undo), center(c.undo)],
                vec![center(c.menu), center(c.menu)],
                vec![center(c.next), center(c.next)],
            ];
            for (i, path) in targets.iter().enumerate() {
                let t0 = 100 + i as u64 * 300;
                let (cmds, n) = finger(&mut ui, &mut arb, path, t0, 90);
                assert_eq!((cmds.len(), n), (0, 0), "visible={visible} path {i}");
            }
            assert_eq!(
                ui.chrome.is_visible(),
                visible,
                "palm never shows or hides chrome"
            );
            assert!(ui.modal.is_none());
        }
    }

    #[test]
    fn large_palm_motion_while_pen_in_proximity_triggers_nothing() {
        let g = panel_geo();
        let m = identity();
        let ui = notebook();
        let mut arb = TouchArbiter::default();
        arb.on_stylus_event(StylusEventKind::Hover, true, 0);
        // Palm slides across the page while writing; stylus events interleave.
        let path = line((1700, 2300), (200, 400), 60);
        for (i, p) in path.iter().enumerate() {
            let t = 10 + i as u64 * 12;
            assert_eq!(arb.on_frame(&[contact(*p)], &m, t, true, &ui, &g), None);
            arb.on_stylus_event(StylusEventKind::Move, true, t + 1);
        }
        // Pen leaves proximity before the palm lifts.
        arb.on_proximity(Some(false), 800);
        assert_eq!(
            arb.on_frame(&[], &m, 800 + STYLUS_QUIET_MS * 2, true, &ui, &g),
            None
        );
        // Nothing resolved: no command could be issued.
        assert!(!ui.chrome.is_visible());
        assert!(ui.modal.is_none());
        assert_eq!(ui.view, AppView::Notebook);
    }

    #[test]
    fn proximity_exit_edge_and_stale_fail_safe() {
        let mut arb = TouchArbiter::default();
        arb.on_proximity(Some(true), 0);
        assert!(arb.suppresses_touch(5_000), "hovering pen suppresses touch");
        arb.on_proximity(Some(false), 5_000);
        assert!(arb.suppresses_touch(5_000 + STYLUS_QUIET_MS - 1));
        assert!(!arb.suppresses_touch(5_000 + STYLUS_QUIET_MS));
        // Unknown samples (scripted source / desync) leave the flag alone.
        arb.on_proximity(None, 6_000);
        assert!(!arb.stylus.in_proximity());
        // A proximity report with no stylus activity for 10 s stops suppressing on its own.
        let mut arb = TouchArbiter::default();
        arb.on_stylus_event(StylusEventKind::Hover, true, 0);
        assert!(arb.suppresses_touch(9_999));
        assert!(!arb.suppresses_touch(10_000));
    }

    // ---------------------------------------------------------------- 7/8/21 pen

    struct PenOutcome {
        ui_taps: usize,
        commands: Vec<UiCommand>,
        chrome_hidden_by_pen: bool,
    }

    /// Mirrors the runtime pen path: route → UI tap resolution, or (page contact) F11.3
    /// pen-content dismissal *before* `on_pen_down`, then the sealed stroke controller.
    fn pen_contact(
        ui: &mut UiState,
        router: &mut PenRouter,
        c: &mut StrokeController<FakeTransport>,
        from: (f32, f32),
        to: (f32, f32),
    ) -> PenOutcome {
        let g = panel_geo();
        let mut out = PenOutcome {
            ui_taps: 0,
            commands: Vec::new(),
            chrome_hidden_by_pen: false,
        };
        let mid = ((from.0 + to.0) / 2.0, (from.1 + to.1) / 2.0);
        let events = [
            (StylusEventKind::Down, from),
            (StylusEventKind::Move, mid),
            (StylusEventKind::Move, to),
            (StylusEventKind::Up, to),
        ];
        for (kind, (x, y)) in events {
            let page = PagePoint::new((x + 0.5) * 8.4, (y + 0.5) * 8.4);
            let p = UiPoint::from_page(page);
            match router.route_event(ui, &g, kind, StylusTool::Pen, p, !c.is_idle()) {
                DownRoute::UiTap(p) => {
                    out.ui_taps += 1;
                    let cmd = ui.on_ui_tap(&g, p, &cards(1), "x", ctx(), 1_000);
                    if cmd != UiCommand::None {
                        out.commands.push(cmd);
                    }
                }
                DownRoute::Consume => {}
                DownRoute::Page => match kind {
                    StylusEventKind::Down => {
                        if c.is_idle() && !c.is_armed() && ui.chrome.pen_content_down() {
                            // Runtime reconstructs here (GC16 FULL), before arming.
                            out.chrome_hidden_by_pen = true;
                        }
                        c.on_pen_down(page, Some(0.5)).unwrap();
                    }
                    StylusEventKind::Move => c.on_pen_move(page, Some(0.5), None).unwrap(),
                    StylusEventKind::Up => {
                        let _ = c.on_pen_up(page, Some(0.5));
                    }
                    StylusEventKind::Hover => {}
                },
            }
        }
        out
    }

    #[test]
    fn pen_content_down_hides_chrome_and_the_same_contact_writes() {
        let mut ui = notebook();
        ui.chrome.reveal(0);
        let mut router = PenRouter::default();
        let mut c: StrokeController<FakeTransport> =
            StrokeController::new("page-1", FakeTransport::new(), 8_000);
        let out = pen_contact(
            &mut ui,
            &mut router,
            &mut c,
            (500.0, 1200.0),
            (900.0, 1300.0),
        );
        assert!(out.chrome_hidden_by_pen);
        assert_eq!(out.ui_taps, 0);
        assert!(!ui.chrome.is_visible());
        assert_eq!(
            c.session.strokes_started(),
            1,
            "one contact: dismiss AND write"
        );
        assert_eq!(c.session.committed_count(), 1);
        let s = &c.session.committed_strokes()[0];
        // The first point is the Down point: nothing lost.
        assert!((s.points[0].x - 500.5 * 8.4).abs() < 1e-3);
        assert!((s.points[0].y - 1200.5 * 8.4).abs() < 1e-3);

        // Hidden chrome: writing is normal and never reveals.
        let out = pen_contact(
            &mut ui,
            &mut router,
            &mut c,
            (500.0, 1400.0),
            (900.0, 1500.0),
        );
        assert!(!out.chrome_hidden_by_pen);
        assert!(!ui.chrome.is_visible());
        assert_eq!(c.session.committed_count(), 2);
    }

    #[test]
    fn pen_on_visible_chrome_controls_is_ui_and_never_inks() {
        let g = panel_geo();
        let layout = ChromeLayout::new(&g);
        for (r, want) in [
            (layout.home, UiCommand::GoHome),
            (layout.undo, UiCommand::Undo),
        ] {
            let mut ui = notebook();
            ui.chrome.reveal(0);
            let mut router = PenRouter::default();
            let mut c: StrokeController<FakeTransport> =
                StrokeController::new("page-1", FakeTransport::new(), 8_000);
            let p = center(r);
            let out = pen_contact(
                &mut ui,
                &mut router,
                &mut c,
                (p.x as f32, p.y as f32),
                (p.x as f32 + 300.0, p.y as f32 + 600.0),
            );
            assert_eq!(out.ui_taps, 1);
            assert_eq!(out.commands, vec![want]);
            assert!(!out.chrome_hidden_by_pen);
            assert_eq!(c.session.strokes_started(), 0);
        }
    }

    // ---------------------------------------------------------------- 12 Home unchanged

    #[test]
    fn home_finger_behavior_is_unchanged() {
        let g = panel_geo();
        let home = UiState::launch_home(0, 9);
        let layout = HomeLayout::new(&g, &home.home, 9);
        let card = center(layout.cards[1].1);
        let mut a = home.clone();
        let mut b = home.clone();
        let finger = a.on_finger(&g, FingerAction::Tap(card), &cards(9), "N", None, 5);
        let pen = b.on_tap(&g, card, &cards(9), "N", None);
        assert_eq!(finger, pen);
        assert_eq!(finger, UiCommand::OpenNotebook("notebook-2".into()));
        assert_eq!(a.home, b.home);
        // Swipes on Home never reach on_finger as page turns (F11.2 origin rule); chrome
        // state stays default on Home.
        assert!(!a.chrome_overlay_visible());
    }

    // ---------------------------------------------------------------- 13 sleep / wake

    #[test]
    fn sleep_wake_preserves_visibility_and_modal_state() {
        let sleep = DisplayEvents {
            invalidated: true,
            restored: false,
        };
        let wake = DisplayEvents {
            invalidated: false,
            restored: true,
        };
        let g = panel_geo();
        let m = identity();

        // Hidden: stays hidden, finger inert while invalid.
        let mut ui = notebook();
        let mut router = PenRouter::default();
        router.on_display_events(sleep);
        let mut arb = TouchArbiter::default();
        arb.on_frame(
            &[contact(pt(930, 1200))],
            &m,
            10,
            router.display_valid(),
            &ui,
            &g,
        );
        assert_eq!(
            arb.on_frame(&[], &m, 60, router.display_valid(), &ui, &g),
            None
        );
        assert!(
            !router.redraw_requested(),
            "finger never paints over the screensaver"
        );
        router.on_display_events(wake);
        router.display_presented();
        ui.chrome.display_restored(50_000);
        assert!(!ui.chrome.is_visible());

        // Visible: restored visible with a fresh countdown (no auto-hide while asleep: the
        // runtime only evaluates the deadline with a valid display).
        let mut ui = notebook();
        ui.chrome.reveal(0);
        let mut router = PenRouter::default();
        router.on_display_events(sleep);
        assert!(!router.display_valid());
        router.on_display_events(wake);
        router.display_presented();
        ui.chrome.display_restored(600_000);
        assert!(ui.chrome.is_visible());
        assert!(!ui.chrome.timeout_due(602_999));
        assert!(ui.chrome.timeout_due(603_000));

        // Modal: restored as-is, timer still suspended.
        let mut ui = notebook();
        ui.chrome.reveal(0);
        ui.on_ui_tap(
            &g,
            center(ChromeLayout::new(&g).menu),
            &cards(1),
            "x",
            ctx(),
            100,
        );
        let before = ui.clone();
        ui.chrome.display_restored(900_000);
        assert_eq!(ui, before);
        assert!(!ui.chrome.timeout_due(10_000_000));
    }

    // ---------------------------------------------------------------- 14/15/16 ink under chrome

    fn tmp(label: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scribe-f113-{label}-{n}"));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn page_files(root: &std::path::Path) -> HashMap<&'static str, Vec<u8>> {
        let dir = page_dir(root, "page-1");
        let mut out = HashMap::new();
        for name in [
            "page.meta",
            "background.raw",
            "strokes.jsonl",
            "edits.jsonl",
        ] {
            out.insert(name, fs::read(dir.join(name)).unwrap());
        }
        out.insert(
            "notebook.meta",
            fs::read(root.join("notebook.meta")).unwrap(),
        );
        out
    }

    /// Packed durable background → stride framebuffer (what `restore_background` does).
    fn restore(fb: &mut [u8], g: &FbGeometry, bg: &[u8]) {
        for y in 0..g.yres as usize {
            let src = &bg[y * g.xres as usize..(y + 1) * g.xres as usize];
            let dst = y * g.line_length as usize;
            fb[dst..dst + g.xres as usize].copy_from_slice(src);
        }
    }

    fn present(g: &FbGeometry, bg: &[u8], strokes: &[Stroke], visible: bool) -> Vec<u8> {
        let mut fb = vec![0x55; g.smem_len as usize];
        restore(&mut fb, g, bg);
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let overlay = visible.then_some(("Work", PAGE));
        compose_notebook_layers(&mut fb, g, &profile, strokes, None, &[], overlay).unwrap();
        fb
    }

    fn inked_rows(g: &FbGeometry, fb: &[u8], rows: std::ops::Range<u32>) -> usize {
        rows.filter(|y| {
            let r = (*y * g.line_length) as usize;
            fb[r..r + g.xres as usize].iter().any(|b| *b != WHITE)
        })
        .count()
    }

    fn band_bytes(g: &FbGeometry, fb: &[u8], rows: std::ops::Range<u32>) -> Vec<u8> {
        rows.flat_map(|y| {
            let r = (y * g.line_length) as usize;
            fb[r..r + g.xres as usize].to_vec()
        })
        .collect()
    }

    fn vertical_stroke(id: &str, x: f32, y0: f32, y1: f32) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: (0..=20)
                .map(|i| {
                    let y = y0 + (y1 - y0) * i as f32 / 20.0;
                    Point::new(x * 8.4, y * 8.4)
                })
                .collect(),
            brush: None,
            finished: true,
        }
    }

    #[test]
    fn ink_under_chrome_is_covered_while_visible_and_restored_on_hide() {
        let g = panel_geo();
        let root = tmp("under");
        let paper = paper_background(&g);
        let mut opened = open_or_create_notebook(&root, InkRequest::Default, &g, &paper).unwrap();
        // Written while hidden: through the top band (y=40) and the bottom band (y=2440).
        let top = vertical_stroke("s-top", 1500.0, 10.0, 400.0);
        let bottom = vertical_stroke("s-bottom", 300.0, 2100.0, 2470.0);
        opened.store.append_committed_stroke(&top).unwrap();
        opened.store.append_committed_stroke(&bottom).unwrap();
        let before = page_files(&root);

        let (_s, loaded) = load_existing_page(&root, "page-1").unwrap();
        assert_eq!(loaded.strokes.len(), 2);
        let bottom_rows = (g.yres - BOTTOM_BAND_H)..g.yres;

        // Hidden: ink visible inside both bands.
        let hidden = present(&g, &loaded.background, &loaded.strokes, false);
        assert!(inked_rows(&g, &hidden, 0..TOP_BAND_H) > 50);
        assert!(inked_rows(&g, &hidden, bottom_rows.clone()) > 20);

        // Visible: bands show exactly the chrome, regardless of ink underneath.
        let visible = present(&g, &loaded.background, &loaded.strokes, true);
        let chrome_only = present(&g, &loaded.background, &[], true);
        assert_eq!(
            band_bytes(&g, &visible, 0..TOP_BAND_H),
            band_bytes(&g, &chrome_only, 0..TOP_BAND_H),
            "top portion of the stroke is temporarily covered"
        );
        assert_eq!(
            band_bytes(&g, &visible, bottom_rows.clone()),
            band_bytes(&g, &chrome_only, bottom_rows.clone())
        );
        // Content between the bands is unaffected by the overlay.
        assert_eq!(
            band_bytes(&g, &visible, TOP_BAND_H..g.yres - BOTTOM_BAND_H),
            band_bytes(&g, &hidden, TOP_BAND_H..g.yres - BOTTOM_BAND_H)
        );

        // Hide again (auto-hide / tap / pen): full recomposition restores the complete strokes.
        let rehidden = present(&g, &loaded.background, &loaded.strokes, false);
        assert_eq!(rehidden, hidden);

        // Durable bytes unchanged by showing/hiding chrome; nothing new written.
        assert_eq!(page_files(&root), before);
        let (_s, reloaded) = load_existing_page(&root, "page-1").unwrap();
        assert_eq!(reloaded.strokes, loaded.strokes);
        assert_eq!(reloaded.background, paper);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn old_pages_with_ink_anywhere_are_never_cropped_or_translated() {
        let g = panel_geo();
        let paper = paper_background(&g);
        // Legacy ink at the four extremes of the panel coordinate space.
        let strokes = vec![
            vertical_stroke("a", 20.0, 2.0, 120.0),
            vertical_stroke("b", 1840.0, 2.0, 120.0),
            vertical_stroke("c", 20.0, 2370.0, 2477.0),
            vertical_stroke("d", 1840.0, 2370.0, 2477.0),
        ];
        let snapshot = strokes.clone();
        let hidden = present(&g, &paper, &strokes, false);
        let _ = present(&g, &paper, &strokes, true);
        assert_eq!(present(&g, &paper, &strokes, false), hidden);
        assert_eq!(strokes, snapshot);
        let px = |x: u32, y: u32| hidden[(y * g.line_length + x) as usize];
        for (x, y) in [(20, 60), (1840, 60), (20, 2420), (1840, 2420)] {
            assert_ne!(
                px(x, y),
                WHITE,
                "ink at ({x},{y}) must render in fullscreen"
            );
        }
    }

    // ---------------------------------------------------------------- Home exit icon

    #[test]
    fn home_exit_icon_is_one_glyph_cell_in_the_compact_bar_and_exits() {
        use crate::home::{render_home, HomeState};
        use crate::ui_gfx::{GLYPH_H, GLYPH_W};
        let g = panel_geo();
        let home = UiState::launch_home(0, 9);
        let layout = HomeLayout::new(&g, &home.home, 9);
        let icon = layout.exit_icon;
        // Exactly one glyph cell (scale 10), centred in a square >= 130 px touch target.
        assert_eq!((icon.w, icon.h), (GLYPH_W * 10, GLYPH_H * 10));
        let hit = layout.exit_hit;
        assert!(hit.w >= 130 && hit.h >= 130);
        assert!(hit.contains(icon.x, icon.y) && hit.contains(icon.right() - 1, icon.bottom() - 1));
        // Hit area never reaches the search field, NEW, tools or any card.
        for other in [layout.search, layout.new_button, layout.tools_button] {
            assert!(!hit.intersects(&other));
        }
        for (_, card, _) in &layout.cards {
            assert!(!hit.intersects(card));
        }

        // Rendering: all ink inside the exit target is the icon, inside its cell.
        let mut fb = vec![0x44; g.smem_len as usize];
        render_home(&mut fb, &g, &cards(9), &HomeState::selecting(0, 9)).unwrap();
        let px = |x: u32, y: u32| fb[(y * g.line_length + x) as usize];
        let mut icon_ink = 0;
        for y in hit.y..hit.bottom() {
            for x in hit.x..hit.right() {
                if px(x, y) != WHITE {
                    assert!(
                        icon.contains(x, y),
                        "icon ink outside its cell at ({x},{y})"
                    );
                    icon_ink += 1;
                }
            }
        }
        assert!(icon_ink > 250, "icon drawn");

        // Pen and finger tap → ExitApp; nothing else changes.
        let mut pen = home.clone();
        assert_eq!(
            pen.on_ui_tap(&g, center(icon), &cards(9), "N", None, 0),
            UiCommand::ExitApp
        );
        assert_eq!(pen, home);
        let mut finger_ui = home.clone();
        assert_eq!(
            tap(&mut finger_ui, pt(icon.x + 5, icon.bottom() - 5), 0),
            vec![UiCommand::ExitApp]
        );
        // Notebook view has no exit icon at that spot (page content / chrome only).
        let mut nb = notebook();
        assert_eq!(
            nb.on_ui_tap(&g, center(icon), &cards(9), "N", ctx(), 0),
            UiCommand::None
        );
    }
}
