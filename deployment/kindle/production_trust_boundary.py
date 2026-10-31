#!/usr/bin/env python3
"""Production trust boundary for Kindle publication evidence (Slice 3D-B2a.4.3g).

Single publisher authority: the repository Ed25519 pin used by release-v1.
Publication evidence is bound through an adjacent signed binding record,
not by extending the fixed six-line release-v1 contract.

Verification-only. Success never authorizes install, launch, promotion, or
rollback. While the pin is UNPROVISIONED, the production entry point refuses.

Digest authentication of opaque evidence bytes is not publication-evidence
claim validation. A production-scoped evidence schema belongs to a later slice.
PublicationTestTrustRoot from Slice 3f is never accepted here.
"""

from __future__ import annotations

import hashlib
import importlib.util
import re
import tempfile
from dataclasses import dataclass
from enum import Enum
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parents[1]
_RELEASE_TOOLS = _REPO / "tools" / "release"
_PRODUCTION_PIN = _RELEASE_TOOLS / "kindle-release-public-key.hex"

BINDING_HEADER = "IGNITE_PUBLICATION_BINDING_V1"
BINDING_SCHEMA = "1"
BINDING_SIGNATURE_SCHEMA = "ignite-kindle-publication-binding-signature-v1"
BINDING_SCOPE = "production-pin-required"
RELEASE_MANIFEST_SCHEMA = "ignite-kindle-release-v1"
RELEASE_SIGNATURE_SCHEMA = "ignite-kindle-release-signature-v1"
TEST_EVIDENCE_SCOPE = "publisher_authentication_scope=test-trust-root-only"
PLACEHOLDER_EVIDENCE_HEADER = "IGNITE_PUBLICATION_EVIDENCE_PROD_PLACEHOLDER_V1"

MAX_BINDING_BYTES = 2048
MAX_EVIDENCE_BYTES = 64 * 1024
MAX_RELEASE_MANIFEST_BYTES = 1024
MAX_SIGNATURE_BYTES = 512

_SHA_RE = re.compile(r"^[0-9a-f]{64}$")
_SIG_RE = re.compile(r"^[0-9a-f]{128}$")
_VERSION_RE = re.compile(
    r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$"
)


class ProductionTrustError(Exception):
    """Fail-closed production trust refusal."""


class VerificationProvenance(Enum):
    """Which key material authenticated the chain. Set only by the entry point."""

    OFFICIAL_PRODUCTION_PIN = "official-production-pin"
    TEST_ONLY_FIXTURE_KEY = "test-only-fixture-key"


class EvidenceClaimStatus(Enum):
    """Digest match is not schema/component-claim validation."""

    DIGEST_AUTHENTICATED_CLAIMS_UNVERIFIED = "digest-authenticated-claims-unverified"


def _load_release_sign():
    path = _RELEASE_TOOLS / "kindle_release_sign.py"
    spec = importlib.util.spec_from_file_location("kindle_release_sign", path)
    if spec is None or spec.loader is None:
        raise ProductionTrustError("cannot load kindle_release_sign.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


_SIGN = _load_release_sign()


@dataclass(frozen=True)
class AuthenticatedReleaseV1:
    version: str
    tag: str
    artifact: str
    bytes_count: int
    sha256: str


@dataclass(frozen=True)
class AuthenticatedPublicationChain:
    """Authenticated release-v1 + binding digests. Not an authorization grant."""

    release: AuthenticatedReleaseV1
    release_manifest_sha256: str
    publication_evidence_sha256: str
    provenance: VerificationProvenance
    evidence_claims: EvidenceClaimStatus


# Back-compat alias used by earlier 3g tests; prefer AuthenticatedPublicationChain.
AuthenticatedProductionRelease = AuthenticatedPublicationChain


def assert_production_pin_unprovisioned() -> None:
    raw = _PRODUCTION_PIN.read_bytes()
    if raw != _SIGN.UNPROVISIONED:
        raise ProductionTrustError("production public-key pin is not UNPROVISIONED")


def load_production_public_key() -> bytes:
    """Load the repository production pin. Refuses UNPROVISIONED and test keys."""
    try:
        return _SIGN.load_public_key(_PRODUCTION_PIN, official=True)
    except _SIGN.ReleaseSigningError as exc:
        message = str(exc)
        if "not provisioned" in message:
            raise ProductionTrustError("official release public key is not provisioned") from exc
        raise ProductionTrustError(message) from exc


def refuse_for_production_publication_evidence(evidence: bytes) -> None:
    """Eligibility gate for production publication-evidence validation claims.

    No production evidence schema is defined in this slice; every current pattern refuses.
    """
    if not evidence:
        raise ProductionTrustError("publication evidence must not be empty")
    if len(evidence) > MAX_EVIDENCE_BYTES:
        raise ProductionTrustError("publication evidence too large")
    if TEST_EVIDENCE_SCOPE.encode("ascii") in evidence:
        raise ProductionTrustError(
            "test-trust-root-only publication evidence cannot authenticate under production pin"
        )
    if evidence.startswith(PLACEHOLDER_EVIDENCE_HEADER.encode("ascii")):
        raise ProductionTrustError(
            "placeholder publication evidence is not production evidence"
        )
    raise ProductionTrustError(
        "publication evidence schema is unknown; production claims remain unvalidated"
    )


def build_publication_binding(
    *,
    release_tag: str,
    release_version: str,
    release_manifest: bytes,
    scriptlet_sha256: str,
    publication_evidence: bytes,
) -> bytes:
    """Build the canonical eight-line publication binding (unsigned)."""
    if not release_tag.startswith("v") or release_tag[1:] != release_version:
        raise ProductionTrustError("release_tag/version inconsistent")
    if not _VERSION_RE.fullmatch(release_version):
        raise ProductionTrustError("noncanonical release_version")
    if not _SHA_RE.fullmatch(scriptlet_sha256):
        raise ProductionTrustError("scriptlet_sha256 malformed")
    if len(publication_evidence) > MAX_EVIDENCE_BYTES:
        raise ProductionTrustError("publication evidence too large")
    release_digest = hashlib.sha256(release_manifest).hexdigest()
    evidence_digest = hashlib.sha256(publication_evidence).hexdigest()
    raw = (
        f"{BINDING_HEADER}\n"
        f"schema={BINDING_SCHEMA}\n"
        f"release_tag={release_tag}\n"
        f"release_version={release_version}\n"
        f"release_manifest_sha256={release_digest}\n"
        f"scriptlet_sha256={scriptlet_sha256}\n"
        f"publication_evidence_sha256={evidence_digest}\n"
        f"publisher_authentication_scope={BINDING_SCOPE}\n"
    ).encode("ascii")
    if len(raw) > MAX_BINDING_BYTES:
        raise ProductionTrustError("binding too large")
    return raw


def binding_signature_envelope(signature: bytes) -> bytes:
    if len(signature) != 64:
        raise ProductionTrustError("Ed25519 signature must be 64 bytes")
    return (
        f"schema={BINDING_SIGNATURE_SCHEMA}\nsignature={signature.hex()}\n"
    ).encode("ascii")


def parse_binding_signature(raw: bytes) -> bytes:
    if not raw:
        raise ProductionTrustError("publication binding signature is missing")
    if len(raw) > MAX_SIGNATURE_BYTES or b"\r" in raw:
        raise ProductionTrustError("publication binding signature is malformed")
    text = raw.decode("ascii", errors="strict")
    if not text.endswith("\n"):
        raise ProductionTrustError("publication binding signature is malformed")
    lines = text[:-1].split("\n")
    if len(lines) != 2 or lines[0] != f"schema={BINDING_SIGNATURE_SCHEMA}":
        raise ProductionTrustError("publication binding signature is malformed")
    if not lines[1].startswith("signature="):
        raise ProductionTrustError("publication binding signature is malformed")
    hex_sig = lines[1][len("signature=") :]
    if not _SIG_RE.fullmatch(hex_sig):
        raise ProductionTrustError("publication binding signature is malformed")
    return bytes.fromhex(hex_sig)


def parse_release_signature(raw: bytes) -> bytes:
    if not raw:
        raise ProductionTrustError("release signature is missing")
    if len(raw) > MAX_SIGNATURE_BYTES or b"\r" in raw:
        raise ProductionTrustError("release signature is malformed")
    text = raw.decode("ascii", errors="strict")
    if not text.endswith("\n"):
        raise ProductionTrustError("release signature is malformed")
    lines = text[:-1].split("\n")
    if len(lines) != 2 or lines[0] != f"schema={RELEASE_SIGNATURE_SCHEMA}":
        raise ProductionTrustError("release signature is malformed")
    if not lines[1].startswith("signature="):
        raise ProductionTrustError("release signature is malformed")
    hex_sig = lines[1][len("signature=") :]
    if not _SIG_RE.fullmatch(hex_sig):
        raise ProductionTrustError("release signature is malformed")
    return bytes.fromhex(hex_sig)


def _verify_raw_signature(message: bytes, signature: bytes, public_key: bytes) -> None:
    with tempfile.TemporaryDirectory(prefix="ignite-prod-trust-") as temporary:
        try:
            _SIGN.verify_signature(message, signature, public_key, Path(temporary))
        except _SIGN.ReleaseSigningError as exc:
            raise ProductionTrustError("signature verification failed") from exc


def parse_release_v1(manifest: bytes) -> AuthenticatedReleaseV1:
    if len(manifest) > MAX_RELEASE_MANIFEST_BYTES or b"\r" in manifest:
        raise ProductionTrustError("release-v1 manifest refused")
    text = manifest.decode("ascii", errors="strict")
    if not text.endswith("\n"):
        raise ProductionTrustError("release-v1 must end with LF")
    lines = text[:-1].split("\n")
    if len(lines) != 6:
        raise ProductionTrustError("release-v1 must contain exactly six lines")
    if lines[0] != f"schema={RELEASE_MANIFEST_SCHEMA}":
        raise ProductionTrustError("unsupported release-v1 schema")

    def field(prefix: str, line: str) -> str:
        if not line.startswith(prefix):
            raise ProductionTrustError("release-v1 field missing")
        return line[len(prefix) :]

    version = field("version=", lines[1])
    tag = field("tag=", lines[2])
    artifact = field("artifact=", lines[3])
    bytes_token = field("bytes=", lines[4])
    sha256 = field("sha256=", lines[5])
    if not _VERSION_RE.fullmatch(version):
        raise ProductionTrustError("release-v1 version noncanonical")
    if tag != f"v{version}":
        raise ProductionTrustError("release-v1 tag/version inconsistent")
    if artifact != f"Install Ignite v{version}.sh":
        raise ProductionTrustError("release-v1 artifact name refused")
    if not re.fullmatch(r"(0|[1-9][0-9]*)", bytes_token):
        raise ProductionTrustError("release-v1 bytes refused")
    if not _SHA_RE.fullmatch(sha256):
        raise ProductionTrustError("release-v1 sha256 refused")
    canonical = (
        f"schema={RELEASE_MANIFEST_SCHEMA}\n"
        f"version={version}\n"
        f"tag={tag}\n"
        f"artifact={artifact}\n"
        f"bytes={bytes_token}\n"
        f"sha256={sha256}\n"
    ).encode("ascii")
    if canonical != manifest:
        raise ProductionTrustError("release-v1 is not exact canonical encoding")
    return AuthenticatedReleaseV1(
        version=version,
        tag=tag,
        artifact=artifact,
        bytes_count=int(bytes_token),
        sha256=sha256,
    )


def parse_binding(binding: bytes) -> dict[str, str]:
    if len(binding) > MAX_BINDING_BYTES or b"\r" in binding:
        raise ProductionTrustError("binding refused")
    text = binding.decode("ascii", errors="strict")
    if not text.endswith("\n"):
        raise ProductionTrustError("binding must end with LF")
    lines = text[:-1].split("\n")
    if len(lines) != 8:
        raise ProductionTrustError("binding must contain exactly eight lines")
    if lines[0] != BINDING_HEADER or lines[1] != f"schema={BINDING_SCHEMA}":
        raise ProductionTrustError("unsupported binding schema")

    def field(prefix: str, line: str) -> str:
        if not line.startswith(prefix):
            raise ProductionTrustError("binding field missing")
        return line[len(prefix) :]

    release_tag = field("release_tag=", lines[2])
    release_version = field("release_version=", lines[3])
    release_manifest_sha256 = field("release_manifest_sha256=", lines[4])
    scriptlet_sha256 = field("scriptlet_sha256=", lines[5])
    publication_evidence_sha256 = field("publication_evidence_sha256=", lines[6])
    if lines[7] != f"publisher_authentication_scope={BINDING_SCOPE}":
        raise ProductionTrustError("binding must declare production-pin-required scope")
    if not release_tag.startswith("v") or release_tag[1:] != release_version:
        raise ProductionTrustError("binding tag/version inconsistent")
    if not _VERSION_RE.fullmatch(release_version):
        raise ProductionTrustError("binding release_version noncanonical")
    for digest in (
        release_manifest_sha256,
        scriptlet_sha256,
        publication_evidence_sha256,
    ):
        if not _SHA_RE.fullmatch(digest):
            raise ProductionTrustError("binding digest malformed")
    canonical = (
        f"{BINDING_HEADER}\n"
        f"schema={BINDING_SCHEMA}\n"
        f"release_tag={release_tag}\n"
        f"release_version={release_version}\n"
        f"release_manifest_sha256={release_manifest_sha256}\n"
        f"scriptlet_sha256={scriptlet_sha256}\n"
        f"publication_evidence_sha256={publication_evidence_sha256}\n"
        f"publisher_authentication_scope={BINDING_SCOPE}\n"
    ).encode("ascii")
    if canonical != binding:
        raise ProductionTrustError("binding is not exact canonical encoding")
    return {
        "release_tag": release_tag,
        "release_version": release_version,
        "release_manifest_sha256": release_manifest_sha256,
        "scriptlet_sha256": scriptlet_sha256,
        "publication_evidence_sha256": publication_evidence_sha256,
    }


def _authenticate_publication_chain_inner(
    *,
    release_manifest: bytes,
    release_signature: bytes,
    binding_manifest: bytes,
    binding_signature: bytes,
    publication_evidence: bytes,
    expected_tag: str,
    public_key: bytes,
    provenance: VerificationProvenance,
) -> AuthenticatedPublicationChain:
    if len(public_key) != 32:
        raise ProductionTrustError("public key must be 32 raw Ed25519 bytes")
    if len(publication_evidence) > MAX_EVIDENCE_BYTES:
        raise ProductionTrustError("publication evidence too large")

    release_sig = parse_release_signature(release_signature)
    _verify_raw_signature(release_manifest, release_sig, public_key)
    release = parse_release_v1(release_manifest)
    if release.tag != expected_tag:
        raise ProductionTrustError("release-v1 tag mismatch")

    binding_sig = parse_binding_signature(binding_signature)
    _verify_raw_signature(binding_manifest, binding_sig, public_key)
    binding = parse_binding(binding_manifest)

    if binding["release_tag"] != release.tag:
        raise ProductionTrustError("binding tag does not match release-v1 tag")
    if binding["release_version"] != release.version:
        raise ProductionTrustError("binding version does not match release-v1 version")

    release_digest = hashlib.sha256(release_manifest).hexdigest()
    if release_digest != binding["release_manifest_sha256"]:
        raise ProductionTrustError(
            "binding release_manifest_sha256 does not match release-v1 bytes"
        )
    if release.sha256 != binding["scriptlet_sha256"]:
        raise ProductionTrustError(
            "binding scriptlet_sha256 does not match authenticated release-v1 sha256"
        )
    evidence_digest = hashlib.sha256(publication_evidence).hexdigest()
    if evidence_digest != binding["publication_evidence_sha256"]:
        raise ProductionTrustError(
            "binding publication_evidence_sha256 does not match evidence bytes"
        )

    return AuthenticatedPublicationChain(
        release=release,
        release_manifest_sha256=release_digest,
        publication_evidence_sha256=evidence_digest,
        provenance=provenance,
        evidence_claims=EvidenceClaimStatus.DIGEST_AUTHENTICATED_CLAIMS_UNVERIFIED,
    )


def authenticate_publication_chain_with_key(
    *,
    release_manifest: bytes,
    release_signature: bytes,
    binding_manifest: bytes,
    binding_signature: bytes,
    publication_evidence: bytes,
    expected_tag: str,
    public_key: bytes,
) -> AuthenticatedPublicationChain:
    """Isolated verification seam for disposable fixtures.

    Digest-binds opaque fixture bytes under test-only provenance. Must not be wired
    to environment variables or production key substitution. Claims remain unverified.
    """
    if not publication_evidence:
        raise ProductionTrustError("publication evidence must not be empty")
    if TEST_EVIDENCE_SCOPE.encode("ascii") in publication_evidence:
        raise ProductionTrustError(
            "test-trust-root-only publication evidence cannot authenticate under production pin"
        )
    return _authenticate_publication_chain_inner(
        release_manifest=release_manifest,
        release_signature=release_signature,
        binding_manifest=binding_manifest,
        binding_signature=binding_signature,
        publication_evidence=publication_evidence,
        expected_tag=expected_tag,
        public_key=public_key,
        provenance=VerificationProvenance.TEST_ONLY_FIXTURE_KEY,
    )


def authenticate_official_publication_chain(
    *,
    release_manifest: bytes,
    release_signature: bytes,
    binding_manifest: bytes,
    binding_signature: bytes,
    publication_evidence: bytes,
    expected_tag: str,
) -> AuthenticatedPublicationChain:
    """Production entry point: repository pin only. No key override.

    Refuses empty, placeholder, unknown-schema, and 3f test-scope evidence.
    """
    public_key = load_production_public_key()
    refuse_for_production_publication_evidence(publication_evidence)
    return _authenticate_publication_chain_inner(
        release_manifest=release_manifest,
        release_signature=release_signature,
        binding_manifest=binding_manifest,
        binding_signature=binding_signature,
        publication_evidence=publication_evidence,
        expected_tag=expected_tag,
        public_key=public_key,
        provenance=VerificationProvenance.OFFICIAL_PRODUCTION_PIN,
    )


def _publisher_authentication_line(provenance: VerificationProvenance) -> str:
    if provenance is VerificationProvenance.OFFICIAL_PRODUCTION_PIN:
        return "publisher_authentication=verified-under-production-pin"
    return "publisher_authentication=verified-under-test-only-fixture-key"


def production_trust_report(
    outcome: AuthenticatedPublicationChain | ProductionTrustError | Exception,
) -> str:
    """Observation-only report. Provenance derives only from the verification result."""
    lines = [
        "IGNITE_PRODUCTION_TRUST_BOUNDARY_V1",
        "schema=1",
        "installation_binding=unverified",
        "execution_binding=unverified",
        "first_launch_admission=unavailable",
        "data_compatibility=unknown",
        "promotion_authorized=false",
    ]
    if isinstance(outcome, AuthenticatedPublicationChain):
        lines.extend(
            [
                _publisher_authentication_line(outcome.provenance),
                "publication_evidence_digest=authenticated",
                "publication_evidence_claims=unverified",
                f"release_tag={outcome.release.tag}",
                f"release_manifest_sha256={outcome.release_manifest_sha256}",
                f"publication_evidence_sha256={outcome.publication_evidence_sha256}",
                "authorization=none",
            ]
        )
    elif isinstance(outcome, ProductionTrustError) and "not provisioned" in str(outcome):
        lines.extend(
            [
                "publisher_authentication=unavailable",
                "publication_evidence_claims=unverified",
                "refusal=official-key-not-provisioned",
            ]
        )
    else:
        lines.extend(
            [
                "publisher_authentication=unavailable",
                "publication_evidence_claims=unverified",
                f"refusal={outcome}",
            ]
        )
    return "\n".join(lines) + "\n"


def sign_binding_for_test(
    binding: bytes, *, private_key_path: Path, public_key: bytes
) -> bytes:
    """Sign a binding with a disposable key. Never writes the production pin."""
    assert_production_pin_unprovisioned()
    if _PRODUCTION_PIN.resolve() == private_key_path.resolve():
        raise ProductionTrustError("refusing to use production pin path for test signing")
    with tempfile.TemporaryDirectory(prefix="ignite-prod-bind-sign-") as temporary:
        work = Path(temporary)
        signature = _SIGN.sign_manifest(binding, private_key_path, work)
        _SIGN.verify_signature(binding, signature, public_key, work)
    return binding_signature_envelope(signature)


def release_signature_envelope(signature: bytes) -> bytes:
    if len(signature) != 64:
        raise ProductionTrustError("Ed25519 signature must be 64 bytes")
    return (
        f"schema={RELEASE_SIGNATURE_SCHEMA}\nsignature={signature.hex()}\n"
    ).encode("ascii")


def sign_release_for_test(
    release_manifest: bytes, *, private_key_path: Path, public_key: bytes
) -> bytes:
    assert_production_pin_unprovisioned()
    with tempfile.TemporaryDirectory(prefix="ignite-prod-rel-sign-") as temporary:
        work = Path(temporary)
        signature = _SIGN.sign_manifest(release_manifest, private_key_path, work)
        _SIGN.verify_signature(release_manifest, signature, public_key, work)
    return release_signature_envelope(signature)
