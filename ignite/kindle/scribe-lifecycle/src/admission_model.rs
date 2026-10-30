//! Offline reference model for a future pre-first-launch admission transaction.
//!
//! This module is compiled only for tests. It has no production command, filesystem path, parser,
//! or constructor that can turn device input into authorization. Synthetic positive evidence exists
//! solely to exercise the intended fail-closed state machine.

use sha2::{Digest as _, Sha256};

const AUTHORIZATION_SCHEMA: u32 = 1;
const TEST_ISSUANCE_CONTEXT: &str = "IGNITE_TEST_ONLY_ADMISSION_MODEL_V1";

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Digest([u8; 32]);

impl Digest {
    fn synthetic(label: &str) -> Self {
        Self(Sha256::digest(label.as_bytes()).into())
    }

    fn hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InstalledReleaseIdentity {
    component_closure: Digest,
    app: Digest,
    lifecycle: Digest,
    host: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CandidateBinding {
    transaction: String,
    pending_record: Digest,
    retained_a: InstalledReleaseIdentity,
    candidate_b: InstalledReleaseIdentity,
    retained_inventory: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LifecycleGeneration {
    host_lease: Digest,
    control_socket_device: u64,
    control_socket_inode: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Evidence<T> {
    Missing,
    Diagnostic(T),
    Trusted(T),
}

impl<T> Evidence<T> {
    fn trusted(&self) -> Option<&T> {
        match self {
            Self::Trusted(value) => Some(value),
            Self::Missing | Self::Diagnostic(_) => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExclusionProof {
    identity: Digest,
    complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CompatibilityProof {
    identity: Digest,
    bidirectional: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FutureWriteProof {
    identity: Digest,
    retained_a_reads_every_b_state: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AssessmentInputs {
    transaction: Evidence<String>,
    pending_record: Evidence<Digest>,
    retained_a: Evidence<InstalledReleaseIdentity>,
    candidate_b: Evidence<InstalledReleaseIdentity>,
    retained_inventory: Evidence<Digest>,
    retained_a_provenance: Evidence<Digest>,
    candidate_b_provenance: Evidence<Digest>,
    retained_a_capabilities: Evidence<Digest>,
    candidate_b_capabilities: Evidence<Digest>,
    stable_inventory_before: Evidence<Digest>,
    stable_inventory_after: Evidence<Digest>,
    writer_exclusion: Evidence<ExclusionProof>,
    usb_exclusion: Evidence<ExclusionProof>,
    compatibility: Evidence<CompatibilityProof>,
    future_write_compatibility: Evidence<FutureWriteProof>,
    lifecycle_generation: Evidence<LifecycleGeneration>,
    all_required_locks_held: bool,
    lock_contended: bool,
    assessment_interrupted: bool,
    conflicting_pending_activation: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum RefusalReason {
    ConflictingPendingActivation,
    AssessmentInterrupted,
    LockContention,
    MissingTrustedTransaction,
    WrongTransaction,
    MissingTrustedPendingRecord,
    WrongPendingRecord,
    MissingProvenance,
    WrongRetainedIdentity,
    WrongCandidateIdentity,
    WrongRetainedInventory,
    MissingCapabilities,
    ChangedInventory,
    MissingWriterExclusion,
    UnsupportedUsbExclusion,
    CompatibilityNotPositive,
    FutureWriteCompatibilityIncomplete,
    RetainedANotCompatibleWithBWrites,
    StaleLifecycleGeneration,
    StaleAssessment,
    TamperedAuthorization,
    CandidateChangedAfterAssessment,
    MissingHandoffCapability,
    ReplayedAuthorization,
    DuplicateAdmission,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthorizationBinding {
    schema: u32,
    issuance_context: &'static str,
    candidate: CandidateBinding,
    retained_a_provenance: Digest,
    candidate_b_provenance: Digest,
    retained_a_capabilities: Digest,
    candidate_b_capabilities: Digest,
    stable_inventory: Digest,
    writer_exclusion: Digest,
    usb_exclusion: Digest,
    compatibility: Digest,
    future_write_compatibility: Digest,
    lifecycle_generation: LifecycleGeneration,
}

impl AuthorizationBinding {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = String::new();
        bytes.push_str("IGNITE_TEST_ONLY_ADMISSION_AUTHORIZATION_V1\n");
        bytes.push_str(&format!("schema={}\n", self.schema));
        bytes.push_str(&format!("issuance_context={}\n", self.issuance_context));
        bytes.push_str(&format!("transaction={}\n", self.candidate.transaction));
        bytes.push_str(&format!(
            "pending_record={}\n",
            self.candidate.pending_record.hex()
        ));
        for (name, identity) in [
            ("retained_a", &self.candidate.retained_a),
            ("candidate_b", &self.candidate.candidate_b),
        ] {
            bytes.push_str(&format!(
                "{name}_closure={}\n{name}_app={}\n{name}_lifecycle={}\n{name}_host={}\n",
                identity.component_closure.hex(),
                identity.app.hex(),
                identity.lifecycle.hex(),
                identity.host.hex()
            ));
        }
        for (name, digest) in [
            ("retained_inventory", &self.candidate.retained_inventory),
            ("retained_a_provenance", &self.retained_a_provenance),
            ("candidate_b_provenance", &self.candidate_b_provenance),
            ("retained_a_capabilities", &self.retained_a_capabilities),
            ("candidate_b_capabilities", &self.candidate_b_capabilities),
            ("stable_inventory", &self.stable_inventory),
            ("writer_exclusion", &self.writer_exclusion),
            ("usb_exclusion", &self.usb_exclusion),
            ("compatibility", &self.compatibility),
            (
                "future_write_compatibility",
                &self.future_write_compatibility,
            ),
            ("host_lease", &self.lifecycle_generation.host_lease),
        ] {
            bytes.push_str(&format!("{name}={}\n", digest.hex()));
        }
        bytes.push_str(&format!(
            "control_socket_device={}\ncontrol_socket_inode={}\n",
            self.lifecycle_generation.control_socket_device,
            self.lifecycle_generation.control_socket_inode
        ));
        bytes.into_bytes()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthorizationRecord {
    binding: AuthorizationBinding,
    digest: Digest,
}

impl AuthorizationRecord {
    fn new(binding: AuthorizationBinding) -> Self {
        let digest = Digest(Sha256::digest(binding.canonical_bytes()).into());
        Self { binding, digest }
    }

    fn valid(&self) -> bool {
        self.digest == Digest(Sha256::digest(self.binding.canonical_bytes()).into())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HandoffCapability {
    authorization: Digest,
    lifecycle_generation: LifecycleGeneration,
    nonce: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AdmissionState {
    NoPendingCandidate,
    CandidatePrepared(CandidateBinding),
    AdmissionRequired(CandidateBinding),
    AssessmentInProgress {
        candidate: CandidateBinding,
        assessment: u64,
    },
    AdmissionRefused {
        candidate: CandidateBinding,
        reasons: Vec<RefusalReason>,
    },
    AdmissionAuthorized(AuthorizationRecord),
    FirstLaunchInProgress {
        record: AuthorizationRecord,
        capability: HandoffCapability,
    },
    FirstLaunchConsumed(AuthorizationRecord),
}

#[derive(Debug)]
struct AdmissionModel {
    state: AdmissionState,
    next_assessment: u64,
    next_capability: u64,
}

impl AdmissionModel {
    fn new() -> Self {
        Self {
            state: AdmissionState::NoPendingCandidate,
            next_assessment: 1,
            next_capability: 1,
        }
    }

    fn prepare(&mut self, candidate: CandidateBinding) -> Result<(), RefusalReason> {
        if !matches!(self.state, AdmissionState::NoPendingCandidate) {
            return Err(RefusalReason::ConflictingPendingActivation);
        }
        self.state = AdmissionState::CandidatePrepared(candidate);
        Ok(())
    }

    fn require_admission(&mut self) -> Result<(), RefusalReason> {
        let AdmissionState::CandidatePrepared(candidate) = &self.state else {
            return Err(RefusalReason::ConflictingPendingActivation);
        };
        self.state = AdmissionState::AdmissionRequired(candidate.clone());
        Ok(())
    }

    fn begin_assessment(&mut self) -> Result<u64, RefusalReason> {
        let candidate = match &self.state {
            AdmissionState::AdmissionRequired(candidate)
            | AdmissionState::AdmissionRefused { candidate, .. } => candidate.clone(),
            AdmissionState::AssessmentInProgress { .. } => {
                return Err(RefusalReason::DuplicateAdmission);
            }
            _ => return Err(RefusalReason::ConflictingPendingActivation),
        };
        let assessment = self.next_assessment;
        self.next_assessment += 1;
        self.state = AdmissionState::AssessmentInProgress {
            candidate,
            assessment,
        };
        Ok(assessment)
    }

    fn finish_assessment(
        &mut self,
        assessment: u64,
        inputs: &AssessmentInputs,
    ) -> Result<AuthorizationRecord, Vec<RefusalReason>> {
        let AdmissionState::AssessmentInProgress {
            candidate,
            assessment: current,
        } = &self.state
        else {
            return Err(vec![RefusalReason::StaleAssessment]);
        };
        if *current != assessment {
            return Err(vec![RefusalReason::StaleAssessment]);
        }
        let candidate = candidate.clone();
        match evaluate(&candidate, inputs) {
            Ok(binding) => {
                let record = AuthorizationRecord::new(binding);
                self.state = AdmissionState::AdmissionAuthorized(record.clone());
                Ok(record)
            }
            Err(reasons) => {
                self.state = AdmissionState::AdmissionRefused {
                    candidate,
                    reasons: reasons.clone(),
                };
                Err(reasons)
            }
        }
    }

    fn begin_first_launch(
        &mut self,
        supplied_record: &AuthorizationRecord,
        current_candidate: &CandidateBinding,
        current_generation: &LifecycleGeneration,
    ) -> Result<HandoffCapability, RefusalReason> {
        let AdmissionState::AdmissionAuthorized(expected) = &self.state else {
            return Err(match self.state {
                AdmissionState::FirstLaunchInProgress { .. }
                | AdmissionState::FirstLaunchConsumed(_) => RefusalReason::DuplicateAdmission,
                _ => RefusalReason::ReplayedAuthorization,
            });
        };
        if !supplied_record.valid() {
            return Err(RefusalReason::TamperedAuthorization);
        }
        if supplied_record != expected {
            return Err(RefusalReason::ReplayedAuthorization);
        }
        if &supplied_record.binding.candidate != current_candidate {
            self.state = AdmissionState::AdmissionRequired(current_candidate.clone());
            return Err(RefusalReason::CandidateChangedAfterAssessment);
        }
        if &supplied_record.binding.lifecycle_generation != current_generation {
            self.state =
                AdmissionState::AdmissionRequired(supplied_record.binding.candidate.clone());
            return Err(RefusalReason::StaleLifecycleGeneration);
        }
        let nonce = Digest::synthetic(&format!(
            "{}:{}",
            supplied_record.digest.hex(),
            self.next_capability
        ));
        self.next_capability += 1;
        let capability = HandoffCapability {
            authorization: supplied_record.digest.clone(),
            lifecycle_generation: current_generation.clone(),
            nonce,
        };
        self.state = AdmissionState::FirstLaunchInProgress {
            record: supplied_record.clone(),
            capability: capability.clone(),
        };
        Ok(capability)
    }

    fn consume_first_launch(
        &mut self,
        capability: Option<&HandoffCapability>,
        current_candidate: &CandidateBinding,
    ) -> Result<(), RefusalReason> {
        let Some(capability) = capability else {
            return Err(RefusalReason::MissingHandoffCapability);
        };
        let AdmissionState::FirstLaunchInProgress {
            record,
            capability: expected,
        } = &self.state
        else {
            return Err(RefusalReason::ReplayedAuthorization);
        };
        if capability != expected {
            return Err(RefusalReason::ReplayedAuthorization);
        }
        if &record.binding.candidate != current_candidate {
            return Err(RefusalReason::CandidateChangedAfterAssessment);
        }
        let record = record.clone();
        self.state = AdmissionState::FirstLaunchConsumed(record);
        Ok(())
    }

    fn crash_and_restart(&mut self) {
        self.state = match &self.state {
            AdmissionState::AssessmentInProgress { candidate, .. }
            | AdmissionState::AdmissionAuthorized(AuthorizationRecord {
                binding: AuthorizationBinding { candidate, .. },
                ..
            })
            | AdmissionState::FirstLaunchInProgress {
                record:
                    AuthorizationRecord {
                        binding: AuthorizationBinding { candidate, .. },
                        ..
                    },
                ..
            } => AdmissionState::AdmissionRequired(candidate.clone()),
            other => other.clone(),
        };
    }
}

fn evaluate(
    expected: &CandidateBinding,
    inputs: &AssessmentInputs,
) -> Result<AuthorizationBinding, Vec<RefusalReason>> {
    let mut refused = Vec::new();
    if inputs.conflicting_pending_activation {
        refused.push(RefusalReason::ConflictingPendingActivation);
    }
    if inputs.assessment_interrupted {
        refused.push(RefusalReason::AssessmentInterrupted);
    }
    if inputs.lock_contended || !inputs.all_required_locks_held {
        refused.push(RefusalReason::LockContention);
    }

    match inputs.transaction.trusted() {
        None => refused.push(RefusalReason::MissingTrustedTransaction),
        Some(transaction) if transaction != &expected.transaction => {
            refused.push(RefusalReason::WrongTransaction);
        }
        Some(_) => {}
    }
    match inputs.pending_record.trusted() {
        None => refused.push(RefusalReason::MissingTrustedPendingRecord),
        Some(pending) if pending != &expected.pending_record => {
            refused.push(RefusalReason::WrongPendingRecord);
        }
        Some(_) => {}
    }
    match inputs.retained_a.trusted() {
        Some(identity) if identity == &expected.retained_a => {}
        Some(_) => refused.push(RefusalReason::WrongRetainedIdentity),
        None => refused.push(RefusalReason::MissingProvenance),
    }
    match inputs.candidate_b.trusted() {
        Some(identity) if identity == &expected.candidate_b => {}
        Some(_) => refused.push(RefusalReason::WrongCandidateIdentity),
        None => refused.push(RefusalReason::MissingProvenance),
    }
    match inputs.retained_inventory.trusted() {
        Some(identity) if identity == &expected.retained_inventory => {}
        Some(_) => refused.push(RefusalReason::WrongRetainedInventory),
        None => refused.push(RefusalReason::MissingProvenance),
    }

    let retained_a_provenance = required_digest(
        &inputs.retained_a_provenance,
        RefusalReason::MissingProvenance,
        &mut refused,
    );
    let candidate_b_provenance = required_digest(
        &inputs.candidate_b_provenance,
        RefusalReason::MissingProvenance,
        &mut refused,
    );
    let retained_a_capabilities = required_digest(
        &inputs.retained_a_capabilities,
        RefusalReason::MissingCapabilities,
        &mut refused,
    );
    let candidate_b_capabilities = required_digest(
        &inputs.candidate_b_capabilities,
        RefusalReason::MissingCapabilities,
        &mut refused,
    );

    let stable_before = inputs.stable_inventory_before.trusted().cloned();
    let stable_after = inputs.stable_inventory_after.trusted().cloned();
    if stable_before.is_none() || stable_after.is_none() || stable_before != stable_after {
        refused.push(RefusalReason::ChangedInventory);
    }

    let writer = inputs
        .writer_exclusion
        .trusted()
        .filter(|proof| proof.complete);
    if writer.is_none() {
        refused.push(RefusalReason::MissingWriterExclusion);
    }
    let usb = inputs
        .usb_exclusion
        .trusted()
        .filter(|proof| proof.complete);
    if usb.is_none() {
        refused.push(RefusalReason::UnsupportedUsbExclusion);
    }
    let compatibility = inputs
        .compatibility
        .trusted()
        .filter(|proof| proof.bidirectional);
    if compatibility.is_none() {
        refused.push(RefusalReason::CompatibilityNotPositive);
    }
    let future = inputs.future_write_compatibility.trusted();
    match future {
        None => refused.push(RefusalReason::FutureWriteCompatibilityIncomplete),
        Some(proof) if !proof.retained_a_reads_every_b_state => {
            refused.push(RefusalReason::RetainedANotCompatibleWithBWrites);
        }
        Some(_) => {}
    }
    let lifecycle_generation = inputs.lifecycle_generation.trusted().cloned();
    if lifecycle_generation.is_none() {
        refused.push(RefusalReason::StaleLifecycleGeneration);
    }

    refused.sort();
    refused.dedup();
    if !refused.is_empty() {
        return Err(refused);
    }
    Ok(AuthorizationBinding {
        schema: AUTHORIZATION_SCHEMA,
        issuance_context: TEST_ISSUANCE_CONTEXT,
        candidate: expected.clone(),
        retained_a_provenance: retained_a_provenance.expect("checked above"),
        candidate_b_provenance: candidate_b_provenance.expect("checked above"),
        retained_a_capabilities: retained_a_capabilities.expect("checked above"),
        candidate_b_capabilities: candidate_b_capabilities.expect("checked above"),
        stable_inventory: stable_before.expect("checked above"),
        writer_exclusion: writer.expect("checked above").identity.clone(),
        usb_exclusion: usb.expect("checked above").identity.clone(),
        compatibility: compatibility.expect("checked above").identity.clone(),
        future_write_compatibility: future.expect("checked above").identity.clone(),
        lifecycle_generation: lifecycle_generation.expect("checked above"),
    })
}

fn required_digest(
    evidence: &Evidence<Digest>,
    reason: RefusalReason,
    refused: &mut Vec<RefusalReason>,
) -> Option<Digest> {
    match evidence.trusted() {
        Some(value) => Some(value.clone()),
        None => {
            refused.push(reason);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(label: &str) -> InstalledReleaseIdentity {
        InstalledReleaseIdentity {
            component_closure: Digest::synthetic(&format!("{label}:closure")),
            app: Digest::synthetic(&format!("{label}:app")),
            lifecycle: Digest::synthetic(&format!("{label}:lifecycle")),
            host: Digest::synthetic(&format!("{label}:host")),
        }
    }

    fn candidate(label: &str) -> CandidateBinding {
        CandidateBinding {
            transaction: format!("txn-{label}"),
            pending_record: Digest::synthetic(&format!("{label}:pending")),
            retained_a: identity(&format!("{label}:a")),
            candidate_b: identity(&format!("{label}:b")),
            retained_inventory: Digest::synthetic(&format!("{label}:retained-inventory")),
        }
    }

    fn generation(label: &str) -> LifecycleGeneration {
        LifecycleGeneration {
            host_lease: Digest::synthetic(&format!("{label}:host-lease")),
            control_socket_device: 10,
            control_socket_inode: 20,
        }
    }

    fn positive(candidate: &CandidateBinding) -> AssessmentInputs {
        let root = Digest::synthetic("stable-root");
        AssessmentInputs {
            transaction: Evidence::Trusted(candidate.transaction.clone()),
            pending_record: Evidence::Trusted(candidate.pending_record.clone()),
            retained_a: Evidence::Trusted(candidate.retained_a.clone()),
            candidate_b: Evidence::Trusted(candidate.candidate_b.clone()),
            retained_inventory: Evidence::Trusted(candidate.retained_inventory.clone()),
            retained_a_provenance: Evidence::Trusted(Digest::synthetic("a-provenance")),
            candidate_b_provenance: Evidence::Trusted(Digest::synthetic("b-provenance")),
            retained_a_capabilities: Evidence::Trusted(Digest::synthetic("a-capabilities")),
            candidate_b_capabilities: Evidence::Trusted(Digest::synthetic("b-capabilities")),
            stable_inventory_before: Evidence::Trusted(root.clone()),
            stable_inventory_after: Evidence::Trusted(root),
            writer_exclusion: Evidence::Trusted(ExclusionProof {
                identity: Digest::synthetic("writer-exclusion"),
                complete: true,
            }),
            usb_exclusion: Evidence::Trusted(ExclusionProof {
                identity: Digest::synthetic("usb-exclusion"),
                complete: true,
            }),
            compatibility: Evidence::Trusted(CompatibilityProof {
                identity: Digest::synthetic("compatibility"),
                bidirectional: true,
            }),
            future_write_compatibility: Evidence::Trusted(FutureWriteProof {
                identity: Digest::synthetic("future-write"),
                retained_a_reads_every_b_state: true,
            }),
            lifecycle_generation: Evidence::Trusted(generation("one")),
            all_required_locks_held: true,
            lock_contended: false,
            assessment_interrupted: false,
            conflicting_pending_activation: false,
        }
    }

    fn authorize(model: &mut AdmissionModel, expected: &CandidateBinding) -> AuthorizationRecord {
        model.prepare(expected.clone()).unwrap();
        model.require_admission().unwrap();
        let assessment = model.begin_assessment().unwrap();
        model
            .finish_assessment(assessment, &positive(expected))
            .unwrap()
    }

    fn assert_refused(inputs: AssessmentInputs, reason: RefusalReason) {
        let expected = candidate("one");
        let mut model = AdmissionModel::new();
        model.prepare(expected.clone()).unwrap();
        model.require_admission().unwrap();
        let assessment = model.begin_assessment().unwrap();
        let reasons = model.finish_assessment(assessment, &inputs).unwrap_err();
        assert!(reasons.contains(&reason), "{reasons:?}");
        assert!(matches!(
            model.state,
            AdmissionState::AdmissionRefused { .. }
        ));
    }

    #[test]
    fn synthetic_complete_evidence_exercises_one_shot_handoff_only() {
        let expected = candidate("one");
        let current_generation = generation("one");
        let mut model = AdmissionModel::new();
        let record = authorize(&mut model, &expected);
        assert_eq!(record.binding.issuance_context, TEST_ISSUANCE_CONTEXT);
        let capability = model
            .begin_first_launch(&record, &expected, &current_generation)
            .unwrap();
        model
            .consume_first_launch(Some(&capability), &expected)
            .unwrap();
        assert!(matches!(
            model.state,
            AdmissionState::FirstLaunchConsumed(_)
        ));
        assert_eq!(
            model.consume_first_launch(Some(&capability), &expected),
            Err(RefusalReason::ReplayedAuthorization)
        );
    }

    #[test]
    fn missing_or_diagnostic_provenance_and_capabilities_refuse() {
        let expected = candidate("one");
        let mut inputs = positive(&expected);
        inputs.candidate_b_provenance = Evidence::Missing;
        inputs.retained_a_capabilities = Evidence::Diagnostic(Digest::synthetic("diagnostic-only"));
        let mut model = AdmissionModel::new();
        model.prepare(expected).unwrap();
        model.require_admission().unwrap();
        let assessment = model.begin_assessment().unwrap();
        let reasons = model.finish_assessment(assessment, &inputs).unwrap_err();
        assert!(reasons.contains(&RefusalReason::MissingProvenance));
        assert!(reasons.contains(&RefusalReason::MissingCapabilities));
    }

    #[test]
    fn wrong_transaction_pending_a_b_or_retained_inventory_identity_refuses() {
        let expected = candidate("one");
        let mut inputs = positive(&expected);
        inputs.transaction = Evidence::Trusted("txn-other".into());
        inputs.pending_record = Evidence::Trusted(Digest::synthetic("other-pending"));
        inputs.retained_a = Evidence::Trusted(identity("other-a"));
        inputs.candidate_b = Evidence::Trusted(identity("other-b"));
        inputs.retained_inventory = Evidence::Trusted(Digest::synthetic("other-inventory"));
        let mut model = AdmissionModel::new();
        model.prepare(expected).unwrap();
        model.require_admission().unwrap();
        let assessment = model.begin_assessment().unwrap();
        let reasons = model.finish_assessment(assessment, &inputs).unwrap_err();
        for reason in [
            RefusalReason::WrongTransaction,
            RefusalReason::WrongPendingRecord,
            RefusalReason::WrongRetainedIdentity,
            RefusalReason::WrongCandidateIdentity,
            RefusalReason::WrongRetainedInventory,
        ] {
            assert!(reasons.contains(&reason), "{reasons:?}");
        }
    }

    #[test]
    fn changed_inventory_and_missing_exclusions_refuse() {
        let expected = candidate("one");
        let mut inputs = positive(&expected);
        inputs.stable_inventory_after = Evidence::Trusted(Digest::synthetic("changed"));
        inputs.writer_exclusion = Evidence::Diagnostic(ExclusionProof {
            identity: Digest::synthetic("partial"),
            complete: false,
        });
        inputs.usb_exclusion = Evidence::Missing;
        let mut model = AdmissionModel::new();
        model.prepare(expected).unwrap();
        model.require_admission().unwrap();
        let assessment = model.begin_assessment().unwrap();
        let reasons = model.finish_assessment(assessment, &inputs).unwrap_err();
        assert!(reasons.contains(&RefusalReason::ChangedInventory));
        assert!(reasons.contains(&RefusalReason::MissingWriterExclusion));
        assert!(reasons.contains(&RefusalReason::UnsupportedUsbExclusion));
    }

    #[test]
    fn incomplete_or_incompatible_future_writes_refuse() {
        let expected = candidate("one");
        let mut missing = positive(&expected);
        missing.future_write_compatibility = Evidence::Missing;
        assert_refused(missing, RefusalReason::FutureWriteCompatibilityIncomplete);

        let mut incompatible = positive(&expected);
        incompatible.future_write_compatibility = Evidence::Trusted(FutureWriteProof {
            identity: Digest::synthetic("incompatible"),
            retained_a_reads_every_b_state: false,
        });
        assert_refused(
            incompatible,
            RefusalReason::RetainedANotCompatibleWithBWrites,
        );
    }

    #[test]
    fn contention_interruption_and_conflicting_pending_refuse() {
        let expected = candidate("one");
        let mut inputs = positive(&expected);
        inputs.lock_contended = true;
        inputs.assessment_interrupted = true;
        inputs.conflicting_pending_activation = true;
        let mut model = AdmissionModel::new();
        model.prepare(expected).unwrap();
        model.require_admission().unwrap();
        let assessment = model.begin_assessment().unwrap();
        let reasons = model.finish_assessment(assessment, &inputs).unwrap_err();
        assert!(reasons.contains(&RefusalReason::LockContention));
        assert!(reasons.contains(&RefusalReason::AssessmentInterrupted));
        assert!(reasons.contains(&RefusalReason::ConflictingPendingActivation));
    }

    #[test]
    fn tampered_stale_or_replayed_authorization_refuses() {
        let expected = candidate("one");
        let mut model = AdmissionModel::new();
        let record = authorize(&mut model, &expected);

        let mut tampered = record.clone();
        tampered.binding.candidate.pending_record = Digest::synthetic("tampered");
        assert_eq!(
            model.begin_first_launch(&tampered, &expected, &generation("one")),
            Err(RefusalReason::TamperedAuthorization)
        );
        assert_eq!(
            model.begin_first_launch(&record, &expected, &generation("stale")),
            Err(RefusalReason::StaleLifecycleGeneration)
        );

        model.crash_and_restart();
        assert_eq!(
            model.begin_first_launch(&record, &expected, &generation("one")),
            Err(RefusalReason::ReplayedAuthorization)
        );
    }

    #[test]
    fn candidate_replacement_after_assessment_invalidates_authorization() {
        let expected = candidate("one");
        let replacement = candidate("two");
        let mut model = AdmissionModel::new();
        let record = authorize(&mut model, &expected);
        assert_eq!(
            model.begin_first_launch(&record, &replacement, &generation("one")),
            Err(RefusalReason::CandidateChangedAfterAssessment)
        );
        assert!(matches!(
            model.state,
            AdmissionState::AdmissionRequired(ref current) if current == &replacement
        ));
    }

    #[test]
    fn direct_start_and_wrong_handoff_are_refused() {
        let expected = candidate("one");
        let mut model = AdmissionModel::new();
        let record = authorize(&mut model, &expected);
        let capability = model
            .begin_first_launch(&record, &expected, &generation("one"))
            .unwrap();
        assert_eq!(
            model.consume_first_launch(None, &expected),
            Err(RefusalReason::MissingHandoffCapability)
        );
        let mut forged = capability;
        forged.nonce = Digest::synthetic("forged");
        assert_eq!(
            model.consume_first_launch(Some(&forged), &expected),
            Err(RefusalReason::ReplayedAuthorization)
        );
    }

    #[test]
    fn duplicate_assessment_and_launch_are_refused() {
        let expected = candidate("one");
        let mut model = AdmissionModel::new();
        model.prepare(expected.clone()).unwrap();
        model.require_admission().unwrap();
        model.begin_assessment().unwrap();
        assert_eq!(
            model.begin_assessment(),
            Err(RefusalReason::DuplicateAdmission)
        );

        let mut launched = AdmissionModel::new();
        let record = authorize(&mut launched, &expected);
        launched
            .begin_first_launch(&record, &expected, &generation("one"))
            .unwrap();
        assert_eq!(
            launched.begin_first_launch(&record, &expected, &generation("one")),
            Err(RefusalReason::DuplicateAdmission)
        );
    }

    #[test]
    fn crash_recovery_never_upgrades_uncertain_state_to_authorized() {
        let expected = candidate("one");
        for checkpoint in ["assessment", "authorized", "handoff"] {
            let mut model = AdmissionModel::new();
            model.prepare(expected.clone()).unwrap();
            model.require_admission().unwrap();
            let assessment = model.begin_assessment().unwrap();
            if checkpoint != "assessment" {
                let record = model
                    .finish_assessment(assessment, &positive(&expected))
                    .unwrap();
                if checkpoint == "handoff" {
                    model
                        .begin_first_launch(&record, &expected, &generation("one"))
                        .unwrap();
                }
            }
            model.crash_and_restart();
            assert!(matches!(
                model.state,
                AdmissionState::AdmissionRequired(ref current) if current == &expected
            ));
        }
    }

    #[test]
    fn restart_preserves_only_non_authorizing_stable_states() {
        let expected = candidate("one");

        let mut empty = AdmissionModel::new();
        empty.crash_and_restart();
        assert_eq!(empty.state, AdmissionState::NoPendingCandidate);

        let mut prepared = AdmissionModel::new();
        prepared.prepare(expected.clone()).unwrap();
        prepared.crash_and_restart();
        assert!(matches!(
            prepared.state,
            AdmissionState::CandidatePrepared(ref current) if current == &expected
        ));

        let mut required = AdmissionModel::new();
        required.prepare(expected.clone()).unwrap();
        required.require_admission().unwrap();
        required.crash_and_restart();
        assert!(matches!(
            required.state,
            AdmissionState::AdmissionRequired(ref current) if current == &expected
        ));
    }

    #[test]
    fn refusal_and_consumed_state_survive_restart_without_becoming_new_authority() {
        let expected = candidate("one");
        let mut refused = AdmissionModel::new();
        refused.prepare(expected.clone()).unwrap();
        refused.require_admission().unwrap();
        let assessment = refused.begin_assessment().unwrap();
        let mut inputs = positive(&expected);
        inputs.compatibility = Evidence::Missing;
        refused.finish_assessment(assessment, &inputs).unwrap_err();
        let before = refused.state.clone();
        refused.crash_and_restart();
        assert_eq!(refused.state, before);

        let mut consumed = AdmissionModel::new();
        let record = authorize(&mut consumed, &expected);
        let capability = consumed
            .begin_first_launch(&record, &expected, &generation("one"))
            .unwrap();
        consumed
            .consume_first_launch(Some(&capability), &expected)
            .unwrap();
        let before = consumed.state.clone();
        consumed.crash_and_restart();
        assert_eq!(consumed.state, before);
        assert_eq!(
            consumed.begin_first_launch(&record, &expected, &generation("one")),
            Err(RefusalReason::DuplicateAdmission)
        );
    }
}
