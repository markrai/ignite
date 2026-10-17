//! Read-only current vs historical Kindle-state comparison (Slice 18E.6C).
//!
//! Inspects durable identity, canonical snapshots, outbox journals, and sync-state
//! **without minting IDs**, writing files, opening `OutboundSync`, or contacting a server.
//! Device transfer is not implemented here: a [`KindleReadGrant`] records operator intent
//! and never opens a socket.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use sync_protocol::{LibraryId, ReplicaId, SyncSnapshotV1, SyncStroke};

use crate::store::page_dir;
use crate::sync_local::{
    build_snapshot_bound_only, identity_path, live_layout, read_identity, SYNC_DIR,
};
use crate::sync_outbound::{parse_journal, SyncStateV1, APPLY_FILE, OUTBOX_FILE, STATE_FILE};
use crate::sync_recovery_extract::file_sha256;
use crate::sync_recovery_plan::{
    plan_recovery, OutboxInput, PlanResult, RecoveryInputs, SequenceAuthority, ServerTailItem,
};
use crate::sync_verified::STAMP_FILE;

/// Phrase required to construct a Kindle-read grant. Not a hostname, path, or credential.
pub const KINDLE_READ_AUTHORIZATION_PHRASE: &str = "I AUTHORIZE READ-ONLY KINDLE STATE ACQUISITION";

/// Saved Review 94 tail is a point-in-time observation, not a live re-read.
pub const REVIEW_94_TAIL_LABEL: &str = "OBSERVED AT REVIEW 94, NOT GUARANTEED CURRENT";

pub const REVIEW_94_HEAD: u64 = 37928;

/// Intended Kindle paths (operator must still confirm). Transfer is not performed by this module.
pub const INTENDED_LIBRARY: &str = "/mnt/us/scribe-data";
pub const INTENDED_SYNC: &str = "/mnt/us/scribe-app/state/sync";
pub const INTENDED_DEVICE: &str = "/mnt/us/scribe-app/state/device";
pub const INTENDED_REPLICA_STATE: &str = "/mnt/us/scribe-app/state";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffCategory {
    Unchanged,
    CapturedLocal,
    UncapturedLocal,
    Identity,
    SyncState,
    Ambiguous,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindleReadGrant {
    device: String,
    ssh: String,
    dest: PathBuf,
}

impl KindleReadGrant {
    /// Construction does not connect. Empty fields and the wrong phrase refuse before any I/O.
    pub fn explicit(
        phrase: &str,
        device: String,
        ssh: String,
        dest: PathBuf,
    ) -> Result<Self, String> {
        if phrase != KINDLE_READ_AUTHORIZATION_PHRASE {
            return Err("kindle read not authorized".into());
        }
        if device.trim().is_empty() || ssh.trim().is_empty() {
            return Err("kindle read requires an explicit device and SSH method".into());
        }
        if dest.as_os_str().is_empty() {
            return Err("kindle read requires a private destination".into());
        }
        Ok(Self { device, ssh, dest })
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    pub fn ssh_method(&self) -> &str {
        &self.ssh
    }

    pub fn dest(&self) -> &Path {
        &self.dest
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityInspection {
    pub live_notebooks: usize,
    pub live_pages: usize,
    pub bound_notebooks: usize,
    pub bound_pages: usize,
    pub unbound_notebooks: Vec<String>,
    pub unbound_pages: Vec<(String, String)>,
    pub retired_notebooks: usize,
    pub retired_pages: usize,
    pub library_bound: bool,
}

impl IdentityInspection {
    pub fn complete(&self) -> bool {
        self.library_bound && self.unbound_notebooks.is_empty() && self.unbound_pages.is_empty()
    }
}

/// Live layout vs durable `identity.json`. Never calls `reconcile_identity` / UUID mint.
pub fn inspect_identity_read_only(library_root: &Path) -> Result<IdentityInspection, String> {
    let (_repo, layout) = live_layout(library_root)?;
    let existing = read_identity(library_root)?;
    let mut live_pages = 0usize;
    let mut unbound_notebooks = Vec::new();
    let mut unbound_pages = Vec::new();
    let mut bound_notebooks = 0usize;
    let mut bound_pages = 0usize;
    for (nb, pages) in &layout.notebooks {
        live_pages += pages.len();
        match existing.as_ref().and_then(|id| id.notebook_id(nb)) {
            Some(_) => bound_notebooks += 1,
            None => unbound_notebooks.push(nb.clone()),
        }
        for page in pages {
            match existing.as_ref().and_then(|id| id.page_id(nb, page)) {
                Some(_) => bound_pages += 1,
                None => unbound_pages.push((nb.clone(), page.clone())),
            }
        }
    }
    let (retired_notebooks, retired_pages, library_bound) = match &existing {
        Some(id) => (id.retired_notebooks.len(), id.retired_pages.len(), true),
        None => (0, 0, false),
    };
    Ok(IdentityInspection {
        live_notebooks: layout.notebooks.len(),
        live_pages,
        bound_notebooks,
        bound_pages,
        unbound_notebooks,
        unbound_pages,
        retired_notebooks,
        retired_pages,
        library_bound,
    })
}

/// Snapshot of currently bound entities. Unbound live pages are listed, never minted.
/// If identity.json is missing, the snapshot is `None`.
pub fn bound_snapshot_read_only(
    library_root: &Path,
) -> Result<(IdentityInspection, Option<SyncSnapshotV1>), String> {
    let inspection = inspect_identity_read_only(library_root)?;
    let Some(identity) = read_identity(library_root)? else {
        return Ok((inspection, None));
    };
    let snap = build_snapshot_bound_only(library_root, &identity)?;
    Ok((inspection, Some(snap)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageChange {
    pub label: String,
    pub kind: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryDiff {
    pub hist_notebooks: usize,
    pub curr_notebooks: usize,
    pub hist_pages: usize,
    pub curr_pages: usize,
    pub added_notebooks: usize,
    pub removed_notebooks: usize,
    pub added_pages: usize,
    pub removed_pages: usize,
    pub page_order_changed: usize,
    pub notebook_meta_changed: usize,
    pub stroke_id_changed: usize,
    pub root_lineage_changed: usize,
    pub geometry_changed: usize,
    pub render_order_changed: usize,
    pub style_changed: usize,
    pub texts_or_links_changed: usize,
    pub background_changed: usize,
    pub exact_canonical_match: bool,
    pub fragment_ids_match: bool,
    pub page_changes: Vec<PageChange>,
}

fn page_count(snap: &SyncSnapshotV1) -> usize {
    snap.notebooks.iter().map(|n| n.pages.len()).sum()
}

fn fragment_ids(snap: &SyncSnapshotV1) -> BTreeSet<String> {
    snap.notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .flat_map(|p| &p.strokes)
        .filter(|s| s.root_id != s.stroke_id)
        .map(|s| s.stroke_id.to_string())
        .collect()
}

fn stroke_geometry_eq(a: &SyncStroke, b: &SyncStroke) -> bool {
    a.points == b.points
}

/// Canonical snapshot comparison. Labels are opaque (`n{i}/p{j}`), never live UUIDs.
pub fn compare_snapshots(hist: &SyncSnapshotV1, curr: &SyncSnapshotV1) -> LibraryDiff {
    let mut diff = LibraryDiff {
        hist_notebooks: hist.notebooks.len(),
        curr_notebooks: curr.notebooks.len(),
        hist_pages: page_count(hist),
        curr_pages: page_count(curr),
        added_notebooks: 0,
        removed_notebooks: 0,
        added_pages: 0,
        removed_pages: 0,
        page_order_changed: 0,
        notebook_meta_changed: 0,
        stroke_id_changed: 0,
        root_lineage_changed: 0,
        geometry_changed: 0,
        render_order_changed: 0,
        style_changed: 0,
        texts_or_links_changed: 0,
        background_changed: 0,
        exact_canonical_match: hist == curr,
        fragment_ids_match: fragment_ids(hist) == fragment_ids(curr),
        page_changes: Vec::new(),
    };
    let hist_n: BTreeMap<_, _> = hist.notebooks.iter().map(|n| (n.notebook_id, n)).collect();
    let curr_n: BTreeMap<_, _> = curr.notebooks.iter().map(|n| (n.notebook_id, n)).collect();
    for id in curr_n.keys() {
        if !hist_n.contains_key(id) {
            diff.added_notebooks += 1;
            diff.page_changes.push(PageChange {
                label: "added-notebook".into(),
                kind: "added_notebook",
            });
        }
    }
    for id in hist_n.keys() {
        if !curr_n.contains_key(id) {
            diff.removed_notebooks += 1;
            diff.page_changes.push(PageChange {
                label: "removed-notebook".into(),
                kind: "removed_notebook",
            });
        }
    }
    for (i, hn) in hist.notebooks.iter().enumerate() {
        let Some(cn) = curr_n.get(&hn.notebook_id) else {
            continue;
        };
        if hn.title != cn.title || hn.folder_id != cn.folder_id {
            diff.notebook_meta_changed += 1;
        }
        let hist_pages: Vec<_> = hn.pages.iter().map(|p| p.page_id).collect();
        let curr_pages: Vec<_> = cn.pages.iter().map(|p| p.page_id).collect();
        if hist_pages != curr_pages {
            let hs: BTreeSet<_> = hist_pages.iter().collect();
            let cs: BTreeSet<_> = curr_pages.iter().collect();
            if hs == cs {
                diff.page_order_changed += 1;
            }
        }
        let hp: BTreeMap<_, _> = hn.pages.iter().map(|p| (p.page_id, p)).collect();
        let cp: BTreeMap<_, _> = cn.pages.iter().map(|p| (p.page_id, p)).collect();
        for pid in cp.keys() {
            if !hp.contains_key(pid) {
                diff.added_pages += 1;
                diff.page_changes.push(PageChange {
                    label: format!("n{i}/added"),
                    kind: "added_page",
                });
            }
        }
        for (j, pid) in hist_pages.iter().enumerate() {
            if !cp.contains_key(pid) {
                diff.removed_pages += 1;
                diff.page_changes.push(PageChange {
                    label: format!("n{i}/p{j}"),
                    kind: "missing_page",
                });
                continue;
            }
            let a = hp[pid];
            let b = cp[pid];
            let label = format!("n{i}/p{j}");
            if a.background != b.background {
                diff.background_changed += 1;
                diff.page_changes.push(PageChange {
                    label: label.clone(),
                    kind: "background",
                });
            }
            if a.texts != b.texts || a.links != b.links {
                diff.texts_or_links_changed += 1;
                diff.page_changes.push(PageChange {
                    label: label.clone(),
                    kind: "texts_or_links",
                });
            }
            if a.strokes == b.strokes {
                continue;
            }
            let a_ids: Vec<_> = a.strokes.iter().map(|s| s.stroke_id).collect();
            let b_ids: Vec<_> = b.strokes.iter().map(|s| s.stroke_id).collect();
            if a_ids != b_ids {
                let aset: BTreeSet<_> = a_ids.iter().collect();
                let bset: BTreeSet<_> = b_ids.iter().collect();
                if aset == bset {
                    diff.render_order_changed += 1;
                    diff.page_changes.push(PageChange {
                        label: label.clone(),
                        kind: "render_order",
                    });
                } else {
                    diff.stroke_id_changed += 1;
                    diff.page_changes.push(PageChange {
                        label: label.clone(),
                        kind: "stroke_id",
                    });
                }
            }
            for sa in &a.strokes {
                if let Some(sb) = b.strokes.iter().find(|s| s.stroke_id == sa.stroke_id) {
                    if sa.root_id != sb.root_id {
                        diff.root_lineage_changed += 1;
                        diff.page_changes.push(PageChange {
                            label: label.clone(),
                            kind: "root_lineage",
                        });
                    }
                    if sa.style != sb.style {
                        diff.style_changed += 1;
                        diff.page_changes.push(PageChange {
                            label: label.clone(),
                            kind: "style",
                        });
                    }
                    if !stroke_geometry_eq(sa, sb) {
                        diff.geometry_changed += 1;
                        diff.page_changes.push(PageChange {
                            label: label.clone(),
                            kind: "geometry",
                        });
                    }
                }
            }
        }
    }
    diff
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutboxClass {
    EmptyValid,
    ValidPending { count: usize, first: u64, last: u64 },
    TrailingPrep,
    TornTail,
    Malformed { detail: String },
}

pub fn classify_outbox(raw: &[u8]) -> OutboxClass {
    match parse_journal(raw) {
        Err(detail) => OutboxClass::Malformed { detail },
        Ok(j) if j.trailing_prep.is_some() => OutboxClass::TrailingPrep,
        Ok(j) if j.torn_tail => OutboxClass::TornTail,
        Ok(j) if j.committed.is_empty() => OutboxClass::EmptyValid,
        Ok(j) => OutboxClass::ValidPending {
            count: j.committed.len(),
            first: j.committed.first().map(|p| p.seq).unwrap_or(0),
            last: j.committed.last().map(|p| p.seq).unwrap_or(0),
        },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncStateDiff {
    pub library_changed: bool,
    pub replica_changed: bool,
    pub genesis_changed: bool,
    pub cursor_changed: bool,
    pub acked_changed: bool,
    pub next_changed: bool,
    pub category: DiffCategory,
}

pub fn compare_sync_state(hist: &SyncStateV1, curr: &SyncStateV1) -> SyncStateDiff {
    let library_changed = hist.library_id != curr.library_id;
    let replica_changed = hist.replica_id != curr.replica_id;
    let genesis_changed = hist.genesis_sha256 != curr.genesis_sha256;
    let cursor_changed = hist.server_cursor != curr.server_cursor;
    let acked_changed = hist.acked_client_seq != curr.acked_client_seq;
    let next_changed = hist.next_client_seq != curr.next_client_seq;
    let any = library_changed
        || replica_changed
        || genesis_changed
        || cursor_changed
        || acked_changed
        || next_changed;
    SyncStateDiff {
        library_changed,
        replica_changed,
        genesis_changed,
        cursor_changed,
        acked_changed,
        next_changed,
        category: if any {
            DiffCategory::SyncState
        } else {
            DiffCategory::Unchanged
        },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CensusEntry {
    pub rel: String,
    pub sha256: Option<String>,
    pub len: u64,
}

/// Lightweight read-only file census for before/after capture consistency.
pub fn consistency_census(
    library_root: &Path,
    sync_dir: &Path,
) -> Result<Vec<CensusEntry>, String> {
    let mut rels: Vec<(String, PathBuf)> = vec![
        ("library.meta".into(), library_root.join("library.meta")),
        (
            format!("{SYNC_DIR}/identity.json"),
            identity_path(library_root),
        ),
        (STATE_FILE.into(), sync_dir.join(STATE_FILE)),
        (OUTBOX_FILE.into(), sync_dir.join(OUTBOX_FILE)),
        (APPLY_FILE.into(), sync_dir.join(APPLY_FILE)),
        (STAMP_FILE.into(), sync_dir.join(STAMP_FILE)),
    ];
    let (_repo, layout) = live_layout(library_root)?;
    for (nb, pages) in &layout.notebooks {
        let nb_dir = library_root.join("notebooks").join(nb);
        rels.push((
            format!("notebooks/{nb}/notebook.meta"),
            nb_dir.join("notebook.meta"),
        ));
        for page in pages {
            let dir = page_dir(&nb_dir, page);
            rels.push((
                format!("notebooks/{nb}/{page}/strokes.jsonl"),
                dir.join("strokes.jsonl"),
            ));
            rels.push((
                format!("notebooks/{nb}/{page}/edits.jsonl"),
                dir.join("edits.jsonl"),
            ));
        }
    }
    let mut out = Vec::new();
    for (rel, path) in rels {
        let meta = std::fs::metadata(&path);
        let len = meta.map(|m| m.len()).unwrap_or(0);
        out.push(CensusEntry {
            rel,
            sha256: file_sha256(&path)?,
            len,
        });
    }
    Ok(out)
}

pub fn census_equal(a: &[CensusEntry], b: &[CensusEntry]) -> bool {
    a == b
}

/// Review 94 empty tail is usable only when the current checkpoint cursor still matches.
pub fn review_94_tail_applicable(current_checkpoint_cursor: u64) -> bool {
    current_checkpoint_cursor == REVIEW_94_HEAD
}

pub fn observed_tail_label() -> &'static str {
    REVIEW_94_TAIL_LABEL
}

/// Plan only when durable identity is complete. Does not mint.
pub fn plan_if_identity_complete(
    library_id: LibraryId,
    replica_id: ReplicaId,
    checkpoint: &sync_engine::LibraryState,
    checkpoint_cursor: u64,
    acked_client_seq: u64,
    local: &SyncSnapshotV1,
    pending: &[sync_protocol::SyncEnvelopeV1],
    tail: &[ServerTailItem],
    sequence_authority: SequenceAuthority,
    inspection: &IdentityInspection,
) -> Result<PlanResult, String> {
    if !inspection.complete() {
        return Err(format!(
            "identity_binding_prerequisite: {} unbound pages, {} unbound notebooks",
            inspection.unbound_pages.len(),
            inspection.unbound_notebooks.len()
        ));
    }
    if !review_94_tail_applicable(checkpoint_cursor) {
        return Err(
            "stale-boundary: current checkpoint cursor does not match Review 94 head 37928".into(),
        );
    }
    Ok(plan_recovery(RecoveryInputs {
        library_id,
        replica_id,
        checkpoint,
        checkpoint_cursor,
        acked_client_seq,
        accepted: &[],
        outbox: OutboxInput::Valid { envelopes: pending },
        local,
        server_tail: tail,
        sequence_authority,
    }))
}

#[cfg(test)]
#[path = "sync_recovery_compare_tests.rs"]
mod tests;
