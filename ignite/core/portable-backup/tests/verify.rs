mod common;

use common::*;
use portable_backup::{
    BackupErrorCode as C, BackupManifestV1, BackupVerifier, EntryPath, Limits, ManifestBuilder,
    SnapshotSource, BACKUP_FORMAT, BACKUP_FORMAT_VERSION,
};
use sync_protocol::{sha256_hex, LibraryId};

fn code<T: std::fmt::Debug>(r: portable_backup::Result<T>) -> C {
    r.unwrap_err().code
}

#[test]
fn complete_backup_verifies_in_any_entry_order() {
    let b = backup();
    let (verified, snaps) = verify(&b, &Limits::DEFAULT).unwrap();
    let m = verified.manifest();
    assert_eq!(m.format, BACKUP_FORMAT);
    assert_eq!(m.format_version, BACKUP_FORMAT_VERSION);
    assert_eq!(m.snapshot_version, 1);
    assert_eq!(m.libraries.len(), 2);
    assert_eq!(m.blobs.len(), 2, "the shared background is listed once");
    let a = &m.libraries[0];
    assert_eq!(a.source_library_id, library_a_id());
    assert_eq!((a.notebook_count, a.page_count), (3, 4));
    assert_eq!(a.source_through_server_seq, 4211);
    assert_eq!(
        a.source_snapshot_sha256,
        sha256_hex(&b.snapshots[&library_a_id()])
    );
    let lb = &m.libraries[1];
    assert_eq!((lb.notebook_count, lb.page_count), (1, 2));

    // Verified snapshots are the source snapshots, unchanged.
    assert_eq!(snaps[0].snapshot(), &library_a());
    assert_eq!(snaps[1].snapshot(), &library_b());
    assert_eq!(snaps[0].source_snapshot_sha256(), &a.source_snapshot_sha256);

    // Snapshots first, blobs interleaved, reverse order.
    let mut v = BackupVerifier::new(&b.manifest, &Limits::DEFAULT).unwrap();
    let blobs: Vec<_> = b.blobs.iter().collect();
    v.add_snapshot(library_b_id(), &b.snapshots[&library_b_id()][..])
        .unwrap();
    v.verify_blob(blobs[1].0, &blobs[1].1[..]).unwrap();
    v.add_snapshot(library_a_id(), &b.snapshots[&library_a_id()][..])
        .unwrap();
    v.verify_blob(blobs[0].0, &blobs[0].1[..]).unwrap();
    assert_eq!(v.finish().unwrap(), verified);
}

#[test]
fn manifest_is_deterministic_and_canonical() {
    let a = canonical(&library_a());
    let bb = canonical(&library_b());
    let one = BackupManifestV1::for_snapshots(
        &[
            SnapshotSource {
                through_server_seq: 4211,
                canonical_bytes: &a,
            },
            SnapshotSource {
                through_server_seq: 17,
                canonical_bytes: &bb,
            },
        ],
        &Limits::DEFAULT,
    )
    .unwrap();
    let two = BackupManifestV1::for_snapshots(
        &[
            SnapshotSource {
                through_server_seq: 17,
                canonical_bytes: &bb,
            },
            SnapshotSource {
                through_server_seq: 4211,
                canonical_bytes: &a,
            },
        ],
        &Limits::DEFAULT,
    )
    .unwrap();
    assert_eq!(one, two);
    let bytes = one.to_canonical_bytes(&Limits::DEFAULT).unwrap();
    assert_eq!(bytes, backup().manifest);
    assert_eq!(
        BackupManifestV1::parse(&bytes, &Limits::DEFAULT).unwrap(),
        one
    );
    let text = String::from_utf8(bytes).unwrap();
    for forbidden in [
        "token",
        "credential",
        "replica",
        "device",
        "server_id",
        "history",
        "operations",
    ] {
        assert!(
            !text.contains(forbidden),
            "manifest mentions {forbidden}: {text}"
        );
    }
}

#[test]
fn corrupted_snapshot_bytes_fail_the_hash() {
    let mut b = backup();
    let snap = b.snapshots.get_mut(&library_a_id()).unwrap();
    let i = snap.iter().position(|c| *c == b'1').unwrap();
    snap[i] = b'2'; // same length, different content
    assert_eq!(code(verify(&b, &Limits::DEFAULT)), C::HashMismatch);
}

#[test]
fn semantically_invalid_snapshots_are_refused_by_sync_protocol() {
    // Not JSON at all.
    let mut b = backup();
    replace_snapshot(&mut b, library_a_id(), b"{not json".to_vec());
    assert_eq!(code(verify(&b, &Limits::DEFAULT)), C::InvalidSnapshot);

    // Duplicate page id (SyncSnapshotV1::validate).
    let mut b = backup();
    let mut s = library_a();
    s.notebooks[2].pages[0].page_id = s.notebooks[0].pages[0].page_id;
    replace_snapshot(&mut b, library_a_id(), serde_json::to_vec(&s).unwrap());
    let e = verify(&b, &Limits::DEFAULT).unwrap_err();
    assert_eq!(e.code, C::InvalidSnapshot);
    assert!(e.message.contains("duplicate page"), "{e}");

    // Unknown field (strict parse).
    let mut b = backup();
    let mut v: serde_json::Value = serde_json::from_slice(&canonical(&library_a())).unwrap();
    v["extra"] = serde_json::json!(1);
    replace_snapshot(&mut b, library_a_id(), serde_json::to_vec(&v).unwrap());
    assert_eq!(code(verify(&b, &Limits::DEFAULT)), C::InvalidSnapshot);

    // Malformed (nil) stroke id.
    let mut b = backup();
    let text = String::from_utf8(canonical(&library_a())).unwrap();
    let sk1 = sync_engine::testkit::sk(1).to_string();
    assert!(text.contains(&sk1));
    let nil = "00000000-0000-0000-0000-000000000000";
    replace_snapshot(&mut b, library_a_id(), text.replace(&sk1, nil).into_bytes());
    assert_eq!(code(verify(&b, &Limits::DEFAULT)), C::InvalidSnapshot);
}

#[test]
fn noncanonical_snapshot_serialization_is_refused() {
    let mut b = backup();
    let pretty = serde_json::to_vec_pretty(&library_a()).unwrap();
    replace_snapshot(&mut b, library_a_id(), pretty);
    assert_eq!(code(verify(&b, &Limits::DEFAULT)), C::NonCanonical);

    // Same content, different key order.
    let mut b = backup();
    let text = String::from_utf8(canonical(&library_b())).unwrap();
    let v_prefix = "{\"v\":1,";
    assert!(text.starts_with(v_prefix));
    let reordered = format!("{{{},\"v\":1}}", &text[v_prefix.len()..text.len() - 1]);
    replace_snapshot(&mut b, library_b_id(), reordered.into_bytes());
    assert_eq!(code(verify(&b, &Limits::DEFAULT)), C::NonCanonical);
}

#[test]
fn unsupported_snapshot_version_is_reported_as_such() {
    let mut b = backup();
    let text = String::from_utf8(canonical(&library_b())).unwrap();
    replace_snapshot(
        &mut b,
        library_b_id(),
        text.replacen("{\"v\":1,", "{\"v\":2,", 1).into_bytes(),
    );
    assert_eq!(code(verify(&b, &Limits::DEFAULT)), C::UnsupportedVersion);
}

#[test]
fn snapshot_must_match_its_path_and_counts() {
    // Manifest lists library X whose snapshot is really library A's bytes.
    let other = LibraryId::from_uuid(sync_engine::testkit::uuid7(1, 99)).unwrap();
    let mut b = backup();
    let a = b.snapshots.remove(&library_a_id()).unwrap();
    with_manifest(&mut b, |m| {
        m.libraries[0].source_library_id = other;
        m.libraries
            .sort_by(|x, y| x.source_library_id.cmp(&y.source_library_id));
    });
    b.snapshots.insert(other, a);
    assert_eq!(code(verify(&b, &Limits::DEFAULT)), C::LibraryMismatch);

    let mut b = backup();
    with_manifest(&mut b, |m| m.libraries[0].page_count += 1);
    assert_eq!(code(verify(&b, &Limits::DEFAULT)), C::CountMismatch);
    let mut b = backup();
    with_manifest(&mut b, |m| m.libraries[1].notebook_count = 0);
    assert_eq!(code(verify(&b, &Limits::DEFAULT)), C::CountMismatch);
}

#[test]
fn snapshot_entries_are_bounded_by_their_declared_length() {
    let b = backup();
    let a = &b.snapshots[&library_a_id()];
    let mut longer = a.clone();
    longer.push(b' ');
    let mut v = BackupVerifier::new(&b.manifest, &Limits::DEFAULT).unwrap();
    assert_eq!(
        code(v.add_snapshot(library_a_id(), &longer[..])),
        C::SizeMismatch
    );
    let mut v = BackupVerifier::new(&b.manifest, &Limits::DEFAULT).unwrap();
    assert_eq!(
        code(v.add_snapshot(library_a_id(), &a[..a.len() - 1])),
        C::SizeMismatch
    );
}

#[test]
fn unexpected_duplicate_and_missing_snapshots() {
    let b = backup();
    let stranger = LibraryId::from_uuid(sync_engine::testkit::uuid7(1, 77)).unwrap();
    let mut v = BackupVerifier::new(&b.manifest, &Limits::DEFAULT).unwrap();
    assert_eq!(
        code(v.add_snapshot(stranger, &b"{}"[..])),
        C::UnexpectedEntry
    );

    let mut v = BackupVerifier::new(&b.manifest, &Limits::DEFAULT).unwrap();
    let a = &b.snapshots[&library_a_id()];
    v.add_snapshot(library_a_id(), &a[..]).unwrap();
    assert_eq!(
        code(v.add_snapshot(library_a_id(), &a[..])),
        C::DuplicateEntry
    );

    let mut v = BackupVerifier::new(&b.manifest, &Limits::DEFAULT).unwrap();
    for (sha, bytes) in &b.blobs {
        v.verify_blob(sha, &bytes[..]).unwrap();
    }
    v.add_snapshot(library_a_id(), &a[..]).unwrap();
    assert_eq!(code(v.finish()), C::MissingEntry);
}

#[test]
fn blob_set_must_match_references_exactly() {
    let (bytes_a, _) = sync_engine::testkit::blob_background();
    let sha_a = sha256_hex(&bytes_a);
    let (bytes_b, _) = blob_b();
    let sha_b = sha256_hex(&bytes_b);

    // A blob's bytes never supplied.
    let mut b = backup();
    b.blobs.remove(&sha_b);
    assert_eq!(code(verify(&b, &Limits::DEFAULT)), C::MissingBlob);

    // A referenced blob absent from the manifest.
    let mut b = backup();
    with_manifest(&mut b, |m| m.blobs.retain(|x| x.sha256 != sha_b));
    b.blobs.remove(&sha_b);
    assert_eq!(code(verify(&b, &Limits::DEFAULT)), C::MissingBlob);

    // A listed blob nobody references (with its bytes present and valid).
    let extra = vec![9u8; 64];
    let extra_sha = sha256_hex(&extra);
    let mut b = backup();
    with_manifest(&mut b, |m| {
        m.blobs.push(portable_backup::ManifestBlob {
            sha256: extra_sha.clone(),
            byte_len: 64,
        });
        m.blobs.sort_by(|x, y| x.sha256.cmp(&y.sha256));
    });
    b.blobs.insert(extra_sha.clone(), extra);
    assert_eq!(code(verify(&b, &Limits::DEFAULT)), C::UnreferencedBlob);

    // A blob entry the manifest does not list at all.
    let b = backup();
    let mut v = BackupVerifier::new(&b.manifest, &Limits::DEFAULT).unwrap();
    assert_eq!(
        code(v.verify_blob(&extra_sha, &[9u8; 64][..])),
        C::UnexpectedEntry
    );

    // Manifest length disagrees with the snapshot reference.
    let mut b = backup();
    with_manifest(&mut b, |m| {
        m.blobs
            .iter_mut()
            .find(|x| x.sha256 == sha_a)
            .unwrap()
            .byte_len += 1;
    });
    let mut v = BackupVerifier::new(&b.manifest, &Limits::DEFAULT).unwrap();
    assert_eq!(
        code(v.add_snapshot(library_b_id(), &b.snapshots[&library_b_id()][..])),
        C::BlobLengthMismatch
    );
}

#[test]
fn blob_bytes_are_streamed_and_checked() {
    let b = backup();
    let (sha, bytes) = b.blobs.iter().next().unwrap();
    let new = || BackupVerifier::new(&b.manifest, &Limits::DEFAULT).unwrap();

    let mut wrong = bytes.clone();
    wrong[0] ^= 0xff;
    assert_eq!(code(new().verify_blob(sha, &wrong[..])), C::HashMismatch);
    assert_eq!(
        code(new().verify_blob(sha, &bytes[..bytes.len() - 1])),
        C::SizeMismatch
    );
    let mut longer = bytes.clone();
    longer.push(0);
    assert_eq!(code(new().verify_blob(sha, &longer[..])), C::SizeMismatch);

    let mut v = new();
    v.verify_blob(sha, &bytes[..]).unwrap();
    assert_eq!(code(v.verify_blob(sha, &bytes[..])), C::DuplicateEntry);

    struct Broken;
    impl std::io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk gone"))
        }
    }
    assert_eq!(code(new().verify_blob(sha, Broken)), C::Io);

    // An endless reader is cut off just past the declared length.
    let endless = std::io::repeat(0u8);
    assert_eq!(code(new().verify_blob(sha, endless)), C::SizeMismatch);
}

#[test]
fn first_refusal_poisons_the_verifier() {
    let b = backup();
    let (sha, bytes) = b.blobs.iter().next().unwrap();
    let mut v = BackupVerifier::new(&b.manifest, &Limits::DEFAULT).unwrap();
    assert!(v.verify_blob(sha, &bytes[1..]).is_err());
    assert_eq!(code(v.verify_blob(sha, &bytes[..])), C::Aborted);
    assert_eq!(
        code(v.add_snapshot(library_a_id(), &b.snapshots[&library_a_id()][..])),
        C::Aborted
    );
    assert_eq!(code(v.finish()), C::Aborted);
}

fn manifest_value() -> serde_json::Value {
    serde_json::from_slice(&backup().manifest).unwrap()
}

fn parse_value(v: &serde_json::Value) -> C {
    code(BackupManifestV1::parse(
        &serde_json::to_vec(v).unwrap(),
        &Limits::DEFAULT,
    ))
}

#[test]
fn invalid_manifests_are_refused() {
    let mut v = manifest_value();
    v["comment"] = serde_json::json!("hi");
    assert_eq!(parse_value(&v), C::InvalidManifest);

    let mut v = manifest_value();
    v["libraries"][0]["source_device"] = serde_json::json!("kindle");
    assert_eq!(parse_value(&v), C::InvalidManifest);

    let mut v = manifest_value();
    v.as_object_mut().unwrap().remove("blobs");
    assert_eq!(parse_value(&v), C::InvalidManifest);

    let mut v = manifest_value();
    v["format"] = serde_json::json!("zip");
    assert_eq!(parse_value(&v), C::InvalidManifest);

    let mut v = manifest_value();
    v["libraries"][0]["source_snapshot_sha256"] = serde_json::json!("ABC");
    assert_eq!(parse_value(&v), C::InvalidManifest);

    let mut v = manifest_value();
    v["libraries"][0]["source_library_id"] = serde_json::json!("not-a-uuid");
    assert_eq!(parse_value(&v), C::InvalidManifest);

    let mut v = manifest_value();
    v["libraries"] = serde_json::json!([]);
    assert_eq!(parse_value(&v), C::InvalidManifest);

    let mut v = manifest_value();
    v["libraries"][0]["source_snapshot_bytes"] = serde_json::json!(0);
    assert_eq!(parse_value(&v), C::InvalidManifest);

    // Unsorted libraries / blobs.
    let mut v = manifest_value();
    v["libraries"].as_array_mut().unwrap().reverse();
    assert_eq!(parse_value(&v), C::InvalidManifest);
    let mut v = manifest_value();
    v["blobs"].as_array_mut().unwrap().reverse();
    assert_eq!(parse_value(&v), C::InvalidManifest);

    // Duplicate IDs.
    let mut v = manifest_value();
    let first = v["libraries"][0].clone();
    v["libraries"].as_array_mut().unwrap().insert(0, first);
    assert_eq!(parse_value(&v), C::DuplicateEntry);
    let mut v = manifest_value();
    let first = v["blobs"][0].clone();
    v["blobs"].as_array_mut().unwrap().insert(0, first);
    assert_eq!(parse_value(&v), C::DuplicateEntry);

    // Valid content, non-canonical spelling.
    let pretty = serde_json::to_vec_pretty(&manifest_value()).unwrap();
    assert_eq!(
        code(BackupManifestV1::parse(&pretty, &Limits::DEFAULT)),
        C::NonCanonical
    );

    assert_eq!(
        code(BackupManifestV1::parse(b"[]", &Limits::DEFAULT)),
        C::InvalidManifest
    );
}

#[test]
fn unsupported_manifest_versions_are_reported_as_such() {
    let mut v = manifest_value();
    v["format_version"] = serde_json::json!(2);
    v["new_field_from_the_future"] = serde_json::json!(true);
    assert_eq!(parse_value(&v), C::UnsupportedVersion);

    let mut v = manifest_value();
    v["snapshot_version"] = serde_json::json!(2);
    assert_eq!(parse_value(&v), C::UnsupportedVersion);
}

#[test]
fn resource_limits_are_enforced() {
    let b = backup();
    let lim = |f: &dyn Fn(&mut Limits)| {
        let mut l = Limits::DEFAULT;
        f(&mut l);
        code(verify(&b, &l))
    };
    assert_eq!(
        lim(&|l| l.max_manifest_bytes = b.manifest.len() as u64 - 1),
        C::LimitExceeded
    );
    assert_eq!(lim(&|l| l.max_libraries = 1), C::LimitExceeded);
    assert_eq!(lim(&|l| l.max_blobs = 1), C::LimitExceeded);
    let a_len = b.snapshots[&library_a_id()].len() as u64;
    let b_len = b.snapshots[&library_b_id()].len() as u64;
    assert_eq!(
        lim(&|l| l.max_snapshot_bytes = a_len.max(b_len) - 1),
        C::LimitExceeded
    );
    assert_eq!(
        lim(&|l| l.max_total_snapshot_bytes = a_len + b_len - 1),
        C::LimitExceeded
    );
    assert_eq!(lim(&|l| l.max_blob_bytes = 1199), C::LimitExceeded);
    assert_eq!(
        lim(&|l| l.max_total_blob_bytes = 2 * 1200 - 1),
        C::LimitExceeded
    );
    // Exactly at every bound still verifies.
    let exact = Limits {
        max_manifest_bytes: b.manifest.len() as u64,
        max_libraries: 2,
        max_snapshot_bytes: a_len.max(b_len),
        max_total_snapshot_bytes: a_len + b_len,
        max_blobs: 2,
        max_blob_bytes: 1200,
        max_total_blob_bytes: 2400,
    };
    verify(&b, &exact).unwrap();
    // A declared size beyond u64 sums never wraps.
    let mut b2 = backup();
    with_manifest(&mut b2, |m| {
        m.blobs[0].byte_len = u64::MAX;
        m.blobs[1].byte_len = u64::MAX;
    });
    let huge = Limits {
        max_blob_bytes: u64::MAX,
        max_total_blob_bytes: u64::MAX,
        ..Limits::DEFAULT
    };
    assert_eq!(
        code(BackupVerifier::new(&b2.manifest, &huge)),
        C::LimitExceeded
    );
}

#[test]
fn entry_paths_route_to_the_verifier() {
    let b = backup();
    let mut v = BackupVerifier::new(&b.manifest, &Limits::DEFAULT).unwrap();
    let mut entries: Vec<(String, Vec<u8>)> = b
        .snapshots
        .iter()
        .map(|(id, s)| (format!("libraries/{id}/snapshot.json"), s.clone()))
        .chain(
            b.blobs
                .iter()
                .map(|(sha, x)| (format!("blobs/{sha}"), x.clone())),
        )
        .collect();
    entries.reverse();
    for (path, bytes) in entries {
        match EntryPath::parse(&path).unwrap() {
            EntryPath::Manifest => unreachable!(),
            EntryPath::Snapshot(id) => {
                v.add_snapshot(id, &bytes[..]).unwrap();
            }
            EntryPath::Blob(sha) => v.verify_blob(&sha, &bytes[..]).unwrap(),
        }
    }
    v.finish().unwrap();
}

fn src(seq: u64, bytes: &[u8]) -> SnapshotSource<'_> {
    SnapshotSource {
        through_server_seq: seq,
        canonical_bytes: bytes,
    }
}

#[test]
fn incremental_builder_matches_for_snapshots_and_bounds_early() {
    let a = canonical(&library_a());
    let bb = canonical(&library_b());
    let mut builder = ManifestBuilder::new();
    let la = builder
        .add_snapshot(src(4211, &bb[..]), &Limits::DEFAULT)
        .unwrap()
        .clone();
    assert_eq!(la.source_library_id, library_b_id());
    assert_eq!(builder.blobs().len(), 2);
    builder
        .add_snapshot(src(u64::MAX, &a[..]), &Limits::DEFAULT)
        .unwrap();
    let built = builder.finish(&Limits::DEFAULT).unwrap();
    let whole = BackupManifestV1::for_snapshots(
        &[src(u64::MAX, &a[..]), src(4211, &bb[..])],
        &Limits::DEFAULT,
    )
    .unwrap();
    assert_eq!(built, whole);
    // Large sequence numbers survive the canonical round trip exactly.
    let bytes = built.to_canonical_bytes(&Limits::DEFAULT).unwrap();
    let back = BackupManifestV1::parse(&bytes, &Limits::DEFAULT).unwrap();
    assert_eq!(back.libraries[0].source_through_server_seq, u64::MAX);

    // Bounds checked before parsing: a too-large snapshot is refused even if it is garbage.
    let tight = Limits {
        max_snapshot_bytes: 4,
        ..Limits::DEFAULT
    };
    assert_eq!(
        code(ManifestBuilder::new().add_snapshot(src(0, b"not json"), &tight)),
        C::LimitExceeded
    );
    let one = Limits {
        max_libraries: 1,
        ..Limits::DEFAULT
    };
    let mut b1 = ManifestBuilder::new();
    b1.add_snapshot(src(0, &a[..]), &one).unwrap();
    assert_eq!(
        code(b1.add_snapshot(src(0, &bb[..]), &one)),
        C::LimitExceeded
    );
    let mut dup = ManifestBuilder::new();
    dup.add_snapshot(src(0, &a[..]), &Limits::DEFAULT).unwrap();
    assert_eq!(
        code(dup.add_snapshot(src(1, &a[..]), &Limits::DEFAULT)),
        C::DuplicateEntry
    );
    // Remaining bounds at finish.
    let few_blobs = Limits {
        max_blobs: 1,
        ..Limits::DEFAULT
    };
    let mut b2 = ManifestBuilder::new();
    b2.add_snapshot(src(0, &bb[..]), &few_blobs).unwrap();
    assert_eq!(code(b2.finish(&few_blobs)), C::LimitExceeded);
    assert_eq!(
        code(ManifestBuilder::new().finish(&Limits::DEFAULT)),
        C::InvalidManifest
    );
}
