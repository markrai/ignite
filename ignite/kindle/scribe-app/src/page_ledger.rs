//! Durable changed-page ledger, **record-only** (Slice 19B.3C).
//!
//! `<library>/.scribe-sync/page-ledger-v1.jsonl` records, before each canonical write, which
//! local objects the current mutation generation may change. Nothing reads it yet: startup,
//! reconciliation, capture, upload and checkpoint decisions are unchanged.
//!
//! Format (one JSON object per `\n`-terminated line, append-only; never compacted, truncated,
//! rotated or addressed by byte offset in v1):
//!
//! ```text
//! {"v":1,"g":42,"k":"session"}
//! {"v":1,"g":42,"k":"page","nb":"notebook-3","page":"page-7"}
//! {"v":1,"g":42,"k":"notebook","nb":"notebook-3"}
//! {"v":1,"g":42,"k":"structure"}
//! {"v":1,"g":42,"k":"all"}
//! ```
//!
//! Ordering, under the per-library session lock in [`crate::sync_verified`]:
//! 1. durable generation bump to `G` (and stamp invalidation) — unchanged;
//! 2. `session g=G` (+ the first scope) appended and fsynced (new file → directory fsync);
//! 3. each scope not yet covered in generation `G` appended and fsynced;
//! 4. only then may the caller write canonical content.
//!
//! The session record follows the bump, so a generation without a `session` record is always a
//! writer that did not record (older binary, ledger disabled, or a crash between bump and
//! record) — never mistaken for a complete one. Any append/fsync failure refuses the mutation,
//! and the next admission in that generation, of any scope, first appends `\n` (isolating any
//! torn fragment) and `all`: a generation that saw a ledger error is marked fully dirty before
//! any further canonical write. Diagnostics are queued and written after the lock is released.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::sync_local::SYNC_DIR;

/// Ledger filename under `<library>/.scribe-sync/` (not part of the verified-stamp census).
pub const LEDGER_FILE: &str = "page-ledger-v1.jsonl";
pub const LEDGER_VERSION: u32 = 1;
/// Rollback switch for the writer. Disabled, generations are bumped without `session` records,
/// which any reader must treat as untracked (full reconciliation).
pub const PAGE_LEDGER_ENABLED: bool = true;

const NOTEBOOKS: &str = "notebooks";

/// What a mutation may change. Wider scopes cover narrower ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerScope {
    Page {
        nb: String,
        page: String,
    },
    Notebook {
        nb: String,
    },
    /// Library-level files: `library.meta`, folders, identity bindings, migration record.
    Structure,
    /// Unknown or bulk change.
    All,
}

impl LedgerScope {
    pub(crate) fn covers(&self, other: &LedgerScope) -> bool {
        match (self, other) {
            (Self::All, _) => true,
            (Self::Notebook { nb }, Self::Page { nb: n2, .. }) => nb == n2,
            (a, b) => a == b,
        }
    }

    fn record(&self, g: u64) -> LedgerRecordV1 {
        let (k, nb, page) = match self {
            Self::Page { nb, page } => ("page", Some(nb.clone()), Some(page.clone())),
            Self::Notebook { nb } => ("notebook", Some(nb.clone()), None),
            Self::Structure => ("structure", None, None),
            Self::All => ("all", None, None),
        };
        LedgerRecordV1 {
            v: LEDGER_VERSION,
            g,
            k: k.into(),
            nb,
            page,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerRecordV1 {
    pub v: u32,
    pub g: u64,
    pub k: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nb: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
}

impl LedgerRecordV1 {
    fn session(g: u64) -> Self {
        Self {
            v: LEDGER_VERSION,
            g,
            k: "session".into(),
            nb: None,
            page: None,
        }
    }

    fn line(&self) -> Result<String, String> {
        let mut s = serde_json::to_string(self).map_err(|e| e.to_string())?;
        s.push('\n');
        Ok(s)
    }
}

/// Local directory names as the layout writes them (`notebook-N`, `page-N`, import ids).
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Scope of a write into `dir` (a page directory, notebook directory, or library-level
/// directory) of the library at `root`. Anything unrecognised widens to [`LedgerScope::All`].
pub fn scope_for_dir(root: &Path, dir: &Path) -> LedgerScope {
    let Ok(rel) = dir.strip_prefix(root) else {
        return LedgerScope::All;
    };
    let parts: Option<Vec<&str>> = rel
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let Some(parts) = parts else {
        return LedgerScope::All;
    };
    match parts.as_slice() {
        [] => LedgerScope::Structure,
        [d] if *d == SYNC_DIR || *d == crate::folders::FOLDERS_DIR => LedgerScope::Structure,
        [n, nb] if *n == NOTEBOOKS && valid_name(nb) => LedgerScope::Notebook { nb: (*nb).into() },
        [n, nb, page, ..] if *n == NOTEBOOKS && valid_name(nb) && valid_name(page) => {
            LedgerScope::Page {
                nb: (*nb).into(),
                page: (*page).into(),
            }
        }
        [n, nb, ..] if *n == NOTEBOOKS && valid_name(nb) => {
            LedgerScope::Notebook { nb: (*nb).into() }
        }
        _ => LedgerScope::All,
    }
}

/// Per-library, per-generation writer state (lives in the session cell, under its lock).
#[derive(Debug, Default)]
pub(crate) struct SessionLedger {
    /// Generation this process bumped to; `None` until the next bump.
    generation: Option<u64>,
    session_recorded: bool,
    recorded: Vec<LedgerScope>,
    /// The file tail may end mid-line (first append of this generation, or after a failure).
    suspect_tail: bool,
    /// An append or fsync failed in this generation. Until `all` is durable, every admission
    /// (even of an already-recorded scope) must first append `all`. Never cleared within the
    /// generation.
    failed: bool,
    /// Diagnostic line queued under the session lock; emitted by the caller after releasing it.
    diag: Option<String>,
}

impl SessionLedger {
    /// A durable generation bump to `gen` just succeeded.
    pub(crate) fn begin(&mut self, generation: u64) {
        *self = Self {
            generation: Some(generation),
            suspect_tail: true,
            ..Self::default()
        };
    }

    /// The diagnostic queued by the last [`Self::record`], to emit after the lock is released.
    pub(crate) fn take_diag(&mut self) -> Option<String> {
        self.diag.take()
    }

    /// The generation ended (stamp published) or the library was released.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Durably record `scope` for the current generation before its canonical write.
    pub(crate) fn record(&mut self, root: &Path, scope: &LedgerScope) -> Result<(), String> {
        let Some(g) = self.generation else {
            return Err("REFUSE PAGE LEDGER: no generation for this session".into());
        };
        // Failure-state resolution precedes de-duplication: after an uncertain write, no
        // admission (not even of a recorded scope) proceeds until `all` is durable.
        let unresolved = self.failed && !self.recorded.contains(&LedgerScope::All);
        if !unresolved && self.session_recorded && self.recorded.iter().any(|r| r.covers(scope)) {
            return Ok(());
        }
        let mut add: Vec<LedgerScope> = Vec::new();
        if unresolved {
            add.push(LedgerScope::All);
        }
        if !self
            .recorded
            .iter()
            .chain(add.iter())
            .any(|r| r.covers(scope))
        {
            add.push(scope.clone());
        }
        let mut bytes = String::new();
        if !self.session_recorded {
            bytes.push_str(&LedgerRecordV1::session(g).line()?);
        }
        for s in &add {
            bytes.push_str(&s.record(g).line()?);
        }
        let t0 = Instant::now();
        match append_durable(root, bytes.as_bytes(), self.suspect_tail) {
            Ok(out) => {
                let us = t0.elapsed().as_micros() as u64;
                STATS.write_us_total.fetch_add(us, Ordering::Relaxed);
                STATS.write_us_max.fetch_max(us, Ordering::Relaxed);
                STATS.fsyncs.fetch_add(1, Ordering::Relaxed);
                STATS.scopes.fetch_add(add.len() as u64, Ordering::Relaxed);
                if out.created {
                    STATS.dir_fsyncs.fetch_add(1, Ordering::Relaxed);
                }
                if out.torn_prefix {
                    STATS.torn_prefix.fetch_add(1, Ordering::Relaxed);
                }
                // Publish all state first; the diagnostic is only queued (no I/O under the lock).
                self.suspect_tail = false;
                let first = !self.session_recorded;
                self.session_recorded = true;
                let recovered = unresolved;
                self.recorded.extend(add);
                if first {
                    STATS.sessions.fetch_add(1, Ordering::Relaxed);
                }
                if first || recovered {
                    self.diag = Some(format!(
                        "page_ledger {} g={g} write_us={us} new_file={} torn_prefix={} failed_before={}",
                        if first { "session" } else { "recovered" },
                        out.created as u8,
                        out.torn_prefix as u8,
                        self.failed as u8
                    ));
                }
                #[cfg(test)]
                if let Some(f) = crate::sync_verified::test_hooks().after_ledger_durable {
                    f();
                }
                Ok(())
            }
            Err(e) => {
                STATS.failures.fetch_add(1, Ordering::Relaxed);
                let first_failure = !self.failed;
                self.failed = true;
                self.suspect_tail = true;
                if first_failure {
                    // Bounded: once per generation; later failures are only counted.
                    self.diag = Some(format!("page_ledger_failed g={g} detail={e}"));
                }
                Err(format!("REFUSE PAGE LEDGER: {e}"))
            }
        }
    }
}

/// Write a diagnostic line without panicking (a closed or failing stderr is ignored). Call
/// only with no session lock held.
pub(crate) fn emit(line: Option<String>) {
    let Some(line) = line else { return };
    #[cfg(test)]
    if let Some(f) = crate::sync_verified::test_hooks().on_ledger_diag {
        f(&line);
    }
    let _ = writeln!(std::io::stderr(), "{line}");
}

struct AppendOutcome {
    created: bool,
    torn_prefix: bool,
}

fn fsync_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| format!("fsync dir {}: {e}", dir.display()))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Append `bytes` and make them durable. A suspect tail that does not end in `\n` is isolated
/// with a leading `\n` (the fragment is kept, never truncated).
fn append_durable(root: &Path, bytes: &[u8], suspect_tail: bool) -> Result<AppendOutcome, String> {
    #[cfg(test)]
    let h = crate::sync_verified::test_hooks();
    let dir = root.join(SYNC_DIR);
    let path = dir.join(LEDGER_FILE);
    let existed = match fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_file() => true,
        Ok(_) => return Err(format!("{} is not a regular file", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => return Err(format!("inspect {}: {e}", path.display())),
    };
    #[cfg(test)]
    if h.fail_ledger_create && !existed {
        return Err(format!("injected: create {}", path.display()));
    }
    let mut f = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut torn_prefix = false;
    if suspect_tail && existed {
        let len = f
            .metadata()
            .map_err(|e| format!("stat {}: {e}", path.display()))?
            .len();
        if len > 0 {
            let mut last = [0u8; 1];
            f.seek(SeekFrom::End(-1))
                .and_then(|_| f.read_exact(&mut last))
                .map_err(|e| format!("read tail {}: {e}", path.display()))?;
            torn_prefix = last[0] != b'\n';
        }
    }
    let mut buf = Vec::with_capacity(bytes.len() + 1);
    if torn_prefix {
        buf.push(b'\n');
    }
    buf.extend_from_slice(bytes);
    #[cfg(test)]
    if let Some(n) = h.torn_ledger_append {
        let _ = f.write_all(&buf[..n.min(buf.len())]);
        return Err(format!("injected: torn append to {}", path.display()));
    }
    f.write_all(&buf)
        .map_err(|e| format!("append {}: {e}", path.display()))?;
    #[cfg(test)]
    if h.fail_ledger_fsync {
        return Err(format!("injected: fsync {}", path.display()));
    }
    f.sync_all()
        .map_err(|e| format!("fsync {}: {e}", path.display()))?;
    if !existed {
        #[cfg(test)]
        if h.fail_ledger_dir_sync {
            return Err(format!("injected: dir fsync {}", dir.display()));
        }
        fsync_dir(&dir)?;
    }
    Ok(AppendOutcome {
        created: !existed,
        torn_prefix,
    })
}

struct Stats {
    sessions: AtomicU64,
    scopes: AtomicU64,
    fsyncs: AtomicU64,
    dir_fsyncs: AtomicU64,
    write_us_total: AtomicU64,
    write_us_max: AtomicU64,
    failures: AtomicU64,
    torn_prefix: AtomicU64,
}

static STATS: Stats = Stats {
    sessions: AtomicU64::new(0),
    scopes: AtomicU64::new(0),
    fsyncs: AtomicU64::new(0),
    dir_fsyncs: AtomicU64::new(0),
    write_us_total: AtomicU64::new(0),
    write_us_max: AtomicU64::new(0),
    failures: AtomicU64::new(0),
    torn_prefix: AtomicU64::new(0),
};

/// Process totals (counts and timings only; no names, titles or paths).
pub fn stats_line() -> String {
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    format!(
        "page_ledger stats enabled={} sessions={} scopes={} fsyncs={} dir_fsyncs={} write_us_total={} write_us_max={} failures={} torn_prefix={}",
        PAGE_LEDGER_ENABLED as u8,
        l(&STATS.sessions),
        l(&STATS.scopes),
        l(&STATS.fsyncs),
        l(&STATS.dir_fsyncs),
        l(&STATS.write_us_total),
        l(&STATS.write_us_max),
        l(&STATS.failures),
        l(&STATS.torn_prefix),
    )
}

/// Strict parse of one complete ledger line (tests and the future 19B.3D reader).
pub fn parse_line(line: &str) -> Result<LedgerRecordV1, String> {
    let r: LedgerRecordV1 = serde_json::from_str(line).map_err(|e| e.to_string())?;
    if r.v != LEDGER_VERSION {
        return Err(format!("unsupported ledger version {}", r.v));
    }
    let field_ok = |v: &Option<String>, want: bool| match v.as_deref() {
        Some(s) => want && valid_name(s),
        None => !want,
    };
    let names_ok = |nb: bool, page: bool| field_ok(&r.nb, nb) && field_ok(&r.page, page);
    let ok = match r.k.as_str() {
        "session" | "structure" | "all" => names_ok(false, false),
        "notebook" => names_ok(true, false),
        "page" => names_ok(true, true),
        _ => false,
    };
    if ok {
        Ok(r)
    } else {
        Err(format!("invalid ledger record {line:?}"))
    }
}

pub fn ledger_path(root: &Path) -> std::path::PathBuf {
    root.join(SYNC_DIR).join(LEDGER_FILE)
}
