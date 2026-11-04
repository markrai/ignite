//! Slice 4: Kindle paper-picker UI → durable notebook creation.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use kindle_platform::FbGeometry;

    use crate::app_ui::{Redraw, UiCommand, UiState};
    use crate::folders::{FolderInfo, FolderStore};
    use crate::home::{HomeCard, HomeLayout};
    use crate::ink::InkRequest;
    use crate::keyboard::{EditorKey, KeyboardLayout, TitleEditor};
    use crate::library::{
        create_notebook_durable, create_notebook_durable_with_template, open_or_create_library,
        CreateNotebookHooks, LibraryManifestWriteHooks,
    };
    use crate::modal::{dialog_spec, DialogLayout, Modal, TitlePurpose};
    use crate::notebook::open_or_create_notebook;
    use crate::paper_templates::{generate_page_background, PageTemplate};
    use crate::profile::RenderProfile;
    use crate::store::{load_existing_page, DEFAULT_PAGE_ID};
    use crate::ui_gfx::{panel_geo, paper_background, Rect, UiPoint};

    fn tmp(label: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scribe-tmpl4-{label}-{n}"));
        let _ = fs::remove_dir_all(&p);
        p
    }

    /// Tall enough for chrome reserve + at least one ruled line.
    fn geo() -> FbGeometry {
        FbGeometry {
            xres: 120,
            yres: 200,
            xres_virtual: 120,
            yres_virtual: 200,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 120,
            smem_len: 120 * 200,
        }
    }

    fn profile() -> RenderProfile {
        RenderProfile::sealed_v2_pressure(0x00).unwrap()
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

    fn dialog_button(g: &FbGeometry, modal: &Modal, label: &str) -> UiPoint {
        let spec = dialog_spec(modal).unwrap();
        let layout = DialogLayout::new(g, &spec);
        let (_, r) = layout
            .buttons
            .iter()
            .find(|(b, _)| b.label == label)
            .unwrap_or_else(|| panic!("no button {label} in {:?}", labels(modal)));
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

    fn tap_key(ui: &mut UiState, g: &FbGeometry, key: EditorKey) -> UiCommand {
        let Modal::TitleEntry { editor, .. } = ui.modal.as_ref().unwrap() else {
            panic!("expected title entry");
        };
        let layout = KeyboardLayout::new(g, editor.symbols());
        let (_, r) = layout
            .keys
            .iter()
            .find(|(k, _)| *k == key)
            .unwrap_or_else(|| panic!("missing key {key:?}"));
        ui.on_tap(g, center(*r), &cards(2), "x", None)
    }

    fn expected_bg(template: PageTemplate) -> Vec<u8> {
        let g = geo();
        let mut bg = generate_page_background(template, g.xres, g.yres).unwrap();
        crate::page_label::scrub_chrome_band(&mut bg, &g).unwrap();
        bg
    }

    /// Drive main-list New → New Notebook → paper → title Submit to a create command.
    fn main_create_command(template: PageTemplate, title: &str) -> UiCommand {
        let g = panel_geo();
        let c = cards(2);
        let mut ui = UiState::launch_home(0, 2);
        let layout = HomeLayout::new(&g, &ui.home, 2);
        ui.on_tap(&g, center(layout.new_button), &c, "Notebook 3", None);
        let chooser = ui.modal.clone().unwrap();
        ui.on_tap(
            &g,
            dialog_button(&g, &chooser, "NEW NOTEBOOK"),
            &c,
            "x",
            None,
        );
        let paper = ui.modal.clone().unwrap();
        assert_eq!(
            labels(&paper),
            ["BLANK", "RULED", "GRID", "DOTTED", "CANCEL"]
        );
        ui.on_tap(
            &g,
            dialog_button(&g, &paper, template.dialog_label()),
            &c,
            "x",
            None,
        );
        match &ui.modal {
            Some(Modal::TitleEntry {
                purpose: TitlePurpose::Create { template: t },
                editor,
            }) => {
                assert_eq!(*t, template);
                assert_eq!(editor.text(), "Notebook 3");
            }
            other => panic!("{other:?}"),
        }
        // Replace the seed title with the requested one.
        ui.show_modal(Modal::TitleEntry {
            purpose: TitlePurpose::Create { template },
            editor: TitleEditor::new(title),
        });
        tap_key(&mut ui, &g, EditorKey::Submit)
    }

    /// Apply a create command through the same durable helper the runtime uses.
    fn apply_create(
        library: &mut crate::library::LibraryRepository,
        cmd: &UiCommand,
    ) -> Result<String, String> {
        match cmd {
            UiCommand::CreateNotebook { title, template } => {
                let commit = create_notebook_durable_with_template(
                    library,
                    &profile(),
                    &geo(),
                    *template,
                    Some(title),
                    &CreateNotebookHooks::default(),
                )?;
                Ok(commit.notebook_id)
            }
            UiCommand::CreateNotebookInFolder {
                title,
                folder_id,
                template,
            } => {
                let commit = create_notebook_durable_with_template(
                    library,
                    &profile(),
                    &geo(),
                    *template,
                    Some(title),
                    &CreateNotebookHooks::default(),
                )?;
                let live = library.manifest().notebook_ids.clone();
                // Mirror runtime: filing failure leaves the notebook on the main list.
                let mut folders = FolderStore::open(library.root());
                let _ = folders.set_folder(&commit.notebook_id, Some(folder_id), &live);
                Ok(commit.notebook_id)
            }
            other => panic!("not a create command: {other:?}"),
        }
    }

    #[test]
    fn paper_picker_offers_four_templates_and_cancel() {
        let paper = Modal::PaperChooser {
            notebook_title: "Notebook 3".into(),
            folder_id: None,
        };
        assert_eq!(
            labels(&paper),
            ["BLANK", "RULED", "GRID", "DOTTED", "CANCEL"]
        );
    }

    #[test]
    fn each_template_choice_reaches_durable_create_with_matching_background() {
        for template in PageTemplate::picker_choices() {
            let root = tmp(&format!("ui-{template:?}"));
            let opened = open_or_create_library(
                &root,
                InkRequest::Default,
                &geo(),
                &paper_background(&geo()),
            )
            .unwrap();
            let mut library = opened.repository;
            let before = library.manifest().notebook_ids.clone();
            let title = format!("T-{template:?}");
            let cmd = main_create_command(template, &title);
            assert_eq!(
                cmd,
                UiCommand::CreateNotebook {
                    title: title.clone(),
                    template,
                }
            );
            // No durable write until the create command is applied.
            assert_eq!(library.manifest().notebook_ids, before);

            let nb_id = apply_create(&mut library, &cmd).unwrap();
            assert_eq!(library.title(&nb_id).unwrap(), title);
            let nb = library.notebook_dir(&nb_id);
            let bg = fs::read(nb.join(DEFAULT_PAGE_ID).join("background.raw")).unwrap();
            assert_eq!(bg, expected_bg(template), "{template:?}");

            let opened = open_or_create_notebook(
                &nb,
                InkRequest::Default,
                &geo(),
                &paper_background(&geo()),
            )
            .unwrap();
            assert_eq!(opened.loaded.background, expected_bg(template));
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn folder_new_preserves_folder_context_and_template() {
        let g = panel_geo();
        let c = cards(3);
        let mut ui = UiState::launch_home(0, 3);
        ui.folders = vec![FolderInfo {
            id: "folder-1".into(),
            title: "Projects".into(),
            notebook_ids: vec!["notebook-2".into()],
        }];
        ui.settle_home(&c);
        ui.home_folder = Some("folder-1".into());
        let layout = HomeLayout::for_rows(&g, &ui.home, &ui.home_rows(&c), true);
        ui.on_tap(&g, center(layout.new_button), &c, "Notebook 4", None);
        let paper = ui.modal.clone().unwrap();
        match &paper {
            Modal::PaperChooser {
                folder_id: Some(id),
                notebook_title,
            } => {
                assert_eq!(id, "folder-1");
                assert_eq!(notebook_title, "Notebook 4");
            }
            other => panic!("{other:?}"),
        }
        ui.on_tap(&g, dialog_button(&g, &paper, "DOTTED"), &c, "x", None);
        ui.show_modal(Modal::TitleEntry {
            purpose: TitlePurpose::CreateNotebookInFolder {
                folder_id: "folder-1".into(),
                template: PageTemplate::Dotted,
            },
            editor: TitleEditor::new("Folder Note"),
        });
        let cmd = tap_key(&mut ui, &g, EditorKey::Submit);
        assert_eq!(
            cmd,
            UiCommand::CreateNotebookInFolder {
                title: "Folder Note".into(),
                folder_id: "folder-1".into(),
                template: PageTemplate::Dotted,
            }
        );

        let root = tmp("folder-create");
        let opened = open_or_create_library(
            &root,
            InkRequest::Default,
            &geo(),
            &paper_background(&geo()),
        )
        .unwrap();
        let mut library = opened.repository;
        // Seed the folder so set_folder can succeed.
        let mut folders = FolderStore::open(&root);
        let folder_id = folders.create("Projects", &[]).unwrap();
        let cmd = UiCommand::CreateNotebookInFolder {
            title: "Folder Note".into(),
            folder_id: folder_id.clone(),
            template: PageTemplate::Dotted,
        };
        let nb_id = apply_create(&mut library, &cmd).unwrap();
        let folders = FolderStore::open(&root);
        assert_eq!(folders.folder_of(&nb_id), Some(folder_id.as_str()));
        let bg = fs::read(
            library
                .notebook_dir(&nb_id)
                .join(DEFAULT_PAGE_ID)
                .join("background.raw"),
        )
        .unwrap();
        assert_eq!(bg, expected_bg(PageTemplate::Dotted));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cancel_before_paper_selection_writes_nothing() {
        let g = panel_geo();
        let c = cards(2);
        let mut ui = UiState::launch_home(0, 2);
        let layout = HomeLayout::new(&g, &ui.home, 2);
        ui.on_tap(&g, center(layout.new_button), &c, "Notebook 3", None);
        let chooser = ui.modal.clone().unwrap();
        ui.on_tap(
            &g,
            dialog_button(&g, &chooser, "NEW NOTEBOOK"),
            &c,
            "x",
            None,
        );
        let paper = ui.modal.clone().unwrap();
        let cmd = ui.on_tap(&g, dialog_button(&g, &paper, "CANCEL"), &c, "x", None);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert!(ui.modal.is_none());
        assert!(!matches!(
            cmd,
            UiCommand::CreateNotebook { .. } | UiCommand::CreateNotebookInFolder { .. }
        ));
    }

    #[test]
    fn cancel_during_title_entry_writes_nothing() {
        let g = panel_geo();
        let mut ui = UiState::launch_home(0, 2);
        ui.show_modal(Modal::TitleEntry {
            purpose: TitlePurpose::Create {
                template: PageTemplate::Grid,
            },
            editor: TitleEditor::new("Would Create"),
        });
        let cmd = tap_key(&mut ui, &g, EditorKey::Cancel);
        assert_eq!(cmd, UiCommand::Redraw(Redraw::Full));
        assert!(ui.modal.is_none());
    }

    #[test]
    fn failed_create_leaves_library_unchanged() {
        let root = tmp("fail-ui");
        let opened = open_or_create_library(
            &root,
            InkRequest::Default,
            &geo(),
            &paper_background(&geo()),
        )
        .unwrap();
        let mut library = opened.repository;
        let before = library.manifest().notebook_ids.clone();
        let hooks = CreateNotebookHooks {
            manifest: LibraryManifestWriteHooks {
                fail_write: true,
                ..Default::default()
            },
            fail_cleanup: false,
        };
        let err = create_notebook_durable_with_template(
            &mut library,
            &profile(),
            &geo(),
            PageTemplate::Ruled,
            Some("Nope"),
            &hooks,
        )
        .unwrap_err();
        assert!(err.contains("library manifest persist failed"), "{err}");
        assert_eq!(library.manifest().notebook_ids, before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn blank_default_paths_without_picker_remain_blank() {
        let root = tmp("blank-default");
        let opened = open_or_create_library(
            &root,
            InkRequest::Default,
            &geo(),
            &paper_background(&geo()),
        )
        .unwrap();
        let mut library = opened.repository;
        // Noninteractive / paper_background path (dev Home, import, etc.).
        let commit = create_notebook_durable(
            &mut library,
            &profile(),
            &geo(),
            &paper_background(&geo()),
            Some("Plain"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let bg = fs::read(
            library
                .notebook_dir(&commit.notebook_id)
                .join(DEFAULT_PAGE_ID)
                .join("background.raw"),
        )
        .unwrap();
        assert_eq!(bg, expected_bg(PageTemplate::Blank));
        assert!(bg.iter().all(|&b| b == 0xFF));

        // Explicit Blank through the picker path matches the same bytes.
        let cmd = main_create_command(PageTemplate::Blank, "Picked Blank");
        let nb = apply_create(&mut library, &cmd).unwrap();
        let picked = fs::read(
            library
                .notebook_dir(&nb)
                .join(DEFAULT_PAGE_ID)
                .join("background.raw"),
        )
        .unwrap();
        assert_eq!(picked, bg);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn modal_outside_tap_does_not_create_or_dismiss_paper_chooser() {
        let g = panel_geo();
        let c = cards(2);
        let mut ui = UiState::launch_home(0, 2);
        ui.show_modal(Modal::PaperChooser {
            notebook_title: "Notebook 3".into(),
            folder_id: None,
        });
        let cmd = ui.on_tap(&g, UiPoint { x: 5, y: 5 }, &c, "x", None);
        assert_eq!(cmd, UiCommand::None);
        assert!(matches!(ui.modal, Some(Modal::PaperChooser { .. })));
    }

    #[test]
    fn load_existing_page_after_ui_create_preserves_template() {
        let (_store, loaded) = {
            let root = tmp("reopen-ui");
            let opened = open_or_create_library(
                &root,
                InkRequest::Default,
                &geo(),
                &paper_background(&geo()),
            )
            .unwrap();
            let mut library = opened.repository;
            let cmd = main_create_command(PageTemplate::Ruled, "Ruled Book");
            let nb = apply_create(&mut library, &cmd).unwrap();
            let dir = library.notebook_dir(&nb);
            let result = load_existing_page(&dir, DEFAULT_PAGE_ID).unwrap();
            let _ = fs::remove_dir_all(root);
            result
        };
        assert_eq!(loaded.background, expected_bg(PageTemplate::Ruled));
    }
}
