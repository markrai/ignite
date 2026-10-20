mod common;

use std::io::{Read, Write};

use backup_archive::{
    inspect_backup, inspect_reader, RestoreErrorKind as K, RestoreScope, ZipErrorCode as Z,
    NOT_RESTORED,
};
use common::*;
use portable_backup::{BackupErrorCode as C, BackupManifestV1, Limits};
use sync_protocol::{sha256_hex, SyncOperationV1};

fn kind<T: std::fmt::Debug>(r: Result<T, backup_archive::RestoreError>) -> K {
    r.unwrap_err().kind
}

fn zip_err(bytes: &[u8]) -> K {
    kind(inspect_bytes(bytes, &Limits::DEFAULT))
}

fn valid_raw() -> RawZip {
    RawZip::of(&logical_entries())
}

fn blob_names(entries: &[(String, Vec<u8>)]) -> Vec<String> {
    entries
        .iter()
        .filter(|(n, _)| n.starts_with("blobs/"))
        .map(|(n, _)| n.clone())
        .collect()
}

// --------------------------------------------------------------------------- acceptance

#[test]
fn archives_from_the_real_exporter_are_accepted() {
    use backup_export::{export_backup, Credential, HttpSource, Secret};
    use scribe_sync_server::{ServerConfig, ServerHandle, Store, TokenVerifier};
    use sync_engine::testkit::{line, nb, pg, replica, sk, Author};

    const TOKEN: &str = "archive-reader-test-token";
    let data = TempDir::new("server");
    let store = Store::open(&data.0).unwrap();
    let entries = logical_entries();
    for name in blob_names(&entries) {
        let bytes = &entries.iter().find(|(n, _)| *n == name).unwrap().1;
        store.put_blob(&name["blobs/".len()..], bytes).unwrap();
    }
    for s in [library_a(), library_b()] {
        store.bootstrap(&s.to_canonical_bytes().unwrap()).unwrap();
    }
    let mut author = Author::new(replica(1));
    let envs: Vec<Vec<u8>> = (0..3)
        .map(|n| {
            author
                .env(SyncOperationV1::AddStroke {
                    notebook_id: nb(1),
                    page_id: pg(2),
                    stroke: line(
                        sk(200 + n),
                        sk(200 + n),
                        (2000.0, 2000.0),
                        (6000.0, 2500.0 + n as f32),
                        6,
                        true,
                    ),
                })
                .to_canonical_bytes()
                .unwrap()
        })
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

    let out = TempDir::new("export");
    let dest = out.0.join("ignite-backup.zip");
    let source = HttpSource::new(
        &server.base_url(),
        Some(Credential::Bearer(Secret::new(TOKEN))),
    )
    .unwrap();
    let report = export_backup(&source, &dest, &Limits::DEFAULT).unwrap();
    let plan = inspect_backup(&dest, &Limits::DEFAULT).unwrap();

    assert_eq!(plan.archive_bytes, report.archive_bytes);
    assert_eq!(plan.archive_sha256, report.archive_sha256);
    assert_eq!(plan.scope, RestoreScope::CurrentStateOnly);
    assert_eq!(plan.libraries.len(), 2);
    for (p, m) in plan.libraries.iter().zip(&report.manifest.libraries) {
        assert_eq!(p.source_library_id, m.source_library_id);
        assert_eq!(p.source_snapshot_sha256, m.source_snapshot_sha256);
        assert_eq!(p.source_through_server_seq, m.source_through_server_seq);
        assert_eq!(
            (p.notebook_count, p.page_count),
            (m.notebook_count, m.page_count)
        );
    }
    assert_eq!(plan.libraries[0].source_library_id, lib_a());
    assert_eq!(plan.libraries[0].source_through_server_seq, 3);
    assert_eq!(
        (plan.libraries[0].blob_count, plan.libraries[1].blob_count),
        (1, 2)
    );
    assert_eq!(plan.blob_count, 2, "the shared background is stored once");
    assert_eq!(plan.total_blob_bytes, 2 * 1200);
    assert_eq!((plan.total_notebooks, plan.total_pages), (4, 6));
    server.shutdown();
}

#[test]
fn an_independent_writer_and_any_entry_order_are_accepted() {
    let entries = logical_entries();
    let plan = inspect_bytes(&valid_raw().build(), &Limits::DEFAULT).unwrap();
    assert_eq!(plan.format_version, 1);
    assert_eq!(plan.snapshot_version, 1);
    assert_eq!(plan.libraries[0].source_through_server_seq, 4211);
    assert_eq!(plan.libraries[1].source_through_server_seq, 7);
    assert_eq!(plan.not_restored, NOT_RESTORED);
    assert!(plan
        .warnings
        .iter()
        .any(|w| w.contains("one after another")));
    assert_eq!(
        plan.libraries[0].source_snapshot_sha256,
        sha256_hex(&entries[1].1)
    );

    // Blobs first, manifest last; DOS host attributes; unknown-host-free ZIP64 records.
    let mut reversed = entries.clone();
    reversed.reverse();
    let mut z = RawZip::of(&reversed);
    for e in &mut z.entries {
        e.host = 0;
        e.external = 0x20; // DOS "archive" attribute
    }
    let p2 = inspect_bytes(&z.build(), &Limits::DEFAULT).unwrap();
    assert_eq!(p2.libraries, plan.libraries);

    let mut z64 = valid_raw();
    z64.zip64_end = true;
    for e in &mut z64.entries {
        e.zip64 = true;
    }
    let p3 = inspect_bytes(&z64.build(), &Limits::DEFAULT).unwrap();
    assert_eq!(p3.libraries, plan.libraries);
}

#[test]
fn archives_from_the_zip_crate_writer_are_accepted() {
    use zip::write::SimpleFileOptions;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut w = zip::ZipWriter::new(&mut buf);
        let o = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (n, d) in logical_entries() {
            w.start_file(n, o).unwrap();
            w.write_all(&d).unwrap();
        }
        w.finish().unwrap();
    }
    inspect_bytes(buf.get_ref(), &Limits::DEFAULT).unwrap();
}

/// Python's `zipfile` is a fully independent implementation. Skipped (with a note) when no
/// Python interpreter is available.
#[test]
fn archives_from_python_zipfile_are_accepted_when_python_is_available() {
    let python = ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("-c")
            .arg("import zipfile")
            .output()
            .is_ok_and(|o| o.status.success())
    });
    let Some(python) = python else {
        eprintln!("SKIPPED: no python interpreter for the independent-writer check");
        return;
    };
    let dir = TempDir::new("python");
    let mut names = Vec::new();
    for (i, (name, data)) in logical_entries().into_iter().enumerate() {
        let f = dir.0.join(format!("e{i}"));
        std::fs::write(&f, data).unwrap();
        names.push((name, f));
    }
    let script = r#"
import sys, zipfile
out, force = sys.argv[1], sys.argv[2] == "1"
pairs = sys.argv[3:]
with zipfile.ZipFile(out, "w", compression=zipfile.ZIP_DEFLATED) as z:
    for name, src in zip(pairs[::2], pairs[1::2]):
        with open(src, "rb") as f, z.open(name, "w", force_zip64=force) as dst:
            dst.write(f.read())
"#;
    for force in ["0", "1"] {
        let out = dir.0.join(format!("py{force}.zip"));
        let mut cmd = std::process::Command::new(python);
        cmd.arg("-I").arg("-c").arg(script).arg(&out).arg(force);
        for (n, f) in &names {
            cmd.arg(n).arg(f);
        }
        let o = cmd.output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let plan = inspect_backup(&out, &Limits::DEFAULT)
            .unwrap_or_else(|e| panic!("python zip (force_zip64={force}) refused: {e}"));
        assert_eq!(plan.libraries.len(), 2);
    }
}

#[test]
fn large_sequence_numbers_are_reported_exactly() {
    let plan = inspect_bytes(
        &RawZip::of(&logical_entries_with_seq(u64::MAX)).build(),
        &Limits::DEFAULT,
    )
    .unwrap();
    assert_eq!(plan.libraries[0].source_through_server_seq, u64::MAX);
}

// --------------------------------------------------------------------------- container

#[test]
fn empty_garbage_and_truncated_input_is_refused() {
    assert_eq!(zip_err(b""), K::Zip(Z::NotZip));
    assert_eq!(zip_err(b"PK"), K::Zip(Z::NotZip));
    assert_eq!(zip_err(&[0u8; 4096]), K::Zip(Z::NotZip));
    assert_eq!(
        zip_err(b"{\"format\":\"ignite-portable-backup\"}"),
        K::Zip(Z::NotZip)
    );
    let good = valid_raw().build();
    for cut in [
        1,
        21,
        22,
        100,
        good.len() / 2,
        good.len() - 23,
        good.len() - 1,
    ] {
        assert!(matches!(zip_err(&good[..cut]), K::Zip(_)), "cut at {cut}");
    }
    // An end record with no entries at all.
    let mut empty = vec![0x50, 0x4b, 0x05, 0x06];
    empty.extend_from_slice(&[0; 18]);
    assert_eq!(zip_err(&empty), K::Zip(Z::MissingManifest));
}

#[test]
fn prefixed_gapped_trailing_and_overlapping_layouts_are_refused() {
    let mut z = valid_raw();
    z.prefix = b"#!/bin/sh\n".to_vec();
    assert_eq!(zip_err(&z.build()), K::Zip(Z::Layout));

    let mut z = valid_raw();
    z.gap = Some((1, b"hidden".to_vec()));
    assert_eq!(zip_err(&z.build()), K::Zip(Z::Layout));

    // Hidden bytes after the last entry, before the central directory.
    let mut z = valid_raw();
    z.gap = Some((z.entries.len() - 1, b"hidden".to_vec()));
    assert_eq!(zip_err(&z.build()), K::Zip(Z::Layout));

    let mut z = valid_raw();
    z.trailing = b"tail".to_vec();
    assert_eq!(zip_err(&z.build()), K::Zip(Z::NotZip));

    // An archive comment moves the end record away from the end of the file.
    let mut z = valid_raw();
    z.comment = b"note".to_vec();
    assert_eq!(zip_err(&z.build()), K::Zip(Z::NotZip));

    // Two directory records sharing one local entry (overlap).
    let mut z = valid_raw();
    z.point_at = Some((4, 3));
    assert!(matches!(
        zip_err(&z.build()),
        K::Zip(Z::Layout | Z::HeaderMismatch)
    ));
}

#[test]
fn missing_and_duplicate_entries_are_refused() {
    let entries = logical_entries();
    assert_eq!(
        zip_err(&RawZip::of(&entries[1..]).build()),
        K::Zip(Z::MissingManifest)
    );

    let mut dup_manifest = entries.clone();
    dup_manifest.push(entries[0].clone());
    assert_eq!(
        zip_err(&RawZip::of(&dup_manifest).build()),
        K::Zip(Z::DuplicateEntry)
    );

    let mut dup_snapshot = entries.clone();
    dup_snapshot.insert(2, entries[1].clone());
    assert_eq!(
        zip_err(&RawZip::of(&dup_snapshot).build()),
        K::Zip(Z::DuplicateEntry)
    );

    // Same name, different content (a conflicting central-directory name).
    let mut conflicting = entries.clone();
    conflicting.push((entries[3].0.clone(), vec![1, 2, 3]));
    assert_eq!(
        zip_err(&RawZip::of(&conflicting).build()),
        K::Zip(Z::DuplicateEntry)
    );

    // Local header names a different file than the central directory.
    let mut z = valid_raw();
    z.entries[1].local_name = Some(entries[2].0.clone().into_bytes());
    assert_eq!(zip_err(&z.build()), K::Zip(Z::HeaderMismatch));
}

#[test]
fn unexpected_and_unsafe_names_are_refused() {
    let lib = lib_a();
    for name in [
        "notes.txt".to_string(),
        "../manifest.json".into(),
        "/manifest.json".into(),
        "C:/manifest.json".into(),
        "blobs\\abc".into(),
        format!("libraries/{lib}/"),
        "libraries/".into(),
        "libraries/019A8EF2-3C4D-7ABC-8DEF-0123456789AB/snapshot.json".into(),
        format!("libraries/{lib}/../{lib}/snapshot.json"),
        format!("blobs/{}", "A".repeat(64)),
        "manifest.json/".into(),
        "__MACOSX/._manifest.json".into(),
    ] {
        let mut entries = logical_entries();
        entries.push((name.clone(), b"x".to_vec()));
        assert_eq!(
            zip_err(&RawZip::of(&entries).build()),
            K::Zip(Z::InvalidEntryName),
            "{name}"
        );
    }
    let mut z = valid_raw();
    z.entries[2].name = vec![0xff, 0xfe];
    z.entries[2].local_name = Some(vec![0xff, 0xfe]);
    assert_eq!(zip_err(&z.build()), K::Zip(Z::InvalidEntryName));
}

#[test]
fn symlinks_directories_and_special_files_are_refused() {
    for (host, external) in [
        (3u8, 0o120777u32 << 16), // symlink
        (3, 0o040755 << 16),      // directory
        (3, 0o060644 << 16),      // block device
        (3, 0o010644 << 16),      // fifo
        (0, 0x10),                // DOS directory
        (0, 0x08),                // DOS volume label
    ] {
        let mut z = valid_raw();
        z.entries[3].host = host;
        z.entries[3].external = external;
        assert_eq!(
            zip_err(&z.build()),
            K::Zip(Z::NotRegularFile),
            "{host} {external:o}"
        );
    }
    // The zip crate's own symlink and directory entries.
    use zip::write::SimpleFileOptions;
    for kind in ["symlink", "dir"] {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let o =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            for (n, d) in logical_entries() {
                w.start_file(n, o).unwrap();
                w.write_all(&d).unwrap();
            }
            if kind == "symlink" {
                w.add_symlink("blobs/link", "/etc/passwd", o).unwrap();
            } else {
                w.add_directory("blobs/", o).unwrap();
            }
            w.finish().unwrap();
        }
        assert!(
            matches!(
                zip_err(buf.get_ref()),
                K::Zip(Z::NotRegularFile | Z::InvalidEntryName | Z::Unsupported)
            ),
            "{kind}"
        );
    }
}

#[test]
fn encryption_and_unsupported_features_are_refused() {
    let set = |f: &dyn Fn(&mut RawEntry)| {
        let mut z = valid_raw();
        f(&mut z.entries[2]);
        zip_err(&z.build())
    };
    assert_eq!(set(&|e| e.flags = 1), K::Zip(Z::Encrypted));
    assert_eq!(set(&|e| e.flags = 1 | (1 << 6)), K::Zip(Z::Encrypted));
    assert_eq!(set(&|e| e.flags = 1 << 13), K::Zip(Z::Encrypted));
    assert_eq!(set(&|e| e.method = 99), K::Zip(Z::Encrypted));
    assert_eq!(set(&|e| e.flags = 1 << 3), K::Zip(Z::Unsupported));
    assert_eq!(set(&|e| e.flags = 1 << 5), K::Zip(Z::Unsupported));
    assert_eq!(
        set(&|e| {
            e.method = 0;
            e.compressed = Some(e.data.clone());
        }),
        K::Zip(Z::Unsupported)
    );
    assert_eq!(set(&|e| e.method = 12), K::Zip(Z::Unsupported));
    assert_eq!(set(&|e| e.method = 14), K::Zip(Z::Unsupported));
    assert_eq!(set(&|e| e.version_needed = 63), K::Zip(Z::Unsupported));
    assert_eq!(set(&|e| e.host = 7), K::Zip(Z::Unsupported));
    assert_eq!(set(&|e| e.internal = 4), K::Zip(Z::Unsupported));
    assert_eq!(set(&|e| e.comment = b"hi".to_vec()), K::Zip(Z::Unsupported));
    // Extended-timestamp extra field (Info-ZIP 0x5455).
    assert_eq!(
        set(&|e| e.extra_central = vec![0x55, 0x54, 5, 0, 1, 0, 0, 0, 0]),
        K::Zip(Z::Unsupported)
    );
    assert_eq!(
        set(&|e| e.extra_local = vec![0x55, 0x54, 5, 0, 1, 0, 0, 0, 0]),
        K::Zip(Z::Unsupported)
    );
    // Malformed ZIP64 extra.
    assert_eq!(
        set(&|e| e.extra_central = vec![1, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        K::Zip(Z::Malformed)
    );
    assert_eq!(
        set(&|e| e.local_flags = Some(1 << 1)),
        K::Zip(Z::HeaderMismatch)
    );
}

#[test]
fn corrupted_data_crc_and_size_lies_are_refused() {
    // Flip a byte inside the compressed stream of a snapshot.
    let mut z = valid_raw();
    let e = &mut z.entries[1];
    let mut c = deflate(&e.data);
    let mid = c.len() / 2;
    c[mid] ^= 0x5a;
    e.compressed = Some(c);
    assert!(matches!(
        zip_err(&z.build()),
        K::Zip(Z::CorruptData | Z::CrcMismatch | Z::SizeMismatch)
    ));

    // Wrong CRC declared consistently in both headers.
    let mut z = valid_raw();
    z.entries[3].crc = Some(0xdead_beef);
    assert_eq!(zip_err(&z.build()), K::Zip(Z::CrcMismatch));
    let mut z = valid_raw();
    z.entries[0].crc = Some(0xdead_beef);
    assert_eq!(zip_err(&z.build()), K::Zip(Z::CrcMismatch));

    // Declared size smaller / larger than the inflated data.
    let mut z = valid_raw();
    let n = z.entries[4].data.len() as u64;
    z.entries[4].size = Some(n - 1);
    assert_eq!(zip_err(&z.build()), K::Zip(Z::SizeMismatch));
    let mut z = valid_raw();
    z.entries[4].size = Some(n + 1);
    assert_eq!(zip_err(&z.build()), K::Zip(Z::SizeMismatch));

    // Local and central sizes disagree.
    let mut z = valid_raw();
    z.entries[4].local_size = Some(n + 5);
    assert_eq!(zip_err(&z.build()), K::Zip(Z::HeaderMismatch));

    // Valid deflate stream followed by extra compressed bytes inside the entry.
    let mut z = valid_raw();
    let e = &mut z.entries[3];
    let mut c = deflate(&e.data);
    c.extend_from_slice(b"smuggled");
    e.compressed = Some(c);
    assert_eq!(zip_err(&z.build()), K::Zip(Z::CorruptData));

    // Deflate stream cut short (consistent headers).
    let mut z = valid_raw();
    let e = &mut z.entries[1];
    let c = deflate(&e.data);
    e.compressed = Some(c[..c.len() / 2].to_vec());
    assert!(matches!(
        zip_err(&z.build()),
        K::Zip(Z::CorruptData | Z::SizeMismatch)
    ));
}

#[test]
fn compression_bombs_and_resource_limits_are_bounded() {
    // Honest bomb: 64 MiB of zeros declared as a blob — refused before inflating anything.
    let zeros = vec![0u8; 64 << 20];
    let bomb = deflate(&zeros);
    assert!(bomb.len() < 128 * 1024, "deflate ratio ~1000:1");
    let mut z = valid_raw();
    let blob = blob_names(&logical_entries())[0].clone();
    let e = z.entry(&blob);
    e.data = zeros.clone();
    e.compressed = Some(bomb.clone());
    assert_eq!(zip_err(&z.build()), K::Zip(Z::LimitExceeded));

    // Lying bomb: declares 1200 bytes (the manifest's size) but inflates to 64 MiB. Inflation
    // stops one byte past the declaration.
    let mut z = valid_raw();
    let e = z.entry(&blob);
    let real_crc = crc(&e.data);
    e.compressed = Some(bomb.clone());
    e.size = Some(1200);
    e.crc = Some(real_crc);
    let t = std::time::Instant::now();
    assert_eq!(zip_err(&z.build()), K::Zip(Z::SizeMismatch));
    assert!(t.elapsed() < std::time::Duration::from_secs(5));

    // A highly compressible but legitimate entry is accepted (no ratio limit).
    let white = vec![0xffu8; 1200];
    assert!(deflate(&white).len() * 50 < white.len());

    let entries = logical_entries();
    let bytes = RawZip::of(&entries).build();
    let lim = |f: &dyn Fn(&mut Limits)| {
        let mut l = Limits::DEFAULT;
        f(&mut l);
        kind(inspect_bytes(&bytes, &l))
    };
    assert_eq!(lim(&|l| l.max_libraries = 1), K::Zip(Z::LimitExceeded));
    assert_eq!(lim(&|l| l.max_blobs = 1), K::Zip(Z::LimitExceeded));
    assert_eq!(lim(&|l| l.max_blob_bytes = 1199), K::Zip(Z::LimitExceeded));
    assert_eq!(
        lim(&|l| l.max_total_blob_bytes = 2399),
        K::Zip(Z::LimitExceeded)
    );
    assert_eq!(
        lim(&|l| l.max_manifest_bytes = entries[0].1.len() as u64 - 1),
        K::Zip(Z::LimitExceeded)
    );
    assert_eq!(
        lim(&|l| l.max_snapshot_bytes = 100),
        K::Zip(Z::LimitExceeded)
    );
    assert_eq!(
        lim(&|l| l.max_total_snapshot_bytes = 100),
        K::Zip(Z::LimitExceeded)
    );

    // Entry count beyond 1 + libraries + blobs, before any directory record is trusted.
    let tiny = Limits {
        max_libraries: 1,
        max_blobs: 1,
        ..Limits::DEFAULT
    };
    assert_eq!(kind(inspect_bytes(&bytes, &tiny)), K::Zip(Z::LimitExceeded));
}

#[test]
fn snapshot_limits_are_exact_boundaries() {
    // The per-snapshot cap admits a snapshot of exactly its size and refuses one byte more,
    // from the declaration alone; the cumulative cap likewise for their sum.
    let entries = logical_entries();
    let bytes = RawZip::of(&entries).build();
    let sizes: Vec<u64> = entries
        .iter()
        .filter(|(n, _)| n.starts_with("libraries/"))
        .map(|(_, d)| d.len() as u64)
        .collect();
    assert_eq!(sizes.len(), 2);
    let largest = *sizes.iter().max().unwrap();
    let total: u64 = sizes.iter().sum();
    let with = |f: &dyn Fn(&mut Limits)| {
        let mut l = Limits::DEFAULT;
        f(&mut l);
        inspect_bytes(&bytes, &l)
    };
    assert!(with(&|l| l.max_snapshot_bytes = largest).is_ok());
    assert_eq!(
        kind(with(&|l| l.max_snapshot_bytes = largest - 1)),
        K::Zip(Z::LimitExceeded)
    );
    assert!(with(&|l| l.max_total_snapshot_bytes = total).is_ok());
    assert_eq!(
        kind(with(&|l| l.max_total_snapshot_bytes = total - 1)),
        K::Zip(Z::LimitExceeded)
    );
}

// --------------------------------------------------------------------------- semantics

fn replace(entries: &mut [(String, Vec<u8>)], name: &str, data: Vec<u8>) {
    entries.iter_mut().find(|(n, _)| n == name).unwrap().1 = data;
}

fn with_manifest(entries: &mut [(String, Vec<u8>)], f: impl FnOnce(&mut BackupManifestV1)) {
    let mut m = manifest_of(entries);
    f(&mut m);
    entries[0].1 = serde_json::to_vec(&m).unwrap();
}

#[test]
fn semantic_refusals_come_from_backup_verifier() {
    let base = logical_entries();
    let snap_a = base[1].0.clone();
    let blobs = blob_names(&base);

    // Snapshot bytes changed (CRC fine at the ZIP level, hash wrong for the manifest).
    let mut e = base.clone();
    let mut s = e[1].1.clone();
    let i = s.iter().position(|c| *c == b'1').unwrap();
    s[i] = b'2';
    replace(&mut e, &snap_a, s);
    assert_eq!(zip_err(&RawZip::of(&e).build()), K::Backup(C::HashMismatch));

    // Pretty-printed snapshot with a manifest that matches its bytes.
    let mut e = base.clone();
    let pretty =
        serde_json::to_vec_pretty(&serde_json::from_slice::<serde_json::Value>(&e[1].1).unwrap())
            .unwrap();
    with_manifest(&mut e, |m| {
        m.libraries[0].source_snapshot_sha256 = sha256_hex(&pretty);
        m.libraries[0].source_snapshot_bytes = pretty.len() as u64;
    });
    replace(&mut e, &snap_a, pretty);
    assert_eq!(zip_err(&RawZip::of(&e).build()), K::Backup(C::NonCanonical));

    // Missing blob entry / extra blob entry / truncated blob / hash-mismatched blob.
    let mut e = base.clone();
    e.retain(|(n, _)| *n != blobs[1]);
    assert_eq!(zip_err(&RawZip::of(&e).build()), K::Backup(C::MissingBlob));

    let mut e = base.clone();
    let extra = vec![7u8; 33];
    e.push((format!("blobs/{}", sha256_hex(&extra)), extra));
    assert_eq!(
        zip_err(&RawZip::of(&e).build()),
        K::Backup(C::UnexpectedEntry)
    );

    let mut e = base.clone();
    let short = e.iter().find(|(n, _)| *n == blobs[0]).unwrap().1[..1000].to_vec();
    replace(&mut e, &blobs[0], short);
    assert_eq!(zip_err(&RawZip::of(&e).build()), K::Backup(C::SizeMismatch));

    let mut e = base.clone();
    let mut wrong = e.iter().find(|(n, _)| *n == blobs[0]).unwrap().1.clone();
    wrong[0] ^= 1;
    replace(&mut e, &blobs[0], wrong);
    assert_eq!(zip_err(&RawZip::of(&e).build()), K::Backup(C::HashMismatch));

    // Manifest problems: unsupported format version, unknown field, count mismatch.
    let mut e = base.clone();
    let mut v: serde_json::Value = serde_json::from_slice(&e[0].1).unwrap();
    v["format_version"] = 2.into();
    e[0].1 = serde_json::to_vec(&v).unwrap();
    assert_eq!(
        zip_err(&RawZip::of(&e).build()),
        K::Backup(C::UnsupportedVersion)
    );

    let mut e = base.clone();
    with_manifest(&mut e, |m| m.libraries[1].page_count += 1);
    assert_eq!(
        zip_err(&RawZip::of(&e).build()),
        K::Backup(C::CountMismatch)
    );

    let mut e = base.clone();
    e[0].1 = b"not json".to_vec();
    assert_eq!(
        zip_err(&RawZip::of(&e).build()),
        K::Backup(C::InvalidManifest)
    );
}

#[test]
fn inspection_never_writes_and_needs_only_read_access() {
    let dir = TempDir::new("ro");
    let path = dir.0.join("backup.zip");
    let bytes = valid_raw().build();
    std::fs::write(&path, &bytes).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms.clone()).unwrap();
    let plan = inspect_backup(&path, &Limits::DEFAULT).unwrap();
    assert_eq!(plan.archive_sha256, sha256_hex(&bytes));
    assert_eq!(
        dir.listing(),
        vec![("backup.zip".to_string(), bytes.len() as u64)]
    );
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&path, perms).unwrap();

    assert_eq!(
        kind(inspect_backup(&dir.0.join("missing.zip"), &Limits::DEFAULT)),
        K::Io
    );

    // The in-memory reader works the same way.
    let mut cursor = std::io::Cursor::new(bytes.clone());
    assert_eq!(inspect_reader(&mut cursor, &Limits::DEFAULT).unwrap(), plan);
    let mut back = Vec::new();
    cursor.set_position(0);
    cursor.read_to_end(&mut back).unwrap();
    assert_eq!(back, bytes);
}

fn zip64_raw() -> RawZip {
    let mut z = valid_raw();
    z.zip64_end = true;
    for e in &mut z.entries {
        e.zip64 = true;
    }
    z
}

#[test]
fn zip64_end_records_must_agree_on_the_entry_count() {
    let n = logical_entries().len() as u16;
    // Valid: both legacy counts are the ZIP64 sentinel (as written by the builder), or both
    // carry the real count.
    inspect_bytes(&zip64_raw().build(), &Limits::DEFAULT).unwrap();
    let mut z = zip64_raw();
    z.eocd_on_disk = Some(n);
    // Legacy total is still 0xFFFF here; an on-disk count equal to the ZIP64 total is valid.
    inspect_bytes(&z.build(), &Limits::DEFAULT).unwrap();

    // Contradictory legacy entries-on-this-disk counts.
    for bad in [0u16, 1, n - 1, n + 1, 0xFFFE] {
        let mut z = zip64_raw();
        z.eocd_on_disk = Some(bad);
        assert_eq!(
            zip_err(&z.build()),
            K::Zip(Z::Malformed),
            "on-disk count {bad}"
        );
    }
    // Without ZIP64, the two legacy counts must simply be equal.
    let mut z = valid_raw();
    z.eocd_on_disk = Some(n - 1);
    assert_eq!(zip_err(&z.build()), K::Zip(Z::Unsupported));
}

#[test]
fn disk_start_must_be_the_single_disk() {
    for (zip64, disk) in [(false, 0xFFFFu16), (true, 0xFFFF), (false, 1), (true, 1)] {
        let mut z = if zip64 { zip64_raw() } else { valid_raw() };
        z.entries[2].disk_start = disk;
        assert_eq!(
            zip_err(&z.build()),
            K::Zip(Z::Unsupported),
            "zip64={zip64} disk={disk:#x}"
        );
    }
}

// --------------------------------------------------------------------------- staging sink

struct Capture {
    snapshots: Vec<sync_protocol::LibraryId>,
    blobs: std::collections::BTreeMap<String, std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
    verified: Vec<String>,
    fail_begin: bool,
}

struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl backup_archive::VerifiedEntrySink for Capture {
    fn snapshot(&mut self, s: portable_backup::VerifiedSnapshot) -> Result<(), String> {
        self.snapshots.push(s.source_library_id());
        Ok(())
    }
    fn begin_blob(
        &mut self,
        sha: &sync_protocol::Sha256Hex,
        _len: u64,
    ) -> Result<Box<dyn Write>, String> {
        if self.fail_begin {
            return Err("disk full".into());
        }
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        self.blobs.insert(sha.to_string(), buf.clone());
        Ok(Box::new(Shared(buf)))
    }
    fn blob_verified(
        &mut self,
        sha: &sync_protocol::Sha256Hex,
        _: Box<dyn Write>,
    ) -> Result<(), String> {
        self.verified.push(sha.to_string());
        Ok(())
    }
}

fn capture() -> Capture {
    Capture {
        snapshots: vec![],
        blobs: Default::default(),
        verified: vec![],
        fail_begin: false,
    }
}

#[test]
fn the_staging_sink_receives_exactly_the_verified_bytes() {
    let entries = logical_entries();
    let bytes = valid_raw().build();
    let mut sink = capture();
    let plan = backup_archive::verify_reader_into(
        &mut std::io::Cursor::new(&bytes),
        &Limits::DEFAULT,
        &mut sink,
    )
    .unwrap();
    assert_eq!(
        plan,
        inspect_bytes(&bytes, &Limits::DEFAULT).unwrap(),
        "same plan as inspection"
    );
    assert_eq!(sink.snapshots, vec![lib_a(), lib_b()]);
    for name in blob_names(&entries) {
        let sha = &name["blobs/".len()..];
        let original = &entries.iter().find(|(n, _)| *n == name).unwrap().1;
        assert_eq!(&*sink.blobs[sha].lock().unwrap(), original);
    }
    assert_eq!(sink.verified.len(), 2);

    // An unlisted blob is refused without ever being offered to the sink.
    let mut e = entries.clone();
    let extra = vec![7u8; 33];
    let extra_sha = sha256_hex(&extra).to_string();
    e.push((format!("blobs/{extra_sha}"), extra));
    let mut sink = capture();
    let err = backup_archive::verify_reader_into(
        &mut std::io::Cursor::new(RawZip::of(&e).build()),
        &Limits::DEFAULT,
        &mut sink,
    )
    .unwrap_err();
    assert_eq!(err.kind, K::Backup(C::UnexpectedEntry));
    assert!(!sink.blobs.contains_key(&extra_sha));

    // A corrupt blob is staged partially but never reported verified.
    let mut z = valid_raw();
    let blob = blob_names(&entries)[0].clone();
    z.entry(&blob).data[3] ^= 1;
    let mut sink = capture();
    let err = backup_archive::verify_reader_into(
        &mut std::io::Cursor::new(z.build()),
        &Limits::DEFAULT,
        &mut sink,
    )
    .unwrap_err();
    assert_eq!(err.kind, K::Backup(C::HashMismatch));
    assert!(sink.verified.is_empty());

    // Sink failures are staging errors, not archive damage; hostile archives still fail first.
    let mut sink = Capture {
        fail_begin: true,
        ..capture()
    };
    let err = backup_archive::verify_reader_into(
        &mut std::io::Cursor::new(&bytes),
        &Limits::DEFAULT,
        &mut sink,
    )
    .unwrap_err();
    assert_eq!(err.kind, K::Staging);
    let mut sink = capture();
    let mut dup = entries.clone();
    dup.push(entries[0].clone());
    let err = backup_archive::verify_reader_into(
        &mut std::io::Cursor::new(RawZip::of(&dup).build()),
        &Limits::DEFAULT,
        &mut sink,
    )
    .unwrap_err();
    assert_eq!(err.kind, K::Zip(Z::DuplicateEntry));
    assert!(sink.snapshots.is_empty() && sink.blobs.is_empty());
}
