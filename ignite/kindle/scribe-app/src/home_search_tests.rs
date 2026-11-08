//! Live hierarchical Home search: docked keyboard, folder matches, session semantics.

#[cfg(test)]
mod tests {
    use crate::app_ui::{Redraw, UiCommand, UiState};
    use crate::folders::FolderInfo;
    use crate::home::{
        folder_count_label, folder_match_count, folder_tile_content, home_rows,
        notebook_meta_label, FolderTileContent, HomeCard, HomeLayout, HomeRow, HomeSearchGeometry,
        HomeState, HomeViewport, ROWS_PER_PAGE,
    };
    use crate::keyboard::{
        docked_keys_height, EditorKey, KeyboardLayout, TitleEditor, SEARCH_KEYBOARD_GAP,
    };
    use crate::modal::Modal;
    use crate::ui_gfx::{panel_geo, text_width, Rect, UiPoint};
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

    /// Folder whose title also matches `"al"` / `"alpha"` (independent of children).
    /// Title length stays near `"Projects"` so search-tile compact layout thresholds hold.
    fn folders_title_alpha() -> Vec<FolderInfo> {
        vec![FolderInfo {
            id: "folder-1".into(),
            title: "Alphaes".into(),
            notebook_ids: vec!["notebook-3".into()],
        }]
    }

    /// Global `"al"` / `"ALPHA"` notebook-only projection for the default fixture.
    fn alpha_notebook_rows() -> Vec<HomeRow> {
        vec![HomeRow::Notebook(0), HomeRow::Notebook(2)]
    }

    /// Notebook hits plus a title-matched folder tile.
    fn alpha_both_rows() -> Vec<HomeRow> {
        vec![
            HomeRow::Notebook(0),
            HomeRow::Notebook(2),
            HomeRow::Folders(vec![0]),
        ]
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
            ui.home_search_chrome_active(),
            ui.home_viewport(g),
        )
    }

    fn key_point(g: &FbGeometry, editor: &TitleEditor, key: EditorKey) -> UiPoint {
        let layout = KeyboardLayout::docked(g, editor.symbols());
        let (_, r) = layout
            .keys
            .iter()
            .find(|(k, _)| *k == key)
            .unwrap_or_else(|| panic!("no key {key:?}"));
        center(*r)
    }

    fn editor_of(ui: &UiState) -> TitleEditor {
        ui.search_editor
            .clone()
            .unwrap_or_else(|| panic!("expected search_editor"))
    }

    fn tap_key(ui: &mut UiState, g: &FbGeometry, c: &[HomeCard], key: EditorKey) -> UiCommand {
        let p = key_point(g, &editor_of(ui), key);
        ui.on_tap(g, p, c, "Notebook 4", None)
    }

    /// Host layout geometry for a rectangular panel (stride = width, layout-only).
    fn panel_geo_sized(xres: u32, yres: u32) -> FbGeometry {
        FbGeometry {
            xres,
            yres,
            xres_virtual: xres,
            yres_virtual: yres,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: xres,
            smem_len: xres.saturating_mul(yres),
        }
    }

    fn search_geom_on_page(
        g: &FbGeometry,
        cards: &[HomeCard],
        folders: &[FolderInfo],
        query: &str,
        page: usize,
    ) -> HomeSearchGeometry {
        let rows = home_rows(cards, folders, None, Some(query));
        let mut state = HomeState::selecting(0, cards.len());
        state.page = page;
        HomeSearchGeometry::new(g, &state, &rows, false, true, false)
    }

    fn folder_row_page(rows: &[HomeRow], rows_per_page: usize) -> usize {
        rows.iter()
            .position(|r| matches!(r, HomeRow::Folders(_)))
            .map(|row| row / rows_per_page.max(1))
            .unwrap_or(0)
    }

    fn assert_layout_no_keyboard_overlap(g: &FbGeometry, geom: &HomeSearchGeometry) {
        let kb = &geom.keyboard;
        let home = &geom.home;

        assert_eq!(
            kb.keys_area.h,
            docked_keys_height(),
            "docked_keys_height must match laid-out keys_area ({}x{})",
            g.xres,
            g.yres
        );
        assert!(
            kb.keys_area.bottom() <= g.yres,
            "keyboard must fit in panel: bottom={} yres={}",
            kb.keys_area.bottom(),
            g.yres
        );
        assert_eq!(
            kb.keys_area.bottom(),
            g.yres,
            "docked keyboard should sit flush with the panel bottom"
        );
        assert!(
            home.content_band.bottom() + SEARCH_KEYBOARD_GAP <= kb.keys_area.y,
            "Home content/pager must leave the search gap above the keys: content_bottom={} keys_top={}",
            home.content_band.bottom(),
            kb.keys_area.y
        );
        assert!(
            !home.content_band.intersects(&kb.keys_area),
            "content band must not overlap the keyboard"
        );
        assert!(
            !home.pager_label.intersects(&kb.keys_area)
                && !home.pager_previous.intersects(&kb.keys_area)
                && !home.pager_next.intersects(&kb.keys_area),
            "pager hit targets must not overlap the keyboard"
        );
        for (_, card, menu) in &home.cards {
            assert!(
                !card.intersects(&kb.keys_area) && !menu.intersects(&kb.keys_area),
                "notebook card/menu must not overlap the keyboard"
            );
        }
        for (_, tile, menu) in &home.folders {
            assert!(
                !tile.intersects(&kb.keys_area) && !menu.intersects(&kb.keys_area),
                "folder tile/menu must not overlap the keyboard"
            );
            assert_folder_tile_content_bounds(*tile, *menu, "Alphaes");
        }
        // Key hit targets stay inside the keys band.
        for (_, r) in &kb.keys {
            assert!(
                r.y >= kb.keys_area.y && r.bottom() <= kb.keys_area.bottom(),
                "key {r:?} escapes keys_area"
            );
        }
    }

    fn assert_folder_tile_content_bounds(
        tile: crate::ui_gfx::Rect,
        menu: crate::ui_gfx::Rect,
        title: &str,
    ) {
        let content = folder_tile_content(tile, menu.x, title);
        assert_folder_tile_content_safe(tile, menu, content);
    }

    fn assert_folder_tile_content_safe(
        tile: crate::ui_gfx::Rect,
        menu: crate::ui_gfx::Rect,
        content: FolderTileContent,
    ) {
        for (name, r) in [
            ("icon", content.icon),
            ("title", content.title),
            ("meta", content.meta),
        ] {
            assert!(
                r.x >= tile.x
                    && r.right() <= tile.right()
                    && r.y >= tile.y
                    && r.bottom() <= tile.bottom(),
                "{name} {r:?} escapes tile {tile:?} (compact={})",
                content.compact
            );
        }
        assert!(
            content.title.right() <= menu.x && content.meta.right() <= menu.x,
            "title/meta must stay left of the menu column"
        );
        assert!(
            content.icon.bottom() <= content.title.y,
            "icon must sit above the title: icon={} title_y={} compact={}",
            content.icon.bottom(),
            content.title.y,
            content.compact
        );
        assert!(
            content.title.bottom() <= content.meta.y,
            "title must sit above the count: title_bottom={} meta_y={} compact={}",
            content.title.bottom(),
            content.meta.y,
            content.compact
        );
        assert!(
            !content.icon.intersects(&content.title)
                && !content.title.intersects(&content.meta)
                && !content.icon.intersects(&content.meta),
            "folder tile content rects must not overlap: {content:?}"
        );
        assert!(content.title_scale >= 3 && content.title_scale <= 7);
    }

    fn assert_search_composer_no_overlap(g: &FbGeometry) {
        let c = cards();
        let folders = folders_title_alpha();
        let rows = home_rows(&c, &folders, None, Some("al"));
        let geom0 = search_geom_on_page(g, &c, &folders, "al", 0);
        assert_layout_no_keyboard_overlap(g, &geom0);
        let folder_page = folder_row_page(&rows, geom0.viewport.rows_per_page);
        if folder_page != 0 {
            let geom_f = search_geom_on_page(g, &c, &folders, "al", folder_page);
            assert_layout_no_keyboard_overlap(g, &geom_f);
            assert!(
                !geom_f.home.folders.is_empty(),
                "folder tiles must remain hittable under the docked keyboard"
            );
        }
    }

    #[test]
    fn live_type_filters_hierarchically_before_done() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.settle_home(&g, &c);

        let l = layout(&g, &ui, &c);
        assert_eq!(
            ui.on_tap(&g, center(l.search), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.search_editor.is_some());
        assert!(
            ui.modal.is_none(),
            "search is not a full-screen title modal"
        );

        for ch in ['a', 'l'] {
            assert!(matches!(
                tap_key(&mut ui, &g, &c, EditorKey::Char(ch)),
                UiCommand::Redraw(Redraw::Region(_))
            ));
        }
        assert_eq!(ui.home_search.as_deref(), Some("al"));
        assert_eq!(
            ui.home_rows(&c),
            alpha_notebook_rows(),
            "matching notebooks only; parent folder title does not match"
        );
        assert_eq!(folder_match_count(&ui.folders[0], &c, "al"), 1);

        assert_eq!(
            tap_key(&mut ui, &g, &c, EditorKey::Submit),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.search_editor.is_none());
        assert_eq!(ui.home_search.as_deref(), Some("al"));
    }

    #[test]
    fn one_shot_shift_letter_updates_live_filter() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.begin_home_search();

        assert!(matches!(
            tap_key(&mut ui, &g, &c, EditorKey::Shift),
            UiCommand::Redraw(Redraw::Region(_))
        ));
        assert!(editor_of(&ui).shift());
        // Shift + 'b' → KeysChanged but text becomes "B"; live filter must update.
        assert!(matches!(
            tap_key(&mut ui, &g, &c, EditorKey::Char('b')),
            UiCommand::Redraw(Redraw::Region(_))
        ));
        assert_eq!(editor_of(&ui).text(), "B");
        assert!(!editor_of(&ui).shift());
        assert_eq!(ui.home_search.as_deref(), Some("B"));
        assert_eq!(ui.home_rows(&c), vec![HomeRow::Notebook(1)]);
    }

    #[test]
    fn cancel_restores_backup_query() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.set_home_search(Some("al".into()));
        ui.settle_home(&g, &c);

        let l = layout(&g, &ui, &c);
        ui.on_tap(&g, center(l.search), &c, "Notebook 4", None);
        assert_eq!(ui.home_search_backup.as_deref(), Some("al"));
        tap_key(&mut ui, &g, &c, EditorKey::Char('x'));
        assert_eq!(ui.home_search.as_deref(), Some("alx"));
        assert_eq!(
            tap_key(&mut ui, &g, &c, EditorKey::Cancel),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.search_editor.is_none());
        assert_eq!(ui.home_search.as_deref(), Some("al"));
        assert!(ui.home_search_backup.is_none());
    }

    #[test]
    fn clear_and_empty_draft_restore_unfiltered_rows() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.set_home_search(Some("alpha".into()));
        ui.home.page = 1;
        ui.home.reveal = true;

        let l = layout(&g, &ui, &c);
        let clear = l.clear_search.expect("clear hit target");
        assert!(clear.w >= 130 && clear.h >= 130);
        assert_eq!(
            ui.on_tap(&g, center(clear), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_search.is_none());
        assert_eq!(ui.home.page, 0);
        assert!(
            ui.home_rows(&c)
                .iter()
                .any(|r| matches!(r, HomeRow::Folders(_))),
            "folders return after clear"
        );

        ui.begin_home_search();
        tap_key(&mut ui, &g, &c, EditorKey::Char('z'));
        assert!(ui.home_rows(&c).is_empty());
        tap_key(&mut ui, &g, &c, EditorKey::Backspace);
        assert!(
            ui.home_rows(&c)
                .iter()
                .any(|r| matches!(r, HomeRow::Folders(_))),
            "empty draft shows the ordinary library"
        );
    }

    #[test]
    fn folder_drill_in_parks_query_shows_full_membership_and_back_restores() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders_title_alpha();
        // Add a non-matching member so full membership differs from filtered hits.
        ui.folders[0].notebook_ids.push("notebook-2".into());
        ui.begin_home_search();
        for ch in ['a', 'l'] {
            tap_key(&mut ui, &g, &c, EditorKey::Char(ch));
        }
        assert_eq!(ui.home_rows(&c), alpha_both_rows());
        let l = layout(&g, &ui, &c);
        assert_eq!(l.folders.len(), 1);
        assert_eq!(
            ui.on_tap(&g, center(l.folders[0].1), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(
            ui.search_editor.is_none(),
            "opening a folder dismisses keyboard"
        );
        assert!(
            ui.home_search.is_none(),
            "global query parked while browsing"
        );
        assert_eq!(ui.home_search_parked.as_deref(), Some("al"));
        assert_eq!(ui.home_folder.as_deref(), Some("folder-1"));
        assert_eq!(
            ui.home_rows(&c),
            vec![HomeRow::Notebook(1), HomeRow::Notebook(2)],
            "title-matched folder opens with full membership"
        );

        let l = layout(&g, &ui, &c);
        assert!(l.in_folder);
        assert_eq!(
            ui.on_tap(&g, center(l.exit_hit), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_folder.is_none());
        assert!(ui.home_search_parked.is_none());
        assert_eq!(ui.home_search.as_deref(), Some("al"));
        assert_eq!(ui.home_rows(&c), alpha_both_rows());
    }

    #[test]
    fn in_folder_search_still_filters_membership() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.folders[0].notebook_ids.push("notebook-2".into());
        ui.home_folder = Some("folder-1".into());
        ui.set_home_search(Some("alpha".into()));
        assert_eq!(
            ui.home_rows(&c),
            home_rows(&c, &ui.folders, Some("folder-1"), Some("alpha"))
        );
        assert_eq!(ui.home_rows(&c), vec![HomeRow::Notebook(2)]);
        let l = layout(&g, &ui, &c);
        assert!(l.in_folder);
        assert!(l.clear_search.is_none());
        assert_eq!(l.hit(center(l.search), &ui.home, c.len()), None);
    }

    #[test]
    fn open_card_uses_library_index_and_clears_editor_session() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.set_home_search(Some("Beta".into()));
        let rows = ui.home_rows(&c);
        assert_eq!(rows, vec![HomeRow::Notebook(1)]);
        let l = layout(&g, &ui, &c);
        assert_eq!(l.cards[0].0, 1);
        ui.begin_home_search();
        assert!(ui.search_editor.is_some());
        let l = layout(&g, &ui, &c);
        assert_eq!(
            ui.on_tap(&g, center(l.cards[0].1), &c, "Notebook 4", None),
            UiCommand::OpenNotebook("notebook-2".into())
        );
        assert!(ui.search_editor.is_none());
        assert!(ui.home_search_backup.is_none());
        assert_eq!(ui.home_search.as_deref(), Some("Beta"));
    }

    #[test]
    fn pagination_while_editing_never_duplicates_or_skips() {
        let g = panel_geo();
        let match_count = 8;
        let c: Vec<HomeCard> = (1..=match_count)
            .map(|i| HomeCard {
                notebook_id: format!("notebook-{i}"),
                title: format!("Match {i}"),
                page_count: Some(1),
            })
            .collect();
        let mut ui = UiState::launch_home(0, c.len());
        ui.begin_home_search();
        for ch in ['m', 'a', 't', 'c', 'h'] {
            tap_key(&mut ui, &g, &c, EditorKey::Char(ch));
        }
        assert_eq!(ui.home_rows(&c).len(), match_count);
        let vp = HomeViewport::for_search_editing(&g);
        assert!(
            vp.rows_per_page < ROWS_PER_PAGE,
            "editing viewport should shrink below full Home capacity"
        );
        assert!(match_count > vp.rows_per_page);

        let mut seen = Vec::new();
        ui.home.reveal = false;
        ui.home.page = 0;
        loop {
            ui.settle_home(&g, &c);
            let l = layout(&g, &ui, &c);
            let page_ids: Vec<usize> = l.cards.iter().map(|(i, _, _)| *i).collect();
            assert!(
                page_ids.len() <= vp.rows_per_page,
                "page shows at most viewport capacity"
            );
            seen.extend(page_ids);
            if !ui.home.can_page_next(ui.home_rows(&c).len(), vp) {
                break;
            }
            let l = layout(&g, &ui, &c);
            assert_eq!(
                ui.on_tap(&g, center(l.pager_next), &c, "Notebook N", None),
                UiCommand::Redraw(Redraw::Region(l.content_band))
            );
        }
        assert_eq!(
            seen,
            (0..match_count).collect::<Vec<_>>(),
            "every match reachable exactly once while keyboard is open"
        );
    }

    #[test]
    fn clear_from_later_page_resets_pagination() {
        let match_count = ROWS_PER_PAGE + 3;
        let c: Vec<HomeCard> = (1..=match_count)
            .map(|i| HomeCard {
                notebook_id: format!("notebook-{i}"),
                title: format!("Match {i}"),
                page_count: Some(1),
            })
            .collect();
        let g = panel_geo();
        let mut ui = UiState::launch_home(0, c.len());
        ui.set_home_search(Some("Match".into()));
        ui.home.reveal = false;
        ui.home.page = 1;
        ui.settle_home(&g, &c);
        assert_eq!(ui.home.page, 1);

        let l = layout(&g, &ui, &c);
        let clear = l.clear_search.expect("clear");
        assert_eq!(
            ui.on_tap(&g, center(clear), &c, "Notebook N", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_search.is_none());
        assert_eq!(ui.home.page, 0);
    }

    #[test]
    fn empty_results_until_cleared() {
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.set_home_search(Some("no-such-title".into()));
        assert!(ui.home_rows(&c).is_empty());
        ui.set_home_search(None);
        assert!(!ui.home_rows(&c).is_empty());
    }

    #[test]
    fn clear_while_editing_preserves_backup_for_cancel_and_done() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.set_home_search(Some("al".into()));
        ui.settle_home(&g, &c);

        // Reopen → Clear → Cancel restores the original committed query.
        let l = layout(&g, &ui, &c);
        ui.on_tap(&g, center(l.search), &c, "Notebook 4", None);
        assert_eq!(ui.home_search_backup.as_deref(), Some("al"));
        let clear = layout(&g, &ui, &c)
            .clear_search
            .expect("clear while editing");
        assert!(matches!(
            ui.on_tap(&g, center(clear), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Region(_))
        ));
        assert!(ui.home_search.is_none());
        assert!(editor_of(&ui).text().is_empty());
        assert_eq!(
            ui.home_search_backup.as_deref(),
            Some("al"),
            "Clear must keep the session backup"
        );
        assert!(
            ui.home_rows(&c)
                .iter()
                .any(|r| matches!(r, HomeRow::Folders(_))),
            "cleared draft shows the unfiltered library while still editing"
        );
        assert_eq!(
            tap_key(&mut ui, &g, &c, EditorKey::Cancel),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.search_editor.is_none());
        assert_eq!(ui.home_search.as_deref(), Some("al"));
        assert_eq!(ui.home_rows(&c), alpha_notebook_rows());

        // Reopen → Clear → Done commits the cleared query (unfiltered library).
        let l = layout(&g, &ui, &c);
        ui.on_tap(&g, center(l.search), &c, "Notebook 4", None);
        let clear = layout(&g, &ui, &c).clear_search.expect("clear");
        ui.on_tap(&g, center(clear), &c, "Notebook 4", None);
        assert_eq!(ui.home_search_backup.as_deref(), Some("al"));
        assert_eq!(
            tap_key(&mut ui, &g, &c, EditorKey::Submit),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.search_editor.is_none());
        assert!(ui.home_search.is_none());
        assert!(ui.home_search_backup.is_none());
        assert!(
            ui.home_rows(&c)
                .iter()
                .any(|r| matches!(r, HomeRow::Folders(_))),
            "Done after Clear leaves the ordinary library"
        );
    }

    #[test]
    fn docked_search_geometry_fits_scribe_and_smaller_panels() {
        assert_search_composer_no_overlap(&panel_geo());
        // Supported smaller panels that still fit keyboard + gap + pager + ≥1 card row.
        assert_search_composer_no_overlap(&panel_geo_sized(1440, 1920));
        assert_search_composer_no_overlap(&panel_geo_sized(1236, 1680));
    }

    #[test]
    fn folder_tile_content_uses_normal_layout_when_tall_and_compact_when_short() {
        let g = panel_geo();
        let c = cards();
        let folders = folders_title_alpha();
        let rows = home_rows(&c, &folders, None, None);
        let state = HomeState::selecting(0, c.len());
        let tall =
            HomeLayout::for_rows(&g, &state, &rows, false, false, HomeViewport::standard(&g));
        assert!(!tall.folders.is_empty());
        for (_, tile, menu) in &tall.folders {
            let content = folder_tile_content(*tile, menu.x, "Alphaes");
            assert!(
                !content.compact,
                "full-height Home tiles must keep the normal folder layout (h={})",
                tile.h
            );
            assert_folder_tile_content_safe(*tile, *menu, content);
        }

        // Search-editing viewports shrink tile height; content must stay non-overlapping.
        for g in [
            panel_geo(),
            panel_geo_sized(1440, 1920),
            panel_geo_sized(1236, 1680),
        ] {
            let rows = home_rows(&c, &folders, None, Some("al"));
            let vp = HomeViewport::for_search_editing(&g);
            let geom = search_geom_on_page(
                &g,
                &c,
                &folders,
                "al",
                folder_row_page(&rows, vp.rows_per_page),
            );
            assert!(
                !geom.home.folders.is_empty(),
                "expected a matching folder tile on {}x{}",
                g.xres,
                g.yres
            );
            for (_, tile, menu) in &geom.home.folders {
                let content = folder_tile_content(*tile, menu.x, "Alphaes");
                assert_folder_tile_content_safe(*tile, *menu, content);
                // Short search tiles on these panels need the compact stack.
                assert!(
                    content.compact,
                    "search-editing tile h={} on {}x{} should use compact folder layout",
                    tile.h, g.xres, g.yres
                );
            }
        }
    }

    #[test]
    fn folder_count_label_fits_meta_width_on_search_tiles() {
        let c = cards();
        let folders = folders_title_alpha();
        let state = HomeState::selecting(0, c.len());
        let counts = [1usize, 2, 128];
        for g in [
            panel_geo(),
            panel_geo_sized(1440, 1920),
            panel_geo_sized(1236, 1680),
        ] {
            let rows = home_rows(&c, &folders, None, Some("al"));
            let vp = HomeViewport::for_search_editing(&g);
            let geom = search_geom_on_page(
                &g,
                &c,
                &folders,
                "al",
                folder_row_page(&rows, vp.rows_per_page),
            );
            let (_, tile, menu) = geom
                .home
                .folders
                .first()
                .copied()
                .expect("matching folder tile");
            let content = folder_tile_content(tile, menu.x, "Alphaes");
            for n in counts {
                let (label, scale) = folder_count_label(n, content.meta.w);
                assert!(
                    label.contains(&n.to_string()),
                    "count {n} must appear in {label:?} on {}x{}",
                    g.xres,
                    g.yres
                );
                let w = text_width(&label, scale);
                assert!(
                    w <= content.meta.w,
                    "count label wider than meta: {label:?} scale={scale} w={w} meta.w={} on {}x{}",
                    content.meta.w,
                    g.xres,
                    g.yres
                );
            }
            // Tall Home tiles keep the full wording at the default meta scale when width allows.
            let tall = HomeLayout::for_rows(
                &g,
                &state,
                &home_rows(&c, &folders, None, None),
                false,
                false,
                HomeViewport::standard(&g),
            );
            if let Some((_, tile, menu)) = tall.folders.first() {
                let tall_content = folder_tile_content(*tile, menu.x, "Alphaes");
                if !tall_content.compact {
                    let (one, scale_one) = folder_count_label(1, tall_content.meta.w);
                    let (many, scale_many) = folder_count_label(2, tall_content.meta.w);
                    assert_eq!(one, "1 notebook");
                    assert_eq!(many, "2 notebooks");
                    assert_eq!(scale_one, 5);
                    assert_eq!(scale_many, 5);
                    assert!(text_width(&one, scale_one) <= tall_content.meta.w);
                    assert!(text_width(&many, scale_many) <= tall_content.meta.w);
                }
            }
        }
    }

    #[test]
    fn foldered_notebook_is_openable_from_global_search() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.set_home_search(Some("alpha".into()));
        assert_eq!(ui.home_rows(&c), alpha_notebook_rows());
        ui.begin_home_search();
        let l = layout(&g, &ui, &c);
        let foldered = l
            .cards
            .iter()
            .find(|(i, _, _)| *i == 2)
            .copied()
            .expect("foldered notebook as a direct search card");
        assert_eq!(
            ui.on_tap(&g, center(foldered.1), &c, "Notebook 4", None),
            UiCommand::OpenNotebook("notebook-3".into())
        );
        assert!(ui.search_editor.is_none());
        assert!(ui.home_search_backup.is_none());
        assert_eq!(ui.home_search.as_deref(), Some("alpha"));
    }

    #[test]
    fn foldered_search_card_keeps_pages_and_parent_folder_menu() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.set_home_search(Some("alpha".into()));
        assert_eq!(
            notebook_meta_label(c[2].page_count, Some(&ui.folders[0].title)),
            "3 pages · Projects"
        );
        assert_eq!(notebook_meta_label(c[0].page_count, None), "1 page");
        assert_eq!(folder_match_count(&ui.folders[0], &c, "alpha"), 1);
        assert!(
            !ui.home_rows(&c)
                .iter()
                .any(|r| matches!(r, HomeRow::Folders(_))),
            "parent folder must not appear solely because a child matches"
        );

        let l = layout(&g, &ui, &c);
        let foldered_menu = l
            .cards
            .iter()
            .find(|(i, _, _)| *i == 2)
            .map(|(_, _, menu)| *menu)
            .expect("foldered notebook menu");
        assert_eq!(
            ui.on_tap(&g, center(foldered_menu), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert_eq!(
            ui.modal,
            Some(Modal::CardMenuInFolder {
                folder_id: "folder-1".into(),
                notebook_id: "notebook-3".into(),
                title: "alpha draft".into(),
            })
        );
    }

    #[test]
    fn back_from_opened_search_folder_pages_to_the_folder_tile() {
        let g = panel_geo();
        let mut c: Vec<HomeCard> = (1..=6)
            .map(|i| HomeCard {
                notebook_id: format!("notebook-{i}"),
                title: format!("Note {i}"),
                page_count: Some(i),
            })
            .collect();
        c.push(HomeCard {
            notebook_id: "notebook-7".into(),
            title: "Other".into(),
            page_count: Some(7),
        });
        let folders = vec![FolderInfo {
            id: "folder-1".into(),
            title: "Match Folder".into(),
            notebook_ids: vec!["notebook-7".into(), "notebook-1".into()],
        }];
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders;
        ui.set_home_search(Some("Match".into()));
        let rows = ui.home_rows(&c);
        assert_eq!(rows, vec![HomeRow::Folders(vec![0])]);
        // Pad with notebook hits so the folder tile sits past page 0 after a mixed query.
        ui.set_home_search(Some("e".into()));
        let rows = ui.home_rows(&c);
        let folder_row = rows
            .iter()
            .position(|r| matches!(r, HomeRow::Folders(_)))
            .expect("folder title Match Folder matches 'e'");
        assert!(
            folder_row >= ROWS_PER_PAGE,
            "folder tile must sit past the first page so Back has a viewport to restore"
        );

        // Open via the real hit path so the global query is parked.
        ui.home.page = folder_row / ROWS_PER_PAGE;
        ui.settle_home(&g, &c);
        let l = layout(&g, &ui, &c);
        assert_eq!(l.folders.len(), 1);
        assert_eq!(
            ui.on_tap(&g, center(l.folders[0].1), &c, "Notebook 8", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_search.is_none());
        assert_eq!(ui.home_search_parked.as_deref(), Some("e"));
        assert_eq!(
            ui.home_rows(&c),
            vec![HomeRow::Notebook(0), HomeRow::Notebook(6)],
            "opened folder shows full membership, not only title hits"
        );
        let l = layout(&g, &ui, &c);
        assert!(l.in_folder);
        assert_eq!(
            ui.on_tap(&g, center(l.exit_hit), &c, "Notebook 8", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_folder.is_none());
        assert_eq!(ui.home_search.as_deref(), Some("e"));
        assert_eq!(ui.home_rows(&c), rows);
        assert_eq!(ui.home.page, folder_row / ROWS_PER_PAGE);
    }

    #[test]
    fn pagination_crosses_notebook_then_folder_boundary() {
        let g = panel_geo();
        let mut c: Vec<HomeCard> = (1..=6)
            .map(|i| HomeCard {
                notebook_id: format!("notebook-{i}"),
                title: format!("Match {i}"),
                page_count: Some(1),
            })
            .collect();
        c.push(HomeCard {
            notebook_id: "notebook-7".into(),
            title: "Other".into(),
            page_count: Some(1),
        });
        let folders = vec![FolderInfo {
            id: "folder-1".into(),
            title: "Match Folder".into(),
            notebook_ids: vec!["notebook-7".into()],
        }];
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders;
        ui.set_home_search(Some("Match".into()));
        ui.home.reveal = false;
        ui.home.page = 0;
        ui.settle_home(&g, &c);
        assert_eq!(
            ui.home_rows(&c),
            (0..6)
                .map(HomeRow::Notebook)
                .chain(std::iter::once(HomeRow::Folders(vec![0])))
                .collect::<Vec<_>>(),
            "six notebook hits then the title-matched folder"
        );

        let mut seen_notebooks = Vec::new();
        let mut seen_folders = Vec::new();
        loop {
            let l = layout(&g, &ui, &c);
            seen_notebooks.extend(l.cards.iter().map(|(i, _, _)| *i));
            seen_folders.extend(l.folders.iter().map(|(i, _, _)| *i));
            if l.folders.is_empty() {
                assert!(
                    !l.cards.is_empty(),
                    "pages before the folder block should show notebooks"
                );
            }
            if !ui
                .home
                .can_page_next(ui.home_rows(&c).len(), ui.home_viewport(&g))
            {
                break;
            }
            let l = layout(&g, &ui, &c);
            assert_eq!(
                ui.on_tap(&g, center(l.pager_next), &c, "Notebook N", None),
                UiCommand::Redraw(Redraw::Full)
            );
        }
        assert_eq!(seen_notebooks, (0..6).collect::<Vec<_>>());
        assert_eq!(seen_folders, vec![0]);
        assert!(
            seen_notebooks
                .iter()
                .zip(seen_notebooks.iter().skip(1))
                .all(|(a, b)| a < b),
            "notebook cards keep library order across pages"
        );
    }

    #[test]
    fn personal_example_excludes_journals_parent_folder() {
        let c = vec![
            HomeCard {
                notebook_id: "notebook-1".into(),
                title: "Personal".into(),
                page_count: Some(1),
            },
            HomeCard {
                notebook_id: "notebook-2".into(),
                title: "Work".into(),
                page_count: Some(2),
            },
        ];
        let folders = vec![
            FolderInfo {
                id: "folder-journals".into(),
                title: "Journals".into(),
                notebook_ids: vec!["notebook-1".into()],
            },
            FolderInfo {
                id: "folder-personal-projects".into(),
                title: "Personal Projects".into(),
                notebook_ids: vec![],
            },
        ];
        assert_eq!(
            home_rows(&c, &folders, None, Some("per")),
            vec![HomeRow::Notebook(0), HomeRow::Folders(vec![1]),],
            "Personal journal + Personal Projects folder; Journals parent excluded"
        );
        assert_eq!(
            folders[1].notebook_ids.len(),
            0,
            "empty matching folder keeps a zero membership count"
        );
    }

    #[test]
    fn open_notebook_from_search_folder_then_home_back_restores_query() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders_title_alpha();
        ui.folders[0].notebook_ids.push("notebook-2".into());
        ui.set_home_search(Some("al".into()));
        assert_eq!(ui.home_rows(&c), alpha_both_rows());

        let l = layout(&g, &ui, &c);
        assert_eq!(
            ui.on_tap(&g, center(l.folders[0].1), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert_eq!(ui.home_search_parked.as_deref(), Some("al"));
        assert!(ui.home_search.is_none());

        let l = layout(&g, &ui, &c);
        let member = l
            .cards
            .iter()
            .find(|(i, _, _)| *i == 2)
            .copied()
            .expect("folder member card");
        assert_eq!(
            ui.on_tap(&g, center(member.1), &c, "Notebook 4", None),
            UiCommand::OpenNotebook("notebook-3".into())
        );
        assert!(ui.search_editor.is_none());
        assert_eq!(
            ui.home_search_parked.as_deref(),
            Some("al"),
            "OpenCard must not discard the parked global query"
        );
        assert_eq!(ui.home_folder.as_deref(), Some("folder-1"));

        // Return from the notebook into Home while still inside the folder.
        ui.enter_home(2, c.len());
        assert_eq!(ui.home_folder.as_deref(), Some("folder-1"));
        assert_eq!(
            ui.home_search_parked.as_deref(),
            Some("al"),
            "enter_home must keep parked query while the folder is still open"
        );

        let l = layout(&g, &ui, &c);
        assert_eq!(
            ui.on_tap(&g, center(l.exit_hit), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_folder.is_none());
        assert!(ui.home_search_parked.is_none());
        assert_eq!(ui.home_search.as_deref(), Some("al"));
        assert_eq!(ui.home_rows(&c), alpha_both_rows());
    }

    #[test]
    fn in_folder_filter_leaves_parked_global_query_for_back() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders_title_alpha();
        ui.folders[0].notebook_ids.push("notebook-2".into());
        ui.set_home_search(Some("al".into()));
        let l = layout(&g, &ui, &c);
        ui.on_tap(&g, center(l.folders[0].1), &c, "Notebook 4", None);
        assert_eq!(ui.home_search_parked.as_deref(), Some("al"));
        assert!(ui.home_search.is_none());

        // In-folder filter is independent of the parked global query.
        ui.set_home_search(Some("Beta".into()));
        assert_eq!(ui.home_rows(&c), vec![HomeRow::Notebook(1)]);
        assert_eq!(
            ui.home_search_parked.as_deref(),
            Some("al"),
            "in-folder search must not overwrite the parked global query"
        );

        let l = layout(&g, &ui, &c);
        assert_eq!(
            ui.on_tap(&g, center(l.exit_hit), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert_eq!(
            ui.home_search.as_deref(),
            Some("al"),
            "Back prefers the parked global query over the in-folder filter"
        );
        assert!(ui.home_search_parked.is_none());
        assert_eq!(ui.home_rows(&c), alpha_both_rows());
    }

    #[test]
    fn clear_in_folder_filter_preserves_parked_global_for_back() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders_title_alpha();
        ui.folders[0].notebook_ids.push("notebook-2".into());
        ui.set_home_search(Some("al".into()));
        let l = layout(&g, &ui, &c);
        ui.on_tap(&g, center(l.folders[0].1), &c, "Notebook 4", None);
        assert_eq!(ui.home_search_parked.as_deref(), Some("al"));

        ui.set_home_search(Some("Beta".into()));
        assert_eq!(ui.home_rows(&c), vec![HomeRow::Notebook(1)]);
        ui.clear_home_search();
        assert!(ui.home_search.is_none());
        assert_eq!(
            ui.home_search_parked.as_deref(),
            Some("al"),
            "clearing an in-folder filter must keep the parked global query"
        );
        assert_eq!(
            ui.home_rows(&c),
            vec![HomeRow::Notebook(1), HomeRow::Notebook(2)],
            "cleared in-folder filter restores full folder membership"
        );

        let l = layout(&g, &ui, &c);
        assert_eq!(
            ui.on_tap(&g, center(l.exit_hit), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert_eq!(ui.home_search.as_deref(), Some("al"));
        assert!(ui.home_search_parked.is_none());
        assert_eq!(ui.home_rows(&c), alpha_both_rows());
    }

    #[test]
    fn clear_discards_parked_query_and_missing_folder_restores_safely() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders_title_alpha();
        ui.set_home_search(Some("al".into()));
        let l = layout(&g, &ui, &c);
        ui.on_tap(&g, center(l.folders[0].1), &c, "Notebook 4", None);
        assert_eq!(ui.home_search_parked.as_deref(), Some("al"));

        // Back restores the global query; clearing on the main list discards park state.
        let l = layout(&g, &ui, &c);
        ui.on_tap(&g, center(l.exit_hit), &c, "Notebook 4", None);
        assert_eq!(ui.home_search.as_deref(), Some("al"));
        assert!(ui.home_search_parked.is_none());
        let l = layout(&g, &ui, &c);
        let clear = l.clear_search.expect("clear on main search results");
        assert_eq!(
            ui.on_tap(&g, center(clear), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_search.is_none());
        assert!(ui.home_search_parked.is_none());

        // Missing open folder restores a still-parked query once, then leaves no stale park.
        ui.set_home_search(Some("al".into()));
        let l = layout(&g, &ui, &c);
        ui.on_tap(&g, center(l.folders[0].1), &c, "Notebook 4", None);
        assert_eq!(ui.home_search_parked.as_deref(), Some("al"));
        ui.folders.clear();
        ui.settle_home(&g, &c);
        assert!(ui.home_folder.is_none());
        assert_eq!(ui.home_search.as_deref(), Some("al"));
        assert!(ui.home_search_parked.is_none());
        ui.settle_home(&g, &c);
        assert_eq!(
            ui.home_search.as_deref(),
            Some("al"),
            "settle must not clear a restored live query"
        );
        assert!(ui.home_search_parked.is_none());
    }

    #[test]
    fn cancel_while_editing_does_not_invent_parked_query() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.begin_home_search();
        tap_key(&mut ui, &g, &c, EditorKey::Char('z'));
        assert_eq!(
            tap_key(&mut ui, &g, &c, EditorKey::Cancel),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_search.is_none());
        assert!(ui.home_search_parked.is_none());
        assert!(ui.home_search_backup.is_none());
    }

    #[test]
    fn unfiltered_folder_open_and_back_leaves_search_clear() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        let l = layout(&g, &ui, &c);
        assert!(l.folders.len() >= 1);
        assert_eq!(
            ui.on_tap(&g, center(l.folders[0].1), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert_eq!(ui.home_folder.as_deref(), Some("folder-1"));
        assert!(ui.home_search.is_none());
        assert!(ui.home_search_parked.is_none());
        let l = layout(&g, &ui, &c);
        assert_eq!(
            ui.on_tap(&g, center(l.exit_hit), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_folder.is_none());
        assert!(ui.home_search.is_none());
        assert!(ui.home_search_parked.is_none());
        assert!(
            ui.home_rows(&c)
                .iter()
                .any(|r| matches!(r, HomeRow::Folders(_))),
            "ordinary library folders return after Back"
        );
    }
}
