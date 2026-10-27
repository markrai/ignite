//! Read-only promotion eligibility assessment.
//!
//! This module deliberately has no state-writing, repair, promotion, or rollback operation. It
//! validates the current candidate and retained release twice while holding the evidence writer's
//! advisory lock, then reports whether the runtime evidence is ready for explicit compatibility
//! review. The standalone command takes that lock shared. The lifecycle decision boundary passes
//! its private exclusive-lock guard into the same validation implementation.

use super::*;
use std::collections::BTreeSet;
use std::fmt::Write as _;

const DEFAULT_APP: &str = "/mnt/us/scribe-app/bin/scribe-app";
const DEFAULT_LIFECYCLE: &str = "/mnt/us/scribe-app/bin/scribe-lifecycle";
const DEFAULT_PREVIOUS_DIR: &str = "/mnt/us/scribe-app/previous-release";
const INVENTORY_NAME: &str = "ignite-lkg-inventory.v1";
const INVENTORY_HEADER: &str = "IGNITE_LKG_INVENTORY_V1";
const INVENTORY_MAX_BYTES: u64 = 32 * 1024;
const RETAINED_FILE_MAX_BYTES: u64 = EXECUTABLE_MAX_BYTES;
const REQUIRED_RETAINED: [&str; 3] = ["scribe-app", "scribe-lifecycle", "scribe-lifecycle-host.sh"];
const RETAINED_ENTRIES: [&str; 18] = [
    "scribe-app",
    "scribe-lifecycle",
    "scribe-lifecycle-host.sh",
    "Scribe.sh",
    "scribe-icon.png",
    "OFL.txt",
    "DESCRIPTION.en_us.html",
    "upstream_info.md",
    "source-sans-3-OFL.txt",
    "install-runtime.sh",
    "uninstall-runtime.sh",
    "backup-runtime.sh",
    "register-helpers.sh",
    "install-recovery.sh",
    "Uninstall Ignite.sh",
    "Backup Ignite Data.sh",
    "Diagnose Ignite.sh",
    "appreg-handler.sql",
];
const MIN_POST_READY_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AssessmentMode {
    ReadOnly,
    BoundaryReadOnly,
}

impl AssessmentMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::BoundaryReadOnly => "boundary-read-only",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromotionAssessment {
    Blocked,
    ReviewRequired,
}

impl PromotionAssessment {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "BLOCKED",
            Self::ReviewRequired => "REVIEW_REQUIRED",
        }
    }

    /// No current assessment is an authorization. A nonzero status prevents callers from treating
    /// successful report generation as permission to promote.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Blocked => 1,
            Self::ReviewRequired => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckStatus {
    Pass,
    Fail,
    Unknown,
}

impl CheckStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Check {
    name: &'static str,
    status: CheckStatus,
    class: &'static str,
    detail: String,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Pass,
            class: "verified",
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, class: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Fail,
            class,
            detail: detail.into(),
        }
    }

    fn unknown(name: &'static str, class: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Unknown,
            class,
            detail: detail.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromotionAssessmentReport {
    checks: Vec<Check>,
    overall: PromotionAssessment,
    mode: AssessmentMode,
}

impl PromotionAssessmentReport {
    #[cfg(test)]
    fn overall(&self) -> PromotionAssessment {
        self.overall
    }

    pub fn exit_code(&self) -> i32 {
        self.overall.exit_code()
    }

    pub(crate) fn record_boundary_confirmation(
        mut self,
        launch_uuid: &str,
        confirmation: Result<(), String>,
    ) -> Self {
        let check = match confirmation {
            Ok(()) => Check::pass(
                "decision_boundary",
                format!(
                    "all exclusive guards remained held and lifecycle generation {launch_uuid} was reconfirmed"
                ),
            ),
            Err(detail) => Check::fail("decision_boundary", "coordination_failure", detail),
        };
        self.checks.insert(0, check);
        self.overall = overall_for(&self.checks);
        self
    }

    #[cfg(test)]
    fn check(&self, name: &str) -> &Check {
        self.checks
            .iter()
            .find(|check| check.name == name)
            .expect("assessment check")
    }
}

impl fmt::Display for PromotionAssessmentReport {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(output, "IGNITE_PROMOTION_ASSESSMENT_V1")?;
        writeln!(output, "mode={}", self.mode.as_str())?;
        for check in &self.checks {
            writeln!(
                output,
                "check.{}={} class={} detail={}",
                check.name,
                check.status.as_str(),
                check.class,
                printable_detail(&check.detail)
            )?;
        }
        writeln!(output, "overall={}", self.overall.as_str())?;
        writeln!(output, "promotion_authorized=false")
    }
}

#[derive(Clone, Debug)]
struct AssessmentPaths {
    update_dir: PathBuf,
    app: PathBuf,
    lifecycle: PathBuf,
    previous_dir: PathBuf,
}

impl Default for AssessmentPaths {
    fn default() -> Self {
        Self {
            update_dir: PathBuf::from(DEFAULT_UPDATE_DIR),
            app: PathBuf::from(DEFAULT_APP),
            lifecycle: PathBuf::from(DEFAULT_LIFECYCLE),
            previous_dir: PathBuf::from(DEFAULT_PREVIOUS_DIR),
        }
    }
}

impl AssessmentPaths {
    fn for_boundary(update_dir: &Path) -> Self {
        Self {
            update_dir: update_dir.to_owned(),
            app: configured_assessment_path(DEFAULT_APP, "IGNITE_TEST_ASSESS_APP_PATH"),
            lifecycle: configured_assessment_path(
                DEFAULT_LIFECYCLE,
                "IGNITE_TEST_ASSESS_LIFECYCLE_PATH",
            ),
            previous_dir: configured_assessment_path(
                DEFAULT_PREVIOUS_DIR,
                "IGNITE_TEST_ASSESS_PREVIOUS_DIR",
            ),
        }
    }
}

#[cfg(debug_assertions)]
fn configured_assessment_path(default: &str, test_variable: &str) -> PathBuf {
    std::env::var_os(test_variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

#[cfg(not(debug_assertions))]
fn configured_assessment_path(default: &str, _test_variable: &str) -> PathBuf {
    PathBuf::from(default)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingAssessment {
    candidate: PendingCandidate,
    inventory_bytes: u64,
    inventory_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HealthAssessment {
    owner: OwnerRecord,
    evidence: EvidenceRecord,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidatedSnapshot {
    pending: PendingAssessment,
    inventory_sha256: String,
    health: HealthAssessment,
}

struct AssessmentReadLock(File);

enum HeldAssessmentLock<'a> {
    Shared,
    Decision {
        _evidence_lock: &'a DecisionEvidenceLock,
        latest_launch_uuid: &'a str,
    },
}

impl HeldAssessmentLock<'_> {
    fn mode(&self) -> AssessmentMode {
        match self {
            Self::Shared => AssessmentMode::ReadOnly,
            Self::Decision { .. } => AssessmentMode::BoundaryReadOnly,
        }
    }

    fn latest_launch_uuid(&self) -> Option<&str> {
        match self {
            Self::Shared => None,
            Self::Decision {
                latest_launch_uuid, ..
            } => Some(latest_launch_uuid),
        }
    }

    fn check(&self) -> Check {
        match self {
            Self::Shared => Check::pass(
                "assessment_lock",
                "existing evidence lock held shared; no state created",
            ),
            Self::Decision { .. } => Check::pass(
                "assessment_lock",
                "private decision evidence lock remains held exclusively; no recursive lock acquisition",
            ),
        }
    }
}

impl AssessmentReadLock {
    fn acquire(update_dir: &Path) -> Result<Self, EvidenceError> {
        ensure_safe_update_dir(update_dir)?;
        let path = update_dir.join(EVIDENCE_LOCK_NAME);
        refuse_symlink(&path, "health evidence lock")?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|error| {
                invalid(format!(
                    "cannot open existing health evidence lock: {error}"
                ))
            })?;
        if !file.metadata()?.file_type().is_file() {
            return Err(invalid("health evidence lock is not a regular file"));
        }
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self(file))
    }
}

impl Drop for AssessmentReadLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn assess_default_promotion() -> PromotionAssessmentReport {
    assess_paths(&AssessmentPaths::default(), || {})
}

pub(crate) fn assess_promotion_under_boundary(
    evidence_lock: &DecisionEvidenceLock,
    update_dir: &Path,
    latest_launch_uuid: &str,
) -> PromotionAssessmentReport {
    let paths = AssessmentPaths::for_boundary(update_dir);
    assess_paths_while_locked(
        &paths,
        HeldAssessmentLock::Decision {
            _evidence_lock: evidence_lock,
            latest_launch_uuid,
        },
        || {},
    )
}

fn assess_paths<F>(paths: &AssessmentPaths, between_passes: F) -> PromotionAssessmentReport
where
    F: FnOnce(),
{
    let mut checks = Vec::with_capacity(9);
    let _lock = match AssessmentReadLock::acquire(&paths.update_dir) {
        Ok(lock) => lock,
        Err(error) => {
            checks.push(Check::fail(
                "assessment_lock",
                if path_missing(&paths.update_dir.join(EVIDENCE_LOCK_NAME)) {
                    "insufficient_evidence"
                } else {
                    "corruption"
                },
                error.to_string(),
            ));
            append_unavailable_checks(&mut checks, "assessment lock unavailable");
            checks.push(compatibility_unknown());
            return finish(AssessmentMode::ReadOnly, checks);
        }
    };
    assess_paths_while_locked(paths, HeldAssessmentLock::Shared, between_passes)
}

fn assess_paths_while_locked<F>(
    paths: &AssessmentPaths,
    held_lock: HeldAssessmentLock<'_>,
    between_passes: F,
) -> PromotionAssessmentReport
where
    F: FnOnce(),
{
    let mut checks = Vec::with_capacity(12);
    checks.push(held_lock.check());

    let pending = match load_pending(paths) {
        Ok(pending) => {
            checks.push(Check::pass(
                "pending_and_live_candidate",
                format!(
                    "transaction={} version={} live executable hashes match",
                    pending.candidate.transaction, pending.candidate.version
                ),
            ));
            Some(pending)
        }
        Err(error) => {
            checks.push(Check::fail(
                "pending_and_live_candidate",
                if path_missing(&paths.update_dir.join(PENDING_NAME)) {
                    "insufficient_state"
                } else {
                    "corruption"
                },
                error.to_string(),
            ));
            None
        }
    };

    let retained = pending
        .as_ref()
        .and_then(|pending| match validate_retained(paths, pending) {
            Ok(digest) => {
                checks.push(Check::pass(
                    "retained_release",
                    "closed 18-entry inventory and every retained entry match",
                ));
                Some(digest)
            }
            Err(error) => {
                checks.push(Check::fail(
                    "retained_release",
                    "corruption",
                    error.to_string(),
                ));
                None
            }
        });
    if pending.is_none() {
        checks.push(Check::unknown(
            "retained_release",
            "dependency_unavailable",
            "trusted retained identity is unavailable",
        ));
    }

    let owner = load_owner(paths, &mut checks);
    let evidence = load_evidence(paths, pending.as_ref(), &mut checks);
    let health = match (owner, evidence) {
        (Some(owner), Some(evidence)) => {
            if owner.attempt.launch_uuid == evidence.launch_uuid {
                checks.push(Check::pass(
                    "launch_ownership",
                    format!("launch_uuid={}", evidence.launch_uuid),
                ));
                Some(HealthAssessment { owner, evidence })
            } else {
                checks.push(Check::fail(
                    "launch_ownership",
                    "insufficient_evidence",
                    "owner and evidence launch UUIDs differ; evidence is stale",
                ));
                None
            }
        }
        _ => {
            checks.push(Check::unknown(
                "launch_ownership",
                "dependency_unavailable",
                "canonical owner and evidence are both required",
            ));
            None
        }
    };

    if let Some(expected) = held_lock.latest_launch_uuid() {
        match health.as_ref() {
            Some(health) if health.evidence.launch_uuid == expected => checks.push(Check::pass(
                "lifecycle_generation",
                format!("durable owner and evidence match current host launch_uuid={expected}"),
            )),
            Some(health) => checks.push(Check::fail(
                "lifecycle_generation",
                "stale_evidence",
                format!(
                    "current host launch_uuid={expected}, durable launch_uuid={}",
                    health.evidence.launch_uuid
                ),
            )),
            None => checks.push(Check::unknown(
                "lifecycle_generation",
                "dependency_unavailable",
                "canonical matching owner and evidence are required to bind the host generation",
            )),
        }
    }

    append_runtime_checks(&mut checks, health.as_ref().map(|health| &health.evidence));
    checks.push(compatibility_unknown());

    let first = match (pending, retained, health) {
        (Some(pending), Some(inventory_sha256), Some(health)) => Some(ValidatedSnapshot {
            pending,
            inventory_sha256,
            health,
        }),
        _ => None,
    };

    between_passes();
    match first {
        Some(first) => match load_snapshot(paths) {
            Ok(second) if first == second => checks.push(Check::pass(
                "state_stability",
                "second full validation matched the first pass",
            )),
            Ok(_) => checks.push(Check::fail(
                "state_stability",
                "concurrent_change",
                "validated state changed between assessment passes",
            )),
            Err(error) => checks.push(Check::fail(
                "state_stability",
                "concurrent_change",
                format!("state became invalid during assessment: {error}"),
            )),
        },
        None => checks.push(Check::unknown(
            "state_stability",
            "dependency_unavailable",
            "first validation pass was incomplete",
        )),
    }

    finish(held_lock.mode(), checks)
}

fn overall_for(checks: &[Check]) -> PromotionAssessment {
    if checks.iter().any(|check| check.status == CheckStatus::Fail) {
        PromotionAssessment::Blocked
    } else {
        PromotionAssessment::ReviewRequired
    }
}

fn finish(mode: AssessmentMode, checks: Vec<Check>) -> PromotionAssessmentReport {
    let overall = overall_for(&checks);
    PromotionAssessmentReport {
        checks,
        overall,
        mode,
    }
}

fn append_unavailable_checks(checks: &mut Vec<Check>, detail: &str) {
    for name in [
        "pending_and_live_candidate",
        "retained_release",
        "ownership_record",
        "evidence_identity",
        "launch_ownership",
        "intentional_stop",
        "readiness",
        "post_ready_observation",
        "state_stability",
    ] {
        checks.push(Check::unknown(name, "dependency_unavailable", detail));
    }
}

fn compatibility_unknown() -> Check {
    Check::unknown(
        "data_compatibility",
        "policy_blocker",
        "pending and evidence state explicitly say data_compatibility=unknown",
    )
}

fn load_pending(paths: &AssessmentPaths) -> Result<PendingAssessment, EvidenceError> {
    let candidate = PendingCandidate::load(&paths.update_dir, &paths.app, &paths.lifecycle)?;
    let bytes = read_bounded_regular(
        &paths.update_dir.join(PENDING_NAME),
        PENDING_MAX_BYTES,
        "pending-health",
    )?;
    if sha256_bytes(&bytes) != candidate.pending_sha256 {
        return Err(invalid(
            "pending-health changed while candidate was validated",
        ));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| invalid("pending-health is not canonical UTF-8/ASCII"))?;
    let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    if lines.len() != 14 {
        return Err(invalid("pending-health field count changed"));
    }
    let inventory_bytes = canonical_u64(field(lines[11], "lkg_inventory_bytes=")?)?;
    let inventory_sha256 = field(lines[12], "lkg_inventory_sha256=")?.to_owned();
    Ok(PendingAssessment {
        candidate,
        inventory_bytes,
        inventory_sha256,
    })
}

fn load_owner(paths: &AssessmentPaths, checks: &mut Vec<Check>) -> Option<OwnerRecord> {
    let path = paths.update_dir.join(OWNER_NAME);
    match read_bounded_regular(&path, OWNER_MAX_BYTES, "launch-health owner")
        .and_then(|bytes| parse_owner(&bytes))
    {
        Ok(owner) => {
            checks.push(Check::pass(
                "ownership_record",
                format!("launch_uuid={}", owner.attempt.launch_uuid),
            ));
            Some(owner)
        }
        Err(error) => {
            checks.push(Check::fail(
                "ownership_record",
                if path_missing(&path) {
                    "insufficient_evidence"
                } else {
                    "corruption"
                },
                error.to_string(),
            ));
            None
        }
    }
}

fn load_evidence(
    paths: &AssessmentPaths,
    pending: Option<&PendingAssessment>,
    checks: &mut Vec<Check>,
) -> Option<EvidenceRecord> {
    let path = paths.update_dir.join(EVIDENCE_NAME);
    let parsed = read_bounded_regular(&path, EVIDENCE_MAX_BYTES, "health evidence")
        .and_then(|bytes| EvidenceRecord::parse(&bytes));
    match (parsed, pending) {
        (Ok(record), Some(pending)) if record.candidate == pending.candidate => {
            checks.push(Check::pass(
                "evidence_identity",
                "evidence matches the exact pending transaction and live B identity",
            ));
            Some(record)
        }
        (Ok(_), Some(_)) => {
            checks.push(Check::fail(
                "evidence_identity",
                "insufficient_evidence",
                "evidence belongs to a different pending candidate",
            ));
            None
        }
        (Ok(_), None) => {
            checks.push(Check::unknown(
                "evidence_identity",
                "dependency_unavailable",
                "current pending candidate is invalid",
            ));
            None
        }
        (Err(error), _) => {
            checks.push(Check::fail(
                "evidence_identity",
                if path_missing(&path) {
                    "insufficient_evidence"
                } else {
                    "corruption"
                },
                error.to_string(),
            ));
            None
        }
    }
}

fn append_runtime_checks(checks: &mut Vec<Check>, evidence: Option<&EvidenceRecord>) {
    let Some(evidence) = evidence else {
        for name in ["intentional_stop", "readiness", "post_ready_observation"] {
            checks.push(Check::unknown(
                name,
                "dependency_unavailable",
                "matching current-launch evidence is unavailable",
            ));
        }
        return;
    };

    if evidence.outcome == Outcome::IntentionalStop
        && evidence.process == ProcessState::Stopped
        && evidence.complete
    {
        checks.push(Check::pass(
            "intentional_stop",
            "observation completed with a successful intentional stop",
        ));
    } else {
        checks.push(Check::fail(
            "intentional_stop",
            "insufficient_evidence",
            format!(
                "requires complete intentional_stop; observed outcome={} process={} complete={}",
                evidence.outcome.as_str(),
                evidence.process.as_str(),
                evidence.complete
            ),
        ));
    }

    if evidence.ready && evidence.readiness == ReadinessState::Ready {
        checks.push(Check::pass(
            "readiness",
            "matching launch reported the Home-ready acknowledgment",
        ));
    } else {
        checks.push(Check::fail(
            "readiness",
            "insufficient_evidence",
            "matching launch did not confirm readiness",
        ));
    }

    match evidence.post_ready_observed_ms {
        Some(milliseconds) if milliseconds >= MIN_POST_READY_MS => checks.push(Check::pass(
            "post_ready_observation",
            format!("monotonic post-ready observation={}ms", milliseconds),
        )),
        Some(milliseconds) => checks.push(Check::fail(
            "post_ready_observation",
            "insufficient_evidence",
            format!(
                "monotonic post-ready observation={}ms; required={}ms",
                milliseconds, MIN_POST_READY_MS
            ),
        )),
        None => checks.push(Check::fail(
            "post_ready_observation",
            "insufficient_evidence",
            "no monotonic post-ready duration is recorded",
        )),
    }
}

fn load_snapshot(paths: &AssessmentPaths) -> Result<ValidatedSnapshot, EvidenceError> {
    let pending = load_pending(paths)?;
    let inventory_sha256 = validate_retained(paths, &pending)?;
    let owner_bytes = read_bounded_regular(
        &paths.update_dir.join(OWNER_NAME),
        OWNER_MAX_BYTES,
        "launch-health owner",
    )?;
    let owner = parse_owner(&owner_bytes)?;
    let evidence_bytes = read_bounded_regular(
        &paths.update_dir.join(EVIDENCE_NAME),
        EVIDENCE_MAX_BYTES,
        "health evidence",
    )?;
    let evidence = EvidenceRecord::parse(&evidence_bytes)?;
    if evidence.candidate != pending.candidate {
        return Err(invalid(
            "health evidence does not match the current pending candidate",
        ));
    }
    if owner.attempt.launch_uuid != evidence.launch_uuid {
        return Err(invalid(
            "health evidence does not belong to the current launch owner",
        ));
    }
    Ok(ValidatedSnapshot {
        pending,
        inventory_sha256,
        health: HealthAssessment { owner, evidence },
    })
}

fn validate_retained(
    paths: &AssessmentPaths,
    pending: &PendingAssessment,
) -> Result<String, EvidenceError> {
    ensure_no_symlink_components(&paths.previous_dir)?;
    let metadata = fs::symlink_metadata(&paths.previous_dir)
        .map_err(|error| invalid(format!("retained release directory unavailable: {error}")))?;
    if !metadata.file_type().is_dir() {
        return Err(invalid("retained release path is not a directory"));
    }
    validate_retained_tree(&paths.previous_dir)?;

    let inventory_path = paths.previous_dir.join(INVENTORY_NAME);
    let bytes = read_bounded_regular(&inventory_path, INVENTORY_MAX_BYTES, "retained inventory")?;
    if bytes.len() as u64 != pending.inventory_bytes {
        return Err(invalid(
            "retained inventory byte count does not match pending-health",
        ));
    }
    let digest = sha256_bytes(&bytes);
    if digest != pending.inventory_sha256 {
        return Err(invalid(
            "retained inventory digest does not match pending-health",
        ));
    }
    validate_inventory_bytes(&paths.previous_dir, &bytes)?;
    Ok(digest)
}

fn validate_retained_tree(previous_dir: &Path) -> Result<(), EvidenceError> {
    let mut allowed: BTreeSet<&str> = RETAINED_ENTRIES.into_iter().collect();
    allowed.insert(INVENTORY_NAME);
    let mut seen = BTreeSet::new();
    for entry in fs::read_dir(previous_dir)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("retained release contains a non-UTF-8 path"))?;
        if !allowed.contains(name.as_str()) {
            return Err(invalid(format!(
                "retained release contains unexpected path {name:?}"
            )));
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(invalid(format!(
                "retained release path {name:?} is not a regular file"
            )));
        }
        seen.insert(name);
    }
    for required in REQUIRED_RETAINED {
        if !seen.contains(required) {
            return Err(invalid(format!(
                "required retained file is missing: {required}"
            )));
        }
    }
    if !seen.contains(INVENTORY_NAME) {
        return Err(invalid("retained inventory is missing"));
    }
    Ok(())
}

fn validate_inventory_bytes(previous_dir: &Path, bytes: &[u8]) -> Result<(), EvidenceError> {
    if bytes.iter().filter(|byte| **byte == b'\n').count() != 22
        || !bytes.ends_with(b"\n")
        || bytes.contains(&b'\r')
        || bytes.contains(&0)
    {
        return Err(invalid("retained inventory has noncanonical framing"));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid("retained inventory is not canonical UTF-8/ASCII"))?;
    let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    if lines.len() != 22
        || lines[0] != INVENTORY_HEADER
        || lines[1] != "schema=1"
        || lines[2] != "kind=ignite-application-release"
        || lines[3] != "entry_count=18"
    {
        return Err(invalid(
            "retained inventory has unsupported or malformed fields",
        ));
    }

    let mut canonical = String::new();
    canonical.push_str(INVENTORY_HEADER);
    canonical.push_str("\nschema=1\nkind=ignite-application-release\nentry_count=18\n");
    for (index, name) in RETAINED_ENTRIES.iter().enumerate() {
        let prefix = format!("entry={name}|");
        let value = lines[index + 4]
            .strip_prefix(&prefix)
            .ok_or_else(|| invalid(format!("retained inventory entry order differs at {name}")))?;
        let path = previous_dir.join(name);
        if value == "absent|0|-" {
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => {
                    return Err(invalid(format!(
                        "retained inventory marks present path {name} absent"
                    )))
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            let mut fields = value.split('|');
            let kind = fields.next();
            let byte_text = fields.next();
            let digest = fields.next();
            if kind != Some("file") || fields.next().is_some() {
                return Err(invalid(format!(
                    "retained inventory entry is malformed: {name}"
                )));
            }
            let byte_text = byte_text.ok_or_else(|| invalid("inventory byte count missing"))?;
            let expected_bytes = canonical_u64(byte_text)?;
            let digest = digest.ok_or_else(|| invalid("inventory digest missing"))?;
            if !valid_sha256(digest) {
                return Err(invalid(format!(
                    "retained inventory digest is malformed: {name}"
                )));
            }
            verify_retained_file(&path, expected_bytes, digest, name)?;
        }
        writeln!(&mut canonical, "{}", lines[index + 4])
            .map_err(|_| invalid("cannot reconstruct retained inventory"))?;
    }
    if canonical.as_bytes() != bytes {
        return Err(invalid("retained inventory bytes are noncanonical"));
    }
    Ok(())
}

fn verify_retained_file(
    path: &Path,
    expected_bytes: u64,
    expected_sha256: &str,
    name: &str,
) -> Result<(), EvidenceError> {
    refuse_symlink(path, "retained inventory entry")?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| invalid(format!("cannot open retained file {name}: {error}")))?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.len() != expected_bytes
        || metadata.len() > RETAINED_FILE_MAX_BYTES
    {
        return Err(invalid(format!(
            "retained file byte identity does not match inventory: {name}"
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
        copied = copied
            .checked_add(count as u64)
            .ok_or_else(|| invalid("retained file byte count overflowed"))?;
        if copied > RETAINED_FILE_MAX_BYTES {
            return Err(invalid(format!(
                "retained file exceeds hashing bound: {name}"
            )));
        }
        hasher.update(&buffer[..count]);
    }
    if copied != metadata.len() || hex_digest(hasher.finalize().as_slice()) != expected_sha256 {
        return Err(invalid(format!(
            "retained file digest identity does not match inventory: {name}"
        )));
    }
    Ok(())
}

fn path_missing(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    )
}

fn printable_detail(detail: &str) -> String {
    detail
        .chars()
        .map(|character| {
            if character.is_ascii_graphic() || character == ' ' {
                character
            } else {
                '?'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use uuid::Uuid;

    const LAUNCH: &str = "00112233445566778899aabbccddeeff";

    struct AssessmentTree {
        root: PathBuf,
        paths: AssessmentPaths,
    }

    impl AssessmentTree {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "ignite-promotion-assessment-{}",
                Uuid::new_v4().simple()
            ));
            let app_root = root.join("scribe-app");
            let update_dir = app_root.join("state/update");
            let bin = app_root.join("bin");
            let previous_dir = app_root.join("previous-release");
            fs::create_dir_all(&update_dir).unwrap();
            fs::create_dir_all(&bin).unwrap();
            fs::create_dir_all(&previous_dir).unwrap();
            let app = bin.join("scribe-app");
            let lifecycle = bin.join("scribe-lifecycle");
            fs::write(&app, b"candidate app B").unwrap();
            fs::write(&lifecycle, b"candidate lifecycle B").unwrap();
            fs::write(previous_dir.join("scribe-app"), b"retained app A").unwrap();
            fs::write(
                previous_dir.join("scribe-lifecycle"),
                b"retained lifecycle A",
            )
            .unwrap();
            fs::write(
                previous_dir.join("scribe-lifecycle-host.sh"),
                b"#!/bin/sh\n",
            )
            .unwrap();

            let paths = AssessmentPaths {
                update_dir,
                app,
                lifecycle,
                previous_dir,
            };
            let tree = Self { root, paths };
            tree.write_inventory_and_pending();
            tree.write_successful_evidence();
            tree
        }

        fn write_inventory_and_pending(&self) {
            let inventory = self.inventory_bytes();
            fs::write(self.paths.previous_dir.join(INVENTORY_NAME), &inventory).unwrap();
            let app = fs::read(&self.paths.app).unwrap();
            let lifecycle = fs::read(&self.paths.lifecycle).unwrap();
            let inventory_sha = sha256_bytes(&inventory);
            let pending = format!(
                "IGNITE_PENDING_HEALTH_V1\n\
                 schema=1\n\
                 status=pending-health\n\
                 candidate_version=7.8.1\n\
                 candidate_txn=20261024T120000Z-123\n\
                 candidate_app_bytes={}\n\
                 candidate_app_sha256={}\n\
                 candidate_lifecycle_bytes={}\n\
                 candidate_lifecycle_sha256={}\n\
                 retained_version=unknown\n\
                 retained_release_identity=sha256:{}\n\
                 lkg_inventory_bytes={}\n\
                 lkg_inventory_sha256={}\n\
                 data_compatibility=unknown\n",
                app.len(),
                sha256_bytes(&app),
                lifecycle.len(),
                sha256_bytes(&lifecycle),
                inventory_sha,
                inventory.len(),
                inventory_sha,
            );
            fs::write(self.paths.update_dir.join(PENDING_NAME), pending).unwrap();
        }

        fn inventory_bytes(&self) -> Vec<u8> {
            let mut inventory = format!(
                "{INVENTORY_HEADER}\nschema=1\nkind=ignite-application-release\nentry_count=18\n"
            );
            for name in RETAINED_ENTRIES {
                let path = self.paths.previous_dir.join(name);
                match fs::read(&path) {
                    Ok(bytes) => writeln!(
                        &mut inventory,
                        "entry={name}|file|{}|{}",
                        bytes.len(),
                        sha256_bytes(&bytes)
                    )
                    .unwrap(),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        writeln!(&mut inventory, "entry={name}|absent|0|-").unwrap()
                    }
                    Err(error) => panic!("inventory fixture: {error}"),
                }
            }
            inventory.into_bytes()
        }

        fn recorder(&self, launch: &str, start: Instant) -> EvidenceRecorder {
            let ownership = LaunchOwnership::claim(
                &self.paths.update_dir,
                LaunchAttempt::capture(launch).unwrap(),
            )
            .unwrap();
            EvidenceRecorder::prepare(ownership, &self.paths.app, &self.paths.lifecycle, start)
                .unwrap()
        }

        fn write_successful_evidence(&self) {
            let start = Instant::now();
            let mut recorder = self.recorder(LAUNCH, start);
            recorder.record_spawned().unwrap();
            recorder
                .record_ready(start + Duration::from_secs(1))
                .unwrap();
            recorder
                .record_intentional_stop(true, start + Duration::from_secs(62))
                .unwrap();
        }

        fn assess(&self) -> PromotionAssessmentReport {
            assess_paths(&self.paths, || {})
        }

        fn assess_under_boundary(&self, latest_launch_uuid: &str) -> PromotionAssessmentReport {
            let lock = acquire_decision_evidence_lock(&self.paths.update_dir, Duration::ZERO)
                .expect("exclusive decision evidence lock");
            assess_paths_while_locked(
                &self.paths,
                HeldAssessmentLock::Decision {
                    _evidence_lock: &lock,
                    latest_launch_uuid,
                },
                || {},
            )
        }
    }

    impl Drop for AssessmentTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn retained_entry_contract_matches_installer_order() {
        let installer = include_str!("../../../../../deployment/kindle/lib/install-runtime.sh");
        assert!(installer.contains("LKG_HANDLER_NAME=appreg-handler.sql"));
        let start = installer.find("write_lkg_inventory() {").unwrap();
        let end = installer[start..]
            .find("validate_lkg_inventory() {")
            .map(|offset| start + offset)
            .unwrap();
        let contract = &installer[start..end];
        assert!(contract.contains("entry_count=18"));
        let mut cursor = 0;
        for token in [
            "scribe-app",
            "scribe-lifecycle",
            "scribe-lifecycle-host.sh",
            "Scribe.sh",
            "scribe-icon.png",
            "OFL.txt",
            "DESCRIPTION.en_us.html",
            "upstream_info.md",
            "source-sans-3-OFL.txt",
            "install-runtime.sh",
            "uninstall-runtime.sh",
            "backup-runtime.sh",
            "register-helpers.sh",
            "install-recovery.sh",
            "Uninstall Ignite.sh",
            "Backup Ignite Data.sh",
            "Diagnose Ignite.sh",
            "$LKG_HANDLER_NAME",
        ] {
            let offset = contract[cursor..].find(token).unwrap_or_else(|| {
                panic!("installer inventory token missing or reordered: {token}")
            });
            cursor += offset + token.len();
        }
    }

    #[test]
    fn valid_runtime_evidence_requires_explicit_compatibility_review() {
        let tree = AssessmentTree::new();
        let report = tree.assess();
        assert_eq!(report.overall(), PromotionAssessment::ReviewRequired);
        for name in [
            "pending_and_live_candidate",
            "retained_release",
            "ownership_record",
            "evidence_identity",
            "launch_ownership",
            "intentional_stop",
            "readiness",
            "post_ready_observation",
            "state_stability",
        ] {
            assert_eq!(report.check(name).status, CheckStatus::Pass, "{name}");
        }
        assert_eq!(
            report.check("data_compatibility").status,
            CheckStatus::Unknown
        );
        assert_eq!(report.exit_code(), 3);
        assert!(report.to_string().contains("promotion_authorized=false\n"));
    }

    #[test]
    fn boundary_assessment_reuses_b1_without_relocking_and_binds_latest_launch() {
        let tree = AssessmentTree::new();
        let report = tree
            .assess_under_boundary(LAUNCH)
            .record_boundary_confirmation(LAUNCH, Ok(()));
        assert_eq!(report.overall(), PromotionAssessment::ReviewRequired);
        assert_eq!(report.mode, AssessmentMode::BoundaryReadOnly);
        assert_eq!(report.check("decision_boundary").status, CheckStatus::Pass);
        assert_eq!(report.check("assessment_lock").status, CheckStatus::Pass);
        assert!(report
            .check("assessment_lock")
            .detail
            .contains("no recursive lock acquisition"));
        assert_eq!(
            report.check("lifecycle_generation").status,
            CheckStatus::Pass
        );
        assert_eq!(
            report.check("data_compatibility").status,
            CheckStatus::Unknown
        );
        assert_eq!(report.exit_code(), 3);
        assert!(report.to_string().contains("mode=boundary-read-only\n"));
        assert!(report.to_string().contains("promotion_authorized=false\n"));
    }

    #[test]
    fn boundary_assessment_rejects_stale_host_launch_uuid() {
        let tree = AssessmentTree::new();
        let report = tree.assess_under_boundary("ffeeddccbbaa99887766554433221100");
        assert_eq!(report.overall(), PromotionAssessment::Blocked);
        assert_eq!(
            report.check("lifecycle_generation").status,
            CheckStatus::Fail
        );
        assert_eq!(report.check("lifecycle_generation").class, "stale_evidence");
    }

    #[test]
    fn boundary_assessment_revalidates_changed_pending_and_damaged_executables() {
        let tree = AssessmentTree::new();
        let pending_path = tree.paths.update_dir.join(PENDING_NAME);
        let pending = fs::read_to_string(&pending_path).unwrap().replace(
            "candidate_txn=20261024T120000Z-123",
            "candidate_txn=20261024T120000Z-124",
        );
        fs::write(pending_path, pending).unwrap();
        let changed_pending = tree.assess_under_boundary(LAUNCH);
        assert_eq!(changed_pending.overall(), PromotionAssessment::Blocked);
        assert_eq!(
            changed_pending.check("evidence_identity").status,
            CheckStatus::Fail
        );

        let tree = AssessmentTree::new();
        fs::write(&tree.paths.lifecycle, b"replaced candidate lifecycle").unwrap();
        let damaged_b = tree.assess_under_boundary(LAUNCH);
        assert_eq!(damaged_b.overall(), PromotionAssessment::Blocked);
        assert_eq!(
            damaged_b.check("pending_and_live_candidate").class,
            "corruption"
        );

        let tree = AssessmentTree::new();
        fs::write(
            tree.paths.previous_dir.join("scribe-app"),
            b"replaced retained app",
        )
        .unwrap();
        let damaged_a = tree.assess_under_boundary(LAUNCH);
        assert_eq!(damaged_a.overall(), PromotionAssessment::Blocked);
        assert_eq!(damaged_a.check("retained_release").class, "corruption");
    }

    #[test]
    fn boundary_assessment_rejects_missing_and_malformed_evidence() {
        let tree = AssessmentTree::new();
        fs::remove_file(tree.paths.update_dir.join(EVIDENCE_NAME)).unwrap();
        let missing = tree.assess_under_boundary(LAUNCH);
        assert_eq!(missing.overall(), PromotionAssessment::Blocked);
        assert_eq!(missing.check("evidence_identity").status, CheckStatus::Fail);
        assert_eq!(
            missing.check("evidence_identity").class,
            "insufficient_evidence"
        );
        assert_eq!(
            missing.check("lifecycle_generation").status,
            CheckStatus::Unknown
        );

        let tree = AssessmentTree::new();
        fs::write(
            tree.paths.update_dir.join(EVIDENCE_NAME),
            b"malformed evidence\n",
        )
        .unwrap();
        let malformed = tree.assess_under_boundary(LAUNCH);
        assert_eq!(malformed.overall(), PromotionAssessment::Blocked);
        assert_eq!(malformed.check("evidence_identity").class, "corruption");
    }

    #[test]
    fn stale_owner_blocks_evidence() {
        let tree = AssessmentTree::new();
        let mut owner =
            parse_owner(&fs::read(tree.paths.update_dir.join(OWNER_NAME)).unwrap()).unwrap();
        owner.attempt.launch_uuid = "ffeeddccbbaa99887766554433221100".to_owned();
        fs::write(tree.paths.update_dir.join(OWNER_NAME), owner_bytes(&owner)).unwrap();
        let report = tree.assess();
        assert_eq!(report.overall(), PromotionAssessment::Blocked);
        assert_eq!(report.check("launch_ownership").status, CheckStatus::Fail);
        assert_eq!(
            report.check("launch_ownership").class,
            "insufficient_evidence"
        );
    }

    #[test]
    fn damaged_live_candidate_and_retained_release_are_corruption() {
        let tree = AssessmentTree::new();
        fs::write(&tree.paths.app, b"damaged B").unwrap();
        let damaged_b = tree.assess();
        assert_eq!(damaged_b.overall(), PromotionAssessment::Blocked);
        assert_eq!(
            damaged_b.check("pending_and_live_candidate").class,
            "corruption"
        );

        let tree = AssessmentTree::new();
        fs::write(tree.paths.previous_dir.join("scribe-app"), b"damaged A").unwrap();
        let damaged_a = tree.assess();
        assert_eq!(damaged_a.overall(), PromotionAssessment::Blocked);
        assert_eq!(damaged_a.check("retained_release").class, "corruption");
    }

    #[test]
    fn failed_stop_and_incomplete_evidence_do_not_qualify() {
        let tree = AssessmentTree::new();
        let start = Instant::now();
        let mut recorder = tree.recorder("ffeeddccbbaa99887766554433221100", start);
        recorder.record_spawned().unwrap();
        recorder
            .record_ready(start + Duration::from_secs(1))
            .unwrap();
        recorder
            .record_intentional_stop(false, start + Duration::from_secs(62))
            .unwrap();
        let failed = tree.assess();
        assert_eq!(failed.overall(), PromotionAssessment::Blocked);
        assert_eq!(failed.check("intentional_stop").status, CheckStatus::Fail);

        let tree = AssessmentTree::new();
        let start = Instant::now();
        let mut recorder = tree.recorder("ffeeddccbbaa99887766554433221100", start);
        recorder.record_spawned().unwrap();
        recorder
            .record_ready(start + Duration::from_secs(1))
            .unwrap();
        recorder
            .record_exit(false, start + Duration::from_secs(62))
            .unwrap();
        let crashed = tree.assess();
        assert_eq!(crashed.overall(), PromotionAssessment::Blocked);
        assert_eq!(crashed.check("intentional_stop").status, CheckStatus::Fail);
        assert!(crashed
            .check("intentional_stop")
            .detail
            .contains("outcome=exited_failure"));

        let tree = AssessmentTree::new();
        let start = Instant::now();
        let mut recorder = tree.recorder("ffeeddccbbaa99887766554433221100", start);
        recorder.record_spawned().unwrap();
        recorder
            .record_ready(start + Duration::from_secs(1))
            .unwrap();
        recorder
            .maybe_record_running(start + Duration::from_secs(61), Duration::from_secs(60))
            .unwrap();
        let incomplete = tree.assess();
        assert_eq!(incomplete.overall(), PromotionAssessment::Blocked);
        assert_eq!(
            incomplete.check("intentional_stop").status,
            CheckStatus::Fail
        );
        assert_eq!(
            incomplete.check("post_ready_observation").status,
            CheckStatus::Pass
        );
    }

    #[test]
    fn concurrent_candidate_change_is_detected_between_passes() {
        let tree = AssessmentTree::new();
        let app = tree.paths.app.clone();
        let report = assess_paths(&tree.paths, || {
            let writer = thread::spawn(move || fs::write(app, b"concurrent B change").unwrap());
            writer.join().unwrap();
        });
        assert_eq!(report.overall(), PromotionAssessment::Blocked);
        assert_eq!(report.check("state_stability").status, CheckStatus::Fail);
        assert_eq!(report.check("state_stability").class, "concurrent_change");
    }

    #[test]
    fn assessment_waits_for_evidence_writer_lock_and_remains_read_only() {
        let tree = AssessmentTree::new();
        let before_pending = fs::read(tree.paths.update_dir.join(PENDING_NAME)).unwrap();
        let before_owner = fs::read(tree.paths.update_dir.join(OWNER_NAME)).unwrap();
        let before_evidence = fs::read(tree.paths.update_dir.join(EVIDENCE_NAME)).unwrap();
        let before_candidate = fs::read(&tree.paths.app).unwrap();
        let before_retained = fs::read(tree.paths.previous_dir.join("scribe-app")).unwrap();
        let before_inventory = fs::read(tree.paths.previous_dir.join(INVENTORY_NAME)).unwrap();
        let before_update_names: BTreeSet<_> = fs::read_dir(&tree.paths.update_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let before_retained_names: BTreeSet<_> = fs::read_dir(&tree.paths.previous_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(tree.paths.update_dir.join(EVIDENCE_LOCK_NAME))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) },
            0
        );
        let paths = tree.paths.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || sender.send(assess_paths(&paths, || {})).unwrap());
        assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
        assert_eq!(
            unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN) },
            0
        );
        let report = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        assert_eq!(report.overall(), PromotionAssessment::ReviewRequired);
        assert_eq!(
            fs::read(tree.paths.update_dir.join(PENDING_NAME)).unwrap(),
            before_pending
        );
        assert_eq!(
            fs::read(tree.paths.update_dir.join(OWNER_NAME)).unwrap(),
            before_owner
        );
        assert_eq!(
            fs::read(tree.paths.update_dir.join(EVIDENCE_NAME)).unwrap(),
            before_evidence
        );
        assert_eq!(fs::read(&tree.paths.app).unwrap(), before_candidate);
        assert_eq!(
            fs::read(tree.paths.previous_dir.join("scribe-app")).unwrap(),
            before_retained
        );
        assert_eq!(
            fs::read(tree.paths.previous_dir.join(INVENTORY_NAME)).unwrap(),
            before_inventory
        );
        assert_eq!(
            fs::read_dir(&tree.paths.update_dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<BTreeSet<_>>(),
            before_update_names
        );
        assert_eq!(
            fs::read_dir(&tree.paths.previous_dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<BTreeSet<_>>(),
            before_retained_names
        );
    }
}
