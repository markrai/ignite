//! `SyncSnapshotV1`: the semantic current state of one library.
//!
//! A snapshot is reconstructed meaning, not a copy of local files: notebooks in library order,
//! pages in notebook order, visible strokes in render order with their lineage roots, plus the library's folders. It
//! deliberately carries **no replica-local state** — no active notebook/page, Home viewport or
//! selection, chrome/modal/tool/touch/sleep state, local directory names, filesystem paths,
//! host names, timestamps, or `ReplicaId`. Opening page 8 on one replica therefore can never
//! move another replica to page 8.
//!
//! Deleted history is not listed as visible strokes; lineage needed to interpret visible
//! fragments (`root_id`) is preserved on every stroke.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::canonical::{from_json, to_canonical_json};
use crate::content::{
    validate_title, BackgroundRef, RenderProfileRef, Sha256Hex, StrokeStyle, SyncPageLink,
    SyncStroke, SyncText,
};
use crate::error::{refuse, ProtocolError, Result};
use crate::geometry::PageSpace;
use crate::ids::{FolderId, LibraryId, NotebookId, PageId};

pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncSnapshotV1 {
    pub v: u32,
    pub library_id: LibraryId,
    pub page_space: PageSpace,
    /// Library display order.
    pub notebooks: Vec<SnapshotNotebook>,
    /// Folders in creation (canonical) order. Omitted when empty, so a library without folders
    /// keeps its pre-folder canonical bytes and hash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<SnapshotFolder>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotFolder {
    pub folder_id: FolderId,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotNotebook {
    pub notebook_id: NotebookId,
    pub title: String,
    /// Notebook page order (never derived from local page names).
    pub pages: Vec<SnapshotPage>,
    /// The folder holding this notebook (absent = top level). Must name a live folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<FolderId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotPage {
    pub page_id: PageId,
    pub background: BackgroundRef,
    pub render: RenderProfileRef,
    /// Visible strokes/fragments in render (bottom-to-top) order.
    pub strokes: Vec<SyncStroke>,
    /// Positioned text objects in creation (canonical) order. Omitted when empty so
    /// libraries without text keep their pre-text canonical bytes and hash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub texts: Vec<SyncText>,
    /// Explicit spatial page-to-page links on this source page. Omitted when empty so
    /// libraries without links keep their pre-link canonical bytes and hash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<SyncPageLink>,
}

impl SyncSnapshotV1 {
    pub fn validate(&self) -> Result<()> {
        if self.v != SNAPSHOT_VERSION {
            refuse!(
                "unsupported snapshot version {} (need {SNAPSHOT_VERSION})",
                self.v
            );
        }
        self.page_space.validate()?;
        let mut folders = HashSet::new();
        for f in &self.folders {
            if !folders.insert(f.folder_id) {
                refuse!("duplicate folder {}", f.folder_id);
            }
            validate_title(&f.title)
                .map_err(|e| ProtocolError::new(format!("folder {}: {e}", f.folder_id)))?;
        }
        let mut notebooks = HashSet::new();
        let mut pages = HashSet::new();
        let mut strokes = HashSet::new();
        let mut texts = HashSet::new();
        let mut links = HashSet::new();
        for nb in &self.notebooks {
            if !notebooks.insert(nb.notebook_id) {
                refuse!("duplicate notebook {}", nb.notebook_id);
            }
            validate_title(&nb.title)
                .map_err(|e| ProtocolError::new(format!("notebook {}: {e}", nb.notebook_id)))?;
            if let Some(f) = nb.folder_id {
                if !folders.contains(&f) {
                    refuse!("notebook {} is in unknown folder {f}", nb.notebook_id);
                }
            }
            if nb.pages.is_empty() {
                refuse!("notebook {} has no pages", nb.notebook_id);
            }
            for page in &nb.pages {
                if !pages.insert(page.page_id) {
                    refuse!("duplicate page {}", page.page_id);
                }
                page.background.validate()?;
                page.render.validate()?;
                for s in &page.strokes {
                    if !strokes.insert(s.stroke_id) {
                        refuse!("duplicate stroke {} (page {})", s.stroke_id, page.page_id);
                    }
                    s.validate()?;
                    if s.style == StrokeStyle::PressureWidthV1 && page.render.pressure.is_none() {
                        refuse!(
                            "pressure stroke {} on page {} whose profile has no pressure curve",
                            s.stroke_id,
                            page.page_id
                        );
                    }
                }
                for t in &page.texts {
                    if !texts.insert(t.text_id) {
                        refuse!("duplicate text {} (page {})", t.text_id, page.page_id);
                    }
                    t.validate()?;
                }
                for l in &page.links {
                    if !links.insert(l.link_id) {
                        refuse!("duplicate link {} (page {})", l.link_id, page.page_id);
                    }
                    l.validate()?;
                    if l.source_notebook_id != nb.notebook_id || l.source_page_id != page.page_id {
                        refuse!(
                            "link {} source {}/{} does not match page {}/{}",
                            l.link_id,
                            l.source_notebook_id,
                            l.source_page_id,
                            nb.notebook_id,
                            page.page_id
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Validated canonical bytes (see [`crate::canonical`]).
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        to_canonical_json(self)
    }

    /// Strict parse + validation. Whitespace is tolerated; compare with
    /// [`Self::to_canonical_bytes`] to test canonical form.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let s: Self = from_json(bytes)?;
        s.validate()?;
        Ok(s)
    }
}

/// SHA-256 over the canonical snapshot bytes. Same semantic snapshot ⇒ same hash. Refuses an
/// invalid snapshot, exactly like [`SyncSnapshotV1::to_canonical_bytes`].
///
/// Hashed while serializing (the same `serde_json` encoder as
/// [`SyncSnapshotV1::to_canonical_bytes`]), so a large library never needs its canonical bytes
/// in memory just to be hashed.
pub fn snapshot_sha256(snapshot: &SyncSnapshotV1) -> Result<Sha256Hex> {
    snapshot.validate()?;
    crate::canonical::canonical_sha256(snapshot)
}
