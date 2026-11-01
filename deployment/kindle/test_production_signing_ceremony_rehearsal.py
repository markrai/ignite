"""Disposable-key rehearsal of the proposed production signing ceremony (3m).

This is NOT the production ceremony. It uses isolated fixture keys only and
keeps tools/release/kindle-release-public-key.hex as UNPROVISIONED.

Demonstrates the proposed operator sequence with current tooling:
  packaging measurement verify → evidence prepare → release-v1 + binding sign
  → independent verify → six-asset preflight → promote-gate refusal

Documents missing production interfaces as refusals, not bypasses.
"""

from __future__ import annotations

import hashlib
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
_SFX = _HERE / "sfx"
_REPO = _HERE.parents[1]
for extra in (_HERE, _SFX):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from authenticated_publication_evidence import generate_test_trust_root  # noqa: E402
from authenticated_publication_preflight import (  # noqa: E402
    PreflightError,
    TrustContext,
    run_preflight,
    run_promote_gate,
)
from build_sfx import build_sfx, default_template  # noqa: E402
from offline_packaging_handoff import (  # noqa: E402
    HandoffError,
    prepare_evidence_from_handoff,
    rehearse_fixture_signing_from_measurement,
    verify_handoff_inputs,
)
from production_trust_boundary import (  # noqa: E402
    ProductionTrustError,
    assert_production_pin_unprovisioned,
    authenticate_publication_chain_with_key,
)
from release_component_profiles import generate_profiles  # noqa: E402
from test_validate_stage import make_payload  # noqa: E402
from verify_sfx_payload_equivalence import verify_equivalence  # noqa: E402
from ci_packaging_measurement import (  # noqa: E402
    MEASUREMENT_BASENAME,
    build_ci_packaging_measurement,
    write_packaging_measurements,
)

VERSION = "3.3.1"
TAG = f"v{VERSION}"
SOURCE_REV = "c" * 40
RUN_ID = "424242"


def _public_key_hex(root, path: Path) -> Path:
    path.write_bytes(root.public_key.hex().encode("ascii") + b"\n")
    return path


def _ceremony_inputs(tmp: Path, *, tag: bytes = b"CEREMONY") -> dict[str, Path | bytes]:
    """Build real packaging measurement inputs for the disposable ceremony."""
    payload = make_payload(tmp / "src", VERSION)
    app = payload / "bin" / "scribe-app"
    app.write_bytes(app.read_bytes() + tag)
    scriptlet = tmp / f"Install Ignite v{VERSION}.sh"
    scriptlet.write_bytes(build_sfx(payload, VERSION, default_template()))
    profiles = generate_profiles(payload, VERSION)
    package_profile = profiles["package"]
    ok, report = verify_equivalence(
        sfx_path=scriptlet,
        payload_path=payload,
        version=VERSION,
        package_profile=package_profile,
    )
    assert ok, report.decode("ascii", errors="replace")
    measurement = build_ci_packaging_measurement(
        version=VERSION,
        scriptlet=scriptlet,
        payload=payload,
        package_profile=package_profile,
        source_revision=SOURCE_REV,
        workflow_run_id=RUN_ID,
    )
    meas_dir = tmp / "measurements"
    write_packaging_measurements(
        out_dir=meas_dir,
        measurement=measurement,
        package_profile=package_profile,
        equivalence_report=report,
    )
    return {
        "scriptlet": scriptlet,
        "measurement": meas_dir / MEASUREMENT_BASENAME,
        "profile": meas_dir / "ignite-kindle-component-profile-package.v1",
        "measurement_bytes": measurement,
        "package_profile": package_profile,
    }


@pytest.fixture
def disposable_root(tmp_path: Path):
    assert_production_pin_unprovisioned()
    return generate_test_trust_root(tmp_path / "keys", label="slice-3m-ceremony")


def test_production_pin_remains_unprovisioned() -> None:
    pin = _REPO / "tools" / "release" / "kindle-release-public-key.hex"
    assert pin.read_bytes() == b"UNPROVISIONED\n"


def test_proposed_ceremony_rehearsal_with_disposable_keys(
    tmp_path: Path, disposable_root
) -> None:
    """Full disposable rehearsal of the proposed offline signing ceremony."""
    inputs = _ceremony_inputs(tmp_path / "build")
    scriptlet = Path(str(inputs["scriptlet"]))
    meas = Path(str(inputs["measurement"]))
    profile = Path(str(inputs["profile"]))
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")

    # Step: independently verify Scriptlet ↔ measurement ↔ canonical profile.
    # Does NOT establish CI provenance or publisher authority.
    handoff = verify_handoff_inputs(
        scriptlet=scriptlet,
        measurement_path=meas,
        package_profile_path=profile,
        expected_version=VERSION,
        expected_source_revision=SOURCE_REV,
    )
    assert handoff.evidence_ready is True
    assert handoff.ci_provenance_established is False
    assert handoff.production_authority is False
    lines = "\n".join(handoff.summary_lines())
    assert "ci_provenance_established_by_this_verifier=false" in lines
    assert "production_publisher_authority=false" in lines
    assert (
        "operator_asserted_source_revision_is_not_ci_provenance_proof=true" in lines
        or ("require_trusted_promotion_path_for_ci_provenance=true" in lines)
    )

    # Step: prepare production-format evidence from validated measurements.
    evidence = prepare_evidence_from_handoff(
        scriptlet=scriptlet,
        measurement_path=meas,
        package_profile_path=profile,
        expected_version=VERSION,
    )
    assert evidence.startswith(b"IGNITE_PUBLICATION_EVIDENCE_PRODUCTION_V1\n")

    # Step: sign release-v1 + binding under disposable publisher (fixture only).
    out = tmp_path / "six"
    assets = rehearse_fixture_signing_from_measurement(
        version=VERSION,
        scriptlet=scriptlet,
        measurement_path=meas,
        package_profile_path=profile,
        private_key_path=disposable_root.private_key_path,
        public_key_path=pub,
        output_dir=out,
    )
    assert (
        assets.publication_evidence == evidence
        or hashlib.sha256(assets.publication_evidence).hexdigest()
        == hashlib.sha256(evidence).hexdigest()
    )

    # Step: independently verify signatures under the disposable public key.
    auth = authenticate_publication_chain_with_key(
        release_manifest=assets.release_manifest,
        release_signature=assets.release_signature,
        binding_manifest=assets.publication_binding,
        binding_signature=assets.binding_signature,
        publication_evidence=assets.publication_evidence,
        expected_tag=TAG,
        public_key=disposable_root.public_key,
    )
    assert auth is not None

    report = run_preflight(
        assets_dir=out,
        expected_tag=TAG,
        trust=TrustContext.FIXTURE,
        public_key_path=pub,
        source_revision=SOURCE_REV,
        expected_workflow=".github/workflows/ci.yml",
    )
    assert report.fixture_rehearsal_ready is True
    assert report.production_promotion_ready is False
    assert report.ci_provenance_verified_by_preflight is False
    assert report.verification_provenance == "verified-under-test-only-fixture-key"

    # Production promote-gate must refuse while pin is UNPROVISIONED.
    with pytest.raises(PreflightError, match="UNPROVISIONED|unavailable|production"):
        run_promote_gate(release_candidate_dir=out, tag=TAG)

    # Real pin untouched.
    assert (
        _REPO / "tools" / "release" / "kindle-release-public-key.hex"
    ).read_bytes() == b"UNPROVISIONED\n"


def test_ceremony_refuses_changed_ci_artifact(tmp_path: Path, disposable_root) -> None:
    inputs = _ceremony_inputs(tmp_path / "a", tag=b"A")
    _other = _ceremony_inputs(tmp_path / "b", tag=b"B")
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    with pytest.raises(HandoffError, match="identity|mismatch"):
        rehearse_fixture_signing_from_measurement(
            version=VERSION,
            scriptlet=Path(str(_other["scriptlet"])),
            measurement_path=Path(str(inputs["measurement"])),
            package_profile_path=Path(str(inputs["profile"])),
            private_key_path=disposable_root.private_key_path,
            public_key_path=pub,
            output_dir=tmp_path / "out",
        )


def test_ceremony_refuses_wrong_version_identity(
    tmp_path: Path, disposable_root
) -> None:
    inputs = _ceremony_inputs(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    with pytest.raises(HandoffError, match="version"):
        rehearse_fixture_signing_from_measurement(
            version="9.9.9",
            scriptlet=Path(str(inputs["scriptlet"])),
            measurement_path=Path(str(inputs["measurement"])),
            package_profile_path=Path(str(inputs["profile"])),
            private_key_path=disposable_root.private_key_path,
            public_key_path=pub,
            output_dir=tmp_path / "out",
        )


def test_ceremony_refuses_wrong_signing_key(tmp_path: Path, disposable_root) -> None:
    inputs = _ceremony_inputs(tmp_path)
    other = generate_test_trust_root(tmp_path / "other", label="wrong-publisher")
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    out = tmp_path / "six"
    # Sign with disposable_root, verify under other key → refuse.
    assets = rehearse_fixture_signing_from_measurement(
        version=VERSION,
        scriptlet=Path(str(inputs["scriptlet"])),
        measurement_path=Path(str(inputs["measurement"])),
        package_profile_path=Path(str(inputs["profile"])),
        private_key_path=disposable_root.private_key_path,
        public_key_path=pub,
        output_dir=out,
    )
    with pytest.raises(ProductionTrustError, match="signature verification failed"):
        authenticate_publication_chain_with_key(
            release_manifest=assets.release_manifest,
            release_signature=assets.release_signature,
            binding_manifest=assets.publication_binding,
            binding_signature=assets.binding_signature,
            publication_evidence=assets.publication_evidence,
            expected_tag=TAG,
            public_key=other.public_key,
        )


def test_ceremony_refuses_incomplete_signed_bundle(
    tmp_path: Path, disposable_root
) -> None:
    inputs = _ceremony_inputs(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    out = tmp_path / "six"
    rehearse_fixture_signing_from_measurement(
        version=VERSION,
        scriptlet=Path(str(inputs["scriptlet"])),
        measurement_path=Path(str(inputs["measurement"])),
        package_profile_path=Path(str(inputs["profile"])),
        private_key_path=disposable_root.private_key_path,
        public_key_path=pub,
        output_dir=out,
    )
    (out / f"ignite-kindle-v{VERSION}.publication-binding.sig").unlink()
    with pytest.raises(PreflightError):
        run_preflight(
            assets_dir=out,
            expected_tag=TAG,
            trust=TrustContext.FIXTURE,
            public_key_path=pub,
        )


def test_ceremony_refuses_incorrect_provenance_assertion(
    tmp_path: Path,
) -> None:
    inputs = _ceremony_inputs(tmp_path)
    with pytest.raises(HandoffError, match="source_revision"):
        verify_handoff_inputs(
            scriptlet=Path(str(inputs["scriptlet"])),
            measurement_path=Path(str(inputs["measurement"])),
            package_profile_path=Path(str(inputs["profile"])),
            expected_version=VERSION,
            expected_source_revision="d" * 40,
        )


def test_fixture_signing_helpers_stay_isolated_while_unprovisioned() -> None:
    """Fixture signing remains available only while the production pin is UNPROVISIONED.

    A future production binding-signing CLI is a planned post-approval
    implementation (see docs/draft/kindle-production-signing-ceremony.md), not a
    permanent absence invariant for this test suite.
    """
    import kindle_release_sign as krs
    import production_trust_boundary as ptb

    assert (
        _REPO / "tools" / "release" / "kindle-release-public-key.hex"
    ).read_bytes() == b"UNPROVISIONED\n"
    assert_production_pin_unprovisioned()
    assert hasattr(krs, "command_sign")
    assert hasattr(ptb, "sign_binding_for_test")
    assert hasattr(ptb, "sign_release_for_test")


def test_fixture_never_grants_production_authority(
    tmp_path: Path, disposable_root
) -> None:
    inputs = _ceremony_inputs(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    out = tmp_path / "six"
    rehearse_fixture_signing_from_measurement(
        version=VERSION,
        scriptlet=Path(str(inputs["scriptlet"])),
        measurement_path=Path(str(inputs["measurement"])),
        package_profile_path=Path(str(inputs["profile"])),
        private_key_path=disposable_root.private_key_path,
        public_key_path=pub,
        output_dir=out,
    )
    report = run_preflight(
        assets_dir=out,
        expected_tag=TAG,
        trust=TrustContext.FIXTURE,
        public_key_path=pub,
    )
    assert report.production_promotion_ready is False
    with pytest.raises(PreflightError):
        run_promote_gate(release_candidate_dir=out, tag=TAG)
