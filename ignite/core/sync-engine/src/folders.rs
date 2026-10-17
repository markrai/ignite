//! Folder rules, shared by the reducer and by replicas that project pending folder operations
//! over a displayed snapshot (the browser). One implementation: a projection can never disagree
//! with the reducer about what a folder operation means.

use std::collections::BTreeSet;

use sync_protocol::{FolderId, NotebookId, SnapshotFolder, SyncOperationV1};

use crate::apply::{Outcome, SemanticError, SemanticErrorCode as C};

/// Where a notebook stands for a folder operation.
pub enum NotebookSlot<'a> {
    /// Live: its folder membership, writable.
    Live(&'a mut Option<FolderId>),
    /// Deleted earlier in canonical order (the operation becomes a NoOp).
    Deleted,
}

/// The state a folder operation reads and writes.
pub trait FolderHost {
    fn folders(&mut self) -> &mut Vec<SnapshotFolder>;
    fn deleted_folders(&mut self) -> &mut BTreeSet<FolderId>;
    /// `Err` when the notebook never existed.
    fn notebook(&mut self, id: NotebookId) -> Result<NotebookSlot<'_>, SemanticError>;
    /// Move every member of `folder` back to the top level.
    fn release_members(&mut self, folder: FolderId);
}

enum Found {
    Live(usize),
    Deleted,
}

fn find(h: &mut dyn FolderHost, id: FolderId) -> Result<Found, SemanticError> {
    if let Some(i) = h.folders().iter().position(|f| f.folder_id == id) {
        return Ok(Found::Live(i));
    }
    if h.deleted_folders().contains(&id) {
        return Ok(Found::Deleted);
    }
    Err(SemanticError::new(
        C::UnknownFolder,
        format!("folder {id} never existed"),
    ))
}

/// Apply one folder operation (`CreateFolder`, `RenameFolder`, `DeleteFolder`,
/// `SetNotebookFolder`). `Ok(None)` = not a folder operation.
pub fn apply_folder_operation(
    h: &mut dyn FolderHost,
    op: &SyncOperationV1,
) -> Result<Option<Outcome>, SemanticError> {
    Ok(Some(match op {
        SyncOperationV1::CreateFolder { folder_id, title } => {
            let id = *folder_id;
            if h.folders().iter().any(|f| f.folder_id == id) || h.deleted_folders().contains(&id) {
                return Err(SemanticError::new(
                    C::DuplicateFolder,
                    format!("folder {id} already used"),
                ));
            }
            h.folders().push(SnapshotFolder {
                folder_id: id,
                title: title.clone(),
            });
            Outcome::Applied
        }
        SyncOperationV1::RenameFolder { folder_id, title } => match find(h, *folder_id)? {
            Found::Deleted => Outcome::NoOp("folder deleted"),
            Found::Live(i) => {
                h.folders()[i].title = title.clone();
                Outcome::Applied
            }
        },
        SyncOperationV1::DeleteFolder { folder_id } => match find(h, *folder_id)? {
            Found::Deleted => Outcome::NoOp("folder already deleted"),
            Found::Live(i) => {
                let f = h.folders().remove(i);
                h.release_members(f.folder_id);
                h.deleted_folders().insert(f.folder_id);
                Outcome::Applied
            }
        },
        SyncOperationV1::SetNotebookFolder {
            notebook_id,
            folder_id,
        } => {
            if let NotebookSlot::Deleted = h.notebook(*notebook_id)? {
                return Ok(Some(Outcome::NoOp("notebook deleted")));
            }
            if let Some(f) = folder_id {
                if let Found::Deleted = find(h, *f)? {
                    return Ok(Some(Outcome::NoOp("folder deleted")));
                }
            }
            match h.notebook(*notebook_id)? {
                NotebookSlot::Live(slot) => *slot = *folder_id,
                NotebookSlot::Deleted => unreachable!("resolved live above"),
            }
            Outcome::Applied
        }
        _ => return Ok(None),
    }))
}

/// Folders + memberships of a displayed snapshot, for projecting pending folder operations.
/// Notebooks absent from `notebooks` count as deleted (the displayed snapshot no longer has
/// them), so an operation on one projects as a NoOp — never an error.
#[derive(Clone, Debug, PartialEq)]
pub struct FolderProjection {
    pub folders: Vec<SnapshotFolder>,
    /// `(notebook, folder)` in library order.
    pub notebooks: Vec<(NotebookId, Option<FolderId>)>,
    pub deleted_folders: BTreeSet<FolderId>,
}

impl FolderHost for FolderProjection {
    fn folders(&mut self) -> &mut Vec<SnapshotFolder> {
        &mut self.folders
    }

    fn deleted_folders(&mut self) -> &mut BTreeSet<FolderId> {
        &mut self.deleted_folders
    }

    fn notebook(&mut self, id: NotebookId) -> Result<NotebookSlot<'_>, SemanticError> {
        Ok(match self.notebooks.iter_mut().find(|(n, _)| *n == id) {
            Some((_, f)) => NotebookSlot::Live(f),
            None => NotebookSlot::Deleted,
        })
    }

    fn release_members(&mut self, folder: FolderId) {
        for (_, f) in &mut self.notebooks {
            if *f == Some(folder) {
                *f = None;
            }
        }
    }
}

impl FolderProjection {
    /// Apply `ops` in order; `changed[i]` = op `i` was applied. An op the reducer would refuse
    /// is skipped here (the server's answer is authoritative; the projection never fails).
    pub fn project(&mut self, ops: &[SyncOperationV1]) -> Vec<bool> {
        ops.iter()
            .map(|op| {
                let before = self.clone();
                match apply_folder_operation(self, op) {
                    Ok(Some(Outcome::Applied)) => true,
                    Ok(_) => false,
                    Err(_) => {
                        *self = before;
                        false
                    }
                }
            })
            .collect()
    }
}
