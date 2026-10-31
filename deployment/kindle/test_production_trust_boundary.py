"""Production trust boundary regressions (Slice 3D-B2a.4.3g)."""

from __future__ import annotations

import hashlib
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parents[1]
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

from authenticated_publication_evidence import (  # noqa: E402
    generate_test_trust_root,
    sign_publication_evidence,
)
from production_trust_boundary import (  # noqa: E402
    BINDING_SIGNATURE_SCHEMA,
    PLACEHOLDER_EVIDENCE_HEADER,
    TEST_EVIDENCE_SCOPE,
    AuthenticatedPublicationChain,
    EvidenceClaimStatus,
    ProductionTrustError,
    VerificationProvenance,
    assert_production_pin_unprovisioned,
    authenticate_official_publication_chain,
    authenticate_publication_chain_with_key,
    build_publication_binding,
    production_trust_report,
    refuse_for_production_publication_evidence,
    sign_binding_for_test,
    sign_release_for_test,
)

VERSION = "3.3.1"
TAG = f"v{VERSION}"
INSTALLER = b"exact installer bytes for production trust boundary\n"


def _release_manifest(installer: bytes = INSTALLER) -> bytes:
    digest = hashlib.sha256(installer).hexdigest()
    return (
        f"schema=ignite-kindle-release-v1\n"
        f"version={VERSION}\n"
        f"tag={TAG}\n"
        f"artifact=Install Ignite v{VERSION}.sh\n"
        f"bytes={len(installer)}\n"
        f"sha256={digest}\n"
    ).encode("ascii")


def _fixture_evidence() -> bytes:
    """Disposable placeholder used only to exercise digest binding (test-only)."""
    return (
        f"{PLACEHOLDER_EVIDENCE_HEADER}\n"
        f"schema=1\n"
        f"publisher_authentication_scope=production-pin-required\n"
    ).encode("ascii")


def _test_scope_evidence() -> bytes:
    return (
        b"IGNITE_PUBLICATION_EVIDENCE_V1\n"
        b"schema=1\n"
        + TEST_EVIDENCE_SCOPE.encode("ascii")
        + b"\n"
    )


@pytest.fixture
def disposable_root(tmp_path: Path):
    assert_production_pin_unprovisioned()
    return generate_test_trust_root(tmp_path / "keys", label="slice-3g-publisher")


def _signed_chain(root, *, evidence: bytes | None = None, installer: bytes = INSTALLER):
    evidence = _fixture_evidence() if evidence is None else evidence
    release = _release_manifest(installer)
    scriptlet_sha = hashlib.sha256(installer).hexdigest()
    binding = build_publication_binding(
        release_tag=TAG,
        release_version=VERSION,
        release_manifest=release,
        scriptlet_sha256=scriptlet_sha,
        publication_evidence=evidence,
    )
    release_sig = sign_release_for_test(
        release,
        private_key_path=root.private_key_path,
        public_key=root.public_key,
    )
    binding_sig = sign_binding_for_test(
        binding,
        private_key_path=root.private_key_path,
        public_key=root.public_key,
    )
    return release, release_sig, binding, binding_sig, evidence


def test_production_pin_remains_unprovisioned() -> None:
    assert_production_pin_unprovisioned()


def test_official_entry_refuses_unprovisioned_pin(disposable_root) -> None:
    release, release_sig, binding, binding_sig, evidence = _signed_chain(disposable_root)
    with pytest.raises(ProductionTrustError, match="not provisioned"):
        authenticate_official_publication_chain(
            release_manifest=release,
            release_signature=release_sig,
            binding_manifest=binding,
            binding_signature=binding_sig,
            publication_evidence=evidence,
            expected_tag=TAG,
        )
    report = production_trust_report(
        ProductionTrustError("official release public key is not provisioned")
    )
    assert "publisher_authentication=unavailable" in report
    assert "publication_evidence_claims=unverified" in report
    assert "promotion_authorized=false" in report
    assert "installation_binding=unverified" in report
    assert "first_launch_admission=unavailable" in report
    assert "verified-under-production-pin" not in report


def test_disposable_key_reports_test_only_provenance(disposable_root) -> None:
    release, release_sig, binding, binding_sig, evidence = _signed_chain(disposable_root)
    auth = authenticate_publication_chain_with_key(
        release_manifest=release,
        release_signature=release_sig,
        binding_manifest=binding,
        binding_signature=binding_sig,
        publication_evidence=evidence,
        expected_tag=TAG,
        public_key=disposable_root.public_key,
    )
    assert isinstance(auth, AuthenticatedPublicationChain)
    assert auth.provenance is VerificationProvenance.TEST_ONLY_FIXTURE_KEY
    assert (
        auth.evidence_claims
        is EvidenceClaimStatus.DIGEST_AUTHENTICATED_CLAIMS_UNVERIFIED
    )
    report = production_trust_report(auth)
    assert "publisher_authentication=verified-under-test-only-fixture-key" in report
    assert "publisher_authentication=verified-under-production-pin" not in report
    assert "publication_evidence_digest=authenticated" in report
    assert "publication_evidence_claims=unverified" in report
    assert "authorization=none" in report
    assert "promotion_authorized=false" in report


def test_test_key_cannot_be_rendered_as_production_pin(disposable_root) -> None:
    release, release_sig, binding, binding_sig, evidence = _signed_chain(disposable_root)
    auth = authenticate_publication_chain_with_key(
        release_manifest=release,
        release_signature=release_sig,
        binding_manifest=binding,
        binding_signature=binding_sig,
        publication_evidence=evidence,
        expected_tag=TAG,
        public_key=disposable_root.public_key,
    )
    # Report must derive provenance from the result, not a caller label.
    report = production_trust_report(auth)
    assert auth.provenance is not VerificationProvenance.OFFICIAL_PRODUCTION_PIN
    assert "verified-under-production-pin" not in report


def test_empty_placeholder_unknown_cannot_yield_validated_publication_claim() -> None:
    with pytest.raises(ProductionTrustError, match="empty"):
        refuse_for_production_publication_evidence(b"")
    with pytest.raises(ProductionTrustError, match="placeholder"):
        refuse_for_production_publication_evidence(_fixture_evidence())
    with pytest.raises(ProductionTrustError, match="unknown"):
        refuse_for_production_publication_evidence(
            b"IGNITE_PUBLICATION_EVIDENCE_UNKNOWN_V9\nschema=9\n"
        )
    with pytest.raises(ProductionTrustError, match="test-trust-root-only"):
        refuse_for_production_publication_evidence(_test_scope_evidence())


def test_empty_evidence_refused_by_fixture_seam(disposable_root) -> None:
    release = _release_manifest()
    binding = build_publication_binding(
        release_tag=TAG,
        release_version=VERSION,
        release_manifest=release,
        scriptlet_sha256=hashlib.sha256(INSTALLER).hexdigest(),
        publication_evidence=b"",
    )
    release_sig = sign_release_for_test(
        release,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
    )
    binding_sig = sign_binding_for_test(
        binding,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
    )
    with pytest.raises(ProductionTrustError, match="empty"):
        authenticate_publication_chain_with_key(
            release_manifest=release,
            release_signature=release_sig,
            binding_manifest=binding,
            binding_signature=binding_sig,
            publication_evidence=b"",
            expected_tag=TAG,
            public_key=disposable_root.public_key,
        )


def test_test_scope_evidence_refused_by_production_boundary(disposable_root) -> None:
    evidence = _test_scope_evidence()
    release, release_sig, binding, binding_sig, evidence = _signed_chain(
        disposable_root, evidence=evidence
    )
    with pytest.raises(ProductionTrustError, match="test-trust-root-only"):
        authenticate_publication_chain_with_key(
            release_manifest=release,
            release_signature=release_sig,
            binding_manifest=binding,
            binding_signature=binding_sig,
            publication_evidence=evidence,
            expected_tag=TAG,
            public_key=disposable_root.public_key,
        )


def test_3f_signed_evidence_not_accepted_as_production_authority(
    disposable_root,
) -> None:
    evidence = _test_scope_evidence()
    envelope = sign_publication_evidence(evidence, disposable_root)
    assert b"ignite-kindle-publication-evidence-signature-v1" in envelope

    release, release_sig, binding, binding_sig, evidence = _signed_chain(
        disposable_root, evidence=evidence
    )
    with pytest.raises(ProductionTrustError, match="test-trust-root-only"):
        authenticate_publication_chain_with_key(
            release_manifest=release,
            release_signature=release_sig,
            binding_manifest=binding,
            binding_signature=binding_sig,
            publication_evidence=evidence,
            expected_tag=TAG,
            public_key=disposable_root.public_key,
        )


def test_binding_authenticates_exact_evidence_bytes_only(disposable_root) -> None:
    release, release_sig, binding, binding_sig, evidence = _signed_chain(disposable_root)
    with pytest.raises(ProductionTrustError, match="publication_evidence_sha256"):
        authenticate_publication_chain_with_key(
            release_manifest=release,
            release_signature=release_sig,
            binding_manifest=binding,
            binding_signature=binding_sig,
            publication_evidence=evidence + b"x",
            expected_tag=TAG,
            public_key=disposable_root.public_key,
        )


def test_missing_and_malformed_binding_signature(disposable_root) -> None:
    release, release_sig, binding, _binding_sig, evidence = _signed_chain(disposable_root)
    with pytest.raises(ProductionTrustError, match="missing"):
        authenticate_publication_chain_with_key(
            release_manifest=release,
            release_signature=release_sig,
            binding_manifest=binding,
            binding_signature=b"",
            publication_evidence=evidence,
            expected_tag=TAG,
            public_key=disposable_root.public_key,
        )
    malformed = b"schema=wrong\nsignature=" + (b"ab" * 64) + b"\n"
    with pytest.raises(ProductionTrustError, match="malformed"):
        authenticate_publication_chain_with_key(
            release_manifest=release,
            release_signature=release_sig,
            binding_manifest=binding,
            binding_signature=malformed,
            publication_evidence=evidence,
            expected_tag=TAG,
            public_key=disposable_root.public_key,
        )


def test_modified_binding_signature_refused(disposable_root) -> None:
    release, release_sig, binding, binding_sig, evidence = _signed_chain(disposable_root)
    tampered = bytearray(binding_sig)
    idx = binding_sig.index(b"signature=") + len(b"signature=")
    tampered[idx] = ord("0") if tampered[idx] != ord("0") else ord("1")
    with pytest.raises(ProductionTrustError):
        authenticate_publication_chain_with_key(
            release_manifest=release,
            release_signature=release_sig,
            binding_manifest=binding,
            binding_signature=bytes(tampered),
            publication_evidence=evidence,
            expected_tag=TAG,
            public_key=disposable_root.public_key,
        )


def test_modified_release_manifest_refused(disposable_root) -> None:
    release, release_sig, binding, binding_sig, evidence = _signed_chain(disposable_root)
    tampered = release.replace(b"bytes=", b"bytez=", 1)
    with pytest.raises(ProductionTrustError):
        authenticate_publication_chain_with_key(
            release_manifest=tampered,
            release_signature=release_sig,
            binding_manifest=binding,
            binding_signature=binding_sig,
            publication_evidence=evidence,
            expected_tag=TAG,
            public_key=disposable_root.public_key,
        )


def test_scriptlet_identity_mismatch_refused(disposable_root) -> None:
    release, release_sig, binding, binding_sig, evidence = _signed_chain(disposable_root)
    wrong_scriptlet = hashlib.sha256(b"different payload\n").hexdigest()
    bad_binding = build_publication_binding(
        release_tag=TAG,
        release_version=VERSION,
        release_manifest=release,
        scriptlet_sha256=wrong_scriptlet,
        publication_evidence=evidence,
    )
    bad_sig = sign_binding_for_test(
        bad_binding,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
    )
    with pytest.raises(ProductionTrustError, match="scriptlet_sha256"):
        authenticate_publication_chain_with_key(
            release_manifest=release,
            release_signature=release_sig,
            binding_manifest=bad_binding,
            binding_signature=bad_sig,
            publication_evidence=evidence,
            expected_tag=TAG,
            public_key=disposable_root.public_key,
        )


def test_ab_tag_mismatch_refused(disposable_root) -> None:
    release, release_sig, binding, binding_sig, evidence = _signed_chain(disposable_root)
    with pytest.raises(ProductionTrustError, match="tag"):
        authenticate_publication_chain_with_key(
            release_manifest=release,
            release_signature=release_sig,
            binding_manifest=binding,
            binding_signature=binding_sig,
            publication_evidence=evidence,
            expected_tag="v9.9.9",
            public_key=disposable_root.public_key,
        )


def test_wrong_publisher_key_refused(disposable_root, tmp_path: Path) -> None:
    other = generate_test_trust_root(tmp_path / "other", label="other-publisher")
    release, release_sig, binding, binding_sig, evidence = _signed_chain(disposable_root)
    with pytest.raises(ProductionTrustError):
        authenticate_publication_chain_with_key(
            release_manifest=release,
            release_signature=release_sig,
            binding_manifest=binding,
            binding_signature=binding_sig,
            publication_evidence=evidence,
            expected_tag=TAG,
            public_key=other.public_key,
        )


def test_success_does_not_authorize_installation_or_promotion(disposable_root) -> None:
    release, release_sig, binding, binding_sig, evidence = _signed_chain(disposable_root)
    auth = authenticate_publication_chain_with_key(
        release_manifest=release,
        release_signature=release_sig,
        binding_manifest=binding,
        binding_signature=binding_sig,
        publication_evidence=evidence,
        expected_tag=TAG,
        public_key=disposable_root.public_key,
    )
    report = production_trust_report(auth)
    for required in (
        "installation_binding=unverified",
        "execution_binding=unverified",
        "first_launch_admission=unavailable",
        "data_compatibility=unknown",
        "promotion_authorized=false",
        "authorization=none",
        "publication_evidence_claims=unverified",
    ):
        assert required in report


def test_binding_schema_is_not_release_v1() -> None:
    assert BINDING_SIGNATURE_SCHEMA != "ignite-kindle-release-signature-v1"
    binding = build_publication_binding(
        release_tag=TAG,
        release_version=VERSION,
        release_manifest=_release_manifest(),
        scriptlet_sha256=hashlib.sha256(INSTALLER).hexdigest(),
        publication_evidence=_fixture_evidence(),
    )
    assert binding.startswith(b"IGNITE_PUBLICATION_BINDING_V1\n")
    assert b"schema=ignite-kindle-release-v1" not in binding
