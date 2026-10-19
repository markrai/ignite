//! Read-only portable backup exporter (Backup & Restore, Slice 1C).
//!
//! Pulls every library from an existing sync server through the `/v1` GET API only:
//!
//! ```text
//! GET /v1/libraries                      library ids (head/counts are informational only)
//! GET /v1/libraries/{id}/snapshot        exact canonical snapshot bytes + through_server_seq
//! GET /v1/blobs/{sha256}                 background bytes (streamed)
//! ```
//!
//! and writes a ZIP whose logical layout and verification come from `portable-backup`:
//! `manifest.json`, `libraries/<source-library-id>/snapshot.json`, `blobs/<sha256>`. Source
//! library identities and snapshot bytes are kept exactly; re-keying is the importer's job.
//!
//! The exporter never pairs, never stores or logs credentials, and never writes to the source.

mod export;
mod source;

pub use export::{
    export_backup, export_backup_with, ExportError, ExportErrorKind, ExportOptions, ExportReport,
};
pub use source::{Credential, HttpSource, ReadOnlySource, Secret, SourceError, SourceResponse};
