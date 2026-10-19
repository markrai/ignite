//! Library re-keying: a restored library gets a new, caller-supplied `LibraryId`.
//!
//! `SyncSnapshotV1::library_id` is the snapshot's only library reference (links, folders and
//! lineage use notebook/page/stroke/text/link/folder IDs), so re-keying changes exactly one
//! field. Every other identity is preserved: links still name their target pages, fragments
//! still name their lineage roots, and Kindle stroke IDs still round-trip. A new identity keeps
//! stale replicas of the source library (which address it by `LibraryId`) from ever mistaking
//! the restored library for theirs.

use sync_engine::LibraryState;
use sync_protocol::{sha256_hex, LibraryId, Sha256Hex};

use crate::error::{fail, BackupError, BackupErrorCode as C, Result};
use crate::verify::VerifiedSnapshot;

/// A re-keyed snapshot ready to become genesis. `canonical_bytes` is exactly what
/// `Store::bootstrap` / `POST /v1/libraries` expects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredSnapshot {
    pub source_library_id: LibraryId,
    /// Hash of the snapshot as stored in the backup (source identity).
    pub source_snapshot_sha256: Sha256Hex,
    pub library_id: LibraryId,
    /// Hash of `canonical_bytes` (restored identity). Always differs from the source hash.
    pub restored_snapshot_sha256: Sha256Hex,
    pub canonical_bytes: Vec<u8>,
}

/// Re-key `source` to `new_library_id`. Never mints an ID; the source is not modified.
pub fn rekey_library(
    source: &VerifiedSnapshot,
    new_library_id: LibraryId,
) -> Result<RestoredSnapshot> {
    let source_library_id = source.source_library_id();
    if new_library_id == source_library_id {
        return fail(
            C::SameLibrary,
            format!(
                "re-keying must assign a new library id (got the source id {source_library_id})"
            ),
        );
    }
    let mut snapshot = source.snapshot().clone();
    snapshot.library_id = new_library_id;
    let canonical_bytes = snapshot
        .to_canonical_bytes()
        .map_err(|e| BackupError::new(C::InvalidSnapshot, e))?;
    LibraryState::genesis(snapshot).map_err(|e| BackupError::new(C::InvalidState, e))?;
    Ok(RestoredSnapshot {
        source_library_id,
        source_snapshot_sha256: source.source_snapshot_sha256().clone(),
        library_id: new_library_id,
        restored_snapshot_sha256: sha256_hex(&canonical_bytes),
        canonical_bytes,
    })
}
