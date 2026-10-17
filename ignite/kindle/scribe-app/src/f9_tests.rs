//! Gate F9 host tests: durable page ordering + insert-after.

#[cfg(test)]
mod tests {
    use crate::notebook::{
        allocate_page_id, arm_dev_notebook_op, commit_new_page_to_manifest,
        consume_dev_notebook_op, create_page_files_from_template, delete_page_durable,
        insert_page_after_durable, list_orphan_page_dirs_for_test, move_page_durable,
        open_or_create_notebook, peek_dev_notebook_op, plan_insert_after, plan_move_page,
        validate_manifest, DevNotebookOp, InsertPageHooks, ManifestWriteHooks, MovePageDirection,
        MovePageHooks, PAGE_TRASH_DIR,
    };
    use crate::page_label::format_label_for_page;
    use crate::store::{page_dir, page_exists, PageStore};
    use kindle_platform::FbGeometry;
    use notebook_core::{Point, Stroke, StrokeId};
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scribe-f9-{n}"));
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
        crate::notebook::persist_active_page(&root, &mut man, "page-1").unwrap();
        (root, man)
    }

    fn read_manifest(root: &Path) -> crate::notebook::PersistedNotebookManifestV1 {
        let raw = fs::read_to_string(root.join("notebook.meta")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn stroke_ids_on(root: &Path, page_id: &str) -> Vec<String> {
        let (_s, loaded) = crate::store::load_existing_page(root, page_id).unwrap();
        loaded
            .strokes
            .iter()
            .map(|st| st.id.as_str().to_string())
            .collect()
    }

    fn page_file_hashes(root: &Path, page_id: &str) -> HashMap<&'static str, u64> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let dir = page_dir(root, page_id);
        let mut out = HashMap::new();
        for name in [
            "page.meta",
            "background.raw",
            "strokes.jsonl",
            "edits.jsonl",
        ] {
            let bytes = fs::read(dir.join(name)).unwrap();
            let mut h = DefaultHasher::new();
            bytes.hash(&mut h);
            out.insert(name, h.finish());
        }
        out
    }

    #[test]
    fn move_b_right_then_left_preserves_identity_and_hud() {
        let (root, mut man) = notebook_with_pages(4);
        crate::notebook::persist_active_page(&root, &mut man, "page-2").unwrap();
        assert_eq!(format_label_for_page(&man, "page-2").unwrap(), "2 of 4");

        let before: HashMap<_, _> = ["page-1", "page-2", "page-3", "page-4"]
            .iter()
            .map(|id| (*id, page_file_hashes(&root, id)))
            .collect();

        let c = move_page_durable(
            &root,
            &mut man,
            "page-2",
            MovePageDirection::Right,
            &MovePageHooks::default(),
        )
        .unwrap();
        assert_eq!(c.from_index, 1);
        assert_eq!(c.to_index, 2);
        assert_eq!(man.page_ids, vec!["page-1", "page-3", "page-2", "page-4"]);
        assert_eq!(man.active_page_id, "page-2");
        assert_eq!(format_label_for_page(&man, "page-2").unwrap(), "3 of 4");

        let c2 = move_page_durable(
            &root,
            &mut man,
            "page-2",
            MovePageDirection::Left,
            &MovePageHooks::default(),
        )
        .unwrap();
        assert_eq!(c2.to_index, 1);
        assert_eq!(man.page_ids, vec!["page-1", "page-2", "page-3", "page-4"]);
        assert_eq!(man.active_page_id, "page-2");
        assert_eq!(format_label_for_page(&man, "page-2").unwrap(), "2 of 4");

        for id in ["page-1", "page-2", "page-3", "page-4"] {
            assert_eq!(
                page_file_hashes(&root, id),
                before[id],
                "page files mutated for {id}"
            );
        }
        assert_eq!(stroke_ids_on(&root, "page-2"), vec!["s-page-2".to_string()]);
    }

    #[test]
    fn boundary_move_refused_manifest_unchanged() {
        let (root, mut man) = notebook_with_pages(3);
        crate::notebook::persist_active_page(&root, &mut man, "page-1").unwrap();
        let before = fs::read(root.join("notebook.meta")).unwrap();
        let err = move_page_durable(
            &root,
            &mut man,
            "page-1",
            MovePageDirection::Left,
            &MovePageHooks::default(),
        )
        .unwrap_err();
        assert!(err.contains("already first"), "{err}");
        assert_eq!(fs::read(root.join("notebook.meta")).unwrap(), before);
        assert_eq!(man.page_ids, vec!["page-1", "page-2", "page-3"]);

        crate::notebook::persist_active_page(&root, &mut man, "page-3").unwrap();
        let before = fs::read(root.join("notebook.meta")).unwrap();
        let err = move_page_durable(
            &root,
            &mut man,
            "page-3",
            MovePageDirection::Right,
            &MovePageHooks::default(),
        )
        .unwrap_err();
        assert!(err.contains("already last"), "{err}");
        assert_eq!(fs::read(root.join("notebook.meta")).unwrap(), before);
    }

    #[test]
    fn move_survives_restart() {
        let (root, mut man) = notebook_with_pages(4);
        crate::notebook::persist_active_page(&root, &mut man, "page-2").unwrap();
        move_page_durable(
            &root,
            &mut man,
            "page-2",
            MovePageDirection::Right,
            &MovePageHooks::default(),
        )
        .unwrap();
        let disk = read_manifest(&root);
        assert_eq!(disk.page_ids, vec!["page-1", "page-3", "page-2", "page-4"]);
        assert_eq!(disk.active_page_id, "page-2");
        let open =
            open_or_create_notebook(&root, crate::ink::InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open.manifest, disk);
        assert_eq!(open.loaded.header.page_id, "page-2");
        assert_eq!(
            format_label_for_page(&open.manifest, "page-2").unwrap(),
            "3 of 4"
        );
        assert_eq!(stroke_ids_on(&root, "page-1"), vec!["s-page-1".to_string()]);
        assert_eq!(stroke_ids_on(&root, "page-3"), vec!["s-page-3".to_string()]);
        assert_eq!(stroke_ids_on(&root, "page-2"), vec!["s-page-2".to_string()]);
        assert_eq!(stroke_ids_on(&root, "page-4"), vec!["s-page-4".to_string()]);
    }

    #[test]
    fn injected_move_manifest_failures_leave_old_state() {
        for (label, hooks) in [
            (
                "write",
                MovePageHooks {
                    manifest: ManifestWriteHooks {
                        fail_write: true,
                        ..Default::default()
                    },
                },
            ),
            (
                "fsync",
                MovePageHooks {
                    manifest: ManifestWriteHooks {
                        fail_fsync: true,
                        ..Default::default()
                    },
                },
            ),
            (
                "rename",
                MovePageHooks {
                    manifest: ManifestWriteHooks {
                        fail_rename: true,
                        ..Default::default()
                    },
                },
            ),
        ] {
            let (root, mut man) = notebook_with_pages(3);
            crate::notebook::persist_active_page(&root, &mut man, "page-2").unwrap();
            let before = fs::read(root.join("notebook.meta")).unwrap();
            let before_ids = man.page_ids.clone();
            let err =
                move_page_durable(&root, &mut man, "page-2", MovePageDirection::Right, &hooks)
                    .unwrap_err();
            assert!(err.contains("manifest persist failed"), "{label}: {err}");
            assert_eq!(fs::read(root.join("notebook.meta")).unwrap(), before);
            assert_eq!(man.page_ids, before_ids);
            assert_eq!(man.active_page_id, "page-2");
        }
    }

    #[test]
    fn post_commit_move_presentation_failure_leaves_order_authoritative() {
        let (root, mut man) = notebook_with_pages(3);
        crate::notebook::persist_active_page(&root, &mut man, "page-2").unwrap();
        move_page_durable(
            &root,
            &mut man,
            "page-2",
            MovePageDirection::Right,
            &MovePageHooks::default(),
        )
        .unwrap();
        let disk = read_manifest(&root);
        assert_eq!(disk.page_ids, vec!["page-1", "page-3", "page-2"]);
        assert_eq!(disk.active_page_id, "page-2");
        // Presentation skipped; restart still opens new order with active B.
        let open =
            open_or_create_notebook(&root, crate::ink::InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open.manifest, disk);
        assert_eq!(open.loaded.header.page_id, "page-2");
    }

    #[test]
    fn insert_after_middle_allocates_monotonic_id() {
        let (root, mut man) = notebook_with_pages(3);
        assert_eq!(man.next_page_ordinal, 4);
        crate::notebook::persist_active_page(&root, &mut man, "page-2").unwrap();
        let before_a = page_file_hashes(&root, "page-1");
        let before_b = page_file_hashes(&root, "page-2");
        let before_c = page_file_hashes(&root, "page-3");

        let commit =
            insert_page_after_durable(&root, &mut man, "page-2", &InsertPageHooks::default())
                .unwrap();
        assert_eq!(commit.inserted_page_id, "page-4");
        assert_eq!(commit.insert_index, 2);
        assert_eq!(man.page_ids, vec!["page-1", "page-2", "page-4", "page-3"]);
        assert_eq!(man.active_page_id, "page-4");
        assert_eq!(man.next_page_ordinal, 5);
        assert_eq!(format_label_for_page(&man, "page-4").unwrap(), "3 of 4");
        assert!(page_exists(&root, "page-4"));
        assert_eq!(page_file_hashes(&root, "page-1"), before_a);
        assert_eq!(page_file_hashes(&root, "page-2"), before_b);
        assert_eq!(page_file_hashes(&root, "page-3"), before_c);

        let (mut store, _) = crate::store::load_existing_page(&root, "page-4").unwrap();
        store
            .append_committed_stroke(&stroke("s-inserted"))
            .unwrap();
        let open =
            open_or_create_notebook(&root, crate::ink::InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open.manifest.active_page_id, "page-4");
        assert_eq!(
            stroke_ids_on(&root, "page-4"),
            vec!["s-inserted".to_string()]
        );
    }

    #[test]
    fn insert_after_final_page() {
        let (root, mut man) = notebook_with_pages(3);
        crate::notebook::persist_active_page(&root, &mut man, "page-3").unwrap();
        let commit =
            insert_page_after_durable(&root, &mut man, "page-3", &InsertPageHooks::default())
                .unwrap();
        assert_eq!(commit.inserted_page_id, "page-4");
        assert_eq!(man.page_ids, vec!["page-1", "page-2", "page-3", "page-4"]);
        assert_eq!(man.active_page_id, "page-4");
        assert_eq!(format_label_for_page(&man, "page-4").unwrap(), "4 of 4");
    }

    #[test]
    fn insert_manifest_failure_cleans_up_or_leaves_orphan() {
        for (label, hooks) in [
            (
                "write",
                InsertPageHooks {
                    manifest: ManifestWriteHooks {
                        fail_write: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            (
                "fsync",
                InsertPageHooks {
                    manifest: ManifestWriteHooks {
                        fail_fsync: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            (
                "rename",
                InsertPageHooks {
                    manifest: ManifestWriteHooks {
                        fail_rename: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
        ] {
            let (root, mut man) = notebook_with_pages(2);
            crate::notebook::persist_active_page(&root, &mut man, "page-1").unwrap();
            let before = fs::read(root.join("notebook.meta")).unwrap();
            let err = insert_page_after_durable(&root, &mut man, "page-1", &hooks).unwrap_err();
            assert!(err.contains("manifest persist failed"), "{label}: {err}");
            assert!(err.contains("cleanup="), "{label}: {err}");
            assert_eq!(fs::read(root.join("notebook.meta")).unwrap(), before);
            assert_eq!(man.page_ids, vec!["page-1", "page-2"]);
            assert!(!page_exists(&root, "page-3"));
            let open =
                open_or_create_notebook(&root, crate::ink::InkRequest::Default, &geo(), &bg())
                    .unwrap();
            assert_eq!(open.manifest.page_ids, vec!["page-1", "page-2"]);
        }
    }

    #[test]
    fn insert_manifest_failure_injected_cleanup_leaves_orphan_not_navigable() {
        let (root, mut man) = notebook_with_pages(2);
        let hooks = InsertPageHooks {
            fail_cleanup: true,
            manifest: ManifestWriteHooks {
                fail_rename: true,
                ..Default::default()
            },
        };
        let err = insert_page_after_durable(&root, &mut man, "page-1", &hooks).unwrap_err();
        assert!(
            err.contains("LeftAsOrphan") || err.contains("cleanup="),
            "{err}"
        );
        assert_eq!(man.page_ids, vec!["page-1", "page-2"]);
        // Injected cleanup skip leaves the created dir; may exist as orphan.
        if page_exists(&root, "page-3") {
            let orphans = list_orphan_page_dirs_for_test(&root, &man.page_ids);
            assert!(orphans.iter().any(|o| o == "page-3"));
        }
        let open =
            open_or_create_notebook(&root, crate::ink::InkRequest::Default, &geo(), &bg()).unwrap();
        assert!(!open.manifest.page_ids.contains(&"page-3".into()));
        assert_eq!(open.manifest.active_page_id, "page-1");
    }

    #[test]
    fn post_commit_insert_survives_presentation_skip() {
        let (root, mut man) = notebook_with_pages(2);
        crate::notebook::persist_active_page(&root, &mut man, "page-1").unwrap();
        let commit =
            insert_page_after_durable(&root, &mut man, "page-1", &InsertPageHooks::default())
                .unwrap();
        let disk = read_manifest(&root);
        assert_eq!(disk.active_page_id, commit.inserted_page_id);
        assert!(disk.page_ids.contains(&commit.inserted_page_id));
        let open =
            open_or_create_notebook(&root, crate::ink::InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open.manifest, disk);
        assert_eq!(open.loaded.header.page_id, commit.inserted_page_id);
    }

    #[test]
    fn compose_move_delete_insert() {
        let (root, mut man) = notebook_with_pages(4);
        // [A,B,C,D] move C left → [A,C,B,D]
        crate::notebook::persist_active_page(&root, &mut man, "page-3").unwrap();
        move_page_durable(
            &root,
            &mut man,
            "page-3",
            MovePageDirection::Left,
            &MovePageHooks::default(),
        )
        .unwrap();
        assert_eq!(man.page_ids, vec!["page-1", "page-3", "page-2", "page-4"]);
        // delete B → [A,C,D]
        delete_page_durable(
            &root,
            &mut man,
            "page-2",
            &crate::notebook::DeletePageHooks::default(),
        )
        .unwrap();
        assert_eq!(man.page_ids, vec!["page-1", "page-3", "page-4"]);
        assert!(root.join(PAGE_TRASH_DIR).is_dir());
        // insert after C → [A,C,N,D]
        crate::notebook::persist_active_page(&root, &mut man, "page-3").unwrap();
        let ins = insert_page_after_durable(&root, &mut man, "page-3", &InsertPageHooks::default())
            .unwrap();
        assert_eq!(
            man.page_ids,
            vec![
                "page-1".to_string(),
                "page-3".to_string(),
                ins.inserted_page_id.clone(),
                "page-4".to_string()
            ]
        );
        assert_eq!(man.active_page_id, ins.inserted_page_id);
        assert_eq!(
            format_label_for_page(&man, &ins.inserted_page_id).unwrap(),
            "3 of 4"
        );
        let open =
            open_or_create_notebook(&root, crate::ink::InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open.manifest.page_ids, man.page_ids);
        assert!(!open.manifest.page_ids.contains(&"page-2".into()));
    }

    #[test]
    fn f7_content_preserved_across_reorder() {
        // Unique stroke content on B; move left/right; stroke id + journal bytes unchanged.
        let (root, mut man) = notebook_with_pages(3);
        crate::notebook::persist_active_page(&root, &mut man, "page-2").unwrap();
        let before = page_file_hashes(&root, "page-2");
        let strokes_before = stroke_ids_on(&root, "page-2");
        move_page_durable(
            &root,
            &mut man,
            "page-2",
            MovePageDirection::Right,
            &MovePageHooks::default(),
        )
        .unwrap();
        move_page_durable(
            &root,
            &mut man,
            "page-2",
            MovePageDirection::Left,
            &MovePageHooks::default(),
        )
        .unwrap();
        assert_eq!(page_file_hashes(&root, "page-2"), before);
        assert_eq!(stroke_ids_on(&root, "page-2"), strokes_before);
    }

    #[test]
    fn trigger_priority_and_busy_pending() {
        let root = tmp();
        arm_dev_notebook_op(&root, DevNotebookOp::DeleteCurrent).unwrap();
        arm_dev_notebook_op(&root, DevNotebookOp::InsertAfter).unwrap();
        arm_dev_notebook_op(&root, DevNotebookOp::MoveRight).unwrap();
        arm_dev_notebook_op(&root, DevNotebookOp::MoveLeft).unwrap();
        // Highest priority is MoveLeft.
        assert_eq!(peek_dev_notebook_op(&root), Some(DevNotebookOp::MoveLeft));
        // Busy path: peek only, do not consume.
        assert_eq!(peek_dev_notebook_op(&root), Some(DevNotebookOp::MoveLeft));
        consume_dev_notebook_op(&root, DevNotebookOp::MoveLeft);
        assert_eq!(peek_dev_notebook_op(&root), Some(DevNotebookOp::MoveRight));
        consume_dev_notebook_op(&root, DevNotebookOp::MoveRight);
        assert_eq!(
            peek_dev_notebook_op(&root),
            Some(DevNotebookOp::InsertAfter)
        );
        consume_dev_notebook_op(&root, DevNotebookOp::InsertAfter);
        assert_eq!(
            peek_dev_notebook_op(&root),
            Some(DevNotebookOp::DeleteCurrent)
        );
        consume_dev_notebook_op(&root, DevNotebookOp::DeleteCurrent);
        assert_eq!(peek_dev_notebook_op(&root), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plan_helpers_validate() {
        let (_root, man) = notebook_with_pages(3);
        let (planned, from, to) = plan_move_page(&man, "page-2", MovePageDirection::Right).unwrap();
        assert_eq!((from, to), (1, 2));
        assert_eq!(planned.page_ids, vec!["page-1", "page-3", "page-2"]);
        validate_manifest(&planned).unwrap();
        let (planned, id, at) = plan_insert_after(&man, "page-2").unwrap();
        assert_eq!(id, "page-4");
        assert_eq!(at, 2);
        assert_eq!(
            planned.page_ids,
            vec!["page-1", "page-2", "page-4", "page-3"]
        );
        assert_eq!(planned.active_page_id, "page-4");
    }
}
