//! Home notebook title search: filter routing, clear/cancel/empty, open by library index.

#[cfg(test)]
mod tests {
    use crate::app_ui::{Redraw, UiCommand, UiState};
    use crate::folders::FolderInfo;
    use crate::home::{home_rows, HomeCard, HomeHit, HomeLayout, HomeRow, ROWS_PER_PAGE};
    use crate::keyboard::{EditorKey, KeyboardLayout, TitleEditor};
    use crate::modal::{Modal, TitlePurpose};
    use crate::ui_gfx::{panel_geo, Rect, UiPoint};
    use kindle_platform::FbGeometry;

    fn cards() -> Vec<HomeCard> {
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

    fn folders() -> Vec<FolderInfo> {
        vec![FolderInfo {
            id: "folder-1".into(),
            title: "Projects".into(),
            notebook_ids: vec!["notebook-3".into()],
        }]
    }

    fn center(r: Rect) -> UiPoint {
        UiPoint {
            x: r.x + r.w / 2,
            y: r.y + r.h / 2,
        }
    }

    fn layout(g: &FbGeometry, ui: &UiState, c: &[HomeCard]) -> HomeLayout {
        HomeLayout::for_rows(
            g,
            &ui.home,
            &ui.home_rows(c),
            ui.open_folder().is_some(),
            ui.home_search_query().is_some(),
        )
    }

    fn key_point(g: &FbGeometry, editor: &TitleEditor, key: EditorKey) -> UiPoint {
        let layout = KeyboardLayout::new(g, editor.symbols());
        let (_, r) = layout
            .keys
            .iter()
            .find(|(k, _)| *k == key)
            .unwrap_or_else(|| panic!("no key {key:?}"));
        center(*r)
    }

    fn editor_of(ui: &UiState) -> TitleEditor {
        match ui.modal.as_ref() {
            Some(Modal::TitleEntry { editor, .. }) => editor.clone(),
            other => panic!("expected title entry, got {other:?}"),
        }
    }

    fn tap_key(ui: &mut UiState, g: &FbGeometry, c: &[HomeCard], key: EditorKey) -> UiCommand {
        let p = key_point(g, &editor_of(ui), key);
        ui.on_tap(g, p, c, "Notebook 4", None)
    }

    #[test]
    fn search_tap_opens_keyboard_submit_filters_cancel_keeps_prior() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.settle_home(&c);

        let l = layout(&g, &ui, &c);
        assert_eq!(
            ui.on_tap(&g, center(l.search), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        match &ui.modal {
            Some(Modal::TitleEntry {
                purpose: TitlePurpose::Search,
                editor,
            }) => assert!(editor.text().is_empty()),
            other => panic!("{other:?}"),
        }

        // Type "al" then SEARCH → filter; library indices 0 and 2.
        for ch in ['a', 'l'] {
            assert!(matches!(
                tap_key(&mut ui, &g, &c, EditorKey::Char(ch)),
                UiCommand::Redraw(_) | UiCommand::None
            ));
        }
        assert_eq!(
            tap_key(&mut ui, &g, &c, EditorKey::Submit),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.modal.is_none());
        assert_eq!(ui.home_search.as_deref(), Some("al"));
        assert_eq!(ui.home.page, 0);
        assert!(!ui.home.reveal);
        assert_eq!(
            ui.home_rows(&c),
            vec![HomeRow::Notebook(0), HomeRow::Notebook(2)]
        );

        // Re-open search; Cancel leaves the prior filter.
        let l = layout(&g, &ui, &c);
        ui.on_tap(&g, center(l.search), &c, "Notebook 4", None);
        assert_eq!(
            tap_key(&mut ui, &g, &c, EditorKey::Cancel),
            UiCommand::Redraw(Redraw::Full)
        );
        assert_eq!(ui.home_search.as_deref(), Some("al"));
    }

    #[test]
    fn empty_submit_and_clear_search_restore_main_list() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.set_home_search(Some("  alpha  ".into()));
        assert_eq!(ui.home_search.as_deref(), Some("alpha"));
        ui.home.page = 1;
        ui.home.reveal = true;

        // Whitespace-only SEARCH clears.
        ui.modal = Some(Modal::TitleEntry {
            purpose: TitlePurpose::Search,
            editor: TitleEditor::new("   "),
        });
        assert_eq!(
            tap_key(&mut ui, &g, &c, EditorKey::Submit),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_search.is_none());
        assert_eq!(ui.home.page, 0);
        assert!(!ui.home.reveal);
        assert!(
            ui.home_rows(&c)
                .iter()
                .any(|r| matches!(r, HomeRow::Folders(_))),
            "folders return after clear"
        );

        // Clear control also clears.
        ui.set_home_search(Some("beta".into()));
        let l = layout(&g, &ui, &c);
        let clear = l.clear_search.expect("clear hit target");
        assert!(clear.w >= 130 && clear.h >= 130);
        assert_eq!(
            ui.on_tap(&g, center(clear), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_search.is_none());
        assert_eq!(ui.home.page, 0);
        assert!(!ui.home.reveal);
    }

    #[test]
    fn clear_search_from_later_results_page_resets_to_page_zero() {
        // Enough matches for ≥2 valid result pages so page 1 is not clamped away by settle.
        let match_count = ROWS_PER_PAGE + 3;
        let mut c: Vec<HomeCard> = (1..=match_count)
            .map(|i| HomeCard {
                notebook_id: format!("notebook-{i}"),
                title: format!("Match {i}"),
                page_count: Some(1),
            })
            .collect();
        c.push(HomeCard {
            notebook_id: "notebook-other".into(),
            title: "Other".into(),
            page_count: Some(1),
        });
        let g = panel_geo();
        let mut ui = UiState::launch_home(0, c.len());
        ui.set_home_search(Some("Match".into()));
        let filtered = ui.home_rows(&c);
        assert_eq!(filtered.len(), match_count);
        assert!(
            filtered.len() > ROWS_PER_PAGE,
            "need a second results page; got {} rows",
            filtered.len()
        );
        // PageNext leaves reveal=false; with reveal set, settle would jump to the selected
        // notebook's row and mask a missing page reset on clear.
        ui.home.reveal = false;
        ui.home.page = 1;
        ui.settle_home(&c);
        assert_eq!(
            ui.home.page, 1,
            "page 1 must remain valid under the active filter (not settle-clamped)"
        );
        assert_eq!(
            layout(&g, &ui, &c)
                .cards
                .iter()
                .map(|(i, _, _)| *i)
                .collect::<Vec<_>>(),
            filtered
                .iter()
                .skip(ROWS_PER_PAGE)
                .filter_map(|r| match r {
                    HomeRow::Notebook(i) => Some(*i),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            "tap layout must show the same filtered library indices as home_rows"
        );

        let l = layout(&g, &ui, &c);
        let clear = l.clear_search.expect("clear hit target");
        assert_eq!(
            ui.on_tap(&g, center(clear), &c, "Notebook N", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_search.is_none());
        assert_eq!(
            ui.home.page, 0,
            "clear must reset pagination, not rely on clamp"
        );
        assert!(!ui.home.reveal);
        ui.settle_home(&c);
        assert_eq!(ui.home.page, 0);
    }

    #[test]
    fn search_submit_bypasses_title_validation_for_short_queries() {
        // "a" is a valid search query but would also pass title validation; empty must clear
        // without the "title can't be empty" error modal from TitleEditor::apply.
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.home_search = Some("alpha".into());
        ui.modal = Some(Modal::TitleEntry {
            purpose: TitlePurpose::Search,
            editor: TitleEditor::new(""),
        });
        let cmd = tap_key(&mut ui, &g, &c, EditorKey::Submit);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert!(
            ui.modal.is_none(),
            "no Invalid/Error modal for empty search"
        );
        assert!(ui.home_search.is_none());
    }

    #[test]
    fn filtered_open_card_uses_original_library_index() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.set_home_search(Some("draft".into()));
        let rows = ui.home_rows(&c);
        assert_eq!(rows, vec![HomeRow::Notebook(2)]);
        let l = layout(&g, &ui, &c);
        assert_eq!(l.cards.len(), 1);
        assert_eq!(l.cards[0].0, 2);
        assert_eq!(
            l.hit(center(l.cards[0].1), &ui.home, c.len()),
            Some(HomeHit::OpenCard(2))
        );
        assert_eq!(
            ui.on_tap(&g, center(l.cards[0].1), &c, "Notebook 4", None),
            UiCommand::OpenNotebook("notebook-3".into())
        );
    }

    #[test]
    fn search_does_not_steal_folder_title_area() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.home_folder = Some("folder-1".into());
        // Even with a leftover query string, open folder wins for chrome/rows.
        ui.home_search = Some("alpha".into());
        assert!(ui.home_search_query().is_none());
        assert_eq!(
            ui.home_rows(&c),
            home_rows(&c, &ui.folders, Some("folder-1"), None)
        );
        let l = layout(&g, &ui, &c);
        assert!(l.in_folder);
        assert!(l.clear_search.is_none());
        assert_eq!(l.hit(center(l.search), &ui.home, c.len()), None);
    }

    #[test]
    fn applying_search_clears_folder_and_stale_reveal() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.home_folder = Some("folder-1".into());
        ui.home.page = 3;
        ui.home.reveal = true;
        ui.modal = Some(Modal::TitleEntry {
            purpose: TitlePurpose::Search,
            editor: TitleEditor::new("Beta"),
        });
        assert_eq!(
            tap_key(&mut ui, &g, &c, EditorKey::Submit),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_folder.is_none());
        assert_eq!(ui.home_search.as_deref(), Some("Beta"));
        assert_eq!(ui.home.page, 0);
        assert!(!ui.home.reveal);
        // settle must not jump away from page 0 due to reveal.
        ui.settle_home(&c);
        assert_eq!(ui.home.page, 0);
    }

    #[test]
    fn empty_results_rows_are_empty_until_cleared() {
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.set_home_search(Some("no-such-title".into()));
        assert!(ui.home_rows(&c).is_empty());
        ui.set_home_search(None);
        assert!(
            !ui.home_rows(&c).is_empty(),
            "clear restores folders + unfiled notebooks"
        );
    }
}
