//! Production trust boundary for Kindle publication evidence (Slice 3D-B2a.4.3g).
//!
//! Single publisher authority: the same Ed25519 pin as [`crate::release_manifest`].
//! Release-v1 remains the six-line installer authenticator. Publication evidence is
//! bound through an adjacent signed binding record — not by extending release-v1.
//!
//! Verification-only. Success never authorizes install, launch, promotion, or rollback.
//! While the pin is `UNPROVISIONED`, every production entry point fails closed.
//!
//! Digest authentication of opaque evidence bytes is not publication-evidence claim
//! validation. A production-scoped evidence schema belongs to a later slice.

use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::release_manifest::{
    authenticate_with_key, decode_hex, encode_hex, official_release_public_key,
    valid_release_version, AuthenticatedReleaseManifest, ReleaseVerifyError,
};

pub const BINDING_HEADER: &str = "IGNITE_PUBLICATION_BINDING_V1";
pub const BINDING_SCHEMA: &str = "1";
pub const BINDING_SIGNATURE_SCHEMA: &str = "ignite-kindle-publication-binding-signature-v1";
pub const BINDING_SCOPE: &str = "production-pin-required";
pub const MAX_BINDING_BYTES: usize = 2048;
pub const MAX_EVIDENCE_BYTES: usize = 64 * 1024;

/// 3f test-only evidence trailer — must never acquire production authority.
pub const TEST_EVIDENCE_SCOPE: &str = "publisher_authentication_scope=test-trust-root-only";

/// Disposable fixture header used only to exercise digest binding. Not production evidence.
pub const PLACEHOLDER_EVIDENCE_HEADER: &str = "IGNITE_PUBLICATION_EVIDENCE_PROD_PLACEHOLDER_V1";

#[derive(Debug, PartialEq, Eq)]
pub enum ProductionTrustError {
    OfficialKeyNotProvisioned,
    InvalidPinnedPublicKey,
    MissingSignature,
    MalformedSignature,
    InvalidSignature,
    ManifestTooLarge,
    UnsupportedSchema,
    MalformedBinding(&'static str),
    ReleaseAuthentication(ReleaseVerifyError),
    ReleaseManifestDigestMismatch,
    ScriptletDigestMismatch,
    EvidenceDigestMismatch,
    TestOnlyEvidenceRefused,
    EmptyEvidence,
    PlaceholderEvidenceRefused,
    UnknownEvidenceSchema,
    TagMismatch,
    EvidenceTooLarge,
}

impl std::fmt::Display for ProductionTrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::OfficialKeyNotProvisioned => "official release signing key is not provisioned",
            Self::InvalidPinnedPublicKey => "pinned release public key is invalid",
            Self::MissingSignature => "publication binding signature is missing",
            Self::MalformedSignature => "publication binding signature is malformed",
            Self::InvalidSignature => "publication binding signature is invalid",
            Self::ManifestTooLarge => "publication binding is too large",
            Self::UnsupportedSchema => "publication binding schema is unsupported",
            Self::MalformedBinding(reason) => reason,
            Self::ReleaseAuthentication(inner) => return write!(f, "{inner}"),
            Self::ReleaseManifestDigestMismatch => {
                "binding release_manifest_sha256 does not match release-v1 bytes"
            }
            Self::ScriptletDigestMismatch => {
                "binding scriptlet_sha256 does not match authenticated release-v1 sha256"
            }
            Self::EvidenceDigestMismatch => {
                "binding publication_evidence_sha256 does not match evidence bytes"
            }
            Self::TestOnlyEvidenceRefused => {
                "test-trust-root-only publication evidence cannot authenticate under production pin"
            }
            Self::EmptyEvidence => "publication evidence must not be empty",
            Self::PlaceholderEvidenceRefused => {
                "placeholder publication evidence is not production evidence"
            }
            Self::UnknownEvidenceSchema => {
                "publication evidence schema is unknown; production claims remain unvalidated"
            }
            Self::TagMismatch => "publication binding tag does not match release-v1 tag",
            Self::EvidenceTooLarge => "publication evidence is too large",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ProductionTrustError {}

impl From<ReleaseVerifyError> for ProductionTrustError {
    fn from(value: ReleaseVerifyError) -> Self {
        match value {
            ReleaseVerifyError::OfficialKeyNotProvisioned => Self::OfficialKeyNotProvisioned,
            ReleaseVerifyError::InvalidPinnedPublicKey => Self::InvalidPinnedPublicKey,
            other => Self::ReleaseAuthentication(other),
        }
    }
}

/// Which key material authenticated the chain. Derived only from the entry point used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerificationProvenance {
    OfficialProductionPin,
    TestOnlyFixtureKey,
}

/// Publication-evidence claim status after a successful binding digest match.
///
/// Digest authentication is not schema or component-claim validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceClaimStatus {
    DigestAuthenticatedClaimsUnverified,
}

/// Authenticated release-v1 + binding digests. Not an authorization grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedPublicationChain {
    release: AuthenticatedReleaseManifest,
    release_manifest_sha256: [u8; 32],
    publication_evidence_sha256: [u8; 32],
    provenance: VerificationProvenance,
    evidence_claims: EvidenceClaimStatus,
}

impl AuthenticatedPublicationChain {
    pub fn release(&self) -> &AuthenticatedReleaseManifest {
        &self.release
    }

    pub fn release_manifest_sha256_hex(&self) -> String {
        encode_hex(&self.release_manifest_sha256)
    }

    pub fn publication_evidence_sha256_hex(&self) -> String {
        encode_hex(&self.publication_evidence_sha256)
    }

    pub fn provenance(&self) -> VerificationProvenance {
        self.provenance
    }

    pub fn evidence_claims(&self) -> EvidenceClaimStatus {
        self.evidence_claims
    }
}

/// Production entry point: repository pin only. No key override.
///
/// Refuses empty, placeholder, unknown-schema, and 3f test-scope evidence. No production
/// evidence schema exists yet, so this entry cannot yield validated publication claims.
pub fn authenticate_official_publication_chain(
    release_manifest: &[u8],
    release_signature: &[u8],
    binding_manifest: &[u8],
    binding_signature: &[u8],
    publication_evidence: &[u8],
    expected_tag: &str,
) -> Result<AuthenticatedPublicationChain, ProductionTrustError> {
    let public_key = official_release_public_key().map_err(ProductionTrustError::from)?;
    refuse_for_production_publication_evidence(publication_evidence)?;
    authenticate_publication_chain_inner(
        release_manifest,
        release_signature,
        binding_manifest,
        binding_signature,
        publication_evidence,
        expected_tag,
        &public_key,
        VerificationProvenance::OfficialProductionPin,
    )
}

/// Isolated verification seam for disposable fixtures. Never the production entry point.
///
/// May digest-bind opaque fixture bytes (including the placeholder) under test-only provenance.
/// Never reports production-pin authentication. Component claims remain unverified.
pub(crate) fn authenticate_publication_chain_with_key(
    release_manifest: &[u8],
    release_signature: &[u8],
    binding_manifest: &[u8],
    binding_signature: &[u8],
    publication_evidence: &[u8],
    expected_tag: &str,
    public_key: &[u8; 32],
) -> Result<AuthenticatedPublicationChain, ProductionTrustError> {
    if publication_evidence.is_empty() {
        return Err(ProductionTrustError::EmptyEvidence);
    }
    if contains_test_only_scope(publication_evidence) {
        return Err(ProductionTrustError::TestOnlyEvidenceRefused);
    }
    authenticate_publication_chain_inner(
        release_manifest,
        release_signature,
        binding_manifest,
        binding_signature,
        publication_evidence,
        expected_tag,
        public_key,
        VerificationProvenance::TestOnlyFixtureKey,
    )
}

/// Eligibility gate for any entry that claims production publication-evidence validation.
///
/// No production evidence schema is defined in this slice; every current byte pattern refuses.
pub fn refuse_for_production_publication_evidence(
    evidence: &[u8],
) -> Result<(), ProductionTrustError> {
    if evidence.is_empty() {
        return Err(ProductionTrustError::EmptyEvidence);
    }
    if evidence.len() > MAX_EVIDENCE_BYTES {
        return Err(ProductionTrustError::EvidenceTooLarge);
    }
    if contains_test_only_scope(evidence) {
        return Err(ProductionTrustError::TestOnlyEvidenceRefused);
    }
    if evidence.starts_with(PLACEHOLDER_EVIDENCE_HEADER.as_bytes()) {
        return Err(ProductionTrustError::PlaceholderEvidenceRefused);
    }
    // A future slice must define the production header/schema before this can succeed.
    Err(ProductionTrustError::UnknownEvidenceSchema)
}

fn authenticate_publication_chain_inner(
    release_manifest: &[u8],
    release_signature: &[u8],
    binding_manifest: &[u8],
    binding_signature: &[u8],
    publication_evidence: &[u8],
    expected_tag: &str,
    public_key: &[u8; 32],
    provenance: VerificationProvenance,
) -> Result<AuthenticatedPublicationChain, ProductionTrustError> {
    if publication_evidence.len() > MAX_EVIDENCE_BYTES {
        return Err(ProductionTrustError::EvidenceTooLarge);
    }

    let release = authenticate_with_key(
        release_manifest,
        release_signature,
        expected_tag,
        public_key,
    )
    .map_err(ProductionTrustError::from)?;

    let binding = authenticate_binding_with_key(binding_manifest, binding_signature, public_key)?;
    if binding.release_tag != release.tag() {
        return Err(ProductionTrustError::TagMismatch);
    }
    if binding.release_version != release.version() {
        return Err(ProductionTrustError::MalformedBinding(
            "binding release_version does not match authenticated release-v1 version",
        ));
    }

    let release_digest: [u8; 32] = Sha256::digest(release_manifest).into();
    if release_digest != binding.release_manifest_sha256 {
        return Err(ProductionTrustError::ReleaseManifestDigestMismatch);
    }

    let scriptlet_digest = decode_hex::<32>(release.installer_sha256_hex().as_bytes()).ok_or(
        ProductionTrustError::MalformedBinding("authenticated release sha256 is malformed"),
    )?;
    if scriptlet_digest != binding.scriptlet_sha256 {
        return Err(ProductionTrustError::ScriptletDigestMismatch);
    }

    let evidence_digest: [u8; 32] = Sha256::digest(publication_evidence).into();
    if evidence_digest != binding.publication_evidence_sha256 {
        return Err(ProductionTrustError::EvidenceDigestMismatch);
    }

    Ok(AuthenticatedPublicationChain {
        release,
        release_manifest_sha256: release_digest,
        publication_evidence_sha256: evidence_digest,
        provenance,
        evidence_claims: EvidenceClaimStatus::DigestAuthenticatedClaimsUnverified,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct AuthenticatedBinding {
    release_tag: String,
    release_version: String,
    release_manifest_sha256: [u8; 32],
    scriptlet_sha256: [u8; 32],
    publication_evidence_sha256: [u8; 32],
}

fn authenticate_binding_with_key(
    binding_bytes: &[u8],
    signature_bytes: &[u8],
    public_key: &[u8; 32],
) -> Result<AuthenticatedBinding, ProductionTrustError> {
    if binding_bytes.len() > MAX_BINDING_BYTES {
        return Err(ProductionTrustError::ManifestTooLarge);
    }
    let signature = parse_binding_signature(signature_bytes)?;
    let verifying_key = VerifyingKey::from_bytes(public_key)
        .map_err(|_| ProductionTrustError::InvalidPinnedPublicKey)?;
    verifying_key
        .verify_strict(binding_bytes, &signature)
        .map_err(|_| ProductionTrustError::InvalidSignature)?;
    parse_binding(binding_bytes)
}

fn parse_binding_signature(bytes: &[u8]) -> Result<Signature, ProductionTrustError> {
    if bytes.is_empty() {
        return Err(ProductionTrustError::MissingSignature);
    }
    let prefix = format!("schema={BINDING_SIGNATURE_SCHEMA}\nsignature=");
    let expected_len = prefix.len() + 128 + 1;
    if bytes.len() != expected_len
        || !bytes.starts_with(prefix.as_bytes())
        || bytes.last() != Some(&b'\n')
    {
        return Err(ProductionTrustError::MalformedSignature);
    }
    let raw = decode_hex::<64>(&bytes[prefix.len()..bytes.len() - 1])
        .ok_or(ProductionTrustError::MalformedSignature)?;
    Ok(Signature::from_bytes(&raw))
}

fn parse_binding(bytes: &[u8]) -> Result<AuthenticatedBinding, ProductionTrustError> {
    if bytes.last() != Some(&b'\n') || bytes.contains(&b'\r') {
        return Err(ProductionTrustError::MalformedBinding(
            "binding must be canonical ASCII/LF",
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| {
        ProductionTrustError::MalformedBinding("binding contains non-UTF8 bytes")
    })?;
    let lines: Vec<&str> = text[..text.len() - 1].split('\n').collect();
    if lines.len() != 8 {
        return Err(ProductionTrustError::MalformedBinding(
            "binding must contain exactly eight lines",
        ));
    }
    if lines[0] != BINDING_HEADER {
        return Err(ProductionTrustError::UnsupportedSchema);
    }
    if lines[1] != format!("schema={BINDING_SCHEMA}") {
        return Err(ProductionTrustError::UnsupportedSchema);
    }
    let release_tag = require_prefix(lines[2], "release_tag=")?;
    let release_version = require_prefix(lines[3], "release_version=")?;
    let release_manifest_sha256 = require_sha(lines[4], "release_manifest_sha256=")?;
    let scriptlet_sha256 = require_sha(lines[5], "scriptlet_sha256=")?;
    let publication_evidence_sha256 = require_sha(lines[6], "publication_evidence_sha256=")?;
    if lines[7] != format!("publisher_authentication_scope={BINDING_SCOPE}") {
        return Err(ProductionTrustError::MalformedBinding(
            "binding must declare production-pin-required scope",
        ));
    }
    if !release_tag.starts_with('v') || &release_tag[1..] != release_version {
        return Err(ProductionTrustError::MalformedBinding(
            "binding tag/version inconsistent",
        ));
    }
    if !valid_release_version(release_version) {
        return Err(ProductionTrustError::MalformedBinding(
            "binding release_version is noncanonical",
        ));
    }
    let canonical = format!(
        "{BINDING_HEADER}\nschema={BINDING_SCHEMA}\nrelease_tag={release_tag}\nrelease_version={release_version}\nrelease_manifest_sha256={}\nscriptlet_sha256={}\npublication_evidence_sha256={}\npublisher_authentication_scope={BINDING_SCOPE}\n",
        encode_hex(&release_manifest_sha256),
        encode_hex(&scriptlet_sha256),
        encode_hex(&publication_evidence_sha256),
    );
    if canonical.as_bytes() != bytes {
        return Err(ProductionTrustError::MalformedBinding(
            "binding is not exact canonical encoding",
        ));
    }
    Ok(AuthenticatedBinding {
        release_tag: release_tag.to_string(),
        release_version: release_version.to_string(),
        release_manifest_sha256,
        scriptlet_sha256,
        publication_evidence_sha256,
    })
}

fn require_prefix<'a>(line: &'a str, prefix: &str) -> Result<&'a str, ProductionTrustError> {
    line.strip_prefix(prefix).ok_or(ProductionTrustError::MalformedBinding(
        "binding field missing",
    ))
}

fn require_sha(line: &str, prefix: &str) -> Result<[u8; 32], ProductionTrustError> {
    let value = require_prefix(line, prefix)?;
    decode_hex::<32>(value.as_bytes()).ok_or(ProductionTrustError::MalformedBinding(
        "binding digest is malformed",
    ))
}

fn contains_test_only_scope(evidence: &[u8]) -> bool {
    evidence
        .windows(TEST_EVIDENCE_SCOPE.len())
        .any(|window| window == TEST_EVIDENCE_SCOPE.as_bytes())
}

fn publisher_authentication_line(provenance: VerificationProvenance) -> &'static str {
    match provenance {
        VerificationProvenance::OfficialProductionPin => {
            "publisher_authentication=verified-under-production-pin"
        }
        VerificationProvenance::TestOnlyFixtureKey => {
            "publisher_authentication=verified-under-test-only-fixture-key"
        }
    }
}

/// Observation-only report lines. Provenance comes only from the verification result.
pub fn production_trust_report_lines(
    outcome: Result<&AuthenticatedPublicationChain, &ProductionTrustError>,
) -> Vec<String> {
    let mut lines = vec![
        "IGNITE_PRODUCTION_TRUST_BOUNDARY_V1".to_string(),
        "schema=1".to_string(),
        "installation_binding=unverified".to_string(),
        "execution_binding=unverified".to_string(),
        "first_launch_admission=unavailable".to_string(),
        "data_compatibility=unknown".to_string(),
        "promotion_authorized=false".to_string(),
    ];
    match outcome {
        Ok(auth) => {
            lines.push(publisher_authentication_line(auth.provenance()).to_string());
            lines.push("publication_evidence_digest=authenticated".to_string());
            lines.push("publication_evidence_claims=unverified".to_string());
            lines.push(format!("release_tag={}", auth.release().tag()));
            lines.push(format!(
                "release_manifest_sha256={}",
                auth.release_manifest_sha256_hex()
            ));
            lines.push(format!(
                "publication_evidence_sha256={}",
                auth.publication_evidence_sha256_hex()
            ));
            lines.push("authorization=none".to_string());
        }
        Err(ProductionTrustError::OfficialKeyNotProvisioned) => {
            lines.push("publisher_authentication=unavailable".to_string());
            lines.push("publication_evidence_claims=unverified".to_string());
            lines.push("refusal=official-key-not-provisioned".to_string());
        }
        Err(err) => {
            lines.push("publisher_authentication=unavailable".to_string());
            lines.push("publication_evidence_claims=unverified".to_string());
            lines.push(format!("refusal={err}"));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::release_manifest::{MANIFEST_SCHEMA, SIGNATURE_SCHEMA};

    const VERSION: &str = "3.3.1";
    const TAG: &str = "v3.3.1";
    const INSTALLER: &[u8] = b"exact installer bytes for trust boundary\n";

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7_u8; 32])
    }

    fn release_manifest() -> Vec<u8> {
        let digest = Sha256::digest(INSTALLER);
        format!(
            "schema={MANIFEST_SCHEMA}\nversion={VERSION}\ntag={TAG}\nartifact=Install Ignite v{VERSION}.sh\nbytes={}\nsha256={}\n",
            INSTALLER.len(),
            encode_hex(&digest),
        )
        .into_bytes()
    }

    fn sign_envelope(message: &[u8], schema: &str, key: &SigningKey) -> Vec<u8> {
        let signature = key.sign(message);
        format!(
            "schema={schema}\nsignature={}\n",
            encode_hex(signature.to_bytes().as_ref())
        )
        .into_bytes()
    }

    fn fixture_evidence() -> Vec<u8> {
        format!("{PLACEHOLDER_EVIDENCE_HEADER}\nschema=1\npublisher_authentication_scope=production-pin-required\n")
            .into_bytes()
    }

    fn binding_for(release_manifest: &[u8], evidence: &[u8]) -> Vec<u8> {
        let scriptlet = Sha256::digest(INSTALLER);
        format!(
            "{BINDING_HEADER}\nschema={BINDING_SCHEMA}\nrelease_tag={TAG}\nrelease_version={VERSION}\nrelease_manifest_sha256={}\nscriptlet_sha256={}\npublication_evidence_sha256={}\npublisher_authentication_scope={BINDING_SCOPE}\n",
            encode_hex(&Sha256::digest(release_manifest)),
            encode_hex(&scriptlet),
            encode_hex(&Sha256::digest(evidence)),
        )
        .into_bytes()
    }

    #[test]
    fn official_entry_refuses_unprovisioned_pin() {
        let key = signing_key();
        let release = release_manifest();
        let evidence = fixture_evidence();
        let binding = binding_for(&release, &evidence);
        let err = authenticate_official_publication_chain(
            &release,
            &sign_envelope(&release, SIGNATURE_SCHEMA, &key),
            &binding,
            &sign_envelope(&binding, BINDING_SIGNATURE_SCHEMA, &key),
            &evidence,
            TAG,
        )
        .expect_err("pin must remain unprovisioned");
        assert_eq!(err, ProductionTrustError::OfficialKeyNotProvisioned);
        let report = production_trust_report_lines(Err(&err));
        assert!(report
            .iter()
            .any(|line| line == "publisher_authentication=unavailable"));
        assert!(report
            .iter()
            .any(|line| line == "publication_evidence_claims=unverified"));
        assert!(report
            .iter()
            .any(|line| line == "promotion_authorized=false"));
        assert!(!report
            .iter()
            .any(|line| line.contains("verified-under-production-pin")));
    }

    #[test]
    fn disposable_key_reports_test_only_provenance_not_production_pin() {
        let key = signing_key();
        let public = key.verifying_key().to_bytes();
        let release = release_manifest();
        let evidence = fixture_evidence();
        let binding = binding_for(&release, &evidence);
        let auth = authenticate_publication_chain_with_key(
            &release,
            &sign_envelope(&release, SIGNATURE_SCHEMA, &key),
            &binding,
            &sign_envelope(&binding, BINDING_SIGNATURE_SCHEMA, &key),
            &evidence,
            TAG,
            &public,
        )
        .expect("valid digest-bound chain under fixture key");
        assert_eq!(auth.provenance(), VerificationProvenance::TestOnlyFixtureKey);
        assert_eq!(
            auth.evidence_claims(),
            EvidenceClaimStatus::DigestAuthenticatedClaimsUnverified
        );
        let report = production_trust_report_lines(Ok(&auth));
        assert!(report.iter().any(|line| {
            line == "publisher_authentication=verified-under-test-only-fixture-key"
        }));
        assert!(!report
            .iter()
            .any(|line| line == "publisher_authentication=verified-under-production-pin"));
        assert!(report
            .iter()
            .any(|line| line == "publication_evidence_digest=authenticated"));
        assert!(report
            .iter()
            .any(|line| line == "publication_evidence_claims=unverified"));
        assert!(report.iter().any(|line| line == "authorization=none"));
        assert!(report
            .iter()
            .any(|line| line == "promotion_authorized=false"));
    }

    #[test]
    fn production_evidence_gate_refuses_empty_placeholder_and_unknown() {
        assert_eq!(
            refuse_for_production_publication_evidence(b"").unwrap_err(),
            ProductionTrustError::EmptyEvidence
        );
        assert_eq!(
            refuse_for_production_publication_evidence(&fixture_evidence()).unwrap_err(),
            ProductionTrustError::PlaceholderEvidenceRefused
        );
        assert_eq!(
            refuse_for_production_publication_evidence(
                b"IGNITE_PUBLICATION_EVIDENCE_UNKNOWN_V9\nschema=9\n"
            )
            .unwrap_err(),
            ProductionTrustError::UnknownEvidenceSchema
        );
        let mut test_scope = fixture_evidence();
        test_scope.extend_from_slice(TEST_EVIDENCE_SCOPE.as_bytes());
        test_scope.push(b'\n');
        assert_eq!(
            refuse_for_production_publication_evidence(&test_scope).unwrap_err(),
            ProductionTrustError::TestOnlyEvidenceRefused
        );
    }

    #[test]
    fn empty_evidence_refused_by_fixture_seam() {
        let key = signing_key();
        let public = key.verifying_key().to_bytes();
        let release = release_manifest();
        let evidence = b"";
        let binding = binding_for(&release, evidence);
        assert_eq!(
            authenticate_publication_chain_with_key(
                &release,
                &sign_envelope(&release, SIGNATURE_SCHEMA, &key),
                &binding,
                &sign_envelope(&binding, BINDING_SIGNATURE_SCHEMA, &key),
                evidence,
                TAG,
                &public,
            )
            .unwrap_err(),
            ProductionTrustError::EmptyEvidence
        );
    }

    #[test]
    fn test_scope_evidence_refused_and_digest_mismatch_detected() {
        let key = signing_key();
        let public = key.verifying_key().to_bytes();
        let release = release_manifest();
        let evidence = fixture_evidence();
        let binding = binding_for(&release, &evidence);
        let release_sig = sign_envelope(&release, SIGNATURE_SCHEMA, &key);
        let binding_sig = sign_envelope(&binding, BINDING_SIGNATURE_SCHEMA, &key);

        let mut test_evidence = evidence.clone();
        test_evidence.extend_from_slice(TEST_EVIDENCE_SCOPE.as_bytes());
        test_evidence.push(b'\n');
        let binding_test = binding_for(&release, &test_evidence);
        assert_eq!(
            authenticate_publication_chain_with_key(
                &release,
                &release_sig,
                &binding_test,
                &sign_envelope(&binding_test, BINDING_SIGNATURE_SCHEMA, &key),
                &test_evidence,
                TAG,
                &public,
            )
            .unwrap_err(),
            ProductionTrustError::TestOnlyEvidenceRefused
        );

        let mut bad_evidence = evidence.clone();
        bad_evidence.push(b'x');
        assert_eq!(
            authenticate_publication_chain_with_key(
                &release,
                &release_sig,
                &binding,
                &binding_sig,
                &bad_evidence,
                TAG,
                &public,
            )
            .unwrap_err(),
            ProductionTrustError::EvidenceDigestMismatch
        );
    }

    #[test]
    fn modified_binding_wrong_key_and_missing_signature_fail() {
        let key = signing_key();
        let other = SigningKey::from_bytes(&[9_u8; 32]);
        let public = key.verifying_key().to_bytes();
        let release = release_manifest();
        let evidence = fixture_evidence();
        let binding = binding_for(&release, &evidence);
        let release_sig = sign_envelope(&release, SIGNATURE_SCHEMA, &key);
        let binding_sig = sign_envelope(&binding, BINDING_SIGNATURE_SCHEMA, &key);

        let mut tampered = binding.clone();
        tampered[20] ^= 0x01;
        assert!(authenticate_publication_chain_with_key(
            &release,
            &release_sig,
            &tampered,
            &binding_sig,
            &evidence,
            TAG,
            &public,
        )
        .is_err());

        assert!(authenticate_publication_chain_with_key(
            &release,
            &sign_envelope(&release, SIGNATURE_SCHEMA, &other),
            &binding,
            &sign_envelope(&binding, BINDING_SIGNATURE_SCHEMA, &other),
            &evidence,
            TAG,
            &public,
        )
        .is_err());

        assert_eq!(
            authenticate_publication_chain_with_key(
                &release,
                &release_sig,
                &binding,
                b"",
                &evidence,
                TAG,
                &public,
            )
            .unwrap_err(),
            ProductionTrustError::MissingSignature
        );
    }
}
