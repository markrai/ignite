//! Slice 1H-D1 tests: library adoption.
//!
//! * `verify_*`: the canonical-data checks, pure.
//! * engine tests: real state transitions on disposable Kindle installations (an offline sync
//!   epoch with queued and uncaptured local work), including every crash point of preparation
//!   and publication.
//! * end-to-end: two real product servers (control plane + sync). A Kindle syncs a library with
//!   server A; A's portable backup is restored onto server B (new server ID, new library ID); the
//!   Kindle adopts B's library and synchronizes with B from client_seq 1.

use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kindle_platform::FbGeometry;
use notebook_core::{new_stroke_id, Point, Stroke};
use scribe_server::config::{Config, DiscoverySection, ServerSection};
use serde_json::{json, Value};
use sync_engine::testkit;
use sync_protocol::{
    snapshot_sha256, FolderId, NotebookId, PageSpec, SyncEnvelopeV1, SyncOperationV1, SyncPoint,
};

use super::*;
use crate::ink::InkRequest;
use crate::library::open_or_create_library;
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use crate::store::{load_existing_page, PageStore};
use crate::sync_local::ensure_sync_identity;
use crate::sync_outbound::{OutboundSync, SyncConfigV1, CONFIG_FILE, OUTBOX_FILE};
use crate::ui_gfx::paper_background;

// ------------------------------------------------------------------ fixtures

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "scribe-adopt-{label}-{}-{n}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ))
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

fn device() -> DeviceGeometry {
    DeviceGeometry {
        width: 200,
        height: 160,
        line_length: 208,
    }
}

fn new_server_id() -> ServerId {
    ServerId::from_uuid(Uuid::now_v7()).unwrap()
}

fn new_library_id() -> LibraryId {
    LibraryId::from_uuid(Uuid::now_v7()).unwrap()
}

/// A disposable Kindle installation laid out like the device: `scribe-data/`,
/// `state/sync/`, and an unrelated sibling (`state/device/`) that adoption must not touch.
struct Kindle {
    root: PathBuf,
    lib: PathBuf,
    sync: PathBuf,
    device_dir: PathBuf,
}

impl Kindle {
    fn new(label: &str) -> Self {
        let root = tmp(label);
        let k = Self {
            lib: root.join("scribe-data"),
            sync: root.join("state").join("sync"),
            device_dir: root.join("state").join("device"),
            root,
        };
        let g = geo();
        open_or_create_library(&k.lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
        fs::create_dir_all(&k.sync).unwrap();
        fs::create_dir_all(&k.device_dir).unwrap();
        fs::write(
            k.device_dir.join("server-profile.json"),
            b"{\"unrelated\":true}\n",
        )
        .unwrap();
        k
    }
    fn open(&self) -> OutboundSync {
        OutboundSync::open(&self.lib, &self.sync).unwrap()
    }
    fn snapshot(&self) -> SyncSnapshotV1 {
        build_snapshot(&self.lib, &read_identity(&self.lib).unwrap().unwrap()).unwrap()
    }
    fn state(&self) -> SyncStateV1 {
        read_json(&self.sync.join(STATE_FILE)).unwrap()
    }
    fn first_page(&self) -> (String, String) {
        let (_, layout) = crate::sync_local::live_layout(&self.lib).unwrap();
        let (nb, pages) = layout.notebooks[0].clone();
        (nb, pages[0].clone())
    }
    fn layout(&self) -> Layout {
        Layout::new(&self.lib, &self.sync).unwrap()
    }
    fn adopt(&self, src: &VerifiedGenesis) -> Result<AdoptionOutcome, AdoptError> {
        adopt_library(&self.lib, &self.sync, src, &AdoptOptions::new(device()))
    }
    fn adopt_with(
        &self,
        src: &VerifiedGenesis,
        hook: Hook<'_>,
    ) -> Result<AdoptionOutcome, AdoptError> {
        adopt_library(
            &self.lib,
            &self.sync,
            src,
            &AdoptOptions {
                hook,
                ..AdoptOptions::new(device())
            },
        )
    }
}

impl Drop for Kindle {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Every file under `dir` (relative path → sha256). Lock files carry no state.
fn tree(dir: &Path) -> BTreeMap<String, String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(base, &p, out);
            } else if !p.to_string_lossy().ends_with(".lock") {
                let rel = p
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, sha256_hex(&fs::read(&p).unwrap()).to_string());
            }
        }
    }
    let mut out = BTreeMap::new();
    if dir.exists() {
        walk(dir, dir, &mut out);
    }
    out
}

/// The installation's three trees.
fn trees(k: &Kindle) -> [BTreeMap<String, String>; 3] {
    [tree(&k.lib), tree(&k.sync), tree(&k.device_dir)]
}

/// A durable local stroke on `nb/page`, captured for sync when `s` is given (runtime path),
/// or uncaptured (the post-commit window) when not.
fn write_stroke(s: Option<&OutboundSync>, lib: &Path, nb: &str, page: &str, y: f32) -> Stroke {
    let stroke = Stroke {
        id: new_stroke_id(),
        points: (0..12)
            .map(|i| Point::new(1000.0 + 700.0 * i as f32, y).with_pressure(0.6))
            .collect(),
        brush: Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
        finished: true,
    };
    let (mut store, _) = load_existing_page(&lib.join("notebooks").join(nb), page).unwrap();
    store.append_committed_stroke(&stroke).unwrap();
    if let Some(s) = s {
        s.stroke_committed(nb, page, &stroke);
    }
    stroke
}

fn all_points(snap: &SyncSnapshotV1) -> Vec<Vec<SyncPoint>> {
    snap.notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .flat_map(|p| p.strokes.iter().map(|s| s.points.clone()))
        .collect()
}

fn local_points(stroke: &Stroke) -> Vec<SyncPoint> {
    stroke
        .points
        .iter()
        .map(|p| SyncPoint::new(p.x, p.y, p.pressure))
        .collect()
}

/// Operations a remote replica adds to a Kindle's library: a folder holding a second notebook
/// with two pages, handwriting on three pages, a text object and a cross-notebook link.
fn content_ops(base: &SyncSnapshotV1) -> Vec<SyncOperationV1> {
    let nb1 = base.notebooks[0].notebook_id;
    let p1 = &base.notebooks[0].pages[0];
    let (nb2, f1) = (testkit::nb(2), testkit::fd(1));
    let (p2, p3) = (testkit::pg(2), testkit::pg(3));
    let spec = |page_id| PageSpec {
        page_id,
        background: BackgroundRef::Blank,
        render: p1.render.clone(),
    };
    vec![
        SyncOperationV1::CreateFolder {
            folder_id: f1,
            title: "Projects".into(),
        },
        SyncOperationV1::CreateNotebook {
            notebook_id: nb2,
            title: "Plans".into(),
            after_notebook_id: Some(nb1),
            first_page: spec(p2),
        },
        SyncOperationV1::CreatePage {
            notebook_id: nb2,
            page: spec(p3),
            after_page_id: Some(p2),
        },
        SyncOperationV1::SetNotebookFolder {
            notebook_id: nb2,
            folder_id: Some(f1),
        },
        SyncOperationV1::AddStroke {
            notebook_id: nb1,
            page_id: p1.page_id,
            stroke: testkit::line(
                testkit::sk(1),
                testkit::sk(1),
                (1000.0, 1000.0),
                (6000.0, 1400.0),
                9,
                true,
            ),
        },
        SyncOperationV1::AddStroke {
            notebook_id: nb2,
            page_id: p2,
            stroke: testkit::line(
                testkit::sk(2),
                testkit::sk(2),
                (2000.0, 2000.0),
                (4000.0, 6000.0),
                7,
                true,
            ),
        },
        SyncOperationV1::AddStroke {
            notebook_id: nb2,
            page_id: p3,
            stroke: testkit::line(
                testkit::sk(3),
                testkit::sk(3),
                (1500.0, 9000.0),
                (9000.0, 9000.0),
                5,
                true,
            ),
        },
        SyncOperationV1::AddText {
            notebook_id: nb2,
            page_id: p2,
            text: testkit::text_obj(1, "Adopted text"),
        },
        SyncOperationV1::AddPageLink {
            notebook_id: nb2,
            page_id: p2,
            link: testkit::page_link(1, nb2, p2, nb1, p1.page_id),
        },
    ]
}

/// A synthetic source library as a Kindle would hold it (its own snapshot: Kindle render
/// profile, blank paper) plus [`content_ops`], re-keyed to `library` the way a restore does.
fn synthetic_genesis(library: LibraryId) -> (Vec<u8>, SyncSnapshotV1) {
    let k = Kindle::new("source");
    let id = ensure_sync_identity(&k.lib).unwrap().identity;
    let base = build_snapshot(&k.lib, &id).unwrap();
    let mut state = LibraryState::genesis(base.clone()).unwrap();
    let mut author = testkit::Author {
        library: base.library_id,
        ..testkit::Author::new(testkit::replica(9))
    };
    for op in content_ops(&base) {
        state.apply_in_place(&author.env(op)).unwrap();
    }
    let mut snap = state.snapshot;
    snap.library_id = library;
    (snap.to_canonical_bytes().unwrap(), snap)
}

fn info_for(library: LibraryId, sha: &Sha256Hex, head: u64) -> ServerLibraryInfo {
    ServerLibraryInfo {
        library_id: library,
        genesis_sha256: sha.clone(),
        head_server_seq: head,
        snapshot_sha256: sha.clone(),
    }
}

/// A verified restored library on a server with ID `server`.
fn restored(server: ServerId) -> VerifiedGenesis {
    let library = new_library_id();
    let (bytes, _) = synthetic_genesis(library);
    let sha = sha256_hex(&bytes);
    VerifiedGenesis::verify(
        AdoptionTarget {
            server_id: server,
            library_id: library,
        },
        server,
        &info_for(library, &sha, 0),
        library,
        &sha,
        bytes,
        Vec::new(),
    )
    .unwrap()
}

/// An existing installation: an (offline) sync epoch of its own library with one queued
/// stroke, one uncaptured stroke, replica preferences in the sync dir.
fn installation(label: &str) -> (Kindle, Stroke, Stroke) {
    let k = Kindle::new(label);
    fs::write(
        k.sync.join(crate::prefs::PREFERENCES_FILE),
        b"{\"v\":1,\"side_button\":\"next_page\"}\n",
    )
    .unwrap();
    let (nb, page) = k.first_page();
    let s = k.open();
    let queued = write_stroke(Some(&s), &k.lib, &nb, &page, 3000.0);
    s.wait_journal_durable(&AtomicBool::new(false)).unwrap();
    assert_eq!(s.pending(), 1);
    s.shutdown(Duration::from_secs(5));
    let uncaptured = write_stroke(None, &k.lib, &nb, &page, 5000.0);
    (k, queued, uncaptured)
}

/// The adopted installation is exactly the genesis, with a fresh epoch bound to the server.
fn assert_adopted(k: &Kindle, src: &VerifiedGenesis, old_replica: ReplicaId) {
    let snap = k.snapshot();
    assert_eq!(&snap, src.snapshot());
    assert_eq!(snapshot_sha256(&snap).unwrap(), *src.genesis_sha256());
    let st = k.state();
    let t = src.target();
    assert_eq!(st.library_id, t.library_id);
    assert_eq!(st.server_id, Some(t.server_id));
    assert_ne!(st.replica_id, old_replica);
    assert_eq!(
        (st.acked_client_seq, st.next_client_seq, st.server_cursor),
        (0, 1, 0)
    );
    assert!(
        st.genesis_uploaded,
        "the library already exists on the server"
    );
    assert_eq!(st.genesis_sha256, *src.genesis_sha256());
    assert_eq!(fs::read(k.sync.join(OUTBOX_FILE)).unwrap(), b"");
    assert_eq!(
        fs::read(k.sync.join(crate::sync_outbound::GENESIS_FILE)).unwrap(),
        src.genesis_bytes
    );
    let l = k.layout();
    assert!(!l.workspace.exists() && !l.staged_library.exists());
    assert!(!adoption_pending(&k.sync));
}

fn retired_dirs(k: &Kindle) -> (PathBuf, PathBuf) {
    let find = |dir: &Path| {
        let parent = dir.parent().unwrap();
        let prefix = format!("{}.retired-", dir.file_name().unwrap().to_string_lossy());
        let mut v: Vec<PathBuf> = fs::read_dir(parent)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
            })
            .collect();
        assert_eq!(v.len(), 1, "exactly one retired copy of {}", dir.display());
        v.pop().unwrap()
    };
    (find(&k.lib), find(&k.sync))
}

// ------------------------------------------------------------------ verification (pure)

struct Answers {
    target: AdoptionTarget,
    server: ServerId,
    info: ServerLibraryInfo,
    doc_library: LibraryId,
    doc_sha: Sha256Hex,
    bytes: Vec<u8>,
    blobs: Vec<Vec<u8>>,
}

impl Answers {
    fn good() -> Self {
        let server = new_server_id();
        let library = new_library_id();
        let (bytes, _) = synthetic_genesis(library);
        let sha = sha256_hex(&bytes);
        Self {
            target: AdoptionTarget {
                server_id: server,
                library_id: library,
            },
            server,
            info: info_for(library, &sha, 0),
            doc_library: library,
            doc_sha: sha,
            bytes,
            blobs: Vec::new(),
        }
    }
    /// Replace the genesis bytes, keeping every advertised hash consistent with them.
    fn with_bytes(mut self, bytes: Vec<u8>) -> Self {
        let sha = sha256_hex(&bytes);
        self.info.genesis_sha256 = sha.clone();
        self.info.snapshot_sha256 = sha.clone();
        self.doc_sha = sha;
        self.bytes = bytes;
        self
    }
    fn verify(self) -> Result<VerifiedGenesis, AdoptError> {
        VerifiedGenesis::verify(
            self.target,
            self.server,
            &self.info,
            self.doc_library,
            &self.doc_sha,
            self.bytes,
            self.blobs,
        )
    }
    fn refused(self, kind: AdoptErrorKind) -> String {
        let e = self.verify().unwrap_err();
        assert_eq!(e.kind, kind, "{e}");
        e.message
    }
}

#[test]
fn verify_accepts_the_selected_servers_canonical_genesis() {
    let a = Answers::good();
    let (server, library, sha) = (a.server, a.target.library_id, a.doc_sha.clone());
    let v = a.verify().unwrap();
    assert_eq!(v.target().server_id, server);
    assert_eq!(v.snapshot().library_id, library);
    assert_eq!(*v.genesis_sha256(), sha);
    assert_eq!((v.head_server_seq(), v.head_snapshot_sha256()), (0, &sha));
    assert_eq!(v.snapshot().notebooks.len(), 2);
    assert_eq!(v.snapshot().folders.len(), 1);
}

#[test]
fn verify_refuses_another_server() {
    let mut a = Answers::good();
    a.server = new_server_id();
    a.refused(AdoptErrorKind::ServerMismatch);
}

#[test]
fn verify_refuses_another_library_in_any_answer() {
    let mut a = Answers::good();
    a.info.library_id = new_library_id();
    a.refused(AdoptErrorKind::LibraryMismatch);

    let mut a = Answers::good();
    a.doc_library = new_library_id();
    a.refused(AdoptErrorKind::LibraryMismatch);

    // Every hash consistent, but the snapshot itself is of another library.
    let a = Answers::good();
    let (bytes, _) = synthetic_genesis(new_library_id());
    let msg = a.with_bytes(bytes).refused(AdoptErrorKind::LibraryMismatch);
    assert!(msg.contains("genesis snapshot is of library"), "{msg}");
}

#[test]
fn verify_refuses_genesis_hash_disagreements() {
    let mut a = Answers::good();
    a.doc_sha = sha256_hex(b"other");
    a.refused(AdoptErrorKind::GenesisMismatch);

    let mut a = Answers::good();
    a.bytes.push(b' ');
    let msg = a.refused(AdoptErrorKind::GenesisMismatch);
    assert!(msg.contains("genesis bytes hash to"), "{msg}");

    // No operations yet the current snapshot differs from the genesis.
    let mut a = Answers::good();
    a.info.snapshot_sha256 = sha256_hex(b"elsewhere");
    a.refused(AdoptErrorKind::GenesisMismatch);
    // With operations the head legitimately differs.
    let mut a = Answers::good();
    a.info.snapshot_sha256 = sha256_hex(b"elsewhere");
    a.info.head_server_seq = 3;
    assert_eq!(a.verify().unwrap().head_server_seq(), 3);
}

#[test]
fn verify_refuses_corrupt_and_noncanonical_snapshots() {
    let a = Answers::good();
    let value: Value = serde_json::from_slice(&a.bytes).unwrap();
    let pretty = serde_json::to_vec_pretty(&value).unwrap();
    let msg = a
        .with_bytes(pretty)
        .refused(AdoptErrorKind::InvalidSnapshot);
    assert!(msg.contains("canonical"), "{msg}");

    Answers::good()
        .with_bytes(b"{\"v\":1,\"library_id\":".to_vec())
        .refused(AdoptErrorKind::InvalidSnapshot);

    // Structurally invalid: a notebook filed in a folder that does not exist.
    let a = Answers::good();
    let mut value: Value = serde_json::from_slice(&a.bytes).unwrap();
    value["folders"] = json!([]);
    let bytes = serde_json::to_vec(&value).unwrap();
    a.with_bytes(bytes).refused(AdoptErrorKind::InvalidSnapshot);
}

#[test]
fn verify_refuses_missing_blobs_and_empty_libraries() {
    // A blob background the server did not supply.
    let a = Answers::good();
    let mut snap = SyncSnapshotV1::parse(&a.bytes).unwrap();
    let (blob, r) = testkit::blob_background();
    snap.notebooks[0].pages[0].background = r;
    let mut a = a.with_bytes(snap.to_canonical_bytes().unwrap());
    a.blobs = vec![blob[1..].to_vec()];
    let msg = a.refused(AdoptErrorKind::GenesisMismatch);
    assert!(msg.contains("background blob"), "{msg}");

    let a = Answers::good();
    let mut snap = SyncSnapshotV1::parse(&a.bytes).unwrap();
    snap.notebooks.clear();
    snap.folders.clear();
    a.with_bytes(snap.to_canonical_bytes().unwrap())
        .refused(AdoptErrorKind::Unsupported);
}

#[test]
fn sync_state_without_a_server_id_keeps_its_exact_bytes() {
    // Field order as the device writes it.
    let legacy = br#"{"v":2,"library_id":"01900000-0000-7000-8000-000000000001","replica_id":"01900000-0000-7000-8000-000000000002","genesis_sha256":"0000000000000000000000000000000000000000000000000000000000000000","genesis_uploaded":true,"acked_client_seq":4,"next_client_seq":5,"server_cursor":37939}"#;
    let st: SyncStateV1 = serde_json::from_slice(legacy).unwrap();
    assert_eq!(st.server_id, None);
    assert_eq!(
        sync_protocol::to_canonical_json(&st).unwrap(),
        legacy.to_vec()
    );
    let bound = SyncStateV1 {
        server_id: Some(new_server_id()),
        ..st
    };
    let bytes = sync_protocol::to_canonical_json(&bound).unwrap();
    assert_eq!(
        serde_json::from_slice::<SyncStateV1>(&bytes).unwrap(),
        bound
    );
}

// ------------------------------------------------------------------ engine (disposable copies)

#[test]
fn adoption_replaces_library_and_epoch_and_keeps_the_old_ones_whole() {
    let (k, queued, uncaptured) = installation("replace");
    let old = k.state();
    let [old_lib, old_sync, device_before] = trees(&k);
    let src = restored(new_server_id());

    let AdoptionOutcome::Adopted(r) = k.adopt(&src).unwrap() else {
        panic!("not adopted")
    };
    assert_adopted(&k, &src, old.replica_id);
    assert_eq!(
        (r.library_id, r.server_id, r.replica_id),
        (
            src.target().library_id,
            src.target().server_id,
            k.state().replica_id
        )
    );
    assert_eq!(
        (r.previous_library_id, r.previous_replica_id),
        (Some(old.library_id), Some(old.replica_id))
    );
    assert_eq!((r.notebooks, r.pages, r.head_server_seq), (2, 3, 0));

    // Identities are the canonical snapshot's: notebooks, pages, strokes, text, link, folder.
    let snap = k.snapshot();
    let ident = read_identity(&k.lib).unwrap().unwrap();
    assert_eq!(ident.library_id, src.target().library_id);
    let bound: Vec<NotebookId> = ident.notebooks.values().copied().collect();
    for n in &src.snapshot().notebooks {
        assert!(bound.contains(&n.notebook_id));
    }
    assert_eq!(snap.folders[0].folder_id, testkit::fd(1));
    assert_eq!(
        snap.notebooks[1].folder_id,
        Some::<FolderId>(testkit::fd(1))
    );
    assert_eq!(snap.notebooks[1].pages[0].texts[0].text_id, testkit::tx(1));
    assert_eq!(snap.notebooks[1].pages[0].links[0].link_id, testkit::lk(1));
    assert_eq!(
        snap.notebooks[1].pages[0].strokes[0].stroke_id,
        testkit::sk(2)
    );

    // Neither local-only stroke was transferred.
    let pts = all_points(&snap);
    assert!(!pts.contains(&local_points(&queued)));
    assert!(!pts.contains(&local_points(&uncaptured)));

    // The previous installation is kept whole, byte for byte; unrelated data is untouched.
    let (ret_lib, ret_sync) = retired_dirs(&k);
    assert_eq!(
        (r.retired_library.as_ref(), r.retired_sync.as_ref()),
        (Some(&ret_lib), Some(&ret_sync))
    );
    assert_eq!(tree(&ret_lib), old_lib);
    assert_eq!(tree(&ret_sync), old_sync);
    assert_eq!(tree(&k.device_dir), device_before);
    assert_eq!(
        fs::read(k.sync.join(crate::prefs::PREFERENCES_FILE)).unwrap(),
        fs::read(ret_sync.join(crate::prefs::PREFERENCES_FILE)).unwrap(),
        "replica preferences carry over"
    );

    // Ordinary startup accepts it: nothing pending, no fault, sequencing from 1.
    let s = k.open();
    assert_eq!(s.fault(), None);
    assert_eq!((s.pending(), s.next_client_seq()), (0, 1));
    assert_eq!(s.state().library_id, src.target().library_id);
    s.shutdown(Duration::from_secs(5));
}

#[test]
fn repeated_adoption_of_the_same_library_changes_nothing() {
    let (k, _, _) = installation("repeat");
    let src = restored(new_server_id());
    assert!(matches!(
        k.adopt(&src).unwrap(),
        AdoptionOutcome::Adopted(_)
    ));
    let replica = k.state().replica_id;
    let before = trees(&k);
    assert_eq!(
        k.adopt(&src).unwrap(),
        AdoptionOutcome::AlreadyAdopted {
            library_id: src.target().library_id,
            replica_id: replica,
        }
    );
    assert_eq!(trees(&k), before);
    retired_dirs(&k); // still exactly one retired copy

    // The same library ID claimed by another server is refused, not re-adopted.
    let mut other = src.clone();
    other.target.server_id = new_server_id();
    let e = k.adopt(&other).unwrap_err();
    assert_eq!(e.kind, AdoptErrorKind::LibraryMismatch, "{e}");
    assert_eq!(trees(&k), before);
}

#[test]
fn running_sync_makes_adoption_refuse_without_changes() {
    let (k, _, _) = installation("busy");
    let s = k.open();
    let before = trees(&k);
    let e = k.adopt(&restored(new_server_id())).unwrap_err();
    assert_eq!(e.kind, AdoptErrorKind::Busy, "{e}");
    assert_eq!(trees(&k), before);
    let l = k.layout();
    assert!(!l.workspace.exists() && !l.staged_library.exists());
    s.shutdown(Duration::from_secs(5));
}

#[test]
fn a_genesis_this_kindle_cannot_represent_is_refused_and_staging_removed() {
    let (k, _, _) = installation("unsupported");
    let before = trees(&k);
    let mut src = restored(new_server_id());
    // Valid protocol data, but rendered with a profile no Kindle build knows.
    src.snapshot.notebooks[0].pages[0].render.rasterizer_id = "future-v9".into();
    src.genesis_bytes = src.snapshot.to_canonical_bytes().unwrap();
    src.genesis_sha256 = sha256_hex(&src.genesis_bytes);
    let e = k.adopt(&src).unwrap_err();
    assert_eq!(e.kind, AdoptErrorKind::Unsupported, "{e}");
    assert_eq!(trees(&k), before);
    let l = k.layout();
    assert!(!l.workspace.exists() && !l.staged_library.exists());
}

#[test]
fn a_staged_library_that_does_not_reproduce_the_genesis_is_never_published() {
    let (k, _, _) = installation("tampered");
    let before = trees(&k);
    let src = restored(new_server_id());
    let staged = k.layout().staged_library;
    // Something writes into the staged library after materialization.
    let tamper = |p: &str| {
        if p == "after_materialize" {
            let (_, layout) = crate::sync_local::live_layout(&staged).unwrap();
            let (nb, pages) = &layout.notebooks[0];
            write_stroke(None, &staged, nb, &pages[0], 8000.0);
        }
        Ok(())
    };
    let e = k.adopt_with(&src, &tamper).unwrap_err();
    assert_eq!(e.kind, AdoptErrorKind::Local, "{e}");
    assert_eq!(trees(&k), before);
    let l = k.layout();
    assert!(!l.workspace.exists() && !l.staged_library.exists());
}

fn crash_at(point: &'static str) -> impl Fn(&str) -> Result<(), String> {
    move |p: &str| {
        if p == point {
            Err(format!("injected crash at {p}"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn interrupted_preparation_leaves_the_old_installation_usable() {
    for point in [
        "after_library_shell",
        "after_sync_epoch",
        "after_bind_notebook",
        "after_create_notebook",
        "after_materialize",
        "after_verify",
    ] {
        let (k, _, _) = installation("prep");
        let old = k.state();
        let before = trees(&k);
        let src = restored(new_server_id());
        let e = k.adopt_with(&src, &crash_at(point)).unwrap_err();
        assert!(e.message.contains("injected crash"), "{point}: {e}");
        assert_eq!(trees(&k), before, "{point}: live installation touched");
        assert!(!adoption_pending(&k.sync), "{point}");
        // The old installation still opens normally with its local work: the queued stroke,
        // and the uncaptured one that startup reconciliation re-derives as usual.
        let s = k.open();
        assert_eq!((s.fault(), s.pending()), (None, 2), "{point}");
        assert_eq!(s.state().library_id, old.library_id);
        s.shutdown(Duration::from_secs(5));
        // Startup recovery removes the preparation; a new adoption then succeeds.
        assert_eq!(
            recover_interrupted_adoption(&k.lib, &k.sync, None).unwrap(),
            AdoptionRecovery::DiscardedPreparation,
            "{point}"
        );
        let l = k.layout();
        assert!(
            !l.workspace.exists() && !l.staged_library.exists(),
            "{point}"
        );
        assert!(matches!(
            k.adopt(&src).unwrap(),
            AdoptionOutcome::Adopted(_)
        ));
        assert_adopted(&k, &src, old.replica_id);
    }
}

#[test]
fn interrupted_publication_is_completed_at_startup_and_fences_sync_until_then() {
    for point in [
        "after_plan",
        "after_retire_library",
        "after_publish_library",
        "after_retire_sync",
        "after_publish_sync",
        "after_plan_removed",
    ] {
        let (k, _, _) = installation("publish");
        let old = k.state();
        let [old_lib, old_sync, device_before] = trees(&k);
        let src = restored(new_server_id());
        let e = k.adopt_with(&src, &crash_at(point)).unwrap_err();
        assert!(e.message.contains("injected crash"), "{point}: {e}");
        if point != "after_plan_removed" {
            // Committed but unpublished: ordinary sync refuses to start.
            assert!(adoption_pending(&k.sync), "{point}");
            if k.sync.exists() {
                let e = OutboundSync::open(&k.lib, &k.sync).err().unwrap();
                assert!(e.contains("adoption"), "{point}: {e}");
            }
        }
        let want = if point == "after_plan_removed" {
            AdoptionRecovery::DiscardedPreparation
        } else {
            AdoptionRecovery::Completed {
                library_id: src.target().library_id,
                replica_id: read_json::<SyncStateV1>(
                    &[&k.layout().staged_sync, &k.sync]
                        .iter()
                        .map(|d| d.join(STATE_FILE))
                        .find(|p| {
                            p.exists()
                                && read_json::<SyncStateV1>(p).unwrap().library_id
                                    == src.target().library_id
                        })
                        .unwrap(),
                )
                .unwrap()
                .replica_id,
            }
        };
        assert_eq!(
            recover_interrupted_adoption(&k.lib, &k.sync, None).unwrap(),
            want,
            "{point}"
        );
        assert_adopted(&k, &src, old.replica_id);
        let (ret_lib, ret_sync) = retired_dirs(&k);
        assert_eq!(tree(&ret_lib), old_lib, "{point}");
        assert_eq!(tree(&ret_sync), old_sync, "{point}");
        assert_eq!(tree(&k.device_dir), device_before, "{point}");
        let s = k.open();
        assert_eq!((s.fault(), s.pending()), (None, 0), "{point}");
        s.shutdown(Duration::from_secs(5));
        // A second recovery has nothing to do.
        assert_eq!(
            recover_interrupted_adoption(&k.lib, &k.sync, None).unwrap(),
            AdoptionRecovery::None
        );
    }
}

#[test]
fn the_adopted_epoch_refuses_the_previous_replica_identity() {
    let (k, _, _) = installation("replica");
    let old_replica = fs::read(k.sync.join(REPLICA_STATE_FILE)).unwrap();
    let old = k.state().replica_id;
    k.adopt(&restored(new_server_id())).unwrap();
    assert_ne!(k.state().replica_id, old);
    // Reusing the previous replica (and so its client sequences) is refused, not resumed.
    fs::write(k.sync.join(REPLICA_STATE_FILE), old_replica).unwrap();
    let e = OutboundSync::open(&k.lib, &k.sync).err().unwrap();
    assert!(e.contains("replica identity changed"), "{e}");
}

#[test]
fn the_adopted_epoch_only_uses_links_to_its_server() {
    let (k, _, _) = installation("links");
    let server = new_server_id();
    k.adopt(&restored(server)).unwrap();
    let link = |id: Option<ServerId>| SyncConfigV1 {
        v: 1,
        server_base_url: "http://127.0.0.1:9".into(),
        token: "t".into(),
        debug_drop_first_ack: false,
        debug_crash_point: None,
        server_id: id,
    };
    let s = k.open();
    s.set_link(Some(link(Some(new_server_id()))));
    assert_eq!(s.link_endpoint(), None, "another server's link is refused");
    s.set_link(Some(link(None)));
    assert_eq!(s.link_endpoint(), None, "an unidentified link is refused");
    s.set_link(Some(link(Some(server))));
    assert_eq!(s.link_endpoint().as_deref(), Some("http://127.0.0.1:9"));
    s.shutdown(Duration::from_secs(5));
    // A developer config.json names no server: offline capture only.
    write_json(&k.sync, CONFIG_FILE, &link(None)).unwrap();
    let s = k.open();
    assert_eq!(s.link_endpoint(), None);
    s.shutdown(Duration::from_secs(5));
}

// ------------------------------------------------------------------ end-to-end: two servers

/// A real product server (control plane + sync) with an administrator and a paired Kindle.
struct Product {
    state: PathBuf,
    running: Option<scribe_server::Running>,
    admin: String,
    credential: String,
}

fn product_config(state: &Path) -> Config {
    Config {
        server: ServerSection {
            listen: "127.0.0.1:0".parse().unwrap(),
            state_dir: state.to_path_buf(),
            web_root: None,
            name: "Adoption Test".into(),
        },
        discovery: DiscoverySection { advertise: false },
    }
}

fn http(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<(&str, &[u8])>,
) -> (u16, Vec<u8>, Option<String>) {
    let mut req = ureq::request(method, url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let r = match body {
        Some((ctype, b)) => req.set("Content-Type", ctype).send_bytes(b),
        None => req.call(),
    };
    let resp = match r {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("transport: {e}"),
    };
    let status = resp.status();
    let cookie = resp.header("set-cookie").map(str::to_string);
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut out).unwrap();
    (status, out, cookie)
}

fn json_of(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap_or(Value::Null)
}

impl Product {
    fn start(label: &str) -> Self {
        let state = tmp(label);
        let cfg = product_config(&state);
        let running =
            scribe_server::serve(&cfg, scribe_server::system_clock(), Arc::new(|_: &str| {}))
                .unwrap();
        let mut p = Self {
            state,
            running: Some(running),
            admin: String::new(),
            credential: String::new(),
        };
        let (_, _, store) = scribe_server::prepare(&cfg, scribe_server::system_clock()).unwrap();
        let code = store.issue_setup_code().unwrap().unwrap();
        let body = json!({ "setup_code": code }).to_string();
        let (st, _, cookie) = http(
            "POST",
            &format!("{}/control/v1/claim", p.url()),
            &[("X-Scribe-Request", "1")],
            Some(("application/json", body.as_bytes())),
        );
        assert_eq!(st, 200);
        p.admin = cookie.unwrap().split(';').next().unwrap().to_string();
        // Pair a Kindle: request, administrator approval, one-time credential.
        let body = json!({ "client_type": "kindle", "display_name": "Kindle Scribe" }).to_string();
        let (_, b, _) = http(
            "POST",
            &format!("{}/control/v1/pairing", p.url()),
            &[("X-Scribe-Request", "1")],
            Some(("application/json", body.as_bytes())),
        );
        let req = json_of(&b);
        let id = req["request_id"].as_str().unwrap();
        let (st, _) = p.admin_call(
            "POST",
            &format!("/control/v1/admin/pairing/{id}/approve"),
            None,
        );
        assert_eq!(st, 200);
        let body = json!({ "request_secret": req["request_secret"] }).to_string();
        let (_, b, _) = http(
            "POST",
            &format!("{}/control/v1/pairing/{id}/poll", p.url()),
            &[("X-Scribe-Request", "1")],
            Some(("application/json", body.as_bytes())),
        );
        p.credential = json_of(&b)["credential"].as_str().unwrap().to_string();
        p
    }
    fn url(&self) -> String {
        self.running.as_ref().unwrap().handle.base_url()
    }
    fn id(&self) -> ServerId {
        self.running.as_ref().unwrap().plane.server_id
    }
    fn link(&self) -> SyncConfigV1 {
        SyncConfigV1 {
            v: 1,
            server_base_url: self.url(),
            token: self.credential.clone(),
            debug_drop_first_ack: false,
            debug_crash_point: None,
            server_id: Some(self.id()),
        }
    }
    fn admin_call(&self, method: &str, path: &str, body: Option<(&str, &[u8])>) -> (u16, Vec<u8>) {
        let (s, b, _) = http(
            method,
            &format!("{}{path}", self.url()),
            &[("Cookie", &self.admin), ("X-Scribe-Request", "1")],
            body,
        );
        (s, b)
    }
    fn sync_call(&self, method: &str, path: &str, body: Option<&[u8]>) -> (u16, Vec<u8>) {
        let bearer = format!("Bearer {}", self.credential);
        let (s, b, _) = http(
            method,
            &format!("{}{path}", self.url()),
            &[("Authorization", &bearer)],
            body.map(|b| ("application/json", b)),
        );
        (s, b)
    }
    fn library(&self, lib: LibraryId) -> Value {
        let (s, b) = self.sync_call("GET", &format!("/v1/libraries/{lib}"), None);
        assert_eq!(s, 200);
        json_of(&b)
    }
    fn head(&self, lib: LibraryId) -> u64 {
        self.library(lib)["head_server_seq"].as_u64().unwrap()
    }
    fn snapshot(&self, lib: LibraryId) -> SyncSnapshotV1 {
        let (s, b) = self.sync_call("GET", &format!("/v1/libraries/{lib}/snapshot"), None);
        assert_eq!(s, 200);
        serde_json::from_value(json_of(&b)["snapshot"].clone()).unwrap()
    }
    fn library_ids(&self) -> Vec<String> {
        let (s, b) = self.sync_call("GET", "/v1/libraries", None);
        assert_eq!(s, 200);
        let mut v: Vec<String> = json_of(&b)["libraries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["library_id"].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    }
    /// `(server_seq, envelope)` after `after`, the whole tail.
    fn tail(&self, lib: LibraryId, after: u64) -> Vec<(u64, SyncEnvelopeV1)> {
        let (s, b) = self.sync_call(
            "GET",
            &format!("/v1/libraries/{lib}/operations?after={after}&limit=500"),
            None,
        );
        assert_eq!(s, 200);
        json_of(&b)["operations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| {
                (
                    o["server_seq"].as_u64().unwrap(),
                    serde_json::from_value(o["envelope"].clone()).unwrap(),
                )
            })
            .collect()
    }
    fn submit(&self, lib: LibraryId, envs: &[SyncEnvelopeV1]) {
        let mut body = b"{\"envelopes\":[".to_vec();
        for (i, e) in envs.iter().enumerate() {
            if i > 0 {
                body.push(b',');
            }
            body.extend(e.to_canonical_bytes().unwrap());
        }
        body.extend(b"]}");
        let (s, b) = self.sync_call(
            "POST",
            &format!("/v1/libraries/{lib}/operations"),
            Some(&body),
        );
        assert_eq!(s, 200, "{}", String::from_utf8_lossy(&b));
    }
    /// A portable backup through the administrator's streamed export.
    fn export(&self) -> Vec<u8> {
        let (s, b) = self.admin_call("POST", "/control/v1/admin/backup/exports", None);
        assert_eq!(s, 202, "{}", String::from_utf8_lossy(&b));
        let id = json_of(&b)["export_id"].as_str().unwrap().to_string();
        let end = Instant::now() + Duration::from_secs(60);
        loop {
            let (_, b) = self.admin_call(
                "GET",
                &format!("/control/v1/admin/backup/exports/{id}"),
                None,
            );
            let v = json_of(&b);
            if v["state"] != "preparing" {
                assert_eq!(v["state"], "ready", "{v}");
                break;
            }
            assert!(Instant::now() < end, "export still preparing");
            std::thread::sleep(Duration::from_millis(50));
        }
        let (s, zip) = self.admin_call(
            "GET",
            &format!("/control/v1/admin/backup/exports/{id}/download"),
            None,
        );
        assert_eq!(s, 200);
        zip
    }
    /// Upload, review and confirm a restore of every library in `zip` (one expected);
    /// returns `(source library, restored library)`.
    fn restore(&self, zip: &[u8]) -> (LibraryId, LibraryId) {
        let (s, b) = self.admin_call(
            "POST",
            "/control/v1/admin/restore/jobs",
            Some(("application/zip", zip)),
        );
        assert_eq!(s, 201, "{}", String::from_utf8_lossy(&b));
        let up = json_of(&b);
        let job = up["job_id"].as_str().unwrap().to_string();
        let lib = &up["libraries"][0];
        let (source, dest) = (
            lib["source_library_id"].as_str().unwrap().to_string(),
            lib["destination_library_id"].as_str().unwrap().to_string(),
        );
        let body = json!({
            "v": 1,
            "confirmation": up["confirmation"],
            "archive_sha256": up["archive"]["sha256"],
            "destinations": [{ "source_library_id": source, "destination_library_id": dest }],
        })
        .to_string();
        let (s, b) = self.admin_call(
            "POST",
            &format!("/control/v1/admin/restore/jobs/{job}/confirm"),
            Some(("application/json", body.as_bytes())),
        );
        assert_eq!(s, 202, "{}", String::from_utf8_lossy(&b));
        let end = Instant::now() + Duration::from_secs(60);
        loop {
            let (_, b) = self.admin_call(
                "GET",
                &format!("/control/v1/admin/restore/jobs/{job}"),
                None,
            );
            let v = json_of(&b);
            match v["state"].as_str() {
                Some("succeeded") => break,
                Some("failed") => panic!("restore failed: {v}"),
                _ => {}
            }
            assert!(Instant::now() < end, "restore still running");
            std::thread::sleep(Duration::from_millis(50));
        }
        let parse = |s: &str| LibraryId::from_uuid(Uuid::parse_str(s).unwrap()).unwrap();
        (parse(&source), parse(&dest))
    }
}

impl Drop for Product {
    fn drop(&mut self) {
        if let Some(r) = self.running.take() {
            r.handle.shutdown();
        }
        let _ = fs::remove_dir_all(&self.state);
    }
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Run sync until everything authored is ACKed, the bridge consumed, the inbox applied and the
/// cursor at the server head.
fn converge(s: &OutboundSync, srv: &Product) {
    let lib = s.state().library_id;
    let t0 = Instant::now();
    loop {
        assert!(s.fault().is_none(), "fault: {:?}", s.fault());
        if s.inbound_ready() {
            s.apply_inbound().unwrap();
            continue;
        }
        if s.state().acked_client_seq + 1 == s.next_client_seq()
            && s.pending() == 0
            && s.accepted_len() == 0
            && s.inbox_len() == 0
            && s.server_cursor() == srv.head(lib)
        {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "no convergence");
        s.kick();
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn author_on(lib: LibraryId) -> testkit::Author {
    testkit::Author {
        library: lib,
        ..testkit::Author::new(ReplicaId::from_uuid(Uuid::now_v7()).unwrap())
    }
}

#[test]
fn a_kindle_adopts_a_library_restored_onto_another_server_and_syncs_from_client_seq_1() {
    // ---- Server A: the Kindle's library, filled by a remote replica and the Kindle itself.
    let a = Product::start("server-a");
    let k = Kindle::new("e2e");
    let (nb, page) = k.first_page();
    let s = k.open();
    s.set_link(Some(a.link()));
    wait("genesis upload", || s.state().genesis_uploaded);
    let lib_a = s.state().library_id;
    let base = a.snapshot(lib_a);
    let mut remote = author_on(lib_a);
    let envs: Vec<SyncEnvelopeV1> = content_ops(&base)
        .into_iter()
        .map(|o| remote.env(o))
        .collect();
    a.submit(lib_a, &envs);
    converge(&s, &a);
    let synced = write_stroke(Some(&s), &k.lib, &nb, &page, 7000.0);
    converge(&s, &a);
    let source = a.snapshot(lib_a);
    assert_eq!(k.snapshot(), source);
    assert!(all_points(&source).contains(&local_points(&synced)));

    // ---- Portable backup of A, restored onto B (new server ID, new library ID).
    let zip = a.export();
    let b = Product::start("server-b");
    assert_ne!(a.id(), b.id());
    let (from, lib_b) = b.restore(&zip);
    assert_eq!(from, lib_a);
    assert_ne!(lib_b, lib_a);
    let mut rekeyed = source.clone();
    rekeyed.library_id = lib_b;
    assert_eq!(b.snapshot(lib_b), rekeyed);
    assert_eq!(b.head(lib_b), 0);

    // ---- Kindle-local work that will be discarded: queued (offline) and uncaptured.
    s.set_link(None);
    let queued = write_stroke(Some(&s), &k.lib, &nb, &page, 9000.0);
    s.wait_journal_durable(&AtomicBool::new(false)).unwrap();
    assert_eq!(s.pending(), 1);
    let old = s.state();
    let a_head = a.head(lib_a);
    s.shutdown(Duration::from_secs(5));
    let uncaptured = write_stroke(None, &k.lib, &nb, &page, 9500.0);
    let a_libraries = a.library_ids();
    let b_libraries = b.library_ids();

    // ---- Adopt B's library through the network path (status + library + genesis).
    let target = AdoptionTarget {
        server_id: b.id(),
        library_id: lib_b,
    };
    // The wrong server for the selection, and a credential B does not know, are refused first.
    let e = fetch_adoption_source(&a.url(), &a.credential, target).unwrap_err();
    assert_eq!(e.kind, AdoptErrorKind::ServerMismatch, "{e}");
    let e = fetch_adoption_source(&b.url(), &a.credential, target).unwrap_err();
    assert_eq!(e.kind, AdoptErrorKind::Unauthorized, "{e}");
    let src = fetch_adoption_source(&b.url(), &b.credential, target).unwrap();
    assert_eq!(src.head_server_seq(), 0);
    let AdoptionOutcome::Adopted(report) = k.adopt(&src).unwrap() else {
        panic!("not adopted")
    };
    assert_adopted(&k, &src, old.replica_id);
    assert_eq!(
        k.snapshot(),
        b.snapshot(lib_b),
        "the Kindle shows exactly B's library"
    );
    let pts = all_points(&k.snapshot());
    assert!(
        pts.contains(&local_points(&synced)),
        "synced history is part of the restore"
    );
    assert!(!pts.contains(&local_points(&queued)));
    assert!(!pts.contains(&local_points(&uncaptured)));
    let (_, retired_sync) = retired_dirs(&k);
    assert_eq!(
        crate::sync_outbound::parse_journal(&fs::read(retired_sync.join(OUTBOX_FILE)).unwrap())
            .unwrap()
            .committed
            .len(),
        1,
        "the queued operation is retired with the old epoch"
    );

    // ---- Ordinary sync with B: nothing to upload, no new library, client_seq starts at 1.
    let s = k.open();
    assert_eq!((s.fault(), s.pending(), s.next_client_seq()), (None, 0, 1));
    s.set_link(Some(a.link()));
    assert_eq!(
        s.link_endpoint(),
        None,
        "server A's link cannot carry B's epoch"
    );
    s.set_link(Some(b.link()));
    converge(&s, &b);
    assert_eq!(b.library_ids(), b_libraries, "no duplicate library on B");
    assert_eq!(b.head(lib_b), 0);
    let first = write_stroke(Some(&s), &k.lib, &nb_of(&k, 0), &page_of(&k, 0, 0), 4000.0);
    converge(&s, &b);
    let tail = b.tail(lib_b, 0);
    assert_eq!(tail.len(), 1);
    let (seq, env) = &tail[0];
    assert_eq!(*seq, 1, "the next valid server sequence");
    assert_eq!(
        (env.replica_id, env.client_seq, env.library_id),
        (report.replica_id, 1, lib_b)
    );
    assert_eq!(
        (s.state().acked_client_seq, s.state().server_cursor),
        (1, 1),
        "acknowledged and its echo consumed"
    );
    assert!(all_points(&b.snapshot(lib_b)).contains(&local_points(&first)));
    assert_eq!(k.snapshot(), b.snapshot(lib_b));
    s.shutdown(Duration::from_secs(5));

    // ---- Restart: the epoch resumes; the next stroke is client_seq 2 / server_seq 2.
    let s = k.open();
    assert_eq!((s.fault(), s.pending(), s.next_client_seq()), (None, 0, 2));
    s.set_link(Some(b.link()));
    converge(&s, &b);
    write_stroke(Some(&s), &k.lib, &nb_of(&k, 0), &page_of(&k, 0, 0), 4500.0);
    converge(&s, &b);
    let tail = b.tail(lib_b, 1);
    assert_eq!(tail.len(), 1);
    assert_eq!(
        (tail[0].0, tail[0].1.client_seq, tail[0].1.replica_id),
        (2, 2, report.replica_id)
    );
    assert_eq!(k.snapshot(), b.snapshot(lib_b));
    s.shutdown(Duration::from_secs(5));

    // Server A never saw the discarded work; neither server gained a library.
    assert_eq!(a.head(lib_a), a_head);
    assert_eq!(a.library_ids(), a_libraries);
    assert_eq!(b.library_ids(), b_libraries);
}

fn nb_of(k: &Kindle, i: usize) -> String {
    crate::sync_local::live_layout(&k.lib).unwrap().1.notebooks[i]
        .0
        .clone()
}

fn page_of(k: &Kindle, i: usize, j: usize) -> String {
    crate::sync_local::live_layout(&k.lib).unwrap().1.notebooks[i].1[j].clone()
}

#[test]
fn operations_after_the_restore_reach_the_adopted_kindle_by_ordinary_inbound_sync() {
    // Source library on A with history of its own (ops after its genesis).
    let a = Product::start("history-a");
    let library = new_library_id();
    let (genesis, snap) = synthetic_genesis(library);
    let (s0, b0) = a.sync_call("POST", "/v1/libraries", Some(&genesis));
    assert_eq!(s0, 201, "{}", String::from_utf8_lossy(&b0));
    let mut on_a = author_on(library);
    let nb1 = snap.notebooks[0].notebook_id;
    let p1 = snap.notebooks[0].pages[0].page_id;
    a.submit(
        library,
        &[
            on_a.env(SyncOperationV1::RenameNotebook {
                notebook_id: nb1,
                title: "Renamed on A".into(),
            }),
            on_a.env(SyncOperationV1::AddStroke {
                notebook_id: nb1,
                page_id: p1,
                stroke: testkit::line(
                    testkit::sk(20),
                    testkit::sk(20),
                    (500.0, 500.0),
                    (900.0, 900.0),
                    4,
                    true,
                ),
            }),
        ],
    );
    assert_eq!(a.head(library), 2);
    let b = Product::start("history-b");
    let (_, lib_b) = b.restore(&a.export());

    // Operations B accepts after the restore (another client of B).
    let restored_genesis = b.snapshot(lib_b);
    let mut on_b = author_on(lib_b);
    let p_new = PageId::from_uuid(Uuid::now_v7()).unwrap();
    b.submit(
        lib_b,
        &[
            on_b.env(SyncOperationV1::AddStroke {
                notebook_id: nb1,
                page_id: p1,
                stroke: testkit::line(
                    testkit::sk(30),
                    testkit::sk(30),
                    (3000.0, 7000.0),
                    (8000.0, 7000.0),
                    6,
                    true,
                ),
            }),
            on_b.env(SyncOperationV1::CreatePage {
                notebook_id: nb1,
                page: PageSpec {
                    page_id: p_new,
                    background: BackgroundRef::Blank,
                    render: snap.notebooks[0].pages[0].render.clone(),
                },
                after_page_id: Some(p1),
            }),
            on_b.env(SyncOperationV1::AddText {
                notebook_id: nb1,
                page_id: p_new,
                text: testkit::text_obj(40, "Written after the restore"),
            }),
        ],
    );
    assert_eq!(b.head(lib_b), 3);

    // A Kindle with an unrelated library of its own adopts B's library.
    let (k, _, _) = installation("history-kindle");
    let old = k.state();
    let src = fetch_adoption_source(
        &b.url(),
        &b.credential,
        AdoptionTarget {
            server_id: b.id(),
            library_id: lib_b,
        },
    )
    .unwrap();
    assert_eq!(
        src.head_server_seq(),
        3,
        "genesis and head are distinguished"
    );
    assert_ne!(src.head_snapshot_sha256(), src.genesis_sha256());
    assert_eq!(src.snapshot(), &restored_genesis);
    k.adopt(&src).unwrap();
    assert_adopted(&k, &src, old.replica_id);
    assert_eq!(
        k.snapshot(),
        restored_genesis,
        "the epoch starts at the genesis"
    );

    // Ordinary inbound sync applies B's three operations; nothing of A's history is replayed.
    let s = k.open();
    s.set_link(Some(b.link()));
    converge(&s, &b);
    assert_eq!(k.snapshot(), b.snapshot(lib_b));
    assert_eq!(s.server_cursor(), 3);
    let tail = b.tail(lib_b, 0);
    assert_eq!(tail.len(), 3);
    assert!(tail.iter().all(|(_, e)| e.replica_id == on_b.replica));
    assert!(!tail.iter().any(|(_, e)| e.replica_id == on_a.replica));
    assert_eq!(
        k.snapshot().notebooks[0].title,
        "Renamed on A",
        "A's history arrives only inside the restored genesis"
    );

    // The adopted replica's first upload: client_seq 1 at server_seq 4.
    write_stroke(Some(&s), &k.lib, &nb_of(&k, 0), &page_of(&k, 0, 1), 2500.0);
    converge(&s, &b);
    let last = b.tail(lib_b, 3);
    assert_eq!(last.len(), 1);
    assert_eq!(
        (last[0].0, last[0].1.client_seq, last[0].1.replica_id),
        (4, 1, k.state().replica_id)
    );
    assert_eq!(k.snapshot(), b.snapshot(lib_b));
    s.shutdown(Duration::from_secs(5));
}

// ------------------------------------------------------------------ 1H-D2 engine additions

/// The installation's own library, re-keyed onto a new library ID the way a restore does (so
/// its notebooks keep their global IDs in the adopted library).
fn restored_copy_of(k: &Kindle, server: ServerId) -> VerifiedGenesis {
    let library = new_library_id();
    let ident = read_identity(&k.lib).unwrap().unwrap();
    let mut snap = build_snapshot(&k.lib, &ident).unwrap();
    snap.library_id = library;
    let bytes = snap.to_canonical_bytes().unwrap();
    let sha = sha256_hex(&bytes);
    VerifiedGenesis::verify(
        AdoptionTarget {
            server_id: server,
            library_id: library,
        },
        server,
        &info_for(library, &sha, 0),
        library,
        &sha,
        bytes,
        Vec::new(),
    )
    .unwrap()
}

#[test]
fn the_adopted_library_has_no_placeholder_content() {
    let (k, _, _) = installation("no-placeholder");
    let src = restored(new_server_id());
    k.adopt(&src).unwrap();
    assert!(
        !k.lib.join(".notebook-trash").exists(),
        "nothing was ever trashed"
    );
    let ident = read_identity(&k.lib).unwrap().unwrap();
    assert!(ident.retired_notebooks.is_empty() && ident.retired_pages.is_empty());
    let snap = k.snapshot();
    assert_eq!(ident.notebooks.len(), snap.notebooks.len());
    assert_eq!(
        crate::library::open_library_read_only(&k.lib)
            .unwrap()
            .manifest()
            .notebook_ids
            .len(),
        snap.notebooks.len()
    );
}

#[test]
fn import_receipts_follow_their_notebooks_by_global_identity() {
    use crate::nbk_import::{
        classify, ImportReceiptV1, Ledger, ReceiptState, ReceiptsV1, SourceStatus, ORIGIN_MARKER,
    };
    let (k, _, _) = installation("receipts");
    let (old_nb, _) = k.first_page();
    let old_ident = read_identity(&k.lib).unwrap().unwrap();
    let global = old_ident.notebooks[&old_nb];
    let receipt = |origin: &str, state, dest: Option<&str>| ImportReceiptV1 {
        origin_kind: "amazon_nbk".into(),
        origin_id: origin.into(),
        state,
        dest_notebook_id: dest.map(str::to_string),
        title: "Amazon notebook".into(),
        book_id: None,
        started_unix_ms: 1,
        finished_unix_ms: Some(2),
        pages: Some(1),
        strokes: Some(1),
        error: None,
    };
    let ledger = ReceiptsV1 {
        v: 1,
        receipts: [
            receipt("aaaa", ReceiptState::Complete, Some(&old_nb)),
            receipt("bbbb", ReceiptState::Pending, Some("notebook-99")),
        ]
        .into_iter()
        .map(|r| (crate::nbk_import::receipt_key(&r.origin_id), r))
        .collect(),
    };
    let path = Ledger::path_for(&k.lib);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec_pretty(&ledger).unwrap()).unwrap();
    let marker = br#"{"v":1,"origin_kind":"amazon_nbk","origin_id":"aaaa","imported_unix_ms":1}"#;
    fs::write(
        k.lib.join("notebooks").join(&old_nb).join(ORIGIN_MARKER),
        marker,
    )
    .unwrap();

    k.adopt(&restored_copy_of(&k, new_server_id())).unwrap();

    let new_ident = read_identity(&k.lib).unwrap().unwrap();
    let new_local = new_ident
        .notebooks
        .iter()
        .find(|(_, g)| **g == global)
        .map(|(l, _)| l.clone())
        .unwrap();
    let carried = Ledger::open(&k.lib).unwrap();
    let a = carried.get("aaaa").unwrap();
    assert_eq!(
        (a.state, a.dest_notebook_id.as_deref()),
        (ReceiptState::Complete, Some(new_local.as_str()))
    );
    assert_eq!(
        fs::read(k.lib.join("notebooks").join(&new_local).join(ORIGIN_MARKER)).unwrap(),
        marker
    );
    assert!(matches!(
        classify(&carried, "aaaa"),
        SourceStatus::AlreadyImported { .. }
    ));
    // A stale local destination is never carried: unknown here, so cleared; retryable.
    let b = carried.get("bbbb").unwrap();
    assert_eq!(
        (b.state, b.dest_notebook_id.as_ref()),
        (ReceiptState::Failed, None)
    );
}

#[test]
fn a_held_adoption_lock_refuses_adoption_and_recovery_without_touching_staging() {
    let (k, _, _) = installation("locked");
    let before = trees(&k);
    let l = k.layout();
    // An "active preparation": staging on disk, its owner holding the lock.
    fs::create_dir_all(l.staged_library.join("notebooks")).unwrap();
    let held = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&l.lock)
        .unwrap();
    held.lock().unwrap();
    let e = k.adopt(&restored(new_server_id())).unwrap_err();
    assert_eq!(e.kind, AdoptErrorKind::Busy, "{e}");
    let e = recover_interrupted_adoption(&k.lib, &k.sync, None).unwrap_err();
    assert!(e.contains("in progress"), "{e}");
    assert!(
        l.staged_library.exists(),
        "a live preparation is never discarded"
    );
    assert_eq!(trees(&k), before);
    drop(held);
    assert_eq!(
        recover_interrupted_adoption(&k.lib, &k.sync, None).unwrap(),
        AdoptionRecovery::DiscardedPreparation
    );
}

#[test]
fn cancellation_and_low_storage_refuse_before_the_commit_and_remove_staging() {
    let (k, _, _) = installation("cancel");
    let before = trees(&k);
    let src = restored(new_server_id());
    let l = k.layout();
    let clean = |k: &Kindle| {
        assert_eq!(trees(k), before);
        assert!(!l.workspace.exists() && !l.staged_library.exists());
    };

    let no_space = AdoptOptions {
        available_bytes: |_| Some(1 << 20),
        ..AdoptOptions::new(device())
    };
    let e = adopt_library(&k.lib, &k.sync, &src, &no_space).unwrap_err();
    assert_eq!(e.kind, AdoptErrorKind::InsufficientStorage, "{e}");
    clean(&k);

    // Cancelled at each stage before the commit (including inside materialization).
    for point in [
        "after_sync_epoch",
        "after_library_shell",
        "after_create_notebook",
    ] {
        let cancel = AtomicBool::new(false);
        let hook = |p: &str| {
            if p == point {
                cancel.store(true, Ordering::SeqCst);
            }
            Ok(())
        };
        let opts = AdoptOptions {
            hook: &hook,
            cancel: Some(&cancel),
            ..AdoptOptions::new(device())
        };
        let e = adopt_library(&k.lib, &k.sync, &src, &opts).unwrap_err();
        assert_eq!(e.kind, AdoptErrorKind::Cancelled, "{point}: {e}");
        clean(&k);
    }
    // After the commit, a cancel request no longer stops it.
    let cancel = AtomicBool::new(false);
    let hook = |p: &str| {
        if p == "after_plan" {
            cancel.store(true, Ordering::SeqCst);
        }
        Ok(())
    };
    let opts = AdoptOptions {
        hook: &hook,
        cancel: Some(&cancel),
        ..AdoptOptions::new(device())
    };
    assert!(matches!(
        adopt_library(&k.lib, &k.sync, &src, &opts).unwrap(),
        AdoptionOutcome::Adopted(_)
    ));
}

#[path = "restore_flow_tests.rs"]
mod flow;
