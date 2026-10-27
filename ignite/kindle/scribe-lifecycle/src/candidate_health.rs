//! Durable, candidate-scoped launch-health evidence.
//!
//! This module deliberately has no promotion or rollback operation. It accepts a launch only when
//! the canonical `pending-health.v1` record matches both live executables, then publishes one
//! bounded observation record. Every encoded state remains explicitly unqualified.

mod assessment;

pub use assessment::assess_default_promotion;

use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::launch_health::valid_attempt_id;

pub const DEFAULT_UPDATE_DIR: &str = "/mnt/us/scribe-app/state/update";
const PENDING_NAME: &str = "pending-health.v1";
const OWNER_NAME: &str = "launch-health-owner.v1";
const OWNER_TEMP_NAME: &str = ".launch-health-owner.v1.tmp";
const EVIDENCE_NAME: &str = "launch-health-evidence.v1";
const EVIDENCE_TEMP_NAME: &str = ".launch-health-evidence.v1.tmp";
const EVIDENCE_LOCK_NAME: &str = ".launch-health-evidence.v1.lock";
const PENDING_MAX_BYTES: u64 = 4096;
const OWNER_MAX_BYTES: u64 = 512;
const EVIDENCE_MAX_BYTES: u64 = 4096;
const EXECUTABLE_MAX_BYTES: u64 = 256 * 1024 * 1024;
const OWNER_HEADER: &str = "IGNITE_LAUNCH_HEALTH_OWNER_V1";
const EVIDENCE_HEADER: &str = "IGNITE_LAUNCH_HEALTH_EVIDENCE_V1";
static LAUNCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct EvidenceError(String);

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EvidenceError {}

impl From<io::Error> for EvidenceError {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}

fn invalid(message: impl Into<String>) -> EvidenceError {
    EvidenceError(message.into())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingCandidate {
    version: String,
    transaction: String,
    pending_sha256: String,
    app_bytes: String,
    app_sha256: String,
    lifecycle_bytes: String,
    lifecycle_sha256: String,
}

impl PendingCandidate {
    fn load(update_dir: &Path, app: &Path, lifecycle: &Path) -> Result<Self, EvidenceError> {
        ensure_safe_update_dir(update_dir)?;
        let pending_path = update_dir.join(PENDING_NAME);
        let bytes = read_bounded_regular(&pending_path, PENDING_MAX_BYTES, "pending-health")?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| invalid("pending-health is not canonical UTF-8/ASCII"))?;
        if bytes.iter().filter(|byte| **byte == b'\n').count() != 14
            || !bytes.ends_with(b"\n")
            || bytes.contains(&b'\r')
            || bytes.contains(&0)
        {
            return Err(invalid(
                "pending-health is not the exact canonical 14-line encoding",
            ));
        }
        let lines: Vec<&str> = text
            .strip_suffix('\n')
            .unwrap_or(text)
            .split('\n')
            .collect();
        if lines.len() != 14
            || lines[0] != "IGNITE_PENDING_HEALTH_V1"
            || lines[1] != "schema=1"
            || lines[2] != "status=pending-health"
            || lines[9] != "retained_version=unknown"
            || lines[13] != "data_compatibility=unknown"
        {
            return Err(invalid(
                "pending-health has unsupported or malformed fields",
            ));
        }

        let version = field(lines[3], "candidate_version=")?;
        let transaction = field(lines[4], "candidate_txn=")?;
        let app_bytes = field(lines[5], "candidate_app_bytes=")?;
        let app_sha256 = field(lines[6], "candidate_app_sha256=")?;
        let lifecycle_bytes = field(lines[7], "candidate_lifecycle_bytes=")?;
        let lifecycle_sha256 = field(lines[8], "candidate_lifecycle_sha256=")?;
        let retained = field(lines[10], "retained_release_identity=sha256:")?;
        let inventory_bytes = field(lines[11], "lkg_inventory_bytes=")?;
        let inventory_sha256 = field(lines[12], "lkg_inventory_sha256=")?;

        if !valid_release_version(version) {
            return Err(invalid("pending-health candidate version is noncanonical"));
        }
        if !valid_transaction(transaction) {
            return Err(invalid("pending-health transaction is malformed"));
        }
        for value in [app_bytes, lifecycle_bytes, inventory_bytes] {
            canonical_u64(value)?;
        }
        for digest in [app_sha256, lifecycle_sha256, retained, inventory_sha256] {
            if !valid_sha256(digest) {
                return Err(invalid(
                    "pending-health contains a malformed SHA-256 digest",
                ));
            }
        }
        if retained != inventory_sha256 {
            return Err(invalid(
                "pending-health retained identity contradicts its inventory digest",
            ));
        }

        let canonical = format!(
            "IGNITE_PENDING_HEALTH_V1\n\
             schema=1\n\
             status=pending-health\n\
             candidate_version={version}\n\
             candidate_txn={transaction}\n\
             candidate_app_bytes={app_bytes}\n\
             candidate_app_sha256={app_sha256}\n\
             candidate_lifecycle_bytes={lifecycle_bytes}\n\
             candidate_lifecycle_sha256={lifecycle_sha256}\n\
             retained_version=unknown\n\
             retained_release_identity=sha256:{retained}\n\
             lkg_inventory_bytes={inventory_bytes}\n\
             lkg_inventory_sha256={inventory_sha256}\n\
             data_compatibility=unknown\n"
        );
        if canonical.as_bytes() != bytes {
            return Err(invalid("pending-health bytes are noncanonical"));
        }

        verify_executable(app, app_bytes, app_sha256, "candidate application")?;
        verify_executable(
            lifecycle,
            lifecycle_bytes,
            lifecycle_sha256,
            "candidate lifecycle",
        )?;

        Ok(Self {
            version: version.to_owned(),
            transaction: transaction.to_owned(),
            pending_sha256: sha256_bytes(&bytes),
            app_bytes: app_bytes.to_owned(),
            app_sha256: app_sha256.to_owned(),
            lifecycle_bytes: lifecycle_bytes.to_owned(),
            lifecycle_sha256: lifecycle_sha256.to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    Spawned,
    Ready,
    ReadyRunning,
    SpawnFailed,
    ExitedSuccess,
    ExitedFailure,
    TimedOut,
    IntentionalStop,
    IntentionalStopFailed,
    Interrupted,
    Unavailable,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Spawned => "spawned",
            Self::Ready => "ready",
            Self::ReadyRunning => "ready_running",
            Self::SpawnFailed => "spawn_failed",
            Self::ExitedSuccess => "exited_success",
            Self::ExitedFailure => "exited_failure",
            Self::TimedOut => "timed_out",
            Self::IntentionalStop => "intentional_stop",
            Self::IntentionalStopFailed => "intentional_stop_failed",
            Self::Interrupted => "interrupted",
            Self::Unavailable => "unavailable",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "spawned" => Self::Spawned,
            "ready" => Self::Ready,
            "ready_running" => Self::ReadyRunning,
            "spawn_failed" => Self::SpawnFailed,
            "exited_success" => Self::ExitedSuccess,
            "exited_failure" => Self::ExitedFailure,
            "timed_out" => Self::TimedOut,
            "intentional_stop" => Self::IntentionalStop,
            "intentional_stop_failed" => Self::IntentionalStopFailed,
            "interrupted" => Self::Interrupted,
            "unavailable" => Self::Unavailable,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessState {
    NotSpawned,
    Running,
    Exited,
    Stopped,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadinessState {
    Awaiting,
    Ready,
    TimedOut,
    Unavailable,
    Interrupted,
}

impl ReadinessState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Awaiting => "awaiting",
            Self::Ready => "ready",
            Self::TimedOut => "timed_out",
            Self::Unavailable => "unavailable",
            Self::Interrupted => "interrupted",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "awaiting" => Self::Awaiting,
            "ready" => Self::Ready,
            "timed_out" => Self::TimedOut,
            "unavailable" => Self::Unavailable,
            "interrupted" => Self::Interrupted,
            _ => return None,
        })
    }
}

impl ProcessState {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotSpawned => "not_spawned",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Stopped => "stopped",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "not_spawned" => Self::NotSpawned,
            "running" => Self::Running,
            "exited" => Self::Exited,
            "stopped" => Self::Stopped,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EvidenceRecord {
    candidate: PendingCandidate,
    launch_uuid: String,
    outcome: Outcome,
    ready: bool,
    readiness: ReadinessState,
    process: ProcessState,
    complete: bool,
    ready_elapsed_ms: Option<u64>,
    post_ready_observed_ms: Option<u64>,
}

impl EvidenceRecord {
    fn encode(&self) -> Vec<u8> {
        format!(
            "{EVIDENCE_HEADER}\n\
             schema=1\n\
             candidate_version={}\n\
             candidate_txn={}\n\
             pending_sha256={}\n\
             candidate_app_bytes={}\n\
             candidate_app_sha256={}\n\
             candidate_lifecycle_bytes={}\n\
             candidate_lifecycle_sha256={}\n\
             launch_uuid={}\n\
             outcome={}\n\
             ready={}\n\
             readiness={}\n\
             process={}\n\
             observation_complete={}\n\
             ready_elapsed_ms={}\n\
             post_ready_observed_ms={}\n\
             qualification=unqualified\n\
             data_compatibility=unknown\n",
            self.candidate.version,
            self.candidate.transaction,
            self.candidate.pending_sha256,
            self.candidate.app_bytes,
            self.candidate.app_sha256,
            self.candidate.lifecycle_bytes,
            self.candidate.lifecycle_sha256,
            self.launch_uuid,
            self.outcome.as_str(),
            bool_text(self.ready),
            self.readiness.as_str(),
            self.process.as_str(),
            bool_text(self.complete),
            option_u64(self.ready_elapsed_ms),
            option_u64(self.post_ready_observed_ms),
        )
        .into_bytes()
    }

    fn parse(bytes: &[u8]) -> Result<Self, EvidenceError> {
        if bytes.is_empty()
            || bytes.len() as u64 > EVIDENCE_MAX_BYTES
            || bytes.iter().filter(|byte| **byte == b'\n').count() != 19
            || !bytes.ends_with(b"\n")
            || bytes.contains(&b'\r')
            || bytes.contains(&0)
        {
            return Err(invalid("health evidence has noncanonical framing"));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| invalid("health evidence is not canonical UTF-8/ASCII"))?;
        let lines: Vec<&str> = text
            .strip_suffix('\n')
            .unwrap_or(text)
            .split('\n')
            .collect();
        if lines.len() != 19
            || lines[0] != EVIDENCE_HEADER
            || lines[1] != "schema=1"
            || lines[17] != "qualification=unqualified"
            || lines[18] != "data_compatibility=unknown"
        {
            return Err(invalid(
                "health evidence has unsupported or malformed fields",
            ));
        }
        let candidate = PendingCandidate {
            version: field(lines[2], "candidate_version=")?.to_owned(),
            transaction: field(lines[3], "candidate_txn=")?.to_owned(),
            pending_sha256: field(lines[4], "pending_sha256=")?.to_owned(),
            app_bytes: field(lines[5], "candidate_app_bytes=")?.to_owned(),
            app_sha256: field(lines[6], "candidate_app_sha256=")?.to_owned(),
            lifecycle_bytes: field(lines[7], "candidate_lifecycle_bytes=")?.to_owned(),
            lifecycle_sha256: field(lines[8], "candidate_lifecycle_sha256=")?.to_owned(),
        };
        if !valid_release_version(&candidate.version)
            || !valid_transaction(&candidate.transaction)
            || !valid_sha256(&candidate.pending_sha256)
            || !valid_sha256(&candidate.app_sha256)
            || !valid_sha256(&candidate.lifecycle_sha256)
        {
            return Err(invalid("health evidence candidate identity is malformed"));
        }
        canonical_u64(&candidate.app_bytes)?;
        canonical_u64(&candidate.lifecycle_bytes)?;

        let launch_uuid = field(lines[9], "launch_uuid=")?;
        if !valid_attempt_id(launch_uuid) {
            return Err(invalid("health evidence launch UUID is malformed"));
        }
        let outcome = Outcome::parse(field(lines[10], "outcome=")?)
            .ok_or_else(|| invalid("health evidence outcome is unsupported"))?;
        let ready = parse_bool(field(lines[11], "ready=")?)?;
        let readiness = ReadinessState::parse(field(lines[12], "readiness=")?)
            .ok_or_else(|| invalid("health evidence readiness state is unsupported"))?;
        let process = ProcessState::parse(field(lines[13], "process=")?)
            .ok_or_else(|| invalid("health evidence process state is unsupported"))?;
        let complete = parse_bool(field(lines[14], "observation_complete=")?)?;
        let ready_elapsed_ms = parse_option_u64(field(lines[15], "ready_elapsed_ms=")?)?;
        let post_ready_observed_ms =
            parse_option_u64(field(lines[16], "post_ready_observed_ms=")?)?;
        let record = Self {
            candidate,
            launch_uuid: launch_uuid.to_owned(),
            outcome,
            ready,
            readiness,
            process,
            complete,
            ready_elapsed_ms,
            post_ready_observed_ms,
        };
        record.validate_semantics()?;
        if record.encode() != bytes {
            return Err(invalid("health evidence bytes are noncanonical"));
        }
        Ok(record)
    }

    fn validate_semantics(&self) -> Result<(), EvidenceError> {
        let no_ready_timing =
            self.ready_elapsed_ms.is_none() && self.post_ready_observed_ms.is_none();
        let has_ready_timing =
            self.ready_elapsed_ms.is_some() && self.post_ready_observed_ms.is_some();
        if self.ready != has_ready_timing
            || self.ready != (self.readiness == ReadinessState::Ready)
            || (!self.ready && !no_ready_timing)
        {
            return Err(invalid(
                "health evidence has contradictory readiness timing",
            ));
        }
        let valid = match self.outcome {
            Outcome::Spawned => {
                !self.ready
                    && self.readiness == ReadinessState::Awaiting
                    && self.process == ProcessState::Running
                    && !self.complete
            }
            Outcome::Ready => {
                self.ready
                    && self.readiness == ReadinessState::Ready
                    && self.process == ProcessState::Running
                    && !self.complete
                    && self.post_ready_observed_ms == Some(0)
            }
            Outcome::ReadyRunning => {
                self.ready
                    && self.readiness == ReadinessState::Ready
                    && self.process == ProcessState::Running
                    && !self.complete
                    && self.post_ready_observed_ms.is_some_and(|value| value > 0)
            }
            Outcome::SpawnFailed => {
                !self.ready
                    && self.readiness == ReadinessState::Awaiting
                    && self.process == ProcessState::NotSpawned
                    && self.complete
            }
            Outcome::ExitedSuccess | Outcome::ExitedFailure => {
                self.process == ProcessState::Exited && self.complete
            }
            Outcome::TimedOut => {
                !self.ready
                    && self.readiness == ReadinessState::TimedOut
                    && self.process == ProcessState::Running
                    && !self.complete
            }
            Outcome::IntentionalStop => self.process == ProcessState::Stopped && self.complete,
            Outcome::IntentionalStopFailed => self.process == ProcessState::Exited && self.complete,
            Outcome::Interrupted => {
                self.readiness != ReadinessState::Awaiting
                    && self.process == ProcessState::Unknown
                    && !self.complete
            }
            Outcome::Unavailable => {
                !self.ready
                    && self.readiness == ReadinessState::Unavailable
                    && self.process == ProcessState::Running
                    && !self.complete
            }
        };
        if valid {
            Ok(())
        } else {
            Err(invalid("health evidence fields are contradictory"))
        }
    }
}

/// Filesystem-free ordering identity captured before the child is spawned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchAttempt {
    launch_uuid: String,
    monotonic_ns: u64,
    process_id: u32,
    sequence: u64,
}

impl LaunchAttempt {
    pub fn capture(launch_uuid: &str) -> Result<Self, EvidenceError> {
        if !valid_attempt_id(launch_uuid) {
            return Err(invalid("launch UUID is malformed"));
        }
        let mut time: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } != 0 {
            return Err(invalid(format!(
                "cannot read monotonic launch order: {}",
                io::Error::last_os_error()
            )));
        }
        let seconds = u64::try_from(time.tv_sec)
            .map_err(|_| invalid("monotonic launch order is negative"))?;
        let nanoseconds = u64::try_from(time.tv_nsec)
            .ok()
            .filter(|value| *value < 1_000_000_000)
            .ok_or_else(|| invalid("monotonic launch order is malformed"))?;
        let monotonic_ns = seconds
            .checked_mul(1_000_000_000)
            .and_then(|value| value.checked_add(nanoseconds))
            .ok_or_else(|| invalid("monotonic launch order overflowed"))?;
        let sequence = LAUNCH_SEQUENCE
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                value.checked_add(1)
            })
            .map_err(|_| invalid("launch sequence exhausted"))?;
        Ok(Self {
            launch_uuid: launch_uuid.to_owned(),
            monotonic_ns,
            process_id: std::process::id(),
            sequence,
        })
    }

    #[cfg(test)]
    fn controlled(launch_uuid: &str, monotonic_ns: u64, process_id: u32, sequence: u64) -> Self {
        assert!(valid_attempt_id(launch_uuid));
        Self {
            launch_uuid: launch_uuid.to_owned(),
            monotonic_ns,
            process_id,
            sequence,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OwnerRecord {
    boot_id: String,
    attempt: LaunchAttempt,
}

impl OwnerRecord {
    fn order_key(&self) -> (u64, u32, u64) {
        (
            self.attempt.monotonic_ns,
            self.attempt.process_id,
            self.attempt.sequence,
        )
    }
}

/// Durable ownership of evidence publication for one launch UUID.
pub struct LaunchOwnership {
    update_dir: PathBuf,
    owner: OwnerRecord,
}

impl LaunchOwnership {
    fn claim(update_dir: &Path, attempt: LaunchAttempt) -> Result<Self, EvidenceError> {
        ensure_safe_update_dir(update_dir)?;
        // Do not create updater state for ordinary first-install launches.
        read_bounded_regular(
            &update_dir.join(PENDING_NAME),
            PENDING_MAX_BYTES,
            "pending-health",
        )?;
        let owner = OwnerRecord {
            boot_id: current_boot_id()?,
            attempt,
        };
        publish_owner(update_dir, &owner)?;
        Ok(Self {
            update_dir: update_dir.to_owned(),
            owner,
        })
    }
}

/// Mutable observation for exactly one candidate launch. Publication errors never affect the child.
pub struct EvidenceRecorder {
    update_dir: PathBuf,
    app: PathBuf,
    lifecycle: PathBuf,
    candidate: PendingCandidate,
    owner: OwnerRecord,
    started_at: Instant,
    ready_at: Option<Instant>,
    running_checkpoint_attempted: bool,
    readiness: ReadinessState,
    claimed: bool,
}

impl EvidenceRecorder {
    pub fn prepare(
        ownership: LaunchOwnership,
        app: &Path,
        lifecycle: &Path,
        started_at: Instant,
    ) -> Result<Self, EvidenceError> {
        require_owner(&ownership.update_dir, &ownership.owner)?;
        let candidate = PendingCandidate::load(&ownership.update_dir, app, lifecycle)?;
        // Hashing can take long enough for a replacement launch to claim ownership.
        require_owner(&ownership.update_dir, &ownership.owner)?;
        Ok(Self {
            update_dir: ownership.update_dir,
            app: app.to_owned(),
            lifecycle: lifecycle.to_owned(),
            candidate,
            owner: ownership.owner,
            started_at,
            ready_at: None,
            running_checkpoint_attempted: false,
            readiness: ReadinessState::Awaiting,
            claimed: false,
        })
    }

    pub fn record_spawned(&mut self) -> Result<(), EvidenceError> {
        self.publish(
            Outcome::Spawned,
            ProcessState::Running,
            false,
            Instant::now(),
        )
    }

    pub fn record_spawn_failed(&mut self) -> Result<(), EvidenceError> {
        self.publish(
            Outcome::SpawnFailed,
            ProcessState::NotSpawned,
            true,
            Instant::now(),
        )
    }

    pub fn record_unavailable(&mut self) -> Result<(), EvidenceError> {
        self.readiness = ReadinessState::Unavailable;
        self.publish(
            Outcome::Unavailable,
            ProcessState::Running,
            false,
            Instant::now(),
        )
    }

    pub fn record_ready(&mut self, now: Instant) -> Result<(), EvidenceError> {
        self.ready_at.get_or_insert(now);
        self.readiness = ReadinessState::Ready;
        self.publish(Outcome::Ready, ProcessState::Running, false, now)
    }

    /// Publish one monotonic stability checkpoint. This is evidence only, never qualification.
    pub fn maybe_record_running(
        &mut self,
        now: Instant,
        checkpoint: Duration,
    ) -> Result<bool, EvidenceError> {
        let Some(ready_at) = self.ready_at else {
            return Ok(false);
        };
        if self.running_checkpoint_attempted || now.saturating_duration_since(ready_at) < checkpoint
        {
            return Ok(false);
        }
        // A checkpoint is deliberately one-shot. Mark it attempted before hashing or publication
        // so an I/O failure cannot turn the watcher into an unbounded retry loop.
        self.running_checkpoint_attempted = true;
        self.publish(Outcome::ReadyRunning, ProcessState::Running, false, now)?;
        Ok(true)
    }

    pub fn record_timeout(&mut self, now: Instant) -> Result<(), EvidenceError> {
        self.readiness = ReadinessState::TimedOut;
        self.publish(Outcome::TimedOut, ProcessState::Running, false, now)
    }

    pub fn record_exit(&mut self, successful: bool, now: Instant) -> Result<(), EvidenceError> {
        self.publish(
            if successful {
                Outcome::ExitedSuccess
            } else {
                Outcome::ExitedFailure
            },
            ProcessState::Exited,
            true,
            now,
        )
    }

    pub fn record_intentional_stop(
        &mut self,
        successful: bool,
        now: Instant,
    ) -> Result<(), EvidenceError> {
        let (outcome, process) = if successful {
            (Outcome::IntentionalStop, ProcessState::Stopped)
        } else {
            (Outcome::IntentionalStopFailed, ProcessState::Exited)
        };
        self.publish(outcome, process, true, now)
    }

    pub fn record_interrupted(&mut self, now: Instant) -> Result<(), EvidenceError> {
        if self.readiness == ReadinessState::Awaiting {
            self.readiness = ReadinessState::Interrupted;
        }
        self.publish(Outcome::Interrupted, ProcessState::Unknown, false, now)
    }

    fn publish(
        &mut self,
        outcome: Outcome,
        process: ProcessState,
        complete: bool,
        now: Instant,
    ) -> Result<(), EvidenceError> {
        let current = PendingCandidate::load(&self.update_dir, &self.app, &self.lifecycle)?;
        if current != self.candidate {
            return Err(invalid(
                "pending candidate or live executable identity changed during observation",
            ));
        }
        let ready_elapsed_ms = self
            .ready_at
            .map(|ready_at| duration_ms(ready_at.saturating_duration_since(self.started_at)));
        let post_ready_observed_ms = self
            .ready_at
            .map(|ready_at| duration_ms(now.saturating_duration_since(ready_at)));
        let record = EvidenceRecord {
            candidate: self.candidate.clone(),
            launch_uuid: self.owner.attempt.launch_uuid.clone(),
            outcome,
            ready: self.ready_at.is_some(),
            readiness: self.readiness,
            process,
            complete,
            ready_elapsed_ms,
            post_ready_observed_ms,
        };
        record.validate_semantics()?;
        let evidence_must_exist = self.claimed;
        // Even if first publication fails, this recorder must never overwrite a later launch.
        self.claimed = true;
        publish_record(
            &self.update_dir,
            &self.owner,
            &record,
            evidence_must_exist,
            || Ok(()),
        )
    }
}

#[derive(Clone, Copy)]
pub enum InitialEvidence {
    Spawned,
    Unavailable,
    SpawnFailed,
}

impl InitialEvidence {
    fn outcome(self) -> &'static str {
        match self {
            Self::Spawned => "spawned",
            Self::Unavailable => "unavailable",
            Self::SpawnFailed => "spawn_failed",
        }
    }
}

pub struct EvidenceReport {
    pub launch_uuid: String,
    pub outcome: &'static str,
    pub result: Result<(), EvidenceError>,
    /// True only when this report is the worker's final report. A failed publication is final
    /// because evidence workers deliberately stop after the first persistence error.
    pub terminal: bool,
}

enum EvidenceCommand {
    Ready(Instant),
    RunningCheckpoint(Instant, Duration),
    TimedOut(Instant),
    Interrupted(Instant),
    Exited(bool, Instant),
    IntentionalStop(bool, Instant),
}

/// Nonblocking lifecycle-side handle for one asynchronous evidence worker.
pub struct EvidenceObserver {
    sender: Sender<EvidenceCommand>,
    ready_at: Option<Instant>,
    checkpoint_attempted: bool,
    available: bool,
}

struct WorkerResources {
    lifecycle: Option<PathBuf>,
    activity: Option<crate::launch_gate::EvidenceWorkerActivity>,
}

impl EvidenceObserver {
    pub fn start<F>(
        update_dir: PathBuf,
        app: PathBuf,
        attempt: LaunchAttempt,
        started_at: Instant,
        initial: InitialEvidence,
        reporter: F,
    ) -> io::Result<Self>
    where
        F: Fn(EvidenceReport) + Send + 'static,
    {
        Self::start_inner(
            update_dir,
            app,
            WorkerResources {
                lifecycle: None,
                activity: None,
            },
            attempt,
            started_at,
            initial,
            reporter,
        )
    }

    pub fn start_tracked<F>(
        update_dir: PathBuf,
        app: PathBuf,
        attempt: LaunchAttempt,
        started_at: Instant,
        initial: InitialEvidence,
        activity: crate::launch_gate::EvidenceWorkerActivity,
        reporter: F,
    ) -> io::Result<Self>
    where
        F: Fn(EvidenceReport) + Send + 'static,
    {
        Self::start_inner(
            update_dir,
            app,
            WorkerResources {
                lifecycle: None,
                activity: Some(activity),
            },
            attempt,
            started_at,
            initial,
            reporter,
        )
    }

    #[cfg(test)]
    fn start_with_lifecycle<F>(
        update_dir: PathBuf,
        app: PathBuf,
        lifecycle: PathBuf,
        attempt: LaunchAttempt,
        started_at: Instant,
        initial: InitialEvidence,
        reporter: F,
    ) -> io::Result<Self>
    where
        F: Fn(EvidenceReport) + Send + 'static,
    {
        Self::start_inner(
            update_dir,
            app,
            WorkerResources {
                lifecycle: Some(lifecycle),
                activity: None,
            },
            attempt,
            started_at,
            initial,
            reporter,
        )
    }

    fn start_inner<F>(
        update_dir: PathBuf,
        app: PathBuf,
        resources: WorkerResources,
        attempt: LaunchAttempt,
        started_at: Instant,
        initial: InitialEvidence,
        reporter: F,
    ) -> io::Result<Self>
    where
        F: Fn(EvidenceReport) + Send + 'static,
    {
        let worker_uuid = attempt.launch_uuid.clone();
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("ignite-health-evidence".to_owned())
            .spawn(move || {
                // This token was acquired by the lifecycle host before child spawn. Holding it
                // through the complete worker closes the spawn-to-thread-start quiescence gap.
                let _activity = resources.activity;
                let ownership = match LaunchOwnership::claim(&update_dir, attempt) {
                    Ok(ownership) => ownership,
                    Err(error) => {
                        reporter(EvidenceReport {
                            launch_uuid: worker_uuid,
                            outcome: "ownership",
                            result: Err(error),
                            terminal: true,
                        });
                        return;
                    }
                };
                let lifecycle = match resources.lifecycle.map_or_else(std::env::current_exe, Ok) {
                    Ok(lifecycle) => lifecycle,
                    Err(error) => {
                        reporter(EvidenceReport {
                            launch_uuid: worker_uuid,
                            outcome: "setup",
                            result: Err(error.into()),
                            terminal: true,
                        });
                        return;
                    }
                };
                let mut recorder =
                    match EvidenceRecorder::prepare(ownership, &app, &lifecycle, started_at) {
                        Ok(recorder) => recorder,
                        Err(error) => {
                            reporter(EvidenceReport {
                                launch_uuid: worker_uuid,
                                outcome: "setup",
                                result: Err(error),
                                terminal: true,
                            });
                            return;
                        }
                    };
                let initial_result = match initial {
                    InitialEvidence::Spawned => recorder.record_spawned(),
                    InitialEvidence::Unavailable => recorder.record_unavailable(),
                    InitialEvidence::SpawnFailed => recorder.record_spawn_failed(),
                };
                let initial_ok = initial_result.is_ok();
                reporter(EvidenceReport {
                    launch_uuid: worker_uuid.clone(),
                    outcome: initial.outcome(),
                    result: initial_result,
                    terminal: !initial_ok || matches!(initial, InitialEvidence::SpawnFailed),
                });
                if !initial_ok || matches!(initial, InitialEvidence::SpawnFailed) {
                    return;
                }

                for command in receiver {
                    let (outcome, result, terminal) = match command {
                        EvidenceCommand::Ready(now) => ("ready", recorder.record_ready(now), false),
                        EvidenceCommand::RunningCheckpoint(now, checkpoint) => {
                            match recorder.maybe_record_running(now, checkpoint) {
                                Ok(false) => continue,
                                Ok(true) => ("ready_running", Ok(()), false),
                                Err(error) => ("ready_running", Err(error), false),
                            }
                        }
                        EvidenceCommand::TimedOut(now) => {
                            ("timed_out", recorder.record_timeout(now), false)
                        }
                        EvidenceCommand::Interrupted(now) => {
                            ("interrupted", recorder.record_interrupted(now), false)
                        }
                        EvidenceCommand::Exited(successful, now) => {
                            ("exited", recorder.record_exit(successful, now), true)
                        }
                        EvidenceCommand::IntentionalStop(successful, now) => (
                            if successful {
                                "intentional_stop"
                            } else {
                                "intentional_stop_failed"
                            },
                            recorder.record_intentional_stop(successful, now),
                            true,
                        ),
                    };
                    let publication_ok = result.is_ok();
                    reporter(EvidenceReport {
                        launch_uuid: worker_uuid.clone(),
                        outcome,
                        result,
                        terminal: !publication_ok || terminal,
                    });
                    if !publication_ok || terminal {
                        return;
                    }
                }
                reporter(EvidenceReport {
                    launch_uuid: worker_uuid,
                    outcome: "abandoned",
                    result: Err(invalid(
                        "evidence command channel closed before a terminal observation",
                    )),
                    terminal: true,
                });
            })?;
        Ok(Self {
            sender,
            ready_at: None,
            checkpoint_attempted: false,
            available: true,
        })
    }

    pub fn record_ready(&mut self, now: Instant) {
        self.ready_at.get_or_insert(now);
        self.send(EvidenceCommand::Ready(now));
    }

    pub fn maybe_record_running(&mut self, now: Instant, checkpoint: Duration) {
        let Some(ready_at) = self.ready_at else {
            return;
        };
        if self.checkpoint_attempted || now.saturating_duration_since(ready_at) < checkpoint {
            return;
        }
        self.checkpoint_attempted = true;
        self.send(EvidenceCommand::RunningCheckpoint(now, checkpoint));
    }

    pub fn record_timeout(&mut self, now: Instant) {
        self.send(EvidenceCommand::TimedOut(now));
    }

    pub fn record_interrupted(&mut self, now: Instant) {
        self.send(EvidenceCommand::Interrupted(now));
    }

    pub fn record_exit(&mut self, successful: bool, now: Instant) {
        self.send(EvidenceCommand::Exited(successful, now));
    }

    pub fn record_intentional_stop(&mut self, successful: bool, now: Instant) {
        self.send(EvidenceCommand::IntentionalStop(successful, now));
    }

    fn send(&mut self, command: EvidenceCommand) {
        if self.available && self.sender.send(command).is_err() {
            self.available = false;
        }
    }
}

fn owner_bytes(owner: &OwnerRecord) -> Vec<u8> {
    format!(
        "{OWNER_HEADER}\n\
         schema=1\n\
         boot_id={}\n\
         monotonic_ns={}\n\
         process_id={}\n\
         sequence={}\n\
         launch_uuid={}\n",
        owner.boot_id,
        owner.attempt.monotonic_ns,
        owner.attempt.process_id,
        owner.attempt.sequence,
        owner.attempt.launch_uuid,
    )
    .into_bytes()
}

fn parse_owner(bytes: &[u8]) -> Result<OwnerRecord, EvidenceError> {
    if bytes.is_empty()
        || bytes.len() as u64 > OWNER_MAX_BYTES
        || bytes.iter().filter(|byte| **byte == b'\n').count() != 7
        || !bytes.ends_with(b"\n")
        || bytes.contains(&b'\r')
        || bytes.contains(&0)
    {
        return Err(invalid("launch-health owner has noncanonical framing"));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid("launch-health owner is not canonical UTF-8/ASCII"))?;
    let lines: Vec<&str> = text
        .strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .collect();
    if lines.len() != 7 || lines[0] != OWNER_HEADER || lines[1] != "schema=1" {
        return Err(invalid(
            "launch-health owner has unsupported or malformed fields",
        ));
    }
    let boot_id = field(lines[2], "boot_id=")?;
    let monotonic_ns = canonical_u64(field(lines[3], "monotonic_ns=")?)?;
    let process_id = canonical_u64(field(lines[4], "process_id=")?)?
        .try_into()
        .map_err(|_| invalid("launch-health owner process ID is out of range"))?;
    let sequence = canonical_u64(field(lines[5], "sequence=")?)?;
    let launch_uuid = field(lines[6], "launch_uuid=")?;
    if !valid_attempt_id(boot_id) || !valid_attempt_id(launch_uuid) {
        return Err(invalid("launch-health owner identity is malformed"));
    }
    let owner = OwnerRecord {
        boot_id: boot_id.to_owned(),
        attempt: LaunchAttempt {
            launch_uuid: launch_uuid.to_owned(),
            monotonic_ns,
            process_id,
            sequence,
        },
    };
    if owner_bytes(&owner) != bytes {
        return Err(invalid("launch-health owner bytes are noncanonical"));
    }
    Ok(owner)
}

fn current_boot_id() -> Result<String, EvidenceError> {
    let path = Path::new("/proc/sys/kernel/random/boot_id");
    refuse_symlink(path, "kernel boot ID")?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| invalid(format!("cannot read kernel boot ID: {error}")))?;
    let mut bytes = Vec::with_capacity(38);
    Read::by_ref(&mut file).take(38).read_to_end(&mut bytes)?;
    if bytes.len() != 37 || !bytes.ends_with(b"\n") {
        return Err(invalid("kernel boot ID has noncanonical framing"));
    }
    let text =
        std::str::from_utf8(&bytes[..36]).map_err(|_| invalid("kernel boot ID is not ASCII"))?;
    if [8, 13, 18, 23]
        .into_iter()
        .any(|index| text.as_bytes()[index] != b'-')
    {
        return Err(invalid("kernel boot ID is malformed"));
    }
    let normalized: String = text.chars().filter(|character| *character != '-').collect();
    if !valid_attempt_id(&normalized) {
        return Err(invalid("kernel boot ID is noncanonical"));
    }
    Ok(normalized)
}

fn publish_owner(update_dir: &Path, owner: &OwnerRecord) -> Result<(), EvidenceError> {
    ensure_safe_update_dir(update_dir)?;
    let target = update_dir.join(OWNER_NAME);
    let temporary = update_dir.join(OWNER_TEMP_NAME);
    refuse_symlink(&target, "launch-health owner")?;
    refuse_symlink(&temporary, "launch-health owner temporary")?;
    let _guard = acquire_publication_lock(update_dir)?;

    match fs::symlink_metadata(&target) {
        Ok(_) => {
            let bytes = read_bounded_regular(&target, OWNER_MAX_BYTES, "launch-health owner")?;
            let current = parse_owner(&bytes)?;
            if current == *owner {
                return Ok(());
            }
            if current.boot_id == owner.boot_id && current.order_key() >= owner.order_key() {
                return Err(invalid(
                    "a newer launch already owns health evidence; stale claim refused",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    if temporary.exists() {
        let metadata = fs::symlink_metadata(&temporary)?;
        if !metadata.file_type().is_file() {
            return Err(invalid("launch-health owner temporary path is unsafe"));
        }
        fs::remove_file(&temporary)?;
    }
    let result = (|| -> Result<(), EvidenceError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temporary)?;
        let encoded = owner_bytes(owner);
        file.write_all(&encoded)?;
        file.sync_all()?;
        fs::rename(&temporary, &target)?;
        sync_directory(update_dir)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn require_owner(update_dir: &Path, owner: &OwnerRecord) -> Result<(), EvidenceError> {
    ensure_safe_update_dir(update_dir)?;
    require_owner_locked(update_dir, owner)
}

fn require_owner_locked(update_dir: &Path, owner: &OwnerRecord) -> Result<(), EvidenceError> {
    let bytes = read_bounded_regular(
        &update_dir.join(OWNER_NAME),
        OWNER_MAX_BYTES,
        "launch-health owner",
    )?;
    if parse_owner(&bytes)? != *owner {
        return Err(invalid(
            "a newer launch owns health evidence; stale publication refused",
        ));
    }
    Ok(())
}

fn publish_record<F>(
    update_dir: &Path,
    owner: &OwnerRecord,
    record: &EvidenceRecord,
    evidence_must_exist: bool,
    before_rename: F,
) -> Result<(), EvidenceError>
where
    F: FnOnce() -> io::Result<()>,
{
    ensure_safe_update_dir(update_dir)?;
    let target = update_dir.join(EVIDENCE_NAME);
    let temporary = update_dir.join(EVIDENCE_TEMP_NAME);
    refuse_symlink(&target, "health evidence")?;
    refuse_symlink(&temporary, "health evidence temporary")?;
    let _guard = acquire_publication_lock(update_dir)?;
    require_owner_locked(update_dir, owner)?;

    if evidence_must_exist {
        let current_bytes = read_bounded_regular(&target, EVIDENCE_MAX_BYTES, "health evidence")?;
        let current = EvidenceRecord::parse(&current_bytes)?;
        if current.launch_uuid != record.launch_uuid {
            return Err(invalid(
                "a newer launch owns the health evidence record; stale update refused",
            ));
        }
    }

    if temporary.exists() {
        let metadata = fs::symlink_metadata(&temporary)?;
        if !metadata.file_type().is_file() {
            return Err(invalid("health evidence temporary path is unsafe"));
        }
        fs::remove_file(&temporary)?;
    }
    let result = (|| -> Result<(), EvidenceError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temporary)?;
        let encoded = record.encode();
        if encoded.len() as u64 > EVIDENCE_MAX_BYTES {
            return Err(invalid("health evidence exceeds its size bound"));
        }
        file.write_all(&encoded)?;
        file.sync_all()?;
        before_rename()?;
        fs::rename(&temporary, &target)?;
        sync_directory(update_dir)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn acquire_publication_lock(update_dir: &Path) -> Result<PublicationLock, EvidenceError> {
    let lock_path = update_dir.join(EVIDENCE_LOCK_NAME);
    refuse_symlink(&lock_path, "health evidence lock")?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|error| invalid(format!("cannot open health evidence lock: {error}")))?;
    lock.set_permissions(fs::Permissions::from_mode(0o600))?;
    PublicationLock::acquire(lock)
}

/// Exclusive evidence boundary used only after the lifecycle launch gate, updater transaction
/// lock, and evidence-worker quiescence have all been established. Unlike publication, assessment
/// must not create a missing lock file because that would manufacture coordination state.
pub(crate) struct DecisionEvidenceLock {
    _lock: File,
}

pub(crate) fn acquire_decision_evidence_lock(
    update_dir: &Path,
    timeout: Duration,
) -> Result<DecisionEvidenceLock, EvidenceError> {
    ensure_safe_update_dir(update_dir)?;
    let lock_path = update_dir.join(EVIDENCE_LOCK_NAME);
    refuse_symlink(&lock_path, "health evidence lock")?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|error| {
            invalid(format!(
                "cannot open existing health evidence lock: {error}"
            ))
        })?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(invalid(
            "health evidence lock is not a singly linked regular file",
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(invalid(
            "health evidence lock is not owned by the effective user",
        ));
    }
    let path_metadata = fs::symlink_metadata(&lock_path)?;
    if path_metadata.file_type().is_symlink()
        || path_metadata.dev() != metadata.dev()
        || path_metadata.ino() != metadata.ino()
    {
        return Err(invalid("health evidence lock changed while it was opened"));
    }
    let deadline = Instant::now() + timeout;
    loop {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(DecisionEvidenceLock { _lock: file });
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
        ) {
            return Err(error.into());
        }
        if Instant::now() >= deadline {
            return Err(invalid(
                "health evidence lock remained busy through the decision deadline",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

struct PublicationLock(File);

impl PublicationLock {
    fn acquire(file: File) -> Result<Self, EvidenceError> {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self(file))
    }
}

impl Drop for PublicationLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn sync_directory(path: &Path) -> Result<(), EvidenceError> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    directory.sync_all()?;
    Ok(())
}

fn ensure_safe_update_dir(update_dir: &Path) -> Result<(), EvidenceError> {
    ensure_no_symlink_components(update_dir)?;
    let metadata = fs::symlink_metadata(update_dir)
        .map_err(|error| invalid(format!("update state directory unavailable: {error}")))?;
    if !metadata.file_type().is_dir() {
        return Err(invalid("update state path is not a directory"));
    }
    Ok(())
}

fn ensure_no_symlink_components(path: &Path) -> Result<(), EvidenceError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "unsafe symlink in update state path: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) => {
                return Err(invalid(format!(
                    "cannot inspect update state path {}: {error}",
                    current.display()
                )));
            }
        }
    }
    Ok(())
}

fn refuse_symlink(path: &Path, label: &str) -> Result<(), EvidenceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(invalid(format!("{label} path is a symlink")))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn read_bounded_regular(
    path: &Path,
    max_bytes: u64,
    label: &str,
) -> Result<Vec<u8>, EvidenceError> {
    refuse_symlink(path, label)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| invalid(format!("cannot open {label}: {error}")))?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.file_type().is_socket() {
        return Err(invalid(format!("{label} is not a regular file")));
    }
    if metadata.len() == 0 || metadata.len() > max_bytes {
        return Err(invalid(format!("{label} size is out of bounds")));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() || bytes.len() as u64 > max_bytes {
        return Err(invalid(format!("{label} changed while being read")));
    }
    Ok(bytes)
}

fn verify_executable(
    path: &Path,
    expected_bytes: &str,
    expected_sha256: &str,
    label: &str,
) -> Result<(), EvidenceError> {
    refuse_symlink(path, label)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| invalid(format!("cannot open {label}: {error}")))?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > EXECUTABLE_MAX_BYTES
        || metadata.len().to_string() != expected_bytes
    {
        return Err(invalid(format!(
            "{label} byte identity does not match pending-health"
        )));
    }
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copied = copied.saturating_add(count as u64);
        if copied > EXECUTABLE_MAX_BYTES {
            return Err(invalid(format!("{label} exceeds the hashing bound")));
        }
        hasher.update(&buffer[..count]);
    }
    if copied != metadata.len() {
        return Err(invalid(format!("{label} changed while being hashed")));
    }
    let digest = hex_digest(hasher.finalize().as_slice());
    if digest != expected_sha256 {
        return Err(invalid(format!(
            "{label} digest identity does not match pending-health"
        )));
    }
    Ok(())
}

fn field<'a>(line: &'a str, prefix: &str) -> Result<&'a str, EvidenceError> {
    line.strip_prefix(prefix)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(format!("missing or empty field {prefix}")))
}

fn valid_release_version(version: &str) -> bool {
    if version.is_empty() || version.len() > 64 || !version.is_ascii() {
        return false;
    }
    let (core, prerelease) = match version.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (version, None),
    };
    let mut components = core.split('.');
    if !components.by_ref().all(valid_numeric_identifier) || core.matches('.').count() != 2 {
        return false;
    }
    prerelease.is_none_or(|suffix| {
        !suffix.is_empty()
            && suffix.split('.').all(|identifier| {
                !identifier.is_empty()
                    && identifier
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    && (!identifier.bytes().all(|byte| byte.is_ascii_digit())
                        || valid_numeric_identifier(identifier))
            })
    })
}

fn valid_numeric_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.bytes().all(|byte| byte.is_ascii_digit())
        && (identifier == "0" || !identifier.starts_with('0'))
}

fn valid_transaction(transaction: &str) -> bool {
    let Some((stamp, pid)) = transaction.split_once('-') else {
        return false;
    };
    stamp.len() == 16
        && stamp.as_bytes()[0..8].iter().all(u8::is_ascii_digit)
        && stamp.as_bytes()[8] == b'T'
        && stamp.as_bytes()[9..15].iter().all(u8::is_ascii_digit)
        && stamp.as_bytes()[15] == b'Z'
        && !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_u64(value: &str) -> Result<u64, EvidenceError> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(invalid("noncanonical unsigned integer"));
    }
    value
        .parse::<u64>()
        .map_err(|_| invalid("unsigned integer is out of range"))
}

fn parse_bool(value: &str) -> Result<bool, EvidenceError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(invalid("noncanonical boolean")),
    }
}

fn bool_text(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

fn parse_option_u64(value: &str) -> Result<Option<u64>, EvidenceError> {
    if value == "none" {
        Ok(None)
    } else {
        canonical_u64(value).map(Some)
    }
}

fn option_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_owned(), |number| number.to_string())
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes).as_slice())
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

#[cfg(test)]
fn read_validated_evidence(
    update_dir: &Path,
    app: &Path,
    lifecycle: &Path,
) -> Result<EvidenceRecord, EvidenceError> {
    let candidate = PendingCandidate::load(update_dir, app, lifecycle)?;
    let bytes = read_bounded_regular(
        &update_dir.join(EVIDENCE_NAME),
        EVIDENCE_MAX_BYTES,
        "health evidence",
    )?;
    let record = EvidenceRecord::parse(&bytes)?;
    if record.candidate != candidate {
        return Err(invalid(
            "health evidence does not match the current pending candidate",
        ));
    }
    let owner_bytes = read_bounded_regular(
        &update_dir.join(OWNER_NAME),
        OWNER_MAX_BYTES,
        "launch-health owner",
    )?;
    let owner = parse_owner(&owner_bytes)?;
    if owner.attempt.launch_uuid != record.launch_uuid {
        return Err(invalid(
            "health evidence does not belong to the current launch owner",
        ));
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch_health::{
        ready_datagram, AckDisposition, LaunchObservation, ObserverEvent, PreparedLaunchHealth,
    };
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use uuid::Uuid;

    const FIRST_LAUNCH: &str = "00112233445566778899aabbccddeeff";
    const SECOND_LAUNCH: &str = "ffeeddccbbaa99887766554433221100";
    const CHILD_DEADLINE: Duration = Duration::from_secs(5);
    const FIXTURE_MARKER: &str = "launch-health-fixture-ran";

    struct TestTree {
        root: PathBuf,
        update: PathBuf,
        app: PathBuf,
        lifecycle: PathBuf,
        retained: PathBuf,
    }

    impl TestTree {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "ignite-candidate-health-{}",
                Uuid::new_v4().simple()
            ));
            let update = root.join("scribe-app/state/update");
            let bin = root.join("scribe-app/bin");
            let retained = root.join("scribe-app/previous-release/keep");
            fs::create_dir_all(&update).unwrap();
            fs::create_dir_all(&bin).unwrap();
            fs::create_dir_all(retained.parent().unwrap()).unwrap();
            let app = bin.join("scribe-app");
            let lifecycle = bin.join("scribe-lifecycle");
            fs::write(&app, b"candidate app bytes").unwrap();
            fs::write(&lifecycle, b"candidate lifecycle bytes").unwrap();
            fs::write(&retained, b"retained A must not change").unwrap();
            let tree = Self {
                root,
                update,
                app,
                lifecycle,
                retained,
            };
            tree.write_pending("7.8.1", "20261024T120000Z-123", None);
            tree
        }

        fn write_pending(&self, version: &str, transaction: &str, app_sha: Option<&str>) {
            let app = fs::read(&self.app).unwrap();
            let lifecycle = fs::read(&self.lifecycle).unwrap();
            let app_sha = app_sha
                .map(str::to_owned)
                .unwrap_or_else(|| sha256_bytes(&app));
            let lifecycle_sha = sha256_bytes(&lifecycle);
            let inventory_sha = sha256_bytes(b"canonical retained inventory\n");
            let record = format!(
                "IGNITE_PENDING_HEALTH_V1\n\
                 schema=1\n\
                 status=pending-health\n\
                 candidate_version={version}\n\
                 candidate_txn={transaction}\n\
                 candidate_app_bytes={}\n\
                 candidate_app_sha256={app_sha}\n\
                 candidate_lifecycle_bytes={}\n\
                 candidate_lifecycle_sha256={lifecycle_sha}\n\
                 retained_version=unknown\n\
                 retained_release_identity=sha256:{inventory_sha}\n\
                 lkg_inventory_bytes=29\n\
                 lkg_inventory_sha256={inventory_sha}\n\
                 data_compatibility=unknown\n",
                app.len(),
                lifecycle.len()
            );
            fs::write(self.update.join(PENDING_NAME), record).unwrap();
        }

        fn recorder(&self, launch: &str, start: Instant) -> EvidenceRecorder {
            let ownership = self.ownership(launch);
            EvidenceRecorder::prepare(ownership, &self.app, &self.lifecycle, start).unwrap()
        }

        fn attempt(&self, launch: &str) -> LaunchAttempt {
            LaunchAttempt::capture(launch).unwrap()
        }

        fn ownership(&self, launch: &str) -> LaunchOwnership {
            LaunchOwnership::claim(&self.update, self.attempt(launch)).unwrap()
        }

        fn record(&self) -> EvidenceRecord {
            read_validated_evidence(&self.update, &self.app, &self.lifecycle).unwrap()
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn run_actual_handshake_fixture(
        tree: &TestTree,
        mode: &str,
        override_attempt: Option<&str>,
    ) -> EvidenceRecord {
        let prepared = PreparedLaunchHealth::new().unwrap();
        let launch = prepared.attempt().to_owned();
        let start = Instant::now();
        let mut recorder = tree.recorder(&launch, start);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("launch_health_process_fixture")
            .arg("--ignored")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env("IGNITE_LAUNCH_HEALTH_TEST_FIXTURE", mode)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(attempt) = override_attempt {
            command.env("IGNITE_LAUNCH_HEALTH_TEST_ACK_OVERRIDE", attempt);
        } else {
            command.env_remove("IGNITE_LAUNCH_HEALTH_TEST_ACK_OVERRIDE");
        }
        prepared.configure_command(&mut command);
        let mut child = command.spawn().unwrap();
        let mut observer = prepared.spawned(start, Duration::from_secs(2));
        recorder.record_spawned().unwrap();

        let deadline = Instant::now() + CHILD_DEADLINE;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "subprocess fixture timed out");
            thread::sleep(Duration::from_millis(5));
        };
        let now = Instant::now();
        for event in observer.poll(now) {
            if matches!(event, ObserverEvent::Ready) {
                recorder.record_ready(now).unwrap();
            }
        }
        let _ = observer.child_exited(status.success());
        recorder
            .record_exit(status.success(), Instant::now())
            .unwrap();
        tree.record()
    }

    fn spawn_stoppable_fixture(mode: &str) -> Child {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("launch_health_process_fixture")
            .arg("--ignored")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env("IGNITE_LAUNCH_HEALTH_TEST_FIXTURE", mode)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        wait_for_fixture_marker(&mut child);
        child
    }

    fn wait_for_fixture_marker(child: &mut Child) {
        let stdout = child.stdout.take().expect("fixture stdout");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut notified = false;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if !notified && line.contains(FIXTURE_MARKER) {
                    let _ = sender.send(true);
                    notified = true;
                }
            }
            if !notified {
                let _ = sender.send(false);
            }
        });
        assert_eq!(receiver.recv_timeout(CHILD_DEADLINE), Ok(true));
    }

    fn terminate_fixture(child: &mut Child) -> ExitStatus {
        assert_eq!(
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
            0
        );
        let deadline = Instant::now() + CHILD_DEADLINE;
        loop {
            if let Some(status) = child.try_wait().expect("query fixture status") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("fixture did not stop before its deadline");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_report(
        receiver: &mpsc::Receiver<EvidenceReport>,
        expected_outcome: &str,
    ) -> EvidenceReport {
        let deadline = Instant::now() + CHILD_DEADLINE;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let report = receiver
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("missing evidence report for {expected_outcome}"));
            if report.outcome == expected_outcome {
                return report;
            }
        }
    }

    fn wait_for_record(tree: &TestTree, launch_uuid: &str) -> EvidenceRecord {
        let deadline = Instant::now() + CHILD_DEADLINE;
        loop {
            if let Ok(record) = read_validated_evidence(&tree.update, &tree.app, &tree.lifecycle) {
                if record.launch_uuid == launch_uuid {
                    return record;
                }
            }
            assert!(
                Instant::now() < deadline,
                "evidence record was not published before its deadline"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn lock_contention_cannot_delay_child_spawn_or_readiness_acknowledgment() {
        let tree = TestTree::new();
        let lock = acquire_publication_lock(&tree.update).unwrap();
        let prepared = PreparedLaunchHealth::new().unwrap();
        let launch = prepared.attempt().to_owned();
        let attempt = LaunchAttempt::capture(&launch).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("launch_health_process_fixture")
            .arg("--ignored")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env("IGNITE_LAUNCH_HEALTH_TEST_FIXTURE", "ack")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        prepared.configure_command(&mut command);

        let mut child = command.spawn().expect("spawn while evidence lock is held");
        let started_at = Instant::now();
        let mut health = prepared.spawned(started_at, Duration::from_secs(2));
        let (report_sender, report_receiver) = mpsc::channel();
        let (handoff_sender, handoff_receiver) = mpsc::channel();
        let update = tree.update.clone();
        let app = tree.app.clone();
        let lifecycle = tree.lifecycle.clone();
        thread::spawn(move || {
            let handoff_started = Instant::now();
            let observer = EvidenceObserver::start_with_lifecycle(
                update,
                app,
                lifecycle,
                attempt,
                started_at,
                InitialEvidence::Spawned,
                move |report| {
                    let _ = report_sender.send(report);
                },
            );
            let _ = handoff_sender.send((observer, handoff_started.elapsed()));
        });
        let (evidence, handoff_elapsed) =
            match handoff_receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(result) => result,
                Err(_) => {
                    drop(lock);
                    panic!("asynchronous handoff waited for the publication lock");
                }
            };
        let mut evidence = evidence.unwrap();
        assert!(
            handoff_elapsed < Duration::from_millis(500),
            "asynchronous handoff waited for the publication lock"
        );
        assert!(report_receiver.try_recv().is_err());

        let deadline = Instant::now() + CHILD_DEADLINE;
        let ready_at = loop {
            let now = Instant::now();
            if health
                .poll(now)
                .iter()
                .any(|event| matches!(event, ObserverEvent::Ready))
            {
                break now;
            }
            assert!(
                now < deadline,
                "child acknowledgment was delayed by lock contention"
            );
            thread::sleep(Duration::from_millis(5));
        };
        evidence.record_ready(ready_at);

        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "acknowledging child did not exit"
            );
            thread::sleep(Duration::from_millis(5));
        };
        evidence.record_exit(status.success(), Instant::now());
        drop(lock);

        assert!(wait_for_report(&report_receiver, "spawned").result.is_ok());
        assert!(wait_for_report(&report_receiver, "ready").result.is_ok());
        assert!(wait_for_report(&report_receiver, "exited").result.is_ok());
        let record = wait_for_record(&tree, &launch);
        assert!(record.ready);
        assert_eq!(record.outcome, Outcome::ExitedSuccess);
    }

    #[test]
    fn actual_child_acknowledgment_persists_only_for_the_matching_launch_uuid() {
        let tree = TestTree::new();
        let matching = run_actual_handshake_fixture(&tree, "ack", None);
        assert!(matching.ready);
        assert_eq!(matching.outcome, Outcome::ExitedSuccess);

        let wrong = run_actual_handshake_fixture(&tree, "override", Some(FIRST_LAUNCH));
        assert_ne!(wrong.launch_uuid, FIRST_LAUNCH);
        assert!(!wrong.ready);
        assert_eq!(wrong.outcome, Outcome::ExitedSuccess);
    }

    #[test]
    fn matching_pending_candidate_records_ready_and_monotonic_running_checkpoint() {
        let tree = TestTree::new();
        let start = Instant::now();
        let mut recorder = tree.recorder(FIRST_LAUNCH, start);
        recorder.record_spawned().unwrap();
        recorder
            .record_ready(start + Duration::from_millis(250))
            .unwrap();
        let ready = tree.record();
        assert_eq!(ready.outcome, Outcome::Ready);
        assert!(ready.ready);
        assert_eq!(ready.ready_elapsed_ms, Some(250));
        assert!(!ready.complete);
        assert!(!recorder
            .maybe_record_running(start + Duration::from_secs(30), Duration::from_secs(60))
            .unwrap());
        assert!(recorder
            .maybe_record_running(start + Duration::from_secs(61), Duration::from_secs(60))
            .unwrap());
        let stable = tree.record();
        assert_eq!(stable.outcome, Outcome::ReadyRunning);
        assert_eq!(stable.post_ready_observed_ms, Some(60_750));
        assert_eq!(stable.launch_uuid, FIRST_LAUNCH);
    }

    #[test]
    fn failed_running_checkpoint_is_attempted_only_once() {
        let tree = TestTree::new();
        let start = Instant::now();
        let mut recorder = tree.recorder(FIRST_LAUNCH, start);
        recorder.record_spawned().unwrap();
        recorder
            .record_ready(start + Duration::from_millis(100))
            .unwrap();

        // A newer owner makes the due checkpoint fail after candidate revalidation. Removing the
        // executable afterward proves a second call returns before hashing or filesystem access.
        let _newer = tree.ownership(SECOND_LAUNCH);
        assert!(recorder
            .maybe_record_running(start + Duration::from_secs(61), Duration::from_secs(60))
            .is_err());
        fs::remove_file(&tree.app).unwrap();
        assert!(!recorder
            .maybe_record_running(start + Duration::from_secs(120), Duration::from_secs(60))
            .unwrap());
    }

    #[test]
    fn asynchronous_checkpoint_failure_emits_one_error_and_never_requeues() {
        let tree = TestTree::new();
        let start = Instant::now();
        let (report_sender, report_receiver) = mpsc::channel();
        let mut observer = EvidenceObserver::start_with_lifecycle(
            tree.update.clone(),
            tree.app.clone(),
            tree.lifecycle.clone(),
            tree.attempt(FIRST_LAUNCH),
            start,
            InitialEvidence::Spawned,
            move |report| {
                let _ = report_sender.send(report);
            },
        )
        .unwrap();
        let spawned = wait_for_report(&report_receiver, "spawned");
        assert!(spawned.result.is_ok());
        assert!(!spawned.terminal);
        observer.record_ready(start + Duration::from_millis(100));
        let ready = wait_for_report(&report_receiver, "ready");
        assert!(ready.result.is_ok());
        assert!(!ready.terminal);

        let _newer = tree.ownership(SECOND_LAUNCH);
        observer.maybe_record_running(start + Duration::from_secs(61), Duration::from_secs(60));
        let failed_checkpoint = wait_for_report(&report_receiver, "ready_running");
        assert!(failed_checkpoint.result.is_err());
        assert!(failed_checkpoint.terminal);

        fs::remove_file(&tree.app).unwrap();
        observer.maybe_record_running(start + Duration::from_secs(120), Duration::from_secs(60));
        assert!(report_receiver
            .recv_timeout(Duration::from_millis(200))
            .is_err());
    }

    #[test]
    fn wrong_launch_uuid_never_records_readiness() {
        let tree = TestTree::new();
        let start = Instant::now();
        let mut recorder = tree.recorder(FIRST_LAUNCH, start);
        recorder.record_spawned().unwrap();
        let mut observation = LaunchObservation::new(FIRST_LAUNCH.to_owned());
        assert!(observation.child_spawned());
        assert!(matches!(
            observation.acknowledge(ready_datagram(SECOND_LAUNCH).unwrap().as_bytes()),
            AckDisposition::IncorrectIdentifier { .. }
        ));
        recorder
            .record_exit(true, start + Duration::from_secs(1))
            .unwrap();
        let record = tree.record();
        assert_eq!(record.outcome, Outcome::ExitedSuccess);
        assert!(!record.ready);
        assert_eq!(record.launch_uuid, FIRST_LAUNCH);
    }

    #[test]
    fn stale_transaction_and_executable_mismatch_are_refused() {
        let tree = TestTree::new();
        let mut recorder = tree.recorder(FIRST_LAUNCH, Instant::now());
        recorder.record_spawned().unwrap();
        tree.write_pending("7.8.1", "20261024T120001Z-124", None);
        assert!(read_validated_evidence(&tree.update, &tree.app, &tree.lifecycle).is_err());
        assert!(recorder.record_timeout(Instant::now()).is_err());

        tree.write_pending("7.8.1", "20261024T120000Z-123", None);
        fs::write(&tree.app, b"changed candidate").unwrap();
        assert!(read_validated_evidence(&tree.update, &tree.app, &tree.lifecycle).is_err());
        let ownership = tree.ownership(SECOND_LAUNCH);
        assert!(
            EvidenceRecorder::prepare(ownership, &tree.app, &tree.lifecycle, Instant::now())
                .is_err()
        );
    }

    #[test]
    fn spawn_failure_and_premature_exit_are_distinct() {
        let tree = TestTree::new();
        let mut failed = tree.recorder(FIRST_LAUNCH, Instant::now());
        failed.record_spawn_failed().unwrap();
        assert_eq!(tree.record().outcome, Outcome::SpawnFailed);
        assert_eq!(tree.record().process, ProcessState::NotSpawned);

        let mut crashed = tree.recorder(SECOND_LAUNCH, Instant::now());
        crashed.record_spawned().unwrap();
        crashed.record_exit(false, Instant::now()).unwrap();
        let record = tree.record();
        assert_eq!(record.outcome, Outcome::ExitedFailure);
        assert!(!record.ready);
    }

    #[test]
    fn readiness_then_unexpected_exit_or_intentional_unload_remain_distinct() {
        let tree = TestTree::new();
        let start = Instant::now();
        let mut crashed = tree.recorder(FIRST_LAUNCH, start);
        crashed.record_spawned().unwrap();
        crashed
            .record_ready(start + Duration::from_millis(100))
            .unwrap();
        crashed
            .record_exit(false, start + Duration::from_secs(3))
            .unwrap();
        let exit = tree.record();
        assert_eq!(exit.outcome, Outcome::ExitedFailure);
        assert!(exit.ready);
        assert_eq!(exit.process, ProcessState::Exited);

        let mut unloaded = tree.recorder(SECOND_LAUNCH, start);
        unloaded.record_spawned().unwrap();
        unloaded
            .record_ready(start + Duration::from_millis(200))
            .unwrap();
        unloaded
            .record_intentional_stop(true, start + Duration::from_secs(4))
            .unwrap();
        let stop = tree.record();
        assert_eq!(stop.outcome, Outcome::IntentionalStop);
        assert!(stop.ready);
        assert_eq!(stop.process, ProcessState::Stopped);
    }

    #[test]
    fn actual_intentional_stop_uses_the_child_exit_status() {
        let tree = TestTree::new();
        for (mode, expected_success, expected_outcome, expected_process) in [
            (
                "stop-clean",
                true,
                Outcome::IntentionalStop,
                ProcessState::Stopped,
            ),
            (
                "stop-abnormal",
                false,
                Outcome::IntentionalStopFailed,
                ProcessState::Exited,
            ),
        ] {
            let launch = Uuid::new_v4().simple().to_string();
            let ownership = tree.ownership(&launch);
            let mut child = spawn_stoppable_fixture(mode);
            let start = Instant::now();
            let mut recorder =
                EvidenceRecorder::prepare(ownership, &tree.app, &tree.lifecycle, start).unwrap();
            recorder.record_spawned().unwrap();
            recorder
                .record_ready(start + Duration::from_millis(1))
                .unwrap();

            let status = terminate_fixture(&mut child);
            assert_eq!(status.success(), expected_success);
            if mode == "stop-abnormal" {
                assert_eq!(status.code(), None, "fixture must terminate by signal");
            }
            recorder
                .record_intentional_stop(status.success(), Instant::now())
                .unwrap();
            let record = tree.record();
            assert_eq!(record.outcome, expected_outcome);
            assert!(record.ready);
            assert_eq!(record.process, expected_process);
            assert!(record.complete);
        }
    }

    #[test]
    fn timeout_and_interruption_never_become_qualification() {
        let tree = TestTree::new();
        let mut recorder = tree.recorder(FIRST_LAUNCH, Instant::now());
        recorder.record_spawned().unwrap();
        recorder.record_timeout(Instant::now()).unwrap();
        let timeout = tree.record();
        assert_eq!(timeout.outcome, Outcome::TimedOut);
        assert_eq!(timeout.readiness, ReadinessState::TimedOut);
        assert!(!timeout.complete);
        assert!(String::from_utf8(timeout.encode())
            .unwrap()
            .contains("qualification=unqualified\n"));
        recorder.record_interrupted(Instant::now()).unwrap();
        let interrupted = tree.record();
        assert_eq!(interrupted.outcome, Outcome::Interrupted);
        assert_eq!(interrupted.readiness, ReadinessState::TimedOut);
    }

    #[test]
    fn rapid_successive_launches_reject_stale_writer() {
        let tree = TestTree::new();
        let mut first = tree.recorder(FIRST_LAUNCH, Instant::now());
        first.record_spawned().unwrap();
        let mut second = tree.recorder(SECOND_LAUNCH, Instant::now());
        second.record_spawned().unwrap();
        assert!(first.record_exit(false, Instant::now()).is_err());
        assert_eq!(tree.record().launch_uuid, SECOND_LAUNCH);
    }

    #[test]
    fn delayed_first_publication_from_an_older_process_cannot_replace_newer_evidence() {
        let tree = TestTree::new();
        let first_attempt = LaunchAttempt::controlled(FIRST_LAUNCH, 100, 4242, 1);
        let mut first_child = spawn_stoppable_fixture("stop-abnormal");
        let second_attempt = LaunchAttempt::controlled(SECOND_LAUNCH, 200, 4242, 2);
        let mut second_child = spawn_stoppable_fixture("stop-abnormal");

        let (current_sender, current_receiver) = mpsc::channel();
        let mut current = EvidenceObserver::start_with_lifecycle(
            tree.update.clone(),
            tree.app.clone(),
            tree.lifecycle.clone(),
            second_attempt,
            Instant::now(),
            InitialEvidence::Spawned,
            move |report| {
                let _ = current_sender.send(report);
            },
        )
        .unwrap();
        assert!(wait_for_report(&current_receiver, "spawned").result.is_ok());

        // The older process begins its evidence worker only after the newer launch owns and has
        // published. Its pre-spawn order token, not lock-acquisition order, decides ownership.
        let (delayed_sender, delayed_receiver) = mpsc::channel();
        let _delayed = EvidenceObserver::start_with_lifecycle(
            tree.update.clone(),
            tree.app.clone(),
            tree.lifecycle.clone(),
            first_attempt,
            Instant::now(),
            InitialEvidence::Spawned,
            move |report| {
                let _ = delayed_sender.send(report);
            },
        )
        .unwrap();
        assert!(wait_for_report(&delayed_receiver, "ownership")
            .result
            .is_err());
        assert_eq!(
            wait_for_record(&tree, SECOND_LAUNCH).launch_uuid,
            SECOND_LAUNCH
        );

        let _ = terminate_fixture(&mut first_child);
        let second_status = terminate_fixture(&mut second_child);
        current.record_intentional_stop(second_status.success(), Instant::now());
        assert!(
            wait_for_report(&current_receiver, "intentional_stop_failed")
                .result
                .is_ok()
        );
        assert_eq!(
            wait_for_record(&tree, SECOND_LAUNCH).outcome,
            Outcome::IntentionalStopFailed
        );
    }

    #[test]
    fn interrupted_publication_preserves_previous_complete_record() {
        let tree = TestTree::new();
        let mut recorder = tree.recorder(FIRST_LAUNCH, Instant::now());
        recorder.record_spawn_failed().unwrap();
        let previous = fs::read(tree.update.join(EVIDENCE_NAME)).unwrap();
        let mut replacement = tree.record();
        replacement.outcome = Outcome::ExitedFailure;
        replacement.process = ProcessState::Exited;
        assert!(
            publish_record(&tree.update, &recorder.owner, &replacement, true, || Err(
                io::Error::new(io::ErrorKind::Interrupted, "test interruption")
            ))
            .is_err()
        );
        assert_eq!(fs::read(tree.update.join(EVIDENCE_NAME)).unwrap(), previous);
        assert!(!tree.update.join(EVIDENCE_TEMP_NAME).exists());
    }

    #[test]
    fn missing_malformed_and_corrupted_state_fail_closed() {
        let tree = TestTree::new();
        fs::remove_file(tree.update.join(PENDING_NAME)).unwrap();
        assert!(LaunchOwnership::claim(&tree.update, tree.attempt(FIRST_LAUNCH)).is_err());
        fs::write(tree.update.join(PENDING_NAME), b"malformed\n").unwrap();
        let ownership = tree.ownership(FIRST_LAUNCH);
        assert!(
            EvidenceRecorder::prepare(ownership, &tree.app, &tree.lifecycle, Instant::now())
                .is_err()
        );

        tree.write_pending("7.8.1", "20261024T120000Z-123", None);
        let mut recorder = tree.recorder(FIRST_LAUNCH, Instant::now());
        recorder.record_spawned().unwrap();
        fs::write(tree.update.join(EVIDENCE_NAME), b"corrupted\n").unwrap();
        assert!(read_validated_evidence(&tree.update, &tree.app, &tree.lifecycle).is_err());
    }

    #[test]
    fn malformed_versions_and_noncanonical_evidence_are_refused() {
        let tree = TestTree::new();
        for version in ["01.2.3", "1.2.3+build", "1.2.3-01", "1.2"] {
            tree.write_pending(version, "20261024T120000Z-123", None);
            let ownership = tree.ownership(FIRST_LAUNCH);
            assert!(EvidenceRecorder::prepare(
                ownership,
                &tree.app,
                &tree.lifecycle,
                Instant::now()
            )
            .is_err());
        }
        tree.write_pending("7.8.1", "20261024T120000Z-123", None);
        let mut recorder = tree.recorder(FIRST_LAUNCH, Instant::now());
        recorder.record_spawned().unwrap();
        let mut bytes = fs::read(tree.update.join(EVIDENCE_NAME)).unwrap();
        bytes.extend_from_slice(b"trailing");
        assert!(EvidenceRecord::parse(&bytes).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_and_evidence_paths_are_refused() {
        use std::os::unix::fs::symlink;

        let tree = TestTree::new();
        let outside = tree.root.join("outside");
        fs::create_dir(&outside).unwrap();
        let unsafe_update = tree.root.join("unsafe-update");
        symlink(&outside, &unsafe_update).unwrap();
        assert!(LaunchOwnership::claim(&unsafe_update, tree.attempt(FIRST_LAUNCH)).is_err());

        symlink(
            tree.root.join("outside-evidence"),
            tree.update.join(EVIDENCE_NAME),
        )
        .unwrap();
        let mut recorder = tree.recorder(FIRST_LAUNCH, Instant::now());
        assert!(recorder.record_spawned().is_err());
    }

    #[test]
    fn evidence_failure_does_not_block_an_ordinary_child_launch() {
        let tree = TestTree::new();
        fs::remove_file(tree.update.join(PENDING_NAME)).unwrap();
        assert!(LaunchOwnership::claim(&tree.update, tree.attempt(FIRST_LAUNCH)).is_err());
        let status = Command::new("sh").args(["-c", "exit 0"]).status().unwrap();
        assert!(status.success());
    }

    #[test]
    fn recording_never_modifies_or_deletes_retained_release() {
        let tree = TestTree::new();
        let before = fs::read(&tree.retained).unwrap();
        let mut recorder = tree.recorder(FIRST_LAUNCH, Instant::now());
        recorder.record_spawned().unwrap();
        recorder.record_ready(Instant::now()).unwrap();
        recorder.record_exit(false, Instant::now()).unwrap();
        assert_eq!(fs::read(&tree.retained).unwrap(), before);
        assert!(tree.retained.exists());
    }
}
