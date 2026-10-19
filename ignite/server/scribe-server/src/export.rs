//! Slice 1G-D1: administrator backup export (the server's portable backup, downloaded).
//!
//! ```text
//! POST   /control/v1/admin/backup/exports                 start one (202, preparing)
//! GET    /control/v1/admin/backup/exports                 the caller's exports
//! GET    /control/v1/admin/backup/exports/{id}            one of the caller's exports
//! GET    /control/v1/admin/backup/exports/{id}/download   the verified ZIP, streamed
//! DELETE /control/v1/admin/backup/exports/{id}            dismiss a ready or failed export
//! ```
//!
//! Callers: exactly the restore routes' rule (`Plane::restore_principal`): a loopback peer, no
//! `Authorization` header, the session cookie of a non-revoked administrator browser, and CSRF
//! markers on mutations. Exports belong to the administrator that started them; another one
//! sees "not found". Authorization is checked again on every request, downloads included.
//!
//! The archive is built by `backup_export::export_backup_with` (the same deterministic ZIP, manifest
//! and verification as the `ignite-backup-export` tool) reading through [`StoreSource`]: the
//! server's own `/v1` GET router, called in-process (no HTTP, no credential). Each library is
//! captured at its own snapshot boundary, one after another: the archive is not an atomic
//! cross-library copy. The finished ZIP is verified once more with the strict reader that
//! restores use (`backup_archive::inspect_backup`, the restore verification limits) and is
//! downloadable only after that, as `ignite-backup.zip`, streamed from the private file. While
//! it is built, the exporter charges every byte it writes (staged snapshots and blobs, then the
//! ZIP) against [`ExportLimits::max_staging_bytes`] and stops (`too_large`) before exceeding it.
//!
//! Lifecycle: `preparing` → `ready` | `failed`; a ready export expires after
//! [`ExportLimits::ttl_ms`]; ready and failed exports can be dismissed. A preparing export
//! cannot be cancelled (the exporter has no safe interruption point); it ends on its own and a
//! server shutdown waits for it. Staging lives in `<ledger>/export-staging/job-export-<random>`
//! (separate from restore staging); startup removes it all. A deletion that fails keeps the
//! export (and its capacity) as `cleanup_pending`, retried by the sweeper; a deletion is
//! deferred while the file is being downloaded. A failed export's files are deleted at once;
//! its result is kept as a diagnostic until it expires, but at most
//! [`ExportLimits::max_failed_results_per_owner`] per administrator and
//! [`ExportLimits::max_failed_results`] in all (the oldest are forgotten first). Failed exports
//! are never retried automatically.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use backup_archive::inspect_backup;
use backup_export::{
    export_backup_with, ExportError, ExportErrorKind, ExportOptions, ReadOnlySource, SourceError,
    SourceResponse,
};
use portable_backup::Limits;
use scribe_sync_server::{ControlReply, DownloadSlot, RequestInfo, Store};
use serde_json::json;
use sync_protocol::Sha256Hex;

use crate::control::{new_id, Clock, Principal};
use crate::fsutil::random_bytes;
use crate::plane::{err, json, EventSink, Plane};
use crate::restore::{verification_limits, CleanupGate};

/// Private export staging root inside the ledger directory (never restore staging).
pub const EXPORT_STAGING_DIR: &str = "export-staging";
/// Every export's directory starts with this (startup removes all of them).
pub const EXPORT_DIR_PREFIX: &str = "job-export-";
/// The archive's name inside its directory and in `Content-Disposition` (fixed, never derived
/// from library names or paths).
pub const ARCHIVE_FILE: &str = "ignite-backup.zip";

/// Path segments (after `/control/v1/`) of the export routes.
const ROUTE: [&str; 3] = ["admin", "backup", "exports"];

#[derive(Clone, Debug)]
pub struct ExportLimits {
    /// Largest archive offered for download: the restore upload ceiling, so every download can
    /// be restored through the browser.
    pub max_archive_bytes: u64,
    /// Most bytes one export may write while it is built (staged snapshots and blobs plus the
    /// ZIP), enforced by the exporter as it writes: the disk an export can take, whatever the
    /// ledger holds.
    pub max_staging_bytes: u64,
    /// Exports holding staging server-wide (including the one being prepared).
    pub max_exports: usize,
    pub max_exports_per_owner: usize,
    /// How long a ready export (or a failed export's result) is kept.
    pub ttl_ms: u64,
    pub sweep_interval: Duration,
    /// Largest chunk read from the archive per send (bounded memory per download).
    pub download_chunk: usize,
    /// Downloads in progress server-wide.
    pub max_downloads: usize,
    /// Failed results (no files left, kept only as diagnostics) remembered server-wide and per
    /// administrator; beyond these the oldest are forgotten.
    pub max_failed_results: usize,
    pub max_failed_results_per_owner: usize,
    /// The same expansion limits restores verify with.
    pub verify: Limits,
}

impl Default for ExportLimits {
    fn default() -> Self {
        Self {
            max_archive_bytes: 2 << 30,
            // Staged content plus an archive of the largest offered size, and ZIP overhead.
            max_staging_bytes: (4 << 30) + (64 << 20),
            max_exports: 2,
            max_exports_per_owner: 1,
            ttl_ms: 15 * 60 * 1000,
            sweep_interval: Duration::from_secs(30),
            download_chunk: 64 * 1024,
            max_downloads: 4,
            max_failed_results: 4,
            max_failed_results_per_owner: 2,
            verify: verification_limits(),
        }
    }
}

/// Failure injection for the export tests (never enabled by the binary).
#[derive(Debug, Default)]
pub struct ExportHooks {
    /// Pause a worker after it started, before it reads anything.
    pub generation_gate: Mutex<Option<Arc<CleanupGate>>>,
    /// The export fails as if the filesystem had refused it.
    pub fail_generation: AtomicBool,
    /// Damage the finished archive before the final verification.
    pub corrupt_archive: AtomicBool,
    /// Every staging deletion fails.
    pub fail_cleanup: AtomicBool,
}

/// Reads for the exporter: the server's own `/v1` GET router, in-process. GET only (the trait
/// cannot express anything else), `/v1/` paths only, no query. Each answer is the router's
/// in-memory reply (a list, one snapshot, or one blob: bounded by the server's own limits and
/// the exporter's per-read caps), handed over as a reader.
pub struct StoreSource<'a>(pub &'a Store);

impl ReadOnlySource for StoreSource<'_> {
    fn get(&self, path: &str) -> Result<SourceResponse<'_>, SourceError> {
        if !path.starts_with("/v1/") || path.contains("..") || path.contains('?') {
            return Err(SourceError(format!("refused non-/v1 path {path:?}")));
        }
        let reply = scribe_sync_server::http::route(self.0, "GET", path, &[]);
        Ok(SourceResponse {
            status: reply.status,
            body: Box::new(Cursor::new(reply.body)),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportState {
    Preparing,
    Ready,
    Failed,
}

/// What a ready export offers (from the final verification).
#[derive(Clone, Debug)]
struct Archive {
    bytes: u64,
    sha256: Sha256Hex,
    format: &'static str,
    format_version: u32,
    libraries: usize,
    notebooks: u64,
    pages: u64,
    blobs: u64,
    blob_bytes: u64,
}

struct Export {
    /// Creation order (oldest failed results are forgotten first).
    seq: u64,
    owner: String,
    dir: PathBuf,
    file: PathBuf,
    state: ExportState,
    /// Dismissed or expired: no longer offered; kept only until its staging is deleted.
    retired: Option<&'static str>,
    /// Its staging directory still exists (counted against capacity until deleted).
    holds_staging: bool,
    /// Ready: until it expires; failed: until its result is forgotten. Unset while preparing.
    expires_ms: Option<u64>,
    archive: Option<Archive>,
    error: Option<(String, String)>,
    /// Downloads of this file in progress (its deletion waits for them).
    downloads: usize,
}

#[derive(Default)]
struct Workers {
    handles: Vec<JoinHandle<()>>,
    closed: bool,
}

/// The single generation slot; released on drop.
struct GenSlot(Arc<AtomicBool>);

impl Drop for GenSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

pub struct ExportJobs {
    pub limits: ExportLimits,
    pub hooks: ExportHooks,
    staging_root: PathBuf,
    clock: Clock,
    jobs: Mutex<HashMap<String, Export>>,
    next_seq: AtomicU64,
    generating: Arc<AtomicBool>,
    downloads: AtomicUsize,
    store: Mutex<Option<Arc<Store>>>,
    workers: Mutex<Workers>,
}

/// One download in progress: released (counts decremented) when the transport is done with the
/// file, including when the client went away.
struct DownloadTicket {
    jobs: Arc<ExportJobs>,
    id: String,
}

impl Drop for DownloadTicket {
    fn drop(&mut self) {
        if let Some(j) = self
            .jobs
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(&self.id)
        {
            j.downloads = j.downloads.saturating_sub(1);
        }
        self.jobs.downloads.fetch_sub(1, Ordering::SeqCst);
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn remove_dir(dir: &Path, fail: &AtomicBool) -> io::Result<()> {
    if fail.load(Ordering::SeqCst) {
        return Err(io::Error::other("injected cleanup failure"));
    }
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// A safe code and message for an exporter failure (its own messages may name local paths).
fn describe_export_error(e: &ExportError) -> (String, String) {
    // ENOSPC / EDQUOT; ERROR_DISK_FULL / ERROR_HANDLE_DISK_FULL.
    #[cfg(unix)]
    const DISK_FULL: [&str; 2] = ["os error 28)", "os error 122)"];
    #[cfg(not(unix))]
    const DISK_FULL: [&str; 2] = ["os error 112)", "os error 39)"];
    let full =
        matches!(e.kind, ExportErrorKind::Io) && DISK_FULL.iter().any(|m| e.message.contains(m));
    if full {
        return (
            "insufficient_storage".into(),
            "The server does not have enough free space to build the backup.".into(),
        );
    }
    match &e.kind {
        ExportErrorKind::StagingLimit => (
            "too_large".into(),
            "The backup needs more temporary space than an export may use.".into(),
        ),
        ExportErrorKind::Backup(c) => (format!("backup_{}", c.as_str()), e.message.clone()),
        ExportErrorKind::Io => (
            "io_error".into(),
            "A filesystem error stopped the export.".into(),
        ),
        _ => (
            "internal".into(),
            "The export could not read the server's libraries.".into(),
        ),
    }
}

impl ExportJobs {
    pub fn new(staging_root: PathBuf, clock: Clock, limits: ExportLimits) -> Self {
        Self {
            limits,
            hooks: ExportHooks::default(),
            staging_root,
            clock,
            jobs: Mutex::new(HashMap::new()),
            next_seq: AtomicU64::new(0),
            generating: Arc::new(AtomicBool::new(false)),
            downloads: AtomicUsize::new(0),
            store: Mutex::new(None),
            workers: Mutex::new(Workers::default()),
        }
    }

    /// Give exports the server's ledger (the same `Arc` the transport serves).
    pub fn attach_store(&self, store: Arc<Store>) {
        *self.store.lock().unwrap() = Some(store);
    }

    pub fn export_count(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }

    pub fn generating(&self) -> bool {
        self.generating.load(Ordering::SeqCst)
    }

    pub fn downloads_in_progress(&self) -> usize {
        self.downloads.load(Ordering::SeqCst)
    }

    pub fn staging_root(&self) -> &Path {
        &self.staging_root
    }

    /// Begin shutdown: refuse new workers, wait for the running one, then release the ledger.
    pub fn join_workers(&self) {
        let handles = {
            let mut w = self.workers.lock().unwrap();
            w.closed = true;
            std::mem::take(&mut w.handles)
        };
        for h in handles {
            let _ = h.join();
        }
        self.store.lock().unwrap().take();
    }

    fn remove(&self, dir: &Path) -> io::Result<()> {
        remove_dir(dir, &self.hooks.fail_cleanup)
    }

    /// Forget the oldest failed results beyond the per-administrator and server-wide caps.
    /// Only results with nothing left on disk qualify: an export being prepared, a ready one,
    /// one being downloaded, or one whose files still await deletion is never dropped (those
    /// are bounded by the staging capacity instead). With this, records stay within
    /// `max_exports + max_failed_results` (per administrator: `max_exports_per_owner +
    /// max_failed_results_per_owner`).
    fn trim_failed(&self, jobs: &mut HashMap<String, Export>) {
        let mut results: Vec<(u64, String, String)> = jobs
            .iter()
            .filter(|(_, j)| {
                j.state == ExportState::Failed
                    && !j.holds_staging
                    && j.retired.is_none()
                    && j.downloads == 0
            })
            .map(|(id, j)| (j.seq, id.clone(), j.owner.clone()))
            .collect();
        // Newest first: each administrator keeps their newest results, then the server keeps
        // the newest of what remains.
        results.sort_by(|a, b| b.0.cmp(&a.0));
        let mut per_owner: HashMap<String, usize> = HashMap::new();
        let mut total = 0;
        for (_, id, owner) in results {
            let mine = per_owner.entry(owner).or_default();
            if *mine >= self.limits.max_failed_results_per_owner
                || total >= self.limits.max_failed_results
            {
                jobs.remove(&id);
            } else {
                *mine += 1;
                total += 1;
            }
        }
    }

    /// Expire ready exports, forget failed results whose time ran out, and delete the staging
    /// of withdrawn exports nobody is downloading (and of failed ones). Never touches an export
    /// being prepared. Returns the ids whose staging could not be deleted.
    pub fn sweep(&self) -> Vec<String> {
        let now = (self.clock)();
        let mut to_delete: Vec<(String, PathBuf, bool)> = Vec::new();
        {
            let mut jobs = self.jobs.lock().unwrap();
            jobs.retain(|_, j| {
                !(j.state == ExportState::Failed
                    && !j.holds_staging
                    && j.expires_ms.is_some_and(|t| t <= now))
            });
            for (id, j) in jobs.iter_mut() {
                if j.retired.is_none()
                    && j.state == ExportState::Ready
                    && j.expires_ms.is_some_and(|t| t <= now)
                {
                    j.retired = Some("expired");
                }
                if j.holds_staging
                    && j.downloads == 0
                    && (j.retired.is_some() || j.state == ExportState::Failed)
                {
                    to_delete.push((id.clone(), j.dir.clone(), j.retired.is_some()));
                }
            }
        }
        let mut failed = Vec::new();
        for (id, dir, retired) in to_delete {
            if self.remove(&dir).is_err() {
                failed.push(id);
                continue;
            }
            let mut jobs = self.jobs.lock().unwrap();
            if retired {
                jobs.remove(&id);
            } else if let Some(j) = jobs.get_mut(&id) {
                j.holds_staging = false;
            }
        }
        self.trim_failed(&mut self.jobs.lock().unwrap());
        failed
    }

    fn summary(&self, id: &str, j: &Export, now: u64) -> serde_json::Value {
        let state = if j.retired.is_some() {
            "cleanup_pending"
        } else if j.state == ExportState::Ready && j.expires_ms.is_some_and(|t| t <= now) {
            "expired"
        } else {
            match j.state {
                ExportState::Preparing => "preparing",
                ExportState::Ready => "ready",
                ExportState::Failed => "failed",
            }
        };
        let downloadable = state == "ready";
        json!({
            "v": 1,
            "export_id": id,
            "state": state,
            "expires_in_s": j.expires_ms.map(|t| t.saturating_sub(now) / 1000),
            "consistency": "per_library_snapshot",
            "archive": j.archive.as_ref().map(|a| json!({
                "file_name": ARCHIVE_FILE,
                "bytes": a.bytes,
                "sha256": a.sha256.as_str(),
                "format": a.format,
                "format_version": a.format_version,
            })),
            "totals": j.archive.as_ref().map(|a| json!({
                "libraries": a.libraries,
                "notebooks": a.notebooks,
                "pages": a.pages,
                "blobs": a.blobs,
                "blob_bytes": a.blob_bytes,
            })),
            "error": j.error.as_ref().map(|(code, message)| json!({ "code": code, "message": message })),
            "download": downloadable.then(|| format!("/control/v1/admin/backup/exports/{id}/download")),
        })
    }

    /// Run `work` on a worker registered for joining (refused once shutdown began).
    fn register_worker(&self, work: impl FnOnce() + Send + 'static) -> Result<(), &'static str> {
        let mut w = self.workers.lock().unwrap();
        if w.closed {
            return Err("shutting_down");
        }
        w.handles.retain(|h| !h.is_finished());
        let handle = std::thread::Builder::new()
            .name("export-worker".into())
            .spawn(work)
            .map_err(|_| "internal")?;
        w.handles.push(handle);
        Ok(())
    }

    /// The worker: build, verify, then publish the result (or the failure).
    fn execute(
        &self,
        store: &Store,
        events: &EventSink,
        id: String,
        dir: PathBuf,
        file: PathBuf,
        slot: GenSlot,
    ) {
        let gate = self.hooks.generation_gate.lock().unwrap().clone();
        if let Some(g) = gate {
            g.pause();
        }
        let run =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.build(store, &file)));
        let result = run.unwrap_or_else(|_| {
            Err(("internal".into(), "The export stopped unexpectedly.".into()))
        });
        let now = (self.clock)();
        let line = match &result {
            Ok(a) => format!(
                "scribe-server event=backup_export_ready export_id={id} libraries={} bytes={}",
                a.libraries, a.bytes
            ),
            Err((code, _)) => {
                format!("scribe-server event=backup_export_failed export_id={id} error={code}")
            }
        };
        {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(j) = jobs.get_mut(&id) {
                j.expires_ms = Some(now + self.limits.ttl_ms);
                match result {
                    Ok(a) => {
                        j.state = ExportState::Ready;
                        j.archive = Some(a);
                    }
                    Err(e) => {
                        j.state = ExportState::Failed;
                        j.error = Some(e);
                    }
                }
            }
        }
        events(&line);
        // A failed export's partial files go at once (retried by the sweeper if that fails).
        let failed = self
            .jobs
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|j| j.state == ExportState::Failed);
        if failed && self.remove(&dir).is_ok() {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(j) = jobs.get_mut(&id) {
                j.holds_staging = false;
            }
            self.trim_failed(&mut jobs);
        }
        drop(slot);
    }

    fn build(&self, store: &Store, file: &Path) -> Result<Archive, (String, String)> {
        if self.hooks.fail_generation.load(Ordering::SeqCst) {
            return Err((
                "io_error".into(),
                "A filesystem error stopped the export.".into(),
            ));
        }
        let libraries = store.list_libraries().map_err(|_| {
            (
                "internal".to_string(),
                "The server's libraries could not be listed.".to_string(),
            )
        })?;
        if libraries.is_empty() {
            return Err((
                "no_libraries".into(),
                "The server has no libraries to back up.".into(),
            ));
        }
        let options = ExportOptions {
            max_staging_bytes: Some(self.limits.max_staging_bytes),
        };
        let report = export_backup_with(&StoreSource(store), file, &self.limits.verify, &options)
            .map_err(|e| describe_export_error(&e))?;
        if self.hooks.corrupt_archive.load(Ordering::SeqCst) {
            let mut bytes = fs::read(file).map_err(|_| ("io_error".to_string(), String::new()))?;
            let mid = bytes.len() / 2;
            bytes[mid] ^= 0xff;
            fs::write(file, bytes).map_err(|_| ("io_error".to_string(), String::new()))?;
        }
        if report.archive_bytes > self.limits.max_archive_bytes {
            return Err((
                "too_large".into(),
                format!(
                    "The backup is {} bytes, more than the {} bytes a browser restore accepts.",
                    report.archive_bytes, self.limits.max_archive_bytes
                ),
            ));
        }
        // The strict reader restores use: what is offered can be restored.
        let plan = inspect_backup(file, &self.limits.verify).map_err(|e| {
            (
                "verification_failed".to_string(),
                format!("The finished backup did not pass verification ({e}). It was not offered."),
            )
        })?;
        if plan.archive_sha256 != report.archive_sha256
            || plan.archive_bytes != report.archive_bytes
        {
            return Err((
                "verification_failed".into(),
                "The finished backup changed while it was verified. It was not offered.".into(),
            ));
        }
        Ok(Archive {
            bytes: plan.archive_bytes,
            sha256: plan.archive_sha256,
            format: plan.format,
            format_version: plan.format_version,
            libraries: plan.libraries.len(),
            notebooks: plan.total_notebooks,
            pages: plan.total_pages,
            blobs: plan.blob_count,
            blob_bytes: plan.total_blob_bytes,
        })
    }
}

/// Periodic expiry and cleanup of exports; stopped on drop, which then waits for a running
/// export and releases the ledger.
pub struct Sweeper {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    jobs: Option<Arc<ExportJobs>>,
}

impl Sweeper {
    pub fn start(jobs: Arc<ExportJobs>, events: EventSink) -> Self {
        let (tx, rx) = mpsc::channel::<()>();
        let interval = jobs.limits.sweep_interval;
        let owned = Arc::clone(&jobs);
        let thread = std::thread::Builder::new()
            .name("export-sweeper".into())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(interval) {
                    for id in jobs.sweep() {
                        events(&format!(
                            "scribe-server event=backup_export_cleanup_failed export_id={id}"
                        ));
                    }
                }
            })
            .expect("spawn export sweeper");
        Self {
            stop: Some(tx),
            thread: Some(thread),
            jobs: Some(owned),
        }
    }
}

impl Drop for Sweeper {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(j) = self.jobs.take() {
            j.join_workers();
        }
    }
}

/// Remove every export staging directory left by an earlier process. Call only while the
/// ledger's process lock is held and before serving.
pub fn remove_export_staging(root: &Path) -> Result<usize, String> {
    let mut n = 0;
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("list export staging: {e}")),
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(EXPORT_DIR_PREFIX) {
            fs::remove_dir_all(e.path())
                .map_err(|err| format!("remove export staging {name}: {err}"))?;
            n += 1;
        }
    }
    Ok(n)
}

fn segments(req: &RequestInfo) -> Vec<&str> {
    req.path()
        .strip_prefix("/control/v1/")
        .map(|r| r.split('/').filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

impl Plane {
    pub(crate) fn is_export_download(req: &RequestInfo) -> bool {
        req.method == "GET"
            && matches!(segments(req).as_slice(), [a, b, c, _, "download"] if [*a, *b, *c] == ROUTE)
    }

    /// `/control/v1/admin/backup/...` except downloads (CSRF already checked by `handle`).
    pub(crate) fn export_route(&self, req: &RequestInfo, rest: &[&str]) -> ControlReply {
        let p = match self.restore_principal(req) {
            Ok(p) => p,
            Err(r) => return r,
        };
        match (rest, req.method.as_str()) {
            (["exports"], "POST") => self.export_start(&p),
            (["exports"], "GET") => self.export_list(&p),
            (["exports", id], "GET") => self.export_status(&p, id),
            (["exports", id], "DELETE") => self.export_dismiss(&p, id),
            (["exports", _, "download"], _) => {
                err(405, "method_not_allowed", "downloads use GET (no ranges)")
            }
            _ => err(404, "not_found", "unknown admin route"),
        }
    }

    fn export_start(&self, p: &Principal) -> ControlReply {
        let exports = &self.exports;
        let Some(store) = exports.store.lock().unwrap().clone() else {
            return err(503, "shutting_down", "The server is shutting down.");
        };
        let now = self.store.now();
        let mut table = exports.jobs.lock().unwrap();
        let held = table.values().filter(|j| j.holds_staging);
        let mine = held.clone().filter(|j| j.owner == p.device_id).count();
        if mine >= exports.limits.max_exports_per_owner {
            return err(
                429,
                "export_exists",
                "You already have a backup export. Download or dismiss it (or wait for it to finish) first.",
            );
        }
        if held.count() >= exports.limits.max_exports {
            return err(
                429,
                "too_many_exports",
                "The server holds the maximum number of backup exports. Try again later.",
            );
        }
        if exports
            .generating
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return err(
                429,
                "export_busy",
                "Another backup is being prepared. Try again shortly.",
            );
        }
        let slot = GenSlot(Arc::clone(&exports.generating));
        let mut r = [0u8; 16];
        if random_bytes(&mut r).is_err() {
            return err(500, "internal", "randomness unavailable");
        }
        let dir = exports
            .staging_root
            .join(format!("{EXPORT_DIR_PREFIX}{}", hex(&r)));
        #[allow(unused_mut)] // only Unix sets a mode
        let mut b = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        if let Err(e) = b.create(&dir) {
            let full =
                e.kind() == io::ErrorKind::StorageFull || e.kind() == io::ErrorKind::QuotaExceeded;
            return if full {
                err(
                    507,
                    "insufficient_storage",
                    "The server does not have enough free space for a backup export.",
                )
            } else {
                err(500, "internal", "could not prepare the export")
            };
        }
        let id = loop {
            let id = new_id(now);
            if !table.contains_key(&id) {
                break id;
            }
        };
        let file = dir.join(ARCHIVE_FILE);
        table.insert(
            id.clone(),
            Export {
                seq: exports.next_seq.fetch_add(1, Ordering::SeqCst),
                owner: p.device_id.clone(),
                dir: dir.clone(),
                file: file.clone(),
                state: ExportState::Preparing,
                retired: None,
                holds_staging: true,
                expires_ms: None,
                archive: None,
                error: None,
                downloads: 0,
            },
        );
        let summary = exports.summary(&id, &table[&id], now);
        drop(table);
        self.event(
            "backup_export_started",
            &[("export_id", &id), ("by", &p.device_id)],
        );
        let jobs = Arc::clone(exports);
        let events = self.events.clone();
        let (wid, wdir) = (id.clone(), dir.clone());
        if let Err(code) =
            exports.register_worker(move || jobs.execute(&store, &events, wid, wdir, file, slot))
        {
            // The caller hears the failure now: no result is kept for it. If its directory
            // cannot be deleted, the record stays (failed, holding staging) for the sweeper.
            let mut table = exports.jobs.lock().unwrap();
            if let Some(j) = table.get_mut(&id) {
                j.state = ExportState::Failed;
                j.expires_ms = Some(now + exports.limits.ttl_ms);
                j.error = Some((code.into(), "The export could not be started.".into()));
            }
            drop(table);
            if exports.remove(&dir).is_ok() {
                exports.jobs.lock().unwrap().remove(&id);
            }
            let status = if code == "shutting_down" { 503 } else { 500 };
            return err(status, code, "The export could not be started.");
        }
        json(202, &summary)
    }

    fn export_list(&self, p: &Principal) -> ControlReply {
        let now = self.store.now();
        let table = self.exports.jobs.lock().unwrap();
        let mut mine: Vec<(&String, &Export)> = table
            .iter()
            .filter(|(_, j)| j.owner == p.device_id)
            .collect();
        mine.sort_by(|a, b| a.0.cmp(b.0));
        let list: Vec<serde_json::Value> = mine
            .into_iter()
            .map(|(id, j)| self.exports.summary(id, j, now))
            .collect();
        json(200, &json!({ "v": 1, "exports": list }))
    }

    fn export_status(&self, p: &Principal, id: &str) -> ControlReply {
        let now = self.store.now();
        let mut table = self.exports.jobs.lock().unwrap();
        // Unknown and foreign-owned exports are indistinguishable.
        let Some(j) = table.get_mut(id).filter(|j| j.owner == p.device_id) else {
            return err(404, "not_found", "No such backup export.");
        };
        if j.retired.is_none()
            && j.state == ExportState::Ready
            && j.expires_ms.is_some_and(|t| t <= now)
        {
            j.retired = Some("expired"); // never offered again; the sweeper deletes it
        }
        if j.retired == Some("expired") {
            return err(
                410,
                "expired",
                "This backup export expired. Start a new one.",
            );
        }
        json(200, &self.exports.summary(id, j, now))
    }

    fn export_dismiss(&self, p: &Principal, id: &str) -> ControlReply {
        let exports = &self.exports;
        let mut table = exports.jobs.lock().unwrap();
        let Some(j) = table.get_mut(id).filter(|j| j.owner == p.device_id) else {
            return err(404, "not_found", "No such backup export.");
        };
        if j.state == ExportState::Preparing {
            return err(409, "export_preparing", "This backup is still being prepared and cannot be cancelled. Dismiss it when it is ready.");
        }
        j.retired.get_or_insert("dismissed");
        if j.downloads > 0 {
            // Deleted by the sweeper once the download in progress has finished.
            return json(
                200,
                &json!({ "ok": true, "export_id": id, "cleanup": "deferred" }),
            );
        }
        if !j.holds_staging {
            table.remove(id);
            return json(200, &json!({ "ok": true, "export_id": id }));
        }
        let dir = j.dir.clone();
        drop(table);
        if exports.remove(&dir).is_err() {
            self.event("backup_export_cleanup_failed", &[("export_id", id)]);
            return err(500, "cleanup_failed", "The export was withdrawn, but its files could not be deleted yet. Deletion is retried automatically.");
        }
        exports.jobs.lock().unwrap().remove(id);
        self.event(
            "backup_export_dismissed",
            &[("export_id", id), ("by", &p.device_id)],
        );
        json(200, &json!({ "ok": true, "export_id": id }))
    }

    /// Authorize (again) and open a ready export for streaming.
    pub(crate) fn export_begin_download(
        &self,
        req: &RequestInfo,
    ) -> Result<DownloadSlot, ControlReply> {
        let p = self.restore_principal(req)?;
        let segs = segments(req);
        let id = segs.get(3).copied().unwrap_or_default();
        let exports = &self.exports;
        let now = self.store.now();
        let mut table = exports.jobs.lock().unwrap();
        let Some(j) = table.get_mut(id).filter(|j| j.owner == p.device_id) else {
            return Err(err(404, "not_found", "No such backup export."));
        };
        if j.retired.is_some()
            || (j.state == ExportState::Ready && j.expires_ms.is_some_and(|t| t <= now))
        {
            return Err(err(
                410,
                "expired",
                "This backup export is no longer available. Start a new one.",
            ));
        }
        let archive = match (j.state, &j.archive) {
            (ExportState::Ready, Some(a)) => a.clone(),
            (ExportState::Preparing, _) => {
                return Err(err(
                    409,
                    "export_preparing",
                    "This backup is still being prepared.",
                ))
            }
            _ => {
                return Err(err(
                    409,
                    "export_failed",
                    "This backup export failed; nothing can be downloaded.",
                ))
            }
        };
        if exports
            .downloads
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < exports.limits.max_downloads).then_some(n + 1)
            })
            .is_err()
        {
            return Err(err(
                429,
                "download_busy",
                "Too many backup downloads are in progress. Try again shortly.",
            ));
        }
        j.downloads += 1;
        // From here on the ticket gives both counts back, whatever happens.
        let ticket = DownloadTicket {
            jobs: Arc::clone(exports),
            id: id.to_string(),
        };
        let path = j.file.clone();
        drop(table);
        let file = File::open(&path)
            .map_err(|_| err(500, "internal", "The backup file could not be opened."))?;
        let length = file.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
        if length != archive.bytes {
            return Err(err(
                500,
                "internal",
                "The backup file does not match its verification.",
            ));
        }
        self.event(
            "backup_export_download",
            &[("export_id", id), ("by", &p.device_id)],
        );
        Ok(DownloadSlot {
            file,
            length,
            content_type: "application/zip",
            headers: vec![(
                "Content-Disposition",
                format!("attachment; filename=\"{ARCHIVE_FILE}\""),
            )],
            chunk: exports.limits.download_chunk,
            ticket: Box::new(ticket),
        })
    }
}
