//! Realistic two-library backup fixture.
//!
//! ```text
//! library A (testkit::library())
//!   folders  fd(1) "Projects", fd(2) "Archive"            (creation order)
//!   nb(1) "Notebook 1"  top level
//!     pg(1) pressure profile: legacy stroke, pressure stroke, two fragments of an erased
//!           lineage (root no longer visible), text, link → nb(2)/pg(3)
//!     pg(2) blank, dangling link → a page that never existed in this library
//!   nb(2) "Work"        in fd(1)
//!     pg(3) blob background A, legacy stroke, text, link → nb(1)/pg(2)
//!   nb(3) "Scratch"     in fd(2)
//!     pg(4) blank
//! library B (uuid7(1, 2))
//!   folder   fd(10) "Inbox"
//!   nb(10) "Meeting notes" in fd(10)
//!     pg(10) blob background A (shared with library A), pressure stroke
//!     pg(11) blob background B, erased fragments, text
//! ```
#![allow(dead_code)]

use std::collections::BTreeMap;

use portable_backup::{
    BackupManifestV1, BackupVerifier, Limits, Result, SnapshotSource, VerifiedBackup,
    VerifiedSnapshot,
};
use sync_engine::testkit::{
    blob_background, fd, genesis, line, lk, nb, page_link, pg, render, sk, text_obj, uuid7,
};
use sync_protocol::{
    sha256_hex, BackgroundEncoding, BackgroundFormat, BackgroundRef, LibraryId, Sha256Hex,
    SnapshotFolder, SnapshotNotebook, SnapshotPage, SyncSnapshotV1,
};

pub fn library_a_id() -> LibraryId {
    sync_engine::testkit::library()
}

pub fn library_b_id() -> LibraryId {
    LibraryId::from_uuid(uuid7(1, 2)).unwrap()
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
    let p1 = &mut s.notebooks[0].pages[0];
    p1.texts.push(text_obj(1, "Agenda\nitem one"));
    p1.links.push(page_link(1, nb(1), pg(1), nb(2), pg(3)));
    let p2 = &mut s.notebooks[0].pages[1];
    // Dangling: the target page was deleted (or never existed); snapshots keep such links.
    p2.links.push(page_link(3, nb(1), pg(2), nb(9), pg(99)));
    let p3 = &mut s.notebooks[1].pages[0];
    p3.texts.push(text_obj(2, "Work page text"));
    p3.links.push(page_link(2, nb(2), pg(3), nb(1), pg(2)));
    s
}

pub fn library_b() -> SyncSnapshotV1 {
    let (_, bg_a) = blob_background();
    let (_, bg_b) = blob_b();
    SyncSnapshotV1 {
        v: 1,
        library_id: library_b_id(),
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
                    strokes: vec![
                        line(sk(12), sk(11), (100.0, 100.0), (900.0, 100.0), 5, false),
                        line(sk(13), sk(11), (1500.0, 100.0), (2500.0, 100.0), 5, false),
                    ],
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

pub fn canonical(s: &SyncSnapshotV1) -> Vec<u8> {
    s.to_canonical_bytes().unwrap()
}

/// A logical backup as a future archive reader would see it.
#[derive(Clone)]
pub struct Backup {
    pub manifest: Vec<u8>,
    pub snapshots: BTreeMap<LibraryId, Vec<u8>>,
    pub blobs: BTreeMap<Sha256Hex, Vec<u8>>,
}

pub fn blob_map() -> BTreeMap<Sha256Hex, Vec<u8>> {
    let mut m = BTreeMap::new();
    for (bytes, r) in [blob_background(), blob_b()] {
        let BackgroundRef::Blob { sha256, .. } = r else {
            unreachable!()
        };
        m.insert(sha256, bytes);
    }
    m
}

pub fn backup() -> Backup {
    let a = canonical(&library_a());
    let b = canonical(&library_b());
    let manifest = BackupManifestV1::for_snapshots(
        &[
            SnapshotSource {
                through_server_seq: 4211,
                canonical_bytes: &a,
            },
            SnapshotSource {
                through_server_seq: 17,
                canonical_bytes: &b,
            },
        ],
        &Limits::DEFAULT,
    )
    .unwrap()
    .to_canonical_bytes(&Limits::DEFAULT)
    .unwrap();
    Backup {
        manifest,
        snapshots: BTreeMap::from([(library_a_id(), a), (library_b_id(), b)]),
        blobs: blob_map(),
    }
}

/// Replace the manifest after editing it as a typed value (kept canonical and structurally valid).
pub fn with_manifest(b: &mut Backup, edit: impl FnOnce(&mut BackupManifestV1)) {
    let mut m = BackupManifestV1::parse(&b.manifest, &Limits::DEFAULT).unwrap();
    edit(&mut m);
    b.manifest = serde_json::to_vec(&m).unwrap();
}

/// Replace one snapshot's bytes and update its manifest hash/length to match, so the refusal
/// comes from the snapshot content itself.
pub fn replace_snapshot(b: &mut Backup, id: LibraryId, bytes: Vec<u8>) {
    let sha = sha256_hex(&bytes);
    let len = bytes.len() as u64;
    with_manifest(b, |m| {
        let l = m
            .libraries
            .iter_mut()
            .find(|l| l.source_library_id == id)
            .unwrap();
        l.source_snapshot_sha256 = sha;
        l.source_snapshot_bytes = len;
    });
    b.snapshots.insert(id, bytes);
}

/// Feed everything: blobs first, then snapshots (archive order is arbitrary).
pub fn verify(b: &Backup, limits: &Limits) -> Result<(VerifiedBackup, Vec<VerifiedSnapshot>)> {
    let mut v = BackupVerifier::new(&b.manifest, limits)?;
    for (sha, bytes) in &b.blobs {
        v.verify_blob(sha, &bytes[..])?;
    }
    let mut snaps = Vec::new();
    for (id, bytes) in &b.snapshots {
        snaps.push(v.add_snapshot(*id, &bytes[..])?);
    }
    Ok((v.finish()?, snaps))
}

/// Every identity and reference in a snapshot except `library_id`, in canonical order.
pub fn identities(s: &SyncSnapshotV1) -> Vec<String> {
    let mut out = Vec::new();
    for f in &s.folders {
        out.push(format!("folder {} {}", f.folder_id, f.title));
    }
    for n in &s.notebooks {
        out.push(format!("notebook {} {:?}", n.notebook_id, n.folder_id));
        for p in &n.pages {
            out.push(format!("page {} {:?}", p.page_id, p.background));
            for st in &p.strokes {
                out.push(format!("stroke {} root {}", st.stroke_id, st.root_id));
            }
            for t in &p.texts {
                out.push(format!("text {}", t.text_id));
            }
            for l in &p.links {
                out.push(format!(
                    "link {} {}/{} -> {}/{}",
                    l.link_id,
                    l.source_notebook_id,
                    l.source_page_id,
                    l.target_notebook_id,
                    l.target_page_id
                ));
            }
        }
    }
    out
}

pub fn link_ids() -> [sync_protocol::LinkId; 3] {
    [lk(1), lk(2), lk(3)]
}
