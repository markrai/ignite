"""Authenticated publication preflight regressions (Slice 3D-B2a.4.3k corrections)."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parents[1]
_SFX = _HERE / "sfx"
_RELEASE = _REPO / "tools" / "release"
for extra in (_HERE, _SFX, str(_RELEASE)):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from authenticated_publication_evidence import generate_test_trust_root  # noqa: E402
from authenticated_publication_preflight import (  # noqa: E402
    MAX_EVIDENCE_BYTES,
    PreflightError,
    TrustContext,
    main as preflight_main,
    run_preflight,
    run_promote_gate,
)
from authenticated_release_assets import (  # noqa: E402
    build_authenticated_release_assets,
    write_authenticated_release_assets,
)
from production_trust_boundary import assert_production_pin_unprovisioned  # noqa: E402
from release_component_profiles import generate_profiles  # noqa: E402
from test_validate_stage import make_payload, write_sfx  # noqa: E402

VERSION = "3.3.1"
TAG = f"v{VERSION}"


def _public_key_hex(root, path: Path) -> Path:
    path.write_bytes(root.public_key.hex().encode("ascii") + b"\n")
    return path


def _build_bundle(tmp_path: Path, root):
    payload = make_payload(tmp_path / "src", VERSION)
    scriptlet = tmp_path / f"Install Ignite v{VERSION}.sh"
    write_sfx(payload, scriptlet, VERSION)
    profiles = generate_profiles(payload, VERSION)
    assets = build_authenticated_release_assets(
        version=VERSION,
        payload=payload,
        scriptlet=scriptlet,
        package_profile=profiles["package"],
        private_key_path=root.private_key_path,
        public_key=root.public_key,
        require_sfx_equivalence=True,
    )
    out = tmp_path / "bundle"
    out.mkdir(parents=True, exist_ok=True)
    write_authenticated_release_assets(assets, out)
    shutil.copy2(scriptlet, out / scriptlet.name)
    return out, scriptlet, assets


@pytest.fixture
def disposable_root(tmp_path: Path):
    assert_production_pin_unprovisioned()
    return generate_test_trust_root(tmp_path / "keys", label="slice-3k-preflight")


def test_valid_fixture_bundle_rehearsal_succeeds(
    tmp_path: Path, disposable_root
) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    report = run_preflight(
        assets_dir=bundle,
        expected_tag=TAG,
        trust=TrustContext.FIXTURE,
        public_key_path=pub,
        source_revision="a" * 40,
        expected_workflow=".github/workflows/ci.yml",
    )
    assert report.fixture_rehearsal_ready is True
    assert report.production_promotion_ready is False
    assert report.publisher_authenticated is True
    assert report.verification_provenance == "verified-under-test-only-fixture-key"
    lines = "\n".join(report.summary_lines())
    assert "fixture_success_is_not_production_promotion_readiness=true" in lines
    assert "operator_asserted_source_revision_is_not_ci_provenance_proof=true" in lines
    assert "ci_provenance_verified_by_preflight=false" in lines


def test_fixture_bundle_production_gate_refuses(
    tmp_path: Path, disposable_root
) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    with pytest.raises(PreflightError, match="UNPROVISIONED|unavailable|production"):
        run_promote_gate(release_candidate_dir=bundle, tag=TAG)


def test_alternate_public_key_not_accepted_by_promote_gate_cli(
    tmp_path: Path, disposable_root
) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    with pytest.raises(SystemExit) as excinfo:
        preflight_main(
            [
                "promote-gate",
                "--release-candidate-dir",
                str(bundle),
                "--tag",
                TAG,
                "--public-key-file",
                str(pub),
            ]
        )
    assert excinfo.value.code != 0


def test_production_trust_refuses_unprovisioned(
    tmp_path: Path, disposable_root
) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    with pytest.raises(PreflightError, match="UNPROVISIONED|unavailable"):
        run_preflight(
            assets_dir=bundle,
            expected_tag=TAG,
            trust=TrustContext.PRODUCTION,
        )


def test_missing_manifest_refused(tmp_path: Path, disposable_root) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    (bundle / f"ignite-kindle-v{VERSION}.manifest").unlink()
    with pytest.raises(PreflightError, match="missing required"):
        run_preflight(
            assets_dir=bundle,
            expected_tag=TAG,
            trust=TrustContext.FIXTURE,
            public_key_path=pub,
        )


def test_missing_evidence_refused(tmp_path: Path, disposable_root) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    (bundle / f"ignite-kindle-v{VERSION}.publication-evidence").unlink()
    with pytest.raises(PreflightError, match="missing required"):
        run_preflight(
            assets_dir=bundle,
            expected_tag=TAG,
            trust=TrustContext.FIXTURE,
            public_key_path=pub,
        )


def test_missing_binding_refused(tmp_path: Path, disposable_root) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    (bundle / f"ignite-kindle-v{VERSION}.publication-binding").unlink()
    with pytest.raises(PreflightError, match="missing required"):
        run_preflight(
            assets_dir=bundle,
            expected_tag=TAG,
            trust=TrustContext.FIXTURE,
            public_key_path=pub,
        )


def test_modified_scriptlet_refused(tmp_path: Path, disposable_root) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    scriptlet = bundle / f"Install Ignite v{VERSION}.sh"
    scriptlet.write_bytes(scriptlet.read_bytes() + b"\n#tamper\n")
    with pytest.raises(PreflightError, match="does not match Scriptlet|identity"):
        run_preflight(
            assets_dir=bundle,
            expected_tag=TAG,
            trust=TrustContext.FIXTURE,
            public_key_path=pub,
        )


def test_wrong_tag_refused(tmp_path: Path, disposable_root) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    with pytest.raises(PreflightError):
        run_preflight(
            assets_dir=bundle,
            expected_tag="v9.9.9",
            trust=TrustContext.FIXTURE,
            public_key_path=pub,
        )


def test_mismatched_publisher_keys_refused(tmp_path: Path, disposable_root) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    other = generate_test_trust_root(tmp_path / "other-keys", label="other-publisher")
    other_pub = _public_key_hex(other, tmp_path / "other.hex")
    with pytest.raises(PreflightError, match="signature"):
        run_preflight(
            assets_dir=bundle,
            expected_tag=TAG,
            trust=TrustContext.FIXTURE,
            public_key_path=other_pub,
        )


def test_truncated_signature_refused(tmp_path: Path, disposable_root) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    sig = bundle / f"ignite-kindle-v{VERSION}.manifest.sig"
    sig.write_bytes(b"schema=ignite-kindle-release-signature-v1\nsignature=00\n")
    with pytest.raises(PreflightError):
        run_preflight(
            assets_dir=bundle,
            expected_tag=TAG,
            trust=TrustContext.FIXTURE,
            public_key_path=pub,
        )


def test_incorrect_component_claims_refused(tmp_path: Path, disposable_root) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    evidence = bundle / f"ignite-kindle-v{VERSION}.publication-evidence"
    raw = evidence.read_bytes().replace(
        b"component_role_count=22", b"component_role_count=21"
    )
    evidence.write_bytes(raw)
    with pytest.raises(PreflightError):
        run_preflight(
            assets_dir=bundle,
            expected_tag=TAG,
            trust=TrustContext.FIXTURE,
            public_key_path=pub,
        )


def test_oversized_evidence_refused_without_full_read(
    tmp_path: Path, disposable_root
) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    evidence = bundle / f"ignite-kindle-v{VERSION}.publication-evidence"
    # Write just over the bound without needing huge RAM if streaming refuses early.
    evidence.write_bytes(b"x" * (MAX_EVIDENCE_BYTES + 1))
    with pytest.raises(PreflightError, match="oversized"):
        run_preflight(
            assets_dir=bundle,
            expected_tag=TAG,
            trust=TrustContext.FIXTURE,
            public_key_path=pub,
        )


def test_symlink_metadata_refused(tmp_path: Path, disposable_root) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    evidence = bundle / f"ignite-kindle-v{VERSION}.publication-evidence"
    target = tmp_path / "outside-evidence"
    target.write_bytes(evidence.read_bytes())
    evidence.unlink()
    try:
        evidence.symlink_to(target)
    except OSError:
        pytest.skip("symlink creation unavailable on this host")
    with pytest.raises(PreflightError, match="symlink"):
        run_preflight(
            assets_dir=bundle,
            expected_tag=TAG,
            trust=TrustContext.FIXTURE,
            public_key_path=pub,
        )


def test_digest_describes_authenticated_bytes_not_later_reread(
    tmp_path: Path, disposable_root
) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    evidence = bundle / f"ignite-kindle-v{VERSION}.publication-evidence"
    before = evidence.read_bytes()
    report = run_preflight(
        assets_dir=bundle,
        expected_tag=TAG,
        trust=TrustContext.FIXTURE,
        public_key_path=pub,
    )
    listed = next(a for a in report.assets if a.name.endswith("publication-evidence"))
    import hashlib

    assert listed.sha256 == hashlib.sha256(before).hexdigest()
    # Changing the file after authentication must not alter the prior report.
    evidence.write_bytes(before + b"\n#after\n")
    assert listed.sha256 == hashlib.sha256(before).hexdigest()


def test_rehearsal_rerun_is_deterministic(tmp_path: Path, disposable_root) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    first = run_preflight(
        assets_dir=bundle,
        expected_tag=TAG,
        trust=TrustContext.FIXTURE,
        public_key_path=pub,
        source_revision="c" * 40,
    )
    second = run_preflight(
        assets_dir=bundle,
        expected_tag=TAG,
        trust=TrustContext.FIXTURE,
        public_key_path=pub,
        source_revision="c" * 40,
    )
    assert first.summary_lines() == second.summary_lines()


def test_cli_production_status_reports_unavailable() -> None:
    rc = preflight_main(["production-status"])
    assert rc == 0


def test_ignite_promote_preflight_env_rejected_by_gate(
    tmp_path: Path, disposable_root, monkeypatch: pytest.MonkeyPatch
) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    monkeypatch.setenv(
        "IGNITE_PROMOTE_PREFLIGHT",
        str(_HERE / "authenticated_publication_preflight.py"),
    )
    with pytest.raises(PreflightError, match="IGNITE_PROMOTE_PREFLIGHT"):
        run_promote_gate(release_candidate_dir=bundle, tag=TAG)


def test_end_to_end_fixture_bundle_matches_rust_interop(
    tmp_path: Path, disposable_root
) -> None:
    bundle, scriptlet, assets = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    report = run_preflight(
        assets_dir=bundle,
        expected_tag=TAG,
        trust=TrustContext.FIXTURE,
        public_key_path=pub,
    )
    assert report.fixture_rehearsal_ready is True
    assert report.production_promotion_ready is False

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

    cargo = shutil.which("cargo")
    if cargo is None:
        pytest.skip("cargo not available for Rust interop")
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


def test_refused_cases_perform_no_github_mutation(
    tmp_path: Path, disposable_root
) -> None:
    bundle, _, _ = _build_bundle(tmp_path, disposable_root)
    pub = _public_key_hex(disposable_root, tmp_path / "fixture.hex")
    (bundle / f"ignite-kindle-v{VERSION}.manifest").unlink()
    import authenticated_publication_preflight as mod

    assert not hasattr(mod, "gh")
    with pytest.raises(PreflightError):
        run_preflight(
            assets_dir=bundle,
            expected_tag=TAG,
            trust=TrustContext.FIXTURE,
            public_key_path=pub,
        )


def test_fixture_emitter_is_not_under_tools_release() -> None:
    assert not (
        _REPO / "tools" / "release" / "emit_offline_authenticated_metadata.py"
    ).exists()
    assert (_HERE / "test_emit_fixture_offline_metadata.py").is_file()
