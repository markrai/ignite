//! Gate F8 durable page deletion (host).

#[cfg(test)]
mod tests {
    use crate::notebook::{
        allocate_page_id, arm_delete_trigger, commit_new_page_to_manifest, consume_delete_trigger,
        create_page_files_from_template, delete_page_durable, delete_trigger_pending,
        list_orphan_page_dirs_for_test, open_or_create_notebook, plan_delete_page,
        successor_after_delete, validate_manifest, DeletePageHooks, ManifestWriteHooks,
        QuarantineOutcome, PAGE_TRASH_DIR,
    };
    use crate::page_label::format_label_for_page;
    use crate::store::{page_dir, page_exists, PageStore};
    use kindle_platform::FbGeometry;
    use notebook_core::{Point, Stroke, StrokeId};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scribe-f8-{n}"));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn geo() -> FbGeometry {
        FbGeometry {
            xres: 40,
            yres: 20,
            xres_virtual: 40,
            yres_virtual: 20,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 40,
            smem_len: 800,
        }
    }

    fn bg() -> Vec<u8> {
        vec![0xAAu8; 40 * 20]
    }

    fn stroke(id: &str) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: vec![Point::new(10.0, 20.0), Point::new(30.0, 40.0)],
            brush: None,
            finished: true,
        }
    }

    /// Fresh notebook with N pages (page-1..page-N), each with a distinct stroke.
    fn notebook_with_pages(n: usize) -> (PathBuf, crate::notebook::PersistedNotebookManifestV1) {
        assert!(n >= 1);
        let root = tmp();
        let g = geo();
        let open =
            open_or_create_notebook(&root, crate::ink::InkRequest::Default, &g, &bg()).unwrap();
        let mut man = open.manifest;
        let mut store = open.store;
        store.append_committed_stroke(&stroke("s-page-1")).unwrap();
        for i in 2..=n {
            let id = allocate_page_id(&man);
            let (mut s, _) = create_page_files_from_template(&root, &id, "page-1").unwrap();
            s.append_committed_stroke(&stroke(&format!("s-page-{i}")))
                .unwrap();
            commit_new_page_to_manifest(&root, &mut man, &id).unwrap();
        }
        // Leave active on page-1 unless caller changes it.
        crate::notebook::persist_active_page(&root, &mut man, "page-1").unwrap();
        (root, man)
    }

    fn read_manifest(root: &std::path::Path) -> crate::notebook::PersistedNotebookManifestV1 {
        let raw = fs::read_to_string(root.join("notebook.meta")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn stroke_ids_on(root: &std::path::Path, page_id: &str) -> Vec<String> {
        let (_s, loaded) = crate::store::load_existing_page(root, page_id).unwrap();
        loaded
            .strokes
            .iter()
            .map(|st| st.id.as_str().to_string())
            .collect()
    }

    #[test]
    fn single_page_delete_refused_byte_identical() {
        let (root, mut man) = notebook_with_pages(1);
        let before_meta = fs::read(root.join("notebook.meta")).unwrap();
        let before_page = fs::read(page_dir(&root, "page-1").join("strokes.jsonl")).unwrap();
        let err = delete_page_durable(&root, &mut man, "page-1", &DeletePageHooks::default())
            .unwrap_err();
        assert!(
            err.contains("REFUSE PAGE DELETE") && err.contains("only page"),
            "{err}"
        );
        assert_eq!(fs::read(root.join("notebook.meta")).unwrap(), before_meta);
        assert_eq!(
            fs::read(page_dir(&root, "page-1").join("strokes.jsonl")).unwrap(),
            before_page
        );
        assert_eq!(man.active_page_id, "page-1");
        assert!(
            !root.join(PAGE_TRASH_DIR).exists()
                || fs::read_dir(root.join(PAGE_TRASH_DIR))
                    .map(|d| d.count() == 0)
                    .unwrap_or(true)
        );
        assert!(page_exists(&root, "page-1"));
    }

    #[test]
    fn delete_middle_page_b() {
        let (root, mut man) = notebook_with_pages(4);
        // A B C D — activate B
        crate::notebook::persist_active_page(&root, &mut man, "page-2").unwrap();
        assert_eq!(format_label_for_page(&man, "page-2").unwrap(), "2 of 4");
        let commit =
            delete_page_durable(&root, &mut man, "page-2", &DeletePageHooks::default()).unwrap();
        assert_eq!(commit.successor_page_id, "page-3");
        assert_eq!(man.page_ids, vec!["page-1", "page-3", "page-4"]);
        assert_eq!(man.active_page_id, "page-3");
        assert!(!page_exists(&root, "page-2"));
        match &commit.quarantine {
            QuarantineOutcome::Moved { trash_entry_name } => {
                assert!(root.join(PAGE_TRASH_DIR).join(trash_entry_name).is_dir());
            }
            other => panic!("expected Moved, got {other:?}"),
        }
        assert_eq!(format_label_for_page(&man, "page-3").unwrap(), "2 of 3");
        assert_eq!(stroke_ids_on(&root, "page-1"), vec!["s-page-1".to_string()]);
        assert_eq!(stroke_ids_on(&root, "page-3"), vec!["s-page-3".to_string()]);
        assert_eq!(stroke_ids_on(&root, "page-4"), vec!["s-page-4".to_string()]);
        // Restart: reopen
        let open =
            open_or_create_notebook(&root, crate::ink::InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open.manifest.page_ids, vec!["page-1", "page-3", "page-4"]);
        assert_eq!(open.manifest.active_page_id, "page-3");
        assert_eq!(
            format_label_for_page(&open.manifest, "page-3").unwrap(),
            "2 of 3"
        );
        assert!(!open.orphan_dirs.iter().any(|o| o.contains("page-2")));
    }

    #[test]
    fn delete_first_and_last() {
        let (root, mut man) = notebook_with_pages(3);
        // delete A
        crate::notebook::persist_active_page(&root, &mut man, "page-1").unwrap();
        let c =
            delete_page_durable(&root, &mut man, "page-1", &DeletePageHooks::default()).unwrap();
        assert_eq!(c.successor_page_id, "page-2");
        assert_eq!(man.page_ids, vec!["page-2", "page-3"]);
        assert_eq!(format_label_for_page(&man, "page-2").unwrap(), "1 of 2");

        // delete last (page-3)
        crate::notebook::persist_active_page(&root, &mut man, "page-3").unwrap();
        let c2 =
            delete_page_durable(&root, &mut man, "page-3", &DeletePageHooks::default()).unwrap();
        assert_eq!(c2.successor_page_id, "page-2");
        assert_eq!(man.page_ids, vec!["page-2".to_string()]);
        assert_eq!(format_label_for_page(&man, "page-2").unwrap(), "1");
    }

    #[test]
    fn allocate_after_delete_never_reuses_trashed_id() {
        let (root, mut man) = notebook_with_pages(3);
        // pages 1,2,3 — ordinal is 4
        assert_eq!(allocate_page_id(&man), "page-4");
        delete_page_durable(&root, &mut man, "page-2", &DeletePageHooks::default()).unwrap();
        assert_eq!(allocate_page_id(&man), "page-4");
        let id = allocate_page_id(&man);
        create_page_files_from_template(&root, &id, "page-1").unwrap();
        commit_new_page_to_manifest(&root, &mut man, &id).unwrap();
        assert_eq!(id, "page-4");
        assert_eq!(man.page_ids, vec!["page-1", "page-3", "page-4"]);
        assert!(!page_exists(&root, "page-2"));
    }

    #[test]
    fn injected_manifest_write_failure_leaves_page_dir_and_old_manifest() {
        let (root, mut man) = notebook_with_pages(2);
        let before = fs::read(root.join("notebook.meta")).unwrap();
        let hooks = DeletePageHooks {
            manifest: ManifestWriteHooks {
                fail_write: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let err = delete_page_durable(&root, &mut man, "page-1", &hooks).unwrap_err();
        assert!(err.contains("manifest persist failed"), "{err}");
        assert!(err.contains("page dir untouched"), "{err}");
        assert_eq!(fs::read(root.join("notebook.meta")).unwrap(), before);
        assert!(page_exists(&root, "page-1"));
        assert_eq!(man.page_ids, vec!["page-1", "page-2"]);
        assert!(man.page_ids.contains(&"page-1".into()));
    }

    #[test]
    fn injected_manifest_fsync_and_rename_failures_leave_old_state() {
        for (label, hooks) in [
            (
                "fsync",
                DeletePageHooks {
                    manifest: ManifestWriteHooks {
                        fail_fsync: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            (
                "rename",
                DeletePageHooks {
                    manifest: ManifestWriteHooks {
                        fail_rename: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
        ] {
            let (root, mut man) = notebook_with_pages(2);
            let before = fs::read(root.join("notebook.meta")).unwrap();
            let err = delete_page_durable(&root, &mut man, "page-2", &hooks).unwrap_err();
            assert!(err.contains("manifest persist failed"), "{label}: {err}");
            assert_eq!(fs::read(root.join("notebook.meta")).unwrap(), before);
            assert!(page_exists(&root, "page-1") && page_exists(&root, "page-2"));
            assert_eq!(man.page_ids, vec!["page-1", "page-2"]);
        }
    }

    #[test]
    fn quarantine_failure_after_manifest_commit_leaves_orphan_not_navigable() {
        let (root, mut man) = notebook_with_pages(3);
        crate::notebook::persist_active_page(&root, &mut man, "page-2").unwrap();
        let hooks = DeletePageHooks {
            fail_quarantine: true,
            ..Default::default()
        };
        let commit = delete_page_durable(&root, &mut man, "page-2", &hooks).unwrap();
        assert_eq!(commit.successor_page_id, "page-3");
        assert_eq!(man.page_ids, vec!["page-1", "page-3"]);
        assert!(!man.page_ids.contains(&"page-2".into()));
        assert!(matches!(
            commit.quarantine,
            QuarantineOutcome::LeftAsOrphan { .. }
        ));
        // Source still present as unreferenced orphan.
        assert!(page_exists(&root, "page-2"));
        let orphans = list_orphan_page_dirs_for_test(&root, &man.page_ids);
        assert!(orphans.iter().any(|o| o == "page-2"));
        let open =
            open_or_create_notebook(&root, crate::ink::InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open.manifest.page_ids, vec!["page-1", "page-3"]);
        assert_eq!(open.manifest.active_page_id, "page-3");
        assert!(!open.manifest.page_ids.contains(&"page-2".into()));
    }

    #[test]
    fn durable_delete_survives_post_commit_presentation_failure_simulation() {
        // Durable commit succeeded; "presentation" never ran — restart must still
        // open the post-delete successor and must not resurrect the deleted page.
        let (root, mut man) = notebook_with_pages(3);
        crate::notebook::persist_active_page(&root, &mut man, "page-2").unwrap();
        let commit =
            delete_page_durable(&root, &mut man, "page-2", &DeletePageHooks::default()).unwrap();
        assert!(!man.page_ids.contains(&"page-2".into()));
        let disk = read_manifest(&root);
        assert_eq!(disk.active_page_id, commit.successor_page_id);
        assert!(!disk.page_ids.contains(&"page-2".into()));
        let open =
            open_or_create_notebook(&root, crate::ink::InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open.manifest, disk);
        assert_eq!(open.loaded.header.page_id, "page-3");
    }

    #[test]
    fn post_commit_successor_load_failure_leaves_deletion_authoritative() {
        use crate::notebook::load_page_for_switch;
        use crate::profile::RenderProfile;

        let (root, mut man) = notebook_with_pages(3);
        crate::notebook::persist_active_page(&root, &mut man, "page-2").unwrap();
        let commit =
            delete_page_durable(&root, &mut man, "page-2", &DeletePageHooks::default()).unwrap();
        assert_eq!(commit.successor_page_id, "page-3");
        assert!(!man.page_ids.contains(&"page-2".into()));

        // Corrupt successor so presentation/load fails after commit.
        let meta = page_dir(&root, "page-3").join("page.meta");
        let backup = fs::read(&meta).unwrap();
        fs::write(&meta, b"{not-json\n").unwrap();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        match load_page_for_switch(&root, "page-3", &profile, &geo()) {
            Ok(_) => panic!("expected successor load failure after meta corruption"),
            Err(err) => assert!(!err.is_empty(), "{err}"),
        }

        // Logical deletion must not roll back.
        let disk = read_manifest(&root);
        assert_eq!(disk.page_ids, vec!["page-1", "page-3"]);
        assert!(!disk.page_ids.contains(&"page-2".into()));
        assert_eq!(disk.active_page_id, "page-3");
        assert!(!page_exists(&root, "page-2"));

        // Repair successor; restart opens post-delete notebook.
        fs::write(&meta, backup).unwrap();
        let open =
            open_or_create_notebook(&root, crate::ink::InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open.manifest.page_ids, vec!["page-1", "page-3"]);
        assert_eq!(open.manifest.active_page_id, "page-3");
        assert_eq!(open.loaded.header.page_id, "page-3");
    }

    #[test]
    fn post_commit_reconstruct_failure_contract_leaves_deletion_authoritative() {
        // Host stand-in for "successor reconstruct failed" after durable commit:
        // deletion remains on disk; deleted page is not restored; restart resolves
        // to the post-delete successor (runtime fails closed without rollback).
        let (root, mut man) = notebook_with_pages(3);
        crate::notebook::persist_active_page(&root, &mut man, "page-2").unwrap();
        delete_page_durable(&root, &mut man, "page-2", &DeletePageHooks::default()).unwrap();
        let disk = read_manifest(&root);
        assert_eq!(disk.page_ids, vec!["page-1", "page-3"]);
        assert!(!disk.page_ids.contains(&"page-2".into()));
        assert_eq!(disk.active_page_id, "page-3");
        // Simulate fail-closed: no code path rewrites page-2 into the manifest.
        assert_eq!(man, disk);
        let open =
            open_or_create_notebook(&root, crate::ink::InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open.manifest, disk);
        assert_eq!(open.loaded.header.page_id, "page-3");
    }

    #[test]
    fn delete_trigger_stays_pending_while_busy_then_consumes_once_when_idle() {
        use crate::eraser::EraserGestureState;
        use crate::nav::{NavDownAction, NavGestureState};

        let root = tmp();
        arm_delete_trigger(&root).unwrap();

        // Mirror runtime short-circuit: ineligible → pending check never runs / no consume.
        let mut nav = NavGestureState::Idle;
        nav.on_down(NavDownAction::UndoLastStroke);
        let eraser = EraserGestureState::Idle;
        let ctrl_idle = false;
        let ctrl_armed = false;
        let delete_eligible = !nav.is_active() && !eraser.is_active() && ctrl_idle && !ctrl_armed;
        assert!(!delete_eligible);
        if delete_eligible && delete_trigger_pending(&root) {
            consume_delete_trigger(&root);
        }
        assert!(
            delete_trigger_pending(&root),
            "busy must leave trigger present"
        );

        // Later idle → consume exactly once.
        nav.on_up();
        let ctrl_idle = true;
        let delete_eligible = !nav.is_active() && !eraser.is_active() && ctrl_idle && !ctrl_armed;
        assert!(delete_eligible);
        if delete_eligible && delete_trigger_pending(&root) {
            consume_delete_trigger(&root);
        }
        assert!(!delete_trigger_pending(&root));
        // One trigger → one consume; second pass is a no-op.
        if delete_eligible && delete_trigger_pending(&root) {
            consume_delete_trigger(&root);
        }
        assert!(!delete_trigger_pending(&root));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delete_trigger_stays_pending_until_consumed_once() {
        let root = tmp();
        assert!(!delete_trigger_pending(&root));
        arm_delete_trigger(&root).unwrap();
        assert!(delete_trigger_pending(&root));
        // Busy path must not consume: existence check only.
        assert!(delete_trigger_pending(&root));
        assert!(delete_trigger_pending(&root));
        consume_delete_trigger(&root);
        assert!(!delete_trigger_pending(&root));
        // Second consume is a no-op (one trigger → one delete).
        consume_delete_trigger(&root);
        assert!(!delete_trigger_pending(&root));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plan_successor_rules() {
        let (_root, man) = notebook_with_pages(4);
        assert_eq!(successor_after_delete(&man, "page-2").unwrap(), "page-3");
        assert_eq!(successor_after_delete(&man, "page-1").unwrap(), "page-2");
        assert_eq!(successor_after_delete(&man, "page-4").unwrap(), "page-3");
        let (planned, succ) = plan_delete_page(&man, "page-2").unwrap();
        assert_eq!(succ, "page-3");
        assert_eq!(planned.page_ids, vec!["page-1", "page-3", "page-4"]);
        validate_manifest(&planned).unwrap();
    }

    #[test]
    fn trash_excluded_from_orphan_scan() {
        let (root, mut man) = notebook_with_pages(2);
        delete_page_durable(&root, &mut man, "page-2", &DeletePageHooks::default()).unwrap();
        assert!(root.join(PAGE_TRASH_DIR).is_dir());
        let orphans = list_orphan_page_dirs_for_test(&root, &man.page_ids);
        assert!(!orphans.iter().any(|o| o == PAGE_TRASH_DIR));
        assert!(!orphans.iter().any(|o| o.starts_with("page-2")));
    }

    #[test]
    fn restart_manifest_matches_live_after_delete() {
        let (root, mut man) = notebook_with_pages(3);
        delete_page_durable(&root, &mut man, "page-2", &DeletePageHooks::default()).unwrap();
        let disk = read_manifest(&root);
        assert_eq!(disk, man);
        let open =
            open_or_create_notebook(&root, crate::ink::InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open.manifest, disk);
        assert_eq!(open.loaded.header.page_id, disk.active_page_id);
    }
}
