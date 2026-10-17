//! Durable server ledger (SQLite) — sequencing authority, not semantic owner.
//!
//! Writers: one long-lived SQLite connection behind an in-process `Mutex`, and every mutating
//! call is one `BEGIN IMMEDIATE` transaction on it. Reading `head_server_seq`, assigning
//! `head + 1`, applying the reducer and committing are therefore strictly serialized. A
//! transaction that fails before `COMMIT` rolls back completely — ledger, head, replica
//! sequence and current snapshot are unchanged and no sequence number is consumed.
//!
//! Why a mutex rather than SQLite's busy handler alone: contending writers in the busy handler
//! sleep with growing back-off (up to 100 ms per poll), so a burst of fast-failing requests can
//! keep re-taking the lock and starve the one writer that could make progress (observed on
//! Linux as all workers parked in `clock_nanosleep`). Parked mutex waiters are woken on
//! release instead, and requests that are provably ahead of their replica's sequence are
//! refused by a lock-free read precheck before they contend at all.
//!
//! One process per ledger: `Store::open` takes an exclusive `ledger.lock` (`File::try_lock`,
//! released by the kernel when the process exits), so a second server on the same data
//! directory is refused. Readers use short-lived WAL read connections and never block writers.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sync_engine::{HistoryV1, LibraryState, Outcome, SemanticError, SemanticErrorCode};
use sync_protocol::{
    check_client_seq, from_json, sha256_hex, to_canonical_json, BackgroundRef, LibraryId,
    Sha256Hex, SyncEnvelopeV1, SyncOperationV1, SyncSnapshotV1, ENVELOPE_VERSION, SNAPSHOT_VERSION,
};

/// v2: `client_seq` / `last_client_seq` stored as 20-digit decimal TEXT (full u64 range).
pub const SCHEMA_VERSION: i64 = 2;
pub const DB_FILE: &str = "ledger.sqlite3";
pub const BLOB_DIR: &str = "blobs";
/// Held exclusively for the lifetime of a [`Store`] (one server process per ledger).
pub const LOCK_FILE: &str = "ledger.lock";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS schema_meta (
    key   TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS libraries (
    library_id       TEXT PRIMARY KEY,
    genesis_bytes    BLOB NOT NULL,
    genesis_sha256   TEXT NOT NULL,
    head_server_seq  INTEGER NOT NULL CHECK (head_server_seq >= 0)
);
CREATE TABLE IF NOT EXISTS replicas (
    library_id       TEXT NOT NULL REFERENCES libraries(library_id),
    replica_id       TEXT NOT NULL,
    last_client_seq  TEXT NOT NULL CHECK (
        length(last_client_seq) = 20 AND last_client_seq NOT GLOB '*[^0-9]*'
        AND last_client_seq <> '00000000000000000000'),
    PRIMARY KEY (library_id, replica_id)
);
CREATE TABLE IF NOT EXISTS operations (
    library_id       TEXT NOT NULL REFERENCES libraries(library_id),
    server_seq       INTEGER NOT NULL CHECK (server_seq >= 1),
    operation_id     TEXT NOT NULL UNIQUE,
    replica_id       TEXT NOT NULL,
    client_seq       TEXT NOT NULL CHECK (
        length(client_seq) = 20 AND client_seq NOT GLOB '*[^0-9]*'
        AND client_seq <> '00000000000000000000'),
    envelope_bytes   BLOB NOT NULL,
    envelope_sha256  TEXT NOT NULL,
    outcome          TEXT NOT NULL CHECK (outcome IN ('applied', 'noop')),
    PRIMARY KEY (library_id, server_seq),
    UNIQUE (library_id, replica_id, client_seq)
);
CREATE TABLE IF NOT EXISTS current_state (
    library_id          TEXT PRIMARY KEY REFERENCES libraries(library_id),
    through_server_seq  INTEGER NOT NULL CHECK (through_server_seq >= 0),
    snapshot_bytes      BLOB NOT NULL,
    snapshot_sha256     TEXT NOT NULL,
    history_bytes       BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS blobs (
    sha256    TEXT PRIMARY KEY,
    byte_len  INTEGER NOT NULL CHECK (byte_len >= 0),
    rel_path  TEXT NOT NULL
);
"#;

/// Transport-independent server failure. `Semantic` is the reducer's refusal; everything else
/// is a protocol/sequencing/storage condition.
#[derive(Debug)]
pub enum StoreError {
    BadRequest { code: &'static str, message: String },
    NotFound { code: &'static str, message: String },
    Conflict { code: &'static str, message: String },
    Semantic { index: usize, error: SemanticError },
    MissingBlobs(Vec<String>),
    Internal(String),
}

impl StoreError {
    fn bad(code: &'static str, m: impl std::fmt::Display) -> Self {
        Self::BadRequest {
            code,
            message: m.to_string(),
        }
    }
    fn conflict(code: &'static str, m: impl std::fmt::Display) -> Self {
        Self::Conflict {
            code,
            message: m.to_string(),
        }
    }
    fn not_found(code: &'static str, m: impl std::fmt::Display) -> Self {
        Self::NotFound {
            code,
            message: m.to_string(),
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest { code, message }
            | Self::NotFound { code, message }
            | Self::Conflict { code, message } => write!(f, "{code}: {message}"),
            Self::Semantic { index, error } => write!(f, "envelope {index}: {error}"),
            Self::MissingBlobs(v) => write!(f, "missing blobs: {v:?}"),
            Self::Internal(m) => write!(f, "internal: {m}"),
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Internal(format!("sqlite: {e}"))
    }
}

type R<T> = Result<T, StoreError>;

/// Failure injection for durability tests (never enabled by the binary).
#[derive(Debug, Default)]
pub struct FaultHooks {
    /// Fail an accepted submission after all writes but before `COMMIT`.
    pub fail_submit_before_commit: AtomicBool,
    /// After a successful `COMMIT`, HTTP still returns an error (lost ACK). The ledger has the row.
    pub fail_drop_response_after_commit: AtomicBool,
}

/// One row of `GET /v1/libraries` (no invented library title: the model has none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibrarySummary {
    pub library_id: LibraryId,
    pub head_server_seq: i64,
    pub snapshot_sha256: String,
    pub notebook_count: usize,
    pub page_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryInfo {
    pub library_id: LibraryId,
    pub genesis_sha256: String,
    pub head_server_seq: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    pub operation_id: String,
    pub server_seq: i64,
    pub outcome: String,
    /// True when this exact envelope had already been accepted (retry after a lost ACK).
    pub duplicate: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TailEntry {
    pub server_seq: i64,
    pub envelope_bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentSnapshot {
    pub through_server_seq: i64,
    pub snapshot_bytes: Vec<u8>,
    pub snapshot_sha256: String,
}

pub struct Store {
    db_path: PathBuf,
    blob_dir: PathBuf,
    /// The only writer connection; every mutation is serialized through it.
    writer: Mutex<Connection>,
    /// Exclusive process lock on `ledger.lock` (released on drop / process exit).
    _process_lock: File,
    pub hooks: FaultHooks,
    /// Operator/test gate. Default **false**: new [`SyncOperationV1::ReplaceStrokeLineages`]
    /// submits are refused (`unsupported_operation`) so installing this binary cannot add
    /// history that older clients cannot decode. Already-accepted envelopes still retry as
    /// duplicates. Once any such operation is in the ledger, rolling the binary back is unsafe.
    pub accept_replace_stroke_lineages: AtomicBool,
}

/// `client_seq` storage: SQLite INTEGER is signed i64, but the S1 contract is the full u64
/// range. Values are stored as exactly 20 zero-padded decimal digits (`u64::MAX` has 20), which
/// is lossless, order-preserving under text comparison, and CHECK-constrained in the schema.
pub fn seq_to_text(v: u64) -> String {
    format!("{v:020}")
}

/// Strict inverse of [`seq_to_text`]; any other text is ledger corruption.
pub fn seq_from_text(s: &str) -> Result<u64, StoreError> {
    if s.len() != 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(StoreError::Internal(format!(
            "corrupt stored client_seq {s:?}"
        )));
    }
    match s.parse::<u64>() {
        Ok(v) if v >= 1 => Ok(v),
        _ => Err(StoreError::Internal(format!(
            "corrupt stored client_seq {s:?}"
        ))),
    }
}

fn referenced_blobs(snapshot: &SyncSnapshotV1) -> Vec<(String, u64)> {
    snapshot
        .notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .filter_map(|p| match &p.background {
            BackgroundRef::Blob {
                sha256, byte_len, ..
            } => Some((sha256.as_str().to_string(), *byte_len)),
            BackgroundRef::Blank => None,
        })
        .collect()
}

fn op_blob(op: &SyncOperationV1) -> Option<(String, u64)> {
    let bg = match op {
        SyncOperationV1::CreateNotebook { first_page, .. } => &first_page.background,
        SyncOperationV1::CreatePage { page, .. } => &page.background,
        _ => return None,
    };
    match bg {
        BackgroundRef::Blob {
            sha256, byte_len, ..
        } => Some((sha256.as_str().to_string(), *byte_len)),
        BackgroundRef::Blank => None,
    }
}

impl Store {
    /// Open (or create) the ledger under `data_dir`.
    pub fn open(data_dir: &Path) -> R<Self> {
        fs::create_dir_all(data_dir.join(BLOB_DIR))
            .map_err(|e| StoreError::Internal(format!("create {}: {e}", data_dir.display())))?;
        let lock_path = data_dir.join(LOCK_FILE);
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| StoreError::Internal(format!("open {}: {e}", lock_path.display())))?;
        lock.try_lock().map_err(|e| {
            StoreError::Internal(format!(
                "ledger {} is in use by another server process ({e})",
                data_dir.display()
            ))
        })?;
        let db_path = data_dir.join(DB_FILE);
        let writer = open_conn(&db_path)?;
        writer.pragma_update(None, "journal_mode", "WAL")?;
        let store = Self {
            db_path,
            blob_dir: data_dir.join(BLOB_DIR),
            writer: Mutex::new(writer),
            _process_lock: lock,
            hooks: FaultHooks::default(),
            accept_replace_stroke_lineages: AtomicBool::new(false),
        };
        let mut conn = store.writer();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(SCHEMA)?;
        let v: Option<i64> = tx
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match v {
            None => {
                tx.execute(
                    "INSERT INTO schema_meta (key, value) VALUES ('schema_version', ?1)",
                    [SCHEMA_VERSION],
                )?;
            }
            Some(SCHEMA_VERSION) => {}
            Some(other) => {
                return Err(StoreError::Internal(format!(
                    "unsupported ledger schema version {other}"
                )))
            }
        }
        tx.commit()?;
        drop(conn);
        Ok(store)
    }

    /// The serialized writer. A poisoned mutex is recovered: a panic can only have happened
    /// inside a transaction, which rusqlite rolled back when it was dropped.
    fn writer(&self) -> MutexGuard<'_, Connection> {
        self.writer.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Short-lived read connection (WAL readers never block the writer).
    fn conn(&self) -> R<Connection> {
        open_conn(&self.db_path)
    }

    // ------------------------------------------------------------------ genesis

    /// Store a library's genesis snapshot (canonical bytes required). Idempotent for identical
    /// bytes; a different genesis for an existing library is a conflict, never a replacement.
    /// Returns `(info, created)`.
    pub fn bootstrap(&self, raw: &[u8]) -> R<(LibraryInfo, bool)> {
        let snap: SyncSnapshotV1 =
            from_json(raw).map_err(|e| StoreError::bad("invalid_snapshot", e))?;
        if snap.v != SNAPSHOT_VERSION {
            return Err(StoreError::bad(
                "unsupported_version",
                format!("snapshot version {}", snap.v),
            ));
        }
        snap.validate()
            .map_err(|e| StoreError::bad("invalid_snapshot", e))?;
        let canonical =
            to_canonical_json(&snap).map_err(|e| StoreError::Internal(e.to_string()))?;
        if canonical != raw {
            return Err(StoreError::bad(
                "non_canonical",
                "genesis must be the canonical SyncSnapshotV1 bytes",
            ));
        }
        let state = LibraryState::genesis(snap.clone())
            .map_err(|e| StoreError::bad("invalid_snapshot", e))?;
        let history = state
            .history
            .to_canonical_bytes()
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let sha = sha256_hex(raw).to_string();
        let lib = snap.library_id.to_string();

        let mut conn = self.writer();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(Vec<u8>, i64)> = tx
            .query_row(
                "SELECT genesis_bytes, head_server_seq FROM libraries WHERE library_id = ?1",
                [&lib],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((bytes, head)) = existing {
            if bytes != raw {
                return Err(StoreError::conflict(
                    "genesis_conflict",
                    format!("library {lib} already has a different genesis"),
                ));
            }
            return Ok((
                LibraryInfo {
                    library_id: snap.library_id,
                    genesis_sha256: sha,
                    head_server_seq: head,
                },
                false,
            ));
        }
        let missing = missing_blobs(&tx, referenced_blobs(&snap))?;
        if !missing.is_empty() {
            return Err(StoreError::MissingBlobs(missing));
        }
        tx.execute(
            "INSERT INTO libraries (library_id, genesis_bytes, genesis_sha256, head_server_seq)
             VALUES (?1, ?2, ?3, 0)",
            params![lib, raw, sha],
        )?;
        tx.execute(
            "INSERT INTO current_state
             (library_id, through_server_seq, snapshot_bytes, snapshot_sha256, history_bytes)
             VALUES (?1, 0, ?2, ?3, ?4)",
            params![lib, raw, sha, history],
        )?;
        tx.commit()?;
        Ok((
            LibraryInfo {
                library_id: snap.library_id,
                genesis_sha256: sha,
                head_server_seq: 0,
            },
            true,
        ))
    }

    pub fn library(&self, library_id: &str) -> R<LibraryInfo> {
        let lib = parse_library(library_id)?;
        let conn = self.conn()?;
        let row: Option<(String, i64)> = conn
            .query_row(
                "SELECT genesis_sha256, head_server_seq FROM libraries WHERE library_id = ?1",
                [lib.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (genesis_sha256, head_server_seq) =
            row.ok_or_else(|| StoreError::not_found("unknown_library", library_id))?;
        Ok(LibraryInfo {
            library_id: lib,
            genesis_sha256,
            head_server_seq,
        })
    }

    /// Every library, ordered by `LibraryId` (S3A read-only discovery). Counts come from the
    /// current canonical snapshot, read in the same statement as the head (one consistent row).
    pub fn list_libraries(&self) -> R<Vec<LibrarySummary>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT l.library_id, l.head_server_seq, c.snapshot_sha256, c.snapshot_bytes
             FROM libraries l JOIN current_state c ON c.library_id = l.library_id
             ORDER BY l.library_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, head, sha, bytes) in rows {
            let snap = SyncSnapshotV1::parse(&bytes)
                .map_err(|e| StoreError::Internal(format!("stored snapshot of {id}: {e}")))?;
            out.push(LibrarySummary {
                library_id: parse_library(&id)?,
                head_server_seq: head,
                snapshot_sha256: sha,
                notebook_count: snap.notebooks.len(),
                page_count: snap.notebooks.iter().map(|n| n.pages.len()).sum(),
            });
        }
        Ok(out)
    }

    pub fn genesis(&self, library_id: &str) -> R<Vec<u8>> {
        let lib = parse_library(library_id)?;
        self.conn()?
            .query_row(
                "SELECT genesis_bytes FROM libraries WHERE library_id = ?1",
                [lib.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::not_found("unknown_library", library_id))
    }

    // ------------------------------------------------------------------ operations

    /// Accept a batch of canonical envelopes atomically (all or nothing), in order.
    pub fn submit(&self, library_id: &str, raw_envelopes: &[Vec<u8>]) -> R<(i64, Vec<Accepted>)> {
        let lib = parse_library(library_id)?;
        let lib_s = lib.to_string();
        // Stateless validation first (cheap, no transaction).
        let mut envs = Vec::with_capacity(raw_envelopes.len());
        for (i, raw) in raw_envelopes.iter().enumerate() {
            let env: SyncEnvelopeV1 = from_json(raw)
                .map_err(|e| StoreError::bad("invalid_envelope", format!("envelope {i}: {e}")))?;
            if env.v != ENVELOPE_VERSION {
                return Err(StoreError::bad(
                    "unsupported_version",
                    format!("envelope {i}: version {}", env.v),
                ));
            }
            let canonical =
                to_canonical_json(&env).map_err(|e| StoreError::Internal(e.to_string()))?;
            if &canonical != raw {
                return Err(StoreError::bad(
                    "non_canonical",
                    format!("envelope {i} is not canonical SyncEnvelopeV1 bytes"),
                ));
            }
            if env.library_id != lib {
                return Err(StoreError::bad(
                    "wrong_library",
                    format!(
                        "envelope {i} is for library {}, route is {lib}",
                        env.library_id
                    ),
                ));
            }
            envs.push(env);
        }
        self.precheck_sequences(&lib_s, &envs)?;

        let mut conn = self.writer();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let head: Option<i64> = tx
            .query_row(
                "SELECT head_server_seq FROM libraries WHERE library_id = ?1",
                [&lib_s],
                |r| r.get(0),
            )
            .optional()?;
        let mut head = head.ok_or_else(|| StoreError::not_found("unknown_library", &lib_s))?;
        let (snap_bytes, hist_bytes): (Vec<u8>, Vec<u8>) = tx.query_row(
            "SELECT snapshot_bytes, history_bytes FROM current_state WHERE library_id = ?1",
            [&lib_s],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut state = load_state(&snap_bytes, &hist_bytes)?;
        let mut changed = false;
        let mut results = Vec::with_capacity(envs.len());

        for (i, (env, raw)) in envs.iter().zip(raw_envelopes).enumerate() {
            let op_id = env.operation_id.to_string();
            let replica = env.replica_id.to_string();
            // 1. Dedup on OperationId (global).
            let prior: Option<(String, i64, Vec<u8>, String)> = tx
                .query_row(
                    "SELECT library_id, server_seq, envelope_bytes, outcome
                     FROM operations WHERE operation_id = ?1",
                    [&op_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            if let Some((plib, seq, bytes, outcome)) = prior {
                if plib != lib_s || &bytes != raw {
                    return Err(StoreError::conflict(
                        "operation_id_conflict",
                        format!("operation {op_id} was already accepted with different bytes"),
                    ));
                }
                results.push(Accepted {
                    operation_id: op_id,
                    server_seq: seq,
                    outcome,
                    duplicate: true,
                });
                continue;
            }
            // New ReplaceStrokeLineages only when the operator/test gate is on. Dedup above
            // still returns an already-accepted envelope so a later disable cannot strand retries.
            if matches!(env.operation, SyncOperationV1::ReplaceStrokeLineages { .. })
                && !self.accept_replace_stroke_lineages.load(Ordering::SeqCst)
            {
                return Err(StoreError::bad(
                    "unsupported_operation",
                    format!("envelope {i}: ReplaceStrokeLineages is not accepted on this server"),
                ));
            }
            // 2. (replica, client_seq) must not belong to another operation.
            let taken: Option<String> = tx
                .query_row(
                    "SELECT operation_id FROM operations
                     WHERE library_id = ?1 AND replica_id = ?2 AND client_seq = ?3",
                    params![lib_s, replica, seq_to_text(env.client_seq)],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(other) = taken {
                return Err(StoreError::conflict(
                    "client_seq_conflict",
                    format!(
                        "replica {replica} client_seq {} already belongs to operation {other}",
                        env.client_seq
                    ),
                ));
            }
            // 3. Exact per-replica continuity (no gaps, no guessing).
            let last: Option<String> = tx
                .query_row(
                    "SELECT last_client_seq FROM replicas WHERE library_id = ?1 AND replica_id = ?2",
                    params![lib_s, replica],
                    |r| r.get(0),
                )
                .optional()?;
            let last = last.as_deref().map(seq_from_text).transpose()?;
            check_client_seq(last, env.client_seq)
                .map_err(|e| StoreError::conflict("client_seq_out_of_order", e))?;
            // 4. Referenced background blobs must already exist (fail closed).
            if let Some(b) = op_blob(&env.operation) {
                let missing = missing_blobs(&tx, vec![b])?;
                if !missing.is_empty() {
                    return Err(StoreError::MissingBlobs(missing));
                }
            }
            // 5. Canonical meaning. Reducer NoOp is not authorization: identity and
            // sequence were already checked. A *new* replacement against a deleted page
            // must not be accepted as a successful repair (replay of an already-ledgered
            // envelope still NoOps in the engine).
            let (next, outcome) = state
                .apply(env)
                .map_err(|error| StoreError::Semantic { index: i, error })?;
            if matches!(env.operation, SyncOperationV1::ReplaceStrokeLineages { .. })
                && matches!(outcome, Outcome::NoOp("page or notebook deleted"))
            {
                return Err(StoreError::Semantic {
                    index: i,
                    error: SemanticError::new(
                        SemanticErrorCode::InvalidOperation,
                        "ReplaceStrokeLineages cannot be newly accepted against a deleted page or notebook",
                    ),
                });
            }
            changed |= next != state;
            state = next;
            // 6. Sequence + persist.
            head += 1;
            tx.execute(
                "INSERT INTO operations (library_id, server_seq, operation_id, replica_id,
                    client_seq, envelope_bytes, envelope_sha256, outcome)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    lib_s,
                    head,
                    op_id,
                    replica,
                    seq_to_text(env.client_seq),
                    raw,
                    sha256_hex(raw).to_string(),
                    outcome.as_str()
                ],
            )?;
            tx.execute(
                "INSERT INTO replicas (library_id, replica_id, last_client_seq) VALUES (?1, ?2, ?3)
                 ON CONFLICT (library_id, replica_id) DO UPDATE SET last_client_seq = excluded.last_client_seq",
                params![lib_s, replica, seq_to_text(env.client_seq)],
            )?;
            results.push(Accepted {
                operation_id: op_id,
                server_seq: head,
                outcome: outcome.as_str().to_string(),
                duplicate: false,
            });
        }

        let accepted_new = results.iter().any(|r| !r.duplicate);
        if accepted_new {
            tx.execute(
                "UPDATE libraries SET head_server_seq = ?2 WHERE library_id = ?1",
                params![lib_s, head],
            )?;
            let (sb, sh, hb) = if changed {
                let sb = state
                    .snapshot
                    .to_canonical_bytes()
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let sh = sha256_hex(&sb).to_string();
                let hb = state
                    .history
                    .to_canonical_bytes()
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                (sb, sh, hb)
            } else {
                let sh = sha256_hex(&snap_bytes).to_string();
                (snap_bytes, sh, hist_bytes)
            };
            tx.execute(
                "UPDATE current_state SET through_server_seq = ?2, snapshot_bytes = ?3,
                    snapshot_sha256 = ?4, history_bytes = ?5 WHERE library_id = ?1",
                params![lib_s, head, sb, sh, hb],
            )?;
            if self.hooks.fail_submit_before_commit.load(Ordering::SeqCst) {
                return Err(StoreError::Internal(
                    "injected failure before COMMIT".into(),
                ));
            }
        }
        tx.commit()?;
        Ok((head, results))
    }

    /// Lock-free **advisory** refusal of envelopes that are provably *ahead* of their replica's
    /// sequence (a gap), so they never contend for the writer. Only `client_seq > expected`
    /// with an unknown `OperationId` is refused here — exactly the case the authoritative
    /// in-transaction checks would also refuse as `client_seq_out_of_order`. Everything else
    /// (dedup, conflicts, exhaustion after `u64::MAX`) is decided inside the serialized
    /// transaction; if `expected` cannot be computed without overflow, nothing is refused here.
    fn precheck_sequences(&self, lib: &str, envs: &[SyncEnvelopeV1]) -> R<()> {
        let conn = self.conn()?;
        // replica → next expected client_seq (`None` = exhausted; leave it to the transaction).
        let mut expected: HashMap<String, Option<u64>> = HashMap::new();
        for env in envs {
            let op_id = env.operation_id.to_string();
            let known: bool = conn
                .query_row(
                    "SELECT 1 FROM operations WHERE operation_id = ?1",
                    [&op_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            let replica = env.replica_id.to_string();
            let want = match expected.get(&replica) {
                Some(w) => *w,
                None => {
                    let last: Option<String> = conn
                        .query_row(
                            "SELECT last_client_seq FROM replicas
                             WHERE library_id = ?1 AND replica_id = ?2",
                            params![lib, replica],
                            |r| r.get(0),
                        )
                        .optional()?;
                    match last.as_deref().map(seq_from_text).transpose()? {
                        None => Some(1),
                        Some(l) => l.checked_add(1),
                    }
                }
            };
            let Some(want) = want else {
                // Exhausted replica: the transaction reports it precisely.
                continue;
            };
            if !known && env.client_seq > want {
                return Err(StoreError::conflict(
                    "client_seq_out_of_order",
                    format!(
                        "client_seq {} out of order (expected {want})",
                        env.client_seq
                    ),
                ));
            }
            if !known {
                expected.insert(replica, env.client_seq.max(want).checked_add(1));
            }
        }
        Ok(())
    }

    /// TEST ONLY: set a replica's last accepted `client_seq` directly, so boundary behavior near
    /// `i64::MAX` / `u64::MAX` can be exercised without submitting ~2^63 operations. Goes through
    /// the same exact TEXT representation and the serialized writer.
    #[doc(hidden)]
    pub fn seed_replica_sequence_for_test(
        &self,
        library_id: &str,
        replica_id: &str,
        last_client_seq: u64,
    ) -> R<()> {
        let lib = parse_library(library_id)?.to_string();
        let conn = self.writer();
        conn.execute(
            "INSERT INTO replicas (library_id, replica_id, last_client_seq) VALUES (?1, ?2, ?3)
             ON CONFLICT (library_id, replica_id) DO UPDATE SET last_client_seq = excluded.last_client_seq",
            params![lib, replica_id, seq_to_text(last_client_seq)],
        )?;
        Ok(())
    }

    /// Operations with `server_seq > after`, ascending, at most `limit`.
    pub fn tail(&self, library_id: &str, after: i64, limit: i64) -> R<(i64, Vec<TailEntry>)> {
        let info = self.library(library_id)?;
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT server_seq, envelope_bytes FROM operations
             WHERE library_id = ?1 AND server_seq > ?2 ORDER BY server_seq ASC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![info.library_id.to_string(), after, limit], |r| {
                Ok(TailEntry {
                    server_seq: r.get(0)?,
                    envelope_bytes: r.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((info.head_server_seq, rows))
    }

    pub fn snapshot(&self, library_id: &str) -> R<CurrentSnapshot> {
        let lib = parse_library(library_id)?;
        self.conn()?
            .query_row(
                "SELECT through_server_seq, snapshot_bytes, snapshot_sha256
                 FROM current_state WHERE library_id = ?1",
                [lib.to_string()],
                |r| {
                    Ok(CurrentSnapshot {
                        through_server_seq: r.get(0)?,
                        snapshot_bytes: r.get(1)?,
                        snapshot_sha256: r.get(2)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::not_found("unknown_library", library_id))
    }

    /// Rebuild the current state from genesis + the full ledger and compare it with the stored
    /// current snapshot/history. `Ok(true)` when bit-identical.
    pub fn verify_replay(&self, library_id: &str) -> R<bool> {
        let genesis = self.genesis(library_id)?;
        let snap: SyncSnapshotV1 =
            from_json(&genesis).map_err(|e| StoreError::Internal(e.to_string()))?;
        let mut state =
            LibraryState::genesis(snap).map_err(|e| StoreError::Internal(e.to_string()))?;
        let (_, entries) = self.tail(library_id, 0, i64::MAX)?;
        for (expect, e) in (1..).zip(&entries) {
            if e.server_seq != expect {
                return Ok(false);
            }
            let env: SyncEnvelopeV1 =
                from_json(&e.envelope_bytes).map_err(|x| StoreError::Internal(x.to_string()))?;
            state = state
                .apply(&env)
                .map_err(|x| StoreError::Internal(x.to_string()))?
                .0;
        }
        let conn = self.conn()?;
        let (through, sb, hb): (i64, Vec<u8>, Vec<u8>) = conn.query_row(
            "SELECT through_server_seq, snapshot_bytes, history_bytes FROM current_state
             WHERE library_id = ?1",
            [parse_library(library_id)?.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let replay_sb = state
            .snapshot
            .to_canonical_bytes()
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let replay_hb = state
            .history
            .to_canonical_bytes()
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(through == entries.len() as i64 && sb == replay_sb && hb == replay_hb)
    }

    // ------------------------------------------------------------------ blobs

    fn blob_path(&self, sha: &Sha256Hex) -> PathBuf {
        self.blob_dir.join(&sha.as_str()[..2]).join(sha.as_str())
    }

    /// Content-addressed put: hash recomputed from bytes; the URL hash must match. Returns
    /// `created`.
    pub fn put_blob(&self, url_sha: &str, bytes: &[u8]) -> R<bool> {
        let want = Sha256Hex::parse(url_sha).map_err(|e| StoreError::bad("invalid_sha256", e))?;
        let got = sha256_hex(bytes);
        if got != want {
            return Err(StoreError::bad(
                "hash_mismatch",
                format!("body sha256 {got} != url {want}"),
            ));
        }
        let path = self.blob_path(&want);
        if let Some(existing) = self.read_blob_file(&want)? {
            if existing != bytes {
                return Err(StoreError::conflict(
                    "blob_conflict",
                    format!("stored blob {want} differs from submitted bytes"),
                ));
            }
            // Ensure the index row exists (a crash may have left only the file).
            let conn = self.writer();
            let n = conn.execute(
                "INSERT OR IGNORE INTO blobs (sha256, byte_len, rel_path) VALUES (?1, ?2, ?3)",
                params![
                    want.as_str(),
                    bytes.len() as i64,
                    rel(&path, &self.blob_dir)
                ],
            )?;
            return Ok(n > 0);
        }
        let dir = path.parent().expect("blob path has a parent");
        fs::create_dir_all(dir).map_err(|e| StoreError::Internal(e.to_string()))?;
        let tmp = dir.join(format!("{}.tmp.{}", want.as_str(), std::process::id()));
        {
            let mut f = File::create(&tmp).map_err(|e| StoreError::Internal(e.to_string()))?;
            f.write_all(bytes)
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            f.sync_all()
                .map_err(|e| StoreError::Internal(e.to_string()))?;
        }
        fs::rename(&tmp, &path).map_err(|e| StoreError::Internal(e.to_string()))?;
        let conn = self.writer();
        conn.execute(
            "INSERT OR IGNORE INTO blobs (sha256, byte_len, rel_path) VALUES (?1, ?2, ?3)",
            params![
                want.as_str(),
                bytes.len() as i64,
                rel(&path, &self.blob_dir)
            ],
        )?;
        Ok(true)
    }

    fn read_blob_file(&self, sha: &Sha256Hex) -> R<Option<Vec<u8>>> {
        match fs::read(self.blob_path(sha)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Internal(e.to_string())),
        }
    }

    /// Stored blob bytes, verified against their hash on every read.
    pub fn get_blob(&self, url_sha: &str) -> R<Vec<u8>> {
        let sha = Sha256Hex::parse(url_sha).map_err(|e| StoreError::bad("invalid_sha256", e))?;
        let known: Option<i64> = self
            .conn()?
            .query_row(
                "SELECT byte_len FROM blobs WHERE sha256 = ?1",
                [sha.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if known.is_none() {
            return Err(StoreError::not_found("unknown_blob", sha.as_str()));
        }
        let bytes = self
            .read_blob_file(&sha)?
            .ok_or_else(|| StoreError::Internal(format!("blob {sha} indexed but file missing")))?;
        if sha256_hex(&bytes) != sha {
            return Err(StoreError::Internal(format!(
                "blob {sha} is corrupt on disk"
            )));
        }
        Ok(bytes)
    }
}

fn open_conn(path: &Path) -> R<Connection> {
    let c = Connection::open(path)?;
    c.busy_timeout(Duration::from_secs(30))?;
    c.pragma_update(None, "foreign_keys", "ON")?;
    c.pragma_update(None, "synchronous", "FULL")?;
    Ok(c)
}

fn rel(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn missing_blobs(tx: &rusqlite::Transaction<'_>, refs: Vec<(String, u64)>) -> R<Vec<String>> {
    let mut missing = Vec::new();
    for (sha, len) in refs {
        let stored: Option<i64> = tx
            .query_row(
                "SELECT byte_len FROM blobs WHERE sha256 = ?1",
                [&sha],
                |r| r.get(0),
            )
            .optional()?;
        match stored {
            Some(l) if l as u64 == len => {}
            _ => missing.push(sha),
        }
    }
    missing.sort();
    missing.dedup();
    Ok(missing)
}

fn parse_library(s: &str) -> R<LibraryId> {
    LibraryId::parse(s).map_err(|e| StoreError::bad("invalid_library_id", e))
}

fn load_state(snapshot_bytes: &[u8], history_bytes: &[u8]) -> R<LibraryState> {
    let snapshot =
        SyncSnapshotV1::parse(snapshot_bytes).map_err(|e| StoreError::Internal(e.to_string()))?;
    let history =
        HistoryV1::parse(history_bytes).map_err(|e| StoreError::Internal(e.to_string()))?;
    let state = LibraryState { snapshot, history };
    state
        .validate()
        .map_err(|e| StoreError::Internal(format!("stored state invalid: {e}")))?;
    Ok(state)
}
