//! Host tests for replica-local preferences dispatch, picker isolation, and links.jsonl.

#[cfg(test)]
mod tests {
    use std::fs;

    use kindle_platform::{StylusEventKind, StylusTool};

    use crate::app_ui::{DownRoute, PenRouter, UiCommand, UiState};
    use crate::home::AppView;
    use crate::link_picker::{
        link_jump_presentation, resolve_live_target, selected_target_is_live, LinkPickerHit,
        LinkPickerState, PendingLink, MISSING_LINK_LABEL,
    };
    use crate::modal::{dialog_spec, DialogLayout, Modal};
    use crate::nav::{dispatch_side_button, SideButtonDispatch};
    use crate::prefs::SideButtonAction;
    use crate::store::{load_page_links, write_page_links};
    use crate::ui_gfx::{panel_geo, Rect, UiPoint};
    use sync_engine::testkit::{lk, nb, page_link, pg};

    fn center(r: Rect) -> UiPoint {
        UiPoint {
            x: r.x + r.w / 2,
            y: r.y + r.h / 2,
        }
    }

    fn dialog_button(g: &kindle_platform::FbGeometry, modal: &Modal, label: &str) -> UiPoint {
        let spec = dialog_spec(modal).unwrap();
        let layout = DialogLayout::new(g, &spec);
        let (_, r) = layout
            .buttons
            .iter()
            .find(|(b, _)| b.label == label)
            .unwrap_or_else(|| panic!("no button {label}"));
        center(*r)
    }

    fn pending() -> PendingLink {
        PendingLink {
            source_notebook_local: "notebook-1".into(),
            source_page_local: "page-1".into(),
            source_notebook_id: nb(1),
            source_page_id: pg(1),
            source_x: 2000.0,
            source_y: 3000.0,
        }
    }

    #[test]
    fn non_writing_views_own_pen_down() {
        let g = panel_geo();
        let p = Some(UiPoint { x: 100, y: 100 });
        for view in [
            AppView::Home,
            AppView::Sync,
            AppView::Tools,
            AppView::Preferences,
            AppView::LinkTargetPicker,
        ] {
            let mut ui = UiState::launch_home(0, 1);
            ui.view = view;
            if view == AppView::LinkTargetPicker {
                ui.link_picker = Some(LinkPickerState::notebooks(pending()));
            }
            assert!(
                !ui.view.allows_page_input(),
                "{view:?} must not be a writing surface"
            );
            let mut router = PenRouter::default();
            let route = router.route_down(&ui, &g, StylusTool::Pen, p);
            assert_ne!(
                route,
                DownRoute::Page,
                "{view:?} Down must not reach the page engine"
            );
            assert_eq!(
                router.route_event(&ui, &g, StylusEventKind::Move, StylusTool::Pen, None, false),
                DownRoute::Consume
            );
        }
    }

    #[test]
    fn create_link_dispatch_consumes_whole_page() {
        let p = notebook_core::PagePoint::new(10.0, 0.0);
        assert!(matches!(
            dispatch_side_button(SideButtonAction::CreateLink, true, p),
            Some(SideButtonDispatch::CreateLink { page }) if page == p
        ));
        assert!(matches!(
            dispatch_side_button(SideButtonAction::NavigationUndo, true, p),
            Some(SideButtonDispatch::Nav(_))
        ));
    }

    #[test]
    fn links_jsonl_round_trip_and_empty_removes_file() {
        let dir =
            std::env::temp_dir().join(format!("scribe-links-{}-{}", std::process::id(), "round"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(load_page_links(&dir).unwrap().is_empty());
        let link = page_link(1, nb(1), pg(1), nb(2), pg(2));
        write_page_links(&dir, std::slice::from_ref(&link)).unwrap();
        let loaded = load_page_links(&dir).unwrap();
        assert_eq!(loaded, vec![link.clone()]);
        write_page_links(&dir, &[]).unwrap();
        assert!(!dir.join("links.jsonl").exists());
        assert!(load_page_links(&dir).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn picker_source_page_is_not_selectable() {
        let g = panel_geo();
        let mut picker = LinkPickerState::notebooks(pending());
        picker.show_pages(
            "notebook-1".into(),
            "Notes".into(),
            vec!["page-1".into(), "page-2".into()],
        );
        let cards = vec![];
        // Source thumbnail is the first cell; tapping it must not confirm.
        let hit = picker.hit(&g, UiPoint { x: 200, y: 400 }, &cards);
        assert!(
            !matches!(
                hit,
                LinkPickerHit::SelectPage { ref page_local, .. } if page_local == "page-1"
            ),
            "source page must stay disabled, got {hit:?}"
        );
    }

    #[test]
    fn preferences_tab_is_a_peer_of_tools() {
        let g = panel_geo();
        let mut ui = UiState::launch_home(0, 1);
        ui.enter_tools();
        let tl = crate::tools_ui::ToolsLayout::new(&g, &ui.tools);
        assert_eq!(
            ui.on_tap(
                &g,
                crate::ui_gfx::UiPoint {
                    x: tl.chrome.prefs_tab.x + 10,
                    y: tl.chrome.prefs_tab.y + 10
                },
                &[],
                "N",
                None
            ),
            UiCommand::OpenPreferences
        );
        ui.enter_preferences();
        assert_eq!(ui.view, AppView::Preferences);
        assert!(!ui.allows_page_input());
        let pl = crate::settings_ui::PreferencesLayout::new(&g);
        assert_eq!(
            ui.on_tap(
                &g,
                UiPoint {
                    x: pl.chrome.tools_tab.x + 10,
                    y: pl.chrome.tools_tab.y + 10
                },
                &[],
                "N",
                None
            ),
            UiCommand::ShowToolsTab
        );
        let create = center(pl.create_link);
        assert_eq!(
            ui.on_tap(&g, create, &[], "N", None),
            UiCommand::SetSideButton(SideButtonAction::CreateLink)
        );
    }

    #[test]
    fn picker_cancel_from_notebook_list_is_empty_and_isolated() {
        let g = panel_geo();
        let mut ui = UiState::launch_home(0, 1);
        ui.enter_link_picker(LinkPickerState::notebooks(pending()));
        assert!(!ui.allows_page_input());
        let hit = ui.on_tap(&g, UiPoint { x: 80, y: 80 }, &[], "N", None);
        assert_eq!(hit, UiCommand::LinkPickerCancel);
    }

    #[test]
    fn link_jump_uses_page_id_and_outside_tap_dismisses() {
        let g = panel_geo();
        let mut ui = UiState::launch_home(0, 1);
        ui.enter_notebook();
        ui.modal = Some(Modal::LinkJump {
            link_id: lk(1),
            label: "Jump to \"Notes - Page 2\"".into(),
            target_live: true,
            notebook_local: Some("notebook-2".into()),
            page_local: Some("page-3".into()),
        });
        assert!(!ui.allows_page_input());
        let jump = dialog_button(&g, ui.modal.as_ref().unwrap(), "Jump to \"Notes - Page 2\"");
        assert_eq!(
            ui.on_tap(&g, jump, &[], "N", None),
            UiCommand::JumpLink {
                notebook_local: "notebook-2".into(),
                page_local: "page-3".into(),
            }
        );

        ui.modal = Some(Modal::LinkJump {
            link_id: lk(1),
            label: "Linked page no longer exists".into(),
            target_live: false,
            notebook_local: None,
            page_local: None,
        });
        let remove = dialog_button(&g, ui.modal.as_ref().unwrap(), "REMOVE LINK");
        assert_eq!(
            ui.on_tap(&g, remove, &[], "N", None),
            UiCommand::RemoveLink { link_id: lk(1) }
        );

        ui.modal = Some(Modal::LinkJump {
            link_id: lk(1),
            label: "Linked page no longer exists".into(),
            target_live: false,
            notebook_local: None,
            page_local: None,
        });
        assert_eq!(
            ui.on_tap(&g, UiPoint { x: 10, y: 10 }, &[], "N", None),
            UiCommand::None
        );
        assert!(ui.modal.is_none());
    }

    #[test]
    fn broken_destination_is_not_page_zero_and_a_dead_target_is_not_written() {
        use crate::ink::InkRequest;
        use crate::library::open_or_create_library;
        use crate::notebook::{
            delete_page_durable, insert_page_after_durable, DeletePageHooks, InsertPageHooks,
        };
        use crate::store::{load_page_links, page_dir};
        use crate::sync_local::{persist_planned_identity, plan_sync_identity, read_identity};
        use crate::sync_outbound::author_add_page_link;
        use sync_protocol::SyncPageLink;

        let root = std::env::temp_dir().join(format!(
            "scribe-link-live-{}-{}",
            std::process::id(),
            "dest"
        ));
        let _ = fs::remove_dir_all(&root);
        let g = panel_geo();
        let bg = vec![0xFFu8; (g.xres as usize) * (g.yres as usize)];
        let mut opened = open_or_create_library(&root, InkRequest::Default, &g, &bg).unwrap();
        let nb_local = opened.repository.manifest().active_notebook_id.clone();
        insert_page_after_durable(
            &opened.notebook_dir,
            &mut opened.notebook.manifest,
            "page-1",
            &InsertPageHooks::default(),
        )
        .unwrap();
        let (from, plan) = plan_sync_identity(opened.repository.root()).unwrap();
        persist_planned_identity(opened.repository.root(), from.as_ref(), &plan).unwrap();
        let identity = read_identity(opened.repository.root()).unwrap().unwrap();
        let nb = identity.notebook_id(&nb_local).unwrap();
        let page2 = identity.page_id(&nb_local, "page-2").unwrap();
        let live = resolve_live_target(&opened.repository, &identity, nb, page2).unwrap();
        assert_eq!(live.ordinal, 2);
        assert_eq!(live.page_local, "page-2");
        let (label, enabled) = link_jump_presentation(Some(&live));
        assert!(enabled);
        assert!(label.contains("Page 2"));
        assert!(!label.contains("Page 0"));

        delete_page_durable(
            &opened.notebook_dir,
            &mut opened.notebook.manifest,
            "page-2",
            &DeletePageHooks::default(),
        )
        .unwrap();
        let identity = read_identity(opened.repository.root()).unwrap().unwrap();
        assert_eq!(identity.page_id(&nb_local, "page-2"), Some(page2));
        assert!(resolve_live_target(&opened.repository, &identity, nb, page2).is_none());
        assert!(!selected_target_is_live(
            &opened.repository,
            &nb_local,
            "page-2"
        ));
        let (label, enabled) = link_jump_presentation(None);
        assert!(!enabled);
        assert_eq!(label, MISSING_LINK_LABEL);
        assert!(!label.contains("Page 0"));

        let source = identity.page_id(&nb_local, "page-1").unwrap();
        let link = SyncPageLink {
            link_id: sync_engine::testkit::lk(9),
            source_notebook_id: nb,
            source_page_id: source,
            source_x: 100.0,
            source_y: 200.0,
            target_notebook_id: nb,
            target_page_id: page2,
        };
        let dir = page_dir(&opened.notebook_dir, "page-1");
        let saved = author_add_page_link(None, &nb_local, "page-1", &dir, &link, || {
            selected_target_is_live(&opened.repository, &nb_local, "page-2")
        })
        .unwrap();
        assert!(saved.is_none());
        assert!(load_page_links(&dir).unwrap().is_empty());
        assert!(!dir.join("links.jsonl").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
