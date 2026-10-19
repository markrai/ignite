//! Disposable source server → real Slice 1C export → archive on disk, plus target-store
//! helpers for observing that nothing changed.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use backup_export::{export_backup, Credential, HttpSource, Secret};
use portable_backup::Limits;
use scribe_sync_server::{ServerConfig, ServerHandle, Store, TokenVerifier};
use sync_engine::testkit::{
    blob_background, fd, genesis, line, nb, page_link, pg, render, replica, sk, text_obj, uuid7,
    Author,
};
use sync_protocol::{
    sha256_hex, BackgroundEncoding, BackgroundFormat, BackgroundRef, LibraryId, Sha256Hex,
    SnapshotFolder, SnapshotNotebook, SnapshotPage, SyncOperationV1, SyncSnapshotV1,
};

pub const TOKEN: &str = "restore-test-token";

pub fn lib_a() -> LibraryId {
    sync_engine::testkit::library()
}
pub fn lib_b() -> LibraryId {
    LibraryId::from_uuid(uuid7(1, 2)).unwrap()
}
pub fn dest(n: u64) -> LibraryId {
    LibraryId::from_uuid(uuid7(9, n)).unwrap()
}
pub fn mapping() -> BTreeMap<LibraryId, LibraryId> {
    BTreeMap::from([(lib_a(), dest(1)), (lib_b(), dest(2))])
}

pub fn blob_b() -> (Vec<u8>, BackgroundRef) {
    let (w, h) = (40u32, 30u32);
    let bytes: Vec<u8> = (0..w * h)
        .map(|i| (i.wrapping_mul(7) % 256) as u8)
        .collect();
    let r = BackgroundRef::Blob {
        sha256: sha256_hex(&bytes),
        byte_len: bytes.len() as u64,
        format: BackgroundFormat {
            encoding: BackgroundEncoding::Gray8,
            width: w,
            height: h,
            stride: w,
        },
    };
    (bytes, r)
}

pub fn sha_of(r: &BackgroundRef) -> Sha256Hex {
    match r {
        BackgroundRef::Blob { sha256, .. } => sha256.clone(),
        BackgroundRef::Blank => unreachable!(),
    }
}

pub fn blobs() -> BTreeMap<Sha256Hex, Vec<u8>> {
    [blob_background(), blob_b()]
        .into_iter()
        .map(|(b, r)| (sha_of(&r), b))
        .collect()
}

/// Folders, erased fragments, pressure, text, valid + dangling links, blob A.
pub fn library_a() -> SyncSnapshotV1 {
    let mut s = genesis();
    s.folders = vec![
        SnapshotFolder {
            folder_id: fd(1),
            title: "Projects".into(),
        },
        SnapshotFolder {
            folder_id: fd(2),
            title: "Archive".into(),
        },
    ];
    s.notebooks[1].folder_id = Some(fd(1));
    s.notebooks[2].folder_id = Some(fd(2));
    s.notebooks[0].pages[0]
        .texts
        .push(text_obj(1, "Agenda\nitem one"));
    s.notebooks[0].pages[0]
        .links
        .push(page_link(1, nb(1), pg(1), nb(2), pg(3)));
    s.notebooks[0].pages[1]
        .links
        .push(page_link(3, nb(1), pg(2), nb(9), pg(99)));
    s
}

/// Blob A (shared with A) and blob B.
pub fn library_b() -> SyncSnapshotV1 {
    let (_, bg_a) = blob_background();
    let (_, bg_b) = blob_b();
    SyncSnapshotV1 {
        v: 1,
        library_id: lib_b(),
        page_space: sync_protocol::PageSpace::CANONICAL,
        notebooks: vec![SnapshotNotebook {
            notebook_id: nb(10),
            title: "Meeting notes".into(),
            pages: vec![
                SnapshotPage {
                    page_id: pg(10),
                    background: bg_a,
                    render: render(true),
                    strokes: vec![line(
                        sk(10),
                        sk(10),
                        (900.0, 900.0),
                        (9000.0, 12000.0),
                        25,
                        true,
                    )],
                    texts: vec![],
                    links: vec![],
                },
                SnapshotPage {
                    page_id: pg(11),
                    background: bg_b,
                    render: render(false),
                    strokes: vec![line(
                        sk(12),
                        sk(11),
                        (100.0, 100.0),
                        (900.0, 100.0),
                        5,
                        false,
                    )],
                    texts: vec![text_obj(10, "Follow up")],
                    links: vec![],
                },
            ],
            folder_id: Some(fd(10)),
        }],
        folders: vec![SnapshotFolder {
            folder_id: fd(10),
            title: "Inbox".into(),
        }],
    }
}

/// An unrelated library with no blobs (bootstraps into any store).
pub fn unrelated_library() -> Vec<u8> {
    let mut s = library_a();
    s.library_id = dest(99);
    for p in s.notebooks.iter_mut().flat_map(|n| n.pages.iter_mut()) {
        p.background = BackgroundRef::Blank;
    }
    s.to_canonical_bytes().unwrap()
}

pub fn stroke_env(author: &mut Author, library: LibraryId, n: u64) -> Vec<u8> {
    author.library = library;
    author
        .env(SyncOperationV1::AddStroke {
            notebook_id: nb(1),
            page_id: pg(2),
            stroke: line(
                sk(300 + n),
                sk(300 + n),
                (2000.0, 2000.0),
                (6000.0, 2500.0 + n as f32),
                6,
                true,
            ),
        })
        .to_canonical_bytes()
        .unwrap()
}

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "backup-restore-{label}-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    pub fn join(&self, n: &str) -> PathBuf {
        self.0.join(n)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A real backup: source server with A (3 operations applied) and B, exported by Slice 1C.
pub struct Backup {
    pub dir: TempDir,
    pub path: PathBuf,
}

impl Backup {
    pub fn create() -> Self {
        let data = TempDir::new("source");
        let store = Store::open(&data.0).unwrap();
        for (sha, bytes) in blobs() {
            store.put_blob(sha.as_str(), &bytes).unwrap();
        }
        for s in [library_a(), library_b()] {
            store.bootstrap(&s.to_canonical_bytes().unwrap()).unwrap();
        }
        let mut author = Author::new(replica(1));
        let envs: Vec<Vec<u8>> = (0..3)
            .map(|n| stroke_env(&mut author, lib_a(), n))
            .collect();
        store.submit(&lib_a().to_string(), &envs).unwrap();
        let server = ServerHandle::start_with(
            "127.0.0.1:0".parse().unwrap(),
            store,
            ServerConfig {
                auth: Some(TokenVerifier::for_token(TOKEN)),
                ..ServerConfig::default()
            },
        )
        .unwrap();
        let dir = TempDir::new("backup");
        let path = dir.join("ignite-backup.zip");
        let src = HttpSource::new(
            &server.base_url(),
            Some(Credential::Bearer(Secret::new(TOKEN))),
        )
        .unwrap();
        export_backup(&src, &path, &Limits::DEFAULT).unwrap();
        server.shutdown();
        drop(data);
        Self { dir, path }
    }

    pub fn entries(&self) -> Vec<(String, Vec<u8>)> {
        let mut z = zip::ZipArchive::new(std::fs::File::open(&self.path).unwrap()).unwrap();
        (0..z.len())
            .map(|i| {
                let mut e = z.by_index(i).unwrap();
                let mut b = Vec::new();
                e.read_to_end(&mut b).unwrap();
                (e.name().to_string(), b)
            })
            .collect()
    }

    pub fn source_snapshot(&self, lib: LibraryId) -> Vec<u8> {
        let name = format!("libraries/{lib}/snapshot.json");
        self.entries()
            .into_iter()
            .find(|(n, _)| *n == name)
            .unwrap()
            .1
    }

    /// Write a variant archive (entries edited) next to the original; returns its path.
    pub fn variant(&self, name: &str, edit: impl FnOnce(&mut Vec<(String, Vec<u8>)>)) -> PathBuf {
        let mut entries = self.entries();
        edit(&mut entries);
        let path = self.dir.join(name);
        let mut w = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let o = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (n, d) in entries {
            w.start_file(n, o).unwrap();
            w.write_all(&d).unwrap();
        }
        w.finish().unwrap();
        path
    }
}

/// The re-keyed canonical bytes a restore of `lib` under `to` must publish.
pub fn expected_restored(backup: &Backup, lib: LibraryId, to: LibraryId) -> Vec<u8> {
    let mut s = SyncSnapshotV1::parse(&backup.source_snapshot(lib)).unwrap();
    s.library_id = to;
    s.to_canonical_bytes().unwrap()
}

/// Everything observable about a target store: per library genesis, current snapshot, head
/// and tail; blob files on disk (name → bytes); unindexed blob names; staging contents.
/// (genesis, through_server_seq, snapshot bytes, snapshot sha, tail).
pub type LibraryState = (Vec<u8>, i64, Vec<u8>, String, Vec<(i64, Vec<u8>)>);

#[derive(Debug, PartialEq, Eq)]
pub struct StoreState {
    pub libraries: BTreeMap<String, LibraryState>,
    pub blob_files: BTreeMap<String, Vec<u8>>,
    pub unindexed: Vec<String>,
    pub staging: Vec<String>,
}

pub fn observe(store: &Store, data: &Path) -> StoreState {
    let mut libraries = BTreeMap::new();
    for l in store.list_libraries().unwrap() {
        let id = l.library_id.to_string();
        let cur = store.snapshot(&id).unwrap();
        let (_, tail) = store.tail(&id, 0, i64::MAX).unwrap();
        libraries.insert(
            id.clone(),
            (
                store.genesis(&id).unwrap(),
                cur.through_server_seq,
                cur.snapshot_bytes,
                cur.snapshot_sha256,
                tail.into_iter()
                    .map(|t| (t.server_seq, t.envelope_bytes))
                    .collect(),
            ),
        );
    }
    let mut blob_files = BTreeMap::new();
    if let Ok(dirs) = std::fs::read_dir(data.join("blobs")) {
        for d in dirs.flatten() {
            for f in std::fs::read_dir(d.path()).unwrap().flatten() {
                let p = f.path();
                let bytes = if p.is_file() {
                    std::fs::read(&p).unwrap()
                } else {
                    b"<dir>".to_vec()
                };
                blob_files.insert(f.file_name().to_string_lossy().into_owned(), bytes);
            }
        }
    }
    let staging = match std::fs::read_dir(data.join("restore-staging")) {
        Ok(d) => d
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => vec![],
    };
    StoreState {
        libraries,
        blob_files,
        unindexed: store.unindexed_blob_files().unwrap(),
        staging,
    }
}

pub fn blob_file(data: &Path, sha: &Sha256Hex) -> PathBuf {
    data.join("blobs")
        .join(&sha.as_str()[..2])
        .join(sha.as_str())
}
