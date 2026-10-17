//! Immutable semantic operations (`SyncOperationV1`) inside a `SyncEnvelopeV1`.
//!
//! S1 defines and validates these; nothing transmits them yet.
//!
//! # Envelope
//! Every operation carries its own globally unique [`OperationId`] (the future server's dedup
//! key), the authoring [`ReplicaId`], the [`LibraryId`] it mutates, and `client_seq`, which
//! starts at 1 and strictly increases per replica. There is no client-authored server
//! sequence: the future server assigns canonical order separately. Payloads are immutable once
//! authored.
//!
//! # Ordering semantics (identity-based, never array indices)
//! * `CreateNotebook.after_notebook_id` / `CreatePage.after_page_id` / `MovePage.after_page_id`:
//!   `None` = first position; `Some(x)` = immediately after `x`. If `x` is unknown or deleted
//!   when the operation is applied in canonical order, the item goes to the end.
//! * A notebook always has at least one page, so `CreateNotebook` includes its first page.
//!
//! # Not operations
//! Replica-local UI intent is never synchronized: open notebook, go Home, select page,
//! show/hide chrome, and **Undo**. Undo is an authoring command; only its resulting semantic
//! mutation (today [`SyncOperationV1::DeleteStrokeLineage`]) is synchronized. Because every
//! envelope names its replica and operation, a future multi-replica undo can be defined on top.
//!
//! # Erase
//! [`SyncOperationV1::EraseRegion`] is user intent, not the local F7 `replace_strokes` journal
//! record (a materialization). It applies **only** to the stroke lineages the author observed
//! (`observed_roots`), so an offline erase can never cut a concurrent stroke its author never
//! saw. The shared notebook core evaluates it deterministically against canonical state: each
//! observed lineage's visible fragments are clipped by the capsule sweep of `path` with
//! `radius`; resulting fragment IDs are UUIDv5(namespace = `operation_id`, name =
//! `"<clipped stroke id>/<fragment index>"`), so every replica derives identical IDs.
//!
//! # Folders
//! Folders group notebooks for display. A notebook is in at most one folder; the folder list
//! keeps creation (canonical) order. [`SyncOperationV1::DeleteFolder`] never deletes notebooks:
//! its members return to the top level. [`SyncOperationV1::SetNotebookFolder`] with no
//! `folder_id` moves a notebook back to the top level.
//!
//! # Whole-stroke delete
//! [`SyncOperationV1::DeleteStrokeLineage`] removes every visible fragment of one lineage. It is
//! a separate operation (not an `EraseRegion`) because undo removes a lineage regardless of
//! geometry; expressing that as a region sweep could clip unrelated strokes.
//!
//! # Explicit lineage replacement
//! [`SyncOperationV1::ReplaceStrokeLineages`] is a compare-and-swap of **named** lineages on
//! one page. It carries the exact visible members before (`expected`) and after (`after`),
//! including caller-supplied fragment identities. It is **not** an erase gesture: no path,
//! radius, or derived UUIDv5 fragment IDs. Untargeted strokes are untouched. A whole-page
//! replacement is out of scope. Old decoders that do not know this variant fail closed
//! (`unknown variant`); they must not skip it.
//!
//! # Move stroke lineages
//! [`SyncOperationV1::MoveStrokeLineages`] translates the visible geometry of one or more
//! authored lineages without changing any stroke or lineage identity. `root_ids` is sorted and
//! unique. Selection and drag-preview state are replica-local and never enter this operation.
//!
//! # Text
//! [`SyncOperationV1::AddText`] / [`SyncOperationV1::UpdateText`] / [`SyncOperationV1::DeleteText`]
//! treat a [`crate::content::SyncText`] as one object. There is no keystroke stream and no
//! field-level merge: a later accepted `UpdateText` replaces the complete previous value
//! (authoritative server/reducer order). `AddText` refuses a live or previously retired
//! `text_id`. `UpdateText` refuses unknown or deleted IDs. `DeleteText` is a NoOp if the object
//! is already gone. Content is Unicode-scalar-counted, line-endings already LF, never trimmed.
//!
//! # Page links
//! [`SyncOperationV1::AddPageLink`] / [`SyncOperationV1::DeletePageLink`] treat a
//! [`crate::content::SyncPageLink`] as one explicit spatial object (a tap location on a source
//! page pointing at a target page). Identity is [`crate::ids::LinkId`] plus global notebook/page
//! IDs — never titles or page numbers. Same-page self-links are refused. There is no OCR and no
//! handwritten `[[WikiLink]]` recognition. `DeletePageLink` is a NoOp if the object is already
//! gone. Retargeting is remove + recreate (no `UpdatePageLink` in this slice).

use serde::{Deserialize, Serialize};

use crate::canonical::{from_json, to_canonical_json};
use crate::content::{
    validate_title, BackgroundRef, RenderProfileRef, SyncPageLink, SyncStroke, SyncText,
};
use crate::error::{refuse, Result};
use crate::geometry::{SyncPoint, CANONICAL_PAGE_WIDTH};
use crate::ids::{
    FolderId, LibraryId, LinkId, NotebookId, OperationId, PageId, ReplicaId, StrokeId, TextId,
};

pub const ENVELOPE_VERSION: u32 = 1;
/// Upper bound on sweep samples in one erase gesture (defensive).
pub const MAX_ERASE_PATH_POINTS: usize = 100_000;
/// Upper bound on named roots in one [`SyncOperationV1::ReplaceStrokeLineages`].
pub const MAX_REPLACE_LINEAGE_ROOTS: usize = 1_024;
/// Upper bound on total `after` strokes in one [`SyncOperationV1::ReplaceStrokeLineages`].
pub const MAX_REPLACE_AFTER_STROKES: usize = 4_096;

/// One named lineage in [`SyncOperationV1::ReplaceStrokeLineages`].
///
/// `expected` is the exact visible membership of `root_id` on the page (render order) that
/// must hold before the replacement. `after` is the exact visible membership afterward.
/// Empty `after` means the lineage has no remaining visible members on the page (a CAS
/// delete of those members, not a whole-page edit). Identities in `after` are taken as
/// given — apply/replay must not mint fragment IDs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineageReplacement {
    pub root_id: StrokeId,
    pub expected: Vec<SyncStroke>,
    pub after: Vec<SyncStroke>,
}

/// A page as created: identity + durable appearance. Strokes arrive as `AddStroke`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageSpec {
    pub page_id: PageId,
    pub background: BackgroundRef,
    pub render: RenderProfileRef,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SyncOperationV1 {
    CreateNotebook {
        notebook_id: NotebookId,
        title: String,
        after_notebook_id: Option<NotebookId>,
        first_page: PageSpec,
    },
    RenameNotebook {
        notebook_id: NotebookId,
        title: String,
    },
    DeleteNotebook {
        notebook_id: NotebookId,
    },
    CreatePage {
        notebook_id: NotebookId,
        page: PageSpec,
        after_page_id: Option<PageId>,
    },
    MovePage {
        notebook_id: NotebookId,
        page_id: PageId,
        after_page_id: Option<PageId>,
    },
    DeletePage {
        notebook_id: NotebookId,
        page_id: PageId,
    },
    /// A completed, durable stroke (never streamed point-by-point). A new authored stroke is its
    /// own lineage root.
    AddStroke {
        notebook_id: NotebookId,
        page_id: PageId,
        stroke: SyncStroke,
    },
    EraseRegion {
        notebook_id: NotebookId,
        page_id: PageId,
        /// Lineage roots visible to the author when the gesture ran; sorted, unique, non-empty.
        observed_roots: Vec<StrokeId>,
        /// Eraser sweep centerline in canonical page space.
        path: Vec<SyncPoint>,
        /// Capsule radius in canonical page units.
        radius: f32,
    },
    DeleteStrokeLineage {
        notebook_id: NotebookId,
        page_id: PageId,
        root_id: StrokeId,
    },
    /// Compare-and-swap the visible members of the named lineages on one page.
    ReplaceStrokeLineages {
        notebook_id: NotebookId,
        page_id: PageId,
        /// Strictly sorted unique roots. The operation touches only these lineages.
        replacements: Vec<LineageReplacement>,
    },
    /// Translate every visible fragment belonging to the named authored lineages.
    MoveStrokeLineages {
        notebook_id: NotebookId,
        page_id: PageId,
        /// Sorted, unique, non-empty authored lineage roots.
        root_ids: Vec<StrokeId>,
        /// Canonical page-space translation.
        dx: f32,
        dy: f32,
    },
    /// Place a new text object. `text.text_id` must never have been used (live or retired).
    AddText {
        notebook_id: NotebookId,
        page_id: PageId,
        text: SyncText,
    },
    /// Replace the live object in full (identity + layout + content). Unknown or deleted IDs refuse.
    UpdateText {
        notebook_id: NotebookId,
        page_id: PageId,
        text: SyncText,
    },
    DeleteText {
        notebook_id: NotebookId,
        page_id: PageId,
        text_id: TextId,
    },
    /// Place an explicit spatial page-to-page link on `page_id` (must be the link's source).
    AddPageLink {
        notebook_id: NotebookId,
        page_id: PageId,
        link: SyncPageLink,
    },
    DeletePageLink {
        notebook_id: NotebookId,
        page_id: PageId,
        link_id: LinkId,
    },
    /// Appended after the existing folders.
    CreateFolder {
        folder_id: FolderId,
        title: String,
    },
    RenameFolder {
        folder_id: FolderId,
        title: String,
    },
    /// Members return to the top level; no notebook is deleted.
    DeleteFolder {
        folder_id: FolderId,
    },
    /// File `notebook_id` in `folder_id`, or (absent) move it back to the top level.
    SetNotebookFolder {
        notebook_id: NotebookId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder_id: Option<FolderId>,
    },
}

impl SyncOperationV1 {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::CreateNotebook {
                notebook_id,
                title,
                after_notebook_id,
                first_page,
            } => {
                validate_title(title)?;
                if *after_notebook_id == Some(*notebook_id) {
                    refuse!("notebook {notebook_id} cannot be created after itself");
                }
                validate_page_spec(first_page)
            }
            Self::RenameNotebook { title, .. } => validate_title(title),
            Self::DeleteNotebook { .. } | Self::DeletePage { .. } => Ok(()),
            Self::CreatePage {
                page,
                after_page_id,
                ..
            } => {
                if *after_page_id == Some(page.page_id) {
                    refuse!("page {} cannot be created after itself", page.page_id);
                }
                validate_page_spec(page)
            }
            Self::MovePage {
                page_id,
                after_page_id,
                ..
            } => {
                if *after_page_id == Some(*page_id) {
                    refuse!("page {page_id} cannot move after itself");
                }
                Ok(())
            }
            Self::AddStroke { stroke, .. } => {
                if stroke.root_id != stroke.stroke_id {
                    refuse!(
                        "AddStroke {} must be its own lineage root (got {})",
                        stroke.stroke_id,
                        stroke.root_id
                    );
                }
                stroke.validate()
            }
            Self::EraseRegion {
                observed_roots,
                path,
                radius,
                ..
            } => {
                if observed_roots.is_empty() {
                    refuse!("EraseRegion observed no stroke lineages");
                }
                if observed_roots.windows(2).any(|w| w[0] >= w[1]) {
                    refuse!("EraseRegion observed_roots must be strictly sorted and unique");
                }
                if path.is_empty() || path.len() > MAX_ERASE_PATH_POINTS {
                    refuse!("EraseRegion path must have 1..={MAX_ERASE_PATH_POINTS} points");
                }
                for p in path {
                    p.validate()?;
                }
                if !radius.is_finite() || *radius <= 0.0 || *radius > CANONICAL_PAGE_WIDTH {
                    refuse!("EraseRegion radius {radius:?} out of range");
                }
                Ok(())
            }
            Self::DeleteStrokeLineage { .. } => Ok(()),
            Self::ReplaceStrokeLineages { replacements, .. } => {
                validate_replace_stroke_lineages(replacements)
            }
            Self::MoveStrokeLineages {
                root_ids, dx, dy, ..
            } => {
                if root_ids.is_empty() {
                    refuse!("MoveStrokeLineages names no stroke lineages");
                }
                if root_ids.windows(2).any(|w| w[0] >= w[1]) {
                    refuse!("MoveStrokeLineages root_ids must be strictly sorted and unique");
                }
                if !dx.is_finite() || !dy.is_finite() {
                    refuse!("MoveStrokeLineages delta must be finite");
                }
                if *dx == 0.0 && *dy == 0.0 {
                    refuse!("MoveStrokeLineages delta must not be zero");
                }
                if dx.abs() > CANONICAL_PAGE_WIDTH
                    || dy.abs() > crate::geometry::CANONICAL_PAGE_HEIGHT
                {
                    refuse!("MoveStrokeLineages delta is larger than the canonical page");
                }
                Ok(())
            }
            Self::AddText { text, .. } | Self::UpdateText { text, .. } => text.validate(),
            Self::DeleteText { .. } => Ok(()),
            Self::AddPageLink {
                notebook_id,
                page_id,
                link,
            } => {
                link.validate()?;
                if link.source_notebook_id != *notebook_id || link.source_page_id != *page_id {
                    refuse!(
                        "AddPageLink source {}/{} must match the addressed page {}/{}",
                        link.source_notebook_id,
                        link.source_page_id,
                        notebook_id,
                        page_id
                    );
                }
                Ok(())
            }
            Self::DeletePageLink { .. } => Ok(()),
            Self::CreateFolder { title, .. } | Self::RenameFolder { title, .. } => {
                validate_title(title)
            }
            Self::DeleteFolder { .. } | Self::SetNotebookFolder { .. } => Ok(()),
        }
    }
}

fn validate_page_spec(p: &PageSpec) -> Result<()> {
    p.background.validate()?;
    p.render.validate()
}

fn validate_replace_stroke_lineages(replacements: &[LineageReplacement]) -> Result<()> {
    if replacements.is_empty() {
        refuse!("ReplaceStrokeLineages names no stroke lineages");
    }
    if replacements.len() > MAX_REPLACE_LINEAGE_ROOTS {
        refuse!("ReplaceStrokeLineages exceeds {MAX_REPLACE_LINEAGE_ROOTS} targeted roots");
    }
    let mut after_n = 0usize;
    let mut after_ids = std::collections::HashSet::new();
    for (i, r) in replacements.iter().enumerate() {
        if i > 0 && replacements[i - 1].root_id >= r.root_id {
            refuse!("ReplaceStrokeLineages replacements must be strictly sorted and unique");
        }
        if r.expected.is_empty() && r.after.is_empty() {
            refuse!(
                "ReplaceStrokeLineages replacement for {} is empty",
                r.root_id
            );
        }
        after_n += r.after.len();
        let mut expected_ids = std::collections::HashSet::new();
        for s in &r.expected {
            if s.root_id != r.root_id {
                refuse!(
                    "ReplaceStrokeLineages expected stroke {} has root {} (need {})",
                    s.stroke_id,
                    s.root_id,
                    r.root_id
                );
            }
            if !expected_ids.insert(s.stroke_id) {
                refuse!(
                    "ReplaceStrokeLineages expected membership repeats {}",
                    s.stroke_id
                );
            }
            s.validate()?;
        }
        for s in &r.after {
            if s.root_id != r.root_id {
                refuse!(
                    "ReplaceStrokeLineages after stroke {} has root {} (need {})",
                    s.stroke_id,
                    s.root_id,
                    r.root_id
                );
            }
            if !after_ids.insert(s.stroke_id) {
                refuse!(
                    "ReplaceStrokeLineages after membership repeats {}",
                    s.stroke_id
                );
            }
            s.validate()?;
        }
    }
    if after_n > MAX_REPLACE_AFTER_STROKES {
        refuse!("ReplaceStrokeLineages exceeds {MAX_REPLACE_AFTER_STROKES} after strokes");
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncEnvelopeV1 {
    pub v: u32,
    pub operation_id: OperationId,
    pub library_id: LibraryId,
    pub replica_id: ReplicaId,
    /// Per-replica authoring sequence: 1, 2, 3, … (strictly increasing).
    pub client_seq: u64,
    pub operation: SyncOperationV1,
}

impl SyncEnvelopeV1 {
    pub fn validate(&self) -> Result<()> {
        if self.v != ENVELOPE_VERSION {
            refuse!(
                "unsupported envelope version {} (need {ENVELOPE_VERSION})",
                self.v
            );
        }
        if self.client_seq == 0 {
            refuse!("client_seq starts at 1");
        }
        self.operation.validate()
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        to_canonical_json(self)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let e: Self = from_json(bytes)?;
        e.validate()?;
        Ok(e)
    }
}

/// Per-replica `client_seq` rule: the next envelope from the same replica must be exactly
/// `previous + 1` (`None` = first envelope, which must be 1). Checked arithmetic: after
/// `u64::MAX` the sequence is exhausted — it never saturates or wraps.
pub fn check_client_seq(previous: Option<u64>, next: u64) -> Result<()> {
    let want = match previous {
        None => 1,
        Some(p) => match p.checked_add(1) {
            Some(w) => w,
            None => refuse!("client_seq exhausted: previous {p} is u64::MAX"),
        },
    };
    if next != want {
        refuse!("client_seq {next} out of order (expected {want})");
    }
    Ok(())
}
