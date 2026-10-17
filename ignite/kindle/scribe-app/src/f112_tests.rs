//! Gate F11.2 host tests: finger taps share the pen's UI hit targets, horizontal content
//! swipes flip pages, and multi-touch / palm / stylus / edge / display-invalid input is inert.

#[cfg(test)]
mod tests {
    use kindle_platform::{FbGeometry, StylusEventKind, TouchContact};

    use crate::app_ui::{NotebookUiContext, PenRouter, Redraw, UiCommand, UiState};
    use crate::chrome::{ChromeLayout, PageChrome};
    use crate::display_events::DisplayEvents;
    use crate::home::{HomeCard, HomeLayout};
    use crate::keyboard::{EditorKey, KeyboardLayout, TitleEditor};
    use crate::modal::{dialog_spec, DialogLayout, Modal, TitlePurpose};
    use crate::touch::{
        classify_origin, CoalesceDecision, FingerAction, FingerOrigin, FingerTracker,
        MovePreviewCoalesce, StylusActivity, TouchArbiter, TouchMapping, LASSO_LONG_PRESS_MS,
        STYLUS_QUIET_MS, TAP_MAX_MS, TAP_MAX_TRAVEL_PX,
    };
    use crate::ui_gfx::{panel_geo, Rect, UiPoint};

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

    /// Identity panel mapping (device range 0..=1859 × 0..=2479).
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

    /// Notebook view with chrome revealed (F11.3: notebooks open fullscreen; these tests
    /// exercise the chrome controls themselves).
    fn notebook_ui() -> UiState {
        let mut ui = UiState::launch_home(0, 1);
        ui.enter_notebook();
        ui.chrome.reveal(0);
        ui
    }

    /// One finger contact along `path` (panel px) over `dur_ms`, then lift. Returns every
    /// action produced (at most one is allowed).
    fn gesture(ui: &UiState, path: &[UiPoint], dur_ms: u64, blocked: bool) -> Vec<FingerAction> {
        let g = panel_geo();
        let m = identity();
        let mut t = FingerTracker::default();
        let mut out = Vec::new();
        let n = path.len().max(2) as u64 - 1;
        for (i, p) in path.iter().enumerate() {
            let ms = 1000 + dur_ms * i as u64 / n;
            out.extend(t.on_frame(&[contact(*p)], &m, ms, blocked, ui, &g));
        }
        out.extend(t.on_frame(&[], &m, 1000 + dur_ms, blocked, ui, &g));
        assert!(t.is_idle());
        out
    }

    fn tap(ui: &UiState, p: UiPoint) -> Vec<FingerAction> {
        gesture(
            ui,
            &[
                p,
                UiPoint {
                    x: p.x + 3,
                    y: p.y + 2,
                },
            ],
            80,
            false,
        )
    }

    fn line(from: (u32, u32), to: (u32, u32), steps: u32) -> Vec<UiPoint> {
        (0..=steps)
            .map(|i| UiPoint {
                x: (from.0 as i64 + (to.0 as i64 - from.0 as i64) * i as i64 / steps as i64) as u32,
                y: (from.1 as i64 + (to.1 as i64 - from.1 as i64) * i as i64 / steps as i64) as u32,
            })
            .collect()
    }

    /// Resolve a finger tap exactly as the runtime does (shared `UiState::on_tap`).
    fn resolve(ui: &mut UiState, a: &[FingerAction], page: Option<PageChrome>) -> UiCommand {
        let g = panel_geo();
        match a {
            [FingerAction::Tap(p)] => ui.on_tap(
                &g,
                *p,
                &cards(9),
                "Notebook 10",
                page.map(|page| NotebookUiContext { page }),
            ),
            [FingerAction::NextPage] => UiCommand::NextPage,
            [FingerAction::PreviousPage] => UiCommand::PreviousPage,
            [] => UiCommand::None,
            many => panic!("more than one action per contact: {many:?}"),
        }
    }

    /// Finger tap at `p` on the current UI, resolved through the shared `on_tap`.
    fn tap_resolve(ui: &mut UiState, p: UiPoint, page: Option<PageChrome>) -> UiCommand {
        let actions = tap(ui, p);
        resolve(ui, &actions, page)
    }

    // ------------------------------------------------------------------- mapping

    #[test]
    fn mapping_corners_center_clamping_and_refusal() {
        let g = panel_geo();
        let m = TouchMapping::new((0, 4095), (0, 4095), &g).unwrap();
        assert_eq!(m.map(0, 0), Some(UiPoint { x: 0, y: 0 }));
        assert_eq!(m.map(4095, 4095), Some(UiPoint { x: 1859, y: 2479 }));
        assert_eq!(m.map(2048, 2048), Some(UiPoint { x: 929, y: 1239 }));
        // Malformed / out-of-range coordinates fail closed.
        assert_eq!(m.map(-1, 10), None);
        assert_eq!(m.map(10, 4096), None);
        let id = identity();
        assert_eq!(id.map(1859, 2479), Some(UiPoint { x: 1859, y: 2479 }));
        assert_eq!(id.map(930, 1240), Some(UiPoint { x: 930, y: 1240 }));
        assert!(TouchMapping::new((5, 5), (0, 10), &g).is_err());
        assert!(TouchMapping::new((0, 10), (9, 3), &g).is_err());
    }

    // ------------------------------------------------------------------- taps

    #[test]
    fn finger_taps_resolve_to_the_same_commands_as_the_pen() {
        let g = panel_geo();
        // Home card → OpenNotebook, identical to the pen (same on_tap).
        let mut ui = UiState::launch_home(0, 9);
        let mut pen_ui = ui.clone();
        let layout = HomeLayout::new(&g, &ui.home, 9);
        let card = center(layout.cards[1].1);
        let finger = tap_resolve(&mut ui, card, None);
        let pen = pen_ui.on_tap(&g, card, &cards(9), "Notebook 10", None);
        assert_eq!(finger, UiCommand::OpenNotebook("notebook-2".into()));
        assert_eq!(finger, pen);

        // + NEW → chooser → NEW NOTEBOOK → title editor.
        let mut ui = UiState::launch_home(0, 9);
        let cmd = tap_resolve(&mut ui, center(layout.new_button), None);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        let chooser = ui.modal.clone().unwrap();
        let spec = crate::modal::dialog_spec(&chooser).unwrap();
        let dl = crate::modal::DialogLayout::new(&g, &spec);
        let (_, r) = dl
            .buttons
            .iter()
            .find(|(b, _)| b.label == "NEW NOTEBOOK")
            .unwrap();
        let cmd = tap_resolve(&mut ui, center(*r), None);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert!(ui.modal.as_ref().unwrap().is_title_entry());

        // Keyboard key → character.
        let kb = KeyboardLayout::new(&g, false);
        let (_, k) = kb
            .keys
            .iter()
            .find(|(k, _)| *k == EditorKey::Char('q'))
            .unwrap();
        let cmd = tap_resolve(&mut ui, center(*k), None);
        assert!(matches!(cmd, UiCommand::Redraw(Redraw::Region(_))));
        match ui.modal.as_ref().unwrap() {
            Modal::TitleEntry { editor, .. } => assert_eq!(editor.text(), "Notebook 10q"),
            other => panic!("{other:?}"),
        }

        // Chrome: HOME, UNDO, Next, Previous, ⋯.
        let c = ChromeLayout::new(&g);
        let page = Some(PageChrome { index: 2, count: 3 });
        for (r, want) in [
            (c.home, UiCommand::GoHome),
            (c.undo, UiCommand::Undo),
            (c.next, UiCommand::NextPage),
            (c.previous, UiCommand::PreviousPage),
        ] {
            let mut ui = notebook_ui();
            assert_eq!(tap_resolve(&mut ui, center(r), page), want);
        }
        let mut ui = notebook_ui();
        assert_eq!(
            tap_resolve(&mut ui, center(c.menu), page),
            UiCommand::Redraw(Redraw::Full)
        );
        // Page menu button by finger.
        let menu = ui.modal.clone().unwrap();
        let spec = dialog_spec(&menu).unwrap();
        let (_, ins) = DialogLayout::new(&g, &spec)
            .buttons
            .into_iter()
            .find(|(b, _)| b.label == "INSERT PAGE AFTER")
            .unwrap();
        assert_eq!(
            tap_resolve(&mut ui, center(ins), page),
            UiCommand::InsertPageAfter
        );
    }

    #[test]
    fn drag_that_starts_on_a_button_never_activates_it() {
        let g = panel_geo();
        let c = ChromeLayout::new(&g);
        let ui = notebook_ui();
        let start = center(c.next);
        // Next button, then drag 500 px: cancelled UI contact, not a tap and not a swipe.
        let a = gesture(
            &ui,
            &line((start.x, start.y), (start.x - 500, start.y), 20),
            300,
            false,
        );
        assert!(a.is_empty(), "{a:?}");
        // Same for a Home card and a dialog button.
        let home = UiState::launch_home(0, 9);
        let card = center(HomeLayout::new(&g, &home.home, 9).cards[0].1);
        assert!(gesture(
            &home,
            &line((card.x, card.y), (card.x - 400, card.y), 10),
            250,
            false
        )
        .is_empty());
        // Too slow for a tap (held), even without movement.
        assert!(gesture(&ui, &[start, start], TAP_HOLD_MS, false).is_empty());
    }
    const TAP_HOLD_MS: u64 = 900;

    // ------------------------------------------------------------------- swipes

    #[test]
    fn content_swipes_flip_exactly_one_page_and_false_gestures_do_nothing() {
        let ui = notebook_ui();
        let mut u = ui.clone();
        // Recorded shapes: fast 782 px / 125 ms, slow 523 px / 961 ms.
        let left_fast = gesture(&ui, &line((1500, 1200), (718, 1202), 10), 125, false);
        assert_eq!(resolve(&mut u, &left_fast, None), UiCommand::NextPage);
        let left_slow = gesture(&ui, &line((1500, 820), (977, 697), 80), 960, false);
        assert_eq!(left_slow, vec![FingerAction::NextPage]);
        let right = gesture(&ui, &line((400, 1200), (1200, 1150), 12), 200, false);
        assert_eq!(resolve(&mut u, &right, None), UiCommand::PreviousPage);

        // Many move events, one command.
        let long = gesture(&ui, &line((1600, 1300), (200, 1310), 400), 1100, false);
        assert_eq!(long, vec![FingerAction::NextPage]);

        // Vertical, diagonal (recorded 1284,-1301), short, too slow: nothing.
        assert!(gesture(&ui, &line((900, 600), (910, 1800), 20), 300, false).is_empty());
        assert!(gesture(&ui, &line((489, 1750), (1773, 449), 30), 440, false).is_empty());
        assert!(gesture(&ui, &line((900, 1200), (700, 1210), 10), 150, false).is_empty());
        assert!(gesture(&ui, &line((1500, 1200), (600, 1200), 50), 2000, false).is_empty());
        // Mild vertical drift keeps |dx| >= 2|dy| (recorded 523/-123).
        assert_eq!(
            gesture(&ui, &line((1400, 1000), (877, 877), 20), 500, false),
            vec![FingerAction::NextPage]
        );
    }

    #[test]
    fn edge_origin_swipes_are_reserved_for_kindle() {
        let g = panel_geo();
        let ui = notebook_ui();
        assert_eq!(
            classify_origin(&ui, &g, UiPoint { x: 10, y: 1200 }),
            FingerOrigin::Edge
        );
        assert_eq!(
            classify_origin(&ui, &g, UiPoint { x: 1850, y: 1200 }),
            FingerOrigin::Edge
        );
        assert_eq!(
            classify_origin(&ui, &g, UiPoint { x: 930, y: 1200 }),
            FingerOrigin::Content
        );
        assert!(gesture(&ui, &line((20, 1200), (900, 1200), 20), 200, false).is_empty());
        assert!(gesture(&ui, &line((1845, 1200), (900, 1200), 20), 200, false).is_empty());
        // The recorded natural swipe starts (x = 1547–1688) are outside the reserved zone.
        for x in [1547, 1640, 1688] {
            assert_eq!(
                classify_origin(&ui, &g, UiPoint { x, y: 1800 }),
                FingerOrigin::Content
            );
        }
    }

    #[test]
    fn swipes_from_chrome_modal_or_home_are_never_page_turns() {
        let g = panel_geo();
        let ui = notebook_ui();
        // Top and bottom chrome bands.
        assert!(gesture(&ui, &line((1400, 60), (400, 70), 20), 200, false).is_empty());
        assert!(gesture(&ui, &line((1400, 2420), (400, 2420), 20), 200, false).is_empty());
        // Kindle-style swipe down from the top edge: UI origin, cancelled drag.
        assert!(gesture(&ui, &line((930, 3), (930, 900), 20), 250, false).is_empty());
        // Modal open over the notebook.
        let mut modal = notebook_ui();
        modal.show_modal(Modal::PageMenu {
            page_number: 2,
            page_count: 3,
        });
        assert_eq!(
            classify_origin(&modal, &g, UiPoint { x: 930, y: 1200 }),
            FingerOrigin::Ui
        );
        assert!(gesture(&modal, &line((1500, 1200), (500, 1200), 20), 200, false).is_empty());
        // Title keyboard.
        let mut kb = UiState::launch_home(0, 2);
        kb.show_modal(Modal::TitleEntry {
            purpose: TitlePurpose::Create,
            editor: TitleEditor::new("x"),
        });
        assert!(gesture(&kb, &line((1500, 900), (500, 900), 20), 200, false).is_empty());
        // Home.
        let home = UiState::launch_home(0, 9);
        assert!(gesture(&home, &line((1500, 1200), (500, 1200), 20), 200, false).is_empty());
    }

    // ------------------------------------------------------------------- multi-touch / palm

    #[test]
    fn second_contact_or_palm_cancels_until_all_lift() {
        let g = panel_geo();
        let m = identity();
        let ui = notebook_ui();
        let home = UiState::launch_home(0, 9);
        let card = center(HomeLayout::new(&g, &home.home, 9).cards[0].1);
        let second = TouchContact {
            slot: 1,
            x: Some(300),
            y: Some(1200),
            palm: false,
            ..TouchContact::default()
        };

        // Two-finger tap on a Home card: nothing.
        let mut t = FingerTracker::default();
        assert_eq!(t.on_frame(&[contact(card)], &m, 0, false, &home, &g), None);
        assert_eq!(
            t.on_frame(&[contact(card), second], &m, 20, false, &home, &g),
            None
        );
        assert_eq!(t.on_frame(&[contact(card)], &m, 40, false, &home, &g), None);
        assert_eq!(t.on_frame(&[], &m, 60, false, &home, &g), None);
        assert!(t.is_idle());

        // Two-finger horizontal swipe (recorded pattern): nothing.
        let mut t = FingerTracker::default();
        for (i, x) in (0..10).map(|i| (i, 1500 - i * 90)) {
            let a = contact(UiPoint { x, y: 1200 });
            let b = TouchContact {
                slot: 1,
                x: Some(x as i32 - 200),
                y: Some(1300),
                palm: false,
                ..TouchContact::default()
            };
            assert_eq!(t.on_frame(&[a, b], &m, i as u64 * 12, false, &ui, &g), None);
        }
        assert_eq!(t.on_frame(&[], &m, 200, false, &ui, &g), None);

        // Palm reclassification mid-swipe (observed MT_TOOL_PALM before lift): nothing.
        let mut t = FingerTracker::default();
        for i in 0..10u32 {
            let mut c = contact(UiPoint {
                x: 1500 - i * 100,
                y: 1200,
            });
            c.palm = i == 9;
            t.on_frame(&[c], &m, u64::from(i) * 12, false, &ui, &g);
        }
        assert_eq!(t.on_frame(&[], &m, 200, false, &ui, &g), None);

        // Fresh single tap afterwards works.
        assert_eq!(tap(&home, card), vec![FingerAction::Tap(card)]);
    }

    // ------------------------------------------------------------------- stylus arbitration

    #[test]
    fn stylus_activity_suppresses_finger_actions() {
        let g = panel_geo();
        let c = ChromeLayout::new(&g);
        let m = identity();
        let ui = notebook_ui();
        let mut stylus = StylusActivity::default();
        stylus.on_stylus_event(StylusEventKind::Hover, 1000);
        assert!(stylus.suppresses_touch(1000 + STYLUS_QUIET_MS - 1));

        // Palm on HOME while the pen hovers / writes: blocked frames → no action.
        let home_btn = center(c.home);
        let mut t = FingerTracker::default();
        for ms in [1010, 1030, 1050] {
            let blocked = stylus.suppresses_touch(ms);
            assert!(t
                .on_frame(&[contact(home_btn)], &m, ms, blocked, &ui, &g)
                .is_none());
            stylus.on_stylus_event(StylusEventKind::Move, ms);
        }
        stylus.on_stylus_event(StylusEventKind::Up, 1060);
        assert!(t
            .on_frame(&[], &m, 1070, stylus.suppresses_touch(1070), &ui, &g)
            .is_none());
        // Swipe while suppressed: blocked on every frame → nothing.
        assert!(gesture(&ui, &line((1500, 1200), (500, 1200), 10), 150, true).is_empty());
        // Stylus contact itself suppresses regardless of time.
        let mut writing = StylusActivity::default();
        writing.on_stylus_event(StylusEventKind::Down, 0);
        assert!(writing.suppresses_touch(10_000));

        // Stylus goes quiet → a fresh finger tap works.
        let later = 1060 + STYLUS_QUIET_MS;
        assert!(!stylus.suppresses_touch(later));
        let mut t = FingerTracker::default();
        t.on_frame(&[contact(home_btn)], &m, later, false, &ui, &g);
        assert_eq!(
            t.on_frame(&[], &m, later + 60, false, &ui, &g),
            Some(FingerAction::Tap(home_btn))
        );

        // Pen arriving mid-gesture cancels it even if the lift frame is unblocked.
        let mut t = FingerTracker::default();
        t.on_frame(
            &[contact(UiPoint { x: 1500, y: 1200 })],
            &m,
            0,
            false,
            &ui,
            &g,
        );
        t.on_frame(
            &[contact(UiPoint { x: 1000, y: 1200 })],
            &m,
            50,
            true,
            &ui,
            &g,
        );
        t.on_frame(
            &[contact(UiPoint { x: 500, y: 1200 })],
            &m,
            90,
            false,
            &ui,
            &g,
        );
        assert_eq!(t.on_frame(&[], &m, 120, false, &ui, &g), None);
    }

    // ------------------------------------------------------------------- sleep / wake

    #[test]
    fn display_invalid_blocks_finger_until_restored() {
        let g = panel_geo();
        let home = UiState::launch_home(0, 9);
        let card = center(HomeLayout::new(&g, &home.home, 9).cards[0].1);
        let mut router = PenRouter::default();
        router.on_display_events(DisplayEvents {
            invalidated: true,
            restored: false,
        });
        let blocked = !router.display_valid();
        assert!(gesture(&home, &[card, card], 80, blocked).is_empty());
        let nb = notebook_ui();
        assert!(gesture(&nb, &line((1500, 1200), (500, 1200), 10), 150, blocked).is_empty());
        // Finger never requests a redraw (avoid painting over a visible screensaver).
        assert!(!router.redraw_requested());

        router.on_display_events(DisplayEvents {
            invalidated: false,
            restored: true,
        });
        router.display_presented();
        let blocked = !router.display_valid();
        assert_eq!(
            gesture(&home, &[card, card], 80, blocked),
            vec![FingerAction::Tap(card)]
        );
        assert_eq!(
            gesture(&nb, &line((1500, 1200), (500, 1200), 10), 150, blocked),
            vec![FingerAction::NextPage]
        );
    }

    #[test]
    fn finger_swipe_uses_the_identical_page_command_as_the_chrome_buttons() {
        // Durability: the runtime executes UiCommand::NextPage/PreviousPage through one
        // executor (navigate → active-page persist). Identical command ⇒ identical durable
        // state, including the pristine-trailing-page refusal and page-1 no-op.
        let g = panel_geo();
        let c = ChromeLayout::new(&g);
        let page = Some(PageChrome { index: 2, count: 3 });
        let mut ui = notebook_ui();
        let next_button = ui.on_tap(
            &g,
            center(c.next),
            &cards(1),
            "x",
            page.map(|page| NotebookUiContext { page }),
        );
        let mut ui2 = notebook_ui();
        let swipe = resolve(
            &mut ui2,
            &gesture(
                &notebook_ui(),
                &line((1500, 1200), (500, 1200), 10),
                150,
                false,
            ),
            page,
        );
        assert_eq!(next_button, swipe);
        let prev_button = ui.on_tap(
            &g,
            center(c.previous),
            &cards(1),
            "x",
            page.map(|page| NotebookUiContext { page }),
        );
        let swipe_right = resolve(
            &mut ui2,
            &gesture(
                &notebook_ui(),
                &line((500, 1200), (1500, 1200), 10),
                150,
                false,
            ),
            page,
        );
        assert_eq!(prev_button, swipe_right);
        let _: &FbGeometry = &g;
    }

    #[test]
    fn stationary_content_hold_activates_lasso_only_at_timeout() {
        let g = panel_geo();
        let m = identity();
        let mut ui = notebook_ui();
        ui.chrome.hide();
        let p = UiPoint { x: 900, y: 1200 };
        let mut t = FingerTracker::default();
        assert_eq!(t.on_frame(&[contact(p)], &m, 1000, false, &ui, &g), None);
        assert_eq!(t.on_tick(1000 + LASSO_LONG_PRESS_MS - 1, false), None);
        assert_eq!(
            t.on_tick(1000 + LASSO_LONG_PRESS_MS, false),
            Some(FingerAction::LassoStarted(p))
        );
        let q = UiPoint { x: 1000, y: 1300 };
        assert_eq!(
            t.on_frame(&[contact(q)], &m, 1500, false, &ui, &g),
            Some(FingerAction::LassoPoint(q))
        );
        assert_eq!(
            t.on_frame(&[], &m, 1510, false, &ui, &g),
            Some(FingerAction::LassoFinished(q))
        );
    }

    #[test]
    fn movement_or_second_finger_wins_before_long_press() {
        let g = panel_geo();
        let m = identity();
        let mut ui = notebook_ui();
        ui.chrome.hide();
        let p = UiPoint { x: 900, y: 1200 };
        let moved = UiPoint {
            x: p.x + TAP_MAX_TRAVEL_PX + 1,
            y: p.y,
        };
        let mut t = FingerTracker::default();
        t.on_frame(&[contact(p)], &m, 0, false, &ui, &g);
        t.on_frame(&[contact(moved)], &m, 100, false, &ui, &g);
        assert_eq!(t.on_tick(LASSO_LONG_PRESS_MS + 100, false), None);

        let mut t = FingerTracker::default();
        t.on_frame(&[contact(p)], &m, 0, false, &ui, &g);
        let second = TouchContact {
            slot: 1,
            x: Some(1200),
            y: Some(1200),
            palm: false,
            ..TouchContact::default()
        };
        assert_eq!(
            t.on_frame(&[contact(p), second], &m, 100, false, &ui, &g),
            None
        );
        assert_eq!(t.on_tick(LASSO_LONG_PRESS_MS + 100, false), None);
    }

    #[test]
    fn selection_drag_is_immediate_and_outside_down_clears() {
        let g = panel_geo();
        let m = identity();
        let mut ui = notebook_ui();
        ui.chrome.hide();
        let bounds = Rect::new(700, 900, 400, 500);
        let inside = UiPoint { x: 800, y: 1000 };
        let mut t = FingerTracker::default();
        assert_eq!(
            t.on_frame_with_selection(&[contact(inside)], &m, 0, false, &ui, &g, Some(bounds)),
            Some(FingerAction::SelectionMoveStarted(inside))
        );
        let moved = UiPoint { x: 850, y: 1040 };
        assert_eq!(
            t.on_frame_with_selection(&[contact(moved)], &m, 10, false, &ui, &g, Some(bounds)),
            Some(FingerAction::SelectionMovePoint(moved))
        );
        assert_eq!(
            t.on_frame_with_selection(&[], &m, 20, false, &ui, &g, Some(bounds)),
            Some(FingerAction::SelectionMoveFinished(moved))
        );

        let outside = UiPoint { x: 1400, y: 1800 };
        let mut t = FingerTracker::default();
        assert_eq!(
            t.on_frame_with_selection(&[contact(outside)], &m, 0, false, &ui, &g, Some(bounds)),
            Some(FingerAction::SelectionCleared)
        );
    }

    #[test]
    fn pen_suppression_never_activates_lasso() {
        let g = panel_geo();
        let m = identity();
        let mut ui = notebook_ui();
        ui.chrome.hide();
        let p = UiPoint { x: 900, y: 1200 };
        let mut arb = TouchArbiter::default();
        arb.on_stylus_event(StylusEventKind::Down, true, 0);
        assert_eq!(arb.on_frame(&[contact(p)], &m, 10, true, &ui, &g), None);
        assert_eq!(arb.on_tick(LASSO_LONG_PRESS_MS + 10, true), None);
    }

    /// FingerTracker emits one SelectionMovePoint per contact frame. Combined with the
    /// runtime's drain loop (`try_next` → handle → present → next), every intermediate
    /// move is processed before Up can become `SelectionMoveFinished`.
    #[test]
    fn selection_move_emits_every_intermediate_point_before_finished() {
        let g = panel_geo();
        let m = identity();
        let mut ui = notebook_ui();
        ui.chrome.hide();
        let bounds = Rect::new(700, 900, 400, 500);
        let start = UiPoint { x: 800, y: 1000 };
        let mut t = FingerTracker::default();
        assert_eq!(
            t.on_frame_with_selection(&[contact(start)], &m, 0, false, &ui, &g, Some(bounds)),
            Some(FingerAction::SelectionMoveStarted(start))
        );
        let mut points = Vec::new();
        for i in 1..=8 {
            let p = UiPoint {
                x: start.x + i * 20,
                y: start.y + i * 10,
            };
            assert_eq!(
                t.on_frame_with_selection(
                    &[contact(p)],
                    &m,
                    u64::from(i) * 10,
                    false,
                    &ui,
                    &g,
                    Some(bounds)
                ),
                Some(FingerAction::SelectionMovePoint(p))
            );
            points.push(p);
        }
        let last = *points.last().unwrap();
        assert_eq!(
            t.on_frame_with_selection(&[], &m, 200, false, &ui, &g, Some(bounds)),
            Some(FingerAction::SelectionMoveFinished(last))
        );
        assert_eq!(points.len(), 8);
    }

    fn coalesce_batch(
        actions: &[FingerAction],
    ) -> (Vec<UiPoint>, Vec<FingerAction>, MovePreviewCoalesce) {
        let mut c = MovePreviewCoalesce::default();
        let mut rendered = Vec::new();
        let mut handled = Vec::new();
        for &action in actions {
            match c.ingest(action) {
                CoalesceDecision::Hold => {}
                CoalesceDecision::Supersede(a) => handled.push(a),
                CoalesceDecision::Handle(a) => {
                    if let Some(p) = c.take_pending_preview() {
                        rendered.push(p);
                    }
                    handled.push(a);
                }
            }
        }
        if let Some(p) = c.take_pending_preview() {
            rendered.push(p);
        }
        (rendered, handled, c)
    }

    #[test]
    fn latest_move_wins_one_preview_at_end_of_batch() {
        let a = UiPoint { x: 10, y: 10 };
        let b = UiPoint { x: 20, y: 20 };
        let c = UiPoint { x: 30, y: 30 };
        let (rendered, handled, stats) = coalesce_batch(&[
            FingerAction::SelectionMovePoint(a),
            FingerAction::SelectionMovePoint(b),
            FingerAction::SelectionMovePoint(c),
        ]);
        assert_eq!(rendered, vec![c]);
        assert!(handled.is_empty());
        assert_eq!(stats.moves_seen, 3);
        assert_eq!(stats.previews_rendered, 1);
        assert_eq!(stats.coalesced, 2);
    }

    #[test]
    fn finish_skips_stale_previews_and_is_not_queued_behind_them() {
        let a = UiPoint { x: 10, y: 10 };
        let b = UiPoint { x: 20, y: 20 };
        let c = UiPoint { x: 30, y: 30 };
        let (rendered, handled, stats) = coalesce_batch(&[
            FingerAction::SelectionMovePoint(a),
            FingerAction::SelectionMovePoint(b),
            FingerAction::SelectionMovePoint(c),
            FingerAction::SelectionMoveFinished(c),
        ]);
        assert!(
            rendered.is_empty(),
            "Finish must not paint A/B/C first: {rendered:?}"
        );
        assert_eq!(handled, vec![FingerAction::SelectionMoveFinished(c)]);
        assert_eq!(stats.moves_seen, 3);
        assert_eq!(stats.previews_rendered, 0);
        assert_eq!(stats.coalesced, 3);
    }

    #[test]
    fn cancel_wins_over_pending_preview() {
        let a = UiPoint { x: 10, y: 10 };
        let b = UiPoint { x: 20, y: 20 };
        let (rendered, handled, stats) = coalesce_batch(&[
            FingerAction::SelectionMovePoint(a),
            FingerAction::SelectionMovePoint(b),
            FingerAction::SelectionMoveCancelled,
        ]);
        assert!(rendered.is_empty());
        assert_eq!(handled, vec![FingerAction::SelectionMoveCancelled]);
        assert_eq!(stats.previews_rendered, 0);
        assert_eq!(stats.coalesced, 2);
    }

    #[test]
    fn lasso_drawing_points_are_not_coalesced() {
        let a = UiPoint { x: 10, y: 10 };
        let b = UiPoint { x: 40, y: 50 };
        let c = UiPoint { x: 80, y: 90 };
        let (rendered, handled, stats) = coalesce_batch(&[
            FingerAction::LassoPoint(a),
            FingerAction::LassoPoint(b),
            FingerAction::LassoPoint(c),
        ]);
        assert!(rendered.is_empty());
        assert_eq!(
            handled,
            vec![
                FingerAction::LassoPoint(a),
                FingerAction::LassoPoint(b),
                FingerAction::LassoPoint(c),
            ]
        );
        assert_eq!(stats.moves_seen, 0);
        assert_eq!(stats.coalesced, 0);
    }

    #[test]
    fn raw_frames_still_all_reach_the_arbiter_before_coalesce() {
        let g = panel_geo();
        let m = identity();
        let mut ui = notebook_ui();
        ui.chrome.hide();
        let bounds = Rect::new(700, 900, 400, 500);
        let start = UiPoint { x: 800, y: 1000 };
        let mut t = FingerTracker::default();
        let mut actions = Vec::new();
        actions.push(
            t.on_frame_with_selection(&[contact(start)], &m, 0, false, &ui, &g, Some(bounds))
                .unwrap(),
        );
        let mut moves = Vec::new();
        for i in 1..=4 {
            let p = UiPoint {
                x: start.x + i * 10,
                y: start.y,
            };
            let a = t
                .on_frame_with_selection(
                    &[contact(p)],
                    &m,
                    u64::from(i) * 8,
                    false,
                    &ui,
                    &g,
                    Some(bounds),
                )
                .unwrap();
            actions.push(a);
            moves.push(p);
        }
        actions.push(
            t.on_frame_with_selection(&[], &m, 50, false, &ui, &g, Some(bounds))
                .unwrap(),
        );
        assert_eq!(actions.len(), 6);
        assert!(matches!(actions[0], FingerAction::SelectionMoveStarted(_)));
        assert!(matches!(actions[1], FingerAction::SelectionMovePoint(_)));
        assert!(matches!(actions[5], FingerAction::SelectionMoveFinished(_)));

        let second = TouchContact {
            slot: 1,
            x: Some(1200),
            y: Some(1200),
            palm: false,
            ..TouchContact::default()
        };
        let mut t = FingerTracker::default();
        t.on_frame_with_selection(&[contact(start)], &m, 0, false, &ui, &g, Some(bounds));
        assert_eq!(
            t.on_frame_with_selection(
                &[contact(start), second],
                &m,
                10,
                false,
                &ui,
                &g,
                Some(bounds)
            ),
            Some(FingerAction::SelectionMoveCancelled)
        );

        let mut arb = TouchArbiter::default();
        arb.on_frame_with_selection(&[contact(start)], &m, 0, true, &ui, &g, Some(bounds));
        arb.on_stylus_event(StylusEventKind::Down, true, 5);
        let cancelled = arb.take_cancelled_action();
        assert_eq!(cancelled, Some(FingerAction::SelectionMoveCancelled));
    }

    /// Slow preview no longer implies one render per Move before Up can run.
    #[test]
    fn slow_preview_consumer_does_not_delay_up_by_move_count() {
        let cadence_ms = 8u64;
        let render_ms = 25u64;
        let moves = 12u64;
        let mut actions = Vec::new();
        let last = UiPoint {
            x: 100 + (moves as u32) * 10,
            y: 100,
        };
        for i in 1..=moves {
            actions.push(FingerAction::SelectionMovePoint(UiPoint {
                x: 100 + (i as u32) * 10,
                y: 100,
            }));
        }
        actions.push(FingerAction::SelectionMoveFinished(last));
        let (rendered, handled, stats) = coalesce_batch(&actions);
        assert!(rendered.is_empty());
        assert_eq!(handled.len(), 1);
        assert!(matches!(handled[0], FingerAction::SelectionMoveFinished(_)));
        assert_eq!(stats.previews_rendered, 0);
        assert_eq!(u64::from(stats.moves_seen), moves);
        // Presentation work in the same drain is the Finish commit, not N previews.
        let presentation_units = u64::from(stats.previews_rendered) + handled.len() as u64;
        assert_eq!(presentation_units, 1);
        let old_serial_age = render_ms * moves - cadence_ms * (moves + 1);
        assert!(old_serial_age > 100);
    }

    /// Fast drags reclassify the same slot as palm shortly before lift. That must Finish at
    /// the last non-palm point, not snap the selection back to the drag origin.
    #[test]
    fn palm_reclassification_during_selection_move_finishes_at_last_point() {
        let g = panel_geo();
        let m = identity();
        let mut ui = notebook_ui();
        ui.chrome.hide();
        let bounds = Rect::new(700, 900, 400, 500);
        let start = UiPoint { x: 800, y: 1000 };
        let last = UiPoint { x: 1100, y: 1000 };
        let mut t = FingerTracker::default();
        assert!(matches!(
            t.on_frame_with_selection(&[contact(start)], &m, 0, false, &ui, &g, Some(bounds)),
            Some(FingerAction::SelectionMoveStarted(_))
        ));
        assert_eq!(
            t.on_frame_with_selection(&[contact(last)], &m, 20, false, &ui, &g, Some(bounds)),
            Some(FingerAction::SelectionMovePoint(last))
        );
        let mut palm = contact(start);
        palm.palm = true;
        assert_eq!(
            t.on_frame_with_selection(&[palm], &m, 30, false, &ui, &g, Some(bounds)),
            None,
            "same-slot palm reclass must not cancel an in-progress selection move"
        );
        assert_eq!(
            t.on_frame_with_selection(&[], &m, 40, false, &ui, &g, Some(bounds)),
            Some(FingerAction::SelectionMoveFinished(last))
        );
    }

    #[test]
    fn palm_at_idle_still_does_not_start_a_selection_move() {
        let g = panel_geo();
        let m = identity();
        let mut ui = notebook_ui();
        ui.chrome.hide();
        let bounds = Rect::new(700, 900, 400, 500);
        let start = UiPoint { x: 800, y: 1000 };
        let mut palm = contact(start);
        palm.palm = true;
        let mut t = FingerTracker::default();
        assert_eq!(
            t.on_frame_with_selection(&[palm], &m, 0, false, &ui, &g, Some(bounds)),
            None
        );
        assert_eq!(
            t.on_frame_with_selection(&[], &m, 20, false, &ui, &g, Some(bounds)),
            None
        );
        assert!(t.is_idle());
    }

    #[test]
    fn saturated_zero_now_ms_classifies_tap_without_proving_short_contact() {
        let g = panel_geo();
        let m = identity();
        let ui = UiState::launch_home(0, 9);
        let p = UiPoint { x: 400, y: 400 };
        let mut t = FingerTracker::default();
        assert_eq!(t.on_frame(&[contact(p)], &m, 0, false, &ui, &g), None);
        assert_eq!(
            t.on_frame(&[], &m, 0, false, &ui, &g),
            Some(FingerAction::Tap(p))
        );
        let d = t.last_classify().expect("classify diag");
        assert_eq!(d.start_now_ms, 0);
        assert_eq!(d.up_now_ms, 0);
        assert_eq!(d.dur_ms, 0);
        assert!(d.classified_as_tap);
        assert_eq!(d.tap_max_ms, TAP_MAX_MS);
    }

    #[test]
    fn tracker_dur_ms_is_the_exact_saturating_sub_used_to_classify() {
        let g = panel_geo();
        let m = identity();
        let ui = UiState::launch_home(0, 9);
        let p = UiPoint { x: 400, y: 400 };
        let mut t = FingerTracker::default();
        t.on_frame(&[contact(p)], &m, 0, false, &ui, &g);
        assert_eq!(t.on_frame(&[], &m, TAP_MAX_MS + 1, false, &ui, &g), None);
        let d = t.last_classify().expect("classify diag");
        assert_eq!(d.dur_ms, TAP_MAX_MS + 1);
        assert!(!d.classified_as_tap);
        assert_eq!(d.start_now_ms, 0);
        assert_eq!(d.up_now_ms, TAP_MAX_MS + 1);
    }

    #[test]
    fn classify_diag_does_not_change_tap_action() {
        let g = panel_geo();
        let m = identity();
        let ui = UiState::launch_home(0, 9);
        let p = UiPoint { x: 500, y: 600 };
        let mut a = FingerTracker::default();
        a.on_frame(&[contact(p)], &m, 10, false, &ui, &g);
        let act_a = a.on_frame(&[], &m, 90, false, &ui, &g);
        let mut b = FingerTracker::default();
        b.on_frame(&[contact(p)], &m, 10, false, &ui, &g);
        let act_b = b.on_frame(&[], &m, 90, false, &ui, &g);
        assert_eq!(act_a, act_b);
        assert_eq!(act_a, Some(FingerAction::Tap(p)));
        let _ = b.last_classify();
        assert_eq!(act_a, Some(FingerAction::Tap(p)));
        let d = b.last_classify().unwrap();
        assert_eq!(d.dur_ms, 80);
        assert!(d.classified_as_tap);
    }
}
