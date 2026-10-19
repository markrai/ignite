//! `Store::put_blob`: safe publication through `install_blob_file` (exclusive temporary file,
//! re-hash, file and directory sync, no-overwrite hard link) before the index row, plus the
//! preserved `created` contract (true = this call inserted the index row).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Barrier;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Value;
use scribe_sync_server::{Store, StoreError};
use sync_engine::testkit;
use sync_protocol::sha256_hex;

// ------------------------------------------------------------------ harness

static N: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "scribe-blobs-{label}-{n}-{}",
            N.fetch_add(1, Ordering::SeqCst)
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

fn blob_file(data: &Path, sha: &str) -> PathBuf {
    data.join("blobs").join(&sha[..2]).join(sha)
}

fn staging(data: &Path) -> Vec<String> {
    match std::fs::read_dir(data.join("upload-staging")) {
        Ok(d) => {
            let mut v: Vec<String> = d
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        }
        Err(_) => Vec::new(),
    }
}

/// Every row of the library and blob tables, every file under `blobs/`, and upload staging.
#[derive(Debug, PartialEq)]
struct State {
    tables: BTreeMap<&'static str, Vec<Vec<Value>>>,
    files: BTreeMap<String, Vec<u8>>,
    staging: Vec<String>,
}

fn observe(data: &Path) -> State {
    let c = rusqlite::Connection::open(data.join("ledger.sqlite3")).unwrap();
    let mut tables = BTreeMap::new();
    for t in ["libraries", "current_state", "operations", "blobs"] {
        let mut st = c.prepare(&format!("SELECT * FROM {t} ORDER BY 1")).unwrap();
        let n = st.column_count();
        let rows = st
            .query_map([], |r| (0..n).map(|i| r.get::<_, Value>(i)).collect())
            .unwrap()
            .collect::<Result<Vec<Vec<Value>>, _>>()
            .unwrap();
        tables.insert(t, rows);
    }
    let mut files = BTreeMap::new();
    for d in std::fs::read_dir(data.join("blobs")).unwrap().flatten() {
        for f in std::fs::read_dir(d.path()).unwrap().flatten() {
            let rel = format!(
                "{}/{}",
                d.file_name().to_string_lossy(),
                f.file_name().to_string_lossy()
            );
            files.insert(rel, std::fs::read(f.path()).unwrap());
        }
    }
    State {
        tables,
        files,
        staging: staging(data),
    }
}

fn index_row(data: &Path, sha: &str) -> Option<(i64, String)> {
    let c = rusqlite::Connection::open(data.join("ledger.sqlite3")).unwrap();
    c.query_row(
        "SELECT byte_len, rel_path FROM blobs WHERE sha256 = ?1",
        [sha],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

/// A store holding an unrelated library and its background blob (left alone by every test).
fn store_with_unrelated(data: &Path) -> Store {
    let store = Store::open(data).unwrap();
    let (bytes, _) = testkit::blob_background();
    assert!(store.put_blob(sha256_hex(&bytes).as_str(), &bytes).unwrap());
    let genesis = testkit::genesis().to_canonical_bytes().unwrap();
    assert!(store.bootstrap(&genesis).unwrap().1);
    store
}

fn upload(n: u8) -> (String, Vec<u8>) {
    let bytes: Vec<u8> = (0..5000u32).map(|i| (i as u8) ^ n).collect();
    (sha256_hex(&bytes).to_string(), bytes)
}

/// `before` and `after` agree except for (at most) the file `allowed` under `blobs/`.
fn unchanged_except_file(before: &State, after: &State, allowed: &str) {
    let mut a = after.files.clone();
    a.remove(&format!("{}/{allowed}", &allowed[..2]));
    assert_eq!(a, before.files, "no other blob file changed");
    assert_eq!(after.tables, before.tables, "no row changed");
    assert_eq!(after.staging, before.staging);
}

fn assert_published(store: &Store, data: &Path, sha: &str, bytes: &[u8]) {
    assert_eq!(std::fs::read(blob_file(data, sha)).unwrap(), bytes);
    assert_eq!(sha256_hex(bytes).as_str(), sha);
    assert_eq!(
        index_row(data, sha),
        Some((bytes.len() as i64, format!("{}/{sha}", &sha[..2])))
    );
    assert_eq!(store.get_blob(sha).unwrap(), bytes);
    assert!(!store
        .unindexed_blob_files()
        .unwrap()
        .contains(&sha.to_string()));
}

fn is_conflict(e: &StoreError, want: &str) -> bool {
    matches!(e, StoreError::Conflict { code, .. } if *code == want)
}

// ------------------------------------------------------------------ tests

#[test]
fn new_then_duplicate_upload_keeps_the_created_contract() {
    let data = TempDir::new("dup");
    let store = store_with_unrelated(&data.0);
    let (sha, bytes) = upload(1);
    assert!(store.put_blob(&sha, &bytes).unwrap(), "new → created");
    assert_published(&store, &data.0, &sha, &bytes);
    assert!(staging(&data.0).is_empty(), "temporary removed");
    let before = observe(&data.0);
    let modified = std::fs::metadata(blob_file(&data.0, &sha))
        .unwrap()
        .modified()
        .unwrap();
    assert!(
        !store.put_blob(&sha, &bytes).unwrap(),
        "identical → not created"
    );
    assert_eq!(observe(&data.0), before, "nothing replaced");
    assert_eq!(
        std::fs::metadata(blob_file(&data.0, &sha))
            .unwrap()
            .modified()
            .unwrap(),
        modified
    );
    // Reopen: still indexed and retrievable.
    drop(store);
    let store = Store::open(&data.0).unwrap();
    assert_published(&store, &data.0, &sha, &bytes);
    assert!(!store.put_blob(&sha, &bytes).unwrap());
    // The URL hash is still checked first.
    let (other, _) = upload(2);
    assert!(matches!(
        store.put_blob(&other, &bytes),
        Err(StoreError::BadRequest {
            code: "hash_mismatch",
            ..
        })
    ));
    assert_eq!(observe(&data.0), before);
}

#[test]
fn an_unindexed_file_is_rechecked_then_indexed_and_a_wrong_one_refused() {
    let data = TempDir::new("orphan");
    let store = store_with_unrelated(&data.0);
    let (sha, bytes) = upload(3);
    let path = blob_file(&data.0, &sha);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // An orphan with the wrong bytes: refused, left untouched, not indexed.
    std::fs::write(&path, b"not the bytes").unwrap();
    let before = observe(&data.0);
    let e = store.put_blob(&sha, &bytes).unwrap_err();
    assert!(is_conflict(&e, "blob_conflict"), "{e}");
    assert_eq!(observe(&data.0), before);
    // A correct orphan (as a crash leaves it): re-hashed, synced, indexed; counts as created.
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(store.unindexed_blob_files().unwrap(), vec![sha.clone()]);
    assert!(store.put_blob(&sha, &bytes).unwrap());
    assert_published(&store, &data.0, &sha, &bytes);
    assert!(staging(&data.0).is_empty());
}

#[test]
fn a_corrupt_indexed_blob_is_refused_and_left_untouched() {
    let data = TempDir::new("corrupt");
    let store = store_with_unrelated(&data.0);
    let (sha, bytes) = upload(4);
    assert!(store.put_blob(&sha, &bytes).unwrap());
    std::fs::write(blob_file(&data.0, &sha), b"tampered").unwrap();
    let before = observe(&data.0);
    let e = store.put_blob(&sha, &bytes).unwrap_err();
    assert!(is_conflict(&e, "blob_conflict"), "{e}");
    assert_eq!(
        observe(&data.0),
        before,
        "file not repaired, temporary removed"
    );
}

#[test]
fn an_indexed_blob_whose_file_is_missing_is_refused_not_repaired() {
    let data = TempDir::new("missing");
    let store = store_with_unrelated(&data.0);
    let (sha, bytes) = upload(5);
    assert!(store.put_blob(&sha, &bytes).unwrap());
    std::fs::remove_file(blob_file(&data.0, &sha)).unwrap();
    let before = observe(&data.0);
    let e = store.put_blob(&sha, &bytes).unwrap_err();
    assert!(is_conflict(&e, "blob_missing"), "{e}");
    assert_eq!(observe(&data.0), before);
    assert!(!blob_file(&data.0, &sha).exists());
}

#[test]
fn inconsistent_index_metadata_is_refused_not_trusted() {
    for (column, value) in [("byte_len", "1"), ("rel_path", "'elsewhere'")] {
        let data = TempDir::new("meta");
        let store = store_with_unrelated(&data.0);
        let (sha, bytes) = upload(6);
        assert!(store.put_blob(&sha, &bytes).unwrap());
        rusqlite::Connection::open(data.0.join("ledger.sqlite3"))
            .unwrap()
            .execute(
                &format!("UPDATE blobs SET {column} = {value} WHERE sha256 = ?1"),
                [&sha],
            )
            .unwrap();
        let before = observe(&data.0);
        let e = store.put_blob(&sha, &bytes).unwrap_err();
        assert!(is_conflict(&e, "blob_conflict"), "{column}: {e}");
        assert_eq!(observe(&data.0), before, "{column}: row left as it was");
    }
}

#[test]
fn concurrent_identical_uploads_agree_and_exactly_one_creates() {
    let data = TempDir::new("same");
    let store = store_with_unrelated(&data.0);
    let (sha, bytes) = upload(7);
    let barrier = Barrier::new(8);
    let results: Vec<Result<bool, StoreError>> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..8)
            .map(|_| {
                s.spawn(|| {
                    barrier.wait();
                    store.put_blob(&sha, &bytes)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let created = results.iter().filter(|r| matches!(r, Ok(true))).count();
    assert!(results.iter().all(|r| r.is_ok()), "{results:?}");
    assert_eq!(created, 1, "{results:?}");
    assert_published(&store, &data.0, &sha, &bytes);
    assert!(staging(&data.0).is_empty());
}

#[test]
fn concurrent_uploads_of_different_blobs_never_collide() {
    let data = TempDir::new("many");
    let store = store_with_unrelated(&data.0);
    let blobs: Vec<(String, Vec<u8>)> = (10..26).map(upload).collect();
    let barrier = Barrier::new(blobs.len());
    std::thread::scope(|s| {
        for (sha, bytes) in &blobs {
            let (store, barrier) = (&store, &barrier);
            s.spawn(move || {
                barrier.wait();
                assert!(store.put_blob(sha, bytes).unwrap());
            });
        }
    });
    for (sha, bytes) in &blobs {
        assert_published(&store, &data.0, sha, bytes);
    }
    assert!(staging(&data.0).is_empty());
}

#[test]
fn temporary_names_never_reuse_an_existing_file() {
    let data = TempDir::new("names");
    let store = store_with_unrelated(&data.0);
    // Files sitting on the next names a fresh store would pick (another upload's, a leftover).
    let dir = data.0.join("upload-staging");
    std::fs::create_dir_all(&dir).unwrap();
    let pid = std::process::id();
    let planted: Vec<PathBuf> = (0..4)
        .map(|n| dir.join(format!("put-{pid}-{n}.tmp")))
        .collect();
    for p in &planted {
        std::fs::write(p, b"someone else's upload").unwrap();
    }
    let (sha, bytes) = upload(30);
    assert!(store.put_blob(&sha, &bytes).unwrap());
    assert_published(&store, &data.0, &sha, &bytes);
    for p in &planted {
        assert_eq!(std::fs::read(p).unwrap(), b"someone else's upload");
    }
    assert_eq!(
        staging(&data.0).len(),
        planted.len(),
        "only its own name removed"
    );
    // A reopened store (process lock held: no upload can be running) clears leftovers only.
    drop(store);
    let store = Store::open(&data.0).unwrap();
    assert!(staging(&data.0).is_empty());
    assert_published(&store, &data.0, &sha, &bytes);
}

#[test]
fn a_failure_to_stage_changes_nothing() {
    let data = TempDir::new("nostage");
    let store = store_with_unrelated(&data.0);
    std::fs::remove_dir(data.0.join("upload-staging")).unwrap(); // empty after the setup upload
    std::fs::write(data.0.join("upload-staging"), b"a file, not a directory").unwrap();
    let before = observe(&data.0);
    let (sha, bytes) = upload(31);
    let e = store.put_blob(&sha, &bytes).unwrap_err();
    assert!(matches!(e, StoreError::Internal(_)), "{e}");
    assert_eq!(observe(&data.0), before);
    assert_eq!(index_row(&data.0, &sha), None);
}

#[test]
fn a_failed_file_sync_links_and_indexes_nothing() {
    let data = TempDir::new("filesync");
    let store = store_with_unrelated(&data.0);
    let before = observe(&data.0);
    let (sha, bytes) = upload(32);
    store
        .hooks
        .fail_blob_file_sync
        .store(true, Ordering::SeqCst);
    let e = store.put_blob(&sha, &bytes).unwrap_err();
    assert!(e.to_string().contains("injected failure"), "{e}");
    assert_eq!(observe(&data.0), before, "no file, no row, no temporary");
    store
        .hooks
        .fail_blob_file_sync
        .store(false, Ordering::SeqCst);
    assert!(store.put_blob(&sha, &bytes).unwrap());
    assert_published(&store, &data.0, &sha, &bytes);
}

#[test]
fn a_failed_directory_sync_indexes_nothing_until_a_retry_succeeds() {
    // 1 = the blob's prefix directory, 2 = the blob root, 3 = the data directory.
    for n in 1..=3 {
        let data = TempDir::new("dirsync");
        let store = store_with_unrelated(&data.0);
        let before = observe(&data.0);
        let (sha, bytes) = upload(33);
        store.hooks.fail_blob_dir_sync.store(n, Ordering::SeqCst);
        let e = store.put_blob(&sha, &bytes).unwrap_err();
        assert!(e.to_string().contains("injected failure"), "{n}: {e}");
        // The link was made before its sync failed: a hash-correct orphan, never indexed.
        let after = observe(&data.0);
        unchanged_except_file(&before, &after, &sha);
        assert_eq!(index_row(&data.0, &sha), None, "{n}");
        assert_eq!(
            sha256_hex(&std::fs::read(blob_file(&data.0, &sha)).unwrap()).as_str(),
            sha
        );
        assert_eq!(store.unindexed_blob_files().unwrap(), vec![sha.clone()]);
        assert!(matches!(
            store.get_blob(&sha),
            Err(StoreError::NotFound { .. })
        ));

        // Retry while syncing still fails: the orphan is found but still not trusted.
        let e = store.put_blob(&sha, &bytes).unwrap_err();
        assert!(e.to_string().contains("injected failure"), "{n}: {e}");
        assert_eq!(observe(&data.0), after, "{n}: retry changed nothing");

        // Cleared, reopened: the retry re-hashes, syncs and indexes the orphan.
        store.hooks.fail_blob_dir_sync.store(0, Ordering::SeqCst);
        drop(store);
        let store = Store::open(&data.0).unwrap();
        assert!(store.put_blob(&sha, &bytes).unwrap(), "{n}");
        assert_published(&store, &data.0, &sha, &bytes);
        let mut expected = before.files.clone();
        expected.insert(format!("{}/{sha}", &sha[..2]), bytes.clone());
        assert_eq!(observe(&data.0).files, expected);
    }
}

/// testkit's genesis (it references the background blob) under a new library id.
fn restored_genesis(n: u64) -> Vec<u8> {
    let mut g = testkit::genesis();
    g.library_id = sync_protocol::LibraryId::from_uuid(testkit::uuid7(9, n)).unwrap();
    g.to_canonical_bytes().unwrap()
}

#[test]
fn bootstrap_many_accepts_only_consistent_blob_index_rows() {
    let (bg, _) = testkit::blob_background();
    let bg_sha = sha256_hex(&bg);
    let (new_sha, new_bytes) = upload(40);
    let new_sha = sync_protocol::Sha256Hex::parse(&new_sha).unwrap();
    // The unindexed blob comes first, so its row is inserted before the existing (possibly
    // inconsistent) background row is checked.
    let batch = [
        (new_sha.clone(), new_bytes.len() as u64),
        (bg_sha.clone(), bg.len() as u64),
    ];
    for edit in [
        None,
        Some("rel_path = 'elsewhere'"),
        Some("rel_path = upper(rel_path)"),
        Some("byte_len = 1"),
    ] {
        let data = TempDir::new("many-index");
        let store = store_with_unrelated(&data.0);
        let staged = data.0.join("staged");
        std::fs::write(&staged, &new_bytes).unwrap();
        store
            .install_blob_file(&new_sha, new_bytes.len() as u64, &staged)
            .unwrap();
        std::fs::remove_file(&staged).unwrap();
        if let Some(set) = edit {
            rusqlite::Connection::open(data.0.join("ledger.sqlite3"))
                .unwrap()
                .execute(
                    &format!("UPDATE blobs SET {set} WHERE sha256 = ?1"),
                    [bg_sha.as_str()],
                )
                .unwrap();
        }
        let before = observe(&data.0);
        let r = store.bootstrap_many(&batch, 2, |i| Ok(restored_genesis(i as u64 + 1)));
        let ids = [1, 2].map(|n| {
            sync_protocol::LibraryId::from_uuid(testkit::uuid7(9, n))
                .unwrap()
                .to_string()
        });
        let Some(set) = edit else {
            // Consistent existing row accepted; the missing row inserted with the canonical path.
            assert_eq!(r.unwrap().len(), 2);
            assert_published(&store, &data.0, new_sha.as_str(), &new_bytes);
            assert_published(&store, &data.0, bg_sha.as_str(), &bg);
            for id in &ids {
                assert!(store.verify_replay(id).unwrap());
            }
            continue;
        };
        let e = r.unwrap_err();
        assert!(is_conflict(&e, "blob_conflict"), "{set}: {e}");
        // The whole batch rolled back: the earlier-inserted index row too; the inconsistent
        // row, every file and every existing library left exactly as they were.
        assert_eq!(index_row(&data.0, new_sha.as_str()), None, "{set}");
        assert_eq!(observe(&data.0), before, "{set}");
        drop(store);
        let store = Store::open(&data.0).unwrap();
        assert_eq!(observe(&data.0), before, "{set}: after reopening");
        for id in &ids {
            assert!(matches!(
                store.library(id),
                Err(StoreError::NotFound { .. })
            ));
        }
    }
}
