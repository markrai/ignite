//! Durable verified-stamp fast path for startup reconciliation (Slice 18C / 18C.1 / 18C.2).
//!
//! After a successful full reconcile, a stamp under `--sync-dir` records sync cursors, the
//! library's local mutation generation, and a census hash of canonical library files. The next
//! launch may skip `build_snapshot` only when that stamp still matches.
//!
//! **Invalidation:** every durable library mutation must successfully [`note_library_mutation`]
//! *before* the mutation's content becomes durable. Failure is fail-closed: the caller must not
//! persist the content write. The first successful call per library root in a process performs a
//! durable gen bump (and clears any stamp); concurrent callers wait for that outcome; later
//! calls for that library skip the filesystem flush.
//!
//! Census v3 fingerprints small metadata by content (not path+size alone) so equal-length
//! legacy rewrites of titles / `page.meta` cannot keep a trusted stamp. Journals and
//! `background.raw` remain size-only (append-only / covered by `page.meta` checksum).
//!
//! Does **not** use mtime as the sole proof. Missing/corrupt/unsupported stamps → full reconcile.

use std::cell::Cell;
#[cfg(test)]
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sync_protocol::{sha256_hex, LibraryId, ReplicaId};

pub use crate::page_ledger::LedgerScope;
use crate::sync_local::SYNC_DIR;

/// Stamp schema written under `--sync-dir`.
pub const STAMP_FILE: &str = "local-verified-v1.json";
/// Durable tombstone left after invalidation so the next launch can distinguish "never stamped"
/// (existing install) from "stamped, then dirtied".
pub const STAMP_CLEARED_FILE: &str = "local-verified-v1.json.cleared";
/// Stamp format that includes a content-aware census (18C.2). v1/v2 stamps are untrusted.
pub const STAMP_VERSION: u32 = 3;
/// Generation counter under `<library>/.scribe-sync/` (app-owned; not notebook content).
pub const MUTATION_GEN_FILE: &str = "local-mutation-gen";
/// Durable ordinary-authoring hold, next to the mutation generation file.
/// Presence means recovery maintenance is active. Not a second accepted-history ledger.
pub const RECOVERY_MAINTENANCE_FILE: &str = "recovery-maintenance";
/// Intent filename (lives in `--sync-dir`, which is outside the library).
pub const RECOVERY_INTENT_FILE: &str = "recovery-intent-v1.json";
/// Ordinary durable mutation refused because recovery maintenance is active or unreadable.
pub const REFUSE_RECOVERY_MAINTENANCE: &str =
    "REFUSE RECOVERY MAINTENANCE: ordinary authoring is held";

const CENSUS_FILENAMES: &[&str] = &[
    "library.meta",
    "notebook.meta",
    "notebook.info",
    "page.meta",
    "strokes.jsonl",
    "edits.jsonl",
    "texts.jsonl",
    "links.jsonl",
    "background.raw",
    "folders.json",
    "identity.json",
    ".scribe-library-migration-v1",
    "import-origin.json",
];

/// Metadata rewritten atomically by supported writers, including older Ignite versions.
/// Content is hashed because a same-size title / header replace is ordinary.
const CENSUS_HASH_CONTENTS: &[&str] = &[
    "library.meta",
    "notebook.meta",
    "notebook.info",
    "page.meta",
    "texts.jsonl",
    "links.jsonl",
    "folders.json",
    "identity.json",
    ".scribe-library-migration-v1",
    "import-origin.json",
];

enum SessionInv {
    /// Durable invalidation has not succeeded for this library in this process.
    Open,
    /// First durable invalidation succeeded; further notes skip the generation fsync.
    DurablyInvalidated,
}

struct SessionState {
    inv: SessionInv,
    inflight_ordinary: u32,
    /// Prepare is freezing a snapshot; ordinary and inbound writers wait.
    quiescing: bool,
    /// Changed-page ledger state for the current generation (record-only, Slice 19B.3C).
    ledger: crate::page_ledger::SessionLedger,
}

struct SessionCell {
    state: Mutex<SessionState>,
    cv: Condvar,
}

struct SessionTable {
    cells: HashMap<PathBuf, Arc<SessionCell>>,
}

thread_local! {
    static SYNC_MUTATION_PERMIT: Cell<u32> = const { Cell::new(0) };
}

/// RAII admission for one durable library mutation. Dropped only after the content write
/// finishes so recovery prepare cannot freeze a snapshot while the write is still in flight.
#[must_use = "keep this guard until the associated durable write has finished"]
pub struct MutationAdmit {
    cell: Option<Arc<SessionCell>>,
    ordinary: bool,
}

impl MutationAdmit {
    pub(crate) fn untracked() -> Self {
        Self {
            cell: None,
            ordinary: false,
        }
    }
}

impl std::fmt::Debug for MutationAdmit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MutationAdmit")
            .field("ordinary", &self.ordinary)
            .field("tracked", &self.cell.is_some())
            .finish()
    }
}

impl Drop for MutationAdmit {
    fn drop(&mut self) {
        let Some(cell) = self.cell.take() else {
            return;
        };
        #[cfg(test)]
        if let Some(f) = hooks().on_admit_drop {
            f();
        }
        if !self.ordinary {
            return;
        }
        let mut g = cell.state.lock().unwrap_or_else(|p| p.into_inner());
        g.inflight_ordinary = g.inflight_ordinary.saturating_sub(1);
        cell.cv.notify_all();
    }
}

/// Hold that drains ordinary in-flight writes and then blocks new admissions until dropped.
/// Publish recovery intent (and the library-local maintenance flag) before dropping.
pub struct MaintenanceHold {
    cell: Arc<SessionCell>,
}

impl Drop for MaintenanceHold {
    fn drop(&mut self) {
        let mut g = self.cell.state.lock().unwrap_or_else(|p| p.into_inner());
        g.quiescing = false;
        self.cell.cv.notify_all();
    }
}

/// Per-library invalidation cells (avoids per-stroke fsync after the first success).
static SESSION: Mutex<Option<SessionTable>> = Mutex::new(None);
/// Live library_root → sync_dir registrations (supports concurrent test libraries).
static REGISTRY: Mutex<Option<HashMap<PathBuf, PathBuf>>> = Mutex::new(None);
#[cfg(test)]
static LAST_REASON: Mutex<Option<FastPathReason>> = Mutex::new(None);

#[cfg(test)]
thread_local! {
    static HOOKS: RefCell<InvalidateHooks> = RefCell::new(InvalidateHooks::default());
}

fn session_table() -> std::sync::MutexGuard<'static, Option<SessionTable>> {
    let mut g = SESSION.lock().unwrap_or_else(|p| p.into_inner());
    if g.is_none() {
        *g = Some(SessionTable {
            cells: HashMap::new(),
        });
    }
    g
}

fn session_cell(key: &Path) -> Arc<SessionCell> {
    session_table()
        .as_mut()
        .expect("initialized")
        .cells
        .entry(key.to_path_buf())
        .or_insert_with(|| {
            Arc::new(SessionCell {
                state: Mutex::new(SessionState {
                    inv: SessionInv::Open,
                    inflight_ordinary: 0,
                    quiescing: false,
                    ledger: Default::default(),
                }),
                cv: Condvar::new(),
            })
        })
        .clone()
}

fn session_is_durably_invalidated(key: &Path) -> bool {
    let cell = session_table()
        .as_ref()
        .and_then(|t| t.cells.get(key).cloned());
    let Some(cell) = cell else {
        return false;
    };
    let invalidated = match cell.state.try_lock() {
        Ok(s) => matches!(s.inv, SessionInv::DurablyInvalidated),
        Err(_) => true,
    };
    invalidated
}

fn session_clear_invalidated(key: &Path) {
    let cell = session_table()
        .as_ref()
        .and_then(|t| t.cells.get(key).cloned());
    if let Some(cell) = cell {
        let mut g = cell.state.lock().unwrap_or_else(|p| p.into_inner());
        g.inv = SessionInv::Open;
        // The next mutation bumps a new generation and starts a new ledger session.
        g.ledger.reset();
    }
}

fn session_remove(key: &Path) {
    if let Some(t) = session_table().as_mut() {
        t.cells.remove(key);
    }
}

fn session_clear_all() {
    if let Some(t) = session_table().as_mut() {
        t.cells.clear();
    }
}

/// Stable key so `C:\lib` and a canonicalized child-walked root share one cell / registry slot.
fn session_key(p: &Path) -> PathBuf {
    if let Ok(c) = fs::canonicalize(p) {
        return c;
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                let _ = out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn registry() -> std::sync::MutexGuard<'static, Option<HashMap<PathBuf, PathBuf>>> {
    let mut g = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    if g.is_none() {
        *g = Some(HashMap::new());
    }
    g
}

/// Test observer for page-ledger diagnostic lines.
pub type DiagHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Deterministic failure injection for durability-ordering tests (thread-local).
#[derive(Clone, Default)]
pub struct InvalidateHooks {
    pub fail_gen_create: bool,
    pub fail_gen_fsync: bool,
    pub fail_gen_rename: bool,
    pub fail_gen_dir_sync: bool,
    pub fail_stamp_delete: bool,
    /// After generation is durably published, fail before stamp clear.
    pub crash_after_gen: bool,
    /// After the per-library session lock is held, before durable generation I/O.
    pub stall_invalidate: Option<Arc<dyn Fn() + Send + Sync>>,
    /// At the start of [`note_library_mutation`], before the session lock.
    pub before_session_lock: Option<Arc<dyn Fn() + Send + Sync>>,
    /// After ordinary admission (inflight incremented, session lock released), before return.
    pub after_ordinary_admit: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Page ledger (19B.3C): fail creating the ledger file.
    pub fail_ledger_create: bool,
    /// Page ledger: write only the first N bytes of the append, then fail (torn record).
    pub torn_ledger_append: Option<usize>,
    /// Page ledger: fail the record fsync (bytes written).
    pub fail_ledger_fsync: bool,
    /// Page ledger: fail the directory fsync after creating the file.
    pub fail_ledger_dir_sync: bool,
    /// Page ledger: after a record set is durable, before the admit returns.
    pub after_ledger_durable: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Page ledger: called with each diagnostic line just before it is written.
    pub on_ledger_diag: Option<DiagHook>,
    /// Baseline (19B.3D): write only the first N bytes of the temp file, then fail.
    pub torn_baseline_write: Option<usize>,
    /// Baseline: fail the temp-file fsync.
    pub fail_baseline_fsync: bool,
    /// Baseline: stop before the publishing rename (temp file left behind).
    pub fail_baseline_rename: bool,
    /// Baseline: after the stamp is written, before the consistency check and publication.
    pub before_baseline_publish: Option<Arc<dyn Fn() + Send + Sync>>,
    /// When a tracked [`MutationAdmit`] is dropped (guard-lifetime tests).
    pub on_admit_drop: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[cfg(test)]
pub fn install_invalidate_hooks(h: InvalidateHooks) -> InvalidateHookGuard {
    HOOKS.with(|c| *c.borrow_mut() = h);
    InvalidateHookGuard
}

#[cfg(test)]
pub fn clear_invalidate_hooks() {
    HOOKS.with(|c| *c.borrow_mut() = InvalidateHooks::default());
}

fn hooks() -> InvalidateHooks {
    #[cfg(test)]
    {
        return HOOKS.with(|c| c.borrow().clone());
    }
    #[cfg(not(test))]
    InvalidateHooks::default()
}

#[cfg(test)]
pub(crate) fn test_hooks() -> InvalidateHooks {
    hooks()
}

/// (mutex poisoned, ordinary writes in flight) of a library's session cell, if any.
#[cfg(test)]
pub(crate) fn session_health_for_test(library_root: &Path) -> Option<(bool, u32)> {
    let cell = session_table()
        .as_ref()
        .and_then(|t| t.cells.get(&session_key(library_root)).cloned())?;
    let poisoned = cell.state.is_poisoned();
    let inflight = cell
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .inflight_ordinary;
    Some((poisoned, inflight))
}

/// Clears injected hooks on drop (including panic unwind).
#[cfg(test)]
pub struct InvalidateHookGuard;

#[cfg(test)]
impl Drop for InvalidateHookGuard {
    fn drop(&mut self) {
        clear_invalidate_hooks();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalVerifiedV1 {
    pub v: u32,
    pub library_id: LibraryId,
    pub replica_id: ReplicaId,
    pub server_cursor: u64,
    pub acked_client_seq: u64,
    pub mutation_gen: u64,
    /// SHA-256 (hex) of the canonical library file census (relative path + size).
    pub census_sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FastPathReason {
    Verified,
    Dirty,
    MissingStamp,
    CorruptStamp,
    RecoveryMarker,
    PendingPrep,
    EpochMismatch,
    GenMismatch,
    CensusMismatch,
    FaultDivergence,
}

impl FastPathReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified_fast_path",
            Self::Dirty => "dirty_fallback",
            Self::MissingStamp => "missing_stamp_fallback",
            Self::CorruptStamp => "corrupt_stamp_fallback",
            Self::RecoveryMarker => "recovery_marker_fallback",
            Self::PendingPrep => "pending_prep_fallback",
            Self::EpochMismatch => "epoch_mismatch_fallback",
            Self::GenMismatch => "gen_mismatch_fallback",
            Self::CensusMismatch => "census_mismatch_fallback",
            Self::FaultDivergence => "fault_divergence_fallback",
        }
    }

    pub fn is_fast(self) -> bool {
        matches!(self, Self::Verified)
    }
}

/// Register the live library ↔ sync-dir pair for stamp clear on mutation. Called from
/// `OutboundSync::open` before reconcile.
pub fn register_paths(library_root: &Path, sync_dir: &Path) {
    let key = session_key(library_root);
    registry()
        .as_mut()
        .expect("initialized")
        .insert(key, sync_dir.to_path_buf());
    #[cfg(test)]
    {
        *LAST_REASON.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// Drop one library's registration (sync teardown for that open).
pub fn clear_registry_for(library_root: &Path) {
    if crate::page_ledger::PAGE_LEDGER_ENABLED {
        crate::page_ledger::emit(Some(crate::page_ledger::stats_line()));
    }
    let key = session_key(library_root);
    if let Some(m) = registry().as_mut() {
        m.remove(&key);
    }
    session_remove(&key);
}

/// Drop all registrations (tests / full process teardown).
pub fn clear_registry() {
    if let Some(m) = registry().as_mut() {
        m.clear();
    }
    session_clear_all();
}

fn gen_path(library_root: &Path) -> PathBuf {
    library_root.join(SYNC_DIR).join(MUTATION_GEN_FILE)
}

fn stamp_path(sync_dir: &Path) -> PathBuf {
    sync_dir.join(STAMP_FILE)
}

fn cleared_path(sync_dir: &Path) -> PathBuf {
    sync_dir.join(STAMP_CLEARED_FILE)
}

/// Read the durable mutation generation (0 if absent).
pub fn read_mutation_gen(library_root: &Path) -> Result<u64, String> {
    let path = gen_path(library_root);
    if !path.exists() {
        return Ok(0);
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let s = raw.trim();
    if s.is_empty() {
        return Ok(0);
    }
    s.parse::<u64>()
        .map_err(|e| format!("corrupt {}: {e}", path.display()))
}

fn fsync_dir(dir: &Path, fail: bool) -> Result<(), String> {
    if fail {
        return Err(format!("injected: dir fsync failed ({})", dir.display()));
    }
    #[cfg(unix)]
    {
        let d = File::open(dir).map_err(|e| format!("open dir {}: {e}", dir.display()))?;
        d.sync_all()
            .map_err(|e| format!("fsync dir {}: {e}", dir.display()))?;
    }
    Ok(())
}

fn write_mutation_gen(library_root: &Path, gen: u64) -> Result<(), String> {
    let h = hooks();
    let dir = library_root.join(SYNC_DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let dest = gen_path(library_root);
    let tmp = dir.join(format!("{MUTATION_GEN_FILE}.tmp"));
    if h.fail_gen_create {
        return Err(format!("injected: create {}", tmp.display()));
    }
    {
        let mut f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        write!(f, "{gen}\n").map_err(|e| format!("write {}: {e}", tmp.display()))?;
        if h.fail_gen_fsync {
            return Err(format!("injected: fsync {}", tmp.display()));
        }
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    if h.fail_gen_rename {
        let _ = fs::remove_file(&tmp);
        return Err(format!("injected: rename {}", tmp.display()));
    }
    #[cfg(windows)]
    if dest.exists() {
        fs::remove_file(&dest).map_err(|e| format!("remove {}: {e}", dest.display()))?;
    }
    fs::rename(&tmp, &dest).map_err(|e| format!("rename {}: {e}", dest.display()))?;
    fsync_dir(&dir, h.fail_gen_dir_sync)?;
    Ok(())
}

fn clear_stamp_file(sync_dir: &Path) -> Result<(), String> {
    let h = hooks();
    let path = stamp_path(sync_dir);
    let had_stamp = path.exists();
    if h.fail_stamp_delete && had_stamp {
        return Err(format!("injected: remove {}", path.display()));
    }
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("remove {}: {e}", path.display())),
    }
    fsync_dir(sync_dir, false)?;
    if !had_stamp {
        return Ok(());
    }
    let tomb = cleared_path(sync_dir);
    {
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tomb)
            .map_err(|e| format!("create {}: {e}", tomb.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tomb.display()))?;
    }
    fsync_dir(sync_dir, false)?;
    Ok(())
}

fn skip_census_dir(name: &str) -> bool {
    name == ".page-trash"
        || name == ".notebook-trash"
        || name == ".scribe-thumb"
        || name.starts_with(".scribe-dev-")
}

fn census_filename(name: &str) -> bool {
    CENSUS_FILENAMES.contains(&name)
}

fn census_hash_contents(name: &str) -> bool {
    CENSUS_HASH_CONTENTS.contains(&name)
}

/// Cost of one census walk (host characterization; not Kindle timing).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CensusStats {
    pub files: usize,
    pub hashed_files: usize,
    pub size_only_files: usize,
    pub bytes_hashed: u64,
    pub elapsed_ms: u128,
}

fn file_fingerprint(
    path: &Path,
    name: &str,
    stats: &mut CensusStats,
) -> Result<(u64, String), String> {
    let len = fs::metadata(path)
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    if census_hash_contents(name) {
        let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        stats.hashed_files += 1;
        stats.bytes_hashed += bytes.len() as u64;
        Ok((len, sha256_hex(&bytes).to_string()))
    } else {
        stats.size_only_files += 1;
        Ok((len, "-".into()))
    }
}

fn walk_census(
    root: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, (u64, String)>,
    stats: &mut CensusStats,
) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read {}: {e}", dir.display())),
    };
    for ent in entries {
        let ent = ent.map_err(|e| format!("read {}: {e}", dir.display()))?;
        let path = ent.path();
        let name = ent.file_name();
        let name = name.to_string_lossy();
        let ft = ent
            .file_type()
            .map_err(|e| format!("stat {}: {e}", path.display()))?;
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if skip_census_dir(&name) {
                continue;
            }
            walk_census(root, &path, out, stats)?;
            continue;
        }
        if !ft.is_file() || !census_filename(&name) {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|e| format!("census prefix: {e}"))?
            .to_string_lossy()
            .replace('\\', "/");
        let (len, fp) = file_fingerprint(&path, &name, stats)?;
        stats.files += 1;
        out.insert(rel, (len, fp));
    }
    Ok(())
}

/// Per-file census: relative path → (length, content SHA-256, or `-` for size-only files).
pub type CensusMap = BTreeMap<String, (u64, String)>;

/// The census hash of a [`CensusMap`] (the value the v3 stamp records).
pub fn census_map_sha256(files: &CensusMap) -> String {
    let mut buf = String::new();
    for (rel, (len, fp)) in files {
        buf.push_str(rel);
        buf.push('\t');
        buf.push_str(&len.to_string());
        buf.push('\t');
        buf.push_str(fp);
        buf.push('\n');
    }
    sha256_hex(buf.as_bytes()).to_string()
}

/// Canonical library census as a per-file map (see [`library_census`]).
pub fn library_census_map(library_root: &Path) -> Result<(CensusMap, CensusStats), String> {
    let t0 = Instant::now();
    let mut files = BTreeMap::new();
    let mut stats = CensusStats::default();
    walk_census(library_root, library_root, &mut files, &mut stats)?;
    stats.elapsed_ms = t0.elapsed().as_millis();
    Ok((files, stats))
}

/// Canonical library census (path, size, and content fingerprint of metadata files).
pub fn library_census(library_root: &Path) -> Result<(String, CensusStats), String> {
    let (files, stats) = library_census_map(library_root)?;
    Ok((census_map_sha256(&files), stats))
}

/// [`library_census_sha256`], also reporting the walk's [`CensusStats`] to the startup
/// breakdown (`stamp` = inside the stamp write, else the fast-path check).
fn census_for_startup(library_root: &Path, stamp: bool) -> Result<String, String> {
    census_map_for_startup(library_root, stamp).map(|(_, h)| h)
}

fn census_map_for_startup(library_root: &Path, stamp: bool) -> Result<(CensusMap, String), String> {
    let t0 = Instant::now();
    let (files, stats) = library_census_map(library_root)?;
    let h = census_map_sha256(&files);
    crate::sync_startup_diag::census(stamp, &stats, t0.elapsed());
    Ok((files, h))
}

/// Canonical library census hash.
pub fn library_census_sha256(library_root: &Path) -> Result<String, String> {
    library_census(library_root).map(|(h, _)| h)
}

fn is_library_root(p: &Path) -> bool {
    p.join("notebooks").is_dir() || p.join("library.meta").is_file()
}

/// Walk parents until a directory that *is* a library root is found (has a `notebooks/`
/// child or `library.meta`). This does not assume any ancestor is named `notebooks`.
pub fn library_root_from_tree(path: &Path) -> Option<PathBuf> {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if is_library_root(p) {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

/// Infer `<library>` from a notebook directory (`…/notebooks/<id>`).
pub fn library_root_from_notebook_dir(notebook_dir: &Path) -> Option<PathBuf> {
    library_root_from_tree(notebook_dir)
}

/// Infer `<library>` from a page directory (`…/notebooks/<id>/<page>`).
pub fn library_root_from_page_dir(page_dir: &Path) -> Option<PathBuf> {
    library_root_from_tree(page_dir)
}

/// Infer `<library>` from `.scribe-folders` directory.
pub fn library_root_from_folders_dir(folders_dir: &Path) -> Option<PathBuf> {
    if folders_dir.file_name()?.to_str()? != crate::folders::FOLDERS_DIR {
        return None;
    }
    Some(folders_dir.parent()?.to_path_buf())
}

pub(crate) fn path_looks_like_library_tree(path: &Path) -> bool {
    library_root_from_tree(path).is_some()
        || path.components().any(|c| {
            let s = c.as_os_str();
            s == "notebooks" || s == crate::folders::FOLDERS_DIR
        })
}

/// Run `f` as an authorized synchronization/recovery mutation. Ordinary recovery-maintenance
/// fencing does not apply. Nested calls are counted.
pub(crate) fn permit_sync_mutations<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            SYNC_MUTATION_PERMIT.with(|c| c.set(c.get().saturating_sub(1)));
        }
    }
    SYNC_MUTATION_PERMIT.with(|c| c.set(c.get().saturating_add(1)));
    let _g = Guard;
    f()
}

fn sync_mutation_permitted() -> bool {
    SYNC_MUTATION_PERMIT.with(|c| c.get() > 0)
}

fn maintenance_flag_path(library_root: &Path) -> PathBuf {
    library_root.join(SYNC_DIR).join(RECOVERY_MAINTENANCE_FILE)
}

fn path_present(path: &Path) -> Result<bool, String> {
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!(
            "REFUSE RECOVERY MAINTENANCE: cannot inspect {}: {e}",
            path.display()
        )),
    }
}

fn ordinary_blocked_on_disk(library_root: &Path, key: &Path) -> Result<bool, String> {
    if path_present(&maintenance_flag_path(library_root))? {
        return Ok(true);
    }
    if let Some(sync) = registry().as_ref().and_then(|m| m.get(key).cloned()) {
        if path_present(&sync.join(RECOVERY_INTENT_FILE))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Publish the library-local ordinary-authoring hold. Call after the recovery intent is durable.
pub fn publish_recovery_maintenance(library_root: &Path) -> Result<(), String> {
    let dir = library_root.join(SYNC_DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let dest = maintenance_flag_path(library_root);
    let tmp = dir.join(format!("{RECOVERY_MAINTENANCE_FILE}.tmp"));
    {
        let mut f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(b"1\n")
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    #[cfg(windows)]
    if dest.exists() {
        fs::remove_file(&dest).map_err(|e| format!("remove {}: {e}", dest.display()))?;
    }
    fs::rename(&tmp, &dest).map_err(|e| format!("rename {}: {e}", dest.display()))?;
    fsync_dir(&dir, false)
}

/// Remove the library-local ordinary-authoring hold. Safe if the file is already absent.
pub fn clear_recovery_maintenance(library_root: &Path) -> Result<(), String> {
    let p = maintenance_flag_path(library_root);
    match fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {e}", p.display())),
    }
}

/// Wait until no ordinary mutation is in flight, then block new admissions until the hold
/// is dropped. Does not itself write intent; the caller must publish intent + flag first.
pub fn hold_authoring_for_maintenance(library_root: &Path) -> Result<MaintenanceHold, String> {
    if library_root.as_os_str().is_empty() {
        return Err("REFUSE SYNC STAMP: empty library root".into());
    }
    let key = session_key(library_root);
    let cell = session_cell(&key);
    let mut g = cell.state.lock().unwrap_or_else(|p| p.into_inner());
    while g.inflight_ordinary > 0 {
        g = cell.cv.wait(g).unwrap_or_else(|p| p.into_inner());
    }
    g.quiescing = true;
    drop(g);
    Ok(MaintenanceHold { cell })
}

/// Mark that the canonical library is about to change. Must succeed **before** the associated
/// content fsync. Concurrent callers wait for the first durable outcome; only success
/// authorizes them. After success, later calls skip the generation fsync.
///
/// Keep the returned guard until the content write is durable so recovery prepare cannot
/// freeze a snapshot while that write is still in flight. Ordinary callers are refused while
/// recovery maintenance is active. Authorized sync/recovery callers use
/// [`permit_sync_mutations`].
///
/// On error the content mutation must not proceed. In-memory session state is not the next
/// process's proof: generation / stamp / census on disk are.
///
/// Records [`LedgerScope::All`] in the page ledger; narrower callers use
/// [`note_library_mutation_scoped`] or the `note_mutation_from_*` helpers.
pub fn note_library_mutation(library_root: &Path) -> Result<MutationAdmit, String> {
    note_library_mutation_scoped(library_root, &LedgerScope::All)
}

/// [`note_library_mutation`], also recording `scope` durably in the page ledger before the
/// admit is returned (record-only; see [`crate::page_ledger`]). A ledger failure refuses the
/// mutation exactly like a generation-bump failure.
pub fn note_library_mutation_scoped(
    library_root: &Path,
    scope: &LedgerScope,
) -> Result<MutationAdmit, String> {
    if library_root.as_os_str().is_empty() {
        return Err("REFUSE SYNC STAMP: empty library root".into());
    }
    #[cfg(test)]
    {
        let h = hooks();
        if let Some(f) = h.before_session_lock {
            f();
        }
    }
    let key = session_key(library_root);
    let cell = session_cell(&key);
    let permit = sync_mutation_permitted();
    let mut g = cell.state.lock().unwrap_or_else(|p| p.into_inner());
    while g.quiescing {
        g = cell.cv.wait(g).unwrap_or_else(|p| p.into_inner());
    }
    if !permit && ordinary_blocked_on_disk(library_root, &key)? {
        return Err(REFUSE_RECOVERY_MAINTENANCE.into());
    }
    if !matches!(g.inv, SessionInv::DurablyInvalidated) {
        #[cfg(test)]
        {
            let h = hooks();
            if let Some(f) = h.stall_invalidate {
                f();
            }
        }
        match invalidate_durable(library_root, &key) {
            Ok(gen) => {
                g.inv = SessionInv::DurablyInvalidated;
                g.ledger.begin(gen);
            }
            Err(e) => {
                drop(g);
                crate::page_ledger::emit(Some(format!(
                    "sync_verified_invalidate_failed detail={e}"
                )));
                return Err(e);
            }
        }
    }
    // After the bump, before the content write; under the session lock. Diagnostics are
    // emitted only after the lock is released.
    let mut ledger_diag = None;
    if crate::page_ledger::PAGE_LEDGER_ENABLED {
        let r = g.ledger.record(library_root, scope);
        ledger_diag = g.ledger.take_diag();
        if let Err(e) = r {
            drop(g);
            crate::page_ledger::emit(ledger_diag);
            return Err(e);
        }
    }
    count_admission(&key);
    let ordinary = !permit;
    if ordinary {
        g.inflight_ordinary = g.inflight_ordinary.saturating_add(1);
    }
    drop(g);
    #[cfg(test)]
    if ordinary {
        let h = hooks();
        if let Some(f) = h.after_ordinary_admit {
            f();
        }
    }
    // Built before emitting, so an unwinding emit still releases the in-flight count.
    let admit = MutationAdmit {
        cell: Some(cell),
        ordinary,
    };
    crate::page_ledger::emit(ledger_diag);
    Ok(admit)
}

/// Durable generation bump + stamp clear; returns the generation now on disk.
fn invalidate_durable(library_root: &Path, key: &Path) -> Result<u64, String> {
    let cur = read_mutation_gen(library_root)?;
    let next = cur.saturating_add(1).max(1);
    write_mutation_gen(library_root, next)?;
    if hooks().crash_after_gen {
        return Err("injected: crash after generation update".into());
    }
    let sync_dir = registry().as_ref().and_then(|m| m.get(key).cloned());
    if let Some(sync_dir) = sync_dir {
        clear_stamp_file(&sync_dir)?;
    }
    Ok(next)
}

/// Convenience: note mutation when the library root can be inferred from a page dir.
/// Records the page scope (wider if the path is not `notebooks/<nb>/<page>`).
pub fn note_mutation_from_page_dir(page_dir: &Path) -> Result<MutationAdmit, String> {
    match library_root_from_page_dir(page_dir) {
        Some(root) => {
            note_library_mutation_scoped(&root, &crate::page_ledger::scope_for_dir(&root, page_dir))
        }
        None if path_looks_like_library_tree(page_dir) => Err(format!(
            "REFUSE SYNC STAMP: cannot resolve library root from {}",
            page_dir.display()
        )),
        None => Ok(MutationAdmit::untracked()),
    }
}

/// Records the notebook scope (wider if the path is not `notebooks/<nb>`).
pub fn note_mutation_from_notebook_dir(notebook_dir: &Path) -> Result<MutationAdmit, String> {
    match library_root_from_notebook_dir(notebook_dir) {
        Some(root) => note_library_mutation_scoped(
            &root,
            &crate::page_ledger::scope_for_dir(&root, notebook_dir),
        ),
        None if path_looks_like_library_tree(notebook_dir) => Err(format!(
            "REFUSE SYNC STAMP: cannot resolve library root from {}",
            notebook_dir.display()
        )),
        None => Ok(MutationAdmit::untracked()),
    }
}

/// Records the library-structure scope.
pub fn note_mutation_from_folders_dir(folders_dir: &Path) -> Result<MutationAdmit, String> {
    match library_root_from_folders_dir(folders_dir) {
        Some(root) => note_library_mutation_scoped(&root, &LedgerScope::Structure),
        None if path_looks_like_library_tree(folders_dir) => Err(format!(
            "REFUSE SYNC STAMP: cannot resolve library root from {}",
            folders_dir.display()
        )),
        None => Ok(MutationAdmit::untracked()),
    }
}

/// Evaluate whether full `build_snapshot` may be skipped.
pub fn evaluate_fast_path(
    library_root: &Path,
    sync_dir: &Path,
    library_id: LibraryId,
    replica_id: ReplicaId,
    server_cursor: u64,
    acked_client_seq: u64,
    has_apply_marker: bool,
    has_trailing_prep: bool,
) -> (FastPathReason, u128) {
    let t0 = Instant::now();
    let reason = evaluate_fast_path_inner(
        library_root,
        sync_dir,
        library_id,
        replica_id,
        server_cursor,
        acked_client_seq,
        has_apply_marker,
        has_trailing_prep,
    );
    #[cfg(test)]
    {
        *LAST_REASON.lock().unwrap_or_else(|p| p.into_inner()) = Some(reason);
    }
    (reason, t0.elapsed().as_millis())
}

/// Test-only: reason chosen by the most recent [`evaluate_fast_path`] call.
#[cfg(test)]
pub fn last_fast_path_reason_for_test() -> Option<FastPathReason> {
    *LAST_REASON.lock().unwrap_or_else(|p| p.into_inner())
}

/// Startup reconcile faulted (divergence, corrupt journal, outbox gap). Always a full
/// fallback; recorded so launch telemetry can distinguish it from a clean missing stamp.
pub fn note_reconcile_fault() {
    #[cfg(test)]
    {
        let mut g = LAST_REASON.lock().unwrap_or_else(|p| p.into_inner());
        if g.map(FastPathReason::is_fast).unwrap_or(true) {
            *g = Some(FastPathReason::FaultDivergence);
        }
    }
}

fn evaluate_fast_path_inner(
    library_root: &Path,
    sync_dir: &Path,
    library_id: LibraryId,
    replica_id: ReplicaId,
    server_cursor: u64,
    acked_client_seq: u64,
    has_apply_marker: bool,
    has_trailing_prep: bool,
) -> FastPathReason {
    if has_apply_marker {
        return FastPathReason::RecoveryMarker;
    }
    if has_trailing_prep {
        return FastPathReason::PendingPrep;
    }
    let path = stamp_path(sync_dir);
    if !path.exists() {
        if cleared_path(sync_dir).exists() {
            return FastPathReason::Dirty;
        }
        return FastPathReason::MissingStamp;
    }
    let raw = match fs::read(&path) {
        Ok(b) => b,
        Err(_) => return FastPathReason::CorruptStamp,
    };
    let stamp: LocalVerifiedV1 = match serde_json::from_slice(&raw) {
        Ok(s) => s,
        Err(_) => return FastPathReason::CorruptStamp,
    };
    if stamp.v != STAMP_VERSION {
        return FastPathReason::CorruptStamp;
    }
    if stamp.census_sha256.is_empty() {
        return FastPathReason::CorruptStamp;
    }
    if stamp.library_id != library_id
        || stamp.replica_id != replica_id
        || stamp.server_cursor != server_cursor
        || stamp.acked_client_seq != acked_client_seq
    {
        return FastPathReason::EpochMismatch;
    }
    let gen = match read_mutation_gen(library_root) {
        Ok(g) => g,
        Err(_) => return FastPathReason::CorruptStamp,
    };
    if gen != stamp.mutation_gen {
        return FastPathReason::GenMismatch;
    }
    match census_for_startup(library_root, false) {
        Ok(h) if h == stamp.census_sha256 => {}
        Ok(_) => return FastPathReason::CensusMismatch,
        Err(_) => return FastPathReason::CensusMismatch,
    }
    if session_is_durably_invalidated(&session_key(library_root)) {
        return FastPathReason::Dirty;
    }
    FastPathReason::Verified
}

/// What a successful [`write_stamp_after_full_reconcile`] recorded: the generation and the
/// census it captured (19B.3D: the verified baseline is built from exactly this capture).
#[derive(Debug)]
pub struct StampWritten {
    pub stamp: LocalVerifiedV1,
    pub census: CensusMap,
}

/// Persist a stamp after a successful full reconcile (not after fast path).
pub fn write_stamp_after_full_reconcile(
    library_root: &Path,
    sync_dir: &Path,
    library_id: LibraryId,
    replica_id: ReplicaId,
    server_cursor: u64,
    acked_client_seq: u64,
) -> Result<StampWritten, String> {
    let mutation_gen = read_mutation_gen(library_root)?;
    let (census, census_sha256) = census_map_for_startup(library_root, true)?;
    let stamp = LocalVerifiedV1 {
        v: STAMP_VERSION,
        library_id,
        replica_id,
        server_cursor,
        acked_client_seq,
        mutation_gen,
        census_sha256,
    };
    let mut bytes = serde_json::to_vec_pretty(&stamp).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let dest = stamp_path(sync_dir);
    let tmp = sync_dir.join(format!("{STAMP_FILE}.tmp"));
    {
        let mut f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(&bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    #[cfg(windows)]
    if dest.exists() {
        fs::remove_file(&dest).map_err(|e| format!("remove {}: {e}", dest.display()))?;
    }
    fs::rename(&tmp, &dest).map_err(|e| format!("rename {}: {e}", dest.display()))?;
    let tomb = cleared_path(sync_dir);
    match fs::remove_file(&tomb) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("remove {}: {e}", tomb.display())),
    }
    fsync_dir(sync_dir, false)?;
    session_clear_invalidated(&session_key(library_root));
    Ok(StampWritten { stamp, census })
}

/// Admissions per library root since process start (never reset by `clear_registry_for`).
static ADMISSIONS: Mutex<Option<HashMap<PathBuf, u64>>> = Mutex::new(None);

fn count_admission(key: &Path) {
    let mut g = ADMISSIONS.lock().unwrap_or_else(|p| p.into_inner());
    *g.get_or_insert_with(HashMap::new)
        .entry(key.to_path_buf())
        .or_default() += 1;
}

/// Number of tracked mutations admitted for `library_root` in this process. Equal values
/// before and after a capture prove no mutation was admitted in between (19B.3D baseline).
pub fn admission_count(library_root: &Path) -> u64 {
    let key = session_key(library_root);
    ADMISSIONS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .and_then(|m| m.get(&key).copied())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sync_protocol::{LibraryId, ReplicaId};
    use uuid::Uuid;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("sync-verified-{}", Uuid::now_v7()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn ids() -> (LibraryId, ReplicaId) {
        (
            LibraryId::from_uuid(Uuid::now_v7()).unwrap(),
            ReplicaId::from_uuid(Uuid::now_v7()).unwrap(),
        )
    }

    fn setup() -> (PathBuf, PathBuf, LibraryId, ReplicaId) {
        let lib = tmp();
        let sync = tmp();
        let (lid, rid) = ids();
        register_paths(&lib, &sync);
        (lib, sync, lid, rid)
    }

    fn cleanup(lib: PathBuf, sync: PathBuf) {
        clear_registry_for(&lib);
        clear_invalidate_hooks();
        let _ = fs::remove_dir_all(lib);
        let _ = fs::remove_dir_all(sync);
    }

    #[test]
    fn missing_stamp_is_not_fast() {
        let (lib, sync, lid, rid) = setup();
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert_eq!(r, FastPathReason::MissingStamp);
        cleanup(lib, sync);
    }

    #[test]
    fn clean_stamp_then_mutation_blocks_fast_path() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 10, 5).unwrap();
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 10, 5, false, false);
        assert!(r.is_fast());
        let _ = note_library_mutation(&lib).unwrap();
        let (r2, _) = evaluate_fast_path(&lib, &sync, lid, rid, 10, 5, false, false);
        assert!(
            matches!(
                r2,
                FastPathReason::Dirty | FastPathReason::GenMismatch | FastPathReason::MissingStamp
            ),
            "{r2:?}"
        );
        cleanup(lib, sync);
    }

    #[test]
    fn recovery_markers_disable_fast_path() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 1, 1).unwrap();
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 1, 1, true, false);
        assert_eq!(r, FastPathReason::RecoveryMarker);
        let (r2, _) = evaluate_fast_path(&lib, &sync, lid, rid, 1, 1, false, true);
        assert_eq!(r2, FastPathReason::PendingPrep);
        cleanup(lib, sync);
    }

    #[test]
    fn path_helpers_resolve_library_root() {
        let lib = tmp();
        let nb = lib.join("notebooks").join("notebook-1");
        let page = nb.join("page-1");
        fs::create_dir_all(&page).unwrap();
        assert_eq!(
            library_root_from_page_dir(&page).as_deref(),
            Some(lib.as_path())
        );
        assert_eq!(
            library_root_from_notebook_dir(&nb).as_deref(),
            Some(lib.as_path())
        );
        let folders = lib.join(crate::folders::FOLDERS_DIR);
        fs::create_dir_all(&folders).unwrap();
        assert_eq!(
            library_root_from_folders_dir(&folders).as_deref(),
            Some(lib.as_path())
        );
        let _ = fs::remove_dir_all(lib);
    }

    #[test]
    fn library_root_from_tree_uses_contents_not_ancestor_name() {
        let outer = tmp();
        let decoy = outer.join("notebooks");
        let lib = decoy.join("actual-lib");
        let page = lib.join("notebooks").join("notebook-1").join("page-1");
        fs::create_dir_all(&page).unwrap();
        fs::write(lib.join("library.meta"), b"{}\n").unwrap();
        assert_eq!(
            library_root_from_page_dir(&page).as_deref(),
            Some(lib.as_path())
        );
        // Legacy / alternate layout: page files beside library.meta, no `notebooks` path component.
        let flat = tmp();
        fs::write(flat.join("library.meta"), b"{}\n").unwrap();
        let flat_page = flat.join("page-1");
        fs::create_dir_all(&flat_page).unwrap();
        assert_eq!(
            library_root_from_page_dir(&flat_page).as_deref(),
            Some(flat.as_path())
        );
        let scratch_root = tmp();
        let stray = scratch_root.join("scratch").join("page-1");
        fs::create_dir_all(&stray).unwrap();
        assert_eq!(library_root_from_page_dir(&stray), None);
        assert!(note_mutation_from_page_dir(&stray).is_ok());
        let _ = fs::remove_dir_all(outer);
        let _ = fs::remove_dir_all(flat);
        let _ = fs::remove_dir_all(scratch_root);
    }

    #[test]
    fn v1_stamp_without_census_is_untrusted() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 0, 0).unwrap();
        let mut stamp: serde_json::Value =
            serde_json::from_slice(&fs::read(sync.join(STAMP_FILE)).unwrap()).unwrap();
        stamp.as_object_mut().unwrap().remove("census_sha256");
        stamp["v"] = serde_json::json!(1);
        fs::write(
            sync.join(STAMP_FILE),
            serde_json::to_vec_pretty(&stamp).unwrap(),
        )
        .unwrap();
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert_eq!(r, FastPathReason::CorruptStamp);
        cleanup(lib, sync);
    }

    #[test]
    fn gen_create_failure_leaves_trusted_stamp_untouched_and_blocks_note() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 3, 1).unwrap();
        let _g = install_invalidate_hooks(InvalidateHooks {
            fail_gen_create: true,
            ..Default::default()
        });
        let err = note_library_mutation(&lib).unwrap_err();
        assert!(err.contains("injected: create"), "{err}");
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 3, 1, false, false);
        assert!(
            r.is_fast(),
            "failed-before-mutation must not dirty stamp: {r:?}"
        );
        cleanup(lib, sync);
    }

    #[test]
    fn gen_fsync_failure_does_not_publish_a_new_generation() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 0, 0).unwrap();
        let before = read_mutation_gen(&lib).unwrap();
        let _g = install_invalidate_hooks(InvalidateHooks {
            fail_gen_fsync: true,
            ..Default::default()
        });
        assert!(note_library_mutation(&lib).is_err());
        assert_eq!(read_mutation_gen(&lib).unwrap(), before);
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert!(r.is_fast(), "{r:?}");
        cleanup(lib, sync);
    }

    #[test]
    fn gen_rename_failure_does_not_leave_a_falsely_clean_published_gen() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 0, 0).unwrap();
        let before = read_mutation_gen(&lib).unwrap();
        let _g = install_invalidate_hooks(InvalidateHooks {
            fail_gen_rename: true,
            ..Default::default()
        });
        assert!(note_library_mutation(&lib).is_err());
        assert_eq!(read_mutation_gen(&lib).unwrap(), before);
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert!(r.is_fast(), "{r:?}");
        cleanup(lib, sync);
    }

    #[test]
    fn gen_dir_sync_failure_is_not_verified() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 0, 0).unwrap();
        let _g = install_invalidate_hooks(InvalidateHooks {
            fail_gen_dir_sync: true,
            ..Default::default()
        });
        assert!(note_library_mutation(&lib).is_err());
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert!(
            !r.is_fast(),
            "partial gen publish must not stay verified: {r:?}"
        );
        cleanup(lib, sync);
    }

    #[test]
    fn stamp_delete_failure_after_gen_update_is_not_verified() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 2, 2).unwrap();
        let _g = install_invalidate_hooks(InvalidateHooks {
            fail_stamp_delete: true,
            ..Default::default()
        });
        assert!(note_library_mutation(&lib).is_err());
        assert!(sync.join(STAMP_FILE).exists());
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 2, 2, false, false);
        assert_eq!(r, FastPathReason::GenMismatch);
        cleanup(lib, sync);
    }

    #[test]
    fn crash_between_gen_update_and_stamp_clear_is_not_verified() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 4, 0).unwrap();
        let _g = install_invalidate_hooks(InvalidateHooks {
            crash_after_gen: true,
            ..Default::default()
        });
        assert!(note_library_mutation(&lib).is_err());
        assert!(sync.join(STAMP_FILE).exists());
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 4, 0, false, false);
        assert_eq!(r, FastPathReason::GenMismatch);
        cleanup(lib, sync);
    }

    #[test]
    fn subsequent_note_after_failure_can_invalidate() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 0, 0).unwrap();
        {
            let _g = install_invalidate_hooks(InvalidateHooks {
                fail_gen_create: true,
                ..Default::default()
            });
            assert!(note_library_mutation(&lib).is_err());
        }
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert!(r.is_fast());
        let _ = note_library_mutation(&lib).unwrap();
        let (r2, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert!(!r2.is_fast(), "{r2:?}");
        cleanup(lib, sync);
    }

    #[test]
    fn legacy_writer_without_gen_bump_trips_census() {
        let (lib, sync, lid, rid) = setup();
        let page = lib.join("notebooks").join("notebook-1").join("page-1");
        fs::create_dir_all(&page).unwrap();
        fs::write(page.join("strokes.jsonl"), b"{}\n").unwrap();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 0, 0).unwrap();
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert!(r.is_fast(), "{r:?}");
        // Simulate a pre-18C writer: append canonical bytes, do not touch mutation-gen.
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(page.join("strokes.jsonl"))
                .unwrap();
            f.write_all(b"{legacy}\n").unwrap();
        }
        let (r2, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert_eq!(r2, FastPathReason::CensusMismatch);
        cleanup(lib, sync);
    }

    #[test]
    fn v2_stamp_is_untrusted_under_v3() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 0, 0).unwrap();
        let mut stamp: serde_json::Value =
            serde_json::from_slice(&fs::read(sync.join(STAMP_FILE)).unwrap()).unwrap();
        stamp["v"] = serde_json::json!(2);
        fs::write(
            sync.join(STAMP_FILE),
            serde_json::to_vec_pretty(&stamp).unwrap(),
        )
        .unwrap();
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert_eq!(r, FastPathReason::CorruptStamp);
        cleanup(lib, sync);
    }

    #[test]
    fn same_size_metadata_rewrite_trips_census() {
        let (lib, sync, lid, rid) = setup();
        let info = lib
            .join("notebooks")
            .join("notebook-1")
            .join("notebook.info");
        fs::create_dir_all(info.parent().unwrap()).unwrap();
        let a = b"{\"v\":1,\"title\":\"AAAAAAAA\"}\n";
        let b = b"{\"v\":1,\"title\":\"BBBBBBBB\"}\n";
        assert_eq!(a.len(), b.len());
        fs::write(&info, a).unwrap();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 0, 0).unwrap();
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert!(r.is_fast(), "{r:?}");
        // Path+size census (v2) would miss this; content fingerprint must not.
        fs::write(&info, b).unwrap();
        let (r2, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert_eq!(r2, FastPathReason::CensusMismatch);
        cleanup(lib, sync);
    }

    #[test]
    fn same_size_atomic_replace_trips_census() {
        let (lib, sync, lid, rid) = setup();
        let info = lib
            .join("notebooks")
            .join("notebook-1")
            .join("notebook.info");
        fs::create_dir_all(info.parent().unwrap()).unwrap();
        let a = b"{\"v\":1,\"title\":\"CCCCCCCC\"}\n";
        let b = b"{\"v\":1,\"title\":\"DDDDDDDD\"}\n";
        assert_eq!(a.len(), b.len());
        fs::write(&info, a).unwrap();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 0, 0).unwrap();
        let tmp = info.with_extension("info.tmp");
        fs::write(&tmp, b).unwrap();
        #[cfg(windows)]
        let _ = fs::remove_file(&info);
        fs::rename(&tmp, &info).unwrap();
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert_eq!(r, FastPathReason::CensusMismatch);
        cleanup(lib, sync);
    }

    #[test]
    fn concurrent_notes_wait_for_durable_success() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 1, 0).unwrap();
        let holding = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let b_at_gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let a_done = Arc::new(std::sync::Mutex::new(None::<Result<(), String>>));
        let b_done = Arc::new((
            std::sync::Mutex::new(None::<Result<(), String>>),
            std::sync::Condvar::new(),
        ));

        let holding_a = Arc::clone(&holding);
        let release_a = Arc::clone(&release);
        let a_done_t = Arc::clone(&a_done);
        let lib_a = lib.clone();
        let a = std::thread::spawn(move || {
            let _h = install_invalidate_hooks(InvalidateHooks {
                stall_invalidate: Some(Arc::new(move || {
                    *holding_a.0.lock().unwrap() = true;
                    holding_a.1.notify_all();
                    let mut g = release_a.0.lock().unwrap();
                    while !*g {
                        g = release_a.1.wait(g).unwrap();
                    }
                })),
                ..Default::default()
            });
            let r = note_library_mutation(&lib_a).map(|_| ());
            *a_done_t.lock().unwrap() = Some(r);
        });

        {
            let mut g = holding.0.lock().unwrap();
            while !*g {
                g = holding.1.wait(g).unwrap();
            }
        }

        let b_at_gate_t = Arc::clone(&b_at_gate);
        let b_done_t = Arc::clone(&b_done);
        let lib_b = lib.clone();
        let b = std::thread::spawn(move || {
            let _h = install_invalidate_hooks(InvalidateHooks {
                before_session_lock: Some(Arc::new(move || {
                    *b_at_gate_t.0.lock().unwrap() = true;
                    b_at_gate_t.1.notify_all();
                })),
                ..Default::default()
            });
            let r = note_library_mutation(&lib_b).map(|_| ());
            *b_done_t.0.lock().unwrap() = Some(r);
            b_done_t.1.notify_all();
        });

        {
            let mut g = b_at_gate.0.lock().unwrap();
            while !*g {
                g = b_at_gate.1.wait(g).unwrap();
            }
        }
        assert!(
            b_done.0.lock().unwrap().is_none(),
            "B must not finish while A holds durable invalidation"
        );
        assert!(a_done.lock().unwrap().is_none(), "A still stalled");

        *release.0.lock().unwrap() = true;
        release.1.notify_all();
        a.join().unwrap();
        b.join().unwrap();
        a_done.lock().unwrap().as_ref().unwrap().as_ref().unwrap();
        b_done.0.lock().unwrap().as_ref().unwrap().as_ref().unwrap();
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 1, 0, false, false);
        assert!(!r.is_fast(), "{r:?}");
        cleanup(lib, sync);
    }

    #[test]
    fn concurrent_failed_invalidation_does_not_authorize_writer() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 0, 0).unwrap();
        let holding = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let b_at_gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let a_done = Arc::new(std::sync::Mutex::new(None::<Result<(), String>>));
        let b_done = Arc::new((
            std::sync::Mutex::new(None::<Result<(), String>>),
            std::sync::Condvar::new(),
        ));

        let holding_a = Arc::clone(&holding);
        let release_a = Arc::clone(&release);
        let a_done_t = Arc::clone(&a_done);
        let lib_a = lib.clone();
        let a = std::thread::spawn(move || {
            let _h = install_invalidate_hooks(InvalidateHooks {
                fail_gen_create: true,
                stall_invalidate: Some(Arc::new(move || {
                    *holding_a.0.lock().unwrap() = true;
                    holding_a.1.notify_all();
                    let mut g = release_a.0.lock().unwrap();
                    while !*g {
                        g = release_a.1.wait(g).unwrap();
                    }
                })),
                ..Default::default()
            });
            let r = note_library_mutation(&lib_a).map(|_| ());
            *a_done_t.lock().unwrap() = Some(r);
        });

        {
            let mut g = holding.0.lock().unwrap();
            while !*g {
                g = holding.1.wait(g).unwrap();
            }
        }

        let b_at_gate_t = Arc::clone(&b_at_gate);
        let b_done_t = Arc::clone(&b_done);
        let lib_b = lib.clone();
        let b = std::thread::spawn(move || {
            let _h = install_invalidate_hooks(InvalidateHooks {
                fail_gen_create: true,
                before_session_lock: Some(Arc::new(move || {
                    *b_at_gate_t.0.lock().unwrap() = true;
                    b_at_gate_t.1.notify_all();
                })),
                ..Default::default()
            });
            let r = note_library_mutation(&lib_b).map(|_| ());
            *b_done_t.0.lock().unwrap() = Some(r);
            b_done_t.1.notify_all();
        });

        {
            let mut g = b_at_gate.0.lock().unwrap();
            while !*g {
                g = b_at_gate.1.wait(g).unwrap();
            }
        }
        assert!(b_done.0.lock().unwrap().is_none());

        *release.0.lock().unwrap() = true;
        release.1.notify_all();
        a.join().unwrap();
        b.join().unwrap();
        assert!(a_done.lock().unwrap().as_ref().unwrap().is_err());
        assert!(b_done.0.lock().unwrap().as_ref().unwrap().is_err());
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert!(
            r.is_fast(),
            "failed invalidation must leave stamp trusted: {r:?}"
        );
        let _ = note_library_mutation(&lib).unwrap();
        let (r2, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert!(!r2.is_fast(), "{r2:?}");
        cleanup(lib, sync);
    }

    #[test]
    fn later_notes_skip_generation_fsync() {
        let (lib, sync, lid, rid) = setup();
        write_stamp_after_full_reconcile(&lib, &sync, lid, rid, 0, 0).unwrap();
        let stalls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stalls_h = Arc::clone(&stalls);
        let _h = install_invalidate_hooks(InvalidateHooks {
            stall_invalidate: Some(Arc::new(move || {
                stalls_h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })),
            ..Default::default()
        });
        let _ = note_library_mutation(&lib).unwrap();
        let gen1 = read_mutation_gen(&lib).unwrap();
        let _ = note_library_mutation(&lib).unwrap();
        let _ = note_library_mutation(&lib).unwrap();
        assert_eq!(read_mutation_gen(&lib).unwrap(), gen1);
        assert_eq!(stalls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let (r, _) = evaluate_fast_path(&lib, &sync, lid, rid, 0, 0, false, false);
        assert!(!r.is_fast(), "{r:?}");
        cleanup(lib, sync);
    }

    #[test]
    fn independent_libraries_do_not_share_invalidation_lock() {
        let (lib1, sync1, lid1, rid1) = setup();
        let (lib2, sync2, lid2, rid2) = setup();
        write_stamp_after_full_reconcile(&lib1, &sync1, lid1, rid1, 0, 0).unwrap();
        write_stamp_after_full_reconcile(&lib2, &sync2, lid2, rid2, 0, 0).unwrap();
        let bar = Arc::new(std::sync::Barrier::new(3));
        let spawn = |lib: PathBuf, bar: Arc<std::sync::Barrier>| {
            std::thread::spawn(move || {
                let bar_h = Arc::clone(&bar);
                let _h = install_invalidate_hooks(InvalidateHooks {
                    stall_invalidate: Some(Arc::new(move || {
                        bar_h.wait();
                    })),
                    ..Default::default()
                });
                note_library_mutation(&lib)
            })
        };
        let t1 = spawn(lib1.clone(), Arc::clone(&bar));
        let t2 = spawn(lib2.clone(), Arc::clone(&bar));
        bar.wait();
        let _ = t1.join().unwrap().unwrap();
        let _ = t2.join().unwrap().unwrap();
        let (r1, _) = evaluate_fast_path(&lib1, &sync1, lid1, rid1, 0, 0, false, false);
        let (r2, _) = evaluate_fast_path(&lib2, &sync2, lid2, rid2, 0, 0, false, false);
        assert!(!r1.is_fast() && !r2.is_fast(), "{r1:?} {r2:?}");
        cleanup(lib1, sync1);
        cleanup(lib2, sync2);
    }

    #[test]
    fn census_characterizes_synthetic_library_cost() {
        let lib = tmp();
        fs::write(lib.join("library.meta"), b"{\"v\":1}\n").unwrap();
        for n in 1..=8 {
            let nb = lib.join("notebooks").join(format!("notebook-{n}"));
            fs::create_dir_all(&nb).unwrap();
            fs::write(nb.join("notebook.meta"), b"{\"v\":1,\"page_ids\":[]}\n").unwrap();
            fs::write(
                nb.join("notebook.info"),
                format!("{{\"v\":1,\"title\":\"Notebook {n}\"}}\n").as_bytes(),
            )
            .unwrap();
            for p in 1..=6 {
                let page = nb.join(format!("page-{p}"));
                fs::create_dir_all(&page).unwrap();
                fs::write(page.join("page.meta"), b"{\"v\":5}\n").unwrap();
                fs::write(page.join("strokes.jsonl"), b"{}\n").unwrap();
                fs::write(page.join("edits.jsonl"), b"").unwrap();
            }
        }
        let (hash, stats) = library_census(&lib).unwrap();
        assert!(!hash.is_empty());
        // 1 library.meta + 8*(meta+info) + 48 page.meta hashed; 48 strokes + 48 edits size-only.
        assert_eq!(stats.files, 1 + 16 + 48 + 48 + 48, "{stats:?}");
        assert_eq!(stats.hashed_files, 1 + 16 + 48, "{stats:?}");
        assert_eq!(stats.size_only_files, 96, "{stats:?}");
        assert!(stats.bytes_hashed > 0);
        eprintln!(
            "census_host files={} hashed={} size_only={} bytes_hashed={} ms={}",
            stats.files,
            stats.hashed_files,
            stats.size_only_files,
            stats.bytes_hashed,
            stats.elapsed_ms
        );
        let _ = fs::remove_dir_all(lib);
    }
}
