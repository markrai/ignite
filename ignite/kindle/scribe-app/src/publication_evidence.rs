//! Canonical production publication evidence (Slice 3D-B2a.4.3h).
//!
//! Schema `IGNITE_PUBLICATION_EVIDENCE_PRODUCTION_V1`. Component contracts are
//! loaded from the shared host/device contract file so Python and Rust cannot
//! silently drift.

use crate::publication_trust_boundary::ProductionTrustError;
use crate::release_manifest::{decode_hex, encode_hex, valid_release_version};

pub const EVIDENCE_HEADER: &str = "IGNITE_PUBLICATION_EVIDENCE_PRODUCTION_V1";
pub const EVIDENCE_SCHEMA: &str = "1";
pub const PRODUCTION_SCOPE: &str = "production-pin-required";
pub const TEST_SCOPE_MARKER: &str = "publisher_authentication_scope=test-trust-root-only";
pub const MAX_EVIDENCE_BYTES: usize = 64 * 1024;
pub const MAX_COMPONENT_BYTES: u64 = 512 * 1024 * 1024;

const CONTRACT_BYTES: &str =
    include_str!("../../../../deployment/kindle/publication_component_contract.v1");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackagePresencePolicy {
    Required,
    Optional,
    AbsentOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentContract {
    pub role: &'static str,
    pub classification: &'static str,
    pub present_mode: &'static str,
    pub mapping: &'static str,
    pub package_relative: Option<&'static str>,
    pub presence_policy: PackagePresencePolicy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedComponentClaim {
    pub role: String,
    pub presence: String,
    pub size: u64,
    pub sha256: String,
    pub classification: String,
    pub mode: String,
    pub mapping: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedProductionEvidence {
    pub version: String,
    pub tag: String,
    pub scriptlet_artifact: String,
    pub scriptlet_bytes: u64,
    pub scriptlet_sha256: String,
    pub payload_dirname: String,
    pub package_profile_sha256: String,
    pub components: Vec<ValidatedComponentClaim>,
}

fn contracts() -> &'static [ComponentContract] {
    use std::sync::OnceLock;
    static CONTRACTS: OnceLock<Vec<ComponentContract>> = OnceLock::new();
    CONTRACTS
        .get_or_init(|| {
            parse_contract_table(CONTRACT_BYTES).expect("embedded component contract must parse")
        })
        .as_slice()
}

fn parse_contract_table(raw: &str) -> Result<Vec<ComponentContract>, &'static str> {
    let lines: Vec<&str> = raw.trim_end_matches('\n').split('\n').collect();
    if lines.len() < 4 {
        return Err("contract too short");
    }
    if lines[0] != "IGNITE_PUBLICATION_COMPONENT_CONTRACT_V1" || lines[1] != "schema=1" {
        return Err("unsupported contract schema");
    }
    let count = lines[2]
        .strip_prefix("role_count=")
        .ok_or("missing role_count")?
        .parse::<usize>()
        .map_err(|_| "bad role_count")?;
    if lines.len() != 3 + count {
        return Err("contract role count mismatch");
    }
    // Leak strings once into 'static for the OnceLock table.
    let mut out = Vec::with_capacity(count);
    for line in &lines[3..] {
        let body = line.strip_prefix("role=").ok_or("role line missing")?;
        let parts: Vec<&str> = body.split('|').collect();
        if parts.len() != 6 {
            return Err("role line field count");
        }
        let role: &'static str = Box::leak(parts[0].to_string().into_boxed_str());
        let classification: &'static str = Box::leak(parts[1].to_string().into_boxed_str());
        let present_mode: &'static str = Box::leak(parts[2].to_string().into_boxed_str());
        let mapping: &'static str = Box::leak(parts[3].to_string().into_boxed_str());
        let rel = parts[4];
        let package_relative = if rel == "-" {
            None
        } else {
            Some(Box::leak(rel.to_string().into_boxed_str()) as &'static str)
        };
        let presence_policy = match parts[5] {
            "required" => PackagePresencePolicy::Required,
            "optional" => PackagePresencePolicy::Optional,
            "absent-only" => PackagePresencePolicy::AbsentOnly,
            _ => return Err("unknown presence policy"),
        };
        out.push(ComponentContract {
            role,
            classification,
            present_mode,
            mapping,
            package_relative,
            presence_policy,
        });
    }
    Ok(out)
}

pub fn is_production_evidence_header(raw: &[u8]) -> bool {
    raw.starts_with(format!("{EVIDENCE_HEADER}\n").as_bytes())
}

pub fn parse_and_validate_production_evidence(
    raw: &[u8],
    expected_version: Option<&str>,
    expected_scriptlet_sha256: Option<&str>,
    expected_scriptlet_bytes: Option<u64>,
) -> Result<ValidatedProductionEvidence, ProductionTrustError> {
    if raw.is_empty() {
        return Err(ProductionTrustError::EmptyEvidence);
    }
    if raw.len() > MAX_EVIDENCE_BYTES || raw.contains(&b'\r') {
        return Err(ProductionTrustError::MalformedEvidence(
            "publication evidence refused",
        ));
    }
    if raw
        .windows(TEST_SCOPE_MARKER.len())
        .any(|window| window == TEST_SCOPE_MARKER.as_bytes())
    {
        return Err(ProductionTrustError::TestOnlyEvidenceRefused);
    }
    let text = std::str::from_utf8(raw).map_err(|_| {
        ProductionTrustError::MalformedEvidence("evidence contains non-UTF8 bytes")
    })?;
    if !text.ends_with('\n') {
        return Err(ProductionTrustError::MalformedEvidence(
            "evidence must end with LF",
        ));
    }
    let lines: Vec<&str> = text[..text.len() - 1].split('\n').collect();
    let contracts = contracts();
    let expected_line_count = 10 + contracts.len() + 1;
    if lines.len() != expected_line_count {
        return Err(ProductionTrustError::MalformedEvidence(
            "evidence line count mismatch",
        ));
    }
    if lines[0] != EVIDENCE_HEADER || lines[1] != format!("schema={EVIDENCE_SCHEMA}") {
        return Err(ProductionTrustError::UnknownEvidenceSchema);
    }

    let version = require_field(lines[2], "version=")?;
    let tag = require_field(lines[3], "tag=")?;
    let scriptlet_artifact = require_field(lines[4], "scriptlet_artifact=")?;
    let scriptlet_bytes_token = require_field(lines[5], "scriptlet_bytes=")?;
    let scriptlet_sha256 = require_field(lines[6], "scriptlet_sha256=")?;
    let payload_dirname = require_field(lines[7], "payload_dirname=")?;
    let package_profile_sha256 = require_field(lines[8], "package_profile_sha256=")?;
    let role_count = require_field(lines[9], "component_role_count=")?;

    if !valid_release_version(version) {
        return Err(ProductionTrustError::MalformedEvidence(
            "noncanonical version",
        ));
    }
    if tag != format!("v{version}") {
        return Err(ProductionTrustError::MalformedEvidence(
            "tag/version mismatch",
        ));
    }
    if scriptlet_artifact != format!("Install Ignite v{version}.sh") {
        return Err(ProductionTrustError::MalformedEvidence(
            "scriptlet artifact mismatch",
        ));
    }
    let scriptlet_bytes = parse_u64_canonical(scriptlet_bytes_token).ok_or(
        ProductionTrustError::MalformedEvidence("noncanonical scriptlet_bytes"),
    )?;
    if scriptlet_bytes == 0 {
        return Err(ProductionTrustError::MalformedEvidence(
            "scriptlet_bytes out of bounds",
        ));
    }
    if decode_hex::<32>(scriptlet_sha256.as_bytes()).is_none() {
        return Err(ProductionTrustError::MalformedEvidence(
            "noncanonical scriptlet_sha256",
        ));
    }
    if payload_dirname != format!("Ignite-Kindle-{version}") {
        return Err(ProductionTrustError::MalformedEvidence(
            "payload dirname mismatch",
        ));
    }
    if decode_hex::<32>(package_profile_sha256.as_bytes()).is_none() {
        return Err(ProductionTrustError::MalformedEvidence(
            "noncanonical package_profile_sha256",
        ));
    }
    if role_count != contracts.len().to_string() {
        return Err(ProductionTrustError::MalformedEvidence(
            "component_role_count mismatch",
        ));
    }
    if let Some(expected) = expected_version {
        if version != expected {
            return Err(ProductionTrustError::EvidenceReleaseMismatch(
                "evidence version disagrees with release-v1",
            ));
        }
    }
    if let Some(expected) = expected_scriptlet_sha256 {
        if scriptlet_sha256 != expected {
            return Err(ProductionTrustError::EvidenceReleaseMismatch(
                "evidence scriptlet_sha256 disagrees with release-v1",
            ));
        }
    }
    if let Some(expected) = expected_scriptlet_bytes {
        if scriptlet_bytes != expected {
            return Err(ProductionTrustError::EvidenceReleaseMismatch(
                "evidence scriptlet_bytes disagrees with release-v1",
            ));
        }
    }

    let mut components = Vec::with_capacity(contracts.len());
    for (idx, contract) in contracts.iter().enumerate() {
        let claim = parse_component_line(lines[10 + idx], contract)?;
        components.push(claim);
    }
    if lines[lines.len() - 1] != format!("publisher_authentication_scope={PRODUCTION_SCOPE}") {
        return Err(ProductionTrustError::MalformedEvidence(
            "production publisher scope marker missing",
        ));
    }

    // Canonical re-encode and demand exact match.
    let rebuilt = rebuild_canonical(
        version,
        scriptlet_bytes,
        scriptlet_sha256,
        package_profile_sha256,
        &components,
    );
    if rebuilt.as_bytes() != raw {
        return Err(ProductionTrustError::MalformedEvidence(
            "evidence is not exact canonical encoding",
        ));
    }

    Ok(ValidatedProductionEvidence {
        version: version.to_string(),
        tag: tag.to_string(),
        scriptlet_artifact: scriptlet_artifact.to_string(),
        scriptlet_bytes,
        scriptlet_sha256: scriptlet_sha256.to_string(),
        payload_dirname: payload_dirname.to_string(),
        package_profile_sha256: package_profile_sha256.to_string(),
        components,
    })
}

fn rebuild_canonical(
    version: &str,
    scriptlet_bytes: u64,
    scriptlet_sha256: &str,
    package_profile_sha256: &str,
    components: &[ValidatedComponentClaim],
) -> String {
    let mut out = format!(
        "{EVIDENCE_HEADER}\nschema={EVIDENCE_SCHEMA}\nversion={version}\ntag=v{version}\nscriptlet_artifact=Install Ignite v{version}.sh\nscriptlet_bytes={scriptlet_bytes}\nscriptlet_sha256={scriptlet_sha256}\npayload_dirname=Ignite-Kindle-{version}\npackage_profile_sha256={package_profile_sha256}\ncomponent_role_count={}\n",
        components.len()
    );
    for claim in components {
        if claim.presence == "file" {
            out.push_str(&format!(
                "component={}|file|{}|{}|{}|{}|{}\n",
                claim.role,
                claim.size,
                claim.sha256,
                claim.classification,
                claim.mode,
                claim.mapping
            ));
        } else {
            out.push_str(&format!(
                "component={}|absent|0|-|{}|-|{}\n",
                claim.role, claim.classification, claim.mapping
            ));
        }
    }
    out.push_str(&format!(
        "publisher_authentication_scope={PRODUCTION_SCOPE}\n"
    ));
    out
}

fn parse_component_line(
    line: &str,
    contract: &ComponentContract,
) -> Result<ValidatedComponentClaim, ProductionTrustError> {
    let body = line.strip_prefix("component=").ok_or(
        ProductionTrustError::MalformedEvidence("component line missing"),
    )?;
    let parts: Vec<&str> = body.split('|').collect();
    if parts.len() != 7 {
        return Err(ProductionTrustError::MalformedEvidence(
            "component line field count",
        ));
    }
    let role = parts[0];
    if role != contract.role {
        return Err(ProductionTrustError::MalformedEvidence(
            "component role order mismatch",
        ));
    }
    let presence = parts[1];
    let size = parse_u64_canonical(parts[2]).ok_or(ProductionTrustError::MalformedEvidence(
        "component size noncanonical",
    ))?;
    let digest = parts[3];
    let klass = parts[4];
    let mode = parts[5];
    let mapping = parts[6];
    if klass != contract.classification {
        return Err(ProductionTrustError::MalformedEvidence(
            "classification mismatch",
        ));
    }
    if mapping != contract.mapping {
        return Err(ProductionTrustError::MalformedEvidence("mapping mismatch"));
    }
    let expected_mode = if presence == "file" {
        contract.present_mode
    } else {
        "-"
    };
    if mode != expected_mode {
        return Err(ProductionTrustError::MalformedEvidence("mode mismatch"));
    }
    match contract.presence_policy {
        PackagePresencePolicy::Required if presence != "file" => {
            return Err(ProductionTrustError::MalformedEvidence(
                "mandatory direct-copy role must be present",
            ));
        }
        PackagePresencePolicy::AbsentOnly if presence != "absent" => {
            return Err(ProductionTrustError::MalformedEvidence(
                "generated/unpackaged role must be absent from package evidence",
            ));
        }
        _ => {}
    }
    match presence {
        "absent" => {
            if size != 0 || digest != "-" || mode != "-" {
                return Err(ProductionTrustError::MalformedEvidence(
                    "absent component invalid",
                ));
            }
        }
        "file" => {
            if digest == "-" || mode == "-" || decode_hex::<32>(digest.as_bytes()).is_none() {
                return Err(ProductionTrustError::MalformedEvidence(
                    "file component invalid",
                ));
            }
            if size == 0 || size > MAX_COMPONENT_BYTES {
                return Err(ProductionTrustError::MalformedEvidence(
                    "component size bound",
                ));
            }
            if contract.package_relative.is_none() {
                return Err(ProductionTrustError::MalformedEvidence(
                    "role cannot be a packaged file",
                ));
            }
        }
        _ => {
            return Err(ProductionTrustError::MalformedEvidence(
                "unknown component presence",
            ))
        }
    }
    Ok(ValidatedComponentClaim {
        role: role.to_string(),
        presence: presence.to_string(),
        size,
        sha256: digest.to_string(),
        classification: klass.to_string(),
        mode: mode.to_string(),
        mapping: mapping.to_string(),
    })
}

fn require_field<'a>(line: &'a str, prefix: &str) -> Result<&'a str, ProductionTrustError> {
    line.strip_prefix(prefix).ok_or(ProductionTrustError::MalformedEvidence(
        "evidence field missing",
    ))
}

fn parse_u64_canonical(token: &str) -> Option<u64> {
    if token.is_empty() || (token.len() > 1 && token.starts_with('0')) {
        return None;
    }
    if !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    token.parse().ok()
}

/// Exported for parity tests: role count and hex of contract body.
pub fn embedded_contract_digest_hex() -> String {
    use sha2::{Digest, Sha256};
    encode_hex(&Sha256::digest(CONTRACT_BYTES.as_bytes()))
}

pub fn embedded_contract_role_count() -> usize {
    contracts().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_contract_has_twenty_two_roles() {
        assert_eq!(embedded_contract_role_count(), 22);
        assert!(CONTRACT_BYTES.starts_with("IGNITE_PUBLICATION_COMPONENT_CONTRACT_V1\n"));
        assert!(CONTRACT_BYTES.contains("|required\n") || CONTRACT_BYTES.contains("|required\r"));
        assert!(contracts()[0].presence_policy == PackagePresencePolicy::Required);
    }

    #[test]
    fn mandatory_direct_copy_absence_fails_claim_validation() {
        // Minimal structurally ordered document with scribe-app absent.
        let mut lines = vec![
            EVIDENCE_HEADER.to_string(),
            format!("schema={EVIDENCE_SCHEMA}"),
            "version=3.3.1".to_string(),
            "tag=v3.3.1".to_string(),
            "scriptlet_artifact=Install Ignite v3.3.1.sh".to_string(),
            "scriptlet_bytes=12".to_string(),
            format!("scriptlet_sha256={}", "ab".repeat(32)),
            "payload_dirname=Ignite-Kindle-3.3.1".to_string(),
            format!("package_profile_sha256={}", "cd".repeat(32)),
            format!("component_role_count={}", contracts().len()),
        ];
        for contract in contracts() {
            if contract.role == "scribe-app" {
                lines.push(format!(
                    "component={}|absent|0|-|{}|-|{}",
                    contract.role, contract.classification, contract.mapping
                ));
            } else if contract.presence_policy == PackagePresencePolicy::AbsentOnly {
                lines.push(format!(
                    "component={}|absent|0|-|{}|-|{}",
                    contract.role, contract.classification, contract.mapping
                ));
            } else if contract.presence_policy == PackagePresencePolicy::Optional {
                lines.push(format!(
                    "component={}|absent|0|-|{}|-|{}",
                    contract.role, contract.classification, contract.mapping
                ));
            } else {
                let mode = contract.present_mode;
                lines.push(format!(
                    "component={}|file|4|{}|{}|{}|{}",
                    contract.role,
                    "11".repeat(32),
                    contract.classification,
                    mode,
                    contract.mapping
                ));
            }
        }
        lines.push(format!("publisher_authentication_scope={PRODUCTION_SCOPE}"));
        let raw = format!("{}\n", lines.join("\n"));
        let err = parse_and_validate_production_evidence(raw.as_bytes(), None, None, None)
            .expect_err("mandatory absence must fail");
        assert!(matches!(
            err,
            ProductionTrustError::MalformedEvidence(msg)
                if msg.contains("mandatory")
        ));
    }
}
