//! Read-only historical recovery planner (Slice 18E.4C).
//!
//! Reconstructs candidate production operations for notebook changes that are present locally
//! but missing from accepted history and the validated pending outbox. Planning never writes
//! files, never allocates live `client_seq` / `OperationId` values, never clears a fault, and
//! never authorizes upload.
//!
//! Candidate operations are uncommitted. Simulation envelopes used for exact-state proof are
//! discarded; they are not a reservation.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use sync_engine::{LibraryState, Outcome, SemanticErrorCode};
use sync_protocol::{
    check_client_seq, sha256_hex, snapshot_sha256, to_canonical_json, LibraryId,
    LineageReplacement, NotebookId, OperationId, PageId, PageSpec, ReplicaId, SnapshotPage,
    StrokeId, SyncEnvelopeV1, SyncOperationV1, SyncSnapshotV1, SyncStroke, ENVELOPE_VERSION,
};
use uuid::Uuid;

/// Whether the caller established Review 89 sequence authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SequenceAuthority {
    Known { next_client_seq: u64 },
    Unknown,
}

/// Validated pending outbox, or a refusal to treat it as sequenced history.
#[derive(Clone, Debug, PartialEq)]
pub enum OutboxInput<'a> {
    Valid { envelopes: &'a [SyncEnvelopeV1] },
    Invalid { detail: &'a str },
}

/// One supplied authoritative server-tail item (synthetic in this slice).
#[derive(Clone, Debug, PartialEq)]
pub struct ServerTailItem {
    pub server_seq: u64,
    pub envelope: SyncEnvelopeV1,
}

/// Immutable planner inputs. Callers retain ownership; planning does not mutate them.
#[derive(Clone, Debug)]
pub struct RecoveryInputs<'a> {
    pub library_id: LibraryId,
    pub replica_id: ReplicaId,
    pub checkpoint: &'a LibraryState,
    pub checkpoint_cursor: u64,
    pub acked_client_seq: u64,
    pub accepted: &'a [SyncEnvelopeV1],
    pub outbox: OutboxInput<'a>,
    pub local: &'a SyncSnapshotV1,
    pub server_tail: &'a [ServerTailItem],
    pub sequence_authority: SequenceAuthority,
}

/// Uncommitted candidate operation. No `client_seq`, no reserved `OperationId`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CandidateOp {
    pub operation: SyncOperationV1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequencingReport {
    pub acked_client_seq: u64,
    pub pending_seq_range: Option<(u64, u64)>,
    pub proposed_operation_count: usize,
    pub depends_on_pending: bool,
    pub legal_submission_order: bool,
    pub future_execution_protocol_required: bool,
    /// Informational only: first sequence a future executor would request. Not reserved.
    pub unreserved_next_client_seq: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DifferenceCounts {
    pub extra_originals: usize,
    pub lineage_replacements: usize,
    pub extra_pages: usize,
    pub pending_preserved: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecoveryPlan {
    pub candidates: Vec<CandidateOp>,
    pub sequencing: SequencingReport,
    pub counts: DifferenceCounts,
    pub fingerprint: String,
    /// Always false. A plan is not upload authorization.
    pub upload_authorized: bool,
    pub exact_state_proven: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryBlock {
    InvalidCheckpoint { detail: String },
    InvalidOutbox { detail: String },
    UnknownSequenceAuthority,
    MissingAcceptedHistory { detail: String },
    SameLineageConflict { detail: String },
    DeletedTarget { detail: String },
    IdentityCollision { detail: String },
    UnsupportedMutation { detail: String },
    AmbiguousChronology { detail: String },
    PendingOperationDependency { detail: String },
    NoLegalClientSequenceOrdering { detail: String },
    StaleAuthoritativeTail { detail: String },
    ExactStateReproductionFailure { detail: String },
    InconsistentAcceptedHistory { detail: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlanResult {
    Feasible(RecoveryPlan),
    Blocked(RecoveryBlock),
}

impl PlanResult {
    pub fn upload_authorized(&self) -> bool {
        false
    }
}

/// Plan recovery. Pure: `inputs` and every borrowed buffer are unchanged.
pub fn plan_recovery(inputs: RecoveryInputs<'_>) -> PlanResult {
    match plan_recovery_inner(&inputs) {
        Ok(plan) => PlanResult::Feasible(plan),
        Err(block) => PlanResult::Blocked(block),
    }
}

fn plan_recovery_inner(inputs: &RecoveryInputs<'_>) -> Result<RecoveryPlan, RecoveryBlock> {
    validate_static(inputs)?;
    let pending = match inputs.outbox {
        OutboxInput::Invalid { detail } => {
            return Err(RecoveryBlock::InvalidOutbox {
                detail: detail.to_string(),
            });
        }
        OutboxInput::Valid { envelopes } => envelopes,
    };
    validate_sequences(inputs, pending)?;
    refuse_operation_id_conflicts(inputs, pending)?;

    let remote = apply_tail(inputs)?;
    let overlay: Vec<&SyncEnvelopeV1> =
        overlay_envelopes(inputs.accepted, inputs.server_tail, pending);

    match apply_envelopes(&remote, overlay.iter().copied()) {
        Ok(synced) => plan_after_overlay(inputs, pending, &remote, synced),
        Err(e) => plan_when_overlay_fails(inputs, pending, &remote, e),
    }
}

fn validate_static(inputs: &RecoveryInputs<'_>) -> Result<(), RecoveryBlock> {
    if matches!(inputs.sequence_authority, SequenceAuthority::Unknown) {
        return Err(RecoveryBlock::UnknownSequenceAuthority);
    }
    if let Err(e) = inputs.checkpoint.validate() {
        return Err(RecoveryBlock::InvalidCheckpoint {
            detail: e.to_string(),
        });
    }
    if inputs.checkpoint.snapshot.library_id != inputs.library_id {
        return Err(RecoveryBlock::InvalidCheckpoint {
            detail: "checkpoint library_id mismatch".into(),
        });
    }
    if inputs.local.library_id != inputs.library_id {
        return Err(RecoveryBlock::InvalidCheckpoint {
            detail: "local snapshot library_id mismatch".into(),
        });
    }
    if let Err(e) = inputs.local.validate() {
        let detail = format!("local snapshot: {e}");
        if detail.contains("duplicate stroke") || detail.contains("duplicate page") {
            return Err(RecoveryBlock::IdentityCollision { detail });
        }
        return Err(RecoveryBlock::InvalidCheckpoint { detail });
    }
    Ok(())
}

fn validate_sequences(
    inputs: &RecoveryInputs<'_>,
    pending: &[SyncEnvelopeV1],
) -> Result<(), RecoveryBlock> {
    let mut prev = if inputs.acked_client_seq == 0 {
        None
    } else {
        Some(inputs.acked_client_seq)
    };
    for env in inputs.accepted.iter().chain(pending.iter()) {
        if env.library_id != inputs.library_id {
            return Err(RecoveryBlock::InconsistentAcceptedHistory {
                detail: "envelope library_id mismatch".into(),
            });
        }
        if env.replica_id != inputs.replica_id {
            return Err(RecoveryBlock::InconsistentAcceptedHistory {
                detail: "envelope replica_id mismatch".into(),
            });
        }
        if let Err(e) = check_client_seq(prev, env.client_seq) {
            return Err(RecoveryBlock::InconsistentAcceptedHistory {
                detail: e.to_string(),
            });
        }
        prev = Some(env.client_seq);
    }
    let want_next = prev.map_or(1, |p| p.saturating_add(1));
    match inputs.sequence_authority {
        SequenceAuthority::Unknown => Err(RecoveryBlock::UnknownSequenceAuthority),
        SequenceAuthority::Known { next_client_seq } if next_client_seq != want_next => {
            Err(RecoveryBlock::InconsistentAcceptedHistory {
                detail: format!("next_client_seq {next_client_seq} != {want_next}"),
            })
        }
        SequenceAuthority::Known { .. } => Ok(()),
    }
}

fn refuse_operation_id_conflicts(
    inputs: &RecoveryInputs<'_>,
    pending: &[SyncEnvelopeV1],
) -> Result<(), RecoveryBlock> {
    let mut seen: BTreeMap<OperationId, &SyncEnvelopeV1> = BTreeMap::new();
    let tail_envs = inputs.server_tail.iter().map(|t| &t.envelope);
    for env in inputs
        .accepted
        .iter()
        .chain(pending.iter())
        .chain(tail_envs)
    {
        if let Some(prev) = seen.insert(env.operation_id, env) {
            if prev != env {
                return Err(RecoveryBlock::IdentityCollision {
                    detail: format!(
                        "operation_id {} appears with conflicting envelopes",
                        env.operation_id
                    ),
                });
            }
        }
    }
    Ok(())
}

fn apply_tail(inputs: &RecoveryInputs<'_>) -> Result<LibraryState, RecoveryBlock> {
    let mut cursor = inputs.checkpoint_cursor;
    let mut state = inputs.checkpoint.clone();
    for item in inputs.server_tail {
        let want = cursor
            .checked_add(1)
            .ok_or_else(|| RecoveryBlock::StaleAuthoritativeTail {
                detail: "server_seq exhausted".into(),
            })?;
        if item.server_seq != want {
            return Err(RecoveryBlock::StaleAuthoritativeTail {
                detail: format!("tail server_seq {} want {want}", item.server_seq),
            });
        }
        match state.apply(&item.envelope) {
            Ok((next, _)) => state = next,
            Err(e) => {
                return Err(RecoveryBlock::StaleAuthoritativeTail {
                    detail: e.to_string(),
                });
            }
        }
        cursor = item.server_seq;
    }
    Ok(state)
}

fn overlay_envelopes<'a>(
    accepted: &'a [SyncEnvelopeV1],
    tail: &'a [ServerTailItem],
    pending: &'a [SyncEnvelopeV1],
) -> Vec<&'a SyncEnvelopeV1> {
    let in_tail: BTreeSet<OperationId> = tail.iter().map(|t| t.envelope.operation_id).collect();
    let mut out = Vec::new();
    for env in accepted {
        if !in_tail.contains(&env.operation_id) {
            out.push(env);
        }
    }
    out.extend(pending.iter());
    out
}

fn apply_envelopes<'a>(
    start: &LibraryState,
    envs: impl Iterator<Item = &'a SyncEnvelopeV1>,
) -> Result<LibraryState, String> {
    let mut state = start.clone();
    for env in envs {
        match state.apply(env) {
            Ok((next, _)) => state = next,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(state)
}

fn plan_after_overlay(
    inputs: &RecoveryInputs<'_>,
    pending: &[SyncEnvelopeV1],
    remote: &LibraryState,
    synced: LibraryState,
) -> Result<RecoveryPlan, RecoveryBlock> {
    let ops = diff_ops(inputs.checkpoint, remote, &synced, inputs.local)?;
    let proven = apply_candidates(inputs, &synced, &ops)?;
    if proven.snapshot != *inputs.local {
        return Err(RecoveryBlock::ExactStateReproductionFailure {
            detail: "candidates applied after pending did not reproduce the local snapshot".into(),
        });
    }
    Ok(finish_plan(inputs, pending, ops, true, false, false))
}

fn plan_when_overlay_fails(
    inputs: &RecoveryInputs<'_>,
    pending: &[SyncEnvelopeV1],
    remote: &LibraryState,
    overlay_err: String,
) -> Result<RecoveryPlan, RecoveryBlock> {
    let before = diff_ops(inputs.checkpoint, remote, remote, inputs.local)?;
    let recovered = apply_candidates(inputs, remote, &before)?;
    match apply_envelopes(&recovered, pending.iter()) {
        Ok(final_state) if final_state.snapshot == *inputs.local => {
            Err(RecoveryBlock::NoLegalClientSequenceOrdering {
                detail: format!(
                    "recovery operations must precede existing pending client_seq; overlay apply failed: {overlay_err}"
                ),
            })
        }
        _ => Err(RecoveryBlock::PendingOperationDependency {
            detail: overlay_err,
        }),
    }
}

fn diff_ops(
    checkpoint: &LibraryState,
    remote: &LibraryState,
    synced: &LibraryState,
    local: &SyncSnapshotV1,
) -> Result<Vec<SyncOperationV1>, RecoveryBlock> {
    refuse_structure(synced, local)?;
    let mut ops = Vec::new();
    let mut seen_nb: BTreeSet<NotebookId> = BTreeSet::new();
    for an in &local.notebooks {
        seen_nb.insert(an.notebook_id);
        let Some(sn) = synced
            .snapshot
            .notebooks
            .iter()
            .find(|n| n.notebook_id == an.notebook_id)
        else {
            if checkpoint
                .history
                .deleted_notebooks
                .contains(&an.notebook_id)
                || remote.history.deleted_notebooks.contains(&an.notebook_id)
                || synced.history.deleted_notebooks.contains(&an.notebook_id)
            {
                return Err(RecoveryBlock::DeletedTarget {
                    detail: format!("notebook {} was deleted remotely", an.notebook_id),
                });
            }
            return Err(RecoveryBlock::UnsupportedMutation {
                detail: format!(
                    "new notebook {} is not a Review 80 extra-page case",
                    an.notebook_id
                ),
            });
        };
        if sn.title != an.title || sn.folder_id != an.folder_id {
            return Err(RecoveryBlock::UnsupportedMutation {
                detail: format!("notebook {} title/folder changed", an.notebook_id),
            });
        }
        ops.extend(diff_pages(
            checkpoint,
            remote,
            synced,
            an.notebook_id,
            sn,
            an,
        )?);
    }
    for sn in &synced.snapshot.notebooks {
        if !seen_nb.contains(&sn.notebook_id) {
            return Err(RecoveryBlock::UnsupportedMutation {
                detail: format!(
                    "local notebook {} missing (not derivable as recovery)",
                    sn.notebook_id
                ),
            });
        }
    }
    if synced.snapshot.folders != local.folders {
        return Err(RecoveryBlock::UnsupportedMutation {
            detail: "folder set differs".into(),
        });
    }
    Ok(ops)
}

fn refuse_structure(synced: &LibraryState, local: &SyncSnapshotV1) -> Result<(), RecoveryBlock> {
    let s_ids: Vec<NotebookId> = synced
        .snapshot
        .notebooks
        .iter()
        .map(|n| n.notebook_id)
        .collect();
    let l_ids: Vec<NotebookId> = local.notebooks.iter().map(|n| n.notebook_id).collect();
    let shared: Vec<NotebookId> = l_ids
        .iter()
        .copied()
        .filter(|id| s_ids.contains(id))
        .collect();
    let expected_shared: Vec<NotebookId> = s_ids
        .iter()
        .copied()
        .filter(|id| l_ids.contains(id))
        .collect();
    if shared != expected_shared {
        return Err(RecoveryBlock::UnsupportedMutation {
            detail: "notebooks were reordered".into(),
        });
    }
    Ok(())
}

fn diff_pages(
    checkpoint: &LibraryState,
    remote: &LibraryState,
    synced: &LibraryState,
    notebook_id: NotebookId,
    sn: &sync_protocol::SnapshotNotebook,
    an: &sync_protocol::SnapshotNotebook,
) -> Result<Vec<SyncOperationV1>, RecoveryBlock> {
    for ap in &an.pages {
        if checkpoint.history.deleted_pages.contains(&ap.page_id)
            || remote.history.deleted_pages.contains(&ap.page_id)
            || synced.history.deleted_pages.contains(&ap.page_id)
        {
            return Err(RecoveryBlock::DeletedTarget {
                detail: format!("page {} was deleted remotely", ap.page_id),
            });
        }
    }
    let mut ops = Vec::new();
    let s_pages: Vec<PageId> = sn.pages.iter().map(|p| p.page_id).collect();
    let a_pages: Vec<PageId> = an.pages.iter().map(|p| p.page_id).collect();
    if a_pages.len() < s_pages.len() || a_pages[..s_pages.len()] != s_pages[..] {
        if s_pages.iter().any(|p| !a_pages.contains(p)) {
            return Err(RecoveryBlock::UnsupportedMutation {
                detail: format!("notebook {notebook_id}: pages removed or reordered"),
            });
        }
        return Err(RecoveryBlock::UnsupportedMutation {
            detail: format!("notebook {notebook_id}: extra pages are not a suffix"),
        });
    }
    for (sp, ap) in sn.pages.iter().zip(&an.pages[..s_pages.len()]) {
        ops.extend(plan_page(checkpoint, remote, synced, notebook_id, sp, ap)?);
    }
    let mut after = s_pages.last().copied();
    for ap in &an.pages[s_pages.len()..] {
        if page_known(synced, ap.page_id) || page_known(remote, ap.page_id) {
            return Err(RecoveryBlock::IdentityCollision {
                detail: format!("page {} already used", ap.page_id),
            });
        }
        ops.push(SyncOperationV1::CreatePage {
            notebook_id,
            page: spec_of(ap),
            after_page_id: after,
        });
        let empty = empty_like(ap);
        ops.extend(plan_page(
            checkpoint,
            remote,
            synced,
            notebook_id,
            &empty,
            ap,
        )?);
        after = Some(ap.page_id);
    }
    Ok(ops)
}

fn page_known(state: &LibraryState, page: PageId) -> bool {
    state
        .snapshot
        .notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .any(|p| p.page_id == page)
        || state.history.deleted_pages.contains(&page)
}

fn empty_like(p: &SnapshotPage) -> SnapshotPage {
    SnapshotPage {
        page_id: p.page_id,
        background: p.background.clone(),
        render: p.render.clone(),
        strokes: Vec::new(),
        texts: Vec::new(),
        links: Vec::new(),
    }
}

fn spec_of(p: &SnapshotPage) -> PageSpec {
    PageSpec {
        page_id: p.page_id,
        background: p.background.clone(),
        render: p.render.clone(),
    }
}

fn plan_page(
    checkpoint: &LibraryState,
    remote: &LibraryState,
    synced: &LibraryState,
    notebook_id: NotebookId,
    expected: &SnapshotPage,
    actual: &SnapshotPage,
) -> Result<Vec<SyncOperationV1>, RecoveryBlock> {
    if expected.background != actual.background || expected.render != actual.render {
        return Err(RecoveryBlock::UnsupportedMutation {
            detail: format!("page {} background/render changed", actual.page_id),
        });
    }
    if expected.links != actual.links || expected.texts != actual.texts {
        return Err(RecoveryBlock::UnsupportedMutation {
            detail: format!("page {} texts/links differ", actual.page_id),
        });
    }
    if expected.strokes == actual.strokes {
        return Ok(Vec::new());
    }
    detect_lineage_conflicts(checkpoint, remote, actual)?;
    detect_id_collisions(synced, expected, actual)?;

    if let Some(ops) = suffix_originals(notebook_id, actual.page_id, expected, actual) {
        return Ok(ops);
    }

    let (visible, unseen, added) = classify_roots(expected, actual)?;
    let mut last_err = String::new();
    let interleaved = interleaved_ops(
        notebook_id,
        actual.page_id,
        &visible,
        &unseen,
        &added,
        actual,
    );
    let mut candidates = vec![interleaved];
    let orders: [&[PageStep]; 3] = [
        &[
            PageStep::ReplaceVisible,
            PageStep::AddOriginals,
            PageStep::ReplaceUnseen,
        ],
        &[
            PageStep::AddOriginals,
            PageStep::ReplaceVisible,
            PageStep::ReplaceUnseen,
        ],
        &[
            PageStep::ReplaceVisible,
            PageStep::ReplaceUnseen,
            PageStep::AddOriginals,
        ],
    ];
    for order in orders {
        candidates.push(assemble_page_ops(
            notebook_id,
            actual.page_id,
            &visible,
            &unseen,
            &added,
            order,
        ));
    }
    for ops in candidates {
        let mut sim = synced.clone();
        match apply_candidates_on(
            &mut sim,
            notebook_id,
            actual.page_id,
            expected.page_id,
            &ops,
        ) {
            Ok(()) => {
                if let Some(got) = page_strokes(&sim, notebook_id, actual.page_id) {
                    if got == actual.strokes.as_slice() {
                        return Ok(ops);
                    }
                    last_err = "stroke vector mismatch after candidate apply".into();
                }
            }
            Err(e) => last_err = e,
        }
    }
    Err(RecoveryBlock::ExactStateReproductionFailure { detail: last_err })
}

#[derive(Clone, Copy)]
enum PageStep {
    ReplaceVisible,
    ReplaceUnseen,
    AddOriginals,
}

fn suffix_originals(
    notebook_id: NotebookId,
    page_id: PageId,
    expected: &SnapshotPage,
    actual: &SnapshotPage,
) -> Option<Vec<SyncOperationV1>> {
    if actual.strokes.len() < expected.strokes.len() {
        return None;
    }
    if actual.strokes[..expected.strokes.len()] != expected.strokes[..] {
        return None;
    }
    let extra = &actual.strokes[expected.strokes.len()..];
    if extra.iter().any(|s| s.root_id != s.stroke_id) {
        return None;
    }
    Some(
        extra
            .iter()
            .map(|s| SyncOperationV1::AddStroke {
                notebook_id,
                page_id,
                stroke: s.clone(),
            })
            .collect(),
    )
}

fn classify_roots(
    expected: &SnapshotPage,
    actual: &SnapshotPage,
) -> Result<
    (
        Vec<LineageReplacement>,
        Vec<LineageReplacement>,
        Vec<SyncStroke>,
    ),
    RecoveryBlock,
> {
    let exp_m = membership(&expected.strokes);
    let act_m = membership(&actual.strokes);
    let mut roots: BTreeSet<StrokeId> = BTreeSet::new();
    roots.extend(exp_m.keys().copied());
    roots.extend(act_m.keys().copied());
    let mut visible = Vec::new();
    let mut unseen = Vec::new();
    let mut added = Vec::new();
    for root in roots {
        let e = exp_m.get(&root).cloned().unwrap_or_default();
        let a = act_m.get(&root).cloned().unwrap_or_default();
        if e == a {
            continue;
        }
        if e.is_empty() && a.len() == 1 && a[0].stroke_id == root && a[0].root_id == a[0].stroke_id
        {
            added.push(a.into_iter().next().unwrap());
            continue;
        }
        let repl = LineageReplacement {
            root_id: root,
            expected: e,
            after: a,
        };
        if repl.expected.is_empty() {
            unseen.push(repl);
        } else {
            visible.push(repl);
        }
    }
    added.sort_by_key(|s| {
        actual
            .strokes
            .iter()
            .position(|x| x.stroke_id == s.stroke_id)
            .unwrap_or(usize::MAX)
    });
    visible.sort_by_key(|r| r.root_id);
    unseen.sort_by_key(|r| r.root_id);
    Ok((visible, unseen, added))
}

fn membership(strokes: &[SyncStroke]) -> BTreeMap<StrokeId, Vec<SyncStroke>> {
    let mut m = BTreeMap::new();
    for s in strokes {
        m.entry(s.root_id).or_insert_with(Vec::new).push(s.clone());
    }
    m
}

fn assemble_page_ops(
    notebook_id: NotebookId,
    page_id: PageId,
    visible: &[LineageReplacement],
    unseen: &[LineageReplacement],
    added: &[SyncStroke],
    order: &[PageStep],
) -> Vec<SyncOperationV1> {
    let mut ops = Vec::new();
    for step in order {
        match step {
            PageStep::ReplaceVisible if !visible.is_empty() => {
                ops.push(replace_op(notebook_id, page_id, visible));
            }
            PageStep::ReplaceUnseen if !unseen.is_empty() => {
                ops.push(replace_op(notebook_id, page_id, unseen));
            }
            PageStep::AddOriginals => {
                for s in added {
                    ops.push(SyncOperationV1::AddStroke {
                        notebook_id,
                        page_id,
                        stroke: s.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    ops
}

/// Visible replacements first (in-place), then AddStroke / unseen replaces in actual render order.
/// Needed when an unseen fragment sits among new originals rather than at the end.
fn interleaved_ops(
    notebook_id: NotebookId,
    page_id: PageId,
    visible: &[LineageReplacement],
    unseen: &[LineageReplacement],
    added: &[SyncStroke],
    actual: &SnapshotPage,
) -> Vec<SyncOperationV1> {
    let mut ops = Vec::new();
    if !visible.is_empty() {
        ops.push(replace_op(notebook_id, page_id, visible));
    }
    let added_ids: BTreeSet<StrokeId> = added.iter().map(|s| s.stroke_id).collect();
    let unseen_by_root: BTreeMap<StrokeId, LineageReplacement> =
        unseen.iter().map(|r| (r.root_id, r.clone())).collect();
    let mut emitted_unseen = BTreeSet::new();
    for s in &actual.strokes {
        if added_ids.contains(&s.stroke_id) {
            ops.push(SyncOperationV1::AddStroke {
                notebook_id,
                page_id,
                stroke: s.clone(),
            });
            continue;
        }
        if unseen_by_root.contains_key(&s.root_id) && emitted_unseen.insert(s.root_id) {
            ops.push(replace_op(
                notebook_id,
                page_id,
                std::slice::from_ref(&unseen_by_root[&s.root_id]),
            ));
        }
    }
    ops
}

fn replace_op(
    notebook_id: NotebookId,
    page_id: PageId,
    replacements: &[LineageReplacement],
) -> SyncOperationV1 {
    let mut replacements = replacements.to_vec();
    replacements.sort_by_key(|r| r.root_id);
    SyncOperationV1::ReplaceStrokeLineages {
        notebook_id,
        page_id,
        replacements,
    }
}

fn detect_lineage_conflicts(
    checkpoint: &LibraryState,
    remote: &LibraryState,
    actual: &SnapshotPage,
) -> Result<(), RecoveryBlock> {
    let before = page_in(checkpoint, actual.page_id);
    let after = page_in(remote, actual.page_id);
    let Some(before) = before else {
        return Ok(());
    };
    let Some(after) = after else {
        return Err(RecoveryBlock::DeletedTarget {
            detail: format!("page {} was deleted remotely", actual.page_id),
        });
    };
    let b_m = membership(&before.strokes);
    let a_m = membership(&after.strokes);
    let loc = membership(&actual.strokes);
    let mut roots: BTreeSet<StrokeId> = BTreeSet::new();
    roots.extend(b_m.keys().copied());
    roots.extend(a_m.keys().copied());
    roots.extend(loc.keys().copied());
    for root in roots {
        let remote_changed = b_m.get(&root) != a_m.get(&root);
        if remote_changed && loc.get(&root) != a_m.get(&root) {
            return Err(RecoveryBlock::SameLineageConflict {
                detail: format!(
                    "lineage {root} on page {} changed remotely; refusing to overwrite",
                    actual.page_id
                ),
            });
        }
    }
    Ok(())
}

fn detect_id_collisions(
    synced: &LibraryState,
    expected: &SnapshotPage,
    actual: &SnapshotPage,
) -> Result<(), RecoveryBlock> {
    let expected_ids: BTreeSet<StrokeId> = expected.strokes.iter().map(|s| s.stroke_id).collect();
    for s in &actual.strokes {
        if expected_ids.contains(&s.stroke_id) {
            continue;
        }
        if stroke_used_elsewhere(synced, expected.page_id, s.stroke_id) {
            return Err(RecoveryBlock::IdentityCollision {
                detail: format!("stroke {} already used", s.stroke_id),
            });
        }
    }
    Ok(())
}

fn stroke_used_elsewhere(state: &LibraryState, page: PageId, id: StrokeId) -> bool {
    if state.history.deleted_strokes.contains(&id) {
        return true;
    }
    state
        .snapshot
        .notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .filter(|p| p.page_id != page)
        .flat_map(|p| &p.strokes)
        .any(|s| s.stroke_id == id)
}

fn page_in(state: &LibraryState, page: PageId) -> Option<&SnapshotPage> {
    state
        .snapshot
        .notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .find(|p| p.page_id == page)
}

fn page_strokes(state: &LibraryState, notebook: NotebookId, page: PageId) -> Option<&[SyncStroke]> {
    let nb = state
        .snapshot
        .notebooks
        .iter()
        .find(|n| n.notebook_id == notebook)?;
    Some(
        nb.pages
            .iter()
            .find(|p| p.page_id == page)?
            .strokes
            .as_slice(),
    )
}

fn apply_candidates_on(
    state: &mut LibraryState,
    notebook_id: NotebookId,
    page_id: PageId,
    expected_page: PageId,
    ops: &[SyncOperationV1],
) -> Result<(), String> {
    let _ = (notebook_id, page_id, expected_page);
    let library = state.snapshot.library_id;
    let replica =
        ReplicaId::from_uuid(library.as_uuid()).expect("library uuid is a valid replica id");
    for (i, op) in ops.iter().enumerate() {
        let env = sim_env(library, replica, i as u64 + 1, op.clone());
        match state.apply(&env) {
            Ok((next, Outcome::Applied | Outcome::NoOp(_))) => *state = next,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

fn apply_candidates(
    inputs: &RecoveryInputs<'_>,
    start: &LibraryState,
    ops: &[SyncOperationV1],
) -> Result<LibraryState, RecoveryBlock> {
    let mut state = start.clone();
    for (i, op) in ops.iter().enumerate() {
        let env = sim_env(
            inputs.library_id,
            inputs.replica_id,
            i as u64 + 1,
            op.clone(),
        );
        match state.apply(&env) {
            Ok((next, _)) => state = next,
            Err(e) if e.code == SemanticErrorCode::ConflictSameLineage => {
                return Err(RecoveryBlock::SameLineageConflict {
                    detail: e.to_string(),
                });
            }
            Err(e)
                if e.code == SemanticErrorCode::DuplicateStroke
                    || e.code == SemanticErrorCode::DuplicatePage =>
            {
                return Err(RecoveryBlock::IdentityCollision {
                    detail: e.to_string(),
                });
            }
            Err(e) => {
                return Err(RecoveryBlock::ExactStateReproductionFailure {
                    detail: e.to_string(),
                });
            }
        }
    }
    Ok(state)
}

fn sim_op_id(replica: ReplicaId, seq: u64) -> OperationId {
    let mut b = *replica.as_uuid().as_bytes();
    for (i, x) in seq.to_be_bytes().iter().enumerate() {
        b[8 + i] ^= *x;
    }
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    OperationId::from_uuid(Uuid::from_bytes(b)).expect("simulation operation id")
}

pub(crate) fn sim_env(
    library: LibraryId,
    replica: ReplicaId,
    seq: u64,
    operation: SyncOperationV1,
) -> SyncEnvelopeV1 {
    let op_id = sim_op_id(replica, seq);
    SyncEnvelopeV1 {
        v: ENVELOPE_VERSION,
        operation_id: op_id,
        library_id: library,
        replica_id: replica,
        client_seq: seq.max(1),
        operation,
    }
}

fn finish_plan(
    inputs: &RecoveryInputs<'_>,
    pending: &[SyncEnvelopeV1],
    ops: Vec<SyncOperationV1>,
    legal: bool,
    depends: bool,
    future: bool,
) -> RecoveryPlan {
    let counts = count_ops(&ops);
    let pending_range = pending
        .first()
        .map(|a| a.client_seq)
        .zip(pending.last().map(|a| a.client_seq));
    let next = match inputs.sequence_authority {
        SequenceAuthority::Known { next_client_seq } => Some(next_client_seq),
        SequenceAuthority::Unknown => None,
    };
    let fingerprint = fingerprint_of(inputs, pending, &ops);
    let n = ops.len();
    RecoveryPlan {
        candidates: ops
            .into_iter()
            .map(|operation| CandidateOp { operation })
            .collect(),
        sequencing: SequencingReport {
            acked_client_seq: inputs.acked_client_seq,
            pending_seq_range: pending_range,
            proposed_operation_count: n,
            depends_on_pending: depends,
            legal_submission_order: legal,
            future_execution_protocol_required: future,
            unreserved_next_client_seq: next,
        },
        counts: DifferenceCounts {
            pending_preserved: pending.len(),
            ..counts
        },
        fingerprint,
        upload_authorized: false,
        exact_state_proven: legal,
    }
}

fn count_ops(ops: &[SyncOperationV1]) -> DifferenceCounts {
    let mut extra_originals = 0;
    let mut lineage_replacements = 0;
    let mut extra_pages = 0;
    for op in ops {
        match op {
            SyncOperationV1::AddStroke { .. } => extra_originals += 1,
            SyncOperationV1::ReplaceStrokeLineages { replacements, .. } => {
                lineage_replacements += replacements.len();
            }
            SyncOperationV1::CreatePage { .. } => extra_pages += 1,
            _ => {}
        }
    }
    DifferenceCounts {
        extra_originals,
        lineage_replacements,
        extra_pages,
        pending_preserved: 0,
    }
}

fn fingerprint_of(
    inputs: &RecoveryInputs<'_>,
    pending: &[SyncEnvelopeV1],
    ops: &[SyncOperationV1],
) -> String {
    #[derive(Serialize)]
    struct Fp<'a> {
        checkpoint: String,
        cursor: u64,
        acked: u64,
        pending: Vec<u64>,
        local: String,
        tail: Vec<u64>,
        ops: &'a [SyncOperationV1],
    }
    let body = Fp {
        checkpoint: snapshot_sha256(&inputs.checkpoint.snapshot)
            .map(|h| h.to_string())
            .unwrap_or_default(),
        cursor: inputs.checkpoint_cursor,
        acked: inputs.acked_client_seq,
        pending: pending.iter().map(|e| e.client_seq).collect(),
        local: snapshot_sha256(inputs.local)
            .map(|h| h.to_string())
            .unwrap_or_default(),
        tail: inputs.server_tail.iter().map(|t| t.server_seq).collect(),
        ops,
    };
    match to_canonical_json(&body) {
        Ok(bytes) => sha256_hex(&bytes).to_string(),
        Err(_) => "fingerprint-unavailable".into(),
    }
}

#[cfg(test)]
#[path = "sync_recovery_plan_tests.rs"]
mod tests;
