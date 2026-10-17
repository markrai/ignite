//! Off-device recovery rehearsal: plan against extracted history, prove with the production
//! engine, never write the source trees.

use serde::Serialize;
use sync_engine::LibraryState;
use sync_protocol::{snapshot_sha256, SyncEnvelopeV1, SyncOperationV1, SyncSnapshotV1};

use crate::sync_recovery_extract::{apply_candidates, ExtractedHistory};
use crate::sync_recovery_plan::{
    plan_recovery, CandidateOp, PlanResult, RecoveryInputs, SequenceAuthority, ServerTailItem,
};

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioOutcome {
    pub name: &'static str,
    pub feasible: bool,
    pub blocked_kind: Option<String>,
    pub extra_originals: usize,
    pub lineage_replacements: usize,
    pub extra_pages: usize,
    pub exact_state_proven: bool,
    pub upload_authorized: bool,
    pub legal_submission_order: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CheckpointOnlyProof {
    pub feasible: bool,
    pub blocked_kind: Option<String>,
    pub extra_originals: usize,
    pub lineage_replacements: usize,
    pub extra_pages: usize,
    pub pending_preserved: usize,
    pub local_pages: usize,
    pub checkpoint_pages: usize,
    pub local_notebooks: usize,
    pub checkpoint_notebooks: usize,
    pub exact_canonical_equality: bool,
    pub fragment_ids_match: bool,
    pub fingerprint: Option<String>,
    pub identity_allocated_pages: usize,
    pub identity_changed: bool,
    pub outbox_bytes: u64,
    pub pending_count: usize,
    pub pairing_present: bool,
    pub upload_authorized: bool,
    pub legal_submission_order: bool,
    pub future_execution_protocol_required: bool,
    pub hypothetical_label: &'static str,
    pub plan_ms: u128,
    pub replay_ms: u128,
}

pub fn plan_with_tail(h: &ExtractedHistory, tail: &[ServerTailItem]) -> PlanResult {
    plan_recovery(RecoveryInputs {
        library_id: h.library_id,
        replica_id: h.replica_id,
        checkpoint: &h.checkpoint,
        checkpoint_cursor: h.checkpoint_cursor,
        acked_client_seq: h.acked_client_seq,
        accepted: &[],
        outbox: h.outbox_input(),
        local: &h.local,
        server_tail: tail,
        sequence_authority: h.sequence_authority.clone(),
    })
}

pub fn plan_with_local_and_tail(
    h: &ExtractedHistory,
    local: &SyncSnapshotV1,
    pending: &[SyncEnvelopeV1],
    tail: &[ServerTailItem],
    authority: SequenceAuthority,
) -> PlanResult {
    let outbox = if h.outbox_invalid.is_some() && pending.is_empty() {
        h.outbox_input()
    } else {
        crate::sync_recovery_plan::OutboxInput::Valid { envelopes: pending }
    };
    plan_recovery(RecoveryInputs {
        library_id: h.library_id,
        replica_id: h.replica_id,
        checkpoint: &h.checkpoint,
        checkpoint_cursor: h.checkpoint_cursor,
        acked_client_seq: h.acked_client_seq,
        accepted: &[],
        outbox,
        local,
        server_tail: tail,
        sequence_authority: authority,
    })
}

pub fn prove_checkpoint_only(h: &ExtractedHistory) -> CheckpointOnlyProof {
    let t0 = std::time::Instant::now();
    let result = plan_with_tail(h, &[]);
    let plan_ms = t0.elapsed().as_millis();
    match result {
        PlanResult::Feasible(plan) => {
            let t1 = std::time::Instant::now();
            let replayed =
                apply_candidates(&h.checkpoint, h.library_id, h.replica_id, &plan.candidates);
            let replay_ms = t1.elapsed().as_millis();
            let (exact, fragments) = match replayed {
                Ok(state) => (
                    state.snapshot == h.local,
                    fragment_ids(&state.snapshot) == fragment_ids(&h.local),
                ),
                Err(_) => (false, false),
            };
            CheckpointOnlyProof {
                feasible: true,
                blocked_kind: None,
                extra_originals: plan.counts.extra_originals,
                lineage_replacements: plan.counts.lineage_replacements,
                extra_pages: plan.counts.extra_pages,
                pending_preserved: plan.counts.pending_preserved,
                local_pages: count_pages(&h.local),
                checkpoint_pages: count_pages(&h.checkpoint.snapshot),
                local_notebooks: h.local.notebooks.len(),
                checkpoint_notebooks: h.checkpoint.snapshot.notebooks.len(),
                exact_canonical_equality: exact,
                fragment_ids_match: fragments,
                fingerprint: Some(plan.fingerprint),
                identity_allocated_pages: h.identity_allocated_pages,
                identity_changed: h.identity_changed,
                outbox_bytes: h.outbox_bytes,
                pending_count: h.pending.len(),
                pairing_present: h.pairing_present,
                upload_authorized: plan.upload_authorized,
                legal_submission_order: plan.sequencing.legal_submission_order,
                future_execution_protocol_required: plan
                    .sequencing
                    .future_execution_protocol_required,
                hypothetical_label: "HYPOTHETICAL: server unchanged since checkpoint",
                plan_ms,
                replay_ms,
            }
        }
        PlanResult::Blocked(b) => CheckpointOnlyProof {
            feasible: false,
            blocked_kind: Some(format!("{b:?}")),
            extra_originals: 0,
            lineage_replacements: 0,
            extra_pages: 0,
            pending_preserved: 0,
            local_pages: count_pages(&h.local),
            checkpoint_pages: count_pages(&h.checkpoint.snapshot),
            local_notebooks: h.local.notebooks.len(),
            checkpoint_notebooks: h.checkpoint.snapshot.notebooks.len(),
            exact_canonical_equality: false,
            fragment_ids_match: false,
            fingerprint: None,
            identity_allocated_pages: h.identity_allocated_pages,
            identity_changed: h.identity_changed,
            outbox_bytes: h.outbox_bytes,
            pending_count: h.pending.len(),
            pairing_present: h.pairing_present,
            upload_authorized: false,
            legal_submission_order: false,
            future_execution_protocol_required: true,
            hypothetical_label: "HYPOTHETICAL: server unchanged since checkpoint",
            plan_ms,
            replay_ms: 0,
        },
    }
}

pub fn outcome_from_plan(name: &'static str, result: &PlanResult) -> ScenarioOutcome {
    match result {
        PlanResult::Feasible(p) => ScenarioOutcome {
            name,
            feasible: true,
            blocked_kind: None,
            extra_originals: p.counts.extra_originals,
            lineage_replacements: p.counts.lineage_replacements,
            extra_pages: p.counts.extra_pages,
            exact_state_proven: p.exact_state_proven,
            upload_authorized: p.upload_authorized,
            legal_submission_order: p.sequencing.legal_submission_order,
        },
        PlanResult::Blocked(b) => ScenarioOutcome {
            name,
            feasible: false,
            blocked_kind: Some(blocker_kind(b)),
            extra_originals: 0,
            lineage_replacements: 0,
            extra_pages: 0,
            exact_state_proven: false,
            upload_authorized: false,
            legal_submission_order: false,
        },
    }
}

pub fn replay_tail_then_candidates(
    checkpoint: &LibraryState,
    library: sync_protocol::LibraryId,
    replica: sync_protocol::ReplicaId,
    tail: &[ServerTailItem],
    candidates: &[CandidateOp],
) -> Result<LibraryState, String> {
    let mut state = checkpoint.clone();
    for item in tail {
        match state.apply(&item.envelope) {
            Ok((next, _)) => state = next,
            Err(e) => return Err(e.to_string()),
        }
    }
    apply_candidates(&state, library, replica, candidates)
}

fn count_pages(snap: &SyncSnapshotV1) -> usize {
    snap.notebooks.iter().map(|n| n.pages.len()).sum()
}

fn fragment_ids(snap: &SyncSnapshotV1) -> Vec<String> {
    let mut ids: Vec<String> = snap
        .notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .flat_map(|p| &p.strokes)
        .filter(|s| s.root_id != s.stroke_id)
        .map(|s| s.stroke_id.to_string())
        .collect();
    ids.sort();
    ids
}

fn blocker_kind(b: &crate::sync_recovery_plan::RecoveryBlock) -> String {
    match b {
        crate::sync_recovery_plan::RecoveryBlock::InvalidCheckpoint { .. } => {
            "InvalidCheckpoint".into()
        }
        crate::sync_recovery_plan::RecoveryBlock::InvalidOutbox { .. } => "InvalidOutbox".into(),
        crate::sync_recovery_plan::RecoveryBlock::UnknownSequenceAuthority => {
            "UnknownSequenceAuthority".into()
        }
        crate::sync_recovery_plan::RecoveryBlock::MissingAcceptedHistory { .. } => {
            "MissingAcceptedHistory".into()
        }
        crate::sync_recovery_plan::RecoveryBlock::SameLineageConflict { .. } => {
            "SameLineageConflict".into()
        }
        crate::sync_recovery_plan::RecoveryBlock::DeletedTarget { .. } => "DeletedTarget".into(),
        crate::sync_recovery_plan::RecoveryBlock::IdentityCollision { .. } => {
            "IdentityCollision".into()
        }
        crate::sync_recovery_plan::RecoveryBlock::UnsupportedMutation { .. } => {
            "UnsupportedMutation".into()
        }
        crate::sync_recovery_plan::RecoveryBlock::AmbiguousChronology { .. } => {
            "AmbiguousChronology".into()
        }
        crate::sync_recovery_plan::RecoveryBlock::PendingOperationDependency { .. } => {
            "PendingOperationDependency".into()
        }
        crate::sync_recovery_plan::RecoveryBlock::NoLegalClientSequenceOrdering { .. } => {
            "NoLegalClientSequenceOrdering".into()
        }
        crate::sync_recovery_plan::RecoveryBlock::StaleAuthoritativeTail { .. } => {
            "StaleAuthoritativeTail".into()
        }
        crate::sync_recovery_plan::RecoveryBlock::ExactStateReproductionFailure { .. } => {
            "ExactStateReproductionFailure".into()
        }
        crate::sync_recovery_plan::RecoveryBlock::InconsistentAcceptedHistory { .. } => {
            "InconsistentAcceptedHistory".into()
        }
    }
}

pub fn snapshot_hash(snap: &SyncSnapshotV1) -> String {
    snapshot_sha256(snap)
        .map(|h| h.to_string())
        .unwrap_or_default()
}

pub fn op_kinds(ops: &[CandidateOp]) -> Vec<&'static str> {
    ops.iter()
        .map(|c| match &c.operation {
            SyncOperationV1::AddStroke { .. } => "AddStroke",
            SyncOperationV1::CreatePage { .. } => "CreatePage",
            SyncOperationV1::ReplaceStrokeLineages { .. } => "ReplaceStrokeLineages",
            SyncOperationV1::CreateNotebook { .. } => "CreateNotebook",
            _ => "other",
        })
        .collect()
}

#[cfg(test)]
#[path = "sync_recovery_rehearse_tests.rs"]
mod tests;
