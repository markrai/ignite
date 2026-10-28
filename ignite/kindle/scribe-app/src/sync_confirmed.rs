//! ServerBaseState persistence: a **checkpoint** through server_seq N plus an append-only
//! **confirmed log** of the server operations after N.
//!
//! ```text
//! checkpoint (through N)  +  confirmed log records N+1 ..= cursor  ==  ServerBaseState
//! ```
//!
//! An inbound transaction appends its applied server items to the log (one durability barrier
//! per batch) — work proportional to the operations, never to the library. A background thread
//! occasionally writes a new checkpoint and then retires the log records it covers.
//!
//! # Confirmed log (`confirmed.log`)
//!
//! Binary, independently framed records, replayed sequentially with a small buffer:
//!
//! ```text
//! magic "SCL1" | payload_len u32 LE | server_seq u64 LE | crc32 u32 LE | payload (exact
//! canonical envelope bytes)          crc32 (IEEE) over server_seq LE bytes ++ payload
//! ```
//!
//! * An incomplete final record (short header, short payload, or a bad checksum on a record
//!   that ends exactly at end-of-file) is a torn append: ignored and physically truncated.
//! * Anything invalid *before* the last record is corruption of durable history: fail closed.
//! * `server_seq` must increase by exactly 1 from record to record.
//!
//! # Checkpoint (`checkpoint/`)
//!
//! Page-chunked so a checkpoint only writes what changed, and capturing one never copies the
//! whole library:
//!
//! ```text
//! checkpoint/manifest.json      {v:3, through_server_seq, library_id, page_space,
//!                                notebooks:[{notebook_id, title, pages:[{page_id, sha256}], folder_id?}], folders?,
//!                                history}
//! checkpoint/pages/<sha256>     canonical JSON of one SnapshotPage (content-addressed)
//! ```
//!
//! Promotion: page files (temp → fsync → rename) → directory fsync → manifest temp → fsync →
//! rename (**the** promotion) → directory fsync → only then retire log records ≤ N → collect
//! unreferenced page files. A crash before the manifest rename leaves the previous checkpoint
//! and the whole log; after it, log records ≤ N are skipped on replay.
//!
//! The legacy single-file `base-state.json` (v2) is read if no manifest exists and converted to
//! a checkpoint at startup (before any sync thread runs); it is removed only after that
//! checkpoint is promoted.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sync_engine::{HistoryV1, LibraryState};
use sync_protocol::{
    sha256_hex, FolderId, LibraryId, NotebookId, PageId, PageSpace, Sha256Hex, SnapshotFolder,
    SnapshotNotebook, SnapshotPage, SyncEnvelopeV1, SyncSnapshotV1, SNAPSHOT_VERSION,
};

pub const CONFIRMED_LOG_FILE: &str = "confirmed.log";
pub const CHECKPOINT_DIR: &str = "checkpoint";
pub const CHECKPOINT_MANIFEST: &str = "manifest.json";
pub const CHECKPOINT_PAGES: &str = "pages";
const MAGIC: &[u8; 4] = b"SCL1";
const HEADER_LEN: usize = 4 + 4 + 8 + 4;
/// Largest payload a record may claim (a canonical envelope is far smaller); a larger length is
/// corruption, never an allocation.
const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;

// ------------------------------------------------------------------------------ crc32

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC: [u32; 256] = crc_table();

fn crc32(parts: &[&[u8]]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for p in parts {
        for &b in *p {
            c = CRC[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
        }
    }
    c ^ 0xFFFF_FFFF
}

// ------------------------------------------------------------------------------ records

/// One framed record.
pub(crate) fn frame(server_seq: u64, payload: &[u8]) -> Vec<u8> {
    let seq = server_seq.to_le_bytes();
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&seq);
    out.extend_from_slice(&crc32(&[&seq, payload]).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Result of scanning a log file.
#[derive(Debug, Default)]
pub(crate) struct LogScan {
    /// Byte length of the valid prefix (a torn tail starts here).
    pub valid_len: u64,
    pub torn_tail: bool,
    /// (server_seq, end offset) of every valid record, in file order.
    pub index: Vec<(u64, u64)>,
}

fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Stream every record of `path` to `each(server_seq, payload)` (one record in memory at a
/// time). Validates framing, checksums and `server_seq` contiguity; a torn final record is
/// reported, not an error; corruption before it is an error. A missing file is empty.
pub(crate) fn scan_log(
    path: &Path,
    mut each: impl FnMut(u64, &[u8]) -> Result<(), String>,
) -> Result<LogScan, String> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LogScan::default()),
        Err(e) => return Err(format!("open {}: {e}", path.display())),
    };
    let file_len = f
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    scan_log_reader(
        BufReader::with_capacity(1 << 20, f),
        file_len,
        &path.display().to_string(),
        &mut each,
    )
}

/// Read-only scan core used by storage diagnostics after they have opened a regular file with
/// their no-follow and file-identity checks. It performs no seek, truncate, create, or repair.
pub(crate) fn scan_log_reader(
    mut r: impl Read,
    file_len: u64,
    description: &str,
    mut each: impl FnMut(u64, &[u8]) -> Result<(), String>,
) -> Result<LogScan, String> {
    let mut out = LogScan::default();
    let mut pos = 0u64;
    let mut prev: Option<u64> = None;
    let mut payload = Vec::new();
    let corrupt = |at: u64, why: &str| {
        format!(
            "CONFIRMED_LOG_CORRUPT at byte {at} of {}: {why} (durable history is never skipped)",
            description
        )
    };
    loop {
        let mut h = [0u8; HEADER_LEN];
        let got = read_exact_or_eof(&mut r, &mut h).map_err(|e| e.to_string())?;
        if got == 0 {
            break;
        }
        if got < HEADER_LEN {
            out.torn_tail = true; // short header at end of file
            break;
        }
        if &h[0..4] != MAGIC {
            return Err(corrupt(pos, "bad record magic"));
        }
        let len = u32::from_le_bytes(h[4..8].try_into().unwrap());
        let seq = u64::from_le_bytes(h[8..16].try_into().unwrap());
        let crc = u32::from_le_bytes(h[16..20].try_into().unwrap());
        let end = pos + HEADER_LEN as u64 + u64::from(len);
        if len > MAX_PAYLOAD {
            if end >= file_len {
                out.torn_tail = true;
                break;
            }
            return Err(corrupt(pos, "impossible record length"));
        }
        payload.resize(len as usize, 0);
        let got = read_exact_or_eof(&mut r, &mut payload).map_err(|e| e.to_string())?;
        if got < len as usize {
            out.torn_tail = true; // short payload at end of file
            break;
        }
        if crc32(&[&seq.to_le_bytes(), &payload]) != crc {
            if end == file_len {
                out.torn_tail = true; // last record, never completed durably
                break;
            }
            return Err(corrupt(pos, "checksum mismatch"));
        }
        if let Some(p) = prev {
            if seq != p + 1 {
                return Err(corrupt(pos, &format!("server_seq {seq} follows {p}")));
            }
        }
        each(seq, &payload)?;
        prev = Some(seq);
        pos = end;
        out.index.push((seq, end));
    }
    out.valid_len = pos;
    Ok(out)
}

/// The append side of the confirmed log (owned by the apply path; compaction takes the same
/// lock). Keeps the record index for bounded compaction.
pub(crate) struct ConfirmedLog {
    path: PathBuf,
    file: File,
    /// (server_seq, end offset) of every record in the file.
    index: Vec<(u64, u64)>,
    len: u64,
}

impl ConfirmedLog {
    /// Open for appending after a scan: a torn tail is truncated away first.
    pub(crate) fn open(path: &Path, scan: &LogScan) -> Result<Self, String> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        if scan.torn_tail {
            file.set_len(scan.valid_len)
                .and_then(|()| file.sync_all())
                .map_err(|e| format!("truncate torn tail of {}: {e}", path.display()))?;
        }
        let mut file = file;
        file.seek(SeekFrom::Start(scan.valid_len))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            index: scan.index.clone(),
            len: scan.valid_len,
        })
    }

    /// Open an **existing** log for appending after a scan, writing nothing: never creates the
    /// file and refuses a torn tail instead of truncating it (Slice 19B.2B deferred loader,
    /// which must stay read-only even if it outlives its instance).
    pub(crate) fn open_existing(path: &Path, scan: &LogScan) -> Result<Self, String> {
        if scan.torn_tail {
            return Err(format!(
                "{} has a torn tail (only a synchronous start truncates it)",
                path.display()
            ));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(false)
            .truncate(false)
            .open(path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        file.seek(SeekFrom::Start(scan.valid_len))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            index: scan.index.clone(),
            len: scan.valid_len,
        })
    }

    pub(crate) fn last_seq(&self) -> Option<u64> {
        self.index.last().map(|x| x.0)
    }

    /// Records after `through`.
    pub(crate) fn records_after(&self, through: u64) -> u64 {
        self.index.iter().filter(|x| x.0 > through).count() as u64
    }

    /// Bytes of records after `through`.
    pub(crate) fn bytes_after(&self, through: u64) -> u64 {
        let start = self
            .index
            .iter()
            .take_while(|x| x.0 <= through)
            .last()
            .map_or(0, |x| x.1);
        self.len - start
    }

    /// Append `items` (contiguous after the current last record) under one durability barrier.
    pub(crate) fn append(&mut self, items: &[(u64, &[u8])]) -> Result<u64, String> {
        if items.is_empty() {
            return Ok(0);
        }
        let mut next = self.last_seq();
        let mut buf = Vec::new();
        let mut ends = Vec::with_capacity(items.len());
        for (seq, payload) in items {
            if next.is_some_and(|p| *seq != p + 1) {
                return Err(format!(
                    "confirmed log: server_seq {seq} would follow {}",
                    next.unwrap()
                ));
            }
            buf.extend(frame(*seq, payload));
            ends.push((*seq, self.len + buf.len() as u64));
            next = Some(*seq);
        }
        self.file
            .write_all(&buf)
            .and_then(|()| self.file.sync_data())
            .map_err(|e| format!("append {}: {e}", self.path.display()))?;
        self.len += buf.len() as u64;
        self.index.extend(ends);
        Ok(buf.len() as u64)
    }

    /// Retire records ≤ `through` (covered by a promoted checkpoint): the tail after them is
    /// copied to a temp file, fsynced and renamed over the log. A crash leaves either the old
    /// log (records ≤ through are skipped on replay) or the new one.
    pub(crate) fn compact_through(
        &mut self,
        through: u64,
        hook: &dyn Fn(&str) -> Result<(), String>,
    ) -> Result<u64, String> {
        let keep_from = self
            .index
            .iter()
            .take_while(|x| x.0 <= through)
            .last()
            .map_or(0, |x| x.1);
        if keep_from == 0 {
            return Ok(0);
        }
        let dir = self.path.parent().ok_or("log has no parent")?;
        let tmp = dir.join(format!("{CONFIRMED_LOG_FILE}.tmp"));
        {
            let mut src = File::open(&self.path).map_err(|e| e.to_string())?;
            src.seek(SeekFrom::Start(keep_from))
                .map_err(|e| e.to_string())?;
            let mut out = BufWriter::new(File::create(&tmp).map_err(|e| e.to_string())?);
            std::io::copy(&mut src.take(self.len - keep_from), &mut out)
                .map_err(|e| e.to_string())?;
            let f = out.into_inner().map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
        }
        hook("confirmed_log_compaction_written")?;
        fs::rename(&tmp, &self.path).map_err(|e| format!("rename log: {e}"))?;
        fsync_dir(dir);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|e| e.to_string())?;
        let new_len = self.len - keep_from;
        file.seek(SeekFrom::Start(new_len))
            .map_err(|e| e.to_string())?;
        self.file = file;
        self.index = self
            .index
            .iter()
            .filter(|x| x.0 > through)
            .map(|&(s, e)| (s, e - keep_from))
            .collect();
        self.len = new_len;
        Ok(keep_from)
    }
}

pub(crate) fn fsync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

// ------------------------------------------------------------------------------ checkpoint

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointPageRef {
    pub page_id: PageId,
    pub sha256: Sha256Hex,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointNotebook {
    pub notebook_id: NotebookId,
    pub title: String,
    pub pages: Vec<CheckpointPageRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<FolderId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointManifestV3 {
    pub v: u32,
    pub through_server_seq: u64,
    pub library_id: LibraryId,
    pub page_space: PageSpace,
    pub notebooks: Vec<CheckpointNotebook>,
    pub history: HistoryV1,
    /// Omitted when empty (pre-folder checkpoints stay byte-identical and parse unchanged).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<SnapshotFolder>,
}

/// A page's content address: SHA-256 of its canonical JSON.
pub(crate) fn page_bytes(p: &SnapshotPage) -> Result<(Vec<u8>, Sha256Hex), String> {
    let bytes = serde_json::to_vec(p).map_err(|e| e.to_string())?;
    let sha = sha256_hex(&bytes);
    Ok((bytes, sha))
}

/// What a checkpoint needs, captured under the sync lock without copying unchanged pages:
/// structure + history (small), and clones of only the pages changed since the previous
/// checkpoint. Unchanged pages are referenced by the hash of the file already on disk.
pub(crate) struct CheckpointCapture<'a> {
    pub through_server_seq: u64,
    pub library_id: LibraryId,
    pub page_space: PageSpace,
    pub notebooks: Vec<(NotebookId, String, Vec<PageId>, Option<FolderId>)>,
    pub folders: Vec<SnapshotFolder>,
    pub history: Cow<'a, HistoryV1>,
    pub changed: Vec<Cow<'a, SnapshotPage>>,
    pub unchanged: HashMap<PageId, Sha256Hex>,
}

impl CheckpointCapture<'static> {
    /// `known`: page hashes of the current checkpoint; pages in `dirty` (or unknown) are cloned.
    pub(crate) fn capture(
        state: &LibraryState,
        through: u64,
        known: &HashMap<PageId, Sha256Hex>,
        dirty: &HashSet<PageId>,
    ) -> Self {
        let mut changed = Vec::new();
        let mut unchanged = HashMap::new();
        let mut notebooks = Vec::with_capacity(state.snapshot.notebooks.len());
        for n in &state.snapshot.notebooks {
            for p in &n.pages {
                match known.get(&p.page_id) {
                    Some(sha) if !dirty.contains(&p.page_id) => {
                        unchanged.insert(p.page_id, sha.clone());
                    }
                    _ => changed.push(Cow::Owned(p.clone())),
                }
            }
            notebooks.push((
                n.notebook_id,
                n.title.clone(),
                n.pages.iter().map(|p| p.page_id).collect(),
                n.folder_id,
            ));
        }
        Self {
            through_server_seq: through,
            library_id: state.snapshot.library_id,
            page_space: state.snapshot.page_space,
            notebooks,
            folders: state.snapshot.folders.clone(),
            history: Cow::Owned(state.history.clone()),
            changed,
            unchanged,
        }
    }
}

impl<'a> CheckpointCapture<'a> {
    /// Every page of `state`, borrowed (single-threaded use: startup conversion / new epoch).
    pub(crate) fn borrow_all(state: &'a LibraryState, through: u64) -> Self {
        Self {
            through_server_seq: through,
            library_id: state.snapshot.library_id,
            page_space: state.snapshot.page_space,
            notebooks: state
                .snapshot
                .notebooks
                .iter()
                .map(|n| {
                    (
                        n.notebook_id,
                        n.title.clone(),
                        n.pages.iter().map(|p| p.page_id).collect(),
                        n.folder_id,
                    )
                })
                .collect(),
            folders: state.snapshot.folders.clone(),
            history: Cow::Borrowed(&state.history),
            changed: state
                .snapshot
                .notebooks
                .iter()
                .flat_map(|n| &n.pages)
                .map(Cow::Borrowed)
                .collect(),
            unchanged: HashMap::new(),
        }
    }
}

/// Outcome of a promoted checkpoint.
#[derive(Debug, Default)]
pub(crate) struct CheckpointWritten {
    pub through_server_seq: u64,
    pub page_hashes: HashMap<PageId, Sha256Hex>,
    pub pages_written: usize,
    pub bytes_written: u64,
}

fn write_file_durable(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let tmp = dir.join(format!("{name}.tmp"));
    {
        let mut f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, dir.join(name)).map_err(|e| format!("rename {}: {e}", tmp.display()))
}

/// Write + promote a checkpoint from a capture (no sync lock needed). `hook` names the crash
/// points the tests exercise. Returns only after the manifest rename (promotion) is durable.
pub(crate) fn write_checkpoint(
    sync_dir: &Path,
    cap: &CheckpointCapture<'_>,
    hook: &dyn Fn(&str) -> Result<(), String>,
) -> Result<CheckpointWritten, String> {
    let dir = sync_dir.join(CHECKPOINT_DIR);
    let pages_dir = dir.join(CHECKPOINT_PAGES);
    fs::create_dir_all(&pages_dir).map_err(|e| format!("create {}: {e}", pages_dir.display()))?;
    let mut out = CheckpointWritten {
        through_server_seq: cap.through_server_seq,
        page_hashes: cap.unchanged.clone(),
        ..Default::default()
    };
    for p in &cap.changed {
        let (bytes, sha) = page_bytes(p)?;
        if !pages_dir.join(sha.as_str()).exists() {
            write_file_durable(&pages_dir, sha.as_str(), &bytes)?;
            out.pages_written += 1;
            out.bytes_written += bytes.len() as u64;
        }
        out.page_hashes.insert(p.page_id, sha);
    }
    fsync_dir(&pages_dir);
    hook("checkpoint_after_pages")?;
    let manifest = CheckpointManifestV3 {
        v: 3,
        through_server_seq: cap.through_server_seq,
        library_id: cap.library_id,
        page_space: cap.page_space,
        notebooks: cap
            .notebooks
            .iter()
            .map(|(id, title, pages, folder)| {
                Ok(CheckpointNotebook {
                    notebook_id: *id,
                    title: title.clone(),
                    folder_id: *folder,
                    pages: pages
                        .iter()
                        .map(|p| {
                            Ok(CheckpointPageRef {
                                page_id: *p,
                                sha256: out.page_hashes.get(p).cloned().ok_or_else(|| {
                                    format!("checkpoint: no content for page {p}")
                                })?,
                            })
                        })
                        .collect::<Result<_, String>>()?,
                })
            })
            .collect::<Result<_, String>>()?,
        history: cap.history.clone().into_owned(),
        folders: cap.folders.clone(),
    };
    let mut bytes = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let tmp = dir.join(format!("{CHECKPOINT_MANIFEST}.tmp"));
    {
        let mut f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(&bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        hook("checkpoint_before_manifest_fsync")?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    hook("checkpoint_before_promotion")?;
    fs::rename(&tmp, dir.join(CHECKPOINT_MANIFEST))
        .map_err(|e| format!("promote checkpoint: {e}"))?;
    fsync_dir(&dir);
    out.bytes_written += bytes.len() as u64;
    Ok(out)
}

/// Remove page files no promoted manifest references (after promotion; also at startup).
pub(crate) fn collect_unreferenced_pages(
    sync_dir: &Path,
    referenced: &HashMap<PageId, Sha256Hex>,
) -> Result<usize, String> {
    let pages_dir = sync_dir.join(CHECKPOINT_DIR).join(CHECKPOINT_PAGES);
    let keep: HashSet<&str> = referenced.values().map(Sha256Hex::as_str).collect();
    let mut removed = 0;
    let rd = match fs::read_dir(&pages_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.to_string()),
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !keep.contains(name.as_str()) {
            fs::remove_file(e.path()).map_err(|e| format!("remove {name}: {e}"))?;
            removed += 1;
        }
    }
    if removed > 0 {
        fsync_dir(&pages_dir);
    }
    Ok(removed)
}

/// A promoted checkpoint: state, its server_seq, and each page's content address.
pub(crate) type LoadedCheckpoint = (LibraryState, u64, HashMap<PageId, Sha256Hex>);

/// Slice 19A: totals of one manifest read ([`read_checkpoint_head`]), reported once to the
/// startup breakdown when dropped (so a failed read still reports). No-op outside startup.
#[derive(Default)]
struct CheckpointHeadStats {
    present: bool,
    manifest_bytes: u64,
    manifest_read: std::time::Duration,
    manifest_parse: std::time::Duration,
}

impl Drop for CheckpointHeadStats {
    fn drop(&mut self) {
        use crate::sync_startup_diag as diag;
        diag::count("ckpt_present", u64::from(self.present));
        if !self.present {
            return;
        }
        diag::count("ckpt_manifest_bytes", self.manifest_bytes);
        diag::time("ckpt_manifest_read", self.manifest_read);
        diag::time("ckpt_manifest_parse", self.manifest_parse);
    }
}

/// Slice 19A: totals of one page phase ([`materialize_checkpoint`]), reported when dropped.
#[derive(Default)]
struct CheckpointPageStats {
    pages: u64,
    page_bytes: u64,
    read: std::time::Duration,
    sha: std::time::Duration,
    parse: std::time::Duration,
    validate: std::time::Duration,
}

impl Drop for CheckpointPageStats {
    fn drop(&mut self) {
        use crate::sync_startup_diag as diag;
        diag::count("ckpt_pages", self.pages);
        diag::count("ckpt_page_bytes", self.page_bytes);
        diag::time("ckpt_read", self.read);
        diag::time("ckpt_sha", self.sha);
        diag::time("ckpt_parse", self.parse);
        diag::time("ckpt_validate", self.validate);
    }
}

/// A promoted checkpoint's manifest, parsed and version-checked, before any page file is read.
/// Its page references and `through_server_seq` are **claims only**: page contents are
/// verified against their addresses, and the state validated, solely by
/// [`materialize_checkpoint`]. Nothing may treat a head as a trusted ServerBaseState.
pub(crate) struct CheckpointHead {
    manifest: CheckpointManifestV3,
}

impl CheckpointHead {
    /// The server_seq the manifest claims to cover.
    pub(crate) fn through(&self) -> u64 {
        self.manifest.through_server_seq
    }

    /// Whether the pages directory holds a file this manifest does not reference (startup's
    /// [`collect_unreferenced_pages`] would remove it). Read-only.
    pub(crate) fn unreferenced_pages_exist(&self, sync_dir: &Path) -> Result<bool, String> {
        let keep: HashSet<&str> = self
            .manifest
            .notebooks
            .iter()
            .flat_map(|n| n.pages.iter().map(|r| r.sha256.as_str()))
            .collect();
        let pages_dir = sync_dir.join(CHECKPOINT_DIR).join(CHECKPOINT_PAGES);
        let rd = match fs::read_dir(&pages_dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.to_string()),
        };
        for e in rd {
            let e = e.map_err(|e| e.to_string())?;
            if !keep.contains(e.file_name().to_string_lossy().as_ref()) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Load the promoted checkpoint: every page file is read once and verified against its
/// content address; the result is validated as a reducer state. Exactly
/// [`read_checkpoint_head`] followed by [`materialize_checkpoint`].
pub(crate) fn load_checkpoint(sync_dir: &Path) -> Result<Option<LoadedCheckpoint>, String> {
    match read_checkpoint_head(sync_dir)? {
        None => Ok(None),
        Some(head) => materialize_checkpoint(sync_dir, head).map(Some),
    }
}

/// The manifest phase of [`load_checkpoint`]: `Ok(None)` when no checkpoint is promoted.
pub(crate) fn read_checkpoint_head(sync_dir: &Path) -> Result<Option<CheckpointHead>, String> {
    let mut st = CheckpointHeadStats::default();
    let mpath = sync_dir.join(CHECKPOINT_DIR).join(CHECKPOINT_MANIFEST);
    let t = Instant::now();
    let bytes = match fs::read(&mpath) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", mpath.display())),
    };
    st.present = true;
    st.manifest_bytes = bytes.len() as u64;
    st.manifest_read = t.elapsed();
    let t = Instant::now();
    let m: CheckpointManifestV3 = serde_json::from_slice(&bytes)
        .map_err(|e| format!("CHECKPOINT_CORRUPT {}: {e}", mpath.display()))?;
    st.manifest_parse = t.elapsed();
    if m.v != 3 {
        return Err(format!("unsupported checkpoint version {}", m.v));
    }
    Ok(Some(CheckpointHead { manifest: m }))
}

/// The page phase of [`load_checkpoint`]: read every page the head names, verify it against its
/// content address, parse it, and validate the assembled state. Any failure is
/// `CHECKPOINT_CORRUPT` (or a read error), exactly as [`load_checkpoint`] reports it.
pub(crate) fn materialize_checkpoint(
    sync_dir: &Path,
    head: CheckpointHead,
) -> Result<LoadedCheckpoint, String> {
    let mut st = CheckpointPageStats::default();
    let m = head.manifest;
    let pages_dir = sync_dir.join(CHECKPOINT_DIR).join(CHECKPOINT_PAGES);
    let mut hashes = HashMap::new();
    let mut notebooks = Vec::with_capacity(m.notebooks.len());
    for n in m.notebooks {
        let mut pages = Vec::with_capacity(n.pages.len());
        for r in n.pages {
            let path = pages_dir.join(r.sha256.as_str());
            let t0 = Instant::now();
            let b = fs::read(&path)
                .map_err(|e| format!("CHECKPOINT_CORRUPT page {}: {e}", r.page_id))?;
            let t1 = Instant::now();
            st.read += t1 - t0;
            st.pages += 1;
            st.page_bytes += b.len() as u64;
            let addressed = sha256_hex(&b) == r.sha256;
            let t2 = Instant::now();
            st.sha += t2 - t1;
            if !addressed {
                return Err(format!(
                    "CHECKPOINT_CORRUPT page {}: content does not match its address",
                    r.page_id
                ));
            }
            let p: Result<SnapshotPage, _> = serde_json::from_slice(&b);
            st.parse += t2.elapsed();
            let p = p.map_err(|e| format!("CHECKPOINT_CORRUPT page {}: {e}", r.page_id))?;
            if p.page_id != r.page_id {
                return Err(format!(
                    "CHECKPOINT_CORRUPT: file for page {} holds page {}",
                    r.page_id, p.page_id
                ));
            }
            hashes.insert(r.page_id, r.sha256);
            pages.push(p);
        }
        notebooks.push(SnapshotNotebook {
            notebook_id: n.notebook_id,
            title: n.title,
            pages,
            folder_id: n.folder_id,
        });
    }
    let state = LibraryState {
        snapshot: SyncSnapshotV1 {
            v: SNAPSHOT_VERSION,
            library_id: m.library_id,
            page_space: m.page_space,
            notebooks,
            folders: m.folders,
        },
        history: m.history,
    };
    let t = Instant::now();
    let valid = state.validate();
    st.validate = t.elapsed();
    valid.map_err(|e| format!("CHECKPOINT_CORRUPT: invalid state: {e}"))?;
    Ok((state, m.through_server_seq, hashes))
}

/// Replay the confirmed log onto `state` (checkpoint through `through`): records ≤ `through`
/// are skipped (they may survive a crash between promotion and compaction), the first applied
/// record must be `through + 1`. Returns the scan (for opening the log) and the last applied
/// server_seq, plus every page the replayed records touched: those pages differ from the
/// checkpoint's files, so the next checkpoint **must** rewrite them (they are not "unchanged"
/// merely because this process did not apply them live). `envelope` decodes a payload.
pub(crate) fn replay_log(
    sync_dir: &Path,
    state: &mut LibraryState,
    through: u64,
    envelope: &dyn Fn(&[u8]) -> Result<SyncEnvelopeV1, String>,
) -> Result<(LogScan, u64, HashSet<PageId>), String> {
    let mut last = through;
    let mut touched = HashSet::new();
    // Slice 19A totals (startup breakdown; reported even when the replay fails).
    let (mut records, mut applied, mut payload_bytes) = (0u64, 0u64, 0u64);
    let (mut decode_t, mut apply_t) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    let t_scan = Instant::now();
    let scan = scan_log(&sync_dir.join(CONFIRMED_LOG_FILE), |seq, payload| {
        records += 1;
        payload_bytes += payload.len() as u64;
        if seq <= through {
            return Ok(());
        }
        if seq != last + 1 {
            return Err(format!(
                "CONFIRMED_LOG_GAP: checkpoint through {through} but next record is server_seq {seq}"
            ));
        }
        let t0 = Instant::now();
        let env = envelope(payload);
        let t1 = Instant::now();
        decode_t += t1 - t0;
        let env = env?;
        let r = state.apply_in_place(&env);
        apply_t += t1.elapsed();
        r.map_err(|e| format!("reducer refused confirmed server_seq {seq}: {e}"))?;
        touched.extend(touched_pages(&env.operation));
        last = seq;
        applied += 1;
        Ok(())
    });
    {
        use crate::sync_startup_diag as diag;
        diag::time("replay_scan", t_scan.elapsed());
        diag::time("replay_decode", decode_t);
        diag::time("replay_apply", apply_t);
        diag::count("replay_records", records);
        diag::count("replay_applied", applied);
        diag::count("replay_payload_bytes", payload_bytes);
    }
    let scan = scan?;
    Ok((scan, last, touched))
}

/// Checkpoint policy (first threshold reached wins). Initial values; tune from Kindle data.
#[derive(Clone, Copy, Debug)]
pub struct CheckpointPolicy {
    pub max_ops: u64,
    pub max_log_bytes: u64,
    /// Checkpoint a dirty state at least this often.
    pub max_age: std::time::Duration,
    /// …or once inbound has been quiet this long (opportunistic, cheap: only changed pages).
    pub idle: std::time::Duration,
}

impl Default for CheckpointPolicy {
    fn default() -> Self {
        Self {
            max_ops: 15_000,
            max_log_bytes: 48 * 1024 * 1024,
            max_age: std::time::Duration::from_secs(7 * 60),
            idle: std::time::Duration::from_secs(90),
        }
    }
}

impl CheckpointPolicy {
    /// Why a checkpoint is due now, if it is.
    pub fn due(
        &self,
        ops: u64,
        log_bytes: u64,
        since_checkpoint: std::time::Duration,
        since_inbound: std::time::Duration,
    ) -> Option<&'static str> {
        if ops == 0 {
            return None;
        }
        if ops >= self.max_ops {
            Some("ops")
        } else if log_bytes >= self.max_log_bytes {
            Some("log_bytes")
        } else if since_checkpoint >= self.max_age {
            Some("age")
        } else if since_inbound >= self.idle {
            Some("idle")
        } else {
            None
        }
    }
}

/// Pages an operation may change (checkpoint dirtiness + materialization scope).
pub(crate) fn touched_pages(op: &sync_protocol::SyncOperationV1) -> Vec<PageId> {
    use sync_protocol::SyncOperationV1 as O;
    match op {
        O::CreateNotebook { first_page, .. } => vec![first_page.page_id],
        O::CreatePage { page, .. } => vec![page.page_id],
        O::AddStroke { page_id, .. }
        | O::EraseRegion { page_id, .. }
        | O::DeleteStrokeLineage { page_id, .. }
        | O::ReplaceStrokeLineages { page_id, .. }
        | O::MoveStrokeLineages { page_id, .. }
        | O::AddText { page_id, .. }
        | O::UpdateText { page_id, .. }
        | O::DeleteText { page_id, .. }
        | O::AddPageLink { page_id, .. }
        | O::DeletePageLink { page_id, .. } => vec![*page_id],
        O::RenameNotebook { .. }
        | O::DeleteNotebook { .. }
        | O::CreateFolder { .. }
        | O::RenameFolder { .. }
        | O::DeleteFolder { .. }
        | O::SetNotebookFolder { .. }
        | O::MovePage { .. }
        | O::DeletePage { .. } => Vec::new(),
    }
}

/// Library structure (notebook order, titles, page order) — what a scoped verification compares
/// besides the touched pages.
pub(crate) type Structure = Vec<(NotebookId, String, Vec<PageId>)>;

pub(crate) fn structure_of(s: &SyncSnapshotV1) -> Structure {
    s.notebooks
        .iter()
        .map(|n| {
            (
                n.notebook_id,
                n.title.clone(),
                n.pages.iter().map(|p| p.page_id).collect(),
            )
        })
        .collect()
}
