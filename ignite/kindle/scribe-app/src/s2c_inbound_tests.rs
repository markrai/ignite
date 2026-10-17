//! Sync Phase S2C host tests: inbound materialization + two-replica convergence against the
//! real authenticated S2A server. "Remote replica B" submits canonical envelopes under its own
//! ReplicaId; the Kindle replica A consumes the tail through its durable inbox and the
//! LocalMaterializer (the runtime's safe-point call is `apply_inbound`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kindle_platform::FbGeometry;
use notebook_core::{new_stroke_id, PagePoint, Point, Stroke, ERASER_HIT_RADIUS_CANONICAL};
use scribe_sync_server::{generate_token, ServerConfig, ServerHandle, Store, TokenVerifier};
use sync_engine::LibraryState;
use sync_protocol::{
    snapshot_sha256, BackgroundFormat, LineageReplacement, PageSpec, SnapshotPage, StrokeStyle,
    SyncPoint, SyncStroke, SyncText, TextFace, TextId,
};

use super::inbound::parse_tail;
use super::*;
use crate::eraser::EraserGestureState;
use crate::ink::InkRequest;
use crate::library::open_or_create_library;
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use crate::store::{load_existing_page, load_page_texts, PageStore};
use crate::sync_local::{build_snapshot, local_stroke_id_for_global, read_identity};
use crate::ui_gfx::paper_background;

// ------------------------------------------------------------------ fixtures

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!(
        "scribe-s2c-{label}-{n}-{}",
        N.fetch_add(1, AtomicOrdering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&p);
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

struct Srv {
    h: ServerHandle,
    token: String,
    _dir: PathBuf,
}

impl Srv {
    fn url(&self) -> String {
        self.h.base_url()
    }
    fn call(&self, method: &str, path: &str, body: Option<&[u8]>) -> (u16, Vec<u8>) {
        let req = ureq::request(method, &format!("{}{path}", self.url()))
            .set("Authorization", &format!("Bearer {}", self.token));
        let r = match body {
            Some(b) => req.send_bytes(b),
            None => req.call(),
        };
        let resp = match r {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("transport {e}"),
        };
        let s = resp.status();
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut out).unwrap();
        (s, out)
    }
    fn head(&self, lib: LibraryId) -> u64 {
        let (_, b) = self.call("GET", &format!("/v1/libraries/{lib}"), None);
        serde_json::from_slice::<serde_json::Value>(&b).unwrap()["head_server_seq"]
            .as_u64()
            .unwrap()
    }
    fn snapshot(&self, lib: LibraryId) -> (String, SyncSnapshotV1) {
        let (s, b) = self.call("GET", &format!("/v1/libraries/{lib}/snapshot"), None);
        assert_eq!(s, 200);
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        (
            v["snapshot_sha256"].as_str().unwrap().to_string(),
            serde_json::from_value(v["snapshot"].clone()).unwrap(),
        )
    }
}

fn server() -> Srv {
    let dir = tmp("server");
    let token = generate_token().unwrap();
    let store = Store::open(&dir).unwrap();
    // S2C tests are the upgraded-client world: the disposable server accepts replacements.
    store
        .accept_replace_stroke_lineages
        .store(true, AtomicOrdering::SeqCst);
    let h = ServerHandle::start_with(
        "127.0.0.1:0".parse().unwrap(),
        store,
        ServerConfig {
            auth: Some(TokenVerifier::for_token(&token)),
            web_root: None,
            control: None,
        },
    )
    .unwrap();
    Srv {
        h,
        token,
        _dir: dir,
    }
}

struct Kindle {
    lib: PathBuf,
    sync: PathBuf,
}

fn config(k: &Kindle, url: &str, token: &str) {
    write_json(
        &k.sync,
        CONFIG_FILE,
        &SyncConfigV1 {
            v: 1,
            server_base_url: url.to_string(),
            token: token.to_string(),
            debug_drop_first_ack: false,
            debug_crash_point: None,
        },
    )
    .unwrap();
}

fn kindle(srv: &Srv) -> (Kindle, OutboundSync) {
    let base = tmp("kindle");
    let k = Kindle {
        lib: base.join("library"),
        sync: base.join("sync"),
    };
    let g = geo();
    open_or_create_library(&k.lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    fs::create_dir_all(&k.sync).unwrap();
    config(&k, &srv.url(), &srv.token);
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    wait("genesis", || s.state().genesis_uploaded);
    (k, s)
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

fn local_snapshot(lib: &Path) -> SyncSnapshotV1 {
    build_snapshot(lib, &read_identity(lib).unwrap().unwrap()).unwrap()
}

fn local_hash(lib: &Path) -> String {
    snapshot_sha256(&local_snapshot(lib)).unwrap().to_string()
}

/// Run the Kindle until: outbox empty, bridge consumed, inbox applied, cursor == server head.
fn converge(s: &OutboundSync, srv: &Srv) {
    let lib = s.state().library_id;
    let t0 = Instant::now();
    loop {
        assert!(s.fault().is_none(), "fault: {:?}", s.fault());
        if s.inbound_ready() {
            s.apply_inbound().unwrap();
            continue;
        }
        // Every authored op ACKed (the journal thread may still hold a fresh capture, so use
        // client_seq rather than the in-memory pending count), bridge consumed, cursor at head.
        if s.state().acked_client_seq + 1 == s.next_client_seq()
            && s.pending() == 0
            && s.accepted_len() == 0
            && s.inbox_len() == 0
            && s.server_cursor() == srv.head(lib)
        {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(90), "no convergence");
        s.kick();
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn assert_match(k: &Kindle, s: &OutboundSync, srv: &Srv) {
    let (h, snap) = srv.snapshot(s.state().library_id);
    assert_eq!(
        local_snapshot(&k.lib),
        snap,
        "Kindle snapshot != server snapshot"
    );
    assert_eq!(local_hash(&k.lib), h);
}

/// The second replica ("B"): authors canonical envelopes under its own ReplicaId.
struct Remote {
    lib: LibraryId,
    replica: ReplicaId,
    next: u64,
}

impl Remote {
    fn new(lib: LibraryId) -> Self {
        Self {
            lib,
            replica: ReplicaId::from_uuid(Uuid::now_v7()).unwrap(),
            next: 1,
        }
    }
    fn env(&mut self, op: SyncOperationV1) -> SyncEnvelopeV1 {
        let e = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: OperationId::from_uuid(Uuid::now_v7()).unwrap(),
            library_id: self.lib,
            replica_id: self.replica,
            client_seq: self.next,
            operation: op,
        };
        self.next += 1;
        e
    }
    fn submit(&mut self, srv: &Srv, ops: Vec<SyncOperationV1>) {
        let envs: Vec<SyncEnvelopeV1> = ops.into_iter().map(|o| self.env(o)).collect();
        for chunk in envs.chunks(100) {
            let mut body = b"{\"envelopes\":[".to_vec();
            for (i, e) in chunk.iter().enumerate() {
                if i > 0 {
                    body.push(b',');
                }
                body.extend(e.to_canonical_bytes().unwrap());
            }
            body.extend(b"]}");
            let (st, b) = srv.call(
                "POST",
                &format!("/v1/libraries/{}/operations", self.lib),
                Some(&body),
            );
            assert_eq!(st, 200, "{}", String::from_utf8_lossy(&b));
        }
    }
}

fn line(x0: f32, y: f32, len: f32, p: f32) -> Vec<SyncPoint> {
    (0..12)
        .map(|i| SyncPoint::new(x0 + len * i as f32 / 11.0, y, Some(p)))
        .collect()
}

fn remote_stroke(points: Vec<SyncPoint>) -> SyncStroke {
    let id = StrokeId::from_uuid(Uuid::now_v7()).unwrap();
    SyncStroke {
        stroke_id: id,
        root_id: id,
        style: StrokeStyle::PressureWidthV1,
        points,
    }
}

fn fragment_of(root: StrokeId, y: f32) -> SyncStroke {
    let id = StrokeId::from_uuid(Uuid::now_v7()).unwrap();
    SyncStroke {
        stroke_id: id,
        root_id: root,
        style: StrokeStyle::PressureWidthV1,
        points: line(200.0, y, 400.0, 0.4),
    }
}

fn replace_one(
    nb: NotebookId,
    pg: PageId,
    expected: &SyncStroke,
    after: Vec<SyncStroke>,
) -> SyncOperationV1 {
    SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb,
        page_id: pg,
        replacements: vec![LineageReplacement {
            root_id: expected.root_id,
            expected: vec![expected.clone()],
            after,
        }],
    }
}

fn add(nb: NotebookId, pg: PageId, s: &SyncStroke) -> SyncOperationV1 {
    SyncOperationV1::AddStroke {
        notebook_id: nb,
        page_id: pg,
        stroke: s.clone(),
    }
}

fn erase_vertical(
    nb: NotebookId,
    pg: PageId,
    roots: Vec<StrokeId>,
    x: f32,
    y0: f32,
    y1: f32,
) -> SyncOperationV1 {
    let mut roots = roots;
    roots.sort();
    SyncOperationV1::EraseRegion {
        notebook_id: nb,
        page_id: pg,
        observed_roots: roots,
        path: vec![
            SyncPoint::new(x, y0, None),
            SyncPoint::new(x, y0, None),
            SyncPoint::new(x, y1, None),
        ],
        radius: ERASER_HIT_RADIUS_CANONICAL,
    }
}

fn first_page(srv: &Srv, lib: LibraryId) -> (NotebookId, SnapshotPage) {
    let (_, s) = srv.snapshot(lib);
    (s.notebooks[0].notebook_id, s.notebooks[0].pages[0].clone())
}

fn blank_spec(like: &SnapshotPage) -> PageSpec {
    PageSpec {
        page_id: PageId::from_uuid(Uuid::now_v7()).unwrap(),
        background: BackgroundRef::Blank,
        render: like.render.clone(),
    }
}

/// Kindle-local authoring exactly as the runtime does it (durable, then post-commit capture).
fn kindle_write(s: &OutboundSync, k: &Kindle, nb: &str, page: &str, y: f32) -> Stroke {
    let stroke = Stroke {
        id: new_stroke_id(),
        points: (0..12)
            .map(|i| Point::new(1000.0 + 700.0 * i as f32, y).with_pressure(0.6))
            .collect(),
        brush: Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
        finished: true,
    };
    let (mut store, _) = load_existing_page(&k.lib.join("notebooks").join(nb), page).unwrap();
    store.append_committed_stroke(&stroke).unwrap();
    s.stroke_committed(nb, page, &stroke);
    stroke
}

/// Kindle-local synced erase along a vertical sweep (runtime path: deterministic IDs + prep).
fn kindle_erase(s: &OutboundSync, k: &Kindle, nb: &str, page: &str, x: f32, y0: f32, y1: f32) {
    let dir = k.lib.join("notebooks").join(nb);
    let (mut store, loaded) = load_existing_page(&dir, page).unwrap();
    let mut g = EraserGestureState::Idle;
    let p0 = PagePoint::new(x, y0);
    assert!(g.on_down(p0, &loaded.strokes, &loaded.root_of));
    g.process_to(p0);
    g.process_to(PagePoint::new(x, y1));
    let intent = g.intent().unwrap();
    let op = s.erase_op_id();
    let commit =
        g.on_up_with(&mut |src, i| s.erase_fragment_local_id(op, nb, page, src, i).unwrap());
    assert!(commit.dirty);
    let t = s
        .prepare_erase(op, nb, page, &intent.observed_roots, &intent.path)
        .unwrap();
    store.append_replace_strokes(&commit.replacements).unwrap();
    s.commit(t);
}

fn page_locals(lib: &Path) -> BTreeMap<String, Vec<String>> {
    let (_, layout) = crate::sync_local::live_layout(lib).unwrap();
    layout.notebooks.into_iter().collect()
}

// ------------------------------------------------------------------ tail validation

fn tail_body(lib: LibraryId, items: &[(u64, &SyncEnvelopeV1)]) -> Vec<u8> {
    let ops: Vec<String> = items
        .iter()
        .map(|(n, e)| {
            format!(
                "{{\"server_seq\":{n},\"envelope\":{}}}",
                String::from_utf8(e.to_canonical_bytes().unwrap()).unwrap()
            )
        })
        .collect();
    let head = items.last().map_or(0, |(n, _)| *n);
    format!(
        "{{\"library_id\":\"{lib}\",\"head_server_seq\":{head},\"operations\":[{}],\"next_after\":{head}}}",
        ops.join(",")
    )
    .into_bytes()
}

#[test]
fn tail_is_refused_on_gap_duplicate_foreign_library_or_non_canonical_bytes() {
    let lib = LibraryId::from_uuid(Uuid::now_v7()).unwrap();
    let mut r = Remote::new(lib);
    let nb = NotebookId::from_uuid(Uuid::now_v7()).unwrap();
    let pg = PageId::from_uuid(Uuid::now_v7()).unwrap();
    let es: Vec<SyncEnvelopeV1> = (0..4)
        .map(|_| r.env(add(nb, pg, &remote_stroke(line(10.0, 10.0, 50.0, 0.5)))))
        .collect();
    // 41, 42, 43 ok
    let (_, items) = parse_tail(
        &tail_body(lib, &[(41, &es[0]), (42, &es[1]), (43, &es[2])]),
        lib,
        40,
    )
    .unwrap();
    assert_eq!(
        items.iter().map(|i| i.server_seq).collect::<Vec<_>>(),
        [41, 42, 43]
    );
    // 41, 42, 44 → gap refused (never skip to 44)
    let e = parse_tail(
        &tail_body(lib, &[(41, &es[0]), (42, &es[1]), (44, &es[2])]),
        lib,
        40,
    )
    .unwrap_err();
    assert!(e.contains("gap"), "{e}");
    // duplicate
    assert!(parse_tail(&tail_body(lib, &[(41, &es[0]), (41, &es[1])]), lib, 40).is_err());
    // starts after the cursor+1
    assert!(parse_tail(&tail_body(lib, &[(42, &es[0])]), lib, 40).is_err());
    // foreign library
    let other = LibraryId::from_uuid(Uuid::now_v7()).unwrap();
    assert!(parse_tail(&tail_body(lib, &[(41, &es[0])]), other, 40).is_err());
    // non-canonical envelope bytes (extra whitespace)
    let text = String::from_utf8(es[0].to_canonical_bytes().unwrap())
        .unwrap()
        .replacen(':', ": ", 1);
    let body = format!(
        "{{\"library_id\":\"{lib}\",\"head_server_seq\":41,\"operations\":[{{\"server_seq\":41,\"envelope\":{text}}}],\"next_after\":41}}"
    );
    assert!(parse_tail(body.as_bytes(), lib, 40).is_err());
}

// ------------------------------------------------------------------ remote ink

#[test]
fn remote_add_stroke_materializes_exactly_with_no_echo() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let n0 = s.next_client_seq();
    let (nb, p0) = first_page(&srv, lib);
    let stroke = remote_stroke(line(1000.0, 5000.0, 9000.0, 0.8));
    Remote::new(lib).submit(&srv, vec![add(nb, p0.page_id, &stroke)]);
    converge(&s, &srv);
    // Exact stroke under the local id that round-trips to its global id.
    let (_, loaded) = load_existing_page(&k.lib.join("notebooks/notebook-1"), "page-1").unwrap();
    let local = loaded.strokes.last().unwrap();
    assert_eq!(
        local.id.as_str(),
        local_stroke_id_for_global(stroke.stroke_id)
    );
    assert_eq!(local.brush.as_deref(), Some(STYLE_PRESSURE_WIDTH_V1));
    let pts: Vec<SyncPoint> = local
        .points
        .iter()
        .map(|p| SyncPoint::new(p.x, p.y, p.pressure))
        .collect();
    assert_eq!(pts, stroke.points);
    // No echo: nothing authored, nothing uploaded.
    assert_eq!(s.next_client_seq(), n0);
    assert_eq!(s.state().acked_client_seq, 0);
    assert_eq!(srv.head(lib), 1);
    assert_eq!(s.server_cursor(), 1);
    assert_match(&k, &s, &srv);
}

#[test]
fn remote_move_rewrites_geometry_preserves_ids_and_emits_no_echo() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let n0 = s.next_client_seq();
    let (nb, p0) = first_page(&srv, lib);
    let stroke = remote_stroke(line(1000.0, 5000.0, 9000.0, 0.8));
    let mut remote = Remote::new(lib);
    remote.submit(&srv, vec![add(nb, p0.page_id, &stroke)]);
    converge(&s, &srv);

    let page_dir = k.lib.join("notebooks/notebook-1");
    let (_, before) = load_existing_page(&page_dir, "page-1").unwrap();
    let local_id = local_stroke_id_for_global(stroke.stroke_id);
    let local_before = before
        .strokes
        .iter()
        .find(|candidate| candidate.id.as_str() == local_id)
        .unwrap();
    let before_points = local_before.points.clone();

    remote.submit(
        &srv,
        vec![SyncOperationV1::MoveStrokeLineages {
            notebook_id: nb,
            page_id: p0.page_id,
            root_ids: vec![stroke.root_id],
            dx: 250.0,
            dy: 400.0,
        }],
    );
    converge(&s, &srv);

    // Reopen from journals: movement survived reconstruction and retained the same fragment
    // and lineage identity. Remote materialization did not allocate an outbound operation.
    let (_, after) = load_existing_page(&page_dir, "page-1").unwrap();
    let local_after = after
        .strokes
        .iter()
        .find(|candidate| candidate.id.as_str() == local_id)
        .unwrap();
    assert_eq!(local_after.id, local_before.id);
    assert_eq!(after.root_of.get(&local_id), before.root_of.get(&local_id));
    for (old, new) in before_points.iter().zip(&local_after.points) {
        assert_eq!((new.x, new.y), (old.x + 250.0, old.y + 400.0));
    }
    assert_eq!(s.next_client_seq(), n0);
    assert_eq!(s.state().acked_client_seq, 0);
    assert_eq!(srv.head(lib), 2);
    assert_match(&k, &s, &srv);
}

#[test]
fn remote_text_add_update_delete_materializes_into_texts_jsonl() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let n0 = s.next_client_seq();
    let (nb, p0) = first_page(&srv, lib);
    let page = k.lib.join("notebooks/notebook-1/page-1");
    let mut text = SyncText {
        text_id: TextId::from_uuid(Uuid::now_v7()).unwrap(),
        x: 120.0,
        y: 400.0,
        width: 5000.0,
        font_size: 340.0,
        face: TextFace::DefaultSansV1,
        content: "alpha line".into(),
    };
    let mut b = Remote::new(lib);
    b.submit(
        &srv,
        vec![SyncOperationV1::AddText {
            notebook_id: nb,
            page_id: p0.page_id,
            text: text.clone(),
        }],
    );
    converge(&s, &srv);
    assert_eq!(load_page_texts(&page).unwrap(), vec![text.clone()]);
    assert_eq!(
        local_snapshot(&k.lib).notebooks[0].pages[0].texts,
        vec![text.clone()]
    );
    assert_eq!(s.next_client_seq(), n0);
    assert_match(&k, &s, &srv);

    text.x = 800.0;
    text.y = 1200.0;
    text.width = 4200.0;
    text.content = "bravo moved".into();
    b.submit(
        &srv,
        vec![SyncOperationV1::UpdateText {
            notebook_id: nb,
            page_id: p0.page_id,
            text: text.clone(),
        }],
    );
    converge(&s, &srv);
    assert_eq!(load_page_texts(&page).unwrap(), vec![text.clone()]);
    assert_eq!(
        local_snapshot(&k.lib).notebooks[0].pages[0].texts,
        vec![text.clone()]
    );
    assert_match(&k, &s, &srv);

    b.submit(
        &srv,
        vec![SyncOperationV1::DeleteText {
            notebook_id: nb,
            page_id: p0.page_id,
            text_id: text.text_id,
        }],
    );
    converge(&s, &srv);
    assert!(load_page_texts(&page).unwrap().is_empty());
    assert!(local_snapshot(&k.lib).notebooks[0].pages[0]
        .texts
        .is_empty());
    assert_match(&k, &s, &srv);
}

#[test]
fn remote_erase_fragments_match_the_reducer_bit_for_bit() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    let mut b = Remote::new(lib);
    let x = remote_stroke(line(1000.0, 6000.0, 9000.0, 0.7));
    b.submit(&srv, vec![add(nb, p0.page_id, &x)]);
    converge(&s, &srv);
    b.submit(
        &srv,
        vec![erase_vertical(
            nb,
            p0.page_id,
            vec![x.stroke_id],
            5500.0,
            5000.0,
            7000.0,
        )],
    );
    converge(&s, &srv);
    let (_, snap) = srv.snapshot(lib);
    let frags: Vec<&SyncStroke> = snap.notebooks[0].pages[0]
        .strokes
        .iter()
        .filter(|s| s.root_id == x.stroke_id)
        .collect();
    assert_eq!(frags.len(), 2, "split into two fragments");
    let (_, loaded) = load_existing_page(&k.lib.join("notebooks/notebook-1"), "page-1").unwrap();
    let ids: Vec<&str> = loaded.strokes.iter().map(|s| s.id.as_str()).collect();
    for f in &frags {
        assert!(ids.contains(&local_stroke_id_for_global(f.stroke_id).as_str()));
    }
    assert_match(&k, &s, &srv);

    // Added and erased before the Kindle ever saw the root (phantom-root path).
    let y = remote_stroke(line(1000.0, 9000.0, 9000.0, 0.4));
    b.submit(
        &srv,
        vec![
            add(nb, p0.page_id, &y),
            erase_vertical(nb, p0.page_id, vec![y.stroke_id], 4000.0, 8000.0, 10000.0),
        ],
    );
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    assert_eq!(s.next_client_seq(), 1);
}

#[test]
fn remote_delete_lineage_removes_all_descendants() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    let mut b = Remote::new(lib);
    let x = remote_stroke(line(1000.0, 6000.0, 9000.0, 0.7));
    b.submit(
        &srv,
        vec![
            add(nb, p0.page_id, &x),
            erase_vertical(nb, p0.page_id, vec![x.stroke_id], 5500.0, 5000.0, 7000.0),
        ],
    );
    converge(&s, &srv);
    b.submit(
        &srv,
        vec![SyncOperationV1::DeleteStrokeLineage {
            notebook_id: nb,
            page_id: p0.page_id,
            root_id: x.stroke_id,
        }],
    );
    converge(&s, &srv);
    let (_, snap) = srv.snapshot(lib);
    assert!(snap.notebooks[0].pages[0].strokes.is_empty());
    assert_match(&k, &s, &srv);
    assert_eq!(s.next_client_seq(), 1);
}

// ------------------------------------------------------------------ structure

#[test]
fn remote_structural_operations_bind_identities_and_keep_local_ordinals() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb1, p1) = first_page(&srv, lib);
    let mut b = Remote::new(lib);
    // A nonblank blob page (verified + fetched by the Kindle before staging).
    let g = geo();
    // A Kindle-produced background: chrome band already scrubbed (as every local page is).
    let mut bytes: Vec<u8> = (0..(g.line_length * g.yres) as usize)
        .map(|i| (i % 97) as u8 + 60)
        .collect();
    crate::page_label::scrub_chrome_band(&mut bytes, &g).unwrap();
    let sha = sha256_hex(&bytes);
    assert!(srv.call("PUT", &format!("/v1/blobs/{sha}"), Some(&bytes)).0 < 300);
    let blob_page = PageSpec {
        page_id: PageId::from_uuid(Uuid::now_v7()).unwrap(),
        background: BackgroundRef::Blob {
            sha256: sha.clone(),
            byte_len: bytes.len() as u64,
            format: BackgroundFormat {
                encoding: sync_protocol::BackgroundEncoding::Gray8,
                width: g.xres,
                height: g.yres,
                stride: g.line_length,
            },
        },
        render: p1.render.clone(),
    };
    let p2 = blank_spec(&p1);
    let p3 = blank_spec(&p1);
    let nb2 = NotebookId::from_uuid(Uuid::now_v7()).unwrap();
    let nb2_first = blank_spec(&p1);
    let nb3 = NotebookId::from_uuid(Uuid::now_v7()).unwrap();
    b.submit(
        &srv,
        vec![
            SyncOperationV1::RenameNotebook {
                notebook_id: nb1,
                title: "Remote Title".into(),
            },
            SyncOperationV1::CreatePage {
                notebook_id: nb1,
                page: blob_page.clone(),
                after_page_id: Some(p1.page_id),
            },
            SyncOperationV1::CreatePage {
                notebook_id: nb1,
                page: p2.clone(),
                after_page_id: None, // placed first
            },
            SyncOperationV1::CreatePage {
                notebook_id: nb1,
                page: p3.clone(),
                after_page_id: Some(blob_page.page_id),
            },
            SyncOperationV1::MovePage {
                notebook_id: nb1,
                page_id: p3.page_id,
                after_page_id: Some(p2.page_id),
            },
            SyncOperationV1::CreateNotebook {
                notebook_id: nb2,
                title: "Second".into(),
                after_notebook_id: None, // before notebook 1
                first_page: nb2_first.clone(),
            },
            SyncOperationV1::CreateNotebook {
                notebook_id: nb3,
                title: "Third".into(),
                after_notebook_id: Some(nb1),
                first_page: blank_spec(&p1),
            },
        ],
    );
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    // Deletes of materialized entities (quarantine paths, bindings retired).
    b.submit(
        &srv,
        vec![
            SyncOperationV1::DeletePage {
                notebook_id: nb1,
                page_id: p2.page_id,
            },
            SyncOperationV1::DeleteNotebook { notebook_id: nb3 },
        ],
    );
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    let ident = read_identity(&k.lib).unwrap().unwrap();
    // Fresh monotonic local ordinals; nb3 got notebook-3 then was quarantined (never reused).
    assert_eq!(ident.notebook_id("notebook-2"), Some(nb2));
    assert!(
        ident.notebook_id("notebook-3").is_none(),
        "deleted notebook binding retired"
    );
    assert!(ident.retired_notebooks.contains_key(&nb3));
    let repo = crate::library::open_library_read_only(&k.lib).unwrap();
    assert_eq!(repo.manifest().notebook_ids, ["notebook-2", "notebook-1"]);
    assert_eq!(repo.manifest().next_notebook_ordinal, 4);
    assert_eq!(repo.title("notebook-1").unwrap(), "Remote Title");
    assert!(k.lib.join(crate::library::NOTEBOOK_TRASH_DIR).exists());
    // Pages: canonical order by identity; p2 deleted into .page-trash.
    let locals = page_locals(&k.lib);
    let nb1_pages: Vec<PageId> = locals["notebook-1"]
        .iter()
        .map(|l| ident.page_id("notebook-1", l).unwrap())
        .collect();
    // [p2, p3(moved after p2), p1, blob] minus deleted p2.
    assert_eq!(nb1_pages, [p3.page_id, p1.page_id, blob_page.page_id]);
    assert!(k.lib.join("notebooks/notebook-1/.page-trash").exists());
    // Every materialized page opens through the runtime's own page-switch validation
    // (framebuffer geometry incl. stride, sealed profile).
    for (nbl, pages) in &locals {
        let dir = k.lib.join("notebooks").join(nbl);
        let m = crate::library::read_notebook_manifest_read_only(&dir).unwrap();
        let (_, first) = load_existing_page(&dir, &m.page_ids[0]).unwrap();
        for pl in pages {
            crate::notebook::load_page_for_switch(&dir, pl, &first.render_profile, &geo())
                .unwrap_or_else(|e| panic!("{nbl}/{pl} not openable by the runtime: {e}"));
        }
    }
    // Blob background bytes exact on disk.
    let blob_local = ident.pages["notebook-1"]
        .iter()
        .find(|(_, g)| **g == blob_page.page_id)
        .unwrap()
        .0;
    let ro =
        crate::store::load_page_read_only(&k.lib.join("notebooks/notebook-1"), blob_local).unwrap();
    assert_eq!(ro.background, bytes);
    // Replica-local UI state: the active notebook/page were not remotely changed.
    assert_eq!(repo.active_notebook_id(), "notebook-1");
    // No outbound echo.
    assert_eq!(s.next_client_seq(), 1);
    assert_eq!(srv.head(lib), 9);
}

// ------------------------------------------------------------------ own echo + interleaving

#[test]
fn own_echo_is_consumed_without_duplicate_effects() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    kindle_write(&s, &k, "notebook-1", "page-1", 4000.0);
    wait("ack", || s.state().acked_client_seq == 1);
    assert_eq!(s.accepted_len(), 1, "ACKed op retained in the bridge");
    converge(&s, &srv);
    assert_eq!(
        s.accepted_len(),
        0,
        "bridge consumed once the cursor crossed it"
    );
    assert_eq!(s.stats().own_echoes, 1);
    let (_, loaded) = load_existing_page(&k.lib.join("notebooks/notebook-1"), "page-1").unwrap();
    assert_eq!(loaded.strokes.len(), 1, "one stroke, not two");
    assert_eq!(s.next_client_seq(), 2);
    assert_eq!(srv.head(lib), 1);
    assert_match(&k, &s, &srv);
}

#[test]
fn interleaved_two_replica_order_converges_everywhere() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    let mut b = Remote::new(lib);
    // seq 1 A AddStroke A1
    kindle_write(&s, &k, "notebook-1", "page-1", 3000.0);
    wait("A1", || srv.head(lib) == 1);
    // seq 2 B AddStroke B1, seq 3 B RenameNotebook
    let b1 = remote_stroke(line(2000.0, 12000.0, 6000.0, 0.3));
    b.submit(
        &srv,
        vec![
            add(nb, p0.page_id, &b1),
            SyncOperationV1::RenameNotebook {
                notebook_id: nb,
                title: "Shared".into(),
            },
        ],
    );
    // seq 4 A AddStroke A2 (the Kindle has not applied B's ops yet)
    kindle_write(&s, &k, "notebook-1", "page-1", 8000.0);
    wait("A2", || srv.head(lib) == 4);
    // seq 5 B CreatePage
    b.submit(
        &srv,
        vec![SyncOperationV1::CreatePage {
            notebook_id: nb,
            page: blank_spec(&p0),
            after_page_id: Some(p0.page_id),
        }],
    );
    // seq 6 A EraseRegion through A1 only
    kindle_erase(&s, &k, "notebook-1", "page-1", 4500.0, 2500.0, 3500.0);
    wait("A-erase", || srv.head(lib) == 6);
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    // Independent fresh replay of the server ledger gives the same state.
    let (_, tail) = srv.call(
        "GET",
        &format!("/v1/libraries/{lib}/operations?after=0&limit=100"),
        None,
    );
    let (_, items) = parse_tail(&tail, lib, 0).unwrap();
    let genesis = SyncSnapshotV1::parse(&fs::read(k.sync.join(GENESIS_FILE)).unwrap()).unwrap();
    let mut st = LibraryState::genesis(genesis).unwrap();
    for it in &items {
        st = st.apply(&it.envelope).unwrap().0;
    }
    assert_eq!(st.snapshot, local_snapshot(&k.lib));
    assert_eq!(srv.head(lib), 6, "no echo operations");
    assert_eq!(s.next_client_seq(), 4);
}

// ------------------------------------------------------------------ offline overlay

fn reopen_offline(k: &Kindle, s: OutboundSync) -> OutboundSync {
    s.shutdown(Duration::from_secs(3));
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    config(k, &url, &"0".repeat(64));
    OutboundSync::open(&k.lib, &k.sync).unwrap()
}

fn reopen_online(k: &Kindle, s: OutboundSync, srv: &Srv) -> OutboundSync {
    s.shutdown(Duration::from_secs(3));
    config(k, &srv.url(), &srv.token);
    OutboundSync::open(&k.lib, &k.sync).unwrap()
}

#[test]
fn offline_local_work_and_remote_server_work_both_survive() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    kindle_write(&s, &k, "notebook-1", "page-1", 2000.0);
    converge(&s, &srv);
    // Offline: L1, L2 locally; meanwhile B submits R1, R2.
    let s = reopen_offline(&k, s);
    let l1 = kindle_write(&s, &k, "notebook-1", "page-1", 5000.0);
    let l2 = kindle_write(&s, &k, "notebook-1", "page-1", 7000.0);
    wait("journal", || s.pending() == 2);
    let mut b = Remote::new(lib);
    let r1 = remote_stroke(line(1000.0, 10000.0, 8000.0, 0.9));
    b.submit(
        &srv,
        vec![
            add(nb, p0.page_id, &r1),
            SyncOperationV1::RenameNotebook {
                notebook_id: nb,
                title: "While Offline".into(),
            },
        ],
    );
    // Reconnect.
    let s = reopen_online(&k, s, &srv);
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    let (_, snap) = srv.snapshot(lib);
    let ids: Vec<String> = snap.notebooks[0].pages[0]
        .strokes
        .iter()
        .map(|x| local_stroke_id_for_global(x.stroke_id))
        .collect();
    assert!(
        ids.contains(&l1.id.as_str().to_string()) && ids.contains(&l2.id.as_str().to_string()),
        "local work kept"
    );
    assert!(
        ids.contains(&local_stroke_id_for_global(r1.stroke_id)),
        "remote work kept"
    );
    assert_eq!(snap.notebooks[0].title, "While Offline");
    assert_eq!(
        (s.pending(), s.accepted_len(), s.server_cursor()),
        (0, 0, srv.head(lib))
    );
}

#[test]
fn same_page_offline_edits_converge_and_erase_respects_observed_roots() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    let s = reopen_offline(&k, s);
    let a = kindle_write(&s, &k, "notebook-1", "page-1", 6000.0);
    wait("journal", || s.pending() == 1);
    let mut b = Remote::new(lib);
    let bs = remote_stroke(line(1000.0, 6050.0, 8000.0, 0.5)); // overlaps A spatially
    b.submit(&srv, vec![add(nb, p0.page_id, &bs)]);
    let s = reopen_online(&k, s, &srv);
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    assert_eq!(
        local_snapshot(&k.lib).notebooks[0].pages[0].strokes.len(),
        2,
        "both strokes survive"
    );
    // B erases across both strokes but observed only its own root: A must be untouched.
    b.submit(
        &srv,
        vec![erase_vertical(
            nb,
            p0.page_id,
            vec![bs.stroke_id],
            4000.0,
            5000.0,
            7000.0,
        )],
    );
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    let snap = local_snapshot(&k.lib);
    let a_gid = crate::sync_local::kindle_global_stroke_id(p0.page_id, a.id.as_str()).unwrap();
    let a_now = snap.notebooks[0].pages[0]
        .strokes
        .iter()
        .find(|x| x.stroke_id == a_gid)
        .unwrap();
    assert_eq!(a_now.points.len(), a.points.len(), "A not clipped");
    assert_eq!(
        snap.notebooks[0].pages[0]
            .strokes
            .iter()
            .filter(|x| x.root_id == bs.stroke_id)
            .count(),
        2
    );
}

#[test]
fn same_root_concurrent_erases_converge_through_the_reducer() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    let mut b = Remote::new(lib);
    let x = remote_stroke(line(1000.0, 6000.0, 12000.0, 0.7));
    b.submit(&srv, vec![add(nb, p0.page_id, &x)]);
    converge(&s, &srv);
    // Both replicas erase root X independently while disconnected.
    let s = reopen_offline(&k, s);
    kindle_erase(&s, &k, "notebook-1", "page-1", 9000.0, 5000.0, 7000.0);
    wait("journal", || s.pending() == 1);
    b.submit(
        &srv,
        vec![erase_vertical(
            nb,
            p0.page_id,
            vec![x.stroke_id],
            4000.0,
            5000.0,
            7000.0,
        )],
    );
    let s = reopen_online(&k, s, &srv);
    converge(&s, &srv);
    // The reducer is root-based, so the Kindle's erase re-applies on the canonical fragments.
    assert_match(&k, &s, &srv);
    let pieces = local_snapshot(&k.lib).notebooks[0].pages[0]
        .strokes
        .iter()
        .filter(|s| s.root_id == x.stroke_id)
        .count();
    assert_eq!(pieces, 3, "two cuts → three pieces, same on both replicas");
}

#[test]
fn pending_local_op_invalid_after_rebase_fails_closed() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (_, p0) = first_page(&srv, lib);
    let mut b = Remote::new(lib);
    // A second notebook exists on both sides.
    let nb2 = NotebookId::from_uuid(Uuid::now_v7()).unwrap();
    b.submit(
        &srv,
        vec![SyncOperationV1::CreateNotebook {
            notebook_id: nb2,
            title: "Doomed".into(),
            after_notebook_id: None,
            first_page: blank_spec(&p0),
        }],
    );
    converge(&s, &srv);
    let nb2_local = read_identity(&k.lib)
        .unwrap()
        .unwrap()
        .notebooks
        .iter()
        .find(|(_, g)| **g == nb2)
        .unwrap()
        .0
        .clone();
    // Offline: the Kindle writes into it; the remote replica deletes it.
    s.shutdown(Duration::from_secs(3));
    let _ = fs::remove_file(k.sync.join(CONFIG_FILE)); // no network at all
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    kindle_write(&s, &k, &nb2_local, "page-1", 5000.0);
    wait("journal", || s.pending() == 1);
    b.submit(
        &srv,
        vec![SyncOperationV1::DeleteNotebook { notebook_id: nb2 }],
    );
    let before = local_snapshot(&k.lib);
    let cursor = s.server_cursor();
    // Pull first (the inbound half), then apply: the pending AddStroke no longer applies.
    let cfg = SyncConfigV1 {
        v: 1,
        server_base_url: srv.url(),
        token: srv.token.clone(),
        debug_drop_first_ack: false,
        debug_crash_point: None,
    };
    assert!(super::inbound::pull_once(&s.shared, &agent(), &cfg)
        .ok()
        .unwrap());
    let e = s.apply_inbound().unwrap_err();
    assert!(e.contains("LOCAL_REBASE_CONFLICT"), "{e}");
    assert!(s.fault().unwrap().contains("LOCAL_REBASE_CONFLICT"));
    // Nothing lost or rewritten; cursor at the last safe position; app data intact.
    assert_eq!(local_snapshot(&k.lib), before);
    assert_eq!(s.server_cursor(), cursor);
    assert_eq!(s.pending(), 1);
    assert_eq!(s.inbox_len(), 1);
    s.shutdown(Duration::from_secs(3));
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    assert_eq!(
        (s.pending(), s.inbox_len()),
        (1, 1),
        "outbox + inbox durable across restart"
    );
    let _ = k;
}

// ------------------------------------------------------------------ counters + pagination

#[test]
fn fifty_remote_operations_never_consume_local_client_seqs() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    kindle_write(&s, &k, "notebook-1", "page-1", 1500.0);
    converge(&s, &srv);
    let n = s.next_client_seq();
    let (nb, p0) = first_page(&srv, lib);
    let mut b = Remote::new(lib);
    let mut ops = Vec::new();
    let mut roots = Vec::new();
    for i in 0..44 {
        let st = remote_stroke(line(
            500.0,
            2500.0 + 400.0 * i as f32,
            3000.0,
            0.1 + 0.02 * i as f32,
        ));
        roots.push(st.stroke_id);
        ops.push(add(nb, p0.page_id, &st));
    }
    ops.push(erase_vertical(
        nb,
        p0.page_id,
        roots[..10].to_vec(),
        2000.0,
        2000.0,
        7000.0,
    ));
    ops.push(SyncOperationV1::DeleteStrokeLineage {
        notebook_id: nb,
        page_id: p0.page_id,
        root_id: roots[20],
    });
    ops.push(SyncOperationV1::RenameNotebook {
        notebook_id: nb,
        title: "Fifty".into(),
    });
    let pg = blank_spec(&p0);
    ops.push(SyncOperationV1::CreatePage {
        notebook_id: nb,
        page: pg.clone(),
        after_page_id: None,
    });
    ops.push(SyncOperationV1::MovePage {
        notebook_id: nb,
        page_id: pg.page_id,
        after_page_id: Some(p0.page_id),
    });
    ops.push(SyncOperationV1::DeletePage {
        notebook_id: nb,
        page_id: pg.page_id,
    });
    assert_eq!(ops.len(), 50);
    b.submit(&srv, ops);
    converge(&s, &srv);
    assert_eq!(
        s.next_client_seq(),
        n,
        "remote operations consumed no local client_seq"
    );
    assert_eq!(srv.head(lib), 51);
    assert_match(&k, &s, &srv);
}

#[test]
fn long_tail_is_paged_contiguously_to_the_head() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    let mut b = Remote::new(lib);
    let ops: Vec<SyncOperationV1> = (0..1250)
        .map(|i| {
            let x = 100.0 + (i % 50) as f32 * 300.0;
            let y = 200.0 + (i / 50) as f32 * 700.0;
            add(
                nb,
                p0.page_id,
                &remote_stroke(vec![
                    SyncPoint::new(x, y, Some(0.5)),
                    SyncPoint::new(x + 100.0, y + 100.0, Some(0.6)),
                ]),
            )
        })
        .collect();
    b.submit(&srv, ops);
    converge(&s, &srv);
    assert_eq!(s.server_cursor(), 1250);
    assert_eq!(s.stats().staged, 1250, "every item staged exactly once");
    assert_eq!(s.stats().applied, 1250);
    assert_match(&k, &s, &srv);
}

// ------------------------------------------------------------------ crash recovery

/// Stage remote work, crash at `point`, "restart", and require exactly-once convergence.
fn crash_and_recover(
    point: &str,
    ops: impl FnOnce(&mut Remote, NotebookId, &SnapshotPage) -> Vec<SyncOperationV1>,
) {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    kindle_write(&s, &k, "notebook-1", "page-1", 1500.0);
    converge(&s, &srv);
    let n = s.next_client_seq();
    s.set_crash_point(Some(point));
    let mut b = Remote::new(lib);
    let o = ops(&mut b, nb, &p0);
    b.submit(&srv, o);
    let t0 = Instant::now();
    while s.fault().is_none() {
        if s.inbound_ready() {
            let _ = s.apply_inbound();
        }
        s.kick();
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "crash point {point} never hit"
        );
    }
    assert!(s.fault().unwrap().contains(point), "{:?}", s.fault());
    drop(s); // process death at that point
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    assert_eq!(s.next_client_seq(), n, "{point}: no echo");
    assert_eq!(
        srv.head(lib),
        1 + b.next - 1,
        "{point}: nothing uploaded by recovery"
    );
    // Idempotent: another restart changes nothing.
    let before = local_hash(&k.lib);
    drop(s);
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    assert!(s.fault().is_none());
    converge(&s, &srv);
    assert_eq!(local_hash(&k.lib), before);
}

fn stroke_ops(_: &mut Remote, nb: NotebookId, p0: &SnapshotPage) -> Vec<SyncOperationV1> {
    vec![add(
        nb,
        p0.page_id,
        &remote_stroke(line(1000.0, 9000.0, 9000.0, 0.8)),
    )]
}

fn erase_ops(_: &mut Remote, nb: NotebookId, p0: &SnapshotPage) -> Vec<SyncOperationV1> {
    let x = remote_stroke(line(1000.0, 9000.0, 9000.0, 0.8));
    vec![
        add(nb, p0.page_id, &x),
        erase_vertical(nb, p0.page_id, vec![x.stroke_id], 5000.0, 8000.0, 10000.0),
    ]
}

fn page_ops(_: &mut Remote, nb: NotebookId, p0: &SnapshotPage) -> Vec<SyncOperationV1> {
    vec![SyncOperationV1::CreatePage {
        notebook_id: nb,
        page: blank_spec(p0),
        after_page_id: Some(p0.page_id),
    }]
}

fn notebook_ops(_: &mut Remote, _: NotebookId, p0: &SnapshotPage) -> Vec<SyncOperationV1> {
    vec![SyncOperationV1::CreateNotebook {
        notebook_id: NotebookId::from_uuid(Uuid::now_v7()).unwrap(),
        title: "Crashy".into(),
        after_notebook_id: None,
        first_page: blank_spec(p0),
    }]
}

#[test]
fn crash_after_inbox_fsync() {
    crash_and_recover("after_inbox_fsync", stroke_ops);
}
#[test]
fn crash_after_marker() {
    crash_and_recover("after_marker", stroke_ops);
}
#[test]
fn crash_after_page_bind_before_files() {
    crash_and_recover("after_bind_page", page_ops);
}
#[test]
fn crash_after_page_files_before_manifest() {
    crash_and_recover("after_create_page_files", page_ops);
}
#[test]
fn crash_after_notebook_bind_before_files() {
    crash_and_recover("after_bind_notebook", notebook_ops);
}
#[test]
fn crash_after_notebook_create() {
    crash_and_recover("after_create_notebook", notebook_ops);
}
#[test]
fn crash_after_stroke_journal() {
    crash_and_recover("after_stroke_journal", stroke_ops);
}
#[test]
fn crash_after_erase_journal() {
    crash_and_recover("after_erase_journal", erase_ops);
}
#[test]
fn crash_after_verification() {
    crash_and_recover("after_verify", erase_ops);
}
#[test]
fn crash_after_base_write() {
    crash_and_recover("after_base_write", stroke_ops);
}
#[test]
fn crash_before_cursor_write() {
    crash_and_recover("before_cursor_write", page_ops);
}
#[test]
fn crash_after_cursor_before_inbox_cleanup() {
    crash_and_recover("after_cursor_write", stroke_ops);
}

// ------------------------------------------------------------------ no-echo audit

#[test]
fn materialization_paths_cannot_author_sync_operations() {
    // Mutation origin is structural: the materializer and the inbound apply path never call
    // the local-authoring capture API, never mint OperationIds and never touch the outbox.
    for (name, src) in [
        ("sync_materialize.rs", include_str!("sync_materialize.rs")),
        ("sync_inbound.rs", include_str!("sync_inbound.rs")),
    ] {
        let code: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            "stroke_committed",
            "lineage_deleted",
            "prepare_",
            "page_created",
            "notebook_created",
            "take_seq",
            "OperationId::from_uuid",
            "Uuid::now_v7",
            "JournalCmd::Append",
            "OUTBOX_FILE",
        ] {
            assert!(!code.contains(forbidden), "{name} contains {forbidden}");
        }
    }
}

#[test]
fn crash_mid_rebase_rebuild_recovers_without_reusing_local_ids() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    // Offline local L1; the remote R reaches the server first → canonical [R, L1].
    let s = reopen_offline(&k, s);
    let l1 = kindle_write(&s, &k, "notebook-1", "page-1", 5000.0);
    wait("journal", || s.pending() == 1);
    let r = remote_stroke(line(1000.0, 9000.0, 9000.0, 0.8));
    Remote::new(lib).submit(&srv, vec![add(nb, p0.page_id, &r)]);
    let s = reopen_online(&k, s, &srv);
    s.set_crash_point(Some("after_stroke_journal")); // first re-journaled stroke of the rebuild
    let t0 = Instant::now();
    while s.fault().is_none() {
        if s.inbound_ready() {
            let _ = s.apply_inbound();
        }
        s.kick();
        std::thread::sleep(Duration::from_millis(10));
        assert!(t0.elapsed() < Duration::from_secs(30));
    }
    assert!(s.fault().unwrap().contains("after_stroke_journal"));
    drop(s);
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    let (_, snap) = srv.snapshot(lib);
    let order: Vec<StrokeId> = snap.notebooks[0].pages[0]
        .strokes
        .iter()
        .map(|x| x.stroke_id)
        .collect();
    let l1_gid = crate::sync_local::kindle_global_stroke_id(p0.page_id, l1.id.as_str()).unwrap();
    assert_eq!(
        order,
        [r.stroke_id, l1_gid],
        "canonical order, each exactly once"
    );
    assert_eq!(srv.head(lib), 2, "no echo");
}

#[test]
fn local_ids_round_trip_every_global_uuid_version() {
    let page = PageId::from_uuid(Uuid::now_v7()).unwrap();
    for u in [
        Uuid::now_v7(),
        Uuid::new_v5(&Uuid::now_v7(), b"frag/0"),
        Uuid::from_u128(0x0190_1234_5678_4abc_8def_0123_4567_89ab),
    ] {
        let g = StrokeId::from_uuid(u).unwrap();
        let l = local_stroke_id_for_global(g);
        assert_eq!(
            crate::sync_local::kindle_global_stroke_id(page, &l).unwrap(),
            g,
            "{l}"
        );
        for gen in [1, 2, 17] {
            let r = crate::sync_local::rebased_local_stroke_id(gen, g);
            assert_eq!(
                crate::sync_local::kindle_global_stroke_id(page, &r).unwrap(),
                g,
                "{r}"
            );
        }
    }
    // Legacy local IDs keep the S1 rule.
    let legacy = crate::sync_local::kindle_global_stroke_id(page, "s-1727000000000-1").unwrap();
    assert_eq!(
        legacy,
        sync_protocol::global_stroke_id(page, "s-1727000000000-1").unwrap()
    );
}

#[test]
fn remote_background_the_runtime_would_rewrite_fails_closed() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    let g = geo();
    // Pixels inside the reserved chrome band: the runtime's F11 scrub would rewrite them.
    let bytes: Vec<u8> = (0..(g.line_length * g.yres) as usize)
        .map(|i| (i % 7) as u8)
        .collect();
    let mut probe = bytes.clone();
    assert!(crate::page_label::scrub_chrome_band(&mut probe, &g).unwrap());
    let sha = sha256_hex(&bytes);
    assert!(srv.call("PUT", &format!("/v1/blobs/{sha}"), Some(&bytes)).0 < 300);
    let before = local_snapshot(&k.lib);
    Remote::new(lib).submit(
        &srv,
        vec![SyncOperationV1::CreatePage {
            notebook_id: nb,
            page: PageSpec {
                page_id: PageId::from_uuid(Uuid::now_v7()).unwrap(),
                background: BackgroundRef::Blob {
                    sha256: sha,
                    byte_len: bytes.len() as u64,
                    format: BackgroundFormat {
                        encoding: sync_protocol::BackgroundEncoding::Gray8,
                        width: g.xres,
                        height: g.yres,
                        stride: g.line_length,
                    },
                },
                render: p0.render.clone(),
            },
            after_page_id: None,
        }],
    );
    let t0 = Instant::now();
    while s.fault().is_none() {
        if s.inbound_ready() {
            let _ = s.apply_inbound();
        }
        s.kick();
        std::thread::sleep(Duration::from_millis(10));
        assert!(t0.elapsed() < Duration::from_secs(30));
    }
    assert!(
        s.fault().unwrap().contains("chrome band"),
        "{:?}",
        s.fault()
    );
    assert_eq!(
        s.server_cursor(),
        0,
        "cursor never passes an unapplied item"
    );
    assert_eq!(
        local_snapshot(&k.lib).notebooks,
        before.notebooks,
        "no live page created"
    );
}

// ------------------------------------------------------------------ accepted-bridge integrity

mod bridge {
    use super::super::inbound::{accepted_line, load_accepted, AcceptedOp, HISTORY_FILE};
    use super::*;

    struct B {
        dir: PathBuf,
        lib: LibraryId,
        me: ReplicaId,
    }

    fn setup() -> B {
        let dir = tmp("bridge");
        fs::create_dir_all(&dir).unwrap();
        B {
            dir,
            lib: LibraryId::from_uuid(Uuid::now_v7()).unwrap(),
            me: ReplicaId::from_uuid(Uuid::now_v7()).unwrap(),
        }
    }

    fn own(b: &B, client_seq: u64, server_seq: u64) -> AcceptedOp {
        own_by(
            b,
            b.me,
            client_seq,
            server_seq,
            OperationId::from_uuid(Uuid::now_v7()).unwrap(),
        )
    }

    fn own_by(
        b: &B,
        replica: ReplicaId,
        client_seq: u64,
        server_seq: u64,
        op: OperationId,
    ) -> AcceptedOp {
        let nb = NotebookId::from_uuid(Uuid::now_v7()).unwrap();
        let pg = PageId::from_uuid(Uuid::now_v7()).unwrap();
        let envelope = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: op,
            library_id: b.lib,
            replica_id: replica,
            client_seq,
            operation: add(
                nb,
                pg,
                &remote_stroke(line(100.0, 100.0 + client_seq as f32, 500.0, 0.5)),
            ),
        };
        let bytes = envelope.to_canonical_bytes().unwrap();
        AcceptedOp {
            client_seq,
            server_seq,
            envelope,
            bytes,
        }
    }

    fn write(b: &B, lines: &[Vec<u8>]) {
        fs::write(b.dir.join(HISTORY_FILE), lines.concat()).unwrap();
    }

    fn load(b: &B, cursor: u64) -> Result<Vec<AcceptedOp>, String> {
        load_accepted(&b.dir, b.lib, b.me, cursor)
    }

    fn refused(b: &B, what: &str) {
        let e = load(b, 0).unwrap_err();
        assert!(e.contains("ACCEPTED_HISTORY_CORRUPT"), "{e}");
        assert!(e.contains(what), "expected {what:?} in {e}");
    }

    #[test]
    fn valid_record_loads() {
        let b = setup();
        let a = own(&b, 1, 7);
        write(&b, &[accepted_line(&a)]);
        assert_eq!(load(&b, 0).unwrap(), vec![a]);
    }

    #[test]
    fn exact_duplicate_is_tolerated_once() {
        let b = setup();
        let a = own(&b, 1, 7);
        let c = own(&b, 2, 9);
        write(
            &b,
            &[accepted_line(&a), accepted_line(&c), accepted_line(&a)],
        );
        assert_eq!(
            load(&b, 0).unwrap(),
            vec![a, c],
            "deduped, client_seq order"
        );
    }

    #[test]
    fn same_client_seq_with_different_server_seq_is_refused() {
        let b = setup();
        let a = own(&b, 1, 7);
        let mut moved = a.clone();
        moved.server_seq = 8;
        write(&b, &[accepted_line(&a), accepted_line(&moved)]);
        refused(&b, "different server_seq");
    }

    #[test]
    fn same_client_seq_with_different_operation_id_is_refused() {
        let b = setup();
        let a = own(&b, 1, 7);
        let other = own(&b, 1, 7); // same client_seq + server_seq, different OperationId
        write(&b, &[accepted_line(&a), accepted_line(&other)]);
        refused(&b, "different OperationId");
    }

    #[test]
    fn same_client_seq_with_different_bytes_is_refused() {
        let b = setup();
        let a = own(&b, 1, 7);
        let same_ids = own_by(&b, b.me, 1, 7, a.envelope.operation_id); // other payload
        assert_ne!(a.bytes, same_ids.bytes);
        write(&b, &[accepted_line(&a), accepted_line(&same_ids)]);
        refused(&b, "different envelope bytes");
    }

    #[test]
    fn same_operation_id_under_two_client_seqs_is_refused() {
        let b = setup();
        let a = own(&b, 1, 7);
        let reused = own_by(&b, b.me, 2, 9, a.envelope.operation_id);
        write(&b, &[accepted_line(&a), accepted_line(&reused)]);
        refused(&b, "recorded under client_seq 1 and 2");
    }

    #[test]
    fn one_server_seq_for_two_own_operations_is_refused() {
        let b = setup();
        write(
            &b,
            &[accepted_line(&own(&b, 1, 7)), accepted_line(&own(&b, 2, 7))],
        );
        refused(&b, "server_seq 7 assigned to own client_seq 1 and 2");
    }

    #[test]
    fn server_seq_must_increase_with_client_seq() {
        let b = setup();
        // Gaps are normal (remote ops in between); inversions are not.
        write(
            &b,
            &[
                accepted_line(&own(&b, 1, 3)),
                accepted_line(&own(&b, 2, 10)),
                accepted_line(&own(&b, 3, 11)),
            ],
        );
        assert_eq!(load(&b, 0).unwrap().len(), 3);
        write(
            &b,
            &[
                accepted_line(&own(&b, 1, 10)),
                accepted_line(&own(&b, 2, 4)),
            ],
        );
        refused(&b, "earlier client_seq 1 has server_seq 10");
    }

    #[test]
    fn corrupt_duplicate_cannot_hide_behind_an_earlier_valid_record() {
        let b = setup();
        let a = own(&b, 1, 7);
        // Non-canonical copy of the same op (whitespace inside the envelope text).
        let text = String::from_utf8(a.bytes.clone())
            .unwrap()
            .replacen(':', ": ", 1);
        let noncanon = format!(
            "{{\"v\":1,\"client_seq\":1,\"server_seq\":7,\"operation_id\":\"{}\",\"envelope\":{}}}\n",
            a.envelope.operation_id,
            serde_json::to_string(&text).unwrap()
        );
        write(&b, &[accepted_line(&a), noncanon.into_bytes()]);
        refused(&b, "not canonical");
        write(
            &b,
            &[
                accepted_line(&a),
                b"{\"v\":1,\"client_seq\":1,garbage}\n".to_vec(),
            ],
        );
        refused(&b, "unparsable");
        // Foreign replica / mismatched ids are refused wherever they appear.
        let foreign = own_by(
            &b,
            ReplicaId::from_uuid(Uuid::now_v7()).unwrap(),
            2,
            9,
            OperationId::from_uuid(Uuid::now_v7()).unwrap(),
        );
        write(&b, &[accepted_line(&a), accepted_line(&foreign)]);
        refused(&b, "not this replica");
        let lying = String::from_utf8(accepted_line(&own(&b, 2, 9)))
            .unwrap()
            .replacen("\"client_seq\":2", "\"client_seq\":5", 1)
            .into_bytes();
        write(&b, &[accepted_line(&a), lying]);
        refused(&b, "client_seq 5 but envelope carries 2");
    }

    #[test]
    fn consumed_records_are_filtered_but_never_exempt_from_validation() {
        let b = setup();
        let old = own(&b, 1, 3);
        let live = own(&b, 2, 8);
        write(&b, &[accepted_line(&old), accepted_line(&live)]);
        assert_eq!(
            load(&b, 5).unwrap(),
            vec![live.clone()],
            "server_seq <= cursor is consumed"
        );
        assert!(load(&b, 8).unwrap().is_empty());
        let mut bad_old = old.clone();
        bad_old.server_seq = 4; // conflicting duplicate of a consumed record
        write(
            &b,
            &[
                accepted_line(&old),
                accepted_line(&live),
                accepted_line(&bad_old),
            ],
        );
        let e = load(&b, 5).unwrap_err();
        assert!(e.contains("different server_seq"), "{e}");
    }
}

#[test]
fn crash_style_exact_bridge_duplicate_recovers_and_converges() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    // Stop right after the ACK, before the echo is consumed, so the op sits in the bridge.
    let s = {
        s.shutdown(Duration::from_secs(3));
        // Online for pushes, but inbound held back by a crash point before the marker.
        let mut c: SyncConfigV1 = read_json(&k.sync.join(CONFIG_FILE)).unwrap();
        c.debug_crash_point = Some("after_marker".into());
        write_json(&k.sync, CONFIG_FILE, &c).unwrap();
        OutboundSync::open(&k.lib, &k.sync).unwrap()
    };
    kindle_write(&s, &k, "notebook-1", "page-1", 3000.0);
    wait("ack", || {
        s.state().acked_client_seq == 1 && s.accepted_len() == 1
    });
    s.shutdown(Duration::from_secs(3));
    // Crash-style state: the ACK record was appended twice and the ACK state write was lost.
    let bridge = k.sync.join(HISTORY_FILE);
    let once = fs::read(&bridge).unwrap();
    assert_eq!(once.iter().filter(|b| **b == b'\n').count(), 1);
    fs::write(&bridge, [once.clone(), once].concat()).unwrap();
    let mut st: SyncStateV1 = read_json(&k.sync.join(STATE_FILE)).unwrap();
    st.acked_client_seq = 0;
    write_json(&k.sync, STATE_FILE, &st).unwrap();
    let _ = fs::remove_file(k.sync.join(APPLY_FILE));
    config(&k, &srv.url(), &srv.token);
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(
        s.state().acked_client_seq,
        1,
        "lagging ACK repaired from the bridge"
    );
    assert_eq!(s.accepted_len(), 1, "exact duplicate collapsed");
    assert_eq!(s.pending(), 0, "not re-uploaded");
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    assert_eq!(srv.head(lib), 1, "no second upload");
    let (_, loaded) = load_existing_page(&k.lib.join("notebooks/notebook-1"), "page-1").unwrap();
    assert_eq!(loaded.strokes.len(), 1);
}

// ------------------------------------------------------------------ pen-path latency (echoes)

/// Every file under the library (relative path → bytes).
fn library_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                out.insert(rel, fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn own_echoes_take_the_fast_path_and_never_touch_the_library() {
    // Regression: every stroke's own echo re-materialized, re-read and re-verified the whole
    // library on the runtime thread. Pure own echoes cannot change ExpectedLocalState.
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    for i in 0..6 {
        kindle_write(&s, &k, "notebook-1", "page-1", 2000.0 + 1500.0 * i as f32);
    }
    wait("acks", || s.state().acked_client_seq == 6);
    let files_before = library_files(&k.lib);
    converge(&s, &srv);
    assert_eq!(
        library_files(&k.lib),
        files_before,
        "own echoes must not rewrite notebook files"
    );
    let st = s.stats();
    assert_eq!(st.own_echoes, 6);
    assert!(st.echo_fast_path >= 1, "{st:?}");
    assert_eq!(
        st.echo_fast_path,
        st.applied.min(st.echo_fast_path),
        "fast path only for applied batches"
    );
    assert!(!k.sync.join(APPLY_FILE).exists());
    assert_match(&k, &s, &srv);

    // A foreign operation still materializes through the general path.
    let (nb, p0) = first_page(&srv, lib);
    let mut remote = Remote::new(lib);
    remote.submit(
        &srv,
        vec![add(
            nb,
            p0.page_id,
            &remote_stroke(line(1000.0, 9000.0, 3000.0, 0.5)),
        )],
    );
    let fast_before = s.stats().echo_fast_path;
    converge(&s, &srv);
    assert_eq!(
        s.stats().echo_fast_path,
        fast_before,
        "foreign op: full path"
    );
    let (_, loaded) = load_existing_page(&k.lib.join("notebooks/notebook-1"), "page-1").unwrap();
    assert_eq!(loaded.strokes.len(), 7);
    assert_match(&k, &s, &srv);

    // Cursor and content survive a restart; startup reconciliation accepts the state.
    let cursor = s.server_cursor();
    let s = reopen_offline(&k, s);
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.server_cursor(), cursor);
    s.shutdown(Duration::from_secs(3));
}

#[test]
fn echo_fast_path_is_refused_when_a_foreign_op_interleaves() {
    // Canonical order own(1) · foreign · own(2): the rebase is real work, never skipped.
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let s = reopen_offline(&k, s);
    kindle_write(&s, &k, "notebook-1", "page-1", 3000.0);
    wait("journal", || s.pending() == 1);
    let s = reopen_online(&k, s, &srv);
    wait("ack 1", || s.state().acked_client_seq == 1);
    let (nb, p0) = first_page(&srv, lib);
    let mut remote = Remote::new(lib);
    remote.submit(
        &srv,
        vec![add(
            nb,
            p0.page_id,
            &remote_stroke(line(1000.0, 8000.0, 3000.0, 0.5)),
        )],
    );
    kindle_write(&s, &k, "notebook-1", "page-1", 6000.0);
    wait("ack 2", || s.state().acked_client_seq == 2);
    wait("staged", || s.inbox_len() == 3);
    let applied = s.apply_inbound().unwrap().expect("one transaction");
    assert_eq!((applied.ops, applied.own_echoes), (3, 2));
    assert!(applied.report.changed(), "the foreign stroke materialized");
    assert_eq!(s.stats().echo_fast_path, 0);
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    s.shutdown(Duration::from_secs(3));
}

// ------------------------------------------------------------------ backlog vs own echoes

/// `n` durable local strokes captured offline in one batch (a large upload backlog).
fn offline_backlog(k: &Kindle, s: &OutboundSync, n: usize) {
    let strokes: Vec<Stroke> = (0..n)
        .map(|i| Stroke {
            id: new_stroke_id(),
            points: (0..6)
                .map(|j| {
                    Point::new(500.0 + 900.0 * j as f32, 300.0 + 20.0 * (i % 1000) as f32)
                        .with_pressure(0.5)
                })
                .collect(),
            brush: Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
            finished: true,
        })
        .collect();
    let (mut store, _) =
        load_existing_page(&k.lib.join("notebooks").join("notebook-1"), "page-1").unwrap();
    store.append_committed_strokes(&strokes).unwrap();
    for st in &strokes {
        s.stroke_committed("notebook-1", "page-1", st);
    }
    s.wait_journal_durable(&std::sync::atomic::AtomicBool::new(false))
        .unwrap();
}

/// Drive the runtime's idle-time inbound loop to convergence; returns the apply transactions.
fn converge_counting(s: &OutboundSync, srv: &Srv) -> usize {
    let mut applies = 0;
    let t0 = Instant::now();
    loop {
        assert!(s.fault().is_none(), "fault: {:?}", s.fault());
        if s.inbound_ready() {
            if s.apply_inbound().unwrap().is_some() {
                applies += 1;
            }
            continue;
        }
        if s.state().acked_client_seq + 1 == s.next_client_seq()
            && s.pending() == 0
            && s.accepted_len() == 0
            && s.inbox_len() == 0
            && s.server_cursor() == srv.head(s.state().library_id)
        {
            return applies;
        }
        assert!(t0.elapsed() < Duration::from_secs(90), "no convergence");
        s.kick();
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// The device drain: consuming own echoes rewrote the whole ServerBaseState and held the lock
/// the uploader's ACK needs, so uploads advanced one batch per base write. Own echoes now wait
/// while a large backlog remains (uploads run free) and are then consumed in few transactions.
#[test]
fn own_echoes_wait_for_a_large_upload_backlog_then_drain_in_few_base_writes() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let s = reopen_offline(&k, s);
    let n = 900;
    offline_backlog(&k, &s, n);
    let s = reopen_online(&k, s, &srv);
    let mut saw_deferred = false;
    let t0 = Instant::now();
    while s.pending() > super::inbound::DEFER_OWN_ECHOES_WHILE_PENDING + 150 {
        assert!(t0.elapsed() < Duration::from_secs(60), "upload stalled");
        if s.inbox_len() > 0 {
            assert!(
                !s.inbound_ready(),
                "own echoes must wait while {} ops await upload",
                s.pending()
            );
            saw_deferred = true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(saw_deferred, "echoes were staged during the backlog");
    let applies = converge_counting(&s, &srv);
    // Lockstep would be one base write per upload batch (n / MAX_BATCH_OPS = 18 here).
    assert!(
        applies * 3 <= n / MAX_BATCH_OPS,
        "{applies} base writes for {n} own echoes"
    );
    assert_match(&k, &s, &srv);
    s.shutdown(Duration::from_secs(3));
}

/// Another replica's edit is never held back by our backlog.
#[test]
fn a_foreign_operation_is_applied_even_during_a_large_upload_backlog() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let s = reopen_offline(&k, s);
    offline_backlog(&k, &s, 900);
    let (nb, p0) = first_page(&srv, lib);
    let mut b = Remote::new(lib);
    let foreign = remote_stroke(line(2000.0, 15000.0, 4000.0, 0.7));
    b.submit(&srv, vec![add(nb, p0.page_id, &foreign)]);
    let s = reopen_online(&k, s, &srv);
    // Once the foreign op is staged it is applied although the backlog is still large.
    wait("foreign op staged", || {
        s.inbox_len() > 0 && s.inbound_ready()
    });
    assert!(
        s.pending() > super::inbound::DEFER_OWN_ECHOES_WHILE_PENDING,
        "backlog still large"
    );
    while s.inbound_ready() {
        s.apply_inbound().unwrap();
    }
    let (_, page) = load_existing_page(&k.lib.join("notebooks/notebook-1"), "page-1").unwrap();
    assert!(
        page.strokes.iter().any(|x| {
            crate::sync_local::kindle_global_stroke_id(p0.page_id, x.id.as_str()).ok()
                == Some(foreign.stroke_id)
        }),
        "foreign stroke materialized during the backlog"
    );
    converge_counting(&s, &srv);
    assert_match(&k, &s, &srv);
    s.shutdown(Duration::from_secs(3));
}

/// After a big upload, consuming thousands of own echoes must not rewrite the acknowledged
/// bridge (`accepted.jsonl`) and inbox after every transaction — that made each transaction
/// cost O(backlog) on the Kindle's runtime thread. They are rewritten only once little remains
/// live; a restart in between (consumed prefix still in the files) loads exactly.
#[test]
fn consuming_a_large_echo_backlog_rewrites_the_bridge_only_when_small() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let s = reopen_offline(&k, s);
    let n = 2000;
    offline_backlog(&k, &s, n);
    let s = reopen_online(&k, s, &srv);
    wait("upload drained", || s.pending() == 0);
    let acc = k.sync.join(super::inbound::HISTORY_FILE);
    let size0 = fs::metadata(&acc).unwrap().len();
    // Consume one transaction's worth, then restart with the consumed prefix still on disk.
    while s.inbound_ready() {
        s.apply_inbound().unwrap();
        if s.server_cursor() > 600 {
            break;
        }
    }
    assert_eq!(
        s.stats().bridge_compactions,
        0,
        "no rewrite while > 500 remain live"
    );
    assert!(
        fs::metadata(&acc).unwrap().len() >= size0,
        "bridge only appended to"
    );
    let s = reopen_online(&k, s, &srv);
    assert!(s.fault().is_none(), "{:?}", s.fault());
    let applies = converge_counting(&s, &srv);
    assert!(applies >= 1);
    assert!(
        s.stats().bridge_compactions >= 1 && s.stats().bridge_compactions <= applies as u64,
        "compacted once small"
    );
    assert!(
        fs::metadata(&acc).unwrap().len() < size0 / 10,
        "consumed records reclaimed at the end"
    );
    assert_match(&k, &s, &srv);
    s.shutdown(Duration::from_secs(3));
    // …and a final restart is clean.
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    s.shutdown(Duration::from_secs(3));
}

// ------------------------------------------------------------------ folders

/// Local folder change + its capture, as the runtime does it (post-commit).
fn local_folder_create(s: &OutboundSync, k: &Kindle, title: &str) -> String {
    let mut f = crate::folders::FolderStore::open(&k.lib);
    let live = crate::library::open_library_read_only(&k.lib)
        .unwrap()
        .manifest()
        .notebook_ids
        .clone();
    let id = f.create(title, &live).unwrap();
    s.folder_changed(SyncOperationV1::CreateFolder {
        folder_id: f.sync_id(&id, s.library_id()).unwrap(),
        title: title.into(),
    });
    id
}

fn local_file(s: &OutboundSync, k: &Kindle, nb: &str, folder: Option<&str>) {
    let mut f = crate::folders::FolderStore::open(&k.lib);
    let live = crate::library::open_library_read_only(&k.lib)
        .unwrap()
        .manifest()
        .notebook_ids
        .clone();
    f.set_folder(nb, folder, &live).unwrap();
    s.notebook_folder_set(nb, folder.map(|id| f.sync_id(id, s.library_id()).unwrap()));
}

#[test]
fn folders_sync_both_ways_and_local_only_folders_are_derived_at_startup() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb1, p1) = first_page(&srv, lib);
    // A second notebook (remote-created, materialized here).
    let mut b = Remote::new(lib);
    let nb2 = NotebookId::from_uuid(Uuid::now_v7()).unwrap();
    b.submit(
        &srv,
        vec![SyncOperationV1::CreateNotebook {
            notebook_id: nb2,
            title: "Remote".into(),
            after_notebook_id: Some(nb1),
            first_page: blank_spec(&p1),
        }],
    );
    converge(&s, &srv);

    // 1. Kindle → server: create "Journals", file notebook-1 in it.
    let journals = local_folder_create(&s, &k, "Journals");
    local_file(&s, &k, "notebook-1", Some(&journals));
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    let (_, snap) = srv.snapshot(lib);
    assert_eq!(snap.folders.len(), 1);
    assert_eq!(snap.folders[0].title, "Journals");
    let j_gid = snap.folders[0].folder_id;
    assert_eq!(snap.notebooks[0].folder_id, Some(j_gid));

    // 2. Server → Kindle: a web folder, notebook-1 moved into it, Journals renamed, notebook 2
    //    filed in Journals.
    let web = FolderId::from_uuid(Uuid::now_v7()).unwrap();
    b.submit(
        &srv,
        vec![
            SyncOperationV1::CreateFolder {
                folder_id: web,
                title: "From Web".into(),
            },
            SyncOperationV1::SetNotebookFolder {
                notebook_id: nb1,
                folder_id: Some(web),
            },
            SyncOperationV1::RenameFolder {
                folder_id: j_gid,
                title: "Diaries".into(),
            },
            SyncOperationV1::SetNotebookFolder {
                notebook_id: nb2,
                folder_id: Some(j_gid),
            },
        ],
    );
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    let file = crate::folders::read_folders(&k.lib).unwrap();
    assert_eq!(file.folders.len(), 2);
    // The Kindle folder keeps its local id; the remote one gets the next local id and
    // remembers the remote identity.
    assert_eq!(file.folders[0].id, journals);
    assert_eq!(file.folders[0].title, "Diaries");
    assert_eq!(file.folders[1].sync_id, Some(web));
    assert_eq!(file.folders[1].title, "From Web");
    assert_eq!(file.folders[1].notebook_ids, ["notebook-1"]);
    let nb2_local = crate::library::open_library_read_only(&k.lib)
        .unwrap()
        .manifest()
        .notebook_ids[1]
        .clone();
    assert_eq!(file.folders[0].notebook_ids, [nb2_local]);

    // 3. Remote delete: members return to the top level; no notebook is deleted.
    b.submit(&srv, vec![SyncOperationV1::DeleteFolder { folder_id: web }]);
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    let file = crate::folders::read_folders(&k.lib).unwrap();
    assert_eq!(file.folders.len(), 1);
    assert_eq!(
        crate::library::open_library_read_only(&k.lib)
            .unwrap()
            .manifest()
            .notebook_ids
            .len(),
        2
    );

    // 4. Local-only folders (made before folders synced: no sync_id, never captured) are
    //    derived by startup reconciliation — exactly once.
    s.shutdown(Duration::from_secs(3));
    let mut legacy = crate::folders::read_folders(&k.lib).unwrap();
    let ord = legacy.next_folder_ordinal;
    legacy.next_folder_ordinal += 1;
    legacy.folders.push(crate::folders::FolderV1 {
        id: format!("folder-{ord}"),
        title: "Legacy".into(),
        notebook_ids: vec!["notebook-1".into()],
        sync_id: None,
    });
    crate::folders::write_materialized(&k.lib, &legacy).unwrap();
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    let (_, snap) = srv.snapshot(lib);
    assert_eq!(
        snap.folders
            .iter()
            .map(|f| f.title.as_str())
            .collect::<Vec<_>>(),
        ["Diaries", "Legacy"]
    );
    assert_eq!(snap.notebooks[0].folder_id, Some(snap.folders[1].folder_id));
    let head = srv.head(lib);
    s.shutdown(Duration::from_secs(3));
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    converge(&s, &srv);
    assert_eq!(srv.head(lib), head, "nothing re-derived on the next start");
    s.shutdown(Duration::from_secs(3));
}

// ------------------------------------------------------------------ ReplaceStrokeLineages inbound (Slice 18E.4A)

fn page_stroke_ids(k: &Kindle) -> Vec<String> {
    let (_, loaded) = load_existing_page(&k.lib.join("notebooks/notebook-1"), "page-1").unwrap();
    loaded
        .strokes
        .iter()
        .map(|s| s.id.as_str().to_string())
        .collect()
}

fn visible_globals(k: &Kindle, page: PageId) -> Vec<StrokeId> {
    let (_, loaded) = load_existing_page(&k.lib.join("notebooks/notebook-1"), "page-1").unwrap();
    loaded
        .strokes
        .iter()
        .map(|s| crate::sync_local::kindle_global_stroke_id(page, s.id.as_str()).unwrap())
        .collect()
}

#[test]
fn inbound_replace_preserves_v7_fragment_ids_across_reopen() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let n0 = s.next_client_seq();
    let (nb, p0) = first_page(&srv, lib);
    let orig = remote_stroke(line(1000.0, 5000.0, 9000.0, 0.8));
    let f1 = fragment_of(orig.root_id, 5100.0);
    let f2 = fragment_of(orig.root_id, 5200.0);
    let mut remote = Remote::new(lib);
    remote.submit(&srv, vec![add(nb, p0.page_id, &orig)]);
    converge(&s, &srv);
    remote.submit(
        &srv,
        vec![replace_one(
            nb,
            p0.page_id,
            &orig,
            vec![f1.clone(), f2.clone()],
        )],
    );
    converge(&s, &srv);
    let ids = visible_globals(&k, p0.page_id);
    assert!(ids.contains(&f1.stroke_id), "{ids:?}");
    assert!(ids.contains(&f2.stroke_id), "{ids:?}");
    assert!(!ids.contains(&orig.stroke_id));
    assert_eq!(
        local_stroke_id_for_global(f1.stroke_id),
        page_stroke_ids(&k)
            .into_iter()
            .find(
                |l| crate::sync_local::kindle_global_stroke_id(p0.page_id, l).unwrap()
                    == f1.stroke_id
            )
            .unwrap()
    );
    assert_eq!(s.next_client_seq(), n0);
    assert_eq!(s.server_cursor(), srv.head(lib));
    s.shutdown(Duration::from_secs(3));
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    converge(&s, &srv);
    let ids = visible_globals(&k, p0.page_id);
    assert_eq!(
        ids.iter()
            .filter(|id| **id == f1.stroke_id || **id == f2.stroke_id)
            .count(),
        2
    );
    assert_match(&k, &s, &srv);
}

#[test]
fn inbound_multi_root_replace_is_atomic_across_reopen() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    let a = remote_stroke(line(1000.0, 2000.0, 3000.0, 0.5));
    let b = remote_stroke(line(1000.0, 4000.0, 3000.0, 0.5));
    let c = remote_stroke(line(1000.0, 6000.0, 3000.0, 0.5));
    let fa = fragment_of(a.root_id, 2100.0);
    let fb = fragment_of(b.root_id, 4100.0);
    let fc = fragment_of(c.root_id, 6100.0);
    let mut remote = Remote::new(lib);
    remote.submit(
        &srv,
        vec![
            add(nb, p0.page_id, &a),
            add(nb, p0.page_id, &b),
            add(nb, p0.page_id, &c),
        ],
    );
    converge(&s, &srv);
    let mut reps = vec![
        LineageReplacement {
            root_id: a.root_id,
            expected: vec![a.clone()],
            after: vec![fa.clone()],
        },
        LineageReplacement {
            root_id: b.root_id,
            expected: vec![b.clone()],
            after: vec![fb.clone()],
        },
        LineageReplacement {
            root_id: c.root_id,
            expected: vec![c.clone()],
            after: vec![fc.clone()],
        },
    ];
    reps.sort_by_key(|r| r.root_id);
    remote.submit(
        &srv,
        vec![SyncOperationV1::ReplaceStrokeLineages {
            notebook_id: nb,
            page_id: p0.page_id,
            replacements: reps,
        }],
    );
    converge(&s, &srv);
    s.shutdown(Duration::from_secs(3));
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    let ids = visible_globals(&k, p0.page_id);
    for id in [fa.stroke_id, fb.stroke_id, fc.stroke_id] {
        assert!(ids.contains(&id), "missing {id} in {ids:?}");
    }
    for id in [a.stroke_id, b.stroke_id, c.stroke_id] {
        assert!(!ids.contains(&id));
    }
    assert_match(&k, &s, &srv);
}

#[test]
fn inbound_replace_preserves_unrelated_local_and_remote_strokes() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    let local = kindle_write(&s, &k, "notebook-1", "page-1", 1500.0);
    converge(&s, &srv);
    let orig = remote_stroke(line(1000.0, 5000.0, 9000.0, 0.8));
    let extra = remote_stroke(line(1000.0, 8000.0, 3000.0, 0.4));
    let f1 = fragment_of(orig.root_id, 5100.0);
    let mut remote = Remote::new(lib);
    remote.submit(
        &srv,
        vec![add(nb, p0.page_id, &orig), add(nb, p0.page_id, &extra)],
    );
    converge(&s, &srv);
    remote.submit(
        &srv,
        vec![replace_one(nb, p0.page_id, &orig, vec![f1.clone()])],
    );
    converge(&s, &srv);
    let ids = visible_globals(&k, p0.page_id);
    let local_gid =
        crate::sync_local::kindle_global_stroke_id(p0.page_id, local.id.as_str()).unwrap();
    assert!(ids.contains(&local_gid));
    assert!(ids.contains(&extra.stroke_id));
    assert!(ids.contains(&f1.stroke_id));
    assert_match(&k, &s, &srv);
}

#[test]
fn inbound_replace_conflicts_with_local_erase_of_the_same_root() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    let orig = remote_stroke(line(1000.0, 3000.0, 9000.0, 0.8));
    let mut remote = Remote::new(lib);
    remote.submit(&srv, vec![add(nb, p0.page_id, &orig)]);
    converge(&s, &srv);
    s.shutdown(Duration::from_secs(3));
    let _ = fs::remove_file(k.sync.join(CONFIG_FILE));
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    kindle_erase(&s, &k, "notebook-1", "page-1", 2000.0, 2000.0, 4000.0);
    wait("erase pending", || s.pending() == 1);
    let before = local_snapshot(&k.lib);
    let cursor = s.server_cursor();
    let f_far = fragment_of(orig.root_id, 9000.0);
    remote.submit(&srv, vec![replace_one(nb, p0.page_id, &orig, vec![f_far])]);
    let cfg = SyncConfigV1 {
        v: 1,
        server_base_url: srv.url(),
        token: srv.token.clone(),
        debug_drop_first_ack: false,
        debug_crash_point: None,
    };
    assert!(super::inbound::pull_once(&s.shared, &agent(), &cfg)
        .ok()
        .unwrap());
    let e = s.apply_inbound().unwrap_err();
    assert!(e.contains("LOCAL_REBASE_CONFLICT"), "{e}");
    assert!(s.fault().unwrap().contains("LOCAL_REBASE_CONFLICT"));
    assert_eq!(local_snapshot(&k.lib), before);
    assert_eq!(s.server_cursor(), cursor);
    assert_eq!(s.pending(), 1);
    assert_eq!(s.inbox_len(), 1);
    s.shutdown(Duration::from_secs(3));
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    assert_eq!(
        (s.pending(), s.inbox_len(), s.server_cursor()),
        (1, 1, cursor),
        "conflict leaves outbox, inbox, and cursor durable"
    );
    let _ = s;
}

#[test]
fn inbound_replace_then_delete_page_replays_without_resurrection() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    let orig = remote_stroke(line(1000.0, 5000.0, 9000.0, 0.8));
    let f1 = fragment_of(orig.root_id, 5100.0);
    let extra_page = blank_spec(&p0);
    let mut remote = Remote::new(lib);
    remote.submit(
        &srv,
        vec![
            SyncOperationV1::CreatePage {
                notebook_id: nb,
                page: extra_page.clone(),
                after_page_id: Some(p0.page_id),
            },
            add(nb, p0.page_id, &orig),
        ],
    );
    converge(&s, &srv);
    remote.submit(
        &srv,
        vec![
            replace_one(nb, p0.page_id, &orig, vec![f1]),
            SyncOperationV1::DeletePage {
                notebook_id: nb,
                page_id: p0.page_id,
            },
        ],
    );
    converge(&s, &srv);
    let snap = local_snapshot(&k.lib);
    assert!(snap.notebooks[0]
        .pages
        .iter()
        .all(|p| p.page_id != p0.page_id));
    s.shutdown(Duration::from_secs(3));
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    assert!(local_snapshot(&k.lib).notebooks[0]
        .pages
        .iter()
        .all(|p| p.page_id != p0.page_id));
    let head = srv.head(lib);
    let env = remote.env(replace_one(
        nb,
        p0.page_id,
        &orig,
        vec![fragment_of(orig.root_id, 1.0)],
    ));
    let mut body = b"{\"envelopes\":[".to_vec();
    body.extend(env.to_canonical_bytes().unwrap());
    body.extend(b"]}");
    let (st, resp) = srv.call(
        "POST",
        &format!("/v1/libraries/{lib}/operations"),
        Some(&body),
    );
    assert_ne!(
        st,
        200,
        "new replace on a deleted page: {}",
        String::from_utf8_lossy(&resp)
    );
    assert_eq!(srv.head(lib), head);
    assert!(local_snapshot(&k.lib).notebooks[0]
        .pages
        .iter()
        .all(|p| p.page_id != p0.page_id));
}

#[test]
fn inbound_replace_duplicate_delivery_does_not_duplicate_fragments() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    let orig = remote_stroke(line(1000.0, 5000.0, 9000.0, 0.8));
    let f1 = fragment_of(orig.root_id, 5100.0);
    let f2 = fragment_of(orig.root_id, 5200.0);
    let mut remote = Remote::new(lib);
    remote.submit(&srv, vec![add(nb, p0.page_id, &orig)]);
    converge(&s, &srv);
    remote.submit(
        &srv,
        vec![replace_one(
            nb,
            p0.page_id,
            &orig,
            vec![f1.clone(), f2.clone()],
        )],
    );
    converge(&s, &srv);
    let once = visible_globals(&k, p0.page_id);
    s.shutdown(Duration::from_secs(3));
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    converge(&s, &srv);
    assert_eq!(visible_globals(&k, p0.page_id), once);
    let n_f = once
        .iter()
        .filter(|id| **id == f1.stroke_id || **id == f2.stroke_id)
        .count();
    assert_eq!(n_f, 2);
    assert_match(&k, &s, &srv);
}

#[test]
fn inbound_replace_invalidates_verified_stamp_before_journal_write() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    converge(&s, &srv);
    s.shutdown(Duration::from_secs(3));
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    assert!(
        k.sync.join(crate::sync_verified::STAMP_FILE).exists(),
        "trusted stamp after clean reopen"
    );
    let orig = remote_stroke(line(1000.0, 5000.0, 9000.0, 0.8));
    let f1 = fragment_of(orig.root_id, 5100.0);
    let mut remote = Remote::new(lib);
    remote.submit(&srv, vec![add(nb, p0.page_id, &orig)]);
    converge(&s, &srv);
    s.shutdown(Duration::from_secs(3));
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    assert!(
        k.sync.join(crate::sync_verified::STAMP_FILE).exists(),
        "trusted stamp after add is applied"
    );
    remote.submit(&srv, vec![replace_one(nb, p0.page_id, &orig, vec![f1])]);
    converge(&s, &srv);
    assert!(
        !k.sync.join(crate::sync_verified::STAMP_FILE).exists(),
        "inbound replace must invalidate the stamp"
    );
    assert!(k
        .sync
        .join(crate::sync_verified::STAMP_CLEARED_FILE)
        .exists());
    assert_match(&k, &s, &srv);
}

#[test]
fn inbound_replace_crash_windows_recover_exactly_once() {
    for point in [
        "after_inbox_fsync",
        "after_marker",
        "after_erase_journal",
        "after_verify",
        "before_cursor_write",
        "after_cursor_write",
    ] {
        crash_and_recover_replace(point);
    }
}

/// Like [`crash_and_recover`], but the orig stroke is already local so the replacement
/// materializes as one `replace_strokes` journal record (the multi-root atomic write).
fn crash_and_recover_replace(point: &str) {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    kindle_write(&s, &k, "notebook-1", "page-1", 1500.0);
    converge(&s, &srv);
    let n = s.next_client_seq();
    let orig = remote_stroke(line(1000.0, 9000.0, 9000.0, 0.8));
    let f1 = fragment_of(orig.root_id, 9100.0);
    let f2 = fragment_of(orig.root_id, 9200.0);
    let mut b = Remote::new(lib);
    b.submit(&srv, vec![add(nb, p0.page_id, &orig)]);
    converge(&s, &srv);
    s.set_crash_point(Some(point));
    b.submit(&srv, vec![replace_one(nb, p0.page_id, &orig, vec![f1, f2])]);
    let t0 = Instant::now();
    while s.fault().is_none() {
        if s.inbound_ready() {
            let _ = s.apply_inbound();
        }
        s.kick();
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "crash point {point} never hit"
        );
    }
    assert!(s.fault().unwrap().contains(point), "{:?}", s.fault());
    drop(s);
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
    assert_eq!(s.next_client_seq(), n, "{point}: no echo");
    assert_eq!(
        srv.head(lib),
        1 + b.next - 1,
        "{point}: nothing uploaded by recovery"
    );
    let before = local_hash(&k.lib);
    drop(s);
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    assert!(s.fault().is_none());
    converge(&s, &srv);
    assert_eq!(local_hash(&k.lib), before);
    let _ = s;
}

#[test]
fn inbound_history_add_erase_replace_unrelated_and_page_ops() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let n0 = s.next_client_seq();
    let (nb, p0) = first_page(&srv, lib);
    let a = remote_stroke(line(1000.0, 2000.0, 4000.0, 0.5));
    let b = remote_stroke(line(1000.0, 5000.0, 4000.0, 0.5));
    let extra = remote_stroke(line(1000.0, 8000.0, 2000.0, 0.3));
    let fb = fragment_of(b.root_id, 5100.0);
    let pg = blank_spec(&p0);
    let mut remote = Remote::new(lib);
    remote.submit(&srv, vec![add(nb, p0.page_id, &a), add(nb, p0.page_id, &b)]);
    converge(&s, &srv);
    remote.submit(
        &srv,
        vec![erase_vertical(
            nb,
            p0.page_id,
            vec![a.stroke_id],
            2000.0,
            1500.0,
            2500.0,
        )],
    );
    converge(&s, &srv);
    remote.submit(
        &srv,
        vec![replace_one(nb, p0.page_id, &b, vec![fb.clone()])],
    );
    converge(&s, &srv);
    remote.submit(
        &srv,
        vec![
            add(nb, p0.page_id, &extra),
            SyncOperationV1::CreatePage {
                notebook_id: nb,
                page: pg.clone(),
                after_page_id: Some(p0.page_id),
            },
        ],
    );
    converge(&s, &srv);
    assert_eq!(s.next_client_seq(), n0);
    let ids = visible_globals(&k, p0.page_id);
    assert!(ids.contains(&fb.stroke_id));
    assert!(ids.contains(&extra.stroke_id));
    s.shutdown(Duration::from_secs(3));
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
}

#[test]
fn inbound_review80_shape_materializes_accepted_history() {
    let srv = server();
    let (k, s) = kindle(&srv);
    let lib = s.state().library_id;
    let (nb, p0) = first_page(&srv, lib);
    let extra_page = blank_spec(&p0);
    let filled = blank_spec(&p0);
    let mut remote = Remote::new(lib);
    let mut extras = Vec::new();
    for i in 0..114 {
        extras.push(remote_stroke(line(
            200.0,
            200.0 + i as f32 * 40.0,
            800.0,
            0.3,
        )));
    }
    let gone1 = remote_stroke(line(1000.0, 8000.0, 3000.0, 0.6));
    let gone2 = remote_stroke(line(1000.0, 8500.0, 3000.0, 0.6));
    let f1 = fragment_of(gone1.root_id, 8010.0);
    let f2 = fragment_of(gone1.root_id, 8020.0);
    let f3 = fragment_of(gone2.root_id, 8510.0);
    let f4 = fragment_of(gone2.root_id, 8520.0);
    let mut ops = Vec::new();
    for e in &extras {
        ops.push(add(nb, p0.page_id, e));
    }
    ops.push(add(nb, p0.page_id, &gone1));
    ops.push(add(nb, p0.page_id, &gone2));
    remote.submit(&srv, ops);
    converge(&s, &srv);
    let mut reps = vec![
        LineageReplacement {
            root_id: gone1.root_id,
            expected: vec![gone1.clone()],
            after: vec![f1.clone(), f2.clone()],
        },
        LineageReplacement {
            root_id: gone2.root_id,
            expected: vec![gone2.clone()],
            after: vec![f3.clone(), f4.clone()],
        },
    ];
    reps.sort_by_key(|r| r.root_id);
    remote.submit(
        &srv,
        vec![
            SyncOperationV1::ReplaceStrokeLineages {
                notebook_id: nb,
                page_id: p0.page_id,
                replacements: reps,
            },
            SyncOperationV1::CreatePage {
                notebook_id: nb,
                page: extra_page.clone(),
                after_page_id: Some(p0.page_id),
            },
            SyncOperationV1::CreatePage {
                notebook_id: nb,
                page: filled.clone(),
                after_page_id: Some(extra_page.page_id),
            },
            add(
                nb,
                filled.page_id,
                &remote_stroke(line(500.0, 500.0, 400.0, 0.4)),
            ),
            add(
                nb,
                filled.page_id,
                &remote_stroke(line(500.0, 900.0, 400.0, 0.4)),
            ),
        ],
    );
    converge(&s, &srv);
    let ids = visible_globals(&k, p0.page_id);
    for id in [f1.stroke_id, f2.stroke_id, f3.stroke_id, f4.stroke_id] {
        assert!(ids.contains(&id), "missing fragment {id}");
    }
    assert_eq!(
        ids.iter()
            .filter(|id| extras.iter().any(|e| e.stroke_id == **id))
            .count(),
        114
    );
    let snap = local_snapshot(&k.lib);
    assert!(snap.notebooks[0]
        .pages
        .iter()
        .any(|p| p.page_id == extra_page.page_id && p.strokes.is_empty()));
    assert!(snap.notebooks[0]
        .pages
        .iter()
        .any(|p| p.page_id == filled.page_id && p.strokes.len() == 2));
    s.shutdown(Duration::from_secs(3));
    let s = OutboundSync::open(&k.lib, &k.sync).unwrap();
    converge(&s, &srv);
    assert_match(&k, &s, &srv);
}

#[test]
fn inbound_parse_tail_decodes_replace_stroke_lineages() {
    let lib = LibraryId::from_uuid(Uuid::now_v7()).unwrap();
    let mut r = Remote::new(lib);
    let nb = NotebookId::from_uuid(Uuid::now_v7()).unwrap();
    let pg = PageId::from_uuid(Uuid::now_v7()).unwrap();
    let orig = remote_stroke(line(10.0, 10.0, 50.0, 0.5));
    let env = r.env(replace_one(
        nb,
        pg,
        &orig,
        vec![fragment_of(orig.root_id, 20.0)],
    ));
    let (_, items) = parse_tail(&tail_body(lib, &[(1, &env)]), lib, 0).unwrap();
    assert_eq!(items.len(), 1);
    assert!(matches!(
        items[0].envelope.operation,
        SyncOperationV1::ReplaceStrokeLineages { .. }
    ));
}
