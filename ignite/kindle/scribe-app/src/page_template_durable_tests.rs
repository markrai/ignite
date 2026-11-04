//! Slice 2: durable notebook integration for page templates (host fixtures only).

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use kindle_platform::{rect_checksum, FbGeometry};
    use notebook_core::{Point, Stroke, StrokeId};
    use sync_protocol::BackgroundRef;

    use crate::ink::InkRequest;
    use crate::library::{
        create_notebook_durable, create_notebook_durable_with_template, open_or_create_library,
        CreateNotebookHooks, LibraryManifestWriteHooks,
    };
    use crate::notebook::{
        create_page_files_from_template, insert_page_after_durable, open_or_create_notebook,
        InsertPageHooks,
    };
    use crate::paper_templates::{generate_page_background, PageTemplate, RULED_SPACING_PX};
    use crate::profile::RenderProfile;
    use crate::store::{
        load_existing_page, resolve_background_ref, BackgroundIdentityV1, PageStore,
        DEFAULT_PAGE_ID,
    };
    use crate::sync_local::background_ref_for;

    fn tmp(label: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scribe-tmpl2-{label}-{n}"));
        let _ = fs::remove_dir_all(&p);
        p
    }

    /// Tall enough for chrome reserve (57) + at least one ruled line (80).
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

    fn paper() -> Vec<u8> {
        vec![0xFF; geo().xres as usize * geo().yres as usize]
    }

    fn profile() -> RenderProfile {
        RenderProfile::sealed_v2_pressure(0x00).unwrap()
    }

    fn fresh_library(label: &str) -> (PathBuf, crate::library::LibraryRepository) {
        let root = tmp(label);
        let opened =
            open_or_create_library(&root, InkRequest::Default, &geo(), &paper()).unwrap();
        (root, opened.repository)
    }

    fn expected_bg(template: PageTemplate) -> Vec<u8> {
        let g = geo();
        let mut bg = generate_page_background(template, g.xres, g.yres).unwrap();
        crate::page_label::scrub_chrome_band(&mut bg, &g).unwrap();
        bg
    }

    fn read_background_raw(nb_dir: &Path, page_id: &str) -> Vec<u8> {
        fs::read(nb_dir.join(page_id).join("background.raw")).unwrap()
    }

    fn stroke(id: &str) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: vec![
                Point::new(1000.0, 1000.0).with_pressure(0.3),
                Point::new(4000.0, 5000.0).with_pressure(0.5),
            ],
            brush: None,
            finished: true,
        }
    }

    #[test]
    fn each_template_persists_exact_background_checksum_and_identity() {
        for template in [
            PageTemplate::Blank,
            PageTemplate::Ruled,
            PageTemplate::Grid,
            PageTemplate::Dotted,
        ] {
            let (root, mut library) = fresh_library(&format!("persist-{template:?}"));
            let commit = create_notebook_durable_with_template(
                &mut library,
                &profile(),
                &geo(),
                template,
                Some(&format!("T-{template:?}")),
                &CreateNotebookHooks::default(),
            )
            .unwrap();
            let nb = library.notebook_dir(&commit.notebook_id);
            let want = expected_bg(template);
            let got = read_background_raw(&nb, DEFAULT_PAGE_ID);
            assert_eq!(got, want, "{template:?} durable bytes");

            let (_store, loaded) = load_existing_page(&nb, DEFAULT_PAGE_ID).unwrap();
            assert_eq!(loaded.background, want, "{template:?} loaded buffer");
            assert_eq!(
                loaded.header.background_bytes,
                want.len() as u64,
                "{template:?}"
            );
            assert_eq!(
                loaded.header.background_checksum,
                rect_checksum(&want),
                "{template:?} checksum"
            );
            let id = loaded
                .header
                .background_identity
                .clone()
                .expect("identity published");
            match template {
                PageTemplate::Blank => {
                    assert!(matches!(id, BackgroundIdentityV1::Blank { .. }));
                }
                _ => {
                    assert!(matches!(id, BackgroundIdentityV1::Blob { .. }));
                }
            }
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn reopen_returns_byte_identical_background_and_refs() {
        for template in [
            PageTemplate::Blank,
            PageTemplate::Ruled,
            PageTemplate::Grid,
            PageTemplate::Dotted,
        ] {
            let (root, mut library) = fresh_library(&format!("reopen-{template:?}"));
            let commit = create_notebook_durable_with_template(
                &mut library,
                &profile(),
                &geo(),
                template,
                None,
                &CreateNotebookHooks::default(),
            )
            .unwrap();
            let nb = library.notebook_dir(&commit.notebook_id);
            let first = read_background_raw(&nb, DEFAULT_PAGE_ID);

            let opened =
                open_or_create_notebook(&nb, InkRequest::Default, &geo(), &paper()).unwrap();
            assert_eq!(opened.loaded.background, first, "{template:?} reopen");
            assert_eq!(
                opened.loaded.background,
                expected_bg(template),
                "{template:?} matches generator"
            );

            let g = geo();
            let bref =
                background_ref_for(&opened.loaded.background, g.xres, g.yres, g.line_length)
                    .unwrap();
            match template {
                PageTemplate::Blank => assert!(matches!(bref, BackgroundRef::Blank)),
                _ => assert!(matches!(bref, BackgroundRef::Blob { .. })),
            }

            let page_dir = nb.join(DEFAULT_PAGE_ID);
            let mut header = opened.loaded.header.clone();
            let resolved = resolve_background_ref(&page_dir, &mut header).unwrap();
            assert_eq!(resolved, bref, "{template:?} resolve_background_ref");

            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn insert_after_inherits_exact_source_background() {
        let (root, mut library) = fresh_library("inherit-insert");
        let commit = create_notebook_durable_with_template(
            &mut library,
            &profile(),
            &geo(),
            PageTemplate::Grid,
            Some("Grid NB"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let nb = library.notebook_dir(&commit.notebook_id);
        let page1 = read_background_raw(&nb, DEFAULT_PAGE_ID);

        let mut manifest = open_or_create_notebook(&nb, InkRequest::Default, &geo(), &paper())
            .unwrap()
            .manifest;
        let inserted = insert_page_after_durable(
            &nb,
            &mut manifest,
            DEFAULT_PAGE_ID,
            &InsertPageHooks::default(),
        )
        .unwrap();
        let page2 = read_background_raw(&nb, &inserted.inserted_page_id);
        assert_eq!(page2, page1, "insert-after must copy source background bytes");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn right_nav_style_create_still_templates_from_page_one() {
        // Mirrors runtime NavDown create: create_page_files_from_template(..., DEFAULT_PAGE_ID).
        let (root, mut library) = fresh_library("inherit-page1");
        let commit = create_notebook_durable_with_template(
            &mut library,
            &profile(),
            &geo(),
            PageTemplate::Ruled,
            None,
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let nb = library.notebook_dir(&commit.notebook_id);
        let mut manifest = open_or_create_notebook(&nb, InkRequest::Default, &geo(), &paper())
            .unwrap()
            .manifest;

        let mid = insert_page_after_durable(
            &nb,
            &mut manifest,
            DEFAULT_PAGE_ID,
            &InsertPageHooks::default(),
        )
        .unwrap()
        .inserted_page_id;
        let page1_bg = read_background_raw(&nb, DEFAULT_PAGE_ID);
        let (_s, loaded) =
            create_page_files_from_template(&nb, "page-99", DEFAULT_PAGE_ID).unwrap();
        assert_eq!(
            loaded.background, page1_bg,
            "right-nav template source remains page-1"
        );
        assert_eq!(read_background_raw(&nb, &mid), page1_bg);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn strokes_survive_reopen_over_patterned_background_without_changing_hash() {
        let (root, mut library) = fresh_library("strokes");
        let commit = create_notebook_durable_with_template(
            &mut library,
            &profile(),
            &geo(),
            PageTemplate::Dotted,
            None,
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let nb = library.notebook_dir(&commit.notebook_id);
        let before_bg = read_background_raw(&nb, DEFAULT_PAGE_ID);
        let before_id = load_existing_page(&nb, DEFAULT_PAGE_ID)
            .unwrap()
            .1
            .header
            .background_identity
            .clone();

        {
            let (mut store, _) = load_existing_page(&nb, DEFAULT_PAGE_ID).unwrap();
            store.append_committed_stroke(&stroke("ink-1")).unwrap();
        }

        let opened = open_or_create_notebook(&nb, InkRequest::Default, &geo(), &paper()).unwrap();
        assert_eq!(opened.loaded.background, before_bg);
        assert_eq!(opened.loaded.header.background_identity, before_id);
        assert_eq!(opened.loaded.strokes.len(), 1);
        assert_eq!(opened.loaded.strokes[0].id.as_str(), "ink-1");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn default_create_path_remains_blank_paper() {
        let (root, mut library) = fresh_library("default-blank");
        let before_ids = library.manifest().notebook_ids.clone();
        create_notebook_durable(
            &mut library,
            &profile(),
            &geo(),
            &paper(),
            Some("Legacy Blank"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let new_id = library
            .manifest()
            .notebook_ids
            .iter()
            .find(|id| !before_ids.contains(id))
            .cloned()
            .unwrap();
        let nb = library.notebook_dir(&new_id);
        let bg = read_background_raw(&nb, DEFAULT_PAGE_ID);
        assert!(bg.iter().all(|&b| b == 0xFF));
        assert!(matches!(
            BackgroundIdentityV1::from_pixels(&bg),
            BackgroundIdentityV1::Blank { .. }
        ));
        let first = library.notebook_dir("notebook-1");
        let first_bg = read_background_raw(&first, DEFAULT_PAGE_ID);
        assert!(first_bg.iter().all(|&b| b == 0xFF));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_manifest_commit_does_not_leave_selectable_notebook() {
        let (root, mut library) = fresh_library("fail-create");
        let before = library.manifest().notebook_ids.clone();
        let next_id = format!("notebook-{}", library.manifest().next_notebook_ordinal);
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
            Some("Should Fail"),
            &hooks,
        )
        .unwrap_err();
        assert!(
            err.contains("library manifest persist failed"),
            "unexpected error: {err}"
        );
        assert_eq!(library.manifest().notebook_ids, before);
        assert!(
            !library.notebook_dir(&next_id).exists(),
            "failed create must not leave a live notebook directory"
        );
        let reopened =
            open_or_create_library(&root, InkRequest::Default, &geo(), &paper()).unwrap();
        assert_eq!(reopened.repository.manifest().notebook_ids, before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ruled_marks_present_in_usable_region_after_durable_create() {
        let (root, mut library) = fresh_library("marks");
        create_notebook_durable_with_template(
            &mut library,
            &profile(),
            &geo(),
            PageTemplate::Ruled,
            None,
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let nb = library.notebook_dir(library.active_notebook_id());
        let bg = read_background_raw(&nb, DEFAULT_PAGE_ID);
        let g = geo();
        let y = RULED_SPACING_PX;
        let i = (y as usize) * (g.xres as usize);
        assert_ne!(bg[i], 0xFF, "expected a rule mark after durable create");
        let _ = fs::remove_dir_all(root);
    }
}
