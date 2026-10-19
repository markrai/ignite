//! Strict ZIP container parsing (structure only; no entry content is trusted here).
//!
//! Accepted profile — what the Slice 1C exporter and ordinary writers (Python `zipfile`) produce:
//!
//! ```text
//! [local header + deflate data]*  central directory  [zip64 EOCD + locator]  EOCD
//! ```
//!
//! * single disk; no archive comment; no bytes before the first entry, between entries, or
//!   after the end record; every entry's data ends exactly where the next header starts;
//! * Deflate (method 8) only; general-purpose flags limited to bits 1, 2 (deflate level hints)
//!   and 11 (UTF-8 names); encryption flags (0, 6, 13) and data descriptors (3) are refused;
//! * only the ZIP64 extra field is understood; any other extra field, entry comment or
//!   unknown host system is refused;
//! * regular files only: unix symlinks, directories and special files, and DOS directory /
//!   volume attributes are refused;
//! * names must be exact canonical `EntryPath`s and unique; the local header must repeat the
//!   central directory's name, flags, method, CRC and sizes.
//!
//! The well-known `zip` crate reader is deliberately not used for this: it collapses duplicate
//! central-directory names and ignores local headers apart from their lengths.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};

use portable_backup::{EntryPath, Limits, MAX_ENTRY_PATH_LEN};

use crate::error::{RestoreError, ZipErrorCode as Z};

const EOCD_SIG: u32 = 0x0605_4b50;
const EOCD_LEN: u64 = 22;
const LOCATOR_SIG: u32 = 0x0706_4b50;
const LOCATOR_LEN: u64 = 20;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
const ZIP64_EOCD_LEN: u64 = 56;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const CENTRAL_LEN: usize = 46;
const LOCAL_SIG: u32 = 0x0403_4b50;
const LOCAL_LEN: usize = 30;
const ZIP64_EXTRA: u16 = 0x0001;
const METHOD_DEFLATE: u16 = 8;
const ALLOWED_FLAGS: u16 = (1 << 1) | (1 << 2) | (1 << 11);
const ENCRYPTION_FLAGS: u16 = (1 << 0) | (1 << 6) | (1 << 13);
const DATA_DESCRIPTOR_FLAG: u16 = 1 << 3;
const HOST_DOS: u8 = 0;
const HOST_UNIX: u8 = 3;
/// Highest "version needed to extract" in the accepted profile (4.5 = ZIP64).
const MAX_VERSION_NEEDED: u16 = 45;
/// Central directory bytes per entry: fixed header + longest name + a full ZIP64 extra.
const MAX_CENTRAL_ENTRY: u64 = CENTRAL_LEN as u64 + MAX_ENTRY_PATH_LEN as u64 + 4 + 28;

/// One validated entry: where its compressed bytes are and what they must inflate to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub path: EntryPath,
    pub crc32: u32,
    pub compressed_size: u64,
    pub size: u64,
    pub data_start: u64,
}

/// Archive layout after structural validation, entries in archive order.
#[derive(Debug)]
pub(crate) struct Container {
    pub archive_bytes: u64,
    pub entries: Vec<Entry>,
}

fn bad(code: Z, msg: impl std::fmt::Display) -> RestoreError {
    RestoreError::zip(code, msg)
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn read_at<R: Read + Seek>(
    r: &mut R,
    offset: u64,
    len: usize,
    what: &str,
) -> Result<Vec<u8>, RestoreError> {
    r.seek(SeekFrom::Start(offset))
        .map_err(|e| RestoreError::io(format!("seek to {what}: {e}")))?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            bad(
                Z::Truncated,
                format!("{what} runs past the end of the archive"),
            )
        } else {
            RestoreError::io(format!("read {what}: {e}"))
        }
    })?;
    Ok(buf)
}

struct CentralDirectory {
    entries: u64,
    size: u64,
    offset: u64,
    /// Where the central directory must end (the ZIP64 EOCD or the EOCD).
    end: u64,
}

fn end_records<R: Read + Seek>(r: &mut R, len: u64) -> Result<CentralDirectory, RestoreError> {
    if len < EOCD_LEN {
        return Err(bad(
            Z::NotZip,
            format!("{len} bytes is too short to be a ZIP archive"),
        ));
    }
    let eocd_at = len - EOCD_LEN;
    let e = read_at(r, eocd_at, EOCD_LEN as usize, "end of central directory")?;
    if u32_at(&e, 0) != EOCD_SIG {
        return Err(bad(
            Z::NotZip,
            "no end-of-central-directory record at the end (truncated, not a ZIP, or trailing data / archive comment)",
        ));
    }
    let (disk, cd_disk) = (u16_at(&e, 4), u16_at(&e, 6));
    let (on_disk, total) = (u16_at(&e, 8), u16_at(&e, 10));
    let (size32, offset32, comment) = (u32_at(&e, 12), u32_at(&e, 16), u16_at(&e, 20));
    if comment != 0 {
        return Err(bad(
            Z::Unsupported,
            "archive comments are not part of the format",
        ));
    }
    let has_locator = eocd_at >= LOCATOR_LEN + ZIP64_EOCD_LEN && {
        let l = read_at(
            r,
            eocd_at - LOCATOR_LEN,
            LOCATOR_LEN as usize,
            "zip64 locator",
        )?;
        u32_at(&l, 0) == LOCATOR_SIG
    };
    let needs_zip64 = total == 0xFFFF || size32 == 0xFFFF_FFFF || offset32 == 0xFFFF_FFFF;
    if !has_locator {
        if needs_zip64 {
            return Err(bad(
                Z::Malformed,
                "ZIP64 sizes are flagged but no ZIP64 locator is present",
            ));
        }
        if disk != 0 || cd_disk != 0 || on_disk != total {
            return Err(bad(Z::Unsupported, "multi-disk archives are not supported"));
        }
        return Ok(CentralDirectory {
            entries: u64::from(total),
            size: u64::from(size32),
            offset: u64::from(offset32),
            end: eocd_at,
        });
    }
    let l = read_at(
        r,
        eocd_at - LOCATOR_LEN,
        LOCATOR_LEN as usize,
        "zip64 locator",
    )?;
    let (z_disk, z_at, disks) = (u32_at(&l, 4), u64_at(&l, 8), u32_at(&l, 16));
    if z_disk != 0 || disks != 1 {
        return Err(bad(Z::Unsupported, "multi-disk archives are not supported"));
    }
    if z_at != eocd_at - LOCATOR_LEN - ZIP64_EOCD_LEN {
        return Err(bad(
            Z::Malformed,
            "ZIP64 end record is not immediately before its locator",
        ));
    }
    let z = read_at(
        r,
        z_at,
        ZIP64_EOCD_LEN as usize,
        "zip64 end of central directory",
    )?;
    if u32_at(&z, 0) != ZIP64_EOCD_SIG || u64_at(&z, 4) != ZIP64_EOCD_LEN - 12 {
        return Err(bad(
            Z::Malformed,
            "invalid ZIP64 end record (or unsupported extensible data)",
        ));
    }
    if u32_at(&z, 16) != 0
        || u32_at(&z, 20) != 0
        || u64_at(&z, 24) != u64_at(&z, 32)
        || disk != 0
        || cd_disk != 0
    {
        return Err(bad(Z::Unsupported, "multi-disk archives are not supported"));
    }
    let (entries, size, offset) = (u64_at(&z, 32), u64_at(&z, 40), u64_at(&z, 48));
    // The 32-bit record must agree with ZIP64 or carry its sentinel — both entry counts
    // (single disk: entries on this disk == total entries, already required of ZIP64).
    let agrees = |v32: u64, sentinel: u64, v64: u64| v32 == sentinel || v32 == v64;
    if !agrees(u64::from(on_disk), 0xFFFF, entries)
        || !agrees(u64::from(total), 0xFFFF, entries)
        || !agrees(u64::from(size32), 0xFFFF_FFFF, size)
        || !agrees(u64::from(offset32), 0xFFFF_FFFF, offset)
    {
        return Err(bad(Z::Malformed, "end records disagree"));
    }
    Ok(CentralDirectory {
        entries,
        size,
        offset,
        end: z_at,
    })
}

/// Values a ZIP64 extra field supplies for the 32-bit fields that hold sentinels, in spec order.
fn zip64_values(extra: &[u8], wanted: usize, what: &str) -> Result<Vec<u64>, RestoreError> {
    let mut found: Option<Vec<u64>> = None;
    let mut i = 0;
    while i < extra.len() {
        if extra.len() - i < 4 {
            return Err(bad(Z::Malformed, format!("{what}: truncated extra field")));
        }
        let (id, n) = (u16_at(extra, i), u16_at(extra, i + 2) as usize);
        let body = extra
            .get(i + 4..i + 4 + n)
            .ok_or_else(|| bad(Z::Malformed, format!("{what}: truncated extra field")))?;
        if id != ZIP64_EXTRA {
            return Err(bad(
                Z::Unsupported,
                format!("{what}: unsupported extra field {id:#06x}"),
            ));
        }
        if found.is_some() {
            return Err(bad(
                Z::Malformed,
                format!("{what}: repeated ZIP64 extra field"),
            ));
        }
        if n != wanted * 8 {
            return Err(bad(
                Z::Malformed,
                format!(
                    "{what}: ZIP64 extra field has {n} bytes, expected {}",
                    wanted * 8
                ),
            ));
        }
        found = Some((0..wanted).map(|k| u64_at(body, k * 8)).collect());
        i += 4 + n;
    }
    match found {
        Some(v) => Ok(v),
        None if wanted == 0 => Ok(Vec::new()),
        None => Err(bad(
            Z::Malformed,
            format!("{what}: ZIP64 sentinel without a ZIP64 extra field"),
        )),
    }
}

/// Resolve sentinel 32-bit fields from a ZIP64 extra (order: size, compressed size, offset).
fn resolve(fields: &mut [&mut u64], extra: &[u8], what: &str) -> Result<(), RestoreError> {
    let wanted = fields.iter().filter(|f| ***f == 0xFFFF_FFFF).count();
    let values = zip64_values(extra, wanted, what)?;
    let mut it = values.into_iter();
    for f in fields.iter_mut() {
        if **f == 0xFFFF_FFFF {
            **f = it.next().expect("counted");
        }
    }
    Ok(())
}

struct Central {
    path: EntryPath,
    name: Vec<u8>,
    flags: u16,
    crc32: u32,
    compressed_size: u64,
    size: u64,
    local_offset: u64,
}

fn parse_central(cd: &[u8], at: &mut usize) -> Result<Central, RestoreError> {
    let h = cd.get(*at..*at + CENTRAL_LEN).ok_or_else(|| {
        bad(
            Z::Malformed,
            "central directory is shorter than its entry count",
        )
    })?;
    if u32_at(h, 0) != CENTRAL_SIG {
        return Err(bad(
            Z::Malformed,
            "central directory entry signature missing",
        ));
    }
    let host = (u16_at(h, 4) >> 8) as u8;
    let (needed, flags, method) = (u16_at(h, 6), u16_at(h, 8), u16_at(h, 10));
    let crc32 = u32_at(h, 16);
    let (mut csize, mut size) = (u64::from(u32_at(h, 20)), u64::from(u32_at(h, 24)));
    let (n, e, c) = (
        u16_at(h, 28) as usize,
        u16_at(h, 30) as usize,
        u16_at(h, 32) as usize,
    );
    let (disk, internal, external) = (u16_at(h, 34), u16_at(h, 36), u32_at(h, 38));
    let mut local_offset = u64::from(u32_at(h, 42));
    let rest = cd
        .get(*at + CENTRAL_LEN..*at + CENTRAL_LEN + n + e + c)
        .ok_or_else(|| {
            bad(
                Z::Malformed,
                "central directory entry runs past the directory",
            )
        })?;
    let name = rest[..n].to_vec();
    let extra = &rest[n..n + e];
    *at += CENTRAL_LEN + n + e + c;

    let shown = String::from_utf8_lossy(&name).into_owned();
    let what = format!("entry {shown:?}");
    if flags & ENCRYPTION_FLAGS != 0 || method == 99 {
        return Err(bad(Z::Encrypted, format!("{what} is encrypted")));
    }
    if flags & DATA_DESCRIPTOR_FLAG != 0 {
        return Err(bad(
            Z::Unsupported,
            format!("{what} uses a data descriptor"),
        ));
    }
    if flags & !ALLOWED_FLAGS != 0 {
        return Err(bad(
            Z::Unsupported,
            format!("{what} uses unsupported flags {flags:#06x}"),
        ));
    }
    if method != METHOD_DEFLATE {
        return Err(bad(
            Z::Unsupported,
            format!("{what} uses compression method {method} (only Deflate)"),
        ));
    }
    if needed > MAX_VERSION_NEEDED {
        return Err(bad(
            Z::Unsupported,
            format!("{what} needs ZIP version {needed}"),
        ));
    }
    if c != 0 {
        return Err(bad(Z::Unsupported, format!("{what} has an entry comment")));
    }
    // Single-disk archives only: the disk start must be 0. The 0xFFFF sentinel (disk number
    // moved to a ZIP64 extra) is refused rather than interpreted.
    if disk != 0 {
        return Err(bad(
            Z::Unsupported,
            format!("{what} has disk start {disk:#06x}; only single-disk archives are supported"),
        ));
    }
    if internal & !1 != 0 {
        return Err(bad(
            Z::Unsupported,
            format!("{what} has unsupported internal attributes"),
        ));
    }
    let mode = external >> 16;
    match host {
        HOST_UNIX | HOST_DOS => {}
        other => {
            return Err(bad(
                Z::Unsupported,
                format!("{what} was written by unsupported host system {other}"),
            ))
        }
    }
    if host == HOST_UNIX || mode != 0 {
        let kind = mode & 0o170000;
        if kind != 0 && kind != 0o100000 {
            return Err(bad(
                Z::NotRegularFile,
                format!("{what} is not a regular file (mode {mode:o})"),
            ));
        }
    }
    if host == HOST_DOS && external & 0x18 != 0 {
        return Err(bad(
            Z::NotRegularFile,
            format!("{what} is a directory or volume label"),
        ));
    }
    let text = std::str::from_utf8(&name)
        .map_err(|_| bad(Z::InvalidEntryName, format!("{what}: name is not UTF-8")))?;
    let path = EntryPath::parse(text).map_err(|e| bad(Z::InvalidEntryName, e))?;
    if path.to_path() != text {
        return Err(bad(
            Z::InvalidEntryName,
            format!("{what}: name is not canonical"),
        ));
    }
    let mut fields = [&mut size, &mut csize, &mut local_offset];
    resolve(&mut fields, extra, &what)?;
    Ok(Central {
        path,
        name,
        flags,
        crc32,
        compressed_size: csize,
        size,
        local_offset,
    })
}

/// Per-entry declared-size bounds and per-kind totals, enforced before any inflation.
fn check_declared(entries: &[Entry], limits: &Limits) -> Result<(), RestoreError> {
    let (mut snapshots, mut blobs) = (0usize, 0usize);
    let (mut snapshot_total, mut blob_total) = (0u64, 0u64);
    for e in entries {
        let (cap, label) = match e.path {
            EntryPath::Manifest => (limits.max_manifest_bytes, "manifest"),
            EntryPath::Snapshot(_) => {
                snapshots += 1;
                snapshot_total = snapshot_total.saturating_add(e.size);
                (limits.max_snapshot_bytes, "snapshot")
            }
            EntryPath::Blob(_) => {
                blobs += 1;
                blob_total = blob_total.saturating_add(e.size);
                (limits.max_blob_bytes, "blob")
            }
        };
        if e.size > cap {
            return Err(bad(
                Z::LimitExceeded,
                format!(
                    "{label} entry {} declares {} bytes (limit {cap})",
                    e.path, e.size
                ),
            ));
        }
    }
    if snapshots > limits.max_libraries || blobs > limits.max_blobs {
        return Err(bad(
            Z::LimitExceeded,
            format!("{snapshots} snapshot / {blobs} blob entries exceed the limits"),
        ));
    }
    if snapshot_total > limits.max_total_snapshot_bytes || blob_total > limits.max_total_blob_bytes
    {
        return Err(bad(
            Z::LimitExceeded,
            "declared entry sizes exceed the total limits",
        ));
    }
    Ok(())
}

/// Validate the whole container structure. Reads only end records, the central directory and
/// local headers; no entry data.
pub(crate) fn parse<R: Read + Seek>(r: &mut R, limits: &Limits) -> Result<Container, RestoreError> {
    let len = r
        .seek(SeekFrom::End(0))
        .map_err(|e| RestoreError::io(format!("archive size: {e}")))?;
    let cd = end_records(r, len)?;
    let max_entries = 1u64
        .saturating_add(limits.max_libraries as u64)
        .saturating_add(limits.max_blobs as u64);
    if cd.entries == 0 {
        return Err(bad(Z::MissingManifest, "the archive has no entries"));
    }
    if cd.entries > max_entries {
        return Err(bad(
            Z::LimitExceeded,
            format!("{} entries (limit {max_entries})", cd.entries),
        ));
    }
    if cd.size > cd.entries.saturating_mul(MAX_CENTRAL_ENTRY) {
        return Err(bad(
            Z::Malformed,
            "central directory is larger than its entries allow",
        ));
    }
    if cd.offset.checked_add(cd.size) != Some(cd.end) {
        return Err(bad(
            Z::Malformed,
            "central directory does not end where the end records begin",
        ));
    }
    let dir = read_at(r, cd.offset, cd.size as usize, "central directory")?;
    let mut at = 0;
    let mut centrals = Vec::with_capacity(cd.entries as usize);
    let mut names = HashSet::new();
    for _ in 0..cd.entries {
        let c = parse_central(&dir, &mut at)?;
        if !names.insert(c.path.clone()) {
            return Err(bad(
                Z::DuplicateEntry,
                format!("entry {} appears more than once", c.path),
            ));
        }
        centrals.push(c);
    }
    if at != dir.len() {
        return Err(bad(
            Z::Malformed,
            "central directory has bytes beyond its entries",
        ));
    }
    if !names.contains(&EntryPath::Manifest) {
        return Err(bad(Z::MissingManifest, "the archive has no manifest.json"));
    }

    // Local headers must repeat the central directory, and the entries must tile the space
    // before the central directory exactly (no prefix, gaps, overlap or hidden data).
    let mut by_offset: Vec<usize> = (0..centrals.len()).collect();
    by_offset.sort_by_key(|&i| centrals[i].local_offset);
    let mut entries: Vec<Option<Entry>> = vec![None; centrals.len()];
    let mut expected = 0u64;
    for i in by_offset {
        let c = &centrals[i];
        let what = format!("entry {}", c.path);
        if c.local_offset != expected {
            return Err(bad(
                Z::Layout,
                format!(
                    "{what} starts at {} (expected {expected}): prefix, gap or overlap",
                    c.local_offset
                ),
            ));
        }
        let h = read_at(r, c.local_offset, LOCAL_LEN, "local header")?;
        if u32_at(&h, 0) != LOCAL_SIG {
            return Err(bad(
                Z::Malformed,
                format!("{what}: local header signature missing"),
            ));
        }
        let (n, e) = (u16_at(&h, 26) as usize, u16_at(&h, 28) as usize);
        let var = read_at(
            r,
            c.local_offset + LOCAL_LEN as u64,
            n + e,
            "local header name",
        )?;
        let (mut csize, mut size) = (u64::from(u32_at(&h, 18)), u64::from(u32_at(&h, 22)));
        let mut fields = [&mut size, &mut csize];
        resolve(&mut fields, &var[n..], &what)?;
        if var[..n] != c.name[..]
            || u16_at(&h, 6) != c.flags
            || u16_at(&h, 8) != METHOD_DEFLATE
            || u32_at(&h, 14) != c.crc32
            || csize != c.compressed_size
            || size != c.size
        {
            return Err(bad(
                Z::HeaderMismatch,
                format!("{what}: local header disagrees with the central directory"),
            ));
        }
        let data_start = c.local_offset + (LOCAL_LEN + n + e) as u64;
        expected = data_start
            .checked_add(c.compressed_size)
            .ok_or_else(|| bad(Z::Malformed, format!("{what}: size overflow")))?;
        entries[i] = Some(Entry {
            path: c.path.clone(),
            crc32: c.crc32,
            compressed_size: c.compressed_size,
            size: c.size,
            data_start,
        });
    }
    if expected != cd.offset {
        return Err(bad(
            Z::Layout,
            "entry data does not end where the central directory begins",
        ));
    }
    let entries: Vec<Entry> = entries
        .into_iter()
        .map(|e| e.expect("every entry placed"))
        .collect();
    check_declared(&entries, limits)?;
    Ok(Container {
        archive_bytes: len,
        entries,
    })
}
