"""Authenticated publication evidence regressions (Slice 3D-B2a.4.3f)."""

from __future__ import annotations

import hashlib
import os
import stat
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
_SFX = _HERE / "sfx"
for extra in (_HERE, _SFX):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from authenticated_publication_evidence import (  # noqa: E402
    AuthEvidenceError,
    PublicationTestTrustRoot,
    authenticate_publication_evidence,
    build_publication_evidence_manifest,
    consume_regular_file_fd,
    generate_test_trust_root,
    observe_authenticated_publication,
    parse_signature_envelope,
    release_evidence_from_authenticated,
    sign_publication_evidence,
    signature_envelope,
    validate_release_evidence_against_authenticated,
    verify_installed_b,
    verify_package_against_authenticated,
    verify_retained_a,
    verify_scriptlet_identity,
)
from build_sfx import build_sfx, default_template  # noqa: E402
from installer_publication_rehearsal import (  # noqa: E402
    ReleaseEvidence,
    build_release_evidence,
    distinguish_payload_bytes,
    init_fixture_root,
)
from test_install_runtime import _run_install  # noqa: E402
from test_sfx import _require_sh  # noqa: E402
from test_validate_stage import make_payload  # noqa: E402

VERSION_A = "3.3.0"
VERSION_B = "3.3.1"


def _require_posix_installer() -> None:
    try:
        _require_sh()
    except AssertionError as exc:
        pytest.skip(str(exc))
    script = _HERE / "lib" / "install-runtime.sh"
    if b"\r" in script.read_bytes()[:64]:
        pytest.skip(
            "install-runtime.sh has CRLF line endings on this filesystem view; "
            "use Git Bash on Windows or a native LF Linux checkout"
        )


def _build_release(tmp: Path, version: str, tag: bytes) -> tuple[Path, Path, object, bytes, bytes]:
    payload = make_payload(tmp / f"src-{version}", version)
    distinguish_payload_bytes(payload, tag)
    scriptlet = tmp / f"Install Ignite v{version}.sh"
    scriptlet.write_bytes(build_sfx(payload, version, default_template()))
    evidence = build_release_evidence(payload, version)
    manifest = build_publication_evidence_manifest(
        version=version, payload=payload, scriptlet=scriptlet
    )
    return payload, scriptlet, evidence, manifest, scriptlet.read_bytes()


@pytest.fixture(scope="module")
def trust_root(tmp_path_factory: pytest.TempPathFactory) -> PublicationTestTrustRoot:
    root = tmp_path_factory.mktemp("pub-ev-trust")
    return generate_test_trust_root(root, label="slice-3f-test-publisher")


def test_production_pin_remains_unprovisioned() -> None:
    pin = _HERE.parents[1] / "tools" / "release" / "kindle-release-public-key.hex"
    assert pin.read_bytes() == b"UNPROVISIONED\n"


def test_valid_signed_a_and_b_disposable_install(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    _require_posix_installer()
    payload_a, scriptlet_a, evidence_a, manifest_a, _ = _build_release(
        tmp_path / "a", VERSION_A, b"RELEASE-A"
    )
    payload_b, scriptlet_b, evidence_b, manifest_b, _ = _build_release(
        tmp_path / "b", VERSION_B, b"RELEASE-B"
    )
    sig_a = sign_publication_evidence(manifest_a, trust_root)
    sig_b = sign_publication_evidence(manifest_b, trust_root)
    auth_a = authenticate_publication_evidence(manifest_a, sig_a, trust_root)
    auth_b = authenticate_publication_evidence(manifest_b, sig_b, trust_root)
    verify_package_against_authenticated(auth_a, payload=payload_a, scriptlet=scriptlet_a)
    verify_package_against_authenticated(auth_b, payload=payload_b, scriptlet=scriptlet_b)

    owned = init_fixture_root(tmp_path / "kindle")
    kindle = owned.path
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    assert (
        verify_installed_b(auth_b, kindle, evidence_a, evidence_b, auth_a=auth_a) == []
    )
    assert (
        verify_retained_a(auth_a, kindle, evidence_a, evidence_b, auth_b=auth_b) == []
    )

    report = observe_authenticated_publication(
        trust_root=trust_root,
        manifest_a=manifest_a,
        signature_a=sig_a,
        manifest_b=manifest_b,
        signature_b=sig_b,
        scriptlet_a=scriptlet_a,
        scriptlet_b=scriptlet_b,
        payload_a=payload_a,
        payload_b=payload_b,
        kindle=kindle,
        evidence_a=evidence_a,
        evidence_b=evidence_b,
    )
    text = report.decode("ascii")
    assert "publisher_authentication=unavailable" in text
    assert "promotion_authorized=false" in text
    assert "installed_b=match" in text
    assert "retained_a=match" in text
    assert "signature_status=verified-under-test-trust-root:slice-3f-test-publisher" in text
    assert "publication_state=b-published-with-pending-health" in text
    report2 = observe_authenticated_publication(
        trust_root=trust_root,
        manifest_a=manifest_a,
        signature_a=sig_a,
        manifest_b=manifest_b,
        signature_b=sig_b,
        scriptlet_a=scriptlet_a,
        scriptlet_b=scriptlet_b,
        payload_a=payload_a,
        payload_b=payload_b,
        kindle=kindle,
        evidence_a=evidence_a,
        evidence_b=evidence_b,
    )
    assert report2 == report


def test_invalid_signature_fails_closed(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    _payload, _scriptlet, _ev, manifest, _ = _build_release(tmp_path, VERSION_B, b"B")
    sig = sign_publication_evidence(manifest, trust_root)
    bad = signature_envelope(b"\x00" * 64)
    with pytest.raises(AuthEvidenceError, match="signature verification failed"):
        authenticate_publication_evidence(manifest, bad, trust_root)
    with pytest.raises(AuthEvidenceError, match="signature missing"):
        authenticate_publication_evidence(manifest, b"", trust_root)
    # Tamper envelope after valid sign.
    raw_sig = parse_signature_envelope(sig)
    flipped = bytes((raw_sig[0] ^ 0x01,)) + raw_sig[1:]
    with pytest.raises(AuthEvidenceError, match="signature verification failed"):
        authenticate_publication_evidence(manifest, signature_envelope(flipped), trust_root)


def test_wrong_signing_key_fails_closed(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    other = generate_test_trust_root(tmp_path / "other-key", label="other-publisher")
    _payload, _scriptlet, _ev, manifest, _ = _build_release(tmp_path / "rel", VERSION_B, b"B")
    sig = sign_publication_evidence(manifest, trust_root)
    with pytest.raises(AuthEvidenceError, match="signature verification failed"):
        authenticate_publication_evidence(manifest, sig, other)


def test_modified_manifest_after_signing_fails(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    _payload, _scriptlet, _ev, manifest, _ = _build_release(tmp_path, VERSION_B, b"B")
    sig = sign_publication_evidence(manifest, trust_root)
    tampered = manifest.replace(b"version=3.3.1\n", b"version=3.3.2\n", 1)
    with pytest.raises(AuthEvidenceError, match="signature verification failed"):
        authenticate_publication_evidence(tampered, sig, trust_root)


def test_modified_scriptlet_fails_package_verify(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    payload, scriptlet, _ev, manifest, _ = _build_release(tmp_path, VERSION_B, b"B")
    sig = sign_publication_evidence(manifest, trust_root)
    auth = authenticate_publication_evidence(manifest, sig, trust_root)
    scriptlet.write_bytes(scriptlet.read_bytes() + b"\n# tamper\n")
    with pytest.raises(AuthEvidenceError, match="scriptlet identity mismatch"):
        verify_scriptlet_identity(auth, scriptlet)
    with pytest.raises(AuthEvidenceError):
        verify_package_against_authenticated(auth, payload=payload, scriptlet=scriptlet)


def test_scriptlet_manifest_version_mismatch(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    payload, scriptlet, _ev, manifest, _ = _build_release(tmp_path, VERSION_B, b"B")
    # Build a Scriptlet named for a different version while keeping manifest for B.
    wrong = tmp_path / "Install Ignite v9.9.9.sh"
    wrong.write_bytes(scriptlet.read_bytes())
    sig = sign_publication_evidence(manifest, trust_root)
    auth = authenticate_publication_evidence(manifest, sig, trust_root)
    with pytest.raises(AuthEvidenceError, match="scriptlet basename mismatch"):
        verify_scriptlet_identity(auth, wrong)


def test_modified_installed_b_fails(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    _require_posix_installer()
    payload_a, scriptlet_a, evidence_a, manifest_a, _ = _build_release(
        tmp_path / "a", VERSION_A, b"RELEASE-A"
    )
    payload_b, scriptlet_b, evidence_b, manifest_b, _ = _build_release(
        tmp_path / "b", VERSION_B, b"RELEASE-B"
    )
    sig_b = sign_publication_evidence(manifest_b, trust_root)
    auth_b = authenticate_publication_evidence(manifest_b, sig_b, trust_root)
    kindle = init_fixture_root(tmp_path / "kindle").path
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    target = kindle / "scribe-app" / "bin" / "scribe-app"
    target.write_bytes(b"Z" + target.read_bytes()[1:])
    errors = verify_installed_b(auth_b, kindle, evidence_a, evidence_b)
    assert any(e.startswith("installed-mismatch:scribe-app") or e.startswith("pending-health") for e in errors)


def test_modified_retained_a_fails(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    _require_posix_installer()
    payload_a, _sa, evidence_a, manifest_a, _ = _build_release(
        tmp_path / "a", VERSION_A, b"RELEASE-A"
    )
    payload_b, _sb, evidence_b, manifest_b, _ = _build_release(
        tmp_path / "b", VERSION_B, b"RELEASE-B"
    )
    auth_a = authenticate_publication_evidence(
        manifest_a, sign_publication_evidence(manifest_a, trust_root), trust_root
    )
    auth_b = authenticate_publication_evidence(
        manifest_b, sign_publication_evidence(manifest_b, trust_root), trust_root
    )
    kindle = init_fixture_root(tmp_path / "kindle").path
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    retained = kindle / "scribe-app" / "previous-release" / "scribe-app"
    retained.write_bytes((kindle / "scribe-app" / "bin" / "scribe-app").read_bytes())
    errors = verify_retained_a(
        auth_a, kindle, evidence_a, evidence_b, auth_b=auth_b
    )
    assert any("retained-a" in e for e in errors)


def test_b_manifest_cannot_authenticate_retained_a(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    _require_posix_installer()
    payload_a, _sa, evidence_a, manifest_a, _ = _build_release(
        tmp_path / "a", VERSION_A, b"RELEASE-A"
    )
    payload_b, _sb, evidence_b, manifest_b, _ = _build_release(
        tmp_path / "b", VERSION_B, b"RELEASE-B"
    )
    auth_b = authenticate_publication_evidence(
        manifest_b, sign_publication_evidence(manifest_b, trust_root), trust_root
    )
    kindle = init_fixture_root(tmp_path / "kindle").path
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    # Using B's authenticated claims + B evidence against retained A must fail
    # (candidate collision and/or digest mismatch). Caller evidence for A cannot
    # be silently paired with B's auth object.
    with pytest.raises(AuthEvidenceError, match="disagrees"):
        validate_release_evidence_against_authenticated(evidence_a, auth_b)
    errors = verify_retained_a(auth_b, kindle, evidence_b, evidence_b)
    assert errors
    assert any(
        e == "retained-a-auth-collides-with-candidate-b" or "retained-a-mismatch" in e
        for e in errors
    )


def test_unsigned_legacy_a_remains_unauthenticated(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    _require_posix_installer()
    payload_a, scriptlet_a, evidence_a, manifest_a, _ = _build_release(
        tmp_path / "a", VERSION_A, b"RELEASE-A"
    )
    payload_b, scriptlet_b, evidence_b, manifest_b, _ = _build_release(
        tmp_path / "b", VERSION_B, b"RELEASE-B"
    )
    kindle = init_fixture_root(tmp_path / "kindle").path
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    sig_b = sign_publication_evidence(manifest_b, trust_root)
    # Missing A signature must fail through the real observation entry point.
    report = observe_authenticated_publication(
        trust_root=trust_root,
        manifest_a=manifest_a,
        signature_a=b"",
        manifest_b=manifest_b,
        signature_b=sig_b,
        scriptlet_a=scriptlet_a,
        scriptlet_b=scriptlet_b,
        payload_a=payload_a,
        payload_b=payload_b,
        kindle=kindle,
    )
    text = report.decode("ascii")
    assert "publisher_authentication=unavailable" in text
    assert "signature_status=missing" in text or "signature_status=failed" in text
    assert "retained_a=match" not in text
    assert b"refused:" in report
    # B can still be authenticated independently when A is unsigned/legacy.
    auth_b = authenticate_publication_evidence(manifest_b, sig_b, trust_root)
    assert verify_installed_b(auth_b, kindle, evidence_a, evidence_b) == []


def test_missing_lkg_fails_installed_b(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    _require_posix_installer()
    payload_a, _sa, evidence_a, _ma, _ = _build_release(tmp_path / "a", VERSION_A, b"A")
    payload_b, _sb, evidence_b, manifest_b, _ = _build_release(tmp_path / "b", VERSION_B, b"B")
    auth_b = authenticate_publication_evidence(
        manifest_b, sign_publication_evidence(manifest_b, trust_root), trust_root
    )
    kindle = init_fixture_root(tmp_path / "kindle").path
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    (
        kindle / "scribe-app" / "previous-release" / "ignite-lkg-inventory.v1"
    ).unlink()
    errors = verify_installed_b(auth_b, kindle, evidence_a, evidence_b)
    assert any("pending-health" in e or "publication-state" in e for e in errors)


def test_corrupted_pending_health_fails_installed_b(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    _require_posix_installer()
    payload_a, _sa, evidence_a, _ma, _ = _build_release(tmp_path / "a", VERSION_A, b"A")
    payload_b, _sb, evidence_b, manifest_b, _ = _build_release(tmp_path / "b", VERSION_B, b"B")
    auth_b = authenticate_publication_evidence(
        manifest_b, sign_publication_evidence(manifest_b, trust_root), trust_root
    )
    kindle = init_fixture_root(tmp_path / "kindle").path
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    pending = kindle / "scribe-app" / "state" / "update" / "pending-health.v1"
    pending.write_bytes(b"CORRUPT\n")
    errors = verify_installed_b(auth_b, kindle, evidence_a, evidence_b)
    assert any("pending-health" in e for e in errors)


def test_interrupted_b_publication_fails_installed_b(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    _require_posix_installer()
    payload_a, _sa, evidence_a, _ma, _ = _build_release(tmp_path / "a", VERSION_A, b"A")
    payload_b, _sb, evidence_b, manifest_b, _ = _build_release(tmp_path / "b", VERSION_B, b"B")
    auth_b = authenticate_publication_evidence(
        manifest_b, sign_publication_evidence(manifest_b, trust_root), trust_root
    )
    kindle = init_fixture_root(tmp_path / "kindle").path
    assert _run_install(payload_a, kindle).returncode == 0
    crashed = _run_install(
        payload_b, kindle, {"IGNITE_FAIL_PUBLISH": "before-pending-health-publish"}
    )
    assert crashed.returncode != 0
    errors = verify_installed_b(auth_b, kindle, evidence_a, evidence_b)
    assert errors
    assert any("publication-state" in e or "pending-health" in e for e in errors)


@pytest.mark.skipif(sys.platform == "win32", reason="symlink substitution on Unix")
def test_path_substitution_during_authenticated_verify(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    payload, scriptlet, _ev, manifest, _ = _build_release(tmp_path, VERSION_B, b"B")
    sig = sign_publication_evidence(manifest, trust_root)
    auth = authenticate_publication_evidence(manifest, sig, trust_root)
    outside = tmp_path / "outside-scriptlet"
    outside.write_bytes(scriptlet.read_bytes())
    scriptlet.unlink()
    os.symlink(outside, scriptlet)
    with pytest.raises(AuthEvidenceError, match="symlink"):
        verify_scriptlet_identity(auth, scriptlet)


def test_consume_regular_file_single_descriptor(tmp_path: Path) -> None:
    path = tmp_path / "blob.bin"
    path.write_bytes(b"abc123")
    data, size, digest, mode = consume_regular_file_fd(path, max_size=1024)
    assert data == b"abc123"
    assert size == 6
    assert digest == hashlib.sha256(b"abc123").hexdigest()
    assert stat.S_ISREG(mode)


def test_noncanonical_and_truncated_manifest_refused(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    _payload, _scriptlet, _ev, manifest, _ = _build_release(tmp_path, VERSION_B, b"B")
    sig = sign_publication_evidence(manifest, trust_root)
    with pytest.raises(AuthEvidenceError):
        authenticate_publication_evidence(manifest + b"extra\n", sig, trust_root)
    with pytest.raises(AuthEvidenceError):
        authenticate_publication_evidence(manifest[:-1], sig, trust_root)
    with pytest.raises(AuthEvidenceError):
        authenticate_publication_evidence(manifest.replace(b"\n", b"\r\n"), sig, trust_root)


def test_retained_optional_absence_enforced(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    _require_posix_installer()
    payload_a, _sa, evidence_a, manifest_a, _ = _build_release(
        tmp_path / "a", VERSION_A, b"RELEASE-A"
    )
    payload_b, _sb, evidence_b, manifest_b, _ = _build_release(
        tmp_path / "b", VERSION_B, b"RELEASE-B"
    )
    # Package fixtures leave optional upstream_info absent in A claims.
    auth_a = authenticate_publication_evidence(
        manifest_a, sign_publication_evidence(manifest_a, trust_root), trust_root
    )
    auth_b = authenticate_publication_evidence(
        manifest_b, sign_publication_evidence(manifest_b, trust_root), trust_root
    )
    opt = next(c for c in auth_a.components if c.role == "license-noto-emoji-upstream-info")
    assert opt.presence == "absent"
    kindle = init_fixture_root(tmp_path / "kindle").path
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    planted = (
        kindle / "scribe-app" / "previous-release" / "upstream_info.md"
    )
    planted.write_bytes(b"unexpected-optional\n")
    errors = verify_retained_a(
        auth_a, kindle, evidence_a, evidence_b, auth_b=auth_b
    )
    assert "retained-a-optional-unexpected:license-noto-emoji-upstream-info" in errors


def test_caller_evidence_disagreeing_with_signed_a_refused(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    payload_a, scriptlet_a, evidence_a, manifest_a, _ = _build_release(
        tmp_path / "a", VERSION_A, b"RELEASE-A"
    )
    payload_b, scriptlet_b, evidence_b, manifest_b, _ = _build_release(
        tmp_path / "b", VERSION_B, b"RELEASE-B"
    )
    sig_a = sign_publication_evidence(manifest_a, trust_root)
    sig_b = sign_publication_evidence(manifest_b, trust_root)
    auth_a = authenticate_publication_evidence(manifest_a, sig_a, trust_root)
    # Supply B's evidence where A's is required.
    with pytest.raises(AuthEvidenceError, match="disagrees"):
        validate_release_evidence_against_authenticated(evidence_b, auth_a)
    kindle = init_fixture_root(tmp_path / "kindle").path
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    report = observe_authenticated_publication(
        trust_root=trust_root,
        manifest_a=manifest_a,
        signature_a=sig_a,
        manifest_b=manifest_b,
        signature_b=sig_b,
        scriptlet_a=scriptlet_a,
        scriptlet_b=scriptlet_b,
        payload_a=payload_a,
        payload_b=payload_b,
        kindle=kindle,
        evidence_a=evidence_b,  # wrong release evidence for A
        evidence_b=evidence_b,
    )
    assert b"refused:" in report
    assert b"disagrees" in report


def test_caller_evidence_disagreeing_with_signed_b_refused(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    payload_a, scriptlet_a, evidence_a, manifest_a, _ = _build_release(
        tmp_path / "a", VERSION_A, b"RELEASE-A"
    )
    payload_b, scriptlet_b, evidence_b, manifest_b, _ = _build_release(
        tmp_path / "b", VERSION_B, b"RELEASE-B"
    )
    sig_a = sign_publication_evidence(manifest_a, trust_root)
    sig_b = sign_publication_evidence(manifest_b, trust_root)
    auth_b = authenticate_publication_evidence(manifest_b, sig_b, trust_root)
    with pytest.raises(AuthEvidenceError, match="disagrees"):
        validate_release_evidence_against_authenticated(evidence_a, auth_b)
    kindle = init_fixture_root(tmp_path / "kindle").path
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    report = observe_authenticated_publication(
        trust_root=trust_root,
        manifest_a=manifest_a,
        signature_a=sig_a,
        manifest_b=manifest_b,
        signature_b=sig_b,
        scriptlet_a=scriptlet_a,
        scriptlet_b=scriptlet_b,
        payload_a=payload_a,
        payload_b=payload_b,
        kindle=kindle,
        evidence_a=evidence_a,
        evidence_b=evidence_a,  # wrong release evidence for B
    )
    assert b"refused:" in report
    assert b"disagrees" in report


def test_b_cannot_authenticate_a_when_component_bytes_match(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    """Identical package bytes across A/B still require A's release identity."""
    _require_posix_installer()
    # Same distinguisher → many direct-copy digests collide across versions.
    payload_a, _sa, evidence_a, manifest_a, _ = _build_release(
        tmp_path / "a", VERSION_A, b"SAME-BYTES"
    )
    payload_b, _sb, evidence_b, manifest_b, _ = _build_release(
        tmp_path / "b", VERSION_B, b"SAME-BYTES"
    )
    auth_a = authenticate_publication_evidence(
        manifest_a, sign_publication_evidence(manifest_a, trust_root), trust_root
    )
    auth_b = authenticate_publication_evidence(
        manifest_b, sign_publication_evidence(manifest_b, trust_root), trust_root
    )
    # At least one independently verifiable role shares digests across A and B.
    shared = [
        c.role
        for c in auth_a.components
        if c.presence == "file"
        and any(
            d.role == c.role and d.presence == "file" and d.sha256 == c.sha256
            for d in auth_b.components
        )
    ]
    assert shared, "fixture should include identical cross-release component bytes"
    kindle = init_fixture_root(tmp_path / "kindle").path
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    # Correct A auth still matches retained A.
    assert (
        verify_retained_a(auth_a, kindle, evidence_a, evidence_b, auth_b=auth_b) == []
    )
    # B's signed identity must not authenticate retained A despite matching bytes.
    errors = verify_retained_a(auth_b, kindle, evidence_b, evidence_b, auth_b=auth_b)
    assert "retained-a-auth-collides-with-candidate-b" in errors


def test_noncanonical_manifest_with_valid_test_signature_refused(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    _payload, _scriptlet, _ev, manifest, _ = _build_release(tmp_path, VERSION_B, b"B")
    # Sign a noncanonical encoding (extra blank line) under the test trust root.
    noncanonical = manifest[:-1] + b"\n\n"
    sig = sign_publication_evidence(noncanonical, trust_root)
    # Signature may verify cryptographically, but canonical parser must refuse.
    with pytest.raises(AuthEvidenceError):
        authenticate_publication_evidence(noncanonical, sig, trust_root)


def test_derived_evidence_agrees_with_authenticated_claims(
    tmp_path: Path, trust_root: PublicationTestTrustRoot
) -> None:
    payload, scriptlet, evidence, manifest, _ = _build_release(
        tmp_path, VERSION_B, b"B"
    )
    auth = authenticate_publication_evidence(
        manifest, sign_publication_evidence(manifest, trust_root), trust_root
    )
    verify_package_against_authenticated(auth, payload=payload, scriptlet=scriptlet)
    derived = release_evidence_from_authenticated(auth, payload=payload)
    validate_release_evidence_against_authenticated(derived, auth)
    validate_release_evidence_against_authenticated(evidence, auth)
    assert isinstance(derived, ReleaseEvidence)
    assert derived.version == auth.version
