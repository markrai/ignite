//! S3B: the browser bridge produces byte-identical canonical envelopes to native Rust, over
//! every sequence boundary. The same cases are pinned into `ignite/web/tests/fixtures/
//! wasm-envelopes-v1.json`, which the web tests replay through the built `.wasm`.

use std::path::{Path, PathBuf};

use scribe_web_wasm::{handle, uuid_v7};
use serde_json::{json, Value};
use sync_protocol::{
    BackgroundRef, FolderId, LibraryId, NotebookId, OperationId, PageId, PageSpec, ReplicaId,
    SnapshotFolder, StrokeId, StrokeStyle, SyncEnvelopeV1, SyncOperationV1, SyncPoint, SyncStroke,
    SyncText, TextFace, TextId, ENVELOPE_VERSION,
};

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/tests/fixtures/wasm-envelopes-v1.json")
}

fn id(n: u8) -> uuid::Uuid {
    uuid_v7(
        1_790_000_000_000 + u64::from(n),
        &[n, 1, 2, 3, 4, 5, 6, 7, 8, n],
    )
}

struct Ids {
    lib: LibraryId,
    rep: ReplicaId,
    op: OperationId,
    nb: NotebookId,
    pg: PageId,
    st: StrokeId,
}

fn ids() -> Ids {
    Ids {
        lib: LibraryId::from_uuid(id(1)).unwrap(),
        rep: ReplicaId::from_uuid(id(2)).unwrap(),
        op: OperationId::from_uuid(id(3)).unwrap(),
        nb: NotebookId::from_uuid(id(4)).unwrap(),
        pg: PageId::from_uuid(id(5)).unwrap(),
        st: StrokeId::from_uuid(id(6)).unwrap(),
    }
}

/// Native construction, independent of the bridge.
fn native(i: &Ids, client_seq: u64, style: StrokeStyle, pts: &[(f32, f32, Option<f32>)]) -> String {
    let env = SyncEnvelopeV1 {
        v: ENVELOPE_VERSION,
        operation_id: i.op,
        library_id: i.lib,
        replica_id: i.rep,
        client_seq,
        operation: SyncOperationV1::AddStroke {
            notebook_id: i.nb,
            page_id: i.pg,
            stroke: SyncStroke {
                stroke_id: i.st,
                root_id: i.st,
                style,
                points: pts
                    .iter()
                    .map(|&(x, y, p)| SyncPoint::new(x, y, p))
                    .collect(),
            },
        },
    };
    String::from_utf8(env.to_canonical_bytes().unwrap()).unwrap()
}

fn request(i: &Ids, client_seq: &str, style: &str, pts: &[(f32, f32, Option<f32>)]) -> Value {
    json!({
        "fn": "encode_add_stroke",
        "library_id": i.lib.to_string(),
        "replica_id": i.rep.to_string(),
        "client_seq": client_seq,
        "operation_id": i.op.to_string(),
        "notebook_id": i.nb.to_string(),
        "page_id": i.pg.to_string(),
        "stroke_id": i.st.to_string(),
        "style": style,
        "points": pts.iter().map(|&(x, y, p)| match p {
            Some(p) => json!({"x": f64::from(x), "y": f64::from(y), "pressure": f64::from(p)}),
            None => json!({"x": f64::from(x), "y": f64::from(y)}),
        }).collect::<Vec<_>>(),
    })
}

fn cases() -> Vec<Value> {
    let i = ids();
    let pressure: Vec<(f32, f32, Option<f32>)> = vec![
        (0.0, 0.0, Some(0.0)),
        (1.5, 2.25, Some(0.125)),
        (7812.3456, 10416.789, Some(0.5)),
        (15624.0, 20832.0, Some(1.0)),
        (0.1, 0.2, Some(0.333_333_34)),
        (1e-3, 20831.999, Some(0.999_999_94)),
    ];
    let legacy: Vec<(f32, f32, Option<f32>)> = vec![(100.0, 200.0, None), (300.75, 400.5, None)];
    let mixed: Vec<(f32, f32, Option<f32>)> = vec![(10.0, 10.0, None), (20.0, 20.0, Some(0.7))];
    let mut out = Vec::new();
    let seqs: [(&str, u64); 7] = [
        ("1", 1),
        ("9007199254740991", (1 << 53) - 1),
        ("9007199254740992", 1 << 53),
        ("9007199254740993", (1 << 53) + 1),
        ("9223372036854775807", i64::MAX as u64),
        ("9223372036854775808", i64::MAX as u64 + 1),
        ("18446744073709551615", u64::MAX),
    ];
    for (text, v) in seqs {
        out.push(json!({
            "name": format!("pressure client_seq {text}"),
            "request": request(&i, text, "pressure-width-v1", &pressure),
            "ok": native(&i, v, StrokeStyle::PressureWidthV1, &pressure),
        }));
    }
    // SEQ20 zero-padded form is accepted and encodes the same value.
    out.push(json!({
        "name": "SEQ20 padded client_seq",
        "request": request(&i, "00000000000000000002", "pressure-width-v1", &pressure),
        "ok": native(&i, 2, StrokeStyle::PressureWidthV1, &pressure),
    }));
    out.push(json!({
        "name": "legacy fixed mouse stroke",
        "request": request(&i, "3", "legacy-fixed-v1", &legacy),
        "ok": native(&i, 3, StrokeStyle::LegacyFixedV1, &legacy),
    }));
    out.push(json!({
        "name": "missing pressure on some samples",
        "request": request(&i, "4", "pressure-width-v1", &mixed),
        "ok": native(&i, 4, StrokeStyle::PressureWidthV1, &mixed),
    }));
    out.push(json!({
        "name": "single sample",
        "request": request(&i, "5", "pressure-width-v1", &[(5.0, 6.0, Some(0.4))]),
        "ok": native(&i, 5, StrokeStyle::PressureWidthV1, &[(5.0, 6.0, Some(0.4))]),
    }));
    for (name, seq) in [
        ("zero client_seq", "0"),
        ("u64 overflow", "18446744073709551616"),
        ("exponent", "1e3"),
        ("negative", "-1"),
        ("fraction", "1.5"),
        ("empty", ""),
    ] {
        out.push(json!({"name": name, "request": request(&i, seq, "pressure-width-v1", &pressure), "err": true}));
    }
    out.push(json!({"name": "point off page", "request": request(&i, "1", "pressure-width-v1", &[(15625.0, 1.0, Some(0.5))]), "err": true}));
    out.push(json!({"name": "pressure above 1", "request": request(&i, "1", "pressure-width-v1", &[(1.0, 1.0, Some(1.5))]), "err": true}));
    out.push(json!({"name": "no points", "request": request(&i, "1", "pressure-width-v1", &[]), "err": true}));
    let mut bad_id = request(&i, "1", "pressure-width-v1", &pressure);
    bad_id["stroke_id"] = json!("00000000-0000-1000-8000-000000000001"); // v1: not allowed
    out.push(json!({"name": "stroke id with unsupported version", "request": bad_id, "err": true}));
    let mut other_op = request(&i, "1", "pressure-width-v1", &pressure);
    other_op["fn"] = json!("encode_add_stroke");
    other_op["fn"] = json!("no_such_fn");
    out.push(json!({"name": "unknown encode fn refused", "request": other_op, "err": true}));
    out.extend(erase_cases(&i));
    out.extend(projection_cases());
    out.extend(folder_cases(&i));
    out.extend(text_cases(&i));
    out.extend(structure_cases(&i));
    // ID minting (fixed inputs → exact v7 text).
    for kind in ["replica", "operation", "stroke", "text", "notebook", "page"] {
        out.push(json!({
            "name": format!("mint {kind}"),
            "request": {"fn": "mint_id", "kind": kind, "unix_ms": "1790000000123", "random_hex": "00112233445566778899"},
            "ok": uuid_v7(1_790_000_000_123, &[0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99]).to_string(),
        }));
    }
    out.push(json!({"name": "mint bad randomness", "request": {"fn": "mint_id", "kind": "stroke", "unix_ms": "1", "random_hex": "zz"}, "err": true}));
    out.push(json!({"name": "mint unknown kind", "request": {"fn": "mint_id", "kind": "library", "unix_ms": "1", "random_hex": "00112233445566778899"}, "err": true}));
    out
}

/// Native `EraseRegion` construction, independent of the bridge (radius = the Kindle eraser's).
fn native_erase(i: &Ids, client_seq: u64, roots: &[StrokeId], path: &[(f32, f32)]) -> String {
    let mut roots = roots.to_vec();
    roots.sort();
    roots.dedup();
    let env = SyncEnvelopeV1 {
        v: ENVELOPE_VERSION,
        operation_id: i.op,
        library_id: i.lib,
        replica_id: i.rep,
        client_seq,
        operation: SyncOperationV1::EraseRegion {
            notebook_id: i.nb,
            page_id: i.pg,
            observed_roots: roots,
            path: path
                .iter()
                .map(|&(x, y)| SyncPoint::new(x, y, None))
                .collect(),
            radius: notebook_core::ERASER_HIT_RADIUS_CANONICAL,
        },
    };
    String::from_utf8(env.to_canonical_bytes().unwrap()).unwrap()
}

fn erase_request(i: &Ids, client_seq: &str, roots: &[StrokeId], path: &[(f32, f32)]) -> Value {
    json!({
        "fn": "encode_erase_region",
        "library_id": i.lib.to_string(),
        "replica_id": i.rep.to_string(),
        "client_seq": client_seq,
        "operation_id": i.op.to_string(),
        "notebook_id": i.nb.to_string(),
        "page_id": i.pg.to_string(),
        "observed_roots": roots.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
        "path": path.iter().map(|&(x, y)| json!({"x": f64::from(x), "y": f64::from(y)})).collect::<Vec<_>>(),
    })
}

fn erase_cases(i: &Ids) -> Vec<Value> {
    let r = |n: u8| StrokeId::from_uuid(id(40 + n)).unwrap();
    // Deliberately unsorted with a duplicate: the bridge must emit the sorted, unique form.
    let roots = [r(3), r(1), r(2), r(1)];
    let path = [(5000.0, 2500.0), (5000.25, 2900.5), (5001.0, 3500.0)];
    let mut out = Vec::new();
    for (text, v) in [
        ("1", 1),
        ("9007199254740993", (1u64 << 53) + 1),
        ("9223372036854775808", i64::MAX as u64 + 1),
        ("18446744073709551615", u64::MAX),
    ] {
        out.push(json!({
            "name": format!("erase client_seq {text}"),
            "request": erase_request(i, text, &roots, &path),
            "ok": native_erase(i, v, &roots, &path),
        }));
    }
    out.push(json!({
        "name": "erase single-sample tap",
        "request": erase_request(i, "7", &[r(1)], &[(1500.0, 5000.0)]),
        "ok": native_erase(i, 7, &[r(1)], &[(1500.0, 5000.0)]),
    }));
    out.push(json!({"name": "erase with no observed roots", "request": erase_request(i, "1", &[], &path), "err": true}));
    out.push(json!({"name": "erase with empty path", "request": erase_request(i, "1", &roots, &[]), "err": true}));
    out.push(json!({"name": "erase path off page", "request": erase_request(i, "1", &roots, &[(-1.0, 5.0)]), "err": true}));
    let mut radius = erase_request(i, "1", &roots, &path);
    radius["radius"] = json!(1000.0); // the browser can never choose the radius
    out.push(
        json!({"name": "erase radius is not a browser input", "request": radius, "err": true}),
    );
    let mut pressured = erase_request(i, "1", &roots, &path);
    pressured["path"][0]["pressure"] = json!(0.5);
    out.push(json!({"name": "erase path carries no pressure", "request": pressured, "err": true}));
    let mut bad_root = erase_request(i, "1", &roots, &path);
    bad_root["observed_roots"][0] = json!("not-a-uuid");
    out.push(json!({"name": "erase bad root id", "request": bad_root, "err": true}));
    out
}

/// `project_page` cases whose expected output comes from the REDUCER (`LibraryState::apply`),
/// an oracle independent of the bridge's projection code.
fn projection_cases() -> Vec<Value> {
    use std::collections::BTreeSet;
    use sync_engine::testkit::{self as tk, Author};
    use sync_engine::{LibraryState, Outcome};

    let genesis = tk::genesis();
    let page = genesis.notebooks[0].pages[0].clone();
    let (nb, pg) = (genesis.notebooks[0].notebook_id, page.page_id);
    let all_roots: Vec<StrokeId> = page
        .strokes
        .iter()
        .map(|s| s.root_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let sk1 = tk::sk(1);
    let pending = tk::line(
        tk::sk(10),
        tk::sk(10),
        (2000.0, 8000.0),
        (8000.0, 8000.0),
        13,
        true,
    );
    let mut with_pending = all_roots.clone();
    with_pending.push(tk::sk(10));
    with_pending.sort();

    enum Step {
        Add(SyncStroke),
        Erase(Vec<StrokeId>, Vec<(f32, f32)>),
        DeleteLineage(StrokeId),
    }
    let scenarios: Vec<(&str, Vec<Step>)> = vec![
        (
            "cut one stroke in two",
            vec![Step::Erase(
                all_roots.clone(),
                vec![(5000.0, 2500.0), (5000.0, 3500.0)],
            )],
        ),
        (
            "observed-roots rule: unobserved stroke untouched",
            vec![Step::Erase(
                all_roots.iter().copied().filter(|r| *r != sk1).collect(),
                vec![(5000.0, 2500.0), (5000.0, 3500.0)],
            )],
        ),
        (
            "erase touching nothing",
            vec![Step::Erase(
                all_roots.clone(),
                vec![(14000.0, 19000.0), (14500.0, 19500.0)],
            )],
        ),
        (
            "single-sample tap on a fragment",
            vec![Step::Erase(all_roots.clone(), vec![(1500.0, 5000.0)])],
        ),
        (
            "sweep along a whole stroke removes it",
            vec![Step::Erase(
                all_roots.clone(),
                vec![(900.0, 1000.0), (3000.0, 1000.0), (5100.0, 1000.0)],
            )],
        ),
        (
            "pending add then erase across it",
            vec![
                Step::Add(pending.clone()),
                Step::Erase(
                    with_pending.clone(),
                    vec![(5000.0, 7700.0), (5000.0, 8300.0)],
                ),
            ],
        ),
        (
            "erase, add, erase again (fragments of fragments)",
            vec![
                Step::Erase(all_roots.clone(), vec![(3000.0, 2600.0), (3000.0, 3400.0)]),
                Step::Add(pending.clone()),
                Step::Erase(
                    with_pending.clone(),
                    vec![(4500.0, 8500.0), (4500.0, 2500.0)],
                ),
            ],
        ),
        (
            "undo newest pending lineage",
            vec![Step::Add(pending.clone()), Step::DeleteLineage(tk::sk(10))],
        ),
        (
            "undo a fragment by root_id",
            vec![Step::DeleteLineage(
                page.strokes
                    .iter()
                    .find(|s| s.stroke_id != s.root_id)
                    .map(|s| s.root_id)
                    .unwrap_or(page.strokes[0].root_id),
            )],
        ),
    ];

    let mut out = Vec::new();
    for (name, steps) in scenarios {
        let mut author = Author::new(tk::replica(7));
        let mut state = LibraryState::genesis(genesis.clone()).unwrap();
        let mut ops = Vec::new();
        let mut changed = Vec::new();
        for step in &steps {
            let (op_json, operation) = match step {
                Step::Add(s) => (
                    json!({"op": "add", "stroke": s}),
                    SyncOperationV1::AddStroke {
                        notebook_id: nb,
                        page_id: pg,
                        stroke: s.clone(),
                    },
                ),
                Step::Erase(roots, path) => {
                    let pts: Vec<SyncPoint> = path
                        .iter()
                        .map(|&(x, y)| SyncPoint::new(x, y, None))
                        .collect();
                    let env_preview = author.op_id(author.next_seq);
                    (
                        json!({
                            "op": "erase",
                            "operation_id": env_preview.to_string(),
                            "observed_roots": roots.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
                            "path": path.iter().map(|&(x, y)| json!({"x": f64::from(x), "y": f64::from(y)})).collect::<Vec<_>>(),
                        }),
                        SyncOperationV1::EraseRegion {
                            notebook_id: nb,
                            page_id: pg,
                            observed_roots: roots.clone(),
                            path: pts,
                            radius: notebook_core::ERASER_HIT_RADIUS_CANONICAL,
                        },
                    )
                }
                Step::DeleteLineage(root) => (
                    json!({"op": "delete_lineage", "root_id": root.to_string()}),
                    SyncOperationV1::DeleteStrokeLineage {
                        notebook_id: nb,
                        page_id: pg,
                        root_id: *root,
                    },
                ),
            };
            let env = author.env(operation);
            let (next, outcome) = state.apply(&env).unwrap();
            changed.push(matches!(outcome, Outcome::Applied) && next.snapshot != state.snapshot);
            state = next;
            ops.push(op_json);
        }
        // Typed serialization: exact f32 text and field order, as the bridge emits.
        #[derive(serde::Serialize)]
        struct Expected<'a> {
            strokes: &'a [SyncStroke],
            changed: &'a [bool],
        }
        let strokes = &state.snapshot.notebooks[0].pages[0].strokes;
        out.push(json!({
            "name": format!("project: {name}"),
            "request": {"fn": "project_page", "strokes": page.strokes, "ops": ops},
            "ok": serde_json::to_string(&Expected { strokes, changed: &changed }).unwrap(),
        }));
    }
    out.push(json!({
        "name": "project: off-page erase path refused",
        "request": {"fn": "project_page", "strokes": page.strokes, "ops": [{"op": "erase", "operation_id": tk::replica(1).to_string(), "observed_roots": [sk1.to_string()], "path": [{"x": 99999.0, "y": 1.0}]}]},
        "err": true,
    }));
    out
}

/// Folder operations: native canonical bytes, refusals, and the reducer-backed projection.
fn folder_cases(i: &Ids) -> Vec<Value> {
    let f1 = FolderId::from_uuid(id(20)).unwrap();
    let f2 = FolderId::from_uuid(id(21)).unwrap();
    let nb2 = NotebookId::from_uuid(id(22)).unwrap();
    let native = |client_seq: u64, operation: SyncOperationV1| {
        let env = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: i.op,
            library_id: i.lib,
            replica_id: i.rep,
            client_seq,
            operation,
        };
        String::from_utf8(env.to_canonical_bytes().unwrap()).unwrap()
    };
    let request = |client_seq: &str, operation: &SyncOperationV1| {
        json!({
            "fn": "encode_folder_op",
            "library_id": i.lib.to_string(),
            "replica_id": i.rep.to_string(),
            "client_seq": client_seq,
            "operation_id": i.op.to_string(),
            "operation": operation,
        })
    };
    let ops = [
        (
            "create folder",
            SyncOperationV1::CreateFolder {
                folder_id: f1,
                title: "Journals".into(),
            },
        ),
        (
            "rename folder",
            SyncOperationV1::RenameFolder {
                folder_id: f1,
                title: "Diaries ✍".into(),
            },
        ),
        (
            "file notebook",
            SyncOperationV1::SetNotebookFolder {
                notebook_id: i.nb,
                folder_id: Some(f1),
            },
        ),
        (
            "unfile notebook",
            SyncOperationV1::SetNotebookFolder {
                notebook_id: i.nb,
                folder_id: None,
            },
        ),
        (
            "delete folder",
            SyncOperationV1::DeleteFolder { folder_id: f1 },
        ),
    ];
    let mut out = Vec::new();
    for (name, op) in &ops {
        out.push(json!({
            "name": format!("folder: {name}"),
            "request": request("18446744073709551615", op),
            "ok": native(u64::MAX, op.clone()),
        }));
    }
    out.push(json!({
        "name": "folder: empty title refused",
        "request": request("1", &SyncOperationV1::CreateFolder { folder_id: f1, title: "  ".into() }),
        "err": true,
    }));
    out.push(json!({
        "name": "folder: only folder operations",
        "request": request("1", &SyncOperationV1::DeleteNotebook { notebook_id: i.nb }),
        "err": true,
    }));
    out.push(json!({
        "name": "folder: mint folder id",
        "request": {"fn": "mint_id", "kind": "folder", "unix_ms": "1790000000000", "random_hex": "00112233445566778899"},
        "ok": FolderId::from_uuid(uuid_v7(1_790_000_000_000, &[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99])).unwrap().to_string(),
    }));

    // Projection: the expected result comes from the full reducer, not the bridge.
    let mut snap = sync_engine::testkit::genesis();
    snap.library_id = i.lib;
    let mut author = sync_engine::testkit::Author::new(i.rep);
    author.library = i.lib;
    let n1 = snap.notebooks[0].notebook_id;
    let n2 = snap.notebooks[1].notebook_id;
    let pending = vec![
        SyncOperationV1::CreateFolder {
            folder_id: f2,
            title: "Work".into(),
        },
        SyncOperationV1::SetNotebookFolder {
            notebook_id: n1,
            folder_id: Some(f2),
        },
        SyncOperationV1::SetNotebookFolder {
            notebook_id: n2,
            folder_id: Some(f2),
        },
        SyncOperationV1::RenameFolder {
            folder_id: f2,
            title: "Work 2".into(),
        },
        SyncOperationV1::SetNotebookFolder {
            notebook_id: n2,
            folder_id: None,
        },
        // Refused by the reducer (never existed): skipped by the projection.
        SyncOperationV1::DeleteFolder { folder_id: f1 },
        // Target absent from the displayed snapshot (deleted elsewhere): a NoOp.
        SyncOperationV1::SetNotebookFolder {
            notebook_id: nb2,
            folder_id: Some(f2),
        },
    ];
    let mut state = sync_engine::LibraryState::genesis(snap.clone()).unwrap();
    let mut changed = Vec::new();
    for op in &pending[..5] {
        let (next, outcome) = state.apply(&author.env(op.clone())).unwrap();
        changed.push(matches!(outcome, sync_engine::Outcome::Applied));
        state = next;
    }
    changed.extend([false, false]);
    #[derive(serde::Serialize)]
    struct M {
        notebook_id: NotebookId,
        #[serde(skip_serializing_if = "Option::is_none")]
        folder_id: Option<FolderId>,
    }
    #[derive(serde::Serialize)]
    struct Expected<'a> {
        folders: &'a [SnapshotFolder],
        notebooks: Vec<M>,
        changed: &'a [bool],
    }
    let memberships = |s: &sync_protocol::SyncSnapshotV1| -> Vec<M> {
        s.notebooks
            .iter()
            .map(|n| M {
                notebook_id: n.notebook_id,
                folder_id: n.folder_id,
            })
            .collect()
    };
    out.push(json!({
        "name": "project folders: reducer rules",
        "request": {
            "fn": "project_folders",
            "folders": snap.folders,
            "notebooks": memberships(&snap),
            "ops": pending,
        },
        "ok": serde_json::to_string(&Expected {
            folders: &state.snapshot.folders,
            notebooks: memberships(&state.snapshot),
            changed: &changed,
        })
        .unwrap(),
    }));
    out.push(json!({
        "name": "project folders: non-folder op refused",
        "request": {"fn": "project_folders", "folders": [], "notebooks": [], "ops": [SyncOperationV1::DeleteNotebook { notebook_id: n1 }]},
        "err": true,
    }));
    out
}

fn text_obj(tid: TextId, content: &str) -> SyncText {
    SyncText {
        text_id: tid,
        x: 100.0,
        y: 200.0,
        width: 5000.0,
        font_size: 340.0,
        face: TextFace::DefaultSansV1,
        content: content.into(),
    }
}

fn text_cases(i: &Ids) -> Vec<Value> {
    let tid = TextId::from_uuid(id(7)).unwrap();
    let native = |client_seq: u64, op: SyncOperationV1| {
        let env = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: i.op,
            library_id: i.lib,
            replica_id: i.rep,
            client_seq,
            operation: op,
        };
        String::from_utf8(env.to_canonical_bytes().unwrap()).unwrap()
    };
    let add = SyncOperationV1::AddText {
        notebook_id: i.nb,
        page_id: i.pg,
        text: text_obj(tid, "hello"),
    };
    let mut out = vec![json!({
        "name": "text: add",
        "request": {
            "fn": "encode_add_text",
            "library_id": i.lib.to_string(),
            "replica_id": i.rep.to_string(),
            "client_seq": "1",
            "operation_id": i.op.to_string(),
            "notebook_id": i.nb.to_string(),
            "page_id": i.pg.to_string(),
            "text": text_obj(tid, "hello"),
        },
        "ok": native(1, add.clone()),
    })];
    out.push(json!({
        "name": "text: add normalizes CR",
        "request": {
            "fn": "encode_add_text",
            "library_id": i.lib.to_string(),
            "replica_id": i.rep.to_string(),
            "client_seq": "1",
            "operation_id": i.op.to_string(),
            "notebook_id": i.nb.to_string(),
            "page_id": i.pg.to_string(),
            "text": text_obj(tid, "a\r\nb"),
        },
        "ok": native(1, SyncOperationV1::AddText {
            notebook_id: i.nb,
            page_id: i.pg,
            text: text_obj(tid, "a\nb"),
        }),
    }));
    out.push(json!({
        "name": "text: empty refused",
        "request": {
            "fn": "encode_add_text",
            "library_id": i.lib.to_string(),
            "replica_id": i.rep.to_string(),
            "client_seq": "1",
            "operation_id": i.op.to_string(),
            "notebook_id": i.nb.to_string(),
            "page_id": i.pg.to_string(),
            "text": text_obj(tid, ""),
        },
        "err": true,
    }));
    let updated = text_obj(tid, "world");
    out.push(json!({
        "name": "text: update",
        "request": {
            "fn": "encode_update_text",
            "library_id": i.lib.to_string(),
            "replica_id": i.rep.to_string(),
            "client_seq": "2",
            "operation_id": i.op.to_string(),
            "notebook_id": i.nb.to_string(),
            "page_id": i.pg.to_string(),
            "text": updated.clone(),
        },
        "ok": native(2, SyncOperationV1::UpdateText {
            notebook_id: i.nb,
            page_id: i.pg,
            text: updated,
        }),
    }));
    out.push(json!({
        "name": "text: delete",
        "request": {
            "fn": "encode_delete_text",
            "library_id": i.lib.to_string(),
            "replica_id": i.rep.to_string(),
            "client_seq": "3",
            "operation_id": i.op.to_string(),
            "notebook_id": i.nb.to_string(),
            "page_id": i.pg.to_string(),
            "text_id": tid.to_string(),
        },
        "ok": native(3, SyncOperationV1::DeleteText {
            notebook_id: i.nb,
            page_id: i.pg,
            text_id: tid,
        }),
    }));
    out.push(json!({
        "name": "project texts: add update delete",
        "request": {
            "fn": "project_texts",
            "texts": [],
            "ops": [
                {"op": "add", "text": text_obj(tid, "hello")},
                {"op": "update", "text": text_obj(tid, "world")},
                {"op": "delete", "text_id": tid.to_string()},
            ],
        },
        "ok": "{\"texts\":[],\"changed\":[true,true,true]}",
    }));
    out
}

fn blank_page(page_id: PageId) -> PageSpec {
    PageSpec {
        page_id,
        background: BackgroundRef::Blank,
        render: scribe_web_wasm::sealed_create_notebook_render(),
    }
}

fn structure_cases(i: &Ids) -> Vec<Value> {
    use sync_engine::testkit::{self as tk, Author};
    use sync_engine::{LibraryState, Outcome};

    let native = |client_seq: u64, operation: SyncOperationV1| {
        let env = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: i.op,
            library_id: i.lib,
            replica_id: i.rep,
            client_seq,
            operation,
        };
        String::from_utf8(env.to_canonical_bytes().unwrap()).unwrap()
    };
    let request = |client_seq: &str, operation: &SyncOperationV1| {
        json!({
            "fn": "encode_structure_op",
            "library_id": i.lib.to_string(),
            "replica_id": i.rep.to_string(),
            "client_seq": client_seq,
            "operation_id": i.op.to_string(),
            "operation": operation,
        })
    };
    let nb_new = NotebookId::from_uuid(id(30)).unwrap();
    let pg_new = PageId::from_uuid(id(31)).unwrap();
    let create = SyncOperationV1::CreateNotebook {
        notebook_id: nb_new,
        title: "Field log".into(),
        after_notebook_id: Some(i.nb),
        first_page: blank_page(pg_new),
    };
    let mut out = vec![json!({
        "name": "structure: create notebook",
        "request": request("1", &create),
        "ok": native(1, create.clone()),
    })];
    out.push(json!({
        "name": "structure: rename notebook",
        "request": request("2", &SyncOperationV1::RenameNotebook { notebook_id: i.nb, title: "Renamed".into() }),
        "ok": native(2, SyncOperationV1::RenameNotebook { notebook_id: i.nb, title: "Renamed".into() }),
    }));
    out.push(json!({
        "name": "structure: create page",
        "request": request("3", &SyncOperationV1::CreatePage {
            notebook_id: i.nb,
            page: blank_page(pg_new),
            after_page_id: Some(i.pg),
        }),
        "ok": native(3, SyncOperationV1::CreatePage {
            notebook_id: i.nb,
            page: blank_page(pg_new),
            after_page_id: Some(i.pg),
        }),
    }));
    out.push(json!({
        "name": "structure: delete page",
        "request": request("4", &SyncOperationV1::DeletePage { notebook_id: i.nb, page_id: i.pg }),
        "ok": native(4, SyncOperationV1::DeletePage { notebook_id: i.nb, page_id: i.pg }),
    }));
    out.push(json!({
        "name": "structure: delete notebook",
        "request": request("5", &SyncOperationV1::DeleteNotebook { notebook_id: i.nb }),
        "ok": native(5, SyncOperationV1::DeleteNotebook { notebook_id: i.nb }),
    }));
    out.push(json!({
        "name": "structure: move_page is not encoded",
        "request": request("1", &SyncOperationV1::MovePage {
            notebook_id: i.nb,
            page_id: i.pg,
            after_page_id: None,
        }),
        "err": true,
    }));
    out.push(json!({
        "name": "structure: folder op refused",
        "request": request("1", &SyncOperationV1::DeleteFolder { folder_id: FolderId::from_uuid(id(20)).unwrap() }),
        "err": true,
    }));
    let undo = json!({
        "fn": "encode_delete_stroke_lineage",
        "library_id": i.lib.to_string(),
        "replica_id": i.rep.to_string(),
        "client_seq": "6",
        "operation_id": i.op.to_string(),
        "notebook_id": i.nb.to_string(),
        "page_id": i.pg.to_string(),
        "root_id": i.st.to_string(),
    });
    out.push(json!({
        "name": "undo: encode delete_stroke_lineage",
        "request": undo,
        "ok": native(6, SyncOperationV1::DeleteStrokeLineage {
            notebook_id: i.nb,
            page_id: i.pg,
            root_id: i.st,
        }),
    }));
    out.push(json!({
        "name": "create notebook render is sealed v2-pressure",
        "request": {"fn": "create_notebook_render"},
        "ok": serde_json::to_string(&scribe_web_wasm::sealed_create_notebook_render()).unwrap(),
    }));

    // Unified library projection: CreateNotebook then SetNotebookFolder in one client order.
    let mut snap = tk::genesis();
    snap.library_id = i.lib;
    let mut author = Author::new(i.rep);
    author.library = i.lib;
    let last = snap.notebooks.last().unwrap().notebook_id;
    let folder = FolderId::from_uuid(id(32)).unwrap();
    let pending = vec![
        SyncOperationV1::CreateFolder {
            folder_id: folder,
            title: "Work".into(),
        },
        SyncOperationV1::CreateNotebook {
            notebook_id: nb_new,
            title: "In folder".into(),
            after_notebook_id: Some(last),
            first_page: blank_page(pg_new),
        },
        SyncOperationV1::SetNotebookFolder {
            notebook_id: nb_new,
            folder_id: Some(folder),
        },
    ];
    let mut state = LibraryState::genesis(snap.clone()).unwrap();
    let mut changed = Vec::new();
    for op in &pending {
        let (next, outcome) = state.apply(&author.env(op.clone())).unwrap();
        changed.push(matches!(outcome, Outcome::Applied));
        state = next;
    }
    #[derive(serde::Serialize)]
    struct Expected<'a> {
        notebooks: &'a [sync_protocol::SnapshotNotebook],
        folders: &'a [SnapshotFolder],
        changed: &'a [bool],
    }
    out.push(json!({
        "name": "project library: create notebook then file it",
        "request": {"fn": "project_library", "snapshot": snap.clone(), "ops": pending},
        "ok": serde_json::to_string(&Expected {
            notebooks: &state.snapshot.notebooks,
            folders: &state.snapshot.folders,
            changed: &changed,
        }).unwrap(),
    }));

    // DeleteNotebook drops the notebook from its folder as well as the list.
    let filed = state
        .snapshot
        .notebooks
        .iter()
        .find(|n| n.notebook_id == nb_new)
        .unwrap();
    assert_eq!(filed.folder_id, Some(folder));
    let delete = vec![SyncOperationV1::DeleteNotebook {
        notebook_id: nb_new,
    }];
    let mut after = state.clone();
    let (next, outcome) = after.apply(&author.env(delete[0].clone())).unwrap();
    assert!(matches!(outcome, Outcome::Applied));
    after = next;
    out.push(json!({
        "name": "project library: delete notebook clears membership",
        "request": {"fn": "project_library", "snapshot": state.snapshot.clone(), "ops": delete},
        "ok": serde_json::to_string(&Expected {
            notebooks: &after.snapshot.notebooks,
            folders: &after.snapshot.folders,
            changed: &[true],
        }).unwrap(),
    }));

    // Sole notebook delete is skipped (reducer refuse), not an error.
    let mut one = after.clone();
    while one.snapshot.notebooks.len() > 1 {
        let id = one.snapshot.notebooks.last().unwrap().notebook_id;
        one = one
            .apply(&author.env(SyncOperationV1::DeleteNotebook { notebook_id: id }))
            .unwrap()
            .0;
    }
    let sole_id = one.snapshot.notebooks[0].notebook_id;
    out.push(json!({
        "name": "project library: sole notebook delete skipped",
        "request": {
            "fn": "project_library",
            "snapshot": one.snapshot.clone(),
            "ops": [SyncOperationV1::DeleteNotebook { notebook_id: sole_id }],
        },
        "ok": serde_json::to_string(&Expected {
            notebooks: &one.snapshot.notebooks,
            folders: &one.snapshot.folders,
            changed: &[false],
        }).unwrap(),
    }));

    // Create then file into a folder that never existed: second op skipped, notebook unfiled.
    let ghost = FolderId::from_uuid(id(33)).unwrap();
    let orphan_nb = NotebookId::from_uuid(id(34)).unwrap();
    let orphan_pg = PageId::from_uuid(id(35)).unwrap();
    let orphan_ops = vec![
        SyncOperationV1::CreateNotebook {
            notebook_id: orphan_nb,
            title: "Orphan".into(),
            after_notebook_id: Some(last),
            first_page: blank_page(orphan_pg),
        },
        SyncOperationV1::SetNotebookFolder {
            notebook_id: orphan_nb,
            folder_id: Some(ghost),
        },
    ];
    let mut orphan_state = LibraryState::genesis(snap.clone()).unwrap();
    let mut orphan_changed = Vec::new();
    for op in &orphan_ops {
        match orphan_state.apply(&author.env(op.clone())) {
            Ok((next, outcome)) => {
                orphan_changed.push(matches!(outcome, Outcome::Applied));
                orphan_state = next;
            }
            Err(_) => orphan_changed.push(false),
        }
    }
    assert_eq!(orphan_changed, vec![true, false]);
    let created = orphan_state
        .snapshot
        .notebooks
        .iter()
        .find(|n| n.notebook_id == orphan_nb)
        .unwrap();
    assert!(created.folder_id.is_none());
    out.push(json!({
        "name": "project library: create-in-folder filing refusal leaves notebook unfiled",
        "request": {"fn": "project_library", "snapshot": snap.clone(), "ops": orphan_ops},
        "ok": serde_json::to_string(&Expected {
            notebooks: &orphan_state.snapshot.notebooks,
            folders: &orphan_state.snapshot.folders,
            changed: &orphan_changed,
        }).unwrap(),
    }));

    out.push(json!({
        "name": "project library: page-content op refused",
        "request": {"fn": "project_library", "snapshot": snap.clone(), "ops": [SyncOperationV1::AddStroke {
            notebook_id: i.nb,
            page_id: i.pg,
            stroke: SyncStroke {
                stroke_id: i.st,
                root_id: i.st,
                style: StrokeStyle::LegacyFixedV1,
                points: vec![SyncPoint::new(1.0, 1.0, None), SyncPoint::new(2.0, 2.0, None)],
            },
        }]},
        "err": true,
    }));
    out
}

#[test]
fn bridge_matches_native_rust_and_fixture_is_current() {
    let cases = cases();
    for c in &cases {
        let got: Value = serde_json::from_str(&handle(&c["request"].to_string())).unwrap();
        if c["err"] == json!(true) {
            assert!(
                got.get("err").is_some(),
                "{}: expected refusal, got {got}",
                c["name"]
            );
        } else {
            assert_eq!(got["ok"], c["ok"], "{}", c["name"]);
        }
    }
    // Every AddStroke from the bridge parses back through sync-protocol with root == stroke.
    for c in cases
        .iter()
        .filter(|c| c["request"]["fn"] == "encode_add_stroke" && c["ok"].is_string())
    {
        let text = c["ok"].as_str().unwrap();
        let env: SyncEnvelopeV1 = sync_protocol::from_json(text.as_bytes()).unwrap();
        assert_eq!(env.to_canonical_bytes().unwrap(), text.as_bytes());
        let SyncOperationV1::AddStroke { stroke, .. } = &env.operation else {
            panic!()
        };
        assert_eq!(stroke.root_id, stroke.stroke_id);
    }
    let mut want = serde_json::to_string_pretty(&json!({ "v": 1, "cases": cases })).unwrap();
    want.push('\n');
    let path = fixture_path();
    if std::env::var("SCRIBE_REGEN_WEB_FIXTURES").is_ok_and(|v| v == "1") {
        std::fs::write(&path, &want).unwrap();
    }
    let have = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert_eq!(
        have,
        want,
        "{} drifted: regenerate with SCRIBE_REGEN_WEB_FIXTURES=1",
        path.display()
    );
}

#[test]
fn minted_ids_obey_s1_rules() {
    for n in 0..=255u8 {
        let u = uuid_v7(1_790_000_000_000, &[n; 10]);
        assert_eq!(u.get_version_num(), 7);
        assert_eq!(u.get_variant(), uuid::Variant::RFC4122);
        StrokeId::from_uuid(u).unwrap();
    }
}
