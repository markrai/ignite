//! Slice 1H-D1: **library adoption** — an existing Kindle installation takes up a library that
//! already lives on an Ignite Server (typically one restored from a portable backup onto a
//! different server, with a new library ID and server ID).
//!
//! The selected server library is authoritative. Kindle-local differences (queued operations,
//! uncaptured strokes, unbound pages, other libraries) are not transferred; the previous library
//! and sync epoch are kept whole as retired copies, never deleted or replayed.
//!
//! # Steps
//!
//! 1. [`fetch_adoption_source`] (or a caller with the same data and [`VerifiedGenesis::verify`])
//!    proves the authenticated server is the selected one and that its genesis for the selected
//!    library is canonical, hashes to the advertised genesis hash and names that library.
//! 2. [`adopt_library`] prepares a complete replacement **beside** the live installation:
//!    * `<library>.adopt/` — a fresh library whose `identity.json` carries the target library ID,
//!      materialized from the genesis by the [`LocalMaterializer`] (notebook, page, stroke, text,
//!      link and folder identities are the snapshot's);
//!    * `<sync>.adopt/sync/` — a new sync epoch (`establish_adopted_epoch`): new replica,
//!      client_seq 1, no pending operations, ServerBaseState = genesis at server_seq 0, bound to
//!      the server ID, genesis already on the server (never POSTed again).
//!
//!    The staged library is verified page by page and its canonical snapshot must hash to the
//!    genesis hash before anything live is touched.
//! 3. Commit point: `<sync>.adopt/plan.json` (atomic). From here the adoption only rolls forward.
//!    When the adoption also pairs a server (Kindle UI restore), its server profile is staged in
//!    the workspace first and the plan records it.
//! 4. Publication by directory renames: live library → `<library>.retired-<adoption>`, staged →
//!    live; the same for the sync directory; then the staged server profile replaces the device's
//!    profile. Then the plan is removed.
//!
//! Adoptions and startup recovery of one installation are serialized by `<sync>.adopt.lock`
//! (beside, never inside, a directory publication renames), so no adoption ever discards
//! another's live preparation.
//!
//! # Genesis versus head
//!
//! A restored library's genesis is the restored snapshot at server_seq 0; operations accepted
//! after the restore follow it in the server's ledger. The adopted epoch starts at the genesis
//! with cursor 0, and ordinary inbound sync pulls everything after it from the *target* server
//! (never the source server's history). [`VerifiedGenesis::head_server_seq`] says how much
//! catching up the first sync will do; at head 0 the server's current snapshot hash must equal
//! the genesis hash.
//!
//! # Interruption
//!
//! [`recover_interrupted_adoption`] runs at startup before the library is opened. Without a plan
//! the preparation never committed: the staging directories are removed and the old
//! installation (and its server profile) is untouched. With a plan, publication — library, sync
//! epoch and server profile — is completed (each step is idempotent).
//! `OutboundSync::open` refuses while a plan is pending, so ordinary sync never runs against a
//! half-adopted installation.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use control_protocol::ServerId;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sync_engine::LibraryState;
use sync_protocol::{
    sha256_hex, snapshot_sha256, BackgroundRef, IdentityFileV1, LibraryId, PageId, Reconciled,
    ReplicaId, ReplicaStateV1, Sha256Hex, SyncSnapshotV1, IDENTITY_FILE_VERSION,
    REPLICA_STATE_FILE,
};
use uuid::Uuid;

use crate::server_link::{load_profile, save_profile, ServerProfileV1, PROFILE_FILE};
use crate::sync_local::{
    build_snapshot, ensure_replica_state, persist_planned_identity, read_identity, verify_scoped,
};
use crate::sync_materialize::{no_hook, profile_for, DeviceGeometry, Hook, LocalMaterializer};
use crate::sync_outbound::{
    establish_adopted_epoch, read_json, sync_dir_fsync, write_atomic, write_json, SyncStateV1,
    BLOB_DIR, LOCK_FILE, STATE_FILE,
};

/// The commit record of a prepared adoption, inside the adoption workspace.
pub const PLAN_FILE: &str = "plan.json";
const PLAN_VERSION: u32 = 1;
/// Replica-local preferences kept in the sync directory (carried into the new epoch).
const CARRIED_FILES: &[&str] = &[crate::prefs::PREFERENCES_FILE];
/// Largest genesis document accepted (the restore's per-snapshot limit plus the envelope).
const MAX_GENESIS_RESPONSE: u64 = (128 << 20) + (64 << 10);
/// Largest library/status document.
const MAX_SMALL_RESPONSE: u64 = 64 << 10;
/// Largest library list.
const MAX_LIST_RESPONSE: u64 = 1 << 20;
/// Largest background blob (the restore's per-blob limit).
const MAX_BLOB_RESPONSE: u64 = 32 << 20;
/// A whole genesis download over the Kindle's Wi-Fi.
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(600);
/// Free space kept beyond the estimated staging requirement.
const SPACE_MARGIN: u64 = 64 << 20;

fn log(msg: impl AsRef<str>) {
    println!("adopt: {}", msg.as_ref());
}

// ------------------------------------------------------------------------------------------
// Errors

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdoptErrorKind {
    /// Transport failure or an unexpected reply; nothing local changed.
    Unreachable,
    /// The server did not accept the credential.
    Unauthorized,
    /// The authenticated server is not the selected one.
    ServerMismatch,
    /// The server's library (or its genesis) is not the selected library.
    LibraryMismatch,
    /// Genesis bytes, advertised genesis hash and current snapshot disagree.
    GenesisMismatch,
    /// The genesis is not a valid canonical snapshot.
    InvalidSnapshot,
    /// Valid, but this Kindle cannot represent it (e.g. no notebooks, foreign panel geometry).
    Unsupported,
    /// Sync is running on this sync directory, or another adoption is.
    Busy,
    /// Cancelled before the commit point; the preparation was removed.
    Cancelled,
    /// Not enough free storage to stage the replacement.
    InsufficientStorage,
    /// The local installation refused the adoption (paths, prior state, I/O).
    Local,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdoptError {
    pub kind: AdoptErrorKind,
    pub message: String,
}

impl AdoptError {
    fn new(kind: AdoptErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    fn local(message: impl Into<String>) -> Self {
        Self::new(AdoptErrorKind::Local, message)
    }
}

impl std::fmt::Display for AdoptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "REFUSE ADOPT ({:?}): {}", self.kind, self.message)
    }
}

// ------------------------------------------------------------------------------------------
// Target + verified canonical data

/// What the user selected: a paired server and one of its libraries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdoptionTarget {
    pub server_id: ServerId,
    pub library_id: LibraryId,
}

/// `GET /v1/libraries/{id}` as the server reported it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ServerLibraryInfo {
    pub library_id: LibraryId,
    pub genesis_sha256: Sha256Hex,
    pub head_server_seq: u64,
    /// Hash of the current (head) snapshot.
    pub snapshot_sha256: Sha256Hex,
}

/// The selected library's genesis, verified against the selected identity. Only
/// [`VerifiedGenesis::verify`] constructs it.
#[derive(Clone, Debug)]
pub struct VerifiedGenesis {
    target: AdoptionTarget,
    genesis_bytes: Vec<u8>,
    genesis_sha256: Sha256Hex,
    snapshot: SyncSnapshotV1,
    head_server_seq: u64,
    head_snapshot_sha256: Sha256Hex,
    blobs: BTreeMap<Sha256Hex, Vec<u8>>,
}

impl VerifiedGenesis {
    /// Verify the server's answers for `target`:
    /// * `authenticated_server` (the server ID behind an accepted credential) is the selected one;
    /// * `info` and the genesis document both name the selected library and the same genesis hash;
    /// * `snapshot_bytes` hash to it, are the canonical bytes of a valid snapshot of that library
    ///   and a valid reducer genesis;
    /// * at head 0 the current snapshot is the genesis;
    /// * every background blob the genesis references is among `blobs`, byte-exact.
    pub fn verify(
        target: AdoptionTarget,
        authenticated_server: ServerId,
        info: &ServerLibraryInfo,
        genesis_library_id: LibraryId,
        genesis_sha256: &Sha256Hex,
        snapshot_bytes: Vec<u8>,
        blobs: Vec<Vec<u8>>,
    ) -> Result<Self, AdoptError> {
        let given: BTreeMap<Sha256Hex, Vec<u8>> =
            blobs.into_iter().map(|b| (sha256_hex(&b), b)).collect();
        Self::verify_with(
            target,
            authenticated_server,
            info,
            genesis_library_id,
            genesis_sha256,
            snapshot_bytes,
            &mut |sha| given.get(sha).cloned(),
        )
    }

    /// [`Self::verify`] with blobs supplied on demand (`blob(sha)`; `None` = unavailable), so a
    /// caller can fetch exactly the referenced blobs without parsing the snapshot itself.
    fn verify_with(
        target: AdoptionTarget,
        authenticated_server: ServerId,
        info: &ServerLibraryInfo,
        genesis_library_id: LibraryId,
        genesis_sha256: &Sha256Hex,
        snapshot_bytes: Vec<u8>,
        blob: &mut dyn FnMut(&Sha256Hex) -> Option<Vec<u8>>,
    ) -> Result<Self, AdoptError> {
        use AdoptErrorKind as K;
        if authenticated_server != target.server_id {
            return Err(AdoptError::new(
                K::ServerMismatch,
                format!(
                    "the server answering is {authenticated_server}, not the selected {}",
                    target.server_id
                ),
            ));
        }
        if info.library_id != target.library_id || genesis_library_id != target.library_id {
            return Err(AdoptError::new(
                K::LibraryMismatch,
                format!(
                    "the server returned library {} / genesis of {}, not the selected {}",
                    info.library_id, genesis_library_id, target.library_id
                ),
            ));
        }
        if *genesis_sha256 != info.genesis_sha256 {
            return Err(AdoptError::new(
                K::GenesisMismatch,
                "the genesis document and the library disagree on the genesis hash",
            ));
        }
        let actual = sha256_hex(&snapshot_bytes);
        if actual != info.genesis_sha256 {
            return Err(AdoptError::new(
                K::GenesisMismatch,
                format!(
                    "genesis bytes hash to {actual}, the server advertises {}",
                    info.genesis_sha256
                ),
            ));
        }
        let snapshot = SyncSnapshotV1::parse(&snapshot_bytes)
            .map_err(|e| AdoptError::new(K::InvalidSnapshot, e.to_string()))?;
        // Canonical: re-encoding the parsed snapshot yields exactly these bytes. Compared by
        // SHA-256 (the re-encoding is hashed as it is produced, never buffered), against the hash
        // of the received bytes that already matched the server's genesis hash.
        let canonical = snapshot_sha256(&snapshot)
            .map_err(|e| AdoptError::new(K::InvalidSnapshot, e.to_string()))?;
        if canonical != actual {
            return Err(AdoptError::new(
                K::InvalidSnapshot,
                "genesis bytes are not the canonical encoding of the snapshot",
            ));
        }
        if snapshot.library_id != target.library_id {
            return Err(AdoptError::new(
                K::LibraryMismatch,
                format!(
                    "the genesis snapshot is of library {}, not {}",
                    snapshot.library_id, target.library_id
                ),
            ));
        }
        // A valid reducer genesis (validated in place; the snapshot is handed back, not cloned).
        let snapshot = LibraryState::genesis(snapshot)
            .map_err(|e| AdoptError::new(K::InvalidSnapshot, e.to_string()))?
            .snapshot;
        if info.head_server_seq == 0 && info.snapshot_sha256 != info.genesis_sha256 {
            return Err(AdoptError::new(
                K::GenesisMismatch,
                "the library has no operations but its current snapshot is not its genesis",
            ));
        }
        if snapshot.notebooks.is_empty() {
            return Err(AdoptError::new(
                K::Unsupported,
                "the library has no notebooks (a Kindle library always holds one)",
            ));
        }
        let mut kept = BTreeMap::new();
        for (sha, len) in referenced_blobs(&snapshot) {
            match blob(&sha) {
                Some(b) if b.len() as u64 == len && sha256_hex(&b) == sha => {
                    kept.insert(sha, b);
                }
                _ => {
                    return Err(AdoptError::new(
                        K::GenesisMismatch,
                        format!("background blob {sha} is missing or has the wrong length"),
                    ))
                }
            }
        }
        Ok(Self {
            target,
            genesis_sha256: actual,
            genesis_bytes: snapshot_bytes,
            snapshot,
            head_server_seq: info.head_server_seq,
            head_snapshot_sha256: info.snapshot_sha256.clone(),
            blobs: kept,
        })
    }

    pub fn target(&self) -> AdoptionTarget {
        self.target
    }
    pub fn genesis_sha256(&self) -> &Sha256Hex {
        &self.genesis_sha256
    }
    pub fn snapshot(&self) -> &SyncSnapshotV1 {
        &self.snapshot
    }
    /// Server operations after the genesis (what the first inbound sync will apply).
    pub fn head_server_seq(&self) -> u64 {
        self.head_server_seq
    }
    pub fn head_snapshot_sha256(&self) -> &Sha256Hex {
        &self.head_snapshot_sha256
    }
}

fn referenced_blobs(snap: &SyncSnapshotV1) -> BTreeMap<Sha256Hex, u64> {
    snap.notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .filter_map(|p| match &p.background {
            BackgroundRef::Blob {
                sha256, byte_len, ..
            } => Some((sha256.clone(), *byte_len)),
            BackgroundRef::Blank => None,
        })
        .collect()
}

/// Bounded `GET` with the paired credential. The body is read up to `max` bytes.
fn get(
    endpoint: &str,
    credential: &str,
    path: &str,
    max: u64,
) -> Result<(u16, Vec<u8>), AdoptError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(30))
        .timeout(DOWNLOAD_DEADLINE)
        .max_idle_connections(0)
        .build();
    let url = format!("{}{path}", endpoint.trim_end_matches('/'));
    let resp = match agent
        .get(&url)
        .set("Authorization", &format!("Bearer {credential}"))
        .call()
    {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r,
        Err(e) => {
            return Err(AdoptError::new(
                AdoptErrorKind::Unreachable,
                format!(
                    "GET {path}: {}",
                    e.to_string().replace(credential, "<redacted>")
                ),
            ))
        }
    };
    let status = resp.status();
    // Sized from Content-Length when given (a growing buffer would reach twice the body).
    let declared = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n <= max);
    let mut body = Vec::with_capacity(declared.map_or(0, |n| n as usize));
    resp.into_reader()
        .take(max + 1)
        .read_to_end(&mut body)
        .map_err(|e| AdoptError::new(AdoptErrorKind::Unreachable, format!("GET {path}: {e}")))?;
    if body.len() as u64 > max {
        return Err(AdoptError::new(
            AdoptErrorKind::Unsupported,
            format!("GET {path}: response exceeds {max} bytes"),
        ));
    }
    Ok((status, body))
}

fn expect_ok(path: &str, (status, body): (u16, Vec<u8>)) -> Result<Vec<u8>, AdoptError> {
    let kind = match status {
        200 => return Ok(body),
        401 | 403 => AdoptErrorKind::Unauthorized,
        404 => AdoptErrorKind::LibraryMismatch,
        _ => AdoptErrorKind::Unreachable,
    };
    let text: String = String::from_utf8_lossy(&body).chars().take(200).collect();
    Err(AdoptError::new(
        kind,
        format!("GET {path}: {status} {text}"),
    ))
}

/// One row of `GET /v1/libraries`: enough to choose and preview a library without downloading it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ServerLibrarySummary {
    pub library_id: LibraryId,
    pub head_server_seq: u64,
    pub snapshot_sha256: Sha256Hex,
    pub notebook_count: usize,
    pub page_count: usize,
}

/// The libraries on the server at `endpoint` (lightweight list; nothing is downloaded or
/// verified here — [`fetch_adoption_source`] does that for the chosen one).
pub fn list_server_libraries(
    endpoint: &str,
    credential: &str,
) -> Result<Vec<ServerLibrarySummary>, AdoptError> {
    #[derive(Deserialize)]
    struct List {
        libraries: Vec<ServerLibrarySummary>,
    }
    let path = "/v1/libraries";
    let raw = expect_ok(path, get(endpoint, credential, path, MAX_LIST_RESPONSE)?)?;
    serde_json::from_slice::<List>(&raw)
        .map(|l| l.libraries)
        .map_err(|e| AdoptError::new(AdoptErrorKind::Unreachable, format!("{path}: {e}")))
}

/// The authenticated server's ID (the credential must open a session).
fn authenticated_server(endpoint: &str, credential: &str) -> Result<ServerId, AdoptError> {
    use crate::server_link::CallError;
    match crate::server_link::fetch_status(endpoint, Some(credential)) {
        Ok(st) if st.session.is_some() => Ok(st.server_id),
        Ok(_) | Err(CallError::Status(401 | 403, _)) => Err(AdoptError::new(
            AdoptErrorKind::Unauthorized,
            "the server did not accept this device's credential",
        )),
        Err(e) => Err(AdoptError::new(
            AdoptErrorKind::Unreachable,
            format!("server status: {e:?}"),
        )),
    }
}

/// Fetch and verify everything adoption needs from the paired server at `endpoint`. The server
/// identity is checked before and after the downloads (the same credential, the same server).
pub fn fetch_adoption_source(
    endpoint: &str,
    credential: &str,
    target: AdoptionTarget,
) -> Result<VerifiedGenesis, AdoptError> {
    let first = authenticated_server(endpoint, credential)?;
    if first != target.server_id {
        return Err(AdoptError::new(
            AdoptErrorKind::ServerMismatch,
            format!(
                "the server at this address is {first}, not the selected {}",
                target.server_id
            ),
        ));
    }
    let lib = target.library_id;
    let path = format!("/v1/libraries/{lib}");
    let raw = expect_ok(&path, get(endpoint, credential, &path, MAX_SMALL_RESPONSE)?)?;
    let info: ServerLibraryInfo = serde_json::from_slice(&raw)
        .map_err(|e| AdoptError::new(AdoptErrorKind::Unreachable, format!("{path}: {e}")))?;
    #[derive(Deserialize)]
    struct GenesisDoc<'a> {
        library_id: LibraryId,
        genesis_sha256: Sha256Hex,
        #[serde(borrow)]
        snapshot: &'a RawValue,
    }
    let path = format!("/v1/libraries/{lib}/genesis");
    let mut raw = expect_ok(
        &path,
        get(endpoint, credential, &path, MAX_GENESIS_RESPONSE)?,
    )?;
    let (genesis_library_id, genesis_sha256, range) = {
        let doc: GenesisDoc<'_> = serde_json::from_slice(&raw).map_err(|e| {
            AdoptError::new(AdoptErrorKind::InvalidSnapshot, format!("{path}: {e}"))
        })?;
        let inner = doc.snapshot.get().as_bytes();
        let start = inner.as_ptr() as usize - raw.as_ptr() as usize;
        (
            doc.library_id,
            doc.genesis_sha256,
            start..start + inner.len(),
        )
    };
    // The snapshot's exact bytes, cut out of the response buffer in place (no second copy).
    raw.truncate(range.end);
    raw.drain(..range.start);
    let snapshot_bytes = raw;
    let mut blob_error = None;
    let verified = VerifiedGenesis::verify_with(
        target,
        first,
        &info,
        genesis_library_id,
        &genesis_sha256,
        snapshot_bytes,
        &mut |sha| {
            let path = format!("/v1/blobs/{sha}");
            match get(endpoint, credential, &path, MAX_BLOB_RESPONSE)
                .and_then(|r| expect_ok(&path, r))
            {
                Ok(b) => Some(b),
                Err(e) => {
                    blob_error = Some(e);
                    None
                }
            }
        },
    );
    if let Some(e) = blob_error {
        return Err(e);
    }
    let verified = verified?;
    // Still the selected server after the downloads.
    let second = authenticated_server(endpoint, credential)?;
    if second != target.server_id {
        return Err(AdoptError::new(
            AdoptErrorKind::ServerMismatch,
            "the server changed while the library was downloaded",
        ));
    }
    Ok(verified)
}

// ------------------------------------------------------------------------------------------
// Layout

/// Every path an adoption uses: siblings of the live library and sync directory, so each
/// publication step is a same-directory rename.
#[derive(Clone, Debug)]
struct Layout {
    library: PathBuf,
    sync: PathBuf,
    /// `<sync>.adopt/`: holds the staged sync epoch and the plan.
    workspace: PathBuf,
    staged_library: PathBuf,
    staged_sync: PathBuf,
    /// `<sync>.adopt.lock`: serializes adoptions and recovery of this installation.
    lock: PathBuf,
}

fn sibling(p: &Path, suffix: &str) -> Result<PathBuf, String> {
    let name = p
        .file_name()
        .ok_or_else(|| format!("{} has no directory name", p.display()))?;
    let mut n = name.to_os_string();
    n.push(suffix);
    Ok(p.with_file_name(n))
}

impl Layout {
    fn new(library_root: &Path, sync_dir: &Path) -> Result<Self, String> {
        let workspace = sibling(sync_dir, ".adopt")?;
        Ok(Self {
            library: library_root.to_path_buf(),
            sync: sync_dir.to_path_buf(),
            staged_library: sibling(library_root, ".adopt")?,
            staged_sync: workspace.join("sync"),
            lock: sibling(sync_dir, ".adopt.lock")?,
            workspace,
        })
    }
    fn plan(&self) -> PathBuf {
        self.workspace.join(PLAN_FILE)
    }
    /// The server profile an adoption publishes with the library (staged until then).
    fn staged_profile_dir(&self) -> &Path {
        &self.workspace
    }
    fn retired_library(&self, adoption: &str) -> Result<PathBuf, String> {
        sibling(&self.library, &format!(".retired-{adoption}"))
    }
    fn retired_sync(&self, adoption: &str) -> Result<PathBuf, String> {
        sibling(&self.sync, &format!(".retired-{adoption}"))
    }
}

/// Take the installation's adoption lock (refuses while another adoption or recovery holds it).
fn adoption_lock(layout: &Layout) -> Result<fs::File, AdoptError> {
    if let Some(parent) = layout.lock.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| AdoptError::local(format!("create {}: {e}", parent.display())))?;
    }
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&layout.lock)
        .map_err(|e| AdoptError::local(format!("open adoption lock: {e}")))?;
    f.try_lock().map_err(|e| {
        AdoptError::new(
            AdoptErrorKind::Busy,
            format!("another library adoption is in progress ({e})"),
        )
    })?;
    Ok(f)
}

/// Whether a committed adoption into `sync_dir` still awaits publication.
pub fn adoption_pending(sync_dir: &Path) -> bool {
    sibling(sync_dir, ".adopt").is_ok_and(|w| w.join(PLAN_FILE).exists())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdoptPlanV1 {
    v: u32,
    /// UUIDv7 text; names the retired directories.
    adoption_id: String,
    server_id: ServerId,
    library_id: LibraryId,
    replica_id: ReplicaId,
    genesis_sha256: Sha256Hex,
    /// A server profile for `server_id` is staged in the workspace and published last.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    profile: bool,
}

fn parent_fsync(p: &Path) {
    if let Some(parent) = p.parent() {
        sync_dir_fsync(parent);
    }
}

fn rename(from: &Path, to: &Path) -> Result<(), String> {
    fs::rename(from, to)
        .map_err(|e| format!("rename {} -> {}: {e}", from.display(), to.display()))?;
    parent_fsync(to);
    Ok(())
}

/// Move the live directory to `retired` (if present) and the staged one into place. Idempotent
/// over every crash point: the staged directory exists exactly until it is live.
fn swap_in(
    live: &Path,
    staged: &Path,
    retired: &Path,
    hook: Hook<'_>,
    after_retire: &str,
) -> Result<(), String> {
    if !staged.exists() {
        return if live.exists() {
            Ok(())
        } else {
            Err(format!(
                "neither {} nor its staged replacement exists",
                live.display()
            ))
        };
    }
    if live.exists() {
        if retired.exists() {
            return Err(format!(
                "{} and {} both exist; refusing to overwrite either",
                live.display(),
                retired.display()
            ));
        }
        rename(live, retired)?;
        hook(after_retire)?;
    }
    rename(staged, live)
}

fn publish(
    layout: &Layout,
    plan: &AdoptPlanV1,
    device_dir: Option<&Path>,
    hook: Hook<'_>,
) -> Result<(), String> {
    swap_in(
        &layout.library,
        &layout.staged_library,
        &layout.retired_library(&plan.adoption_id)?,
        hook,
        "after_retire_library",
    )?;
    hook("after_publish_library")?;
    swap_in(
        &layout.sync,
        &layout.staged_sync,
        &layout.retired_sync(&plan.adoption_id)?,
        hook,
        "after_retire_sync",
    )?;
    hook("after_publish_sync")?;
    // The published pair must be the planned one before the plan goes away.
    let id = read_identity(&layout.library)?.ok_or("published library has no identity")?;
    let st: SyncStateV1 = read_json(&layout.sync.join(STATE_FILE))?;
    if id.library_id != plan.library_id
        || st.library_id != plan.library_id
        || st.replica_id != plan.replica_id
        || st.server_id != Some(plan.server_id)
    {
        return Err("the published library and sync state are not the planned ones".into());
    }
    if plan.profile {
        publish_profile(layout, plan, device_dir)?;
        hook("after_publish_profile")?;
    }
    fs::remove_file(layout.plan()).map_err(|e| format!("remove plan: {e}"))?;
    sync_dir_fsync(&layout.workspace);
    hook("after_plan_removed")?;
    remove_workspace(layout)
}

/// Replace the device's server profile with the staged one (idempotent: an equal profile is left
/// alone; once the staged copy is gone the device must already hold the planned server).
fn publish_profile(
    layout: &Layout,
    plan: &AdoptPlanV1,
    device_dir: Option<&Path>,
) -> Result<(), String> {
    let dir = device_dir.ok_or("the adoption pairs a server but no device directory was given")?;
    let Some(staged) = load_profile(layout.staged_profile_dir())? else {
        return match load_profile(dir)? {
            Some(cur) if cur.server_id == plan.server_id => Ok(()),
            _ => Err("the staged server profile is missing".into()),
        };
    };
    if staged.server_id != plan.server_id {
        return Err("the staged server profile names another server".into());
    }
    if load_profile(dir)?.as_ref() != Some(&staged) {
        save_profile(dir, &staged)?;
        sync_dir_fsync(dir);
    }
    Ok(())
}

/// Remove the (now empty) workspace: only the files an adoption writes there.
fn remove_workspace(layout: &Layout) -> Result<(), String> {
    for f in [
        PLAN_FILE,
        &format!("{PLAN_FILE}.tmp"),
        PROFILE_FILE,
        "server-profile.tmp",
    ] {
        match fs::remove_file(layout.workspace.join(f)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("remove {f}: {e}")),
        }
    }
    match fs::remove_dir(&layout.workspace) {
        Ok(()) => parent_fsync(&layout.workspace),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("remove {}: {e}", layout.workspace.display())),
    }
    Ok(())
}

/// Remove an uncommitted preparation: the staged library and the workspace (staged sync epoch).
/// Never touches the live installation.
fn discard_staging(layout: &Layout) -> Result<bool, String> {
    if layout.plan().exists() {
        return Err("refusing to discard a committed adoption".into());
    }
    let mut removed = false;
    for d in [&layout.staged_library, &layout.workspace] {
        match fs::remove_dir_all(d) {
            Ok(()) => {
                parent_fsync(d);
                removed = true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("remove {}: {e}", d.display())),
        }
    }
    Ok(removed)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdoptionRecovery {
    /// No adoption was in progress.
    None,
    /// An uncommitted preparation was removed; the previous installation is unchanged.
    DiscardedPreparation,
    /// A committed adoption was published.
    Completed {
        library_id: LibraryId,
        replica_id: ReplicaId,
    },
}

/// Startup recovery: run before the library is opened (see module docs). `device_dir` is where
/// the server profile lives (required when the interrupted adoption paired a server).
pub fn recover_interrupted_adoption(
    library_root: &Path,
    sync_dir: &Path,
    device_dir: Option<&Path>,
) -> Result<AdoptionRecovery, String> {
    let layout = Layout::new(library_root, sync_dir)?;
    if !layout.workspace.exists() && !layout.staged_library.exists() {
        return Ok(AdoptionRecovery::None);
    }
    let _adopting = adoption_lock(&layout).map_err(|e| e.message)?;
    recover_with(&layout, device_dir, &no_hook)
}

fn recover_with(
    layout: &Layout,
    device_dir: Option<&Path>,
    hook: Hook<'_>,
) -> Result<AdoptionRecovery, String> {
    if !layout.workspace.exists() && !layout.staged_library.exists() {
        return Ok(AdoptionRecovery::None);
    }
    if !layout.plan().exists() {
        let removed = discard_staging(layout)?;
        log(format!(
            "recovery: removed an uncommitted adoption preparation (staging_present={removed})"
        ));
        return Ok(AdoptionRecovery::DiscardedPreparation);
    }
    let plan: AdoptPlanV1 = read_json(&layout.plan())?;
    if plan.v != PLAN_VERSION || Uuid::try_parse(&plan.adoption_id).is_err() {
        return Err(format!("invalid adoption plan (version {})", plan.v));
    }
    publish(layout, &plan, device_dir, hook)?;
    crate::sync_verified::clear_registry_for(&layout.library);
    log(format!(
        "recovery: completed adoption {} library={} replica={}",
        plan.adoption_id, plan.library_id, plan.replica_id
    ));
    Ok(AdoptionRecovery::Completed {
        library_id: plan.library_id,
        replica_id: plan.replica_id,
    })
}

// ------------------------------------------------------------------------------------------
// Adoption

/// The server pairing an adoption publishes together with the library (Kindle UI restore).
#[derive(Clone, Copy)]
pub struct AdoptProfile<'a> {
    /// Where the device's `server-profile.json` lives.
    pub device_dir: &'a Path,
    /// The (provisional) pairing with the adopted library's server.
    pub profile: &'a ServerProfileV1,
}

pub struct AdoptOptions<'a> {
    /// This Kindle's panel (blank pages and blob backgrounds are made for it).
    pub device: DeviceGeometry,
    /// Progress points, and deterministic crash points in tests: an `Err` stops the adoption
    /// where it is, as a crash would (nothing is cleaned up).
    pub hook: Hook<'a>,
    /// Set to cancel; honoured until the commit point (the preparation is then removed).
    pub cancel: Option<&'a AtomicBool>,
    /// Publish this server profile with the library.
    pub profile: Option<AdoptProfile<'a>>,
    /// Free bytes on the filesystem holding a path (`None` = unknown).
    pub available_bytes: fn(&Path) -> Option<u64>,
}

impl AdoptOptions<'_> {
    pub fn new(device: DeviceGeometry) -> Self {
        Self {
            device,
            hook: &no_hook,
            cancel: None,
            profile: None,
            available_bytes: kindle_platform::fs_space::available_bytes,
        }
    }
    fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::SeqCst))
    }
}

/// Free space an adoption of `source` stages on a panel of `device`: the new library — one
/// full-panel background per page plus page journals (about the canonical JSON size) — and the
/// sync epoch (genesis copy + checkpoint, about twice the canonical JSON), plus blobs, with a
/// quarter of the JSON-derived part and [`SPACE_MARGIN`] spare. Measured for a Vega-sized
/// library (69.7 MiB JSON, 71 pages, Kindle Scribe panel): 521.6 MiB staged. The previous
/// installation is retired by rename and needs no space.
pub fn staging_bytes(source: &VerifiedGenesis, device: DeviceGeometry) -> u64 {
    let g = source.genesis_bytes.len() as u64;
    let pages: u64 = source
        .snapshot
        .notebooks
        .iter()
        .map(|n| n.pages.len() as u64)
        .sum();
    let backgrounds = pages * u64::from(device.width) * u64::from(device.height);
    let blobs: u64 = source.blobs.values().map(|b| 2 * b.len() as u64).sum();
    (g.saturating_mul(3) + g.saturating_mul(3) / 4)
        .saturating_add(backgrounds)
        .saturating_add(blobs)
        .saturating_add(SPACE_MARGIN)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdoptionReport {
    pub adoption_id: String,
    pub server_id: ServerId,
    pub library_id: LibraryId,
    pub replica_id: ReplicaId,
    pub genesis_sha256: Sha256Hex,
    /// Server operations after the genesis, applied by the first inbound sync.
    pub head_server_seq: u64,
    pub notebooks: usize,
    pub pages: usize,
    pub previous_library_id: Option<LibraryId>,
    pub previous_replica_id: Option<ReplicaId>,
    /// Where the previous installation was kept (absent when there was none).
    pub retired_library: Option<PathBuf>,
    pub retired_sync: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdoptionOutcome {
    Adopted(AdoptionReport),
    /// The installation already syncs this library with this server: nothing changed.
    AlreadyAdopted {
        library_id: LibraryId,
        replica_id: ReplicaId,
    },
}

enum Stop {
    Refused(AdoptError),
    /// Injected crash: leave everything as it is.
    Crashed(String),
}

impl From<AdoptError> for Stop {
    fn from(e: AdoptError) -> Self {
        Stop::Refused(e)
    }
}

fn local<T>(r: Result<T, String>) -> Result<T, Stop> {
    r.map_err(|e| Stop::Refused(AdoptError::local(e)))
}

/// A point before the commit: honour cancellation, then the hook.
fn at(opts: &AdoptOptions<'_>, point: &str) -> Result<(), Stop> {
    if opts.cancelled() {
        return Err(AdoptError::new(AdoptErrorKind::Cancelled, "cancelled").into());
    }
    (opts.hook)(point).map_err(Stop::Crashed)
}

/// Adopt `source` into the installation at `library_root` + `sync_dir` (see module docs). The
/// caller must have shut down this installation's `OutboundSync`; a running one makes this
/// refuse with [`AdoptErrorKind::Busy`]. The runtime must reopen the library afterwards.
pub fn adopt_library(
    library_root: &Path,
    sync_dir: &Path,
    source: &VerifiedGenesis,
    opts: &AdoptOptions<'_>,
) -> Result<AdoptionOutcome, AdoptError> {
    let layout = Layout::new(library_root, sync_dir).map_err(AdoptError::local)?;
    if is_within(sync_dir, library_root) || is_within(library_root, sync_dir) {
        return Err(AdoptError::local(
            "the library and the sync directory must not contain each other",
        ));
    }
    if let Some(p) = opts.profile {
        if p.profile.server_id != source.target().server_id {
            return Err(AdoptError::new(
                AdoptErrorKind::ServerMismatch,
                "the server pairing is not for the selected server",
            ));
        }
    }
    // One adoption at a time; under that lock, finish or discard an earlier interrupted one
    // (which therefore cannot belong to a live preparation).
    let _adopting = adoption_lock(&layout)?;
    recover_with(&layout, opts.profile.map(|p| p.device_dir), &no_hook)
        .map_err(AdoptError::local)?;
    fs::create_dir_all(sync_dir)
        .map_err(|e| AdoptError::local(format!("create {}: {e}", sync_dir.display())))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(sync_dir.join(LOCK_FILE))
        .map_err(|e| AdoptError::local(format!("open sync lock: {e}")))?;
    lock.try_lock().map_err(|e| {
        AdoptError::new(
            AdoptErrorKind::Busy,
            format!("sync is running on {} ({e})", sync_dir.display()),
        )
    })?;

    let previous: Option<SyncStateV1> = match sync_dir.join(STATE_FILE).exists() {
        true => Some(read_json(&sync_dir.join(STATE_FILE)).map_err(AdoptError::local)?),
        false => None,
    };
    let target = source.target();
    if let Some(st) = previous
        .as_ref()
        .filter(|s| s.library_id == target.library_id)
    {
        if st.server_id == Some(target.server_id) && st.genesis_sha256 == source.genesis_sha256 {
            log(format!(
                "library {} is already adopted from server {} (replica {}); nothing to do",
                st.library_id, target.server_id, st.replica_id
            ));
            return Ok(AdoptionOutcome::AlreadyAdopted {
                library_id: st.library_id,
                replica_id: st.replica_id,
            });
        }
        return Err(AdoptError::new(
            AdoptErrorKind::LibraryMismatch,
            "this installation already syncs this library ID under another server or genesis",
        ));
    }
    let previous_replica = read_replica(sync_dir).map_err(AdoptError::local)?;
    let previous_library = read_identity(library_root)
        .ok()
        .flatten()
        .map(|i| i.library_id);

    match prepare(&layout, source, opts, previous_replica) {
        Ok(plan) => {
            drop(lock); // released only now that the committed plan fences `OutboundSync::open`
            if let Err(e) = publish(
                &layout,
                &plan,
                opts.profile.map(|p| p.device_dir),
                opts.hook,
            ) {
                return Err(AdoptError::local(format!(
                    "publication interrupted (completed at next start): {e}"
                )));
            }
            crate::sync_verified::clear_registry_for(library_root);
            let retired = |p: PathBuf| p.exists().then_some(p);
            let report = AdoptionReport {
                adoption_id: plan.adoption_id.clone(),
                server_id: plan.server_id,
                library_id: plan.library_id,
                replica_id: plan.replica_id,
                genesis_sha256: plan.genesis_sha256.clone(),
                head_server_seq: source.head_server_seq,
                notebooks: source.snapshot.notebooks.len(),
                pages: source
                    .snapshot
                    .notebooks
                    .iter()
                    .map(|n| n.pages.len())
                    .sum(),
                previous_library_id: previous_library,
                previous_replica_id: previous_replica,
                retired_library: layout
                    .retired_library(&plan.adoption_id)
                    .ok()
                    .and_then(retired),
                retired_sync: layout
                    .retired_sync(&plan.adoption_id)
                    .ok()
                    .and_then(retired),
            };
            log(format!(
                "adopted library={} server={} replica={} genesis_sha256={} head_server_seq={} notebooks={} pages={} previous_library={:?} previous_replica={:?}",
                report.library_id,
                report.server_id,
                report.replica_id,
                report.genesis_sha256,
                report.head_server_seq,
                report.notebooks,
                report.pages,
                report.previous_library_id,
                report.previous_replica_id
            ));
            Ok(AdoptionOutcome::Adopted(report))
        }
        Err(Stop::Crashed(e)) => Err(AdoptError::local(format!("interrupted: {e}"))),
        Err(Stop::Refused(e)) => {
            if let Err(c) = discard_staging(&layout) {
                log(format!("could not remove the refused preparation: {c}"));
            }
            Err(e)
        }
    }
}

fn is_within(child: &Path, parent: &Path) -> bool {
    match (fs::canonicalize(child), fs::canonicalize(parent)) {
        (Ok(c), Ok(p)) => c.starts_with(p),
        _ => false,
    }
}

fn read_replica(sync_dir: &Path) -> Result<Option<ReplicaId>, String> {
    match fs::read(sync_dir.join(REPLICA_STATE_FILE)) {
        Ok(b) => ReplicaStateV1::parse(&b)
            .map(|r| Some(r.replica_id))
            .map_err(|e| format!("replica state: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("replica state: {e}")),
    }
}

/// Build and verify the replacement; on success the plan is durable (the commit point).
fn prepare(
    layout: &Layout,
    source: &VerifiedGenesis,
    opts: &AdoptOptions<'_>,
    previous_replica: Option<ReplicaId>,
) -> Result<AdoptPlanV1, Stop> {
    let hook = opts.hook;
    let target = source.target();
    let adoption_id = Uuid::now_v7().to_string();
    for p in [
        local(layout.retired_library(&adoption_id))?,
        local(layout.retired_sync(&adoption_id))?,
        layout.staged_library.clone(),
        layout.workspace.clone(),
    ] {
        if p.exists() {
            return Err(AdoptError::local(format!("{} already exists", p.display())).into());
        }
    }
    let need = staging_bytes(source, opts.device);
    for dir in [&layout.library, &layout.sync] {
        let parent = dir.parent().unwrap_or(dir);
        match (opts.available_bytes)(parent) {
            Some(free) if free < need => {
                return Err(AdoptError::new(
                    AdoptErrorKind::InsufficientStorage,
                    format!("{free} bytes free, {need} needed"),
                )
                .into())
            }
            Some(_) => {}
            None => log("free storage unknown; staging without a space check"),
        }
    }
    at(opts, "before_staging")?;

    // 1. The new sync epoch: fresh replica, existing server library, genesis at server_seq 0.
    let d = opts.device;
    let lib = &layout.staged_library;
    let sync = &layout.staged_sync;
    local(fs::create_dir_all(sync).map_err(|e| e.to_string()))?;
    let replica = local(ensure_replica_state(sync, None))?.replica_id;
    if Some(replica) == previous_replica {
        return Err(AdoptError::local("the new replica reuses the previous replica ID").into());
    }
    let blobs: Vec<&[u8]> = source.blobs.values().map(Vec::as_slice).collect();
    local(establish_adopted_epoch(
        sync,
        replica,
        target.server_id,
        &source.genesis_bytes,
        &source.snapshot,
        &blobs,
    ))?;
    for f in CARRIED_FILES {
        match fs::read(layout.sync.join(f)) {
            Ok(b) => local(write_atomic(sync, f, &b))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(AdoptError::local(format!("read {f}: {e}")).into()),
        }
    }
    at(opts, "after_sync_epoch")?;

    // 2. A library that starts as the genesis' first notebook (its first page), bound to the
    //    target library and that notebook's identities: no placeholder content ever exists.
    let blob_dir = sync.join(BLOB_DIR);
    let crashed = std::cell::Cell::new(false);
    let cancelled = std::cell::Cell::new(false);
    let tagged = |p: &str| {
        if opts.cancelled() {
            cancelled.set(true);
            return Err("cancelled".into());
        }
        hook(p).inspect_err(|_| crashed.set(true))
    };
    let materializer = LocalMaterializer {
        library_root: lib,
        device: d,
        blob_dir: &blob_dir,
        hook: &tagged,
        scope: None,
    };
    let unsupported = |e: String| Stop::Refused(AdoptError::new(AdoptErrorKind::Unsupported, e));
    let first = &source.snapshot.notebooks[0];
    let p0 = &first.pages[0];
    let (bytes, (w, h, stride)) = materializer
        .background(&p0.background)
        .map_err(unsupported)?;
    let profile = profile_for(&p0.render).map_err(unsupported)?;
    let geo = kindle_platform::FbGeometry {
        xres: w,
        yres: h,
        xres_virtual: stride,
        yres_virtual: h,
        xoffset: 0,
        yoffset: 0,
        bits_per_pixel: 8,
        line_length: stride,
        smem_len: stride.saturating_mul(h),
    };
    local(crate::library::create_library_with_first_notebook(
        lib,
        &first.title,
        &profile,
        &geo,
        &bytes,
    ))?;
    drop(bytes);
    let (_, shell) = local(crate::sync_local::live_layout(lib))?;
    let (nb_local, pages_local) = &shell.notebooks[0];
    let identity = IdentityFileV1 {
        v: IDENTITY_FILE_VERSION,
        library_id: target.library_id,
        notebooks: BTreeMap::from([(nb_local.clone(), first.notebook_id)]),
        pages: BTreeMap::from([(
            nb_local.clone(),
            BTreeMap::from([(pages_local[0].clone(), p0.page_id)]),
        )]),
        retired_notebooks: BTreeMap::new(),
        retired_pages: BTreeMap::new(),
    };
    local(identity.validate().map_err(|e| e.to_string()))?;
    let plan_identity = Reconciled {
        identity,
        changed: true,
        allocated_library: false,
        allocated_notebooks: Vec::new(),
        allocated_pages: Vec::new(),
        retired_notebooks: Vec::new(),
        retired_pages: Vec::new(),
    };
    local(persist_planned_identity(lib, None, &plan_identity))?;
    at(opts, "after_library_shell")?;

    // 3. Materialize the rest of the genesis into the staged library.
    let report = materializer.reconcile(&source.snapshot).map_err(|e| {
        match (crashed.get(), cancelled.get()) {
            (true, _) => Stop::Crashed(e),
            (_, true) => AdoptError::new(AdoptErrorKind::Cancelled, "cancelled").into(),
            _ => unsupported(e),
        }
    })?;
    crate::sync_verified::clear_registry_for(lib);
    // This Kindle's import history, re-resolved to the adopted notebooks.
    let receipts = local(crate::nbk_import::carry_receipts(&layout.library, lib))?;
    at(opts, "after_materialize")?;

    // 4. Verify: every page, the whole canonical snapshot, identities.
    let identity = local(read_identity(lib))?
        .ok_or_else(|| Stop::from(AdoptError::local("staged library has no identity")))?;
    let pages: std::collections::HashSet<PageId> = source
        .snapshot
        .notebooks
        .iter()
        .flat_map(|n| n.pages.iter().map(|p| p.page_id))
        .collect();
    local(verify_scoped(lib, &identity, &source.snapshot, &pages))?;
    let built_sha = local(
        build_snapshot(lib, &identity)
            .and_then(|built| snapshot_sha256(&built).map_err(|e| e.to_string())),
    )?;
    let state: SyncStateV1 = local(read_json(&sync.join(STATE_FILE)))?;
    if built_sha != source.genesis_sha256
        || identity.library_id != target.library_id
        || state.library_id != target.library_id
        || state.replica_id != replica
        || state.server_id != Some(target.server_id)
        || state.genesis_sha256 != source.genesis_sha256
    {
        return Err(AdoptError::local(format!(
            "the prepared library does not reproduce the genesis (local {built_sha}, genesis {})",
            source.genesis_sha256
        ))
        .into());
    }
    log(format!(
        "prepared library={} replica={} notebooks_created={} pages_created={} strokes={} import_receipts={receipts}",
        target.library_id,
        replica,
        report.notebooks_created,
        report.pages_created,
        report.strokes_appended
    ));
    at(opts, "after_verify")?;

    // 5. Commit point (the server profile, if any, is staged first so the plan can name it).
    if let Some(p) = opts.profile {
        local(save_profile(layout.staged_profile_dir(), p.profile))?;
    }
    let plan = AdoptPlanV1 {
        v: PLAN_VERSION,
        adoption_id,
        server_id: target.server_id,
        library_id: target.library_id,
        replica_id: replica,
        genesis_sha256: source.genesis_sha256.clone(),
        profile: opts.profile.is_some(),
    };
    local(write_json(&layout.workspace, PLAN_FILE, &plan))?;
    hook("after_plan").map_err(Stop::Crashed)?;
    Ok(plan)
}

#[cfg(test)]
#[path = "sync_adopt_tests.rs"]
mod tests;
