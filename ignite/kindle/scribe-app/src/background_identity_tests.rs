//! Persisted background identity: authoring, skip-read, migration, crash, and invalidation.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use notebook_core::{Point, Stroke, StrokeId};

    use crate::profile::RenderProfile;
    use crate::store::{
        background_path_for_test, create_new_page, install_background, load_existing_page,
        load_page_journals_read_only, page_dir, page_meta_path_for_test, reset_background_io_stats,
        resolve_background_ref, strip_background_identity_for_test, take_background_io_stats,
        write_meta, BackgroundIdentityV1, PageStore, PersistedPageHeader,
        BACKGROUND_IDENTITY_VERSION,
    };
    use crate::sync_local::background_ref_for;

    fn tmp(label: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scribe-bgid-{label}-{n}"));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn profile() -> RenderProfile {
        RenderProfile::sealed_v2_pressure(0x00).unwrap()
    }

    fn header_on_disk(dir: &Path) -> PersistedPageHeader {
        serde_json::from_str(&fs::read_to_string(page_meta_path_for_test(dir)).unwrap()).unwrap()
    }

    fn resolve_page(root: &Path, page: &str) -> crate::store::BackgroundIoStats {
        reset_background_io_stats();
        let dir = page_dir(root, page);
        let mut j = load_page_journals_read_only(root, page).unwrap();
        let _ = resolve_background_ref(&dir, &mut j.header).unwrap();
        take_background_io_stats()
    }

    #[test]
    fn new_page_receives_a_valid_persistent_identity() {
        let root = tmp("create");
        let bg = vec![0xFFu8; 20 * 5];
        let (_s, loaded) = create_new_page(&root, "page-1", 20, 5, 20, &bg, &profile()).unwrap();
        let id = loaded
            .header
            .background_identity
            .as_ref()
            .expect("identity published with the first page.meta");
        id.validate().unwrap();
        assert!(
            matches!(id, BackgroundIdentityV1::Blank { v } if *v == BACKGROUND_IDENTITY_VERSION)
        );
        assert_eq!(
            header_on_disk(&page_dir(&root, "page-1")).background_identity,
            loaded.header.background_identity
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn restart_reuses_identity_without_hashing_background_bytes() {
        let root = tmp("reuse");
        let mut bg = vec![0xFFu8; 40 * 8];
        bg[0] = 0x10;
        create_new_page(&root, "page-1", 40, 8, 40, &bg, &profile()).unwrap();
        let first = resolve_page(&root, "page-1");
        assert_eq!(first.identity_hits, 1);
        assert_eq!(first.files_read, 0);
        assert_eq!(first.sha256_ops, 0);
        let second = resolve_page(&root, "page-1");
        assert_eq!(second.identity_hits, 1);
        assert_eq!(second.sha256_ops, 0);
        assert_eq!(second.bytes_read, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_identity_migrates_once_without_changing_visible_state() {
        let root = tmp("migrate");
        let mut bg = vec![0xFFu8; 30 * 6];
        bg[3] = 0x22;
        let (_s, loaded) = create_new_page(&root, "page-1", 30, 6, 30, &bg, &profile()).unwrap();
        let dir = page_dir(&root, "page-1");
        let before_checksum = loaded.header.background_checksum;
        let before_strokes = loaded.strokes.len();
        strip_background_identity_for_test(&dir).unwrap();
        assert!(header_on_disk(&dir).background_identity.is_none());

        reset_background_io_stats();
        let mut j = load_page_journals_read_only(&root, "page-1").unwrap();
        let r1 = resolve_background_ref(&dir, &mut j.header).unwrap();
        let st = take_background_io_stats();
        assert_eq!(st.migrated, 1);
        assert_eq!(st.sha256_ops, 1);
        assert!(header_on_disk(&dir).background_identity.is_some());

        reset_background_io_stats();
        let r2 = resolve_background_ref(&dir, &mut header_on_disk(&dir)).unwrap();
        let st2 = take_background_io_stats();
        assert_eq!(st2.identity_hits, 1);
        assert_eq!(st2.sha256_ops, 0);
        assert_eq!(r1, r2);
        assert_eq!(header_on_disk(&dir).background_checksum, before_checksum);
        assert_eq!(
            load_page_journals_read_only(&root, "page-1")
                .unwrap()
                .strokes
                .len(),
            before_strokes
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_identity_fails_safely_and_reconstructs() {
        let root = tmp("malformed");
        let mut bg = vec![0xFFu8; 20 * 5];
        bg[1] = 0x01;
        create_new_page(&root, "page-1", 20, 5, 20, &bg, &profile()).unwrap();
        let dir = page_dir(&root, "page-1");
        let mut h = header_on_disk(&dir);
        h.background_identity = Some(BackgroundIdentityV1::Blob {
            v: BACKGROUND_IDENTITY_VERSION,
            sha256: "not-a-digest".into(),
        });
        write_meta(&dir, &h).unwrap();

        reset_background_io_stats();
        let r = resolve_background_ref(&dir, &mut header_on_disk(&dir)).unwrap();
        let st = take_background_io_stats();
        assert_eq!(st.migrated, 1);
        assert_eq!(st.sha256_ops, 1);
        match r {
            sync_protocol::BackgroundRef::Blob { sha256, .. } => {
                assert_eq!(sha256.as_str().len(), 64);
            }
            other => panic!("{other:?}"),
        }
        header_on_disk(&dir)
            .background_identity
            .unwrap()
            .validate()
            .unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ordinary_stroke_addition_does_not_invalidate_identity() {
        let root = tmp("stroke");
        let bg = vec![0xFFu8; 20 * 5];
        let (mut store, loaded) =
            create_new_page(&root, "page-1", 20, 5, 20, &bg, &profile()).unwrap();
        let before = loaded.header.background_identity.clone();
        let stroke = Stroke {
            id: StrokeId::new("s-1"),
            points: vec![Point::new(1.0, 1.0), Point::new(2.0, 2.0)],
            brush: None,
            finished: true,
        };
        store.append_committed_stroke(&stroke).unwrap();
        drop(store);
        assert_eq!(
            header_on_disk(&page_dir(&root, "page-1")).background_identity,
            before
        );
        let st = resolve_page(&root, "page-1");
        assert_eq!(st.identity_hits, 1);
        assert_eq!(st.sha256_ops, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn page_open_does_not_rehash_an_unchanged_background() {
        let root = tmp("nav");
        let mut bg = vec![0xFFu8; 20 * 5];
        bg[2] = 0x33;
        create_new_page(&root, "page-1", 20, 5, 20, &bg, &profile()).unwrap();
        reset_background_io_stats();
        let _ = load_existing_page(&root, "page-1").unwrap();
        let st = take_background_io_stats();
        assert_eq!(st.sha256_ops, 0, "open verifies CRC, it does not SHA-256");
        let st = resolve_page(&root, "page-1");
        assert_eq!(st.identity_hits, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn replaced_background_updates_identity() {
        let root = tmp("replace");
        let paper = vec![0xFFu8; 20 * 5];
        let (_s, mut loaded) =
            create_new_page(&root, "page-1", 20, 5, 20, &paper, &profile()).unwrap();
        let dir = page_dir(&root, "page-1");
        let before = loaded.header.background_identity.clone();
        let mut inked = paper.clone();
        inked[0] = 0x00;
        install_background(&dir, &inked, &mut loaded.header).unwrap();
        let after = header_on_disk(&dir).background_identity;
        assert_ne!(after, before);
        assert!(matches!(after, Some(BackgroundIdentityV1::Blob { .. })));
        let expected = background_ref_for(&inked, 20, 5, 20).unwrap();
        reset_background_io_stats();
        let got = resolve_background_ref(&dir, &mut header_on_disk(&dir)).unwrap();
        assert_eq!(got, expected);
        assert_eq!(take_background_io_stats().sha256_ops, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn crash_between_background_and_identity_recovers_or_refuses() {
        let root = tmp("crash-bg");
        let paper = vec![0xFFu8; 20 * 5];
        create_new_page(&root, "page-1", 20, 5, 20, &paper, &profile()).unwrap();
        let dir = page_dir(&root, "page-1");
        // Identity cleared, bytes unchanged: reconstruct.
        strip_background_identity_for_test(&dir).unwrap();
        let st = resolve_page(&root, "page-1");
        assert_eq!(st.migrated, 1);

        // Identity cleared, bytes changed, checksum still old: refuse.
        strip_background_identity_for_test(&dir).unwrap();
        let mut torn = paper.clone();
        torn[0] = 0x10;
        fs::write(background_path_for_test(&dir), &torn).unwrap();
        let mut h = header_on_disk(&dir);
        reset_background_io_stats();
        let err = resolve_background_ref(&dir, &mut h).unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn crash_between_identity_temp_and_promotion_recovers() {
        let root = tmp("crash-tmp");
        let bg = vec![0xFFu8; 20 * 5];
        create_new_page(&root, "page-1", 20, 5, 20, &bg, &profile()).unwrap();
        let dir = page_dir(&root, "page-1");
        strip_background_identity_for_test(&dir).unwrap();
        fs::write(dir.join("page.meta.tmp"), b"{broken").unwrap();
        let st = resolve_page(&root, "page-1");
        assert_eq!(st.migrated, 1);
        header_on_disk(&dir)
            .background_identity
            .unwrap()
            .validate()
            .unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn identity_matches_authoring_hash_for_blob_and_blank() {
        let root = tmp("shapes");
        let blank = vec![0xFFu8; 16];
        create_new_page(&root, "page-blank", 4, 4, 4, &blank, &profile()).unwrap();
        let mut blob = vec![0xFFu8; 16];
        blob[15] = 0x00;
        create_new_page(&root, "page-blob", 4, 4, 4, &blob, &profile()).unwrap();
        assert!(matches!(
            header_on_disk(&page_dir(&root, "page-blank")).background_identity,
            Some(BackgroundIdentityV1::Blank { .. })
        ));
        let blob_id = header_on_disk(&page_dir(&root, "page-blob"))
            .background_identity
            .unwrap();
        let from_bytes = BackgroundIdentityV1::from_pixels(&blob);
        assert_eq!(blob_id, from_bytes);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unexpected_size_mismatch_does_not_trust_cached_identity() {
        let root = tmp("size");
        let bg = vec![0xFFu8; 20 * 5];
        create_new_page(&root, "page-1", 20, 5, 20, &bg, &profile()).unwrap();
        let dir = page_dir(&root, "page-1");
        fs::write(background_path_for_test(&dir), vec![0xFFu8; 8]).unwrap();
        let mut h = header_on_disk(&dir);
        let err = resolve_background_ref(&dir, &mut h).unwrap_err();
        assert!(err.contains("size") || err.contains("byte count"), "{err}");
        let _ = fs::remove_dir_all(root);
    }
}
