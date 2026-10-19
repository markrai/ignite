mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use backup_archive::{inspect_backup, RestoreErrorKind as AK, ZipErrorCode as Z};
use backup_restore::{
    remove_restore_staging, restore_backup, FailPoint, ImportErrorKind as K, RestoreOptions,
};
use common::*;
use portable_backup::{BackupErrorCode as C, Limits};
use scribe_sync_server::{Store, StoreError};
use sync_engine::testkit::{replica, Author};
use sync_protocol::{sha256_hex, LibraryId, StrokeStyle, SyncSnapshotV1};

fn opts() -> RestoreOptions {
    RestoreOptions::default()
}

fn kind<T: std::fmt::Debug>(r: Result<T, backup_restore::ImportError>) -> K {
    r.unwrap_err().kind
}

/// Run a failing restore and prove the store is exactly as before (libraries, blob files,
/// index, staging) — also after reopening it.
fn fails_without_change(
    data: &TempDir,
    store: Store,
    archive: &std::path::Path,
    map: &BTreeMap<LibraryId, LibraryId>,
    o: &RestoreOptions,
) -> (K, Store) {
    let before = observe(&store, &data.0);
    let e = restore_backup(&store, archive, map, o).unwrap_err();
    assert_eq!(observe(&store, &data.0), before, "store changed after {e}");
    drop(store);
    let store = Store::open(&data.0).unwrap();
    assert_eq!(
        observe(&store, &data.0),
        before,
        "store changed after reopen ({e})"
    );
    (e.kind, store)
}

#[test]
fn a_real_export_restores_completely_under_new_identities() {
    let backup = Backup::create();
    let plan = inspect_backup(&backup.path, &Limits::DEFAULT).unwrap();
    let data = TempDir::new("target");
    let store = Store::open(&data.0).unwrap();
    let report = restore_backup(&store, &backup.path, &mapping(), &opts()).unwrap();

    assert_eq!(report.archive_sha256, plan.archive_sha256);
    assert_eq!(
        (report.blobs_installed, report.blobs_already_present),
        (2, 0)
    );
    let libs = store.list_libraries().unwrap();
    assert_eq!(
        libs.iter().map(|l| l.library_id).collect::<Vec<_>>(),
        vec![dest(1), dest(2)]
    );
    for src in [lib_a(), lib_b()] {
        assert!(matches!(
            store.library(&src.to_string()),
            Err(StoreError::NotFound { .. })
        ));
    }
    for (planned, (restored, summary)) in plan
        .libraries
        .iter()
        .zip(report.libraries.iter().zip(&libs))
    {
        assert_eq!(restored.source_library_id, planned.source_library_id);
        assert_eq!(
            restored.source_snapshot_sha256,
            planned.source_snapshot_sha256
        );
        assert_eq!(
            (restored.notebook_count, restored.page_count),
            (planned.notebook_count, planned.page_count)
        );
        assert_eq!(
            (summary.notebook_count as u64, summary.page_count as u64),
            (planned.notebook_count, planned.page_count)
        );
        let id = restored.library_id.to_string();
        let expected = expected_restored(&backup, restored.source_library_id, restored.library_id);
        assert_eq!(
            store.genesis(&id).unwrap(),
            expected,
            "genesis = re-keyed canonical snapshot"
        );
        let cur = store.snapshot(&id).unwrap();
        assert_eq!(cur.snapshot_bytes, expected);
        assert_eq!(cur.through_server_seq, 0);
        assert_eq!(
            cur.snapshot_sha256,
            restored.restored_snapshot_sha256.to_string()
        );
        assert_eq!(cur.snapshot_sha256, sha256_hex(&expected).to_string());
        assert_eq!(store.library(&id).unwrap().head_server_seq, 0);
        assert!(
            store.tail(&id, 0, i64::MAX).unwrap().1.is_empty(),
            "no source history"
        );
        assert!(store.verify_replay(&id).unwrap());
    }

    // Content: everything but the library id equals the exported source state.
    let a = SyncSnapshotV1::parse(&store.genesis(&dest(1).to_string()).unwrap()).unwrap();
    let mut src_a = SyncSnapshotV1::parse(&backup.source_snapshot(lib_a())).unwrap();
    src_a.library_id = dest(1);
    assert_eq!(a, src_a);
    assert_eq!(
        a.folders
            .iter()
            .map(|f| f.title.as_str())
            .collect::<Vec<_>>(),
        ["Projects", "Archive"]
    );
    assert_eq!(
        a.notebooks
            .iter()
            .map(|n| n.title.as_str())
            .collect::<Vec<_>>(),
        ["Notebook 1", "Work", "Scratch"]
    );
    let p1 = &a.notebooks[0].pages[0];
    assert!(p1
        .strokes
        .iter()
        .any(|s| s.style == StrokeStyle::PressureWidthV1));
    let frags: Vec<_> = p1
        .strokes
        .iter()
        .filter(|s| s.root_id != s.stroke_id)
        .collect();
    assert_eq!(frags.len(), 2);
    assert!(
        p1.strokes.iter().all(|s| s.stroke_id != frags[0].root_id),
        "erased root stays erased"
    );
    assert_eq!(p1.texts[0].content, "Agenda\nitem one");
    assert_eq!(p1.links[0].target_page_id, a.notebooks[1].pages[0].page_id);
    let dangling = &a.notebooks[0].pages[1].links[0];
    assert!(a
        .notebooks
        .iter()
        .all(|n| n.pages.iter().all(|p| p.page_id != dangling.target_page_id)));
    assert_eq!(
        a.notebooks[0].pages[1].strokes.len(),
        3,
        "the 3 source operations are in the snapshot"
    );

    // Blobs: each stored once, readable, hash-correct, indexed.
    let state = observe(&store, &data.0);
    assert_eq!(state.blob_files.len(), 2);
    for (sha, bytes) in blobs() {
        assert_eq!(store.get_blob(sha.as_str()).unwrap(), bytes);
    }
    assert!(state.unindexed.is_empty());
    assert!(state.staging.is_empty(), "staging removed");

    // Normal sync continues on a restored library.
    let mut author = Author::new(replica(7));
    let env = stroke_env(&mut author, dest(1), 50);
    let (head, accepted) = store.submit(&dest(1).to_string(), &[env]).unwrap();
    assert_eq!((head, accepted[0].outcome.as_str()), (1, "applied"));
    assert!(store.verify_replay(&dest(1).to_string()).unwrap());

    // Reopen: still all there.
    drop(store);
    let store = Store::open(&data.0).unwrap();
    assert_eq!(store.list_libraries().unwrap().len(), 2);
    assert!(store.verify_replay(&dest(2).to_string()).unwrap());
}

#[test]
fn existing_libraries_and_blobs_are_left_byte_for_byte_unchanged() {
    let backup = Backup::create();
    let data = TempDir::new("busy");
    let store = Store::open(&data.0).unwrap();
    // Unrelated pre-existing state: the source library A itself (with history) and blob A.
    for (sha, bytes) in blobs().into_iter().take(1) {
        store.put_blob(sha.as_str(), &bytes).unwrap();
    }
    store
        .bootstrap(&library_a().to_canonical_bytes().unwrap())
        .unwrap();
    let mut author = Author::new(replica(3));
    let envs: Vec<Vec<u8>> = (0..2)
        .map(|n| stroke_env(&mut author, lib_a(), 70 + n))
        .collect();
    store.submit(&lib_a().to_string(), &envs).unwrap();
    let before = observe(&store, &data.0);
    let existing_blob = before.blob_files.clone();

    let report = restore_backup(&store, &backup.path, &mapping(), &opts()).unwrap();
    assert_eq!(
        (report.blobs_installed, report.blobs_already_present),
        (1, 1)
    );
    let after = observe(&store, &data.0);
    let id = lib_a().to_string();
    assert_eq!(
        after.libraries[&id], before.libraries[&id],
        "existing library untouched"
    );
    for (name, bytes) in &existing_blob {
        assert_eq!(
            &after.blob_files[name], bytes,
            "existing blob file untouched"
        );
    }
    assert_eq!(after.libraries.len(), 3);
    assert_eq!(after.blob_files.len(), 2, "shared blob not duplicated");
}

#[test]
fn concurrent_readers_never_see_a_subset_of_restored_libraries() {
    let backup = Backup::create();
    let data = TempDir::new("readers");
    let store = Store::open(&data.0).unwrap();
    assert!(store.bootstrap(&unrelated_library()).unwrap().1);
    let base = store.list_libraries().unwrap().len();
    let done = AtomicBool::new(false);
    /// Stops the reader even if a restore below panics (otherwise the scope never ends).
    struct StopOnDrop<'a>(&'a AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let seen = std::thread::scope(|s| {
        let reader = s.spawn(|| {
            let mut counts = std::collections::BTreeSet::new();
            while !done.load(Ordering::SeqCst) {
                counts.insert(store.list_libraries().unwrap().len());
            }
            counts.insert(store.list_libraries().unwrap().len());
            counts
        });
        let stop = StopOnDrop(&done);
        for round in 0..3u64 {
            let map = BTreeMap::from([
                (lib_a(), dest(10 + 2 * round)),
                (lib_b(), dest(11 + 2 * round)),
            ]);
            restore_backup(&store, &backup.path, &map, &opts()).unwrap();
        }
        drop(stop);
        reader.join().unwrap()
    });
    assert!(
        seen.iter().all(|n| (n - base).is_multiple_of(2)),
        "observed counts {seen:?}"
    );
    assert_eq!(seen.last(), Some(&(base + 6)));
}

#[test]
fn mapping_errors_change_nothing() {
    let backup = Backup::create();
    let data = TempDir::new("mapping");
    let mut store = Store::open(&data.0).unwrap();
    store
        .bootstrap(&library_b().to_canonical_bytes().unwrap())
        .ok();
    let cases: Vec<(BTreeMap<LibraryId, LibraryId>, K)> = vec![
        (BTreeMap::new(), K::Mapping),
        (BTreeMap::from([(lib_a(), dest(1))]), K::Mapping), // incomplete
        (
            BTreeMap::from([(lib_a(), dest(1)), (lib_b(), dest(2)), (dest(5), dest(6))]),
            K::Mapping,
        ), // extra
        (
            BTreeMap::from([(lib_a(), dest(1)), (lib_b(), dest(1))]),
            K::Mapping,
        ), // duplicate
        (
            BTreeMap::from([(lib_a(), lib_a()), (lib_b(), dest(2))]),
            K::Mapping,
        ), // original id
        (
            BTreeMap::from([(lib_a(), dest(1)), (lib_b(), lib_a())]),
            K::Mapping,
        ), // another source id
        (
            BTreeMap::from([(lib_a(), dest(1)), (lib_b(), lib_b())]),
            K::Mapping,
        ),
    ];
    for (map, want) in cases {
        let (got, s) = fails_without_change(&data, store, &backup.path, &map, &opts());
        assert_eq!(got, want, "{map:?}");
        store = s;
    }
    // A destination that already exists in the store.
    let map = BTreeMap::from([(lib_a(), dest(1)), (lib_b(), dest(2))]);
    restore_backup(&store, &backup.path, &map, &opts()).unwrap();
    let (got, s) = fails_without_change(&data, store, &backup.path, &map, &opts());
    assert_eq!(got, K::Conflict, "retry with published ids");
    let partial = BTreeMap::from([(lib_a(), dest(20)), (lib_b(), dest(2))]);
    let (got, _) = fails_without_change(&data, s, &backup.path, &partial, &opts());
    assert_eq!(got, K::Conflict);
}

#[test]
fn invalid_archives_change_nothing() {
    let backup = Backup::create();
    let data = TempDir::new("archive");
    let mut store = Store::open(&data.0).unwrap();
    let map = mapping();
    let entries = backup.entries();
    let snap_a = format!("libraries/{}/snapshot.json", lib_a());
    let first_blob = entries
        .iter()
        .find(|(n, _)| n.starts_with("blobs/"))
        .unwrap()
        .0
        .clone();

    let garbage = backup.dir.join("garbage.zip");
    std::fs::write(&garbage, b"not a zip at all").unwrap();
    let corrupt_snapshot = backup.variant("snap.zip", |e| {
        let s = &mut e.iter_mut().find(|(n, _)| *n == snap_a).unwrap().1;
        let i = s.iter().position(|c| *c == b'1').unwrap();
        s[i] = b'2';
    });
    let corrupt_blob = backup.variant("blob.zip", |e| {
        e.iter_mut().find(|(n, _)| *n == first_blob).unwrap().1[5] ^= 0xff;
    });
    let missing_blob = backup.variant("missing.zip", |e| e.retain(|(n, _)| *n != first_blob));
    let extra_entry = backup.variant("extra.zip", |e| {
        let x = vec![1u8; 10];
        e.push((format!("blobs/{}", sha256_hex(&x)), x));
    });
    let future = backup.variant("future.zip", |e| {
        let m = String::from_utf8(e[0].1.clone()).unwrap();
        e[0].1 = m
            .replacen("\"format_version\":1", "\"format_version\":2", 1)
            .into_bytes();
    });
    let cases: Vec<(std::path::PathBuf, RestoreOptions, K)> = vec![
        (garbage, opts(), K::Archive(AK::Zip(Z::NotZip))),
        (
            corrupt_snapshot,
            opts(),
            K::Archive(AK::Backup(C::HashMismatch)),
        ),
        (
            corrupt_blob,
            opts(),
            K::Archive(AK::Backup(C::HashMismatch)),
        ),
        (missing_blob, opts(), K::Archive(AK::Backup(C::MissingBlob))),
        (
            extra_entry,
            opts(),
            K::Archive(AK::Backup(C::UnexpectedEntry)),
        ),
        (
            future,
            opts(),
            K::Archive(AK::Backup(C::UnsupportedVersion)),
        ),
        (
            backup.path.clone(),
            RestoreOptions {
                max_archive_bytes: Some(1000),
                ..opts()
            },
            K::TooLarge,
        ),
        (
            backup.path.clone(),
            RestoreOptions {
                limits: Limits {
                    max_blob_bytes: 1000,
                    ..Limits::DEFAULT
                },
                ..opts()
            },
            K::Archive(AK::Zip(Z::LimitExceeded)),
        ),
        (backup.dir.join("does-not-exist.zip"), opts(), K::Io),
        (backup.dir.0.clone(), opts(), K::Io),
    ];
    for (path, o, want) in cases {
        let (got, s) = fails_without_change(&data, store, &path, &map, &o);
        assert_eq!(got, want, "{}", path.display());
        store = s;
    }
}

#[test]
fn a_changed_archive_after_inspection_is_verified_again() {
    let backup = Backup::create();
    let plan = inspect_backup(&backup.path, &Limits::DEFAULT).unwrap();
    // Replace the file with a tampered variant after the plan was made.
    let tampered = backup.variant("t.zip", |e| {
        let last = e.len() - 1;
        e[last].1[0] ^= 1;
    });
    std::fs::copy(&tampered, &backup.path).unwrap();
    let data = TempDir::new("toctou");
    let store = Store::open(&data.0).unwrap();
    let (got, _) = fails_without_change(&data, store, &backup.path, &mapping(), &opts());
    assert_eq!(got, K::Archive(AK::Backup(C::HashMismatch)));
    assert_ne!(
        sha256_hex(&std::fs::read(&backup.path).unwrap()),
        plan.archive_sha256
    );
}

#[test]
fn a_corrupt_or_unreadable_existing_blob_is_refused_and_left_untouched() {
    let backup = Backup::create();
    let all = blobs();
    let (sha_b, bytes_b) = all
        .iter()
        .last()
        .map(|(s, b)| (s.clone(), b.clone()))
        .unwrap();

    // Same length, different bytes, at blob B's path (no index row).
    let data = TempDir::new("corrupt");
    let store = Store::open(&data.0).unwrap();
    let path = blob_file(&data.0, &sha_b);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut evil = bytes_b.clone();
    evil[0] ^= 0xff;
    std::fs::write(&path, &evil).unwrap();
    let e = restore_backup(&store, &backup.path, &mapping(), &opts()).unwrap_err();
    assert_eq!(e.kind, K::Conflict, "{e}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        evil,
        "existing file untouched"
    );
    assert!(store.list_libraries().unwrap().is_empty());

    // A directory where blob B's file should go (filesystem refusal).
    let data = TempDir::new("fsfail");
    let store = Store::open(&data.0).unwrap();
    std::fs::create_dir_all(blob_file(&data.0, &sha_b)).unwrap();
    let e = restore_backup(&store, &backup.path, &mapping(), &opts()).unwrap_err();
    assert!(matches!(e.kind, K::Store(_) | K::Conflict), "{e}");
    assert!(store.list_libraries().unwrap().is_empty());
    // Whatever was installed before the failure is unindexed and hash-correct (checked here).
    for name in store.unindexed_blob_files().unwrap() {
        let sha = sync_protocol::Sha256Hex::parse(&name).unwrap();
        assert_eq!(
            sha256_hex(&std::fs::read(blob_file(&data.0, &sha)).unwrap()),
            sha
        );
    }
    assert!(observe(&store, &data.0).staging.is_empty());

    // Staging cannot be created at all.
    let data = TempDir::new("nostaging");
    let store = Store::open(&data.0).unwrap();
    std::fs::write(data.join("restore-staging"), b"a file, not a directory").unwrap();
    let before = observe(&store, &data.0);
    assert!(matches!(
        kind(restore_backup(&store, &backup.path, &mapping(), &opts())),
        K::Store(_)
    ));
    assert_eq!(observe(&store, &data.0), before);
}

#[test]
fn a_failure_inside_the_library_transaction_publishes_nothing() {
    let backup = Backup::create();
    let data = TempDir::new("txfail");
    let store = Store::open(&data.0).unwrap();
    assert!(store.bootstrap(&unrelated_library()).unwrap().1); // unrelated existing
    let before = observe(&store, &data.0);
    // Fail after the FIRST library row pair is inserted, before COMMIT.
    store
        .hooks
        .fail_bootstrap_many_after_libraries
        .store(1, Ordering::SeqCst);
    let e = restore_backup(&store, &backup.path, &mapping(), &opts()).unwrap_err();
    assert_eq!(e.kind, K::Store("internal".into()), "{e}");
    let after = observe(&store, &data.0);
    assert_eq!(
        after.libraries, before.libraries,
        "neither restored library is visible"
    );
    for id in [dest(1), dest(2)] {
        assert!(matches!(
            store.library(&id.to_string()),
            Err(StoreError::NotFound { .. })
        ));
    }
    // Blob files were installed before the transaction: hash-checked, unindexed, unreferenced.
    let shas: Vec<String> = blobs().keys().map(|s| s.to_string()).collect();
    assert_eq!(after.unindexed, shas);
    assert!(after.staging.is_empty());

    // Reopen, then retry (hook off): succeeds and reuses the orphaned files.
    store
        .hooks
        .fail_bootstrap_many_after_libraries
        .store(0, Ordering::SeqCst);
    drop(store);
    let store = Store::open(&data.0).unwrap();
    assert_eq!(observe(&store, &data.0).libraries, before.libraries);
    let report = restore_backup(&store, &backup.path, &mapping(), &opts()).unwrap();
    assert_eq!(
        (report.blobs_installed, report.blobs_already_present),
        (0, 2)
    );
    assert!(store.unindexed_blob_files().unwrap().is_empty());
    assert_eq!(store.list_libraries().unwrap().len(), 3);
}

#[test]
fn a_failed_blob_directory_sync_stops_before_publication() {
    let backup = Backup::create();
    // 1 = the blob's prefix directory, 2 = the blob root, 3 = the data directory.
    for n in 1..=3 {
        let data = TempDir::new("dirsync");
        let store = Store::open(&data.0).unwrap();
        assert!(store.bootstrap(&unrelated_library()).unwrap().1);
        let before = observe(&store, &data.0);
        store.hooks.fail_blob_dir_sync.store(n, Ordering::SeqCst);
        let e = restore_backup(&store, &backup.path, &mapping(), &opts()).unwrap_err();
        assert_eq!(e.kind, K::Store("internal".into()), "{n}: {e}");
        assert!(e.message.contains("injected failure"), "{n}: {e}");
        let after = observe(&store, &data.0);
        assert_eq!(after.libraries, before.libraries, "{n}: nothing published");
        // The first blob was linked before its sync failed: one hash-correct orphan, no more.
        assert_eq!(after.unindexed.len(), 1, "{n}");
        let sha = sync_protocol::Sha256Hex::parse(&after.unindexed[0]).unwrap();
        assert!(blobs().contains_key(&sha));
        assert_eq!(
            sha256_hex(&std::fs::read(blob_file(&data.0, &sha)).unwrap()),
            sha
        );
        assert!(after.staging.is_empty());

        // A retry finds the orphan already in place, but must still sync before trusting it.
        let e = restore_backup(&store, &backup.path, &mapping(), &opts()).unwrap_err();
        assert!(e.message.contains("injected failure"), "{n}: {e}");
        assert_eq!(
            observe(&store, &data.0),
            after,
            "{n}: retry changed nothing"
        );

        // Once syncing works: reopen, retry, and the orphan is reused.
        store.hooks.fail_blob_dir_sync.store(0, Ordering::SeqCst);
        drop(store);
        let store = Store::open(&data.0).unwrap();
        let report = restore_backup(&store, &backup.path, &mapping(), &opts()).unwrap();
        assert_eq!(
            (report.blobs_installed, report.blobs_already_present),
            (1, 1)
        );
        assert!(store.unindexed_blob_files().unwrap().is_empty());
        assert_eq!(store.list_libraries().unwrap().len(), 3);
    }
}

#[test]
fn simulated_crashes_before_publication_leave_only_recoverable_leftovers() {
    let backup = Backup::create();
    for (point, blobs_expected) in [
        (FailPoint::AfterVerification, 0usize),
        (FailPoint::AfterBlobInstall, 2),
    ] {
        let data = TempDir::new("crash");
        let store = Store::open(&data.0).unwrap();
        let o = RestoreOptions {
            fail_point: Some(point),
            ..opts()
        };
        assert_eq!(
            kind(restore_backup(&store, &backup.path, &mapping(), &o)),
            K::Interrupted
        );
        drop(store); // "process exit"
        let store = Store::open(&data.0).unwrap();
        let s = observe(&store, &data.0);
        assert!(s.libraries.is_empty(), "{point:?}: nothing published");
        assert_eq!(s.blob_files.len(), blobs_expected);
        assert_eq!(
            s.unindexed.len(),
            blobs_expected,
            "installed files are unindexed orphans"
        );
        assert_eq!(s.staging.len(), 1, "the crashed job's staging remains");
        assert_eq!(remove_restore_staging(&store).unwrap(), 1);
        assert!(observe(&store, &data.0).staging.is_empty());
        restore_backup(&store, &backup.path, &mapping(), &opts()).unwrap();
        let s = observe(&store, &data.0);
        assert_eq!(s.libraries.len(), 2);
        assert!(s.unindexed.is_empty());
        for id in [dest(1), dest(2)] {
            assert!(store.verify_replay(&id.to_string()).unwrap());
        }
    }
}

#[test]
fn bootstrap_many_is_all_or_nothing_and_never_merges() {
    let data = TempDir::new("many");
    let store = Store::open(&data.0).unwrap();
    let mut a = library_a();
    a.library_id = dest(1);
    let a = a.to_canonical_bytes().unwrap();
    let mut c = library_a();
    c.library_id = dest(3);
    c.notebooks[0].pages[0].background = sync_protocol::BackgroundRef::Blank;
    c.notebooks[1].pages[0].background = sync_protocol::BackgroundRef::Blank;
    let c = c.to_canonical_bytes().unwrap();
    // Blob not on disk → refused before any library.
    let (sha, bytes) = blobs().into_iter().next().unwrap();
    let e = store
        .bootstrap_many(&[(sha.clone(), bytes.len() as u64)], 1, |_| Ok(a.clone()))
        .unwrap_err();
    assert!(matches!(e, StoreError::MissingBlobs(_)));
    // Second library is invalid → the first is rolled back too.
    let e = store
        .bootstrap_many(&[], 2, |i| {
            Ok(if i == 0 { c.clone() } else { b"{}".to_vec() })
        })
        .unwrap_err();
    assert!(matches!(e, StoreError::BadRequest { .. }), "{e}");
    assert!(store.list_libraries().unwrap().is_empty());
    // Same id twice in one batch.
    let e = store.bootstrap_many(&[], 2, |_| Ok(c.clone())).unwrap_err();
    assert!(matches!(
        e,
        StoreError::BadRequest {
            code: "duplicate_library",
            ..
        }
    ));
    // Success, then an identical retry is a conflict (never idempotent, never merged).
    assert_eq!(
        store
            .bootstrap_many(&[], 1, |_| Ok(c.clone()))
            .unwrap()
            .len(),
        1
    );
    let e = store.bootstrap_many(&[], 1, |_| Ok(c.clone())).unwrap_err();
    assert!(matches!(
        e,
        StoreError::Conflict {
            code: "library_exists",
            ..
        }
    ));
    // The single-library bootstrap keeps its idempotent behavior.
    assert!(!store.bootstrap(&c).unwrap().1);
}

fn flip_first_byte_of_every_file(dir: &std::path::Path) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        let mut b = std::fs::read(&p).unwrap();
        b[0] ^= 0x20;
        std::fs::write(&p, b).unwrap();
    }
}

#[test]
fn staged_files_are_rechecked_before_use() {
    let backup = Backup::create();
    // Staged blobs tampered with after verification, before installation.
    let data = TempDir::new("tamper-blob");
    let store = Store::open(&data.0).unwrap();
    let o = RestoreOptions {
        staging_hook: Some((FailPoint::AfterVerification, |d| {
            flip_first_byte_of_every_file(&d.join("blobs"))
        })),
        ..opts()
    };
    let (got, _) = fails_without_change(&data, store, &backup.path, &mapping(), &o);
    assert_eq!(got, K::Store("hash_mismatch".into()));

    // Staged libraries tampered with after blob installation, before publication.
    let data = TempDir::new("tamper-lib");
    let store = Store::open(&data.0).unwrap();
    let o = RestoreOptions {
        staging_hook: Some((FailPoint::AfterBlobInstall, |d| {
            flip_first_byte_of_every_file(&d.join("libraries"))
        })),
        ..opts()
    };
    let e = restore_backup(&store, &backup.path, &mapping(), &o).unwrap_err();
    assert_eq!(e.kind, K::Store("internal".into()), "{e}");
    assert!(e.message.contains("changed after verification"), "{e}");
    assert!(store.list_libraries().unwrap().is_empty());
    assert!(observe(&store, &data.0).staging.is_empty());
}

#[test]
fn install_blob_file_never_trusts_or_overwrites() {
    let data = TempDir::new("install");
    let store = Store::open(&data.0).unwrap();
    let (sha, bytes) = blobs().into_iter().next().unwrap();
    let staged = data.join("staged");
    std::fs::write(&staged, &bytes).unwrap();
    let len = bytes.len() as u64;

    // Staged bytes that do not match the claimed hash or length.
    let wrong = data.join("wrong");
    let mut w = bytes.clone();
    w[0] ^= 1;
    std::fs::write(&wrong, &w).unwrap();
    assert!(matches!(
        store.install_blob_file(&sha, len, &wrong),
        Err(StoreError::BadRequest {
            code: "hash_mismatch",
            ..
        })
    ));
    assert!(matches!(
        store.install_blob_file(&sha, len + 1, &staged),
        Err(StoreError::BadRequest {
            code: "hash_mismatch",
            ..
        })
    ));
    assert!(!blob_file(&data.0, &sha).exists());

    // Installed once, then verified as existing (the file is not replaced).
    use scribe_sync_server::BlobInstall;
    assert_eq!(
        store.install_blob_file(&sha, len, &staged).unwrap(),
        BlobInstall::Installed
    );
    let modified = std::fs::metadata(blob_file(&data.0, &sha))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        store.install_blob_file(&sha, len, &staged).unwrap(),
        BlobInstall::Existing
    );
    assert_eq!(
        std::fs::metadata(blob_file(&data.0, &sha))
            .unwrap()
            .modified()
            .unwrap(),
        modified
    );
    assert_eq!(store.unindexed_blob_files().unwrap(), vec![sha.to_string()]);

    // Indexed but the file is gone: a damaged store is refused, not silently repaired.
    let (sha2, bytes2) = blobs().into_iter().next_back().unwrap();
    store.put_blob(sha2.as_str(), &bytes2).unwrap();
    std::fs::remove_file(blob_file(&data.0, &sha2)).unwrap();
    let staged2 = data.join("staged2");
    std::fs::write(&staged2, &bytes2).unwrap();
    assert!(matches!(
        store.install_blob_file(&sha2, bytes2.len() as u64, &staged2),
        Err(StoreError::Conflict {
            code: "blob_missing",
            ..
        })
    ));
    assert!(!blob_file(&data.0, &sha2).exists());
}
