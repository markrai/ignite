//! Home folders: a grouping of notebooks, stored outside every notebook/page/library schema.
//!
//! Stored in the app-owned namespace `<library>/.scribe-folders/folders.json`, beside (not
//! inside) `library.meta`: notebook order and identity are unaffected. A notebook is in at most
//! one folder; notebooks in no folder are shown in the main list. Membership of a notebook that
//! no longer exists (deleted here or remotely) is simply ignored when shown, and pruned on the
//! next write.
//!
//! Sync: folders are part of the semantic snapshot (`SnapshotFolder` + `SnapshotNotebook::
//! folder_id`). A folder's global [`FolderId`] is its `sync_id` (minted at creation here, or
//! the remote creator's ID when materialized), or — for folders made before sync knew about
//! folders — UUIDv5(library, `scribe-kindle-folder/<id>`). Local changes are captured after
//! they commit; a lost capture is re-derived by startup reconciliation (folder state is fully
//! derivable). Remote changes arrive only through [`materialized`], written by the sync
//! materializer.
//!
//! Deleting a folder never deletes notebooks: its notebooks return to the main list.
//! Writes are atomic (temp file, fsync, rename, directory fsync). A file that cannot be read is
//! never overwritten: the store opens read-only and every change is refused with an error.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sync_protocol::{FolderId, LibraryId, SnapshotFolder};
use uuid::Uuid;

use crate::library::validate_title;

pub const FOLDERS_DIR: &str = ".scribe-folders";
pub const FOLDERS_FILE: &str = "folders.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FolderV1 {
    /// `folder-<n>` (monotonic, never reused).
    pub id: String,
    pub title: String,
    pub notebook_ids: Vec<String>,
    /// Global sync identity (absent only for folders created before folders synced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_id: Option<FolderId>,
}

/// The folder's global identity in `library` (see the module docs).
pub fn global_folder_id(library: LibraryId, f: &FolderV1) -> FolderId {
    f.sync_id.unwrap_or_else(|| {
        let name = format!("scribe-kindle-folder/{}", f.id);
        FolderId::from_uuid(Uuid::new_v5(&library.as_uuid(), name.as_bytes()))
            .expect("UUIDv5 is a valid folder identity")
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FoldersFileV1 {
    pub v: u32,
    pub next_folder_ordinal: u64,
    /// Display order = creation order.
    pub folders: Vec<FolderV1>,
}

impl Default for FoldersFileV1 {
    fn default() -> Self {
        Self {
            v: 1,
            next_folder_ordinal: 1,
            folders: Vec::new(),
        }
    }
}

/// One folder as Home shows it: members are the live notebooks, in library order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderInfo {
    pub id: String,
    pub title: String,
    pub notebook_ids: Vec<String>,
}

pub fn default_folder_title(ordinal: u64) -> String {
    format!("Folder {ordinal}")
}

pub struct FolderStore {
    dir: PathBuf,
    data: FoldersFileV1,
    /// Set when the file exists but could not be read: nothing is ever written then.
    unreadable: Option<String>,
}

fn validate(data: &FoldersFileV1) -> Result<(), String> {
    if data.v != 1 {
        return Err(format!("unsupported folders version {}", data.v));
    }
    let mut ids = HashSet::new();
    let mut sync_ids = HashSet::new();
    let mut members = HashSet::new();
    for f in &data.folders {
        if let Some(g) = f.sync_id {
            if !sync_ids.insert(g) {
                return Err(format!("folder sync id {g} used twice"));
            }
        }
        let ordinal =
            f.id.strip_prefix("folder-")
                .and_then(|n| n.parse::<u64>().ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("invalid folder id {:?}", f.id))?;
        if ordinal >= data.next_folder_ordinal || !ids.insert(f.id.as_str()) {
            return Err(format!("folder id {} reused or out of order", f.id));
        }
        validate_title(&f.title)?;
        for n in &f.notebook_ids {
            if !members.insert(n.as_str()) {
                return Err(format!("notebook {n} is in two folders"));
            }
        }
    }
    Ok(())
}

/// Strict read (missing file = no folders). Sync uses this: an unreadable file fails closed.
pub fn read_folders(library_root: &Path) -> Result<FoldersFileV1, String> {
    let path = library_root.join(FOLDERS_DIR).join(FOLDERS_FILE);
    match fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FoldersFileV1::default()),
        Err(e) => Err(format!("read folders: {e}")),
        Ok(bytes) => serde_json::from_slice::<FoldersFileV1>(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|d| validate(&d).map(|()| d))
            .map_err(|e| format!("folders unreadable: {e}")),
    }
}

/// Validate and write atomically (temp file, fsync, rename, directory fsync).
fn write_folders(dir: &Path, next: &FoldersFileV1) -> Result<(), String> {
    validate(next)?;
    let _admit = crate::sync_verified::note_mutation_from_folders_dir(dir)?;
    fs::create_dir_all(dir).map_err(|e| format!("create folders dir: {e}"))?;
    let tmp = dir.join(format!("{FOLDERS_FILE}.tmp"));
    let mut bytes = serde_json::to_vec_pretty(next).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    {
        let mut f = File::create(&tmp).map_err(|e| format!("write folders: {e}"))?;
        f.write_all(&bytes)
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("write folders: {e}"))?;
    }
    fs::rename(&tmp, dir.join(FOLDERS_FILE)).map_err(|e| format!("write folders: {e}"))?;
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Folders + memberships as the sync snapshot states them: `(folders, notebook local id →
/// folder)`, for the live notebooks in `live` only.
pub fn snapshot_folders(
    data: &FoldersFileV1,
    library: LibraryId,
    live: &[String],
) -> (
    Vec<SnapshotFolder>,
    std::collections::HashMap<String, FolderId>,
) {
    let live: HashSet<&str> = live.iter().map(String::as_str).collect();
    let mut members = std::collections::HashMap::new();
    let folders = data
        .folders
        .iter()
        .map(|f| {
            let g = global_folder_id(library, f);
            for n in f.notebook_ids.iter().filter(|n| live.contains(n.as_str())) {
                members.insert(n.clone(), g);
            }
            SnapshotFolder {
                folder_id: g,
                title: f.title.clone(),
            }
        })
        .collect();
    (folders, members)
}

/// The folder file that states `expected` (sync materialization). Existing folders keep their
/// local ids (matched by global identity); new ones get the next local id and remember the
/// remote `sync_id`. `members`: `(notebook local id, folder)` for every filed live notebook.
pub fn materialized(
    current: &FoldersFileV1,
    library: LibraryId,
    expected: &[SnapshotFolder],
    members: &[(String, FolderId)],
) -> FoldersFileV1 {
    let mut next = FoldersFileV1 {
        v: 1,
        next_folder_ordinal: current.next_folder_ordinal,
        folders: Vec::with_capacity(expected.len()),
    };
    for e in expected {
        let (id, sync_id) = match current
            .folders
            .iter()
            .find(|f| global_folder_id(library, f) == e.folder_id)
        {
            Some(f) => (f.id.clone(), f.sync_id),
            None => {
                let id = format!("folder-{}", next.next_folder_ordinal);
                next.next_folder_ordinal += 1;
                (id, Some(e.folder_id))
            }
        };
        next.folders.push(FolderV1 {
            id,
            title: e.title.clone(),
            notebook_ids: members
                .iter()
                .filter(|(_, f)| *f == e.folder_id)
                .map(|(n, _)| n.clone())
                .collect(),
            sync_id,
        });
    }
    next
}

/// Write the materialized folder file (sync only; see [`materialized`]).
pub fn write_materialized(library_root: &Path, next: &FoldersFileV1) -> Result<(), String> {
    write_folders(&library_root.join(FOLDERS_DIR), next)
}

impl FolderStore {
    /// Open the library's folders (none yet is fine).
    pub fn open(library_root: &Path) -> Self {
        let dir = library_root.join(FOLDERS_DIR);
        let (data, unreadable) = match read_folders(library_root) {
            Ok(d) => (d, None),
            Err(e) => (FoldersFileV1::default(), Some(e)),
        };
        Self {
            dir,
            data,
            unreadable,
        }
    }

    /// Re-read the file (after sync materialized remote folder changes).
    pub fn reload(&mut self) {
        let root = self.dir.parent().map(Path::to_path_buf).unwrap_or_default();
        *self = Self::open(&root);
    }

    pub fn title(&self, id: &str) -> Option<&str> {
        self.data
            .folders
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.title.as_str())
    }

    /// Global identity of local folder `id` in `library`.
    pub fn sync_id(&self, id: &str, library: LibraryId) -> Option<FolderId> {
        self.data
            .folders
            .iter()
            .find(|f| f.id == id)
            .map(|f| global_folder_id(library, f))
    }

    /// Why folders are read-only (the file exists but could not be read), if they are.
    pub fn unreadable(&self) -> Option<&str> {
        self.unreadable.as_deref()
    }

    /// Folders as Home shows them: only live notebooks, in `library_order`.
    pub fn view(&self, library_order: &[String]) -> Vec<FolderInfo> {
        self.data
            .folders
            .iter()
            .map(|f| {
                let members: HashSet<&str> = f.notebook_ids.iter().map(String::as_str).collect();
                FolderInfo {
                    id: f.id.clone(),
                    title: f.title.clone(),
                    notebook_ids: library_order
                        .iter()
                        .filter(|n| members.contains(n.as_str()))
                        .cloned()
                        .collect(),
                }
            })
            .collect()
    }

    pub fn next_default_title(&self) -> String {
        default_folder_title(self.data.next_folder_ordinal)
    }

    pub fn folder_of(&self, notebook_id: &str) -> Option<&str> {
        self.data
            .folders
            .iter()
            .find(|f| f.notebook_ids.iter().any(|n| n == notebook_id))
            .map(|f| f.id.as_str())
    }

    fn folder_mut(data: &mut FoldersFileV1, id: &str) -> Result<usize, String> {
        data.folders
            .iter()
            .position(|f| f.id == id)
            .ok_or_else(|| format!("no folder {id}"))
    }

    /// Validate `next`, persist it atomically, and only then adopt it (memory == disk).
    fn commit(&mut self, mut next: FoldersFileV1, live: &[String]) -> Result<(), String> {
        if let Some(why) = &self.unreadable {
            return Err(format!("folders are read-only: {why}"));
        }
        let live: HashSet<&str> = live.iter().map(String::as_str).collect();
        for f in &mut next.folders {
            f.notebook_ids.retain(|n| live.contains(n.as_str()));
        }
        write_folders(&self.dir, &next)?;
        self.data = next;
        Ok(())
    }

    /// Create an empty folder; returns its id.
    pub fn create(&mut self, title: &str, live: &[String]) -> Result<String, String> {
        validate_title(title)?;
        let mut next = self.data.clone();
        let id = format!("folder-{}", next.next_folder_ordinal);
        next.next_folder_ordinal += 1;
        next.folders.push(FolderV1 {
            id: id.clone(),
            title: title.to_string(),
            notebook_ids: Vec::new(),
            sync_id: Some(
                FolderId::from_uuid(Uuid::now_v7()).map_err(|e| format!("folder id: {e}"))?,
            ),
        });
        self.commit(next, live)?;
        Ok(id)
    }

    pub fn rename(&mut self, id: &str, title: &str, live: &[String]) -> Result<(), String> {
        validate_title(title)?;
        let mut next = self.data.clone();
        let i = Self::folder_mut(&mut next, id)?;
        next.folders[i].title = title.to_string();
        self.commit(next, live)
    }

    /// Delete a folder; its notebooks return to the main list (never deleted).
    pub fn delete(&mut self, id: &str, live: &[String]) -> Result<(), String> {
        let mut next = self.data.clone();
        let i = Self::folder_mut(&mut next, id)?;
        next.folders.remove(i);
        self.commit(next, live)
    }

    /// Put `notebook_id` into `folder` (`None` = back to the main list), leaving any other.
    pub fn set_folder(
        &mut self,
        notebook_id: &str,
        folder: Option<&str>,
        live: &[String],
    ) -> Result<(), String> {
        if !live.iter().any(|n| n == notebook_id) {
            return Err(format!("notebook {notebook_id} not in library"));
        }
        let mut next = self.data.clone();
        for f in &mut next.folders {
            f.notebook_ids.retain(|n| n != notebook_id);
        }
        if let Some(id) = folder {
            let i = Self::folder_mut(&mut next, id)?;
            next.folders[i].notebook_ids.push(notebook_id.to_string());
        }
        self.commit(next, live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "scribe-folders-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn live(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("notebook-{i}")).collect()
    }

    #[test]
    fn create_rename_move_delete_round_trip_and_persist() {
        let root = tmp("round-trip");
        let lib = live(4);
        let mut s = FolderStore::open(&root);
        assert_eq!(s.next_default_title(), "Folder 1");
        let a = s.create("Work", &lib).unwrap();
        let b = s.create("Home", &lib).unwrap();
        assert_eq!((a.as_str(), b.as_str()), ("folder-1", "folder-2"));
        s.set_folder("notebook-3", Some(&a), &lib).unwrap();
        s.set_folder("notebook-1", Some(&a), &lib).unwrap();
        // Change folder: leaves the old one.
        s.set_folder("notebook-3", Some(&b), &lib).unwrap();
        s.rename(&a, "Office", &lib).unwrap();
        let v = FolderStore::open(&root).view(&lib);
        assert_eq!(v[0].title, "Office");
        assert_eq!(v[0].notebook_ids, ["notebook-1"]);
        assert_eq!(v[1].notebook_ids, ["notebook-3"]);
        // Remove from folder, then delete a folder: its notebook returns to the main list.
        s.set_folder("notebook-1", None, &lib).unwrap();
        s.set_folder("notebook-2", Some(&b), &lib).unwrap();
        s.delete(&b, &lib).unwrap();
        let s2 = FolderStore::open(&root);
        assert_eq!(s2.view(&lib).len(), 1);
        assert_eq!(s2.folder_of("notebook-2"), None);
        // Ids are never reused.
        let mut s2 = s2;
        assert_eq!(s2.create("New", &lib).unwrap(), "folder-3");
    }

    #[test]
    fn deleted_notebooks_are_hidden_then_pruned_and_order_follows_the_library() {
        let root = tmp("prune");
        let mut s = FolderStore::open(&root);
        let lib = live(3);
        let a = s.create("A", &lib).unwrap();
        for n in ["notebook-3", "notebook-1", "notebook-2"] {
            s.set_folder(n, Some(&a), &lib).unwrap();
        }
        assert_eq!(
            s.view(&lib)[0].notebook_ids,
            ["notebook-1", "notebook-2", "notebook-3"]
        );
        // notebook-2 deleted (here or remotely): hidden at once, pruned on the next write.
        let lib2 = vec!["notebook-1".to_string(), "notebook-3".to_string()];
        assert_eq!(s.view(&lib2)[0].notebook_ids, ["notebook-1", "notebook-3"]);
        s.rename(&a, "A2", &lib2).unwrap();
        let raw = fs::read_to_string(root.join(FOLDERS_DIR).join(FOLDERS_FILE)).unwrap();
        assert!(!raw.contains("notebook-2"));
    }

    #[test]
    fn refusals_leave_memory_and_disk_unchanged() {
        let root = tmp("refuse");
        let mut s = FolderStore::open(&root);
        let lib = live(2);
        assert!(s.create("", &lib).is_err());
        assert!(s.create(&"x".repeat(200), &lib).is_err());
        let a = s.create("A", &lib).unwrap();
        assert!(s.set_folder("notebook-9", Some(&a), &lib).is_err());
        assert!(s.set_folder("notebook-1", Some("folder-99"), &lib).is_err());
        assert!(s.rename("folder-99", "B", &lib).is_err());
        assert!(s.delete("folder-99", &lib).is_err());
        assert_eq!(FolderStore::open(&root).view(&lib), s.view(&lib));
    }

    #[test]
    fn an_unreadable_file_is_never_overwritten() {
        let root = tmp("corrupt");
        let dir = root.join(FOLDERS_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FOLDERS_FILE), b"{not json").unwrap();
        let mut s = FolderStore::open(&root);
        assert!(s.unreadable().is_some());
        assert!(s.view(&live(2)).is_empty());
        assert!(s.create("A", &live(2)).is_err());
        assert_eq!(fs::read(dir.join(FOLDERS_FILE)).unwrap(), b"{not json");
    }
}
