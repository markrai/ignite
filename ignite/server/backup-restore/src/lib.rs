//! Native transactional restore of a portable backup into a `scribe-sync-server` [`Store`]
//! (Backup & Restore, Slice 1E). Not exposed over HTTP.
//!
//! **Guarantee:** either every library in the archive becomes visible with its complete
//! verified state, or none of them does. Existing libraries are never modified, replaced or
//! deleted; existing blob files are never replaced or truncated.
//!
//! ```text
//! 0. mapping checks        every destination id unique, ≠ its source, absent from the store
//! 1. acquire               copy the archive (bounded) into a private staging directory inside
//!                          the store's data directory; only the copy is read from now on
//! 2. verify                one pass of backup-archive's strict ZIP reader + BackupVerifier over
//!                          the copy; each verified snapshot is re-keyed and staged; each blob
//!                          is teed to a staged file while being verified (bounded memory)
//!                          → the store is untouched until this has fully succeeded
//! 3. install blobs         Store::install_blob_file: re-hash the staged file, fsync, verify an
//!                          existing file or hard-link the staged one into place (never
//!                          overwrite), then sync the directory chain (Unix; an error stops
//!                          the restore here); no index rows yet
//! 4. publish               Store::bootstrap_many: ONE SQLite transaction that indexes the blobs
//!                          and inserts every library (genesis = current state, seq 0), then
//!                          COMMIT
//! 5. cleanup               remove the staging directory
//! ```
//!
//! SQLite atomicity covers step 4 only. Blob files (step 3) are outside it: a failure or
//! crash between 3 and a successful 4 can leave content-addressed blob files, hash-checked when
//! they were linked, that no index row and no library references (`Store::unindexed_blob_files`
//! lists their names without re-reading them); they never damage existing data and a retry
//! re-hashes and reuses them. On Windows no directory sync is available, so step 3 does not
//! confirm that a new directory entry is durable before step 4 (see `Store::install_blob_file`). A crash can also leave a staging directory;
//! [`remove_restore_staging`] removes those when no restore is running (e.g. at startup).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use backup_archive::{verify_reader_into, RestoreErrorKind as ArchiveKind, VerifiedEntrySink};
use portable_backup::{rekey_library, Limits, VerifiedSnapshot};
use scribe_sync_server::{BlobInstall, Store, StoreError};
use sha2::{Digest, Sha256};
use sync_protocol::{sha256_hex, LibraryId, Sha256Hex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportErrorKind {
    /// The destination mapping is incomplete, has extra entries, duplicates, or reuses a
    /// source identity.
    Mapping,
    /// A destination id already exists, or an existing blob file conflicts.
    Conflict,
    /// The archive was refused (ZIP structure, semantic verification) or could not be staged.
    Archive(ArchiveKind),
    /// The archive exceeds the acquisition bound.
    TooLarge,
    /// The store refused publication (code from `StoreError`).
    Store(String),
    /// Local filesystem failure while acquiring or staging.
    Io,
    /// A test fail point simulated a process crash; staging was intentionally left behind.
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportError {
    pub kind: ImportErrorKind,
    pub message: String,
}

impl ImportError {
    fn new(kind: ImportErrorKind, message: impl fmt::Display) -> Self {
        Self {
            kind,
            message: message.to_string(),
        }
    }
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ImportError {}

impl From<StoreError> for ImportError {
    fn from(e: StoreError) -> Self {
        let message = e.to_string();
        match &e {
            StoreError::Conflict { .. } => Self::new(ImportErrorKind::Conflict, message),
            StoreError::BadRequest { code, .. } | StoreError::NotFound { code, .. } => {
                Self::new(ImportErrorKind::Store((*code).into()), message)
            }
            StoreError::MissingBlobs(_) => {
                Self::new(ImportErrorKind::Store("missing_blobs".into()), message)
            }
            StoreError::Semantic { .. } => {
                Self::new(ImportErrorKind::Store("semantic".into()), message)
            }
            StoreError::Internal(_) => {
                Self::new(ImportErrorKind::Store("internal".into()), message)
            }
        }
    }
}

type R<T> = Result<T, ImportError>;

fn io_err(what: impl fmt::Display) -> impl FnOnce(std::io::Error) -> ImportError {
    move |e| ImportError::new(ImportErrorKind::Io, format!("{what}: {e}"))
}

/// Simulated crash points (tests only): the restore stops and leaves its staging directory,
/// exactly as a killed process would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailPoint {
    /// After complete verification and staging, before any blob is installed.
    AfterVerification,
    /// After every blob file is installed, before the library transaction.
    AfterBlobInstall,
}

/// Tests only: a function run on the staging directory at a phase boundary.
#[doc(hidden)]
pub type StagingHook = (FailPoint, fn(&Path));

#[derive(Clone, Debug)]
pub struct RestoreOptions {
    pub limits: Limits,
    /// Largest archive accepted for acquisition; `None` derives a bound from `limits`.
    pub max_archive_bytes: Option<u64>,
    #[doc(hidden)]
    pub fail_point: Option<FailPoint>,
    /// Tests only: run `f(staging_dir)` at `point` and continue (e.g. to tamper with staged
    /// files and prove they are re-checked before use).
    #[doc(hidden)]
    pub staging_hook: Option<StagingHook>,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self {
            limits: Limits::DEFAULT,
            max_archive_bytes: None,
            fail_point: None,
            staging_hook: None,
        }
    }
}

/// The largest archive that could hold content within `limits`: every entry's headers plus
/// all declared content and deflate's worst-case expansion (stored blocks, < 0.4 %).
pub fn default_max_archive_bytes(limits: &Limits) -> u64 {
    let entries = 1u64
        .saturating_add(limits.max_libraries as u64)
        .saturating_add(limits.max_blobs as u64);
    let content = limits
        .max_manifest_bytes
        .saturating_add(limits.max_total_snapshot_bytes)
        .saturating_add(limits.max_total_blob_bytes);
    content
        .saturating_add(content / 256)
        .saturating_add(entries.saturating_mul(512))
        .saturating_add(4096)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredLibrary {
    pub source_library_id: LibraryId,
    pub library_id: LibraryId,
    pub source_snapshot_sha256: Sha256Hex,
    /// Genesis = current snapshot hash of the restored library.
    pub restored_snapshot_sha256: Sha256Hex,
    pub notebook_count: u64,
    pub page_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreReport {
    /// Hash of the staged copy that was verified and restored.
    pub archive_sha256: Sha256Hex,
    pub archive_bytes: u64,
    /// Ascending by source library id.
    pub libraries: Vec<RestoredLibrary>,
    pub blobs_installed: u64,
    pub blobs_already_present: u64,
}

/// Restore every library of the archive at `archive` into `store`, as new libraries with the
/// caller-chosen identities in `mapping` (source id → destination id). See the crate docs for
/// the ordering and guarantees.
pub fn restore_backup(
    store: &Store,
    archive: &Path,
    mapping: &BTreeMap<LibraryId, LibraryId>,
    options: &RestoreOptions,
) -> R<RestoreReport> {
    check_mapping_shape(store, mapping)?;
    let mut staging = Staging::create(store)?;

    // 1. Acquire an immutable private copy.
    let max = options
        .max_archive_bytes
        .unwrap_or_else(|| default_max_archive_bytes(&options.limits));
    let (archive_bytes, archive_sha256) = acquire(archive, &staging.path("archive.zip"), max)?;

    // 2. Verify the copy completely, staging re-keyed snapshots and verified blob bytes.
    let mut sink = StagingSink {
        dir: staging.dir.clone(),
        mapping,
        libraries: Vec::new(),
        blobs: BTreeMap::new(),
        pending_blob: None,
        next: 0,
        mapping_error: None,
    };
    let copy = File::open(staging.path("archive.zip")).map_err(io_err("open staged archive"))?;
    let verified = verify_reader_into(&mut BufReader::new(copy), &options.limits, &mut sink);
    if let Some(m) = sink.mapping_error.take() {
        return Err(ImportError::new(ImportErrorKind::Mapping, m));
    }
    verified.map_err(|e| ImportError::new(ImportErrorKind::Archive(e.kind), e.message))?;
    let sources: BTreeSet<LibraryId> = sink
        .libraries
        .iter()
        .map(|l| l.0.source_library_id)
        .collect();
    if let Some(extra) = mapping.keys().find(|k| !sources.contains(k)) {
        return Err(ImportError::new(
            ImportErrorKind::Mapping,
            format!("mapping names library {extra}, which the archive does not contain"),
        ));
    }
    if let Some(reused) = mapping.values().find(|v| sources.contains(v)) {
        return Err(ImportError::new(
            ImportErrorKind::Mapping,
            format!("destination {reused} is a source library identity in this archive"),
        ));
    }
    let mut libraries = std::mem::take(&mut sink.libraries);
    libraries.sort_by_key(|(l, _)| l.source_library_id);
    let blobs = std::mem::take(&mut sink.blobs);
    if let Some((FailPoint::AfterVerification, f)) = options.staging_hook {
        f(&staging.dir);
    }
    if options.fail_point == Some(FailPoint::AfterVerification) {
        staging.keep = true;
        return Err(ImportError::new(
            ImportErrorKind::Interrupted,
            "simulated crash after verification",
        ));
    }

    // 3. Install blob files (re-hashed, synced where the platform allows, never overwriting)
    //    — outside SQLite. Any error, including a failed directory sync, stops before step 4.
    let (mut installed, mut present) = (0, 0);
    for (sha, (len, path)) in &blobs {
        match store.install_blob_file(sha, *len, path)? {
            BlobInstall::Installed => installed += 1,
            BlobInstall::Existing => present += 1,
        }
    }
    if let Some((FailPoint::AfterBlobInstall, f)) = options.staging_hook {
        f(&staging.dir);
    }
    if options.fail_point == Some(FailPoint::AfterBlobInstall) {
        staging.keep = true;
        return Err(ImportError::new(
            ImportErrorKind::Interrupted,
            "simulated crash after blob install",
        ));
    }

    // 4. Publish every library in one transaction.
    let index: Vec<(Sha256Hex, u64)> = blobs.iter().map(|(s, (l, _))| (s.clone(), *l)).collect();
    let infos = store.bootstrap_many(&index, libraries.len(), |i| {
        let (lib, path) = &libraries[i];
        let bytes =
            fs::read(path).map_err(|e| format!("read staged library {}: {e}", lib.library_id))?;
        if sha256_hex(&bytes) != lib.restored_snapshot_sha256 {
            return Err(format!(
                "staged library {} changed after verification",
                lib.library_id
            ));
        }
        Ok(bytes)
    })?;
    for ((lib, _), info) in libraries.iter().zip(&infos) {
        debug_assert_eq!(info.library_id, lib.library_id);
        debug_assert_eq!(
            info.genesis_sha256,
            lib.restored_snapshot_sha256.to_string()
        );
    }

    // 5. Cleanup (Staging's Drop).
    drop(staging);
    Ok(RestoreReport {
        archive_sha256,
        archive_bytes,
        libraries: libraries.into_iter().map(|(l, _)| l).collect(),
        blobs_installed: installed,
        blobs_already_present: present,
    })
}

/// Remove every restore staging directory. Call only when no restore can be running (e.g.
/// at server startup, before serving). Returns how many were removed.
pub fn remove_restore_staging(store: &Store) -> R<usize> {
    let root = store.restore_staging_root()?;
    let mut n = 0;
    for e in fs::read_dir(&root)
        .map_err(io_err("list restore staging"))?
        .flatten()
    {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("job-") {
            fs::remove_dir_all(e.path()).map_err(io_err(format!("remove {name}")))?;
            n += 1;
        }
    }
    Ok(n)
}

fn check_mapping_shape(store: &Store, mapping: &BTreeMap<LibraryId, LibraryId>) -> R<()> {
    if mapping.is_empty() {
        return Err(ImportError::new(
            ImportErrorKind::Mapping,
            "the library mapping is empty",
        ));
    }
    let mut dest = BTreeSet::new();
    for (source, target) in mapping {
        if source == target {
            return Err(ImportError::new(
                ImportErrorKind::Mapping,
                format!("library {source} cannot be restored under its original identity"),
            ));
        }
        if !dest.insert(*target) {
            return Err(ImportError::new(
                ImportErrorKind::Mapping,
                format!("destination {target} is used more than once"),
            ));
        }
        match store.library(&target.to_string()) {
            Err(StoreError::NotFound { .. }) => {}
            Ok(_) => {
                return Err(ImportError::new(
                    ImportErrorKind::Conflict,
                    format!("destination library {target} already exists; a restore never merges or replaces"),
                ))
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Bounded copy into `dest` (created new), hashing on the way. The source must be a regular
/// file; it is read exactly once.
fn acquire(source: &Path, dest: &Path, max: u64) -> R<(u64, Sha256Hex)> {
    let meta = fs::metadata(source).map_err(io_err(format!("archive {}", source.display())))?;
    if !meta.is_file() {
        return Err(ImportError::new(
            ImportErrorKind::Io,
            format!("{} is not a regular file", source.display()),
        ));
    }
    if meta.len() > max {
        return Err(ImportError::new(
            ImportErrorKind::TooLarge,
            format!("archive is {} bytes (limit {max})", meta.len()),
        ));
    }
    let mut input = File::open(source)
        .map_err(io_err("open archive"))?
        .take(max.saturating_add(1));
    let mut out = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)
            .map_err(io_err("create staged archive"))?,
    );
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = input.read(&mut buf).map_err(io_err("read archive"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > max {
            return Err(ImportError::new(
                ImportErrorKind::TooLarge,
                format!("archive grew past {max} bytes while copying"),
            ));
        }
        h.update(&buf[..n]);
        out.write_all(&buf[..n])
            .map_err(io_err("write staged archive"))?;
    }
    out.flush().map_err(io_err("write staged archive"))?;
    let d: [u8; 32] = h.finalize().into();
    Ok((total, Sha256Hex::from_digest(&d)))
}

struct Staging {
    dir: PathBuf,
    keep: bool,
}

impl Staging {
    fn create(store: &Store) -> R<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = store.restore_staging_root()?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = root.join(format!(
            "job-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).map_err(io_err(format!("create {}", dir.display())))?;
        let s = Self { dir, keep: false };
        for sub in ["libraries", "blobs"] {
            fs::create_dir(s.dir.join(sub)).map_err(io_err("create staging"))?;
        }
        Ok(s)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// Stages verified entries under names derived from verified values only (a counter for
/// in-flight blobs, the verified sha256 afterwards, an index for libraries).
struct StagingSink<'a> {
    dir: PathBuf,
    mapping: &'a BTreeMap<LibraryId, LibraryId>,
    libraries: Vec<(RestoredLibrary, PathBuf)>,
    /// Verified blobs: sha → (length, staged file).
    blobs: BTreeMap<Sha256Hex, (u64, PathBuf)>,
    pending_blob: Option<(Sha256Hex, u64, PathBuf)>,
    next: u64,
    mapping_error: Option<String>,
}

impl VerifiedEntrySink for StagingSink<'_> {
    fn snapshot(&mut self, s: VerifiedSnapshot) -> Result<(), String> {
        let source = s.source_library_id();
        let Some(target) = self.mapping.get(&source) else {
            let m = format!("archive library {source} has no destination in the mapping");
            self.mapping_error = Some(m.clone());
            return Err(m);
        };
        let restored = rekey_library(&s, *target).map_err(|e| e.to_string())?;
        let path = self
            .dir
            .join("libraries")
            .join(format!("{}.json", self.libraries.len()));
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("stage library {source}: {e}"))?;
        f.write_all(&restored.canonical_bytes)
            .map_err(|e| format!("stage library {source}: {e}"))?;
        let snap = s.snapshot();
        self.libraries.push((
            RestoredLibrary {
                source_library_id: source,
                library_id: restored.library_id,
                source_snapshot_sha256: restored.source_snapshot_sha256,
                restored_snapshot_sha256: restored.restored_snapshot_sha256,
                notebook_count: snap.notebooks.len() as u64,
                page_count: snap.notebooks.iter().map(|n| n.pages.len() as u64).sum(),
            },
            path,
        ));
        Ok(())
    }

    fn begin_blob(&mut self, sha256: &Sha256Hex, byte_len: u64) -> Result<Box<dyn Write>, String> {
        self.next += 1;
        let path = self
            .dir
            .join("blobs")
            .join(format!("incoming-{}", self.next));
        let f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("stage blob {sha256}: {e}"))?;
        self.pending_blob = Some((sha256.clone(), byte_len, path));
        Ok(Box::new(BufWriter::new(f)))
    }

    fn blob_verified(&mut self, sha256: &Sha256Hex, staged: Box<dyn Write>) -> Result<(), String> {
        drop(staged);
        let Some((sha, len, path)) = self.pending_blob.take().filter(|(s, _, _)| s == sha256)
        else {
            return Err(format!("blob {sha256} was not being staged"));
        };
        let verified = self.dir.join("blobs").join(sha.as_str());
        fs::rename(&path, &verified).map_err(|e| format!("stage blob {sha}: {e}"))?;
        self.blobs.insert(sha, (len, verified));
        Ok(())
    }
}
