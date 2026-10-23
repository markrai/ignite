//! Gate F10 durable multi-notebook library.
//!
//! This layer owns notebook identity, title metadata, legacy-root migration, selection,
//! creation, switching, and deletion. The F2-F9 page engine remains notebook-local and is
//! called only with `notebooks/<notebook-id>/` as its data root.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kindle_platform::FbGeometry;
use serde::{Deserialize, Serialize};

use crate::ink::InkRequest;
use crate::notebook::{
    create_notebook_files_with_profile, open_or_create_notebook, require_same_profile,
    validate_manifest as validate_notebook_manifest, NotebookOpen, PersistedNotebookManifestV1,
    MANIFEST_FILE as NOTEBOOK_MANIFEST_FILE, PAGE_TRASH_DIR,
};
use crate::profile::RenderProfile;
use crate::store::{load_existing_page, page_exists};

pub const LIBRARY_MANIFEST_VERSION: u32 = 1;
pub const NOTEBOOK_INFO_VERSION: u32 = 1;
pub const LIBRARY_MANIFEST_FILE: &str = "library.meta";
pub const NOTEBOOKS_DIR: &str = "notebooks";
pub const NOTEBOOK_INFO_FILE: &str = "notebook.info";
pub const NOTEBOOK_TRASH_DIR: &str = ".notebook-trash";
const MIGRATION_RECORD_FILE: &str = ".scribe-library-migration-v1";
const MIGRATION_STAGE_NAME: &str = ".notebook-1.migrating";
pub const MAX_TITLE_CHARS: usize = 96;

pub const DEV_HOME_TRIGGER: &str = ".scribe-dev-home";
pub const DEV_LIBRARY_NEXT_TRIGGER: &str = ".scribe-dev-library-next";
pub const DEV_LIBRARY_PREV_TRIGGER: &str = ".scribe-dev-library-prev";
pub const DEV_LIBRARY_OPEN_TRIGGER: &str = ".scribe-dev-library-open";
pub const DEV_CREATE_NOTEBOOK_TRIGGER: &str = ".scribe-dev-create-notebook";
pub const DEV_DELETE_NOTEBOOK_TRIGGER: &str = ".scribe-dev-delete-notebook";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevLibraryOp {
    Home,
    Next,
    Previous,
    Open,
    Create,
    Delete,
}

/// Deterministic F10 priority. At most one matching trigger is consumed per idle loop.
pub fn peek_dev_library_op(root: &Path) -> Option<DevLibraryOp> {
    [
        (DEV_HOME_TRIGGER, DevLibraryOp::Home),
        (DEV_LIBRARY_NEXT_TRIGGER, DevLibraryOp::Next),
        (DEV_LIBRARY_PREV_TRIGGER, DevLibraryOp::Previous),
        (DEV_LIBRARY_OPEN_TRIGGER, DevLibraryOp::Open),
        (DEV_CREATE_NOTEBOOK_TRIGGER, DevLibraryOp::Create),
        (DEV_DELETE_NOTEBOOK_TRIGGER, DevLibraryOp::Delete),
    ]
    .into_iter()
    .find_map(|(name, op)| root.join(name).exists().then_some(op))
}

pub fn consume_dev_library_op(root: &Path, op: DevLibraryOp) {
    let name = match op {
        DevLibraryOp::Home => DEV_HOME_TRIGGER,
        DevLibraryOp::Next => DEV_LIBRARY_NEXT_TRIGGER,
        DevLibraryOp::Previous => DEV_LIBRARY_PREV_TRIGGER,
        DevLibraryOp::Open => DEV_LIBRARY_OPEN_TRIGGER,
        DevLibraryOp::Create => DEV_CREATE_NOTEBOOK_TRIGGER,
        DevLibraryOp::Delete => DEV_DELETE_NOTEBOOK_TRIGGER,
    };
    let _ = fs::remove_file(root.join(name));
}

#[cfg(test)]
pub fn arm_dev_library_op(root: &Path, op: DevLibraryOp) -> Result<(), String> {
    let name = match op {
        DevLibraryOp::Home => DEV_HOME_TRIGGER,
        DevLibraryOp::Next => DEV_LIBRARY_NEXT_TRIGGER,
        DevLibraryOp::Previous => DEV_LIBRARY_PREV_TRIGGER,
        DevLibraryOp::Open => DEV_LIBRARY_OPEN_TRIGGER,
        DevLibraryOp::Create => DEV_CREATE_NOTEBOOK_TRIGGER,
        DevLibraryOp::Delete => DEV_DELETE_NOTEBOOK_TRIGGER,
    };
    fs::write(root.join(name), b"1\n").map_err(|e| e.to_string())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedLibraryManifestV1 {
    pub v: u32,
    pub notebook_ids: Vec<String>,
    pub active_notebook_id: String,
    pub next_notebook_ordinal: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedNotebookInfoV1 {
    pub v: u32,
    pub title: String,
}

#[derive(Clone, Debug, Default)]
pub struct LibraryManifestWriteHooks {
    pub fail_write: bool,
    pub fail_fsync: bool,
    pub fail_rename: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CreateNotebookHooks {
    pub manifest: LibraryManifestWriteHooks,
    pub fail_cleanup: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SwitchNotebookHooks {
    pub manifest: LibraryManifestWriteHooks,
}

/// F11 rename fault hooks: the only durable object a rename touches is `notebook.info`.
#[derive(Clone, Debug, Default)]
pub struct RenameNotebookHooks {
    pub info: LibraryManifestWriteHooks,
}

#[derive(Clone, Debug, Default)]
pub struct DeleteNotebookHooks {
    pub manifest: LibraryManifestWriteHooks,
    pub fail_quarantine: bool,
}

/// Migration-only fault hooks. They model process loss without changing authority rules.
#[derive(Clone, Debug, Default)]
pub struct MigrationHooks {
    /// Stop after this many source objects have been moved into staging.
    pub fail_after_moves: Option<usize>,
    /// Stop after staging has been atomically renamed to `notebook-1`.
    pub fail_after_publish: bool,
    /// Fail the final `library.meta` write before it becomes authoritative.
    pub manifest: LibraryManifestWriteHooks,
}

#[derive(Clone, Debug)]
pub struct LibraryRepository {
    root: PathBuf,
    /// `library.meta`, except `active_notebook_id`, which is the replica-local selection
    /// ([`crate::nav_state::selected_notebook`]). Every canonical write persists it as is.
    manifest: PersistedLibraryManifestV1,
    /// `active_notebook_id` as it is in `library.meta` on disk (the selection's `base`).
    disk_active_notebook_id: String,
    summaries: BTreeMap<String, NotebookHomeSummary>,
    orphan_dirs: Vec<String>,
}

/// Title + page count for Home. Filled at library-shell open; Home cards must not re-read disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotebookHomeSummary {
    pub title: String,
    pub page_count: Option<usize>,
}

pub struct LibraryShell {
    pub repository: LibraryRepository,
    pub migrated_legacy: bool,
}

pub struct LibraryOpen {
    pub repository: LibraryRepository,
    pub notebook_dir: PathBuf,
    pub notebook: NotebookOpen,
    pub migrated_legacy: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateCleanupOutcome {
    Removed,
    LeftAsOrphan { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateNotebookCommit {
    pub notebook_id: String,
    pub title: String,
    pub manifest_persist_us: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchNotebookCommit {
    pub from_notebook_id: String,
    pub target_notebook_id: String,
    pub manifest_persist_us: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameNotebookCommit {
    pub notebook_id: String,
    pub old_title: String,
    pub new_title: String,
    /// `false` when the requested title equals the stored one (no write performed).
    pub changed: bool,
    pub info_persist_us: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotebookQuarantineOutcome {
    Moved { trash_entry_name: String },
    LeftAsOrphan { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteNotebookCommit {
    pub deleted_notebook_id: String,
    pub active_notebook_id: String,
    pub quarantine: NotebookQuarantineOutcome,
    pub manifest_persist_us: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct MigrationRecordV1 {
    v: u32,
    notebook_id: String,
    objects: Vec<String>,
}

impl LibraryRepository {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest(&self) -> &PersistedLibraryManifestV1 {
        &self.manifest
    }

    pub fn active_notebook_id(&self) -> &str {
        &self.manifest.active_notebook_id
    }

    pub fn active_notebook_dir(&self) -> PathBuf {
        self.notebook_dir(&self.manifest.active_notebook_id)
    }

    pub fn notebook_dir(&self, notebook_id: &str) -> PathBuf {
        self.root.join(NOTEBOOKS_DIR).join(notebook_id)
    }

    pub fn title(&self, notebook_id: &str) -> Result<&str, String> {
        self.summaries
            .get(notebook_id)
            .map(|s| s.title.as_str())
            .ok_or_else(|| format!("REFUSE LIBRARY: missing title for {notebook_id}"))
    }

    pub fn entries(&self) -> Vec<(&str, &str)> {
        self.manifest
            .notebook_ids
            .iter()
            .map(|id| {
                (
                    id.as_str(),
                    self.summaries
                        .get(id)
                        .map(|s| s.title.as_str())
                        .unwrap_or("?"),
                )
            })
            .collect()
    }

    pub fn orphan_dirs(&self) -> &[String] {
        &self.orphan_dirs
    }

    /// Cached page count from library-shell open (or later mutation). No disk I/O.
    pub fn notebook_page_count(&self, notebook_id: &str) -> Result<usize, String> {
        self.summaries
            .get(notebook_id)
            .and_then(|s| s.page_count)
            .ok_or_else(|| format!("notebook {notebook_id} page count unavailable"))
    }

    /// Home cards read this map only — never `notebook.meta` / page dirs.
    pub fn home_summaries(&self) -> &BTreeMap<String, NotebookHomeSummary> {
        &self.summaries
    }

    pub fn set_cached_page_count(&mut self, notebook_id: &str, page_count: usize) {
        if let Some(s) = self.summaries.get_mut(notebook_id) {
            s.page_count = Some(page_count);
        }
    }

    /// The title `create_notebook_durable(.., None, ..)` would assign next.
    pub fn next_default_title(&self) -> String {
        default_title(self.manifest.next_notebook_ordinal)
    }
}

fn library_manifest_path(root: &Path) -> PathBuf {
    root.join(LIBRARY_MANIFEST_FILE)
}

fn migration_record_path(root: &Path) -> PathBuf {
    root.join(MIGRATION_RECORD_FILE)
}

fn sync_dir(path: &Path) {
    #[cfg(unix)]
    if let Ok(f) = File::open(path) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn write_json_atomic<T: Serialize>(
    dir: &Path,
    final_name: &str,
    value: &T,
    hooks: &LibraryManifestWriteHooks,
) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // Library root writes (library.meta, migration record) and notebook-info under notebooks/<id>/.
    // Do not treat an unresolved notebook path as the library root — that would bump the
    // wrong generation file and leave the real stamp trusted.
    let _admit = match crate::sync_verified::library_root_from_tree(dir) {
        Some(root) => crate::sync_verified::note_library_mutation_scoped(
            &root,
            &crate::page_ledger::scope_for_dir(&root, dir),
        )?,
        None if final_name == LIBRARY_MANIFEST_FILE || final_name == MIGRATION_RECORD_FILE => {
            crate::sync_verified::note_library_mutation_scoped(
                dir,
                &crate::sync_verified::LedgerScope::Structure,
            )?
        }
        None if crate::sync_verified::path_looks_like_library_tree(dir) => {
            return Err(format!(
                "REFUSE SYNC STAMP: cannot resolve library root from {}",
                dir.display()
            ));
        }
        None => crate::sync_verified::MutationAdmit::untracked(),
    };
    let final_path = dir.join(final_name);
    let tmp_path = dir.join(format!("{final_name}.tmp"));
    let mut json = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    json.push(b'\n');
    if hooks.fail_write {
        return Err("injected: library manifest write failed".into());
    }
    {
        let mut f = File::create(&tmp_path).map_err(|e| e.to_string())?;
        f.write_all(&json).map_err(|e| e.to_string())?;
        if hooks.fail_fsync {
            drop(f);
            let _ = fs::remove_file(&tmp_path);
            return Err("injected: library manifest fsync failed".into());
        }
        f.sync_all().map_err(|e| e.to_string())?;
    }
    if hooks.fail_rename {
        let _ = fs::remove_file(&tmp_path);
        return Err("injected: library manifest rename failed".into());
    }
    fs::rename(&tmp_path, &final_path).map_err(|e| e.to_string())?;
    sync_dir(dir);
    Ok(())
}

fn write_library_manifest_atomic(
    root: &Path,
    manifest: &PersistedLibraryManifestV1,
    hooks: &LibraryManifestWriteHooks,
) -> Result<(), String> {
    write_json_atomic(root, LIBRARY_MANIFEST_FILE, manifest, hooks)
}

fn write_notebook_info(dir: &Path, title: &str) -> Result<(), String> {
    write_notebook_info_with(dir, title, &LibraryManifestWriteHooks::default())
}

fn write_notebook_info_with(
    dir: &Path,
    title: &str,
    hooks: &LibraryManifestWriteHooks,
) -> Result<(), String> {
    validate_title(title)?;
    let info = PersistedNotebookInfoV1 {
        v: NOTEBOOK_INFO_VERSION,
        title: title.to_string(),
    };
    write_json_atomic(dir, NOTEBOOK_INFO_FILE, &info, hooks)
}

fn parse_notebook_ordinal(id: &str) -> Result<u64, String> {
    let Some(raw) = id.strip_prefix("notebook-") else {
        return Err(format!("REFUSE LIBRARY: invalid notebook id {id:?}"));
    };
    if raw.is_empty() || raw.starts_with('0') || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("REFUSE LIBRARY: invalid notebook id {id:?}"));
    }
    raw.parse::<u64>()
        .map_err(|_| format!("REFUSE LIBRARY: invalid notebook id {id:?}"))
}

pub fn validate_library_manifest(m: &PersistedLibraryManifestV1) -> Result<(), String> {
    if m.v != LIBRARY_MANIFEST_VERSION {
        return Err(format!(
            "REFUSE LIBRARY OPEN: unsupported library version {} (need {LIBRARY_MANIFEST_VERSION})",
            m.v
        ));
    }
    if m.notebook_ids.is_empty() {
        return Err("REFUSE LIBRARY OPEN: notebook_ids must be nonempty".into());
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let mut max_ordinal = 0u64;
    for id in &m.notebook_ids {
        if !seen.insert(id.as_str()) {
            return Err(format!("REFUSE LIBRARY OPEN: duplicate notebook id {id}"));
        }
        max_ordinal = max_ordinal.max(parse_notebook_ordinal(id)?);
    }
    if !seen.contains(m.active_notebook_id.as_str()) {
        return Err(format!(
            "REFUSE LIBRARY OPEN: active_notebook_id {} not in notebook_ids",
            m.active_notebook_id
        ));
    }
    if m.next_notebook_ordinal == 0 || m.next_notebook_ordinal <= max_ordinal {
        return Err(format!(
            "REFUSE LIBRARY OPEN: invalid next_notebook_ordinal {} (must exceed {max_ordinal})",
            m.next_notebook_ordinal
        ));
    }
    Ok(())
}

pub fn validate_title(title: &str) -> Result<(), String> {
    if title.trim().is_empty() {
        return Err("REFUSE NOTEBOOK TITLE: title is empty after trimming".into());
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(format!(
            "REFUSE NOTEBOOK TITLE: title exceeds {MAX_TITLE_CHARS} characters"
        ));
    }
    if title.chars().any(char::is_control) {
        return Err("REFUSE NOTEBOOK TITLE: control characters are not allowed".into());
    }
    Ok(())
}

fn read_library_manifest(root: &Path) -> Result<PersistedLibraryManifestV1, String> {
    let path = library_manifest_path(root);
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("REFUSE LIBRARY OPEN: read {}: {e}", path.display()))?;
    let manifest: PersistedLibraryManifestV1 = serde_json::from_str(&raw)
        .map_err(|e| format!("REFUSE LIBRARY OPEN: bad library.meta: {e}"))?;
    validate_library_manifest(&manifest)?;
    Ok(manifest)
}

fn read_notebook_info(dir: &Path) -> Result<PersistedNotebookInfoV1, String> {
    let path = dir.join(NOTEBOOK_INFO_FILE);
    let raw = fs::read_to_string(&path).map_err(|e| {
        format!(
            "REFUSE LIBRARY OPEN: read notebook title {}: {e}",
            path.display()
        )
    })?;
    let info: PersistedNotebookInfoV1 = serde_json::from_str(&raw)
        .map_err(|e| format!("REFUSE LIBRARY OPEN: bad {}: {e}", path.display()))?;
    if info.v != NOTEBOOK_INFO_VERSION {
        return Err(format!(
            "REFUSE LIBRARY OPEN: unsupported notebook.info version {}",
            info.v
        ));
    }
    validate_title(&info.title)?;
    Ok(info)
}

fn read_notebook_manifest(dir: &Path) -> Result<PersistedNotebookManifestV1, String> {
    let m = read_notebook_manifest_structural(dir)?;
    ensure_notebook_pages_exist(dir, &m)?;
    Ok(m)
}

/// Parse + structurally validate `notebook.meta`. Does **not** `page_exists` each page.
/// Launch/Home uses this so a missing page under an unopened notebook cannot block Home.
fn read_notebook_manifest_structural(dir: &Path) -> Result<PersistedNotebookManifestV1, String> {
    let path = dir.join(NOTEBOOK_MANIFEST_FILE);
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("REFUSE LIBRARY OPEN: read {}: {e}", path.display()))?;
    let m: PersistedNotebookManifestV1 = serde_json::from_str(&raw)
        .map_err(|e| format!("REFUSE LIBRARY OPEN: bad {}: {e}", path.display()))?;
    validate_notebook_manifest(&m)?;
    Ok(m)
}

fn ensure_notebook_pages_exist(dir: &Path, m: &PersistedNotebookManifestV1) -> Result<(), String> {
    for page_id in &m.page_ids {
        if !page_exists(dir, page_id) {
            return Err(format!(
                "REFUSE LIBRARY OPEN: notebook {} references missing page {page_id}",
                dir.display()
            ));
        }
    }
    Ok(())
}

fn validate_notebook_directory(dir: &Path, expected_id: &str) -> Result<(), String> {
    if !dir.is_dir() {
        return Err(format!(
            "REFUSE LIBRARY OPEN: referenced notebook directory missing ({})",
            dir.display()
        ));
    }
    let info = read_notebook_info(dir)?;
    validate_title(&info.title)?;
    let manifest = read_notebook_manifest(dir)?;
    if manifest.notebook_id != expected_id {
        return Err(format!(
            "REFUSE LIBRARY OPEN: notebook identity mismatch directory={expected_id} manifest={}",
            manifest.notebook_id
        ));
    }
    Ok(())
}

fn list_orphan_notebook_dirs(root: &Path, ids: &[String]) -> Vec<String> {
    let notebooks = root.join(NOTEBOOKS_DIR);
    let Ok(rd) = fs::read_dir(notebooks) else {
        return Vec::new();
    };
    let known: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || known.contains(name.as_str()) {
            continue;
        }
        out.push(name);
    }
    out.sort();
    out
}

fn load_repository(root: &Path) -> Result<LibraryRepository, String> {
    // Launch-critical: `library.meta`, each listed notebook directory, `notebook.info` titles,
    // and structural `notebook.meta` (page *count* for Home). Per-page `page_exists` is an
    // open-time / sync-time gate — a missing page must not block Home.
    let mut manifest = read_library_manifest(root)?;
    let disk_active_notebook_id = manifest.active_notebook_id.clone();
    manifest.active_notebook_id = crate::nav_state::selected_notebook(root, &manifest);
    let mut summaries = BTreeMap::new();
    for id in &manifest.notebook_ids {
        let dir = root.join(NOTEBOOKS_DIR).join(id);
        if !dir.is_dir() {
            return Err(format!(
                "REFUSE LIBRARY OPEN: referenced notebook directory missing ({})",
                dir.display()
            ));
        }
        let info = read_notebook_info(&dir)?;
        let page_count = match read_notebook_manifest_structural(&dir) {
            Ok(m) if m.notebook_id == *id => Some(m.page_ids.len()),
            Ok(m) => {
                return Err(format!(
                    "REFUSE LIBRARY OPEN: notebook identity mismatch directory={id} manifest={}",
                    m.notebook_id
                ));
            }
            Err(_) => None,
        };
        summaries.insert(
            id.clone(),
            NotebookHomeSummary {
                title: info.title,
                page_count,
            },
        );
    }
    let orphan_dirs = list_orphan_notebook_dirs(root, &manifest.notebook_ids);
    for orphan in &orphan_dirs {
        eprintln!("warning: orphan notebook directory ignored (not in library manifest): {orphan}");
    }
    Ok(LibraryRepository {
        root: root.to_path_buf(),
        manifest,
        disk_active_notebook_id,
        summaries,
        orphan_dirs,
    })
}

/// Open and fully validate an existing library **without writing anything**: no creation, no
/// legacy migration, no migration-record cleanup. For read-only consumers (sync export).
pub fn open_library_read_only(root: &Path) -> Result<LibraryRepository, String> {
    if !library_manifest_path(root).is_file() {
        return Err(format!(
            "REFUSE LIBRARY OPEN: {} has no {LIBRARY_MANIFEST_FILE} (read-only open never creates or migrates)",
            root.display()
        ));
    }
    load_repository(root)
}

/// Read + validate one notebook's manifest (every listed page must exist). Read-only.
pub fn read_notebook_manifest_read_only(
    notebook_dir: &Path,
) -> Result<PersistedNotebookManifestV1, String> {
    read_notebook_manifest(notebook_dir)
}

fn default_title(ordinal: u64) -> String {
    format!("Notebook {ordinal}")
}

fn create_initial_library(
    root: &Path,
    profile: &RenderProfile,
    geo: &FbGeometry,
    background: &[u8],
) -> Result<(), String> {
    create_library_with_first_notebook(root, &default_title(1), profile, geo, background)
}

/// Create a new library whose only notebook is `notebook-1` titled `title`, with one page of
/// `background` (panel `geo`) and `profile`. Sync adoption uses it to start a library directly
/// as the first notebook of the canonical state it then materializes (no placeholder notebook).
pub fn create_library_with_first_notebook(
    root: &Path,
    title: &str,
    profile: &RenderProfile,
    geo: &FbGeometry,
    background: &[u8],
) -> Result<(), String> {
    validate_title(title)?;
    if library_manifest_path(root).exists() {
        return Err(format!(
            "REFUSE LIBRARY CREATE: {} already holds a library",
            root.display()
        ));
    }
    let notebooks = root.join(NOTEBOOKS_DIR);
    fs::create_dir_all(&notebooks).map_err(|e| e.to_string())?;
    let id = "notebook-1";
    let dir = notebooks.join(id);
    if dir.exists() {
        return Err(format!(
            "REFUSE LIBRARY CREATE: unreferenced target already exists ({})",
            dir.display()
        ));
    }
    if let Err(e) = create_notebook_files_with_profile(&dir, id, profile, geo, background)
        .and_then(|()| write_notebook_info(&dir, title))
    {
        let _ = fs::remove_dir_all(&dir);
        return Err(e);
    }
    let manifest = PersistedLibraryManifestV1 {
        v: LIBRARY_MANIFEST_VERSION,
        notebook_ids: vec![id.into()],
        active_notebook_id: id.into(),
        next_notebook_ordinal: 2,
    };
    if let Err(e) =
        write_library_manifest_atomic(root, &manifest, &LibraryManifestWriteHooks::default())
    {
        let _ = fs::remove_dir_all(&dir);
        return Err(format!("REFUSE LIBRARY CREATE: library manifest: {e}"));
    }
    Ok(())
}

fn validate_legacy_root(root: &Path) -> Result<MigrationRecordV1, String> {
    let manifest = read_notebook_manifest(root)?;
    if manifest.notebook_id != "notebook-1" {
        return Err(format!(
            "REFUSE LEGACY MIGRATION: expected notebook-1 identity, got {}",
            manifest.notebook_id
        ));
    }
    let mut profile: Option<RenderProfile> = None;
    for page_id in &manifest.page_ids {
        let (store, loaded) = load_existing_page(root, page_id).map_err(|e| {
            format!("REFUSE LEGACY MIGRATION: page {page_id} validation failed: {e}")
        })?;
        drop(store);
        if let Some(expected) = &profile {
            require_same_profile(expected, &loaded.render_profile)?;
        } else {
            profile = Some(loaded.render_profile);
        }
    }

    let mut objects = vec![NOTEBOOK_MANIFEST_FILE.to_string()];
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("page-") {
            if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                return Err(format!(
                    "REFUSE LEGACY MIGRATION: page-like object is not a directory: {name}"
                ));
            }
            objects.push(name);
        }
    }
    if root.join(PAGE_TRASH_DIR).exists() {
        if !root.join(PAGE_TRASH_DIR).is_dir() {
            return Err("REFUSE LEGACY MIGRATION: .page-trash is not a directory".into());
        }
        objects.push(PAGE_TRASH_DIR.into());
    }
    objects.sort();
    objects.dedup();
    for page_id in &manifest.page_ids {
        if !objects.iter().any(|o| o == page_id) {
            return Err(format!(
                "REFUSE LEGACY MIGRATION: referenced page missing from move plan: {page_id}"
            ));
        }
    }
    Ok(MigrationRecordV1 {
        v: 1,
        notebook_id: "notebook-1".into(),
        objects,
    })
}

fn validate_migration_record(record: &MigrationRecordV1) -> Result<(), String> {
    if record.v != 1 || record.notebook_id != "notebook-1" || record.objects.is_empty() {
        return Err("REFUSE LEGACY MIGRATION: malformed migration record".into());
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for object in &record.objects {
        let safe = object == NOTEBOOK_MANIFEST_FILE
            || object == PAGE_TRASH_DIR
            || (object.starts_with("page-")
                && !object.contains('/')
                && !object.contains('\\')
                && object.len() > "page-".len());
        if !safe || !seen.insert(object.as_str()) {
            return Err(format!(
                "REFUSE LEGACY MIGRATION: unsafe or duplicate object {object:?}"
            ));
        }
    }
    if !seen.contains(NOTEBOOK_MANIFEST_FILE) {
        return Err("REFUSE LEGACY MIGRATION: record omits notebook.meta".into());
    }
    Ok(())
}

fn read_migration_record(root: &Path) -> Result<MigrationRecordV1, String> {
    let path = migration_record_path(root);
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("REFUSE LEGACY MIGRATION: read record: {e}"))?;
    let record: MigrationRecordV1 = serde_json::from_str(&raw)
        .map_err(|e| format!("REFUSE LEGACY MIGRATION: bad record: {e}"))?;
    validate_migration_record(&record)?;
    Ok(record)
}

fn migrate_legacy_root(root: &Path, hooks: &MigrationHooks) -> Result<(), String> {
    let record_path = migration_record_path(root);
    let record = if record_path.exists() {
        read_migration_record(root)?
    } else {
        let record = validate_legacy_root(root)?;
        write_json_atomic(
            root,
            MIGRATION_RECORD_FILE,
            &record,
            &LibraryManifestWriteHooks::default(),
        )?;
        record
    };

    // The moves below also run when resuming from an existing record (no write above), so
    // admit them explicitly: a bulk layout change, recorded as `all` (19B.3C).
    let _admit = crate::sync_verified::note_library_mutation(root)?;
    let notebooks = root.join(NOTEBOOKS_DIR);
    let stage = notebooks.join(MIGRATION_STAGE_NAME);
    let target = notebooks.join(&record.notebook_id);
    fs::create_dir_all(&notebooks).map_err(|e| e.to_string())?;

    if target.exists() {
        if stage.exists() {
            return Err(
                "REFUSE LEGACY MIGRATION: both published target and staging directory exist".into(),
            );
        }
        for object in &record.objects {
            if root.join(object).exists() {
                return Err(format!(
                    "REFUSE LEGACY MIGRATION: published target exists but source object remains: {object}"
                ));
            }
            if !target.join(object).exists() {
                return Err(format!(
                    "REFUSE LEGACY MIGRATION: published target missing recorded object: {object}"
                ));
            }
        }
    } else {
        fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
        let mut moved = 0usize;
        for object in &record.objects {
            let src = root.join(object);
            let dst = stage.join(object);
            match (src.exists(), dst.exists()) {
                (true, false) => {
                    fs::rename(&src, &dst).map_err(|e| {
                        format!(
                            "REFUSE LEGACY MIGRATION: move {} to staging: {e}",
                            src.display()
                        )
                    })?;
                    sync_dir(root);
                    sync_dir(&stage);
                    moved += 1;
                    if hooks.fail_after_moves == Some(moved) {
                        return Err(format!(
                            "injected: migration interrupted after {moved} object moves"
                        ));
                    }
                }
                (false, true) => {}
                (true, true) => {
                    return Err(format!(
                        "REFUSE LEGACY MIGRATION: object exists in source and staging: {object}"
                    ));
                }
                (false, false) => {
                    return Err(format!(
                        "REFUSE LEGACY MIGRATION: recorded object missing from source and staging: {object}"
                    ));
                }
            }
        }
        let info_path = stage.join(NOTEBOOK_INFO_FILE);
        if !info_path.exists() {
            write_notebook_info(&stage, &default_title(1))?;
        } else {
            read_notebook_info(&stage)?;
        }
        sync_dir(&stage);
        fs::rename(&stage, &target)
            .map_err(|e| format!("REFUSE LEGACY MIGRATION: publish staging directory: {e}"))?;
        sync_dir(&notebooks);
        if hooks.fail_after_publish {
            return Err("injected: migration interrupted after stage publish".into());
        }
    }

    validate_notebook_directory(&target, &record.notebook_id)?;
    let library = PersistedLibraryManifestV1 {
        v: LIBRARY_MANIFEST_VERSION,
        notebook_ids: vec![record.notebook_id.clone()],
        active_notebook_id: record.notebook_id,
        next_notebook_ordinal: 2,
    };
    write_library_manifest_atomic(root, &library, &hooks.manifest)
        .map_err(|e| format!("REFUSE LEGACY MIGRATION: library commit failed: {e}"))?;
    let _ = fs::remove_file(&record_path);
    sync_dir(root);
    Ok(())
}

pub fn open_or_create_library(
    root: &Path,
    ink_request: InkRequest,
    geo: &FbGeometry,
    background: &[u8],
) -> Result<LibraryOpen, String> {
    open_or_create_library_with_hooks(
        root,
        ink_request,
        geo,
        background,
        &MigrationHooks::default(),
    )
}

/// Create/migrate the library and load a Home summary. Does **not** open page journals
/// or bind the active notebook.
pub fn open_or_create_library_shell(
    root: &Path,
    ink_request: InkRequest,
    geo: &FbGeometry,
    background: &[u8],
) -> Result<LibraryShell, String> {
    open_or_create_library_shell_with_hooks(
        root,
        ink_request,
        geo,
        background,
        &MigrationHooks::default(),
    )
}

pub fn open_or_create_library_with_hooks(
    root: &Path,
    ink_request: InkRequest,
    geo: &FbGeometry,
    background: &[u8],
    migration_hooks: &MigrationHooks,
) -> Result<LibraryOpen, String> {
    let shell = open_or_create_library_shell_with_hooks(
        root,
        ink_request,
        geo,
        background,
        migration_hooks,
    )?;
    let notebook_dir = shell.repository.active_notebook_dir();
    crate::launch_clock::mark("notebook_bind_start");
    let notebook = open_or_create_notebook(&notebook_dir, ink_request, geo, background)?;
    crate::launch_clock::mark("notebook_bind_end");
    Ok(LibraryOpen {
        repository: shell.repository,
        notebook_dir,
        notebook,
        migrated_legacy: shell.migrated_legacy,
    })
}

fn open_or_create_library_shell_with_hooks(
    root: &Path,
    ink_request: InkRequest,
    geo: &FbGeometry,
    background: &[u8],
    migration_hooks: &MigrationHooks,
) -> Result<LibraryShell, String> {
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let had_library = library_manifest_path(root).exists();
    let had_legacy =
        root.join(NOTEBOOK_MANIFEST_FILE).exists() || migration_record_path(root).exists();
    let migrated_legacy = !had_library && had_legacy;

    if had_library {
        let _ = read_library_manifest(root)?;
        if migration_record_path(root).exists() {
            let _ = fs::remove_file(migration_record_path(root));
        }
    } else if had_legacy {
        migrate_legacy_root(root, migration_hooks)?;
    } else {
        let profile = RenderProfile::sealed_v2_pressure(crate::ink::ink_for_new_page(ink_request))?;
        create_initial_library(root, &profile, geo, background)?;
    }

    crate::launch_clock::mark("repository_open_start");
    let repository = load_repository(root)?;
    crate::launch_clock::mark("repository_open_end");
    Ok(LibraryShell {
        repository,
        migrated_legacy,
    })
}

pub fn open_active_notebook(
    library: &LibraryRepository,
    ink_request: InkRequest,
    geo: &FbGeometry,
    background: &[u8],
) -> Result<(PathBuf, NotebookOpen), String> {
    let dir = library.active_notebook_dir();
    validate_notebook_directory(&dir, library.active_notebook_id())?;
    let notebook = open_or_create_notebook(&dir, ink_request, geo, background)?;
    Ok((dir, notebook))
}

fn cleanup_created_notebook(dir: &Path, fail: bool) -> CreateCleanupOutcome {
    if fail {
        return CreateCleanupOutcome::LeftAsOrphan {
            reason: "injected: cleanup failed".into(),
        };
    }
    match fs::remove_dir_all(dir) {
        Ok(()) => CreateCleanupOutcome::Removed,
        Err(e) => CreateCleanupOutcome::LeftAsOrphan {
            reason: format!("cleanup remove_dir_all failed: {e}"),
        },
    }
}

pub fn create_notebook_durable(
    library: &mut LibraryRepository,
    profile: &RenderProfile,
    geo: &FbGeometry,
    background: &[u8],
    explicit_title: Option<&str>,
    hooks: &CreateNotebookHooks,
) -> Result<CreateNotebookCommit, String> {
    let ordinal = library.manifest.next_notebook_ordinal;
    let next_ordinal = ordinal
        .checked_add(1)
        .ok_or("REFUSE NOTEBOOK CREATE: ordinal overflow")?;
    let id = format!("notebook-{ordinal}");
    let title = explicit_title
        .map(str::to_string)
        .unwrap_or_else(|| default_title(ordinal));
    validate_title(&title)?;
    if library.manifest.notebook_ids.iter().any(|v| v == &id) {
        return Err(format!("REFUSE NOTEBOOK CREATE: id collision {id}"));
    }
    let dir = library.notebook_dir(&id);
    if dir.exists() {
        return Err(format!(
            "REFUSE NOTEBOOK CREATE: target already exists and will not be attached ({})",
            dir.display()
        ));
    }
    create_notebook_files_with_profile(&dir, &id, profile, geo, background)
        .and_then(|()| write_notebook_info(&dir, &title))
        .map_err(|e| {
            let cleanup = cleanup_created_notebook(&dir, hooks.fail_cleanup);
            format!("REFUSE NOTEBOOK CREATE: file creation failed; cleanup={cleanup:?}: {e}")
        })?;

    let mut planned = library.manifest.clone();
    planned.notebook_ids.push(id.clone());
    planned.active_notebook_id = id.clone();
    planned.next_notebook_ordinal = next_ordinal;
    validate_library_manifest(&planned)?;
    let t0 = Instant::now();
    if let Err(e) = write_library_manifest_atomic(&library.root, &planned, &hooks.manifest) {
        let cleanup = cleanup_created_notebook(&dir, hooks.fail_cleanup);
        return Err(format!(
            "REFUSE NOTEBOOK CREATE: library manifest persist failed (new notebook not selectable; cleanup={cleanup:?}): {e}"
        ));
    }
    let manifest_persist_us = t0.elapsed().as_micros() as u64;
    library.disk_active_notebook_id = planned.active_notebook_id.clone();
    library.manifest = planned;
    library.summaries.insert(
        id.clone(),
        NotebookHomeSummary {
            title: title.clone(),
            page_count: Some(1),
        },
    );
    library.orphan_dirs = list_orphan_notebook_dirs(&library.root, &library.manifest.notebook_ids);
    Ok(CreateNotebookCommit {
        notebook_id: id,
        title,
        manifest_persist_us,
    })
}

pub fn switch_notebook_durable(
    library: &mut LibraryRepository,
    target_id: &str,
    hooks: &SwitchNotebookHooks,
) -> Result<SwitchNotebookCommit, String> {
    if !library
        .manifest
        .notebook_ids
        .iter()
        .any(|id| id == target_id)
    {
        return Err(format!(
            "REFUSE NOTEBOOK SWITCH: target {target_id} not in library"
        ));
    }
    validate_notebook_directory(&library.notebook_dir(target_id), target_id)?;
    let from = library.manifest.active_notebook_id.clone();
    if from == target_id {
        return Ok(SwitchNotebookCommit {
            from_notebook_id: from,
            target_notebook_id: target_id.to_string(),
            manifest_persist_us: 0,
        });
    }
    // 19B.3D.2: the selection is replica-local state, not a library mutation; `library.meta`
    // is not rewritten. A failed save only loses the reopen choice and is logged.
    let t0 = Instant::now();
    let saved = if hooks.manifest.fail_write {
        Err("injected: selection write failed".to_string())
    } else {
        crate::nav_state::remember_notebook(
            &library.root,
            &library.disk_active_notebook_id,
            target_id,
        )
    };
    if let Err(e) = saved {
        eprintln!("warning: nav_state_save_failed notebook={target_id} detail={e}");
    }
    let manifest_persist_us = t0.elapsed().as_micros() as u64;
    library.manifest.active_notebook_id = target_id.to_string();
    Ok(SwitchNotebookCommit {
        from_notebook_id: from,
        target_notebook_id: target_id.to_string(),
        manifest_persist_us,
    })
}

/// F11: durably retitle a notebook. Only `notebook.info` is rewritten (temp → fsync →
/// rename → dir sync); the notebook ID, directory, `library.meta`, `notebook.meta`, and every
/// page file are untouched. RAM changes only after the durable write succeeds, so on failure
/// the old title remains authoritative both on disk and in memory.
pub fn rename_notebook_durable(
    library: &mut LibraryRepository,
    notebook_id: &str,
    new_title: &str,
    hooks: &RenameNotebookHooks,
) -> Result<RenameNotebookCommit, String> {
    if !library
        .manifest
        .notebook_ids
        .iter()
        .any(|id| id == notebook_id)
    {
        return Err(format!(
            "REFUSE NOTEBOOK RENAME: notebook {notebook_id} not in library"
        ));
    }
    validate_title(new_title).map_err(|e| format!("REFUSE NOTEBOOK RENAME: {e}"))?;
    let dir = library.notebook_dir(notebook_id);
    validate_notebook_directory(&dir, notebook_id)?;
    let old_title = library.title(notebook_id)?.to_string();
    if old_title == new_title {
        return Ok(RenameNotebookCommit {
            notebook_id: notebook_id.to_string(),
            old_title,
            new_title: new_title.to_string(),
            changed: false,
            info_persist_us: 0,
        });
    }
    let t0 = Instant::now();
    write_notebook_info_with(&dir, new_title, &hooks.info)
        .map_err(|e| format!("REFUSE NOTEBOOK RENAME: notebook.info persist failed: {e}"))?;
    let info_persist_us = t0.elapsed().as_micros() as u64;
    if let Some(s) = library.summaries.get_mut(notebook_id) {
        s.title = new_title.to_string();
    }
    Ok(RenameNotebookCommit {
        notebook_id: notebook_id.to_string(),
        old_title,
        new_title: new_title.to_string(),
        changed: true,
        info_persist_us,
    })
}

fn unique_trash_entry_name(notebook_id: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{notebook_id}__{n}")
}

fn plan_delete_notebook(
    manifest: &PersistedLibraryManifestV1,
    notebook_id: &str,
) -> Result<PersistedLibraryManifestV1, String> {
    if manifest.notebook_ids.len() < 2 {
        return Err("REFUSE NOTEBOOK DELETE: cannot delete the only notebook".into());
    }
    let Some(index) = manifest
        .notebook_ids
        .iter()
        .position(|id| id == notebook_id)
    else {
        return Err(format!(
            "REFUSE NOTEBOOK DELETE: notebook {notebook_id} not in library"
        ));
    };
    let mut next = manifest.clone();
    next.notebook_ids.remove(index);
    if manifest.active_notebook_id == notebook_id {
        let successor_index = index.min(next.notebook_ids.len() - 1);
        next.active_notebook_id = next.notebook_ids[successor_index].clone();
    }
    // Monotonic allocation survives deletion; never fill the hole.
    validate_library_manifest(&next)?;
    Ok(next)
}

pub fn delete_notebook_durable(
    library: &mut LibraryRepository,
    notebook_id: &str,
    hooks: &DeleteNotebookHooks,
) -> Result<DeleteNotebookCommit, String> {
    let planned = plan_delete_notebook(&library.manifest, notebook_id)?;
    let src = library.notebook_dir(notebook_id);
    validate_notebook_directory(&src, notebook_id)?;
    // Held through the quarantine move, which runs after the manifest write's own admit ends.
    let _admit = crate::sync_verified::note_mutation_from_notebook_dir(&src)
        .map_err(|e| format!("REFUSE NOTEBOOK DELETE: {e}"))?;
    let trash = library.root.join(NOTEBOOK_TRASH_DIR);
    fs::create_dir_all(&trash).map_err(|e| format!("REFUSE NOTEBOOK DELETE: create trash: {e}"))?;
    let trash_entry_name = unique_trash_entry_name(notebook_id);
    let dst = trash.join(&trash_entry_name);
    if dst.exists() {
        return Err(format!(
            "REFUSE NOTEBOOK DELETE: trash collision {trash_entry_name}"
        ));
    }

    // Manifest-first: after this succeeds, the deletion is authoritative.
    let t0 = Instant::now();
    write_library_manifest_atomic(&library.root, &planned, &hooks.manifest).map_err(|e| {
        format!(
            "REFUSE NOTEBOOK DELETE: library manifest persist failed (directory untouched): {e}"
        )
    })?;
    let manifest_persist_us = t0.elapsed().as_micros() as u64;
    library.disk_active_notebook_id = planned.active_notebook_id.clone();
    library.manifest = planned;
    library.summaries.remove(notebook_id);

    let quarantine = if hooks.fail_quarantine {
        NotebookQuarantineOutcome::LeftAsOrphan {
            reason: "injected: quarantine move failed".into(),
        }
    } else {
        match fs::rename(&src, &dst) {
            Ok(()) => {
                sync_dir(&trash);
                sync_dir(&library.root.join(NOTEBOOKS_DIR));
                NotebookQuarantineOutcome::Moved { trash_entry_name }
            }
            Err(e) => NotebookQuarantineOutcome::LeftAsOrphan {
                reason: format!("quarantine rename failed: {e}"),
            },
        }
    };
    library.orphan_dirs = list_orphan_notebook_dirs(&library.root, &library.manifest.notebook_ids);
    Ok(DeleteNotebookCommit {
        deleted_notebook_id: notebook_id.to_string(),
        active_notebook_id: library.manifest.active_notebook_id.clone(),
        quarantine,
        manifest_persist_us,
    })
}

// ---- Sync S2C: remote materialization primitives (see `sync_materialize`) ----

/// Next free local notebook id at or after `next_notebook_ordinal`, skipping any directory
/// already on disk (never reuses or overwrites an ordinal).
pub fn allocate_fresh_notebook_id(library: &LibraryRepository) -> String {
    let mut n = library.manifest.next_notebook_ordinal;
    loop {
        let id = format!("notebook-{n}");
        if !library.manifest.notebook_ids.contains(&id) && !library.notebook_dir(&id).exists() {
            return id;
        }
        n = n.saturating_add(1);
    }
}

fn notebook_ordinal(id: &str) -> Option<u64> {
    id.strip_prefix("notebook-")?.parse().ok()
}

/// Move a notebook directory that is NOT in `library.meta` into `.notebook-trash/`.
pub fn quarantine_notebook_dir(
    library: &LibraryRepository,
    notebook_id: &str,
) -> Result<(), String> {
    if library
        .manifest
        .notebook_ids
        .iter()
        .any(|v| v == notebook_id)
    {
        return Err(format!("REFUSE: {notebook_id} is live"));
    }
    let src = library.notebook_dir(notebook_id);
    if !src.exists() {
        return Ok(());
    }
    let _admit = crate::sync_verified::note_mutation_from_notebook_dir(&src)?;
    let trash = library.root.join(NOTEBOOK_TRASH_DIR);
    fs::create_dir_all(&trash).map_err(|e| format!("create trash: {e}"))?;
    fs::rename(&src, trash.join(unique_trash_entry_name(notebook_id)))
        .map_err(|e| format!("quarantine {notebook_id}: {e}"))
}

/// Create notebook `notebook_id` (title + one page with exact background) and insert it at
/// `index` in library order. The active notebook is unchanged (replica-local UI state).
#[allow(clippy::too_many_arguments)]
pub fn create_notebook_at_durable(
    library: &mut LibraryRepository,
    notebook_id: &str,
    title: &str,
    index: usize,
    first_page_id: &str,
    dims: (u32, u32, u32),
    background: &[u8],
    profile: &RenderProfile,
) -> Result<(), String> {
    validate_title(title)?;
    if library
        .manifest
        .notebook_ids
        .iter()
        .any(|v| v == notebook_id)
    {
        return Err(format!(
            "REFUSE NOTEBOOK CREATE: {notebook_id} already in library"
        ));
    }
    let dir = library.notebook_dir(notebook_id);
    if dir.exists() {
        // Leftover of an interrupted create: never attach unknown content.
        quarantine_notebook_dir(library, notebook_id)?;
    }
    crate::notebook::create_notebook_dir_with_page(
        &dir,
        notebook_id,
        first_page_id,
        dims,
        background,
        profile,
    )?;
    write_notebook_info(&dir, title)?;
    let mut planned = library.manifest.clone();
    planned.notebook_ids.insert(
        index.min(planned.notebook_ids.len()),
        notebook_id.to_string(),
    );
    if let Some(n) = notebook_ordinal(notebook_id) {
        planned.next_notebook_ordinal = planned.next_notebook_ordinal.max(n.saturating_add(1));
    }
    validate_library_manifest(&planned)?;
    write_library_manifest_atomic(
        &library.root,
        &planned,
        &LibraryManifestWriteHooks::default(),
    )?;
    library.disk_active_notebook_id = planned.active_notebook_id.clone();
    library.manifest = planned;
    library.summaries.insert(
        notebook_id.to_string(),
        NotebookHomeSummary {
            title: title.to_string(),
            page_count: Some(1),
        },
    );
    library.orphan_dirs = list_orphan_notebook_dirs(&library.root, &library.manifest.notebook_ids);
    Ok(())
}

/// Replace library notebook order with `order`, a permutation of the current notebooks.
pub fn reorder_notebooks_durable(
    library: &mut LibraryRepository,
    order: &[String],
) -> Result<(), String> {
    let mut a = library.manifest.notebook_ids.clone();
    let mut b = order.to_vec();
    a.sort();
    b.sort();
    if a != b {
        return Err("REFUSE NOTEBOOK REORDER: not a permutation of the library".into());
    }
    if library.manifest.notebook_ids == order {
        return Ok(());
    }
    let mut planned = library.manifest.clone();
    planned.notebook_ids = order.to_vec();
    validate_library_manifest(&planned)?;
    write_library_manifest_atomic(
        &library.root,
        &planned,
        &LibraryManifestWriteHooks::default(),
    )?;
    library.disk_active_notebook_id = planned.active_notebook_id.clone();
    library.manifest = planned;
    Ok(())
}

/// What an older build's notebook switch does: rewrite `library.meta` (admitted mutation).
#[cfg(test)]
pub(crate) fn write_library_manifest_for_test(
    root: &Path,
    manifest: &PersistedLibraryManifestV1,
) -> Result<(), String> {
    write_library_manifest_atomic(root, manifest, &LibraryManifestWriteHooks::default())
}

#[cfg(test)]
pub(crate) fn read_library_manifest_for_test(
    root: &Path,
) -> Result<PersistedLibraryManifestV1, String> {
    read_library_manifest(root)
}

#[cfg(test)]
pub(crate) fn list_orphans_for_test(root: &Path, ids: &[String]) -> Vec<String> {
    list_orphan_notebook_dirs(root, ids)
}
