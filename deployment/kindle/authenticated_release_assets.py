#!/usr/bin/env python3
"""Build mutually bound Kindle release authentication assets (Slice 3D-B2a.4.3h).

Order (must not rebuild the Scriptlet after measuring it):

  staged payload
    → final customer Scriptlet (single authoritative measurement)
    → SFX equivalence against that path
    → remasure / refuse if identity changed
    → release-v1 + production evidence + binding from that identity
    → remasure again before signing
    → sign release-v1 + binding with one disposable publisher key

Production promotion signing remains disabled while the pin is UNPROVISIONED.
Fixture-signed assets are written under ``fixture-authenticated-assets/`` only.
"""

from __future__ import annotations

import hashlib
import re
import sys
from dataclasses import dataclass
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parents[1]
_SFX = _HERE / "sfx"
_RELEASE_TOOLS = _REPO / "tools" / "release"
_PRODUCTION_PIN = _RELEASE_TOOLS / "kindle-release-public-key.hex"
for extra in (_HERE, _SFX, str(_RELEASE_TOOLS)):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from production_publication_evidence import (  # noqa: E402
    MAX_SCRIPTLET_BYTES,
    build_production_publication_evidence,
)
from production_trust_boundary import (  # noqa: E402
    assert_production_pin_unprovisioned,
    build_publication_binding,
    sign_binding_for_test,
    sign_release_for_test,
)
from verify_sfx_payload_equivalence import verify_equivalence  # noqa: E402

_SHA_RE = re.compile(r"^[0-9a-f]{64}$")
READ_CHUNK = 1024 * 1024
FIXTURE_AUTH_SUBDIR = "fixture-authenticated-assets"


class AuthenticatedAssetError(RuntimeError):
    pass


@dataclass(frozen=True)
class ScriptletIdentity:
    """One authoritative measurement of the finalized customer Scriptlet."""

    path: Path
    size: int
    sha256: str


@dataclass(frozen=True)
class AuthenticatedReleaseAssetSet:
    version: str
    tag: str
    scriptlet_path: Path
    scriptlet_sha256: str
    scriptlet_bytes: int
    release_manifest: bytes
    release_signature: bytes
    publication_evidence: bytes
    publication_binding: bytes
    binding_signature: bytes
    package_profile_sha256: str


def release_asset_basenames(version: str) -> dict[str, str]:
    return {
        "scriptlet": f"Install Ignite v{version}.sh",
        "release_manifest": f"ignite-kindle-v{version}.manifest",
        "release_signature": f"ignite-kindle-v{version}.manifest.sig",
        "publication_evidence": f"ignite-kindle-v{version}.publication-evidence",
        "publication_binding": f"ignite-kindle-v{version}.publication-binding",
        "binding_signature": f"ignite-kindle-v{version}.publication-binding.sig",
    }


def measure_scriptlet(scriptlet: Path, *, expected_name: str | None = None) -> ScriptletIdentity:
    """Streaming SHA-256 measurement of the finalized Scriptlet (bounded)."""
    if expected_name is not None and scriptlet.name != expected_name:
        raise AuthenticatedAssetError(f"scriptlet basename must be {expected_name!r}")
    digest = hashlib.sha256()
    size = 0
    try:
        with scriptlet.open("rb") as handle:
            while True:
                chunk = handle.read(READ_CHUNK)
                if not chunk:
                    break
                size += len(chunk)
                if size > MAX_SCRIPTLET_BYTES:
                    raise AuthenticatedAssetError("scriptlet exceeds size bound")
                digest.update(chunk)
    except OSError as exc:
        raise AuthenticatedAssetError(f"cannot read scriptlet: {exc}") from exc
    if size <= 0:
        raise AuthenticatedAssetError("scriptlet must not be empty")
    return ScriptletIdentity(path=scriptlet, size=size, sha256=digest.hexdigest())


def assert_scriptlet_unchanged(identity: ScriptletIdentity) -> None:
    """Remeasure and refuse if the finalized Scriptlet mutated after measurement.

    Residual TOCTOU: a concurrent replacement between the end of this remasurement
    and a subsequent open by another process is not locked; this seam detects
    changes across the asset-generation steps that remasure explicitly.
    """
    current = measure_scriptlet(identity.path, expected_name=identity.path.name)
    if current.size != identity.size or current.sha256 != identity.sha256:
        raise AuthenticatedAssetError(
            "scriptlet identity changed during authenticated asset generation"
        )


def build_release_v1_from_identity(*, version: str, identity: ScriptletIdentity) -> bytes:
    expected = f"Install Ignite v{version}.sh"
    if identity.path.name != expected:
        raise AuthenticatedAssetError(f"scriptlet basename must be {expected!r}")
    if not _SHA_RE.fullmatch(identity.sha256):
        raise AuthenticatedAssetError("scriptlet sha256 malformed")
    tag = f"v{version}"
    return (
        f"schema=ignite-kindle-release-v1\n"
        f"version={version}\n"
        f"tag={tag}\n"
        f"artifact={expected}\n"
        f"bytes={identity.size}\n"
        f"sha256={identity.sha256}\n"
    ).encode("ascii")


def build_release_v1_manifest(*, version: str, scriptlet: Path) -> bytes:
    """Convenience wrapper: measure once then build release-v1."""
    identity = measure_scriptlet(scriptlet, expected_name=f"Install Ignite v{version}.sh")
    return build_release_v1_from_identity(version=version, identity=identity)


def build_authenticated_release_assets(
    *,
    version: str,
    payload: Path,
    scriptlet: Path,
    package_profile: bytes,
    private_key_path: Path,
    public_key: bytes,
    require_sfx_equivalence: bool = True,
) -> AuthenticatedReleaseAssetSet:
    """Construct the full asset set from one Scriptlet measurement + staged payload.

    ``private_key_path`` / ``public_key`` must be disposable fixture material.
    Never the production pin.
    """
    assert_production_pin_unprovisioned()
    _refuse_production_key_paths(private_key_path)
    if payload.name != f"Ignite-Kindle-{version}":
        raise AuthenticatedAssetError("payload directory name mismatch")
    expected_name = f"Install Ignite v{version}.sh"
    if scriptlet.name != expected_name:
        raise AuthenticatedAssetError("scriptlet basename mismatch")

    identity = measure_scriptlet(scriptlet, expected_name=expected_name)

    if require_sfx_equivalence:
        ok, report = verify_equivalence(
            sfx_path=scriptlet,
            payload_path=payload,
            version=version,
            package_profile=package_profile,
        )
        if not ok:
            raise AuthenticatedAssetError(
                "SFX equivalence failed:\n" + report.decode("ascii", errors="replace")
            )
        assert_scriptlet_unchanged(identity)

    profile_sha = hashlib.sha256(package_profile).hexdigest()
    release_manifest = build_release_v1_from_identity(version=version, identity=identity)

    evidence = build_production_publication_evidence(
        version=version,
        payload=payload,
        scriptlet=scriptlet,
        scriptlet_bytes=identity.size,
        scriptlet_sha256=identity.sha256,
    )
    if f"package_profile_sha256={profile_sha}\n".encode("ascii") not in evidence:
        raise AuthenticatedAssetError("evidence package_profile_sha256 mismatch")
    if f"scriptlet_sha256={identity.sha256}\n".encode("ascii") not in evidence:
        raise AuthenticatedAssetError("evidence scriptlet_sha256 mismatch")

    binding = build_publication_binding(
        release_tag=f"v{version}",
        release_version=version,
        release_manifest=release_manifest,
        scriptlet_sha256=identity.sha256,
        publication_evidence=evidence,
    )

    # Detect mutation after evidence construction, before signing.
    assert_scriptlet_unchanged(identity)

    release_sig = sign_release_for_test(
        release_manifest, private_key_path=private_key_path, public_key=public_key
    )
    binding_sig = sign_binding_for_test(
        binding, private_key_path=private_key_path, public_key=public_key
    )

    assert_scriptlet_unchanged(identity)

    return AuthenticatedReleaseAssetSet(
        version=version,
        tag=f"v{version}",
        scriptlet_path=scriptlet,
        scriptlet_sha256=identity.sha256,
        scriptlet_bytes=identity.size,
        release_manifest=release_manifest,
        release_signature=release_sig,
        publication_evidence=evidence,
        publication_binding=binding,
        binding_signature=binding_sig,
        package_profile_sha256=profile_sha,
    )


def write_authenticated_release_assets(
    assets: AuthenticatedReleaseAssetSet, output_dir: Path
) -> dict[str, Path]:
    """Write canonical asset filenames. Refuses to overwrite existing files."""
    output_dir.mkdir(parents=True, exist_ok=True)
    names = release_asset_basenames(assets.version)
    mapping = {
        "release_manifest": assets.release_manifest,
        "release_signature": assets.release_signature,
        "publication_evidence": assets.publication_evidence,
        "publication_binding": assets.publication_binding,
        "binding_signature": assets.binding_signature,
    }
    written: dict[str, Path] = {}
    for key, content in mapping.items():
        path = output_dir / names[key]
        if path.exists():
            raise AuthenticatedAssetError(f"refusing to replace existing asset: {path.name}")
        path.write_bytes(content)
        written[key] = path
    written["scriptlet"] = assets.scriptlet_path
    return written


def _refuse_production_key_paths(*paths: Path) -> None:
    pin = _PRODUCTION_PIN.resolve()
    for path in paths:
        if path.resolve() == pin:
            raise AuthenticatedAssetError(
                "refusing to use the production public-key pin for fixture signing"
            )


def load_fixture_public_key(path: Path) -> bytes:
    """Load a disposable public key file (64 lowercase hex + LF). Never the production pin."""
    _refuse_production_key_paths(path)
    raw = path.read_bytes()
    if raw == b"UNPROVISIONED\n":
        raise AuthenticatedAssetError("fixture public key is unprovisioned")
    if len(raw) != 65 or raw[-1:] != b"\n" or not re.fullmatch(rb"[0-9a-f]{64}", raw[:-1]):
        raise AuthenticatedAssetError(
            "fixture public key must be 64 lowercase hex characters plus LF"
        )
    return bytes.fromhex(raw[:-1].decode("ascii"))


def emit_fixture_authenticated_assets_from_package_output(
    *,
    version: str,
    out_root: Path,
    private_key_path: Path,
    public_key_path: Path,
) -> AuthenticatedReleaseAssetSet:
    """Post-package integration point used by ``package_release`` fixture workflow.

    Expects the ordinary packaging layout under ``out_root``:
      - ``Install Ignite v{version}.sh``
      - ``staging/Ignite-Kindle-{version}/``
      - ``component-profiles/ignite-kindle-component-profile-package.v1``

    Writes fixture-signed assets only under ``fixture-authenticated-assets/``.
    Does not replace or relabel the customer Scriptlet or checksum sidecars.
    """
    assert_production_pin_unprovisioned()
    _refuse_production_key_paths(private_key_path, public_key_path)
    scriptlet = out_root / f"Install Ignite v{version}.sh"
    payload = out_root / "staging" / f"Ignite-Kindle-{version}"
    profile_path = (
        out_root / "component-profiles" / "ignite-kindle-component-profile-package.v1"
    )
    if not scriptlet.is_file():
        raise AuthenticatedAssetError(f"missing packaged Scriptlet: {scriptlet.name}")
    if not payload.is_dir():
        raise AuthenticatedAssetError("missing staged payload directory")
    if not profile_path.is_file():
        raise AuthenticatedAssetError("missing package component profile")

    public_key = load_fixture_public_key(public_key_path)
    assets = build_authenticated_release_assets(
        version=version,
        payload=payload,
        scriptlet=scriptlet,
        package_profile=profile_path.read_bytes(),
        private_key_path=private_key_path,
        public_key=public_key,
        require_sfx_equivalence=True,
    )
    dest = out_root / FIXTURE_AUTH_SUBDIR
    write_authenticated_release_assets(assets, dest)
    return assets


# Back-compat alias used by earlier 3h call sites.
def emit_authenticated_assets_after_package(
    *,
    version: str,
    out_root: Path,
    payload: Path,
    scriptlet: Path,
    package_profile_path: Path,
    private_key_path: Path,
    public_key: bytes,
) -> AuthenticatedReleaseAssetSet:
    assert_production_pin_unprovisioned()
    _refuse_production_key_paths(private_key_path)
    profile = package_profile_path.read_bytes()
    assets = build_authenticated_release_assets(
        version=version,
        payload=payload,
        scriptlet=scriptlet,
        package_profile=profile,
        private_key_path=private_key_path,
        public_key=public_key,
        require_sfx_equivalence=True,
    )
    write_authenticated_release_assets(assets, out_root / FIXTURE_AUTH_SUBDIR)
    return assets
