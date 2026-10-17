//! Sync Phase S2B host tests: durable outbox, crash reconciliation, deterministic erase IDs,
//! ordering across worker restarts, and outbound replication to a real authenticated S2A
//! server over loopback HTTP (genesis, blobs, batches, lost ACK, offline queue, refusals).

use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kindle_platform::FbGeometry;
use notebook_core::{new_stroke_id, PagePoint, Point, Stroke};
use scribe_sync_server::{generate_token, ServerConfig, ServerHandle, Store, TokenVerifier};
use sync_engine::LibraryState;
use sync_protocol::{snapshot_sha256, LinkId, SyncPageLink, SyncSnapshotV1};
use uuid::Uuid;

use super::*;
use crate::eraser::EraserGestureState;
use crate::ink::InkRequest;
use crate::library::{
    create_notebook_durable, delete_notebook_durable, open_or_create_library,
    rename_notebook_durable, CreateNotebookHooks, DeleteNotebookHooks, RenameNotebookHooks,
};
use crate::notebook::{
    delete_page_durable, insert_page_after_durable, move_page_durable, plan_move_page,
    DeletePageHooks, InsertPageHooks, MovePageDirection, MovePageHooks,
    PersistedNotebookManifestV1,
};
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use crate::profile::RenderProfile;
use crate::store::{
    load_existing_page, load_page_links, page_dir, write_page_links, PageStore,
    PersistedFragmentV2, PersistedPointV1, PersistedReplacementV2,
};
use crate::sync_local::{build_snapshot, read_identity, SYNC_DIR};
use crate::ui_gfx::paper_background;

// ------------------------------------------------------------------ fixtures

static N: AtomicU64 = AtomicU64::new(0);

pub(super) fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!(
        "scribe-s2b-{label}-{n}-{}",
        N.fetch_add(1, AtomicOrdering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&p);
    p
}

pub(super) fn geo() -> FbGeometry {
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

fn profile() -> RenderProfile {
    RenderProfile::sealed_v2_pressure(0x00).unwrap()
}

pub(super) struct Env {
    pub(super) lib: PathBuf,
    pub(super) sync: PathBuf,
}

/// A library (notebook-1/page-1 with one pre-existing stroke) + a separate sync dir.
pub(super) fn env(label: &str) -> Env {
    let base = tmp(label);
    let lib = base.join("library");
    let sync = base.join("sync-state");
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    append_stroke(
        &lib,
        "notebook-1",
        "page-1",
        &[(500.0, 500.0), (4000.0, 4000.0)],
    );
    Env { lib, sync }
}

pub(super) fn nb_dir(lib: &Path, nb: &str) -> PathBuf {
    lib.join("notebooks").join(nb)
}

fn manifest(lib: &Path, nb: &str) -> PersistedNotebookManifestV1 {
    serde_json::from_slice(&fs::read(nb_dir(lib, nb).join("notebook.meta")).unwrap()).unwrap()
}

/// Durable local stroke (the product's append path) — no capture.
pub(super) fn append_stroke(lib: &Path, nb: &str, page: &str, pts: &[(f32, f32)]) -> Stroke {
    let stroke = Stroke {
        id: new_stroke_id(),
        points: pts
            .iter()
            .map(|(x, y)| Point::new(*x, *y).with_pressure(0.5))
            .collect(),
        brush: Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
        finished: true,
    };
    let (mut store, _) = load_existing_page(&nb_dir(lib, nb), page).unwrap();
    store.append_committed_stroke(&stroke).unwrap();
    stroke
}

/// Durable local stroke + post-commit capture (what the runtime does on pen Up).
pub(super) fn write(
    sync: &OutboundSync,
    lib: &Path,
    nb: &str,
    page: &str,
    pts: &[(f32, f32)],
) -> Stroke {
    let s = append_stroke(lib, nb, page, pts);
    sync.stroke_committed(nb, page, &s);
    s
}

/// Local undo of `root`'s lineage (the orch replace path) — no capture.
fn local_undo(lib: &Path, nb: &str, page: &str, root: &str) {
    let (mut store, loaded) = load_existing_page(&nb_dir(lib, nb), page).unwrap();
    let targets: Vec<PersistedReplacementV2> = loaded
        .strokes
        .iter()
        .filter(|s| {
            loaded
                .root_of
                .get(s.id.as_str())
                .map_or(s.id.as_str(), String::as_str)
                == root
        })
        .map(|s| PersistedReplacementV2 {
            target_stroke_id: s.id.as_str().to_string(),
            fragments: Vec::new(),
        })
        .collect();
    assert!(!targets.is_empty());
    store.append_replace_strokes(&targets).unwrap();
}

/// One local same-ID geometry rewrite for every visible fragment in `root`.
fn local_move(lib: &Path, nb: &str, page: &str, root: &str, dx: f32, dy: f32) {
    let (mut store, loaded) = load_existing_page(&nb_dir(lib, nb), page).unwrap();
    let replacements: Vec<PersistedReplacementV2> = loaded
        .strokes
        .iter()
        .filter(|stroke| {
            loaded
                .root_of
                .get(stroke.id.as_str())
                .map_or(stroke.id.as_str(), String::as_str)
                == root
        })
        .map(|stroke| PersistedReplacementV2 {
            target_stroke_id: stroke.id.as_str().to_string(),
            fragments: vec![PersistedFragmentV2 {
                id: stroke.id.as_str().to_string(),
                style: stroke.brush.clone(),
                points: stroke
                    .points
                    .iter()
                    .map(|point| PersistedPointV1 {
                        x: point.x + dx,
                        y: point.y + dy,
                        pressure: point.pressure,
                    })
                    .collect(),
            }],
        })
        .collect();
    assert!(!replacements.is_empty());
    store.append_replace_strokes(&replacements).unwrap();
}

/// Runtime ordering for one selection move: durable prep, one atomic local edit, commit.
fn synced_move(
    sync: &OutboundSync,
    lib: &Path,
    nb: &str,
    page: &str,
    root: &str,
    dx: f32,
    dy: f32,
) {
    let ticket = sync
        .prepare_move_stroke_lineages(nb, page, &[root.to_string()], dx, dy)
        .unwrap();
    local_move(lib, nb, page, root, dx, dy);
    sync.commit(ticket);
}

/// A synced eraser gesture exactly as the runtime drives it: Down + stationary sweep, moves,
/// deterministic fragment IDs, write-ahead prep, durable replace, commit.
pub(super) fn synced_erase(
    sync: &OutboundSync,
    lib: &Path,
    nb: &str,
    page: &str,
    path: &[(f32, f32)],
) {
    let (mut store, loaded) = load_existing_page(&nb_dir(lib, nb), page).unwrap();
    let mut g = EraserGestureState::Idle;
    let p0 = PagePoint::new(path[0].0, path[0].1);
    assert!(g.on_down(p0, &loaded.strokes, &loaded.root_of));
    g.process_to(p0);
    for (x, y) in &path[1..] {
        g.process_to(PagePoint::new(*x, *y));
    }
    let intent = g.intent().expect("canonical sweep path");
    let op = sync.erase_op_id();
    let commit =
        g.on_up_with(&mut |src, i| sync.erase_fragment_local_id(op, nb, page, src, i).unwrap());
    assert!(commit.dirty, "test erase must hit something");
    let t = sync
        .prepare_erase(op, nb, page, &intent.observed_roots, &intent.path)
        .expect("prep durable");
    store.append_replace_strokes(&commit.replacements).unwrap();
    sync.commit(t);
}

pub(super) fn local_snapshot(lib: &Path) -> SyncSnapshotV1 {
    build_snapshot(lib, &read_identity(lib).unwrap().unwrap()).unwrap()
}

pub(super) fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub(super) fn pending_seqs(sync_dir: &Path) -> Vec<u64> {
    parse_journal(&fs::read(sync_dir.join(OUTBOX_FILE)).unwrap())
        .unwrap()
        .committed
        .iter()
        .map(|o| o.seq)
        .collect()
}

pub(super) fn pending_envelopes(sync_dir: &Path) -> Vec<Vec<u8>> {
    parse_journal(&fs::read(sync_dir.join(OUTBOX_FILE)).unwrap())
        .unwrap()
        .committed
        .into_iter()
        .map(|o| o.bytes)
        .collect()
}

/// Genesis + every journaled op through the reducer.
pub(super) fn replay_local(sync_dir: &Path) -> LibraryState {
    let genesis = SyncSnapshotV1::parse(&fs::read(sync_dir.join(GENESIS_FILE)).unwrap()).unwrap();
    let base = LibraryState::genesis(genesis).unwrap();
    let ops = parse_journal(&fs::read(sync_dir.join(OUTBOX_FILE)).unwrap())
        .unwrap()
        .committed;
    apply_all(&base, &ops).unwrap()
}

pub(super) fn write_config(sync_dir: &Path, url: &str, token: &str, drop_ack: bool) {
    write_json(
        sync_dir,
        CONFIG_FILE,
        &SyncConfigV1 {
            v: 1,
            server_base_url: url.to_string(),
            token: token.to_string(),
            debug_drop_first_ack: drop_ack,
            debug_crash_point: None,
        },
    )
    .unwrap();
}

pub(super) fn stop(s: OutboundSync) {
    s.shutdown(Duration::from_secs(3));
}

// ------------------------------------------------------------------ server harness

pub(super) struct Server {
    pub(super) handle: ServerHandle,
    pub(super) token: String,
    _dir: PathBuf,
}

pub(super) fn server(label: &str) -> Server {
    let dir = tmp(&format!("{label}-server"));
    let token = generate_token().unwrap();
    let handle = ServerHandle::start_with(
        "127.0.0.1:0".parse().unwrap(),
        Store::open(&dir).unwrap(),
        ServerConfig {
            auth: Some(TokenVerifier::for_token(&token)),
            web_root: None,
            control: None,
        },
    )
    .unwrap();
    Server {
        handle,
        token,
        _dir: dir,
    }
}

impl Server {
    pub(super) fn url(&self) -> String {
        self.handle.base_url()
    }

    pub(super) fn get(&self, path: &str) -> (u16, serde_json::Value) {
        let r = ureq::get(&format!("{}{path}", self.url()))
            .set("Authorization", &format!("Bearer {}", self.token))
            .call();
        let resp = match r {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("transport: {e}"),
        };
        let status = resp.status();
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut body).unwrap();
        (
            status,
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
        )
    }

    pub(super) fn head_seq(&self, lib: &LibraryId) -> i64 {
        self.get(&format!("/v1/libraries/{lib}")).1["head_server_seq"]
            .as_i64()
            .unwrap()
    }

    pub(super) fn snapshot_hash(&self, lib: &LibraryId) -> String {
        let (s, v) = self.get(&format!("/v1/libraries/{lib}/snapshot"));
        assert_eq!(s, 200, "{v}");
        v["snapshot_sha256"].as_str().unwrap().to_string()
    }
}

// ------------------------------------------------------------------ epoch + refusals

#[test]
fn epoch_genesis_is_current_snapshot_and_state_lives_outside_library() {
    let e = env("epoch");
    let before = local_snapshot_after_identity(&e.lib);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none());
    let st = s.state();
    assert_eq!(
        (st.acked_client_seq, st.next_client_seq, st.server_cursor),
        (0, 1, 0)
    );
    assert!(!st.genesis_uploaded);
    let genesis = fs::read(e.sync.join(GENESIS_FILE)).unwrap();
    assert_eq!(sha256_hex(&genesis), st.genesis_sha256);
    let g = SyncSnapshotV1::parse(&genesis).unwrap();
    assert_eq!(g, before);
    stop(s);
    // ServerBaseState = checkpoint (+ confirmed log); the legacy base-state.json is gone.
    assert!(!e.sync.join(BASE_FILE).exists());
    for f in [
        STATE_FILE,
        GENESIS_FILE,
        "checkpoint/manifest.json",
        confirmed::CONFIRMED_LOG_FILE,
        OUTBOX_FILE,
        "replica.json",
    ] {
        assert!(e.sync.join(f).exists(), "{f}");
        assert!(!e.lib.join(f).exists(), "{f} leaked into the library");
    }
    // Reopen: same epoch, no new genesis.
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert_eq!(s.state().genesis_sha256, st.genesis_sha256);
    stop(s);
}

fn local_snapshot_after_identity(lib: &Path) -> SyncSnapshotV1 {
    crate::sync_local::ensure_sync_identity(lib).unwrap();
    local_snapshot(lib)
}

#[test]
fn sync_dir_inside_library_is_refused() {
    let e = env("inside");
    let err = OutboundSync::open(&e.lib, &e.lib.join("sync"))
        .err()
        .expect("refused");
    assert!(err.contains("inside the library"), "{err}");
}

#[test]
fn sync_dir_of_another_library_is_refused() {
    let a = env("other-a");
    let b = env("other-b");
    stop(OutboundSync::open(&a.lib, &a.sync).unwrap());
    crate::sync_local::ensure_sync_identity(&b.lib).unwrap();
    let err = OutboundSync::open(&b.lib, &a.sync).err().expect("refused");
    assert!(err.contains("belongs to library"), "{err}");
}

#[test]
fn second_process_on_same_sync_dir_is_refused() {
    let e = env("lock");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let err = OutboundSync::open(&e.lib, &e.sync).err().expect("refused");
    assert!(err.contains("in use"), "{err}");
    stop(s);
}

#[test]
fn config_debug_never_shows_token() {
    let c = SyncConfigV1 {
        v: 1,
        server_base_url: "http://h:1".into(),
        token: "f".repeat(64),
        debug_drop_first_ack: false,
        debug_crash_point: None,
    };
    assert!(!format!("{c:?}").contains(&c.token));
    assert_eq!(redact(&format!("x {} y", c.token), &c), "x <redacted> y");
}

// ------------------------------------------------------------------ capture + durability

/// Kindle-authored operation types captured offline; after a worker restart the outbox is
/// byte-identical, contiguous from 1, and genesis + outbox replays to the Kindle's state.
#[test]
fn offline_capture_of_all_operation_types_survives_restart_in_order() {
    let e = env("all-ops");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let nb1 = nb_dir(&e.lib, "notebook-1");

    // AddStroke ×2, EraseRegion (splits the first), DeleteStrokeLineage (undo the second).
    let first = write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1000.0, 8000.0), (9000.0, 8000.0)],
    );
    let second = write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1000.0, 12000.0), (9000.0, 12000.0)],
    );
    synced_erase(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(5000.0, 7000.0), (5000.0, 9000.0)],
    );
    synced_move(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        first.id.as_str(),
        100.0,
        200.0,
    );
    local_undo(&e.lib, "notebook-1", "page-1", second.id.as_str());
    s.lineage_deleted("notebook-1", "page-1", second.id.as_str());

    // CreatePage (insert after page-1), MovePage (page-1 right), DeletePage.
    let mut m = manifest(&e.lib, "notebook-1");
    let c = insert_page_after_durable(&nb1, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    s.page_created(&nb1, "notebook-1", &c.inserted_page_id, Some("page-1"));
    write(
        &s,
        &e.lib,
        "notebook-1",
        &c.inserted_page_id,
        &[(100.0, 100.0), (200.0, 300.0)],
    );
    let (planned, _, to) = plan_move_page(&m, "page-1", MovePageDirection::Right).unwrap();
    let after = to.checked_sub(1).map(|i| planned.page_ids[i].clone());
    let t = s
        .prepare_move_page("notebook-1", "page-1", after.as_deref())
        .unwrap();
    move_page_durable(
        &nb1,
        &mut m,
        "page-1",
        MovePageDirection::Right,
        &MovePageHooks::default(),
    )
    .unwrap();
    s.commit(t);

    // CreateNotebook (nonblank background → blob), RenameNotebook, DeletePage there,
    // DeleteNotebook.
    let g = geo();
    let mut lib = crate::library::open_library_read_only(&e.lib).unwrap();
    let bg: Vec<u8> = (0..paper_background(&g).len())
        .map(|i| (i % 7) as u8 + 3)
        .collect();
    let cn = create_notebook_durable(
        &mut lib,
        &profile(),
        &g,
        &bg,
        Some("Work"),
        &CreateNotebookHooks::default(),
    )
    .unwrap();
    let nb2 = lib.notebook_dir(&cn.notebook_id);
    s.notebook_created(&nb2, &cn.notebook_id, &cn.title, Some("notebook-1"));
    let t = s
        .prepare_rename_notebook(&cn.notebook_id, "Work 2")
        .unwrap();
    rename_notebook_durable(
        &mut lib,
        &cn.notebook_id,
        "Work 2",
        &RenameNotebookHooks::default(),
    )
    .unwrap();
    s.commit(t);
    let mut m2 = manifest(&e.lib, &cn.notebook_id);
    let c2 =
        insert_page_after_durable(&nb2, &mut m2, "page-1", &InsertPageHooks::default()).unwrap();
    s.page_created(&nb2, &cn.notebook_id, &c2.inserted_page_id, Some("page-1"));
    let t = s.prepare_delete_page(&cn.notebook_id, "page-1").unwrap();
    delete_page_durable(&nb2, &mut m2, "page-1", &DeletePageHooks::default()).unwrap();
    s.commit(t);
    // A third notebook, then delete it.
    let cn3 = create_notebook_durable(
        &mut lib,
        &profile(),
        &g,
        &paper_background(&g),
        Some("Scratch"),
        &CreateNotebookHooks::default(),
    )
    .unwrap();
    s.notebook_created(
        &lib.notebook_dir(&cn3.notebook_id),
        &cn3.notebook_id,
        &cn3.title,
        Some(&cn.notebook_id),
    );
    let t = s.prepare_delete_notebook(&cn3.notebook_id).unwrap();
    delete_notebook_durable(&mut lib, &cn3.notebook_id, &DeleteNotebookHooks::default()).unwrap();
    s.commit(t);

    wait_until("journal", Duration::from_secs(5), || s.pending() == 14);
    assert!(s.fault().is_none(), "{:?}", s.fault());
    stop(s);

    let kinds: Vec<&str> = parse_journal(&fs::read(e.sync.join(OUTBOX_FILE)).unwrap())
        .unwrap()
        .committed
        .iter()
        .map(|o| op_kind(&o.envelope.operation))
        .collect();
    assert_eq!(
        kinds,
        [
            "add_stroke",
            "add_stroke",
            "erase_region",
            "move_stroke_lineages",
            "delete_stroke_lineage",
            "create_page",
            "add_stroke",
            "move_page",
            "create_notebook",
            "rename_notebook",
            "create_page",
            "delete_page",
            "create_notebook",
            "delete_notebook",
        ]
    );
    let before = pending_envelopes(&e.sync);
    assert_eq!(pending_seqs(&e.sync), (1..=14).collect::<Vec<_>>());
    // The durable outbox replays to exactly the Kindle's local state (d- IDs converge).
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));

    // Worker restart: reconciliation keeps the exact bytes and order; seq continues at 15.
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(pending_envelopes(&e.sync), before);
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(300.0, 300.0), (600.0, 900.0)],
    );
    wait_until("journal", Duration::from_secs(5), || s.pending() == 15);
    stop(s);
    assert_eq!(pending_seqs(&e.sync), (1..=15).collect::<Vec<_>>());
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
    // A nonblank created background is kept content-addressed in sync state.
    assert!(fs::read_dir(e.sync.join(BLOB_DIR)).unwrap().count() >= 1);
}

#[test]
fn erase_fragments_use_deterministic_ids_matching_the_reducer() {
    let e = env("dids");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1000.0, 8000.0), (9000.0, 8000.0)],
    );
    synced_erase(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(5000.0, 7000.0), (5000.0, 9000.0)],
    );
    wait_until("journal", Duration::from_secs(5), || s.pending() == 2);
    stop(s);
    let (_, loaded) = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1").unwrap();
    let derived: Vec<&str> = loaded
        .strokes
        .iter()
        .map(|s| s.id.as_str())
        .filter(|id| id.starts_with(crate::sync_local::DERIVED_FRAGMENT_PREFIX))
        .collect();
    assert_eq!(derived.len(), 2, "split into two d- fragments");
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

// ------------------------------------------------------------------ crash reconciliation

#[test]
fn lost_post_commit_stroke_and_undo_are_rederived_exactly() {
    let e = env("lost-stroke");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let a = write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (20.0, 20.0)],
    );
    wait_until("journal", Duration::from_secs(5), || s.pending() == 1);
    stop(s);
    // Crash window: durable locally, capture never journaled.
    append_stroke(
        &e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    local_undo(&e.lib, "notebook-1", "page-1", a.id.as_str());
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 3);
    stop(s);
    let kinds: Vec<&str> = parse_journal(&fs::read(e.sync.join(OUTBOX_FILE)).unwrap())
        .unwrap()
        .committed
        .iter()
        .map(|o| op_kind(&o.envelope.operation))
        .collect();
    assert_eq!(kinds, ["add_stroke", "delete_stroke_lineage", "add_stroke"]);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

#[test]
fn lost_page_create_is_rederived_with_a_fresh_id() {
    let e = env("lost-create");
    stop(OutboundSync::open(&e.lib, &e.sync).unwrap());
    let nb1 = nb_dir(&e.lib, "notebook-1");
    let mut m = manifest(&e.lib, "notebook-1");
    insert_page_after_durable(&nb1, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 1);
    stop(s);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

#[test]
fn trailing_prep_is_committed_when_the_local_mutation_landed() {
    let e = env("prep-commit");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let nb1 = nb_dir(&e.lib, "notebook-1");
    let mut m = manifest(&e.lib, "notebook-1");
    insert_page_after_durable(&nb1, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    s.page_created(&nb1, "notebook-1", "page-2", Some("page-1"));
    let t = s
        .prepare_move_page("notebook-1", "page-1", Some("page-2"))
        .unwrap();
    move_page_durable(
        &nb1,
        &mut m,
        "page-1",
        MovePageDirection::Right,
        &MovePageHooks::default(),
    )
    .unwrap();
    drop(t); // crash before the commit record
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 2);
    stop(s);
    assert_eq!(pending_seqs(&e.sync), [1, 2]);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

#[test]
fn trailing_prep_is_aborted_when_the_local_mutation_never_landed() {
    let e = env("prep-abort");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (20.0, 20.0)],
    );
    let t = s.prepare_rename_notebook("notebook-1", "Never").unwrap();
    drop(t); // crash before the local rename
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 1);
    // The aborted client_seq is reused by the next operation.
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    wait_until("journal", Duration::from_secs(5), || s.pending() == 2);
    stop(s);
    assert_eq!(pending_seqs(&e.sync), [1, 2]);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

#[test]
fn trailing_move_prep_is_committed_when_same_id_geometry_landed() {
    let e = env("move-prep-commit");
    let (_, loaded) = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1").unwrap();
    let root = loaded.strokes[0].id.as_str().to_string();
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let ticket = s
        .prepare_move_stroke_lineages(
            "notebook-1",
            "page-1",
            std::slice::from_ref(&root),
            120.0,
            80.0,
        )
        .unwrap();
    local_move(&e.lib, "notebook-1", "page-1", &root, 120.0, 80.0);
    drop(ticket);
    drop(s);

    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 1);
    stop(s);
    let kinds: Vec<&str> = parse_journal(&fs::read(e.sync.join(OUTBOX_FILE)).unwrap())
        .unwrap()
        .committed
        .iter()
        .map(|op| op_kind(&op.envelope.operation))
        .collect();
    assert_eq!(kinds, ["move_stroke_lineages"]);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

#[test]
fn trailing_move_prep_is_aborted_when_same_id_geometry_did_not_land() {
    let e = env("move-prep-abort");
    let (_, loaded) = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1").unwrap();
    let root = loaded.strokes[0].id.as_str().to_string();
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let ticket = s
        .prepare_move_stroke_lineages(
            "notebook-1",
            "page-1",
            std::slice::from_ref(&root),
            120.0,
            80.0,
        )
        .unwrap();
    drop(ticket);
    drop(s);

    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 0);
    stop(s);
    assert!(pending_seqs(&e.sync).is_empty());
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

fn two_page_env(label: &str) -> Env {
    let e = env(label);
    let nb1 = nb_dir(&e.lib, "notebook-1");
    let mut m = manifest(&e.lib, "notebook-1");
    insert_page_after_durable(&nb1, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    e
}

fn page_link_between(lib: &Path) -> SyncPageLink {
    let id = read_identity(lib).unwrap().unwrap();
    let nb = id.notebook_id("notebook-1").unwrap();
    SyncPageLink {
        link_id: LinkId::from_uuid(Uuid::now_v7()).unwrap(),
        source_notebook_id: nb,
        source_page_id: id.page_id("notebook-1", "page-1").unwrap(),
        source_x: 100.0,
        source_y: 200.0,
        target_notebook_id: nb,
        target_page_id: id.page_id("notebook-1", "page-2").unwrap(),
    }
}

fn source_page_dir(lib: &Path) -> PathBuf {
    page_dir(&nb_dir(lib, "notebook-1"), "page-1")
}

#[test]
fn trailing_add_page_link_prep_is_committed_when_the_local_link_landed() {
    let e = two_page_env("link-add-commit");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let link = page_link_between(&e.lib);
    let dir = source_page_dir(&e.lib);
    let t = s
        .prepare_add_page_link("notebook-1", "page-1", &link)
        .unwrap();
    write_page_links(&dir, std::slice::from_ref(&link)).unwrap();
    drop(t);
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 1);
    stop(s);
    let kinds: Vec<&str> = parse_journal(&fs::read(e.sync.join(OUTBOX_FILE)).unwrap())
        .unwrap()
        .committed
        .iter()
        .map(|o| op_kind(&o.envelope.operation))
        .collect();
    assert_eq!(kinds, ["add_page_link"]);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

#[test]
fn trailing_add_page_link_prep_is_aborted_when_the_local_link_never_landed() {
    let e = two_page_env("link-add-abort");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let link = page_link_between(&e.lib);
    let t = s
        .prepare_add_page_link("notebook-1", "page-1", &link)
        .unwrap();
    drop(t);
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 0);
    assert!(load_page_links(&source_page_dir(&e.lib))
        .unwrap()
        .is_empty());
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(30.0, 30.0), (40.0, 40.0)],
    );
    wait_until("journal", Duration::from_secs(5), || s.pending() == 1);
    stop(s);
    assert_eq!(pending_seqs(&e.sync), [1]);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

#[test]
fn trailing_delete_page_link_prep_is_committed_when_the_local_delete_landed() {
    let e = two_page_env("link-del-commit");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let link = page_link_between(&e.lib);
    let dir = source_page_dir(&e.lib);
    let saved = author_add_page_link(Some(&s), "notebook-1", "page-1", &dir, &link, || true)
        .unwrap()
        .unwrap();
    assert_eq!(saved.len(), 1);
    wait_until("add durable", Duration::from_secs(5), || s.pending() == 1);
    let t = s
        .prepare_delete_page_link("notebook-1", "page-1", link.link_id)
        .unwrap();
    write_page_links(&dir, &[]).unwrap();
    drop(t);
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 2);
    stop(s);
    let kinds: Vec<&str> = parse_journal(&fs::read(e.sync.join(OUTBOX_FILE)).unwrap())
        .unwrap()
        .committed
        .iter()
        .map(|o| op_kind(&o.envelope.operation))
        .collect();
    assert_eq!(kinds, ["add_page_link", "delete_page_link"]);
    assert!(load_page_links(&dir).unwrap().is_empty());
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

#[test]
fn trailing_delete_page_link_prep_is_aborted_when_the_local_delete_never_landed() {
    let e = two_page_env("link-del-abort");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let link = page_link_between(&e.lib);
    let dir = source_page_dir(&e.lib);
    author_add_page_link(Some(&s), "notebook-1", "page-1", &dir, &link, || true)
        .unwrap()
        .unwrap();
    wait_until("add durable", Duration::from_secs(5), || s.pending() == 1);
    let t = s
        .prepare_delete_page_link("notebook-1", "page-1", link.link_id)
        .unwrap();
    drop(t);
    drop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 1);
    assert_eq!(load_page_links(&dir).unwrap(), vec![link]);
    stop(s);
    let kinds: Vec<&str> = parse_journal(&fs::read(e.sync.join(OUTBOX_FILE)).unwrap())
        .unwrap()
        .committed
        .iter()
        .map(|o| op_kind(&o.envelope.operation))
        .collect();
    assert_eq!(kinds, ["add_page_link"]);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

#[test]
fn add_page_link_aborts_when_the_target_vanishes_before_the_local_write() {
    let e = two_page_env("link-add-vanish");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let link = page_link_between(&e.lib);
    let dir = source_page_dir(&e.lib);
    let mut checks = 0;
    let saved = author_add_page_link(Some(&s), "notebook-1", "page-1", &dir, &link, || {
        checks += 1;
        checks == 1
    })
    .unwrap();
    assert!(saved.is_none());
    assert_eq!(checks, 2);
    assert!(load_page_links(&dir).unwrap().is_empty());
    assert!(!dir.join("links.jsonl").exists());
    stop(s);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 0);
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(12.0, 12.0), (18.0, 18.0)],
    );
    wait_until("journal", Duration::from_secs(5), || s.pending() == 1);
    stop(s);
    assert_eq!(pending_seqs(&e.sync), [1]);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

#[test]
fn explicit_abort_reuses_the_client_seq() {
    let e = env("abort");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let t = s.prepare_delete_page("notebook-1", "page-1").unwrap();
    s.abort(t); // e.g. REFUSE PAGE DELETE (only page)
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (20.0, 20.0)],
    );
    wait_until("journal", Duration::from_secs(5), || s.pending() == 1);
    stop(s);
    assert_eq!(pending_seqs(&e.sync), [1]);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    stop(s);
}

#[test]
fn torn_outbox_tail_is_discarded() {
    let e = env("torn");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (20.0, 20.0)],
    );
    wait_until("journal", Duration::from_secs(5), || s.pending() == 1);
    stop(s);
    let mut f = OpenOptions::new()
        .append(true)
        .open(e.sync.join(OUTBOX_FILE))
        .unwrap();
    f.write_all(br#"{"v":1,"kind":"op","seq":2,"envel"#)
        .unwrap();
    drop(f);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 1);
    stop(s);
}

#[test]
fn unexplained_divergence_faults_sync_but_never_touches_the_notebook() {
    let e = env("diverge");
    stop(OutboundSync::open(&e.lib, &e.sync).unwrap());
    // A retitle outside any capture is not a crash window the outbox can explain.
    let mut lib = crate::library::open_library_read_only(&e.lib).unwrap();
    rename_notebook_durable(
        &mut lib,
        "notebook-1",
        "Elsewhere",
        &RenameNotebookHooks::default(),
    )
    .unwrap();
    let before = local_snapshot(&e.lib);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().unwrap().contains("diverged"));
    assert!(!s.upload_permitted());
    assert!(
        s.capture_ready(),
        "valid outbox must keep sequence authority"
    );
    // Capture continues while upload is prohibited. The retitle is still underivable, so
    // the fault remains; the new stroke is journaled and the notebook is not rewritten.
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (20.0, 20.0)],
    );
    wait_until("captured", Duration::from_secs(5), || s.pending() == 1);
    let ticket = s.prepare_delete_page("notebook-1", "page-1");
    assert!(
        ticket.is_some(),
        "write-ahead capture must run while faulted"
    );
    s.abort(ticket.unwrap());
    assert!(s.fault().unwrap().contains("diverged"));
    assert!(!s.upload_permitted());
    stop(s);
    assert_ne!(local_snapshot(&e.lib), before);
    assert_eq!(pending_seqs(&e.sync), [1]);
}

#[test]
fn corrupt_outbox_record_faults_sync() {
    let e = env("corrupt");
    stop(OutboundSync::open(&e.lib, &e.sync).unwrap());
    fs::write(e.sync.join(OUTBOX_FILE), b"not json\n").unwrap();
    let before = fs::read(e.sync.join(OUTBOX_FILE)).unwrap();
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().unwrap().contains("corrupt outbox"));
    assert!(!s.capture_ready());
    assert!(!s.upload_permitted());
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (20.0, 20.0)],
    );
    assert!(s.prepare_rename_notebook("notebook-1", "Nope").is_none());
    stop(s);
    assert_eq!(
        fs::read(e.sync.join(OUTBOX_FILE)).unwrap(),
        before,
        "invalid outbox bytes must not be rewritten or appended"
    );
}

// ------------------------------------------------------------------ transport (real server)

fn library_id(sync_dir: &Path) -> LibraryId {
    read_json::<SyncStateV1>(&sync_dir.join(STATE_FILE))
        .unwrap()
        .library_id
}

#[test]
fn uploads_genesis_blobs_and_operations_and_converges_with_the_server() {
    let srv = server("converge");
    let e = env("converge");
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    // Nonblank created notebook: its blob must be PUT before the op that references it.
    let g = geo();
    let mut lib = crate::library::open_library_read_only(&e.lib).unwrap();
    let bg: Vec<u8> = (0..paper_background(&g).len())
        .map(|i| (i % 5) as u8 + 9)
        .collect();
    let cn = create_notebook_durable(
        &mut lib,
        &profile(),
        &g,
        &bg,
        Some("B"),
        &CreateNotebookHooks::default(),
    )
    .unwrap();
    s.notebook_created(
        &lib.notebook_dir(&cn.notebook_id),
        &cn.notebook_id,
        &cn.title,
        Some("notebook-1"),
    );
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1000.0, 8000.0), (9000.0, 8000.0)],
    );
    synced_erase(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(5000.0, 7000.0), (5000.0, 9000.0)],
    );
    write(
        &s,
        &e.lib,
        &cn.notebook_id,
        "page-1",
        &[(50.0, 50.0), (70.0, 90.0)],
    );
    s.kick();
    wait_until("upload", Duration::from_secs(20), || {
        s.pending() == 0 && s.state().acked_client_seq == 4
    });
    assert!(s.fault().is_none(), "{:?}", s.fault());
    let lid = library_id(&e.sync);
    assert_eq!(srv.head_seq(&lid), 4);
    let local_hash = snapshot_sha256(&local_snapshot(&e.lib)).unwrap();
    assert_eq!(
        srv.snapshot_hash(&lid),
        local_hash.as_str(),
        "MATCH expected"
    );
    stop(s);
    // ACK persisted: acked state + base advanced, outbox compacted.
    let st: SyncStateV1 = read_json(&e.sync.join(STATE_FILE)).unwrap();
    assert!(st.genesis_uploaded);
    assert_eq!(st.acked_client_seq, 4);
    assert!(pending_seqs(&e.sync).is_empty());
    // Worker restart continues at client_seq 5 against the same server.
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1.0, 1.0), (2.0, 2.0)],
    );
    wait_until("upload", Duration::from_secs(20), || {
        s.state().acked_client_seq == 5
    });
    stop(s);
    assert_eq!(srv.head_seq(&lid), 5);
    assert_eq!(
        srv.snapshot_hash(&lid),
        snapshot_sha256(&local_snapshot(&e.lib)).unwrap().as_str()
    );
}

#[test]
fn crash_windows_around_ack_persistence_recover_without_resending() {
    let srv = server("ack-crash");
    let e = env("ack-crash");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    for i in 0..4 {
        let y = 100.0 + 100.0 * i as f32;
        write(&s, &e.lib, "notebook-1", "page-1", &[(10.0, y), (20.0, y)]);
    }
    wait_until("journal", Duration::from_secs(5), || s.pending() == 4);
    stop(s);
    let uncompacted = fs::read(e.sync.join(OUTBOX_FILE)).unwrap();
    let state_before_ack = fs::read(e.sync.join(STATE_FILE)).unwrap();
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    wait_until("upload", Duration::from_secs(20), || {
        s.state().acked_client_seq == 4
    });
    stop(s);
    // Crash after base-state.json but before sync-state.json + before compaction.
    let mut stale: SyncStateV1 = serde_json::from_slice(&state_before_ack).unwrap();
    stale.genesis_uploaded = true;
    write_json(&e.sync, STATE_FILE, &stale).unwrap();
    fs::write(e.sync.join(OUTBOX_FILE), &uncompacted).unwrap();
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.state().acked_client_seq, 4);
    assert_eq!(s.pending(), 0);
    std::thread::sleep(Duration::from_millis(300));
    stop(s);
    assert_eq!(srv.head_seq(&library_id(&e.sync)), 4);
    assert!(pending_seqs(&e.sync).is_empty());
}

#[test]
fn lost_ack_resends_identical_bytes_and_the_server_dedups() {
    let srv = server("lost-ack");
    let e = env("lost-ack");
    write_config(&e.sync, &srv.url(), &srv.token, true);
    // Queue offline-style first so the dropped batch carries several ops.
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    for i in 0..3 {
        let y = 1000.0 + 500.0 * i as f32;
        write(
            &s,
            &e.lib,
            "notebook-1",
            "page-1",
            &[(100.0, y), (900.0, y)],
        );
    }
    s.kick();
    wait_until("upload", Duration::from_secs(30), || {
        s.state().acked_client_seq == 3
    });
    let stats = s.stats();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert!(stats.duplicate_acks >= 1, "{stats:?}");
    stop(s);
    let lid = library_id(&e.sync);
    // No duplicate ledger entries: server_seq 1..=3 only.
    assert_eq!(srv.head_seq(&lid), 3);
    assert_eq!(
        srv.snapshot_hash(&lid),
        snapshot_sha256(&local_snapshot(&e.lib)).unwrap().as_str()
    );
}

#[test]
fn offline_queue_survives_restart_then_uploads_in_bounded_batches() {
    let e = env("offline");
    // Nothing listens on this port: every attempt is a transport failure.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    write_config(&e.sync, &dead_url, &"0".repeat(64), false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    for i in 0..120 {
        let x = 100.0 + (i as f32) * 100.0;
        write(
            &s,
            &e.lib,
            "notebook-1",
            "page-1",
            &[(x, 100.0), (x, 400.0)],
        );
    }
    wait_until("journal", Duration::from_secs(10), || s.pending() == 120);
    wait_until("retry", Duration::from_secs(10), || {
        s.stats().transport_errors >= 1
    });
    assert!(s.fault().is_none(), "transport failure must not fault sync");
    let t0 = Instant::now();
    stop(s);
    assert!(t0.elapsed() < Duration::from_secs(3));

    let srv = server("offline");
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert_eq!(s.pending(), 120);
    wait_until("upload", Duration::from_secs(30), || {
        s.state().acked_client_seq == 120
    });
    let stats = s.stats();
    stop(s);
    assert!(stats.batches >= 3, "≤ {MAX_BATCH_OPS} per batch: {stats:?}");
    let lid = library_id(&e.sync);
    assert_eq!(srv.head_seq(&lid), 120);
    assert_eq!(
        srv.snapshot_hash(&lid),
        snapshot_sha256(&local_snapshot(&e.lib)).unwrap().as_str()
    );
}

#[test]
fn wrong_token_stops_sync_and_preserves_the_outbox() {
    let srv = server("badtoken");
    let e = env("badtoken");
    // Queue one op while unconfigured, then start with a wrong token.
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(10.0, 10.0), (20.0, 20.0)],
    );
    wait_until("journal", Duration::from_secs(5), || s.pending() == 1);
    stop(s);
    write_config(&e.sync, &srv.url(), &"a".repeat(64), false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    wait_until("fault", Duration::from_secs(10), || s.fault().is_some());
    let f = s.fault().unwrap();
    assert!(f.contains("401"), "{f}");
    assert!(!f.contains(&"a".repeat(64)), "token must never be logged");
    stop(s);
    assert_eq!(pending_seqs(&e.sync), [1]);
    assert!(
        !read_json::<SyncStateV1>(&e.sync.join(STATE_FILE))
            .unwrap()
            .genesis_uploaded
    );
}

#[test]
fn genesis_conflict_stops_sync() {
    let srv = server("gconflict");
    let e = env("gconflict");
    // Someone else registered this library ID with different content.
    let mut other = local_snapshot_after_identity(&e.lib);
    other.notebooks[0].title = "Not the Kindle's".into();
    let r = ureq::post(&format!("{}/v1/libraries", srv.url()))
        .set("Authorization", &format!("Bearer {}", srv.token))
        .send_bytes(&other.to_canonical_bytes().unwrap())
        .unwrap();
    assert_eq!(r.status(), 201);
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    wait_until("fault", Duration::from_secs(10), || s.fault().is_some());
    assert!(s.fault().unwrap().contains("genesis"), "{:?}", s.fault());
    stop(s);
}

#[test]
fn shutdown_is_bounded_while_a_request_hangs() {
    // Accepts TCP but never answers: the uploader is stuck in a read.
    let hang = TcpListener::bind("127.0.0.1:0").unwrap();
    let e = env("hang");
    write_config(
        &e.sync,
        &format!("http://{}", hang.local_addr().unwrap()),
        &"b".repeat(64),
        false,
    );
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let t0 = Instant::now();
    s.shutdown(Duration::from_millis(500));
    assert!(
        t0.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t0.elapsed()
    );
    drop(hang);
}

// ------------------------------------------------------------------ no contamination

/// Every file under `root` (relative path → bytes).
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                let rel = p
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn sync_never_writes_notebook_files_or_puts_the_token_in_the_library() {
    let srv = server("clean");
    let e = env("clean");
    write_config(&e.sync, &srv.url(), &srv.token, false);
    crate::sync_local::ensure_sync_identity(&e.lib).unwrap();
    let before = tree(&e.lib);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    wait_until("genesis", Duration::from_secs(20), || {
        s.state().genesis_uploaded
    });
    stop(s);
    let after = tree(&e.lib);
    // Opening + uploading touches nothing in the library (identity was already established).
    assert_eq!(before, after);
    for (path, bytes) in &after {
        assert!(
            !bytes
                .windows(srv.token.len())
                .any(|w| w == srv.token.as_bytes()),
            "token leaked into {path}"
        );
        assert!(
            path.starts_with("notebooks/") || path == "library.meta" || path.starts_with(SYNC_DIR),
            "unexpected library file {path}"
        );
    }
}

// ------------------------------------------------------------------ pen-path latency (sync on)

/// Post-commit capture is all the pen path does for sync; time just that call.
fn timed_write(s: &OutboundSync, lib: &Path, i: usize) -> Duration {
    let x = 100.0 + (i as f32) * 50.0;
    let stroke = append_stroke(lib, "notebook-1", "page-1", &[(x, 100.0), (x, 900.0)]);
    let t = Instant::now();
    s.stroke_committed("notebook-1", "page-1", &stroke);
    t.elapsed()
}

#[test]
fn capture_never_waits_for_ack_persistence() {
    // Regression: the uploader held the shared sync mutex across the accepted-bridge append and
    // the sync-state fsync, so a pen Up during an ACK waited for that persistence.
    let srv = server("ackslow");
    let e = env("ackslow");
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    wait_until("genesis", Duration::from_secs(30), || {
        s.state().genesis_uploaded
    });
    s.shared.lock().unwrap().ack_persist_delay = Duration::from_millis(400);
    let mut worst = Duration::ZERO;
    for i in 0..40 {
        worst = worst.max(timed_write(&s, &e.lib, i));
        std::thread::sleep(Duration::from_millis(30)); // handwriting cadence
    }
    assert!(
        s.stats().batches >= 2,
        "ACK persistence must overlap the strokes: {:?}",
        s.stats()
    );
    assert!(
        worst < Duration::from_millis(100),
        "capture waited {worst:?} behind ACK persistence"
    );
    wait_until("all acked", Duration::from_secs(60), || {
        s.state().acked_client_seq == 40
    });
    assert!(s.fault().is_none());
    stop(s);
    let lid = library_id(&e.sync);
    assert_eq!(srv.head_seq(&lid), 40, "no op lost or duplicated");
    assert_eq!(
        srv.snapshot_hash(&lid),
        snapshot_sha256(&local_snapshot(&e.lib)).unwrap().as_str()
    );
}

#[test]
fn a_hung_server_never_blocks_strokes_and_queued_work_survives_exit() {
    // Accepts TCP but never answers: the uploader is stuck inside a request.
    let hang = TcpListener::bind("127.0.0.1:0").unwrap();
    let e = env("hungpen");
    write_config(
        &e.sync,
        &format!("http://{}", hang.local_addr().unwrap()),
        &"c".repeat(64),
        false,
    );
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let mut worst = Duration::ZERO;
    for i in 0..30 {
        worst = worst.max(timed_write(&s, &e.lib, i));
    }
    assert!(
        worst < Duration::from_millis(50),
        "capture waited {worst:?} on a hung server"
    );
    wait_until("journal", Duration::from_secs(10), || s.pending() == 30);
    assert!(s.fault().is_none(), "a slow server is not a sync fault");
    let t0 = Instant::now();
    s.shutdown(Duration::from_millis(500));
    assert!(
        t0.elapsed() < Duration::from_millis(1500),
        "exit bounded: {:?}",
        t0.elapsed()
    );
    drop(hang);

    // Every local stroke is in the notebook; every queued op survived the exit, in order.
    let (_, loaded) = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1").unwrap();
    assert_eq!(loaded.strokes.len(), 31);
    assert_eq!(pending_seqs(&e.sync), (1..=30).collect::<Vec<u64>>());
    assert_eq!(
        replay_local(&e.sync).snapshot,
        local_snapshot(&e.lib),
        "queued work reproduces the notebook exactly"
    );
}
