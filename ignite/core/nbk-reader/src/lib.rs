//! Read-only decoder for Amazon Kindle Scribe notebooks — the **one-way import source**.
//!
//! Scope (see `docs/NBK_IMPORT.md`):
//! * Reads a notebook directory (`nbk`, optional `actions.log`) into memory and never writes,
//!   renames, locks or truncates anything there. SQLite is parsed in-process (no SQLite
//!   library), so the source is never opened for writing and no journal is ever created.
//! * Decodes only the proven `NBK_MATERIALIZED` state (reading order → page sections →
//!   structural `nmdl.stroke`). The heuristic action-log replay used by the Python viewer is
//!   **not** an import source: a notebook whose action log holds more stroke inserts than its
//!   materialized state is refused ([`NbkError::PendingActions`]) rather than guessed.

pub mod catalog;
pub mod codec;
pub mod container;
pub mod decode;
pub mod ion;
pub mod sqlite;

use std::fs;
use std::path::Path;
use std::time::SystemTime;

pub use decode::{DecodedNotebook, DecodedPage, DecodedStroke, Diagnostics};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NbkError {
    /// Reading the source failed.
    Io(String),
    /// The Kindle has an open write in progress (hot journal / WAL): try again later.
    SourceBusy(String),
    /// A source file changed while it was being read: not a consistent snapshot.
    SourceChanged(String),
    /// The container is not a readable KDF/SQLite image of the expected shape.
    Container(String),
    /// Decodes, but not into a state the importer can faithfully reproduce.
    Unsupported(String),
    /// The action log is ahead of the materialized state (heuristic replay is not imported).
    PendingActions {
        nbk_strokes: usize,
        action_stroke_inserts: usize,
    },
}

impl std::fmt::Display for NbkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "SOURCE_IO: {e}"),
            Self::SourceBusy(e) => write!(f, "SOURCE_BUSY: {e}"),
            Self::SourceChanged(e) => write!(f, "SOURCE_CHANGED_DURING_READ: {e}"),
            Self::Container(e) => write!(f, "NBK_CONTAINER: {e}"),
            Self::Unsupported(e) => write!(f, "NBK_UNSUPPORTED: {e}"),
            Self::PendingActions {
                nbk_strokes,
                action_stroke_inserts,
            } => write!(
                f,
                "NBK_PENDING_ACTIONS: action log has {action_stroke_inserts} stroke inserts, \
                 materialized notebook has {nbk_strokes} strokes"
            ),
        }
    }
}

impl std::error::Error for NbkError {}

/// Bytes of one source notebook, read as a consistent snapshot.
#[derive(Clone, Debug)]
pub struct SourceSnapshot {
    pub nbk: Vec<u8>,
    pub actions_log: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

fn stamp(p: &Path) -> Result<Option<Stamp>, NbkError> {
    match fs::metadata(p) {
        Ok(m) => Ok(Some(Stamp {
            len: m.len(),
            modified: m.modified().ok(),
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(NbkError::Io(format!("{}: {e}", p.display()))),
    }
}

/// Read `dir/nbk` (+ `actions.log`) read-only. Refuses a hot rollback journal / non-empty WAL
/// (the Kindle is mid-write) and any file whose size or mtime changed during the read.
pub fn read_source(dir: &Path) -> Result<SourceSnapshot, NbkError> {
    for side in ["nbk-journal", "nbk-wal"] {
        if let Some(s) = stamp(&dir.join(side))? {
            if s.len > 0 {
                return Err(NbkError::SourceBusy(format!("{side} is not empty")));
            }
        }
    }
    let nbk_path = dir.join("nbk");
    let actions_path = dir.join("actions.log");
    let before = (stamp(&nbk_path)?, stamp(&actions_path)?);
    if before.0.is_none() {
        return Err(NbkError::Unsupported("no nbk file".into()));
    }
    let nbk = fs::read(&nbk_path).map_err(|e| NbkError::Io(format!("read nbk: {e}")))?;
    let actions_log = match fs::read(&actions_path) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(NbkError::Io(format!("read actions.log: {e}"))),
    };
    let after = (stamp(&nbk_path)?, stamp(&actions_path)?);
    if before != after || before.0.map(|s| s.len) != Some(nbk.len() as u64) {
        return Err(NbkError::SourceChanged("nbk or actions.log changed".into()));
    }
    Ok(SourceSnapshot { nbk, actions_log })
}

/// Standard-alphabet base64 (padding optional; other characters are skipped like Python's
/// non-validating `b64decode`).
fn base64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// Number of `InsertStrokeAction` records in an `actions.log` (an upper bound of the strokes
/// the Python replay would decode from it).
pub fn count_action_stroke_inserts(actions_log: &[u8]) -> usize {
    let text = String::from_utf8_lossy(actions_log);
    text.lines()
        .filter(|l| {
            !l.trim().is_empty()
                && !l.starts_with("ActionTableSaveStatus:")
                && !l.starts_with("SyncTableSaveStatus:")
        })
        .filter_map(|l| l.split_once(':').map(|(_, b64)| base64_decode(b64)))
        .filter(|raw| raw.windows(18).any(|w| w == b"InsertStrokeAction"))
        .count()
}

/// Decode a snapshot into the importable current notebook state (see crate docs for the
/// selection rule).
pub fn decode_snapshot(snap: &SourceSnapshot) -> Result<DecodedNotebook, NbkError> {
    let store = container::read_fragments(&snap.nbk)?;
    let nb = decode::decode_store(&store);
    if !nb.is_usable() {
        return Err(NbkError::Unsupported(
            "no main reading order / pages".into(),
        ));
    }
    if let Some(p) = nb.pages.iter().find(|p| p.missing) {
        return Err(NbkError::Unsupported(format!(
            "reading order lists missing page {}",
            p.page_id
        )));
    }
    let inserts = snap
        .actions_log
        .as_deref()
        .map_or(0, count_action_stroke_inserts);
    let strokes = nb.stroke_count();
    if inserts > 0 && strokes < inserts {
        return Err(NbkError::PendingActions {
            nbk_strokes: strokes,
            action_stroke_inserts: inserts,
        });
    }
    Ok(nb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_standard_alphabet_without_padding() {
        assert_eq!(base64_decode("aGVsbG8"), b"hello");
        assert_eq!(base64_decode("aGVsbG8="), b"hello");
    }

    #[test]
    fn counts_only_insert_stroke_records() {
        let insert = "id1:SW5zZXJ0U3Ryb2tlQWN0aW9u"; // "InsertStrokeAction"
        let other = "id2:Q3JlYXRlTm90ZUFjdGlvbg"; // "CreateNoteAction"
        let log = format!("{insert}\n{other}\nActionTableSaveStatus:success\n{insert}\n");
        assert_eq!(count_action_stroke_inserts(log.as_bytes()), 2);
    }
}
