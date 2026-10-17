//! ignite-web protocol bridge (`wasm32-unknown-unknown`, no `wasm-bindgen`).
//!
//! ignite-web must never re-implement Rust canonical JSON or u64 sequencing in TypeScript. This
//! crate is the browser's only way to create protocol values: it builds the real
//! `sync_protocol::SyncEnvelopeV1` and returns the bytes of `sync-protocol`'s own canonical
//! encoder, which the browser stores and resends **exactly**.
//!
//! ABI (plain wasm memory, JSON in / JSON out):
//!
//! ```text
//! swa_alloc(len) -> ptr            swa_free(ptr, len)
//! swa_call(ptr, len) -> u64        (out_ptr << 32) | out_len ; output is {"ok":…} or {"err":"…"}
//! ```
//!
//! Requests (`fn`):
//! * `mint_id {kind, unix_ms: "<decimal>", random_hex: "<20 hex>"}` → canonical UUIDv7 text of
//!   `kind` ∈ replica|operation|stroke|folder|text|notebook|page. Randomness is
//!   `crypto.getRandomValues` from the caller; the resulting ID is validated by the S1
//!   constructors (version, variant, not nil/max).
//! * `encode_add_stroke {library_id, replica_id, client_seq: "<decimal u64>", operation_id,
//!   notebook_id, page_id, stroke_id, style, points:[{x,y,pressure?}]}` → canonical envelope
//!   text. `root_id == stroke_id`; `client_seq` is a decimal string (full u64, never a JS
//!   Number).
//! * `encode_erase_region {library_id, replica_id, client_seq, operation_id, notebook_id,
//!   page_id, observed_roots:[…], path:[{x,y}]}` → canonical `EraseRegion` envelope text. The
//!   radius is the Kindle eraser's (`notebook_core::ERASER_HIT_RADIUS_CANONICAL`), never a
//!   browser value; `observed_roots` is sorted and de-duplicated here; path samples carry no
//!   pressure (as on the Kindle).
//! * `project_page {strokes:[SyncStroke], ops:[{op:"add",stroke} | {op:"erase",operation_id,
//!   observed_roots,path} | {op:"delete_lineage",root_id}]}` → `{"strokes":[…],"changed":[bool…]}`:
//!   one page's visible strokes after applying the ops in order with the **reducer's own** erase
//!   (`sync_engine::erase_page_strokes`) and lineage delete. An `add` whose stroke ID is already
//!   present is skipped (already in the snapshot).
//! * `encode_folder_op {library_id, replica_id, client_seq, operation_id, operation}` →
//!   canonical envelope text. `operation` is a `SyncOperationV1` and must be one of
//!   `create_folder`, `rename_folder`, `delete_folder`, `set_notebook_folder`.
//! * `project_folders {folders:[SnapshotFolder], notebooks:[{notebook_id, folder_id?}],
//!   ops:[folder SyncOperationV1]}` → `{"folders":[…],"notebooks":[…],"changed":[bool…]}`:
//!   internal folder-only helper. Viewer optimistic UI uses `project_library`.
//! * `encode_structure_op {library_id, replica_id, client_seq, operation_id, operation}` →
//!   canonical envelope text. `operation` must be `create_notebook` | `rename_notebook` |
//!   `delete_notebook` | `create_page` | `delete_page` (not `move_page`).
//! * `encode_delete_stroke_lineage {library_id, replica_id, client_seq, operation_id,
//!   notebook_id, page_id, root_id}` → canonical undo envelope. `root_id` is the lineage root,
//!   which may differ from a visible fragment id.
//! * `project_library {snapshot: SyncSnapshotV1, ops:[library-scoped SyncOperationV1]}` →
//!   `{"notebooks":[…],"folders":[…],"changed":[bool…]}`: `LibraryState::genesis` then apply each
//!   op in order. Folder and structure ops share this one projection. An op the reducer would
//!   refuse is skipped. Page-content ops (ink/erase/text/undo) are refused here — they go
//!   through `project_page` / `project_texts`. Page links (`AddPageLink` /
//!   `DeletePageLink`) are Kindle-authored; the browser reconstructs them from
//!   `SyncSnapshotV1` only.
//! * `create_notebook_render` → sealed Kindle `scribe-raster-v2-pressure` `RenderProfileRef`
//!   (black ink) for a new notebook's first page. Not invented in TypeScript.
//! * `encode_add_text` / `encode_update_text` / `encode_delete_text` → canonical text-object
//!   envelopes. Content is LF-normalized here (never trimmed).
//! * `project_texts {texts:[SyncText], ops:[{op:"add",text}|{op:"update",text}|{op:"delete",text_id}]}`
//!   → `{"texts":[…],"changed":[bool…]}` using the same identity rules as the reducer overlay
//!   (skip an add whose id is already present; skip update/delete of an unknown id).
//!
//! Browser authoring: `AddStroke`, `EraseRegion`, `DeleteStrokeLineage`, the three text
//! operations, the four folder operations, and the five notebook/page structure operations
//! above. `MovePage` is not encoded here.

use std::collections::BTreeSet;

use notebook_core::ERASER_HIT_RADIUS_CANONICAL;
use serde::{Deserialize, Serialize};
use sync_protocol::{
    normalize_text_content, FolderId, LibraryId, NotebookId, OperationId, PageId, PressureCurveRef,
    RenderProfileRef, ReplicaId, SnapshotFolder, SnapshotNotebook, StrokeId, StrokeStyle,
    SyncEnvelopeV1, SyncOperationV1, SyncPoint, SyncSnapshotV1, SyncStroke, SyncText, TextId,
    ENVELOPE_VERSION,
};
use uuid::Uuid;

/// The Kindle eraser's capsule radius (canonical units); the only radius the browser can send.
pub const ERASE_RADIUS: f32 = ERASER_HIT_RADIUS_CANONICAL;

/// Sealed Kindle `scribe-raster-v2-pressure` (black ink) for a browser-created notebook page.
/// Matches `scribe-app` `RenderProfile::sealed_v2_pressure(0x00)` + `PressureCurveV1::sealed_v1`.
pub fn sealed_create_notebook_render() -> RenderProfileRef {
    RenderProfileRef {
        profile_version: 2,
        ink_byte: 0x00,
        legacy_fixed_radius: 2,
        transform_id: "scribe1-portrait-identity-v1".into(),
        rasterizer_id: "scribe-raster-v2-pressure".into(),
        pressure: Some(PressureCurveRef {
            q_r1_max: 820,
            q_r2_max: 2866,
            q_r3_max: 3685,
            min_radius: 1,
            max_radius: 4,
            missing_fallback_radius: 2,
        }),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathPointIn {
    x: f32,
    y: f32,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum ProjOp {
    Add {
        stroke: SyncStroke,
    },
    Erase {
        operation_id: String,
        observed_roots: Vec<String>,
        path: Vec<PathPointIn>,
    },
    DeleteLineage {
        root_id: String,
    },
}

/// A parsed projection step.
pub enum PageOp {
    Add(SyncStroke),
    Erase {
        operation_id: OperationId,
        observed_roots: BTreeSet<StrokeId>,
        path: Vec<SyncPoint>,
    },
    DeleteLineage {
        root_id: StrokeId,
    },
}

#[derive(Serialize)]
struct Projection {
    strokes: Vec<SyncStroke>,
    changed: Vec<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PointIn {
    x: f32,
    y: f32,
    #[serde(default)]
    pressure: Option<f32>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Membership {
    notebook_id: NotebookId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    folder_id: Option<FolderId>,
}

#[derive(Serialize)]
struct FolderProjectionOut {
    folders: Vec<SnapshotFolder>,
    notebooks: Vec<Membership>,
    changed: Vec<bool>,
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

fn is_structure_op(op: &SyncOperationV1) -> bool {
    matches!(
        op,
        SyncOperationV1::CreateNotebook { .. }
            | SyncOperationV1::RenameNotebook { .. }
            | SyncOperationV1::DeleteNotebook { .. }
            | SyncOperationV1::CreatePage { .. }
            | SyncOperationV1::DeletePage { .. }
    )
}

/// Library chrome (notebooks, pages, folders). Not page-content mutations.
fn is_library_op(op: &SyncOperationV1) -> bool {
    is_folder_op(op) || is_structure_op(op) || matches!(op, SyncOperationV1::MovePage { .. })
}

#[derive(Deserialize)]
#[serde(tag = "fn", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    MintId {
        kind: String,
        unix_ms: String,
        random_hex: String,
    },
    EncodeAddStroke {
        library_id: String,
        replica_id: String,
        client_seq: String,
        operation_id: String,
        notebook_id: String,
        page_id: String,
        stroke_id: String,
        style: StrokeStyle,
        points: Vec<PointIn>,
    },
    EncodeEraseRegion {
        library_id: String,
        replica_id: String,
        client_seq: String,
        operation_id: String,
        notebook_id: String,
        page_id: String,
        observed_roots: Vec<String>,
        path: Vec<PathPointIn>,
    },
    ProjectPage {
        strokes: Vec<SyncStroke>,
        ops: Vec<ProjOp>,
    },
    EncodeFolderOp {
        library_id: String,
        replica_id: String,
        client_seq: String,
        operation_id: String,
        operation: SyncOperationV1,
    },
    ProjectFolders {
        folders: Vec<SnapshotFolder>,
        notebooks: Vec<Membership>,
        ops: Vec<SyncOperationV1>,
    },
    EncodeStructureOp {
        library_id: String,
        replica_id: String,
        client_seq: String,
        operation_id: String,
        operation: SyncOperationV1,
    },
    EncodeDeleteStrokeLineage {
        library_id: String,
        replica_id: String,
        client_seq: String,
        operation_id: String,
        notebook_id: String,
        page_id: String,
        root_id: String,
    },
    ProjectLibrary {
        snapshot: SyncSnapshotV1,
        ops: Vec<SyncOperationV1>,
    },
    CreateNotebookRender,
    EncodeAddText {
        library_id: String,
        replica_id: String,
        client_seq: String,
        operation_id: String,
        notebook_id: String,
        page_id: String,
        text: SyncText,
    },
    EncodeUpdateText {
        library_id: String,
        replica_id: String,
        client_seq: String,
        operation_id: String,
        notebook_id: String,
        page_id: String,
        text: SyncText,
    },
    EncodeDeleteText {
        library_id: String,
        replica_id: String,
        client_seq: String,
        operation_id: String,
        notebook_id: String,
        page_id: String,
        text_id: String,
    },
    ProjectTexts {
        texts: Vec<SyncText>,
        ops: Vec<TextProjOp>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum TextProjOp {
    Add { text: SyncText },
    Update { text: SyncText },
    Delete { text_id: String },
}

#[derive(Serialize)]
struct TextProjection {
    texts: Vec<SyncText>,
    changed: Vec<bool>,
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Full-u64 decimal (no sign, no leading zeros except the zero-padded SEQ20 form, ≥ 1).
pub fn parse_client_seq(s: &str) -> Result<u64, String> {
    if s.is_empty() || s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("client_seq {s:?} is not a decimal u64"));
    }
    let v: u64 = s
        .parse()
        .map_err(|_| format!("client_seq {s:?} exceeds u64"))?;
    if v == 0 {
        return Err("client_seq must be >= 1".into());
    }
    Ok(v)
}

/// UUIDv7 from a millisecond timestamp and 10 random bytes (RFC 9562 layout).
pub fn uuid_v7(unix_ms: u64, random: &[u8; 10]) -> Uuid {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&unix_ms.to_be_bytes()[2..]);
    b[6] = 0x70 | (random[0] & 0x0f);
    b[7] = random[1];
    b[8] = 0x80 | (random[2] & 0x3f);
    b[9..].copy_from_slice(&random[3..]);
    Uuid::from_bytes(b)
}

fn mint(kind: &str, unix_ms: &str, random_hex: &str) -> Result<String, String> {
    let ms: u64 = unix_ms
        .parse()
        .map_err(|_| "unix_ms must be a decimal integer".to_string())?;
    if ms >= 1 << 48 {
        return Err("unix_ms out of UUIDv7 range".into());
    }
    if random_hex.len() != 20 || !random_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("random_hex must be 20 hex characters (10 random bytes)".into());
    }
    let mut r = [0u8; 10];
    for (i, byte) in r.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&random_hex[2 * i..2 * i + 2], 16).map_err(err)?;
    }
    let u = uuid_v7(ms, &r);
    // Construct through the S1 typed IDs so every rule is checked by sync-protocol itself.
    Ok(match kind {
        "replica" => ReplicaId::from_uuid(u).map_err(err)?.to_string(),
        "operation" => OperationId::from_uuid(u).map_err(err)?.to_string(),
        "stroke" => StrokeId::from_uuid(u).map_err(err)?.to_string(),
        "folder" => FolderId::from_uuid(u).map_err(err)?.to_string(),
        "text" => TextId::from_uuid(u).map_err(err)?.to_string(),
        "notebook" => NotebookId::from_uuid(u).map_err(err)?.to_string(),
        "page" => PageId::from_uuid(u).map_err(err)?.to_string(),
        other => return Err(format!("unknown id kind {other:?}")),
    })
}

#[allow(clippy::too_many_arguments)]
fn encode_add_stroke(
    library_id: &str,
    replica_id: &str,
    client_seq: &str,
    operation_id: &str,
    notebook_id: &str,
    page_id: &str,
    stroke_id: &str,
    style: StrokeStyle,
    points: Vec<PointIn>,
) -> Result<String, String> {
    let stroke_id = StrokeId::parse(stroke_id).map_err(err)?;
    let env = SyncEnvelopeV1 {
        v: ENVELOPE_VERSION,
        operation_id: OperationId::parse(operation_id).map_err(err)?,
        library_id: LibraryId::parse(library_id).map_err(err)?,
        replica_id: ReplicaId::parse(replica_id).map_err(err)?,
        client_seq: parse_client_seq(client_seq)?,
        operation: SyncOperationV1::AddStroke {
            notebook_id: NotebookId::parse(notebook_id).map_err(err)?,
            page_id: PageId::parse(page_id).map_err(err)?,
            stroke: SyncStroke {
                stroke_id,
                root_id: stroke_id,
                style,
                points: points
                    .into_iter()
                    .map(|p| SyncPoint::new(p.x, p.y, p.pressure))
                    .collect(),
            },
        },
    };
    // The one authoritative canonical encoder (validates the envelope first).
    let bytes = env.to_canonical_bytes().map_err(err)?;
    String::from_utf8(bytes).map_err(err)
}

fn roots(observed: &[String]) -> Result<BTreeSet<StrokeId>, String> {
    observed
        .iter()
        .map(|r| StrokeId::parse(r).map_err(err))
        .collect()
}

fn path_points(path: &[PathPointIn]) -> Vec<SyncPoint> {
    path.iter()
        .map(|p| SyncPoint::new(p.x, p.y, None))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn encode_erase_region(
    library_id: &str,
    replica_id: &str,
    client_seq: &str,
    operation_id: &str,
    notebook_id: &str,
    page_id: &str,
    observed_roots: &[String],
    path: &[PathPointIn],
) -> Result<String, String> {
    let env = SyncEnvelopeV1 {
        v: ENVELOPE_VERSION,
        operation_id: OperationId::parse(operation_id).map_err(err)?,
        library_id: LibraryId::parse(library_id).map_err(err)?,
        replica_id: ReplicaId::parse(replica_id).map_err(err)?,
        client_seq: parse_client_seq(client_seq)?,
        operation: SyncOperationV1::EraseRegion {
            notebook_id: NotebookId::parse(notebook_id).map_err(err)?,
            page_id: PageId::parse(page_id).map_err(err)?,
            // BTreeSet: sorted + unique, the protocol's required form.
            observed_roots: roots(observed_roots)?.into_iter().collect(),
            path: path_points(path),
            radius: ERASE_RADIUS,
        },
    };
    let bytes = env.to_canonical_bytes().map_err(err)?;
    String::from_utf8(bytes).map_err(err)
}

/// One page's visible strokes after `ops` (in order), using the reducer's erase. `changed[i]`
/// says whether op `i` altered the page.
pub fn project_page_strokes(
    mut strokes: Vec<SyncStroke>,
    ops: &[PageOp],
) -> (Vec<SyncStroke>, Vec<bool>) {
    let mut changed = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            PageOp::Add(stroke) => {
                if strokes.iter().any(|s| s.stroke_id == stroke.stroke_id) {
                    changed.push(false);
                } else {
                    strokes.push(stroke.clone());
                    changed.push(true);
                }
            }
            PageOp::Erase {
                operation_id,
                observed_roots,
                path,
            } => {
                let r = sync_engine::erase_page_strokes(
                    &strokes,
                    observed_roots,
                    path,
                    ERASE_RADIUS,
                    *operation_id,
                );
                changed.push(!r.replaced.is_empty());
                strokes = r.strokes;
            }
            PageOp::DeleteLineage { root_id } => {
                let before = strokes.len();
                strokes.retain(|s| s.root_id != *root_id);
                changed.push(strokes.len() != before);
            }
        }
    }
    (strokes, changed)
}

fn project_page(strokes: Vec<SyncStroke>, ops: Vec<ProjOp>) -> Result<String, String> {
    let check = |pts: &[SyncPoint]| -> Result<(), String> {
        pts.iter().try_for_each(|p| p.validate().map_err(err))
    };
    for s in &strokes {
        check(&s.points)?;
    }
    let mut parsed = Vec::with_capacity(ops.len());
    for op in ops {
        parsed.push(match op {
            ProjOp::Add { stroke } => {
                check(&stroke.points)?;
                PageOp::Add(stroke)
            }
            ProjOp::Erase {
                operation_id,
                observed_roots,
                path,
            } => {
                let path = path_points(&path);
                check(&path)?;
                PageOp::Erase {
                    operation_id: OperationId::parse(&operation_id).map_err(err)?,
                    observed_roots: roots(&observed_roots)?,
                    path,
                }
            }
            ProjOp::DeleteLineage { root_id } => PageOp::DeleteLineage {
                root_id: StrokeId::parse(&root_id).map_err(err)?,
            },
        });
    }
    let (strokes, changed) = project_page_strokes(strokes, &parsed);
    serde_json::to_string(&Projection { strokes, changed }).map_err(err)
}

fn encode_folder_op(
    library_id: &str,
    replica_id: &str,
    client_seq: &str,
    operation_id: &str,
    operation: SyncOperationV1,
) -> Result<String, String> {
    if !is_folder_op(&operation) {
        return Err("encode_folder_op builds folder operations only".into());
    }
    let env = SyncEnvelopeV1 {
        v: ENVELOPE_VERSION,
        operation_id: OperationId::parse(operation_id).map_err(err)?,
        library_id: LibraryId::parse(library_id).map_err(err)?,
        replica_id: ReplicaId::parse(replica_id).map_err(err)?,
        client_seq: parse_client_seq(client_seq)?,
        operation,
    };
    let bytes = env.to_canonical_bytes().map_err(err)?;
    String::from_utf8(bytes).map_err(err)
}

fn project_folders(
    folders: Vec<SnapshotFolder>,
    notebooks: Vec<Membership>,
    ops: Vec<SyncOperationV1>,
) -> Result<String, String> {
    if let Some(op) = ops.iter().find(|o| !is_folder_op(o)) {
        return Err(format!(
            "project_folders takes folder operations only, got {op:?}"
        ));
    }
    let mut p = sync_engine::FolderProjection {
        folders,
        notebooks: notebooks
            .into_iter()
            .map(|m| (m.notebook_id, m.folder_id))
            .collect(),
        deleted_folders: BTreeSet::new(),
    };
    let changed = p.project(&ops);
    serde_json::to_string(&FolderProjectionOut {
        folders: p.folders,
        notebooks: p
            .notebooks
            .into_iter()
            .map(|(notebook_id, folder_id)| Membership {
                notebook_id,
                folder_id,
            })
            .collect(),
        changed,
    })
    .map_err(err)
}

fn encode_envelope(
    library_id: &str,
    replica_id: &str,
    client_seq: &str,
    operation_id: &str,
    operation: SyncOperationV1,
) -> Result<String, String> {
    let env = SyncEnvelopeV1 {
        v: ENVELOPE_VERSION,
        operation_id: OperationId::parse(operation_id).map_err(err)?,
        library_id: LibraryId::parse(library_id).map_err(err)?,
        replica_id: ReplicaId::parse(replica_id).map_err(err)?,
        client_seq: parse_client_seq(client_seq)?,
        operation,
    };
    let bytes = env.to_canonical_bytes().map_err(err)?;
    String::from_utf8(bytes).map_err(err)
}

fn encode_structure_op(
    library_id: &str,
    replica_id: &str,
    client_seq: &str,
    operation_id: &str,
    operation: SyncOperationV1,
) -> Result<String, String> {
    if !is_structure_op(&operation) {
        return Err("encode_structure_op builds notebook/page structure operations only".into());
    }
    encode_envelope(library_id, replica_id, client_seq, operation_id, operation)
}

fn encode_delete_stroke_lineage(
    library_id: &str,
    replica_id: &str,
    client_seq: &str,
    operation_id: &str,
    notebook_id: &str,
    page_id: &str,
    root_id: &str,
) -> Result<String, String> {
    encode_envelope(
        library_id,
        replica_id,
        client_seq,
        operation_id,
        SyncOperationV1::DeleteStrokeLineage {
            notebook_id: NotebookId::parse(notebook_id).map_err(err)?,
            page_id: PageId::parse(page_id).map_err(err)?,
            root_id: StrokeId::parse(root_id).map_err(err)?,
        },
    )
}

#[derive(Serialize)]
struct LibraryProjectionOut {
    notebooks: Vec<SnapshotNotebook>,
    folders: Vec<SnapshotFolder>,
    changed: Vec<bool>,
}

/// Dummy envelope for optimistic apply: the reducer keys off the operation, not these ids.
fn projection_env(
    library_id: LibraryId,
    operation: SyncOperationV1,
) -> Result<SyncEnvelopeV1, String> {
    Ok(SyncEnvelopeV1 {
        v: ENVELOPE_VERSION,
        operation_id: OperationId::from_uuid(uuid_v7(1, &[0x0a; 10])).map_err(err)?,
        library_id,
        replica_id: ReplicaId::from_uuid(uuid_v7(1, &[0x0b; 10])).map_err(err)?,
        client_seq: 1,
        operation,
    })
}

fn project_library(snapshot: SyncSnapshotV1, ops: Vec<SyncOperationV1>) -> Result<String, String> {
    if let Some(op) = ops.iter().find(|o| !is_library_op(o)) {
        return Err(format!(
            "project_library takes library-scoped operations only, got {op:?}"
        ));
    }
    let mut state = sync_engine::LibraryState::genesis(snapshot).map_err(err)?;
    let mut changed = Vec::with_capacity(ops.len());
    for op in ops {
        let env = projection_env(state.snapshot.library_id, op)?;
        match state.apply(&env) {
            Ok((next, outcome)) => {
                changed.push(matches!(outcome, sync_engine::Outcome::Applied));
                state = next;
            }
            Err(_) => changed.push(false),
        }
    }
    serde_json::to_string(&LibraryProjectionOut {
        notebooks: state.snapshot.notebooks,
        folders: state.snapshot.folders,
        changed,
    })
    .map_err(err)
}

fn prepared_text(mut text: SyncText) -> Result<SyncText, String> {
    text.content = normalize_text_content(&text.content);
    text.validate().map_err(err)?;
    Ok(text)
}

fn encode_text_envelope(
    library_id: &str,
    replica_id: &str,
    client_seq: &str,
    operation_id: &str,
    operation: SyncOperationV1,
) -> Result<String, String> {
    let env = SyncEnvelopeV1 {
        v: ENVELOPE_VERSION,
        operation_id: OperationId::parse(operation_id).map_err(err)?,
        library_id: LibraryId::parse(library_id).map_err(err)?,
        replica_id: ReplicaId::parse(replica_id).map_err(err)?,
        client_seq: parse_client_seq(client_seq)?,
        operation,
    };
    let bytes = env.to_canonical_bytes().map_err(err)?;
    String::from_utf8(bytes).map_err(err)
}

fn encode_add_text(
    library_id: &str,
    replica_id: &str,
    client_seq: &str,
    operation_id: &str,
    notebook_id: &str,
    page_id: &str,
    text: SyncText,
) -> Result<String, String> {
    encode_text_envelope(
        library_id,
        replica_id,
        client_seq,
        operation_id,
        SyncOperationV1::AddText {
            notebook_id: NotebookId::parse(notebook_id).map_err(err)?,
            page_id: PageId::parse(page_id).map_err(err)?,
            text: prepared_text(text)?,
        },
    )
}

fn encode_update_text(
    library_id: &str,
    replica_id: &str,
    client_seq: &str,
    operation_id: &str,
    notebook_id: &str,
    page_id: &str,
    text: SyncText,
) -> Result<String, String> {
    encode_text_envelope(
        library_id,
        replica_id,
        client_seq,
        operation_id,
        SyncOperationV1::UpdateText {
            notebook_id: NotebookId::parse(notebook_id).map_err(err)?,
            page_id: PageId::parse(page_id).map_err(err)?,
            text: prepared_text(text)?,
        },
    )
}

fn encode_delete_text(
    library_id: &str,
    replica_id: &str,
    client_seq: &str,
    operation_id: &str,
    notebook_id: &str,
    page_id: &str,
    text_id: &str,
) -> Result<String, String> {
    encode_text_envelope(
        library_id,
        replica_id,
        client_seq,
        operation_id,
        SyncOperationV1::DeleteText {
            notebook_id: NotebookId::parse(notebook_id).map_err(err)?,
            page_id: PageId::parse(page_id).map_err(err)?,
            text_id: TextId::parse(text_id).map_err(err)?,
        },
    )
}

fn project_page_texts(
    mut texts: Vec<SyncText>,
    ops: &[TextProjOp],
) -> Result<(Vec<SyncText>, Vec<bool>), String> {
    let mut changed = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            TextProjOp::Add { text } => {
                let text = prepared_text(text.clone())?;
                if texts.iter().any(|t| t.text_id == text.text_id) {
                    changed.push(false);
                } else {
                    texts.push(text);
                    changed.push(true);
                }
            }
            TextProjOp::Update { text } => {
                let text = prepared_text(text.clone())?;
                if let Some(slot) = texts.iter_mut().find(|t| t.text_id == text.text_id) {
                    *slot = text;
                    changed.push(true);
                } else {
                    changed.push(false);
                }
            }
            TextProjOp::Delete { text_id } => {
                let id = TextId::parse(text_id).map_err(err)?;
                let before = texts.len();
                texts.retain(|t| t.text_id != id);
                changed.push(texts.len() != before);
            }
        }
    }
    Ok((texts, changed))
}

fn project_texts(texts: Vec<SyncText>, ops: Vec<TextProjOp>) -> Result<String, String> {
    for t in &texts {
        t.validate().map_err(err)?;
    }
    let (texts, changed) = project_page_texts(texts, &ops)?;
    serde_json::to_string(&TextProjection { texts, changed }).map_err(err)
}

/// JSON request → JSON response (`{"ok":…}` / `{"err":"…"}`). Shared by the wasm export and
/// native tests, so the browser runs exactly this code.
pub fn handle(request: &str) -> String {
    let result = serde_json::from_str::<Request>(request)
        .map_err(|e| format!("bad request: {e}"))
        .and_then(|r| match r {
            Request::MintId {
                kind,
                unix_ms,
                random_hex,
            } => mint(&kind, &unix_ms, &random_hex),
            Request::EncodeAddStroke {
                library_id,
                replica_id,
                client_seq,
                operation_id,
                notebook_id,
                page_id,
                stroke_id,
                style,
                points,
            } => encode_add_stroke(
                &library_id,
                &replica_id,
                &client_seq,
                &operation_id,
                &notebook_id,
                &page_id,
                &stroke_id,
                style,
                points,
            ),
            Request::EncodeEraseRegion {
                library_id,
                replica_id,
                client_seq,
                operation_id,
                notebook_id,
                page_id,
                observed_roots,
                path,
            } => encode_erase_region(
                &library_id,
                &replica_id,
                &client_seq,
                &operation_id,
                &notebook_id,
                &page_id,
                &observed_roots,
                &path,
            ),
            Request::ProjectPage { strokes, ops } => project_page(strokes, ops),
            Request::EncodeFolderOp {
                library_id,
                replica_id,
                client_seq,
                operation_id,
                operation,
            } => encode_folder_op(
                &library_id,
                &replica_id,
                &client_seq,
                &operation_id,
                operation,
            ),
            Request::ProjectFolders {
                folders,
                notebooks,
                ops,
            } => project_folders(folders, notebooks, ops),
            Request::EncodeStructureOp {
                library_id,
                replica_id,
                client_seq,
                operation_id,
                operation,
            } => encode_structure_op(
                &library_id,
                &replica_id,
                &client_seq,
                &operation_id,
                operation,
            ),
            Request::EncodeDeleteStrokeLineage {
                library_id,
                replica_id,
                client_seq,
                operation_id,
                notebook_id,
                page_id,
                root_id,
            } => encode_delete_stroke_lineage(
                &library_id,
                &replica_id,
                &client_seq,
                &operation_id,
                &notebook_id,
                &page_id,
                &root_id,
            ),
            Request::ProjectLibrary { snapshot, ops } => project_library(snapshot, ops),
            Request::CreateNotebookRender => {
                serde_json::to_string(&sealed_create_notebook_render()).map_err(err)
            }
            Request::EncodeAddText {
                library_id,
                replica_id,
                client_seq,
                operation_id,
                notebook_id,
                page_id,
                text,
            } => encode_add_text(
                &library_id,
                &replica_id,
                &client_seq,
                &operation_id,
                &notebook_id,
                &page_id,
                text,
            ),
            Request::EncodeUpdateText {
                library_id,
                replica_id,
                client_seq,
                operation_id,
                notebook_id,
                page_id,
                text,
            } => encode_update_text(
                &library_id,
                &replica_id,
                &client_seq,
                &operation_id,
                &notebook_id,
                &page_id,
                text,
            ),
            Request::EncodeDeleteText {
                library_id,
                replica_id,
                client_seq,
                operation_id,
                notebook_id,
                page_id,
                text_id,
            } => encode_delete_text(
                &library_id,
                &replica_id,
                &client_seq,
                &operation_id,
                &notebook_id,
                &page_id,
                &text_id,
            ),
            Request::ProjectTexts { texts, ops } => project_texts(texts, ops),
        });
    match result {
        Ok(v) => serde_json::json!({ "ok": v }).to_string(),
        Err(e) => serde_json::json!({ "err": e }).to_string(),
    }
}

// ------------------------------------------------------------------ wasm ABI

/// Allocate an exact `len`-byte buffer for the caller to fill (request buffers).
#[no_mangle]
pub extern "C" fn swa_alloc(len: usize) -> *mut u8 {
    Box::into_raw(vec![0u8; len].into_boxed_slice()) as *mut u8
}

/// Free a buffer from [`swa_alloc`] or a [`swa_call`] result (exact length).
///
/// # Safety
/// `ptr`/`len` must come from `swa_alloc(len)` or a `swa_call` result, freed exactly once.
#[no_mangle]
pub unsafe extern "C" fn swa_free(ptr: *mut u8, len: usize) {
    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
}

/// Handle one JSON request held in `[ptr, ptr+len)`; returns `(out_ptr << 32) | out_len`.
///
/// # Safety
/// `ptr`/`len` must describe an initialized buffer from [`swa_alloc`].
#[no_mangle]
pub unsafe extern "C" fn swa_call(ptr: *const u8, len: usize) -> u64 {
    let req = std::slice::from_raw_parts(ptr, len);
    let out = match std::str::from_utf8(req) {
        Ok(s) => handle(s),
        Err(e) => serde_json::json!({ "err": format!("request is not UTF-8: {e}") }).to_string(),
    };
    let boxed = out.into_bytes().into_boxed_slice();
    let l = boxed.len();
    let p = Box::into_raw(boxed) as *mut u8;
    ((p as usize as u64) << 32) | l as u64
}
