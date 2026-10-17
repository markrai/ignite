//! Canonical application rules for every `SyncOperationV1`.
//!
//! Target resolution uses three cases, identically for every operation:
//!
//! ```text
//! live      → apply
//! deleted   → deterministic NoOp (a delete already ordered earlier wins; the op is still
//!             accepted so offline replicas keep progressing)
//! unknown   → refuse (never existed: malformed or from another library)
//! ```
//!
//! Placement (`after_*_id`): `None` → first; `Some(x)` live in the same container → right
//! after `x`; `Some(unknown/deleted/elsewhere)` → end. Indices are never identity.
//!
//! Structural minimums: a notebook always keeps ≥ 1 page and a library ≥ 1 notebook; deleting
//! the sole page / sole notebook is refused (`sole_page` / `sole_notebook`).
//!
//! IDs are never reused: creating a notebook/page/stroke/folder/text whose ID is live *or* deleted is
//! refused as a duplicate.
//!
//! [`SyncOperationV1::ReplaceStrokeLineages`] compare-and-swaps named lineages on one page
//! (plan every root, then mutate once). Fragment IDs are caller-supplied. A deleted page is a
//! NoOp so concurrent delete still lets history replay. `live == after` is a reducer NoOp, not
//! ledger authorization.
//!
//! Text is whole-object last-write-wins: `UpdateText` replaces the complete live value. A later
//! accepted update wins wholly. `DeleteText` after delete is a NoOp; `UpdateText` after delete
//! or for an unknown ID is refused.
//!
//! Folders: `CreateFolder` appends; `DeleteFolder` returns its members to the top level;
//! `SetNotebookFolder` targets resolve like everything else (a deleted notebook *or* a deleted
//! destination folder → NoOp; never-existing → refused).

use std::collections::{BTreeSet, HashSet};
use std::fmt;

use sync_protocol::{
    validate_title, FolderId, LineageReplacement, LinkId, NotebookId, OperationId, PageId,
    SnapshotFolder, SnapshotNotebook, SnapshotPage, StrokeId, StrokeStyle, SyncEnvelopeV1,
    SyncOperationV1, SyncStroke, TextId,
};

use crate::erase::erase_page_strokes;
use crate::state::LibraryState;

/// Why an operation was refused. Stable snake_case codes (HTTP surfaces them verbatim).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticErrorCode {
    /// Structurally invalid operation (fails `SyncOperationV1::validate`).
    InvalidOperation,
    WrongLibrary,
    UnknownNotebook,
    UnknownPage,
    UnknownLineage,
    UnknownFolder,
    DuplicateNotebook,
    DuplicatePage,
    DuplicateStroke,
    DuplicateText,
    UnknownText,
    DuplicateLink,
    UnknownLink,
    DuplicateFolder,
    PageNotInNotebook,
    SolePage,
    SoleNotebook,
    /// The application would produce a snapshot `SyncSnapshotV1::validate` rejects.
    InvalidResult,
    /// Corrupt or unsupported reducer state (not the operation's fault).
    InvalidState,
    /// A named lineage's live members match neither `expected` nor `after`.
    ConflictSameLineage,
}

impl SemanticErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidOperation => "invalid_operation",
            Self::WrongLibrary => "wrong_library",
            Self::UnknownNotebook => "unknown_notebook",
            Self::UnknownPage => "unknown_page",
            Self::UnknownLineage => "unknown_lineage",
            Self::UnknownFolder => "unknown_folder",
            Self::DuplicateNotebook => "duplicate_notebook",
            Self::DuplicatePage => "duplicate_page",
            Self::DuplicateStroke => "duplicate_stroke",
            Self::DuplicateText => "duplicate_text",
            Self::UnknownText => "unknown_text",
            Self::DuplicateLink => "duplicate_link",
            Self::UnknownLink => "unknown_link",
            Self::DuplicateFolder => "duplicate_folder",
            Self::PageNotInNotebook => "page_not_in_notebook",
            Self::SolePage => "sole_page",
            Self::SoleNotebook => "sole_notebook",
            Self::InvalidResult => "invalid_result",
            Self::InvalidState => "invalid_state",
            Self::ConflictSameLineage => "conflict_same_lineage",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticError {
    pub code: SemanticErrorCode,
    pub message: String,
}

impl SemanticError {
    pub fn new(code: SemanticErrorCode, message: impl fmt::Display) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
}

impl fmt::Display for SemanticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for SemanticError {}

/// Successful application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// State changed (or a rename set the same title).
    Applied,
    /// Accepted, state unchanged (target deleted earlier, or erase touched nothing).
    NoOp(&'static str),
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::NoOp(_) => "noop",
        }
    }
}

use SemanticErrorCode as C;

fn err<T>(code: SemanticErrorCode, msg: impl fmt::Display) -> Result<T, SemanticError> {
    Err(SemanticError::new(code, msg))
}

enum Target<T> {
    Live(T),
    Deleted,
}

fn find_notebook(st: &LibraryState, id: NotebookId) -> Result<Target<usize>, SemanticError> {
    if let Some(i) = st
        .snapshot
        .notebooks
        .iter()
        .position(|n| n.notebook_id == id)
    {
        return Ok(Target::Live(i));
    }
    if st.history.deleted_notebooks.contains(&id) {
        return Ok(Target::Deleted);
    }
    err(C::UnknownNotebook, format!("notebook {id} never existed"))
}

fn page_is_live_anywhere(st: &LibraryState, id: PageId) -> bool {
    st.snapshot
        .notebooks
        .iter()
        .any(|n| n.pages.iter().any(|p| p.page_id == id))
}

/// Resolve (notebook, page) for page-scoped operations.
fn find_page(
    st: &LibraryState,
    nb: NotebookId,
    page: PageId,
) -> Result<Target<(usize, usize)>, SemanticError> {
    let ni = match find_notebook(st, nb)? {
        Target::Live(i) => i,
        Target::Deleted => return Ok(Target::Deleted),
    };
    if let Some(pi) = st.snapshot.notebooks[ni]
        .pages
        .iter()
        .position(|p| p.page_id == page)
    {
        return Ok(Target::Live((ni, pi)));
    }
    if st.history.deleted_pages.contains(&page) {
        return Ok(Target::Deleted);
    }
    if page_is_live_anywhere(st, page) {
        return err(
            C::PageNotInNotebook,
            format!("page {page} is not in notebook {nb}"),
        );
    }
    err(C::UnknownPage, format!("page {page} never existed"))
}

fn stroke_id_used(st: &LibraryState, id: StrokeId) -> bool {
    st.history.deleted_strokes.contains(&id)
        || st
            .snapshot
            .notebooks
            .iter()
            .flat_map(|n| &n.pages)
            .flat_map(|p| &p.strokes)
            .any(|s| s.stroke_id == id)
}

/// Insert at the S1 placement position: `anchored == false` → first; otherwise right after the
/// first item matching `is_anchor`, or at the end when no item matches (unknown/deleted anchor).
fn place<T>(items: &mut Vec<T>, item: T, anchored: bool, is_anchor: impl Fn(&T) -> bool) {
    let at = if !anchored {
        0
    } else {
        items
            .iter()
            .position(is_anchor)
            .map_or(items.len(), |i| i + 1)
    };
    items.insert(at, item);
}

fn retire_page(st: &mut LibraryState, page: &SnapshotPage) {
    st.history.deleted_pages.insert(page.page_id);
    for s in &page.strokes {
        st.history.deleted_strokes.insert(s.stroke_id);
    }
    for t in &page.texts {
        st.history.deleted_texts.insert(t.text_id);
    }
    for l in &page.links {
        st.history.deleted_links.insert(l.link_id);
    }
}

fn text_id_used(st: &LibraryState, id: TextId) -> bool {
    st.history.known_texts.contains(&id)
        || st.history.deleted_texts.contains(&id)
        || st
            .snapshot
            .notebooks
            .iter()
            .flat_map(|n| &n.pages)
            .flat_map(|p| &p.texts)
            .any(|t| t.text_id == id)
}

fn link_id_used(st: &LibraryState, id: LinkId) -> bool {
    st.history.known_links.contains(&id)
        || st.history.deleted_links.contains(&id)
        || st
            .snapshot
            .notebooks
            .iter()
            .flat_map(|n| &n.pages)
            .flat_map(|p| &p.links)
            .any(|l| l.link_id == id)
}

fn live_lineage<'a>(strokes: &'a [SyncStroke], root: StrokeId) -> Vec<&'a SyncStroke> {
    strokes.iter().filter(|s| s.root_id == root).collect()
}

fn lineage_eq(live: &[&SyncStroke], listed: &[SyncStroke]) -> bool {
    live.len() == listed.len() && live.iter().zip(listed).all(|(a, b)| *a == b)
}

/// Compare-and-swap named lineages. Plans every root, then mutates once.
fn apply_replace_stroke_lineages(
    st: &mut LibraryState,
    notebook_id: NotebookId,
    page_id: PageId,
    replacements: &[LineageReplacement],
) -> Result<Outcome, SemanticError> {
    match find_page(st, notebook_id, page_id)? {
        Target::Deleted => Ok(Outcome::NoOp("page or notebook deleted")),
        Target::Live((ni, pi)) => {
            let page_strokes = st.snapshot.notebooks[ni].pages[pi].strokes.clone();
            let mut pending = 0u32;
            let mut already = 0u32;
            for r in replacements {
                let live = live_lineage(&page_strokes, r.root_id);
                if lineage_eq(&live, &r.after) {
                    already += 1;
                } else if lineage_eq(&live, &r.expected) {
                    pending += 1;
                } else {
                    return err(
                        C::ConflictSameLineage,
                        format!(
                            "lineage {} on page {page_id} matches neither expected nor after",
                            r.root_id
                        ),
                    );
                }
            }
            if already > 0 && pending > 0 {
                return err(
                    C::InvalidOperation,
                    "ReplaceStrokeLineages mixes already-applied and pending roots",
                );
            }
            if pending == 0 {
                return Ok(Outcome::NoOp("already applied"));
            }

            let mut releasing = HashSet::new();
            for r in replacements {
                for s in &r.expected {
                    releasing.insert(s.stroke_id);
                }
            }
            for r in replacements {
                if r.expected.is_empty() {
                    if st.history.known_roots.contains(&r.root_id) {
                        return err(
                            C::UnknownLineage,
                            format!(
                                "lineage {} is known but not visible; refusing silent resurrection",
                                r.root_id
                            ),
                        );
                    }
                } else if !st.history.known_roots.contains(&r.root_id) {
                    return err(
                        C::UnknownLineage,
                        format!("lineage {} never existed", r.root_id),
                    );
                }
                for s in &r.after {
                    if stroke_id_used(st, s.stroke_id) && !releasing.contains(&s.stroke_id) {
                        return err(
                            C::DuplicateStroke,
                            format!("stroke {} already used", s.stroke_id),
                        );
                    }
                }
            }

            let targeted: BTreeSet<StrokeId> = replacements.iter().map(|r| r.root_id).collect();
            let by_root: std::collections::BTreeMap<StrokeId, &LineageReplacement> =
                replacements.iter().map(|r| (r.root_id, r)).collect();
            let mut emitted = BTreeSet::new();
            let mut new_strokes = Vec::with_capacity(page_strokes.len());
            for s in &page_strokes {
                if targeted.contains(&s.root_id) {
                    if emitted.insert(s.root_id) {
                        new_strokes.extend(by_root[&s.root_id].after.iter().cloned());
                    }
                } else {
                    new_strokes.push(s.clone());
                }
            }
            for r in replacements {
                if emitted.insert(r.root_id) {
                    new_strokes.extend(r.after.iter().cloned());
                }
                if r.expected.is_empty() {
                    st.history.known_roots.insert(r.root_id);
                }
                for s in &r.expected {
                    if !r.after.iter().any(|a| a.stroke_id == s.stroke_id) {
                        st.history.deleted_strokes.insert(s.stroke_id);
                    }
                }
            }
            st.snapshot.notebooks[ni].pages[pi].strokes = new_strokes;
            Ok(Outcome::Applied)
        }
    }
}

pub(crate) fn apply_envelope(
    state: &LibraryState,
    env: &SyncEnvelopeV1,
) -> Result<(LibraryState, Outcome), SemanticError> {
    env.validate()
        .map_err(|e| SemanticError::new(C::InvalidOperation, e))?;
    if env.library_id != state.snapshot.library_id {
        return err(
            C::WrongLibrary,
            format!(
                "envelope library {} != state library {}",
                env.library_id, state.snapshot.library_id
            ),
        );
    }
    let mut next = state.clone();
    let outcome = apply_operation(&mut next, env.operation_id, &env.operation)?;
    if outcome != Outcome::Applied && next != *state {
        return err(C::InvalidState, "internal: no-op changed state");
    }
    next.validate()?;
    Ok((next, outcome))
}

fn apply_operation(
    st: &mut LibraryState,
    operation_id: OperationId,
    op: &SyncOperationV1,
) -> Result<Outcome, SemanticError> {
    match op {
        SyncOperationV1::CreateNotebook {
            notebook_id,
            title,
            after_notebook_id,
            first_page,
        } => {
            let id = *notebook_id;
            if st.snapshot.notebooks.iter().any(|n| n.notebook_id == id)
                || st.history.deleted_notebooks.contains(&id)
            {
                return err(C::DuplicateNotebook, format!("notebook {id} already used"));
            }
            if page_is_live_anywhere(st, first_page.page_id)
                || st.history.deleted_pages.contains(&first_page.page_id)
            {
                return err(
                    C::DuplicatePage,
                    format!("page {} already used", first_page.page_id),
                );
            }
            let nb = SnapshotNotebook {
                notebook_id: id,
                title: title.clone(),
                pages: vec![SnapshotPage {
                    page_id: first_page.page_id,
                    background: first_page.background.clone(),
                    render: first_page.render.clone(),
                    strokes: Vec::new(),
                    texts: Vec::new(),
                    links: Vec::new(),
                }],
                folder_id: None,
            };
            let after = *after_notebook_id;
            place(&mut st.snapshot.notebooks, nb, after.is_some(), |n| {
                Some(n.notebook_id) == after
            });
            Ok(Outcome::Applied)
        }
        SyncOperationV1::RenameNotebook { notebook_id, title } => {
            match find_notebook(st, *notebook_id)? {
                Target::Deleted => Ok(Outcome::NoOp("notebook deleted")),
                Target::Live(i) => {
                    st.snapshot.notebooks[i].title = title.clone();
                    Ok(Outcome::Applied)
                }
            }
        }
        SyncOperationV1::DeleteNotebook { notebook_id } => match find_notebook(st, *notebook_id)? {
            Target::Deleted => Ok(Outcome::NoOp("notebook already deleted")),
            Target::Live(i) => {
                if st.snapshot.notebooks.len() == 1 {
                    return err(
                        C::SoleNotebook,
                        format!("notebook {notebook_id} is the library's only notebook"),
                    );
                }
                let nb = st.snapshot.notebooks.remove(i);
                st.history.deleted_notebooks.insert(nb.notebook_id);
                for p in &nb.pages {
                    retire_page(st, p);
                }
                Ok(Outcome::Applied)
            }
        },
        SyncOperationV1::CreatePage {
            notebook_id,
            page,
            after_page_id,
        } => {
            let ni = match find_notebook(st, *notebook_id)? {
                Target::Deleted => return Ok(Outcome::NoOp("notebook deleted")),
                Target::Live(i) => i,
            };
            if page_is_live_anywhere(st, page.page_id)
                || st.history.deleted_pages.contains(&page.page_id)
            {
                return err(
                    C::DuplicatePage,
                    format!("page {} already used", page.page_id),
                );
            }
            let new_page = SnapshotPage {
                page_id: page.page_id,
                background: page.background.clone(),
                render: page.render.clone(),
                strokes: Vec::new(),
                texts: Vec::new(),
                links: Vec::new(),
            };
            let after = *after_page_id;
            place(
                &mut st.snapshot.notebooks[ni].pages,
                new_page,
                after.is_some(),
                |p| Some(p.page_id) == after,
            );
            Ok(Outcome::Applied)
        }
        SyncOperationV1::MovePage {
            notebook_id,
            page_id,
            after_page_id,
        } => match find_page(st, *notebook_id, *page_id)? {
            Target::Deleted => Ok(Outcome::NoOp("page or notebook deleted")),
            Target::Live((ni, pi)) => {
                let pages = &mut st.snapshot.notebooks[ni].pages;
                let page = pages.remove(pi);
                let after = *after_page_id;
                place(pages, page, after.is_some(), |p| Some(p.page_id) == after);
                Ok(Outcome::Applied)
            }
        },
        SyncOperationV1::DeletePage {
            notebook_id,
            page_id,
        } => match find_page(st, *notebook_id, *page_id)? {
            Target::Deleted => Ok(Outcome::NoOp("page or notebook already deleted")),
            Target::Live((ni, pi)) => {
                if st.snapshot.notebooks[ni].pages.len() == 1 {
                    return err(
                        C::SolePage,
                        format!("page {page_id} is notebook {notebook_id}'s only page"),
                    );
                }
                let page = st.snapshot.notebooks[ni].pages.remove(pi);
                retire_page(st, &page);
                Ok(Outcome::Applied)
            }
        },
        SyncOperationV1::AddStroke {
            notebook_id,
            page_id,
            stroke,
        } => match find_page(st, *notebook_id, *page_id)? {
            Target::Deleted => Ok(Outcome::NoOp("page or notebook deleted")),
            Target::Live((ni, pi)) => {
                if stroke_id_used(st, stroke.stroke_id) {
                    return err(
                        C::DuplicateStroke,
                        format!("stroke {} already used", stroke.stroke_id),
                    );
                }
                if st.history.known_roots.contains(&stroke.root_id) {
                    return err(
                        C::DuplicateStroke,
                        format!("lineage root {} already used", stroke.root_id),
                    );
                }
                st.history.known_roots.insert(stroke.root_id);
                st.snapshot.notebooks[ni].pages[pi]
                    .strokes
                    .push(stroke.clone());
                Ok(Outcome::Applied)
            }
        },
        SyncOperationV1::DeleteStrokeLineage {
            notebook_id,
            page_id,
            root_id,
        } => match find_page(st, *notebook_id, *page_id)? {
            Target::Deleted => Ok(Outcome::NoOp("page or notebook deleted")),
            Target::Live((ni, pi)) => {
                if !st.history.known_roots.contains(root_id) {
                    return err(
                        C::UnknownLineage,
                        format!("lineage {root_id} never existed"),
                    );
                }
                let strokes = &mut st.snapshot.notebooks[ni].pages[pi].strokes;
                let mut removed = Vec::new();
                strokes.retain(|s| {
                    let hit = s.root_id == *root_id;
                    if hit {
                        removed.push(s.stroke_id);
                    }
                    !hit
                });
                if removed.is_empty() {
                    return Ok(Outcome::NoOp("lineage has no visible fragments on page"));
                }
                st.history.deleted_strokes.extend(removed);
                Ok(Outcome::Applied)
            }
        },
        SyncOperationV1::MoveStrokeLineages {
            notebook_id,
            page_id,
            root_ids,
            dx,
            dy,
        } => match find_page(st, *notebook_id, *page_id)? {
            Target::Deleted => Ok(Outcome::NoOp("page or notebook deleted")),
            Target::Live((ni, pi)) => {
                if let Some(root) = root_ids
                    .iter()
                    .find(|root| !st.history.known_roots.contains(root))
                {
                    return err(C::UnknownLineage, format!("lineage {root} never existed"));
                }
                let selected: BTreeSet<StrokeId> = root_ids.iter().copied().collect();
                let strokes = &mut st.snapshot.notebooks[ni].pages[pi].strokes;
                if !strokes.iter().any(|s| selected.contains(&s.root_id)) {
                    return Ok(Outcome::NoOp("lineages have no visible fragments on page"));
                }
                // Validate the complete result before changing the state. A client normally
                // clamps at the page edge; concurrent accepted moves can still make a stale
                // delta invalid, in which case canonical order deterministically refuses it.
                for stroke in strokes.iter().filter(|s| selected.contains(&s.root_id)) {
                    for point in &stroke.points {
                        let moved = sync_protocol::SyncPoint::new(
                            point.x + *dx,
                            point.y + *dy,
                            point.pressure,
                        );
                        if let Err(e) = moved.validate() {
                            return err(
                                C::InvalidResult,
                                format!(
                                    "moving lineage {} would leave the canonical page: {e}",
                                    stroke.root_id
                                ),
                            );
                        }
                    }
                }
                for stroke in strokes.iter_mut().filter(|s| selected.contains(&s.root_id)) {
                    for point in &mut stroke.points {
                        point.x += *dx;
                        point.y += *dy;
                    }
                }
                Ok(Outcome::Applied)
            }
        },
        SyncOperationV1::EraseRegion {
            notebook_id,
            page_id,
            observed_roots,
            path,
            radius,
        } => match find_page(st, *notebook_id, *page_id)? {
            Target::Deleted => Ok(Outcome::NoOp("page or notebook deleted")),
            Target::Live((ni, pi)) => {
                let observed: BTreeSet<StrokeId> = observed_roots.iter().copied().collect();
                if let Some(r) = observed
                    .iter()
                    .find(|r| !st.history.known_roots.contains(r))
                {
                    return err(
                        C::UnknownLineage,
                        format!("observed lineage {r} never existed"),
                    );
                }
                let res = erase_page_strokes(
                    &st.snapshot.notebooks[ni].pages[pi].strokes,
                    &observed,
                    path,
                    *radius,
                    operation_id,
                );
                if res.replaced.is_empty() {
                    return Ok(Outcome::NoOp("sweep touched no observed stroke"));
                }
                let new_ids: Vec<StrokeId> = res
                    .strokes
                    .iter()
                    .map(|s| s.stroke_id)
                    .filter(|id| !res.replaced.contains(id))
                    .collect();
                for id in &new_ids {
                    let already = st.history.deleted_strokes.contains(id);
                    if already {
                        return err(C::DuplicateStroke, format!("fragment {id} already used"));
                    }
                }
                st.history
                    .deleted_strokes
                    .extend(res.replaced.iter().copied());
                st.snapshot.notebooks[ni].pages[pi].strokes = res.strokes;
                Ok(Outcome::Applied)
            }
        },
        SyncOperationV1::ReplaceStrokeLineages {
            notebook_id,
            page_id,
            replacements,
        } => apply_replace_stroke_lineages(st, *notebook_id, *page_id, replacements),
        SyncOperationV1::AddText {
            notebook_id,
            page_id,
            text,
        } => match find_page(st, *notebook_id, *page_id)? {
            Target::Deleted => Ok(Outcome::NoOp("page or notebook deleted")),
            Target::Live((ni, pi)) => {
                if text_id_used(st, text.text_id) {
                    return err(
                        C::DuplicateText,
                        format!("text {} already used", text.text_id),
                    );
                }
                st.history.known_texts.insert(text.text_id);
                st.snapshot.notebooks[ni].pages[pi].texts.push(text.clone());
                Ok(Outcome::Applied)
            }
        },
        SyncOperationV1::UpdateText {
            notebook_id,
            page_id,
            text,
        } => match find_page(st, *notebook_id, *page_id)? {
            Target::Deleted => Ok(Outcome::NoOp("page or notebook deleted")),
            Target::Live((ni, pi)) => {
                if st.history.deleted_texts.contains(&text.text_id) {
                    return err(
                        C::UnknownText,
                        format!("text {} has been deleted", text.text_id),
                    );
                }
                let texts = &mut st.snapshot.notebooks[ni].pages[pi].texts;
                let Some(slot) = texts.iter_mut().find(|t| t.text_id == text.text_id) else {
                    return err(
                        C::UnknownText,
                        format!("text {} never existed on this page", text.text_id),
                    );
                };
                *slot = text.clone();
                Ok(Outcome::Applied)
            }
        },
        SyncOperationV1::DeleteText {
            notebook_id,
            page_id,
            text_id,
        } => match find_page(st, *notebook_id, *page_id)? {
            Target::Deleted => Ok(Outcome::NoOp("page or notebook deleted")),
            Target::Live((ni, pi)) => {
                let texts = &mut st.snapshot.notebooks[ni].pages[pi].texts;
                let before = texts.len();
                texts.retain(|t| t.text_id != *text_id);
                if texts.len() == before {
                    if st.history.deleted_texts.contains(text_id)
                        || st.history.known_texts.contains(text_id)
                    {
                        return Ok(Outcome::NoOp("text already gone"));
                    }
                    return err(C::UnknownText, format!("text {text_id} never existed"));
                }
                st.history.deleted_texts.insert(*text_id);
                Ok(Outcome::Applied)
            }
        },
        SyncOperationV1::AddPageLink {
            notebook_id,
            page_id,
            link,
        } => match find_page(st, *notebook_id, *page_id)? {
            Target::Deleted => Ok(Outcome::NoOp("page or notebook deleted")),
            Target::Live((ni, pi)) => {
                if link_id_used(st, link.link_id) {
                    return err(
                        C::DuplicateLink,
                        format!("link {} already used", link.link_id),
                    );
                }
                if !page_is_live_anywhere(st, link.target_page_id) {
                    if st.history.deleted_pages.contains(&link.target_page_id)
                        || st
                            .history
                            .deleted_notebooks
                            .contains(&link.target_notebook_id)
                    {
                        return Ok(Outcome::NoOp("target page or notebook deleted"));
                    }
                    return err(
                        C::UnknownPage,
                        format!(
                            "link {} target page {} never existed",
                            link.link_id, link.target_page_id
                        ),
                    );
                }
                // Target notebook must be the live owner of that page (exact IDs).
                let target_ok = st.snapshot.notebooks.iter().any(|n| {
                    n.notebook_id == link.target_notebook_id
                        && n.pages.iter().any(|p| p.page_id == link.target_page_id)
                });
                if !target_ok {
                    return err(
                        C::PageNotInNotebook,
                        format!(
                            "link {} target page {} is not in notebook {}",
                            link.link_id, link.target_page_id, link.target_notebook_id
                        ),
                    );
                }
                st.history.known_links.insert(link.link_id);
                st.snapshot.notebooks[ni].pages[pi].links.push(link.clone());
                Ok(Outcome::Applied)
            }
        },
        SyncOperationV1::DeletePageLink {
            notebook_id,
            page_id,
            link_id,
        } => match find_page(st, *notebook_id, *page_id)? {
            Target::Deleted => Ok(Outcome::NoOp("page or notebook deleted")),
            Target::Live((ni, pi)) => {
                let links = &mut st.snapshot.notebooks[ni].pages[pi].links;
                let before = links.len();
                links.retain(|l| l.link_id != *link_id);
                if links.len() == before {
                    if st.history.deleted_links.contains(link_id)
                        || st.history.known_links.contains(link_id)
                    {
                        return Ok(Outcome::NoOp("link already gone"));
                    }
                    return err(C::UnknownLink, format!("link {link_id} never existed"));
                }
                st.history.deleted_links.insert(*link_id);
                Ok(Outcome::Applied)
            }
        },
        SyncOperationV1::CreateFolder { .. }
        | SyncOperationV1::RenameFolder { .. }
        | SyncOperationV1::DeleteFolder { .. }
        | SyncOperationV1::SetNotebookFolder { .. } => {
            crate::folders::apply_folder_operation(st, op)?
                .ok_or_else(|| SemanticError::new(C::InvalidState, "internal: not a folder op"))
        }
    }
}

// ------------------------------------------------------------------------------------------
// In-place application (sequential replay without a whole-library clone per operation)

/// [`apply_envelope`] without cloning the library: the same checks in the same order, the same
/// resulting state, outcome and error. Only the validity proof differs in *cost*: instead of
/// re-validating the entire snapshot, it re-checks what the operation can have changed, which
/// is exact because the state was valid before (see [`validate_touched`]). A detected violation
/// is reported by the full validator, so the error is the canonical one.
///
/// On `Err` the state is unspecified (possibly partially updated) and must be discarded.
pub(crate) fn apply_envelope_in_place(
    st: &mut LibraryState,
    env: &SyncEnvelopeV1,
) -> Result<Outcome, SemanticError> {
    env.validate()
        .map_err(|e| SemanticError::new(C::InvalidOperation, e))?;
    if env.library_id != st.snapshot.library_id {
        return err(
            C::WrongLibrary,
            format!(
                "envelope library {} != state library {}",
                env.library_id, st.snapshot.library_id
            ),
        );
    }
    let before = Footprint::of(st, &env.operation);
    let outcome = apply_operation(st, env.operation_id, &env.operation)?;
    if outcome != Outcome::Applied && Footprint::of(st, &env.operation) != before {
        return err(C::InvalidState, "internal: no-op changed state");
    }
    validate_touched(st, &env.operation)?;
    Ok(outcome)
}

/// (notebook, page) an operation addresses; `None` for library-level folder operations.
fn target_of(op: &SyncOperationV1) -> Option<(NotebookId, Option<PageId>)> {
    Some(match op {
        SyncOperationV1::CreateFolder { .. }
        | SyncOperationV1::RenameFolder { .. }
        | SyncOperationV1::DeleteFolder { .. } => return None,
        SyncOperationV1::CreateNotebook { notebook_id, .. }
        | SyncOperationV1::RenameNotebook { notebook_id, .. }
        | SyncOperationV1::DeleteNotebook { notebook_id }
        | SyncOperationV1::SetNotebookFolder { notebook_id, .. }
        | SyncOperationV1::CreatePage { notebook_id, .. } => (*notebook_id, None),
        SyncOperationV1::MovePage {
            notebook_id,
            page_id,
            ..
        }
        | SyncOperationV1::DeletePage {
            notebook_id,
            page_id,
        }
        | SyncOperationV1::AddStroke {
            notebook_id,
            page_id,
            ..
        }
        | SyncOperationV1::EraseRegion {
            notebook_id,
            page_id,
            ..
        }
        | SyncOperationV1::DeleteStrokeLineage {
            notebook_id,
            page_id,
            ..
        }
        | SyncOperationV1::MoveStrokeLineages {
            notebook_id,
            page_id,
            ..
        }
        | SyncOperationV1::AddText {
            notebook_id,
            page_id,
            ..
        }
        | SyncOperationV1::UpdateText {
            notebook_id,
            page_id,
            ..
        }
        | SyncOperationV1::DeleteText {
            notebook_id,
            page_id,
            ..
        }
        | SyncOperationV1::AddPageLink {
            notebook_id,
            page_id,
            ..
        }
        | SyncOperationV1::DeletePageLink {
            notebook_id,
            page_id,
            ..
        }
        | SyncOperationV1::ReplaceStrokeLineages {
            notebook_id,
            page_id,
            ..
        } => (*notebook_id, Some(*page_id)),
    })
}

fn is_folder_op(op: &SyncOperationV1) -> bool {
    matches!(
        op,
        SyncOperationV1::CreateFolder { .. }
            | SyncOperationV1::RenameFolder { .. }
            | SyncOperationV1::DeleteFolder { .. }
            | SyncOperationV1::SetNotebookFolder { .. }
    )
}

/// Everything a no-op could conceivably have touched, cheaply comparable (the in-place
/// counterpart of the clone path's `next != state` guard).
#[derive(PartialEq)]
struct Footprint {
    notebooks: Vec<NotebookId>,
    title: Option<String>,
    pages: Vec<PageId>,
    strokes: Option<Vec<StrokeId>>,
    texts: Option<Vec<TextId>>,
    links: Option<Vec<LinkId>>,
    /// Folder list and memberships (folder operations only; empty otherwise).
    folders: Vec<(FolderId, String)>,
    memberships: Vec<Option<FolderId>>,
    history: [usize; 9],
}

impl Footprint {
    fn of(st: &LibraryState, op: &SyncOperationV1) -> Self {
        let (nb, page) = target_of(op).map_or((None, None), |(n, p)| (Some(n), p));
        let n = st
            .snapshot
            .notebooks
            .iter()
            .find(|n| Some(n.notebook_id) == nb);
        let h = &st.history;
        let folder_op = is_folder_op(op);
        Self {
            notebooks: st
                .snapshot
                .notebooks
                .iter()
                .map(|n| n.notebook_id)
                .collect(),
            title: n.map(|n| n.title.clone()),
            pages: n.map_or_else(Vec::new, |n| n.pages.iter().map(|p| p.page_id).collect()),
            strokes: n
                .and_then(|n| n.pages.iter().find(|p| Some(p.page_id) == page))
                .map(|p| p.strokes.iter().map(|s| s.stroke_id).collect()),
            texts: n
                .and_then(|n| n.pages.iter().find(|p| Some(p.page_id) == page))
                .map(|p| p.texts.iter().map(|t| t.text_id).collect()),
            links: n
                .and_then(|n| n.pages.iter().find(|p| Some(p.page_id) == page))
                .map(|p| p.links.iter().map(|l| l.link_id).collect()),
            folders: if folder_op {
                st.snapshot
                    .folders
                    .iter()
                    .map(|f| (f.folder_id, f.title.clone()))
                    .collect()
            } else {
                Vec::new()
            },
            memberships: if folder_op {
                st.snapshot.notebooks.iter().map(|n| n.folder_id).collect()
            } else {
                Vec::new()
            },
            history: [
                h.deleted_notebooks.len(),
                h.deleted_pages.len(),
                h.deleted_strokes.len(),
                h.known_roots.len(),
                h.deleted_folders.len(),
                h.known_texts.len(),
                h.deleted_texts.len(),
                h.known_links.len(),
                h.deleted_links.len(),
            ],
        }
    }
}

/// `LibraryState::validate` after one in-place application to a **valid** state, checking only
/// what the operation can have changed. Why that is exact, per validator rule:
///
/// * duplicate notebook/page IDs: creates refuse any live or deleted ID; moves/deletes only
///   reorder or remove;
/// * duplicate stroke IDs: `AddStroke` refuses any live or deleted ID; erase fragments are
///   re-checked here against the whole library; removals cannot duplicate;
/// * titles, `≥ 1 page`, page background/render, stroke content, the pressure-style rule and
///   the history rules (live ≠ deleted, roots known) are re-checked on the touched notebook
///   (its pages' strokes only on the touched page — no other page's strokes change).
fn validate_touched(st: &LibraryState, op: &SyncOperationV1) -> Result<(), SemanticError> {
    // AddStroke (the bulk of every replay) only appends one stroke to one page: from a valid
    // state, only that stroke can break a rule. Its ID is already proven unused (live or
    // deleted) and its root recorded by the application; its content passed
    // `SyncOperationV1::validate`; what remains is the page-dependent pressure rule.
    if let SyncOperationV1::AddStroke {
        notebook_id,
        page_id,
        stroke,
    } = op
    {
        let appended = st
            .snapshot
            .notebooks
            .iter()
            .find(|n| n.notebook_id == *notebook_id)
            .and_then(|n| n.pages.iter().find(|p| p.page_id == *page_id));
        let ok = match appended {
            // Target deleted: a NoOp, nothing changed.
            None => true,
            Some(p) => p.strokes.last().is_some_and(|s| {
                s.stroke_id == stroke.stroke_id
                    && s.validate().is_ok()
                    && !(s.style == StrokeStyle::PressureWidthV1 && p.render.pressure.is_none())
                    && !st.history.deleted_strokes.contains(&s.stroke_id)
                    && st.history.known_roots.contains(&s.root_id)
            }),
        };
        if ok {
            return Ok(());
        }
        st.validate()?;
        return err(
            C::InvalidState,
            "internal: scoped validation disagrees with full validation",
        );
    }
    let folders_valid = !is_folder_op(op) || folders_ok(st);
    let Some((nb, page)) = target_of(op) else {
        if folders_valid {
            return Ok(());
        }
        st.validate()?;
        return err(
            C::InvalidState,
            "internal: scoped validation disagrees with full validation",
        );
    };
    let erase = matches!(
        op,
        SyncOperationV1::EraseRegion { .. } | SyncOperationV1::ReplaceStrokeLineages { .. }
    );
    let stroke_scope = match op {
        SyncOperationV1::AddStroke { .. }
        | SyncOperationV1::EraseRegion { .. }
        | SyncOperationV1::DeleteStrokeLineage { .. }
        | SyncOperationV1::MoveStrokeLineages { .. }
        | SyncOperationV1::ReplaceStrokeLineages { .. }
        | SyncOperationV1::AddText { .. }
        | SyncOperationV1::UpdateText { .. }
        | SyncOperationV1::DeleteText { .. }
        | SyncOperationV1::AddPageLink { .. }
        | SyncOperationV1::DeletePageLink { .. } => page,
        _ => None,
    };
    if folders_valid && touched_ok(st, nb, stroke_scope, erase) {
        return Ok(());
    }
    // Invalid: the full validator names the violation exactly as the clone path would.
    st.validate()?;
    err(
        C::InvalidState,
        "internal: scoped validation disagrees with full validation",
    )
}

/// The folder rules of `SyncSnapshotV1::validate` + `LibraryState::validate` (cheap: folders and
/// memberships only).
fn folders_ok(st: &LibraryState) -> bool {
    let mut live = HashSet::new();
    for f in &st.snapshot.folders {
        if !live.insert(f.folder_id)
            || validate_title(&f.title).is_err()
            || st.history.deleted_folders.contains(&f.folder_id)
        {
            return false;
        }
    }
    st.snapshot
        .notebooks
        .iter()
        .all(|n| n.folder_id.is_none_or(|f| live.contains(&f)))
}

fn touched_ok(
    st: &LibraryState,
    nb: NotebookId,
    stroke_scope: Option<PageId>,
    erase: bool,
) -> bool {
    let h = &st.history;
    let Some(n) = st.snapshot.notebooks.iter().find(|n| n.notebook_id == nb) else {
        return true; // removed (or never live): nothing left to validate
    };
    if h.deleted_notebooks.contains(&n.notebook_id)
        || validate_title(&n.title).is_err()
        || n.pages.is_empty()
    {
        return false;
    }
    let mut page_ids = HashSet::new();
    for p in &n.pages {
        if !page_ids.insert(p.page_id)
            || h.deleted_pages.contains(&p.page_id)
            || p.background.validate().is_err()
            || p.render.validate().is_err()
        {
            return false;
        }
        if Some(p.page_id) != stroke_scope {
            continue;
        }
        let mut ids = HashSet::new();
        for s in &p.strokes {
            if !ids.insert(s.stroke_id)
                || s.validate().is_err()
                || (s.style == StrokeStyle::PressureWidthV1 && p.render.pressure.is_none())
                || h.deleted_strokes.contains(&s.stroke_id)
                || !h.known_roots.contains(&s.root_id)
            {
                return false;
            }
        }
        let mut text_ids = HashSet::new();
        for t in &p.texts {
            if !text_ids.insert(t.text_id)
                || t.validate().is_err()
                || h.deleted_texts.contains(&t.text_id)
                || !h.known_texts.contains(&t.text_id)
            {
                return false;
            }
        }
        let mut link_ids = HashSet::new();
        for l in &p.links {
            if !link_ids.insert(l.link_id)
                || l.validate().is_err()
                || h.deleted_links.contains(&l.link_id)
                || !h.known_links.contains(&l.link_id)
            {
                return false;
            }
        }
        if erase {
            let clash = st
                .snapshot
                .notebooks
                .iter()
                .flat_map(|o| &o.pages)
                .filter(|o| o.page_id != p.page_id)
                .flat_map(|o| &o.strokes)
                .any(|s| ids.contains(&s.stroke_id));
            if clash {
                return false;
            }
        }
    }
    true
}

impl crate::folders::FolderHost for LibraryState {
    fn folders(&mut self) -> &mut Vec<SnapshotFolder> {
        &mut self.snapshot.folders
    }

    fn deleted_folders(&mut self) -> &mut BTreeSet<FolderId> {
        &mut self.history.deleted_folders
    }

    fn notebook(
        &mut self,
        id: NotebookId,
    ) -> Result<crate::folders::NotebookSlot<'_>, SemanticError> {
        Ok(match find_notebook(self, id)? {
            Target::Deleted => crate::folders::NotebookSlot::Deleted,
            Target::Live(i) => {
                crate::folders::NotebookSlot::Live(&mut self.snapshot.notebooks[i].folder_id)
            }
        })
    }

    fn release_members(&mut self, folder: FolderId) {
        for nb in &mut self.snapshot.notebooks {
            if nb.folder_id == Some(folder) {
                nb.folder_id = None;
            }
        }
    }
}
