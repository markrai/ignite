//! Slice 1F-B1: administrative backup upload and inspection (no restoration yet).
//!
//! ```text
//! POST   /control/v1/admin/restore/jobs        raw ZIP (application/zip), streamed → 201 summary
//! GET    /control/v1/admin/restore/jobs/{id}   the owner's prepared summary
//! DELETE /control/v1/admin/restore/jobs/{id}   cancel: forget the job, delete its staged archive
//! ```
//!
//! Caller (every route, checked from headers before any body byte is read): a **loopback** TCP
//! peer (defense in depth only: a local proxy could relay remote traffic; `X-Forwarded-*` is
//! never consulted), **no** `Authorization` header (a Bearer is refused even for an
//! administrator's credential), and the session **cookie** of a non-revoked `Role::Admin`
//! **browser** device. Mutations also need the control plane's CSRF markers.
//!
//! An upload is streamed by the transport into `<ledger>/restore-staging/job-upload-<random>/
//! archive.zip` (directory created exclusively, 0700; file `create_new`, 0600; on Windows both
//! inherit the private state directory's ACL). Nothing is extracted: the archive is verified
//! in one streaming pass by `backup_archive::inspect_backup` under [`verification_limits`]. The
//! job then holds the verified hash, the read-only plan and an immutable proposed mapping onto
//! fresh library ids. No ledger row, blob file or blob index row is ever written here.
//!
//! Jobs live in memory only. Expired jobs are removed by [`Sweeper`] (and lazily); a restart
//! forgets them and startup removes every `job-*` staging directory under the ledger lock.
//!
//! Cleanup is never assumed to succeed. A cancelled or expired job whose staging directory
//! cannot be deleted stays in the table as `cleanup_pending` (unusable, still counted against
//! capacity) and the sweeper retries the deletion. A failed upload's directory that its drop
//! guard cannot delete is remembered the same way (counted against the server-wide capacity,
//! retried by the sweeper). What remains unavoidable: a deletion that keeps failing keeps that
//! capacity until it succeeds or the server restarts (startup then fails closed if a `job-*`
//! directory still cannot be removed), and files can only be deleted as far as the operating
//! system allows (e.g. a file another process holds open on Windows).

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use backup_archive::{inspect_backup, RestoreErrorKind, RestorePlan};
use control_protocol::{ClientType, Role};
use portable_backup::Limits;
use scribe_sync_server::{
    ControlReply, RequestInfo, Store, StoreError, UploadSlot, UploadTicket, UploadedBody,
};
use serde_json::json;
use sync_protocol::LibraryId;

use crate::control::{new_id, verifier, Clock, Principal};
use crate::fsutil::random_bytes;
use crate::plane::{cookie_value, csrf_ok, err, has_authorization, json, EventSink, Plane};

/// Path segments (after `/control/v1/`) of the streaming upload route.
pub const UPLOAD_ROUTE: [&str; 3] = ["admin", "restore", "jobs"];
/// Staging directories of prepared uploads (the `job-` prefix is what
/// `backup_restore::remove_restore_staging` removes at startup).
pub const UPLOAD_DIR_PREFIX: &str = "job-upload-";
pub const ARCHIVE_FILE: &str = "archive.zip";

/// The server-side limits of the HTTP restore path.
#[derive(Clone, Debug)]
pub struct RestoreLimits {
    /// Transport ceiling for the compressed archive (HTTP body bytes = stored upload bytes).
    pub max_upload_bytes: u64,
    /// Prepared jobs server-wide, counting an upload in progress.
    pub max_jobs: usize,
    pub max_jobs_per_owner: usize,
    pub job_ttl_ms: u64,
    pub idle_timeout: Duration,
    pub upload_deadline: Duration,
    pub sweep_interval: Duration,
    /// What the archive may expand to; enforced while inspecting, not just from the manifest.
    pub verify: Limits,
}

impl Default for RestoreLimits {
    fn default() -> Self {
        Self {
            max_upload_bytes: 2 << 30,
            max_jobs: 2,
            max_jobs_per_owner: 1,
            job_ttl_ms: 15 * 60 * 1000,
            idle_timeout: Duration::from_secs(30),
            upload_deadline: Duration::from_secs(30 * 60),
            sweep_interval: Duration::from_secs(30),
            verify: verification_limits(),
        }
    }
}

/// Conservative expansion limits for archives received over HTTP (far below
/// `Limits::DEFAULT`, which allows ~66 GiB). A Kindle page background is ~4.6 MB and a whole
/// library snapshot is typically well under 1 MiB, so these leave ample headroom.
pub const fn verification_limits() -> Limits {
    Limits {
        max_manifest_bytes: 8 << 20,
        max_libraries: 64,
        // One snapshot is held in memory while it is verified: this bounds the inspector.
        max_snapshot_bytes: 64 << 20,
        max_total_snapshot_bytes: 512 << 20,
        max_blobs: 10_000,
        max_blob_bytes: 32 << 20,
        max_total_blob_bytes: 4 << 30,
    }
}

/// Failure injection for the restore HTTP tests (never enabled by the binary).
#[derive(Debug, Default)]
pub struct RestoreHooks {
    /// Treat every peer as non-loopback (it can only refuse more).
    pub treat_peers_as_remote: AtomicBool,
    /// The staged-archive writer fails with "storage full" after this many bytes (0 = off).
    pub fail_write_after_bytes: AtomicU64,
    /// Every staging deletion fails (shared with upload drop guards).
    pub fail_cleanup: Arc<AtomicBool>,
    /// Pause a sweep's orphan cleanup after it has begun and before anything is deleted.
    pub orphan_cleanup_gate: Mutex<Option<Arc<CleanupGate>>>,
}

/// Test rendezvous for [`RestoreHooks::orphan_cleanup_gate`]: the sweep signals `started`, then
/// waits for `resume`.
pub struct CleanupGate {
    started: Mutex<mpsc::Sender<()>>,
    resume: Mutex<mpsc::Receiver<()>>,
}

impl CleanupGate {
    /// The gate, the receiver of "cleanup started" and the sender of "resume".
    pub fn new() -> (Arc<Self>, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (started_tx, started_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let gate = Arc::new(Self {
            started: Mutex::new(started_tx),
            resume: Mutex::new(resume_rx),
        });
        (gate, started_rx, resume_tx)
    }

    fn pause(&self) {
        let _ = self.started.lock().unwrap().send(());
        // Bounded, so a broken test fails instead of hanging the server.
        let _ = self
            .resume
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10));
    }
}

impl std::fmt::Debug for CleanupGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CleanupGate")
    }
}

/// Prepared-job lifecycle. 1F-B1 only creates `Ready` jobs; 1F-B2 adds execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Ready,
    Executing,
    Succeeded,
    Failed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Executing => "executing",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

struct Job {
    owner: String,
    dir: PathBuf,
    /// The verified staged archive (read by 1F-B2's confirmation).
    #[allow(dead_code)]
    archive: PathBuf,
    plan: RestorePlan,
    /// Immutable proposal: source → fresh destination, in plan order.
    mapping: Vec<(LibraryId, LibraryId)>,
    /// SHA-256 of the one-time confirmation secret (returned once, never stored; 1F-B2).
    #[allow(dead_code)]
    confirmation_verifier: [u8; 32],
    expires_ms: u64,
    state: JobState,
    /// Cancelled or expired ([`CANCELLED`] / [`EXPIRED`]): unusable, kept only until its
    /// staging directory is deleted.
    retired: Option<&'static str>,
}

const CANCELLED: &str = "cancelled";
const EXPIRED: &str = "expired";

impl Job {
    fn expired(&self, now: u64) -> bool {
        self.retired == Some(EXPIRED)
            || (self.retired.is_none() && self.state == JobState::Ready && self.expires_ms <= now)
    }

    fn usable(&self, now: u64) -> bool {
        self.retired.is_none() && self.state == JobState::Ready && self.expires_ms > now
    }
}

/// What one sweep did (for event lines).
#[derive(Debug, Default)]
pub struct SweepReport {
    /// Jobs that expired in this sweep.
    pub expired: Vec<String>,
    /// Retired jobs whose staging is now deleted and which are forgotten.
    pub removed: Vec<String>,
    /// Retired jobs whose staging could not be deleted (retried next sweep).
    pub cleanup_failed: Vec<String>,
    /// Failed uploads' directories still waiting for deletion.
    pub orphans_pending: usize,
}

/// Delete a staging directory (already gone counts as deleted).
fn remove_staging_dir(dir: &Path, fail: &AtomicBool) -> io::Result<()> {
    if fail.load(Ordering::SeqCst) {
        return Err(io::Error::other("injected cleanup failure"));
    }
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Prepared restore jobs of this server process.
pub struct RestoreJobs {
    pub limits: RestoreLimits,
    pub hooks: RestoreHooks,
    staging_root: PathBuf,
    clock: Clock,
    uploading: Arc<AtomicBool>,
    jobs: Mutex<HashMap<String, Job>>,
    /// Failed uploads' staging directories their drop guard could not delete. An entry leaves
    /// this list only after its directory was deleted, so it is counted until then.
    orphans: Arc<Mutex<Vec<Orphan>>>,
}

/// Synchronization invariant (capacity is never undercounted, never double-counted):
/// * an orphan is in `orphans` from the moment its deletion first failed until a deletion
///   succeeded; a sweep only marks it `cleaning` (so concurrent sweeps skip it) and removes it
///   after deleting the directory, so `pending_cleanup()` counts it throughout;
/// * the drop guard that registers an orphan runs while its upload still holds the single
///   upload permit, so no other upload can be admitted in between;
/// * a retired job stays in `jobs` until its directory was deleted (`clean_retired`);
/// * lock order is `jobs` then `orphans` (`occupied`); no code takes `jobs` while holding
///   `orphans`, and no lock is held while deleting.
struct Orphan {
    path: PathBuf,
    cleaning: bool,
}

/// The single upload slot; released on drop.
struct UploadPermit(Arc<AtomicBool>);

impl Drop for UploadPermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// A staging directory removed on drop unless kept. A deletion that fails is handed to the
/// sweeper (and keeps counting against capacity) instead of being forgotten.
struct StagingDir {
    path: PathBuf,
    keep: bool,
    fail: Arc<AtomicBool>,
    orphans: Arc<Mutex<Vec<Orphan>>>,
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if !self.keep && remove_staging_dir(&self.path, &self.fail).is_err() {
            let mut o = self.orphans.lock().unwrap_or_else(|p| p.into_inner());
            o.push(Orphan {
                path: std::mem::take(&mut self.path),
                cleaning: false,
            });
        }
    }
}

/// Transport ticket of one upload. Fields drop in order: the staging directory (unless kept)
/// is removed before the upload slot is released.
struct UploadState {
    owner: String,
    archive: PathBuf,
    dir: StagingDir,
    _permit: UploadPermit,
}

/// The staged archive file, with the storage-full fault hook.
struct StagedWriter {
    file: File,
    written: u64,
    fail_after: u64,
}

impl Write for StagedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.fail_after != 0 && self.written + buf.len() as u64 > self.fail_after {
            return Err(io::Error::from(io::ErrorKind::StorageFull));
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn storage_error(e: &io::Error) -> ControlReply {
    if e.kind() == io::ErrorKind::StorageFull || e.kind() == io::ErrorKind::QuotaExceeded {
        err(
            507,
            "insufficient_storage",
            "not enough space to stage the upload",
        )
    } else {
        err(500, "internal", "could not stage the upload")
    }
}

fn too_many_jobs() -> ControlReply {
    err(
        429,
        "too_many_jobs",
        "Too many prepared restores. Cancel one or wait for it to expire.",
    )
}

impl RestoreJobs {
    pub fn new(staging_root: PathBuf, clock: Clock, limits: RestoreLimits) -> Self {
        Self {
            limits,
            hooks: RestoreHooks::default(),
            staging_root,
            clock,
            uploading: Arc::new(AtomicBool::new(false)),
            jobs: Mutex::new(HashMap::new()),
            orphans: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Failed uploads' staging directories still waiting for deletion.
    pub fn pending_cleanup(&self) -> usize {
        self.orphans.lock().unwrap().len()
    }

    fn remove_dir(&self, dir: &Path) -> io::Result<()> {
        remove_staging_dir(dir, &self.hooks.fail_cleanup)
    }

    /// Delete a retired job's staging; forget the job only once that succeeded.
    fn clean_retired(&self, id: &str, dir: &Path) -> bool {
        if self.remove_dir(dir).is_err() {
            return false;
        }
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.get(id).is_some_and(|j| j.retired.is_some()) {
            jobs.remove(id);
        }
        true
    }

    /// Capacity in use: every held job (usable or awaiting cleanup) and every orphan.
    fn occupied(&self, jobs: &HashMap<String, Job>, owner: &str) -> (usize, usize) {
        let mine = jobs.values().filter(|j| j.owner == owner).count();
        (jobs.len() + self.pending_cleanup(), mine)
    }

    pub fn staging_root(&self) -> &Path {
        &self.staging_root
    }

    /// Jobs currently held (any state).
    pub fn job_count(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }

    /// Is an upload slot taken right now?
    pub fn upload_in_progress(&self) -> bool {
        self.uploading.load(Ordering::SeqCst)
    }

    /// Retire every expired `Ready` job, then delete the staging of every retired job and of
    /// every orphaned upload; whatever cannot be deleted stays (and counts) for the next sweep.
    /// Jobs in any other state are never touched (1F-B2: an executing restore owns its
    /// staging).
    pub fn sweep(&self) -> SweepReport {
        let now = (self.clock)();
        let mut report = SweepReport::default();
        let retired: Vec<(String, PathBuf)> = {
            let mut jobs = self.jobs.lock().unwrap();
            for (id, j) in jobs.iter_mut() {
                if j.retired.is_none() && j.state == JobState::Ready && j.expires_ms <= now {
                    j.retired = Some(EXPIRED);
                    report.expired.push(id.clone());
                }
            }
            jobs.iter()
                .filter(|(_, j)| j.retired.is_some())
                .map(|(id, j)| (id.clone(), j.dir.clone()))
                .collect()
        };
        for (id, dir) in retired {
            if self.clean_retired(&id, &dir) {
                report.removed.push(id);
            } else {
                report.cleanup_failed.push(id);
            }
        }
        // Claim the orphans no other sweep is cleaning; they stay listed (and counted).
        let claimed: Vec<PathBuf> = {
            let mut orphans = self.orphans.lock().unwrap();
            orphans
                .iter_mut()
                .filter(|o| !o.cleaning)
                .map(|o| {
                    o.cleaning = true;
                    o.path.clone()
                })
                .collect()
        };
        if !claimed.is_empty() {
            let gate = self.hooks.orphan_cleanup_gate.lock().unwrap().clone();
            if let Some(g) = gate {
                g.pause();
            }
        }
        let deleted: Vec<bool> = claimed.iter().map(|d| self.remove_dir(d).is_ok()).collect();
        let mut orphans = self.orphans.lock().unwrap();
        for (path, ok) in claimed.iter().zip(deleted) {
            if let Some(i) = orphans.iter().position(|o| o.path == *path) {
                if ok {
                    orphans.remove(i);
                } else {
                    orphans[i].cleaning = false;
                }
            }
        }
        report.orphans_pending = orphans.len();
        report
    }

    /// Take the upload slot and check job capacity (the upload counts as a job).
    fn reserve(&self, owner: &str) -> Result<UploadPermit, ControlReply> {
        if self
            .uploading
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(err(
                429,
                "upload_busy",
                "Another backup upload is in progress. Try again shortly.",
            ));
        }
        let permit = UploadPermit(Arc::clone(&self.uploading));
        let jobs = self.jobs.lock().unwrap();
        let (all, mine) = self.occupied(&jobs, owner);
        if all + 1 > self.limits.max_jobs || mine + 1 > self.limits.max_jobs_per_owner {
            return Err(too_many_jobs());
        }
        Ok(permit)
    }

    /// Create the private staging directory and archive file (exclusively; never an existing
    /// name, so no symlink or earlier file is followed or reused).
    fn create_staging(&self) -> io::Result<(StagingDir, PathBuf, File)> {
        let mut r = [0u8; 16];
        random_bytes(&mut r).map_err(io::Error::other)?;
        let path = self
            .staging_root
            .join(format!("{UPLOAD_DIR_PREFIX}{}", hex(&r)));
        #[allow(unused_mut)] // only Unix sets a mode
        let mut b = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(&path)?;
        let dir = StagingDir {
            path,
            keep: false,
            fail: Arc::clone(&self.hooks.fail_cleanup),
            orphans: Arc::clone(&self.orphans),
        };
        let archive = dir.path.join(ARCHIVE_FILE);
        let mut o = OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let file = o.open(&archive)?;
        Ok((dir, archive, file))
    }

    fn summary(&self, id: &str, j: &Job, now: u64) -> serde_json::Value {
        let p = &j.plan;
        let state = if j.retired.is_some() {
            "cleanup_pending"
        } else if j.expired(now) {
            EXPIRED
        } else {
            j.state.as_str()
        };
        json!({
            "v": 1,
            "job_id": id,
            "state": state,
            "usable": j.usable(now),
            "expires_in_s": j.expires_ms.saturating_sub(now) / 1000,
            "archive": {
                "sha256": p.archive_sha256.as_str(),
                "bytes": p.archive_bytes,
                "format": p.format,
                "format_version": p.format_version,
                "snapshot_version": p.snapshot_version,
            },
            "scope": "current_state_only",
            "not_restored": p.not_restored,
            "replaces_existing": false,
            "existing_libraries": "never replaced, merged or modified; every library is restored under a new identity",
            "totals": {
                "libraries": p.libraries.len(),
                "notebooks": p.total_notebooks,
                "pages": p.total_pages,
                "blobs": p.blob_count,
                "blob_bytes": p.total_blob_bytes,
            },
            "libraries": p.libraries.iter().zip(&j.mapping).map(|(l, (src, dst))| {
                debug_assert_eq!(l.source_library_id, *src);
                json!({
                    "source_library_id": src.to_string(),
                    "destination_library_id": dst.to_string(),
                    "source_snapshot_sha256": l.source_snapshot_sha256.as_str(),
                    "source_snapshot_bytes": l.source_snapshot_bytes,
                    "source_through_server_seq": l.source_through_server_seq,
                    "notebook_count": l.notebook_count,
                    "page_count": l.page_count,
                    "blob_count": l.blob_count,
                })
            }).collect::<Vec<_>>(),
            "warnings": p.warnings,
        })
    }
}

/// Periodic expiry of prepared jobs; stopped and joined on drop.
pub struct Sweeper {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Sweeper {
    pub fn start(jobs: Arc<RestoreJobs>, events: EventSink) -> Self {
        let (tx, rx) = mpsc::channel::<()>();
        let interval = jobs.limits.sweep_interval;
        let thread = std::thread::Builder::new()
            .name("restore-sweeper".into())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(interval) {
                    log_sweep(&events, &jobs.sweep());
                }
            })
            .expect("spawn restore sweeper");
        Self {
            stop: Some(tx),
            thread: Some(thread),
        }
    }
}

impl Drop for Sweeper {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn log_sweep(events: &EventSink, r: &SweepReport) {
    for id in &r.expired {
        events(&format!(
            "scribe-server event=restore_job_expired job_id={id}"
        ));
    }
    for id in &r.cleanup_failed {
        events(&format!(
            "scribe-server event=restore_cleanup_failed job_id={id}"
        ));
    }
    if r.orphans_pending > 0 {
        events(&format!(
            "scribe-server event=restore_cleanup_failed orphaned_uploads={}",
            r.orphans_pending
        ));
    }
}

/// A fresh library identity: the control plane's UUIDv7 (ms timestamp + 74 CSPRNG bits).
fn new_library_id(now: u64) -> LibraryId {
    LibraryId::parse(&new_id(now)).expect("a UUIDv7 is a valid library id")
}

impl Plane {
    fn restore_peer_ok(&self, req: &RequestInfo) -> bool {
        !self
            .restore
            .hooks
            .treat_peers_as_remote
            .load(Ordering::SeqCst)
            && req
                .peer
                .is_some_and(|p| p.ip().to_canonical().is_loopback())
    }

    /// The caller of a restore route: loopback peer, no `Authorization` header, cookie session
    /// of a non-revoked browser administrator. Checked again before a job is registered.
    pub(crate) fn restore_principal(&self, req: &RequestInfo) -> Result<Principal, ControlReply> {
        if !self.restore_peer_ok(req) {
            return Err(err(
                403,
                "loopback_required",
                "Backup restore is only available on the server itself (or an SSH tunnel to it).",
            ));
        }
        if has_authorization(req) {
            return Err(err(
                403,
                "browser_session_required",
                "Backup restore requires an administrator browser session, not a bearer credential.",
            ));
        }
        let Some(cookie) = cookie_value(req.cookie.as_deref()) else {
            return Err(err(401, "unauthorized", "not signed in"));
        };
        let p = match self.store.authenticate(cookie) {
            Ok(Some(p)) => p,
            Ok(None) => return Err(err(401, "unauthorized", "not signed in")),
            Err(_) => return Err(err(500, "internal", "control storage error")),
        };
        if p.role != Role::Admin || p.client_type != ClientType::Browser {
            return Err(err(403, "forbidden", "administrator access required"));
        }
        Ok(p)
    }

    pub(crate) fn is_restore_upload(req: &RequestInfo) -> bool {
        if req.method != "POST" {
            return false;
        }
        let Some(rest) = req.path().strip_prefix("/control/v1/") else {
            return false;
        };
        let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        segs == UPLOAD_ROUTE
    }

    /// Headers only, before any body byte: CSRF, caller, content type and declared length,
    /// upload slot and job capacity, then the private staging file.
    pub(crate) fn restore_begin_upload(
        &self,
        req: &RequestInfo,
    ) -> Result<UploadSlot, ControlReply> {
        if !csrf_ok(req) {
            return Err(err(
                403,
                "csrf",
                "missing X-Scribe-Request or cross-origin request",
            ));
        }
        let p = self.restore_principal(req)?;
        let media = req
            .content_type
            .as_deref()
            .and_then(|c| c.split(';').next())
            .map(str::trim);
        if !media.is_some_and(|m| m.eq_ignore_ascii_case("application/zip")) {
            return Err(err(
                400,
                "wrong_content_type",
                "the backup must be sent as application/zip",
            ));
        }
        let limits = &self.restore.limits;
        if let Some(declared) = req.content_length.as_deref() {
            match declared.trim().parse::<u64>() {
                Ok(n) if n > limits.max_upload_bytes => {
                    return Err(err(
                        413,
                        "body_too_large",
                        &format!("limit {} bytes", limits.max_upload_bytes),
                    ))
                }
                Ok(_) => {}
                Err(_) => return Err(err(400, "bad_request", "invalid Content-Length")),
            }
        }
        let permit = self.restore.reserve(&p.device_id)?;
        let (dir, archive, file) = self
            .restore
            .create_staging()
            .map_err(|e| storage_error(&e))?;
        let sink = BufWriter::with_capacity(
            256 * 1024,
            StagedWriter {
                file,
                written: 0,
                fail_after: self
                    .restore
                    .hooks
                    .fail_write_after_bytes
                    .load(Ordering::SeqCst),
            },
        );
        Ok(UploadSlot {
            sink: Box::new(sink),
            max_bytes: limits.max_upload_bytes,
            idle_timeout: limits.idle_timeout,
            deadline: limits.upload_deadline,
            ticket: Box::new(UploadState {
                owner: p.device_id,
                archive,
                dir,
                _permit: permit,
            }),
        })
    }

    /// The archive is fully received: sync, verify (streaming, nothing extracted), re-check
    /// the caller, propose fresh destinations and register the job.
    pub(crate) fn restore_finish_upload(
        &self,
        ledger: &Store,
        req: &RequestInfo,
        ticket: UploadTicket,
        body: UploadedBody,
    ) -> ControlReply {
        let Ok(mut up) = ticket.downcast::<UploadState>() else {
            return err(500, "internal", "upload state lost");
        };
        // Write access is needed to flush on Windows; nothing is written.
        if let Err(e) = OpenOptions::new()
            .write(true)
            .open(&up.archive)
            .and_then(|f| f.sync_all())
        {
            return storage_error(&e);
        }
        let plan = match inspect_backup(&up.archive, &self.restore.limits.verify) {
            Ok(plan) => plan,
            Err(e) if e.kind == RestoreErrorKind::Io => {
                return err(500, "internal", "could not read the staged upload")
            }
            Err(e) => {
                let code = match e.kind {
                    RestoreErrorKind::Zip(c) => format!("zip_{}", c.as_str()),
                    RestoreErrorKind::Backup(c) => format!("backup_{}", c.as_str()),
                    _ => "invalid".into(),
                };
                self.event(
                    "restore_upload_refused",
                    &[("by", &up.owner), ("archive_error", &code)],
                );
                return json(
                    422,
                    &json!({ "error": "archive_invalid", "archive_error": code, "message": e.message }),
                );
            }
        };
        if plan.archive_sha256 != body.sha256 || plan.archive_bytes != body.bytes {
            return err(
                500,
                "internal",
                "the staged upload changed after it was received",
            );
        }
        // Authorization may have been revoked while the upload streamed.
        match self.restore_principal(req) {
            Ok(p) if p.device_id == up.owner => {}
            Ok(_) => return err(401, "unauthorized", "not signed in"),
            Err(r) => return r,
        }
        let now = self.store.now();
        let mapping = match propose_destinations(ledger, &plan, now) {
            Ok(m) => m,
            Err(r) => return r,
        };
        let mut secret = [0u8; 32];
        if random_bytes(&mut secret).is_err() {
            return err(500, "internal", "randomness unavailable");
        }
        let confirmation = control_protocol::b64url(&secret);
        let jobs = &self.restore;
        let mut table = jobs.jobs.lock().unwrap();
        // This upload's own reservation is its slot, so it is not counted here again.
        let (all, mine) = jobs.occupied(&table, &up.owner);
        if all >= jobs.limits.max_jobs || mine >= jobs.limits.max_jobs_per_owner {
            return too_many_jobs();
        }
        let id = loop {
            let id = new_id(now);
            if !table.contains_key(&id) {
                break id;
            }
        };
        up.dir.keep = true;
        let job = Job {
            owner: up.owner.clone(),
            dir: up.dir.path.clone(),
            archive: up.archive.clone(),
            plan,
            mapping,
            confirmation_verifier: verifier(&secret),
            expires_ms: now + jobs.limits.job_ttl_ms,
            state: JobState::Ready,
            retired: None,
        };
        let mut out = jobs.summary(&id, &job, now);
        out["confirmation"] = json!(confirmation);
        self.event(
            "restore_upload_prepared",
            &[
                ("job_id", &id),
                ("by", &job.owner),
                ("bytes", &job.plan.archive_bytes.to_string()),
                ("libraries", &job.plan.libraries.len().to_string()),
            ],
        );
        table.insert(id, job);
        json(201, &out)
    }

    /// `GET /control/v1/admin/restore/jobs` (the caller's own jobs) and
    /// `GET`/`DELETE /control/v1/admin/restore/jobs/{id}` (CSRF already checked by `handle`).
    pub(crate) fn restore_route(&self, req: &RequestInfo, rest: &[&str]) -> ControlReply {
        let p = match self.restore_principal(req) {
            Ok(p) => p,
            Err(r) => return r,
        };
        match (rest, req.method.as_str()) {
            (["jobs"], "GET") => self.restore_list(&p),
            (["jobs", id], method @ ("GET" | "DELETE")) => self.restore_job(&p, id, method),
            _ => err(404, "not_found", "unknown admin route"),
        }
    }

    /// The caller's jobs only (a lost 201 can be recovered: summary, never the confirmation).
    fn restore_list(&self, p: &Principal) -> ControlReply {
        log_sweep(&self.events, &self.restore.sweep());
        let now = self.store.now();
        let table = self.restore.jobs.lock().unwrap();
        let mut mine: Vec<(&String, &Job)> = table
            .iter()
            .filter(|(_, j)| j.owner == p.device_id)
            .collect();
        mine.sort_by_key(|(id, j)| (j.expires_ms, (*id).clone()));
        let jobs: Vec<serde_json::Value> = mine
            .into_iter()
            .map(|(id, j)| self.restore.summary(id, j, now))
            .collect();
        json(200, &json!({ "v": 1, "jobs": jobs }))
    }

    fn restore_job(&self, p: &Principal, id: &str, method: &str) -> ControlReply {
        let now = self.store.now();
        let mut table = self.restore.jobs.lock().unwrap();
        // Unknown and foreign-owned jobs are indistinguishable.
        let Some(job) = table.get_mut(id).filter(|j| j.owner == p.device_id) else {
            return err(404, "not_found", "No such restore job.");
        };
        if job.state == JobState::Executing {
            return if method == "GET" {
                json(200, &self.restore.summary(id, job, now))
            } else {
                err(409, "job_executing", "This restore is running.")
            };
        }
        if method == "GET" && job.usable(now) {
            return json(200, &self.restore.summary(id, job, now));
        }
        // Withdraw it (an expired job is never revived), then delete its staging. The job is
        // forgotten, releasing its capacity, only once that deletion succeeded.
        let newly = job.retired.is_none();
        let reason = if job.expired(now) { EXPIRED } else { CANCELLED };
        let reason = *job.retired.get_or_insert(reason);
        let dir = job.dir.clone();
        drop(table);
        if newly {
            let what = if reason == EXPIRED {
                "restore_job_expired"
            } else {
                "restore_job_cancelled"
            };
            self.event(what, &[("job_id", id), ("by", &p.device_id)]);
        }
        let cleaned = self.restore.clean_retired(id, &dir);
        if !cleaned {
            self.event("restore_cleanup_failed", &[("job_id", id)]);
        }
        match (method, reason, cleaned) {
            ("DELETE", _, false) => err(
                500,
                "cleanup_failed",
                "The prepared restore was withdrawn, but its staged data could not be deleted yet. Deletion is retried automatically.",
            ),
            (_, EXPIRED, _) => err(
                410,
                "expired",
                "This prepared restore expired. Upload the backup again.",
            ),
            ("GET", _, _) => err(410, "cancelled", "This prepared restore was cancelled."),
            _ => json(200, &json!({ "ok": true, "job_id": id })),
        }
    }
}

/// One fresh, unused identity per archive library (never a source id of this archive, never
/// an existing library, never repeated).
fn propose_destinations(
    ledger: &Store,
    plan: &RestorePlan,
    now: u64,
) -> Result<Vec<(LibraryId, LibraryId)>, ControlReply> {
    let sources: BTreeSet<LibraryId> = plan.libraries.iter().map(|l| l.source_library_id).collect();
    let mut chosen = BTreeSet::new();
    let mut out = Vec::with_capacity(plan.libraries.len());
    for l in &plan.libraries {
        let mut picked = None;
        for _ in 0..16 {
            let id = new_library_id(now);
            if sources.contains(&id) || chosen.contains(&id) {
                continue;
            }
            match ledger.library(&id.to_string()) {
                Err(StoreError::NotFound { .. }) => {
                    picked = Some(id);
                    break;
                }
                Ok(_) => continue,
                Err(_) => return Err(err(500, "internal", "ledger storage error")),
            }
        }
        let Some(id) = picked else {
            return Err(err(
                500,
                "internal",
                "could not allocate a library identity",
            ));
        };
        chosen.insert(id);
        out.push((l.source_library_id, id));
    }
    Ok(out)
}
