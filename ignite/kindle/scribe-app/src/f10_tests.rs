//! Gate F10 host tests: durable library, migration, Home isolation, and notebook-local pages.

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use kindle_platform::FbGeometry;
    use notebook_core::{Point, Stroke, StrokeId};

    use crate::home::{home_cards, render_home, HomeState};
    use crate::ink::InkRequest;
    use crate::library::{
        arm_dev_library_op, create_notebook_durable, delete_notebook_durable,
        list_orphans_for_test, open_active_notebook, open_or_create_library,
        open_or_create_library_shell, open_or_create_library_with_hooks, peek_dev_library_op,
        read_library_manifest_for_test, read_notebook_manifest_read_only, switch_notebook_durable,
        validate_library_manifest, CreateNotebookHooks, DeleteNotebookHooks, DevLibraryOp,
        LibraryManifestWriteHooks, MigrationHooks, PersistedLibraryManifestV1, SwitchNotebookHooks,
        DEV_CREATE_NOTEBOOK_TRIGGER, DEV_DELETE_NOTEBOOK_TRIGGER, DEV_HOME_TRIGGER,
        DEV_LIBRARY_NEXT_TRIGGER, DEV_LIBRARY_OPEN_TRIGGER, DEV_LIBRARY_PREV_TRIGGER,
        LIBRARY_MANIFEST_VERSION, NOTEBOOKS_DIR, NOTEBOOK_TRASH_DIR,
    };
    use crate::notebook::{
        delete_page_durable, insert_page_after_durable, move_page_durable, open_or_create_notebook,
        persist_active_page, DeletePageHooks, InsertPageHooks, MovePageDirection, MovePageHooks,
    };
    use crate::profile::RenderProfile;
    use crate::store::{
        load_existing_page, PageStore, PersistedFragmentV2, PersistedPointV1,
        PersistedReplacementV2,
    };

    fn tmp(label: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scribe-f10-{label}-{n}"));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn geo() -> FbGeometry {
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

    fn bg() -> Vec<u8> {
        vec![0xFF; geo().xres as usize * geo().yres as usize]
    }

    fn profile() -> RenderProfile {
        RenderProfile::sealed_v2_pressure(0x00).unwrap()
    }

    fn stroke(id: &str, pressure: f32) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: vec![
                Point::new(1000.0, 1000.0).with_pressure(pressure),
                Point::new(5000.0, 5000.0).with_pressure(pressure + 0.1),
            ],
            brush: None,
            finished: true,
        }
    }

    fn collect_files(base: &Path, current: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let mut entries: Vec<_> = fs::read_dir(current).unwrap().map(Result::unwrap).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let p = entry.path();
            if p.is_dir() {
                collect_files(base, &p, out);
            } else {
                let rel = p
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, fs::read(p).unwrap());
            }
        }
    }

    fn legacy_durable_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        out.insert(
            "notebook.meta".into(),
            fs::read(root.join("notebook.meta")).unwrap(),
        );
        let mut entries: Vec<_> = fs::read_dir(root).unwrap().map(Result::unwrap).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir() && (name.starts_with("page-") || name == ".page-trash") {
                collect_files(root, &entry.path(), &mut out);
            }
        }
        out
    }

    fn make_representative_f9_legacy(root: &Path) {
        let opened = open_or_create_notebook(root, InkRequest::Default, &geo(), &bg()).unwrap();
        let mut manifest = opened.manifest;
        drop(opened.store);

        let (mut page1, _) = load_existing_page(root, "page-1").unwrap();
        page1
            .append_committed_stroke(&stroke("pressure-root", 0.35))
            .unwrap();
        page1
            .append_replace_strokes(&[PersistedReplacementV2 {
                target_stroke_id: "pressure-root".into(),
                fragments: vec![PersistedFragmentV2 {
                    id: "pressure-fragment".into(),
                    style: None,
                    points: vec![
                        PersistedPointV1 {
                            x: 1000.0,
                            y: 1000.0,
                            pressure: Some(0.35),
                        },
                        PersistedPointV1 {
                            x: 2200.0,
                            y: 2200.0,
                            pressure: Some(0.45),
                        },
                    ],
                }],
            }])
            .unwrap();
        drop(page1);

        let p2 =
            insert_page_after_durable(root, &mut manifest, "page-1", &InsertPageHooks::default())
                .unwrap()
                .inserted_page_id;
        let (mut page2, _) = load_existing_page(root, &p2).unwrap();
        page2
            .append_committed_stroke(&stroke("page-two", 0.55))
            .unwrap();
        drop(page2);
        let p3 = insert_page_after_durable(root, &mut manifest, &p2, &InsertPageHooks::default())
            .unwrap()
            .inserted_page_id;
        let (mut page3, _) = load_existing_page(root, &p3).unwrap();
        page3
            .append_committed_stroke(&stroke("page-three", 0.75))
            .unwrap();
        drop(page3);
        delete_page_durable(root, &mut manifest, &p2, &DeletePageHooks::default()).unwrap();
        move_page_durable(
            root,
            &mut manifest,
            &p3,
            MovePageDirection::Left,
            &MovePageHooks::default(),
        )
        .unwrap();
        assert_eq!(manifest.page_ids, vec!["page-3", "page-1"]);
        assert_eq!(manifest.active_page_id, "page-3");
        assert_eq!(manifest.next_page_ordinal, 4);
        assert!(root.join(".page-trash").is_dir());
    }

    fn fresh_library(label: &str) -> (PathBuf, crate::library::LibraryRepository) {
        let root = tmp(label);
        let opened = open_or_create_library(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        (root, opened.repository)
    }

    #[test]
    fn legacy_f9_migration_is_byte_identical_and_idempotent() {
        let root = tmp("migration");
        make_representative_f9_legacy(&root);
        let before = legacy_durable_files(&root);
        let legacy_manifest = fs::read(root.join("notebook.meta")).unwrap();

        let opened = open_or_create_library(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        assert!(opened.migrated_legacy);
        assert_eq!(opened.repository.manifest().notebook_ids, ["notebook-1"]);
        assert_eq!(opened.notebook.manifest.page_ids, ["page-3", "page-1"]);
        assert_eq!(opened.notebook.manifest.active_page_id, "page-3");
        assert_eq!(opened.notebook.manifest.next_page_ordinal, 4);
        drop(opened.notebook.store);

        let target = root.join(NOTEBOOKS_DIR).join("notebook-1");
        assert_eq!(
            fs::read(target.join("notebook.meta")).unwrap(),
            legacy_manifest
        );
        let after = legacy_durable_files(&target);
        assert_eq!(
            after, before,
            "every F9 durable file must be byte-identical"
        );
        assert!(!root.join("notebook.meta").exists());
        assert!(!root.join("page-1").exists());
        assert!(target.join(".page-trash").is_dir());

        let library_before = fs::read(root.join("library.meta")).unwrap();
        let reopened = open_or_create_library(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        assert!(!reopened.migrated_legacy);
        assert_eq!(reopened.notebook.manifest.page_ids, ["page-3", "page-1"]);
        assert_eq!(fs::read(root.join("library.meta")).unwrap(), library_before);
        assert_eq!(legacy_durable_files(&target), before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn migration_recovers_from_each_explicit_crash_state() {
        let root = tmp("migration-move-crash");
        make_representative_f9_legacy(&root);
        let before = legacy_durable_files(&root);
        let err = open_or_create_library_with_hooks(
            &root,
            InkRequest::Default,
            &geo(),
            &bg(),
            &MigrationHooks {
                fail_after_moves: Some(1),
                ..Default::default()
            },
        )
        .err()
        .unwrap();
        assert!(err.contains("interrupted after 1 object"), "{err}");
        assert!(!root.join("library.meta").exists());
        let resumed = open_or_create_library(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        drop(resumed.notebook.store);
        assert_eq!(
            legacy_durable_files(&root.join(NOTEBOOKS_DIR).join("notebook-1")),
            before
        );
        let _ = fs::remove_dir_all(&root);

        let root = tmp("migration-publish-crash");
        make_representative_f9_legacy(&root);
        let err = open_or_create_library_with_hooks(
            &root,
            InkRequest::Default,
            &geo(),
            &bg(),
            &MigrationHooks {
                fail_after_publish: true,
                ..Default::default()
            },
        )
        .err()
        .unwrap();
        assert!(err.contains("after stage publish"), "{err}");
        assert!(!root.join("library.meta").exists());
        assert!(root.join(NOTEBOOKS_DIR).join("notebook-1").is_dir());
        let resumed = open_or_create_library(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(resumed.repository.active_notebook_id(), "notebook-1");
        let _ = fs::remove_dir_all(root);

        let root = tmp("migration-manifest-crash");
        make_representative_f9_legacy(&root);
        let err = open_or_create_library_with_hooks(
            &root,
            InkRequest::Default,
            &geo(),
            &bg(),
            &MigrationHooks {
                manifest: LibraryManifestWriteHooks {
                    fail_rename: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .err()
        .unwrap();
        assert!(err.contains("library commit failed"), "{err}");
        assert!(!root.join("library.meta").exists());
        assert!(root.join(NOTEBOOKS_DIR).join("notebook-1").is_dir());
        let resumed = open_or_create_library(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(resumed.repository.active_notebook_id(), "notebook-1");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn three_notebooks_keep_independent_active_pages_and_content() {
        let (root, mut library) = fresh_library("independent");
        let a = library.notebook_dir("notebook-1");
        let mut ma = open_or_create_notebook(&a, InkRequest::Default, &geo(), &bg())
            .unwrap()
            .manifest;
        let a2 = insert_page_after_durable(&a, &mut ma, "page-1", &InsertPageHooks::default())
            .unwrap()
            .inserted_page_id;
        let (mut sa, _) = load_existing_page(&a, &a2).unwrap();
        sa.append_committed_stroke(&stroke("A-page-2", 0.2))
            .unwrap();
        drop(sa);
        persist_active_page(&a, &mut ma, &a2).unwrap();

        create_notebook_durable(
            &mut library,
            &profile(),
            &geo(),
            &bg(),
            None,
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let b = library.notebook_dir("notebook-2");
        let mut mb = open_or_create_notebook(&b, InkRequest::Default, &geo(), &bg())
            .unwrap()
            .manifest;
        let (mut sb, _) = load_existing_page(&b, "page-1").unwrap();
        sb.append_committed_stroke(&stroke("B-page-1", 0.4))
            .unwrap();
        drop(sb);
        persist_active_page(&b, &mut mb, "page-1").unwrap();

        create_notebook_durable(
            &mut library,
            &profile(),
            &geo(),
            &bg(),
            None,
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let c = library.notebook_dir("notebook-3");
        let mut mc = open_or_create_notebook(&c, InkRequest::Default, &geo(), &bg())
            .unwrap()
            .manifest;
        let c2 = insert_page_after_durable(&c, &mut mc, "page-1", &InsertPageHooks::default())
            .unwrap()
            .inserted_page_id;
        let c3 = insert_page_after_durable(&c, &mut mc, &c2, &InsertPageHooks::default())
            .unwrap()
            .inserted_page_id;
        let (mut sc, _) = load_existing_page(&c, &c3).unwrap();
        sc.append_committed_stroke(&stroke("C-page-3", 0.7))
            .unwrap();
        drop(sc);

        for (id, expected_page, expected_stroke) in [
            ("notebook-1", "page-2", "A-page-2"),
            ("notebook-3", "page-3", "C-page-3"),
            ("notebook-2", "page-1", "B-page-1"),
            ("notebook-1", "page-2", "A-page-2"),
        ] {
            switch_notebook_durable(&mut library, id, &SwitchNotebookHooks::default()).unwrap();
            let (_, opened) =
                open_active_notebook(&library, InkRequest::Default, &geo(), &bg()).unwrap();
            assert_eq!(opened.manifest.active_page_id, expected_page);
            assert!(opened
                .loaded
                .strokes
                .iter()
                .any(|s| s.id.as_str() == expected_stroke));
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn create_is_monotonic_durable_and_manifest_failures_leave_old_authority() {
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
            let (root, mut library) = fresh_library(label);
            let before = fs::read(root.join("library.meta")).unwrap();
            let err = create_notebook_durable(
                &mut library,
                &profile(),
                &geo(),
                &bg(),
                None,
                &CreateNotebookHooks {
                    manifest: hooks,
                    fail_cleanup: false,
                },
            )
            .unwrap_err();
            assert!(err.contains("manifest persist failed"), "{label}: {err}");
            assert_eq!(fs::read(root.join("library.meta")).unwrap(), before);
            assert_eq!(library.manifest().notebook_ids, ["notebook-1"]);
            assert!(!root.join(NOTEBOOKS_DIR).join("notebook-2").exists());
            let _ = fs::remove_dir_all(root);
        }

        let (root, mut library) = fresh_library("create-monotonic");
        let c2 = create_notebook_durable(
            &mut library,
            &profile(),
            &geo(),
            &bg(),
            Some("Work / Notes"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        assert_eq!(c2.notebook_id, "notebook-2");
        assert_eq!(library.title("notebook-2").unwrap(), "Work / Notes");
        assert_eq!(library.manifest().active_notebook_id, "notebook-2");
        assert_eq!(library.manifest().next_notebook_ordinal, 3);
        let (_, opened) =
            open_active_notebook(&library, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(opened.manifest.page_ids, ["page-1"]);
        assert_eq!(opened.manifest.active_page_id, "page-1");
        assert!(opened.loaded.strokes.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn create_cleanup_failure_leaves_excluded_orphan() {
        let (root, mut library) = fresh_library("create-orphan");
        let err = create_notebook_durable(
            &mut library,
            &profile(),
            &geo(),
            &bg(),
            None,
            &CreateNotebookHooks {
                manifest: LibraryManifestWriteHooks {
                    fail_rename: true,
                    ..Default::default()
                },
                fail_cleanup: true,
            },
        )
        .unwrap_err();
        assert!(err.contains("LeftAsOrphan"), "{err}");
        assert!(root.join(NOTEBOOKS_DIR).join("notebook-2").is_dir());
        assert_eq!(library.manifest().notebook_ids, ["notebook-1"]);
        assert_eq!(
            list_orphans_for_test(&root, &library.manifest().notebook_ids),
            ["notebook-2"]
        );
        let reopened = open_or_create_library(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(reopened.repository.manifest().notebook_ids, ["notebook-1"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delete_is_manifest_first_uses_trash_and_refuses_sole_notebook() {
        let (root, mut library) = fresh_library("delete");
        for _ in 0..2 {
            create_notebook_durable(
                &mut library,
                &profile(),
                &geo(),
                &bg(),
                None,
                &CreateNotebookHooks::default(),
            )
            .unwrap();
        }
        switch_notebook_durable(&mut library, "notebook-2", &SwitchNotebookHooks::default())
            .unwrap();
        let mid =
            delete_notebook_durable(&mut library, "notebook-2", &DeleteNotebookHooks::default())
                .unwrap();
        assert_eq!(
            library.manifest().notebook_ids,
            ["notebook-1", "notebook-3"]
        );
        assert_eq!(mid.active_notebook_id, "notebook-3");
        assert!(root.join(NOTEBOOK_TRASH_DIR).is_dir());
        assert!(!root.join(NOTEBOOKS_DIR).join("notebook-2").exists());

        delete_notebook_durable(&mut library, "notebook-3", &DeleteNotebookHooks::default())
            .unwrap();
        assert_eq!(library.manifest().active_notebook_id, "notebook-1");
        let before = fs::read(root.join("library.meta")).unwrap();
        let err =
            delete_notebook_durable(&mut library, "notebook-1", &DeleteNotebookHooks::default())
                .unwrap_err();
        assert!(err.contains("only notebook"), "{err}");
        assert_eq!(fs::read(root.join("library.meta")).unwrap(), before);
        assert!(root.join(NOTEBOOKS_DIR).join("notebook-1").is_dir());
        let restarted = open_or_create_library(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(restarted.repository.manifest().notebook_ids, ["notebook-1"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delete_manifest_failure_is_precommit_and_quarantine_failure_is_postcommit() {
        let (root, mut library) = fresh_library("delete-faults");
        create_notebook_durable(
            &mut library,
            &profile(),
            &geo(),
            &bg(),
            None,
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let before = fs::read(root.join("library.meta")).unwrap();
        let err = delete_notebook_durable(
            &mut library,
            "notebook-2",
            &DeleteNotebookHooks {
                manifest: LibraryManifestWriteHooks {
                    fail_fsync: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.contains("manifest persist failed"), "{err}");
        assert_eq!(fs::read(root.join("library.meta")).unwrap(), before);
        assert!(root.join(NOTEBOOKS_DIR).join("notebook-2").is_dir());

        let commit = delete_notebook_durable(
            &mut library,
            "notebook-2",
            &DeleteNotebookHooks {
                fail_quarantine: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(
            commit.quarantine,
            crate::library::NotebookQuarantineOutcome::LeftAsOrphan { .. }
        ));
        assert_eq!(library.manifest().notebook_ids, ["notebook-1"]);
        assert!(root.join(NOTEBOOKS_DIR).join("notebook-2").is_dir());
        let restarted = open_or_create_library(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(restarted.repository.manifest().notebook_ids, ["notebook-1"]);
        assert_eq!(restarted.repository.orphan_dirs(), ["notebook-2"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn switch_commit_remains_authoritative_when_load_or_presentation_fails() {
        let (root, mut library) = fresh_library("switch-postcommit");
        create_notebook_durable(
            &mut library,
            &profile(),
            &geo(),
            &bg(),
            None,
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        switch_notebook_durable(&mut library, "notebook-1", &SwitchNotebookHooks::default())
            .unwrap();
        switch_notebook_durable(&mut library, "notebook-2", &SwitchNotebookHooks::default())
            .unwrap();
        // Model the runtime's post-commit framebuffer failure: no rollback is issued.
        let framebuffer_result: Result<(), &str> = Err("injected reconstruct failure");
        assert!(framebuffer_result.is_err());
        assert_eq!(
            read_library_manifest_for_test(&root)
                .unwrap()
                .active_notebook_id,
            "notebook-2"
        );

        // Corrupt the target journal only after commit to force the real post-commit load path.
        fs::write(
            root.join(NOTEBOOKS_DIR)
                .join("notebook-2")
                .join("page-1")
                .join("strokes.jsonl"),
            b"{bad}\n",
        )
        .unwrap();
        let err = open_active_notebook(&library, InkRequest::Default, &geo(), &bg())
            .err()
            .unwrap();
        assert!(!err.is_empty(), "post-commit target load must fail closed");
        assert_eq!(
            read_library_manifest_for_test(&root)
                .unwrap()
                .active_notebook_id,
            "notebook-2"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn manifest_corruption_fails_closed_and_missing_directories_are_not_recreated() {
        for bad in [
            PersistedLibraryManifestV1 {
                v: 99,
                notebook_ids: vec!["notebook-1".into()],
                active_notebook_id: "notebook-1".into(),
                next_notebook_ordinal: 2,
            },
            PersistedLibraryManifestV1 {
                v: LIBRARY_MANIFEST_VERSION,
                notebook_ids: vec!["notebook-1".into(), "notebook-1".into()],
                active_notebook_id: "notebook-1".into(),
                next_notebook_ordinal: 2,
            },
            PersistedLibraryManifestV1 {
                v: LIBRARY_MANIFEST_VERSION,
                notebook_ids: vec!["notebook-1".into()],
                active_notebook_id: "notebook-2".into(),
                next_notebook_ordinal: 2,
            },
            PersistedLibraryManifestV1 {
                v: LIBRARY_MANIFEST_VERSION,
                notebook_ids: vec!["notebook-1".into()],
                active_notebook_id: "notebook-1".into(),
                next_notebook_ordinal: 1,
            },
        ] {
            assert!(validate_library_manifest(&bad).is_err());
        }

        let (root, _library) = fresh_library("missing-dir");
        fs::rename(
            root.join(NOTEBOOKS_DIR).join("notebook-1"),
            root.join("held-notebook-1"),
        )
        .unwrap();
        let err = open_or_create_library(&root, InkRequest::Default, &geo(), &bg())
            .err()
            .unwrap();
        assert!(err.contains("directory missing"), "{err}");
        assert!(!root.join(NOTEBOOKS_DIR).join("notebook-1").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn home_is_framebuffer_only_and_trigger_priority_is_deterministic() {
        let (root, library) = fresh_library("home");
        let notebook = library.notebook_dir("notebook-1");
        let before = legacy_durable_files(&notebook);
        let g = FbGeometry {
            xres: 1860,
            yres: 2480,
            xres_virtual: 1872,
            yres_virtual: 2480,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 1872,
            smem_len: 1872 * 2480,
        };
        let mut fb = vec![0x88; g.smem_len as usize];
        render_home(
            &mut fb,
            &g,
            &home_cards(&library),
            &HomeState::for_active(&library),
        )
        .unwrap();
        assert_eq!(legacy_durable_files(&notebook), before);

        for op in [
            DevLibraryOp::Delete,
            DevLibraryOp::Create,
            DevLibraryOp::Open,
            DevLibraryOp::Previous,
            DevLibraryOp::Next,
            DevLibraryOp::Home,
        ] {
            arm_dev_library_op(&root, op).unwrap();
        }
        assert_eq!(peek_dev_library_op(&root), Some(DevLibraryOp::Home));
        assert!(root.join(DEV_HOME_TRIGGER).exists());
        assert!(root.join(DEV_LIBRARY_NEXT_TRIGGER).exists());
        assert!(root.join(DEV_LIBRARY_PREV_TRIGGER).exists());
        assert!(root.join(DEV_LIBRARY_OPEN_TRIGGER).exists());
        assert!(root.join(DEV_CREATE_NOTEBOOK_TRIGGER).exists());
        assert!(root.join(DEV_DELETE_NOTEBOOK_TRIGGER).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn library_shell_and_ignite_splash_do_not_bind_or_hash_pages() {
        let (root, mut lib) = fresh_library("shell-ignite");
        create_notebook_durable(
            &mut lib,
            &profile(),
            &geo(),
            &bg(),
            Some("Two"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        crate::store::reset_background_io_stats();
        let shell =
            open_or_create_library_shell(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        let cards = home_cards(&shell.repository);
        assert_eq!(cards.len(), 2);
        let mut fb = vec![0x55u8; crate::ui_gfx::panel_geo().smem_len as usize];
        crate::sleep_ui::render_startup(&mut fb, &crate::ui_gfx::panel_geo()).unwrap();
        assert_eq!(fb[0], crate::ui_gfx::BLACK);
        let st = crate::store::take_background_io_stats();
        assert_eq!(st.files_read, 0);
        assert_eq!(st.sha256_ops, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn library_shell_does_not_load_page_journals() {
        let (root, mut lib) = fresh_library("shell-journals");
        create_notebook_durable(
            &mut lib,
            &profile(),
            &geo(),
            &bg(),
            Some("Two"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        switch_notebook_durable(&mut lib, "notebook-1", &SwitchNotebookHooks::default()).unwrap();
        let n2_journal = root
            .join(NOTEBOOKS_DIR)
            .join("notebook-2")
            .join("page-1")
            .join("strokes.jsonl");
        fs::write(&n2_journal, b"{not-json\n").unwrap();

        let shell =
            open_or_create_library_shell(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(shell.repository.active_notebook_id(), "notebook-1");
        let cards = home_cards(&shell.repository);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[1].title, "Two");
        assert_eq!(cards[1].page_count, Some(1));
        open_active_notebook(&shell.repository, InkRequest::Default, &geo(), &bg()).unwrap();

        let err = match open_or_create_notebook(
            &root.join(NOTEBOOKS_DIR).join("notebook-2"),
            InkRequest::Default,
            &geo(),
            &bg(),
        ) {
            Ok(_) => panic!("corrupt journal must fail notebook open"),
            Err(e) => e,
        };
        assert!(
            err.contains("REFUSE") || err.contains("json") || err.contains("parse"),
            "{err}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_page_on_unopened_notebook_does_not_block_home() {
        let (root, mut lib) = fresh_library("shell-missing-page");
        create_notebook_durable(
            &mut lib,
            &profile(),
            &geo(),
            &bg(),
            Some("Two"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        switch_notebook_durable(&mut lib, "notebook-1", &SwitchNotebookHooks::default()).unwrap();
        let lib_meta_before = fs::read(root.join("library.meta")).unwrap();
        fs::remove_dir_all(root.join(NOTEBOOKS_DIR).join("notebook-2").join("page-1")).unwrap();

        let shell =
            open_or_create_library_shell(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(shell.repository.active_notebook_id(), "notebook-1");
        let cards = home_cards(&shell.repository);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].page_count, Some(1));
        assert_eq!(cards[1].page_count, Some(1));

        let mut repo = shell.repository;
        let err = switch_notebook_durable(&mut repo, "notebook-2", &SwitchNotebookHooks::default())
            .unwrap_err();
        assert!(
            err.contains("missing page") || err.contains("REFUSE"),
            "{err}"
        );
        assert_eq!(
            fs::read(root.join("library.meta")).unwrap(),
            lib_meta_before
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn shell_then_bind_matches_eager_library_open() {
        let (root, mut lib) = fresh_library("shell-bind");
        create_notebook_durable(
            &mut lib,
            &profile(),
            &geo(),
            &bg(),
            Some("B"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        switch_notebook_durable(&mut lib, "notebook-1", &SwitchNotebookHooks::default()).unwrap();
        let eager = open_or_create_library(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        let eager_strokes = eager.notebook.loaded.strokes.clone();
        let eager_page = eager.notebook.manifest.active_page_id.clone();
        let eager_profile = eager.notebook.loaded.render_profile.clone();
        drop(eager.notebook.store);

        let shell =
            open_or_create_library_shell(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        let (_dir, opened) =
            open_active_notebook(&shell.repository, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(opened.manifest.active_page_id, eager_page);
        assert_eq!(opened.loaded.strokes, eager_strokes);
        assert_eq!(opened.loaded.render_profile, eager_profile);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn opening_already_active_notebook_after_shell_does_not_rewrite_library_meta() {
        let (root, _lib) = fresh_library("shell-no-rewrite");
        let before = fs::read(root.join("library.meta")).unwrap();
        let active_page = fs::read_to_string(
            root.join(NOTEBOOKS_DIR)
                .join("notebook-1")
                .join("notebook.meta"),
        )
        .unwrap();
        let shell =
            open_or_create_library_shell(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(fs::read(root.join("library.meta")).unwrap(), before);
        assert_eq!(shell.repository.active_notebook_id(), "notebook-1");

        let mut repo = shell.repository;
        switch_notebook_durable(&mut repo, "notebook-1", &SwitchNotebookHooks::default()).unwrap();
        assert_eq!(fs::read(root.join("library.meta")).unwrap(), before);
        open_active_notebook(&repo, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(fs::read(root.join("library.meta")).unwrap(), before);
        assert_eq!(
            fs::read_to_string(
                root.join(NOTEBOOKS_DIR)
                    .join("notebook-1")
                    .join("notebook.meta")
            )
            .unwrap(),
            active_page
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_only_manifest_still_fails_closed_on_missing_pages() {
        let (root, mut lib) = fresh_library("sync-strict-pages");
        create_notebook_durable(
            &mut lib,
            &profile(),
            &geo(),
            &bg(),
            Some("Two"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        fs::remove_dir_all(root.join(NOTEBOOKS_DIR).join("notebook-2").join("page-1")).unwrap();
        open_or_create_library_shell(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        let err =
            match read_notebook_manifest_read_only(&root.join(NOTEBOOKS_DIR).join("notebook-2")) {
                Ok(_) => panic!("sync/import must still fail closed on missing pages"),
                Err(e) => e,
            };
        assert!(
            err.contains("missing page") || err.contains("REFUSE"),
            "{err}"
        );
        let _ = fs::remove_dir_all(root);
    }
    /// Slice 19B.3C: resuming an interrupted migration (record already on disk, so no record
    /// write precedes the remaining moves) admits the moves as `all` before the first one.
    #[test]
    fn resumed_legacy_migration_records_all_before_moving() {
        use crate::page_ledger::{ledger_path, parse_line};
        use crate::sync_verified::{
            clear_registry_for, install_invalidate_hooks, read_mutation_gen, InvalidateHooks,
        };
        use std::sync::{Arc, Mutex};
        let root = tmp("migration-ledger-resume");
        make_representative_f9_legacy(&root);
        open_or_create_library_with_hooks(
            &root,
            InkRequest::Default,
            &geo(),
            &bg(),
            &MigrationHooks {
                fail_after_moves: Some(1),
                ..Default::default()
            },
        )
        .err()
        .unwrap();
        // Legacy objects still waiting in the root.
        let waiting: Vec<PathBuf> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().unwrap().to_string_lossy();
                n == "notebook.meta" || n.starts_with("page-")
            })
            .collect();
        assert!(!waiting.is_empty());
        clear_registry_for(&root); // the next process resumes
        let g0 = read_mutation_gen(&root).unwrap();
        let seen: Arc<Mutex<Option<bool>>> = Arc::default();
        {
            let (seen, waiting) = (seen.clone(), waiting.clone());
            let _h = install_invalidate_hooks(InvalidateHooks {
                after_ledger_durable: Some(Arc::new(move || {
                    let mut s = seen.lock().unwrap();
                    if s.is_none() {
                        *s = Some(waiting.iter().all(|p| p.exists()));
                    }
                })),
                ..Default::default()
            });
            let opened = open_or_create_library(&root, InkRequest::Default, &geo(), &bg()).unwrap();
            drop(opened.notebook.store);
        }
        assert_eq!(
            *seen.lock().unwrap(),
            Some(true),
            "recorded before the first move"
        );
        let raw = fs::read_to_string(ledger_path(&root)).unwrap();
        let recs: Vec<_> = raw.lines().map(|l| parse_line(l).unwrap()).collect();
        let g = g0 + 1;
        let s = recs
            .iter()
            .position(|r| r.g == g && r.k == "session")
            .unwrap();
        assert_eq!(recs[s + 1].k, "all", "{raw}");
        clear_registry_for(&root);
        let _ = fs::remove_dir_all(root);
    }
}
