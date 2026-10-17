//! Gate F11 host tests: Home launch/viewport/hit-testing, modals, title keyboard, durable
//! rename, notebook chrome, pen routing isolation, and chrome/background separation.

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use kindle_platform::{ClockDomain, FbGeometry, StylusEventKind, StylusTool};
    use notebook_core::{PagePoint, Point, Stroke, StrokeId};

    use crate::app_ui::{DownRoute, NotebookUiContext, PenRouter, Redraw, UiCommand, UiState};
    use crate::chrome::{
        page_chrome_for, render_notebook_chrome, ChromeHit, ChromeLayout, PageChrome,
        BOTTOM_BAND_H, TOP_BAND_H,
    };
    use crate::home::{
        home_cards, render_home, viewport_page_count, AppView, HomeCard, HomeHit, HomeLayout,
        HomeState, ROWS_PER_PAGE,
    };
    use crate::ink::InkRequest;
    use crate::keyboard::{
        render_title_editor, EditorKey, EditorOutcome, KeyboardLayout, TitleEditor,
    };
    use crate::library::{
        create_notebook_durable, delete_notebook_durable, open_or_create_library,
        read_library_manifest_for_test, rename_notebook_durable, CreateNotebookHooks,
        DeleteNotebookHooks, LibraryManifestWriteHooks, LibraryRepository, RenameNotebookHooks,
        MAX_TITLE_CHARS, NOTEBOOK_INFO_FILE,
    };
    use crate::modal::{dialog_spec, render_dialog, DialogLayout, Modal, TitlePurpose};
    use crate::notebook::{insert_page_after_durable, open_or_create_notebook, InsertPageHooks};
    use crate::orch::{FakeTransport, StrokeController, TransportStep, UndoResult};
    use crate::profile::RenderProfile;
    use crate::store::{load_existing_page, MemoryPageStore};
    use crate::ui_gfx::{panel_geo, paper_background, Rect, UiPoint, WHITE};

    // ---------------------------------------------------------------- fixtures

    fn tmp(label: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scribe-f11-{label}-{n}"));
        let _ = fs::remove_dir_all(&p);
        p
    }

    /// Small storage geometry (fast durable fixtures), as in F10 tests.
    fn store_geo() -> FbGeometry {
        FbGeometry {
            xres: 200,
            yres: 160,
            xres_virtual: 208,
            yres_virtual: 160,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 208,
            smem_len: 208 * 160,
        }
    }

    fn profile() -> RenderProfile {
        RenderProfile::sealed_v2_pressure(0x00).unwrap()
    }

    fn fresh_library(label: &str) -> (PathBuf, LibraryRepository) {
        let root = tmp(label);
        let g = store_geo();
        let opened =
            open_or_create_library(&root, InkRequest::Default, &g, &paper_background(&g)).unwrap();
        (root, opened.repository)
    }

    fn library_with(label: &str, n: usize) -> (PathBuf, LibraryRepository) {
        let (root, mut lib) = fresh_library(label);
        let g = store_geo();
        for _ in 1..n {
            create_notebook_durable(
                &mut lib,
                &profile(),
                &g,
                &paper_background(&g),
                None,
                &CreateNotebookHooks::default(),
            )
            .unwrap();
        }
        (root, lib)
    }

    fn collect_files(base: &Path, current: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(current).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                collect_files(base, &path, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, fs::read(&path).unwrap());
            }
        }
    }

    fn all_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        collect_files(root, root, &mut out);
        out
    }

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

    fn home_ui(total: usize, selected: usize) -> UiState {
        UiState::launch_home(selected, total)
    }

    /// Notebook view with chrome revealed (F11.3: notebooks open fullscreen; these tests
    /// exercise the chrome controls themselves).
    fn notebook_ui() -> UiState {
        let mut ui = UiState::launch_home(0, 1);
        ui.enter_notebook();
        ui.chrome.reveal(0);
        ui
    }

    fn nb_ctx(index: usize, count: usize) -> Option<NotebookUiContext> {
        Some(NotebookUiContext {
            page: PageChrome { index, count },
        })
    }

    fn dialog_button(g: &FbGeometry, modal: &Modal, label: &str) -> UiPoint {
        let spec = dialog_spec(modal).unwrap();
        let layout = DialogLayout::new(g, &spec);
        let (_, r) = layout
            .buttons
            .iter()
            .find(|(b, _)| b.label == label)
            .unwrap_or_else(|| panic!("no button {label}"));
        center(*r)
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

    /// Tap one key through the full UI state machine.
    fn tap_key(ui: &mut UiState, g: &FbGeometry, key: EditorKey) -> UiCommand {
        let p = key_point(g, &editor_of(ui), key);
        ui.on_tap(g, p, &cards(2), "Notebook 3", None)
    }

    // ---------------------------------------------------------------- Home launch/viewport

    #[test]
    fn home_launch_selects_active_notebook_without_mutating_library() {
        let (root, mut lib) = library_with("launch", 8);
        // Activate notebook-7 (index 6 → viewport page 1).
        crate::library::switch_notebook_durable(
            &mut lib,
            "notebook-7",
            &crate::library::SwitchNotebookHooks::default(),
        )
        .unwrap();
        let before = all_files(&root);
        let state = HomeState::for_active(&lib);
        let ui = UiState::launch_home(state.selected_index, lib.manifest().notebook_ids.len());
        assert_eq!(ui.view, AppView::Home);
        assert!(ui.modal.is_none());
        assert_eq!(ui.home.selected_index, 6);
        assert_eq!(ui.home.page, 1);
        assert!(ui.home.visible_range(8).contains(&6));
        assert!(!ui.allows_page_input());

        // Rendering Home and building cards is read-only.
        let g = panel_geo();
        let mut fb = vec![0u8; g.smem_len as usize];
        let c = home_cards(&lib);
        assert_eq!(c.len(), 8);
        assert!(c.iter().all(|card| card.page_count == Some(1)));
        render_home(&mut fb, &g, &c, &ui.home).unwrap();
        assert_eq!(all_files(&root), before);
        assert_eq!(lib.active_notebook_id(), "notebook-7");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn home_cards_use_cached_summaries_without_rereading_meta() {
        let (root, lib) = library_with("cards-cache", 3);
        let meta2 = root
            .join("notebooks")
            .join("notebook-2")
            .join("notebook.meta");
        let before = fs::read(&meta2).unwrap();
        let cards = home_cards(&lib);
        assert_eq!(cards.len(), 3);
        assert!(cards.iter().all(|c| c.page_count == Some(1)));
        for card in &cards {
            let s = lib.home_summaries().get(&card.notebook_id).unwrap();
            assert_eq!(card.title, s.title);
            assert_eq!(card.page_count, s.page_count);
        }
        fs::write(&meta2, b"{corrupt").unwrap();
        let cards2 = home_cards(&lib);
        assert_eq!(
            cards2
                .iter()
                .find(|c| c.notebook_id == "notebook-2")
                .unwrap()
                .page_count,
            Some(1),
            "Home cards must not reread notebook.meta"
        );
        fs::write(&meta2, before).unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn home_viewport_paging_for_1_6_7_20_notebooks() {
        for (total, pages) in [(1usize, 1usize), (6, 1), (7, 2), (20, 4)] {
            assert_eq!(viewport_page_count(total), pages, "total={total}");
            let mut seen = Vec::new();
            let mut state = HomeState::selecting(0, total);
            loop {
                let range = state.visible_range(total);
                assert!(!range.is_empty(), "empty viewport page total={total}");
                assert!(range.len() <= ROWS_PER_PAGE);
                seen.extend(range);
                if !state.can_page_next(total) {
                    break;
                }
                state.page += 1;
            }
            assert_eq!(
                seen,
                (0..total).collect::<Vec<_>>(),
                "every notebook reachable"
            );
            // Out-of-range viewport (e.g. after deletions) normalizes to a valid, nonempty page.
            let mut stale = HomeState {
                selected_index: total + 5,
                page: 99,
                sync: false,
                reveal: false,
            };
            stale.normalize(total);
            assert_eq!(stale.page, pages - 1);
            assert_eq!(stale.selected_index, total - 1);
            assert!(!stale.visible_range(total).is_empty());
        }
    }

    #[test]
    fn selected_notebook_is_always_in_the_visible_viewport() {
        for total in 1..=20 {
            for idx in 0..total {
                let s = HomeState::selecting(idx, total);
                assert!(s.visible_range(total).contains(&idx), "{total}/{idx}");
                let mut ui = home_ui(total, 0);
                ui.enter_home(idx, total);
                assert!(ui.home.visible_range(total).contains(&idx));
            }
            // Developer cursor wraps across viewport pages and stays visible.
            let mut s = HomeState::selecting(total - 1, total);
            s.select_next(total);
            assert_eq!(s.selected_index, 0);
            assert!(s.visible_range(total).contains(&0));
            s.select_previous(total);
            assert!(s.visible_range(total).contains(&(total - 1)));
        }
    }

    #[test]
    fn home_render_is_full_clear_and_shows_only_the_viewport_page() {
        let g = panel_geo();
        let mut fb = vec![0x55; g.smem_len as usize];
        let state = HomeState::selecting(13, 20);
        let summary = render_home(&mut fb, &g, &cards(20), &state).unwrap();
        assert_eq!(summary.visible_entries, 6);
        assert_eq!(summary.total_entries, 20);
        assert_eq!(summary.viewport_page, 2);
        assert_eq!(summary.viewport_pages, 4);
        for y in 0..g.yres as usize {
            let row = &fb[y * 1872..y * 1872 + 1860];
            assert!(!row.contains(&0x55), "stale pixel survived at row {y}");
        }
    }

    // ---------------------------------------------------------------- Home hit-testing

    #[test]
    fn home_hit_testing_new_cards_menus_and_pager() {
        let g = panel_geo();
        let total = 14;
        let state = HomeState::selecting(7, total); // page 1 → indices 6..12
        let layout = HomeLayout::new(&g, &state, total);
        assert_eq!(layout.cards.len(), 6);
        assert_eq!(
            layout.hit(center(layout.new_button), &state, total),
            Some(HomeHit::New)
        );
        for (index, card, menu) in &layout.cards {
            let body = UiPoint {
                x: card.x + 100,
                y: card.y + card.h / 2,
            };
            assert_eq!(
                layout.hit(body, &state, total),
                Some(HomeHit::OpenCard(*index))
            );
            assert_eq!(
                layout.hit(center(*menu), &state, total),
                Some(HomeHit::CardMenu(*index))
            );
            // Large pen targets.
            assert!(card.h >= 250 && menu.w >= 200);
        }
        assert_eq!(layout.cards[0].0, 6);
        assert_eq!(
            layout.hit(center(layout.pager_previous), &state, total),
            Some(HomeHit::PagePrevious)
        );
        assert_eq!(
            layout.hit(center(layout.pager_next), &state, total),
            Some(HomeHit::PageNext)
        );
        // Header whitespace is inert.
        assert_eq!(layout.hit(UiPoint { x: 900, y: 20 }, &state, total), None);

        // Pager arrows are inert at the ends.
        let first = HomeState::selecting(0, total);
        let l0 = HomeLayout::new(&g, &first, total);
        assert_eq!(l0.hit(center(l0.pager_previous), &first, total), None);
        let last = HomeState::selecting(13, total);
        let l2 = HomeLayout::new(&g, &last, total);
        assert_eq!(l2.hit(center(l2.pager_next), &last, total), None);
        assert_eq!(l2.cards.len(), 2);
    }

    #[test]
    fn home_taps_resolve_to_open_new_menu_and_paging_commands() {
        let g = panel_geo();
        let c = cards(9);
        let mut ui = home_ui(9, 0);
        let layout = HomeLayout::new(&g, &ui.home, 9);

        let cmd = ui.on_tap(&g, center(layout.cards[2].1), &c, "Notebook 10", None);
        assert_eq!(cmd, UiCommand::OpenNotebook("notebook-3".into()));
        assert_eq!(ui.home.selected_index, 2);

        // + NEW asks which: notebook or folder.
        let cmd = ui.on_tap(&g, center(layout.new_button), &c, "Notebook 10", None);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        let chooser = ui.modal.clone().unwrap();
        assert!(matches!(chooser, Modal::NewChooser { .. }), "{chooser:?}");
        let cmd = ui.on_tap(
            &g,
            dialog_button(&g, &chooser, "NEW NOTEBOOK"),
            &c,
            "Notebook 10",
            None,
        );
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        match &ui.modal {
            Some(Modal::TitleEntry {
                purpose: TitlePurpose::Create,
                editor,
            }) => assert_eq!(editor.text(), "Notebook 10"),
            other => panic!("{other:?}"),
        }
        // Cancel closes without any durable command.
        let cmd = tap_key(&mut ui, &g, EditorKey::Cancel);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert!(ui.modal.is_none());

        let cmd = ui.on_tap(&g, center(layout.cards[1].2), &c, "Notebook 10", None);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert_eq!(
            ui.modal,
            Some(Modal::CardMenu {
                notebook_id: "notebook-2".into(),
                title: "Notebook 2".into()
            })
        );
        let modal = ui.modal.clone().unwrap();
        let cmd = ui.on_tap(&g, dialog_button(&g, &modal, "CANCEL"), &c, "x", None);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert!(ui.modal.is_none());

        let cmd = ui.on_tap(&g, center(layout.pager_next), &c, "x", None);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert_eq!(ui.home.page, 1);
        let l1 = HomeLayout::new(&g, &ui.home, 9);
        let cmd = ui.on_tap(&g, center(l1.cards[0].1), &c, "x", None);
        assert_eq!(cmd, UiCommand::OpenNotebook("notebook-7".into()));
    }

    #[test]
    fn opening_the_active_notebook_needs_no_manifest_write() {
        let (root, mut lib) = library_with("open-active", 2);
        let meta = root.join("library.meta");
        let before = fs::read(&meta).unwrap();
        let active = lib.active_notebook_id().to_string();
        let commit = crate::library::switch_notebook_durable(
            &mut lib,
            &active,
            &crate::library::SwitchNotebookHooks::default(),
        )
        .unwrap();
        assert_eq!(commit.manifest_persist_us, 0);
        assert_eq!(fs::read(&meta).unwrap(), before);
        let _ = fs::remove_dir_all(root);
    }

    // ---------------------------------------------------------------- title keyboard

    #[test]
    fn title_editor_typing_shift_symbols_space_backspace() {
        let mut e = TitleEditor::new("");
        assert_eq!(e.apply(EditorKey::Shift), EditorOutcome::KeysChanged);
        assert!(e.shift());
        // One-shot Shift uppercases one letter then releases.
        assert_eq!(e.apply(EditorKey::Char('w')), EditorOutcome::KeysChanged);
        assert!(!e.shift());
        for c in "ork".chars() {
            assert_eq!(e.apply(EditorKey::Char(c)), EditorOutcome::TextChanged);
        }
        assert_eq!(e.text(), "Work");
        assert_eq!(e.apply(EditorKey::Space), EditorOutcome::TextChanged);
        assert_eq!(e.apply(EditorKey::Mode), EditorOutcome::KeysChanged);
        assert!(e.symbols());
        // Shift is inert in symbols mode; symbols are never case-mapped.
        assert_eq!(e.apply(EditorKey::Shift), EditorOutcome::Unchanged);
        for c in "2-_.'&".chars() {
            e.apply(EditorKey::Char(c));
        }
        assert_eq!(e.text(), "Work 2-_.'&");
        assert_eq!(e.apply(EditorKey::Backspace), EditorOutcome::TextChanged);
        assert_eq!(e.text(), "Work 2-_.'");
        assert_eq!(e.apply(EditorKey::Mode), EditorOutcome::KeysChanged);
        assert!(!e.symbols());
        let mut empty = TitleEditor::new("");
        assert_eq!(empty.apply(EditorKey::Backspace), EditorOutcome::Unchanged);
    }

    #[test]
    fn title_editor_limits_refusals_cancel_and_save() {
        let long = "x".repeat(MAX_TITLE_CHARS + 20);
        let mut e = TitleEditor::new(&long);
        assert_eq!(e.text().chars().count(), MAX_TITLE_CHARS);
        assert_eq!(e.apply(EditorKey::Char('y')), EditorOutcome::Unchanged);
        assert_eq!(e.apply(EditorKey::Space), EditorOutcome::Unchanged);
        assert!(
            matches!(e.apply(EditorKey::Submit), EditorOutcome::Submit(t) if t.len() == MAX_TITLE_CHARS)
        );

        assert_eq!(TitleEditor::new("a\u{7}b\nc").text(), "abc");
        assert!(matches!(
            TitleEditor::new("").apply(EditorKey::Submit),
            EditorOutcome::Invalid(_)
        ));
        assert!(matches!(
            TitleEditor::new("    ").apply(EditorKey::Submit),
            EditorOutcome::Invalid(_)
        ));
        assert_eq!(
            TitleEditor::new("  Ideas  ").apply(EditorKey::Submit),
            EditorOutcome::Submit("Ideas".into())
        );
        assert_eq!(
            TitleEditor::new("Ideas").apply(EditorKey::Cancel),
            EditorOutcome::Cancel
        );
        // Control characters can never be typed.
        let mut e = TitleEditor::new("a");
        assert_eq!(e.apply(EditorKey::Char('\n')), EditorOutcome::Unchanged);
    }

    #[test]
    fn keyboard_keys_are_large_distinct_and_hit_test_to_themselves() {
        let g = panel_geo();
        for symbols in [false, true] {
            let layout = KeyboardLayout::new(&g, symbols);
            for (i, (key, r)) in layout.keys.iter().enumerate() {
                assert!(r.w >= 150 && r.h >= 150, "{key:?} too small");
                assert!(r.right() <= g.xres && r.bottom() <= g.yres);
                assert!(!r.intersects(&layout.field));
                assert_eq!(layout.hit(center(*r)), Some(*key));
                for (other, r2) in layout.keys.iter().skip(i + 1) {
                    assert!(!r.intersects(r2), "{key:?} overlaps {other:?}");
                }
            }
            let chars: String = layout
                .keys
                .iter()
                .filter_map(|(k, _)| match k {
                    EditorKey::Char(c) => Some(*c),
                    _ => None,
                })
                .collect();
            if symbols {
                for c in "0123456789-_.'&".chars() {
                    assert!(chars.contains(c), "symbol {c}");
                }
            } else {
                assert_eq!(chars, "qwertyuiopasdfghjklzxcvbnm");
            }
            for k in [
                EditorKey::Shift,
                EditorKey::Mode,
                EditorKey::Space,
                EditorKey::Backspace,
                EditorKey::Cancel,
                EditorKey::Submit,
            ] {
                assert!(layout.keys.iter().any(|(x, _)| *x == k), "{k:?}");
            }
        }
    }

    #[test]
    fn keyboard_flow_through_ui_state_creates_with_typed_title() {
        let g = panel_geo();
        let mut ui = home_ui(2, 0);
        ui.show_modal(Modal::TitleEntry {
            purpose: TitlePurpose::Create,
            editor: TitleEditor::new("Notebook 3"),
        });
        // Clear the proposed title.
        for _ in 0.."Notebook 3".len() {
            let cmd = tap_key(&mut ui, &g, EditorKey::Backspace);
            assert!(matches!(cmd, UiCommand::Redraw(Redraw::Region(_))));
        }
        assert_eq!(editor_of(&ui).text(), "");
        // Empty save → error dialog that resumes the editor.
        let cmd = tap_key(&mut ui, &g, EditorKey::Submit);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        let err = ui.modal.clone().unwrap();
        assert!(matches!(
            err,
            Modal::Error {
                resume: Some(_),
                ..
            }
        ));
        let cmd = ui.on_tap(&g, dialog_button(&g, &err, "OK"), &cards(2), "x", None);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert!(ui.modal.as_ref().unwrap().is_title_entry());

        tap_key(&mut ui, &g, EditorKey::Shift);
        for c in "work".chars() {
            tap_key(&mut ui, &g, EditorKey::Char(c));
        }
        assert_eq!(editor_of(&ui).text(), "Work");
        // Taps between keys are swallowed and keep the editor open.
        let cmd = ui.on_tap(&g, UiPoint { x: 5, y: 5 }, &cards(2), "x", None);
        assert_eq!(cmd, UiCommand::None);
        let cmd = tap_key(&mut ui, &g, EditorKey::Submit);
        assert_eq!(cmd, UiCommand::CreateNotebook("Work".into()));
        assert!(ui.modal.is_none());
    }

    #[test]
    fn rename_flow_uses_card_menu_and_skips_unchanged_title() {
        let g = panel_geo();
        let c = cards(2);
        let mut ui = home_ui(2, 0);
        ui.show_modal(Modal::CardMenu {
            notebook_id: "notebook-2".into(),
            title: "Personal".into(),
        });
        let m = ui.modal.clone().unwrap();
        ui.on_tap(&g, dialog_button(&g, &m, "RENAME"), &c, "x", None);
        assert_eq!(editor_of(&ui).text(), "Personal");
        // Save unchanged → no durable command.
        let cmd = tap_key(&mut ui, &g, EditorKey::Submit);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert!(ui.modal.is_none());

        ui.show_modal(m.clone());
        ui.on_tap(&g, dialog_button(&g, &m, "RENAME"), &c, "x", None);
        for _ in 0..8 {
            tap_key(&mut ui, &g, EditorKey::Backspace);
        }
        tap_key(&mut ui, &g, EditorKey::Shift);
        for ch in "ideas".chars() {
            tap_key(&mut ui, &g, EditorKey::Char(ch));
        }
        let cmd = tap_key(&mut ui, &g, EditorKey::Submit);
        assert_eq!(
            cmd,
            UiCommand::RenameNotebook {
                notebook_id: "notebook-2".into(),
                title: "Ideas".into()
            }
        );
    }

    // ---------------------------------------------------------------- durable rename

    #[test]
    fn rename_success_touches_only_notebook_info() {
        let (root, mut lib) = library_with("rename-ok", 2);
        let before = all_files(&root);
        let commit = rename_notebook_durable(
            &mut lib,
            "notebook-2",
            "Ideas",
            &RenameNotebookHooks::default(),
        )
        .unwrap();
        assert!(commit.changed);
        assert_eq!(commit.old_title, "Notebook 2");
        assert_eq!(lib.title("notebook-2").unwrap(), "Ideas");
        assert_eq!(
            lib.manifest().notebook_ids,
            vec!["notebook-1", "notebook-2"]
        );
        let after = all_files(&root);
        let info_key = format!("notebooks/notebook-2/{NOTEBOOK_INFO_FILE}");
        for (k, v) in &before {
            if *k != info_key {
                assert_eq!(after.get(k), Some(v), "{k} changed");
            }
        }
        assert_eq!(before.len(), after.len(), "no files added/removed");
        assert_ne!(after[&info_key], before[&info_key]);
        assert!(root.join("notebooks/notebook-2").is_dir());

        // Durable across reopen.
        let g = store_geo();
        let reopened =
            open_or_create_library(&root, InkRequest::Default, &g, &paper_background(&g)).unwrap();
        assert_eq!(reopened.repository.title("notebook-2").unwrap(), "Ideas");

        // Same title → no write.
        let snapshot = all_files(&root);
        let same =
            rename_notebook_durable(&mut lib, "notebook-2", "Ideas", &Default::default()).unwrap();
        assert!(!same.changed);
        assert_eq!(all_files(&root), snapshot);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rename_write_fsync_rename_failures_keep_old_title_on_disk_and_in_ram() {
        for (label, hooks) in [
            (
                "write",
                LibraryManifestWriteHooks {
                    fail_write: true,
                    ..Default::default()
                },
            ),
            (
                "fsync",
                LibraryManifestWriteHooks {
                    fail_fsync: true,
                    ..Default::default()
                },
            ),
            (
                "rename",
                LibraryManifestWriteHooks {
                    fail_rename: true,
                    ..Default::default()
                },
            ),
        ] {
            let (root, mut lib) = library_with(&format!("rename-{label}"), 2);
            let before = all_files(&root);
            let err = rename_notebook_durable(
                &mut lib,
                "notebook-2",
                "Ideas",
                &RenameNotebookHooks { info: hooks },
            )
            .unwrap_err();
            assert!(err.starts_with("REFUSE NOTEBOOK RENAME"), "{err}");
            assert_eq!(lib.title("notebook-2").unwrap(), "Notebook 2", "{label}");
            assert_eq!(all_files(&root), before, "{label}: bytes changed");
            assert!(!root
                .join("notebooks/notebook-2")
                .join(format!("{NOTEBOOK_INFO_FILE}.tmp"))
                .exists());
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn rename_refuses_invalid_titles_and_unknown_ids_without_writes() {
        let (root, mut lib) = library_with("rename-invalid", 1);
        let before = all_files(&root);
        for bad in ["", "   ", "a\nb"] {
            assert!(
                rename_notebook_durable(&mut lib, "notebook-1", bad, &Default::default()).is_err()
            );
        }
        let too_long = "x".repeat(MAX_TITLE_CHARS + 1);
        assert!(
            rename_notebook_durable(&mut lib, "notebook-1", &too_long, &Default::default())
                .is_err()
        );
        assert!(
            rename_notebook_durable(&mut lib, "notebook-9", "Ok", &Default::default()).is_err()
        );
        assert_eq!(all_files(&root), before);
        assert_eq!(lib.title("notebook-1").unwrap(), "Notebook 1");
        let _ = fs::remove_dir_all(root);
    }

    // ---------------------------------------------------------------- notebook delete UI

    #[test]
    fn notebook_delete_requires_confirmation_and_refuses_sole_notebook() {
        let g = panel_geo();
        let menu = Modal::CardMenu {
            notebook_id: "notebook-1".into(),
            title: "Work".into(),
        };
        // First Delete tap never deletes: it asks.
        let mut ui = home_ui(2, 0);
        ui.show_modal(menu.clone());
        let cmd = ui.on_tap(&g, dialog_button(&g, &menu, "DELETE"), &cards(2), "x", None);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        let confirm = ui.modal.clone().unwrap();
        let spec = dialog_spec(&confirm).unwrap();
        assert_eq!(spec.heading, "Delete \"Work\"?");
        assert!(spec.body[0].contains("removed from the library"));
        let cmd = ui.on_tap(
            &g,
            dialog_button(&g, &confirm, "CANCEL"),
            &cards(2),
            "x",
            None,
        );
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert!(ui.modal.is_none());

        ui.show_modal(confirm.clone());
        let cmd = ui.on_tap(
            &g,
            dialog_button(&g, &confirm, "DELETE"),
            &cards(2),
            "x",
            None,
        );
        assert_eq!(cmd, UiCommand::DeleteNotebook("notebook-1".into()));

        // Sole notebook: explanatory notice, no delete command.
        let mut ui = home_ui(1, 0);
        ui.show_modal(menu.clone());
        let cmd = ui.on_tap(&g, dialog_button(&g, &menu, "DELETE"), &cards(1), "x", None);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        let notice = ui.modal.clone().unwrap();
        let spec = dialog_spec(&notice).unwrap();
        assert_eq!(spec.body[0], "You can't delete the only notebook.");
        assert_eq!(spec.body[1], "Create another notebook first.");
        let cmd = ui.on_tap(&g, dialog_button(&g, &notice, "OK"), &cards(1), "x", None);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert!(ui.modal.is_none());
    }

    #[test]
    fn notebook_delete_leaves_home_selection_and_viewport_coherent() {
        let (root, mut lib) = library_with("delete-home", 7);
        // Active = notebook-7 (index 6, viewport page 1). Delete it.
        assert_eq!(lib.active_notebook_id(), "notebook-7");
        delete_notebook_durable(&mut lib, "notebook-7", &DeleteNotebookHooks::default()).unwrap();
        let total = lib.manifest().notebook_ids.len();
        let mut ui = home_ui(7, 6);
        ui.enter_home(HomeState::for_active(&lib).selected_index, total);
        // F10 rule: deleting the last entry activates the preceding notebook.
        assert_eq!(lib.active_notebook_id(), "notebook-6");
        assert_eq!(ui.home.selected_index, 5);
        assert_eq!(ui.home.page, 0);
        assert!(!ui.home.visible_range(total).is_empty());
        assert!(root.join(".notebook-trash").is_dir());
        let _ = fs::remove_dir_all(root);
    }

    // ---------------------------------------------------------------- notebook chrome

    #[test]
    fn chrome_hit_testing_and_reserved_bands() {
        let g = panel_geo();
        let c = ChromeLayout::new(&g);
        assert_eq!(c.hit(center(c.home)), Some(ChromeHit::Home));
        assert_eq!(c.hit(center(c.undo)), Some(ChromeHit::Undo));
        assert_eq!(c.hit(center(c.menu)), Some(ChromeHit::PageMenu));
        assert_eq!(c.hit(center(c.previous)), Some(ChromeHit::PreviousPage));
        assert_eq!(c.hit(center(c.next)), Some(ChromeHit::NextPage));
        assert_eq!(c.hit(center(c.title)), Some(ChromeHit::Inert));
        assert_eq!(c.hit(UiPoint { x: 5, y: 2470 }), Some(ChromeHit::Inert));
        assert_eq!(
            c.hit(UiPoint {
                x: 900,
                y: TOP_BAND_H
            }),
            None
        );
        assert_eq!(
            c.hit(UiPoint {
                x: 900,
                y: 2480 - BOTTOM_BAND_H - 1
            }),
            None
        );
        assert_eq!(c.hit(UiPoint { x: 900, y: 1200 }), None);
        for r in [c.home, c.undo, c.menu, c.previous, c.next] {
            assert!(r.w >= 200 && r.h >= 80, "{r:?}");
        }
    }

    #[test]
    fn chrome_taps_map_to_home_undo_navigation_and_page_menu() {
        let g = panel_geo();
        let c = ChromeLayout::new(&g);
        let mut ui = notebook_ui();
        let none = cards(1);
        assert_eq!(
            ui.on_tap(&g, center(c.home), &none, "x", nb_ctx(2, 3)),
            UiCommand::GoHome
        );
        assert_eq!(
            ui.on_tap(&g, center(c.undo), &none, "x", nb_ctx(2, 3)),
            UiCommand::Undo
        );
        assert_eq!(
            ui.on_tap(&g, center(c.previous), &none, "x", nb_ctx(2, 3)),
            UiCommand::PreviousPage
        );
        // First page: Previous is a no-op.
        assert_eq!(
            ui.on_tap(&g, center(c.previous), &none, "x", nb_ctx(1, 3)),
            UiCommand::None
        );
        assert_eq!(
            ui.on_tap(&g, center(c.next), &none, "x", nb_ctx(3, 3)),
            UiCommand::NextPage
        );
        assert_eq!(
            ui.on_tap(&g, center(c.title), &none, "x", nb_ctx(3, 3)),
            UiCommand::None
        );
        assert!(ui.modal.is_none());
        assert_eq!(
            ui.on_tap(&g, center(c.menu), &none, "x", nb_ctx(2, 3)),
            UiCommand::Redraw(Redraw::Full)
        );
        assert_eq!(
            ui.modal,
            Some(Modal::PageMenu {
                page_number: 2,
                page_count: 3
            })
        );
        assert!(!ui.allows_page_input());
    }

    #[test]
    fn page_menu_actions_boundaries_and_delete_confirmation() {
        let g = panel_geo();
        let none = cards(1);
        let tap = |ui: &mut UiState, label: &str| {
            let m = ui.modal.clone().unwrap();
            ui.on_tap(&g, dialog_button(&g, &m, label), &none, "x", nb_ctx(1, 1))
        };
        let menu = |n, c| Modal::PageMenu {
            page_number: n,
            page_count: c,
        };
        let mut ui = notebook_ui();

        ui.show_modal(menu(2, 3));
        assert_eq!(
            tap(&mut ui, "INSERT PAGE AFTER"),
            UiCommand::InsertPageAfter
        );
        assert!(ui.modal.is_none());
        ui.show_modal(menu(2, 3));
        assert_eq!(tap(&mut ui, "MOVE PAGE LEFT"), UiCommand::MovePageLeft);
        ui.show_modal(menu(2, 3));
        assert_eq!(tap(&mut ui, "MOVE PAGE RIGHT"), UiCommand::MovePageRight);

        // Boundaries: disabled buttons are visible but inert; the menu stays open.
        ui.show_modal(menu(1, 3));
        let spec = dialog_spec(ui.modal.as_ref().unwrap()).unwrap();
        assert!(
            !spec
                .buttons
                .iter()
                .find(|b| b.label == "MOVE PAGE LEFT")
                .unwrap()
                .enabled
        );
        assert_eq!(tap(&mut ui, "MOVE PAGE LEFT"), UiCommand::None);
        assert_eq!(ui.modal, Some(menu(1, 3)));
        ui.show_modal(menu(3, 3));
        assert_eq!(tap(&mut ui, "MOVE PAGE RIGHT"), UiCommand::None);
        assert_eq!(ui.modal, Some(menu(3, 3)));

        // Delete asks first.
        ui.show_modal(menu(3, 3));
        assert_eq!(tap(&mut ui, "DELETE PAGE"), UiCommand::Redraw(Redraw::Full));
        assert_eq!(ui.modal, Some(Modal::ConfirmDeletePage { page_number: 3 }));
        assert_eq!(
            dialog_spec(ui.modal.as_ref().unwrap()).unwrap().heading,
            "Delete page 3?"
        );
        assert_eq!(tap(&mut ui, "CANCEL"), UiCommand::Redraw(Redraw::Full));
        assert!(ui.modal.is_none());
        ui.show_modal(Modal::ConfirmDeletePage { page_number: 3 });
        assert_eq!(tap(&mut ui, "DELETE"), UiCommand::DeletePage);

        // Sole page: refusal notice, no delete.
        ui.show_modal(menu(1, 1));
        assert_eq!(tap(&mut ui, "DELETE PAGE"), UiCommand::Redraw(Redraw::Full));
        assert_eq!(ui.modal, Some(Modal::sole_page_notice()));
        ui.show_modal(menu(2, 3));
        assert_eq!(tap(&mut ui, "CANCEL"), UiCommand::Redraw(Redraw::Full));
        assert!(ui.modal.is_none());
    }

    #[test]
    fn undo_button_uses_sealed_visible_lineage_undo() {
        let mut c = StrokeController::with_store(
            "page-1",
            FakeTransport::new(),
            8_000,
            ClockDomain::LinuxMonotonic,
            MemoryPageStore::new(),
        );
        c.session
            .load_committed_strokes(vec![
                Stroke {
                    id: StrokeId::new("s1"),
                    points: vec![Point::new(1000.0, 3000.0), Point::new(2000.0, 3000.0)],
                    brush: None,
                    finished: true,
                },
                Stroke {
                    id: StrokeId::new("s2"),
                    points: vec![Point::new(1000.0, 4000.0), Point::new(2000.0, 4000.0)],
                    brush: None,
                    finished: true,
                },
            ])
            .unwrap();
        let g = panel_geo();
        let mut ui = notebook_ui();
        let cmd = ui.on_tap(
            &g,
            center(ChromeLayout::new(&g).undo),
            &cards(1),
            "x",
            nb_ctx(1, 1),
        );
        assert_eq!(cmd, UiCommand::Undo);
        // The runtime maps UiCommand::Undo to the same controller call as the F4 gesture.
        match c.undo_last_visible_stroke().unwrap() {
            UndoResult::Deleted { stroke_id, .. } => assert_eq!(stroke_id.as_str(), "s2"),
            other => panic!("{other:?}"),
        }
        assert_eq!(c.session.committed_count(), 1);
    }

    // ---------------------------------------------------------------- input isolation

    enum Ev {
        Down(f32, f32, StylusTool),
        Move(f32, f32),
        Up(f32, f32),
    }

    /// Mirrors the runtime pre-dispatch: route every event through `PenRouter::route_event`
    /// and hand only `Page` events to the sealed controller. Returns UI tap count.
    fn drive(
        ui: &mut UiState,
        router: &mut PenRouter,
        c: &mut StrokeController<FakeTransport>,
        events: &[Ev],
    ) -> usize {
        let g = panel_geo();
        let mut taps = 0;
        for ev in events {
            let (kind, x, y, tool) = match *ev {
                Ev::Down(x, y, t) => (StylusEventKind::Down, x, y, t),
                Ev::Move(x, y) => (StylusEventKind::Move, x, y, StylusTool::Pen),
                Ev::Up(x, y) => (StylusEventKind::Up, x, y, StylusTool::Pen),
            };
            // Panel pixel centers → canonical page units.
            let page = PagePoint::new((x + 0.5) * 8.4, (y + 0.5) * 8.4);
            let p = UiPoint::from_page(page);
            match router.route_event(ui, &g, kind, tool, p, !c.is_idle()) {
                DownRoute::UiTap(_) => taps += 1,
                DownRoute::Consume => {}
                DownRoute::Page => match kind {
                    StylusEventKind::Down if tool == StylusTool::Pen => {
                        c.on_pen_down(page, Some(0.5)).unwrap();
                    }
                    StylusEventKind::Move => c.on_pen_move(page, Some(0.5), None).unwrap(),
                    StylusEventKind::Up => {
                        let _ = c.on_pen_up(page, Some(0.5));
                    }
                    _ => {}
                },
            }
        }
        taps
    }

    fn fresh_ctrl() -> StrokeController<FakeTransport> {
        StrokeController::new("page-1", FakeTransport::new(), 8_000)
    }

    fn scribble(y0: f32) -> Vec<Ev> {
        vec![
            Ev::Down(400.0, y0, StylusTool::Pen),
            Ev::Move(600.0, y0 + 40.0),
            Ev::Move(900.0, y0 + 300.0),
            Ev::Up(1000.0, y0 + 400.0),
        ]
    }

    #[test]
    fn chrome_tap_never_creates_stroke_but_content_does() {
        let mut ui = notebook_ui();
        let mut router = PenRouter::default();
        let mut c = fresh_ctrl();
        // Contact starts in the top band and drags into page content: still UI-owned.
        let taps = drive(&mut ui, &mut router, &mut c, &scribble(40.0));
        assert_eq!(taps, 1);
        // Bottom band.
        let taps = drive(&mut ui, &mut router, &mut c, &scribble(2400.0));
        assert_eq!(taps, 1);
        assert_eq!(c.session.committed_count(), 0);
        assert_eq!(c.session.strokes_started(), 0);
        assert!(!c.transport().steps.contains(&TransportStep::Stamp));
        assert!(!c.transport().steps.contains(&TransportStep::Arm));

        // Control: the same scribble in page content inks normally.
        let taps = drive(&mut ui, &mut router, &mut c, &scribble(1000.0));
        assert_eq!(taps, 0);
        assert_eq!(c.session.committed_count(), 1);
    }

    #[test]
    fn home_and_modal_taps_never_create_strokes() {
        let mut router = PenRouter::default();
        let mut c = fresh_ctrl();

        let mut home = home_ui(3, 0);
        assert_eq!(drive(&mut home, &mut router, &mut c, &scribble(1000.0)), 1);
        // Eraser on Home is consumed without action.
        let eraser = [
            Ev::Down(400.0, 1000.0, StylusTool::Eraser),
            Ev::Move(500.0, 1000.0),
            Ev::Up(500.0, 1000.0),
        ];
        assert_eq!(drive(&mut home, &mut router, &mut c, &eraser), 0);

        let mut ui = notebook_ui();
        ui.show_modal(Modal::PageMenu {
            page_number: 1,
            page_count: 1,
        });
        assert_eq!(drive(&mut ui, &mut router, &mut c, &scribble(1000.0)), 1);
        ui.show_modal(Modal::TitleEntry {
            purpose: TitlePurpose::Create,
            editor: TitleEditor::new("x"),
        });
        assert_eq!(drive(&mut ui, &mut router, &mut c, &scribble(1500.0)), 1);
        assert_eq!(c.session.committed_count(), 0);
        assert_eq!(c.session.strokes_started(), 0);
        assert!(!c.transport().steps.contains(&TransportStep::Stamp));
    }

    #[test]
    fn contact_stays_ui_owned_when_the_tap_switches_view() {
        // A Home card tap opens the notebook while the pen is still down: the rest of that
        // contact must not leak into the page engine.
        let mut ui = home_ui(2, 0);
        let mut router = PenRouter::default();
        let mut c = fresh_ctrl();
        assert_eq!(
            drive(
                &mut ui,
                &mut router,
                &mut c,
                &[Ev::Down(400.0, 1000.0, StylusTool::Pen)]
            ),
            1
        );
        ui.enter_notebook();
        drive(
            &mut ui,
            &mut router,
            &mut c,
            &[
                Ev::Move(600.0, 1100.0),
                Ev::Move(700.0, 1200.0),
                Ev::Up(700.0, 1200.0),
            ],
        );
        assert_eq!(c.session.strokes_started(), 0);
        // Next contact in content inks.
        drive(&mut ui, &mut router, &mut c, &scribble(1000.0));
        assert_eq!(c.session.committed_count(), 1);
        // Stream gap clears UI ownership (chrome revealed so the band is a UI target).
        let g = panel_geo();
        ui.chrome.reveal(0);
        router.route_down(&ui, &g, StylusTool::Pen, Some(UiPoint { x: 10, y: 10 }));
        assert!(
            router.route_event(&ui, &g, StylusEventKind::Move, StylusTool::Pen, None, false)
                == DownRoute::Consume
        );
        router.on_stream_gap();
        assert_eq!(
            router.route_event(&ui, &g, StylusEventKind::Move, StylusTool::Pen, None, false),
            DownRoute::Page
        );
    }

    #[test]
    fn eraser_keeps_full_page_reach_in_notebook_view() {
        let ui = notebook_ui();
        let mut router = PenRouter::default();
        let g = panel_geo();
        assert_eq!(
            router.route_event(
                &ui,
                &g,
                StylusEventKind::Down,
                StylusTool::Eraser,
                Some(UiPoint { x: 900, y: 40 }),
                false
            ),
            DownRoute::Page
        );
    }

    // ---------------------------------------------------------------- rendering

    #[test]
    fn chrome_render_touches_only_band_rows_and_never_the_background() {
        let g = panel_geo();
        let bg = paper_background(&g);
        let bg_before = bg.clone();
        let mut fb = vec![0x44; g.smem_len as usize];
        let layout =
            render_notebook_chrome(&mut fb, &g, "Work", PageChrome { index: 3, count: 7 }).unwrap();
        for y in 0..g.yres {
            let row = &fb[(y * 1872) as usize..(y * 1872 + 1860) as usize];
            let in_band = layout.top_band.contains(0, y) || layout.bottom_band.contains(0, y);
            if in_band {
                assert!(row.iter().all(|b| *b != 0x44), "band row {y} not painted");
            } else {
                assert!(row.iter().all(|b| *b == 0x44), "content row {y} touched");
            }
            assert!(fb[(y * 1872 + 1860) as usize..((y + 1) * 1872) as usize]
                .iter()
                .all(|b| *b == 0x44));
        }
        assert_eq!(bg, bg_before);
        assert_eq!(PageChrome { index: 3, count: 7 }.label(), "3 of 7");
    }

    #[test]
    fn modal_overlays_and_title_editor_render_cleanly() {
        let g = panel_geo();
        let mut fb = vec![0x44; g.smem_len as usize];
        let spec = dialog_spec(&Modal::error("The notebook couldn't be created.")).unwrap();
        assert_eq!(spec.heading, "Something went wrong");
        let layout = render_dialog(&mut fb, &g, &spec).unwrap();
        // Outside the dialog halo, the underlying view is untouched.
        assert_eq!(fb[0], 0x44);
        assert_eq!(fb[(layout.frame.bottom() + 100) as usize * 1872 + 10], 0x44);
        assert!(layout.frame.w >= 1500);
        assert!(layout.buttons.iter().all(|(_, r)| r.h >= 150));

        // Title editor is full-screen: no remnant of the previous view.
        let mut fb = vec![0x44; g.smem_len as usize];
        render_title_editor(
            &mut fb,
            &g,
            "NEW NOTEBOOK",
            "CREATE",
            &TitleEditor::new("Work"),
        )
        .unwrap();
        for y in 0..g.yres as usize {
            assert!(!fb[y * 1872..y * 1872 + 1860].contains(&0x44));
        }
    }

    #[test]
    fn ui_transitions_request_full_refresh_and_typing_requests_region() {
        let g = panel_geo();
        let mut ui = home_ui(2, 0);
        let layout = HomeLayout::new(&g, &ui.home, 2);
        assert_eq!(
            ui.on_tap(&g, center(layout.new_button), &cards(2), "Notebook 3", None),
            UiCommand::Redraw(Redraw::Full)
        );
        let chooser = ui.modal.clone().unwrap();
        assert_eq!(
            ui.on_tap(
                &g,
                dialog_button(&g, &chooser, "NEW NOTEBOOK"),
                &cards(2),
                "Notebook 3",
                None
            ),
            UiCommand::Redraw(Redraw::Full)
        );
        match tap_key(&mut ui, &g, EditorKey::Char('a')) {
            UiCommand::Redraw(Redraw::Region(r)) => {
                let kb = KeyboardLayout::new(&g, false);
                assert!(r.intersects(&kb.field));
                assert!(r.h < g.yres);
            }
            other => panic!("{other:?}"),
        }
        match tap_key(&mut ui, &g, EditorKey::Mode) {
            UiCommand::Redraw(Redraw::Region(r)) => {
                let kb = KeyboardLayout::new(&g, true);
                assert!(r.bottom() >= kb.keys_area.bottom());
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            tap_key(&mut ui, &g, EditorKey::Cancel),
            UiCommand::Redraw(Redraw::Full)
        );
    }

    // ---------------------------------------------------------------- chrome vs durable bg

    #[test]
    fn chrome_home_and_modals_never_reach_background_raw() {
        let (root, mut lib) = fresh_library("bg-clean");
        let g = store_geo();
        let paper = paper_background(&g);

        // Paint every UI surface into a framebuffer (panel geometry) first.
        let pg = panel_geo();
        let mut fb = vec![0x44; pg.smem_len as usize];
        render_home(
            &mut fb,
            &pg,
            &home_cards(&lib),
            &HomeState::for_active(&lib),
        )
        .unwrap();
        render_notebook_chrome(&mut fb, &pg, "Work", PageChrome { index: 1, count: 1 }).unwrap();
        render_dialog(
            &mut fb,
            &pg,
            &dialog_spec(&Modal::sole_notebook_notice()).unwrap(),
        )
        .unwrap();

        // New notebook + inserted page get plain paper, not framebuffer contents.
        let commit = create_notebook_durable(
            &mut lib,
            &profile(),
            &g,
            &paper,
            Some("Work"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let dir = lib.notebook_dir(&commit.notebook_id);
        let mut opened = open_or_create_notebook(&dir, InkRequest::Default, &g, &paper).unwrap();
        let ins = insert_page_after_durable(
            &dir,
            &mut opened.manifest,
            "page-1",
            &InsertPageHooks::default(),
        )
        .unwrap();
        for page in ["page-1".to_string(), ins.inserted_page_id.clone()] {
            let (_s, loaded) = load_existing_page(&dir, &page).unwrap();
            assert!(
                loaded.background.iter().all(|b| *b == WHITE),
                "{page} background is not plain paper"
            );
            assert!(loaded.strokes.is_empty());
        }
        assert_eq!(
            page_chrome_for(&opened.manifest, &ins.inserted_page_id)
                .unwrap()
                .label(),
            "2 of 2"
        );
        assert!(page_chrome_for(&opened.manifest, "page-99").is_err());
        // Library authority unchanged by rendering; create is still prebuild → commit.
        let m = read_library_manifest_for_test(&root).unwrap();
        assert_eq!(m.active_notebook_id, commit.notebook_id);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn search_placeholder_is_inert_and_tools_opens_a_fresh_tools_screen() {
        let g = panel_geo_for_tools();
        let c = cards(3);
        let mut ui = home_ui(3, 0);
        let layout = HomeLayout::new(&g, &ui.home, 3);
        let before = ui.clone();
        assert_eq!(
            ui.on_tap(&g, center(layout.search), &c, "Notebook 4", None),
            UiCommand::None
        );
        assert_eq!(ui, before, "search placeholder changes nothing");

        assert_eq!(
            ui.on_tap(&g, center(layout.tools_button), &c, "Notebook 4", None),
            UiCommand::OpenTools
        );
        ui.enter_tools();
        assert_eq!(ui.view, AppView::Tools);
        assert!(!ui.allows_page_input(), "Tools is never a writing surface");
        let tl = crate::tools_ui::ToolsLayout::new(&g, &ui.tools);
        assert_eq!(
            ui.on_tap(&g, center(tl.primary.as_ref().unwrap().0), &c, "N", None),
            UiCommand::ToolsScan
        );
        assert_eq!(
            ui.on_tap(&g, center(tl.chrome.back), &c, "N", None),
            UiCommand::GoHome
        );
        // Busy: Back is inert while an import runs.
        ui.tools.phase = crate::tools_ui::ToolsPhase::Importing {
            done: 0,
            total: 2,
            current: "Journal".into(),
        };
        assert_eq!(
            ui.on_tap(&g, center(tl.chrome.back), &c, "N", None),
            UiCommand::None
        );
        // Re-entering Tools starts fresh.
        ui.enter_tools();
        assert_eq!(ui.tools, crate::tools_ui::ToolsState::default());
    }

    fn panel_geo_for_tools() -> FbGeometry {
        crate::ui_gfx::panel_geo()
    }
}
