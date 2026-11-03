//! Host-side A→B→C lifecycle coverage for the bounded no-migration updater.
//!
//! These tests exercise intent persistence, storage-contract gating, seal binding of
//! authorized bytes, and qualification journal recovery. They do not claim capability
//! coverage=complete and do not touch notebook data.

#![cfg(test)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::install_intent::{self, InstallIntent};
use crate::installation_authorization::{AuthorizationDecision, PrerequisiteStatus};
use crate::publication_trust_boundary::VerificationProvenance;
use crate::seal_authorized_install::{seal_from_authorized_decision, SealRequest};
use crate::storage_contract::{self, StorageContract, STORAGE_CONTRACT_ID};
use crate::update_qualification::{self, QualifyJournal, QualifyPhase};

static NEXT: AtomicU64 = AtomicU64::new(1);

fn temp_root(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ignite-updater-lifecycle-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn write_scriptlet(path: &Path, body: &[u8]) -> String {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
    encode_hex(&Sha256::digest(body))
}

fn encode_hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn authorized_decision(path: &Path, sha: &str) -> AuthorizationDecision {
    AuthorizationDecision {
        expected_tag: "v0.9.15".into(),
        staging_root: path
            .parent()
            .and_then(|assets| assets.parent())
            .unwrap()
            .to_path_buf(),
        provenance: Some(VerificationProvenance::OfficialProductionPin),
        candidate_authenticated: PrerequisiteStatus::Satisfied,
        publication_claims_validated: PrerequisiteStatus::Satisfied,
        artifact_identity_bound: PrerequisiteStatus::Satisfied,
        device_compatible: PrerequisiteStatus::Satisfied,
        upgrade_compatible: PrerequisiteStatus::Satisfied,
        recovery_ready: PrerequisiteStatus::Satisfied,
        data_compatibility_verified: PrerequisiteStatus::Satisfied,
        storage_contract_compatible: PrerequisiteStatus::Satisfied,
        execution_binding: PrerequisiteStatus::Satisfied,
        installation_policy_satisfied: PrerequisiteStatus::Satisfied,
        installation_authorized: true,
        refusal_reasons: Vec::new(),
        report_lines: Vec::new(),
        bound_scriptlet_path: Some(path.to_path_buf()),
        bound_scriptlet_sha256: Some(sha.to_string()),
    }
}

#[test]
fn intent_then_seal_binds_opened_bytes_not_intent_hash() {
    let root = temp_root("seal-bind");
    let staging = root.join("staging");
    let assets = staging.join("assets");
    fs::create_dir_all(&assets).unwrap();
    storage_contract::write_contract_file_atomic(&assets, &StorageContract::current()).unwrap();
    let scriptlet = assets.join("Install Ignite v0.9.15.sh");
    let original =
        b"#!/bin/sh\nIGNITE_STORAGE_CONTRACT='ignite-storage-v1'\necho authenticated-original\n";
    let sha = write_scriptlet(&scriptlet, original);

    let update = root.join("update");
    fs::create_dir_all(&update).unwrap();
    let intent = InstallIntent {
        expected_tag: "v0.9.15".into(),
        version: "0.9.15".into(),
        staging_root: staging.clone(),
        storage_contract: STORAGE_CONTRACT_ID.into(),
        intent_txn: "txn-seal-1".into(),
    };
    install_intent::write_intent_atomic(&update, &intent).unwrap();

    // Intent must not carry a scriptlet hash trust anchor.
    let intent_text = fs::read_to_string(install_intent::intent_path(&update)).unwrap();
    assert!(intent_text.contains("trust_anchor=false\n"));
    assert!(!intent_text.contains("scriptlet_sha256="));

    let sealed = root.join("run").join("sealed.sh");
    let receipt = root.join("run").join("receipt.v1");
    let req = SealRequest {
        expected_tag: "v0.9.15".into(),
        version: "0.9.15".into(),
        staging_root: staging,
        sealed_scriptlet_path: sealed.clone(),
        seal_receipt_path: receipt.clone(),
        installed_version: "0.9.14".into(),
        device: crate::installation_authorization::DeviceFacts {
            architecture: "armv7l".into(),
            firmware: Some("5.18.6".into()),
            app_root: root.join("app"),
            data_root: root.join("data"),
            recovery_root: root.join("recovery"),
            documents_root: root.join("documents"),
        },
    };
    let decision = authorized_decision(&scriptlet, &sha);
    let result = seal_from_authorized_decision(&req, &decision).unwrap();
    assert_eq!(result.scriptlet_sha256, sha);
    assert_eq!(fs::read(&sealed).unwrap(), original);
    let receipt_text = fs::read_to_string(&receipt).unwrap();
    assert!(receipt_text.contains("authorization_revalidated=true\n"));
    assert!(receipt_text.contains(&format!("storage_contract={STORAGE_CONTRACT_ID}\n")));

    // Path replacement after seal must not affect already sealed bytes.
    fs::write(&scriptlet, b"#!/bin/sh\necho replaced-after-seal\n").unwrap();
    assert_eq!(fs::read(&sealed).unwrap(), original);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn storage_contract_mismatch_refuses_same_gate() {
    let root = temp_root("contract-mismatch");
    let assets = root.join("assets");
    fs::create_dir_all(&assets).unwrap();
    storage_contract::write_contract_file_atomic(
        &assets,
        &StorageContract {
            storage_contract: "other-contract-v2".into(),
        },
    )
    .unwrap();
    let err = storage_contract::require_same_storage_contract(&assets).unwrap_err();
    assert!(matches!(
        err,
        storage_contract::ContractError::Mismatch { .. }
    ));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn two_update_qualify_journal_allows_second_install_preflight() {
    let root = temp_root("ab-bc");
    let update = root.join("update");
    let prev = root.join("previous-release");
    fs::create_dir_all(&update).unwrap();
    fs::create_dir_all(&prev).unwrap();

    // Simulate A→B install publishing pending-health + LKG metadata.
    let pending = format!(
        "IGNITE_PENDING_HEALTH_V1\n\
         schema=1\n\
         status=pending-health\n\
         candidate_version=0.9.15\n\
         candidate_txn=txn-b\n\
         candidate_app_bytes=1\n\
         candidate_app_sha256={}\n\
         candidate_lifecycle_bytes=1\n\
         candidate_lifecycle_sha256={}\n\
         retained_release_identity=previous-release\n\
         lkg_inventory_bytes=1\n\
         lkg_inventory_sha256={}\n\
         data_compatibility=unknown\n",
        "11".repeat(32),
        "22".repeat(32),
        "33".repeat(32)
    );
    fs::write(update.join("pending-health.v1"), pending).unwrap();
    fs::write(prev.join("ignite-lkg-inventory.v1"), b"inventory\n").unwrap();
    fs::write(prev.join("appreg-handler.sql"), b"handler\n").unwrap();
    fs::write(root.join("library.meta"), b"{\"v\":1,\"notebooks\":[]}\n").unwrap();

    // Qualify B — retires pending, LKG metadata, and the retained A tree.
    update_qualification::commit_qualification(&update, &prev, STORAGE_CONTRACT_ID).unwrap();
    assert!(!update.join("pending-health.v1").exists());
    assert!(!prev.exists());
    // Notebook data untouched.
    assert_eq!(
        fs::read_to_string(root.join("library.meta")).unwrap(),
        "{\"v\":1,\"notebooks\":[]}\n"
    );

    // Second install preflight: no pending, no ambiguous LKG / retained A.
    assert!(!update.join("pending-health.v1").exists());
    assert!(!prev.exists());

    // Simulate B→C retaining B, then crash after pending removal; reconcile must unblock.
    fs::create_dir_all(&prev).unwrap();
    fs::write(update.join("pending-health.v1"), b"IGNITE_PENDING_HEALTH_V1\nschema=1\nstatus=pending-health\ncandidate_version=0.9.16\ncandidate_txn=txn-c\ncandidate_app_bytes=1\ncandidate_app_sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\ncandidate_lifecycle_bytes=1\ncandidate_lifecycle_sha256=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\nretained_release_identity=previous-release\nlkg_inventory_bytes=1\nlkg_inventory_sha256=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\ndata_compatibility=unknown\n").unwrap();
    fs::write(prev.join("ignite-lkg-inventory.v1"), b"inventory2\n").unwrap();
    fs::write(prev.join("appreg-handler.sql"), b"handler2\n").unwrap();
    let (txn, sha) = update_qualification::pending_digest(&update).unwrap();
    update_qualification::write_journal_for_test(
        &update,
        &QualifyJournal {
            pending_txn: txn,
            pending_sha256: sha,
            storage_contract: STORAGE_CONTRACT_ID.into(),
            phase: QualifyPhase::Prepared,
        },
    )
    .unwrap();
    fs::remove_file(update.join("pending-health.v1")).unwrap();
    // Exact crash window: journal still `prepared` while pending is already gone.
    update_qualification::reconcile_interrupted_qualification(&update, &prev).unwrap();
    assert!(!prev.exists());
    assert!(!update.join("ignite-qualify.journal").exists());
    assert_eq!(
        fs::read_to_string(root.join("library.meta")).unwrap(),
        "{\"v\":1,\"notebooks\":[]}\n"
    );

    // Partial LKG cleanup safety: only inventory remains under pending_removed.
    fs::create_dir_all(&prev).unwrap();
    fs::write(prev.join("ignite-lkg-inventory.v1"), b"leftover\n").unwrap();
    update_qualification::write_journal_for_test(
        &update,
        &QualifyJournal {
            pending_txn: "txn-partial".into(),
            pending_sha256: "aa".repeat(32),
            storage_contract: STORAGE_CONTRACT_ID.into(),
            phase: QualifyPhase::PendingRemoved,
        },
    )
    .unwrap();
    update_qualification::reconcile_interrupted_qualification(&update, &prev).unwrap();
    assert!(!prev.exists());
    assert!(!update.join("ignite-qualify.journal").exists());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn matching_contract_file_without_scriptlet_claim_refuses_release_binding() {
    let root = temp_root("contract-unbound");
    let assets = root.join("assets");
    fs::create_dir_all(&assets).unwrap();
    storage_contract::write_contract_file_atomic(&assets, &StorageContract::current()).unwrap();
    // Matching file alone must not authorize the scoped contract gate.
    let err = storage_contract::require_release_bound_storage_contract(
        &assets,
        b"#!/bin/sh\necho attacker-substituted-file-only\n",
    )
    .unwrap_err();
    assert!(matches!(
        err,
        storage_contract::ContractError::UnauthenticatedClaim(_)
    ));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn forged_compatible_without_seal_receipt_is_insufficient() {
    let root = temp_root("forged");
    let update = root.join("update");
    fs::create_dir_all(&update).unwrap();
    // A forged intent alone must not be treated as authorization.
    let intent = InstallIntent {
        expected_tag: "v0.9.15".into(),
        version: "0.9.15".into(),
        staging_root: root.join("staging"),
        storage_contract: STORAGE_CONTRACT_ID.into(),
        intent_txn: "forged".into(),
    };
    install_intent::write_intent_atomic(&update, &intent).unwrap();
    let text = fs::read_to_string(install_intent::intent_path(&update)).unwrap();
    assert!(text.contains("trust_anchor=false\n"));
    assert!(!text.contains("authorization_revalidated=true"));
    let _ = fs::remove_dir_all(root);
}
