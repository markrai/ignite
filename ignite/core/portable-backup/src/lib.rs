//! Portable Ignite backup (Backup & Restore, Slice 1B): the **logical** contents of a backup
//! and their semantic verification. No archive format, filesystem, network or UI lives here.
//!
//! ```text
//! manifest.json                       BackupManifestV1 (canonical JSON)
//! libraries/<library-id>/snapshot.json  exact canonical SyncSnapshotV1 bytes of the source library
//! blobs/<sha256>                       background bytes, stored once, shared by every library
//! ```
//!
//! A backup is current state only: no operation history, `HistoryV1`, credentials, device or
//! replica identities, server identity, or browser state. Restoring a snapshot makes it the
//! genesis of a new library; `LibraryState::genesis` rebuilds the history it needs.
//!
//! # Verification
//! [`BackupVerifier`] is fed entries one at a time, in any order, as a future archive reader
//! produces them: the manifest first, then snapshots ([`BackupVerifier::add_snapshot`]) and blobs
//! ([`BackupVerifier::verify_blob`], streamed and hashed incrementally). [`BackupVerifier::finish`]
//! proves completeness. The verifier fails closed: the first refusal poisons it.
//!
//! Snapshot parsing, validation and canonical bytes come from `sync-protocol`; semantic state
//! from `sync-engine`. This crate adds only backup-level rules (manifest, counts, hashes,
//! blob completeness, bounds).
//!
//! # Security boundary
//! [`EntryPath::parse`] validates logical entry names only. ZIP-specific concerns (symlinks,
//! compression ratios, duplicate central-directory names, encryption) belong to the archive
//! reader. [`Limits`] bounds the logical sizes and counts this crate accepts.
//!
//! # Re-keying
//! [`rekey_library`] gives a verified snapshot an explicitly supplied new `LibraryId`, changing
//! nothing else. The source snapshot hash and the restored snapshot hash are kept distinct.

mod error;
mod manifest;
mod paths;
mod rekey;
mod verify;

pub use error::{BackupError, BackupErrorCode, Result};
pub use manifest::{
    BackupManifestV1, Limits, ManifestBlob, ManifestBuilder, ManifestLibrary, SnapshotSource,
    BACKUP_FORMAT, BACKUP_FORMAT_VERSION,
};
pub use paths::{EntryPath, MANIFEST_PATH, MAX_ENTRY_PATH_LEN};
pub use rekey::{rekey_library, RestoredSnapshot};
pub use verify::{BackupVerifier, VerifiedBackup, VerifiedSnapshot};
