//! Sync Phase S1: bridge between the Kindle's local durable format and the canonical sync
//! model (`sync-protocol`).
//!
//! Local files (`library.meta`, `notebook.meta`, `strokes.jsonl`, …) stay the crash-safe
//! materialization. Snapshot/verify never rewrite journals or `background.raw`. The one
//! allowed write is sealing a missing/invalid `background_identity` into `page.meta` after
//! checksum verification (in-place migration; visible page content unchanged).
//! * Global identities live in the app-owned namespace `<library>/.scribe-sync/identity.json`,
//!   outside every existing schema.
//! * Replica identity lives outside the library (`/mnt/us/scribe-app/state/replica.json` on the
//!   Kindle), so copying `/mnt/us/scribe-data` never clones it.
//!
//! Nothing here runs on the pen hot path; the runtime does not call it.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use sync_protocol::{
    global_stroke_id, reconcile_identity, sha256_hex, snapshot_sha256, BackgroundEncoding,
    BackgroundFormat, BackgroundRef, IdentityFileV1, LiveLayout, PageId, PageSpace,
    PressureCurveRef, Reconciled, RenderProfileRef, ReplicaId, ReplicaStateV1, Sha256Hex,
    SnapshotNotebook, SnapshotPage, StrokeStyle, SyncPoint, SyncSnapshotV1, SyncStroke,
    REPLICA_STATE_FILE, SNAPSHOT_VERSION,
};
use uuid::Uuid;

use crate::library::{open_library_read_only, read_notebook_manifest_read_only, LibraryRepository};
use crate::pressure_curve::{STYLE_LEGACY_FIXED_V1, STYLE_PRESSURE_WIDTH_V1};
use crate::profile::RenderProfile;
use crate::store::{
    load_page_journals_read_only, page_dir, resolve_background_ref,
    resolve_background_ref_read_only,
};
use crate::ui_gfx::WHITE;

/// App-owned sync namespace inside a library root (not part of any notebook/page schema).
pub const SYNC_DIR: &str = ".scribe-sync";
pub const IDENTITY_FILE: &str = "identity.json";
/// Lock file serializing identity transactions (empty; the kernel holds the lock state).
pub const IDENTITY_LOCK_FILE: &str = "identity.lock";
/// Lock file serializing replica-state creation.
pub const REPLICA_LOCK_FILE: &str = "replica.lock";
/// Kindle application state (outside the library data root).
pub const DEFAULT_REPLICA_STATE_DIR: &str = "/mnt/us/scribe-app/state";

/// Failure injection for durability tests.
#[derive(Clone, Debug, Default)]
pub struct SyncWriteHooks {
    pub fail_write: bool,
    /// After `write_all`, before `fsync`.
    pub fail_sync: bool,
    pub fail_rename: bool,
    /// After atomic rename, before directory fsync.
    pub fail_dirsync: bool,
    /// After rename + directory sync: the new file is durable, but the caller lost confirmation.
    pub fail_after_commit: bool,
}

pub fn identity_path(library_root: &Path) -> PathBuf {
    library_root.join(SYNC_DIR).join(IDENTITY_FILE)
}

fn sync_dir(path: &Path) {
    #[cfg(unix)]
    if let Ok(f) = File::open(path) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// temp → write → fsync → rename → directory sync. The final file is either the old bytes or
/// the complete new bytes.
fn write_atomic(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    hooks: &SyncWriteHooks,
) -> Result<(), String> {
    if name == IDENTITY_FILE {
        let root = dir.parent().ok_or_else(|| {
            format!(
                "REFUSE SYNC STAMP: identity dir {} has no parent",
                dir.display()
            )
        })?;
        let _admit = crate::sync_verified::note_library_mutation(root)?;
    }
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let tmp = dir.join(format!("{name}.tmp"));
    if hooks.fail_write {
        return Err(format!("injected: write {name} failed"));
    }
    {
        let mut f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        if hooks.fail_sync {
            drop(f);
            let _ = fs::remove_file(&tmp);
            return Err(format!("injected: fsync {name} failed"));
        }
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    if hooks.fail_rename {
        let _ = fs::remove_file(&tmp);
        return Err(format!("injected: rename {name} failed"));
    }
    fs::rename(&tmp, dir.join(name)).map_err(|e| format!("rename {}: {e}", tmp.display()))?;
    if hooks.fail_dirsync {
        return Err(format!("injected: dirsync {name} failed"));
    }
    sync_dir(dir);
    if hooks.fail_after_commit {
        return Err(format!("injected: after-commit {name} lost confirmation"));
    }
    Ok(())
}

/// Exclusive advisory lock on `<dir>/<name>`, held until drop.
///
/// Uses `std::fs::File::lock` (`flock(LOCK_EX)` on Unix, `LockFileEx` on Windows). The lock
/// belongs to the open file description, so it excludes other processes *and* other threads
/// that open the file separately. The kernel drops it when the descriptor closes — including
/// on crash or SIGKILL — so a dead holder can never leave a stale lock; the empty lock file
/// itself carries no state. Proven on the Kindle's `/mnt/us` (`fuse.fsp` over vfat, kernel
/// 4.9.77): a second process is excluded while the first holds `flock -x`, acquires it after
/// release, and a SIGKILLed holder releases it. Hard links (the other no-overwrite primitive)
/// are refused there (`Operation not permitted`).
struct ExclusiveLock {
    file: File,
}

impl ExclusiveLock {
    fn acquire(dir: &Path, name: &str) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let path = dir.join(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("open lock {}: {e}", path.display()))?;
        file.lock()
            .map_err(|e| format!("lock {}: {e}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn file_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut v = bytes.to_vec();
    v.push(b'\n');
    v
}

/// Validated library + its authoritative layout (library.meta order, notebook.meta page order).
/// Trash, orphan directories and dev triggers are never part of the layout.
pub fn live_layout(library_root: &Path) -> Result<(LibraryRepository, LiveLayout), String> {
    let repo = open_library_read_only(library_root)?;
    let mut layout = LiveLayout::default();
    for nb in &repo.manifest().notebook_ids {
        let m = read_notebook_manifest_read_only(&repo.notebook_dir(nb))?;
        layout.notebooks.push((nb.clone(), m.page_ids.clone()));
    }
    Ok((repo, layout))
}

/// Read `identity.json` if present (strict parse + validation).
pub fn read_identity(library_root: &Path) -> Result<Option<IdentityFileV1>, String> {
    let p = identity_path(library_root);
    match fs::read(&p) {
        Ok(bytes) => IdentityFileV1::parse(&bytes)
            .map(Some)
            .map_err(|e| format!("REFUSE SYNC IDENTITY: {}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("REFUSE SYNC IDENTITY: read {}: {e}", p.display())),
    }
}

/// Idempotently bootstrap/repair global identities for an existing library.
///
/// 1. Validate the library read-only (existing validators).
/// 2. Allocate IDs for every live notebook/page lacking one.
/// 3. Retire bindings of entities no longer live (never deleted, never reused).
/// 4. Persist atomically, **only if something changed**.
/// 5. Return the map.
///
/// On any error the durable file and the returned state are unchanged.
///
/// Concurrency: steps 2–4 run as one transaction under an exclusive lock on
/// `.scribe-sync/identity.lock`; the authoritative file is re-read *after* acquiring it. Every
/// concurrent caller therefore observes the state the previous holder published, exactly one
/// caller mints any given entity's ID, and every returned map is the durable map at release
/// time (later transactions only add or retire bindings, never change them). Readers need no
/// lock: publication is an atomic rename.
pub fn ensure_sync_identity(library_root: &Path) -> Result<Reconciled, String> {
    ensure_sync_identity_with(library_root, &mut Uuid::now_v7, &SyncWriteHooks::default())
}

pub fn ensure_sync_identity_with(
    library_root: &Path,
    mint: &mut dyn FnMut() -> Uuid,
    hooks: &SyncWriteHooks,
) -> Result<Reconciled, String> {
    // Refuse a non-library before creating anything (no `.scribe-sync` in foreign dirs).
    open_library_read_only(library_root)?;
    let _txn = ExclusiveLock::acquire(&library_root.join(SYNC_DIR), IDENTITY_LOCK_FILE)?;
    let (_repo, layout) = live_layout(library_root)?;
    let existing = read_identity(library_root)?;
    let r = reconcile_identity(existing.as_ref(), &layout, mint).map_err(|e| e.to_string())?;
    if r.changed {
        let bytes = r.identity.to_canonical_bytes().map_err(|e| e.to_string())?;
        crate::sync_verified::permit_sync_mutations(|| {
            write_atomic(
                &library_root.join(SYNC_DIR),
                IDENTITY_FILE,
                &file_bytes(&bytes),
                hooks,
            )
        })?;
    }
    Ok(r)
}

/// [`ensure_sync_identity`] without writing: the identity map the library *would* get, plus
/// the durable map it was planned from. Publish it with [`persist_planned_identity`] once the
/// caller has proven it wants it (startup recovery persists only after an exact replay).
pub fn plan_sync_identity(
    library_root: &Path,
) -> Result<(Option<IdentityFileV1>, Reconciled), String> {
    open_library_read_only(library_root)?;
    let (_repo, layout) = live_layout(library_root)?;
    let existing = read_identity(library_root)?;
    let r = reconcile_identity(existing.as_ref(), &layout, &mut Uuid::now_v7)
        .map_err(|e| e.to_string())?;
    Ok((existing, r))
}

/// Publish a plan from [`plan_sync_identity`], refusing if `identity.json` changed since it
/// was read (nothing is overwritten that the plan did not start from).
pub fn persist_planned_identity(
    library_root: &Path,
    planned_from: Option<&IdentityFileV1>,
    plan: &Reconciled,
) -> Result<(), String> {
    persist_planned_identity_with(library_root, planned_from, plan, &SyncWriteHooks::default())
}

/// [`persist_planned_identity`] with durability-injection hooks.
pub fn persist_planned_identity_with(
    library_root: &Path,
    planned_from: Option<&IdentityFileV1>,
    plan: &Reconciled,
    hooks: &SyncWriteHooks,
) -> Result<(), String> {
    if !plan.changed {
        return Ok(());
    }
    crate::sync_verified::permit_sync_mutations(|| {
        persist_planned_identity_inner(library_root, planned_from, plan, hooks)
    })
}

fn persist_planned_identity_inner(
    library_root: &Path,
    planned_from: Option<&IdentityFileV1>,
    plan: &Reconciled,
    hooks: &SyncWriteHooks,
) -> Result<(), String> {
    let _admit = crate::sync_verified::note_library_mutation(library_root)?;
    let _txn = ExclusiveLock::acquire(&library_root.join(SYNC_DIR), IDENTITY_LOCK_FILE)?;
    if read_identity(library_root)?.as_ref() != planned_from {
        return Err(
            "REFUSE SYNC IDENTITY: identity.json changed while recovery was planned".into(),
        );
    }
    let bytes = plan
        .identity
        .to_canonical_bytes()
        .map_err(|e| e.to_string())?;
    write_atomic(
        &library_root.join(SYNC_DIR),
        IDENTITY_FILE,
        &file_bytes(&bytes),
        hooks,
    )
}

fn is_within(child: &Path, parent: &Path) -> bool {
    match (fs::canonicalize(child), fs::canonicalize(parent)) {
        (Ok(c), Ok(p)) => c.starts_with(p),
        _ => false,
    }
}

/// Load or create this installation's replica identity under `state_dir`.
///
/// Exactly-once creation under an exclusive lock on `<state_dir>/replica.lock` (see
/// [`ExclusiveLock`]): while holding it, an existing `replica.json` is read and returned — never
/// replaced — otherwise complete bytes are written to a temp file, fsynced, renamed into place,
/// and the directory synced. Concurrent creators serialize, so exactly one ID is ever
/// published and every caller returns it. A crash leaves either no `replica.json` (temp residue
/// is discarded by the next holder) or the complete file; an unparsable `replica.json` is
/// refused, never silently replaced. Refuses a state directory inside the library
/// (`library_root`), since copying the library would then clone the replica.
pub fn ensure_replica_state_with(
    state_dir: &Path,
    library_root: Option<&Path>,
    mint: &mut dyn FnMut() -> Uuid,
) -> Result<ReplicaStateV1, String> {
    fs::create_dir_all(state_dir).map_err(|e| format!("create {}: {e}", state_dir.display()))?;
    if let Some(lib) = library_root {
        if is_within(state_dir, lib) {
            return Err(format!(
                "REFUSE REPLICA STATE: {} is inside the library {}; replica identity must live outside library data",
                state_dir.display(),
                lib.display()
            ));
        }
    }
    let path = state_dir.join(REPLICA_STATE_FILE);
    let read = |p: &Path| -> Result<ReplicaStateV1, String> {
        let bytes = fs::read(p).map_err(|e| format!("read {}: {e}", p.display()))?;
        ReplicaStateV1::parse(&bytes)
            .map_err(|e| format!("REFUSE REPLICA STATE: {}: {e}", p.display()))
    };
    let _txn = ExclusiveLock::acquire(state_dir, REPLICA_LOCK_FILE)?;
    if path.exists() {
        return read(&path);
    }
    let id = ReplicaId::from_uuid(mint()).map_err(|e| e.to_string())?;
    let bytes = ReplicaStateV1::new(id)
        .to_canonical_bytes()
        .map_err(|e| e.to_string())?;
    write_atomic(
        state_dir,
        REPLICA_STATE_FILE,
        &file_bytes(&bytes),
        &SyncWriteHooks::default(),
    )?;
    read(&path)
}

pub fn ensure_replica_state(
    state_dir: &Path,
    library_root: Option<&Path>,
) -> Result<ReplicaStateV1, String> {
    ensure_replica_state_with(state_dir, library_root, &mut Uuid::now_v7)
}

/// Background reference for durable background bytes: all-white paper is `Blank`, anything
/// else a content-addressed gray8 `Blob` (packed or stride layout).
pub fn background_ref_for(
    background: &[u8],
    w: u32,
    h: u32,
    line_length: u32,
) -> Result<BackgroundRef, String> {
    if background.iter().all(|&b| b == WHITE) {
        return Ok(BackgroundRef::Blank);
    }
    let len = background.len() as u64;
    let stride = if len == u64::from(w) * u64::from(h) {
        w
    } else if len == u64::from(line_length) * u64::from(h) {
        line_length
    } else {
        return Err(format!(
            "REFUSE SNAPSHOT: background of {len} bytes matches neither {w}x{h} nor stride {line_length}"
        ));
    };
    Ok(BackgroundRef::Blob {
        sha256: sha256_hex(background),
        byte_len: len,
        format: BackgroundFormat {
            encoding: BackgroundEncoding::Gray8,
            width: w,
            height: h,
            stride,
        },
    })
}

/// Kindle-local stroke ID prefix for fragments produced by a **synced** erase (S2B). The
/// embedded UUID is exactly the reducer's fragment ID, `UUIDv5(operation_id, "<source>/<i>")`,
/// so local and server state converge bit-for-bit. Pre-S2B IDs (`s-<v7>`, legacy) are unchanged.
pub const DERIVED_FRAGMENT_PREFIX: &str = "d-";

/// Local ID for piece `index` of `source` cut by the synced erase `operation_id`.
pub fn derived_fragment_local_id(
    operation_id: sync_protocol::OperationId,
    source: sync_protocol::StrokeId,
    index: usize,
) -> String {
    format!(
        "{DERIVED_FRAGMENT_PREFIX}{}",
        sync_engine_fragment_uuid(operation_id, source, index)
    )
}

fn sync_engine_fragment_uuid(
    operation_id: sync_protocol::OperationId,
    source: sync_protocol::StrokeId,
    index: usize,
) -> Uuid {
    // Must stay identical to `sync_engine::fragment_id` (pinned by the S2B parity tests).
    Uuid::new_v5(
        &operation_id.as_uuid(),
        format!("{source}/{index}").as_bytes(),
    )
}

/// S2C rebase generations: `r<gen>-<uuid>` is a local stroke ID whose global ID is the
/// embedded UUID (any version). Used when canonical ordering forces a page's strokes to be
/// re-journaled in a new order; the generation keeps every local ID unique in the journal.
pub const REBASED_PREFIX: char = 'r';

pub fn rebased_local_stroke_id(generation: u32, id: sync_protocol::StrokeId) -> String {
    format!("{REBASED_PREFIX}{generation}-{id}")
}

/// `Some((generation, uuid))` for an `r<gen>-<uuid>` local ID.
pub fn parse_rebased_local_stroke_id(local: &str) -> Option<(u32, Uuid)> {
    let rest = local.strip_prefix(REBASED_PREFIX)?;
    let (gen, uuid) = rest.split_once('-')?;
    if gen.is_empty() || !gen.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let u = sync_protocol::ids::parse_canonical_uuid("StrokeId", uuid).ok()?;
    Some((gen.parse().ok()?, u))
}

/// Global `StrokeId` → the Kindle local stroke ID that maps back to it exactly (S2C remote
/// materialization): v4/v7 use the S1 UUID-backed form `s-<uuid>`, v5 (reducer fragments,
/// legacy-derived IDs) the S2B `d-<uuid>` form. Never mints a new identity.
pub fn local_stroke_id_for_global(id: sync_protocol::StrokeId) -> String {
    if id.as_uuid().get_version_num() == 5 {
        format!("{DERIVED_FRAGMENT_PREFIX}{id}")
    } else {
        sync_protocol::uuid_backed_local_stroke_id(id)
    }
}

/// Kindle local stroke ID → global `StrokeId`: the S1 rule, plus the S2B `d-<UUIDv5>` family
/// (the embedded UUID is the global ID). This mapping is replica-local; no wire format changes.
pub fn kindle_global_stroke_id(
    page: sync_protocol::PageId,
    local: &str,
) -> Result<sync_protocol::StrokeId, String> {
    if let Some((_, u)) = parse_rebased_local_stroke_id(local) {
        return sync_protocol::StrokeId::from_uuid(u).map_err(|e| e.to_string());
    }
    if let Some(rest) = local.strip_prefix(DERIVED_FRAGMENT_PREFIX) {
        if let Ok(u) = sync_protocol::ids::parse_canonical_uuid("StrokeId", rest) {
            if u.get_version_num() == 5 {
                return sync_protocol::StrokeId::from_uuid(u).map_err(|e| e.to_string());
            }
        }
    }
    global_stroke_id(page, local).map_err(|e| e.to_string())
}

/// Insert explicit live bindings (from synced create operations) into `identity.json` under
/// the identity lock. Never mints: an existing binding must match exactly.
pub fn bind_identities(
    library_root: &Path,
    notebooks: &[(String, sync_protocol::NotebookId)],
    pages: &[(String, String, sync_protocol::PageId)],
) -> Result<bool, String> {
    let _txn = ExclusiveLock::acquire(&library_root.join(SYNC_DIR), IDENTITY_LOCK_FILE)?;
    let mut id = read_identity(library_root)?
        .ok_or("REFUSE SYNC IDENTITY: no identity.json (epoch not established)")?;
    let mut changed = false;
    for (local, nid) in notebooks {
        match id.notebooks.get(local) {
            Some(existing) if existing == nid => {}
            Some(existing) => {
                return Err(format!(
                    "REFUSE SYNC IDENTITY: notebook {local} is bound to {existing}, not {nid}"
                ))
            }
            None => {
                id.notebooks.insert(local.clone(), *nid);
                id.pages.entry(local.clone()).or_default();
                changed = true;
            }
        }
    }
    for (nb, local, pid) in pages {
        let map = id.pages.entry(nb.clone()).or_default();
        match map.get(local) {
            Some(existing) if existing == pid => {}
            Some(existing) => {
                return Err(format!(
                    "REFUSE SYNC IDENTITY: page {nb}/{local} is bound to {existing}, not {pid}"
                ))
            }
            None => {
                map.insert(local.clone(), *pid);
                changed = true;
            }
        }
    }
    if changed {
        let bytes = id.to_canonical_bytes().map_err(|e| e.to_string())?;
        write_atomic(
            &library_root.join(SYNC_DIR),
            IDENTITY_FILE,
            &file_bytes(&bytes),
            &SyncWriteHooks::default(),
        )?;
    }
    Ok(changed)
}

/// Retire bindings of entities that left the live library, **without minting** anything. If a
/// live entity has no binding (a capture was missed) this refuses instead of inventing an ID.
pub fn retire_identities(library_root: &Path) -> Result<(), String> {
    let _txn = ExclusiveLock::acquire(&library_root.join(SYNC_DIR), IDENTITY_LOCK_FILE)?;
    let (_repo, layout) = live_layout(library_root)?;
    let existing = read_identity(library_root)?;
    let mut wanted_mint = false;
    let r = reconcile_identity(existing.as_ref(), &layout, &mut || {
        wanted_mint = true;
        Uuid::now_v7()
    })
    .map_err(|e| e.to_string())?;
    if wanted_mint {
        return Err(
            "REFUSE SYNC IDENTITY: a live notebook/page has no sync binding (missed capture)"
                .into(),
        );
    }
    if r.changed {
        let bytes = r.identity.to_canonical_bytes().map_err(|e| e.to_string())?;
        write_atomic(
            &library_root.join(SYNC_DIR),
            IDENTITY_FILE,
            &file_bytes(&bytes),
            &SyncWriteHooks::default(),
        )?;
    }
    Ok(())
}

/// Public for S2B capture of new pages' render profiles.
pub fn render_ref_for(p: &RenderProfile) -> RenderProfileRef {
    render_ref(p)
}

fn render_ref(p: &RenderProfile) -> RenderProfileRef {
    RenderProfileRef {
        profile_version: p.profile_version,
        ink_byte: p.ink_byte,
        legacy_fixed_radius: p.brush_radius,
        transform_id: p.transform_id.clone(),
        rasterizer_id: p.rasterizer_id.clone(),
        pressure: p.pressure.as_ref().map(|c| PressureCurveRef {
            q_r1_max: c.q_r1_max,
            q_r2_max: c.q_r2_max,
            q_r3_max: c.q_r3_max,
            min_radius: c.min_radius,
            max_radius: c.max_radius,
            missing_fallback_radius: c.missing_fallback_radius,
        }),
    }
}

fn style_of(brush: Option<&str>) -> Result<StrokeStyle, String> {
    match brush {
        None | Some(STYLE_LEGACY_FIXED_V1) => Ok(StrokeStyle::LegacyFixedV1),
        Some(STYLE_PRESSURE_WIDTH_V1) => Ok(StrokeStyle::PressureWidthV1),
        Some(other) => Err(format!("REFUSE SNAPSHOT: unknown stroke style {other:?}")),
    }
}

/// Build the semantic snapshot of a library whose identities are already ensured.
///
/// Reads durable state only (never the framebuffer, never local UI state): library.meta order,
/// notebook titles, notebook.meta page order, and each page's replayed visible strokes with
/// lineage. `active_notebook_id`, `active_page_id`, `next_*_ordinal`, local names, trash and
/// orphans are ignored by construction.
pub fn build_snapshot(
    library_root: &Path,
    identity: &IdentityFileV1,
) -> Result<SyncSnapshotV1, String> {
    build_snapshot_inner(library_root, identity, true, false)
}

/// [`build_snapshot`] without persisting background-identity migrations.
/// Safe against an immutable acquisition: never writes `page.meta`.
pub fn build_snapshot_read_only(
    library_root: &Path,
    identity: &IdentityFileV1,
) -> Result<SyncSnapshotV1, String> {
    build_snapshot_inner(library_root, identity, false, false)
}

/// Like [`build_snapshot_read_only`], but omits live notebooks/pages that have no durable
/// binding. Never mints. Used to compare bound content when extra pages are still unbound.
pub fn build_snapshot_bound_only(
    library_root: &Path,
    identity: &IdentityFileV1,
) -> Result<SyncSnapshotV1, String> {
    build_snapshot_inner(library_root, identity, false, true)
}

fn build_snapshot_inner(
    library_root: &Path,
    identity: &IdentityFileV1,
    persist_background_identity: bool,
    skip_unbound: bool,
) -> Result<SyncSnapshotV1, String> {
    let (repo, layout) = live_layout(library_root)?;
    let folder_file =
        crate::folders::read_folders(library_root).map_err(|e| format!("REFUSE SNAPSHOT: {e}"))?;
    let (folders, members) = crate::folders::snapshot_folders(
        &folder_file,
        identity.library_id,
        &repo.manifest().notebook_ids,
    );
    let mut notebooks = Vec::new();
    for (nb_local, page_locals) in &layout.notebooks {
        let notebook_id = match identity.notebook_id(nb_local) {
            Some(id) => id,
            None if skip_unbound => continue,
            None => {
                return Err(format!(
                    "REFUSE SNAPSHOT: notebook {nb_local} has no sync identity (run identity first)"
                ))
            }
        };
        let nb_dir = repo.notebook_dir(nb_local);
        let mut pages = Vec::new();
        for page_local in page_locals {
            let page_id = match identity.page_id(nb_local, page_local) {
                Some(id) => id,
                None if skip_unbound => continue,
                None => {
                    return Err(format!(
                        "REFUSE SNAPSHOT: page {nb_local}/{page_local} has no sync identity"
                    ))
                }
            };
            // Slice 19B.3B: per-page cost (startup breakdown; no page identity is recorded).
            let t_page = std::time::Instant::now();
            crate::sync_startup_diag::page_begin();
            let page = page_snapshot(&nb_dir, page_local, page_id, persist_background_identity);
            crate::sync_startup_diag::page_end(t_page.elapsed());
            pages.push(page?);
        }
        if skip_unbound && pages.is_empty() {
            continue;
        }
        notebooks.push(SnapshotNotebook {
            notebook_id,
            title: repo.title(nb_local)?.to_string(),
            pages,
            folder_id: members.get(nb_local).copied(),
        });
    }
    crate::store::record_snapshot_counts(
        notebooks.len(),
        notebooks.iter().map(|n| n.pages.len()).sum(),
    );
    let snap = SyncSnapshotV1 {
        v: SNAPSHOT_VERSION,
        library_id: identity.library_id,
        page_space: PageSpace::CANONICAL,
        notebooks,
        folders,
    };
    snap.validate().map_err(|e| {
        format!(
            "REFUSE SNAPSHOT: {library}: {e}",
            library = library_root.display()
        )
    })?;
    Ok(snap)
}

/// One page's canonical snapshot from its durable files.
fn page_snapshot(
    nb_dir: &Path,
    page_local: &str,
    page_id: PageId,
    persist_background_identity: bool,
) -> Result<SnapshotPage, String> {
    let mut page = load_page_journals_read_only(nb_dir, page_local)?;
    let dir = page_dir(nb_dir, page_local);
    let background = if persist_background_identity {
        resolve_background_ref(&dir, &mut page.header)?
    } else {
        resolve_background_ref_read_only(&dir, &page.header)?
    };
    let mut strokes = Vec::with_capacity(page.strokes.len());
    for s in &page.strokes {
        let local = s.id.as_str();
        let root_local = page.root_of.get(local).map(String::as_str).unwrap_or(local);
        strokes.push(SyncStroke {
            stroke_id: kindle_global_stroke_id(page_id, local)?,
            root_id: kindle_global_stroke_id(page_id, root_local)?,
            style: style_of(s.brush.as_deref())?,
            points: s
                .points
                .iter()
                .map(|p| SyncPoint::new(p.x, p.y, p.pressure))
                .collect(),
        });
    }
    Ok(SnapshotPage {
        page_id,
        background,
        render: render_ref(&page.render_profile),
        strokes,
        texts: crate::store::load_page_texts(&dir)?,
        links: crate::store::load_page_links(&dir)?,
    })
}

/// Verify the library files against `expected` for one inbound transaction: the whole library
/// **structure** (notebook order, titles, page order — manifests only) and the full content of
/// the `pages` the transaction touched. Pages outside `pages` are neither read nor hashed: they
/// were verified when last changed and this transaction did not write them (startup
/// reconciliation still compares the complete library on every launch).
pub fn verify_scoped(
    library_root: &Path,
    identity: &IdentityFileV1,
    expected: &SyncSnapshotV1,
    pages: &std::collections::HashSet<PageId>,
) -> Result<(), String> {
    let (repo, layout) = live_layout(library_root)?;
    let mismatch = |what: String| format!("MATERIALIZE_VERIFY: {what}");
    let folder_file = crate::folders::read_folders(library_root).map_err(mismatch)?;
    let (folders, members) = crate::folders::snapshot_folders(
        &folder_file,
        identity.library_id,
        &repo.manifest().notebook_ids,
    );
    if folders != expected.folders {
        return Err(mismatch("folders differ from the expected state".into()));
    }
    if layout.notebooks.len() != expected.notebooks.len() {
        return Err(mismatch(format!(
            "{} local notebooks, {} expected",
            layout.notebooks.len(),
            expected.notebooks.len()
        )));
    }
    for ((nb_local, page_locals), en) in layout.notebooks.iter().zip(&expected.notebooks) {
        if identity.notebook_id(nb_local) != Some(en.notebook_id) {
            return Err(mismatch(format!("notebook order differs at {nb_local}")));
        }
        if repo.title(nb_local)? != en.title {
            return Err(mismatch(format!("title of {nb_local} differs")));
        }
        if members.get(nb_local).copied() != en.folder_id {
            return Err(mismatch(format!("folder of {nb_local} differs")));
        }
        if page_locals.len() != en.pages.len() {
            return Err(mismatch(format!("page count of {nb_local} differs")));
        }
        let nb_dir = repo.notebook_dir(nb_local);
        for (page_local, ep) in page_locals.iter().zip(&en.pages) {
            if identity.page_id(nb_local, page_local) != Some(ep.page_id) {
                return Err(mismatch(format!(
                    "page order of {nb_local} differs at {page_local}"
                )));
            }
            if pages.contains(&ep.page_id)
                && page_snapshot(&nb_dir, page_local, ep.page_id, true)? != *ep
            {
                return Err(mismatch(format!(
                    "page {nb_local}/{page_local} differs from the expected canonical state"
                )));
            }
        }
    }
    Ok(())
}

/// Canonical bytes + hash of a snapshot.
pub fn snapshot_bytes_and_hash(snap: &SyncSnapshotV1) -> Result<(Vec<u8>, Sha256Hex), String> {
    let bytes = snap.to_canonical_bytes().map_err(|e| e.to_string())?;
    let hash = snapshot_sha256(snap).map_err(|e| e.to_string())?;
    Ok((bytes, hash))
}
