//! Gate F2 notebook manifest: ordered pages + durable active page.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kindle_platform::FbGeometry;
use serde::{Deserialize, Serialize};

use crate::ink::{ink_for_new_page, resolve_existing_ink, InkRequest};
use crate::page_label::scrub_chrome_band;
use crate::profile::RenderProfile;
use crate::store::{
    create_new_page, install_background, load_existing_page, migrate_all_pages, page_dir,
    page_exists, validate_live_framebuffer, FsPageStore, LoadedPage, DEFAULT_PAGE_ID,
};

pub const NOTEBOOK_MANIFEST_VERSION: u32 = 1;
pub const DEFAULT_NOTEBOOK_ID: &str = "notebook-1";
pub const MANIFEST_FILE: &str = "notebook.meta";
/// Recoverable quarantine for deleted page directories (not a notebook page).
pub const PAGE_TRASH_DIR: &str = ".page-trash";
/// Developer trigger file: when present and session idle, delete the current page (F8).
pub const DEV_DELETE_CURRENT_PAGE_TRIGGER: &str = ".scribe-dev-delete-current-page";
/// F9: move current page one slot left in `page_ids`.
pub const DEV_MOVE_PAGE_LEFT_TRIGGER: &str = ".scribe-dev-move-page-left";
/// F9: move current page one slot right in `page_ids`.
pub const DEV_MOVE_PAGE_RIGHT_TRIGGER: &str = ".scribe-dev-move-page-right";
/// F9: insert a blank page after the current page.
pub const DEV_INSERT_PAGE_AFTER_TRIGGER: &str = ".scribe-dev-insert-page-after";

/// Notebook-structure developer ops (F8 delete + F9 order/insert).
///
/// Poll priority when multiple trigger files exist (highest first):
/// 1. move-left
/// 2. move-right
/// 3. insert-after
/// 4. delete-current
///
/// At most one op is selected per poll; the matching file is consumed only when eligible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevNotebookOp {
    MoveLeft,
    MoveRight,
    InsertAfter,
    DeleteCurrent,
}

/// Peek the highest-priority pending notebook-structure trigger (does not consume).
pub fn peek_dev_notebook_op(data_dir: &Path) -> Option<DevNotebookOp> {
    if data_dir.join(DEV_MOVE_PAGE_LEFT_TRIGGER).exists() {
        Some(DevNotebookOp::MoveLeft)
    } else if data_dir.join(DEV_MOVE_PAGE_RIGHT_TRIGGER).exists() {
        Some(DevNotebookOp::MoveRight)
    } else if data_dir.join(DEV_INSERT_PAGE_AFTER_TRIGGER).exists() {
        Some(DevNotebookOp::InsertAfter)
    } else if data_dir.join(DEV_DELETE_CURRENT_PAGE_TRIGGER).exists() {
        Some(DevNotebookOp::DeleteCurrent)
    } else {
        None
    }
}

/// Consume the trigger file for `op` (one-shot).
pub fn consume_dev_notebook_op(data_dir: &Path, op: DevNotebookOp) {
    let name = match op {
        DevNotebookOp::MoveLeft => DEV_MOVE_PAGE_LEFT_TRIGGER,
        DevNotebookOp::MoveRight => DEV_MOVE_PAGE_RIGHT_TRIGGER,
        DevNotebookOp::InsertAfter => DEV_INSERT_PAGE_AFTER_TRIGGER,
        DevNotebookOp::DeleteCurrent => DEV_DELETE_CURRENT_PAGE_TRIGGER,
    };
    let _ = fs::remove_file(data_dir.join(name));
}

/// True when the developer delete-trigger file exists (does not consume it).
#[cfg(test)]
pub fn delete_trigger_pending(data_dir: &Path) -> bool {
    data_dir.join(DEV_DELETE_CURRENT_PAGE_TRIGGER).exists()
}

/// Remove the developer delete-trigger file (one-shot consume).
#[cfg(test)]
pub fn consume_delete_trigger(data_dir: &Path) {
    consume_dev_notebook_op(data_dir, DevNotebookOp::DeleteCurrent);
}

/// Create the developer delete-trigger file (tests / operator tooling).
#[cfg(test)]
pub fn arm_delete_trigger(data_dir: &Path) -> Result<(), String> {
    arm_dev_notebook_op(data_dir, DevNotebookOp::DeleteCurrent)
}

#[cfg(test)]
pub fn arm_dev_notebook_op(data_dir: &Path, op: DevNotebookOp) -> Result<(), String> {
    let name = match op {
        DevNotebookOp::MoveLeft => DEV_MOVE_PAGE_LEFT_TRIGGER,
        DevNotebookOp::MoveRight => DEV_MOVE_PAGE_RIGHT_TRIGGER,
        DevNotebookOp::InsertAfter => DEV_INSERT_PAGE_AFTER_TRIGGER,
        DevNotebookOp::DeleteCurrent => DEV_DELETE_CURRENT_PAGE_TRIGGER,
    };
    fs::write(data_dir.join(name), b"1\n").map_err(|e| e.to_string())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedNotebookManifestV1 {
    pub v: u32,
    pub notebook_id: String,
    pub page_ids: Vec<String>,
    pub active_page_id: String,
    pub next_page_ordinal: u64,
}

pub struct NotebookOpen {
    pub manifest: PersistedNotebookManifestV1,
    /// The page loaded: the replica-local selection ([`crate::nav_state`]) when still valid,
    /// else `manifest.active_page_id`.
    pub page_id: String,
    pub store: FsPageStore,
    pub loaded: LoadedPage,
    pub orphan_dirs: Vec<String>,
    pub adopted_from_existing_page: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageSwitchDirection {
    Prev,
    Next,
    New,
}

fn manifest_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MANIFEST_FILE)
}

fn write_manifest_atomic(data_dir: &Path, m: &PersistedNotebookManifestV1) -> Result<(), String> {
    write_manifest_atomic_with(data_dir, m, &ManifestWriteHooks::default())
}

/// Fault-injection hooks for host tests of manifest persistence.
#[derive(Clone, Debug, Default)]
pub struct ManifestWriteHooks {
    pub fail_write: bool,
    pub fail_fsync: bool,
    pub fail_rename: bool,
}

pub fn write_manifest_atomic_with(
    data_dir: &Path,
    m: &PersistedNotebookManifestV1,
    hooks: &ManifestWriteHooks,
) -> Result<(), String> {
    fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let _admit = crate::sync_verified::note_mutation_from_notebook_dir(data_dir)?;
    let final_p = manifest_path(data_dir);
    let tmp = data_dir.join("notebook.meta.tmp");
    let json = serde_json::to_string_pretty(m).map_err(|e| e.to_string())?;
    {
        if hooks.fail_write {
            return Err("injected: manifest write failed".into());
        }
        let mut f = File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
        f.write_all(b"\n").map_err(|e| e.to_string())?;
        if hooks.fail_fsync {
            let _ = fs::remove_file(&tmp);
            return Err("injected: manifest fsync failed".into());
        }
        f.sync_all().map_err(|e| e.to_string())?;
    }
    if hooks.fail_rename {
        let _ = fs::remove_file(&tmp);
        return Err("injected: manifest rename failed".into());
    }
    fs::rename(&tmp, &final_p).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        if let Ok(dirf) = File::open(data_dir) {
            let _ = dirf.sync_all();
        }
    }
    Ok(())
}

pub fn validate_manifest(m: &PersistedNotebookManifestV1) -> Result<(), String> {
    if m.v != NOTEBOOK_MANIFEST_VERSION {
        return Err(format!(
            "REFUSE NOTEBOOK OPEN: unsupported manifest version {} (need {NOTEBOOK_MANIFEST_VERSION})",
            m.v
        ));
    }
    if m.page_ids.is_empty() {
        return Err("REFUSE NOTEBOOK OPEN: empty page_ids".into());
    }
    let mut seen = HashSet::new();
    for id in &m.page_ids {
        if id.is_empty() {
            return Err("REFUSE NOTEBOOK OPEN: empty page id".into());
        }
        if !seen.insert(id.clone()) {
            return Err(format!("REFUSE NOTEBOOK OPEN: duplicate page id {id}"));
        }
    }
    if !seen.contains(&m.active_page_id) {
        return Err(format!(
            "REFUSE NOTEBOOK OPEN: active_page_id {} not in page_ids",
            m.active_page_id
        ));
    }
    if m.next_page_ordinal == 0 {
        return Err("REFUSE NOTEBOOK OPEN: next_page_ordinal must be >= 1".into());
    }
    // Collision: proposed next id already present.
    let next_id = format!("page-{}", m.next_page_ordinal);
    if seen.contains(&next_id) {
        return Err(format!(
            "REFUSE NOTEBOOK OPEN: next_page_ordinal {} collides with existing {next_id}",
            m.next_page_ordinal
        ));
    }
    Ok(())
}

fn list_orphan_page_dirs(data_dir: &Path, page_ids: &[String]) -> Vec<String> {
    let Ok(rd) = fs::read_dir(data_dir) else {
        return Vec::new();
    };
    let known: HashSet<&str> = page_ids.iter().map(|s| s.as_str()).collect();
    let mut orphans = Vec::new();
    for ent in rd.flatten() {
        let Ok(ft) = ent.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let name = ent.file_name().to_string_lossy().into_owned();
        // Hidden dirs (including `.page-trash`) are never notebook pages.
        if name.starts_with('.') {
            continue;
        }
        if name.starts_with("page-") && !known.contains(name.as_str()) {
            orphans.push(name);
        }
    }
    orphans.sort();
    orphans
}

#[cfg(test)]
pub(crate) fn list_orphan_page_dirs_for_test(data_dir: &Path, page_ids: &[String]) -> Vec<String> {
    list_orphan_page_dirs(data_dir, page_ids)
}

fn ensure_referenced_pages_exist(
    data_dir: &Path,
    m: &PersistedNotebookManifestV1,
) -> Result<(), String> {
    for id in &m.page_ids {
        if !page_exists(data_dir, id) {
            return Err(format!(
                "REFUSE NOTEBOOK OPEN: manifest references missing page directory {id}"
            ));
        }
    }
    Ok(())
}

/// Scrub baked page-chrome out of `loaded.background` and rewrite durable files if needed.
pub fn repair_background_chrome(
    data_dir: &Path,
    page_id: &str,
    loaded: &mut LoadedPage,
) -> Result<bool, String> {
    let geo = FbGeometry {
        xres: loaded.header.panel_width,
        yres: loaded.header.panel_height,
        xres_virtual: loaded.header.panel_width,
        yres_virtual: loaded.header.panel_height,
        xoffset: 0,
        yoffset: 0,
        bits_per_pixel: 8,
        line_length: loaded.header.line_length,
        smem_len: loaded
            .header
            .line_length
            .saturating_mul(loaded.header.panel_height),
    };
    let changed = scrub_chrome_band(&mut loaded.background, &geo)?;
    if !changed {
        return Ok(false);
    }
    let dir = page_dir(data_dir, page_id);
    install_background(&dir, &loaded.background, &mut loaded.header)?;
    eprintln!("repaired: scrubbed baked chrome from background page={page_id}");
    Ok(true)
}

/// Open notebook: adopt F1.1 page-1, create fresh, or load existing manifest.
pub fn open_or_create_notebook(
    data_dir: &Path,
    ink_request: InkRequest,
    fb_geo: &FbGeometry,
    background_for_new: &[u8],
) -> Result<NotebookOpen, String> {
    fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let man_p = manifest_path(data_dir);
    if man_p.exists() {
        let raw = fs::read_to_string(&man_p).map_err(|e| e.to_string())?;
        let manifest: PersistedNotebookManifestV1 = serde_json::from_str(&raw)
            .map_err(|e| format!("REFUSE NOTEBOOK OPEN: bad notebook.meta: {e}"))?;
        validate_manifest(&manifest)?;
        ensure_referenced_pages_exist(data_dir, &manifest)?;
        // Format migration is meta-only and must cover every referenced page so
        // sync/read-only paths see current format after open. Journal/background
        // load and chrome scrub run per page when that page is shown
        // (`load_page_for_switch` / active-page load below).
        migrate_all_pages(data_dir, &manifest.page_ids)?;
        let orphans = list_orphan_page_dirs(data_dir, &manifest.page_ids);
        for o in &orphans {
            eprintln!("warning: orphan page directory ignored (not in manifest): {o}");
        }
        let page_id = crate::nav_state::selected_page(data_dir, &manifest);
        let (store, mut loaded) = load_existing_page(data_dir, &page_id)?;
        repair_background_chrome(data_dir, &page_id, &mut loaded)?;
        resolve_existing_ink(ink_request, loaded.render_profile.ink_byte)?;
        validate_live_framebuffer(&loaded.header, fb_geo)?;
        return Ok(NotebookOpen {
            manifest,
            page_id,
            store,
            loaded,
            orphan_dirs: orphans,
            adopted_from_existing_page: false,
        });
    }

    // No manifest: adopt lone F1.1 page-1, or create fresh page-1.
    if page_exists(data_dir, DEFAULT_PAGE_ID) {
        let (store, mut loaded) = load_existing_page(data_dir, DEFAULT_PAGE_ID)?;
        repair_background_chrome(data_dir, DEFAULT_PAGE_ID, &mut loaded)?;
        resolve_existing_ink(ink_request, loaded.render_profile.ink_byte)?;
        validate_live_framebuffer(&loaded.header, fb_geo)?;
        let manifest = PersistedNotebookManifestV1 {
            v: NOTEBOOK_MANIFEST_VERSION,
            notebook_id: DEFAULT_NOTEBOOK_ID.into(),
            page_ids: vec![DEFAULT_PAGE_ID.into()],
            active_page_id: DEFAULT_PAGE_ID.into(),
            next_page_ordinal: 2,
        };
        validate_manifest(&manifest)?;
        write_manifest_atomic(data_dir, &manifest)?;
        println!("notebook_manifest_created_from_existing_page=true");
        let orphans = list_orphan_page_dirs(data_dir, &manifest.page_ids);
        for o in &orphans {
            eprintln!("warning: orphan page directory ignored (not in manifest): {o}");
        }
        return Ok(NotebookOpen {
            manifest,
            page_id: DEFAULT_PAGE_ID.into(),
            store,
            loaded,
            orphan_dirs: orphans,
            adopted_from_existing_page: true,
        });
    }

    // Fresh notebook.
    let ink = ink_for_new_page(ink_request);
    let profile = RenderProfile::sealed_v2_pressure(ink)?;
    let mut bg = background_for_new.to_vec();
    scrub_chrome_band(&mut bg, fb_geo)?;
    let (store, loaded) = create_new_page(
        data_dir,
        DEFAULT_PAGE_ID,
        fb_geo.xres,
        fb_geo.yres,
        fb_geo.line_length,
        &bg,
        &profile,
    )?;
    let manifest = PersistedNotebookManifestV1 {
        v: NOTEBOOK_MANIFEST_VERSION,
        notebook_id: DEFAULT_NOTEBOOK_ID.into(),
        page_ids: vec![DEFAULT_PAGE_ID.into()],
        active_page_id: DEFAULT_PAGE_ID.into(),
        next_page_ordinal: 2,
    };
    write_manifest_atomic(data_dir, &manifest)?;
    Ok(NotebookOpen {
        manifest,
        page_id: DEFAULT_PAGE_ID.into(),
        store,
        loaded,
        orphan_dirs: Vec::new(),
        adopted_from_existing_page: false,
    })
}

/// Create a brand-new notebook directory with one current-format blank page.
///
/// F10 owns notebook selection above this function. This page-engine helper deliberately
/// knows only the notebook-local root and the persistent notebook id. The caller is
/// responsible for attaching the directory to `library.meta` (or cleaning it up) after
/// this function succeeds.
pub fn create_notebook_files_with_profile(
    data_dir: &Path,
    notebook_id: &str,
    profile: &RenderProfile,
    fb_geo: &FbGeometry,
    background: &[u8],
) -> Result<(), String> {
    if data_dir.exists() {
        return Err(format!(
            "REFUSE NOTEBOOK CREATE: target directory already exists ({})",
            data_dir.display()
        ));
    }
    fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let mut bg = background.to_vec();
    scrub_chrome_band(&mut bg, fb_geo)?;
    let (store, _loaded) = create_new_page(
        data_dir,
        DEFAULT_PAGE_ID,
        fb_geo.xres,
        fb_geo.yres,
        fb_geo.line_length,
        &bg,
        profile,
    )?;
    drop(store);
    let manifest = PersistedNotebookManifestV1 {
        v: NOTEBOOK_MANIFEST_VERSION,
        notebook_id: notebook_id.to_string(),
        page_ids: vec![DEFAULT_PAGE_ID.into()],
        active_page_id: DEFAULT_PAGE_ID.into(),
        next_page_ordinal: 2,
    };
    validate_manifest(&manifest)?;
    write_manifest_atomic(data_dir, &manifest)
}

pub fn page_index(manifest: &PersistedNotebookManifestV1, page_id: &str) -> Option<usize> {
    manifest.page_ids.iter().position(|p| p == page_id)
}

/// Allocate next page id string from ordinal (does not mutate yet).
pub fn allocate_page_id(manifest: &PersistedNotebookManifestV1) -> String {
    format!("page-{}", manifest.next_page_ordinal)
}

/// Create a new page directory by copying page-1 background + profile. Does not update manifest.
pub fn create_page_files_from_template(
    data_dir: &Path,
    new_page_id: &str,
    template_page_id: &str,
) -> Result<(FsPageStore, LoadedPage), String> {
    if page_exists(data_dir, new_page_id) {
        return Err(format!(
            "REFUSE PAGE CREATE: {new_page_id} already exists on disk"
        ));
    }
    let (_tmpl_store, tmpl) = load_existing_page(data_dir, template_page_id)?;
    let geo = FbGeometry {
        xres: tmpl.header.panel_width,
        yres: tmpl.header.panel_height,
        xres_virtual: tmpl.header.panel_width,
        yres_virtual: tmpl.header.panel_height,
        xoffset: 0,
        yoffset: 0,
        bits_per_pixel: 8,
        line_length: tmpl.header.line_length,
        smem_len: tmpl
            .header
            .line_length
            .saturating_mul(tmpl.header.panel_height),
    };
    let mut bg = tmpl.background.clone();
    scrub_chrome_band(&mut bg, &geo)?;
    create_new_page(
        data_dir,
        new_page_id,
        tmpl.header.panel_width,
        tmpl.header.panel_height,
        tmpl.header.line_length,
        &bg,
        &tmpl.render_profile,
    )
}

/// Remember the page shown after plain navigation. Inside a library this is replica-local
/// state ([`crate::nav_state`]): `notebook.meta` is not rewritten, so browsing is not a library
/// mutation. A failed save only loses the reopen position and is logged, not returned. A
/// notebook outside a library keeps the legacy canonical `active_page_id` write.
pub fn persist_active_page(
    data_dir: &Path,
    manifest: &mut PersistedNotebookManifestV1,
    active_page_id: &str,
) -> Result<u64, String> {
    if !manifest.page_ids.iter().any(|p| p == active_page_id) {
        return Err(format!(
            "REFUSE: active_page_id {active_page_id} not in page_ids"
        ));
    }
    let t0 = Instant::now();
    match crate::nav_state::remember(data_dir, manifest, active_page_id) {
        Ok(true) => {}
        Ok(false) => {
            manifest.active_page_id = active_page_id.to_string();
            write_manifest_atomic(data_dir, manifest)?;
        }
        Err(e) => eprintln!("warning: nav_state_save_failed page={active_page_id} detail={e}"),
    }
    Ok(t0.elapsed().as_micros() as u64)
}

/// After successful create+reconstruct+bind: append page, set active, bump ordinal.
pub fn commit_new_page_to_manifest(
    data_dir: &Path,
    manifest: &mut PersistedNotebookManifestV1,
    new_page_id: &str,
) -> Result<u64, String> {
    if manifest.page_ids.iter().any(|p| p == new_page_id) {
        return Err(format!("REFUSE: page {new_page_id} already in manifest"));
    }
    let expected = allocate_page_id(manifest);
    if new_page_id != expected {
        return Err(format!(
            "REFUSE: new page id {new_page_id} != allocated {expected}"
        ));
    }
    let t0 = Instant::now();
    manifest.page_ids.push(new_page_id.to_string());
    manifest.active_page_id = new_page_id.to_string();
    manifest.next_page_ordinal = manifest.next_page_ordinal.saturating_add(1);
    validate_manifest(manifest)?;
    write_manifest_atomic(data_dir, manifest)?;
    Ok(t0.elapsed().as_micros() as u64)
}

/// Require same sealed render profile across notebook pages.
pub fn require_same_profile(
    expected: &RenderProfile,
    actual: &RenderProfile,
) -> Result<(), String> {
    if expected != actual {
        return Err(format!(
            "REFUSE PAGE SWITCH: render profile mismatch \
             (expected ink={:02x} r={} {} {}; got ink={:02x} r={} {} {})",
            expected.ink_byte,
            expected.brush_radius,
            expected.transform_id,
            expected.rasterizer_id,
            actual.ink_byte,
            actual.brush_radius,
            actual.transform_id,
            actual.rasterizer_id
        ));
    }
    Ok(())
}

/// Load a page for switching: validate profile + framebuffer; do not restore yet.
pub fn load_page_for_switch(
    data_dir: &Path,
    page_id: &str,
    notebook_profile: &RenderProfile,
    fb_geo: &FbGeometry,
) -> Result<(FsPageStore, LoadedPage), String> {
    let (store, mut loaded) = load_existing_page(data_dir, page_id)?;
    repair_background_chrome(data_dir, page_id, &mut loaded)?;
    require_same_profile(notebook_profile, &loaded.render_profile)?;
    validate_live_framebuffer(&loaded.header, fb_geo)?;
    Ok((store, loaded))
}

/// Fault-injection hooks for durable page deletion (host tests).
#[derive(Clone, Debug, Default)]
pub struct DeletePageHooks {
    /// Fail the post-commit quarantine rename (manifest already committed).
    pub fail_quarantine: bool,
    pub manifest: ManifestWriteHooks,
}

/// Post-commit quarantine of the deleted page directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuarantineOutcome {
    /// Directory moved under `.page-trash/`.
    Moved { trash_entry_name: String },
    /// Manifest committed but rename failed; page dir remains an unreferenced orphan.
    LeftAsOrphan { reason: String },
}

/// Result of a durable page deletion (manifest already committed on `Ok`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletePageCommit {
    pub deleted_page_id: String,
    pub successor_page_id: String,
    pub quarantine: QuarantineOutcome,
    pub manifest_persist_us: u64,
}

/// Successor after deleting `page_id`: the page that slides into the deleted index,
/// or the previous page when deleting the last entry.
pub fn successor_after_delete(
    manifest: &PersistedNotebookManifestV1,
    page_id: &str,
) -> Result<String, String> {
    if manifest.page_ids.len() < 2 {
        return Err("REFUSE PAGE DELETE: cannot delete the only page".into());
    }
    let Some(idx) = page_index(manifest, page_id) else {
        return Err(format!(
            "REFUSE PAGE DELETE: page id {page_id} not in manifest"
        ));
    };
    if idx + 1 < manifest.page_ids.len() {
        Ok(manifest.page_ids[idx + 1].clone())
    } else {
        Ok(manifest.page_ids[idx - 1].clone())
    }
}

/// Build the post-delete manifest without mutating `manifest` or touching disk.
pub fn plan_delete_page(
    manifest: &PersistedNotebookManifestV1,
    page_id: &str,
) -> Result<(PersistedNotebookManifestV1, String), String> {
    if manifest.page_ids.len() < 2 {
        return Err("REFUSE PAGE DELETE: cannot delete the only page".into());
    }
    let Some(idx) = page_index(manifest, page_id) else {
        return Err(format!(
            "REFUSE PAGE DELETE: page id {page_id} not in manifest"
        ));
    };
    let successor = successor_after_delete(manifest, page_id)?;
    let mut next = manifest.clone();
    next.page_ids.remove(idx);
    next.active_page_id = successor.clone();
    // next_page_ordinal unchanged — deleted IDs are never reused via allocate_page_id.
    validate_manifest(&next)?;
    Ok((next, successor))
}

fn trash_root(data_dir: &Path) -> PathBuf {
    data_dir.join(PAGE_TRASH_DIR)
}

fn unique_trash_entry_name(page_id: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{page_id}__{n}")
}

/// Durably delete `page_id`: commit the new manifest first, then quarantine the
/// now-unreferenced page directory under `.page-trash/`.
///
/// Invariant: a committed manifest never references a directory that was already moved.
/// Quarantine failure after commit does **not** roll back the manifest — deletion remains
/// authoritative and the source dir may remain as a recoverable orphan.
///
/// Does **not** mutate `manifest` until the durable manifest write succeeds.
pub fn delete_page_durable(
    data_dir: &Path,
    manifest: &mut PersistedNotebookManifestV1,
    page_id: &str,
    hooks: &DeletePageHooks,
) -> Result<DeletePageCommit, String> {
    let (planned, successor) = plan_delete_page(manifest, page_id)?;
    if !page_exists(data_dir, page_id) {
        return Err(format!(
            "REFUSE PAGE DELETE: page directory {page_id} missing"
        ));
    }
    if !page_exists(data_dir, &successor) {
        return Err(format!(
            "REFUSE PAGE DELETE: successor directory {successor} missing"
        ));
    }

    // Held through the quarantine move, which runs after the manifest write's own admit ends.
    let _admit = crate::sync_verified::note_mutation_from_notebook_dir(data_dir)
        .map_err(|e| format!("REFUSE PAGE DELETE: {e}"))?;
    let trash = trash_root(data_dir);
    fs::create_dir_all(&trash).map_err(|e| format!("REFUSE PAGE DELETE: create trash: {e}"))?;
    let trash_name = unique_trash_entry_name(page_id);
    let src = crate::store::page_dir(data_dir, page_id);
    let dst = trash.join(&trash_name);
    if dst.exists() {
        return Err(format!("REFUSE PAGE DELETE: trash collision {trash_name}"));
    }

    // Manifest first — crash before this leaves the old notebook intact.
    let t0 = Instant::now();
    write_manifest_atomic_with(data_dir, &planned, &hooks.manifest).map_err(|e| {
        format!("REFUSE PAGE DELETE: manifest persist failed (page dir untouched): {e}")
    })?;
    let manifest_persist_us = t0.elapsed().as_micros() as u64;
    *manifest = planned;

    // Quarantine is best-effort after logical commit.
    let quarantine = if hooks.fail_quarantine {
        eprintln!(
            "warning: page_delete quarantine failed page={page_id} (injected); left as orphan"
        );
        QuarantineOutcome::LeftAsOrphan {
            reason: "injected: quarantine move failed".into(),
        }
    } else {
        match fs::rename(&src, &dst) {
            Ok(()) => QuarantineOutcome::Moved {
                trash_entry_name: trash_name,
            },
            Err(e) => {
                eprintln!(
                    "warning: page_delete quarantine failed page={page_id}: {e}; left as orphan"
                );
                QuarantineOutcome::LeftAsOrphan {
                    reason: format!("quarantine rename failed: {e}"),
                }
            }
        }
    };

    Ok(DeletePageCommit {
        deleted_page_id: page_id.to_string(),
        successor_page_id: successor,
        quarantine,
        manifest_persist_us,
    })
}

// ---- Sync S2C: remote materialization primitives ----
//
// Used only by `sync_materialize` to make the local files match a canonical sync state. They
// reuse the F8/F9 conventions (manifest-first commits, `.page-trash/` quarantine, monotonic
// ordinals) and never touch the replica-local active page unless that page itself is removed.

fn page_ordinal(page_id: &str) -> Option<u64> {
    page_id.strip_prefix("page-")?.parse().ok()
}

/// Next free local page id at or after `next_page_ordinal`, skipping any directory already on
/// disk (e.g. left by an interrupted create), so no ordinal is ever reused or overwritten.
pub fn allocate_fresh_page_id(data_dir: &Path, manifest: &PersistedNotebookManifestV1) -> String {
    let mut n = manifest.next_page_ordinal;
    loop {
        let id = format!("page-{n}");
        if !manifest.page_ids.contains(&id) && !crate::store::page_dir(data_dir, &id).exists() {
            return id;
        }
        n = n.saturating_add(1);
    }
}

/// Move a page directory that is NOT referenced by the manifest into `.page-trash/`.
pub fn quarantine_page_dir(data_dir: &Path, page_id: &str) -> Result<(), String> {
    let src = crate::store::page_dir(data_dir, page_id);
    if !src.exists() {
        return Ok(());
    }
    let _admit = crate::sync_verified::note_mutation_from_page_dir(&src)?;
    let trash = trash_root(data_dir);
    fs::create_dir_all(&trash).map_err(|e| format!("create trash: {e}"))?;
    let dst = trash.join(unique_trash_entry_name(page_id));
    fs::rename(&src, &dst).map_err(|e| format!("quarantine {page_id}: {e}"))
}

/// Insert an already-created page directory at `index` (active page unchanged; the ordinal
/// counter moves past the new id).
pub fn insert_page_at_durable(
    data_dir: &Path,
    manifest: &mut PersistedNotebookManifestV1,
    page_id: &str,
    index: usize,
) -> Result<(), String> {
    if manifest.page_ids.iter().any(|p| p == page_id) {
        return Err(format!("REFUSE PAGE INSERT: {page_id} already in manifest"));
    }
    if !page_exists(data_dir, page_id) {
        return Err(format!("REFUSE PAGE INSERT: {page_id} has no page files"));
    }
    let mut next = manifest.clone();
    next.page_ids
        .insert(index.min(next.page_ids.len()), page_id.to_string());
    if let Some(n) = page_ordinal(page_id) {
        next.next_page_ordinal = next.next_page_ordinal.max(n.saturating_add(1));
    }
    validate_manifest(&next)?;
    write_manifest_atomic(data_dir, &next)?;
    *manifest = next;
    Ok(())
}

/// Remove `page_id` (manifest first, then quarantine). The active page is preserved unless it
/// is the removed page, in which case the F8 successor becomes active.
pub fn remove_page_durable(
    data_dir: &Path,
    manifest: &mut PersistedNotebookManifestV1,
    page_id: &str,
) -> Result<(), String> {
    if manifest.active_page_id == page_id {
        return delete_page_durable(data_dir, manifest, page_id, &DeletePageHooks::default())
            .map(|_| ());
    }
    if manifest.page_ids.len() < 2 {
        return Err("REFUSE PAGE DELETE: cannot delete the only page".into());
    }
    let Some(idx) = page_index(manifest, page_id) else {
        return Err(format!(
            "REFUSE PAGE DELETE: page id {page_id} not in manifest"
        ));
    };
    let mut next = manifest.clone();
    next.page_ids.remove(idx);
    validate_manifest(&next)?;
    write_manifest_atomic(data_dir, &next)?;
    *manifest = next;
    if let Err(e) = quarantine_page_dir(data_dir, page_id) {
        eprintln!("warning: page quarantine failed page={page_id}: {e}; left as orphan");
    }
    Ok(())
}

/// Replace the page order with `order`, a permutation of the current page ids.
pub fn reorder_pages_durable(
    data_dir: &Path,
    manifest: &mut PersistedNotebookManifestV1,
    order: &[String],
) -> Result<(), String> {
    let mut a = manifest.page_ids.clone();
    let mut b = order.to_vec();
    a.sort();
    b.sort();
    if a != b {
        return Err("REFUSE PAGE REORDER: not a permutation of the current pages".into());
    }
    if manifest.page_ids == order {
        return Ok(());
    }
    let mut next = manifest.clone();
    next.page_ids = order.to_vec();
    validate_manifest(&next)?;
    write_manifest_atomic(data_dir, &next)?;
    *manifest = next;
    Ok(())
}

/// Create a brand-new notebook directory holding one page (exact background bytes).
pub fn create_notebook_dir_with_page(
    nb_dir: &Path,
    notebook_id: &str,
    page_id: &str,
    dims: (u32, u32, u32),
    background: &[u8],
    profile: &RenderProfile,
) -> Result<(), String> {
    fs::create_dir_all(nb_dir).map_err(|e| e.to_string())?;
    let (w, h, line) = dims;
    let (store, _) = create_new_page(nb_dir, page_id, w, h, line, background, profile)?;
    drop(store);
    let manifest = PersistedNotebookManifestV1 {
        v: NOTEBOOK_MANIFEST_VERSION,
        notebook_id: notebook_id.to_string(),
        page_ids: vec![page_id.to_string()],
        active_page_id: page_id.to_string(),
        next_page_ordinal: page_ordinal(page_id).map_or(2, |n| n.saturating_add(1)),
    };
    validate_manifest(&manifest)?;
    write_manifest_atomic(nb_dir, &manifest)
}

// ---- Gate F9: durable page ordering + insert-after ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MovePageDirection {
    Left,
    Right,
}

/// Fault-injection hooks for durable page reorder (host tests).
#[derive(Clone, Debug, Default)]
pub struct MovePageHooks {
    pub manifest: ManifestWriteHooks,
}

/// Result of a durable page reorder (manifest already committed on `Ok`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovePageCommit {
    pub page_id: String,
    pub from_index: usize,
    pub to_index: usize,
    pub manifest_persist_us: u64,
}

/// Build the post-move manifest without mutating `manifest` or touching disk.
pub fn plan_move_page(
    manifest: &PersistedNotebookManifestV1,
    page_id: &str,
    direction: MovePageDirection,
) -> Result<(PersistedNotebookManifestV1, usize, usize), String> {
    let Some(from) = page_index(manifest, page_id) else {
        return Err(format!(
            "REFUSE PAGE MOVE: page id {page_id} not in manifest"
        ));
    };
    let to = match direction {
        MovePageDirection::Left => {
            if from == 0 {
                return Err("REFUSE PAGE MOVE: already first page".into());
            }
            from - 1
        }
        MovePageDirection::Right => {
            if from + 1 >= manifest.page_ids.len() {
                return Err("REFUSE PAGE MOVE: already last page".into());
            }
            from + 1
        }
    };
    let mut next = manifest.clone();
    next.page_ids.swap(from, to);
    // active_page_id and next_page_ordinal unchanged — identity preserved.
    validate_manifest(&next)?;
    Ok((next, from, to))
}

/// Durably reorder `page_id` one slot left or right. Manifest-only; page files untouched.
///
/// Does **not** mutate `manifest` until the durable manifest write succeeds.
pub fn move_page_durable(
    data_dir: &Path,
    manifest: &mut PersistedNotebookManifestV1,
    page_id: &str,
    direction: MovePageDirection,
    hooks: &MovePageHooks,
) -> Result<MovePageCommit, String> {
    let (planned, from, to) = plan_move_page(manifest, page_id, direction)?;
    if !page_exists(data_dir, page_id) {
        return Err(format!(
            "REFUSE PAGE MOVE: page directory {page_id} missing"
        ));
    }
    let t0 = Instant::now();
    write_manifest_atomic_with(data_dir, &planned, &hooks.manifest)
        .map_err(|e| format!("REFUSE PAGE MOVE: manifest persist failed: {e}"))?;
    let manifest_persist_us = t0.elapsed().as_micros() as u64;
    *manifest = planned;
    Ok(MovePageCommit {
        page_id: page_id.to_string(),
        from_index: from,
        to_index: to,
        manifest_persist_us,
    })
}

/// Fault-injection hooks for durable insert-after (host tests).
#[derive(Clone, Debug, Default)]
pub struct InsertPageHooks {
    /// Fail cleanup of the uncommitted page directory after a pre-commit manifest failure.
    pub fail_cleanup: bool,
    pub manifest: ManifestWriteHooks,
}

/// Cleanup of an uncommitted insert page directory after pre-commit failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertCleanupOutcome {
    Removed,
    LeftAsOrphan { reason: String },
}

/// Result of a durable insert-after (manifest already committed on `Ok`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InsertPageCommit {
    pub inserted_page_id: String,
    pub insert_index: usize,
    pub manifest_persist_us: u64,
}

/// Build the post-insert manifest without mutating `manifest` or touching disk.
pub fn plan_insert_after(
    manifest: &PersistedNotebookManifestV1,
    after_page_id: &str,
) -> Result<(PersistedNotebookManifestV1, String, usize), String> {
    let Some(idx) = page_index(manifest, after_page_id) else {
        return Err(format!(
            "REFUSE PAGE INSERT: page id {after_page_id} not in manifest"
        ));
    };
    let new_id = allocate_page_id(manifest);
    let insert_at = idx + 1;
    let mut next = manifest.clone();
    next.page_ids.insert(insert_at, new_id.clone());
    next.active_page_id = new_id.clone();
    next.next_page_ordinal = next.next_page_ordinal.saturating_add(1);
    validate_manifest(&next)?;
    Ok((next, new_id, insert_at))
}

/// Durably insert a blank page after `after_page_id`.
///
/// Page files are created first; the manifest is committed second. On manifest failure the
/// new directory is cleaned up when possible (else left as an unreferenced orphan).
/// Does **not** mutate `manifest` until the durable manifest write succeeds.
pub fn insert_page_after_durable(
    data_dir: &Path,
    manifest: &mut PersistedNotebookManifestV1,
    after_page_id: &str,
    hooks: &InsertPageHooks,
) -> Result<InsertPageCommit, String> {
    let (planned, new_id, insert_at) = plan_insert_after(manifest, after_page_id)?;
    if !page_exists(data_dir, after_page_id) {
        return Err(format!(
            "REFUSE PAGE INSERT: after page directory {after_page_id} missing"
        ));
    }
    // Template from the current page so profile/background match the notebook.
    create_page_files_from_template(data_dir, &new_id, after_page_id)
        .map_err(|e| format!("REFUSE PAGE INSERT: create page files failed: {e}"))?;

    let t0 = Instant::now();
    if let Err(e) = write_manifest_atomic_with(data_dir, &planned, &hooks.manifest) {
        let cleanup = if hooks.fail_cleanup {
            eprintln!(
                "warning: page_insert cleanup failed page={new_id} (injected); left as orphan"
            );
            InsertCleanupOutcome::LeftAsOrphan {
                reason: "injected: cleanup failed".into(),
            }
        } else {
            let dir = page_dir(data_dir, &new_id);
            match fs::remove_dir_all(&dir) {
                Ok(()) => InsertCleanupOutcome::Removed,
                Err(ce) => {
                    eprintln!(
                        "warning: page_insert cleanup failed page={new_id}: {ce}; left as orphan"
                    );
                    InsertCleanupOutcome::LeftAsOrphan {
                        reason: format!("cleanup remove_dir_all failed: {ce}"),
                    }
                }
            }
        };
        return Err(format!(
            "REFUSE PAGE INSERT: manifest persist failed (new page not navigable; cleanup={cleanup:?}): {e}"
        ));
    }
    let manifest_persist_us = t0.elapsed().as_micros() as u64;
    *manifest = planned;
    Ok(InsertPageCommit {
        inserted_page_id: new_id,
        insert_index: insert_at,
        manifest_persist_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::RenderProfile;
    use crate::store::PageStore;
    use notebook_core::{Point, Stroke, StrokeId};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scribe-f2-nb-{n}"));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn geo() -> FbGeometry {
        FbGeometry {
            xres: 40,
            yres: 20,
            xres_virtual: 40,
            yres_virtual: 20,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 40,
            smem_len: 800,
        }
    }

    fn bg() -> Vec<u8> {
        vec![0xAAu8; 40 * 20]
    }

    fn stroke(id: &str) -> Stroke {
        Stroke {
            id: StrokeId::new(id),
            points: vec![Point::new(10.0, 20.0), Point::new(30.0, 40.0)],
            brush: None,
            finished: true,
        }
    }

    #[test]
    fn fresh_notebook_creates_page1_manifest() {
        let root = tmp();
        let g = geo();
        let open = open_or_create_notebook(&root, InkRequest::Default, &g, &bg()).unwrap();
        assert_eq!(open.manifest.page_ids, vec!["page-1".to_string()]);
        assert_eq!(open.manifest.active_page_id, "page-1");
        assert_eq!(open.manifest.next_page_ordinal, 2);
        assert!(!open.adopted_from_existing_page);
        assert!(manifest_path(&root).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn adopt_existing_page1_without_rewriting() {
        let root = tmp();
        let g = geo();
        let profile = RenderProfile::sealed_v2_pressure(0x00).unwrap();
        let (mut store, _) = create_new_page(&root, "page-1", 40, 20, 40, &bg(), &profile).unwrap();
        store.append_committed_stroke(&stroke("s-a")).unwrap();
        drop(store);
        let journal_before = fs::read(root.join("page-1/strokes.jsonl")).unwrap();
        let open = open_or_create_notebook(&root, InkRequest::Default, &g, &bg()).unwrap();
        assert!(open.adopted_from_existing_page);
        assert_eq!(open.loaded.strokes.len(), 1);
        assert_eq!(open.manifest.next_page_ordinal, 2);
        let journal_after = fs::read(root.join("page-1/strokes.jsonl")).unwrap();
        assert_eq!(journal_before, journal_after);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ordered_pages_preserved() {
        let mut m = PersistedNotebookManifestV1 {
            v: 1,
            notebook_id: "n".into(),
            page_ids: vec!["page-1".into(), "page-2".into(), "page-3".into()],
            active_page_id: "page-2".into(),
            next_page_ordinal: 4,
        };
        validate_manifest(&m).unwrap();
        assert_eq!(page_index(&m, "page-3"), Some(2));
        m.active_page_id = "page-9".into();
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn refuse_duplicates_empty_bad_version_collision() {
        let mut m = PersistedNotebookManifestV1 {
            v: 99,
            notebook_id: "n".into(),
            page_ids: vec!["page-1".into()],
            active_page_id: "page-1".into(),
            next_page_ordinal: 2,
        };
        assert!(validate_manifest(&m).unwrap_err().contains("version"));
        m.v = 1;
        m.page_ids.clear();
        assert!(validate_manifest(&m).unwrap_err().contains("empty"));
        m.page_ids = vec!["page-1".into(), "page-1".into()];
        m.active_page_id = "page-1".into();
        m.next_page_ordinal = 2;
        assert!(validate_manifest(&m).unwrap_err().contains("duplicate"));
        m.page_ids = vec!["page-1".into(), "page-2".into()];
        m.next_page_ordinal = 2; // collides with page-2
        assert!(validate_manifest(&m).unwrap_err().contains("collides"));
    }

    #[test]
    fn missing_referenced_page_refused() {
        let root = tmp();
        let m = PersistedNotebookManifestV1 {
            v: 1,
            notebook_id: "n".into(),
            page_ids: vec!["page-1".into()],
            active_page_id: "page-1".into(),
            next_page_ordinal: 2,
        };
        write_manifest_atomic(&root, &m).unwrap();
        let err = match open_or_create_notebook(&root, InkRequest::Default, &geo(), &bg()) {
            Ok(_) => panic!("expected refuse"),
            Err(e) => e,
        };
        assert!(err.contains("missing page"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn orphan_reported_not_adopted() {
        let root = tmp();
        let g = geo();
        let open = open_or_create_notebook(&root, InkRequest::Default, &g, &bg()).unwrap();
        drop(open);
        // Plant orphan directory with junk.
        fs::create_dir_all(root.join("page-99")).unwrap();
        fs::write(root.join("page-99/note.txt"), b"orphan").unwrap();
        let open2 = open_or_create_notebook(&root, InkRequest::Default, &g, &bg()).unwrap();
        assert!(open2.orphan_dirs.iter().any(|o| o == "page-99"));
        assert!(!open2.manifest.page_ids.contains(&"page-99".into()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn create_page_copies_template_background_empty_journal() {
        let root = tmp();
        let g = geo();
        let open = open_or_create_notebook(&root, InkRequest::Default, &g, &bg()).unwrap();
        let id = allocate_page_id(&open.manifest);
        let (_s, loaded) = create_page_files_from_template(&root, &id, "page-1").unwrap();
        assert_eq!(loaded.strokes.len(), 0);
        assert_eq!(loaded.render_profile, open.loaded.render_profile);
        assert_eq!(loaded.background, open.loaded.background);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn page_isolation_across_journals() {
        let root = tmp();
        let g = geo();
        let open = open_or_create_notebook(&root, InkRequest::Default, &g, &bg()).unwrap();
        let mut store1 = open.store;
        store1.append_committed_stroke(&stroke("s-a")).unwrap();
        drop(store1);
        let mut man = open.manifest;
        let id2 = allocate_page_id(&man);
        let (mut store2, _) = create_page_files_from_template(&root, &id2, "page-1").unwrap();
        store2.append_committed_stroke(&stroke("s-b")).unwrap();
        drop(store2);
        commit_new_page_to_manifest(&root, &mut man, &id2).unwrap();
        let id3 = allocate_page_id(&man);
        let (mut store3, _) = create_page_files_from_template(&root, &id3, "page-1").unwrap();
        store3.append_committed_stroke(&stroke("s-c")).unwrap();
        drop(store3);
        commit_new_page_to_manifest(&root, &mut man, &id3).unwrap();

        let (_s, p1) = load_existing_page(&root, "page-1").unwrap();
        let (_s, p2) = load_existing_page(&root, "page-2").unwrap();
        let (_s, p3) = load_existing_page(&root, "page-3").unwrap();
        assert_eq!(p1.strokes.len(), 1);
        assert_eq!(p1.strokes[0].id.as_str(), "s-a");
        assert_eq!(p2.strokes[0].id.as_str(), "s-b");
        assert_eq!(p3.strokes[0].id.as_str(), "s-c");
        let _ = fs::remove_dir_all(root);
    }

    fn notebook_with_n_pages(n: usize) -> (PathBuf, PersistedNotebookManifestV1) {
        assert!(n >= 1);
        let root = tmp();
        let g = geo();
        let open = open_or_create_notebook(&root, InkRequest::Default, &g, &bg()).unwrap();
        let mut man = open.manifest;
        for _ in 2..=n {
            let id = allocate_page_id(&man);
            let (_s, _) = create_page_files_from_template(&root, &id, "page-1").unwrap();
            commit_new_page_to_manifest(&root, &mut man, &id).unwrap();
        }
        persist_active_page(&root, &mut man, "page-1").unwrap();
        (root, man)
    }

    fn paint_chrome_contamination(dir: &Path, page_id: &str) {
        let pdir = page_dir(dir, page_id);
        let meta: crate::store::PersistedPageHeader =
            serde_json::from_str(&fs::read_to_string(pdir.join("page.meta")).unwrap()).unwrap();
        let mut raw = fs::read(pdir.join("background.raw")).unwrap();
        // Darken the last row so scrub_chrome_band must rewrite durable background.
        let y = meta.panel_height.saturating_sub(1);
        let row = (y as usize) * meta.line_length as usize;
        for x in 0..(meta.panel_width as usize) {
            raw[row + x] = 0x00;
        }
        fs::write(pdir.join("background.raw"), &raw).unwrap();
        let mut header = meta;
        header.background_checksum = kindle_platform::rect_checksum(&raw);
        crate::store::write_meta(&pdir, &header).unwrap();
    }

    #[test]
    fn open_does_not_require_healthy_non_active_page_journals() {
        let (root, _man) = notebook_with_n_pages(3);
        // Corrupt page-2 journals; active is page-1.
        fs::write(root.join("page-2/strokes.jsonl"), b"{not-json\n").unwrap();
        fs::write(root.join("page-2/edits.jsonl"), b"{also-bad\n").unwrap();

        let open = open_or_create_notebook(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open.loaded.header.page_id, "page-1");
        assert_eq!(open.manifest.page_ids.len(), 3);

        // Active page remains usable; corrupt sibling is discovered only on switch.
        let profile = open.loaded.render_profile.clone();
        let err = match load_page_for_switch(&root, "page-2", &profile, &geo()) {
            Ok(_) => panic!("expected page-2 switch to fail"),
            Err(e) => e,
        };
        assert!(
            err.contains("REFUSE") || err.contains("json") || err.contains("parse"),
            "unexpected switch error: {err}"
        );
        // Re-open still succeeds on the healthy active page.
        let open2 = open_or_create_notebook(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open2.loaded.header.page_id, "page-1");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn open_repairs_active_page_chrome_lazily_repairs_others_on_switch() {
        let (root, _man) = notebook_with_n_pages(2);
        paint_chrome_contamination(&root, "page-1");
        paint_chrome_contamination(&root, "page-2");
        let contaminated = fs::read(root.join("page-2/background.raw")).unwrap();

        let open = open_or_create_notebook(&root, InkRequest::Default, &geo(), &bg()).unwrap();
        assert_eq!(open.loaded.header.page_id, "page-1");
        // Active page must be scrubbed during open.
        let y = open.loaded.header.panel_height.saturating_sub(1);
        let row = (y as usize) * open.loaded.header.line_length as usize;
        assert_ne!(
            open.loaded.background[row], 0x00,
            "active page chrome band should be repaired on open"
        );
        // Non-active page must still be contaminated on disk (not eagerly repaired).
        assert_eq!(
            fs::read(root.join("page-2/background.raw")).unwrap(),
            contaminated,
            "non-active page must not be repaired during open"
        );

        let profile = open.loaded.render_profile.clone();
        let (_s, loaded2) = load_page_for_switch(&root, "page-2", &profile, &geo()).unwrap();
        let row2 = (loaded2.header.panel_height.saturating_sub(1) as usize)
            * loaded2.header.line_length as usize;
        assert_ne!(
            loaded2.background[row2], 0x00,
            "page-2 chrome must be repaired when switched to"
        );
        // Repair is durable.
        assert_ne!(
            fs::read(root.join("page-2/background.raw")).unwrap()[row2],
            0x00
        );
        // Idempotent: second switch does not fail and leaves clean background.
        let before = fs::read(root.join("page-2/background.raw")).unwrap();
        let (_s, _) = load_page_for_switch(&root, "page-2", &profile, &geo()).unwrap();
        assert_eq!(
            fs::read(root.join("page-2/background.raw")).unwrap(),
            before
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn open_still_refuses_corrupt_active_page() {
        let (root, _man) = notebook_with_n_pages(2);
        fs::write(root.join("page-1/strokes.jsonl"), b"{not-json\n").unwrap();
        let err = match open_or_create_notebook(&root, InkRequest::Default, &geo(), &bg()) {
            Ok(_) => panic!("expected refuse on corrupt active page"),
            Err(e) => e,
        };
        assert!(
            err.contains("REFUSE") || err.contains("json") || err.contains("parse"),
            "unexpected open error: {err}"
        );
        let _ = fs::remove_dir_all(root);
    }
}
