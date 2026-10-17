//! Reducer state: the semantic snapshot plus the identity history needed to apply operations
//! deterministically.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sync_protocol::{
    from_json, to_canonical_json, FolderId, LinkId, NotebookId, PageId, StrokeId, SyncEnvelopeV1,
    SyncSnapshotV1, TextId,
};

use crate::apply::{Outcome, SemanticError, SemanticErrorCode};

pub const HISTORY_VERSION: u32 = 1;

/// Identities that left the live state, plus every lineage root ever seen.
///
/// The snapshot shows only what is visible. History lets the reducer tell *deleted* (a
/// concurrent delete already won → deterministic no-op) from *unknown* (never existed →
/// refusal), and guarantees deleted IDs are never reused. It is derived purely from genesis +
/// the ordered operations, so a fresh replay reproduces it exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryV1 {
    pub v: u32,
    pub deleted_notebooks: BTreeSet<NotebookId>,
    pub deleted_pages: BTreeSet<PageId>,
    pub deleted_strokes: BTreeSet<StrokeId>,
    /// Every lineage root that has ever been referenced (live or deleted).
    pub known_roots: BTreeSet<StrokeId>,
    /// Omitted when empty, so pre-folder history keeps its canonical bytes.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub deleted_folders: BTreeSet<FolderId>,
    /// Every text object that has ever been added (live or deleted).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub known_texts: BTreeSet<TextId>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub deleted_texts: BTreeSet<TextId>,
    /// Every page link that has ever been added (live or deleted).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub known_links: BTreeSet<LinkId>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub deleted_links: BTreeSet<LinkId>,
}

impl HistoryV1 {
    pub fn empty() -> Self {
        Self {
            v: HISTORY_VERSION,
            deleted_notebooks: BTreeSet::new(),
            deleted_pages: BTreeSet::new(),
            deleted_strokes: BTreeSet::new(),
            known_roots: BTreeSet::new(),
            deleted_folders: BTreeSet::new(),
            known_texts: BTreeSet::new(),
            deleted_texts: BTreeSet::new(),
            known_links: BTreeSet::new(),
            deleted_links: BTreeSet::new(),
        }
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, SemanticError> {
        to_canonical_json(self).map_err(|e| SemanticError::new(SemanticErrorCode::InvalidState, e))
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, SemanticError> {
        let h: Self =
            from_json(bytes).map_err(|e| SemanticError::new(SemanticErrorCode::InvalidState, e))?;
        if h.v != HISTORY_VERSION {
            return Err(SemanticError::new(
                SemanticErrorCode::InvalidState,
                format!("unsupported history version {}", h.v),
            ));
        }
        Ok(h)
    }
}

/// Complete reducer state for one library.
#[derive(Clone, Debug, PartialEq)]
pub struct LibraryState {
    pub snapshot: SyncSnapshotV1,
    pub history: HistoryV1,
}

impl LibraryState {
    /// State at server sequence 0.
    pub fn genesis(snapshot: SyncSnapshotV1) -> Result<Self, SemanticError> {
        let mut history = HistoryV1::empty();
        for page in snapshot.notebooks.iter().flat_map(|n| &n.pages) {
            for s in &page.strokes {
                history.known_roots.insert(s.root_id);
            }
            for t in &page.texts {
                history.known_texts.insert(t.text_id);
            }
            for l in &page.links {
                history.known_links.insert(l.link_id);
            }
        }
        let state = Self { snapshot, history };
        state.validate()?;
        Ok(state)
    }

    /// Snapshot validity plus history consistency (no live identity is recorded as deleted;
    /// every live stroke's root is known).
    pub fn validate(&self) -> Result<(), SemanticError> {
        self.snapshot
            .validate()
            .map_err(|e| SemanticError::new(SemanticErrorCode::InvalidResult, e))?;
        let h = &self.history;
        if h.v != HISTORY_VERSION {
            return Err(SemanticError::new(
                SemanticErrorCode::InvalidState,
                "unsupported history version",
            ));
        }
        for f in &self.snapshot.folders {
            if h.deleted_folders.contains(&f.folder_id) {
                return Err(SemanticError::new(
                    SemanticErrorCode::InvalidResult,
                    format!("live folder {} is recorded as deleted", f.folder_id),
                ));
            }
        }
        for nb in &self.snapshot.notebooks {
            if h.deleted_notebooks.contains(&nb.notebook_id) {
                return Err(SemanticError::new(
                    SemanticErrorCode::InvalidResult,
                    format!("live notebook {} is recorded as deleted", nb.notebook_id),
                ));
            }
            for p in &nb.pages {
                if h.deleted_pages.contains(&p.page_id) {
                    return Err(SemanticError::new(
                        SemanticErrorCode::InvalidResult,
                        format!("live page {} is recorded as deleted", p.page_id),
                    ));
                }
                for s in &p.strokes {
                    if h.deleted_strokes.contains(&s.stroke_id) {
                        return Err(SemanticError::new(
                            SemanticErrorCode::InvalidResult,
                            format!("live stroke {} is recorded as deleted", s.stroke_id),
                        ));
                    }
                    if !h.known_roots.contains(&s.root_id) {
                        return Err(SemanticError::new(
                            SemanticErrorCode::InvalidResult,
                            format!("stroke {} has unknown root {}", s.stroke_id, s.root_id),
                        ));
                    }
                }
                for t in &p.texts {
                    if h.deleted_texts.contains(&t.text_id) {
                        return Err(SemanticError::new(
                            SemanticErrorCode::InvalidResult,
                            format!("live text {} is recorded as deleted", t.text_id),
                        ));
                    }
                    if !h.known_texts.contains(&t.text_id) {
                        return Err(SemanticError::new(
                            SemanticErrorCode::InvalidResult,
                            format!("text {} is live but not known", t.text_id),
                        ));
                    }
                }
                for l in &p.links {
                    if h.deleted_links.contains(&l.link_id) {
                        return Err(SemanticError::new(
                            SemanticErrorCode::InvalidResult,
                            format!("live link {} is recorded as deleted", l.link_id),
                        ));
                    }
                    if !h.known_links.contains(&l.link_id) {
                        return Err(SemanticError::new(
                            SemanticErrorCode::InvalidResult,
                            format!("link {} is live but not known", l.link_id),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Apply one envelope in canonical order. Pure: `self` is untouched.
    pub fn apply(&self, env: &SyncEnvelopeV1) -> Result<(LibraryState, Outcome), SemanticError> {
        crate::apply::apply_envelope(self, env)
    }

    /// [`Self::apply`] in place, for sequential replay: identical resulting state, outcome and
    /// error, without cloning the whole library per envelope (replaying *n* operations is no
    /// longer *n* full copies + *n* full validations). `self` must be valid (every state built
    /// by [`Self::genesis`] / `apply` / `validate` is). **On `Err` `self` is unspecified and
    /// must be discarded**; use [`Self::apply`] where a refusal must leave the state intact.
    pub fn apply_in_place(&mut self, env: &SyncEnvelopeV1) -> Result<Outcome, SemanticError> {
        crate::apply::apply_envelope_in_place(self, env)
    }
}

/// Fresh replay: genesis followed by envelopes in canonical order.
pub fn replay<'a>(
    genesis: SyncSnapshotV1,
    ordered: impl IntoIterator<Item = &'a SyncEnvelopeV1>,
) -> Result<LibraryState, SemanticError> {
    let mut state = LibraryState::genesis(genesis)?;
    for env in ordered {
        state = state.apply(env)?.0;
    }
    Ok(state)
}
