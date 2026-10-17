//! Kindle-local folders on Home: row layout/paging, menus, picker and the + NEW chooser.
//! Pure UI-state tests; the durable store is covered in `folders.rs`.

#[cfg(test)]
mod tests {
    use crate::app_ui::{Redraw, UiCommand, UiState};
    use crate::folders::FolderInfo;
    use crate::home::{
        home_rows, render_home_view, HomeCard, HomeHit, HomeLayout, HomeRow, HomeState,
        FOLDERS_PER_ROW, ROWS_PER_PAGE,
    };
    use crate::modal::{dialog_spec, DialogLayout, Modal, TitlePurpose, PICKER_PAGE};
    use crate::ui_gfx::{panel_geo, Rect, UiPoint};
    use kindle_platform::FbGeometry;

    fn cards(n: usize) -> Vec<HomeCard> {
        (1..=n)
            .map(|i| HomeCard {
                notebook_id: format!("notebook-{i}"),
                title: format!("Notebook {i}"),
                page_count: Some(1),
            })
            .collect()
    }

    fn folder(n: usize, members: &[usize]) -> FolderInfo {
        FolderInfo {
            id: format!("folder-{n}"),
            title: format!("Folder {n}"),
            notebook_ids: members.iter().map(|i| format!("notebook-{i}")).collect(),
        }
    }

    fn center(r: Rect) -> UiPoint {
        UiPoint {
            x: r.x + r.w / 2,
            y: r.y + r.h / 2,
        }
    }

    fn ui_with(folders: Vec<FolderInfo>, total: usize) -> UiState {
        let mut ui = UiState::launch_home(0, total);
        ui.folders = folders;
        ui.settle_home(&cards(total));
        ui
    }

    fn layout(g: &FbGeometry, ui: &UiState, c: &[HomeCard]) -> HomeLayout {
        HomeLayout::for_rows(g, &ui.home, &ui.home_rows(c), ui.open_folder().is_some())
    }

    fn button(g: &FbGeometry, modal: &Modal, label: &str) -> UiPoint {
        let spec = dialog_spec(modal).unwrap();
        let l = DialogLayout::new(g, &spec);
        let (_, r) = l
            .buttons
            .iter()
            .find(|(b, _)| b.label == label)
            .unwrap_or_else(|| panic!("no button {label} in {modal:?}"));
        center(*r)
    }

    fn labels(modal: &Modal) -> Vec<String> {
        dialog_spec(modal)
            .unwrap()
            .buttons
            .into_iter()
            .map(|b| b.label)
            .collect()
    }

    #[test]
    fn folders_come_first_three_per_row_and_page_without_repeating() {
        let c = cards(10);
        // 7 folders → 3 folder rows; notebooks 1..3 are filed, 4..10 stay on the main list.
        let mut folders: Vec<FolderInfo> = (1..=7).map(|n| folder(n, &[])).collect();
        folders[0] = folder(1, &[1, 2]);
        folders[6] = folder(7, &[3]);
        let rows = home_rows(&c, &folders, None);
        assert_eq!(
            rows[..3],
            [
                HomeRow::Folders(vec![0, 1, 2]),
                HomeRow::Folders(vec![3, 4, 5]),
                HomeRow::Folders(vec![6]),
            ]
        );
        assert_eq!(
            rows[3..],
            (3..10).map(HomeRow::Notebook).collect::<Vec<_>>(),
            "filed notebooks leave the main list; the rest keep library order"
        );

        // Folder view: that folder's notebooks, library order.
        assert_eq!(
            home_rows(&c, &folders, Some("folder-1")),
            vec![HomeRow::Notebook(0), HomeRow::Notebook(1)]
        );

        // Many folders span pages; each folder appears on exactly one page.
        let many: Vec<FolderInfo> = (1..=20).map(|n| folder(n, &[])).collect();
        let g = panel_geo();
        let rows = home_rows(&c, &many, None);
        assert_eq!(rows.len(), 20usize.div_ceil(FOLDERS_PER_ROW) + 10);
        let mut seen = Vec::new();
        let mut state = HomeState::selecting(0, 10);
        state.settle(&rows, &c, &many);
        assert_eq!(state.page, 1, "notebook 0 follows the 7 folder rows");
        state.page = 0;
        loop {
            let l = HomeLayout::for_rows(&g, &state, &rows, false);
            assert!(l.cards.len() + l.folders.len() / FOLDERS_PER_ROW <= ROWS_PER_PAGE + 1);
            seen.extend(l.folders.iter().map(|(f, _, _)| *f));
            // Tiles of one row share y and never overlap.
            for w in l.folders.windows(2) {
                if w[0].1.y == w[1].1.y {
                    assert!(w[0].1.right() < w[1].1.x);
                }
            }
            if !state.can_page_next(rows.len()) {
                break;
            }
            state.page += 1;
        }
        assert_eq!(
            seen,
            (0..20).collect::<Vec<_>>(),
            "no folder repeated or lost"
        );
    }

    #[test]
    fn launch_reveals_the_row_of_the_active_notebook_or_its_folder() {
        let c = cards(12);
        let folders: Vec<FolderInfo> = (1..=18).map(|n| folder(n, &[])).collect();
        // 6 folder rows fill page 0; notebook index 0 is on page 1.
        let mut ui = UiState::launch_home(0, 12);
        ui.folders = folders.clone();
        ui.settle_home(&c);
        assert_eq!(ui.home.page, 1);
        // Filed in folder 1 → its tile row is on page 0.
        let mut ui = UiState::launch_home(0, 12);
        ui.folders = folders;
        ui.folders[0] = folder(1, &[1]);
        ui.settle_home(&c);
        assert_eq!(ui.home.page, 0);
    }

    #[test]
    fn opening_a_folder_new_inside_it_and_back() {
        let g = panel_geo();
        let c = cards(4);
        let mut ui = ui_with(vec![folder(1, &[2, 3]), folder(2, &[])], 4);
        let l = layout(&g, &ui, &c);
        assert_eq!(l.folders.len(), 2);
        assert_eq!(l.cards.len(), 2, "notebooks 1 and 4 on the main list");

        let cmd = ui.on_tap(&g, center(l.folders[0].1), &c, "Notebook 5", None);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert_eq!(ui.home_folder.as_deref(), Some("folder-1"));
        let l = layout(&g, &ui, &c);
        assert!(l.folders.is_empty());
        assert_eq!(
            l.cards.iter().map(|(i, _, _)| *i).collect::<Vec<_>>(),
            vec![1, 2]
        );
        let mut fb = vec![0u8; g.smem_len as usize];
        render_home_view(&mut fb, &g, &c, &ui.folders, Some("folder-1"), &ui.home).unwrap();

        // + NEW inside a folder: straight to the editor, filed in this folder.
        ui.on_tap(&g, center(l.new_button), &c, "Notebook 5", None);
        match &ui.modal {
            Some(Modal::TitleEntry {
                purpose: TitlePurpose::CreateNotebookInFolder { folder_id },
                editor,
            }) => {
                assert_eq!(folder_id, "folder-1");
                assert_eq!(editor.text(), "Notebook 5");
            }
            other => panic!("{other:?}"),
        }
        ui.modal = None;

        // Top-left is Back inside a folder, not Exit.
        assert_eq!(l.hit(center(l.exit_hit), &ui.home, 4), Some(HomeHit::Back));
        let cmd = ui.on_tap(&g, center(l.exit_hit), &c, "Notebook 5", None);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert_eq!(ui.home_folder, None);

        // A vanished open folder falls back to the main list.
        ui.home_folder = Some("folder-9".into());
        ui.settle_home(&c);
        assert_eq!(ui.home_folder, None);
    }

    #[test]
    fn new_chooser_offers_notebook_or_folder() {
        let g = panel_geo();
        let c = cards(2);
        let mut ui = ui_with(vec![folder(1, &[])], 2);
        ui.next_folder_title = "Folder 2".into();
        let l = layout(&g, &ui, &c);
        ui.on_tap(&g, center(l.new_button), &c, "Notebook 3", None);
        let chooser = ui.modal.clone().unwrap();
        assert_eq!(labels(&chooser), ["NEW NOTEBOOK", "NEW FOLDER", "CANCEL"]);
        ui.on_tap(&g, button(&g, &chooser, "NEW FOLDER"), &c, "x", None);
        match &ui.modal {
            Some(Modal::TitleEntry {
                purpose: TitlePurpose::CreateFolder,
                editor,
            }) => assert_eq!(editor.text(), "Folder 2"),
            other => panic!("{other:?}"),
        }
        ui.modal = Some(chooser.clone());
        assert_eq!(
            ui.on_tap(&g, button(&g, &chooser, "CANCEL"), &c, "x", None),
            UiCommand::Redraw(Redraw::Full)
        );
        assert!(ui.modal.is_none());
    }

    #[test]
    fn notebook_menus_add_change_and_remove_folder() {
        let g = panel_geo();
        let c = cards(3);
        let mut ui = ui_with(vec![folder(1, &[2]), folder(2, &[])], 3);
        let l = layout(&g, &ui, &c);

        // Main list notebook: ADD TO FOLDER → picker of every folder.
        ui.on_tap(&g, center(l.cards[0].2), &c, "x", None);
        let menu = ui.modal.clone().unwrap();
        assert_eq!(
            labels(&menu),
            ["RENAME", "ADD TO FOLDER", "DELETE", "CANCEL"]
        );
        ui.on_tap(&g, button(&g, &menu, "ADD TO FOLDER"), &c, "x", None);
        let picker = ui.modal.clone().unwrap();
        assert_eq!(labels(&picker), ["Folder 1", "Folder 2", "CANCEL"]);
        assert_eq!(
            ui.on_tap(&g, button(&g, &picker, "Folder 2"), &c, "x", None),
            UiCommand::SetNotebookFolder {
                notebook_id: "notebook-1".into(),
                folder: Some("folder-2".into()),
            }
        );

        // Inside folder 1: CHANGE FOLDER (others only) and REMOVE FROM FOLDER.
        ui.home_folder = Some("folder-1".into());
        let l = layout(&g, &ui, &c);
        ui.on_tap(&g, center(l.cards[0].2), &c, "x", None);
        let menu = ui.modal.clone().unwrap();
        assert_eq!(
            labels(&menu),
            [
                "RENAME",
                "CHANGE FOLDER",
                "REMOVE FROM FOLDER",
                "DELETE",
                "CANCEL"
            ]
        );
        ui.on_tap(&g, button(&g, &menu, "CHANGE FOLDER"), &c, "x", None);
        assert_eq!(labels(ui.modal.as_ref().unwrap()), ["Folder 2", "CANCEL"]);
        ui.modal = Some(menu.clone());
        assert_eq!(
            ui.on_tap(&g, button(&g, &menu, "REMOVE FROM FOLDER"), &c, "x", None),
            UiCommand::SetNotebookFolder {
                notebook_id: "notebook-2".into(),
                folder: None,
            }
        );
        // RENAME and DELETE keep working from the in-folder menu.
        ui.modal = Some(menu.clone());
        ui.on_tap(&g, button(&g, &menu, "RENAME"), &c, "x", None);
        assert!(matches!(
            ui.modal,
            Some(Modal::TitleEntry {
                purpose: TitlePurpose::Rename { .. },
                ..
            })
        ));
        ui.modal = Some(menu.clone());
        ui.on_tap(&g, button(&g, &menu, "DELETE"), &c, "x", None);
        assert!(matches!(
            ui.modal,
            Some(Modal::ConfirmDeleteNotebook { .. })
        ));

        // Only one folder: CHANGE FOLDER explains; no folders: ADD TO FOLDER explains.
        let mut ui = ui_with(vec![folder(1, &[1])], 3);
        ui.home_folder = Some("folder-1".into());
        ui.modal = Some(Modal::CardMenuInFolder {
            notebook_id: "notebook-1".into(),
            title: "Notebook 1".into(),
            folder_id: "folder-1".into(),
        });
        let menu = ui.modal.clone().unwrap();
        ui.on_tap(&g, button(&g, &menu, "CHANGE FOLDER"), &c, "x", None);
        assert_eq!(ui.modal, Some(Modal::no_other_folder_notice()));
        let mut ui = ui_with(Vec::new(), 3);
        let menu = Modal::CardMenu {
            notebook_id: "notebook-1".into(),
            title: "Notebook 1".into(),
        };
        ui.modal = Some(menu.clone());
        ui.on_tap(&g, button(&g, &menu, "ADD TO FOLDER"), &c, "x", None);
        assert_eq!(ui.modal, Some(Modal::no_folders_notice()));
    }

    #[test]
    fn picker_pages_through_many_folders() {
        let g = panel_geo();
        let c = cards(1);
        let n = PICKER_PAGE * 2 + 1;
        let mut ui = ui_with((1..=n).map(|i| folder(i, &[])).collect(), 1);
        let menu = Modal::CardMenu {
            notebook_id: "notebook-1".into(),
            title: "Notebook 1".into(),
        };
        ui.modal = Some(menu.clone());
        ui.on_tap(&g, button(&g, &menu, "ADD TO FOLDER"), &c, "x", None);
        let p0 = ui.modal.clone().unwrap();
        let spec = dialog_spec(&p0).unwrap();
        assert_eq!(spec.buttons.len(), PICKER_PAGE + 3);
        assert!(
            !spec
                .buttons
                .iter()
                .find(|b| b.label == "PREVIOUS")
                .unwrap()
                .enabled
        );
        // The dialog fits on the panel.
        let l = DialogLayout::new(&g, &spec);
        assert!(l.frame.bottom() <= g.yres);

        ui.on_tap(&g, button(&g, &p0, "NEXT"), &c, "x", None);
        ui.on_tap(
            &g,
            button(&g, &ui.modal.clone().unwrap(), "NEXT"),
            &c,
            "x",
            None,
        );
        let last = ui.modal.clone().unwrap();
        assert_eq!(labels(&last)[0], format!("Folder {n}"));
        assert!(
            !dialog_spec(&last)
                .unwrap()
                .buttons
                .iter()
                .find(|b| b.label == "NEXT")
                .unwrap()
                .enabled
        );
        assert_eq!(
            ui.on_tap(&g, button(&g, &last, &format!("Folder {n}")), &c, "x", None),
            UiCommand::SetNotebookFolder {
                notebook_id: "notebook-1".into(),
                folder: Some(format!("folder-{n}")),
            }
        );
    }

    #[test]
    fn folder_menu_renames_and_deletes_with_confirmation() {
        let g = panel_geo();
        let c = cards(2);
        let mut ui = ui_with(vec![folder(1, &[1])], 2);
        let l = layout(&g, &ui, &c);
        ui.on_tap(&g, center(l.folders[0].2), &c, "x", None);
        let menu = ui.modal.clone().unwrap();
        assert_eq!(labels(&menu), ["RENAME", "DELETE", "CANCEL"]);

        ui.on_tap(&g, button(&g, &menu, "DELETE"), &c, "x", None);
        let confirm = ui.modal.clone().unwrap();
        assert!(matches!(confirm, Modal::ConfirmDeleteFolder { .. }));
        assert_eq!(
            ui.on_tap(&g, button(&g, &confirm, "DELETE"), &c, "x", None),
            UiCommand::DeleteFolder("folder-1".into())
        );

        ui.modal = Some(menu.clone());
        ui.on_tap(&g, button(&g, &menu, "RENAME"), &c, "x", None);
        match &ui.modal {
            Some(Modal::TitleEntry {
                purpose:
                    TitlePurpose::RenameFolder {
                        folder_id,
                        original,
                    },
                editor,
            }) => {
                assert_eq!(folder_id, "folder-1");
                assert_eq!(original, "Folder 1");
                assert_eq!(editor.text(), "Folder 1");
            }
            other => panic!("{other:?}"),
        }
    }
}
