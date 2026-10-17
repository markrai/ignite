//! Sync Phase S2C: the Kindle **LocalMaterializer** — canonical sync state → Kindle files.
//!
//! Separate from the reducer (`sync-engine`), the network and the UI. Given the expected
//! canonical `SyncSnapshotV1` (server base + local pending overlay), it makes the durable
//! Kindle library represent exactly that state, using the product's own durable primitives:
//!
//! * notebooks/pages are matched by **global identity** (`identity.json`), never by local name;
//!   new ones get fresh monotonic local ordinals (`notebook-N`/`page-N`, never reused), bound to
//!   their global IDs *before* their files are created so a crash can never leave a live
//!   unbound entity;
//! * deletes use the F8/F10 quarantine paths; order changes rewrite manifests only;
//! * folders (`folders.json`) are rewritten to state the expected folders and memberships,
//!   keeping each existing folder's local id (matched by global identity);
//! * strokes are reconciled with the page journals: new strokes are appended under the local ID
//!   that round-trips to their exact global ID; erase, undo, and
//!   [`sync_protocol::SyncOperationV1::ReplaceStrokeLineages`] are `replace_strokes` records
//!   whose fragment IDs are the reducer's (caller-supplied for replacements, UUIDv5 for erase).
//!
//! It is a *diff* against the actual files, so it is idempotent: re-running after a crash only
//! performs what is still missing. It never authors sync operations (no OperationId, no
//! client_seq, no outbox): mutation origin is RemoteMaterialization by construction.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use notebook_core::{Point, Stroke, StrokeId as LocalStrokeId};
use sync_protocol::{
    sha256_hex, BackgroundEncoding, BackgroundRef, NotebookId, PageId, RenderProfileRef,
    SnapshotPage, StrokeId, StrokeStyle, SyncSnapshotV1, SyncStroke,
};

use crate::library::{
    allocate_fresh_notebook_id, create_notebook_at_durable, delete_notebook_durable,
    open_library_read_only, read_notebook_manifest_read_only, rename_notebook_durable,
    reorder_notebooks_durable, DeleteNotebookHooks, LibraryRepository, RenameNotebookHooks,
};
use crate::notebook::{
    allocate_fresh_page_id, insert_page_at_durable, quarantine_page_dir, remove_page_durable,
    reorder_pages_durable,
};
use crate::pressure_curve::{STYLE_LEGACY_FIXED_V1, STYLE_PRESSURE_WIDTH_V1};
use crate::profile::RenderProfile;
use crate::store::{
    create_new_page, load_existing_page, load_page_journals_read_only, load_page_links,
    load_page_texts, page_dir, page_exists, resolve_background_ref, write_page_links,
    write_page_texts, PageStore, PersistedFragmentV2, PersistedPointV1, PersistedReplacementV2,
};
use crate::sync_local::{
    bind_identities, kindle_global_stroke_id, local_stroke_id_for_global,
    parse_rebased_local_stroke_id, read_identity, rebased_local_stroke_id, render_ref_for,
    retire_identities,
};
use crate::ui_gfx::WHITE;

/// Panel geometry used for newly created blank pages (and required of blob backgrounds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceGeometry {
    pub width: u32,
    pub height: u32,
    pub line_length: u32,
}

/// (width, height, stride) of a page background.
type PanelDims = (u32, u32, u32);

/// Deterministic crash-point hook (tests). `Err` aborts the reconciliation at that point.
pub type Hook<'a> = &'a dyn Fn(&str) -> Result<(), String>;

pub fn no_hook(_: &str) -> Result<(), String> {
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaterializeReport {
    pub notebooks_created: usize,
    pub notebooks_deleted: usize,
    pub notebooks_renamed: usize,
    pub notebooks_reordered: bool,
    pub pages_created: usize,
    pub pages_deleted: usize,
    pub pages_reordered: usize,
    pub strokes_appended: usize,
    pub strokes_replaced: usize,
    pub texts_written: usize,
    pub links_written: usize,
    pub folders_changed: bool,
}

impl MaterializeReport {
    pub fn changed(&self) -> bool {
        *self != Self::default()
    }
}

pub struct LocalMaterializer<'a> {
    pub library_root: &'a Path,
    pub device: DeviceGeometry,
    /// Verified content-addressed background blobs (`<sync-dir>/blobs`).
    pub blob_dir: &'a Path,
    pub hook: Hook<'a>,
    /// Pages whose strokes may differ (an inbound transaction's touched pages). `None` = all.
    /// Structure (notebooks, titles, page order) is always reconciled; stroke reconciliation —
    /// which reads the page and its background — only runs for these pages and pages created
    /// in this run.
    pub scope: Option<&'a HashSet<PageId>>,
}

fn err(msg: impl Into<String>) -> String {
    format!("MATERIALIZE: {}", msg.into())
}

/// The sealed local profile a `RenderProfileRef` names (refuses anything unknown).
pub fn profile_for(render: &RenderProfileRef) -> Result<RenderProfile, String> {
    for p in [
        RenderProfile::sealed_v2_pressure(render.ink_byte),
        RenderProfile::sealed_v1(render.ink_byte),
    ]
    .into_iter()
    .flatten()
    {
        if render_ref_for(&p) == *render {
            return Ok(p);
        }
    }
    Err(err(format!("unsupported render profile {render:?}")))
}

impl LocalMaterializer<'_> {
    /// Exact background bytes + (width, height, stride) for a page spec. Blobs must already be
    /// verified into `blob_dir`; they are re-hashed here (never trust a file name).
    fn background(&self, bg: &BackgroundRef) -> Result<(Vec<u8>, PanelDims), String> {
        let d = self.device;
        match bg {
            // The page header always carries this panel's line_length (the runtime refuses any
            // other stride); the bytes may be packed (w*h) or strided — both are accepted.
            BackgroundRef::Blank => Ok((
                vec![WHITE; (d.width as usize) * (d.height as usize)],
                (d.width, d.height, d.line_length),
            )),
            BackgroundRef::Blob {
                sha256,
                byte_len,
                format,
            } => {
                let BackgroundEncoding::Gray8 = format.encoding;
                if format.width != d.width
                    || format.height != d.height
                    || !(format.stride == d.width || format.stride == d.line_length)
                {
                    return Err(err(format!(
                        "background {sha256} is {}x{} stride {} but this panel is {}x{} stride {}",
                        format.width,
                        format.height,
                        format.stride,
                        d.width,
                        d.height,
                        d.line_length
                    )));
                }
                let path = self.blob_dir.join(sha256.as_str());
                let bytes = std::fs::read(&path)
                    .map_err(|e| err(format!("background blob {sha256} unavailable: {e}")))?;
                if bytes.len() as u64 != *byte_len || sha256_hex(&bytes) != *sha256 {
                    return Err(err(format!("background blob {sha256} failed verification")));
                }
                // The runtime scrubs the reserved chrome band of any background it opens (F11);
                // a remote background it would rewrite could never match the canonical state.
                let geo = kindle_platform::FbGeometry {
                    xres: format.width,
                    yres: format.height,
                    xres_virtual: format.width,
                    yres_virtual: format.height,
                    xoffset: 0,
                    yoffset: 0,
                    bits_per_pixel: 8,
                    line_length: d.line_length,
                    smem_len: d.line_length.saturating_mul(format.height),
                };
                let mut probe = bytes.clone();
                if crate::page_label::scrub_chrome_band(&mut probe, &geo)? {
                    return Err(err(format!(
                        "background {sha256} has pixels in the reserved chrome band (the runtime would rewrite it): unsupported"
                    )));
                }
                Ok((bytes, (format.width, format.height, d.line_length)))
            }
        }
    }

    /// Make the library represent `expected` exactly. Idempotent.
    pub fn reconcile(&self, expected: &SyncSnapshotV1) -> Result<MaterializeReport, String> {
        crate::sync_verified::permit_sync_mutations(|| self.reconcile_inner(expected))
    }

    fn reconcile_inner(&self, expected: &SyncSnapshotV1) -> Result<MaterializeReport, String> {
        let root = self.library_root;
        let mut rep = MaterializeReport::default();
        let mut lib = open_library_read_only(root)?;
        let ident = read_identity(root)?.ok_or_else(|| err("no identity.json"))?;
        if ident.library_id != expected.library_id {
            return Err(err("library identity mismatch"));
        }
        let mut nb_local: HashMap<NotebookId, String> = ident
            .notebooks
            .iter()
            .map(|(l, g)| (*g, l.clone()))
            .collect();
        let is_live =
            |lib: &LibraryRepository, l: &str| lib.manifest().notebook_ids.iter().any(|x| x == l);

        // 1. Create missing notebooks (bind first, then files, then library order).
        for (i, en) in expected.notebooks.iter().enumerate() {
            if nb_local
                .get(&en.notebook_id)
                .is_some_and(|l| is_live(&lib, l))
            {
                continue;
            }
            let local = nb_local
                .get(&en.notebook_id)
                .cloned()
                .unwrap_or_else(|| allocate_fresh_notebook_id(&lib));
            let p0 = &en.pages[0];
            let first_local = ident
                .pages
                .get(&local)
                .and_then(|m| {
                    m.iter()
                        .find(|(_, g)| **g == p0.page_id)
                        .map(|(l, _)| l.clone())
                })
                .unwrap_or_else(|| "page-1".to_string());
            bind_identities(
                root,
                &[(local.clone(), en.notebook_id)],
                &[(local.clone(), first_local.clone(), p0.page_id)],
            )?;
            (self.hook)("after_bind_notebook")?;
            let (bytes, dims) = self.background(&p0.background)?;
            let profile = profile_for(&p0.render)?;
            let index = match i.checked_sub(1) {
                None => 0,
                Some(prev) => {
                    let prev_local = &nb_local[&expected.notebooks[prev].notebook_id];
                    lib.manifest()
                        .notebook_ids
                        .iter()
                        .position(|x| x == prev_local)
                        .map_or(0, |p| p + 1)
                }
            };
            create_notebook_at_durable(
                &mut lib,
                &local,
                &en.title,
                index,
                &first_local,
                dims,
                &bytes,
                &profile,
            )?;
            (self.hook)("after_create_notebook")?;
            nb_local.insert(en.notebook_id, local);
            rep.notebooks_created += 1;
        }

        // 2. Delete notebooks the canonical state no longer has (quarantine path).
        let wanted: HashSet<NotebookId> =
            expected.notebooks.iter().map(|n| n.notebook_id).collect();
        let live: Vec<String> = lib.manifest().notebook_ids.clone();
        for l in &live {
            let gid = ident
                .notebooks
                .get(l)
                .copied()
                .or_else(|| nb_local.iter().find(|(_, v)| *v == l).map(|(g, _)| *g))
                .ok_or_else(|| err(format!("live notebook {l} has no sync identity")))?;
            if !wanted.contains(&gid) {
                delete_notebook_durable(&mut lib, l, &DeleteNotebookHooks::default())?;
                rep.notebooks_deleted += 1;
            }
        }

        // 3. Library order + titles.
        let order: Vec<String> = expected
            .notebooks
            .iter()
            .map(|n| nb_local[&n.notebook_id].clone())
            .collect();
        if lib.manifest().notebook_ids != order {
            reorder_notebooks_durable(&mut lib, &order)?;
            rep.notebooks_reordered = true;
        }
        for en in &expected.notebooks {
            let l = &nb_local[&en.notebook_id];
            if lib.title(l)? != en.title {
                rename_notebook_durable(&mut lib, l, &en.title, &RenameNotebookHooks::default())?;
                rep.notebooks_renamed += 1;
            }
        }

        // 3b. Folders + memberships (one atomic file write, only when it differs).
        let current = crate::folders::read_folders(root).map_err(err)?;
        let members: Vec<(String, sync_protocol::FolderId)> = expected
            .notebooks
            .iter()
            .filter_map(|n| n.folder_id.map(|f| (nb_local[&n.notebook_id].clone(), f)))
            .collect();
        let folders = crate::folders::materialized(
            &current,
            expected.library_id,
            &expected.folders,
            &members,
        );
        if folders != current {
            crate::folders::write_materialized(root, &folders).map_err(err)?;
            (self.hook)("after_folders")?;
            rep.folders_changed = true;
        }

        // 4. Pages, then 5. strokes, per notebook.
        for en in &expected.notebooks {
            let nbl = nb_local[&en.notebook_id].clone();
            self.pages(&lib, &nbl, en.notebook_id, &en.pages, &mut rep)?;
        }

        // 6. Drop bindings of entities that left the live library (never mints).
        retire_identities(root)?;
        Ok(rep)
    }

    fn pages(
        &self,
        lib: &LibraryRepository,
        nbl: &str,
        _nb: NotebookId,
        pages: &[SnapshotPage],
        rep: &mut MaterializeReport,
    ) -> Result<(), String> {
        let root = self.library_root;
        let dir = lib.notebook_dir(nbl);
        let mut manifest = read_notebook_manifest_read_only(&dir)?;
        let bindings: BTreeMap<String, PageId> = read_identity(root)?
            .and_then(|i| i.pages.get(nbl).cloned())
            .unwrap_or_default();
        let mut page_local: HashMap<PageId, String> =
            bindings.iter().map(|(l, g)| (*g, l.clone())).collect();
        let mut created: HashSet<PageId> = HashSet::new();

        for (j, ep) in pages.iter().enumerate() {
            let live = page_local
                .get(&ep.page_id)
                .is_some_and(|l| manifest.page_ids.contains(l));
            if live {
                continue;
            }
            let local = page_local
                .get(&ep.page_id)
                .cloned()
                .unwrap_or_else(|| allocate_fresh_page_id(&dir, &manifest));
            bind_identities(root, &[], &[(nbl.to_string(), local.clone(), ep.page_id)])?;
            (self.hook)("after_bind_page")?;
            if page_exists(&dir, &local) || dir.join(&local).exists() {
                // Partial files of an interrupted create: never attach them.
                quarantine_page_dir(&dir, &local)?;
            }
            let (bytes, (w, h, stride)) = self.background(&ep.background)?;
            let profile = profile_for(&ep.render)?;
            let (store, _) = create_new_page(&dir, &local, w, h, stride, &bytes, &profile)?;
            drop(store);
            (self.hook)("after_create_page_files")?;
            let index = match j.checked_sub(1) {
                None => 0,
                Some(prev) => {
                    let pl = &page_local[&pages[prev].page_id];
                    manifest
                        .page_ids
                        .iter()
                        .position(|x| x == pl)
                        .map_or(0, |p| p + 1)
                }
            };
            insert_page_at_durable(&dir, &mut manifest, &local, index)?;
            page_local.insert(ep.page_id, local);
            created.insert(ep.page_id);
            rep.pages_created += 1;
        }

        let wanted: HashSet<PageId> = pages.iter().map(|p| p.page_id).collect();
        for l in manifest.page_ids.clone() {
            let gid = page_local
                .iter()
                .find(|(_, v)| **v == l)
                .map(|(g, _)| *g)
                .ok_or_else(|| err(format!("live page {nbl}/{l} has no sync identity")))?;
            if !wanted.contains(&gid) {
                remove_page_durable(&dir, &mut manifest, &l)?;
                rep.pages_deleted += 1;
            }
        }
        let order: Vec<String> = pages
            .iter()
            .map(|p| page_local[&p.page_id].clone())
            .collect();
        if manifest.page_ids != order {
            reorder_pages_durable(&dir, &mut manifest, &order)?;
            rep.pages_reordered += 1;
        }

        for ep in pages {
            if self.scope.is_none_or(|s| s.contains(&ep.page_id)) || created.contains(&ep.page_id) {
                self.strokes(&dir, &page_local[&ep.page_id], ep, rep)?;
                self.texts(&dir, &page_local[&ep.page_id], ep, rep)?;
                self.links(&dir, &page_local[&ep.page_id], ep, rep)?;
            }
        }
        Ok(())
    }

    fn strokes(
        &self,
        dir: &Path,
        page_local: &str,
        ep: &SnapshotPage,
        rep: &mut MaterializeReport,
    ) -> Result<(), String> {
        let mut page = load_page_journals_read_only(dir, page_local)?;
        let bg = resolve_background_ref(&page_dir(dir, page_local), &mut page.header)?;
        if bg != ep.background || render_ref_for(&page.render_profile) != ep.render {
            return Err(err(format!(
                "page {page_local}: background/render differ from canonical state (unsupported)"
            )));
        }
        let mut actual = Vec::with_capacity(page.strokes.len());
        let mut local_of: HashMap<StrokeId, String> = HashMap::new();
        for s in &page.strokes {
            let local = s.id.as_str();
            let root_local = page.root_of.get(local).map(String::as_str).unwrap_or(local);
            let gid = kindle_global_stroke_id(ep.page_id, local)?;
            local_of.insert(gid, local.to_string());
            actual.push(SyncStroke {
                stroke_id: gid,
                root_id: kindle_global_stroke_id(ep.page_id, root_local)?,
                style: match s.brush.as_deref() {
                    None | Some(STYLE_LEGACY_FIXED_V1) => StrokeStyle::LegacyFixedV1,
                    Some(STYLE_PRESSURE_WIDTH_V1) => StrokeStyle::PressureWidthV1,
                    Some(o) => return Err(err(format!("unknown local style {o}"))),
                },
                points: s
                    .points
                    .iter()
                    .map(|p| sync_protocol::SyncPoint::new(p.x, p.y, p.pressure))
                    .collect(),
            });
        }
        if actual == ep.strokes {
            return Ok(());
        }
        let (mut store, _) = load_existing_page(dir, page_local)?;
        // Local IDs are never reused within a page journal: the next rebase generation is
        // above every generation the journal has ever contained.
        let used: HashSet<String> = store.ids_ever_used().clone();
        let gen = 1 + used
            .iter()
            .filter_map(|l| parse_rebased_local_stroke_id(l).map(|(g, _)| g))
            .max()
            .unwrap_or(0);
        let local_for = |g: StrokeId| {
            let d = local_stroke_id_for_global(g);
            if used.contains(&d) {
                rebased_local_stroke_id(gen, g)
            } else {
                d
            }
        };
        let actions = match plan_page_strokes(&actual, &ep.strokes) {
            Ok(a) => a,
            Err(_) => {
                // Canonical order differs from the local journal order (a rebase moved remote
                // strokes before pending local ones). Re-journal the page in canonical order.
                return rebuild_page(&mut store, &page.strokes, &ep.strokes, gen, self.hook, rep);
            }
        };
        let mut replacements = Vec::new();
        for a in &actions {
            match a {
                StrokeAction::Replace { target, fragments } => {
                    replacements.push(PersistedReplacementV2 {
                        target_stroke_id: local_of[target].clone(),
                        fragments: fragments
                            .iter()
                            .map(|f| fragment(f, &local_for(f.stroke_id)))
                            .collect(),
                    });
                }
                StrokeAction::Rewrite { target, stroke } => {
                    let local = local_of[target].clone();
                    replacements.push(PersistedReplacementV2 {
                        target_stroke_id: local.clone(),
                        fragments: vec![fragment(stroke, &local)],
                    });
                }
                StrokeAction::Append(_) | StrokeAction::Phantom { .. } => {}
            }
        }
        if !replacements.is_empty() {
            store.append_replace_strokes(&replacements)?;
            rep.strokes_replaced += replacements.len();
            (self.hook)("after_erase_journal")?;
        }
        for a in actions {
            match a {
                StrokeAction::Replace { .. } | StrokeAction::Rewrite { .. } => {}
                StrokeAction::Append(s) => {
                    let local = local_for(s.stroke_id);
                    store.append_committed_stroke(&local_stroke(&s, &local)?)?;
                    rep.strokes_appended += 1;
                    (self.hook)("after_stroke_journal")?;
                }
                StrokeAction::Phantom { root, fragments } => {
                    // A remote stroke that was added and then erased before we saw it: its
                    // root never existed locally. Journal the root (geometry of its first
                    // fragment; invisible once replaced) then the canonical fragments.
                    append_phantom(&mut store, root, &fragments, &local_for)?;
                    rep.strokes_appended += 1;
                    rep.strokes_replaced += 1;
                    (self.hook)("after_erase_journal")?;
                }
            }
        }
        Ok(())
    }

    fn texts(
        &self,
        dir: &Path,
        page_local: &str,
        ep: &SnapshotPage,
        rep: &mut MaterializeReport,
    ) -> Result<(), String> {
        let page = page_dir(dir, page_local);
        let actual = load_page_texts(&page)?;
        if actual == ep.texts {
            return Ok(());
        }
        write_page_texts(&page, &ep.texts)?;
        rep.texts_written += 1;
        (self.hook)("after_texts_journal")?;
        Ok(())
    }

    fn links(
        &self,
        dir: &Path,
        page_local: &str,
        ep: &SnapshotPage,
        rep: &mut MaterializeReport,
    ) -> Result<(), String> {
        let page = page_dir(dir, page_local);
        let actual = load_page_links(&page)?;
        if actual == ep.links {
            return Ok(());
        }
        write_page_links(&page, &ep.links)?;
        rep.links_written += 1;
        (self.hook)("after_links_journal")?;
        Ok(())
    }
}

fn style_name(s: StrokeStyle) -> &'static str {
    match s {
        StrokeStyle::LegacyFixedV1 => STYLE_LEGACY_FIXED_V1,
        StrokeStyle::PressureWidthV1 => STYLE_PRESSURE_WIDTH_V1,
    }
}

/// A lineage never visible locally: journal its root (geometry of its first fragment; invisible
/// once replaced), then replace it with the canonical fragments.
fn append_phantom(
    store: &mut crate::store::FsPageStore,
    root: StrokeId,
    fragments: &[SyncStroke],
    local_id: &dyn Fn(StrokeId) -> String,
) -> Result<(), String> {
    let first = &fragments[0];
    let phantom = SyncStroke {
        stroke_id: root,
        root_id: root,
        style: first.style,
        points: first.points.clone(),
    };
    store.append_committed_stroke(&local_stroke(&phantom, &local_id(root))?)?;
    store.append_replace_strokes(&[PersistedReplacementV2 {
        target_stroke_id: local_id(root),
        fragments: fragments
            .iter()
            .map(|f| fragment(f, &local_id(f.stroke_id)))
            .collect(),
    }])?;
    Ok(())
}

/// Canonical re-journal of a page (append-only): one `replace_strokes` record removes every
/// visible stroke, then the expected list is appended in canonical order under rebase
/// generation `gen` (`r<gen>-<uuid>` local IDs, which map to the same global IDs). Idempotent
/// across crashes: a partial rebuild is simply rebuilt again with the next generation.
fn rebuild_page(
    store: &mut crate::store::FsPageStore,
    visible: &[Stroke],
    expected: &[SyncStroke],
    gen: u32,
    hook: Hook<'_>,
    rep: &mut MaterializeReport,
) -> Result<(), String> {
    if !visible.is_empty() {
        store.append_replace_strokes(
            &visible
                .iter()
                .map(|s| PersistedReplacementV2 {
                    target_stroke_id: s.id.as_str().to_string(),
                    fragments: Vec::new(),
                })
                .collect::<Vec<_>>(),
        )?;
        rep.strokes_replaced += visible.len();
        hook("after_erase_journal")?;
    }
    let local = |g: StrokeId| rebased_local_stroke_id(gen, g);
    let mut i = 0;
    while i < expected.len() {
        let s = &expected[i];
        if s.root_id == s.stroke_id {
            store.append_committed_stroke(&local_stroke(s, &local(s.stroke_id))?)?;
            rep.strokes_appended += 1;
            hook("after_stroke_journal")?;
            i += 1;
            continue;
        }
        let root = s.root_id;
        let mut group = Vec::new();
        while i < expected.len() && expected[i].root_id == root && expected[i].stroke_id != root {
            group.push(expected[i].clone());
            i += 1;
        }
        append_phantom(store, root, &group, &local)?;
        rep.strokes_appended += 1;
        rep.strokes_replaced += 1;
        hook("after_erase_journal")?;
    }
    Ok(())
}

/// Canonical stroke → local durable stroke (exact geometry; `local` maps back to its global ID).
fn local_stroke(s: &SyncStroke, local: &str) -> Result<Stroke, String> {
    if s.root_id != s.stroke_id {
        return Err(err("an appended stroke must be its own root"));
    }
    Ok(Stroke {
        id: LocalStrokeId::new(local.to_string()),
        points: s
            .points
            .iter()
            .map(|p| {
                let mut pt = Point::new(p.x, p.y);
                pt.pressure = p.pressure;
                pt
            })
            .collect(),
        brush: Some(style_name(s.style).to_string()),
        finished: true,
    })
}

fn fragment(s: &SyncStroke, local: &str) -> PersistedFragmentV2 {
    PersistedFragmentV2 {
        id: local.to_string(),
        style: Some(style_name(s.style).to_string()),
        points: s
            .points
            .iter()
            .map(|p| PersistedPointV1 {
                x: p.x,
                y: p.y,
                pressure: p.pressure,
            })
            .collect(),
    }
}

/// One journal-expressible step from the actual to the expected visible stroke list.
#[derive(Clone, Debug, PartialEq)]
pub enum StrokeAction {
    /// Replace a visible stroke by fragments of its lineage (empty = lineage/stroke removed).
    Replace {
        target: StrokeId,
        fragments: Vec<SyncStroke>,
    },
    /// Rewrite one live stroke's canonical geometry while retaining its exact identity/root.
    Rewrite {
        target: StrokeId,
        stroke: SyncStroke,
    },
    /// Append a new stroke at the end of the page.
    Append(SyncStroke),
    /// Append fragments whose root was never visible locally.
    Phantom {
        root: StrokeId,
        fragments: Vec<SyncStroke>,
    },
}

/// Plan the page-journal records turning `actual` into `expected` (both in render order).
///
/// Reducer semantics make every change expressible: surviving strokes keep their relative
/// order and bytes; erase/undo replace a stroke in place by fragments sharing its root; new
/// strokes (and fragments of lineages never seen locally) are appended at the end. Anything
/// else (reordered or rewritten strokes) is refused rather than guessed.
pub fn plan_page_strokes(
    actual: &[SyncStroke],
    expected: &[SyncStroke],
) -> Result<Vec<StrokeAction>, String> {
    let e_ids: HashSet<StrokeId> = expected.iter().map(|s| s.stroke_id).collect();
    let a_ids: HashSet<StrokeId> = actual.iter().map(|s| s.stroke_id).collect();
    let kept_a: Vec<usize> = (0..actual.len())
        .filter(|&i| e_ids.contains(&actual[i].stroke_id))
        .collect();
    let kept_e: Vec<usize> = (0..expected.len())
        .filter(|&i| a_ids.contains(&expected[i].stroke_id))
        .collect();
    if kept_a.len() != kept_e.len()
        || kept_a.iter().zip(&kept_e).any(|(&a, &e)| {
            actual[a].stroke_id != expected[e].stroke_id
                || actual[a].root_id != expected[e].root_id
                || actual[a].style != expected[e].style
        })
    {
        return Err(err(
            "surviving strokes were reordered or changed identity/style (unsupported)",
        ));
    }
    let mut actions = Vec::new();
    for (&a, &e) in kept_a.iter().zip(&kept_e) {
        if actual[a] != expected[e] {
            actions.push(StrokeAction::Rewrite {
                target: actual[a].stroke_id,
                stroke: expected[e].clone(),
            });
        }
    }
    let gaps = kept_a.len() + 1;
    for g in 0..gaps {
        let a_lo = if g == 0 { 0 } else { kept_a[g - 1] + 1 };
        let a_hi = if g < kept_a.len() {
            kept_a[g]
        } else {
            actual.len()
        };
        let e_lo = if g == 0 { 0 } else { kept_e[g - 1] + 1 };
        let e_hi = if g < kept_e.len() {
            kept_e[g]
        } else {
            expected.len()
        };
        let removed = &actual[a_lo..a_hi];
        let mut frags: Vec<Vec<SyncStroke>> = vec![Vec::new(); removed.len()];
        let mut ptr = 0;
        let mut tail: Vec<SyncStroke> = Vec::new();
        for n in &expected[e_lo..e_hi] {
            let own = removed[ptr..].iter().position(|r| r.root_id == n.root_id);
            match own {
                Some(off) if tail.is_empty() => {
                    ptr += off;
                    frags[ptr].push(n.clone());
                }
                _ if g == gaps - 1 => tail.push(n.clone()),
                _ => return Err(err("a new stroke would be inserted mid-page (unsupported)")),
            }
        }
        for (r, f) in removed.iter().zip(frags) {
            actions.push(StrokeAction::Replace {
                target: r.stroke_id,
                fragments: f,
            });
        }
        let mut i = 0;
        while i < tail.len() {
            let s = &tail[i];
            if s.root_id == s.stroke_id {
                actions.push(StrokeAction::Append(s.clone()));
                i += 1;
                continue;
            }
            if a_ids.contains(&s.root_id) || e_ids.contains(&s.root_id) {
                return Err(err("fragment of a visible lineage appended (unsupported)"));
            }
            let root = s.root_id;
            let mut group = Vec::new();
            while i < tail.len() && tail[i].root_id == root && tail[i].stroke_id != root {
                group.push(tail[i].clone());
                i += 1;
            }
            actions.push(StrokeAction::Phantom {
                root,
                fragments: group,
            });
        }
    }
    Ok(actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sync_protocol::SyncPoint;
    use uuid::Uuid;

    fn sid(n: u128) -> StrokeId {
        StrokeId::from_uuid(Uuid::from_u128((0x0190_0000_0000_7000_8000_u128 << 48) | n)).unwrap()
    }

    fn s(id: u128, root: u128) -> SyncStroke {
        SyncStroke {
            stroke_id: sid(id),
            root_id: sid(root),
            style: StrokeStyle::PressureWidthV1,
            points: vec![SyncPoint::new(id as f32, 1.0, Some(0.5))],
        }
    }

    #[test]
    fn plans_append_erase_undo_and_phantom() {
        // actual [A, B, C] → expected [A, B1, B2, D, X1, X2] : B erased into 2, C undone,
        // D appended, X added-then-erased elsewhere.
        let actual = vec![s(1, 1), s(2, 2), s(3, 3)];
        let expected = vec![s(1, 1), s(21, 2), s(22, 2), s(4, 4), s(51, 5), s(52, 5)];
        let plan = plan_page_strokes(&actual, &expected).unwrap();
        assert_eq!(
            plan,
            vec![
                StrokeAction::Replace {
                    target: sid(2),
                    fragments: vec![s(21, 2), s(22, 2)]
                },
                StrokeAction::Replace {
                    target: sid(3),
                    fragments: vec![]
                },
                StrokeAction::Append(s(4, 4)),
                StrokeAction::Phantom {
                    root: sid(5),
                    fragments: vec![s(51, 5), s(52, 5)]
                },
            ]
        );
        assert!(plan_page_strokes(&expected, &expected).unwrap().is_empty());
    }

    #[test]
    fn plans_multi_root_replacement_as_one_gap_of_replaces() {
        let actual = vec![s(1, 1), s(2, 2), s(3, 3)];
        let expected = vec![s(11, 1), s(21, 2), s(31, 3)];
        assert_eq!(
            plan_page_strokes(&actual, &expected).unwrap(),
            vec![
                StrokeAction::Replace {
                    target: sid(1),
                    fragments: vec![s(11, 1)]
                },
                StrokeAction::Replace {
                    target: sid(2),
                    fragments: vec![s(21, 2)]
                },
                StrokeAction::Replace {
                    target: sid(3),
                    fragments: vec![s(31, 3)]
                },
            ]
        );
    }

    #[test]
    fn plans_same_id_rewrites_and_refuses_reorders_or_mid_page_inserts() {
        let a = vec![s(1, 1), s(2, 2)];
        assert!(plan_page_strokes(&a, &[s(2, 2), s(1, 1)]).is_err());
        let mut changed = s(1, 1);
        changed.points[0].x = 9.0;
        assert_eq!(
            plan_page_strokes(&a, &[changed.clone(), s(2, 2)]).unwrap(),
            vec![StrokeAction::Rewrite {
                target: sid(1),
                stroke: changed,
            }]
        );
        assert!(plan_page_strokes(&a, &[s(1, 1), s(7, 7), s(2, 2)]).is_err());
    }
}
