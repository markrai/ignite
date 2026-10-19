//! Export pipeline: list → snapshots → blobs (all GET, spooled to a private staging directory)
//! → deterministic ZIP → re-read and verify with `BackupVerifier` → publish without overwriting.

use std::cell::Cell;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use portable_backup::{
    BackupError, BackupErrorCode, BackupManifestV1, BackupVerifier, EntryPath, Limits,
    ManifestBlob, ManifestBuilder, SnapshotSource,
};
use serde::Deserialize;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use sync_protocol::{sha256_hex, LibraryId, Sha256Hex};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

use crate::source::{ReadOnlySource, SourceError};

const CHUNK: usize = 64 * 1024;
/// JSON wrapper around a snapshot in `GET /v1/libraries/{id}/snapshot` (ids, hash, keys).
const SNAPSHOT_ENVELOPE_SLACK: u64 = 4096;
/// `GET /v1/libraries`: fixed overhead plus a generous per-library row allowance.
const LIST_BASE_BYTES: u64 = 64 * 1024;
const LIST_ROW_BYTES: u64 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportErrorKind {
    /// Destination path refused (exists, not a `.zip`, parent missing, …).
    Destination,
    /// The server refused the credential (401/403) or none was supplied.
    Unauthorized,
    /// Any other unexpected HTTP status.
    Http(u16),
    /// Connection, timeout or interrupted transfer.
    Transport,
    /// A response that is not the documented JSON shape.
    Malformed,
    /// A portable-backup refusal (integrity, canonical form, limits, completeness).
    Backup(BackupErrorCode),
    /// Local filesystem or ZIP failure.
    Io,
    /// The export would write more into its staging directory than
    /// [`ExportOptions::max_staging_bytes`] allows.
    StagingLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportError {
    pub kind: ExportErrorKind,
    pub message: String,
}

impl ExportError {
    fn new(kind: ExportErrorKind, message: impl fmt::Display) -> Self {
        Self {
            kind,
            message: message.to_string(),
        }
    }
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ExportError {}

impl From<BackupError> for ExportError {
    fn from(e: BackupError) -> Self {
        Self::new(ExportErrorKind::Backup(e.code), e)
    }
}

impl From<SourceError> for ExportError {
    fn from(e: SourceError) -> Self {
        Self::new(ExportErrorKind::Transport, e)
    }
}

type R<T> = Result<T, ExportError>;

fn io_err(what: impl fmt::Display) -> impl FnOnce(io::Error) -> ExportError {
    move |e| ExportError::new(ExportErrorKind::Io, format!("{what}: {e}"))
}

/// What a successful export published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportReport {
    pub path: PathBuf,
    pub manifest: BackupManifestV1,
    pub archive_bytes: u64,
    pub archive_sha256: Sha256Hex,
}

/// Optional bounds of [`export_backup_with`]. The default bounds nothing beyond `Limits`
/// (what [`export_backup`] and the command-line tool use).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportOptions {
    /// Most bytes the export may write into its staging directory in total: staged snapshots
    /// and blobs plus the ZIP being built (publishing is a hard link and writes nothing). Each
    /// write is charged before it is made, so the staging never holds more than this; the
    /// export then fails with [`ExportErrorKind::StagingLimit`] and its staging is removed.
    pub max_staging_bytes: Option<u64>,
}

/// Export every library `source` lists into a new ZIP at `destination`.
///
/// Read-only towards the source (GET only). Each library is captured from its own snapshot
/// response (its `through_server_seq` is that response's, never the list's head); libraries
/// are captured one after another, so the archive is **not** a cross-library atomic
/// point-in-time copy. The archive appears at `destination` only after it was finalized and
/// re-verified; an existing file is never replaced. On any error nothing is published and
/// the staging directory is removed.
pub fn export_backup(
    source: &dyn ReadOnlySource,
    destination: &Path,
    limits: &Limits,
) -> R<ExportReport> {
    export_backup_with(source, destination, limits, &ExportOptions::default())
}

/// [`export_backup`] with optional bounds (the same archive bytes whenever it succeeds).
pub fn export_backup_with(
    source: &dyn ReadOnlySource,
    destination: &Path,
    limits: &Limits,
    options: &ExportOptions,
) -> R<ExportReport> {
    let (dir, file_name) = check_destination(destination)?;
    let staging = Staging::create(&dir, &file_name, options.max_staging_bytes)?;
    let manifest = fetch_all(source, &staging, limits)?;
    let manifest_bytes = manifest.to_canonical_bytes(limits)?;
    let archive = staging.path("archive.zip");
    // A refused ZIP write surfaces through the ZIP writer as an I/O error: report the limit.
    write_zip(&archive, &staging, &manifest, &manifest_bytes)
        .map_err(|e| staging.budget.reported(e))?;
    verify_zip(&archive, &manifest, &manifest_bytes, limits)?;
    let (archive_bytes, archive_sha256) = hash_file(&archive)?;
    publish(&archive, destination, &dir)?;
    drop(staging);
    Ok(ExportReport {
        path: destination.to_path_buf(),
        manifest,
        archive_bytes,
        archive_sha256,
    })
}

// ----------------------------------------------------------------------------- destination

/// Conservative destination rules: a `.zip` file name (not hidden), in an existing directory,
/// that does not exist yet (not even as a dangling symlink).
fn check_destination(destination: &Path) -> R<(PathBuf, String)> {
    let bad = |why: &str| {
        ExportError::new(
            ExportErrorKind::Destination,
            format!("destination {}: {why}", destination.display()),
        )
    };
    let name = destination
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| bad("needs a UTF-8 file name"))?;
    if name.starts_with('.') || !name.to_ascii_lowercase().ends_with(".zip") || name.len() <= 4 {
        return Err(bad(
            "file name must end in .zip and must not start with '.'",
        ));
    }
    let dir = match destination.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    match fs::metadata(&dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(bad("parent is not a directory")),
        Err(_) => return Err(bad("parent directory does not exist")),
    }
    match fs::symlink_metadata(destination) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok((dir, name.to_string())),
        Ok(_) => Err(bad("already exists; refusing to overwrite a backup")),
        Err(e) => Err(bad(&format!("cannot be inspected: {e}"))),
    }
}

/// Publish without replacing anything: a hard link fails if `destination` exists (no
/// check-then-rename race). The staged file is removed with the staging directory.
fn publish(archive: &Path, destination: &Path, dir: &Path) -> R<()> {
    match fs::hard_link(archive, destination) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            return Err(ExportError::new(
                ExportErrorKind::Destination,
                format!(
                    "destination {} appeared during the export; refusing to overwrite it",
                    destination.display()
                ),
            ))
        }
        Err(e) => {
            return Err(ExportError::new(
                ExportErrorKind::Destination,
                format!(
                    "cannot publish {} without overwriting (hard link failed: {e}); choose a directory on a filesystem with hard links",
                    destination.display()
                ),
            ))
        }
    }
    sync_dir(dir);
    Ok(())
}

#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

/// Bytes written into the staging directory so far, against an optional ceiling.
struct Budget {
    max: Option<u64>,
    used: Cell<u64>,
    exceeded: Cell<bool>,
}

impl Budget {
    fn new(max: Option<u64>) -> Self {
        Self {
            max,
            used: Cell::new(0),
            exceeded: Cell::new(false),
        }
    }

    /// Account for `n` more bytes before writing them; refused (nothing charged) past `max`.
    fn charge(&self, n: u64) -> io::Result<()> {
        let next = self.used.get().saturating_add(n);
        if self.max.is_some_and(|m| next > m) {
            self.exceeded.set(true);
            return Err(io::Error::other("staging budget exceeded"));
        }
        self.used.set(next);
        Ok(())
    }

    fn error(&self) -> ExportError {
        ExportError::new(
            ExportErrorKind::StagingLimit,
            format!(
                "the export needs more than its {}-byte staging budget",
                self.max.unwrap_or(u64::MAX)
            ),
        )
    }

    /// `e`, or the budget error if a refused write caused it.
    fn reported(&self, e: ExportError) -> ExportError {
        if self.exceeded.get() {
            self.error()
        } else {
            e
        }
    }
}

/// Private working directory beside the destination (same filesystem, so the final hard link
/// works). Removed on every exit path, success included.
struct Staging {
    dir: PathBuf,
    budget: Budget,
}

impl Staging {
    fn create(dir: &Path, file_name: &str, max_bytes: Option<u64>) -> R<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!(
            ".{file_name}.export-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir(&path).map_err(io_err(format!("create staging {}", path.display())))?;
        let s = Self {
            dir: path,
            budget: Budget::new(max_bytes),
        };
        for sub in ["libraries", "blobs"] {
            fs::create_dir(s.dir.join(sub)).map_err(io_err("create staging"))?;
        }
        Ok(s)
    }

    fn charge(&self, n: u64) -> R<()> {
        self.budget.charge(n).map_err(|_| self.budget.error())
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn snapshot(&self, id: &LibraryId) -> PathBuf {
        self.dir.join("libraries").join(format!("{id}.json"))
    }

    fn blob(&self, sha: &Sha256Hex) -> PathBuf {
        self.dir.join("blobs").join(sha.as_str())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

// ----------------------------------------------------------------------------- fetching

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LibraryList {
    v: u32,
    libraries: Vec<LibraryRow>,
}

/// Only `library_id` is used. The head and counts describe the moment of listing; each
/// library's content and sequence come from its own snapshot response instead.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct LibraryRow {
    library_id: LibraryId,
    head_server_seq: u64,
    snapshot_sha256: Sha256Hex,
    notebook_count: u64,
    page_count: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotResponse<'a> {
    library_id: LibraryId,
    through_server_seq: u64,
    snapshot_sha256: Sha256Hex,
    #[serde(borrow)]
    snapshot: &'a RawValue,
}

fn fetch_all(
    source: &dyn ReadOnlySource,
    staging: &Staging,
    limits: &Limits,
) -> R<BackupManifestV1> {
    let list_cap =
        LIST_BASE_BYTES.saturating_add(LIST_ROW_BYTES.saturating_mul(limits.max_libraries as u64));
    let list_bytes = get_bounded(source, "/v1/libraries", list_cap)?;
    let list: LibraryList = serde_json::from_slice(&list_bytes).map_err(|e| {
        ExportError::new(
            ExportErrorKind::Malformed,
            format!("GET /v1/libraries: {e}"),
        )
    })?;
    if list.v != 1 {
        return Err(ExportError::new(
            ExportErrorKind::Malformed,
            format!("GET /v1/libraries: unsupported list version {}", list.v),
        ));
    }
    let mut ids: Vec<LibraryId> = list.libraries.iter().map(|l| l.library_id).collect();
    ids.sort();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err(ExportError::new(
            ExportErrorKind::Malformed,
            "GET /v1/libraries lists a library twice",
        ));
    }
    if ids.is_empty() {
        return Err(ExportError::new(
            ExportErrorKind::Backup(BackupErrorCode::InvalidManifest),
            "the server has no libraries to export",
        ));
    }
    if ids.len() > limits.max_libraries {
        return Err(ExportError::new(
            ExportErrorKind::Backup(BackupErrorCode::LimitExceeded),
            format!("{} libraries (limit {})", ids.len(), limits.max_libraries),
        ));
    }

    let mut builder = ManifestBuilder::new();
    for id in &ids {
        let path = format!("/v1/libraries/{id}/snapshot");
        let body = get_bounded(
            source,
            &path,
            limits
                .max_snapshot_bytes
                .saturating_add(SNAPSHOT_ENVELOPE_SLACK),
        )?;
        let resp: SnapshotResponse<'_> = serde_json::from_slice(&body).map_err(|e| {
            ExportError::new(ExportErrorKind::Malformed, format!("GET {path}: {e}"))
        })?;
        if resp.library_id != *id {
            return Err(ExportError::new(
                ExportErrorKind::Malformed,
                format!("GET {path} answered for library {}", resp.library_id),
            ));
        }
        let bytes = resp.snapshot.get().as_bytes();
        let actual = sha256_hex(bytes);
        if actual != resp.snapshot_sha256 {
            return Err(ExportError::new(
                ExportErrorKind::Backup(BackupErrorCode::HashMismatch),
                format!(
                    "library {id}: snapshot bytes hash to {actual}, the server reported {}",
                    resp.snapshot_sha256
                ),
            ));
        }
        let entry = builder.add_snapshot(
            SnapshotSource {
                through_server_seq: resp.through_server_seq,
                canonical_bytes: bytes,
            },
            limits,
        )?;
        if entry.source_library_id != *id {
            return Err(ExportError::new(
                ExportErrorKind::Backup(BackupErrorCode::LibraryMismatch),
                format!(
                    "snapshot of library {id} is library {}",
                    entry.source_library_id
                ),
            ));
        }
        staging.charge(bytes.len() as u64)?;
        write_new(&staging.snapshot(id), bytes)?;
    }
    let manifest = builder.finish(limits)?;
    // The blobs' declared sizes are known now: refuse before downloading any of them.
    let declared = manifest
        .blobs
        .iter()
        .fold(0u64, |a, b| a.saturating_add(b.byte_len));
    let budget = &staging.budget;
    if budget
        .max
        .is_some_and(|m| budget.used.get().saturating_add(declared) > m)
    {
        budget.exceeded.set(true);
        return Err(budget.error());
    }
    for blob in &manifest.blobs {
        fetch_blob(source, staging, blob)?;
    }
    Ok(manifest)
}

/// GET into memory, refusing bodies larger than `cap` (reads at most `cap + 1` bytes).
fn get_bounded(source: &dyn ReadOnlySource, path: &str, cap: u64) -> R<Vec<u8>> {
    let resp = source.get(path)?;
    check_status(resp.status, path)?;
    let mut buf = Vec::new();
    resp.body
        .take(cap.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| {
            ExportError::new(
                ExportErrorKind::Transport,
                format!("GET {path}: transfer interrupted: {e}"),
            )
        })?;
    if buf.len() as u64 > cap {
        return Err(ExportError::new(
            ExportErrorKind::Backup(BackupErrorCode::LimitExceeded),
            format!("GET {path}: response exceeds {cap} bytes"),
        ));
    }
    Ok(buf)
}

fn check_status(status: u16, path: &str) -> R<()> {
    match status {
        200 => Ok(()),
        401 | 403 => Err(ExportError::new(
            ExportErrorKind::Unauthorized,
            format!("GET {path}: the server refused access ({status}); supply a valid credential for this server"),
        )),
        s => Err(ExportError::new(ExportErrorKind::Http(s), format!("GET {path}: HTTP {s}"))),
    }
}

/// Stream one blob to staging: bounded to its declared length, hashed while writing.
fn fetch_blob(source: &dyn ReadOnlySource, staging: &Staging, blob: &ManifestBlob) -> R<()> {
    let path = format!("/v1/blobs/{}", blob.sha256);
    let resp = source.get(&path)?;
    if resp.status == 404 {
        return Err(ExportError::new(
            ExportErrorKind::Backup(BackupErrorCode::MissingBlob),
            format!(
                "blob {} is referenced but the server does not have it",
                blob.sha256
            ),
        ));
    }
    check_status(resp.status, &path)?;
    let target = staging.blob(&blob.sha256);
    let mut out = BufWriter::new(create_new(&target)?);
    let mut body = resp.body.take(blob.byte_len.saturating_add(1));
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let n = match body.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(ExportError::new(
                    ExportErrorKind::Transport,
                    format!("GET {path}: transfer interrupted: {e}"),
                ))
            }
        };
        total += n as u64;
        if total > blob.byte_len {
            return Err(ExportError::new(
                ExportErrorKind::Backup(BackupErrorCode::SizeMismatch),
                format!(
                    "blob {} is longer than its referenced {} bytes",
                    blob.sha256, blob.byte_len
                ),
            ));
        }
        hasher.update(&chunk[..n]);
        staging.charge(n as u64)?;
        out.write_all(&chunk[..n])
            .map_err(io_err("write staged blob"))?;
    }
    if total != blob.byte_len {
        return Err(ExportError::new(
            ExportErrorKind::Backup(BackupErrorCode::SizeMismatch),
            format!(
                "blob {} is {total} bytes, referenced as {}",
                blob.sha256, blob.byte_len
            ),
        ));
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let got = Sha256Hex::from_digest(&digest);
    if got != blob.sha256 {
        return Err(ExportError::new(
            ExportErrorKind::Backup(BackupErrorCode::HashMismatch),
            format!("blob {} content hashes to {got}", blob.sha256),
        ));
    }
    out.flush().map_err(io_err("write staged blob"))?;
    Ok(())
}

fn create_new(path: &Path) -> R<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_err(format!("create {}", path.display())))
}

fn write_new(path: &Path, bytes: &[u8]) -> R<()> {
    let mut f = create_new(path)?;
    f.write_all(bytes)
        .map_err(io_err(format!("write {}", path.display())))
}

// ----------------------------------------------------------------------------- ZIP writer

/// Writer side only: deterministic entry order (manifest, snapshots by library id, blobs by
/// sha256), Deflate, a fixed 1980-01-01 timestamp and 0644 permissions, so equal backups give
/// byte-identical archives. Reader-side defenses against hostile archives (symlinks,
/// compression ratios, duplicate names, encryption) belong to the future importer.
fn write_zip(
    archive: &Path,
    staging: &Staging,
    manifest: &BackupManifestV1,
    manifest_bytes: &[u8],
) -> R<()> {
    let file = Charged {
        file: create_new(archive)?,
        budget: &staging.budget,
        pos: 0,
        charged: 0,
    };
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::default())
        .unix_permissions(0o644);
    let zerr =
        |e: zip::result::ZipError| ExportError::new(ExportErrorKind::Io, format!("write ZIP: {e}"));
    zip.start_file(EntryPath::Manifest.to_path(), opts)
        .map_err(zerr)?;
    zip.write_all(manifest_bytes).map_err(io_err("write ZIP"))?;
    for l in &manifest.libraries {
        zip.start_file(EntryPath::Snapshot(l.source_library_id).to_path(), opts)
            .map_err(zerr)?;
        let mut f = File::open(staging.snapshot(&l.source_library_id))
            .map_err(io_err("read staged snapshot"))?;
        io::copy(&mut f, &mut zip).map_err(io_err("write ZIP"))?;
    }
    for b in &manifest.blobs {
        zip.start_file(EntryPath::Blob(b.sha256.clone()).to_path(), opts)
            .map_err(zerr)?;
        let mut f = File::open(staging.blob(&b.sha256)).map_err(io_err("read staged blob"))?;
        io::copy(&mut f, &mut zip).map_err(io_err("write ZIP"))?;
    }
    let buf = zip.finish().map_err(zerr)?;
    let file = buf
        .into_inner()
        .map_err(|e| ExportError::new(ExportErrorKind::Io, format!("flush ZIP: {}", e.error())))?;
    file.file.sync_all().map_err(io_err("sync ZIP"))
}

/// The ZIP file, charging the staging budget for every byte that extends it (the ZIP writer
/// seeks back to patch headers; rewriting bytes already charged is free).
struct Charged<'a> {
    file: File,
    budget: &'a Budget,
    pos: u64,
    /// Highest end offset already charged.
    charged: u64,
}

impl Write for Charged<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let end = self.pos.saturating_add(buf.len() as u64);
        if end > self.charged {
            self.budget.charge(end - self.charged)?;
            self.charged = end;
        }
        let n = self.file.write(buf)?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for Charged<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.pos = self.file.seek(to)?;
        Ok(self.pos)
    }
}

/// Re-read the finalized archive: exactly the expected entries in the expected order, then
/// every entry through `BackupVerifier` (streamed; CRCs are checked by the ZIP reader).
fn verify_zip(
    archive: &Path,
    manifest: &BackupManifestV1,
    manifest_bytes: &[u8],
    limits: &Limits,
) -> R<()> {
    let zerr = |e: zip::result::ZipError| {
        ExportError::new(ExportErrorKind::Io, format!("re-read ZIP: {e}"))
    };
    let mut zip =
        ZipArchive::new(File::open(archive).map_err(io_err("re-read ZIP"))?).map_err(zerr)?;
    let mut expected = vec![EntryPath::Manifest];
    expected.extend(
        manifest
            .libraries
            .iter()
            .map(|l| EntryPath::Snapshot(l.source_library_id)),
    );
    expected.extend(
        manifest
            .blobs
            .iter()
            .map(|b| EntryPath::Blob(b.sha256.clone())),
    );
    if zip.len() != expected.len() {
        return Err(ExportError::new(
            ExportErrorKind::Io,
            format!("ZIP has {} entries, expected {}", zip.len(), expected.len()),
        ));
    }
    for (i, want) in expected.iter().enumerate() {
        let e = zip.by_index_raw(i).map_err(zerr)?;
        if !e.is_file() || EntryPath::parse(e.name())? != *want {
            return Err(ExportError::new(
                ExportErrorKind::Io,
                format!("ZIP entry {i} is {:?}, expected {want}", e.name()),
            ));
        }
    }
    let mut read_manifest = Vec::new();
    zip.by_index(0)
        .map_err(zerr)?
        .take(limits.max_manifest_bytes.saturating_add(1))
        .read_to_end(&mut read_manifest)
        .map_err(io_err("re-read manifest"))?;
    if read_manifest != manifest_bytes {
        return Err(ExportError::new(
            ExportErrorKind::Io,
            "ZIP manifest differs from the generated manifest",
        ));
    }
    let mut v = BackupVerifier::new(&read_manifest, limits)?;
    for (i, want) in expected.iter().enumerate().skip(1) {
        let entry = zip.by_index(i).map_err(zerr)?;
        match want {
            EntryPath::Snapshot(id) => {
                v.add_snapshot(*id, entry)?;
            }
            EntryPath::Blob(sha) => v.verify_blob(sha, entry)?,
            EntryPath::Manifest => unreachable!("manifest is entry 0"),
        }
    }
    if v.finish()?.manifest() != manifest {
        return Err(ExportError::new(
            ExportErrorKind::Io,
            "verified manifest differs",
        ));
    }
    Ok(())
}

fn hash_file(path: &Path) -> R<(u64, Sha256Hex)> {
    let mut f = File::open(path).map_err(io_err("hash ZIP"))?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut chunk).map_err(io_err("hash ZIP"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hasher.update(&chunk[..n]);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    Ok((total, Sha256Hex::from_digest(&digest)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_zip_file_is_charged_for_growth_only_and_never_written_past_the_budget() {
        let dir =
            std::env::temp_dir().join(format!("backup-export-charged-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.bin");
        let budget = Budget::new(Some(100));
        let mut f = Charged {
            file: File::create(&path).unwrap(),
            budget: &budget,
            pos: 0,
            charged: 0,
        };
        f.write_all(&[1; 60]).unwrap();
        // Patching earlier bytes (as the ZIP writer does for local headers) costs nothing.
        f.seek(SeekFrom::Start(10)).unwrap();
        f.write_all(&[2; 50]).unwrap();
        assert_eq!(budget.used.get(), 60);
        f.seek(SeekFrom::End(0)).unwrap();
        f.write_all(&[3; 40]).unwrap();
        assert_eq!(budget.used.get(), 100);
        // One byte more is refused before it reaches the file.
        assert!(f.write_all(&[4]).is_err());
        assert!(budget.exceeded.get());
        drop(f);
        assert_eq!(fs::metadata(&path).unwrap().len(), 100);
        let e = budget.reported(ExportError::new(ExportErrorKind::Io, "write ZIP: refused"));
        assert_eq!(e.kind, ExportErrorKind::StagingLimit);
        let _ = fs::remove_dir_all(&dir);
    }
}
