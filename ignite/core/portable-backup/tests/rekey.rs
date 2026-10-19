mod common;

use std::path::PathBuf;

use common::*;
use portable_backup::{rekey_library, BackupErrorCode as C, Limits};
use scribe_sync_server::{Store, StoreError};
use sync_engine::testkit::uuid7;
use sync_engine::LibraryState;
use sync_protocol::{sha256_hex, snapshot_sha256, BackgroundRef, LibraryId, SyncSnapshotV1};

fn new_id(n: u64) -> LibraryId {
    LibraryId::from_uuid(uuid7(1, 0x100 + n)).unwrap()
}

#[test]
fn rekey_changes_only_the_library_identity() {
    let b = backup();
    let before = b.clone();
    let (_, snaps) = verify(&b, &Limits::DEFAULT).unwrap();
    for (n, src) in snaps.iter().enumerate() {
        let target = new_id(n as u64);
        let r = rekey_library(src, target).unwrap();
        assert_eq!(r.library_id, target);
        assert_eq!(r.source_library_id, src.source_library_id());
        assert_eq!(&r.source_snapshot_sha256, src.source_snapshot_sha256());
        assert_ne!(r.restored_snapshot_sha256, r.source_snapshot_sha256);

        let restored = SyncSnapshotV1::parse(&r.canonical_bytes).unwrap();
        assert_eq!(
            restored.to_canonical_bytes().unwrap(),
            r.canonical_bytes,
            "canonical"
        );
        assert_eq!(
            snapshot_sha256(&restored).unwrap(),
            r.restored_snapshot_sha256
        );
        assert_eq!(sha256_hex(&r.canonical_bytes), r.restored_snapshot_sha256);
        LibraryState::genesis(restored.clone()).unwrap();

        // Everything except library_id is identical, including order and references.
        let mut expect = src.snapshot().clone();
        expect.library_id = target;
        assert_eq!(restored, expect);
        assert_eq!(identities(&restored), identities(src.snapshot()));
        // Only the library id text differs between the two canonical byte strings.
        let src_text = String::from_utf8(b.snapshots[&src.source_library_id()].clone()).unwrap();
        assert_eq!(
            src_text.replace(&src.source_library_id().to_string(), &target.to_string()),
            String::from_utf8(r.canonical_bytes.clone()).unwrap()
        );
        // Deterministic.
        assert_eq!(rekey_library(src, target).unwrap(), r);
    }
    // The source backup is untouched.
    assert_eq!(b.manifest, before.manifest);
    assert_eq!(b.snapshots, before.snapshots);
    assert_eq!(snaps[0].snapshot(), &library_a());
}

#[test]
fn rekey_preserves_lineage_text_links_and_folders() {
    let (_, snaps) = verify(&backup(), &Limits::DEFAULT).unwrap();
    let r = rekey_library(&snaps[0], new_id(7)).unwrap();
    let s = SyncSnapshotV1::parse(&r.canonical_bytes).unwrap();
    let p1 = &s.notebooks[0].pages[0];
    // Erased lineage: two visible fragments sharing a root that is no longer visible itself.
    let frags: Vec<_> = p1
        .strokes
        .iter()
        .filter(|x| x.root_id != x.stroke_id)
        .collect();
    assert_eq!(frags.len(), 2);
    assert_eq!(frags[0].root_id, frags[1].root_id);
    assert!(p1.strokes.iter().all(|x| x.stroke_id != frags[0].root_id));
    assert!(p1
        .strokes
        .iter()
        .any(|x| x.style == sync_protocol::StrokeStyle::PressureWidthV1));
    assert_eq!(p1.texts[0].content, "Agenda\nitem one");
    let links: Vec<_> = s
        .notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .flat_map(|p| &p.links)
        .map(|l| l.link_id)
        .collect();
    let mut want = link_ids().to_vec();
    want.sort();
    let mut got = links.clone();
    got.sort();
    assert_eq!(got, want);
    // The valid link still targets a live page; the dangling one still dangles.
    let live: Vec<_> = s
        .notebooks
        .iter()
        .flat_map(|n| n.pages.iter().map(move |p| (n.notebook_id, p.page_id)))
        .collect();
    let l1 = &p1.links[0];
    assert!(live.contains(&(l1.target_notebook_id, l1.target_page_id)));
    let dangling = &s.notebooks[0].pages[1].links[0];
    assert!(!live.contains(&(dangling.target_notebook_id, dangling.target_page_id)));
    assert_eq!(
        s.folders
            .iter()
            .map(|f| f.title.as_str())
            .collect::<Vec<_>>(),
        ["Projects", "Archive"]
    );
    assert_eq!(s.notebooks[1].folder_id, Some(s.folders[0].folder_id));
    assert_eq!(s.notebooks[2].folder_id, Some(s.folders[1].folder_id));
}

#[test]
fn rekey_refuses_to_keep_the_source_identity() {
    let (_, snaps) = verify(&backup(), &Limits::DEFAULT).unwrap();
    let e = rekey_library(&snaps[0], snaps[0].source_library_id()).unwrap_err();
    assert_eq!(e.code, C::SameLibrary);
}

// ------------------------------------------------------------------------- server genesis

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "portable-backup-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn referenced_blobs(s: &SyncSnapshotV1) -> Vec<String> {
    let mut v: Vec<String> = s
        .notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .filter_map(|p| match &p.background {
            BackgroundRef::Blob { sha256, .. } => Some(sha256.to_string()),
            BackgroundRef::Blank => None,
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

#[test]
fn rekeyed_snapshots_become_genesis_in_a_disposable_store() {
    let b = backup();
    let (verified, snaps) = verify(&b, &Limits::DEFAULT).unwrap();
    let dir = TempDir::new("genesis");
    let store = Store::open(&dir.0).unwrap();

    for (n, src) in snaps.iter().enumerate() {
        let r = rekey_library(src, new_id(n as u64)).unwrap();
        let lib = r.library_id.to_string();

        // Without its blobs the server refuses, naming exactly the required ones.
        if n == 0 {
            match store.bootstrap(&r.canonical_bytes) {
                Err(StoreError::MissingBlobs(missing)) => {
                    assert_eq!(missing, referenced_blobs(src.snapshot()))
                }
                other => panic!("expected MissingBlobs, got {other:?}"),
            }
        }
        for m in &verified.manifest().blobs {
            if referenced_blobs(src.snapshot()).contains(&m.sha256.to_string()) {
                store
                    .put_blob(m.sha256.as_str(), &b.blobs[&m.sha256])
                    .unwrap();
            }
        }
        let (info, created) = store.bootstrap(&r.canonical_bytes).unwrap();
        assert!(created);
        assert_eq!(info.library_id, r.library_id);
        assert_eq!(info.head_server_seq, 0);
        assert_eq!(info.genesis_sha256, r.restored_snapshot_sha256.to_string());
        let cur = store.snapshot(&lib).unwrap();
        assert_eq!(cur.through_server_seq, 0);
        assert_eq!(cur.snapshot_bytes, r.canonical_bytes);
        assert_eq!(cur.snapshot_sha256, r.restored_snapshot_sha256.to_string());
        assert!(store.verify_replay(&lib).unwrap());
        // Identical re-import is idempotent, never a second library.
        assert!(!store.bootstrap(&r.canonical_bytes).unwrap().1);
    }

    // The source libraries can coexist with their restored copies (distinct identities).
    let a_src = &b.snapshots[&library_a_id()];
    assert!(store.bootstrap(a_src).unwrap().1);
    let libs = store.list_libraries().unwrap();
    assert_eq!(libs.len(), 3);
    let a_restored = libs.iter().find(|l| l.library_id == new_id(0)).unwrap();
    assert_eq!((a_restored.notebook_count, a_restored.page_count), (3, 4));
    let b_restored = libs.iter().find(|l| l.library_id == new_id(1)).unwrap();
    assert_eq!((b_restored.notebook_count, b_restored.page_count), (1, 2));
}
