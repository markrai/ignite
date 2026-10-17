//! Gate F11.1 host tests: Kindle sleep/wake display invalidation, first-contact fail-safe,
//! and preservation of the current UiState across screensaver cycles.

#[cfg(test)]
mod tests {
    use kindle_platform::{ClockDomain, FbGeometry, StylusEventKind, StylusTool};
    use notebook_core::PagePoint;

    use crate::app_ui::{DownRoute, NotebookUiContext, PenRouter, UiCommand, UiState};
    use crate::chrome::{ChromeLayout, PageChrome};
    use crate::display_events::DisplayEvents;
    use crate::home::{AppView, HomeCard, HomeLayout, HomeState};
    use crate::keyboard::{EditorKey, KeyboardLayout, TitleEditor};
    use crate::modal::{Modal, TitlePurpose};
    use crate::orch::{FakeTransport, StrokeController, TransportStep};
    use crate::store::MemoryPageStore;
    use crate::ui_gfx::{panel_geo, Rect, UiPoint};

    const SLEEP: DisplayEvents = DisplayEvents {
        invalidated: true,
        restored: false,
    };
    const WAKE: DisplayEvents = DisplayEvents {
        invalidated: false,
        restored: true,
    };

    fn cards(n: usize) -> Vec<HomeCard> {
        (1..=n)
            .map(|i| HomeCard {
                notebook_id: format!("notebook-{i}"),
                title: format!("Notebook {i}"),
                page_count: Some(1),
            })
            .collect()
    }

    fn center(r: Rect) -> UiPoint {
        UiPoint {
            x: r.x + r.w / 2,
            y: r.y + r.h / 2,
        }
    }

    fn down(
        router: &mut PenRouter,
        ui: &UiState,
        g: &FbGeometry,
        p: UiPoint,
        tool: StylusTool,
    ) -> DownRoute {
        router.route_event(ui, g, StylusEventKind::Down, tool, Some(p), false)
    }

    fn ev(
        router: &mut PenRouter,
        ui: &UiState,
        g: &FbGeometry,
        kind: StylusEventKind,
    ) -> DownRoute {
        router.route_event(ui, g, kind, StylusTool::Pen, None, false)
    }

    /// Simulates the runtime's successful full present after a redraw request.
    fn present(router: &mut PenRouter) -> bool {
        if router.redraw_requested() {
            router.display_presented();
            true
        } else {
            false
        }
    }

    #[test]
    fn home_survives_sleep_wake_and_contact_while_invalid_is_consumed() {
        let g = panel_geo();
        let c = cards(14);
        let mut ui = UiState::launch_home(8, 14); // viewport page 1
        let before = ui.clone();
        let mut router = PenRouter::default();

        router.on_display_events(SLEEP);
        assert!(!router.display_valid());
        assert!(
            !router.redraw_requested(),
            "never paint over the screensaver"
        );

        // A contact while invalid: consumed end to end, requests a repaint (fail-safe).
        let layout = HomeLayout::new(&g, &ui.home, 14);
        let card = center(layout.cards[0].1);
        assert_eq!(
            down(&mut router, &ui, &g, card, StylusTool::Pen),
            DownRoute::Consume
        );
        assert_eq!(
            ev(&mut router, &ui, &g, StylusEventKind::Move),
            DownRoute::Consume
        );
        assert_eq!(
            ev(&mut router, &ui, &g, StylusEventKind::Up),
            DownRoute::Consume
        );
        assert!(router.redraw_requested());

        router.on_display_events(WAKE);
        assert!(router.redraw_requested(), "wake requests a full present");
        assert!(
            !router.display_valid(),
            "input gated until the present completes"
        );
        assert!(present(&mut router));
        assert!(router.display_valid());

        // UiState untouched: same view, selection, and viewport.
        assert_eq!(ui, before);
        assert_eq!(ui.view, AppView::Home);
        assert_eq!(ui.home, HomeState::selecting(8, 14));

        // Next contact behaves normally.
        match down(&mut router, &ui, &g, card, StylusTool::Pen) {
            DownRoute::UiTap(p) => assert_eq!(
                ui.on_tap(&g, p, &c, "x", None),
                UiCommand::OpenNotebook("notebook-7".into())
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn invisible_home_card_tap_never_opens_a_notebook() {
        // Regression for the physical defect: blank panel after wake, pen Down at the center
        // of a Home card must not yield OpenNotebook.
        let g = panel_geo();
        let c = cards(3);
        let mut ui = UiState::launch_home(0, 3);
        let mut router = PenRouter::default();
        router.on_display_events(SLEEP);
        let layout = HomeLayout::new(&g, &ui.home, 3);
        for (_, card, menu) in &layout.cards {
            for p in [center(*card), center(*menu)] {
                let route = down(&mut router, &ui, &g, p, StylusTool::Pen);
                assert_eq!(route, DownRoute::Consume);
                assert!(!matches!(route, DownRoute::UiTap(_)));
                ev(&mut router, &ui, &g, StylusEventKind::Up);
            }
        }
        // Even if the fail-safe redraw completes mid-contact, that contact stays consumed.
        down(
            &mut router,
            &ui,
            &g,
            center(layout.new_button),
            StylusTool::Pen,
        );
        assert!(present(&mut router));
        assert_eq!(
            ev(&mut router, &ui, &g, StylusEventKind::Move),
            DownRoute::Consume
        );
        assert_eq!(
            ev(&mut router, &ui, &g, StylusEventKind::Up),
            DownRoute::Consume
        );
        // No command was ever resolved; the modal never opened.
        assert!(ui.modal.is_none());
        let _ = ui.on_tap(&g, UiPoint { x: 0, y: 0 }, &c, "x", None);
        assert!(ui.modal.is_none());
    }

    #[test]
    fn notebook_page_state_and_chrome_survive_sleep_wake() {
        let g = panel_geo();
        let mut ui = UiState::launch_home(0, 1);
        ui.enter_notebook();
        ui.chrome.reveal(0); // F11.3: chrome visible when the Kindle sleeps
        let before = ui.clone();
        let page = PageChrome { index: 3, count: 7 };
        let mut router = PenRouter::default();

        router.on_display_events(SLEEP);
        // Chrome UNDO under an invisible panel: consumed, no command.
        let undo = center(ChromeLayout::new(&g).undo);
        assert_eq!(
            down(&mut router, &ui, &g, undo, StylusTool::Pen),
            DownRoute::Consume
        );
        ev(&mut router, &ui, &g, StylusEventKind::Up);
        // Eraser too.
        assert_eq!(
            down(
                &mut router,
                &ui,
                &g,
                UiPoint { x: 900, y: 1200 },
                StylusTool::Eraser
            ),
            DownRoute::Consume
        );
        ev(&mut router, &ui, &g, StylusEventKind::Up);

        router.on_display_events(WAKE);
        assert!(present(&mut router));
        assert_eq!(ui, before);
        assert_eq!(ui.view, AppView::Notebook);
        // Page position is owned by the notebook manifest, which sleep never touches; chrome
        // resolves identically after wake.
        let mut ui2 = ui.clone();
        assert_eq!(
            ui2.on_tap(&g, undo, &cards(1), "x", Some(NotebookUiContext { page })),
            UiCommand::Undo
        );
        assert_eq!(page.label(), "3 of 7");
    }

    #[test]
    fn modal_and_title_editor_text_survive_sleep_wake() {
        let g = panel_geo();
        let mut ui = UiState::launch_home(0, 2);
        let mut editor = TitleEditor::new("Id");
        editor.apply(EditorKey::Mode);
        editor.apply(EditorKey::Char('3'));
        ui.show_modal(Modal::TitleEntry {
            purpose: TitlePurpose::Rename {
                notebook_id: "notebook-2".into(),
                original: "Personal".into(),
            },
            editor,
        });
        let before = ui.clone();
        let mut router = PenRouter::default();

        router.on_display_events(SLEEP);
        // A key tap while invisible must not type.
        let kb = KeyboardLayout::new(&g, true);
        let (_, key) = kb
            .keys
            .iter()
            .find(|(k, _)| *k == EditorKey::Char('1'))
            .unwrap();
        assert_eq!(
            down(&mut router, &ui, &g, center(*key), StylusTool::Pen),
            DownRoute::Consume
        );
        ev(&mut router, &ui, &g, StylusEventKind::Up);
        router.on_display_events(WAKE);
        assert!(present(&mut router));

        assert_eq!(ui, before, "same modal, same text, same keyboard mode");
        match ui.modal.as_ref().unwrap() {
            Modal::TitleEntry { editor, .. } => {
                assert_eq!(editor.text(), "Id3");
                assert!(editor.symbols());
            }
            other => panic!("{other:?}"),
        }
        // Modal still owns input after wake.
        match down(&mut router, &ui, &g, center(*key), StylusTool::Pen) {
            DownRoute::UiTap(_) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn repeated_cycles_do_not_leak_ownership_or_duplicate_state() {
        let g = panel_geo();
        let ui = UiState::launch_home(2, 9);
        let before = ui.clone();
        let mut router = PenRouter::default();
        for _ in 0..3 {
            // A UI contact is down when the Kindle sleeps: ownership is dropped, no Up invented.
            let p = center(HomeLayout::new(&g, &ui.home, 9).new_button);
            assert!(matches!(
                down(&mut router, &ui, &g, p, StylusTool::Pen),
                DownRoute::UiTap(_)
            ));
            router.on_display_events(SLEEP);
            router.on_display_events(SLEEP); // duplicate relay is harmless
            router.on_display_events(WAKE);
            assert!(present(&mut router));
            assert!(!present(&mut router), "exactly one present per wake");
            assert!(router.display_valid());
            assert_eq!(router, PenRouter::default(), "no residual ownership");
        }
        assert_eq!(ui, before);
        // Both events in one poll (fast cycle): still one gated full present.
        router.on_display_events(DisplayEvents {
            invalidated: true,
            restored: true,
        });
        assert!(!router.display_valid() && router.redraw_requested());
        assert!(present(&mut router));
        // Wake without a seen invalidate (missed relay) also gates and repaints.
        router.on_display_events(WAKE);
        assert!(!router.display_valid() && router.redraw_requested());
        assert!(present(&mut router));
    }

    #[test]
    fn no_ink_during_invalid_display_and_interrupted_stroke_is_not_committed() {
        let g = panel_geo();
        let mut ui = UiState::launch_home(0, 1);
        ui.enter_notebook();
        let mut router = PenRouter::default();
        let mut c: StrokeController<FakeTransport, MemoryPageStore> = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            MemoryPageStore::new(),
        );
        let pt = |x: f32, y: f32| PagePoint::new(x * 8.4, y * 8.4);

        // A stroke is in progress when the Kindle sleeps.
        assert_eq!(
            down(
                &mut router,
                &ui,
                &g,
                UiPoint { x: 400, y: 1000 },
                StylusTool::Pen
            ),
            DownRoute::Page
        );
        c.on_pen_down(pt(400.0, 1000.0), Some(0.5)).unwrap();
        c.on_pen_move(pt(500.0, 1050.0), Some(0.5), None).unwrap();
        router.on_display_events(SLEEP);
        // Runtime: abandon the wet stroke through the stream-gap path (no Up invented).
        c.on_stream_gap(true).unwrap();
        assert_eq!(c.session.committed_count(), 0);
        assert!(!c.is_armed());

        // Late samples of that contact and new contacts while invalid never reach ink.
        let steps_before = c.transport().steps.len();
        for kind in [
            StylusEventKind::Move,
            StylusEventKind::Up,
            StylusEventKind::Down,
            StylusEventKind::Move,
            StylusEventKind::Up,
        ] {
            let r = router.route_event(
                &ui,
                &g,
                kind,
                StylusTool::Pen,
                Some(UiPoint { x: 600, y: 1100 }),
                false,
            );
            assert_eq!(r, DownRoute::Consume);
        }
        assert_eq!(c.transport().steps.len(), steps_before);
        assert!(!c.transport().steps[steps_before..].contains(&TransportStep::Stamp));
        assert_eq!(c.session.committed_count(), 0);
    }
}
