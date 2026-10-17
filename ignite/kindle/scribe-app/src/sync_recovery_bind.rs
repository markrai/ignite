//! Disposable unbound-page identity preparation (Slice 18E.6D).
//!
//! Uses production [`plan_sync_identity`] / [`persist_planned_identity`]. Never mints a
//! library or replica ID. Writes only when the caller points at an independent disposable
//! tree; protected acquisitions are refused.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use sync_protocol::{IdentityFileV1, PageId, Reconciled, SnapshotPage, SyncSnapshotV1};

use crate::store::page_dir;
use crate::sync_local::{
    persist_planned_identity_with, plan_sync_identity, read_identity, SyncWriteHooks,
    IDENTITY_FILE, SYNC_DIR,
};
use crate::sync_recovery_compare::inspect_identity_read_only;
use crate::sync_recovery_extract::file_sha256;

/// Substring that a write destination must contain (Review 96 disposable tree).
pub const DISPOSABLE_MARK: &str = "18E6D";

#[derive(Clone, Debug)]
pub struct BindReport {
    pub unbound_before: usize,
    pub allocated_pages: usize,
    pub existing_page_bindings_preserved: bool,
    pub retired_preserved: bool,
    pub library_preserved: bool,
    pub notebooks_preserved: bool,
    pub identity_path: PathBuf,
}

/// Refuse writes into protected acquisitions or anywhere other than an `18E6D` work tree.
pub fn require_disposable_bind_root(path: &Path) -> Result<(), String> {
    let s = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    if s.contains("ignite-divergence-analysis-18d3\\reference")
        || s.contains("ignite-currentstate-18e6c")
        || s.contains("ignite-livetail-18e6b")
        || s.contains("c:\\dev\\project\\ignite\\")
        || s.contains("c:\\dev\\project\\scribe\\")
    {
        return Err(format!(
            "REFUSE IDENTITY BIND: {} is a protected or product path",
            path.display()
        ));
    }
    if !s.contains("18e6d")
        && !s.contains("18e7")
        && !s.contains("18e9a")
        && !s.contains("18e9b")
        && !s.contains("18e9c")
    {
        return Err(format!(
            "REFUSE IDENTITY BIND: {} is not under a disposable {DISPOSABLE_MARK}, 18E7, 18E9A, 18E9B, or 18E9C tree",
            path.display()
        ));
    }
    Ok(())
}

fn page_ids_of(id: &IdentityFileV1) -> BTreeSet<PageId> {
    id.pages
        .values()
        .flat_map(|m| m.values().copied())
        .collect()
}

fn notebook_ids_of(id: &IdentityFileV1) -> BTreeMap<String, sync_protocol::NotebookId> {
    id.notebooks.clone()
}

fn live_page_keys(id: &IdentityFileV1) -> BTreeSet<(String, String, PageId)> {
    let mut s = BTreeSet::new();
    for (nb, pages) in &id.pages {
        for (local, pid) in pages {
            s.insert((nb.clone(), local.clone(), *pid));
        }
    }
    s
}

pub(crate) fn checkpoint_collision(
    plan: &Reconciled,
    checkpoint_pages: &BTreeSet<PageId>,
) -> Result<(), String> {
    for b in &plan.allocated_pages {
        if checkpoint_pages.contains(&b.sync_id) {
            return Err(format!(
                "REFUSE IDENTITY BIND: allocated page collides with checkpoint {}",
                b.sync_id
            ));
        }
        if plan.identity.retired_pages.contains_key(&b.sync_id) {
            return Err("REFUSE IDENTITY BIND: allocated page collides with a retired id".into());
        }
    }
    Ok(())
}

/// Plan + persist bindings for currently unbound live pages. `allow_protected` is only for
/// synthetic temp-dir tests; production rehearsal must pass `false`.
pub fn prepare_unbound_page_identities(
    library_root: &Path,
    checkpoint_pages: &BTreeSet<PageId>,
    require_disposable: bool,
    hooks: &SyncWriteHooks,
) -> Result<BindReport, String> {
    if require_disposable {
        require_disposable_bind_root(library_root)?;
    }
    let inspection = inspect_identity_read_only(library_root)?;
    let before = read_identity(library_root)?
        .ok_or_else(|| "REFUSE IDENTITY BIND: missing identity.json".to_string())?;
    if inspection.unbound_pages.is_empty() {
        if inspection.complete() {
            return Ok(BindReport {
                unbound_before: 0,
                allocated_pages: 0,
                existing_page_bindings_preserved: true,
                retired_preserved: true,
                library_preserved: true,
                notebooks_preserved: true,
                identity_path: library_root.join(SYNC_DIR).join(IDENTITY_FILE),
            });
        }
        return Err("REFUSE IDENTITY BIND: no unbound live pages".into());
    }
    let before_pages = live_page_keys(&before);
    let before_nbs = notebook_ids_of(&before);
    let before_retired_n = before.retired_notebooks.clone();
    let before_retired_p = before.retired_pages.clone();
    let before_lib = before.library_id;

    let (from, plan) = plan_sync_identity(library_root)?;
    if from.as_ref() != Some(&before) {
        return Err("REFUSE IDENTITY BIND: identity changed while inspecting".into());
    }
    if plan.allocated_library {
        return Err("REFUSE IDENTITY BIND: allocator would mint a library id".into());
    }
    if !plan.allocated_notebooks.is_empty() {
        return Err("REFUSE IDENTITY BIND: allocator would mint notebook ids".into());
    }
    if !plan.retired_notebooks.is_empty() || !plan.retired_pages.is_empty() {
        return Err("REFUSE IDENTITY BIND: allocator would retire existing bindings".into());
    }
    if plan.allocated_pages.len() != inspection.unbound_pages.len() {
        return Err(format!(
            "REFUSE IDENTITY BIND: expected {} new page ids, plan has {}",
            inspection.unbound_pages.len(),
            plan.allocated_pages.len()
        ));
    }
    if plan.identity.library_id != before_lib {
        return Err("REFUSE IDENTITY BIND: library id would change".into());
    }
    if notebook_ids_of(&plan.identity) != before_nbs {
        return Err("REFUSE IDENTITY BIND: notebook bindings would change".into());
    }
    for key in &before_pages {
        if !live_page_keys(&plan.identity).contains(key) {
            return Err("REFUSE IDENTITY BIND: an existing page binding would change".into());
        }
    }
    if plan.identity.retired_notebooks != before_retired_n
        || plan.identity.retired_pages != before_retired_p
    {
        return Err("REFUSE IDENTITY BIND: retired identities would change".into());
    }
    checkpoint_collision(&plan, checkpoint_pages)?;
    persist_planned_identity_with(library_root, from.as_ref(), &plan, hooks)?;

    let after = read_identity(library_root)?
        .ok_or_else(|| "REFUSE IDENTITY BIND: identity.json missing after persist".to_string())?;
    let after_insp = inspect_identity_read_only(library_root)?;
    if !after_insp.complete() {
        return Err("REFUSE IDENTITY BIND: live pages still unbound after persist".into());
    }
    Ok(BindReport {
        unbound_before: inspection.unbound_pages.len(),
        allocated_pages: plan.allocated_pages.len(),
        existing_page_bindings_preserved: before_pages.is_subset(&live_page_keys(&after)),
        retired_preserved: after.retired_notebooks == before_retired_n
            && after.retired_pages == before_retired_p,
        library_preserved: after.library_id == before_lib,
        notebooks_preserved: notebook_ids_of(&after) == before_nbs,
        identity_path: library_root.join(SYNC_DIR).join(IDENTITY_FILE),
    })
}

/// SHA-256 of every file in a page directory (canonical sources: meta, journals, background,
/// texts, links). Missing files are omitted, not invented.
pub fn page_source_census(
    nb_dir: &Path,
    page_local: &str,
) -> Result<BTreeMap<String, String>, String> {
    let dir = page_dir(nb_dir, page_local);
    let mut out = BTreeMap::new();
    if !dir.is_dir() {
        return Err(format!("missing page dir {}", dir.display()));
    }
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        if let Some(h) = file_sha256(&dir.join(&name))? {
            out.insert(name, h);
        }
    }
    Ok(out)
}

/// SHA-256 of every file under `root` (relative POSIX-style paths).
pub fn file_tree_census(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<(), String> {
        for ent in std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
            let ent = ent.map_err(|e| e.to_string())?;
            let p = ent.path();
            if p.is_dir() {
                walk(root, &p, out)?;
            } else if p.is_file() {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                if let Some(h) = file_sha256(&p)? {
                    out.insert(rel, h);
                }
            }
        }
        Ok(())
    }
    walk(root, root, &mut out)?;
    Ok(out)
}

/// Notebook-content files that identity preparation must not rewrite.
pub fn notebook_content_census(library_root: &Path) -> Result<BTreeMap<String, String>, String> {
    Ok(file_tree_census(library_root)?
        .into_iter()
        .filter(|(rel, _)| !rel.contains("/.scribe-sync/") && !rel.starts_with(".scribe-sync/"))
        .collect())
}

/// Bound pages match including IDs; newly bound pages match all fields except `page_id`.
pub fn snapshots_equal_after_binding(
    before_bound: &SyncSnapshotV1,
    after_full: &SyncSnapshotV1,
    new_page_ids: &BTreeSet<PageId>,
) -> Result<(), String> {
    if after_full.library_id != before_bound.library_id {
        return Err("library id changed".into());
    }
    if after_full.notebooks.len() != before_bound.notebooks.len() {
        return Err("notebook count changed".into());
    }
    for (bn, an) in before_bound.notebooks.iter().zip(&after_full.notebooks) {
        if bn.notebook_id != an.notebook_id || bn.title != an.title || bn.folder_id != an.folder_id
        {
            return Err("notebook metadata changed".into());
        }
        let mut a_pages: BTreeMap<PageId, &SnapshotPage> =
            an.pages.iter().map(|p| (p.page_id, p)).collect();
        for bp in &bn.pages {
            let ap = a_pages
                .remove(&bp.page_id)
                .ok_or_else(|| "existing page id missing after bind".to_string())?;
            if ap != bp {
                return Err("existing bound page content changed".into());
            }
        }
        for (pid, ap) in a_pages {
            if !new_page_ids.contains(&pid) {
                return Err("unexpected extra page id after bind".into());
            }
            if ap.page_id != pid {
                return Err("new page id mismatch".into());
            }
        }
    }
    Ok(())
}

/// Production journal + page-directory census for one live page. Used to prove the two
/// historically unbound pages match between protected inputs before any disposable bind.
pub fn complete_page_record_match(
    a_lib: &Path,
    b_lib: &Path,
    notebook_local: &str,
    page_local: &str,
) -> Result<(usize, bool), String> {
    let a_nb = a_lib.join("notebooks").join(notebook_local);
    let b_nb = b_lib.join("notebooks").join(notebook_local);
    let ja = crate::store::load_page_journals_read_only(&a_nb, page_local)?;
    let jb = crate::store::load_page_journals_read_only(&b_nb, page_local)?;
    if ja.header != jb.header {
        return Err("page.meta differs".into());
    }
    if ja.strokes != jb.strokes {
        return Err("replayed strokes differ".into());
    }
    if ja.root_of != jb.root_of {
        return Err("root lineage differs".into());
    }
    if ja.render_profile != jb.render_profile {
        return Err("render profile differs".into());
    }
    let a_dir = page_dir(&a_nb, page_local);
    let b_dir = page_dir(&b_nb, page_local);
    if crate::store::load_page_texts(&a_dir)? != crate::store::load_page_texts(&b_dir)? {
        return Err("texts differ".into());
    }
    if crate::store::load_page_links(&a_dir)? != crate::store::load_page_links(&b_dir)? {
        return Err("links differ".into());
    }
    let ca = page_source_census(&a_nb, page_local)?;
    let cb = page_source_census(&b_nb, page_local)?;
    if ca != cb {
        return Err("page directory census differs".into());
    }
    Ok((ja.strokes.len(), ja.strokes.is_empty()))
}

pub fn new_page_ids(before: &IdentityFileV1, after: &IdentityFileV1) -> BTreeSet<PageId> {
    page_ids_of(after)
        .difference(&page_ids_of(before))
        .copied()
        .collect()
}

#[cfg(test)]
#[path = "sync_recovery_bind_tests.rs"]
mod tests;
