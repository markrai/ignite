//! Streaming-friendly semantic verification of one logical backup.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use sync_engine::LibraryState;
use sync_protocol::{
    from_json, sha256_hex, BackgroundRef, LibraryId, Sha256Hex, SyncSnapshotV1, SNAPSHOT_VERSION,
};

use crate::error::{fail, BackupError, BackupErrorCode as C, Result};
use crate::manifest::{BackupManifestV1, Limits};

const CHUNK: usize = 64 * 1024;
/// Upper bound on the up-front buffer reservation for one snapshot (it grows past this only
/// as bytes actually arrive, never beyond the manifest's declared length + 1).
const MAX_RESERVE: u64 = 64 << 20;

/// A snapshot that passed every per-library check. Backup completeness is only established by
/// [`BackupVerifier::finish`]; callers that restore must stage until it succeeds.
#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedSnapshot {
    snapshot: SyncSnapshotV1,
    source_snapshot_sha256: Sha256Hex,
    source_through_server_seq: u64,
}

impl VerifiedSnapshot {
    /// The source snapshot exactly as stored in the backup (source `library_id`).
    pub fn snapshot(&self) -> &SyncSnapshotV1 {
        &self.snapshot
    }

    pub fn source_library_id(&self) -> LibraryId {
        self.snapshot.library_id
    }

    pub fn source_snapshot_sha256(&self) -> &Sha256Hex {
        &self.source_snapshot_sha256
    }

    pub fn source_through_server_seq(&self) -> u64 {
        self.source_through_server_seq
    }
}

/// Proof that every manifest entry was supplied and verified, with nothing missing or extra.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedBackup {
    manifest: BackupManifestV1,
}

impl VerifiedBackup {
    pub fn manifest(&self) -> &BackupManifestV1 {
        &self.manifest
    }

    pub fn into_manifest(self) -> BackupManifestV1 {
        self.manifest
    }
}

/// Verifies a backup entry by entry, in any order, after the manifest.
///
/// * [`Self::new`] — strict manifest parse (bounded by [`Limits`]).
/// * [`Self::add_snapshot`] — reads at most the declared length + 1 bytes, checks length and
///   hash, then `SyncSnapshotV1::parse`, exact canonical bytes, `LibraryState::genesis`, path /
///   library identity, counts and blob references against the manifest.
/// * [`Self::verify_blob`] — streams and hashes in fixed chunks; never holds a whole blob.
/// * [`Self::finish`] — every library supplied; every listed blob referenced and verified.
///
/// The first refusal poisons the verifier: later calls return [`C::Aborted`].
#[derive(Debug)]
pub struct BackupVerifier {
    manifest: BackupManifestV1,
    /// Library → (manifest index, snapshot verified).
    libraries: BTreeMap<LibraryId, (usize, bool)>,
    /// Blob → (declared length, bytes verified).
    blobs: BTreeMap<Sha256Hex, (u64, bool)>,
    /// Blobs referenced by verified snapshots.
    referenced: BTreeMap<Sha256Hex, u64>,
    failed: bool,
}

impl BackupVerifier {
    pub fn new(manifest_bytes: &[u8], limits: &Limits) -> Result<Self> {
        let manifest = BackupManifestV1::parse(manifest_bytes, limits)?;
        let libraries = manifest
            .libraries
            .iter()
            .enumerate()
            .map(|(i, l)| (l.source_library_id, (i, false)))
            .collect();
        let blobs = manifest
            .blobs
            .iter()
            .map(|b| (b.sha256.clone(), (b.byte_len, false)))
            .collect();
        Ok(Self {
            manifest,
            libraries,
            blobs,
            referenced: BTreeMap::new(),
            failed: false,
        })
    }

    pub fn manifest(&self) -> &BackupManifestV1 {
        &self.manifest
    }

    /// Verify `libraries/<library_id>/snapshot.json`. `library_id` comes from the entry path
    /// ([`crate::EntryPath::Snapshot`]).
    pub fn add_snapshot<R: Read>(
        &mut self,
        library_id: LibraryId,
        reader: R,
    ) -> Result<VerifiedSnapshot> {
        self.guard()?;
        let r = self.add_snapshot_inner(library_id, reader);
        self.failed |= r.is_err();
        r
    }

    /// Verify `blobs/<sha256>` by streaming its bytes.
    pub fn verify_blob<R: Read>(&mut self, sha256: &Sha256Hex, reader: R) -> Result<()> {
        self.guard()?;
        let r = self.verify_blob_inner(sha256, reader);
        self.failed |= r.is_err();
        r
    }

    pub fn finish(self) -> Result<VerifiedBackup> {
        if self.failed {
            return aborted();
        }
        for (id, (_, done)) in &self.libraries {
            if !done {
                return fail(
                    C::MissingEntry,
                    format!("snapshot of library {id} was not supplied"),
                );
            }
        }
        for (sha, (_, verified)) in &self.blobs {
            if !self.referenced.contains_key(sha) {
                return fail(
                    C::UnreferencedBlob,
                    format!("blob {sha} is not referenced by any snapshot"),
                );
            }
            if !verified {
                return fail(
                    C::MissingBlob,
                    format!("blob {sha} bytes were not supplied"),
                );
            }
        }
        Ok(VerifiedBackup {
            manifest: self.manifest,
        })
    }

    fn guard(&self) -> Result<()> {
        if self.failed {
            aborted()
        } else {
            Ok(())
        }
    }

    fn add_snapshot_inner<R: Read>(
        &mut self,
        library_id: LibraryId,
        reader: R,
    ) -> Result<VerifiedSnapshot> {
        let Some(&(index, done)) = self.libraries.get(&library_id) else {
            return fail(
                C::UnexpectedEntry,
                format!("library {library_id} is not listed in the manifest"),
            );
        };
        if done {
            return fail(
                C::DuplicateEntry,
                format!("snapshot of library {library_id} was supplied twice"),
            );
        }
        let entry = self.manifest.libraries[index].clone();
        let bytes = read_exact_declared(reader, entry.source_snapshot_bytes, &library_id)?;
        let sha = sha256_hex(&bytes);
        if sha != entry.source_snapshot_sha256 {
            return fail(
                C::HashMismatch,
                format!(
                    "snapshot of library {library_id} hashes to {sha}, manifest says {}",
                    entry.source_snapshot_sha256
                ),
            );
        }
        let ins = inspect_snapshot(&bytes)?;
        drop(bytes);
        if ins.snapshot.library_id != library_id {
            return fail(
                C::LibraryMismatch,
                format!(
                    "snapshot at library path {library_id} is library {}",
                    ins.snapshot.library_id
                ),
            );
        }
        if ins.notebooks != entry.notebook_count || ins.pages != entry.page_count {
            return fail(
                C::CountMismatch,
                format!(
                    "library {library_id} has {} notebooks / {} pages, manifest says {} / {}",
                    ins.notebooks, ins.pages, entry.notebook_count, entry.page_count
                ),
            );
        }
        for (sha, len) in &ins.blobs {
            match self.blobs.get(sha) {
                None => {
                    return fail(
                        C::MissingBlob,
                        format!("library {library_id} references blob {sha}, which the manifest does not list"),
                    )
                }
                Some((declared, _)) if declared != len => {
                    return fail(
                        C::BlobLengthMismatch,
                        format!("library {library_id} references blob {sha} as {len} bytes, manifest says {declared}"),
                    )
                }
                Some(_) => {}
            }
        }
        self.referenced.extend(ins.blobs);
        self.libraries.insert(library_id, (index, true));
        Ok(VerifiedSnapshot {
            snapshot: ins.snapshot,
            source_snapshot_sha256: entry.source_snapshot_sha256,
            source_through_server_seq: entry.source_through_server_seq,
        })
    }

    fn verify_blob_inner<R: Read>(&mut self, sha256: &Sha256Hex, reader: R) -> Result<()> {
        let Some(&(declared, verified)) = self.blobs.get(sha256) else {
            return fail(
                C::UnexpectedEntry,
                format!("blob {sha256} is not listed in the manifest"),
            );
        };
        if verified {
            return fail(
                C::DuplicateEntry,
                format!("blob {sha256} was supplied twice"),
            );
        }
        let mut take = reader.take(declared.saturating_add(1));
        let mut hasher = Sha256::new();
        let mut chunk = vec![0u8; CHUNK];
        let mut total = 0u64;
        loop {
            let n = match take.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return fail(C::Io, format!("read blob {sha256}: {e}")),
            };
            total += n as u64;
            if total > declared {
                return fail(
                    C::SizeMismatch,
                    format!("blob {sha256} is longer than its declared {declared} bytes"),
                );
            }
            hasher.update(&chunk[..n]);
        }
        if total != declared {
            return fail(
                C::SizeMismatch,
                format!("blob {sha256} is {total} bytes, manifest says {declared}"),
            );
        }
        let digest: [u8; 32] = hasher.finalize().into();
        let got = Sha256Hex::from_digest(&digest);
        if got != *sha256 {
            return fail(
                C::HashMismatch,
                format!("blob {sha256} content hashes to {got}"),
            );
        }
        self.blobs.insert(sha256.clone(), (declared, true));
        Ok(())
    }
}

fn aborted<T>() -> Result<T> {
    fail(
        C::Aborted,
        "verification already failed; discard this backup",
    )
}

/// Read exactly `declared` bytes; one extra byte proves the entry is too long.
fn read_exact_declared<R: Read>(
    reader: R,
    declared: u64,
    library_id: &LibraryId,
) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(declared.min(MAX_RESERVE) as usize);
    reader
        .take(declared.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| {
            BackupError::new(C::Io, format!("read snapshot of library {library_id}: {e}"))
        })?;
    let got = buf.len() as u64;
    if got > declared {
        return fail(
            C::SizeMismatch,
            format!(
                "snapshot of library {library_id} is longer than its declared {declared} bytes"
            ),
        );
    }
    if got < declared {
        return fail(
            C::SizeMismatch,
            format!("snapshot of library {library_id} is {got} bytes, manifest says {declared}"),
        );
    }
    Ok(buf)
}

pub(crate) struct Inspected {
    pub snapshot: SyncSnapshotV1,
    pub notebooks: u64,
    pub pages: u64,
    /// Referenced background blobs (sha256 → byte length).
    pub blobs: BTreeMap<Sha256Hex, u64>,
}

/// The per-snapshot checks shared by the verifier and the manifest builder. All protocol
/// rules come from `sync-protocol` (strict parse + validate + canonical bytes) and
/// `sync-engine` (genesis state); nothing here re-implements them.
pub(crate) fn inspect_snapshot(bytes: &[u8]) -> Result<Inspected> {
    #[derive(Deserialize)]
    struct Version {
        v: u32,
    }
    let ver: Version = from_json(bytes).map_err(|e| BackupError::new(C::InvalidSnapshot, e))?;
    if ver.v != SNAPSHOT_VERSION {
        return fail(
            C::UnsupportedVersion,
            format!("snapshot version {} (need {SNAPSHOT_VERSION})", ver.v),
        );
    }
    let snap = SyncSnapshotV1::parse(bytes).map_err(|e| BackupError::new(C::InvalidSnapshot, e))?;
    let canonical = snap
        .to_canonical_bytes()
        .map_err(|e| BackupError::new(C::InvalidSnapshot, e))?;
    if canonical != bytes {
        return fail(
            C::NonCanonical,
            format!("snapshot of library {} is not canonical", snap.library_id),
        );
    }
    drop(canonical);
    let id = snap.library_id;
    let snapshot = LibraryState::genesis(snap)
        .map_err(|e| BackupError::new(C::InvalidState, format!("library {id}: {e}")))?
        .snapshot;
    let mut blobs = BTreeMap::new();
    let mut pages = 0u64;
    for page in snapshot.notebooks.iter().flat_map(|n| &n.pages) {
        pages += 1;
        if let BackgroundRef::Blob {
            sha256, byte_len, ..
        } = &page.background
        {
            if let Some(prev) = blobs.insert(sha256.clone(), *byte_len) {
                if prev != *byte_len {
                    return fail(
                        C::BlobLengthMismatch,
                        format!("library {id} references blob {sha256} with lengths {prev} and {byte_len}"),
                    );
                }
            }
        }
    }
    Ok(Inspected {
        notebooks: snapshot.notebooks.len() as u64,
        pages,
        blobs,
        snapshot,
    })
}
