//! Fixtures: a realistic logical backup, an independent raw ZIP writer for hostile archives,
//! and a disposable loopback sync server for exporter round trips.
#![allow(dead_code)]

use std::io::Write;
use std::path::PathBuf;

use backup_archive::{inspect_backup, RestoreError, RestorePlan};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use portable_backup::{BackupManifestV1, Limits, ManifestBuilder, SnapshotSource};
use sync_engine::testkit::{
    blob_background, fd, genesis, line, nb, page_link, pg, render, sk, text_obj, uuid7,
};
use sync_protocol::{
    sha256_hex, BackgroundEncoding, BackgroundFormat, BackgroundRef, LibraryId, Sha256Hex,
    SnapshotFolder, SnapshotNotebook, SnapshotPage, SyncSnapshotV1,
};

pub fn lib_a() -> LibraryId {
    sync_engine::testkit::library()
}
pub fn lib_b() -> LibraryId {
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

pub fn sha_of(r: &BackgroundRef) -> Sha256Hex {
    match r {
        BackgroundRef::Blob { sha256, .. } => sha256.clone(),
        BackgroundRef::Blank => unreachable!(),
    }
}

/// Library A: folders, erased fragments, pressure, text, valid + dangling links, blob A.
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

/// Library B: blob A (shared with A) and blob B.
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

/// Logical entries `(name, bytes)` of a valid two-library backup, in exporter order.
pub fn logical_entries_with_seq(seq_a: u64) -> Vec<(String, Vec<u8>)> {
    let a = library_a().to_canonical_bytes().unwrap();
    let b = library_b().to_canonical_bytes().unwrap();
    let mut builder = ManifestBuilder::new();
    builder
        .add_snapshot(
            SnapshotSource {
                through_server_seq: seq_a,
                canonical_bytes: &a,
            },
            &Limits::DEFAULT,
        )
        .unwrap();
    builder
        .add_snapshot(
            SnapshotSource {
                through_server_seq: 7,
                canonical_bytes: &b,
            },
            &Limits::DEFAULT,
        )
        .unwrap();
    let manifest = builder.finish(&Limits::DEFAULT).unwrap();
    let mut out = vec![(
        "manifest.json".to_string(),
        manifest.to_canonical_bytes(&Limits::DEFAULT).unwrap(),
    )];
    out.push((format!("libraries/{}/snapshot.json", lib_a()), a));
    out.push((format!("libraries/{}/snapshot.json", lib_b()), b));
    for (bytes, r) in [blob_background(), blob_b()] {
        out.push((format!("blobs/{}", sha_of(&r)), bytes));
    }
    let mut sorted = out.split_off(3);
    sorted.sort();
    out.extend(sorted);
    out
}

pub fn logical_entries() -> Vec<(String, Vec<u8>)> {
    logical_entries_with_seq(4211)
}

pub fn manifest_of(entries: &[(String, Vec<u8>)]) -> BackupManifestV1 {
    BackupManifestV1::parse(&entries[0].1, &Limits::DEFAULT).unwrap()
}

// --------------------------------------------------------------------------- raw ZIP writer

pub fn deflate(data: &[u8]) -> Vec<u8> {
    let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

pub fn crc(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

/// One entry with every header field under test control. Defaults describe a plain Deflate
/// regular file (unix host, mode 0644), the way the exporter writes it.
#[derive(Clone)]
pub struct RawEntry {
    pub name: Vec<u8>,
    pub local_name: Option<Vec<u8>>,
    pub data: Vec<u8>,
    pub compressed: Option<Vec<u8>>,
    pub method: u16,
    pub flags: u16,
    pub local_flags: Option<u16>,
    pub version_needed: u16,
    pub host: u8,
    pub external: u32,
    pub internal: u16,
    pub crc: Option<u32>,
    pub size: Option<u64>,
    pub local_size: Option<u64>,
    pub extra_local: Vec<u8>,
    pub extra_central: Vec<u8>,
    pub comment: Vec<u8>,
    /// Write ZIP64 sentinels + extra fields in both headers (valid ZIP64 for a small file).
    pub zip64: bool,
    /// Central-directory "disk number start" (0 for a single-disk archive).
    pub disk_start: u16,
}

impl RawEntry {
    pub fn new(name: &str, data: &[u8]) -> Self {
        Self {
            name: name.as_bytes().to_vec(),
            local_name: None,
            data: data.to_vec(),
            compressed: None,
            method: 8,
            flags: 0,
            local_flags: None,
            version_needed: 20,
            host: 3,
            external: 0o100644 << 16,
            internal: 0,
            crc: None,
            size: None,
            local_size: None,
            extra_local: vec![],
            extra_central: vec![],
            comment: vec![],
            zip64: false,
            disk_start: 0,
        }
    }
}

#[derive(Clone, Default)]
pub struct RawZip {
    pub entries: Vec<RawEntry>,
    pub prefix: Vec<u8>,
    /// Bytes inserted after entry `i`'s data.
    pub gap: Option<(usize, Vec<u8>)>,
    pub trailing: Vec<u8>,
    pub comment: Vec<u8>,
    pub zip64_end: bool,
    /// Central-directory record for entry `.0` points at entry `.1`'s local header.
    pub point_at: Option<(usize, usize)>,
    /// Override the legacy EOCD "entries on this disk" count.
    pub eocd_on_disk: Option<u16>,
}

fn le16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn le32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn le64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

impl RawZip {
    pub fn of(entries: &[(String, Vec<u8>)]) -> Self {
        Self {
            entries: entries.iter().map(|(n, d)| RawEntry::new(n, d)).collect(),
            ..Self::default()
        }
    }

    pub fn entry(&mut self, name: &str) -> &mut RawEntry {
        self.entries
            .iter_mut()
            .find(|e| e.name == name.as_bytes())
            .unwrap()
    }

    pub fn build(&self) -> Vec<u8> {
        let mut out = self.prefix.clone();
        let mut offsets = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            offsets.push(out.len() as u64);
            let comp = e.compressed.clone().unwrap_or_else(|| deflate(&e.data));
            let crc = e.crc.unwrap_or_else(|| crc(&e.data));
            let size = e.local_size.or(e.size).unwrap_or(e.data.len() as u64);
            le32(&mut out, 0x0403_4b50);
            le16(&mut out, if e.zip64 { 45 } else { e.version_needed });
            le16(&mut out, e.local_flags.unwrap_or(e.flags));
            le16(&mut out, e.method);
            le16(&mut out, 0);
            le16(&mut out, 0x21);
            le32(&mut out, crc);
            let mut extra = e.extra_local.clone();
            if e.zip64 {
                le32(&mut out, 0xFFFF_FFFF);
                le32(&mut out, 0xFFFF_FFFF);
                let mut z = Vec::new();
                le16(&mut z, 1);
                le16(&mut z, 16);
                le64(&mut z, size);
                le64(&mut z, comp.len() as u64);
                z.extend(extra);
                extra = z;
            } else {
                le32(&mut out, comp.len() as u32);
                le32(&mut out, size as u32);
            }
            let name = e.local_name.clone().unwrap_or_else(|| e.name.clone());
            le16(&mut out, name.len() as u16);
            le16(&mut out, extra.len() as u16);
            out.extend_from_slice(&name);
            out.extend_from_slice(&extra);
            out.extend_from_slice(&comp);
            if let Some((g, bytes)) = &self.gap {
                if *g == i {
                    out.extend_from_slice(bytes);
                }
            }
        }
        let cd_start = out.len() as u64;
        for (i, e) in self.entries.iter().enumerate() {
            let comp = e.compressed.clone().unwrap_or_else(|| deflate(&e.data));
            let crc = e.crc.unwrap_or_else(|| crc(&e.data));
            let size = e.size.unwrap_or(e.data.len() as u64);
            let offset = offsets[self.point_at.filter(|p| p.0 == i).map_or(i, |p| p.1)];
            le32(&mut out, 0x0201_4b50);
            le16(&mut out, (u16::from(e.host) << 8) | 45);
            le16(&mut out, if e.zip64 { 45 } else { e.version_needed });
            le16(&mut out, e.flags);
            le16(&mut out, e.method);
            le16(&mut out, 0);
            le16(&mut out, 0x21);
            le32(&mut out, crc);
            let mut extra = e.extra_central.clone();
            if e.zip64 {
                le32(&mut out, 0xFFFF_FFFF);
                le32(&mut out, 0xFFFF_FFFF);
                let mut z = Vec::new();
                le16(&mut z, 1);
                le16(&mut z, 24);
                le64(&mut z, size);
                le64(&mut z, comp.len() as u64);
                le64(&mut z, offset);
                z.extend(extra);
                extra = z;
            } else {
                le32(&mut out, comp.len() as u32);
                le32(&mut out, size as u32);
            }
            le16(&mut out, e.name.len() as u16);
            le16(&mut out, extra.len() as u16);
            le16(&mut out, e.comment.len() as u16);
            le16(&mut out, e.disk_start);
            le16(&mut out, e.internal);
            le32(&mut out, e.external);
            le32(&mut out, if e.zip64 { 0xFFFF_FFFF } else { offset as u32 });
            out.extend_from_slice(&e.name);
            out.extend_from_slice(&extra);
            out.extend_from_slice(&e.comment);
        }
        let cd_size = out.len() as u64 - cd_start;
        let n = self.entries.len() as u64;
        if self.zip64_end {
            let z_at = out.len() as u64;
            le32(&mut out, 0x0606_4b50);
            le64(&mut out, 44);
            le16(&mut out, 45);
            le16(&mut out, 45);
            le32(&mut out, 0);
            le32(&mut out, 0);
            le64(&mut out, n);
            le64(&mut out, n);
            le64(&mut out, cd_size);
            le64(&mut out, cd_start);
            le32(&mut out, 0x0706_4b50);
            le32(&mut out, 0);
            le64(&mut out, z_at);
            le32(&mut out, 1);
        }
        le32(&mut out, 0x0605_4b50);
        le16(&mut out, 0);
        le16(&mut out, 0);
        let (n16, size32, off32) = if self.zip64_end {
            (0xFFFF, 0xFFFF_FFFF, 0xFFFF_FFFF)
        } else {
            (n as u16, cd_size as u32, cd_start as u32)
        };
        le16(&mut out, self.eocd_on_disk.unwrap_or(n16));
        le16(&mut out, n16);
        le32(&mut out, size32);
        le32(&mut out, off32);
        le16(&mut out, self.comment.len() as u16);
        out.extend_from_slice(&self.comment);
        out.extend_from_slice(&self.trailing);
        out
    }
}

// --------------------------------------------------------------------------- file inspection

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "backup-archive-{label}-{}-{}-{}",
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

    pub fn listing(&self) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = std::fs::read_dir(&self.0)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.metadata().unwrap().len(),
                )
            })
            .collect();
        v.sort();
        v
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Inspect `bytes` as a file on disk and prove nothing on disk changed (archive bytes and
/// the directory listing), whatever the outcome.
pub fn inspect_bytes(bytes: &[u8], limits: &Limits) -> Result<RestorePlan, RestoreError> {
    let dir = TempDir::new("inspect");
    let path = dir.0.join("backup.zip");
    std::fs::write(&path, bytes).unwrap();
    let before = dir.listing();
    let r = inspect_backup(&path, limits);
    assert_eq!(dir.listing(), before, "inspection changed the directory");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "inspection changed the archive"
    );
    r
}
