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

    /// Global `"al"` / `"ALPHA"` projection for the default fixture: notebooks first.
    fn alpha_search_rows() -> Vec<HomeRow> {
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
            assert_folder_tile_content_bounds(*tile, *menu, "Projects");
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
        let folders = folders();
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
            alpha_search_rows(),
            "matching notebooks first, then the folder tile"
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
    fn folder_drill_in_keeps_query_and_back_restores_main_matches() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.begin_home_search();
        for ch in ['a', 'l'] {
            tap_key(&mut ui, &g, &c, EditorKey::Char(ch));
        }
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
        assert_eq!(ui.home_search.as_deref(), Some("al"));
        assert_eq!(ui.home_folder.as_deref(), Some("folder-1"));
        assert_eq!(ui.home_rows(&c), vec![HomeRow::Notebook(2)]);

        let l = layout(&g, &ui, &c);
        assert!(l.in_folder);
        assert_eq!(
            ui.on_tap(&g, center(l.exit_hit), &c, "Notebook 4", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_folder.is_none());
        assert_eq!(ui.home_search.as_deref(), Some("al"));
        assert_eq!(ui.home_rows(&c), alpha_search_rows());
    }

    #[test]
    fn filtered_folder_title_area_is_not_a_search_opener() {
        let g = panel_geo();
        let c = cards();
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders();
        ui.set_home_search(Some("alpha".into()));
        ui.home_folder = Some("folder-1".into());
        assert_eq!(
            ui.home_rows(&c),
            home_rows(&c, &ui.folders, Some("folder-1"), Some("alpha"))
        );
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
        assert_eq!(ui.home_rows(&c), alpha_search_rows());

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
        let folders = folders();
        let rows = home_rows(&c, &folders, None, None);
        let state = HomeState::selecting(0, c.len());
        let tall =
            HomeLayout::for_rows(&g, &state, &rows, false, false, HomeViewport::standard(&g));
        assert!(!tall.folders.is_empty());
        for (_, tile, menu) in &tall.folders {
            let content = folder_tile_content(*tile, menu.x, "Projects");
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
                let content = folder_tile_content(*tile, menu.x, "Projects");
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
        let folders = folders();
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
            let content = folder_tile_content(tile, menu.x, "Projects");
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
                let tall_content = folder_tile_content(*tile, menu.x, "Projects");
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
        assert_eq!(ui.home_rows(&c), alpha_search_rows());
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
    fn back_from_filtered_folder_pages_to_the_folder_tile() {
        let g = panel_geo();
        let mut c: Vec<HomeCard> = (1..=6)
            .map(|i| HomeCard {
                notebook_id: format!("notebook-{i}"),
                title: format!("Match {i}"),
                page_count: Some(i),
            })
            .collect();
        c.push(HomeCard {
            notebook_id: "notebook-7".into(),
            title: "Match foldered".into(),
            page_count: Some(7),
        });
        let folders = vec![FolderInfo {
            id: "folder-1".into(),
            title: "Projects".into(),
            notebook_ids: vec!["notebook-7".into()],
        }];
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders;
        ui.set_home_search(Some("Match".into()));
        let rows = ui.home_rows(&c);
        assert_eq!(
            rows,
            (0..7)
                .map(HomeRow::Notebook)
                .chain(std::iter::once(HomeRow::Folders(vec![0])))
                .collect::<Vec<_>>()
        );
        let folder_row = rows
            .iter()
            .position(|r| matches!(r, HomeRow::Folders(_)))
            .unwrap();
        assert!(
            folder_row >= ROWS_PER_PAGE,
            "folder tile must sit past the first page so Back has a viewport to restore"
        );

        ui.home_folder = Some("folder-1".into());
        ui.home.page = 0;
        let l = layout(&g, &ui, &c);
        assert!(l.in_folder);
        assert_eq!(ui.home_rows(&c), vec![HomeRow::Notebook(6)]);
        assert_eq!(
            ui.on_tap(&g, center(l.exit_hit), &c, "Notebook 8", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.home_folder.is_none());
        assert_eq!(ui.home_search.as_deref(), Some("Match"));
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
            title: "Match foldered".into(),
            page_count: Some(1),
        });
        let folders = vec![FolderInfo {
            id: "folder-1".into(),
            title: "Projects".into(),
            notebook_ids: vec!["notebook-7".into()],
        }];
        let mut ui = UiState::launch_home(0, c.len());
        ui.folders = folders;
        ui.set_home_search(Some("Match".into()));
        ui.home.reveal = false;
        ui.home.page = 0;
        ui.settle_home(&g, &c);

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
        assert_eq!(seen_notebooks, (0..7).collect::<Vec<_>>());
        assert_eq!(seen_folders, vec![0]);
        assert!(
            seen_notebooks
                .iter()
                .zip(seen_notebooks.iter().skip(1))
                .all(|(a, b)| a < b),
            "notebook cards keep library order across pages"
        );
    }
}
