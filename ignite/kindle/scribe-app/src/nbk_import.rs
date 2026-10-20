//! One-way import of Amazon Kindle notebooks (NBK) into native Scribe notebooks.
//!
//! **Not synchronization.** Each Amazon notebook is copied at most once; afterwards the copy is
//! an ordinary native notebook (normal persistence and Scribe sync). Later edits to the Amazon
//! source are never re-imported. Amazon state is strictly read-only: sources are read through
//! `nbk-reader` (in-process parsing of in-memory copies) and nothing under the source root or
//! the Kindle catalog is ever opened for writing.
//!
//! Identity: the canonical Amazon notebook UUID — `Nodes.uuid` of a `STANDALONE_NOTEBOOK` in
//! the Kindle catalog, which equals the `/mnt/us/.notebooks/<uuid>/` directory name (FW 5.18.6
//! evidence). Never the title, content, size or mtime.
//!
//! Receipts (`<library>/.scribe-import/receipts.json`, keyed `amazon_nbk:<uuid>`) are the
//! import ledger. They outlive the destination notebook: deleting an imported notebook never
//! makes its source eligible again. Each destination notebook also carries an immutable
//! `import-origin.json` provenance marker.
//!
//! Crash safety (pending → complete state machine):
//! 1. decode fully in memory (nothing written yet);
//! 2. receipt `pending` with the destination id is made durable **before** creating it;
//! 3. create the empty notebook, then empty pages (sync creates are flushed durable before
//!    anything is built on them), then append strokes in bounded batches, each one's captures
//!    durable in the sync journal before the next batch is written (backpressure: the import
//!    never outruns the journal);
//! 4. receipt `complete` — only once every capture of the notebook is durable.
//!
//! If sync faults (or stops) mid-import the whole run halts: the partial destination is kept
//! (deleting it without a journaled delete would diverge the library from the sync journal),
//! its receipt becomes `failed` (retryable; a retry removes it through the synced delete path),
//! and no later notebook is started. Startup recovery re-derives the partial's missing history.
//!
//! A `pending` receipt found later (crash) has its partial destination removed through the
//! normal (synced) delete path — at startup before any UI, and again before a retry — then
//! becomes `failed` (retryable). A source never has more than one completed native notebook.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};

use kindle_platform::FbGeometry;
use nbk_reader::catalog::{read_catalog, STANDALONE_NOTEBOOK};
use nbk_reader::{decode_snapshot, read_source, DecodedNotebook, DecodedStroke};
use notebook_core::{new_stroke_id, Point, Stroke};
use serde::{Deserialize, Serialize};

use crate::library::{
    allocate_fresh_notebook_id, create_notebook_at_durable, delete_notebook_durable,
    open_library_read_only, quarantine_notebook_dir, read_notebook_manifest_read_only,
    DeleteNotebookHooks, LibraryRepository, MAX_TITLE_CHARS,
};
use crate::notebook::{insert_page_after_durable, InsertPageHooks};
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use crate::profile::RenderProfile;
use crate::store::load_existing_page;
use crate::sync_outbound::DurableWaitError;

pub const ORIGIN_KIND: &str = "amazon_nbk";
pub const IMPORT_DIR: &str = ".scribe-import";
pub const RECEIPTS_FILE: &str = "receipts.json";
pub const ORIGIN_MARKER: &str = "import-origin.json";
/// FW 5.18.6: Amazon notebook storage and the notebook catalog (read-only).
pub const KINDLE_NOTEBOOKS_ROOT: &str = "/mnt/us/.notebooks";
pub const KINDLE_CATALOG: &str = "/var/local/ksdk.content.db";
pub const FALLBACK_TITLE: &str = "Kindle notebook";
/// Amazon notebook canvas == Scribe canonical page (coordinates map 1:1).
const CANVAS: (i128, i128) = (15624, 20832);
/// Raw `nmdl.pressure` range, identical to the Scribe digitizer's ABS_PRESSURE range.
const RAW_PRESSURE_MAX: f32 = 4095.0;
const STROKES_PER_FSYNC: usize = 256;

/// Test-only fault points (crash = stop without any cleanup, as if the process died).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    CrashAfterPending,
    CrashAfterFirstPage,
    FailAfterFirstPage,
}

#[cfg(test)]
thread_local! {
    pub(crate) static FAULT: std::cell::Cell<Option<Fault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn fault(point: Fault) -> bool {
    FAULT.with(|f| f.get() == Some(point))
}

const SIMULATED_CRASH: &str = "SIMULATED CRASH";
/// Error prefixes of a halted import (sync stopped / wait cancelled).
const SYNC_HALT: &str = "sync halted";
const CANCELLED: &str = "import cancelled";

fn halt(e: DurableWaitError, what: &str) -> String {
    match e {
        DurableWaitError::Cancelled => format!("{CANCELLED} (waiting for {what})"),
        e => format!("{SYNC_HALT} ({what}): {e}"),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

// ------------------------------------------------------------------------------ sources

#[derive(Clone, Debug)]
pub struct ImportSources {
    pub notebooks_root: PathBuf,
    /// Kindle notebook catalog; `None` (or absent file) → every UUID-named directory is a
    /// candidate and titles fall back to [`FALLBACK_TITLE`].
    pub catalog: Option<PathBuf>,
}

impl ImportSources {
    pub fn kindle() -> Self {
        Self {
            notebooks_root: PathBuf::from(KINDLE_NOTEBOOKS_ROOT),
            catalog: Some(PathBuf::from(KINDLE_CATALOG)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceNotebook {
    /// Stable Amazon notebook UUID (import identity).
    pub origin_id: String,
    /// Display only (never identity).
    pub title: String,
    pub dir: PathBuf,
}

fn is_uuid_like(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// A catalog title made valid for a native notebook (never used as identity).
pub fn native_title(raw: Option<&str>) -> String {
    let cleaned: String = raw
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let t: String = cleaned.trim().chars().take(MAX_TITLE_CHARS).collect();
    let t = t.trim().to_string();
    if t.is_empty() {
        FALLBACK_TITLE.to_string()
    } else {
        t
    }
}

/// Standalone Amazon notebooks available locally (catalog order independent; sorted by title).
pub fn discover(src: &ImportSources) -> Result<Vec<SourceNotebook>, String> {
    let has_nbk = |id: &str| src.notebooks_root.join(id).join("nbk").is_file();
    let mut out = Vec::new();
    match src.catalog.as_ref().filter(|p| p.is_file()) {
        Some(catalog) => {
            let bytes = fs::read(catalog).map_err(|e| format!("read Kindle catalog: {e}"))?;
            let entries = read_catalog(&bytes).map_err(|e| format!("Kindle catalog: {e}"))?;
            for e in entries {
                if e.node_type.as_deref() == Some(STANDALONE_NOTEBOOK)
                    && !e.deleted
                    && is_uuid_like(&e.uuid)
                    && has_nbk(&e.uuid)
                {
                    out.push(SourceNotebook {
                        title: native_title(e.title.as_deref()),
                        dir: src.notebooks_root.join(&e.uuid),
                        origin_id: e.uuid,
                    });
                }
            }
        }
        None => {
            let rd = match fs::read_dir(&src.notebooks_root) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(format!("read {}: {e}", src.notebooks_root.display())),
            };
            for entry in rd.flatten() {
                let id = entry.file_name().to_string_lossy().into_owned();
                if is_uuid_like(&id) && has_nbk(&id) {
                    out.push(SourceNotebook {
                        title: FALLBACK_TITLE.to_string(),
                        dir: entry.path(),
                        origin_id: id,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| {
        (a.title.to_lowercase(), &a.origin_id).cmp(&(b.title.to_lowercase(), &b.origin_id))
    });
    Ok(out)
}

// ------------------------------------------------------------------------------ receipts

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptState {
    Pending,
    Complete,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportReceiptV1 {
    pub origin_kind: String,
    pub origin_id: String,
    pub state: ReceiptState,
    /// Native destination (allocated before creation; kept after completion for provenance).
    pub dest_notebook_id: Option<String>,
    pub title: String,
    /// Amazon internal `book_id` (secondary provenance, never identity).
    pub book_id: Option<String>,
    pub started_unix_ms: u64,
    pub finished_unix_ms: Option<u64>,
    pub pages: Option<usize>,
    pub strokes: Option<usize>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptsV1 {
    pub v: u32,
    /// Key: `amazon_nbk:<origin_id>` — one receipt per origin identity (uniqueness).
    pub receipts: BTreeMap<String, ImportReceiptV1>,
}

/// Immutable provenance written into each imported notebook directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportOriginV1 {
    pub v: u32,
    pub origin_kind: String,
    pub origin_id: String,
    pub imported_unix_ms: u64,
}

pub fn receipt_key(origin_id: &str) -> String {
    format!("{ORIGIN_KIND}:{origin_id}")
}

/// The import ledger. A corrupt ledger fails closed (idempotency cannot be proven).
pub struct Ledger {
    path: PathBuf,
    pub data: ReceiptsV1,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent")?;
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

impl Ledger {
    pub fn path_for(library_root: &Path) -> PathBuf {
        library_root.join(IMPORT_DIR).join(RECEIPTS_FILE)
    }

    pub fn open(library_root: &Path) -> Result<Self, String> {
        let path = Self::path_for(library_root);
        let data = match fs::read(&path) {
            Ok(b) => {
                let d: ReceiptsV1 = serde_json::from_slice(&b)
                    .map_err(|e| format!("REFUSE IMPORT: unreadable import ledger: {e}"))?;
                if d.v != 1 {
                    return Err(format!("REFUSE IMPORT: import ledger version {}", d.v));
                }
                d
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ReceiptsV1 {
                v: 1,
                receipts: BTreeMap::new(),
            },
            Err(e) => return Err(format!("REFUSE IMPORT: read import ledger: {e}")),
        };
        Ok(Self { path, data })
    }

    pub fn get(&self, origin_id: &str) -> Option<&ImportReceiptV1> {
        self.data.receipts.get(&receipt_key(origin_id))
    }

    fn put(&mut self, r: ImportReceiptV1) -> Result<(), String> {
        let key = receipt_key(&r.origin_id);
        let prev = self.data.receipts.insert(key.clone(), r);
        let bytes = serde_json::to_vec_pretty(&self.data).map_err(|e| e.to_string())?;
        if let Err(e) = write_atomic(&self.path, &bytes) {
            // Keep memory == disk.
            match prev {
                Some(p) => self.data.receipts.insert(key, p),
                None => self.data.receipts.remove(&key),
            };
            return Err(format!("import ledger write: {e}"));
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------ scan

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceStatus {
    /// Never imported.
    Eligible,
    /// A previous attempt failed or was interrupted (retryable).
    Retry,
    /// Completed once; never imported again (even if the copy was deleted).
    AlreadyImported { dest_notebook_id: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanEntry {
    pub source: SourceNotebook,
    pub status: SourceStatus,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub entries: Vec<ScanEntry>,
}

impl ScanReport {
    pub fn total(&self) -> usize {
        self.entries.len()
    }

    pub fn eligible(&self) -> Vec<&SourceNotebook> {
        self.entries
            .iter()
            .filter(|e| !matches!(e.status, SourceStatus::AlreadyImported { .. }))
            .map(|e| &e.source)
            .collect()
    }

    pub fn already_imported(&self) -> usize {
        self.entries.len() - self.eligible().len()
    }
}

pub fn classify(ledger: &Ledger, origin_id: &str) -> SourceStatus {
    match ledger.get(origin_id) {
        None => SourceStatus::Eligible,
        Some(r) if r.state == ReceiptState::Complete => SourceStatus::AlreadyImported {
            dest_notebook_id: r.dest_notebook_id.clone(),
        },
        Some(_) => SourceStatus::Retry,
    }
}

/// Discover sources and classify them against the ledger (no decoding, no writes).
pub fn scan(src: &ImportSources, library_root: &Path) -> Result<ScanReport, String> {
    let ledger = Ledger::open(library_root)?;
    Ok(ScanReport {
        entries: discover(src)?
            .into_iter()
            .map(|s| ScanEntry {
                status: classify(&ledger, &s.origin_id),
                source: s,
            })
            .collect(),
    })
}

// ------------------------------------------------------------------------------ sync hooks

/// How an import reports its native mutations to Scribe sync (same captures as the runtime's
/// own create/insert/write paths).
pub trait ImportSync {
    fn notebook_created(&self, library: &LibraryRepository, notebook_id: &str, title: &str);
    fn page_created(&self, notebook_dir: &Path, notebook_id: &str, page_id: &str, after: &str);
    fn strokes_committed(&self, notebook_id: &str, page_id: &str, strokes: &[Stroke]);
    /// Every capture reported so far is durable in the sync journal (bulk backpressure: waits
    /// out a backlog, no fixed timeout). `Err` halts the import — a local journal write
    /// failure, unknown sequence authority, stopped journal, or cancelled wait is never
    /// success. Server unavailability and snapshot divergence are not this error.
    fn flush(&self, cancel: &AtomicBool) -> Result<(), DurableWaitError>;
    /// Remove a partial destination through the normal delete path.
    fn delete_notebook(
        &self,
        library: &mut LibraryRepository,
        notebook_id: &str,
    ) -> Result<(), String>;
}

/// Sync off: local persistence only.
pub struct NoSync;

impl ImportSync for NoSync {
    fn notebook_created(&self, _: &LibraryRepository, _: &str, _: &str) {}
    fn page_created(&self, _: &Path, _: &str, _: &str, _: &str) {}
    fn strokes_committed(&self, _: &str, _: &str, _: &[Stroke]) {}
    fn flush(&self, _: &AtomicBool) -> Result<(), DurableWaitError> {
        Ok(())
    }
    fn delete_notebook(&self, library: &mut LibraryRepository, id: &str) -> Result<(), String> {
        delete_notebook_durable(library, id, &DeleteNotebookHooks::default()).map(|_| ())
    }
}

fn after_notebook<'a>(library: &'a LibraryRepository, notebook_id: &str) -> Option<&'a str> {
    let ids = &library.manifest().notebook_ids;
    ids.iter()
        .position(|n| n == notebook_id)
        .and_then(|i| i.checked_sub(1))
        .map(|i| ids[i].as_str())
}

impl ImportSync for crate::sync_outbound::OutboundSync {
    fn notebook_created(&self, library: &LibraryRepository, id: &str, title: &str) {
        crate::sync_outbound::OutboundSync::notebook_created(
            self,
            &library.notebook_dir(id),
            id,
            title,
            after_notebook(library, id),
        );
    }
    fn page_created(&self, dir: &Path, nb: &str, page: &str, after: &str) {
        crate::sync_outbound::OutboundSync::page_created(self, dir, nb, page, Some(after));
    }
    fn strokes_committed(&self, nb: &str, page: &str, strokes: &[Stroke]) {
        for s in strokes {
            self.stroke_committed(nb, page, s);
        }
    }
    fn flush(&self, cancel: &AtomicBool) -> Result<(), DurableWaitError> {
        self.wait_journal_durable(cancel)
    }
    /// Synced delete. With sync faulted the notebook is kept: deleting it locally without a
    /// journaled `DeleteNotebook` would leave the library diverged from the sync journal.
    fn delete_notebook(&self, library: &mut LibraryRepository, id: &str) -> Result<(), String> {
        if let Some(f) = self.fault() {
            return Err(format!("sync faulted ({f}); notebook kept"));
        }
        let Some(ticket) = self.prepare_delete_notebook(id) else {
            return Err("sync could not journal the delete; notebook kept".into());
        };
        let r = delete_notebook_durable(library, id, &DeleteNotebookHooks::default());
        if r.is_ok() {
            self.commit(ticket)
        } else {
            self.abort(ticket)
        }
        r.map(|_| ())
    }
}

/// The runtime's installed sync (no-op when sync is off).
pub struct GlobalSync;

impl ImportSync for GlobalSync {
    fn notebook_created(&self, library: &LibraryRepository, id: &str, title: &str) {
        crate::sync_outbound::with_global(|s| ImportSync::notebook_created(s, library, id, title));
    }
    fn page_created(&self, dir: &Path, nb: &str, page: &str, after: &str) {
        crate::sync_outbound::with_global(|s| ImportSync::page_created(s, dir, nb, page, after));
    }
    fn strokes_committed(&self, nb: &str, page: &str, strokes: &[Stroke]) {
        crate::sync_outbound::with_global(|s| ImportSync::strokes_committed(s, nb, page, strokes));
    }
    fn flush(&self, cancel: &AtomicBool) -> Result<(), DurableWaitError> {
        crate::sync_outbound::with_global(|s| ImportSync::flush(s, cancel)).unwrap_or(Ok(()))
    }
    fn delete_notebook(&self, library: &mut LibraryRepository, id: &str) -> Result<(), String> {
        crate::sync_outbound::with_global(|s| ImportSync::delete_notebook(s, library, id))
            .unwrap_or_else(|| NoSync.delete_notebook(library, id))
    }
}

// ------------------------------------------------------------------------------ import

/// Where imported notebooks are created (same inputs as a native "+ NEW").
pub struct ImportTarget<'a> {
    pub library: &'a mut LibraryRepository,
    pub profile: &'a RenderProfile,
    pub geo: &'a FbGeometry,
    pub paper: &'a [u8],
    /// Cancels a bulk durability wait (app shutdown).
    pub cancel: &'a AtomicBool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportOutcome {
    Imported {
        notebook_id: String,
        pages: usize,
        strokes: usize,
        /// Amazon strokes with no decodable points (nothing to draw).
        skipped_empty_strokes: usize,
    },
    AlreadyImported,
    Failed(String),
    /// Sync faulted/stopped or the wait was cancelled: this notebook did not import **and the
    /// run must stop** (no later notebook may be created without durable sync history).
    Halted(String),
}

/// Native strokes per page (canvas checked; empty strokes dropped).
fn convert(nb: &DecodedNotebook) -> Result<(Vec<Vec<Stroke>>, usize), String> {
    let mut pages = Vec::with_capacity(nb.pages.len());
    let mut skipped = 0;
    for p in &nb.pages {
        if (p.width, p.height) != CANVAS {
            return Err(format!(
                "NBK_UNSUPPORTED: page {} canvas {}x{} (only {}x{} is supported)",
                p.page_id, p.width, p.height, CANVAS.0, CANVAS.1
            ));
        }
        let mut strokes = Vec::with_capacity(p.strokes.len());
        for s in &p.strokes {
            match native_stroke(s) {
                Some(n) => strokes.push(n),
                None => skipped += 1,
            }
        }
        pages.push(strokes);
    }
    Ok((pages, skipped))
}

fn native_stroke(s: &DecodedStroke) -> Option<Stroke> {
    if s.xs.is_empty() || s.xs.len() != s.ys.len() {
        return None;
    }
    let points =
        s.xs.iter()
            .zip(&s.ys)
            .enumerate()
            .map(|(i, (x, y))| {
                let mut p = Point::new(*x as f32, *y as f32);
                if let Some(pr) = s.pressure.as_ref().and_then(|v| v.get(i)) {
                    p = p.with_pressure((*pr as f32 / RAW_PRESSURE_MAX).clamp(0.0, 1.0));
                }
                p
            })
            .collect();
    Some(Stroke {
        id: new_stroke_id(),
        points,
        brush: s
            .pressure
            .as_ref()
            .map(|_| STYLE_PRESSURE_WIDTH_V1.to_string()),
        finished: true,
    })
}

fn marker_origin(library: &LibraryRepository, notebook_id: &str) -> Option<ImportOriginV1> {
    let b = fs::read(library.notebook_dir(notebook_id).join(ORIGIN_MARKER)).ok()?;
    serde_json::from_slice(&b).ok()
}

/// Remove the partial destination of an interrupted/failed attempt and forget the reserved id.
///
/// A destination is only ever removed when it is provably this import's: its provenance marker
/// names this origin, or (`trust_unmarked`) nothing else can have created it since the
/// reservation — startup recovery before any UI, or the same run that just created it. A
/// reserved id that was never created is released (a later user notebook may reuse it; it must
/// never be mistaken for a partial copy).
fn remove_partial(
    library: &mut LibraryRepository,
    receipt: &mut ImportReceiptV1,
    sync: &dyn ImportSync,
    trust_unmarked: bool,
) -> Result<(), String> {
    let Some(dest) = receipt.dest_notebook_id.clone() else {
        return Ok(());
    };
    let in_library = library.manifest().notebook_ids.contains(&dest);
    let on_disk = library.notebook_dir(&dest).exists();
    if in_library || on_disk {
        match marker_origin(library, &dest) {
            Some(m) if m.origin_id != receipt.origin_id => {
                return Err(format!(
                    "partial destination {dest} carries another origin ({}); left untouched",
                    m.origin_id
                ))
            }
            Some(_) => {}
            None if trust_unmarked => {}
            None => {
                return Err(format!(
                    "partial destination {dest} has no import marker; left untouched"
                ))
            }
        }
        if in_library {
            sync.delete_notebook(library, &dest)
                .map_err(|e| format!("remove partial {dest}: {e}"))?;
        } else {
            quarantine_notebook_dir(library, &dest)?;
        }
    }
    receipt.dest_notebook_id = None;
    Ok(())
}

fn fail(ledger: &mut Ledger, receipt: &mut ImportReceiptV1, why: String) -> ImportOutcome {
    receipt.state = ReceiptState::Failed;
    receipt.finished_unix_ms = Some(now_ms());
    receipt.error = Some(why.clone());
    if let Err(e) = ledger.put(receipt.clone()) {
        return ImportOutcome::Failed(format!("{why}; {e}"));
    }
    ImportOutcome::Failed(why)
}

/// Import one Amazon notebook (see module docs for the state machine).
pub fn import_one(
    t: &mut ImportTarget<'_>,
    ledger: &mut Ledger,
    src: &SourceNotebook,
    sync: &dyn ImportSync,
) -> ImportOutcome {
    let previous = ledger.get(&src.origin_id).cloned();
    if previous
        .as_ref()
        .is_some_and(|r| r.state == ReceiptState::Complete)
    {
        return ImportOutcome::AlreadyImported;
    }
    match crate::sync_verified::note_library_mutation(t.library.root()) {
        Ok(_admit) => import_one_admitted(t, ledger, src, sync, previous, _admit),
        Err(e) => ImportOutcome::Failed(e),
    }
}

fn import_one_admitted(
    t: &mut ImportTarget<'_>,
    ledger: &mut Ledger,
    src: &SourceNotebook,
    sync: &dyn ImportSync,
    previous: Option<ImportReceiptV1>,
    _admit: crate::sync_verified::MutationAdmit,
) -> ImportOutcome {
    // Sync must be healthy and caught up before anything is removed or created.
    if let Err(e) = sync.flush(t.cancel) {
        return ImportOutcome::Halted(halt(e, "import start"));
    }
    let mut receipt = ImportReceiptV1 {
        origin_kind: ORIGIN_KIND.into(),
        origin_id: src.origin_id.clone(),
        state: ReceiptState::Failed,
        dest_notebook_id: None,
        title: src.title.clone(),
        book_id: None,
        started_unix_ms: now_ms(),
        finished_unix_ms: None,
        pages: None,
        strokes: None,
        error: None,
    };
    // A previous attempt's partial destination goes first (never two copies). Startup
    // recovery already handled crashes; here only a marked partial copy is removed.
    if let Some(prev) = &previous {
        let mut prev = prev.clone();
        if let Err(e) = remove_partial(t.library, &mut prev, sync, false) {
            return fail(ledger, &mut prev, e);
        }
        if let Err(e) = ledger.put(prev) {
            return ImportOutcome::Failed(e);
        }
    }

    // 1. Decode fully in memory; nothing native is written before this succeeds.
    let decoded = match read_source(&src.dir).and_then(|s| decode_snapshot(&s)) {
        Ok(d) => d,
        Err(e) => return fail(ledger, &mut receipt, e.to_string()),
    };
    receipt.book_id = decoded.book_id.clone();
    let (pages, skipped) = match convert(&decoded) {
        Ok(v) => v,
        Err(e) => return fail(ledger, &mut receipt, e),
    };
    let stroke_total: usize = pages.iter().map(Vec::len).sum();

    // 2. Durable intent before any native mutation.
    let dest = allocate_fresh_notebook_id(t.library);
    receipt.state = ReceiptState::Pending;
    receipt.dest_notebook_id = Some(dest.clone());
    if let Err(e) = ledger.put(receipt.clone()) {
        return ImportOutcome::Failed(e);
    }
    #[cfg(test)]
    if fault(Fault::CrashAfterPending) {
        return ImportOutcome::Failed(SIMULATED_CRASH.into());
    }

    // 3. Empty notebook → empty pages → strokes.
    match build(t, &dest, src, &pages, sync) {
        Ok(()) => {
            receipt.state = ReceiptState::Complete;
            receipt.finished_unix_ms = Some(now_ms());
            receipt.pages = Some(pages.len());
            receipt.strokes = Some(stroke_total);
            match ledger.put(receipt) {
                Ok(()) => ImportOutcome::Imported {
                    notebook_id: dest,
                    pages: pages.len(),
                    strokes: stroke_total,
                    skipped_empty_strokes: skipped,
                },
                // Receipt stays `pending`: the next start/retry removes and redoes it.
                Err(e) => ImportOutcome::Failed(e),
            }
        }
        Err(e) if e == SIMULATED_CRASH => ImportOutcome::Failed(e),
        // Cancelled (shutdown): leave the receipt `pending`, exactly like a crash; startup
        // recovery removes the partial copy through the synced delete path.
        Err(e) if e.starts_with(CANCELLED) => ImportOutcome::Halted(e),
        // Sync stopped: keep the partial copy (see module docs), receipt `failed`, halt the run.
        Err(e) if e.starts_with(SYNC_HALT) => {
            let why = format!("{e}; partial copy {dest} kept until retry");
            match fail(ledger, &mut receipt, why) {
                ImportOutcome::Failed(w) => ImportOutcome::Halted(w),
                o => o,
            }
        }
        Err(e) => {
            // Best effort now (this run created it); startup recovery repeats it if needed.
            let cleanup = remove_partial(t.library, &mut receipt, sync, true);
            let why = match cleanup {
                Ok(()) => e,
                Err(c) => format!("{e}; partial copy kept for later removal: {c}"),
            };
            fail(ledger, &mut receipt, why)
        }
    }
}

fn build(
    t: &mut ImportTarget<'_>,
    dest: &str,
    src: &SourceNotebook,
    pages: &[Vec<Stroke>],
    sync: &dyn ImportSync,
) -> Result<(), String> {
    let index = t.library.manifest().notebook_ids.len();
    let dims = (t.geo.xres, t.geo.yres, t.geo.line_length);
    create_notebook_at_durable(
        t.library, dest, &src.title, index, "page-1", dims, t.paper, t.profile,
    )?;
    let nb_dir = t.library.notebook_dir(dest);
    let origin = ImportOriginV1 {
        v: 1,
        origin_kind: ORIGIN_KIND.into(),
        origin_id: src.origin_id.clone(),
        imported_unix_ms: now_ms(),
    };
    write_atomic(
        &nb_dir.join(ORIGIN_MARKER),
        &serde_json::to_vec_pretty(&origin).map_err(|e| e.to_string())?,
    )?;
    sync.notebook_created(t.library, dest, &src.title);
    sync.flush(t.cancel)
        .map_err(|e| halt(e, "notebook create"))?;

    let mut manifest = read_notebook_manifest_read_only(&nb_dir)?;
    for _ in 1..pages.len() {
        let after = manifest
            .page_ids
            .last()
            .cloned()
            .ok_or("notebook has no page")?;
        let commit =
            insert_page_after_durable(&nb_dir, &mut manifest, &after, &InsertPageHooks::default())?;
        sync.page_created(&nb_dir, dest, &commit.inserted_page_id, &after);
        sync.flush(t.cancel).map_err(|e| halt(e, "page create"))?;
    }
    if manifest.page_ids.len() != pages.len() {
        return Err(format!(
            "page count {} != source {}",
            manifest.page_ids.len(),
            pages.len()
        ));
    }
    t.library
        .set_cached_page_count(dest, manifest.page_ids.len());
    for (page_id, strokes) in manifest.page_ids.iter().zip(pages) {
        if strokes.is_empty() {
            continue;
        }
        let (mut store, _) = load_existing_page(&nb_dir, page_id)?;
        for chunk in strokes.chunks(STROKES_PER_FSYNC) {
            store.append_committed_strokes(chunk)?;
            sync.strokes_committed(dest, page_id, chunk);
            // Backpressure: this batch's captures are durable before the next is written.
            sync.flush(t.cancel).map_err(|e| halt(e, "strokes"))?;
        }
        #[cfg(test)]
        {
            if fault(Fault::CrashAfterFirstPage) {
                return Err(SIMULATED_CRASH.into());
            }
            if fault(Fault::FailAfterFirstPage) {
                return Err("simulated failure after the first page".into());
            }
        }
    }
    Ok(())
}

/// Startup: finish nothing, remove every interrupted import's partial destination and mark it
/// retryable. Runs before any UI so nobody can write into a partial copy.
pub fn recover_interrupted(
    library: &mut LibraryRepository,
    sync: &dyn ImportSync,
) -> Result<Vec<String>, String> {
    let mut ledger = Ledger::open(library.root())?;
    let pending: Vec<ImportReceiptV1> = ledger
        .data
        .receipts
        .values()
        .filter(|r| r.state == ReceiptState::Pending)
        .cloned()
        .collect();
    let mut log = Vec::new();
    for mut r in pending {
        let why = match remove_partial(library, &mut r, sync, true) {
            Ok(()) => "interrupted before completion; partial copy removed".to_string(),
            Err(e) => format!("interrupted before completion; {e}"),
        };
        log.push(format!(
            "nbk_import_recovered origin={} dest={:?} detail={why}",
            r.origin_id, r.dest_notebook_id
        ));
        let _ = fail(&mut ledger, &mut r, why);
    }
    Ok(log)
}

/// Library adoption (`sync_adopt`): carry this Kindle's import ledger from the library `from`
/// into the adopted library `to` (staged, not yet live). Receipts are device history — which
/// Amazon notebooks were already imported here — so every receipt is kept and its source stays
/// ineligible. A receipt's destination is re-resolved through the notebook's global sync
/// identity (`from` local name → global ID → `to` local name), never copied as a local name;
/// the destination's provenance marker moves with it. When the adopted library does not hold
/// that notebook the destination is cleared. An interrupted (`pending`) receipt becomes
/// `failed` (retryable). Returns how many receipts were carried.
pub(crate) fn carry_receipts(from: &Path, to: &Path) -> Result<usize, String> {
    let old = Ledger::open(from)?;
    if old.data.receipts.is_empty() {
        return Ok(0);
    }
    let from_ids = crate::sync_local::read_identity(from)?;
    let to_ids = crate::sync_local::read_identity(to)?
        .ok_or("REFUSE IMPORT LEDGER: the adopted library has no sync identity")?;
    let to_local: BTreeMap<_, _> = to_ids.notebooks.iter().map(|(l, g)| (*g, l)).collect();
    let (from_lib, to_lib) = (open_library_read_only(from)?, open_library_read_only(to)?);
    let mut carried = ReceiptsV1 {
        v: 1,
        receipts: BTreeMap::new(),
    };
    for (key, r) in &old.data.receipts {
        let mut r = r.clone();
        let old_dest = r.dest_notebook_id.take();
        let new_dest = old_dest
            .as_deref()
            .and_then(|l| from_ids.as_ref()?.notebooks.get(l))
            .and_then(|g| to_local.get(g))
            .map(|l| l.to_string());
        if let (Some(od), Some(nd)) = (&old_dest, &new_dest) {
            let marker = from_lib.notebook_dir(od).join(ORIGIN_MARKER);
            if let Ok(b) = fs::read(&marker) {
                let m: Option<ImportOriginV1> = serde_json::from_slice(&b).ok();
                if m.is_some_and(|m| m.origin_id == r.origin_id) {
                    write_atomic(&to_lib.notebook_dir(nd).join(ORIGIN_MARKER), &b)?;
                }
            }
        }
        if r.state == ReceiptState::Pending {
            r.state = ReceiptState::Failed;
            r.error = Some("interrupted before the library was restored".into());
        }
        r.dest_notebook_id = new_dest;
        carried.receipts.insert(key.clone(), r);
    }
    let bytes = serde_json::to_vec_pretty(&carried).map_err(|e| e.to_string())?;
    write_atomic(&Ledger::path_for(to), &bytes)?;
    Ok(carried.receipts.len())
}

#[cfg(test)]
#[path = "nbk_import_tests.rs"]
mod tests;
