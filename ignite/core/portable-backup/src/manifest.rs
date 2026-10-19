//! `BackupManifestV1` (`manifest.json`) and the logical bounds a backup must respect.
//!
//! Canonical form, like every sync DTO: compact JSON, fields in declaration order, libraries
//! strictly ascending by `source_library_id`, blobs strictly ascending by `sha256`. Parsing is
//! strict (`deny_unknown_fields`, typed IDs and hashes, explicit versions) and refuses
//! non-canonical bytes, so one logical backup has exactly one manifest.
//!
//! The manifest repeats only what integrity and planning need before a snapshot is read:
//! each snapshot's hash and size, counts to cross-check, and the blob set (hash + length) so a
//! reader can verify and bound blob entries independently of snapshot order.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sync_protocol::{from_json, to_canonical_json, LibraryId, Sha256Hex, SNAPSHOT_VERSION};

use crate::error::{fail, BackupErrorCode as C, Result};
use crate::verify::inspect_snapshot;

/// Value of [`BackupManifestV1::format`].
pub const BACKUP_FORMAT: &str = "ignite-portable-backup";
pub const BACKUP_FORMAT_VERSION: u32 = 1;

/// Explicit bounds on logical input. Defaults are generous for real libraries yet finite;
/// callers may tighten them. Byte counts are `u64` so 32-bit (wasm32) builds stay exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_manifest_bytes: u64,
    pub max_libraries: usize,
    pub max_snapshot_bytes: u64,
    pub max_total_snapshot_bytes: u64,
    pub max_blobs: usize,
    /// Matches the sync server's largest accepted blob body (32 MiB).
    pub max_blob_bytes: u64,
    pub max_total_blob_bytes: u64,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        max_manifest_bytes: 8 << 20,
        max_libraries: 256,
        max_snapshot_bytes: 512 << 20,
        max_total_snapshot_bytes: 2 << 30,
        max_blobs: 50_000,
        max_blob_bytes: 32 << 20,
        max_total_blob_bytes: 64 << 30,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupManifestV1 {
    /// Always [`BACKUP_FORMAT`].
    pub format: String,
    /// Always [`BACKUP_FORMAT_VERSION`].
    pub format_version: u32,
    /// `SyncSnapshotV1::v` of every snapshot in the backup.
    pub snapshot_version: u32,
    /// Strictly ascending by `source_library_id`; at least one.
    pub libraries: Vec<ManifestLibrary>,
    /// Every referenced blob exactly once, strictly ascending by `sha256`.
    pub blobs: Vec<ManifestBlob>,
}

/// One source library. Everything here describes the library as it was exported; a restore
/// assigns a new identity and therefore a new snapshot hash (see `rekey_library`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestLibrary {
    pub source_library_id: LibraryId,
    /// SHA-256 of the exact canonical snapshot bytes in the backup.
    pub source_snapshot_sha256: Sha256Hex,
    /// Length of those bytes.
    pub source_snapshot_bytes: u64,
    /// Provenance only: the source server's `through_server_seq` for this snapshot. Never used
    /// to position a restored library (which starts at server sequence 0).
    pub source_through_server_seq: u64,
    pub notebook_count: u64,
    pub page_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestBlob {
    pub sha256: Sha256Hex,
    pub byte_len: u64,
}

/// One library's canonical snapshot bytes plus provenance, for [`BackupManifestV1::for_snapshots`].
#[derive(Clone, Copy, Debug)]
pub struct SnapshotSource<'a> {
    pub through_server_seq: u64,
    pub canonical_bytes: &'a [u8],
}

impl BackupManifestV1 {
    /// Strict parse: size bound, format/version, unknown fields, structure, limits, canonical form.
    pub fn parse(bytes: &[u8], limits: &Limits) -> Result<Self> {
        if bytes.len() as u64 > limits.max_manifest_bytes {
            return fail(
                C::LimitExceeded,
                format!(
                    "manifest is {} bytes (limit {})",
                    bytes.len(),
                    limits.max_manifest_bytes
                ),
            );
        }
        // Read the identifying header leniently first, so a newer manifest is reported as an
        // unsupported version rather than as unknown fields.
        #[derive(Deserialize)]
        struct Header {
            format: String,
            format_version: u32,
        }
        let h: Header = from_json(bytes).map_err(|e| err(C::InvalidManifest, e))?;
        check_header(&h.format, h.format_version)?;
        let m: Self = from_json(bytes).map_err(|e| err(C::InvalidManifest, e))?;
        m.validate(limits)?;
        let canonical = to_canonical_json(&m).map_err(|e| err(C::InvalidManifest, e))?;
        if canonical != bytes {
            return fail(
                C::NonCanonical,
                "manifest is not canonical BackupManifestV1 bytes",
            );
        }
        Ok(m)
    }

    /// Validated canonical bytes.
    pub fn to_canonical_bytes(&self, limits: &Limits) -> Result<Vec<u8>> {
        self.validate(limits)?;
        to_canonical_json(self).map_err(|e| err(C::InvalidManifest, e))
    }

    /// Describe a set of canonical snapshots. Each one is fully verified exactly as
    /// [`crate::BackupVerifier`] will verify it; input order does not matter.
    pub fn for_snapshots(sources: &[SnapshotSource<'_>], limits: &Limits) -> Result<Self> {
        let mut b = ManifestBuilder::new();
        for s in sources {
            b.add_snapshot(*s, limits)?;
        }
        b.finish(limits)
    }

    /// Structure and bounds (no snapshot or blob content is consulted).
    pub fn validate(&self, limits: &Limits) -> Result<()> {
        check_header(&self.format, self.format_version)?;
        if self.snapshot_version != SNAPSHOT_VERSION {
            return fail(
                C::UnsupportedVersion,
                format!(
                    "snapshot version {} (need {SNAPSHOT_VERSION})",
                    self.snapshot_version
                ),
            );
        }
        if self.libraries.is_empty() {
            return fail(
                C::InvalidManifest,
                "a backup must contain at least one library",
            );
        }
        if self.libraries.len() > limits.max_libraries {
            return fail(
                C::LimitExceeded,
                format!(
                    "{} libraries (limit {})",
                    self.libraries.len(),
                    limits.max_libraries
                ),
            );
        }
        let mut total = 0u64;
        for (i, l) in self.libraries.iter().enumerate() {
            if let Some(prev) = i.checked_sub(1).map(|p| &self.libraries[p]) {
                if prev.source_library_id == l.source_library_id {
                    return fail(
                        C::DuplicateEntry,
                        format!("library {} is listed twice", l.source_library_id),
                    );
                }
                if prev.source_library_id > l.source_library_id {
                    return fail(
                        C::InvalidManifest,
                        "libraries must be strictly ascending by source_library_id",
                    );
                }
            }
            if l.source_snapshot_bytes == 0 {
                return fail(
                    C::InvalidManifest,
                    format!("library {} has an empty snapshot", l.source_library_id),
                );
            }
            if l.source_snapshot_bytes > limits.max_snapshot_bytes {
                return fail(
                    C::LimitExceeded,
                    format!(
                        "snapshot of library {} is {} bytes (limit {})",
                        l.source_library_id, l.source_snapshot_bytes, limits.max_snapshot_bytes
                    ),
                );
            }
            total = total
                .checked_add(l.source_snapshot_bytes)
                .filter(|t| *t <= limits.max_total_snapshot_bytes)
                .ok_or_else(|| {
                    err(
                        C::LimitExceeded,
                        format!(
                            "snapshots exceed {} bytes in total",
                            limits.max_total_snapshot_bytes
                        ),
                    )
                })?;
        }
        if self.blobs.len() > limits.max_blobs {
            return fail(
                C::LimitExceeded,
                format!("{} blobs (limit {})", self.blobs.len(), limits.max_blobs),
            );
        }
        let mut total = 0u64;
        for (i, b) in self.blobs.iter().enumerate() {
            if let Some(prev) = i.checked_sub(1).map(|p| &self.blobs[p]) {
                if prev.sha256 == b.sha256 {
                    return fail(
                        C::DuplicateEntry,
                        format!("blob {} is listed twice", b.sha256),
                    );
                }
                if prev.sha256 > b.sha256 {
                    return fail(
                        C::InvalidManifest,
                        "blobs must be strictly ascending by sha256",
                    );
                }
            }
            if b.byte_len == 0 {
                return fail(C::InvalidManifest, format!("blob {} is empty", b.sha256));
            }
            if b.byte_len > limits.max_blob_bytes {
                return fail(
                    C::LimitExceeded,
                    format!(
                        "blob {} is {} bytes (limit {})",
                        b.sha256, b.byte_len, limits.max_blob_bytes
                    ),
                );
            }
            total = total
                .checked_add(b.byte_len)
                .filter(|t| *t <= limits.max_total_blob_bytes)
                .ok_or_else(|| {
                    err(
                        C::LimitExceeded,
                        format!(
                            "blobs exceed {} bytes in total",
                            limits.max_total_blob_bytes
                        ),
                    )
                })?;
        }
        Ok(())
    }
}

/// Incremental [`BackupManifestV1`] construction for bounded-memory producers: each snapshot is
/// verified and described as it arrives, and its bytes need not be retained afterwards. The
/// result is identical to [`BackupManifestV1::for_snapshots`] over the same snapshots, in any
/// order.
#[derive(Clone, Debug, Default)]
pub struct ManifestBuilder {
    libraries: Vec<ManifestLibrary>,
    blobs: BTreeMap<Sha256Hex, u64>,
}

impl ManifestBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Verify one canonical snapshot exactly as [`crate::BackupVerifier`] will, and record it.
    /// Library count, duplicate library and snapshot size bounds are enforced here, before
    /// the snapshot is parsed; the remaining bounds are enforced by [`Self::finish`].
    pub fn add_snapshot(
        &mut self,
        source: SnapshotSource<'_>,
        limits: &Limits,
    ) -> Result<&ManifestLibrary> {
        let len = source.canonical_bytes.len() as u64;
        if len > limits.max_snapshot_bytes {
            return fail(
                C::LimitExceeded,
                format!(
                    "snapshot is {len} bytes (limit {})",
                    limits.max_snapshot_bytes
                ),
            );
        }
        if self.libraries.len() >= limits.max_libraries {
            return fail(
                C::LimitExceeded,
                format!("more than {} libraries", limits.max_libraries),
            );
        }
        let ins = inspect_snapshot(source.canonical_bytes)?;
        let id = ins.snapshot.library_id;
        if self.libraries.iter().any(|l| l.source_library_id == id) {
            return fail(C::DuplicateEntry, format!("library {id} was added twice"));
        }
        for (sha, len) in &ins.blobs {
            if let Some(l) = self.blobs.get(sha) {
                if l != len {
                    return fail(
                        C::BlobLengthMismatch,
                        format!("blob {sha} is referenced with lengths {l} and {len}"),
                    );
                }
            }
        }
        self.blobs.extend(ins.blobs);
        self.libraries.push(ManifestLibrary {
            source_library_id: id,
            source_snapshot_sha256: sync_protocol::sha256_hex(source.canonical_bytes),
            source_snapshot_bytes: len,
            source_through_server_seq: source.through_server_seq,
            notebook_count: ins.notebooks,
            page_count: ins.pages,
        });
        Ok(self.libraries.last().expect("just pushed"))
    }

    /// Blobs referenced so far, ascending by sha256.
    pub fn blobs(&self) -> Vec<ManifestBlob> {
        self.blobs
            .iter()
            .map(|(sha256, byte_len)| ManifestBlob {
                sha256: sha256.clone(),
                byte_len: *byte_len,
            })
            .collect()
    }

    /// The canonical, validated manifest.
    pub fn finish(mut self, limits: &Limits) -> Result<BackupManifestV1> {
        self.libraries
            .sort_by(|a, b| a.source_library_id.cmp(&b.source_library_id));
        let m = BackupManifestV1 {
            format: BACKUP_FORMAT.to_string(),
            format_version: BACKUP_FORMAT_VERSION,
            snapshot_version: SNAPSHOT_VERSION,
            blobs: self.blobs(),
            libraries: self.libraries,
        };
        m.validate(limits)?;
        Ok(m)
    }
}

fn check_header(format: &str, version: u32) -> Result<()> {
    if format != BACKUP_FORMAT {
        return fail(
            C::InvalidManifest,
            format!("format {format:?} is not {BACKUP_FORMAT:?}"),
        );
    }
    if version != BACKUP_FORMAT_VERSION {
        return fail(
            C::UnsupportedVersion,
            format!("backup format version {version} (need {BACKUP_FORMAT_VERSION})"),
        );
    }
    Ok(())
}

fn err(code: C, message: impl std::fmt::Display) -> crate::BackupError {
    crate::BackupError::new(code, message)
}
