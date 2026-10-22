//! Verified local baseline and shadow ledger validation (Slice 19B.3D). **Diagnostic only.**
//!
//! After a successful full reconcile writes the v3 stamp, the exact census that stamp captured
//! (plus the library layout) is published as `<sync>/local-verified-baseline-v1.json`. On the next
//! full reconcile, before the new baseline replaces it, the shadow check answers two questions
//! independently and logs one `ledger_shadow` line:
//!
//! * **A — claimed:** the page ledger's scopes for every generation in `(baseline, current]`,
//!   read strictly ([`crate::page_ledger::read_provenance`]); any doubt makes the check ineligible.
//! * **B — actual:** the per-file difference between the baseline census and the census the new
//!   stamp just captured (content SHA-256 for metadata, size for append-only journals and
//!   backgrounds — the fast path's trust model). Never derived from the ledger or the snapshots.
//!
//! Every actual change must be covered by a recorded scope; misses and over-coverage are counted.
//! Nothing here influences reconciliation, the stamp, capture, upload or checkpoints, and every
//! failure is only logged. The baseline holds local ids, sizes and hashes — no notebook content,
//! titles or credentials.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sync_protocol::{LibraryId, ReplicaId};

use crate::page_ledger::{read_provenance, LedgerScope, Provenance};
use crate::sync_verified::{census_map_sha256, CensusMap, StampWritten};

pub const BASELINE_FILE: &str = "local-verified-baseline-v1.json";
pub const BASELINE_VERSION: u32 = 1;
/// Larger baselines are refused on read.
pub const BASELINE_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Canonical membership: notebooks in library order, each with its page order (local ids).
pub type Layout = Vec<(String, Vec<String>)>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedBaselineV1 {
    pub v: u32,
    pub library_id: LibraryId,
    pub replica_id: ReplicaId,
    /// Generation the stamp recorded (the census below is the library at this generation).
    pub mutation_gen: u64,
    pub server_cursor: u64,
    pub acked_client_seq: u64,
    /// The stamp's census hash; must equal the hash of `census` (self-binding).
    pub stamp_census_sha256: String,
    pub layout: Layout,
    pub census: CensusMap,
}

pub fn baseline_path(sync_dir: &Path) -> PathBuf {
    sync_dir.join(BASELINE_FILE)
}

fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_rel(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && Path::new(p)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

fn valid_fp(fp: &str) -> bool {
    fp == "-" || (fp.len() == 64 && fp.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Why a baseline cannot be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaselineReject {
    Missing,
    Unreadable,
    TooLarge,
    Corrupt,
    Unsupported,
    /// Library or replica differ from the current sync state (restore, adoption, re-pair).
    OtherIdentity,
}

impl BaselineReject {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Missing => "baseline_missing",
            Self::Unreadable => "baseline_unreadable",
            Self::TooLarge => "baseline_too_large",
            Self::Corrupt => "baseline_corrupt",
            Self::Unsupported => "baseline_unsupported",
            Self::OtherIdentity => "baseline_other_identity",
        }
    }
}

/// Strict read: version, self-binding hash, names, fingerprints and identity must all check.
pub fn read_baseline(
    sync_dir: &Path,
    library_id: LibraryId,
    replica_id: ReplicaId,
) -> Result<VerifiedBaselineV1, BaselineReject> {
    let path = baseline_path(sync_dir);
    let raw = match fs::metadata(&path) {
        Ok(m) if !m.is_file() => return Err(BaselineReject::Unreadable),
        Ok(m) if m.len() > BASELINE_MAX_BYTES => return Err(BaselineReject::TooLarge),
        Ok(_) => fs::read(&path).map_err(|_| BaselineReject::Unreadable)?,
        Err(e) if e.kind() == ErrorKind::NotFound => return Err(BaselineReject::Missing),
        Err(_) => return Err(BaselineReject::Unreadable),
    };
    let probe: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|_| BaselineReject::Corrupt)?;
    if probe.get("v").and_then(|v| v.as_u64()) != Some(u64::from(BASELINE_VERSION)) {
        return Err(BaselineReject::Unsupported);
    }
    let b: VerifiedBaselineV1 =
        serde_json::from_value(probe).map_err(|_| BaselineReject::Corrupt)?;
    let names_ok = b
        .layout
        .iter()
        .all(|(nb, pages)| valid_id(nb) && pages.iter().all(|p| valid_id(p)));
    let census_ok = b
        .census
        .iter()
        .all(|(p, (_, fp))| valid_rel(p) && valid_fp(fp));
    if !names_ok || !census_ok || census_map_sha256(&b.census) != b.stamp_census_sha256 {
        return Err(BaselineReject::Corrupt);
    }
    if b.library_id != library_id || b.replica_id != replica_id {
        return Err(BaselineReject::OtherIdentity);
    }
    Ok(b)
}

fn fsync_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| format!("fsync dir {}: {e}", dir.display()))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Atomic, durable publication: temp → write → fsync → rename → directory fsync. An interrupted
/// publication leaves the previous baseline (or a stray temp file, never read).
pub fn write_baseline(sync_dir: &Path, b: &VerifiedBaselineV1) -> Result<u64, String> {
    #[cfg(test)]
    let h = crate::sync_verified::test_hooks();
    let mut bytes = serde_json::to_vec(b).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let dest = baseline_path(sync_dir);
    let tmp = sync_dir.join(format!("{BASELINE_FILE}.tmp"));
    {
        let mut f = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        #[cfg(test)]
        if let Some(n) = h.torn_baseline_write {
            let _ = f.write_all(&bytes[..n.min(bytes.len())]);
            return Err("injected: baseline write interrupted".into());
        }
        f.write_all(&bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        #[cfg(test)]
        if h.fail_baseline_fsync {
            return Err("injected: baseline fsync failed".into());
        }
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    #[cfg(test)]
    if h.fail_baseline_rename {
        return Err("injected: baseline rename failed".into());
    }
    #[cfg(windows)]
    if dest.exists() {
        fs::remove_file(&dest).map_err(|e| format!("remove {}: {e}", dest.display()))?;
    }
    fs::rename(&tmp, &dest).map_err(|e| format!("rename {}: {e}", dest.display()))?;
    fsync_dir(sync_dir)?;
    Ok(bytes.len() as u64)
}

// ---------------------------------------------------------------- shadow comparison

/// One actual canonical change, classified by where it lives.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActualChange {
    /// A file inside `notebooks/<nb>/<page>/`.
    Page { nb: String, page: String },
    /// A notebook-level file (`notebook.meta`, `notebook.info`, import marker).
    NotebookMeta { nb: String },
    /// `library.meta`, folders, identity bindings, migration record.
    Structure,
    /// A census file anywhere else.
    Unknown,
}

pub fn classify(rel: &str) -> ActualChange {
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.as_slice() {
        ["notebooks", nb, page, _file] if valid_id(nb) && valid_id(page) => ActualChange::Page {
            nb: (*nb).into(),
            page: (*page).into(),
        },
        ["notebooks", nb, _file] if valid_id(nb) => ActualChange::NotebookMeta { nb: (*nb).into() },
        ["library.meta"]
        | [".scribe-library-migration-v1"]
        | [".scribe-folders", "folders.json"]
        | [".scribe-sync", "identity.json"] => ActualChange::Structure,
        _ => ActualChange::Unknown,
    }
}

/// Per-file differences between two censuses (added, removed or changed entries).
pub fn census_diff(before: &CensusMap, after: &CensusMap) -> Vec<String> {
    let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    keys.into_iter()
        .filter(|k| before.get(*k) != after.get(*k))
        .cloned()
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    pub changed_files: usize,
    /// Distinct actual changes, by class.
    pub actual_pages: usize,
    pub actual_notebook_meta: usize,
    pub actual_structure: usize,
    pub actual_unknown: usize,
    /// How each actual change was covered.
    pub exact: usize,
    pub by_notebook: usize,
    pub by_all: usize,
    pub misses: usize,
    /// Notebook-level changes with no page change in that notebook (e.g. a page switch).
    pub metadata_only_notebooks: usize,
    /// Pages the ledger claims may have changed, and how many of them did not.
    pub predicted_pages: usize,
    pub overcovered_pages: usize,
    pub pages_examined: usize,
    /// Misses by class (privacy-safe: counts only).
    pub miss_pages: usize,
    pub miss_notebook_meta: usize,
    pub miss_structure: usize,
    pub miss_unknown: usize,
}

fn all_pages(layout: &Layout) -> BTreeSet<(String, String)> {
    layout
        .iter()
        .flat_map(|(nb, pages)| pages.iter().map(move |p| (nb.clone(), p.clone())))
        .collect()
}

/// Question B against question A: every actual change must be covered by a recorded scope.
pub fn compare(
    baseline: &VerifiedBaselineV1,
    current: &CensusMap,
    current_layout: &Layout,
    claimed: &Provenance,
) -> Coverage {
    let changed = census_diff(&baseline.census, current);
    let actual: BTreeSet<ActualChange> = changed.iter().map(|p| classify(p)).collect();
    let mut cov = Coverage {
        changed_files: changed.len(),
        ..Default::default()
    };
    let universe: BTreeSet<(String, String)> = all_pages(&baseline.layout)
        .union(&all_pages(current_layout))
        .cloned()
        .collect();
    cov.pages_examined = universe.len();
    let has = |s: &LedgerScope| claimed.scopes.iter().any(|c| c == s);
    let all = claimed.has_all;
    let changed_page_nbs: BTreeSet<&String> = actual
        .iter()
        .filter_map(|a| match a {
            ActualChange::Page { nb, .. } => Some(nb),
            _ => None,
        })
        .collect();
    for a in &actual {
        let (exact, notebook) = match a {
            ActualChange::Page { nb, page } => {
                cov.actual_pages += 1;
                (
                    has(&LedgerScope::Page {
                        nb: nb.clone(),
                        page: page.clone(),
                    }),
                    has(&LedgerScope::Notebook { nb: nb.clone() }),
                )
            }
            ActualChange::NotebookMeta { nb } => {
                cov.actual_notebook_meta += 1;
                if !changed_page_nbs.contains(nb) {
                    cov.metadata_only_notebooks += 1;
                }
                (false, has(&LedgerScope::Notebook { nb: nb.clone() }))
            }
            ActualChange::Structure => {
                cov.actual_structure += 1;
                (has(&LedgerScope::Structure), false)
            }
            ActualChange::Unknown => {
                cov.actual_unknown += 1;
                (false, false)
            }
        };
        if exact {
            cov.exact += 1;
        } else if notebook {
            cov.by_notebook += 1;
        } else if all {
            cov.by_all += 1;
        } else {
            cov.misses += 1;
            match a {
                ActualChange::Page { .. } => cov.miss_pages += 1,
                ActualChange::NotebookMeta { .. } => cov.miss_notebook_meta += 1,
                ActualChange::Structure => cov.miss_structure += 1,
                ActualChange::Unknown => cov.miss_unknown += 1,
            }
        }
    }
    // Predicted page set: page scopes, every page of a notebook scope, or everything for `all`.
    let predicted: BTreeSet<(String, String)> = if all {
        universe.clone()
    } else {
        claimed
            .scopes
            .iter()
            .flat_map(|s| match s {
                LedgerScope::Page { nb, page } => vec![(nb.clone(), page.clone())],
                LedgerScope::Notebook { nb } => {
                    universe.iter().filter(|(n, _)| n == nb).cloned().collect()
                }
                _ => Vec::new(),
            })
            .collect()
    };
    let actual_pages: BTreeSet<(String, String)> = actual
        .iter()
        .filter_map(|a| match a {
            ActualChange::Page { nb, page } => Some((nb.clone(), page.clone())),
            _ => None,
        })
        .collect();
    cov.predicted_pages = predicted.len();
    cov.overcovered_pages = predicted.difference(&actual_pages).count();
    cov
}

/// The shadow result for one full reconcile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shadow {
    /// The check could not run (reason); never a statement about the ledger's coverage.
    Ineligible(&'static str),
    /// Ran: zero misses means complete.
    Checked(Coverage),
}

/// Run the shadow check of the previous baseline against the census a new stamp just captured.
pub fn shadow_check(
    sync_dir: &Path,
    library_root: &Path,
    written: &StampWritten,
    current_layout: &Layout,
    timing: &mut ShadowTiming,
) -> Shadow {
    let t = Instant::now();
    let baseline = match read_baseline(sync_dir, written.stamp.library_id, written.stamp.replica_id)
    {
        Ok(b) => b,
        Err(e) => return Shadow::Ineligible(e.as_str()),
    };
    timing.baseline_read_us = t.elapsed().as_micros() as u64;
    timing.baseline_gen = Some(baseline.mutation_gen);
    let t = Instant::now();
    let claimed = read_provenance(
        library_root,
        baseline.mutation_gen,
        written.stamp.mutation_gen,
    );
    timing.ledger_parse_us = t.elapsed().as_micros() as u64;
    let claimed = match claimed {
        Ok(p) => p,
        Err(e) => return Shadow::Ineligible(e.as_str()),
    };
    timing.ledger_bytes = claimed.bytes;
    timing.sessions = claimed.sessions.len();
    timing.scopes = claimed.scopes.len();
    let t = Instant::now();
    let cov = compare(&baseline, &written.census, current_layout, &claimed);
    timing.compare_us = t.elapsed().as_micros() as u64;
    Shadow::Checked(cov)
}

#[derive(Clone, Debug, Default)]
pub struct ShadowTiming {
    pub baseline_gen: Option<u64>,
    pub baseline_read_us: u64,
    pub ledger_parse_us: u64,
    pub ledger_bytes: u64,
    pub sessions: usize,
    pub scopes: usize,
    pub compare_us: u64,
}

#[cfg(test)]
static LAST_LINES: std::sync::Mutex<Option<std::collections::HashMap<PathBuf, Vec<String>>>> =
    std::sync::Mutex::new(None);

/// The lines the last [`after_verified_reconcile`] produced for `library_root` (tests).
#[cfg(test)]
pub(crate) fn last_lines_for_test(library_root: &Path) -> Vec<String> {
    LAST_LINES
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(library_root).cloned())
        .unwrap_or_default()
}

/// Called once after a full reconcile wrote the stamp. `admissions_before` is
/// [`crate::sync_verified::admission_count`] taken before the stamp's census: if any mutation
/// was admitted since, the capture is not a consistent point and nothing is checked or published.
/// Returns the log lines (the caller logs them); never fails the reconcile.
pub fn after_verified_reconcile(
    library_root: &Path,
    sync_dir: &Path,
    written: &StampWritten,
    admissions_before: u64,
) -> Vec<String> {
    let out = shadow_and_publish(library_root, sync_dir, written, admissions_before);
    #[cfg(test)]
    LAST_LINES
        .lock()
        .unwrap()
        .get_or_insert_with(Default::default)
        .insert(library_root.to_path_buf(), out.clone());
    out
}

fn shadow_and_publish(
    library_root: &Path,
    sync_dir: &Path,
    written: &StampWritten,
    admissions_before: u64,
) -> Vec<String> {
    let t0 = Instant::now();
    let mut out = Vec::new();
    #[cfg(test)]
    if let Some(f) = crate::sync_verified::test_hooks().before_baseline_publish {
        f();
    }
    let layout: Result<Layout, String> = crate::sync_local::live_layout(library_root)
        .map(|(_, l)| l.notebooks.into_iter().collect());
    if crate::sync_verified::admission_count(library_root) != admissions_before {
        out.push("ledger_shadow result=ineligible reason=concurrent_mutation".into());
        out.push("local_baseline skipped reason=concurrent_mutation".into());
        return out;
    }
    let layout = match layout {
        Ok(l) => l,
        Err(_) => {
            out.push("ledger_shadow result=ineligible reason=layout_unreadable".into());
            out.push("local_baseline skipped reason=layout_unreadable".into());
            return out;
        }
    };
    let mut timing = ShadowTiming::default();
    let shadow = shadow_check(sync_dir, library_root, written, &layout, &mut timing);
    let base = format!(
        "baseline_g={} current_g={} sessions={} scopes={} ledger_bytes={} baseline_read_us={} ledger_parse_us={} compare_us={}",
        timing.baseline_gen.map_or("-".into(), |g| g.to_string()),
        written.stamp.mutation_gen,
        timing.sessions,
        timing.scopes,
        timing.ledger_bytes,
        timing.baseline_read_us,
        timing.ledger_parse_us,
        timing.compare_us
    );
    out.push(match &shadow {
        Shadow::Ineligible(r) => format!("ledger_shadow result=ineligible reason={r} {base}"),
        Shadow::Checked(c) => format!(
            "ledger_shadow result={} {base} changed_files={} pages_examined={} actual_pages={} actual_notebook_meta={} actual_structure={} actual_unknown={} predicted_pages={} exact={} by_notebook={} by_all={} misses={} miss_pages={} miss_notebook_meta={} miss_structure={} miss_unknown={} overcovered_pages={} metadata_only_notebooks={}",
            if c.misses == 0 { "complete" } else { "miss" },
            c.changed_files,
            c.pages_examined,
            c.actual_pages,
            c.actual_notebook_meta,
            c.actual_structure,
            c.actual_unknown,
            c.predicted_pages,
            c.exact,
            c.by_notebook,
            c.by_all,
            c.misses,
            c.miss_pages,
            c.miss_notebook_meta,
            c.miss_structure,
            c.miss_unknown,
            c.overcovered_pages,
            c.metadata_only_notebooks
        ),
    });
    let b = VerifiedBaselineV1 {
        v: BASELINE_VERSION,
        library_id: written.stamp.library_id,
        replica_id: written.stamp.replica_id,
        mutation_gen: written.stamp.mutation_gen,
        server_cursor: written.stamp.server_cursor,
        acked_client_seq: written.stamp.acked_client_seq,
        stamp_census_sha256: written.stamp.census_sha256.clone(),
        layout,
        census: written.census.clone(),
    };
    let t = Instant::now();
    out.push(match write_baseline(sync_dir, &b) {
        Ok(bytes) => format!(
            "local_baseline written g={} files={} bytes={bytes} write_us={} total_us={}",
            b.mutation_gen,
            b.census.len(),
            t.elapsed().as_micros(),
            t0.elapsed().as_micros()
        ),
        Err(e) => format!("local_baseline_write_failed detail={e}"),
    });
    out
}
