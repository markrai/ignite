//! Replica-local page navigation: the page each notebook last showed (19B.3D.1).
//!
//! Stored in `<library>/.scribe-sync/navigation-v1.json`, beside the mutation generation and the
//! page ledger and outside the verified-stamp census. It is a convenience preference, not
//! notebook content: never synchronized, never admitted as a library mutation (no generation
//! bump, no ledger record, no stamp invalidation), and written temp → rename without fsync. A
//! crash can lose the last selection; it can never touch a notebook.
//!
//! `notebook.meta` keeps its canonical `active_page_id`, which older builds still rewrite on
//! every page switch and which page create / insert / delete still set. Each entry records the
//! canonical value it was written against (`base`); when `notebook.meta` names a different page
//! the canonical choice is newer and wins. Missing, unreadable, malformed or stale state falls
//! back to `notebook.meta`, and nothing here ever rewrites a notebook to correct it.
//!
//! The notebook chosen on Home (19B.3D.2) follows the same rule in its own file,
//! `<library>/.scribe-sync/active-notebook-v1.json`, against `library.meta`'s
//! `active_notebook_id`. A separate file keeps `navigation-v1.json` readable by 19B.3D.1, whose
//! strict parser would discard every page entry on an unknown field.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::library::PersistedLibraryManifestV1;
use crate::notebook::PersistedNotebookManifestV1;
use crate::sync_local::SYNC_DIR;

pub const NAV_FILE: &str = "navigation-v1.json";
const NAV_VERSION: u32 = 1;
/// Larger files are ignored (and replaced on the next write).
const NAV_MAX_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NavFile {
    v: u32,
    notebooks: BTreeMap<String, NavEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NavEntry {
    /// Page last shown.
    page: String,
    /// `notebook.meta` `active_page_id` when `page` was chosen.
    base: String,
}

pub const ACTIVE_NOTEBOOK_FILE: &str = "active-notebook-v1.json";
const ACTIVE_NOTEBOOK_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveNotebookFile {
    v: u32,
    /// Notebook last opened.
    notebook: String,
    /// `library.meta` `active_notebook_id` when `notebook` was chosen.
    base: String,
}

/// Serializes read-modify-write of the file within the process (the app is a singleton).
static WRITE: Mutex<()> = Mutex::new(());

/// `<library>/.scribe-sync/navigation-v1.json`, or `None` for a notebook outside a library.
pub fn nav_path(notebook_dir: &Path) -> Option<PathBuf> {
    let root = crate::sync_verified::library_root_from_notebook_dir(notebook_dir)?;
    Some(root.join(SYNC_DIR).join(NAV_FILE))
}

fn read(path: &Path) -> NavFile {
    let usable = fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() <= NAV_MAX_BYTES);
    let parsed = usable
        .then(|| fs::read(path).ok())
        .flatten()
        .and_then(|raw| serde_json::from_slice::<NavFile>(&raw).ok());
    match parsed {
        Some(f) if f.v == NAV_VERSION => f,
        _ => NavFile {
            v: NAV_VERSION,
            ..NavFile::default()
        },
    }
}

/// The page to show when `manifest`'s notebook opens: the remembered page while it still exists
/// and `notebook.meta` has not changed its own choice since, else `active_page_id`.
pub fn selected_page(notebook_dir: &Path, manifest: &PersistedNotebookManifestV1) -> String {
    let canonical = &manifest.active_page_id;
    let Some(path) = nav_path(notebook_dir) else {
        return canonical.clone();
    };
    match read(&path).notebooks.remove(&manifest.notebook_id) {
        Some(e) if e.base == *canonical && manifest.page_ids.contains(&e.page) => e.page,
        _ => canonical.clone(),
    }
}

/// Remember `page_id` as shown in `manifest`'s notebook. Entries for notebooks that no longer
/// exist are dropped. `Ok(false)` for a notebook outside a library (nothing to write).
pub fn remember(
    notebook_dir: &Path,
    manifest: &PersistedNotebookManifestV1,
    page_id: &str,
) -> Result<bool, String> {
    let Some(path) = nav_path(notebook_dir) else {
        return Ok(false);
    };
    let Some(sync_dir) = path.parent() else {
        return Ok(false);
    };
    let notebooks_dir = sync_dir
        .parent()
        .map(|root| root.join(crate::library::NOTEBOOKS_DIR));
    let _w = WRITE.lock().unwrap_or_else(|p| p.into_inner());
    let mut f = read(&path);
    f.notebooks.retain(|id, _| {
        !id.contains(['/', '\\']) && notebooks_dir.as_ref().is_some_and(|d| d.join(id).is_dir())
    });
    f.notebooks.insert(
        manifest.notebook_id.clone(),
        NavEntry {
            page: page_id.to_string(),
            base: manifest.active_page_id.clone(),
        },
    );
    publish(sync_dir, NAV_FILE, &f)?;
    Ok(true)
}

/// Temp → rename, no fsync (convenience state). Caller holds [`WRITE`].
fn publish<T: Serialize>(sync_dir: &Path, name: &str, value: &T) -> Result<(), String> {
    let mut json = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    json.push(b'\n');
    fs::create_dir_all(sync_dir).map_err(|e| format!("create {}: {e}", sync_dir.display()))?;
    let tmp = sync_dir.join(format!("{name}.tmp"));
    let path = sync_dir.join(name);
    fs::write(&tmp, &json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| format!("rename {}: {e}", path.display()))
}

/// `<library>/.scribe-sync/active-notebook-v1.json`.
pub fn active_notebook_path(library_root: &Path) -> PathBuf {
    library_root.join(SYNC_DIR).join(ACTIVE_NOTEBOOK_FILE)
}

/// The notebook to treat as open: the remembered one while it is still in the library and
/// `library.meta` has not changed its own choice since, else `active_notebook_id`.
pub fn selected_notebook(library_root: &Path, manifest: &PersistedLibraryManifestV1) -> String {
    let canonical = &manifest.active_notebook_id;
    let path = active_notebook_path(library_root);
    let usable = fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() <= NAV_MAX_BYTES);
    let parsed = usable
        .then(|| fs::read(&path).ok())
        .flatten()
        .and_then(|raw| serde_json::from_slice::<ActiveNotebookFile>(&raw).ok());
    match parsed {
        Some(f)
            if f.v == ACTIVE_NOTEBOOK_VERSION
                && f.base == *canonical
                && manifest.notebook_ids.contains(&f.notebook) =>
        {
            f.notebook
        }
        _ => canonical.clone(),
    }
}

/// Remember `notebook_id` as opened, against `canonical` (`library.meta`'s
/// `active_notebook_id` as it is on disk now).
pub fn remember_notebook(
    library_root: &Path,
    canonical: &str,
    notebook_id: &str,
) -> Result<(), String> {
    let _w = WRITE.lock().unwrap_or_else(|p| p.into_inner());
    publish(
        &library_root.join(SYNC_DIR),
        ACTIVE_NOTEBOOK_FILE,
        &ActiveNotebookFile {
            v: ACTIVE_NOTEBOOK_VERSION,
            notebook: notebook_id.to_string(),
            base: canonical.to_string(),
        },
    )
}
