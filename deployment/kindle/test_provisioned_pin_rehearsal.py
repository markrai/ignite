"""Isolated provisioned-pin full-chain rehearsal (Slice 3D-B2a.4.3p).

Provisions a disposable Ed25519 identity into a temporary repository pin and
exercises the real production verification / promote-gate paths against that
isolated filesystem pin. Never mutates tools/release/kindle-release-public-key.hex.

Compatible with a repository pin that is either UNPROVISIONED or an intentionally
provisioned official public key.
"""

from __future__ import annotations

import hashlib
import shutil
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
_SFX = _HERE / "sfx"
_REPO = _HERE.parents[1]
_RELEASE = _REPO / "tools" / "release"
for extra in (_HERE, _SFX, str(_RELEASE)):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from authenticated_publication_evidence import generate_test_trust_root  # noqa: E402
from authenticated_publication_preflight import (  # noqa: E402
    PreflightError,
    run_promote_gate,
)
from build_sfx import build_sfx, default_template  # noqa: E402
from ci_packaging_measurement import (  # noqa: E402
    build_ci_packaging_measurement,
    write_packaging_measurements,
)
from kindle_release_sign import KNOWN_TEST_PUBLIC_KEYS, UNPROVISIONED  # noqa: E402
from offline_release_ceremony import sign_ceremony  # noqa: E402
from production_trust_boundary import (  # noqa: E402
    ProductionTrustError,
    assert_production_pin_well_formed,
    authenticate_official_publication_chain,
    authenticate_publication_chain_with_key,
    load_production_public_key,
    production_pin_is_unprovisioned,
    refuse_fixture_use_of_production_pin,
)
from release_component_profiles import generate_profiles  # noqa: E402
from release_identity import EXPECTED_WORKFLOW  # noqa: E402
from test_validate_stage import make_payload  # noqa: E402
from verify_sfx_payload_equivalence import verify_equivalence  # noqa: E402

VERSION = "3.3.2"
TAG = f"v{VERSION}"
SOURCE_REV = "b" * 40
_REPO_PIN = _RELEASE / "kindle-release-public-key.hex"
_REPO_PIN_AT_IMPORT = _REPO_PIN.read_bytes()


def _assert_repo_pin_untouched() -> None:
    """Suite never mutates the committed production pin (any well-formed state)."""
    assert _REPO_PIN.read_bytes() == _REPO_PIN_AT_IMPORT


def _public_key_hex(root, path: Path) -> Path:
    path.write_bytes(root.public_key.hex().encode("ascii") + b"\n")
    return path


def _ci_artifacts(tmp: Path) -> Path:
    payload = make_payload(tmp / "src", VERSION)
    app = payload / "bin" / "scribe-app"
    app.write_bytes(app.read_bytes() + b"\n#3p-rehearsal\n")
    scriptlet = tmp / "artifacts" / f"Install Ignite v{VERSION}.sh"
    scriptlet.parent.mkdir(parents=True, exist_ok=True)
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
        workflow_run_id="900001",
    )
    write_packaging_measurements(
        out_dir=scriptlet.parent,
        measurement=measurement,
        package_profile=package_profile,
        equivalence_report=report,
    )
    digest = hashlib.sha256(scriptlet.read_bytes()).hexdigest()
    (scriptlet.parent / f"Install Ignite v{VERSION}.sha256").write_bytes(
        f"{digest}  Install Ignite v{VERSION}.sh\n".encode("ascii")
    )
    return scriptlet.parent


@pytest.fixture
def isolated_publisher(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Disposable publisher whose public half is the isolated production pin."""
    import authenticated_publication_preflight as ap
    import production_trust_boundary as ptb

    root = generate_test_trust_root(tmp_path / "keys", label="3p-isolated-publisher")
    pin = tmp_path / "isolated-production-pin.hex"
    pin.write_bytes(root.public_key.hex().encode("ascii") + b"\n")
    monkeypatch.setattr(ap, "_PRODUCTION_PIN", pin)
    monkeypatch.setattr(ptb, "_PRODUCTION_PIN", pin)
    pub = _public_key_hex(root, tmp_path / "matching-pub.hex")
    yield {"root": root, "pin": pin, "pub": pub}
    _assert_repo_pin_untouched()


def _sign_under_isolated_pin(tmp_path: Path, isolated_publisher) -> Path:
    artifacts = _ci_artifacts(tmp_path / "arts")
    out = tmp_path / "signed"
    sign_ceremony(
        tag=TAG,
        ci_artifacts=artifacts,
        private_key=isolated_publisher["root"].private_key_path,
        public_key_path=isolated_publisher["pub"],
        output_dir=out,
        expected_source_revision=SOURCE_REV,
    )
    # Ceremony output is the six-asset authenticated set; attach Linux stubs so
    # the release-candidate layout mirrors the nine-file promote candidate.
    linux = f"ignite-server-{VERSION}-linux-x86_64.tar.gz"
    (out / linux).write_bytes(b"linux-stub\n")
    digest = hashlib.sha256(b"linux-stub\n").hexdigest()
    (out / f"{linux}.sha256").write_bytes(f"{digest}  {linux}\n".encode("ascii"))
    sha_sidecar = artifacts / f"Install Ignite v{VERSION}.sha256"
    if not (out / sha_sidecar.name).exists():
        shutil.copy2(sha_sidecar, out / sha_sidecar.name)
    return out


def test_repo_pin_is_well_formed_sentinel_or_provisioned() -> None:
    """Accept UNPROVISIONED or a valid intentionally provisioned official key."""
    assert_production_pin_well_formed()
    raw = _REPO_PIN.read_bytes()
    if raw == UNPROVISIONED:
        assert production_pin_is_unprovisioned() is True
    else:
        assert production_pin_is_unprovisioned() is False
        key = load_production_public_key()
        assert len(key) == 32
        assert key not in KNOWN_TEST_PUBLIC_KEYS
    _assert_repo_pin_untouched()


def test_missing_pin_is_not_unprovisioned(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import production_trust_boundary as ptb

    missing = tmp_path / "absent-pin.hex"
    monkeypatch.setattr(ptb, "_PRODUCTION_PIN", missing)
    with pytest.raises(ProductionTrustError, match="cannot read"):
        production_pin_is_unprovisioned()
    with pytest.raises(ProductionTrustError, match="cannot read"):
        assert_production_pin_well_formed()
    with pytest.raises(ProductionTrustError, match="unreadable|malformed"):
        refuse_fixture_use_of_production_pin(public_key=bytes.fromhex("cd" * 32))
    _assert_repo_pin_untouched()


def test_malformed_official_pin_refuses_all_fixture_signing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Hex-decodable but officially invalid pins refuse even differing fixture keys."""
    import production_trust_boundary as ptb

    bad = tmp_path / "kindle-release-public-key.hex"
    # Uppercase hex decodes, but official loader requires lowercase.
    bad.write_bytes(("AB" * 32).encode("ascii") + b"\n")
    monkeypatch.setattr(ptb, "_PRODUCTION_PIN", bad)
    different_fixture = bytes.fromhex("cd" * 32)
    with pytest.raises(ProductionTrustError, match="malformed|refusing"):
        refuse_fixture_use_of_production_pin(public_key=different_fixture)
    with pytest.raises(ProductionTrustError, match="malformed|refusing"):
        refuse_fixture_use_of_production_pin(public_key=bytes.fromhex("ab" * 32))
    with pytest.raises(ProductionTrustError, match="production pin path"):
        refuse_fixture_use_of_production_pin(public_key_path=bad)
    _assert_repo_pin_untouched()


def test_known_test_key_as_official_pin_refuses_fixture_signing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import production_trust_boundary as ptb

    known = next(iter(KNOWN_TEST_PUBLIC_KEYS))
    pin = tmp_path / "kindle-release-public-key.hex"
    pin.write_bytes(known.hex().encode("ascii") + b"\n")
    monkeypatch.setattr(ptb, "_PRODUCTION_PIN", pin)
    with pytest.raises(ProductionTrustError, match="malformed|refusing|test key"):
        refuse_fixture_use_of_production_pin(public_key=bytes.fromhex("cd" * 32))
    _assert_repo_pin_untouched()


def test_valid_official_pin_allows_distinct_fixture_key(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import production_trust_boundary as ptb

    official = generate_test_trust_root(tmp_path / "official", label="3p-official")
    pin = tmp_path / "kindle-release-public-key.hex"
    pin.write_bytes(official.public_key.hex().encode("ascii") + b"\n")
    monkeypatch.setattr(ptb, "_PRODUCTION_PIN", pin)
    fixture = generate_test_trust_root(tmp_path / "fixture", label="3p-fixture")
    assert fixture.public_key != official.public_key
    refuse_fixture_use_of_production_pin(public_key=fixture.public_key)
    with pytest.raises(ProductionTrustError, match="production public key material"):
        refuse_fixture_use_of_production_pin(public_key=official.public_key)
    alt_path = tmp_path / "official-copy.hex"
    alt_path.write_bytes(official.public_key.hex().encode("ascii") + b"\n")
    with pytest.raises(ProductionTrustError, match="production public key material"):
        refuse_fixture_use_of_production_pin(public_key_path=alt_path)
    _assert_repo_pin_untouched()


def test_unprovisioned_pin_allows_fixture_signing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import production_trust_boundary as ptb

    unprov = tmp_path / "kindle-release-public-key.hex"
    unprov.write_bytes(UNPROVISIONED)
    monkeypatch.setattr(ptb, "_PRODUCTION_PIN", unprov)
    refuse_fixture_use_of_production_pin(public_key=bytes.fromhex("cd" * 32))
    _assert_repo_pin_untouched()


def test_provisioned_pin_full_chain_acceptance(
    isolated_publisher, tmp_path: Path
) -> None:
    signed = _sign_under_isolated_pin(tmp_path, isolated_publisher)
    receipt = (signed / f"ignite-kindle-v{VERSION}.signing-receipt.v1").read_text(
        encoding="ascii"
    )
    assert "preflight_trust_context=production" in receipt
    assert "official_pin_matched_caller_public_key=true" in receipt
    assert load_production_public_key() == isolated_publisher["root"].public_key

    chain = authenticate_official_publication_chain(
        release_manifest=(signed / f"ignite-kindle-v{VERSION}.manifest").read_bytes(),
        release_signature=(
            signed / f"ignite-kindle-v{VERSION}.manifest.sig"
        ).read_bytes(),
        binding_manifest=(
            signed / f"ignite-kindle-v{VERSION}.publication-binding"
        ).read_bytes(),
        binding_signature=(
            signed / f"ignite-kindle-v{VERSION}.publication-binding.sig"
        ).read_bytes(),
        publication_evidence=(
            signed / f"ignite-kindle-v{VERSION}.publication-evidence"
        ).read_bytes(),
        expected_tag=TAG,
    )
    assert chain.release.tag == TAG

    gate = run_promote_gate(
        release_candidate_dir=signed,
        tag=TAG,
        source_revision=SOURCE_REV,
        expected_workflow=EXPECTED_WORKFLOW,
    )
    assert gate.production_promotion_ready is True
    _assert_repo_pin_untouched()


def test_provisioned_pin_refuses_wrong_publisher_key(
    isolated_publisher, tmp_path: Path
) -> None:
    signed = _sign_under_isolated_pin(tmp_path, isolated_publisher)
    other = generate_test_trust_root(tmp_path / "other", label="3p-wrong")
    with pytest.raises(ProductionTrustError, match="signature"):
        authenticate_publication_chain_with_key(
            release_manifest=(
                signed / f"ignite-kindle-v{VERSION}.manifest"
            ).read_bytes(),
            release_signature=(
                signed / f"ignite-kindle-v{VERSION}.manifest.sig"
            ).read_bytes(),
            binding_manifest=(
                signed / f"ignite-kindle-v{VERSION}.publication-binding"
            ).read_bytes(),
            binding_signature=(
                signed / f"ignite-kindle-v{VERSION}.publication-binding.sig"
            ).read_bytes(),
            publication_evidence=(
                signed / f"ignite-kindle-v{VERSION}.publication-evidence"
            ).read_bytes(),
            expected_tag=TAG,
            public_key=other.public_key,
        )
    authenticate_official_publication_chain(
        release_manifest=(signed / f"ignite-kindle-v{VERSION}.manifest").read_bytes(),
        release_signature=(
            signed / f"ignite-kindle-v{VERSION}.manifest.sig"
        ).read_bytes(),
        binding_manifest=(
            signed / f"ignite-kindle-v{VERSION}.publication-binding"
        ).read_bytes(),
        binding_signature=(
            signed / f"ignite-kindle-v{VERSION}.publication-binding.sig"
        ).read_bytes(),
        publication_evidence=(
            signed / f"ignite-kindle-v{VERSION}.publication-evidence"
        ).read_bytes(),
        expected_tag=TAG,
    )
    _assert_repo_pin_untouched()


def test_provisioned_pin_refuses_mutated_scriptlet_and_binding(
    isolated_publisher, tmp_path: Path
) -> None:
    signed = _sign_under_isolated_pin(tmp_path, isolated_publisher)
    assert (
        run_promote_gate(
            release_candidate_dir=signed,
            tag=TAG,
            source_revision=SOURCE_REV,
            expected_workflow=EXPECTED_WORKFLOW,
        ).production_promotion_ready
        is True
    )

    mutated = tmp_path / "nine-mut"
    shutil.copytree(signed, mutated)
    scriptlet = mutated / f"Install Ignite v{VERSION}.sh"
    scriptlet.write_bytes(scriptlet.read_bytes() + b"\n#mutated\n")
    with pytest.raises(PreflightError):
        run_promote_gate(
            release_candidate_dir=mutated,
            tag=TAG,
            source_revision=SOURCE_REV,
            expected_workflow=EXPECTED_WORKFLOW,
        )

    binding_mut = tmp_path / "nine-bind"
    shutil.copytree(signed, binding_mut)
    bind = binding_mut / f"ignite-kindle-v{VERSION}.publication-binding"
    bind.write_bytes(bind.read_bytes() + b"\n")
    with pytest.raises(PreflightError):
        run_promote_gate(
            release_candidate_dir=binding_mut,
            tag=TAG,
            source_revision=SOURCE_REV,
            expected_workflow=EXPECTED_WORKFLOW,
        )

    evidence_mut = tmp_path / "nine-ev"
    shutil.copytree(signed, evidence_mut)
    evidence = evidence_mut / f"ignite-kindle-v{VERSION}.publication-evidence"
    evidence.write_bytes(evidence.read_bytes() + b"\n#tamper\n")
    with pytest.raises(PreflightError):
        run_promote_gate(
            release_candidate_dir=evidence_mut,
            tag=TAG,
            source_revision=SOURCE_REV,
            expected_workflow=EXPECTED_WORKFLOW,
        )

    sig_mut = tmp_path / "nine-sig"
    shutil.copytree(signed, sig_mut)
    sig = sig_mut / f"ignite-kindle-v{VERSION}.manifest.sig"
    raw = bytearray(sig.read_bytes())
    for i, ch in enumerate(raw):
        if ch in b"0123456789abcdef":
            raw[i] = ord("0") if ch != ord("0") else ord("1")
            break
    sig.write_bytes(bytes(raw))
    with pytest.raises(PreflightError):
        run_promote_gate(
            release_candidate_dir=sig_mut,
            tag=TAG,
            source_revision=SOURCE_REV,
            expected_workflow=EXPECTED_WORKFLOW,
        )
    _assert_repo_pin_untouched()


def test_fixture_material_cannot_impersonate_official_pin_path(
    isolated_publisher, tmp_path: Path
) -> None:
    pin = isolated_publisher["pin"]
    with pytest.raises(ProductionTrustError, match="production pin path"):
        refuse_fixture_use_of_production_pin(private_key_path=pin)
    with pytest.raises(ProductionTrustError, match="production pin path|fixture"):
        refuse_fixture_use_of_production_pin(public_key_path=pin)
    with pytest.raises(ProductionTrustError, match="production public key material"):
        refuse_fixture_use_of_production_pin(
            public_key=isolated_publisher["root"].public_key
        )
    _assert_repo_pin_untouched()


def test_unprovisioned_state_still_refuses_publication(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Negative unprovisioned coverage isolated from the repository pin file."""
    import authenticated_publication_preflight as ap
    import production_trust_boundary as ptb

    unprov = tmp_path / "kindle-release-public-key.hex"
    unprov.write_bytes(UNPROVISIONED)
    monkeypatch.setattr(ap, "_PRODUCTION_PIN", unprov)
    monkeypatch.setattr(ptb, "_PRODUCTION_PIN", unprov)
    assert production_pin_is_unprovisioned() is True
    with pytest.raises(PreflightError, match="UNPROVISIONED|unavailable"):
        run_promote_gate(
            release_candidate_dir=tmp_path / "missing-candidate",
            tag=TAG,
            source_revision=SOURCE_REV,
            expected_workflow=EXPECTED_WORKFLOW,
        )
    _assert_repo_pin_untouched()
