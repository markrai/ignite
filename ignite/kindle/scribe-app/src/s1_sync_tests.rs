//! Sync Phase S1 host tests: global identity bootstrap/reconcile, replica state, semantic
//! snapshot export, and proof that no existing durable byte is rewritten.

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use kindle_platform::FbGeometry;
    use notebook_core::{new_stroke_id, Point, Stroke, StrokeId};
    use sync_protocol::{
        classify_local_stroke_id, global_stroke_id, snapshot_sha256, BackgroundRef,
        LocalStrokeIdKind, StrokeStyle, SyncSnapshotV1, CANONICAL_PAGE_HEIGHT,
        CANONICAL_PAGE_WIDTH,
    };
    use uuid::Uuid;

    use crate::ink::InkRequest;
    use crate::library::{
        create_notebook_durable, delete_notebook_durable, open_or_create_library,
        rename_notebook_durable, switch_notebook_durable, CreateNotebookHooks, DeleteNotebookHooks,
        LibraryRepository, RenameNotebookHooks, SwitchNotebookHooks,
    };
    use crate::notebook::{
        delete_page_durable, insert_page_after_durable, move_page_durable, persist_active_page,
        DeletePageHooks, InsertPageHooks, MovePageDirection, MovePageHooks,
        PersistedNotebookManifestV1,
    };
    use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
    use crate::profile::RenderProfile;
    use crate::store::{
        load_existing_page, PageStore, PersistedFragmentV2, PersistedPointV1,
        PersistedReplacementV2,
    };
    use crate::sync_local::{
        build_snapshot, ensure_replica_state, ensure_replica_state_with, ensure_sync_identity,
        ensure_sync_identity_with, identity_path, read_identity, snapshot_bytes_and_hash,
        SyncWriteHooks, IDENTITY_FILE, IDENTITY_LOCK_FILE, REPLICA_LOCK_FILE, SYNC_DIR,
    };
    use crate::ui_gfx::paper_background;

    // ------------------------------------------------------------------ fixtures

    fn tmp(label: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scribe-s1-{label}-{n}"));
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

    fn profile() -> RenderProfile {
        RenderProfile::sealed_v2_pressure(0x00).unwrap()
    }

    fn legacy(id: &str, pts: &[(f32, f32, Option<f32>)]) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: pts
                .iter()
                .map(|(x, y, p)| {
                    let mut pt = Point::new(*x, *y);
                    pt.pressure = *p;
                    pt
                })
                .collect(),
            brush: None,
            finished: true,
        }
    }

    fn pressure_stroke(id: StrokeId, pts: &[(f32, f32, f32)]) -> Stroke {
        Stroke {
            id,
            points: pts
                .iter()
                .map(|(x, y, p)| Point::new(*x, *y).with_pressure(*p))
                .collect(),
            brush: Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
            finished: true,
        }
    }

    fn frag(id: &str, style: Option<&str>, pts: &[(f32, f32, Option<f32>)]) -> PersistedFragmentV2 {
        PersistedFragmentV2 {
            id: id.to_string(),
            style: style.map(str::to_string),
            points: pts
                .iter()
                .map(|(x, y, p)| PersistedPointV1 {
                    x: *x,
                    y: *y,
                    pressure: *p,
                })
                .collect(),
        }
    }

    fn manifest(nb_dir: &Path) -> PersistedNotebookManifestV1 {
        serde_json::from_slice(&fs::read(nb_dir.join("notebook.meta")).unwrap()).unwrap()
    }

    fn repo(root: &Path) -> LibraryRepository {
        let g = geo();
        open_or_create_library(root, InkRequest::Default, &g, &paper_background(&g))
            .unwrap()
            .repository
    }

    /// Stroke ids used by the representative library (page-1 of notebook-1).
    struct Fixture {
        new_stroke: String,
        erased_frag_new: String,
        second_gen_frag: String,
    }

    /// Representative F11 library via real durable product paths:
    ///
    /// ```text
    /// notebook-1 "Notebook 1"   pages [page-1, page-3, page-2] (inserted + reordered)
    ///   page-1: legacy strokes (fixed, one with pressure), a UUID-backed pressure stroke,
    ///           a partial erase of a legacy stroke (legacy-id + UUID-id fragments) and a
    ///           second-generation erase of a fragment (root lineage must survive)
    ///   page-3: one pressure stroke
    /// notebook-2 "Work Renamed" (renamed), nonblank synthetic background, pages [page-1, page-2]
    /// ```
    fn build_library(root: &Path) -> Fixture {
        let g = geo();
        let paper = paper_background(&g);
        let mut lib = open_or_create_library(root, InkRequest::Default, &g, &paper)
            .unwrap()
            .repository;
        let nb1 = lib.notebook_dir("notebook-1");
        let mut m1 = manifest(&nb1);

        let (mut store, _) = load_existing_page(&nb1, "page-1").unwrap();
        store
            .append_committed_stroke(&legacy(
                "s-1727000000000-1",
                &[(100.0, 200.0, Some(0.25)), (900.5, 1234.75, Some(0.5))],
            ))
            .unwrap();
        store
            .append_committed_stroke(&legacy(
                "s-1727000000000-2",
                &[(15_000.0, 20_000.0, None), (15_624.0, 20_832.0, None)],
            ))
            .unwrap();
        let new_stroke = new_stroke_id();
        store
            .append_committed_stroke(&pressure_stroke(
                new_stroke.clone(),
                &[
                    (1.5, 2.25, 0.125),
                    (3000.0, 4000.0, 0.875),
                    (3001.0, 4002.0, 1.0),
                ],
            ))
            .unwrap();
        // F7 partial erase of the legacy stroke → a legacy-style fragment + a UUID fragment.
        let erased_frag_new = new_stroke_id().as_str().to_string();
        store
            .append_replace_strokes(&[PersistedReplacementV2 {
                target_stroke_id: "s-1727000000000-1".into(),
                fragments: vec![
                    frag("s-1727000000500-7", None, &[(100.0, 200.0, Some(0.25))]),
                    frag(&erased_frag_new, None, &[(900.5, 1234.75, Some(0.5))]),
                ],
            }])
            .unwrap();
        // Second-generation erase: fragment of a fragment keeps the original root.
        let second_gen_frag = new_stroke_id().as_str().to_string();
        store
            .append_replace_strokes(&[PersistedReplacementV2 {
                target_stroke_id: erased_frag_new.clone(),
                fragments: vec![frag(
                    &second_gen_frag,
                    None,
                    &[(900.5, 1234.75, Some(0.5)), (901.0, 1235.0, Some(0.5))],
                )],
            }])
            .unwrap();
        drop(store);

        let p2 = insert_page_after_durable(&nb1, &mut m1, "page-1", &InsertPageHooks::default())
            .unwrap()
            .inserted_page_id;
        let p3 = insert_page_after_durable(&nb1, &mut m1, "page-1", &InsertPageHooks::default())
            .unwrap()
            .inserted_page_id;
        assert_eq!((p2.as_str(), p3.as_str()), ("page-2", "page-3"));
        assert_eq!(m1.page_ids, ["page-1", "page-3", "page-2"]);
        let (mut s3, _) = load_existing_page(&nb1, "page-3").unwrap();
        s3.append_committed_stroke(&pressure_stroke(
            new_stroke_id(),
            &[(10.0, 10.0, 0.5), (20.0, 30.0, 0.6)],
        ))
        .unwrap();
        drop(s3);
        // Reorder: page-2 left then right again is a no-op; move page-3 right → [1, 2, 3]…
        move_page_durable(
            &nb1,
            &mut m1,
            "page-3",
            MovePageDirection::Right,
            &MovePageHooks::default(),
        )
        .unwrap();
        // …and back, so the semantic order differs from the local name order.
        move_page_durable(
            &nb1,
            &mut m1,
            "page-3",
            MovePageDirection::Left,
            &MovePageHooks::default(),
        )
        .unwrap();
        assert_eq!(m1.page_ids, ["page-1", "page-3", "page-2"]);

        // Notebook 2 with a nonblank synthetic durable background, then renamed.
        let bg: Vec<u8> = (0..paper.len()).map(|i| (i % 251) as u8).collect();
        let c = create_notebook_durable(
            &mut lib,
            &profile(),
            &g,
            &bg,
            Some("Work"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        assert_eq!(c.notebook_id, "notebook-2");
        rename_notebook_durable(
            &mut lib,
            "notebook-2",
            "Work Renamed",
            &RenameNotebookHooks::default(),
        )
        .unwrap();
        let nb2 = lib.notebook_dir("notebook-2");
        let mut m2 = manifest(&nb2);
        insert_page_after_durable(&nb2, &mut m2, "page-1", &InsertPageHooks::default()).unwrap();
        Fixture {
            new_stroke: new_stroke.as_str().to_string(),
            erased_frag_new,
            second_gen_frag,
        }
    }

    /// Every file under `root` (relative path → bytes).
    fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            for e in fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(base, &p, out);
                } else {
                    let rel = p
                        .strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.insert(rel, fs::read(&p).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    const DURABLE: [&str; 7] = [
        "library.meta",
        "notebook.info",
        "notebook.meta",
        "page.meta",
        "background.raw",
        "strokes.jsonl",
        "edits.jsonl",
    ];

    // ------------------------------------------------------------------ 26/27 bytes + idempotence

    #[test]
    fn bootstrap_and_snapshot_never_rewrite_durable_bytes_and_are_idempotent() {
        let root = tmp("bytes");
        build_library(&root);
        let state = tmp("bytes-state");
        let before = tree(&root);
        let durable: Vec<&String> = before
            .keys()
            .filter(|k| DURABLE.iter().any(|d| k.ends_with(d)))
            .collect();
        assert!(durable.len() >= 20, "representative library: {durable:?}");

        let replica = ensure_replica_state(&state, Some(&root)).unwrap();
        let first = ensure_sync_identity(&root).unwrap();
        assert!(first.changed && first.allocated_library);
        let id_bytes = fs::read(identity_path(&root)).unwrap();
        let snap1 = build_snapshot(&root, &first.identity).unwrap();
        let (bytes1, hash1) = snapshot_bytes_and_hash(&snap1).unwrap();

        // Second bootstrap: nothing allocated, identity.json byte-identical (not even rewritten).
        let mtime = fs::metadata(identity_path(&root))
            .unwrap()
            .modified()
            .unwrap();
        let second = ensure_sync_identity(&root).unwrap();
        assert!(!second.changed);
        assert_eq!(second.identity, first.identity);
        assert_eq!(fs::read(identity_path(&root)).unwrap(), id_bytes);
        assert_eq!(
            fs::metadata(identity_path(&root))
                .unwrap()
                .modified()
                .unwrap(),
            mtime
        );
        let snap2 = build_snapshot(&root, &second.identity).unwrap();
        let (bytes2, hash2) = snapshot_bytes_and_hash(&snap2).unwrap();
        assert_eq!(bytes1, bytes2);
        assert_eq!(hash1, hash2);
        assert_eq!(ensure_replica_state(&state, Some(&root)).unwrap(), replica);

        // Every pre-existing file is byte-identical; the only new files are the identity map
        // and its (empty) transaction lock, both in the app-owned `.scribe-sync/` namespace.
        let after = tree(&root);
        for (path, bytes) in &before {
            assert_eq!(after.get(path), Some(bytes), "{path} changed");
        }
        let added: BTreeSet<&String> = after.keys().filter(|k| !before.contains_key(*k)).collect();
        assert_eq!(
            added.into_iter().cloned().collect::<Vec<_>>(),
            vec![
                format!("{SYNC_DIR}/{IDENTITY_FILE}"),
                format!("{SYNC_DIR}/{IDENTITY_LOCK_FILE}")
            ]
        );
        // Replica state lives only in its own state root.
        assert!(!after.keys().any(|k| k.contains("replica")));
        assert!(state.join("replica.json").is_file());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn identity_covers_exactly_the_authoritative_entities() {
        let root = tmp("authoritative");
        build_library(&root);
        let mut lib = repo(&root);
        // Trash + orphans + dev triggers must never receive identities.
        let nb1 = lib.notebook_dir("notebook-1");
        let mut m1 = manifest(&nb1);
        delete_page_durable(&nb1, &mut m1, "page-2", &DeletePageHooks::default()).unwrap();
        let c = create_notebook_durable(
            &mut lib,
            &profile(),
            &geo(),
            &paper_background(&geo()),
            Some("Doomed"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        delete_notebook_durable(&mut lib, &c.notebook_id, &DeleteNotebookHooks::default()).unwrap();
        fs::create_dir_all(root.join("notebooks/notebook-99")).unwrap();
        fs::write(root.join(".scribe-dev-home"), b"1\n").unwrap();

        let r = ensure_sync_identity(&root).unwrap();
        let id = &r.identity;
        assert_eq!(
            id.notebooks.keys().collect::<Vec<_>>(),
            ["notebook-1", "notebook-2"]
        );
        assert_eq!(
            id.pages["notebook-1"].keys().collect::<Vec<_>>(),
            ["page-1", "page-3"]
        );
        assert_eq!(id.pages["notebook-2"].len(), 2);
        assert!(id.retired_notebooks.is_empty() && id.retired_pages.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------------ 28 new local entities

    #[test]
    fn new_local_entities_get_exactly_one_new_id_and_existing_ids_never_change() {
        let root = tmp("new-entities");
        build_library(&root);
        let base = ensure_sync_identity(&root).unwrap().identity;
        let mut lib = repo(&root);
        let c = create_notebook_durable(
            &mut lib,
            &profile(),
            &geo(),
            &paper_background(&geo()),
            Some("Fresh"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let nb1 = lib.notebook_dir("notebook-1");
        let mut m1 = manifest(&nb1);
        let ins = insert_page_after_durable(&nb1, &mut m1, "page-2", &InsertPageHooks::default())
            .unwrap();
        let r = ensure_sync_identity(&root).unwrap();
        assert_eq!(r.allocated_notebooks.len(), 1);
        assert_eq!(r.allocated_notebooks[0].local_id, c.notebook_id);
        let new_pages: BTreeSet<(String, String)> = r
            .allocated_pages
            .iter()
            .map(|b| (b.notebook_local_id.clone(), b.local_id.clone()))
            .collect();
        assert_eq!(
            new_pages,
            BTreeSet::from([
                (c.notebook_id.clone(), "page-1".to_string()),
                ("notebook-1".to_string(), ins.inserted_page_id.clone()),
            ])
        );
        for b in base.notebook_bindings() {
            assert_eq!(r.identity.notebook_id(&b.local_id), Some(b.sync_id));
        }
        for b in base.page_bindings() {
            assert_eq!(
                r.identity.page_id(&b.notebook_local_id, &b.local_id),
                Some(b.sync_id)
            );
        }
        assert_eq!(r.identity.library_id, base.library_id);
        let again = ensure_sync_identity(&root).unwrap();
        assert!(
            !again.changed
                && again.allocated_pages.is_empty()
                && again.allocated_notebooks.is_empty()
        );
        let _ = fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------------ 29 deletes retain identity

    #[test]
    fn deleted_entities_keep_retired_identities_that_are_never_reused() {
        let root = tmp("delete");
        build_library(&root);
        let mut lib = repo(&root);
        let c = create_notebook_durable(
            &mut lib,
            &profile(),
            &geo(),
            &paper_background(&geo()),
            Some("A"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let base = ensure_sync_identity(&root).unwrap().identity;
        let a_id = base.notebook_id(&c.notebook_id).unwrap();
        let a_page = base.page_id(&c.notebook_id, "page-1").unwrap();
        let x_id = base.page_id("notebook-1", "page-3").unwrap();

        delete_notebook_durable(&mut lib, &c.notebook_id, &DeleteNotebookHooks::default()).unwrap();
        let nb1 = lib.notebook_dir("notebook-1");
        let mut m1 = manifest(&nb1);
        delete_page_durable(&nb1, &mut m1, "page-3", &DeletePageHooks::default()).unwrap();

        let r = ensure_sync_identity(&root).unwrap();
        let id = &r.identity;
        assert_eq!(id.retired_notebooks[&a_id].local_id, c.notebook_id);
        assert_eq!(id.retired_pages[&a_page].notebook_id, a_id);
        assert_eq!(id.retired_pages[&x_id].local_id, "page-3");
        assert!(id.notebook_id(&c.notebook_id).is_none());
        assert!(id.page_id("notebook-1", "page-3").is_none());

        // New entities after deletion receive fresh IDs (local ordinals are monotonic anyway).
        let c2 = create_notebook_durable(
            &mut lib,
            &profile(),
            &geo(),
            &paper_background(&geo()),
            Some("B"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let mut m1 = manifest(&nb1);
        insert_page_after_durable(&nb1, &mut m1, "page-1", &InsertPageHooks::default()).unwrap();
        let r2 = ensure_sync_identity(&root).unwrap();
        let retired: BTreeSet<Uuid> = [a_id.as_uuid(), a_page.as_uuid(), x_id.as_uuid()].into();
        for b in r2.allocated_notebooks.iter() {
            assert!(!retired.contains(&b.sync_id.as_uuid()));
        }
        for b in r2.allocated_pages.iter() {
            assert!(!retired.contains(&b.sync_id.as_uuid()));
        }
        assert!(r2.identity.notebook_id(&c2.notebook_id).is_some());
        assert_eq!(r2.identity.retired_notebooks.len(), 1);
        assert_eq!(r2.identity.retired_pages.len(), 2);
        // Trash is never part of the active snapshot.
        let snap = build_snapshot(&root, &r2.identity).unwrap();
        let live_nbs: Vec<_> = snap.notebooks.iter().map(|n| n.notebook_id).collect();
        assert!(!live_nbs.contains(&a_id));
        assert!(snap
            .notebooks
            .iter()
            .flat_map(|n| &n.pages)
            .all(|p| p.page_id != x_id && p.page_id != a_page));
        let _ = fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------------ 30/31 stroke identity

    #[test]
    fn legacy_and_new_strokes_map_to_global_ids_with_exact_lineage() {
        let root = tmp("strokes");
        let fx = build_library(&root);
        let id = ensure_sync_identity(&root).unwrap().identity;
        let snap = build_snapshot(&root, &id).unwrap();
        let page1 = id.page_id("notebook-1", "page-1").unwrap();
        let p = &snap.notebooks[0].pages[0];
        assert_eq!(p.page_id, page1);

        let g = |local: &str| global_stroke_id(page1, local).unwrap();
        let uuid_of = |local: &str| match classify_local_stroke_id(local).unwrap() {
            LocalStrokeIdKind::UuidBacked(s) => s,
            LocalStrokeIdKind::Legacy => panic!("{local} should be UUID-backed"),
        };
        // Visible order: erased legacy stroke replaced in place by its fragments.
        let ids: Vec<_> = p.strokes.iter().map(|s| (s.stroke_id, s.root_id)).collect();
        assert_eq!(
            ids,
            vec![
                (g("s-1727000000500-7"), g("s-1727000000000-1")),
                (uuid_of(&fx.second_gen_frag), g("s-1727000000000-1")),
                (g("s-1727000000000-2"), g("s-1727000000000-2")),
                (uuid_of(&fx.new_stroke), uuid_of(&fx.new_stroke)),
            ]
        );
        // Legacy mapping is UUIDv5(PageId, exact local id): stable and page-scoped.
        assert_eq!(
            g("s-1727000000000-2").as_uuid(),
            Uuid::new_v5(&page1.as_uuid(), b"s-1727000000000-2")
        );
        let other_page = id.page_id("notebook-1", "page-3").unwrap();
        assert_ne!(
            global_stroke_id(other_page, "s-1727000000000-2").unwrap(),
            g("s-1727000000000-2")
        );
        // New strokes preserve their embedded UUID directly.
        assert_eq!(uuid_of(&fx.new_stroke).to_string(), &fx.new_stroke[2..]);
        assert!(!p
            .strokes
            .iter()
            .any(|s| s.stroke_id == uuid_of(&fx.erased_frag_new)));
        // Styles + exact pressure preserved.
        assert_eq!(p.strokes[3].style, StrokeStyle::PressureWidthV1);
        assert_eq!(p.strokes[3].points[0].pressure, Some(0.125));
        assert_eq!(p.strokes[0].style, StrokeStyle::LegacyFixedV1);
        assert_eq!(p.strokes[0].points[0].pressure, Some(0.25));
        assert_eq!(p.strokes[2].points[1].x, 15_624.0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn newly_authored_stroke_ids_are_globally_unique_and_convert_directly() {
        let page = sync_protocol::PageId::parse("019a8ef2-0000-7000-8000-00000000000a").unwrap();
        let mut seen = BTreeSet::new();
        for _ in 0..20_000 {
            let local = new_stroke_id();
            let s = local.as_str();
            let LocalStrokeIdKind::UuidBacked(global) = classify_local_stroke_id(s).unwrap() else {
                panic!("{s} not UUID-backed");
            };
            assert!(seen.insert(global), "duplicate {s}");
            assert_eq!(global_stroke_id(page, s).unwrap(), global);
            assert_eq!(sync_protocol::uuid_backed_local_stroke_id(global), s);
            let json = serde_json::to_string(&global).unwrap();
            assert_eq!(
                serde_json::from_str::<sync_protocol::StrokeId>(&json).unwrap(),
                global
            );
        }
    }

    #[test]
    fn protocol_page_space_matches_notebook_core() {
        assert_eq!(CANONICAL_PAGE_WIDTH, notebook_core::CANONICAL_WIDTH);
        assert_eq!(CANONICAL_PAGE_HEIGHT, notebook_core::CANONICAL_HEIGHT);
    }

    // ------------------------------------------------------------------ 32 snapshot round trip

    #[test]
    fn snapshot_round_trips_semantically_with_exact_order_and_lineage() {
        let root = tmp("roundtrip");
        build_library(&root);
        let id = ensure_sync_identity(&root).unwrap().identity;
        let snap = build_snapshot(&root, &id).unwrap();

        // Semantic order: library.meta notebooks, notebook.meta pages (not local names).
        assert_eq!(snap.notebooks.len(), 2);
        assert_eq!(snap.notebooks[0].title, "Notebook 1");
        assert_eq!(snap.notebooks[1].title, "Work Renamed");
        let order: Vec<_> = snap.notebooks[0].pages.iter().map(|p| p.page_id).collect();
        assert_eq!(
            order,
            ["page-1", "page-3", "page-2"]
                .map(|p| id.page_id("notebook-1", p).unwrap())
                .to_vec()
        );
        // Backgrounds: paper → Blank; synthetic → content-addressed blob of the durable bytes.
        assert_eq!(snap.notebooks[0].pages[0].background, BackgroundRef::Blank);
        let nb2_dir = root.join("notebooks/notebook-2");
        let raw = fs::read(nb2_dir.join("page-1/background.raw")).unwrap();
        match &snap.notebooks[1].pages[0].background {
            BackgroundRef::Blob {
                sha256,
                byte_len,
                format,
            } => {
                assert_eq!(*sha256, sync_protocol::sha256_hex(&raw));
                assert_eq!(*byte_len, raw.len() as u64);
                assert_eq!((format.width, format.height), (200, 160));
            }
            other => panic!("expected blob, got {other:?}"),
        }
        assert_eq!(snap.notebooks[0].pages[0].render.ink_byte, 0x00);
        assert!(snap.notebooks[0].pages[0].render.pressure.is_some());

        let bytes = snap.to_canonical_bytes().unwrap();
        let back = SyncSnapshotV1::parse(&bytes).unwrap();
        assert_eq!(back, snap, "semantic equality");
        assert_eq!(
            back.to_canonical_bytes().unwrap(),
            bytes,
            "serialize/deserialize/serialize"
        );
        assert_eq!(
            snapshot_sha256(&back).unwrap(),
            snapshot_sha256(&snap).unwrap()
        );
        // Nothing local leaks into the payload.
        let text = String::from_utf8(bytes).unwrap();
        for leak in [
            "notebook-1",
            "page-1",
            "active",
            "ordinal",
            "replica",
            "scribe-data",
            "s-1727",
            root.to_string_lossy().as_ref(),
        ] {
            assert!(!text.contains(leak), "snapshot leaks {leak:?}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------------ 33 local UI exclusion

    #[test]
    fn replica_local_ui_state_does_not_change_the_snapshot() {
        let root = tmp("ui-state");
        build_library(&root);
        let id = ensure_sync_identity(&root).unwrap().identity;
        let (before, h0) = snapshot_bytes_and_hash(&build_snapshot(&root, &id).unwrap()).unwrap();
        let meta0 = fs::read(root.join("library.meta")).unwrap();

        // Active notebook (library.meta) and active page (notebook.meta) change durably…
        let mut lib = repo(&root);
        switch_notebook_durable(&mut lib, "notebook-1", &SwitchNotebookHooks::default()).unwrap();
        let nb1 = lib.notebook_dir("notebook-1");
        let mut m1 = manifest(&nb1);
        persist_active_page(&nb1, &mut m1, "page-2").unwrap();
        assert_ne!(fs::read(root.join("library.meta")).unwrap(), meta0);

        // …but the shared semantic snapshot is byte-identical, and no identity is allocated.
        let r = ensure_sync_identity(&root).unwrap();
        assert!(!r.changed);
        let (after, h1) =
            snapshot_bytes_and_hash(&build_snapshot(&root, &r.identity).unwrap()).unwrap();
        assert_eq!(before, after);
        assert_eq!(h0, h1);

        // A real shared-content change does change it.
        rename_notebook_durable(
            &mut lib,
            "notebook-1",
            "Retitled",
            &RenameNotebookHooks::default(),
        )
        .unwrap();
        let (_, h2) =
            snapshot_bytes_and_hash(&build_snapshot(&root, &r.identity).unwrap()).unwrap();
        assert_ne!(h1, h2);
        let _ = fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------------ 6 replica identity

    #[test]
    fn replica_identity_is_per_installation_and_never_part_of_the_library() {
        let root = tmp("replica-lib");
        build_library(&root);
        let state_a = tmp("replica-a");
        let state_b = tmp("replica-b");
        let a = ensure_replica_state(&state_a, Some(&root)).unwrap();
        assert_eq!(
            ensure_replica_state(&state_a, Some(&root)).unwrap(),
            a,
            "same install"
        );
        let b = ensure_replica_state(&state_b, Some(&root)).unwrap();
        assert_ne!(a.replica_id, b.replica_id, "different state root");
        let id = ensure_sync_identity(&root).unwrap().identity;
        assert_ne!(a.replica_id.as_uuid(), id.library_id.as_uuid());

        // Copying only the library copies no replica identity.
        let copy = tmp("replica-copy");
        for (rel, bytes) in tree(&root) {
            let p = copy.join(&rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, bytes).unwrap();
        }
        assert!(!tree(&copy).keys().any(|k| k.contains("replica")));
        assert_eq!(
            read_identity(&copy).unwrap().unwrap().library_id,
            id.library_id
        );

        // A state dir inside the library is refused.
        assert!(ensure_replica_state(&root.join("state"), Some(&root)).is_err());
        // Sequential idempotence (the concurrent race is tested separately below).
        let state_c = tmp("replica-c");
        let first = ensure_replica_state_with(&state_c, None, &mut Uuid::now_v7).unwrap();
        let second = ensure_replica_state_with(&state_c, None, &mut Uuid::now_v7).unwrap();
        assert_eq!(first, second);
        for d in [&root, &state_a, &state_b, &copy, &state_c] {
            let _ = fs::remove_dir_all(d);
        }
    }

    // ------------------------------------------------------------------ concurrency

    const RACERS: usize = 20;

    /// Run `f` on `n` threads released together by a barrier; collect every result.
    fn race<T: Send>(n: usize, f: impl Fn() -> T + Sync) -> Vec<T> {
        let barrier = std::sync::Barrier::new(n);
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..n)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        f()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        })
    }

    fn dir_names(dir: &Path) -> BTreeSet<String> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn concurrent_replica_creation_publishes_exactly_one_id() {
        for round in 0..30 {
            let state = tmp(&format!("replica-race-{round}"));
            let results = race(8, || ensure_replica_state(&state, None).unwrap());
            let winner = results[0];
            assert!(
                results.iter().all(|r| *r == winner),
                "round {round}: {results:?}"
            );
            let durable = sync_protocol::ReplicaStateV1::parse(
                &fs::read(state.join("replica.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(durable, winner);
            assert_eq!(
                dir_names(&state),
                BTreeSet::from(["replica.json".to_string(), REPLICA_LOCK_FILE.to_string()]),
                "no temporary residue"
            );
            // The published file never changes afterward.
            let bytes = fs::read(state.join("replica.json")).unwrap();
            let again = race(4, || ensure_replica_state(&state, None).unwrap());
            assert!(again.iter().all(|r| *r == winner));
            assert_eq!(fs::read(state.join("replica.json")).unwrap(), bytes);
            let _ = fs::remove_dir_all(&state);
        }
    }

    #[test]
    fn concurrent_empty_bootstrap_yields_one_identity_for_everyone() {
        let root = tmp("identity-race");
        build_library(&root);
        let before = tree(&root);
        let results = race(RACERS, || ensure_sync_identity(&root).unwrap());
        let first = &results[0].identity;
        for r in &results {
            assert_eq!(&r.identity, first, "every caller sees the same map");
        }
        assert_eq!(results.iter().filter(|r| r.allocated_library).count(), 1);
        assert_eq!(
            results.iter().filter(|r| r.changed).count(),
            1,
            "one publisher"
        );
        let live_pages: usize = first.pages.values().map(|p| p.len()).sum();
        assert_eq!(
            results
                .iter()
                .map(|r| r.allocated_pages.len())
                .sum::<usize>(),
            live_pages,
            "one PageId per live page across all callers"
        );
        assert_eq!(
            results
                .iter()
                .map(|r| r.allocated_notebooks.len())
                .sum::<usize>(),
            first.notebooks.len()
        );
        assert_eq!(read_identity(&root).unwrap().as_ref(), Some(first));
        let after = tree(&root);
        for (k, v) in &before {
            assert_eq!(after.get(k), Some(v), "{k} changed");
        }
        assert!(dir_names(&root.join(SYNC_DIR))
            .iter()
            .all(|n| !n.ends_with(".tmp")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_reconcile_allocates_each_new_entity_once() {
        let root = tmp("identity-race-new");
        build_library(&root);
        let base = ensure_sync_identity(&root).unwrap().identity;
        let mut lib = repo(&root);
        let c = create_notebook_durable(
            &mut lib,
            &profile(),
            &geo(),
            &paper_background(&geo()),
            Some("Fresh"),
            &CreateNotebookHooks::default(),
        )
        .unwrap();
        let nb1 = lib.notebook_dir("notebook-1");
        let mut m1 = manifest(&nb1);
        let ins = insert_page_after_durable(&nb1, &mut m1, "page-2", &InsertPageHooks::default())
            .unwrap();

        let results = race(RACERS, || ensure_sync_identity(&root).unwrap());
        let first = &results[0].identity;
        assert!(results.iter().all(|r| &r.identity == first));
        let nbs: Vec<_> = results
            .iter()
            .flat_map(|r| &r.allocated_notebooks)
            .collect();
        assert_eq!(nbs.len(), 1, "exactly one NotebookId minted");
        assert_eq!(nbs[0].local_id, c.notebook_id);
        let pages: Vec<_> = results.iter().flat_map(|r| &r.allocated_pages).collect();
        assert_eq!(
            pages.len(),
            2,
            "new notebook's page-1 + inserted page, once each"
        );
        assert!(pages.iter().any(|b| b.local_id == ins.inserted_page_id));
        assert_eq!(first.notebook_id(&c.notebook_id), Some(nbs[0].sync_id));
        for b in base.page_bindings() {
            assert_eq!(
                first.page_id(&b.notebook_local_id, &b.local_id),
                Some(b.sync_id)
            );
        }
        assert_eq!(read_identity(&root).unwrap().as_ref(), Some(first));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_reconcile_of_complete_identity_mutates_nothing() {
        let root = tmp("identity-race-idle");
        build_library(&root);
        let base = ensure_sync_identity(&root).unwrap().identity;
        let bytes = fs::read(identity_path(&root)).unwrap();
        let mtime = fs::metadata(identity_path(&root))
            .unwrap()
            .modified()
            .unwrap();
        let results = race(RACERS, || ensure_sync_identity(&root).unwrap());
        assert!(results.iter().all(|r| !r.changed && r.identity == base));
        assert_eq!(fs::read(identity_path(&root)).unwrap(), bytes);
        assert_eq!(
            fs::metadata(identity_path(&root))
                .unwrap()
                .modified()
                .unwrap(),
            mtime
        );
        let _ = fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------------ durability / read-only

    #[test]
    fn failed_identity_persist_changes_nothing_and_retry_succeeds() {
        let root = tmp("durable");
        build_library(&root);
        let before = tree(&root);
        for hooks in [
            SyncWriteHooks {
                fail_write: true,
                ..Default::default()
            },
            SyncWriteHooks {
                fail_rename: true,
                ..Default::default()
            },
        ] {
            assert!(ensure_sync_identity_with(&root, &mut Uuid::now_v7, &hooks).is_err());
            assert!(read_identity(&root).unwrap().is_none());
            let now = tree(&root);
            assert!(
                now.keys()
                    .all(|k| before.contains_key(k)
                        || *k == format!("{SYNC_DIR}/{IDENTITY_LOCK_FILE}")),
                "no residue besides the empty lock file: {:?}",
                now.keys()
            );
        }
        let r = ensure_sync_identity(&root).unwrap();
        assert_eq!(read_identity(&root).unwrap(), Some(r.identity));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn export_is_strictly_read_only_even_with_torn_journal_tails() {
        let root = tmp("torn");
        build_library(&root);
        let journal = root.join("notebooks/notebook-1/page-1/strokes.jsonl");
        let mut raw = fs::read(&journal).unwrap();
        raw.extend_from_slice(br#"{"v":1,"type":"stroke","id":"s-9","fin"#); // torn record
        fs::write(&journal, &raw).unwrap();
        let before = tree(&root);
        let id = ensure_sync_identity(&root).unwrap().identity;
        let snap = build_snapshot(&root, &id).unwrap();
        assert_eq!(snap.notebooks[0].pages[0].strokes.len(), 4, "tail ignored");
        let after = tree(&root);
        for (k, v) in &before {
            assert_eq!(
                after.get(k),
                Some(v),
                "{k} rewritten (tail must not be truncated)"
            );
        }
        // An older page format is refused, never migrated in place.
        let meta_p = root.join("notebooks/notebook-2/page-2/page.meta");
        let mut meta: serde_json::Value =
            serde_json::from_slice(&fs::read(&meta_p).unwrap()).unwrap();
        meta["v"] = serde_json::json!(4);
        let old = serde_json::to_vec(&meta).unwrap();
        fs::write(&meta_p, &old).unwrap();
        let err = build_snapshot(&root, &id).unwrap_err();
        assert!(err.contains("open it in Scribe once to migrate"), "{err}");
        assert_eq!(fs::read(&meta_p).unwrap(), old);
        let _ = fs::remove_dir_all(&root);
    }
}
