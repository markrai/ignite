"""Production publication evidence + release-asset regressions (Slice 3D-B2a.4.3h)."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parents[1]
_SFX = _HERE / "sfx"
for extra in (_HERE, _SFX):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from authenticated_publication_evidence import (  # noqa: E402
    build_publication_evidence_manifest as build_3f_evidence,
    generate_test_trust_root,
)
from authenticated_release_assets import (  # noqa: E402
    FIXTURE_AUTH_SUBDIR,
    AuthenticatedAssetError,
    assert_scriptlet_unchanged,
    build_authenticated_release_assets,
    build_release_v1_from_identity,
    measure_scriptlet,
    release_asset_basenames,
    write_authenticated_release_assets,
)
from package_release import PackageError, package  # noqa: E402
from test_package_release import _dropbear_dir, _pinned  # noqa: E402
from test_validate_stage import _write_arm32le_elf  # noqa: E402
from validate_stage import installer_sfx_name  # noqa: E402
from production_publication_evidence import (  # noqa: E402
    EVIDENCE_HEADER,
    PRODUCTION_SCOPE,
    ComponentClaim,
    ProductionEvidenceError,
    build_component_contract_bytes,
    build_production_evidence_from_claims,
    build_production_publication_evidence,
    parse_and_validate_production_evidence,
)
from production_trust_boundary import (  # noqa: E402
    EvidenceClaimStatus,
    ProductionTrustError,
    VerificationProvenance,
    authenticate_official_publication_chain,
    authenticate_publication_chain_with_key,
    build_publication_binding,
    production_trust_report,
    refuse_for_production_publication_evidence,
    sign_binding_for_test,
    sign_release_for_test,
)
from release_component_profiles import generate_profiles  # noqa: E402
from installer_publication_rehearsal import distinguish_payload_bytes  # noqa: E402
from test_validate_stage import make_payload, write_sfx  # noqa: E402

VERSION = "3.3.1"
TAG = f"v{VERSION}"


def _build_fixture(tmp_path: Path, *, tag: bytes = b"RELEASE-B"):
    payload = make_payload(tmp_path / "src", VERSION)
    distinguish_payload_bytes(payload, tag)
    scriptlet = tmp_path / f"Install Ignite v{VERSION}.sh"
    write_sfx(payload, scriptlet, VERSION)
    profiles = generate_profiles(payload, VERSION)
    return payload, scriptlet, profiles["package"]


@pytest.fixture
def disposable_root(tmp_path: Path):
    return generate_test_trust_root(tmp_path / "keys", label="slice-3h-publisher")


def test_contract_file_matches_python_authority() -> None:
    committed = (_HERE / "publication_component_contract.v1").read_bytes()
    assert committed == build_component_contract_bytes()
    assert b"role_count=22\n" in committed


def test_build_and_validate_production_evidence(tmp_path: Path) -> None:
    payload, scriptlet, _profile = _build_fixture(tmp_path)
    evidence = build_production_publication_evidence(
        version=VERSION, payload=payload, scriptlet=scriptlet
    )
    assert evidence.startswith(f"{EVIDENCE_HEADER}\n".encode("ascii"))
    assert f"publisher_authentication_scope={PRODUCTION_SCOPE}\n".encode() in evidence
    assert b"test-trust-root-only" not in evidence
    validated = parse_and_validate_production_evidence(evidence)
    assert validated.version == VERSION
    assert len(validated.components) == 22
    assert validated.components[0].role == "scribe-app"


def test_3f_test_evidence_is_not_production_evidence(tmp_path: Path) -> None:
    payload, scriptlet, _ = _build_fixture(tmp_path)
    three_f = build_3f_evidence(version=VERSION, payload=payload, scriptlet=scriptlet)
    assert b"test-trust-root-only" in three_f
    with pytest.raises(ProductionEvidenceError, match="test-trust-root-only"):
        parse_and_validate_production_evidence(three_f)
    with pytest.raises(ProductionTrustError):
        refuse_for_production_publication_evidence(three_f)


def test_fixture_signed_chain_validates_claims(tmp_path: Path, disposable_root) -> None:
    payload, scriptlet, profile = _build_fixture(tmp_path)
    assets = build_authenticated_release_assets(
        version=VERSION,
        payload=payload,
        scriptlet=scriptlet,
        package_profile=profile,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
    )
    auth = authenticate_publication_chain_with_key(
        release_manifest=assets.release_manifest,
        release_signature=assets.release_signature,
        binding_manifest=assets.publication_binding,
        binding_signature=assets.binding_signature,
        publication_evidence=assets.publication_evidence,
        expected_tag=TAG,
        public_key=disposable_root.public_key,
    )
    assert auth.provenance is VerificationProvenance.TEST_ONLY_FIXTURE_KEY
    assert auth.evidence_claims is EvidenceClaimStatus.PRODUCTION_CLAIMS_VALIDATED
    report = production_trust_report(auth)
    assert "verified-under-test-only-fixture-key" in report
    assert "verified-under-production-pin" not in report
    assert "publication_evidence_claims=validated" in report
    assert "promotion_authorized=false" in report
    assert "authorization=none" in report


def test_official_entry_still_refuses_unprovisioned(tmp_path: Path, disposable_root) -> None:
    payload, scriptlet, profile = _build_fixture(tmp_path)
    assets = build_authenticated_release_assets(
        version=VERSION,
        payload=payload,
        scriptlet=scriptlet,
        package_profile=profile,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
    )
    with pytest.raises(ProductionTrustError, match="not provisioned"):
        authenticate_official_publication_chain(
            release_manifest=assets.release_manifest,
            release_signature=assets.release_signature,
            binding_manifest=assets.publication_binding,
            binding_signature=assets.binding_signature,
            publication_evidence=assets.publication_evidence,
            expected_tag=TAG,
        )


def test_modified_scriptlet_digest_fails(tmp_path: Path, disposable_root) -> None:
    payload, scriptlet, profile = _build_fixture(tmp_path)
    assets = build_authenticated_release_assets(
        version=VERSION,
        payload=payload,
        scriptlet=scriptlet,
        package_profile=profile,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
    )
    # Tamper evidence scriptlet hash while keeping binding digest of original evidence.
    with pytest.raises(ProductionTrustError, match="publication_evidence_sha256"):
        authenticate_publication_chain_with_key(
            release_manifest=assets.release_manifest,
            release_signature=assets.release_signature,
            binding_manifest=assets.publication_binding,
            binding_signature=assets.binding_signature,
            publication_evidence=assets.publication_evidence + b"x",
            expected_tag=TAG,
            public_key=disposable_root.public_key,
        )


def test_invalid_classification_fails_closed(tmp_path: Path) -> None:
    payload, scriptlet, _ = _build_fixture(tmp_path)
    evidence = build_production_publication_evidence(
        version=VERSION, payload=payload, scriptlet=scriptlet
    )
    validated = parse_and_validate_production_evidence(evidence)
    bad_components = []
    for claim in validated.components:
        if claim.role == "scribe-app":
            bad_components.append(
                ComponentClaim(
                    role=claim.role,
                    presence=claim.presence,
                    size=claim.size,
                    sha256=claim.sha256,
                    classification="transformed",
                    mode=claim.mode,
                    mapping=claim.mapping,
                )
            )
        else:
            bad_components.append(claim)
    bad = build_production_evidence_from_claims(
        version=VERSION,
        scriptlet_bytes=validated.scriptlet_bytes,
        scriptlet_sha256=validated.scriptlet_sha256,
        package_profile_sha256=validated.package_profile_sha256,
        components=bad_components,
    )
    with pytest.raises(ProductionEvidenceError, match="classification"):
        parse_and_validate_production_evidence(bad)


def test_wrong_mode_and_mapping_fail(tmp_path: Path) -> None:
    payload, scriptlet, _ = _build_fixture(tmp_path)
    evidence = build_production_publication_evidence(
        version=VERSION, payload=payload, scriptlet=scriptlet
    )
    validated = parse_and_validate_production_evidence(evidence)

    def _rewrite(role: str, **kwargs) -> bytes:
        components = []
        for claim in validated.components:
            if claim.role == role:
                data = claim.__dict__.copy()
                data.update(kwargs)
                components.append(ComponentClaim(**data))
            else:
                components.append(claim)
        return build_production_evidence_from_claims(
            version=VERSION,
            scriptlet_bytes=validated.scriptlet_bytes,
            scriptlet_sha256=validated.scriptlet_sha256,
            package_profile_sha256=validated.package_profile_sha256,
            components=components,
        )

    with pytest.raises(ProductionEvidenceError, match="mode"):
        parse_and_validate_production_evidence(_rewrite("scribe-app", mode="0644"))
    with pytest.raises(ProductionEvidenceError, match="mapping"):
        parse_and_validate_production_evidence(
            _rewrite("scribe-app", mapping="APP/bin/wrong")
        )


def test_duplicate_or_reordered_role_fails(tmp_path: Path) -> None:
    payload, scriptlet, _ = _build_fixture(tmp_path)
    evidence = build_production_publication_evidence(
        version=VERSION, payload=payload, scriptlet=scriptlet
    )
    lines = evidence.decode("ascii").splitlines()
    # Swap two component lines (roles 0 and 1).
    i0 = 10
    i1 = 11
    lines[i0], lines[i1] = lines[i1], lines[i0]
    swapped = ("\n".join(lines) + "\n").encode("ascii")
    with pytest.raises(ProductionEvidenceError):
        parse_and_validate_production_evidence(swapped)


def test_wrong_key_and_missing_signature(tmp_path: Path, disposable_root) -> None:
    other = generate_test_trust_root(tmp_path / "other", label="other")
    payload, scriptlet, profile = _build_fixture(tmp_path)
    assets = build_authenticated_release_assets(
        version=VERSION,
        payload=payload,
        scriptlet=scriptlet,
        package_profile=profile,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
    )
    with pytest.raises(ProductionTrustError):
        authenticate_publication_chain_with_key(
            release_manifest=assets.release_manifest,
            release_signature=assets.release_signature,
            binding_manifest=assets.publication_binding,
            binding_signature=assets.binding_signature,
            publication_evidence=assets.publication_evidence,
            expected_tag=TAG,
            public_key=other.public_key,
        )
    with pytest.raises(ProductionTrustError, match="missing"):
        authenticate_publication_chain_with_key(
            release_manifest=assets.release_manifest,
            release_signature=assets.release_signature,
            binding_manifest=assets.publication_binding,
            binding_signature=b"",
            publication_evidence=assets.publication_evidence,
            expected_tag=TAG,
            public_key=disposable_root.public_key,
        )


def test_tag_mismatch_and_placeholder_refused(tmp_path: Path, disposable_root) -> None:
    payload, scriptlet, profile = _build_fixture(tmp_path)
    assets = build_authenticated_release_assets(
        version=VERSION,
        payload=payload,
        scriptlet=scriptlet,
        package_profile=profile,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
    )
    with pytest.raises(ProductionTrustError, match="tag"):
        authenticate_publication_chain_with_key(
            release_manifest=assets.release_manifest,
            release_signature=assets.release_signature,
            binding_manifest=assets.publication_binding,
            binding_signature=assets.binding_signature,
            publication_evidence=assets.publication_evidence,
            expected_tag="v9.9.9",
            public_key=disposable_root.public_key,
        )
    with pytest.raises(ProductionTrustError, match="placeholder"):
        refuse_for_production_publication_evidence(
            b"IGNITE_PUBLICATION_EVIDENCE_PROD_PLACEHOLDER_V1\nschema=1\n"
        )


def test_asset_filenames_match_conventions() -> None:
    names = release_asset_basenames(VERSION)
    assert names["release_manifest"] == f"ignite-kindle-v{VERSION}.manifest"
    assert names["publication_evidence"] == f"ignite-kindle-v{VERSION}.publication-evidence"
    assert names["publication_binding"] == f"ignite-kindle-v{VERSION}.publication-binding"
    assert names["binding_signature"].endswith(".publication-binding.sig")


def test_write_assets_refuses_overwrite(tmp_path: Path, disposable_root) -> None:
    payload, scriptlet, profile = _build_fixture(tmp_path)
    assets = build_authenticated_release_assets(
        version=VERSION,
        payload=payload,
        scriptlet=scriptlet,
        package_profile=profile,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
    )
    out = tmp_path / "out"
    write_authenticated_release_assets(assets, out)
    with pytest.raises(AuthenticatedAssetError, match="replace"):
        write_authenticated_release_assets(assets, out)


def _write_interop_fixture(
    fixture_dir: Path, assets, disposable_root, *, scriptlet_bytes: bytes | None = None
) -> None:
    fixture_dir.mkdir(parents=True, exist_ok=True)
    (fixture_dir / "release.manifest").write_bytes(assets.release_manifest)
    (fixture_dir / "release.sig").write_bytes(assets.release_signature)
    (fixture_dir / "binding").write_bytes(assets.publication_binding)
    (fixture_dir / "binding.sig").write_bytes(assets.binding_signature)
    (fixture_dir / "evidence").write_bytes(assets.publication_evidence)
    (fixture_dir / "public_key.hex").write_bytes(
        disposable_root.public_key.hex().encode("ascii") + b"\n"
    )
    (fixture_dir / "expected_tag").write_text(TAG + "\n", encoding="ascii")
    data = (
        scriptlet_bytes
        if scriptlet_bytes is not None
        else assets.scriptlet_path.read_bytes()
    )
    (fixture_dir / "scriptlet").write_bytes(data)


def _run_rust_interop(
    fixture_dir: Path, *, test_filter: str, expect: str | None = None
) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env["IGNITE_PUBLICATION_INTEROP_DIR"] = str(fixture_dir)
    if expect is not None:
        env["IGNITE_PUBLICATION_INTEROP_EXPECT"] = expect
    elif "IGNITE_PUBLICATION_INTEROP_EXPECT" in env:
        del env["IGNITE_PUBLICATION_INTEROP_EXPECT"]
    return subprocess.run(
        [
            "cargo",
            "test",
            "-p",
            "scribe-app",
            test_filter,
            "--lib",
            "--",
            "--nocapture",
        ],
        cwd=str(_REPO / "ignite" / "kindle" / "scribe-app"),
        env=env,
        check=False,
        capture_output=True,
        text=True,
    )


def test_host_rust_interoperability(tmp_path: Path, disposable_root) -> None:
    """Host builds exact bytes; Rust verifies metadata and the actual Scriptlet."""
    payload, scriptlet, profile = _build_fixture(tmp_path)
    assets = build_authenticated_release_assets(
        version=VERSION,
        payload=payload,
        scriptlet=scriptlet,
        package_profile=profile,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
    )
    fixture_dir = tmp_path / "interop"
    _write_interop_fixture(fixture_dir, assets, disposable_root)

    auth = authenticate_publication_chain_with_key(
        release_manifest=assets.release_manifest,
        release_signature=assets.release_signature,
        binding_manifest=assets.publication_binding,
        binding_signature=assets.binding_signature,
        publication_evidence=assets.publication_evidence,
        expected_tag=TAG,
        public_key=disposable_root.public_key,
    )
    assert auth.evidence_claims is EvidenceClaimStatus.PRODUCTION_CLAIMS_VALIDATED

    completed = _run_rust_interop(fixture_dir, test_filter="publication_interop_host_bytes")
    if completed.returncode != 0:
        pytest.fail(
            "Rust interop failed:\n" + completed.stdout + "\n" + completed.stderr
        )


def test_rust_rejects_modified_and_truncated_scriptlet(
    tmp_path: Path, disposable_root
) -> None:
    payload, scriptlet, profile = _build_fixture(tmp_path)
    assets = build_authenticated_release_assets(
        version=VERSION,
        payload=payload,
        scriptlet=scriptlet,
        package_profile=profile,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
    )
    original = scriptlet.read_bytes()

    modified_dir = tmp_path / "interop-modified"
    _write_interop_fixture(
        modified_dir,
        assets,
        disposable_root,
        scriptlet_bytes=original[:-1] + bytes([original[-1] ^ 0x01]),
    )
    completed = _run_rust_interop(
        modified_dir,
        test_filter="publication_interop_modified_scriptlet_fails",
        expect="modified-scriptlet",
    )
    if completed.returncode != 0:
        pytest.fail(
            "modified Scriptlet interop failed unexpectedly:\n"
            + completed.stdout
            + "\n"
            + completed.stderr
        )

    truncated_dir = tmp_path / "interop-truncated"
    _write_interop_fixture(
        truncated_dir,
        assets,
        disposable_root,
        scriptlet_bytes=original[:-16],
    )
    completed = _run_rust_interop(
        truncated_dir,
        test_filter="publication_interop_modified_scriptlet_fails",
        expect="modified-scriptlet",
    )
    if completed.returncode != 0:
        pytest.fail(
            "truncated Scriptlet interop failed unexpectedly:\n"
            + completed.stdout
            + "\n"
            + completed.stderr
        )


def test_mandatory_direct_copy_absence_fails_even_when_signed(
    tmp_path: Path, disposable_root
) -> None:
    payload, scriptlet, profile = _build_fixture(tmp_path)
    good = build_production_publication_evidence(
        version=VERSION, payload=payload, scriptlet=scriptlet
    )
    validated = parse_and_validate_production_evidence(good)
    # Craft a complete evidence document with scribe-app absent (bypassing builder).
    lines = good.decode("ascii").splitlines()
    for idx, line in enumerate(lines):
        if line.startswith("component=scribe-app|"):
            lines[idx] = (
                "component=scribe-app|absent|0|-|direct-copy|-|APP/bin/scribe-app"
            )
            break
    else:
        raise AssertionError("scribe-app component line missing")
    bad_evidence = ("\n".join(lines) + "\n").encode("ascii")
    with pytest.raises(ProductionEvidenceError, match="mandatory"):
        parse_and_validate_production_evidence(bad_evidence)

    identity = measure_scriptlet(scriptlet, expected_name=scriptlet.name)
    release = build_release_v1_from_identity(version=VERSION, identity=identity)
    binding = build_publication_binding(
        release_tag=TAG,
        release_version=VERSION,
        release_manifest=release,
        scriptlet_sha256=identity.sha256,
        publication_evidence=bad_evidence,
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
    with pytest.raises(ProductionTrustError, match="mandatory|claims invalid"):
        authenticate_publication_chain_with_key(
            release_manifest=release,
            release_signature=release_sig,
            binding_manifest=binding,
            binding_signature=binding_sig,
            publication_evidence=bad_evidence,
            expected_tag=TAG,
            public_key=disposable_root.public_key,
        )
    del validated, profile


def test_scriptlet_mutation_during_asset_generation_refused(
    tmp_path: Path, disposable_root, monkeypatch: pytest.MonkeyPatch
) -> None:
    payload, scriptlet, profile = _build_fixture(tmp_path)
    real_assert = assert_scriptlet_unchanged
    state = {"mutated": False}

    def mutate_then_assert(identity):
        if not state["mutated"]:
            state["mutated"] = True
            identity.path.write_bytes(identity.path.read_bytes() + b"\nMUTATED\n")
        return real_assert(identity)

    monkeypatch.setattr(
        "authenticated_release_assets.assert_scriptlet_unchanged", mutate_then_assert
    )
    with pytest.raises(AuthenticatedAssetError, match="identity changed"):
        build_authenticated_release_assets(
            version=VERSION,
            payload=payload,
            scriptlet=scriptlet,
            package_profile=profile,
            private_key_path=disposable_root.private_key_path,
            public_key=disposable_root.public_key,
        )


def _package_stub_inputs(tmp_path: Path) -> tuple[Path, Path, Path]:
    """Lightweight bins/dropbear/PINNED fixtures used by test_package_release."""
    bins = tmp_path / "bins"
    bins.mkdir(parents=True)
    _write_arm32le_elf(bins / "scribe-app", size=4096)
    _write_arm32le_elf(bins / "scribe-lifecycle", size=4096)
    drop = _dropbear_dir(tmp_path)
    pin = _pinned(tmp_path / "PINNED.txt", drop / "dropbearmulti")
    return bins, drop, pin


def test_package_release_fixture_workflow_integration(
    tmp_path: Path, disposable_root
) -> None:
    """Exercise package_release.package() fixture-auth path end-to-end."""
    import package_release as pr

    bins, drop, pin = _package_stub_inputs(tmp_path / "pkg-inputs")
    pub_path = tmp_path / "fixture-public.hex"
    pub_path.write_bytes(disposable_root.public_key.hex().encode("ascii") + b"\n")

    # Ordinary packaging: no fixture keys → no authenticated asset directory.
    out_plain = tmp_path / "out-plain"
    plain = package(
        repo=_REPO,
        version=VERSION,
        release_bin_dir=bins,
        dropbear_dir=drop,
        out_root=out_plain,
        pinned=pin,
        support_zip=False,
    )
    assert not (out_plain / FIXTURE_AUTH_SUBDIR).exists()
    assert "fixture_authenticated_assets_dir" not in plain
    plain_scriptlet = Path(str(plain["sfx"]))
    plain_checksum = Path(str(plain["sfx_checksum"]))
    plain_scriptlet_bytes = plain_scriptlet.read_bytes()
    plain_checksum_text = plain_checksum.read_text(encoding="utf-8")
    assert plain_scriptlet.name == installer_sfx_name(VERSION)

    # One fixture key only → fail closed through the actual packaging entry point.
    with pytest.raises(PackageError, match="both"):
        package(
            repo=_REPO,
            version=VERSION,
            release_bin_dir=bins,
            dropbear_dir=drop,
            out_root=tmp_path / "out-one-key",
            pinned=pin,
            support_zip=False,
            fixture_signing_private_key=disposable_root.private_key_path,
            fixture_signing_public_key=None,
        )

    # Both disposable keys → authenticated metadata under fixture-authenticated-assets/.
    out_auth = tmp_path / "out-auth"
    auth = package(
        repo=_REPO,
        version=VERSION,
        release_bin_dir=bins,
        dropbear_dir=drop,
        out_root=out_auth,
        pinned=pin,
        support_zip=False,
        fixture_signing_private_key=disposable_root.private_key_path,
        fixture_signing_public_key=pub_path,
    )
    fixture_dir = out_auth / FIXTURE_AUTH_SUBDIR
    assert Path(str(auth["fixture_authenticated_assets_dir"])) == fixture_dir
    names = release_asset_basenames(VERSION)
    for key in (
        "release_manifest",
        "release_signature",
        "publication_evidence",
        "publication_binding",
        "binding_signature",
    ):
        assert (fixture_dir / names[key]).is_file(), names[key]

    # Customer Scriptlet + checksum remain ordinary packaging outputs (unchanged by signing).
    auth_scriptlet = Path(str(auth["sfx"]))
    auth_checksum = Path(str(auth["sfx_checksum"]))
    assert auth_scriptlet.name == installer_sfx_name(VERSION)
    assert auth_scriptlet.read_bytes() == plain_scriptlet_bytes
    assert auth["sfx_sha256"] == plain["sfx_sha256"]
    assert auth_checksum.read_text(encoding="utf-8") == plain_checksum_text
    checksum_parts = auth_checksum.read_text(encoding="utf-8").split(None, 1)
    assert checksum_parts[0] == auth["sfx_sha256"]
    assert checksum_parts[1].strip() == auth_scriptlet.name

    # CLI accepts one key without the other, but package() refuses incomplete pairs.
    ns = pr.parse_args(
        [
            "--fixture-signing-private-key",
            str(disposable_root.private_key_path),
        ]
    )
    assert ns.fixture_signing_private_key is not None
    assert ns.fixture_signing_public_key is None


def test_promotion_not_authorized_after_fixture_success(
    tmp_path: Path, disposable_root
) -> None:
    payload, scriptlet, profile = _build_fixture(tmp_path)
    assets = build_authenticated_release_assets(
        version=VERSION,
        payload=payload,
        scriptlet=scriptlet,
        package_profile=profile,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
    )
    auth = authenticate_publication_chain_with_key(
        release_manifest=assets.release_manifest,
        release_signature=assets.release_signature,
        binding_manifest=assets.publication_binding,
        binding_signature=assets.binding_signature,
        publication_evidence=assets.publication_evidence,
        expected_tag=TAG,
        public_key=disposable_root.public_key,
    )
    report = production_trust_report(auth)
    assert "promotion_authorized=false" in report
    assert "installation_binding=unverified" in report
    assert "first_launch_admission=unavailable" in report
    assert auth.provenance is VerificationProvenance.TEST_ONLY_FIXTURE_KEY
