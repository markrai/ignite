//! Restore side of Ignite portable backups (Backup & Restore, Slice 1D): a strict ZIP reader,
//! complete verification, and a read-only [`RestorePlan`]. Nothing is restored, written or
//! extracted; no IDs are minted.
//!
//! ```text
//! inspect_backup(path)
//!   1. container::parse   structure only: end records, central directory, local headers,
//!                         layout, accepted features, canonical unique names, declared sizes
//!   2. manifest.json      inflated (bounded, CRC-checked) → BackupVerifier::new
//!   3. every other entry  streamed through EntryReader into BackupVerifier::add_snapshot /
//!                         verify_blob (bounded inflation, exact size, CRC, full consumption)
//!   4. BackupVerifier::finish → RestorePlan (metadata only)
//! ```
//!
//! Semantic rules (canonical bytes, hashes, counts, references, completeness, versions) are
//! `portable-backup`'s; this crate adds only the ZIP layer. An archive is valid only when both
//! layers accept it. Entry names are only ever parsed into `EntryPath` values and never used
//! to build filesystem paths.
//!
//! # Compression ratio
//! There is deliberately no ratio limit: a blank or template background (gray8, megabytes of
//! one value) legitimately deflates by ~1000:1, deflate's own ceiling. Inflation is bounded
//! instead by absolute sizes: each entry by its declared size (itself bounded by `Limits` per
//! kind and in total), and the manifest's declarations are enforced by `BackupVerifier`.

mod container;
mod entry;
mod error;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

use portable_backup::{BackupVerifier, EntryPath, Limits, BACKUP_FORMAT, BACKUP_FORMAT_VERSION};
use sha2::{Digest, Sha256};
use sync_protocol::{BackgroundRef, LibraryId, Sha256Hex};

use crate::container::Entry;
use crate::entry::EntryReader;
pub use crate::error::{RestoreError, RestoreErrorKind, ZipErrorCode};

/// What a restore of this archive would and would not bring back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreScope {
    /// Each library's visible state at export time becomes the genesis of a new library.
    CurrentStateOnly,
}

/// Never part of a portable backup, so never restored.
pub const NOT_RESTORED: &[&str] = &[
    "operation history (each restored library starts at server sequence 0)",
    "deleted-object history (tombstones of deleted notebooks, pages, strokes, text and links)",
    "device credentials and pairings",
    "server identity",
    "browser replica state",
    "edits not synchronized to the source server before export",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedLibrary {
    /// Identity in the source server; a restore assigns a new one.
    pub source_library_id: LibraryId,
    pub source_snapshot_sha256: Sha256Hex,
    pub source_snapshot_bytes: u64,
    /// Provenance only.
    pub source_through_server_seq: u64,
    pub notebook_count: u64,
    pub page_count: u64,
    /// Distinct background blobs this library references.
    pub blob_count: u64,
}

/// Read-only description of a fully verified archive. Holds metadata only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestorePlan {
    pub archive_bytes: u64,
    /// SHA-256 of the archive file as inspected (identifies it for a later import).
    pub archive_sha256: Sha256Hex,
    pub format: &'static str,
    pub format_version: u32,
    pub snapshot_version: u32,
    pub scope: RestoreScope,
    /// Ascending by source library id.
    pub libraries: Vec<PlannedLibrary>,
    pub total_notebooks: u64,
    pub total_pages: u64,
    /// Distinct blobs in the archive (each stored once even when shared).
    pub blob_count: u64,
    pub total_blob_bytes: u64,
    pub not_restored: &'static [&'static str],
    pub warnings: Vec<String>,
}

/// Open `path` read-only and inspect it. See [`inspect_reader`].
pub fn inspect_backup(path: &Path, limits: &Limits) -> Result<RestorePlan, RestoreError> {
    let file =
        File::open(path).map_err(|e| RestoreError::io(format!("open {}: {e}", path.display())))?;
    inspect_reader(&mut BufReader::new(file), limits)
}

/// Fully verify an archive and describe what restoring it would do. Fails closed: any ZIP or
/// semantic refusal is an error, and no partial plan is returned.
pub fn inspect_reader<R: Read + Seek>(
    r: &mut R,
    limits: &Limits,
) -> Result<RestorePlan, RestoreError> {
    let container = container::parse(r, limits)?;
    let manifest_entry = container
        .entries
        .iter()
        .find(|e| e.path == EntryPath::Manifest)
        .expect("container::parse requires a manifest");
    let manifest_bytes = read_whole(r, manifest_entry)?;
    let mut verifier = BackupVerifier::new(&manifest_bytes, limits)?;

    let mut blob_counts = std::collections::BTreeMap::new();
    for entry in container
        .entries
        .iter()
        .filter(|e| e.path != EntryPath::Manifest)
    {
        let mut reader = EntryReader::open(r, entry)?;
        let outcome = match &entry.path {
            EntryPath::Snapshot(id) => verifier.add_snapshot(*id, &mut reader).map(|s| {
                let blobs: BTreeSet<&Sha256Hex> = s
                    .snapshot()
                    .notebooks
                    .iter()
                    .flat_map(|n| &n.pages)
                    .filter_map(|p| match &p.background {
                        BackgroundRef::Blob { sha256, .. } => Some(sha256),
                        BackgroundRef::Blank => None,
                    })
                    .collect();
                blob_counts.insert(*id, blobs.len() as u64);
            }),
            EntryPath::Blob(sha) => verifier.verify_blob(sha, &mut reader),
            EntryPath::Manifest => unreachable!("filtered"),
        };
        if let Some(zip_failure) = reader.failure() {
            return Err(zip_failure.clone());
        }
        outcome?;
        reader.finish()?;
    }
    let verified = verifier.finish()?;
    let manifest = verified.manifest();

    let libraries: Vec<PlannedLibrary> = manifest
        .libraries
        .iter()
        .map(|l| PlannedLibrary {
            source_library_id: l.source_library_id,
            source_snapshot_sha256: l.source_snapshot_sha256.clone(),
            source_snapshot_bytes: l.source_snapshot_bytes,
            source_through_server_seq: l.source_through_server_seq,
            notebook_count: l.notebook_count,
            page_count: l.page_count,
            blob_count: blob_counts[&l.source_library_id],
        })
        .collect();
    let mut warnings = vec![
        "restored libraries receive new library identities at import; the source ids above are provenance only".to_string(),
    ];
    if libraries.len() > 1 {
        warnings.push(
            "libraries were captured one after another, not at a single point in time".to_string(),
        );
    }
    let archive_sha256 = hash_all(r)?;
    Ok(RestorePlan {
        archive_bytes: container.archive_bytes,
        archive_sha256,
        format: BACKUP_FORMAT,
        format_version: BACKUP_FORMAT_VERSION,
        snapshot_version: manifest.snapshot_version,
        scope: RestoreScope::CurrentStateOnly,
        total_notebooks: libraries.iter().map(|l| l.notebook_count).sum(),
        total_pages: libraries.iter().map(|l| l.page_count).sum(),
        blob_count: manifest.blobs.len() as u64,
        total_blob_bytes: manifest.blobs.iter().map(|b| b.byte_len).sum(),
        libraries,
        not_restored: NOT_RESTORED,
        warnings,
    })
}

/// The manifest is small and bounded (declared size ≤ `max_manifest_bytes`, checked during
/// container parsing); read it whole.
fn read_whole<R: Read + Seek>(r: &mut R, entry: &Entry) -> Result<Vec<u8>, RestoreError> {
    let mut reader = EntryReader::open(r, entry)?;
    let mut bytes = Vec::with_capacity(usize::try_from(entry.size).unwrap_or(0));
    let read = reader.read_to_end(&mut bytes);
    if let Some(e) = reader.failure() {
        return Err(e.clone());
    }
    read.map_err(|e| RestoreError::io(format!("read {}: {e}", entry.path)))?;
    reader.finish()?;
    Ok(bytes)
}

fn hash_all<R: Read + Seek>(r: &mut R) -> Result<Sha256Hex, RestoreError> {
    r.rewind()
        .map_err(|e| RestoreError::io(format!("rewind: {e}")))?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = r
            .read(&mut chunk)
            .map_err(|e| RestoreError::io(format!("hash archive: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    Ok(Sha256Hex::from_digest(&digest))
}
