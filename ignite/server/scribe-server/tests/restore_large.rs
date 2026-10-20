//! Slice 1H-B1R: real-size snapshots through the HTTP restore limits.
//!
//! A real library (24 notebooks / 71 pages of handwriting) is ~77 MB of canonical snapshot JSON,
//! which the former 64 MiB per-snapshot cap refused. One synthetic library of that size (pressure
//! strokes made of random-walk samples, like a pen's) is generated once and taken through the
//! whole path: export, strict inspection at the old and new caps, HTTP upload, confirmed restore
//! and a server-side export of the restored server. Exact boundaries and the cumulative limit are
//! covered with small archives in `backup-archive`'s tests.
//!
//! The real-size test is `#[ignore]`d in the default (unoptimized) run, where it takes about two
//! minutes; it runs in seconds with `--release -- --ignored`.

mod common;

use backup_archive::{inspect_backup, RestoreErrorKind, ZipErrorCode};
use common::*;
use scribe_server::export::{ExportLimits, StoreSource};
use scribe_server::restore::verification_limits;
use sync_protocol::{
    NotebookId, PageId, SnapshotNotebook, SnapshotPage, StrokeId, StrokeStyle, SyncPoint,
    SyncSnapshotV1, SyncStroke,
};

const VEGA_SNAPSHOT_BYTES: u64 = 77_313_154;

/// Deterministic xorshift.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 {
        (self.next() % 1_000_000) as f32 / 1_000_000.0
    }
}

/// 24 notebooks / 71 pages of pressure strokes until the canonical snapshot is at least
/// `target` bytes (each sample is ~49 bytes of JSON).
fn large_library(target: u64) -> Vec<u8> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut snap = SyncSnapshotV1 {
        library_id: LibraryId::from_uuid(testkit::uuid7(0xA0, 1)).unwrap(),
        notebooks: Vec::new(),
        folders: Vec::new(),
        ..testkit::genesis()
    };
    let mut page = 0u64;
    for n in 0..24u64 {
        snap.notebooks.push(SnapshotNotebook {
            notebook_id: NotebookId::from_uuid(testkit::uuid7(0x4E, n + 1)).unwrap(),
            title: format!("Synthetic notebook {}", n + 1),
            pages: (0..if n == 23 { 2 } else { 3 })
                .map(|_| {
                    page += 1;
                    SnapshotPage {
                        page_id: PageId::from_uuid(testkit::uuid7(0x50, page)).unwrap(),
                        background: sync_protocol::BackgroundRef::Blank,
                        render: testkit::render(true),
                        strokes: Vec::new(),
                        texts: Vec::new(),
                        links: Vec::new(),
                    }
                })
                .collect(),
            folder_id: None,
        });
    }
    let (w, h) = (
        sync_protocol::CANONICAL_PAGE_WIDTH - 1.0,
        sync_protocol::CANONICAL_PAGE_HEIGHT - 1.0,
    );
    let (mut estimate, mut id) = (20_000u64, 0u64);
    while estimate < target {
        let slot = (id % 71) as usize;
        let n = 40 + (rng.next() % 260) as usize;
        id += 1;
        let (mut x, mut y) = (
            500.0 + rng.unit() * (w - 1000.0),
            500.0 + rng.unit() * (h - 1000.0),
        );
        let (mut dx, mut dy) = (0.0f32, 0.0f32);
        let points = (0..n)
            .map(|_| {
                dx = (dx + rng.unit() * 4.0 - 2.0).clamp(-14.0, 14.0);
                dy = (dy + rng.unit() * 4.0 - 2.0).clamp(-14.0, 14.0);
                x = (x + dx).clamp(1.0, w);
                y = (y + dy).clamp(1.0, h);
                let p = ((40.0 + rng.unit() * 200.0).round() / 256.0).min(1.0);
                SyncPoint::new((x * 8.0).round() / 8.0, (y * 8.0).round() / 8.0, Some(p))
            })
            .collect();
        let sid = StrokeId::from_uuid(testkit::uuid7(0x51, id)).unwrap();
        snap.notebooks[slot / 3].pages[slot % 3]
            .strokes
            .push(SyncStroke {
                stroke_id: sid,
                root_id: sid,
                style: StrokeStyle::PressureWidthV1,
                points,
            });
        estimate += 49 * n as u64 + 160;
    }
    snap.to_canonical_bytes().unwrap()
}

/// The former HTTP verification limits (only the per-snapshot cap differed).
fn former_limits() -> Limits {
    Limits {
        max_snapshot_bytes: 64 << 20,
        ..verification_limits()
    }
}

#[test]
fn the_verification_limits_raise_only_the_per_snapshot_cap_and_export_shares_them() {
    let l = verification_limits();
    assert_eq!(l.max_snapshot_bytes, 128 << 20);
    // Everything else as before.
    assert_eq!(
        l,
        Limits {
            max_snapshot_bytes: 128 << 20,
            ..former_limits()
        }
    );
    assert_eq!(
        (l.max_libraries, l.max_total_snapshot_bytes),
        (64, 512 << 20)
    );
    assert_eq!(
        (l.max_blobs, l.max_blob_bytes, l.max_total_blob_bytes),
        (10_000, 32 << 20, 4 << 30)
    );
    assert_eq!(l.max_manifest_bytes, 8 << 20);
    assert_eq!(RestoreLimits::default().max_upload_bytes, 2 << 30);
    assert_eq!(RestoreLimits::default().verify, l);
    // The server-side export verifies what it offers with exactly the restore's limits.
    assert_eq!(ExportLimits::default().verify, l);
    // The portable-backup format's broad defaults are untouched.
    assert_eq!(Limits::DEFAULT.max_snapshot_bytes, 512 << 20);
}

#[test]
#[ignore = "real-size (77 MB) path: ~2 min unoptimized; run `cargo test --release -p scribe-server --test restore_large -- --ignored`"]
fn a_vega_sized_snapshot_restores_over_http_and_the_restored_server_exports_it_again() {
    // A library the size of Vega's (77.3 MB), exported by the real exporter.
    let bytes = large_library(78_650_000);
    let size = bytes.len() as u64;
    assert!(size > 64 << 20 && size < 128 << 20, "{size}");
    assert!(
        size.abs_diff(VEGA_SNAPSHOT_BYTES) < 1 << 20,
        "close to the real library: {size}"
    );
    let source_dir = temp("large-source");
    let source = Store::open(&source_dir).unwrap();
    let (info, _) = source.bootstrap(&bytes).unwrap();
    let zip = source_dir.join("large.zip");
    export_backup(&StoreSource(&source), &zip, &Limits::DEFAULT).unwrap();
    drop(source);

    // Refused before only because of the cap; accepted now with every other check.
    let old = inspect_backup(&zip, &former_limits()).unwrap_err();
    assert_eq!(
        old.kind,
        RestoreErrorKind::Zip(ZipErrorCode::LimitExceeded),
        "{old}"
    );
    let plan = inspect_backup(&zip, &verification_limits()).unwrap();
    assert_eq!(
        (plan.libraries.len(), plan.total_notebooks, plan.total_pages),
        (1, 24, 71)
    );
    assert_eq!(plan.libraries[0].source_snapshot_bytes, size);

    // Upload, review and confirm over HTTP on an independent server.
    let env = Env::new("large-target");
    let before = env.ledger();
    let srv = env.start(limits());
    let admin = claim(&env, &srv);
    let archive = std::fs::read(&zip).unwrap();
    let up = upload(&srv, &cookie(&admin), &archive);
    assert_eq!(up.status, 201, "{:?}", up.body);
    assert_eq!(up.body["totals"]["pages"], 71);
    let job_id = up.body["job_id"].as_str().unwrap().to_string();
    let dest = up.body["libraries"][0]["destination_library_id"]
        .as_str()
        .unwrap()
        .to_string();
    let body = json!({
        "v": 1,
        "confirmation": up.body["confirmation"],
        "archive_sha256": up.body["archive"]["sha256"],
        "destinations": [{ "source_library_id": info.library_id.to_string(), "destination_library_id": dest }],
    });
    let c = call(
        &srv.base,
        "POST",
        &format!("{UPLOAD}/{job_id}/confirm"),
        &cookie(&admin),
        Some(body),
    );
    assert_eq!(c.status, 202, "{:?}", c.body);
    let end = Instant::now() + Duration::from_secs(300);
    let done = loop {
        let g = job(&srv, "GET", &job_id, &cookie(&admin));
        let state = g.body["state"].as_str().unwrap_or_default().to_string();
        if state == "succeeded" || state == "failed" {
            break g.body;
        }
        assert!(Instant::now() < end, "still {state}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(done["state"], "succeeded", "{done}");

    // Exactly the backed-up state under the new id (the id occurs once, at the top).
    let source_id = info.library_id.to_string();
    let rekeyed = String::from_utf8(bytes)
        .unwrap()
        .replacen(&source_id, &dest, 1);
    let expected = sha256_hex(rekeyed.as_bytes());
    assert_eq!(
        done["result"]["report"]["libraries"][0]["restored_snapshot_sha256"],
        expected.as_str()
    );
    let listed = call(&srv.base, "GET", "/v1/libraries", &cookie(&admin), None);
    let row = listed.body["libraries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["library_id"] == dest.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        (
            row["head_server_seq"].as_u64(),
            row["snapshot_sha256"].as_str()
        ),
        (Some(0), Some(expected.as_str()))
    );
    let after = env.ledger();
    for (t, rows) in &before.tables {
        for r in rows {
            assert!(after.tables[t].contains(r), "existing {t} row changed");
        }
    }

    // The restored server's own export (same limits) offers it again.
    let start = call(
        &srv.base,
        "POST",
        "/control/v1/admin/backup/exports",
        &cookie(&admin),
        None,
    );
    assert_eq!(start.status, 202, "{:?}", start.body);
    let export_id = start.body["export_id"].as_str().unwrap().to_string();
    let ready = loop {
        let s = call(
            &srv.base,
            "GET",
            &format!("/control/v1/admin/backup/exports/{export_id}"),
            &cookie(&admin),
            None,
        );
        if s.body["state"] != "preparing" {
            break s.body;
        }
        assert!(Instant::now() < end, "export still preparing");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(ready["state"], "ready", "{ready}");
    assert_eq!(ready["totals"]["libraries"], 2); // Env's seeded library + the restored one
    srv.stop();
    let _ = std::fs::remove_dir_all(&source_dir);
}

/// Rewrite the declared (uncompressed) size of `name` in its local header and central
/// directory record. The CRC and data are left alone: only the declaration lies.
fn declare_size(zip: &mut [u8], name: &str, size: u32) {
    let mut patched = 0;
    for i in 0..zip.len().saturating_sub(46) {
        let sig = u32::from_le_bytes(zip[i..i + 4].try_into().unwrap());
        let (name_len_at, name_at, size_at) = match sig {
            0x0403_4b50 => (26, 30, 22),
            0x0201_4b50 => (28, 46, 24),
            _ => continue,
        };
        let n = u16::from_le_bytes(
            zip[i + name_len_at..i + name_len_at + 2]
                .try_into()
                .unwrap(),
        ) as usize;
        if zip.get(i + name_at..i + name_at + n) == Some(name.as_bytes()) {
            zip[i + size_at..i + size_at + 4].copy_from_slice(&size.to_le_bytes());
            patched += 1;
        }
    }
    assert_eq!(patched, 2, "local header and central directory record");
}

#[test]
fn a_snapshot_declared_over_128_mib_is_refused_before_anything_is_inflated() {
    let b = backup();
    let name = b
        .entries()
        .into_iter()
        .map(|(n, _)| n)
        .find(|n| n.starts_with("libraries/"))
        .unwrap();
    for declared in [(128u32 << 20) + 1, u32::MAX - 1] {
        let mut zip = b.bytes.clone();
        declare_size(&mut zip, &name, declared);
        let path = b.dir.join(format!("declared-{declared}.zip"));
        std::fs::write(&path, &zip).unwrap();
        let t = Instant::now();
        let e = inspect_backup(&path, &verification_limits()).unwrap_err();
        assert_eq!(
            e.kind,
            RestoreErrorKind::Zip(ZipErrorCode::LimitExceeded),
            "{e}"
        );
        assert!(e.message.contains("limit 134217728"), "{e}");
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "decided from the declaration alone"
        );
    }
    // Through the HTTP upload as well: refused, nothing staged.
    let env = Env::new("declared-over");
    let srv = env.start(limits());
    let admin = claim(&env, &srv);
    let mut zip = b.bytes.clone();
    declare_size(&mut zip, &name, (128 << 20) + 1);
    let r = upload(&srv, &cookie(&admin), &zip);
    assert_eq!(
        (r.status, r.body["archive_error"].as_str()),
        (422, Some("zip_limit_exceeded")),
        "{:?}",
        r.body
    );
    assert!(
        env.staging().is_empty(),
        "nothing staged: {:?}",
        env.staging()
    );
    srv.stop();
}
