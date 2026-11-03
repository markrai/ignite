"""Offline release ceremony CLI regressions (Slice 3D-B2a.4.3n corrections)."""

from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
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
from authenticated_publication_preflight import PreflightError, run_promote_gate  # noqa: E402
from build_sfx import build_sfx, default_template  # noqa: E402
from ci_packaging_measurement import (  # noqa: E402
    MEASUREMENT_BASENAME,
    PACKAGE_PROFILE_BASENAME,
    build_ci_packaging_measurement,
    write_packaging_measurements,
)
from offline_release_ceremony import (  # noqa: E402
    CeremonyError,
    CeremonyHooks,
    PROMOTE_ASSETS_SUBDIR,
    discover_ci_artifacts,
    main as ceremony_main,
    prepare_ceremony,
    sign_ceremony,
)
from production_trust_boundary import (  # noqa: E402
    ProductionTrustError,
    assert_production_pin_well_formed,
    parse_binding,
    sign_publication_binding,
)
from release_component_profiles import generate_profiles  # noqa: E402
from test_validate_stage import make_payload  # noqa: E402
from verify_sfx_payload_equivalence import verify_equivalence  # noqa: E402

VERSION = "3.3.1"
TAG = f"v{VERSION}"
SOURCE_REV = "e" * 40


def _public_key_hex(root, path: Path) -> Path:
    path.write_bytes(root.public_key.hex().encode("ascii") + b"\n")
    return path


def _ci_artifacts(tmp: Path, *, tag: bytes = b"CEREMONY-3N") -> Path:
    payload = make_payload(tmp / "src", VERSION)
    app = payload / "bin" / "scribe-app"
    app.write_bytes(app.read_bytes() + tag)
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
        workflow_run_id="99",
    )
    write_packaging_measurements(
        out_dir=scriptlet.parent,
        measurement=measurement,
        package_profile=package_profile,
        equivalence_report=report,
    )
    sha = hashlib.sha256(scriptlet.read_bytes()).hexdigest()
    (scriptlet.parent / f"Install Ignite v{VERSION}.sha256").write_bytes(
        f"{sha}  Install Ignite v{VERSION}.sh\n".encode("ascii")
    )
    return scriptlet.parent


def _nested_ci_artifacts(tmp: Path) -> Path:
    """Legitimate nested CI layout: customer dir + packaging-measurements dir."""
    flat = _ci_artifacts(tmp / "flat")
    root = tmp / "ci-run"
    customer = root / "ignite-kindle-release"
    measurements = root / "ignite-kindle-packaging-measurements"
    customer.mkdir(parents=True)
    measurements.mkdir(parents=True)
    for name in (
        f"Install Ignite v{VERSION}.sh",
        f"Install Ignite v{VERSION}.sha256",
    ):
        shutil.copy2(flat / name, customer / name)
    for name in (MEASUREMENT_BASENAME, PACKAGE_PROFILE_BASENAME):
        shutil.copy2(flat / name, measurements / name)
    return root


_REPO_PIN = _REPO / "tools" / "release" / "kindle-release-public-key.hex"
_REPO_PIN_AT_IMPORT = _REPO_PIN.read_bytes()


def _assert_repo_pin_untouched() -> None:
    assert _REPO_PIN.read_bytes() == _REPO_PIN_AT_IMPORT


@pytest.fixture
def disposable_root(tmp_path: Path):
    assert_production_pin_well_formed()
    return generate_test_trust_root(tmp_path / "keys", label="slice-3n-ceremony")


def _encrypted_pem(tmp: Path, *, passphrase: str) -> Path:
    tmp.mkdir(parents=True, exist_ok=True)
    enc = tmp / "encrypted.pem"
    r = subprocess.run(
        [
            "openssl",
            "genpkey",
            "-algorithm",
            "ED25519",
            "-aes-256-cbc",
            "-pass",
            f"pass:{passphrase}",
            "-out",
            os.fspath(enc),
        ],
        check=False,
        capture_output=True,
    )
    assert r.returncode == 0, r.stderr.decode("utf-8", errors="replace")
    return enc


def _pub_from_pem(pem: Path, out: Path, *, passphrase: str | None = None) -> Path:
    args = ["openssl", "pkey", "-in", os.fspath(pem), "-pubout", "-outform", "DER"]
    env = os.environ.copy()
    if passphrase is not None:
        args.extend(["-passin", "env:IGNITE_SIGNING_KEY_PASSPHRASE"])
        env["IGNITE_SIGNING_KEY_PASSPHRASE"] = passphrase
    der_path = out.with_suffix(".der")
    r = subprocess.run(args + ["-out", os.fspath(der_path)], check=False, env=env)
    assert r.returncode == 0
    der = der_path.read_bytes()
    assert len(der) == 44
    out.write_bytes(der[12:].hex().encode("ascii") + b"\n")
    return out


def test_production_pin_is_well_formed() -> None:
    assert_production_pin_well_formed()
    _assert_repo_pin_untouched()


def test_prepare_cli_success(tmp_path: Path) -> None:
    artifacts = _ci_artifacts(tmp_path)
    receipt = tmp_path / "prepare.receipt"
    assert (
        ceremony_main(
            [
                "prepare",
                "--tag",
                TAG,
                "--ci-artifacts",
                str(artifacts),
                "--expected-source-revision",
                SOURCE_REV,
                "--receipt-out",
                str(receipt),
            ]
        )
        == 0
    )
    text = receipt.read_text(encoding="ascii")
    assert "mode=prepare" in text
    assert "signing_performed=false" in text
    assert "publisher_signatures_verified=false" in text
    assert "ci_provenance_established_by_this_ceremony=false" in text
    assert "production_promotion_ready=false" in text
    assert "production_publisher_authority=false" in text


def test_prepare_receipt_out_refuses_existing(tmp_path: Path) -> None:
    artifacts = _ci_artifacts(tmp_path)
    receipt = tmp_path / "prepare.receipt"
    receipt.write_bytes(b"occupied\n")
    assert (
        ceremony_main(
            [
                "prepare",
                "--tag",
                TAG,
                "--ci-artifacts",
                str(artifacts),
                "--receipt-out",
                str(receipt),
            ]
        )
        == 1
    )
    assert receipt.read_bytes() == b"occupied\n"


def test_prepare_receipt_out_refuses_dangling_symlink(tmp_path: Path) -> None:
    if sys.platform == "win32":
        pytest.skip("symlink privileges vary on Windows")
    artifacts = _ci_artifacts(tmp_path)
    receipt = tmp_path / "prepare.receipt"
    try:
        os.symlink(tmp_path / "missing-target", receipt)
    except OSError:
        pytest.skip("symlink creation unavailable")
    assert (
        ceremony_main(
            [
                "prepare",
                "--tag",
                TAG,
                "--ci-artifacts",
                str(artifacts),
                "--receipt-out",
                str(receipt),
            ]
        )
        == 1
    )
    assert receipt.is_symlink()


def test_prepare_deterministic_rerun(tmp_path: Path) -> None:
    artifacts = _ci_artifacts(tmp_path)
    a = prepare_ceremony(
        tag=TAG, ci_artifacts=artifacts, expected_source_revision=SOURCE_REV
    )
    b = prepare_ceremony(
        tag=TAG, ci_artifacts=artifacts, expected_source_revision=SOURCE_REV
    )
    assert a.evidence == b.evidence
    assert a.release_manifest == b.release_manifest
    assert a.publication_binding == b.publication_binding


def test_full_sign_ceremony_cli(tmp_path: Path, disposable_root) -> None:
    artifacts = _ci_artifacts(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")
    out = tmp_path / "signed"
    assert (
        ceremony_main(
            [
                "sign",
                "--tag",
                TAG,
                "--ci-artifacts",
                str(artifacts),
                "--private-key",
                str(disposable_root.private_key_path),
                "--public-key",
                str(pub),
                "--output",
                str(out),
                "--expected-source-revision",
                SOURCE_REV,
            ]
        )
        == 0
    )
    receipt = (out / f"ignite-kindle-v{VERSION}.signing-receipt.v1").read_text(
        encoding="ascii"
    )
    assert "publisher_signatures_verified=true" in receipt
    assert "production_publisher_authority=false" in receipt
    assert "production_promotion_ready=false" in receipt
    assert "preflight_trust_context=fixture" in receipt
    assert (out / PROMOTE_ASSETS_SUBDIR).is_dir()
    with pytest.raises(PreflightError):
        run_promote_gate(release_candidate_dir=out, tag=TAG)


def test_nested_ci_layout_accepted(tmp_path: Path, disposable_root) -> None:
    root = _nested_ci_artifacts(tmp_path)
    discovered = discover_ci_artifacts(root, tag=TAG)
    assert discovered.scriptlet.name.endswith(".sh")
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")
    out = tmp_path / "signed-nested"
    sign_ceremony(
        tag=TAG,
        ci_artifacts=root,
        private_key=disposable_root.private_key_path,
        public_key_path=pub,
        output_dir=out,
        expected_source_revision=SOURCE_REV,
    )
    assert out.is_dir()


def test_duplicate_scriptlet_refused(tmp_path: Path) -> None:
    artifacts = _ci_artifacts(tmp_path)
    nested = artifacts / "ignite-kindle-release"
    nested.mkdir()
    shutil.copy2(
        artifacts / f"Install Ignite v{VERSION}.sh",
        nested / f"Install Ignite v{VERSION}.sh",
    )
    with pytest.raises(CeremonyError, match="ambiguous Scriptlet"):
        discover_ci_artifacts(artifacts, tag=TAG)


def test_duplicate_measurement_refused(tmp_path: Path) -> None:
    artifacts = _ci_artifacts(tmp_path)
    nested = artifacts / "packaging-measurements"
    nested.mkdir()
    shutil.copy2(artifacts / MEASUREMENT_BASENAME, nested / MEASUREMENT_BASENAME)
    with pytest.raises(CeremonyError, match="ambiguous measurement"):
        discover_ci_artifacts(artifacts, tag=TAG)


def test_missing_checksum_refused(tmp_path: Path) -> None:
    artifacts = _ci_artifacts(tmp_path)
    (artifacts / f"Install Ignite v{VERSION}.sha256").unlink()
    with pytest.raises(CeremonyError, match="missing checksum"):
        discover_ci_artifacts(artifacts, tag=TAG)


def test_malformed_checksum_refused(tmp_path: Path) -> None:
    artifacts = _ci_artifacts(tmp_path)
    (artifacts / f"Install Ignite v{VERSION}.sha256").write_bytes(b"not-a-checksum\n")
    with pytest.raises(CeremonyError, match="checksum"):
        discover_ci_artifacts(artifacts, tag=TAG)


def test_inconsistent_checksum_refused(tmp_path: Path) -> None:
    artifacts = _ci_artifacts(tmp_path)
    (artifacts / f"Install Ignite v{VERSION}.sha256").write_bytes(
        f"{'ab' * 32}  Install Ignite v{VERSION}.sh\n".encode("ascii")
    )
    with pytest.raises(CeremonyError, match="checksum|digest"):
        prepare_ceremony(tag=TAG, ci_artifacts=artifacts)


def test_encrypted_pem_signing(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    artifacts = _ci_artifacts(tmp_path)
    passphrase = "unit-test-passphrase-not-a-secret"
    pem = _encrypted_pem(tmp_path / "enc-keys", passphrase=passphrase)
    pub = _pub_from_pem(pem, tmp_path / "enc.hex", passphrase=passphrase)
    monkeypatch.setenv("IGNITE_SIGNING_KEY_PASSPHRASE", passphrase)
    out = tmp_path / "signed-enc"
    written = sign_ceremony(
        tag=TAG,
        ci_artifacts=artifacts,
        private_key=pem,
        public_key_path=pub,
        output_dir=out,
        expected_source_revision=SOURCE_REV,
    )
    assert written["receipt"].is_file()
    assert passphrase.encode("ascii") not in written["receipt"].read_bytes()


def test_wrong_private_vs_public_key(tmp_path: Path, disposable_root) -> None:
    artifacts = _ci_artifacts(tmp_path)
    other = generate_test_trust_root(tmp_path / "other", label="other-3n")
    pub = _public_key_hex(other, tmp_path / "other.hex")
    with pytest.raises(CeremonyError, match="OpenSSL|verification|failed|signature"):
        sign_ceremony(
            tag=TAG,
            ci_artifacts=artifacts,
            private_key=disposable_root.private_key_path,
            public_key_path=pub,
            output_dir=tmp_path / "out",
            expected_source_revision=SOURCE_REV,
        )


def test_malformed_binding_refuses_before_signing(
    tmp_path: Path, disposable_root
) -> None:
    pub = disposable_root.public_key
    calls: list[str] = []

    class Boom:
        @staticmethod
        def sign_manifest(*_a, **_k):
            calls.append("sign")
            raise AssertionError("OpenSSL must not run for malformed binding")

    import production_trust_boundary as ptb

    monkey_sign = ptb._SIGN
    original = monkey_sign.sign_manifest
    monkey_sign.sign_manifest = Boom.sign_manifest  # type: ignore[method-assign]
    try:
        with pytest.raises(ProductionTrustError, match="binding|schema|eight|canonical"):
            sign_publication_binding(
                b"not-a-binding\n",
                private_key_path=disposable_root.private_key_path,
                public_key=pub,
            )
    finally:
        monkey_sign.sign_manifest = original  # type: ignore[method-assign]
    assert calls == []


def test_binding_digest_inconsistency_refused(
    tmp_path: Path, disposable_root
) -> None:
    artifacts = _ci_artifacts(tmp_path)
    prepared = prepare_ceremony(tag=TAG, ci_artifacts=artifacts)
    fields = parse_binding(prepared.publication_binding)
    # Flip evidence digest while keeping structure otherwise valid.
    bad_digest = "ff" * 32
    bad = prepared.publication_binding.replace(
        fields["publication_evidence_sha256"].encode("ascii"),
        bad_digest.encode("ascii"),
        1,
    )
    # Re-canonicalize by rebuilding lines with wrong digest then parse should fail
    # or digest relationship check fails when evidence supplied.
    with pytest.raises(ProductionTrustError, match="evidence|digest|canonical|binding"):
        sign_publication_binding(
            bad,
            private_key_path=disposable_root.private_key_path,
            public_key=disposable_root.public_key,
            release_manifest=prepared.release_manifest,
            publication_evidence=prepared.evidence,
        )


def test_sign_binding_cli_requires_context(tmp_path: Path, disposable_root) -> None:
    artifacts = _ci_artifacts(tmp_path)
    prepared = prepare_ceremony(tag=TAG, ci_artifacts=artifacts)
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")
    binding_path = tmp_path / "binding.bin"
    manifest_path = tmp_path / "manifest.bin"
    evidence_path = tmp_path / "evidence.bin"
    binding_path.write_bytes(prepared.publication_binding)
    manifest_path.write_bytes(prepared.release_manifest)
    evidence_path.write_bytes(prepared.evidence)
    out = tmp_path / "binding.sig"
    assert (
        ceremony_main(
            [
                "sign-binding",
                "--binding",
                str(binding_path),
                "--release-manifest",
                str(manifest_path),
                "--publication-evidence",
                str(evidence_path),
                "--private-key",
                str(disposable_root.private_key_path),
                "--public-key",
                str(pub),
                "--out",
                str(out),
            ]
        )
        == 0
    )


def test_scriptlet_mutation_after_prepare(tmp_path: Path, disposable_root) -> None:
    artifacts = _ci_artifacts(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")
    scriptlet = artifacts / f"Install Ignite v{VERSION}.sh"

    def mutate() -> None:
        scriptlet.write_bytes(scriptlet.read_bytes() + b"\n# after-prepare\n")

    with pytest.raises(CeremonyError, match="changed|identity|Scriptlet"):
        sign_ceremony(
            tag=TAG,
            ci_artifacts=artifacts,
            private_key=disposable_root.private_key_path,
            public_key_path=pub,
            output_dir=tmp_path / "out",
            expected_source_revision=SOURCE_REV,
            hooks=CeremonyHooks(after_prepare=mutate),
        )
    assert not (tmp_path / "out").exists()


def test_measurement_mutation_after_prepare(tmp_path: Path, disposable_root) -> None:
    artifacts = _ci_artifacts(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")
    meas = artifacts / MEASUREMENT_BASENAME

    def mutate() -> None:
        meas.write_bytes(meas.read_bytes() + b"\n")

    with pytest.raises(CeremonyError, match="changed|measurement"):
        sign_ceremony(
            tag=TAG,
            ci_artifacts=artifacts,
            private_key=disposable_root.private_key_path,
            public_key_path=pub,
            output_dir=tmp_path / "out",
            expected_source_revision=SOURCE_REV,
            hooks=CeremonyHooks(after_prepare=mutate),
        )
    assert not (tmp_path / "out").exists()


def test_scriptlet_replacement_after_signing_before_finalize(
    tmp_path: Path, disposable_root
) -> None:
    artifacts = _ci_artifacts(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")
    scriptlet = artifacts / f"Install Ignite v{VERSION}.sh"

    def mutate_source() -> None:
        scriptlet.write_bytes(scriptlet.read_bytes() + b"\n# after-sign\n")

    # Mutate source after both signatures; staged copy still uses original path
    # at copy time — hook after_binding_sign runs before copy, so copy sees
    # mutated bytes and must refuse identity mismatch.
    with pytest.raises(
        CeremonyError, match="scriptlet identity changed|staged Scriptlet|changed"
    ):
        sign_ceremony(
            tag=TAG,
            ci_artifacts=artifacts,
            private_key=disposable_root.private_key_path,
            public_key_path=pub,
            output_dir=tmp_path / "out",
            expected_source_revision=SOURCE_REV,
            hooks=CeremonyHooks(after_binding_sign=mutate_source),
        )
    assert not (tmp_path / "out").exists()


def test_second_signature_failure_cleans_up(tmp_path: Path, disposable_root) -> None:
    artifacts = _ci_artifacts(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")

    def fail_binding() -> None:
        raise CeremonyError("injected binding-sign failure")

    with pytest.raises(CeremonyError, match="injected binding-sign failure"):
        sign_ceremony(
            tag=TAG,
            ci_artifacts=artifacts,
            private_key=disposable_root.private_key_path,
            public_key_path=pub,
            output_dir=tmp_path / "out",
            expected_source_revision=SOURCE_REV,
            hooks=CeremonyHooks(before_binding_sign=fail_binding),
        )
    assert not (tmp_path / "out").exists()
    leftovers = list(tmp_path.glob(".ignite-kindle-ceremony-stage-*"))
    assert leftovers == []


def test_stage_write_failure_cleans_up(tmp_path: Path, disposable_root) -> None:
    artifacts = _ci_artifacts(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")

    def fail_stage(_staging: Path) -> None:
        raise CeremonyError("injected stage-write failure")

    with pytest.raises(CeremonyError, match="injected stage-write failure"):
        sign_ceremony(
            tag=TAG,
            ci_artifacts=artifacts,
            private_key=disposable_root.private_key_path,
            public_key_path=pub,
            output_dir=tmp_path / "out",
            expected_source_revision=SOURCE_REV,
            hooks=CeremonyHooks(after_stage_write=fail_stage),
        )
    assert not (tmp_path / "out").exists()
    assert list(tmp_path.glob(".ignite-kindle-ceremony-stage-*")) == []


def test_preflight_failure_cleans_up(tmp_path: Path, disposable_root) -> None:
    artifacts = _ci_artifacts(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")

    def break_staged(staging: Path) -> None:
        # Remove binding signature so preflight refuses.
        (staging / f"ignite-kindle-v{VERSION}.publication-binding.sig").unlink()

    with pytest.raises(CeremonyError, match="preflight"):
        sign_ceremony(
            tag=TAG,
            ci_artifacts=artifacts,
            private_key=disposable_root.private_key_path,
            public_key_path=pub,
            output_dir=tmp_path / "out",
            expected_source_revision=SOURCE_REV,
            hooks=CeremonyHooks(before_preflight=break_staged),
        )
    assert not (tmp_path / "out").exists()
    assert list(tmp_path.glob(".ignite-kindle-ceremony-stage-*")) == []


def test_existing_destination_refused(tmp_path: Path, disposable_root) -> None:
    artifacts = _ci_artifacts(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")
    out = tmp_path / "signed"
    out.mkdir()
    with pytest.raises(CeremonyError, match="unused|exists|overwrite|destination"):
        sign_ceremony(
            tag=TAG,
            ci_artifacts=artifacts,
            private_key=disposable_root.private_key_path,
            public_key_path=pub,
            output_dir=out,
            expected_source_revision=SOURCE_REV,
        )


def test_sign_binding_out_refuses_existing(
    tmp_path: Path, disposable_root
) -> None:
    artifacts = _ci_artifacts(tmp_path)
    prepared = prepare_ceremony(tag=TAG, ci_artifacts=artifacts)
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")
    binding_path = tmp_path / "binding.bin"
    manifest_path = tmp_path / "manifest.bin"
    evidence_path = tmp_path / "evidence.bin"
    binding_path.write_bytes(prepared.publication_binding)
    manifest_path.write_bytes(prepared.release_manifest)
    evidence_path.write_bytes(prepared.evidence)
    out = tmp_path / "binding.sig"
    out.write_bytes(b"occupied\n")
    assert (
        ceremony_main(
            [
                "sign-binding",
                "--binding",
                str(binding_path),
                "--release-manifest",
                str(manifest_path),
                "--publication-evidence",
                str(evidence_path),
                "--private-key",
                str(disposable_root.private_key_path),
                "--public-key",
                str(pub),
                "--out",
                str(out),
            ]
        )
        == 1
    )
    assert out.read_bytes() == b"occupied\n"


def test_sign_binding_refuses_oversized_binding(
    tmp_path: Path, disposable_root
) -> None:
    from production_trust_boundary import MAX_BINDING_BYTES

    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")
    binding_path = tmp_path / "binding.bin"
    manifest_path = tmp_path / "manifest.bin"
    evidence_path = tmp_path / "evidence.bin"
    binding_path.write_bytes(b"x" * (MAX_BINDING_BYTES + 1))
    manifest_path.write_bytes(b"schema=ignite-kindle-release-v1\n")
    evidence_path.write_bytes(b"evidence\n")
    out = tmp_path / "binding.sig"
    assert (
        ceremony_main(
            [
                "sign-binding",
                "--binding",
                str(binding_path),
                "--release-manifest",
                str(manifest_path),
                "--publication-evidence",
                str(evidence_path),
                "--private-key",
                str(disposable_root.private_key_path),
                "--public-key",
                str(pub),
                "--out",
                str(out),
            ]
        )
        == 1
    )
    assert not out.exists()


def test_ceremony_with_isolated_provisioned_pin(
    tmp_path: Path, disposable_root, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Caller key matching an isolated provisioned pin uses production verification."""
    import authenticated_publication_preflight as ap
    import production_trust_boundary as ptb

    pin = tmp_path / "isolated-publisher-pin.hex"
    pin.write_bytes(disposable_root.public_key.hex().encode("ascii") + b"\n")
    monkeypatch.setattr(ap, "_PRODUCTION_PIN", pin)
    monkeypatch.setattr(ptb, "_PRODUCTION_PIN", pin)

    artifacts = _ci_artifacts(tmp_path / "arts")
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")
    out = tmp_path / "signed-prod"
    sign_ceremony(
        tag=TAG,
        ci_artifacts=artifacts,
        private_key=disposable_root.private_key_path,
        public_key_path=pub,
        output_dir=out,
        expected_source_revision=SOURCE_REV,
    )
    receipt = (out / f"ignite-kindle-v{VERSION}.signing-receipt.v1").read_text(
        encoding="ascii"
    )
    assert "preflight_trust_context=production" in receipt
    assert "official_pin_matched_caller_public_key=true" in receipt
    assert "production_publisher_authority=true" in receipt
    assert "production_promotion_ready=false" in receipt
    assert "ceremony_success_is_not_production_promotion_readiness=true" in receipt
    assert "ci_provenance_established_by_this_ceremony=false" in receipt
    # Promote-gate crypto path works under the isolated pin; ceremony still did
    # not claim promotion readiness or CI provenance.
    gate = run_promote_gate(release_candidate_dir=out, tag=TAG)
    assert gate.production_promotion_ready is True
    _assert_repo_pin_untouched()


def test_alternate_key_with_isolated_provisioned_pin_no_authority(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Alternate caller key never establishes official publisher authority."""
    import authenticated_publication_preflight as ap
    import production_trust_boundary as ptb

    pin_root = generate_test_trust_root(tmp_path / "pin-keys", label="iso-pin")
    alt_root = generate_test_trust_root(tmp_path / "alt-keys", label="iso-alt")
    pin = tmp_path / "isolated-publisher-pin.hex"
    pin.write_bytes(pin_root.public_key.hex().encode("ascii") + b"\n")
    monkeypatch.setattr(ap, "_PRODUCTION_PIN", pin)
    monkeypatch.setattr(ptb, "_PRODUCTION_PIN", pin)

    artifacts = _ci_artifacts(tmp_path / "arts")
    pub = _public_key_hex(alt_root, tmp_path / "alt.hex")
    out = tmp_path / "signed-alt"
    sign_ceremony(
        tag=TAG,
        ci_artifacts=artifacts,
        private_key=alt_root.private_key_path,
        public_key_path=pub,
        output_dir=out,
        expected_source_revision=SOURCE_REV,
    )
    receipt = (out / f"ignite-kindle-v{VERSION}.signing-receipt.v1").read_text(
        encoding="ascii"
    )
    assert "preflight_trust_context=fixture" in receipt
    assert "production_publisher_authority=false" in receipt
    assert "caller_supplied_key_is_not_production_pin_authority=true" in receipt
    assert "production_promotion_ready=false" in receipt
    with pytest.raises(PreflightError):
        run_promote_gate(release_candidate_dir=out, tag=TAG)
    _assert_repo_pin_untouched()


def test_production_binding_signer_independent_of_pin(
    tmp_path: Path, disposable_root, monkeypatch: pytest.MonkeyPatch
) -> None:
    import production_trust_boundary as ptb

    fake_pin = tmp_path / "kindle-release-public-key.hex"
    fake_pin.write_bytes(("ab" * 32).encode("ascii") + b"\n")
    monkeypatch.setattr(ptb, "_PRODUCTION_PIN", fake_pin)
    with pytest.raises(ProductionTrustError):
        ptb.assert_production_pin_unprovisioned()

    artifacts = _ci_artifacts(tmp_path / "arts")
    prepared = prepare_ceremony(tag=TAG, ci_artifacts=artifacts)
    envelope = sign_publication_binding(
        prepared.publication_binding,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
        release_manifest=prepared.release_manifest,
        publication_evidence=prepared.evidence,
    )
    assert b"signature=" in envelope
    _assert_repo_pin_untouched()


def test_tools_release_entry_point(tmp_path: Path, disposable_root) -> None:
    artifacts = _ci_artifacts(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")
    out = tmp_path / "signed"
    script = _RELEASE / "ignite_kindle_offline_sign.py"
    completed = subprocess.run(
        [
            sys.executable,
            os.fspath(script),
            "sign",
            "--tag",
            TAG,
            "--ci-artifacts",
            str(artifacts),
            "--private-key",
            str(disposable_root.private_key_path),
            "--public-key",
            str(pub),
            "--output",
            str(out),
            "--expected-source-revision",
            SOURCE_REV,
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert (out / f"ignite-kindle-v{VERSION}.signing-receipt.v1").is_file()


def test_rust_interop_from_ceremony(tmp_path: Path, disposable_root) -> None:
    artifacts = _ci_artifacts(tmp_path)
    pub = _public_key_hex(disposable_root, tmp_path / "pub.hex")
    out = tmp_path / "signed"
    sign_ceremony(
        tag=TAG,
        ci_artifacts=artifacts,
        private_key=disposable_root.private_key_path,
        public_key_path=pub,
        output_dir=out,
        expected_source_revision=SOURCE_REV,
    )
    cargo = shutil.which("cargo")
    if cargo is None:
        pytest.skip("cargo not available")
    interop = tmp_path / "interop"
    interop.mkdir()
    (interop / "release.manifest").write_bytes(
        (out / f"ignite-kindle-v{VERSION}.manifest").read_bytes()
    )
    (interop / "release.sig").write_bytes(
        (out / f"ignite-kindle-v{VERSION}.manifest.sig").read_bytes()
    )
    (interop / "binding").write_bytes(
        (out / f"ignite-kindle-v{VERSION}.publication-binding").read_bytes()
    )
    (interop / "binding.sig").write_bytes(
        (out / f"ignite-kindle-v{VERSION}.publication-binding.sig").read_bytes()
    )
    (interop / "evidence").write_bytes(
        (out / f"ignite-kindle-v{VERSION}.publication-evidence").read_bytes()
    )
    (interop / "public_key.hex").write_bytes(pub.read_bytes())
    (interop / "expected_tag").write_text(TAG + "\n", encoding="ascii")
    (interop / "scriptlet").write_bytes(
        (out / f"Install Ignite v{VERSION}.sh").read_bytes()
    )
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


@pytest.mark.skipif(sys.platform == "win32", reason="symlink privileges vary on Windows")
def test_symlink_scriptlet_refused(tmp_path: Path) -> None:
    artifacts = _ci_artifacts(tmp_path)
    real = artifacts / f"Install Ignite v{VERSION}.sh"
    alias_dir = artifacts / "alias"
    alias_dir.mkdir()
    link = alias_dir / f"Install Ignite v{VERSION}.sh"
    try:
        os.symlink(real, link)
    except OSError:
        pytest.skip("symlink creation unavailable")
    with pytest.raises(CeremonyError, match="symlink|ambiguous"):
        discover_ci_artifacts(artifacts, tag=TAG)
