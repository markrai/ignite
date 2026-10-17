//! Scribe sync domain model (Sync Phase S1).
//!
//! Platform-independent: no Kindle, framebuffer, evdev, appmgrd/LIPC, filesystem layout, or
//! networking. Builds for the Kindle (armv7), servers, and `wasm32-unknown-unknown`.
//!
//! ```text
//! local durable files (library.meta, strokes.jsonl, …)   ← replica-private materialization
//!        !=
//! canonical sync model (this crate)                        ← IDs, snapshots, operations
//!        !=
//! transport / API                                          ← future (S2+)
//! ```
//!
//! Every client is an offline-capable replica; no client owns notebook content. A future
//! server replicates, deduplicates (by [`OperationId`]) and assigns canonical order, but local
//! writes never require it. See `docs/SYNC_ARCHITECTURE.md`.

mod error;

pub mod binding;
pub mod canonical;
pub mod content;
pub mod geometry;
pub mod ids;
pub mod ops;
pub mod replica;
pub mod snapshot;
pub mod stroke_ids;

pub use binding::{
    reconcile_identity, IdentityFileV1, LiveLayout, NotebookBinding, PageBinding, Reconciled,
    IDENTITY_FILE_VERSION,
};
pub use canonical::{from_json, sha256_hex, to_canonical_json};
pub use content::{
    normalize_text_content, validate_text_content, validate_title, BackgroundEncoding,
    BackgroundFormat, BackgroundRef, PressureCurveRef, RenderProfileRef, Sha256Hex, StrokeStyle,
    SyncPageLink, SyncStroke, SyncText, TextFace, DEFAULT_SANS_V1_FACE, MAX_TEXT_CHARS,
    MAX_TEXT_FONT_SIZE, MIN_TEXT_FONT_SIZE,
};
pub use error::{ProtocolError, Result};
pub use geometry::{PageSpace, SyncPoint, CANONICAL_PAGE_HEIGHT, CANONICAL_PAGE_WIDTH};
pub use ids::{
    FolderId, LibraryId, LinkId, NotebookId, OperationId, PageId, ReplicaId, StrokeId, TextId,
};
pub use ops::{
    check_client_seq, LineageReplacement, PageSpec, SyncEnvelopeV1, SyncOperationV1,
    ENVELOPE_VERSION, MAX_ERASE_PATH_POINTS, MAX_REPLACE_AFTER_STROKES, MAX_REPLACE_LINEAGE_ROOTS,
};
pub use replica::{ReplicaStateV1, REPLICA_STATE_FILE, REPLICA_STATE_VERSION};
pub use snapshot::{
    snapshot_sha256, SnapshotFolder, SnapshotNotebook, SnapshotPage, SyncSnapshotV1,
    SNAPSHOT_VERSION,
};
pub use stroke_ids::{
    classify_local_stroke_id, global_stroke_id, uuid_backed_local_stroke_id, LocalStrokeIdKind,
};
