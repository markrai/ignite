"""CI packaging measurement + offline handoff regressions (Slice 3D-B2a.4.3l).

Uses genuinely packaged disposable Scriptlets (make_payload + build_sfx +
release profiles), not synthetic evidence inventories unrelated to the installer.
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
from ci_packaging_measurement import (  # noqa: E402
    MEASUREMENT_BASENAME,
    MEASUREMENT_HEADER,
    MeasurementError,
    build_ci_packaging_measurement,
    build_production_evidence_from_measurement,
    parse_ci_packaging_measurement,
    verify_measurement_against_scriptlet,
    write_packaging_measurements,
)
from offline_packaging_handoff import (  # noqa: E402
    HandoffError,
    prepare_evidence_from_handoff,
    rehearse_fixture_signing_from_measurement,
    verify_handoff_inputs,
)
from production_publication_evidence import (  # noqa: E402
    parse_and_validate_production_evidence,
)
from production_trust_boundary import (  # noqa: E402
    assert_production_pin_well_formed,
    refuse_fixture_use_of_production_pin,
)
from release_component_profiles import COMPONENT_ROLES, generate_profiles  # noqa: E402
from test_validate_stage import make_payload  # noqa: E402
from verify_sfx_payload_equivalence import verify_equivalence  # noqa: E402

VERSION = "3.3.1"
TAG = f"v{VERSION}"
SOURCE_REV = "a" * 40


def _public_key_hex(root, path: Path) -> Path:
    path.write_bytes(root.public_key.hex().encode("ascii") + b"\n")
    return path


def _packaged(
    tmp: Path, *, version: str = VERSION, tag: bytes = b"MEAS"
) -> tuple[Path, Path, bytes, bytes]:
    """Real packaging path: stage → SFX → profiles → equivalence."""
    payload = make_payload(tmp / "src", version)
    # Distinguish fixture bytes without leaving the packaging contract.
    app = payload / "bin" / "scribe-app"
    app.write_bytes(app.read_bytes() + tag)
    scriptlet = tmp / f"Install Ignite v{version}.sh"
    scriptlet.write_bytes(build_sfx(payload, version, default_template()))
    profiles = generate_profiles(payload, version)
    package_profile = profiles["package"]
    ok, report = verify_equivalence(
        sfx_path=scriptlet,
        payload_path=payload,
        version=version,
        package_profile=package_profile,
    )
    assert ok, report.decode("ascii", errors="replace")
    return payload, scriptlet, package_profile, report


def _measurement_bundle(
    tmp: Path,
    *,
    source_revision: str | None = SOURCE_REV,
    workflow_run_id: str | None = "12345",
) -> tuple[Path, Path, Path, bytes]:
    payload, scriptlet, package_profile, report = _packaged(tmp / "pkg")
    measurement = build_ci_packaging_measurement(
        version=VERSION,
        scriptlet=scriptlet,
        payload=payload,
        package_profile=package_profile,
        source_revision=source_revision,
        workflow_run_id=workflow_run_id,
    )
    out = tmp / "measurements"
    write_packaging_measurements(
        out_dir=out,
        measurement=measurement,
        package_profile=package_profile,
        equivalence_report=report,
    )
    return (
        scriptlet,
        out / MEASUREMENT_BASENAME,
        out / "ignite-kindle-component-profile-package.v1",
        measurement,
    )


_REPO_PIN = _REPO / "tools" / "release" / "kindle-release-public-key.hex"
_REPO_PIN_AT_IMPORT = _REPO_PIN.read_bytes()


def _assert_repo_pin_untouched() -> None:
    assert _REPO_PIN.read_bytes() == _REPO_PIN_AT_IMPORT


@pytest.fixture
def disposable_root(tmp_path: Path):
    assert_production_pin_well_formed()
    return generate_test_trust_root(tmp_path / "keys", label="slice-3l-handoff")


def test_production_pin_is_well_formed() -> None:
    assert_production_pin_well_formed()
    _assert_repo_pin_untouched()


def test_build_and_verify_measurement_roundtrip(tmp_path: Path) -> None:
    scriptlet, meas_path, profile_path, raw = _measurement_bundle(tmp_path)
    parsed = parse_ci_packaging_measurement(raw)
    assert parsed["version"] == VERSION
    assert parsed["tag"] == TAG
    assert len(parsed["components"]) == len(COMPONENT_ROLES)
    assert raw.startswith(MEASUREMENT_HEADER.encode("ascii") + b"\n")
    assert b"\r" not in raw

    result = verify_measurement_against_scriptlet(
        measurement=raw,
        scriptlet=scriptlet,
        package_profile=profile_path.read_bytes(),
        expected_version=VERSION,
        expected_source_revision=SOURCE_REV,
    )
    assert result.scriptlet_integrity is True
    assert result.scriptlet_identity_match is True
    assert result.component_consistency is True
    assert result.package_profile_match is True
    assert result.ci_provenance_established_by_this_verifier is False
    assert result.production_publisher_authority is False
    lines = "\n".join(result.summary_lines())
    assert "offline_evidence_inputs_ready=true" in lines
    assert "production_promotion_ready=false" in lines
    assert "measurement_is_not_publisher_signed=true" in lines
    assert "scriptlet_executed=false" in lines
    assert "ci_provenance_established_by_this_verifier=false" in lines


def test_measurement_without_profile_is_limited(tmp_path: Path) -> None:
    scriptlet, meas_path, _profile, raw = _measurement_bundle(tmp_path)
    result = verify_measurement_against_scriptlet(
        measurement=raw, scriptlet=scriptlet, package_profile=None
    )
    assert result.component_consistency is True
    assert result.package_profile_match is False
    assert "offline_evidence_inputs_ready=false" in "\n".join(result.summary_lines())


def test_digest_consistent_semantically_false_profile_refused(
    tmp_path: Path,
) -> None:
    """Tampered profile + matching unsigned measurement digest must not pass."""
    scriptlet, _meas_path, profile_path, raw = _measurement_bundle(tmp_path)
    good_profile = profile_path.read_bytes()
    bad_profile = good_profile.replace(
        b"promotion_authorized=false\n", b"promotion_authorized=true\n", 1
    )
    assert bad_profile != good_profile
    bad_sha = hashlib.sha256(bad_profile).hexdigest()
    lines = raw.decode("ascii").splitlines()
    for idx, line in enumerate(lines):
        if line.startswith("package_profile_sha256="):
            lines[idx] = f"package_profile_sha256={bad_sha}"
            break
    else:
        raise AssertionError("package_profile_sha256 missing")
    bad_measurement = ("\n".join(lines) + "\n").encode("ascii")
    assert hashlib.sha256(bad_profile).hexdigest() == bad_sha

    with pytest.raises(MeasurementError, match="canonical|contents"):
        verify_measurement_against_scriptlet(
            measurement=bad_measurement,
            scriptlet=scriptlet,
            package_profile=bad_profile,
        )

    bad_meas_path = tmp_path / "tampered.measurement"
    bad_prof_path = tmp_path / "tampered.profile"
    bad_meas_path.write_bytes(bad_measurement)
    bad_prof_path.write_bytes(bad_profile)
    with pytest.raises(HandoffError, match="canonical|contents|ready"):
        prepare_evidence_from_handoff(
            scriptlet=scriptlet,
            measurement_path=bad_meas_path,
            package_profile_path=bad_prof_path,
            expected_version=VERSION,
        )


def test_unrelated_profile_with_matching_digest_field_refused(
    tmp_path: Path,
) -> None:
    """A foreign profile whose digest is correctly declared still refuses."""
    scriptlet_a, _m_a, profile_a, raw_a = _measurement_bundle(tmp_path / "a")
    _payload_b, _scriptlet_b, profile_b, _ = _packaged(tmp_path / "b", tag=b"OTHER")
    assert profile_a.read_bytes() != profile_b
    foreign = profile_b
    foreign_sha = hashlib.sha256(foreign).hexdigest()
    lines = raw_a.decode("ascii").splitlines()
    for idx, line in enumerate(lines):
        if line.startswith("package_profile_sha256="):
            lines[idx] = f"package_profile_sha256={foreign_sha}"
            break
    forged = ("\n".join(lines) + "\n").encode("ascii")
    with pytest.raises(MeasurementError, match="canonical|contents"):
        verify_measurement_against_scriptlet(
            measurement=forged,
            scriptlet=scriptlet_a,
            package_profile=foreign,
        )


def test_unsigned_verify_and_prepare_independent_of_provisioned_pin(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Unsigned handoff remains usable once a production pin is provisioned."""
    import production_trust_boundary as ptb

    fake_pin = tmp_path / "kindle-release-public-key.hex"
    # Syntactically valid pin bytes (64 lowercase hex + LF); not the repo pin.
    # Use a non-known-test key pattern that is still distinct from disposable fixtures.
    fake_pin.write_bytes(("ab" * 32).encode("ascii") + b"\n")
    monkeypatch.setattr(ptb, "_PRODUCTION_PIN", fake_pin)
    # Pin is no longer UNPROVISIONED under the monkeypatch.
    from production_trust_boundary import ProductionTrustError

    with pytest.raises(ProductionTrustError):
        ptb.assert_production_pin_unprovisioned()
    # Fixture helpers remain available via isolation checks, not UNPROVISIONED.
    refuse_fixture_use_of_production_pin(
        private_key_path=tmp_path / "disposable.pem",
        public_key=bytes.fromhex("cd" * 32),
    )

    scriptlet, meas_path, profile_path, _ = _measurement_bundle(tmp_path / "pkg")
    handoff = verify_handoff_inputs(
        scriptlet=scriptlet,
        measurement_path=meas_path,
        package_profile_path=profile_path,
        expected_version=VERSION,
    )
    assert handoff.evidence_ready is True
    assert handoff.production_authority is False
    assert "production_publisher_authority=false" in "\n".join(handoff.summary_lines())
    assert "production_promotion_ready=false" in "\n".join(handoff.summary_lines())

    evidence = prepare_evidence_from_handoff(
        scriptlet=scriptlet,
        measurement_path=meas_path,
        package_profile_path=profile_path,
        expected_version=VERSION,
    )
    validated = parse_and_validate_production_evidence(
        evidence,
        expected_version=VERSION,
        expected_scriptlet_sha256=hashlib.sha256(scriptlet.read_bytes()).hexdigest(),
        expected_scriptlet_bytes=scriptlet.stat().st_size,
    )
    assert validated.version == VERSION
    # Real repository pin must remain untouched by the monkeypatch.
    _assert_repo_pin_untouched()


def test_oversized_measurement_refused(tmp_path: Path) -> None:
    from ci_packaging_measurement import MAX_MEASUREMENT_BYTES, read_bounded_plain_file

    path = tmp_path / "huge.measurement"
    path.write_bytes(b"x" * (MAX_MEASUREMENT_BYTES + 1))
    with pytest.raises(MeasurementError, match="size out of bounds"):
        read_bounded_plain_file(
            path, max_bytes=MAX_MEASUREMENT_BYTES, label="measurement"
        )


def test_empty_measurement_refused(tmp_path: Path) -> None:
    from ci_packaging_measurement import read_bounded_plain_file

    path = tmp_path / "empty.measurement"
    path.write_bytes(b"")
    with pytest.raises(MeasurementError, match="size out of bounds|empty"):
        read_bounded_plain_file(path, max_bytes=64 * 1024, label="measurement")


def test_oversized_profile_refused_by_handoff(tmp_path: Path) -> None:
    scriptlet, meas_path, _profile, _ = _measurement_bundle(tmp_path)
    huge = tmp_path / "huge.profile"
    huge.write_bytes(b"y" * (64 * 1024 + 1))
    with pytest.raises(HandoffError, match="size out of bounds"):
        verify_handoff_inputs(
            scriptlet=scriptlet,
            measurement_path=meas_path,
            package_profile_path=huge,
        )


@pytest.mark.skipif(
    sys.platform == "win32", reason="symlink privileges vary on Windows"
)
def test_symlink_measurement_refused(tmp_path: Path) -> None:
    import os

    scriptlet, meas_path, profile_path, _ = _measurement_bundle(tmp_path)
    link = tmp_path / "link.measurement"
    try:
        os.symlink(meas_path.name, link)
    except OSError:
        pytest.skip("symlink creation unavailable on this host")
    with pytest.raises(HandoffError, match="symlink"):
        verify_handoff_inputs(
            scriptlet=scriptlet,
            measurement_path=link,
            package_profile_path=profile_path,
        )


def test_deterministic_rerun_unchanged_inputs(tmp_path: Path) -> None:
    payload, scriptlet, package_profile, _ = _packaged(tmp_path)
    a = build_ci_packaging_measurement(
        version=VERSION,
        scriptlet=scriptlet,
        payload=payload,
        package_profile=package_profile,
        source_revision=SOURCE_REV,
        workflow_run_id="99",
    )
    b = build_ci_packaging_measurement(
        version=VERSION,
        scriptlet=scriptlet,
        payload=payload,
        package_profile=package_profile,
        source_revision=SOURCE_REV,
        workflow_run_id="99",
    )
    assert a == b


def test_scriptlet_changed_after_measurement(tmp_path: Path) -> None:
    scriptlet, meas_path, profile_path, raw = _measurement_bundle(tmp_path)
    scriptlet.write_bytes(scriptlet.read_bytes() + b"\n# mutated\n")
    with pytest.raises(MeasurementError, match="identity"):
        verify_measurement_against_scriptlet(
            measurement=raw,
            scriptlet=scriptlet,
            package_profile=profile_path.read_bytes(),
        )


def test_measurement_belongs_to_another_scriptlet(tmp_path: Path) -> None:
    scriptlet_a, _meas_a, profile_a, raw_a = _measurement_bundle(
        tmp_path / "a", source_revision=SOURCE_REV
    )
    _payload_b, scriptlet_b, _profile_b, _ = _packaged(tmp_path / "b", tag=b"OTHER")
    with pytest.raises(MeasurementError, match="identity"):
        verify_measurement_against_scriptlet(
            measurement=raw_a,
            scriptlet=scriptlet_b,
            package_profile=profile_a.read_bytes(),
        )
    del scriptlet_a


def test_component_digest_changed_in_measurement(tmp_path: Path) -> None:
    scriptlet, _meas, profile_path, raw = _measurement_bundle(tmp_path)
    text = raw.decode("ascii")
    # Flip one hex nibble in the first file component digest.
    lines = text.splitlines()
    for idx, line in enumerate(lines):
        if line.startswith("component=scribe-app|file|"):
            parts = line.split("|")
            digest = parts[3]
            flipped = ("0" if digest[0] != "0" else "1") + digest[1:]
            parts[3] = flipped
            lines[idx] = "|".join(parts)
            break
    else:
        raise AssertionError("scribe-app line missing")
    bad = ("\n".join(lines) + "\n").encode("ascii")
    with pytest.raises(MeasurementError, match="mismatch|inventory"):
        verify_measurement_against_scriptlet(
            measurement=bad,
            scriptlet=scriptlet,
            package_profile=profile_path.read_bytes(),
        )


def test_component_role_missing(tmp_path: Path) -> None:
    _scriptlet, _meas, _profile, raw = _measurement_bundle(tmp_path)
    lines = [
        ln
        for ln in raw.decode("ascii").splitlines()
        if not ln.startswith("component=scribe-app|")
    ]
    # Keep role_count claim but drop a component line → order/count fail.
    bad = ("\n".join(lines) + "\n").encode("ascii")
    with pytest.raises(MeasurementError):
        parse_ci_packaging_measurement(bad)


def test_component_role_duplicated(tmp_path: Path) -> None:
    _scriptlet, _meas, _profile, raw = _measurement_bundle(tmp_path)
    lines = raw.decode("ascii").splitlines()
    app_line = next(ln for ln in lines if ln.startswith("component=scribe-app|"))
    # Insert duplicate before measurement_scope.
    out: list[str] = []
    for ln in lines:
        if ln.startswith("measurement_scope="):
            out.append(app_line)
        out.append(ln)
    bad = ("\n".join(out) + "\n").encode("ascii")
    with pytest.raises(MeasurementError, match="order|trailing|role|scope"):
        parse_ci_packaging_measurement(bad)


def test_component_count_inconsistent(tmp_path: Path) -> None:
    _scriptlet, _meas, _profile, raw = _measurement_bundle(tmp_path)
    bad = raw.replace(b"component_role_count=22\n", b"component_role_count=21\n", 1)
    with pytest.raises(MeasurementError, match="component_role_count"):
        parse_ci_packaging_measurement(bad)


def test_wrong_version_tag(tmp_path: Path) -> None:
    scriptlet, _meas, profile_path, raw = _measurement_bundle(tmp_path)
    with pytest.raises(MeasurementError, match="version"):
        verify_measurement_against_scriptlet(
            measurement=raw,
            scriptlet=scriptlet,
            package_profile=profile_path.read_bytes(),
            expected_version="9.9.9",
        )


def test_incorrect_packaging_schema(tmp_path: Path) -> None:
    _scriptlet, _meas, _profile, raw = _measurement_bundle(tmp_path)
    bad = raw.replace(
        b"IGNITE_CI_PACKAGING_MEASUREMENT_V1\nschema=1\n",
        b"IGNITE_CI_PACKAGING_MEASUREMENT_V1\nschema=99\n",
        1,
    )
    with pytest.raises(MeasurementError, match="schema"):
        parse_ci_packaging_measurement(bad)


def test_noncanonical_and_malformed_measurement(tmp_path: Path) -> None:
    with pytest.raises(MeasurementError):
        parse_ci_packaging_measurement(b"")
    with pytest.raises(MeasurementError):
        parse_ci_packaging_measurement(b"not-a-measurement\n")
    _scriptlet, _meas, _profile, raw = _measurement_bundle(tmp_path)
    with pytest.raises(MeasurementError):
        parse_ci_packaging_measurement(raw.replace(b"\n", b"\r\n"))


def test_missing_measurement_artifact(tmp_path: Path) -> None:
    scriptlet = tmp_path / f"Install Ignite v{VERSION}.sh"
    scriptlet.write_bytes(b"#!/bin/sh\n")
    with pytest.raises(HandoffError, match="missing measurement"):
        verify_handoff_inputs(
            scriptlet=scriptlet,
            measurement_path=tmp_path / "missing.v1",
            package_profile_path=None,
        )


def test_incorrect_claimed_source_revision(tmp_path: Path) -> None:
    scriptlet, _meas, profile_path, raw = _measurement_bundle(tmp_path)
    with pytest.raises(MeasurementError, match="source_revision"):
        verify_measurement_against_scriptlet(
            measurement=raw,
            scriptlet=scriptlet,
            package_profile=profile_path.read_bytes(),
            expected_source_revision="b" * 40,
        )


def test_untrusted_workflow_context_is_not_ci_provenance(tmp_path: Path) -> None:
    """Operator-asserted run id is recorded but never proves CI provenance."""
    scriptlet, meas_path, profile_path, _ = _measurement_bundle(tmp_path)
    result = verify_handoff_inputs(
        scriptlet=scriptlet,
        measurement_path=meas_path,
        package_profile_path=profile_path,
        expected_version=VERSION,
        expected_source_revision=SOURCE_REV,
    )
    assert result.ci_provenance_established is False
    lines = "\n".join(result.summary_lines())
    assert "require_trusted_promotion_path_for_ci_provenance=true" in lines
    assert "ci_provenance_established_by_this_verifier=false" in lines


def test_matching_scriptlet_digest_alone_does_not_qualify(tmp_path: Path) -> None:
    """A forged measurement that only copies Scriptlet identity must still fail."""
    scriptlet, _meas, profile_path, raw = _measurement_bundle(tmp_path)
    parsed = parse_ci_packaging_measurement(raw)
    # Keep Scriptlet identity; corrupt a component digest claim.
    lines = raw.decode("ascii").splitlines()
    for idx, line in enumerate(lines):
        if line.startswith("component=icon|file|"):
            parts = line.split("|")
            parts[3] = "ff" * 32
            lines[idx] = "|".join(parts)
            break
    forged = ("\n".join(lines) + "\n").encode("ascii")
    # Identity still matches Scriptlet.
    assert f"scriptlet_sha256={parsed['scriptlet_sha256']}" in forged.decode("ascii")
    with pytest.raises(MeasurementError, match="mismatch|inventory"):
        verify_measurement_against_scriptlet(
            measurement=forged,
            scriptlet=scriptlet,
            package_profile=profile_path.read_bytes(),
        )


def test_evidence_from_validated_measurements(tmp_path: Path) -> None:
    scriptlet, meas_path, profile_path, raw = _measurement_bundle(tmp_path)
    evidence = build_production_evidence_from_measurement(
        raw,
        scriptlet=scriptlet,
        package_profile=profile_path.read_bytes(),
    )
    validated = parse_and_validate_production_evidence(
        evidence,
        expected_version=VERSION,
        expected_scriptlet_sha256=hashlib.sha256(scriptlet.read_bytes()).hexdigest(),
        expected_scriptlet_bytes=scriptlet.stat().st_size,
    )
    assert validated.version == VERSION
    assert len(validated.components) == 22
    # prepare-evidence entry point
    again = prepare_evidence_from_handoff(
        scriptlet=scriptlet,
        measurement_path=meas_path,
        package_profile_path=profile_path,
        expected_version=VERSION,
    )
    assert again == evidence


def test_mismatched_publication_evidence_refused(
    tmp_path: Path, disposable_root
) -> None:
    scriptlet, meas_path, profile_path, _ = _measurement_bundle(tmp_path)
    out = tmp_path / "fixture-assets"
    assets = rehearse_fixture_signing_from_measurement(
        version=VERSION,
        scriptlet=scriptlet,
        measurement_path=meas_path,
        package_profile_path=profile_path,
        private_key_path=disposable_root.private_key_path,
        public_key_path=_public_key_hex(disposable_root, tmp_path / "fixture.hex"),
        output_dir=out,
    )
    # Corrupt evidence after signing.
    evidence_path = out / f"ignite-kindle-v{VERSION}.publication-evidence"
    evidence_path.write_bytes(assets.publication_evidence + b"#tamper\n")
    pub = tmp_path / "fixture.hex"
    with pytest.raises((PreflightError, Exception)):
        run_preflight(
            assets_dir=out,
            expected_tag=TAG,
            trust=TrustContext.FIXTURE,
            public_key_path=pub,
        )


def test_full_fixture_rehearsal_from_real_measurements(
    tmp_path: Path, disposable_root
) -> None:
    scriptlet, meas_path, profile_path, _ = _measurement_bundle(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    out = tmp_path / "six"
    assets = rehearse_fixture_signing_from_measurement(
        version=VERSION,
        scriptlet=scriptlet,
        measurement_path=meas_path,
        package_profile_path=profile_path,
        private_key_path=disposable_root.private_key_path,
        public_key_path=pub,
        output_dir=out,
    )
    assert (
        assets.scriptlet_sha256
        == hashlib.sha256(
            (out / f"Install Ignite v{VERSION}.sh").read_bytes()
        ).hexdigest()
    )
    report = run_preflight(
        assets_dir=out,
        expected_tag=TAG,
        trust=TrustContext.FIXTURE,
        public_key_path=pub,
        source_revision=SOURCE_REV,
    )
    assert report.fixture_rehearsal_ready is True
    assert report.production_promotion_ready is False
    assert report.ci_provenance_verified_by_preflight is False
    # Fixture key is not production authority.
    with pytest.raises(PreflightError, match="UNPROVISIONED|unavailable|production"):
        run_promote_gate(release_candidate_dir=out, tag=TAG)


def test_fixture_signing_key_not_production_authority(
    tmp_path: Path, disposable_root
) -> None:
    scriptlet, meas_path, profile_path, _ = _measurement_bundle(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    out = tmp_path / "six"
    rehearse_fixture_signing_from_measurement(
        version=VERSION,
        scriptlet=scriptlet,
        measurement_path=meas_path,
        package_profile_path=profile_path,
        private_key_path=disposable_root.private_key_path,
        public_key_path=pub,
        output_dir=out,
    )
    result = verify_handoff_inputs(
        scriptlet=out / f"Install Ignite v{VERSION}.sh",
        measurement_path=meas_path,
        package_profile_path=profile_path,
    )
    assert result.production_authority is False
    lines = "\n".join(result.summary_lines())
    assert "production_publisher_authority=false" in lines
    assert "fixture_signing_is_not_production_authority=true" in lines


def test_handoff_refuses_github_mutation_claims(tmp_path: Path) -> None:
    scriptlet, meas_path, profile_path, _ = _measurement_bundle(tmp_path)
    result = verify_handoff_inputs(
        scriptlet=scriptlet,
        measurement_path=meas_path,
        package_profile_path=profile_path,
    )
    assert result.github_mutation is False
    assert "github_mutation=false" in "\n".join(result.summary_lines())


def test_package_release_emits_measurement_compatible_with_handoff(
    tmp_path: Path, disposable_root
) -> None:
    """End-to-end through package() — real packaging measurement → fixture six."""
    from package_release import package, sha256_file
    from test_validate_stage import _write_arm32le_elf

    version = "0.9.9"
    bins = tmp_path / "bins"
    bins.mkdir()
    _write_arm32le_elf(bins / "scribe-app", size=4096)
    _write_arm32le_elf(bins / "scribe-lifecycle", size=4096)
    drop = tmp_path / "dropbear"
    drop.mkdir()
    _write_arm32le_elf(drop / "dropbearmulti", size=2048)
    digest = hashlib.sha256((drop / "dropbearmulti").read_bytes()).hexdigest()
    (drop / "elf.sha256").write_text(digest + "\n", encoding="utf-8")
    (drop / "provenance.txt").write_text(
        "name=dropbearmulti\nrole=test-stub\nupstream_version=2024.85\n",
        encoding="utf-8",
    )
    pin = tmp_path / "PINNED.txt"
    pin.write_text(
        f"elf_sha256={digest}\nelf_bytes={(drop / 'dropbearmulti').stat().st_size}\n",
        encoding="utf-8",
    )
    out = tmp_path / "out"
    result = package(
        repo=_REPO,
        version=version,
        release_bin_dir=bins,
        dropbear_dir=drop,
        out_root=out,
        pinned=pin,
        support_zip=False,
    )
    scriptlet = Path(str(result["sfx"]))
    meas = Path(str(result["packaging_measurement"]))
    profile = (
        out / "packaging-measurements" / "ignite-kindle-component-profile-package.v1"
    )
    assert sha256_file(meas) == result["packaging_measurement_sha256"]
    handoff = verify_handoff_inputs(
        scriptlet=scriptlet,
        measurement_path=meas,
        package_profile_path=profile,
        expected_version=version,
    )
    assert handoff.evidence_ready is True
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    assets_dir = tmp_path / "ceremony"
    assets = rehearse_fixture_signing_from_measurement(
        version=version,
        scriptlet=scriptlet,
        measurement_path=meas,
        package_profile_path=profile,
        private_key_path=disposable_root.private_key_path,
        public_key_path=pub,
        output_dir=assets_dir,
    )
    assert assets.scriptlet_bytes == int(result["sfx_bytes"])
    report = run_preflight(
        assets_dir=assets_dir,
        expected_tag=f"v{version}",
        trust=TrustContext.FIXTURE,
        public_key_path=pub,
    )
    assert report.fixture_rehearsal_ready is True
    assert report.production_promotion_ready is False


def test_separate_build_measurement_rejected(tmp_path: Path) -> None:
    """Measurement from build A must not validate against Scriptlet from build B."""
    _s_a, _m_a, profile_a, raw_a = _measurement_bundle(tmp_path / "a")
    # Rebuild with different payload bytes.
    _payload_b, scriptlet_b, profile_b, _ = _packaged(tmp_path / "b", tag=b"BUILD-B")
    with pytest.raises(MeasurementError, match="identity"):
        verify_measurement_against_scriptlet(
            measurement=raw_a,
            scriptlet=scriptlet_b,
            package_profile=profile_a.read_bytes(),
        )
    # Even if we swap in B's profile, Scriptlet identity still fails first.
    with pytest.raises(MeasurementError, match="identity"):
        verify_measurement_against_scriptlet(
            measurement=raw_a,
            scriptlet=scriptlet_b,
            package_profile=profile_b,
        )


def test_measurement_fixture_bundle_rust_interop(
    tmp_path: Path, disposable_root
) -> None:
    """Evidence prepared from CI measurements must satisfy the Rust host interop."""
    import os
    import shutil
    import subprocess

    scriptlet, meas_path, profile_path, _ = _measurement_bundle(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    out = tmp_path / "six"
    assets = rehearse_fixture_signing_from_measurement(
        version=VERSION,
        scriptlet=scriptlet,
        measurement_path=meas_path,
        package_profile_path=profile_path,
        private_key_path=disposable_root.private_key_path,
        public_key_path=pub,
        output_dir=out,
    )
    cargo = shutil.which("cargo")
    if cargo is None:
        pytest.skip("cargo not available for Rust interop")
    interop = tmp_path / "interop"
    interop.mkdir()
    (interop / "release.manifest").write_bytes(assets.release_manifest)
    (interop / "release.sig").write_bytes(assets.release_signature)
    (interop / "binding").write_bytes(assets.publication_binding)
    (interop / "binding.sig").write_bytes(assets.binding_signature)
    (interop / "evidence").write_bytes(assets.publication_evidence)
    (interop / "public_key.hex").write_bytes(
        disposable_root.public_key.hex().encode("ascii") + b"\n"
    )
    (interop / "expected_tag").write_text(TAG + "\n", encoding="ascii")
    (interop / "scriptlet").write_bytes(scriptlet.read_bytes())
    env = os.environ.copy()
    env["IGNITE_PUBLICATION_INTEROP_DIR"] = str(interop)
    result = subprocess.run(
        [
            cargo,
            "test",
            "-p",
            "scribe-app",
            "publication_interop_host_bytes",
            "--lib",
            "--",
            "--nocapture",
        ],
        cwd=str(_REPO / "ignite" / "kindle" / "scribe-app"),
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        pytest.fail(result.stdout + "\n" + result.stderr)


def test_cli_verify_entry_point(tmp_path: Path) -> None:
    from ci_packaging_measurement import main as meas_main
    from offline_packaging_handoff import main as handoff_main

    scriptlet, meas_path, profile_path, _ = _measurement_bundle(tmp_path)
    assert (
        meas_main(
            [
                "verify",
                "--measurement",
                str(meas_path),
                "--scriptlet",
                str(scriptlet),
                "--package-profile",
                str(profile_path),
                "--expected-version",
                VERSION,
                "--expected-source-revision",
                SOURCE_REV,
            ]
        )
        == 0
    )
    assert (
        handoff_main(
            [
                "verify",
                "--measurement",
                str(meas_path),
                "--scriptlet",
                str(scriptlet),
                "--package-profile",
                str(profile_path),
                "--expected-version",
                VERSION,
            ]
        )
        == 0
    )
    # Missing measurement → refuse, no mutation.
    assert (
        handoff_main(
            [
                "verify",
                "--measurement",
                str(tmp_path / "gone.v1"),
                "--scriptlet",
                str(scriptlet),
            ]
        )
        == 1
    )
