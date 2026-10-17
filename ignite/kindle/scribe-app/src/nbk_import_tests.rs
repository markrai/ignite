//! NBK one-way import: identity/idempotency contract, crash safety, provenance, source
//! read-only guarantee, and sync exactness. Sources are temp copies of immutable private
//! notebook samples (samples themselves are only read); titles come from a real SQLite
//! catalog shaped like the Kindle's `ksdk.content.db`.
//!
//! Private sample roots and role→fixture mappings live outside Ignite. Set both:
//! `IGNITE_NBK_IMPORT_SAMPLES` and `IGNITE_NBK_IMPORT_FIXTURES` when running ignored tests.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use kindle_platform::FbGeometry;
use serde::Deserialize;

use super::*;
use crate::ink::InkRequest;
use crate::library::{delete_notebook_durable, open_or_create_library, DeleteNotebookHooks};
use crate::store::load_page_read_only;
use crate::sync_outbound::OutboundSync;
use crate::ui_gfx::paper_background;

/// Logical fixture roles for private notebook samples (ids resolved from an external manifest).
fn journal() -> &'static str {
    &fixtures().journal.id
}
fn scratch() -> &'static str {
    &fixtures().scratch.id
}
fn pending() -> &'static str {
    &fixtures().pending.id
}
fn scratch_pages_strokes() -> (usize, usize) {
    fixtures().scratch.counts()
}
fn journal_pages_strokes() -> (usize, usize) {
    fixtures().journal.counts()
}

#[derive(Debug, Clone)]
struct RoleFixture {
    id: String,
    pages: Option<u32>,
    strokes: Option<u32>,
}

impl RoleFixture {
    fn counts(&self) -> (usize, usize) {
        (
            self.pages.expect("role fixture missing pages") as usize,
            self.strokes.expect("role fixture missing strokes") as usize,
        )
    }
}

#[derive(Debug, Clone)]
struct NbkImportFixtures {
    journal: RoleFixture,
    scratch: RoleFixture,
    pending: RoleFixture,
}

static FIXTURES: OnceLock<NbkImportFixtures> = OnceLock::new();

fn fixtures() -> &'static NbkImportFixtures {
    FIXTURES.get_or_init(|| {
        NbkImportFixtures::load_from_env().unwrap_or_else(|e| panic!("NBK import fixtures: {e}"))
    })
}

#[derive(Debug, Deserialize)]
struct ManifestFile {
    version: u32,
    roles: UniqueRoleMap,
}

/// Map that rejects duplicate keys during JSON deserialization (serde_json stream).
#[derive(Debug)]
struct UniqueRoleMap(BTreeMap<String, RoleEntry>);

#[derive(Debug, Deserialize)]
struct RoleEntry {
    id: String,
    #[serde(default)]
    pages: Option<u32>,
    #[serde(default)]
    strokes: Option<u32>,
}

impl<'de> Deserialize<'de> for UniqueRoleMap {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{self, MapAccess, Visitor};
        use std::fmt;
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = UniqueRoleMap;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("roles object with unique keys")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = BTreeMap::new();
                while let Some((k, v)) = map.next_entry::<String, RoleEntry>()? {
                    if out.contains_key(&k) {
                        return Err(de::Error::custom(format!(
                            "duplicate role declaration: {k}"
                        )));
                    }
                    out.insert(k, v);
                }
                Ok(UniqueRoleMap(out))
            }
        }
        deserializer.deserialize_map(V)
    }
}

impl NbkImportFixtures {
    fn load_from_env() -> Result<Self, String> {
        let raw = std::env::var("IGNITE_NBK_IMPORT_FIXTURES").unwrap_or_default();
        Self::load_from_path(&Self::require_absolute_env_path(raw.trim())?)
    }

    fn require_absolute_env_path(raw: &str) -> Result<PathBuf, String> {
        if raw.is_empty() {
            return Err(
                "IGNITE_NBK_IMPORT_FIXTURES must be set to an absolute fixture manifest path \
                 when running private NBK import tests"
                    .into(),
            );
        }
        let path = PathBuf::from(raw);
        if !path.is_absolute() {
            return Err(format!(
                "IGNITE_NBK_IMPORT_FIXTURES must be absolute, got {}",
                path.display()
            ));
        }
        Ok(path)
    }

    fn load_from_path(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("failed to read fixture manifest {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Result<Self, String> {
        let file: ManifestFile = serde_json::from_str(text)
            .map_err(|e| format!("malformed fixture manifest JSON: {e}"))?;
        if file.version != 1 {
            return Err(format!(
                "unsupported fixture manifest version {} (expected 1)",
                file.version
            ));
        }
        let mut roles = file.roles.0;
        let required = ["journal", "scratch", "pending"];
        for r in required {
            if !roles.contains_key(r) {
                return Err(format!("fixture manifest missing required role: {r}"));
            }
        }
        let unknown: Vec<_> = roles
            .keys()
            .filter(|k| !required.contains(&k.as_str()))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "fixture manifest has unknown role(s): {}",
                unknown.join(", ")
            ));
        }
        let mut take = |name: &str| -> Result<RoleFixture, String> {
            let e = roles.remove(name).unwrap();
            let id = e.id.trim().to_string();
            if id.is_empty() {
                return Err(format!("fixture role {name} has empty id"));
            }
            if (e.pages.is_some()) != (e.strokes.is_some()) {
                return Err(format!(
                    "fixture role {name}: pages and strokes must both be set or both omitted"
                ));
            }
            Ok(RoleFixture {
                id,
                pages: e.pages,
                strokes: e.strokes,
            })
        };
        let journal = take("journal")?;
        let scratch = take("scratch")?;
        let pending = take("pending")?;
        // Distinct fixture identities per role (original tests used three distinct notebooks).
        let mut seen = BTreeSet::new();
        for (name, id) in [
            ("journal", journal.id.as_str()),
            ("scratch", scratch.id.as_str()),
            ("pending", pending.id.as_str()),
        ] {
            if !seen.insert(id.to_string()) {
                return Err(format!(
                    "fixture identity reused across roles (role {name})"
                ));
            }
        }
        if journal.pages.is_none() || scratch.pages.is_none() {
            return Err(
                "fixture roles journal and scratch must declare pages and strokes expectations"
                    .into(),
            );
        }
        Ok(Self {
            journal,
            scratch,
            pending,
        })
    }
}

static N: AtomicU64 = AtomicU64::new(0);
static NO_CANCEL: AtomicBool = AtomicBool::new(false);

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!(
        "scribe-nbk-import-{label}-{n}-{}",
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

fn geo() -> FbGeometry {
    FbGeometry {
        xres: 200,
        yres: 160,
        xres_virtual: 208,
        yres_virtual: 160,
        xoffset: 0,
        yoffset: 0,
        bits_per_pixel: 8,
        line_length: 208,
        smem_len: 208 * 160,
    }
}

fn samples_root() -> PathBuf {
    // Private notebook samples must live outside the public Ignite tree.
    // Example: IGNITE_NBK_IMPORT_SAMPLES=C:\path\to\samples
    let raw = std::env::var("IGNITE_NBK_IMPORT_SAMPLES").unwrap_or_default();
    let raw = raw.trim();
    assert!(
        !raw.is_empty(),
        "IGNITE_NBK_IMPORT_SAMPLES must be set to an absolute samples/ directory \
         (feature nbk-import-samples). Refusing to search the Ignite tree."
    );
    let root = PathBuf::from(raw);
    assert!(
        root.is_absolute(),
        "IGNITE_NBK_IMPORT_SAMPLES must be absolute, got {}",
        root.display()
    );
    root
}

fn sample(uuid: &str) -> PathBuf {
    samples_root().join(uuid)
}

/// Fake Kindle: `.notebooks/<id>/` copies + catalog.
struct Kindle {
    root: PathBuf,
    catalog: PathBuf,
}

impl Kindle {
    fn new() -> Self {
        let base = tmp("kindle");
        let k = Kindle {
            root: base.join(".notebooks"),
            catalog: base.join("ksdk.content.db"),
        };
        fs::create_dir_all(&k.root).unwrap();
        let c = rusqlite::Connection::open(&k.catalog).unwrap();
        c.execute_batch(
            "CREATE TABLE Nodes
            (
                uuid TEXT NOT NULL PRIMARY KEY,
                title_collation    TEXT,
                author_collation   TEXT,
                node_type          TEXT,
                download_state     INTEGER DEFAULT 0,
                search_metadata    TEXT,
                last_access_time   INTEGER DEFAULT 0,
                last_modified_time INTEGER DEFAULT 0,
                creation_time      INTEGER DEFAULT 0,
                additional_data    TEXT
            );",
        )
        .unwrap();
        k
    }

    fn sources(&self) -> ImportSources {
        ImportSources {
            notebooks_root: self.root.clone(),
            catalog: Some(self.catalog.clone()),
        }
    }

    /// Put `from`'s notebook files under id `id` with catalog title `title`.
    fn add(&self, id: &str, from: &str, title: &str) {
        self.set_content(id, from);
        let c = rusqlite::Connection::open(&self.catalog).unwrap();
        c.execute(
            "INSERT OR REPLACE INTO Nodes (uuid, title_collation, node_type, additional_data)
             VALUES (?1, ?2, 'STANDALONE_NOTEBOOK', ?3)",
            rusqlite::params![
                id,
                title,
                format!(r#"{{"title":"{title}","notebook_template":"plain","delete_state":0}}"#)
            ],
        )
        .unwrap();
    }

    /// Replace a source's content (the Amazon notebook was edited on the Kindle).
    fn set_content(&self, id: &str, from: &str) {
        let dir = self.root.join(id);
        fs::create_dir_all(&dir).unwrap();
        for f in ["nbk", "actions.log", "nbk-journal"] {
            let src = sample(from).join(f);
            if src.exists() {
                fs::copy(src, dir.join(f)).unwrap();
            }
        }
    }

    fn rename(&self, id: &str, title: &str) {
        let c = rusqlite::Connection::open(&self.catalog).unwrap();
        c.execute(
            "UPDATE Nodes SET title_collation = ?2 WHERE uuid = ?1",
            rusqlite::params![id, title],
        )
        .unwrap();
    }

    /// Every file's bytes (Amazon state must never change because of Scribe).
    fn fingerprint(&self) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        out.insert("catalog".into(), fs::read(&self.catalog).unwrap());
        for d in fs::read_dir(&self.root).unwrap() {
            let d = d.unwrap();
            for f in fs::read_dir(d.path()).unwrap() {
                let f = f.unwrap();
                out.insert(
                    format!(
                        "{}/{}",
                        d.file_name().to_string_lossy(),
                        f.file_name().to_string_lossy()
                    ),
                    fs::read(f.path()).unwrap(),
                );
            }
        }
        out
    }
}

const ID_A: &str = "aaaaaaaa-0000-0000-0000-000000000001";
const ID_B: &str = "bbbbbbbb-0000-0000-0000-000000000002";
const ID_C: &str = "cccccccc-0000-0000-0000-000000000003";

struct Scribe {
    lib: LibraryRepository,
    profile: RenderProfile,
    paper: Vec<u8>,
}

impl Scribe {
    fn new() -> Self {
        let g = geo();
        let paper = paper_background(&g);
        let lib = open_or_create_library(&tmp("library"), InkRequest::Default, &g, &paper)
            .unwrap()
            .repository;
        Self {
            lib,
            profile: RenderProfile::sealed_v2_pressure(0x00).unwrap(),
            paper,
        }
    }

    fn reopen(&mut self) {
        self.lib = crate::library::open_library_read_only(self.lib.root()).unwrap();
    }

    fn import_all(&mut self, k: &Kindle, sync: &dyn ImportSync) -> Vec<(String, ImportOutcome)> {
        let g = geo();
        let mut ledger = Ledger::open(self.lib.root()).unwrap();
        let sources = discover(&k.sources()).unwrap();
        let mut target = ImportTarget {
            library: &mut self.lib,
            profile: &self.profile,
            geo: &g,
            paper: &self.paper,
            cancel: &NO_CANCEL,
        };
        sources
            .iter()
            .map(|s| {
                (
                    s.origin_id.clone(),
                    import_one(&mut target, &mut ledger, s, sync),
                )
            })
            .collect()
    }

    fn scan(&self, k: &Kindle) -> ScanReport {
        scan(&k.sources(), self.lib.root()).unwrap()
    }

    /// Imported notebooks: (notebook id, origin id from its marker).
    fn imported(&self) -> Vec<(String, String)> {
        self.lib
            .manifest()
            .notebook_ids
            .iter()
            .filter_map(|id| marker_origin(&self.lib, id).map(|m| (id.clone(), m.origin_id)))
            .collect()
    }

    fn titles(&self) -> Vec<String> {
        self.lib
            .manifest()
            .notebook_ids
            .iter()
            .map(|id| self.lib.title(id).unwrap().to_string())
            .collect()
    }
}

fn imported_ok(r: &ImportOutcome) -> bool {
    matches!(r, ImportOutcome::Imported { .. })
}

// ------------------------------------------------------------------------------ content

#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn imports_title_pages_and_exact_geometry_into_a_native_notebook() {
    let k = Kindle::new();
    k.add(ID_A, scratch(), "Scratch");
    let before = k.fingerprint();
    let mut s = Scribe::new();
    let out = s.import_all(&k, &NoSync);
    let ImportOutcome::Imported {
        notebook_id,
        pages,
        strokes,
        ..
    } = &out[0].1
    else {
        panic!("{out:?}");
    };
    assert_eq!((*pages, *strokes), scratch_pages_strokes());
    assert_eq!(s.lib.title(notebook_id).unwrap(), "Scratch");
    assert_eq!(
        s.lib.manifest().active_notebook_id,
        "notebook-1",
        "active unchanged"
    );

    // Native pages hold exactly the decoded geometry (canonical == Amazon canvas).
    let decoded = decode_snapshot(&read_source(&sample(scratch())).unwrap()).unwrap();
    let dir = s.lib.notebook_dir(notebook_id);
    let manifest = read_notebook_manifest_read_only(&dir).unwrap();
    assert_eq!(manifest.page_ids.len(), scratch_pages_strokes().0);
    for (page_id, dp) in manifest.page_ids.iter().zip(&decoded.pages) {
        let page = load_page_read_only(&dir, page_id).unwrap();
        assert_eq!(page.strokes.len(), dp.strokes.len());
        for (n, d) in page.strokes.iter().zip(&dp.strokes) {
            let xs: Vec<i64> = n.points.iter().map(|p| p.x as i64).collect();
            let ys: Vec<i64> = n.points.iter().map(|p| p.y as i64).collect();
            assert_eq!((&xs, &ys), (&d.xs, &d.ys));
            let p0 = d.pressure.as_ref().unwrap()[0] as f32 / 4095.0;
            assert!((n.points[0].pressure.unwrap() - p0).abs() < 1e-6);
            assert_eq!(n.brush.as_deref(), Some(STYLE_PRESSURE_WIDTH_V1));
        }
    }
    // Provenance marker + receipt.
    let marker = marker_origin(&s.lib, notebook_id).unwrap();
    assert_eq!(
        (marker.origin_kind.as_str(), marker.origin_id.as_str()),
        (ORIGIN_KIND, ID_A)
    );
    let ledger = Ledger::open(s.lib.root()).unwrap();
    let r = ledger.get(ID_A).unwrap();
    assert_eq!(r.state, ReceiptState::Complete);
    assert_eq!(r.dest_notebook_id.as_deref(), Some(notebook_id.as_str()));
    assert!(r.book_id.is_some());
    assert_eq!(k.fingerprint(), before, "Amazon state must never change");
}

// ------------------------------------------------------------------------------ identity

#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn same_source_imported_twice_yields_one_notebook() {
    let k = Kindle::new();
    k.add(ID_A, scratch(), "Scratch");
    let mut s = Scribe::new();
    assert!(imported_ok(&s.import_all(&k, &NoSync)[0].1));
    assert_eq!(
        s.import_all(&k, &NoSync)[0].1,
        ImportOutcome::AlreadyImported
    );
    s.reopen();
    assert_eq!(s.imported().len(), 1);
    assert_eq!(s.lib.manifest().notebook_ids.len(), 2);
    let scan = s.scan(&k);
    assert_eq!(
        (scan.total(), scan.eligible().len(), scan.already_imported()),
        (1, 0, 1)
    );
}

#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn edited_or_renamed_source_is_still_recognized_and_skipped() {
    let k = Kindle::new();
    k.add(ID_A, scratch(), "Scratch");
    let mut s = Scribe::new();
    assert!(imported_ok(&s.import_all(&k, &NoSync)[0].1));
    // Edited on the Kindle (content replaced) and renamed: same stable id.
    k.set_content(ID_A, journal());
    k.rename(ID_A, "Scratch (renamed)");
    let scan = s.scan(&k);
    assert!(matches!(
        scan.entries[0].status,
        SourceStatus::AlreadyImported { .. }
    ));
    assert_eq!(scan.entries[0].source.title, "Scratch (renamed)");
    assert_eq!(
        s.import_all(&k, &NoSync)[0].1,
        ImportOutcome::AlreadyImported
    );
    assert_eq!(s.imported().len(), 1);
    assert_eq!(
        s.titles(),
        ["Notebook 1", "Scratch"],
        "never re-imported or retitled"
    );
}

#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn same_title_or_identical_content_with_different_ids_all_import() {
    let k = Kindle::new();
    k.add(ID_A, scratch(), "Scratch");
    k.add(ID_B, scratch(), "Scratch"); // identical title AND identical content
    k.add(ID_C, journal(), "Scratch"); // identical title, different content
    let mut s = Scribe::new();
    let out = s.import_all(&k, &NoSync);
    assert!(out.iter().all(|(_, r)| imported_ok(r)), "{out:?}");
    let mut origins: Vec<String> = s.imported().into_iter().map(|(_, o)| o).collect();
    origins.sort();
    assert_eq!(origins, [ID_A, ID_B, ID_C]);
    assert_eq!(
        s.import_all(&k, &NoSync)
            .iter()
            .filter(|(_, r)| imported_ok(r))
            .count(),
        0
    );
}

#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn deleting_the_imported_copy_does_not_make_the_source_eligible_again() {
    let k = Kindle::new();
    k.add(ID_A, scratch(), "Scratch");
    let mut s = Scribe::new();
    let ImportOutcome::Imported { notebook_id, .. } = s.import_all(&k, &NoSync).remove(0).1 else {
        panic!()
    };
    delete_notebook_durable(&mut s.lib, &notebook_id, &DeleteNotebookHooks::default()).unwrap();
    let scan = s.scan(&k);
    assert_eq!(scan.eligible().len(), 0);
    assert_eq!(
        s.import_all(&k, &NoSync)[0].1,
        ImportOutcome::AlreadyImported
    );
    assert!(s.imported().is_empty());
    assert_eq!(s.lib.manifest().notebook_ids, ["notebook-1"]);
}

// ------------------------------------------------------------------------------ failures

#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn a_corrupt_or_unsupported_source_fails_alone_and_stays_retryable() {
    let k = Kindle::new();
    k.add(ID_A, scratch(), "Good");
    k.add(ID_B, scratch(), "Corrupt");
    fs::write(k.root.join(ID_B).join("nbk"), b"SQLite format 3\0 garbage").unwrap();
    k.add(ID_C, pending(), "Pending actions");
    let before = k.fingerprint();
    let mut s = Scribe::new();
    let out: BTreeMap<String, ImportOutcome> = s.import_all(&k, &NoSync).into_iter().collect();
    assert!(imported_ok(&out[ID_A]));
    assert!(
        matches!(&out[ID_B], ImportOutcome::Failed(e) if e.contains("NBK_CONTAINER")),
        "{out:?}"
    );
    assert!(matches!(&out[ID_C], ImportOutcome::Failed(e) if e.contains("NBK_PENDING_ACTIONS")));
    assert_eq!(
        s.imported().len(),
        1,
        "no partial notebooks for failed sources"
    );
    let scan = s.scan(&k);
    let status = |id: &str| {
        scan.entries
            .iter()
            .find(|e| e.source.origin_id == id)
            .unwrap()
            .status
            .clone()
    };
    assert_eq!(status(ID_B), SourceStatus::Retry);
    assert_eq!(status(ID_C), SourceStatus::Retry);
    assert_eq!(k.fingerprint(), before);
    // Fixed on the Kindle → the retry imports it.
    k.set_content(ID_B, scratch());
    let out: BTreeMap<String, ImportOutcome> = s.import_all(&k, &NoSync).into_iter().collect();
    assert!(imported_ok(&out[ID_B]));
    assert_eq!(s.imported().len(), 2);
}

#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn a_failure_mid_import_is_rolled_back_and_retryable_without_duplicates() {
    let k = Kindle::new();
    k.add(ID_A, scratch(), "Scratch");
    let mut s = Scribe::new();
    FAULT.with(|f| f.set(Some(Fault::FailAfterFirstPage)));
    let out = s.import_all(&k, &NoSync);
    FAULT.with(|f| f.set(None));
    assert!(matches!(&out[0].1, ImportOutcome::Failed(_)), "{out:?}");
    assert!(s.imported().is_empty(), "partial copy removed");
    assert_eq!(s.lib.manifest().notebook_ids, ["notebook-1"]);
    assert_eq!(s.scan(&k).entries[0].status, SourceStatus::Retry);
    assert!(imported_ok(&s.import_all(&k, &NoSync)[0].1));
    assert_eq!(s.imported().len(), 1);
}

#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn restart_during_a_pending_import_never_creates_a_second_notebook() {
    for fault_point in [Fault::CrashAfterPending, Fault::CrashAfterFirstPage] {
        let k = Kindle::new();
        k.add(ID_A, scratch(), "Scratch");
        let mut s = Scribe::new();
        FAULT.with(|f| f.set(Some(fault_point)));
        let _ = s.import_all(&k, &NoSync);
        FAULT.with(|f| f.set(None));
        let ledger = Ledger::open(s.lib.root()).unwrap();
        assert_eq!(ledger.get(ID_A).unwrap().state, ReceiptState::Pending);

        // "Restart": reopen the library, then startup recovery before any UI.
        s.reopen();
        let log = recover_interrupted(&mut s.lib, &NoSync).unwrap();
        assert_eq!(log.len(), 1, "{fault_point:?}");
        assert!(
            s.imported().is_empty(),
            "{fault_point:?}: partial copy removed"
        );
        assert_eq!(s.scan(&k).entries[0].status, SourceStatus::Retry);

        assert!(imported_ok(&s.import_all(&k, &NoSync)[0].1));
        s.reopen();
        assert_eq!(s.imported().len(), 1, "{fault_point:?}: exactly one copy");
        assert!(recover_interrupted(&mut s.lib, &NoSync).unwrap().is_empty());
        assert_eq!(
            s.import_all(&k, &NoSync)[0].1,
            ImportOutcome::AlreadyImported
        );
    }
}

#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn a_reserved_but_never_created_id_is_never_mistaken_for_a_partial_copy() {
    let k = Kindle::new();
    k.add(ID_A, scratch(), "Scratch");
    let mut s = Scribe::new();
    FAULT.with(|f| f.set(Some(Fault::CrashAfterPending)));
    let _ = s.import_all(&k, &NoSync);
    FAULT.with(|f| f.set(None));
    let reserved = Ledger::open(s.lib.root())
        .unwrap()
        .get(ID_A)
        .unwrap()
        .dest_notebook_id
        .clone()
        .unwrap();
    s.reopen();
    recover_interrupted(&mut s.lib, &NoSync).unwrap();
    // The user then creates a notebook, which takes the released id.
    let g = geo();
    let c = crate::library::create_notebook_durable(
        &mut s.lib,
        &s.profile,
        &g,
        &s.paper,
        Some("Mine"),
        &crate::library::CreateNotebookHooks::default(),
    )
    .unwrap();
    assert_eq!(c.notebook_id, reserved);
    // A retry must never touch it.
    assert!(imported_ok(&s.import_all(&k, &NoSync)[0].1));
    assert_eq!(s.lib.title(&reserved).unwrap(), "Mine");
    assert_eq!(s.imported().len(), 1);
}

// ------------------------------------------------------------------------------ sync

#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn imported_notebooks_are_captured_exactly_for_sync_including_after_a_crash() {
    let k = Kindle::new();
    k.add(ID_A, scratch(), "Scratch");
    k.add(ID_B, scratch(), "Scratch 2");
    let mut s = Scribe::new();
    let sync_dir = tmp("sync");
    // Offline capture (no server configured): the outbox is the proof.
    let sync = OutboundSync::open(s.lib.root(), &sync_dir).unwrap();
    FAULT.with(|f| f.set(Some(Fault::CrashAfterFirstPage)));
    let first = discover(&k.sources()).unwrap().remove(0);
    {
        let g = geo();
        let mut ledger = Ledger::open(s.lib.root()).unwrap();
        let mut t = ImportTarget {
            library: &mut s.lib,
            profile: &s.profile,
            geo: &g,
            paper: &s.paper,
            cancel: &NO_CANCEL,
        };
        let _ = import_one(&mut t, &mut ledger, &first, &sync);
    }
    FAULT.with(|f| f.set(None));
    sync.shutdown(std::time::Duration::from_secs(3));

    // Restart: sync reconciliation must accept the partial state (creates were flushed,
    // strokes are a re-derivable tail), then recovery deletes the partial copy (synced).
    let sync = OutboundSync::open(s.lib.root(), &sync_dir).unwrap();
    assert!(sync.fault().is_none(), "{:?}", sync.fault());
    s.reopen();
    recover_interrupted(&mut s.lib, &sync).unwrap();
    let out = s.import_all(&k, &sync);
    assert!(out.iter().all(|(_, r)| imported_ok(r)), "{out:?}");
    sync.shutdown(std::time::Duration::from_secs(3));

    // Base + outbox replays to exactly the local library.
    let sync = OutboundSync::open(s.lib.root(), &sync_dir).unwrap();
    assert!(sync.fault().is_none(), "{:?}", sync.fault());
    assert_eq!(s.imported().len(), 2);
    sync.shutdown(std::time::Duration::from_secs(3));
}

#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn a_corrupt_ledger_fails_closed() {
    let s = Scribe::new();
    let p = Ledger::path_for(s.lib.root());
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, b"{not json").unwrap();
    assert!(Ledger::open(s.lib.root()).is_err());
    let k = Kindle::new();
    k.add(ID_A, scratch(), "Scratch");
    assert!(scan(&k.sources(), s.lib.root()).is_err());
}

#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn discovery_uses_the_catalog_and_skips_deleted_and_non_notebook_entries() {
    let k = Kindle::new();
    k.add(ID_A, scratch(), "  Tab\tTitle  ");
    k.add(ID_B, scratch(), "Deleted one");
    let c = rusqlite::Connection::open(&k.catalog).unwrap();
    c.execute(
        "UPDATE Nodes SET additional_data = '{\"delete_state\":1}' WHERE uuid = ?1",
        [ID_B],
    )
    .unwrap();
    c.execute(
        "INSERT INTO Nodes (uuid, title_collation, node_type) VALUES (?1, 'Folder', 'FOLDER')",
        [ID_C],
    )
    .unwrap();
    k.set_content(ID_C, scratch());
    let found = discover(&k.sources()).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].origin_id.as_str(), found[0].title.as_str()),
        (ID_A, "TabTitle")
    );
    // Without a catalog: UUID-named directories, fallback titles.
    let no_catalog = ImportSources {
        notebooks_root: k.root.clone(),
        catalog: None,
    };
    let found = discover(&no_catalog).unwrap();
    assert_eq!(found.len(), 3);
    assert!(found.iter().all(|f| f.title == FALLBACK_TITLE));
}

// ------------------------------------------------------------------------------ backpressure

fn import_one_with(s: &mut Scribe, src: &SourceNotebook, sync: &dyn ImportSync) -> ImportOutcome {
    let g = geo();
    let mut ledger = Ledger::open(s.lib.root()).unwrap();
    let mut t = ImportTarget {
        library: &mut s.lib,
        profile: &s.profile,
        geo: &g,
        paper: &s.paper,
        cancel: &NO_CANCEL,
    };
    import_one(&mut t, &mut ledger, src, sync)
}

fn source(k: &Kindle, id: &str) -> SourceNotebook {
    discover(&k.sources())
        .unwrap()
        .into_iter()
        .find(|s| s.origin_id == id)
        .unwrap()
}

/// Large notebook import followed immediately by another: each batch must be durable before
/// the next is written, and a journal slower than the old 5 s bound is waited out instead of
/// faulting sync.
#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn a_large_import_followed_at_once_by_another_never_outruns_or_faults_the_journal() {
    let k = Kindle::new();
    k.add(ID_A, journal(), "Journal");
    k.add(ID_B, scratch(), "Scratch");
    let mut s = Scribe::new();
    let sync_dir = tmp("sync");
    let sync = OutboundSync::open(s.lib.root(), &sync_dir).unwrap();

    let out = import_one_with(&mut s, &source(&k, ID_A), &sync);
    let ImportOutcome::Imported { strokes, pages, .. } = out else {
        panic!("{out:?}")
    };
    assert_eq!((pages, strokes), journal_pages_strokes());
    // Backpressure: the import returned with nothing left behind in the journal.
    let st = sync.stats();
    assert_eq!(
        st.captured, st.journal_records_durable,
        "no backlog after an import"
    );
    assert!(
        st.journal_syncs < st.captured / 4,
        "durability is batched: {} barriers for {} records",
        st.journal_syncs,
        st.captured
    );

    // The next import's journal is slower than the old PREP_TIMEOUT (held 6 s).
    let gate = Arc::new(crate::sync_outbound::JournalGate::default());
    gate.close();
    sync.set_journal_gate(Some(Arc::clone(&gate)));
    let opener = {
        let gate = Arc::clone(&gate);
        std::thread::spawn(move || {
            gate.wait_held();
            std::thread::sleep(std::time::Duration::from_secs(6));
            gate.open();
        })
    };
    let out = import_one_with(&mut s, &source(&k, ID_B), &sync);
    opener.join().unwrap();
    sync.set_journal_gate(None);
    assert!(imported_ok(&out), "{out:?}");
    assert!(sync.fault().is_none(), "{:?}", sync.fault());
    let st = sync.stats();
    assert_eq!(st.captured, st.journal_records_durable);
    sync.shutdown(std::time::Duration::from_secs(3));

    // Restart: base + outbox replays to exactly the library (both notebooks, every stroke).
    let sync = OutboundSync::open(s.lib.root(), &sync_dir).unwrap();
    assert!(sync.fault().is_none(), "{:?}", sync.fault());
    sync.shutdown(std::time::Duration::from_secs(3));
    s.reopen();
    assert_eq!(s.imported().len(), 2);
}

/// Sync faults mid-import: the run halts at once (this notebook: partial copy kept, receipt
/// `failed`; later notebooks: never started), a restart recovers cleanly, and a retry
/// completes everything with exactly one copy per source.
#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn a_sync_fault_mid_import_halts_the_run_and_recovers_after_restart() {
    let k = Kindle::new();
    k.add(ID_A, scratch(), "A");
    k.add(ID_B, scratch(), "B");
    k.add(ID_C, scratch(), "C");
    let mut s = Scribe::new();
    let sync_dir = tmp("sync");
    let sync = OutboundSync::open(s.lib.root(), &sync_dir).unwrap();
    // Let the first two journal groups (notebook create, first page) through, then fault
    // while the third is held — as a stalled journal did on the device.
    let gate = Arc::new(crate::sync_outbound::JournalGate::default());
    gate.close();
    sync.set_journal_gate(Some(Arc::clone(&gate)));
    let inject = sync.fault_injector();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..2 {
                gate.wait_held();
                gate.release_one();
            }
            gate.wait_held();
            inject("simulated journal stall");
            gate.open();
        });
        let out: BTreeMap<String, ImportOutcome> = s.import_all(&k, &sync).into_iter().collect();
        assert!(
            matches!(&out[ID_A], ImportOutcome::Halted(e) if e.starts_with("sync halted") && e.contains("partial copy")),
            "{out:?}"
        );
        for id in [ID_B, ID_C] {
            assert!(
                matches!(&out[id], ImportOutcome::Halted(e) if e.contains("import start")),
                "{id}: {out:?}"
            );
        }
    });
    sync.set_journal_gate(None);
    // Nothing after the fault was created; A's partial copy is kept (deleting it without a
    // journaled delete would diverge the library from the sync journal).
    let ledger = Ledger::open(s.lib.root()).unwrap();
    let a = ledger.get(ID_A).unwrap();
    assert_eq!(a.state, ReceiptState::Failed);
    let partial = a.dest_notebook_id.clone().unwrap();
    assert!(ledger.get(ID_B).is_none() && ledger.get(ID_C).is_none());
    assert_eq!(
        s.lib.manifest().notebook_ids,
        ["notebook-1", partial.as_str()]
    );
    sync.shutdown(std::time::Duration::from_secs(3));

    // Restart: recovery accepts the partial state; the retry replaces the partial copy through
    // the synced delete and imports all three.
    let sync = OutboundSync::open(s.lib.root(), &sync_dir).unwrap();
    assert!(sync.fault().is_none(), "{:?}", sync.fault());
    s.reopen();
    assert!(recover_interrupted(&mut s.lib, &sync).unwrap().is_empty());
    let out = s.import_all(&k, &sync);
    assert!(out.iter().all(|(_, r)| imported_ok(r)), "{out:?}");
    sync.shutdown(std::time::Duration::from_secs(3));
    s.reopen();
    let mut origins: Vec<String> = s.imported().into_iter().map(|(_, o)| o).collect();
    origins.sort();
    assert_eq!(origins, [ID_A, ID_B, ID_C], "exactly one copy per source");
    assert!(!s.lib.manifest().notebook_ids.contains(&partial));
    // …and a final restart replays base + outbox to exactly the library (no duplicates).
    let sync = OutboundSync::open(s.lib.root(), &sync_dir).unwrap();
    assert!(sync.fault().is_none(), "{:?}", sync.fault());
    sync.shutdown(std::time::Duration::from_secs(3));
}

#[test]
#[ignore = "private samples: set IGNITE_NBK_IMPORT_SAMPLES and IGNITE_NBK_IMPORT_FIXTURES; run with -- --ignored"]
fn import_sync_flush_reports_healthy_and_faulted_sync_truthfully() {
    let s = Scribe::new();
    let sync = OutboundSync::open(s.lib.root(), &tmp("sync")).unwrap();
    assert_eq!(ImportSync::flush(&sync, &NO_CANCEL), Ok(()));
    assert_eq!(ImportSync::flush(&NoSync, &NO_CANCEL), Ok(()));
    sync.fail_external("simulated upload fault".into());
    // Upload/reconcile faults are not local journal failures.
    assert_eq!(ImportSync::flush(&sync, &NO_CANCEL), Ok(()));
    sync.journal_fault_injector()("simulated disk stall");
    assert!(matches!(
        ImportSync::flush(&sync, &NO_CANCEL),
        Err(DurableWaitError::Faulted(_))
    ));
    sync.shutdown(std::time::Duration::from_secs(3));
}

// ------------------------------------------------------------------------------ fixture resolver
// Synthetic validation of the external manifest (no private notebook data required).

fn valid_manifest_json() -> String {
    r#"{
      "version": 1,
      "roles": {
        "journal": { "id": "aaaaaaaa-1111-1111-1111-111111111111", "pages": 2, "strokes": 10 },
        "scratch": { "id": "bbbbbbbb-2222-2222-2222-222222222222", "pages": 1, "strokes": 3 },
        "pending": { "id": "cccccccc-3333-3333-3333-333333333333" }
      }
    }"#
    .into()
}

#[test]
fn fixture_manifest_resolves_all_roles() {
    let f = NbkImportFixtures::parse(&valid_manifest_json()).unwrap();
    assert_eq!(f.journal.id, "aaaaaaaa-1111-1111-1111-111111111111");
    assert_eq!(f.scratch.counts(), (1, 3));
    assert_eq!(f.journal.counts(), (2, 10));
    assert_eq!(f.pending.id, "cccccccc-3333-3333-3333-333333333333");
    assert!(f.pending.pages.is_none());
}

#[test]
fn fixture_manifest_rejects_missing_role() {
    let j = r#"{"version":1,"roles":{
      "journal":{"id":"aaaaaaaa-1111-1111-1111-111111111111","pages":1,"strokes":1},
      "scratch":{"id":"bbbbbbbb-2222-2222-2222-222222222222","pages":1,"strokes":1}
    }}"#;
    let err = NbkImportFixtures::parse(j).unwrap_err();
    assert!(err.contains("missing required role: pending"), "{err}");
}

#[test]
fn fixture_manifest_rejects_unknown_role() {
    let j = r#"{"version":1,"roles":{
      "journal":{"id":"aaaaaaaa-1111-1111-1111-111111111111","pages":1,"strokes":1},
      "scratch":{"id":"bbbbbbbb-2222-2222-2222-222222222222","pages":1,"strokes":1},
      "pending":{"id":"cccccccc-3333-3333-3333-333333333333"},
      "extra":{"id":"dddddddd-4444-4444-4444-444444444444"}
    }}"#;
    let err = NbkImportFixtures::parse(j).unwrap_err();
    assert!(err.contains("unknown role"), "{err}");
}

#[test]
fn fixture_manifest_rejects_duplicate_role_key() {
    let j = r#"{"version":1,"roles":{
      "journal":{"id":"aaaaaaaa-1111-1111-1111-111111111111","pages":1,"strokes":1},
      "scratch":{"id":"bbbbbbbb-2222-2222-2222-222222222222","pages":1,"strokes":1},
      "pending":{"id":"cccccccc-3333-3333-3333-333333333333"},
      "journal":{"id":"dddddddd-4444-4444-4444-444444444444","pages":1,"strokes":1}
    }}"#;
    let err = NbkImportFixtures::parse(j).unwrap_err();
    assert!(
        err.contains("duplicate role") || err.contains("malformed"),
        "{err}"
    );
}

#[test]
fn fixture_manifest_rejects_malformed_json() {
    let err = NbkImportFixtures::parse("{not json").unwrap_err();
    assert!(err.contains("malformed"), "{err}");
}

#[test]
fn fixture_manifest_rejects_unsupported_version() {
    let j = r#"{"version":99,"roles":{
      "journal":{"id":"aaaaaaaa-1111-1111-1111-111111111111","pages":1,"strokes":1},
      "scratch":{"id":"bbbbbbbb-2222-2222-2222-222222222222","pages":1,"strokes":1},
      "pending":{"id":"cccccccc-3333-3333-3333-333333333333"}
    }}"#;
    let err = NbkImportFixtures::parse(j).unwrap_err();
    assert!(
        err.contains("unsupported fixture manifest version"),
        "{err}"
    );
}

#[test]
fn fixture_manifest_rejects_empty_id() {
    let j = r#"{"version":1,"roles":{
      "journal":{"id":"  ","pages":1,"strokes":1},
      "scratch":{"id":"bbbbbbbb-2222-2222-2222-222222222222","pages":1,"strokes":1},
      "pending":{"id":"cccccccc-3333-3333-3333-333333333333"}
    }}"#;
    let err = NbkImportFixtures::parse(j).unwrap_err();
    assert!(err.contains("empty id"), "{err}");
}

#[test]
fn fixture_manifest_rejects_reused_fixture_identity() {
    let j = r#"{"version":1,"roles":{
      "journal":{"id":"aaaaaaaa-1111-1111-1111-111111111111","pages":1,"strokes":1},
      "scratch":{"id":"aaaaaaaa-1111-1111-1111-111111111111","pages":1,"strokes":1},
      "pending":{"id":"cccccccc-3333-3333-3333-333333333333"}
    }}"#;
    let err = NbkImportFixtures::parse(j).unwrap_err();
    assert!(err.contains("reused across roles"), "{err}");
}

#[test]
fn fixture_manifest_rejects_missing_pages_strokes_on_content_roles() {
    let j = r#"{"version":1,"roles":{
      "journal":{"id":"aaaaaaaa-1111-1111-1111-111111111111"},
      "scratch":{"id":"bbbbbbbb-2222-2222-2222-222222222222","pages":1,"strokes":1},
      "pending":{"id":"cccccccc-3333-3333-3333-333333333333"}
    }}"#;
    let err = NbkImportFixtures::parse(j).unwrap_err();
    assert!(err.contains("pages and strokes"), "{err}");
}

#[test]
fn fixture_manifest_env_path_must_be_absolute_and_non_empty() {
    let err = NbkImportFixtures::require_absolute_env_path("").unwrap_err();
    assert!(err.contains("must be set"), "{err}");
    let err = NbkImportFixtures::require_absolute_env_path("relative.json").unwrap_err();
    assert!(err.contains("must be absolute"), "{err}");
    let abs = std::env::temp_dir().join("ignite-nbk-fixtures.json");
    let path = NbkImportFixtures::require_absolute_env_path(abs.to_str().unwrap()).unwrap();
    assert!(path.is_absolute());
}

#[test]
fn fixture_manifest_load_from_path_missing_file_fails() {
    let missing = std::env::temp_dir().join(format!(
        "ignite-nbk-fixtures-missing-{}.json",
        std::process::id()
    ));
    let err = NbkImportFixtures::load_from_path(&missing).unwrap_err();
    assert!(err.contains("failed to read"), "{err}");
}
