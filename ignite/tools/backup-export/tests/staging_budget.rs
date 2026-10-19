//! The optional staging budget (`ExportOptions::max_staging_bytes`): enforced while the export
//! runs (staged snapshots and blobs, then the ZIP), never only after it; an export within the
//! budget is byte-identical to an unbounded one.

use std::cell::Cell;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use backup_export::{
    export_backup, export_backup_with, ExportErrorKind, ExportOptions, ReadOnlySource, SourceError,
    SourceResponse,
};
use portable_backup::Limits;
use scribe_sync_server::Store;
use sync_engine::testkit;
use sync_protocol::{sha256_hex, BackgroundRef, LibraryId};

const BLOBS: u64 = 4;
const SIDE: u32 = 1024; // 1 MiB incompressible background per library

fn temp(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "backup-export-budget-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Total bytes of every file under `dir` (files vanishing mid-walk count as gone).
fn disk_usage(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => disk_usage(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// What the exporter's staging directories (`.<name>.export-*` beside the destination) hold.
/// The published file is a hard link to the staged archive, so it is not counted again.
fn staging_usage(out: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(out) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| disk_usage(&e.path()))
        .sum()
}

/// A ledger whose libraries each use their own incompressible background.
fn ledger(dir: &Path) -> Store {
    let store = Store::open(dir).unwrap();
    let mut x = 0x2545_f491_4f6c_dd1du64;
    for i in 0..BLOBS {
        let bytes: Vec<u8> = (0..SIDE * SIDE)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let sha = sha256_hex(&bytes);
        store.put_blob(sha.as_str(), &bytes).unwrap();
        let mut g = testkit::genesis();
        g.library_id = LibraryId::from_uuid(testkit::uuid7(1, 10 + i)).unwrap();
        for nb in &mut g.notebooks {
            for page in &mut nb.pages {
                if let BackgroundRef::Blob {
                    sha256,
                    byte_len,
                    format,
                } = &mut page.background
                {
                    *sha256 = sha.clone();
                    *byte_len = bytes.len() as u64;
                    (format.width, format.height, format.stride) = (SIDE, SIDE, SIDE);
                }
            }
        }
        store.bootstrap(&g.to_canonical_bytes().unwrap()).unwrap();
    }
    store
}

/// The ledger's own `/v1` GET router; samples the destination directory's disk usage before
/// every response and every body read (each one follows the exporter's previous write).
struct Measured<'a> {
    store: &'a Store,
    out: PathBuf,
    peak: Cell<u64>,
    log: Mutex<Vec<String>>,
}

impl Measured<'_> {
    fn sample(&self) {
        self.peak.set(self.peak.get().max(staging_usage(&self.out)));
    }
}

struct Sampling<'a> {
    inner: std::io::Cursor<Vec<u8>>,
    src: &'a Measured<'a>,
}

impl Read for Sampling<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.src.sample();
        self.inner.read(buf)
    }
}

impl<'s> ReadOnlySource for Measured<'s> {
    fn get(&self, path: &str) -> Result<SourceResponse<'_>, SourceError> {
        self.sample();
        self.log.lock().unwrap().push(path.to_string());
        let r = scribe_sync_server::http::route(self.store, "GET", path, &[]);
        Ok(SourceResponse {
            status: r.status,
            body: Box::new(Sampling {
                inner: std::io::Cursor::new(r.body),
                src: self,
            }),
        })
    }
}

/// Polls `dir`'s disk usage until stopped; returns the largest value seen.
fn poller(dir: PathBuf) -> (Arc<AtomicBool>, std::thread::JoinHandle<u64>) {
    let stop = Arc::new(AtomicBool::new(false));
    let s = Arc::clone(&stop);
    let t = std::thread::spawn(move || {
        let mut peak = 0;
        while !s.load(Ordering::SeqCst) {
            peak = peak.max(staging_usage(&dir));
        }
        peak
    });
    (stop, t)
}

struct Run {
    result: Result<backup_export::ExportReport, backup_export::ExportError>,
    peak: u64,
    requests: Vec<String>,
    leftovers: u64,
}

fn run(store: &Store, out: &Path, budget: Option<u64>) -> Run {
    let src = Measured {
        store,
        out: out.to_path_buf(),
        peak: Cell::new(0),
        log: Mutex::new(Vec::new()),
    };
    let dest = out.join("b.zip");
    let _ = std::fs::remove_file(&dest);
    let (stop, t) = poller(out.to_path_buf());
    let result = export_backup_with(
        &src,
        &dest,
        &Limits::DEFAULT,
        &ExportOptions {
            max_staging_bytes: budget,
        },
    );
    stop.store(true, Ordering::SeqCst);
    let polled = t.join().unwrap();
    let leftovers = disk_usage(out) - std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    Run {
        result,
        peak: polled.max(src.peak.get()),
        requests: src.log.into_inner().unwrap(),
        leftovers,
    }
}

#[test]
fn the_staging_budget_is_enforced_during_generation_and_exactly() {
    let data = temp("ledger");
    let store = ledger(&data);
    let out = temp("out");

    // Unbounded (what the command-line tool does): the reference archive and its needs.
    let reference = export_backup(
        &Measured {
            store: &store,
            out: out.clone(),
            peak: Cell::new(0),
            log: Mutex::new(Vec::new()),
        },
        &out.join("reference.zip"),
        &Limits::DEFAULT,
    )
    .unwrap();
    let reference_bytes = std::fs::read(out.join("reference.zip")).unwrap();
    std::fs::remove_file(out.join("reference.zip")).unwrap();
    let staged: u64 = reference
        .manifest
        .libraries
        .iter()
        .map(|l| l.source_snapshot_bytes)
        .sum::<u64>()
        + reference
            .manifest
            .blobs
            .iter()
            .map(|b| b.byte_len)
            .sum::<u64>();
    let needed = staged + reference.archive_bytes;
    assert!(
        reference.archive_bytes > BLOBS * u64::from(SIDE * SIDE),
        "incompressible"
    );

    // Exactly enough: succeeds, and the archive is byte-identical to the unbounded one.
    let ok = run(&store, &out, Some(needed));
    let report = ok.result.unwrap();
    assert_eq!(std::fs::read(out.join("b.zip")).unwrap(), reference_bytes);
    assert_eq!(report.archive_sha256, reference.archive_sha256);
    assert!(ok.peak <= needed, "peak {} > budget {needed}", ok.peak);
    std::fs::remove_file(out.join("b.zip")).unwrap();

    // One byte short: everything is staged, then the ZIP hits the budget part-way.
    let zip_short = run(&store, &out, Some(needed - 1));
    let e = zip_short.result.unwrap_err();
    assert_eq!(e.kind, ExportErrorKind::StagingLimit, "{e}");
    assert!(
        zip_short.peak < needed,
        "peak {} reached {needed}",
        zip_short.peak
    );
    assert_eq!(
        zip_short
            .requests
            .iter()
            .filter(|r| r.starts_with("/v1/blobs/"))
            .count() as u64,
        BLOBS,
        "failed while building the ZIP"
    );
    assert_eq!(zip_short.leftovers, 0, "staging removed");
    assert!(!out.join("b.zip").exists());

    // Half the ZIP's size: stops well before the archive would be complete.
    let half = staged + reference.archive_bytes / 2;
    let r = run(&store, &out, Some(half));
    assert_eq!(r.result.unwrap_err().kind, ExportErrorKind::StagingLimit);
    assert!(r.peak <= half, "peak {} > budget {half}", r.peak);
    assert_eq!(r.leftovers, 0);

    // Less than the blobs' declared sizes: refused before downloading any blob.
    let small = 2 * u64::from(SIDE * SIDE);
    let r = run(&store, &out, Some(small));
    assert_eq!(r.result.unwrap_err().kind, ExportErrorKind::StagingLimit);
    assert!(
        r.requests.iter().all(|p| !p.starts_with("/v1/blobs/")),
        "{:?}",
        r.requests
    );
    assert!(r.peak <= small);
    assert_eq!(r.leftovers, 0);

    // Smaller than the snapshots themselves: refused while staging them.
    let r = run(&store, &out, Some(10));
    assert_eq!(r.result.unwrap_err().kind, ExportErrorKind::StagingLimit);
    assert!(r.peak <= 10, "peak {}", r.peak);
    assert_eq!(r.leftovers, 0);

    drop(store);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&out);
}
