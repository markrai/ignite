//! Scribe sync engine (Phase S2A): the deterministic semantic reducer.
//!
//! ```text
//! LibraryState(genesis SyncSnapshotV1) + canonically ordered SyncEnvelopeV1s → LibraryState
//! ```
//!
//! **The server assigns order; this crate defines meaning.** Given the same state and the same
//! envelope, every platform (Kindle armv7, host x86_64, wasm32) produces bit-identical canonical
//! snapshot bytes. Application is pure: no filesystem, HTTP, SQLite, Kindle APIs, clocks or
//! randomness (new IDs are derived with UUIDv5 from the operation ID).
//!
//! Each application either yields one validated new state ([`Outcome::Applied`] or
//! [`Outcome::NoOp`]) or fails closed with a deterministic [`SemanticError`]. Nothing is ever
//! repaired silently. See `docs/SYNC_ARCHITECTURE.md` for the per-operation rules.

mod apply;
mod erase;
mod folders;
mod state;
#[cfg(feature = "testkit")]
pub mod testkit;

pub use apply::{Outcome, SemanticError, SemanticErrorCode};
pub use erase::{erase_page_strokes, fragment_id, sweep_segments, EraseResult};
pub use folders::{apply_folder_operation, FolderHost, FolderProjection, NotebookSlot};
pub use state::{replay, HistoryV1, LibraryState, HISTORY_VERSION};
