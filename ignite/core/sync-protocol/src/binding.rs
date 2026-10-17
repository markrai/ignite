//! Local ↔ global identity bindings and the replica-private identity map.
//!
//! A replica materializes shared entities under its own **local IDs** (the Kindle uses
//! directory names such as `notebook-12` / `page-7`). Local IDs are storage names: they never
//! travel as identity and carry no ordering meaning. Each is bound to a **global ID**
//! ([`NotebookId`], [`PageId`]) that is identical on every replica. A remote notebook
//! `019a…` might be materialized locally as `notebook-12` on one Kindle and something else
//! elsewhere; they are the same notebook because the global ID matches.
//!
//! [`IdentityFileV1`] is the Kindle's durable record of those bindings (stored in
//! `<library>/.scribe-sync/identity.json`, outside every notebook/page schema). It keeps
//! currently **live** bindings and **retired** bindings of entities deleted locally, so a
//! deleted entity's global ID is remembered (future tombstones/history) and never reused.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::canonical::{from_json, to_canonical_json};
use crate::error::{refuse, ProtocolError, Result};
use crate::ids::{LibraryId, NotebookId, PageId};

pub const IDENTITY_FILE_VERSION: u32 = 1;
const MAX_LOCAL_ID_LEN: usize = 64;

/// Validate an opaque local storage name (e.g. `notebook-3`, `page-7`).
pub fn validate_local_id(kind: &str, s: &str) -> Result<()> {
    if s.is_empty()
        || s.len() > MAX_LOCAL_ID_LEN
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        refuse!("invalid local {kind} id {s:?}");
    }
    Ok(())
}

/// A live notebook's local storage name bound to its global identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotebookBinding {
    pub local_id: String,
    pub sync_id: NotebookId,
}

/// A live page's local storage name (scoped to its notebook's local name) bound to its global
/// identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageBinding {
    pub notebook_local_id: String,
    pub local_id: String,
    pub sync_id: PageId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredNotebook {
    /// Local name the notebook had when it left the live library.
    pub local_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredPage {
    pub notebook_id: NotebookId,
    pub local_id: String,
}

/// `identity.json` v1. All maps are ordered, so serialization is deterministic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityFileV1 {
    pub v: u32,
    pub library_id: LibraryId,
    /// Live notebooks: local notebook ID → global notebook ID.
    pub notebooks: BTreeMap<String, NotebookId>,
    /// Live pages: local notebook ID → (local page ID → global page ID).
    pub pages: BTreeMap<String, BTreeMap<String, PageId>>,
    /// Notebooks no longer in the local library (global ID → last local name).
    pub retired_notebooks: BTreeMap<NotebookId, RetiredNotebook>,
    /// Pages no longer in their notebook (global ID → owner + last local name).
    pub retired_pages: BTreeMap<PageId, RetiredPage>,
}

impl IdentityFileV1 {
    pub fn validate(&self) -> Result<()> {
        if self.v != IDENTITY_FILE_VERSION {
            refuse!(
                "unsupported identity.json version {} (need {IDENTITY_FILE_VERSION})",
                self.v
            );
        }
        let mut seen: HashSet<Uuid> = HashSet::new();
        let mut unique = |u: Uuid, what: &str| -> Result<()> {
            if !seen.insert(u) {
                refuse!("identity.json reuses global id {u} ({what})");
            }
            Ok(())
        };
        unique(self.library_id.as_uuid(), "library")?;
        for (local, id) in &self.notebooks {
            validate_local_id("notebook", local)?;
            unique(id.as_uuid(), "notebook")?;
        }
        if self.pages.keys().collect::<BTreeSet<_>>() != self.notebooks.keys().collect() {
            refuse!("identity.json pages must list exactly the live notebooks");
        }
        for (nb, pages) in &self.pages {
            for (local, id) in pages {
                validate_local_id("page", local)?;
                unique(id.as_uuid(), &format!("page in {nb}"))?;
            }
        }
        for (id, r) in &self.retired_notebooks {
            validate_local_id("notebook", &r.local_id)?;
            unique(id.as_uuid(), "retired notebook")?;
        }
        for (id, r) in &self.retired_pages {
            validate_local_id("page", &r.local_id)?;
            unique(id.as_uuid(), "retired page")?;
        }
        Ok(())
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        to_canonical_json(self)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let f: Self = from_json(bytes)?;
        f.validate()?;
        Ok(f)
    }

    pub fn notebook_bindings(&self) -> Vec<NotebookBinding> {
        self.notebooks
            .iter()
            .map(|(local_id, sync_id)| NotebookBinding {
                local_id: local_id.clone(),
                sync_id: *sync_id,
            })
            .collect()
    }

    pub fn page_bindings(&self) -> Vec<PageBinding> {
        self.pages
            .iter()
            .flat_map(|(nb, pages)| {
                pages.iter().map(move |(local_id, sync_id)| PageBinding {
                    notebook_local_id: nb.clone(),
                    local_id: local_id.clone(),
                    sync_id: *sync_id,
                })
            })
            .collect()
    }

    pub fn notebook_id(&self, local: &str) -> Option<NotebookId> {
        self.notebooks.get(local).copied()
    }

    pub fn page_id(&self, notebook_local: &str, page_local: &str) -> Option<PageId> {
        self.pages.get(notebook_local)?.get(page_local).copied()
    }
}

/// The authoritative local layout: library notebook order, and each notebook's page order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveLayout {
    pub notebooks: Vec<(String, Vec<String>)>,
}

/// What a reconcile changed. `changed == false` means the file must not be rewritten.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reconciled {
    pub identity: IdentityFileV1,
    pub changed: bool,
    pub allocated_library: bool,
    pub allocated_notebooks: Vec<NotebookBinding>,
    pub allocated_pages: Vec<PageBinding>,
    pub retired_notebooks: Vec<NotebookBinding>,
    pub retired_pages: Vec<PageBinding>,
}

/// Bring an identity map in line with the live local layout.
///
/// * New live notebooks/pages get exactly one freshly minted global ID each (in library/page
///   order, so minting order is deterministic).
/// * Bindings of entities no longer live move to the retired maps; they are never deleted and
///   their IDs are never handed to another local entity (a reused local name gets a new ID).
/// * Existing live bindings never change.
///
/// `mint` supplies new UUIDs (v4/v7); any collision with a known ID is refused.
pub fn reconcile_identity(
    existing: Option<&IdentityFileV1>,
    live: &LiveLayout,
    mint: &mut dyn FnMut() -> Uuid,
) -> Result<Reconciled> {
    let mut used: HashSet<Uuid> = HashSet::new();
    let mut allocated_library = false;
    let mut id = match existing {
        Some(f) => {
            f.validate()?;
            f.clone()
        }
        None => {
            allocated_library = true;
            IdentityFileV1 {
                v: IDENTITY_FILE_VERSION,
                library_id: LibraryId::from_uuid(mint())?,
                notebooks: BTreeMap::new(),
                pages: BTreeMap::new(),
                retired_notebooks: BTreeMap::new(),
                retired_pages: BTreeMap::new(),
            }
        }
    };
    used.insert(id.library_id.as_uuid());
    used.extend(id.notebooks.values().map(|n| n.as_uuid()));
    used.extend(
        id.pages
            .values()
            .flat_map(|p| p.values().map(|x| x.as_uuid())),
    );
    used.extend(id.retired_notebooks.keys().map(|n| n.as_uuid()));
    used.extend(id.retired_pages.keys().map(|p| p.as_uuid()));

    let mut fresh = |used: &mut HashSet<Uuid>| -> Result<Uuid> {
        let u = mint();
        if !matches!(u.get_version_num(), 4 | 7) {
            refuse!("minted id {u} is not UUIDv4/v7");
        }
        if !used.insert(u) {
            refuse!("minted id {u} collides with a known global id");
        }
        Ok(u)
    };

    let mut live_nbs: BTreeSet<&str> = BTreeSet::new();
    for (nb, pages) in &live.notebooks {
        validate_local_id("notebook", nb)?;
        if !live_nbs.insert(nb) {
            refuse!("live layout lists notebook {nb} twice");
        }
        let mut seen_pages = BTreeSet::new();
        for p in pages {
            validate_local_id("page", p)?;
            if !seen_pages.insert(p) {
                refuse!("live layout lists page {nb}/{p} twice");
            }
        }
    }

    let mut out = Reconciled {
        identity: id.clone(),
        changed: allocated_library,
        allocated_library,
        allocated_notebooks: Vec::new(),
        allocated_pages: Vec::new(),
        retired_notebooks: Vec::new(),
        retired_pages: Vec::new(),
    };

    // Retire notebooks that left the library (with all their pages).
    let gone: Vec<String> = id
        .notebooks
        .keys()
        .filter(|k| !live_nbs.contains(k.as_str()))
        .cloned()
        .collect();
    for local in gone {
        let nb_id = id.notebooks.remove(&local).expect("listed above");
        for (page_local, page_id) in id.pages.remove(&local).unwrap_or_default() {
            id.retired_pages.insert(
                page_id,
                RetiredPage {
                    notebook_id: nb_id,
                    local_id: page_local.clone(),
                },
            );
            out.retired_pages.push(PageBinding {
                notebook_local_id: local.clone(),
                local_id: page_local,
                sync_id: page_id,
            });
        }
        id.retired_notebooks.insert(
            nb_id,
            RetiredNotebook {
                local_id: local.clone(),
            },
        );
        out.retired_notebooks.push(NotebookBinding {
            local_id: local,
            sync_id: nb_id,
        });
    }

    for (nb_local, page_locals) in &live.notebooks {
        let nb_id = match id.notebooks.get(nb_local) {
            Some(n) => *n,
            None => {
                let n = NotebookId::from_uuid(fresh(&mut used)?)?;
                id.notebooks.insert(nb_local.clone(), n);
                id.pages.insert(nb_local.clone(), BTreeMap::new());
                out.allocated_notebooks.push(NotebookBinding {
                    local_id: nb_local.clone(),
                    sync_id: n,
                });
                n
            }
        };
        let live_pages: BTreeSet<&str> = page_locals.iter().map(String::as_str).collect();
        let pages = id.pages.entry(nb_local.clone()).or_default();
        let gone: Vec<String> = pages
            .keys()
            .filter(|k| !live_pages.contains(k.as_str()))
            .cloned()
            .collect();
        for page_local in gone {
            let page_id = pages.remove(&page_local).expect("listed above");
            id.retired_pages.insert(
                page_id,
                RetiredPage {
                    notebook_id: nb_id,
                    local_id: page_local.clone(),
                },
            );
            out.retired_pages.push(PageBinding {
                notebook_local_id: nb_local.clone(),
                local_id: page_local,
                sync_id: page_id,
            });
        }
        for page_local in page_locals {
            if !pages.contains_key(page_local) {
                let p = PageId::from_uuid(fresh(&mut used)?)?;
                pages.insert(page_local.clone(), p);
                out.allocated_pages.push(PageBinding {
                    notebook_local_id: nb_local.clone(),
                    local_id: page_local.clone(),
                    sync_id: p,
                });
            }
        }
    }

    id.validate()
        .map_err(|e| ProtocolError::new(format!("internal: reconciled identity invalid: {e}")))?;
    out.changed |= !out.allocated_notebooks.is_empty()
        || !out.allocated_pages.is_empty()
        || !out.retired_notebooks.is_empty()
        || !out.retired_pages.is_empty();
    out.identity = id;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic v7-shaped UUIDs for tests.
    fn minter() -> impl FnMut() -> Uuid {
        let mut n: u64 = 0;
        move || {
            n += 1;
            let mut b = [0u8; 16];
            b[8..].copy_from_slice(&n.to_be_bytes());
            b[0] = 0x01;
            b[6] = 0x70; // version 7
            b[8] = (b[8] & 0x3f) | 0x80; // RFC variant
            Uuid::from_bytes(b)
        }
    }

    fn layout(spec: &[(&str, &[&str])]) -> LiveLayout {
        LiveLayout {
            notebooks: spec
                .iter()
                .map(|(nb, pages)| {
                    (
                        nb.to_string(),
                        pages.iter().map(|p| p.to_string()).collect(),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn bootstrap_then_idempotent() {
        let mut m = minter();
        let live = layout(&[
            ("notebook-1", &["page-1", "page-3"]),
            ("notebook-2", &["page-1"]),
        ]);
        let first = reconcile_identity(None, &live, &mut m).unwrap();
        assert!(first.changed && first.allocated_library);
        assert_eq!(first.allocated_notebooks.len(), 2);
        assert_eq!(first.allocated_pages.len(), 3);
        let again = reconcile_identity(Some(&first.identity), &live, &mut m).unwrap();
        assert!(!again.changed);
        assert_eq!(again.identity, first.identity);
        assert_eq!(
            again.identity.to_canonical_bytes().unwrap(),
            first.identity.to_canonical_bytes().unwrap()
        );
    }

    #[test]
    fn new_entities_get_one_id_and_deleted_ones_retire_without_reuse() {
        let mut m = minter();
        let live = layout(&[
            ("notebook-1", &["page-1", "page-2"]),
            ("notebook-2", &["page-1"]),
        ]);
        let base = reconcile_identity(None, &live, &mut m).unwrap().identity;
        // Delete notebook-2 and page-2; add notebook-3 and page-4.
        let live2 = layout(&[
            ("notebook-1", &["page-1", "page-4"]),
            ("notebook-3", &["page-1"]),
        ]);
        let r = reconcile_identity(Some(&base), &live2, &mut m).unwrap();
        assert_eq!(r.allocated_notebooks.len(), 1);
        assert_eq!(r.allocated_pages.len(), 2); // notebook-1/page-4, notebook-3/page-1
        assert_eq!(r.retired_notebooks.len(), 1);
        assert_eq!(r.retired_pages.len(), 2); // notebook-2/page-1 and notebook-1/page-2
        let id = &r.identity;
        assert_eq!(id.notebook_id("notebook-1"), base.notebook_id("notebook-1"));
        assert_eq!(
            id.page_id("notebook-1", "page-1"),
            base.page_id("notebook-1", "page-1")
        );
        let dead_nb = base.notebook_id("notebook-2").unwrap();
        assert_eq!(id.retired_notebooks[&dead_nb].local_id, "notebook-2");
        let dead_page = base.page_id("notebook-1", "page-2").unwrap();
        assert_eq!(
            id.retired_pages[&dead_page].notebook_id,
            base.notebook_id("notebook-1").unwrap()
        );
        // A local name that comes back later is a new entity with a new ID.
        let live3 = layout(&[
            ("notebook-1", &["page-1", "page-4", "page-2"]),
            ("notebook-3", &["page-1"]),
        ]);
        let r3 = reconcile_identity(Some(id), &live3, &mut m).unwrap();
        assert_ne!(r3.identity.page_id("notebook-1", "page-2"), Some(dead_page));
        assert!(r3.identity.retired_pages.contains_key(&dead_page));
        let again = reconcile_identity(Some(&r3.identity), &live3, &mut m).unwrap();
        assert!(!again.changed);
    }

    #[test]
    fn corrupt_identity_files_are_refused() {
        let mut m = minter();
        let live = layout(&[("notebook-1", &["page-1"])]);
        let good = reconcile_identity(None, &live, &mut m).unwrap().identity;
        let mut dup = good.clone();
        let nb = dup.notebooks["notebook-1"];
        dup.retired_notebooks.insert(
            nb,
            RetiredNotebook {
                local_id: "notebook-9".into(),
            },
        );
        assert!(dup.validate().is_err());
        let mut v2 = good.clone();
        v2.v = 2;
        assert!(v2.validate().is_err());
        let mut bytes = good.to_canonical_bytes().unwrap();
        assert!(IdentityFileV1::parse(&bytes).is_ok());
        bytes.truncate(bytes.len() - 1);
        bytes.extend_from_slice(br#","extra":1}"#);
        assert!(IdentityFileV1::parse(&bytes).is_err());
        // A minter that repeats an ID is refused, not silently accepted.
        let fixed = Uuid::parse_str("019a8ef2-0000-7000-8000-000000000001").unwrap();
        let mut repeat = move || fixed;
        assert!(
            reconcile_identity(None, &layout(&[("notebook-1", &["page-1"])]), &mut repeat).is_err()
        );
    }
}
