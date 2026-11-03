#!/usr/bin/env python3
"""Single-command offline Kindle release ceremony (Slice 3D-B2a.4.3n).

Modes:
  prepare  — verify CI packaging inputs; no private key; no signing
  sign     — requires explicit --private-key; emits six-asset bundle + receipt
  sign-binding — sign a canonical binding with associated digest context

Does not mutate GitHub, execute the Scriptlet, provision the production pin, or
upload anything. CI provenance remains the promote resolve/verify path.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import secrets
import shutil
import stat
import sys
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parents[1]
_SFX = _HERE / "sfx"
_RELEASE = _REPO / "tools" / "release"
for extra in (_HERE, _SFX, str(_RELEASE)):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

import authenticated_publication_preflight as _preflight_mod  # noqa: E402
from authenticated_publication_preflight import (  # noqa: E402
    PreflightError,
    TrustContext,
    run_preflight,
)
from authenticated_release_assets import (  # noqa: E402
    AuthenticatedAssetError,
    assert_scriptlet_unchanged,
    build_release_v1_from_identity,
    measure_scriptlet,
    release_asset_basenames,
)
from ci_packaging_measurement import (  # noqa: E402
    MEASUREMENT_BASENAME,
    PACKAGE_PROFILE_BASENAME,
    MeasurementError,
    read_bounded_plain_file,
)
from kindle_release_sign import (  # noqa: E402
    UNPROVISIONED,
    ReleaseSigningError,
    load_public_key,
    version_from_tag,
)
from offline_packaging_handoff import (  # noqa: E402
    HandoffError,
    prepare_evidence_from_handoff,
    verify_handoff_inputs,
)
from production_trust_boundary import (  # noqa: E402
    MAX_BINDING_BYTES,
    MAX_EVIDENCE_BYTES,
    MAX_RELEASE_MANIFEST_BYTES,
    ProductionTrustError,
    authenticate_publication_chain_with_key,
    build_publication_binding,
    parse_binding,
    sign_publication_binding,
    sign_release_manifest,
)

RECEIPT_HEADER = "IGNITE_KINDLE_OFFLINE_SIGNING_RECEIPT_V1"
RECEIPT_SCHEMA = "1"
RECEIPT_BASENAME_TMPL = "ignite-kindle-v{version}.signing-receipt.v1"
PROMOTE_ASSETS_SUBDIR = "promote-authenticated-assets"
MAX_RECEIPT_BYTES = 64 * 1024
# Canonical sha256sum sidecar is one short LF line (digest + two spaces + name).
MAX_CHECKSUM_SIDECAR_BYTES = 256
_SHA_RE = __import__("re").compile(r"^[0-9a-f]{64}$")


class CeremonyError(ValueError):
    """Fail-closed offline ceremony refusal."""


@dataclass
class CeremonyHooks:
    """Test-only injection points. Never exposed as a shipping CLI flag."""

    after_prepare: Callable[[], None] | None = None
    before_release_sign: Callable[[], None] | None = None
    after_release_sign: Callable[[], None] | None = None
    before_binding_sign: Callable[[], None] | None = None
    after_binding_sign: Callable[[], None] | None = None
    after_stage_write: Callable[[Path], None] | None = None
    before_preflight: Callable[[Path], None] | None = None
    before_finalize: Callable[[Path], None] | None = None


@dataclass
class DiscoveredInputs:
    version: str
    tag: str
    scriptlet: Path
    measurement: Path
    package_profile: Path
    checksum_sidecar: Path


@dataclass
class PreparedCeremony:
    inputs: DiscoveredInputs
    scriptlet_bytes: int
    scriptlet_sha256: str
    measurement_sha256: str
    package_profile_sha256: str
    evidence: bytes
    release_manifest: bytes
    publication_binding: bytes
    identity: object
    handoff_detail: tuple[str, ...] = ()
    readiness_lines: list[str] = field(default_factory=list)


def _sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _public_key_fingerprint(public_key: bytes) -> str:
    return f"ed25519-sha256:{_sha256_bytes(public_key)}"


def _lstat_is_symlink(path: Path) -> bool:
    try:
        return stat.S_ISLNK(os.lstat(os.fspath(path)).st_mode)
    except OSError:
        return False


def _unique_named_files(root: Path, basename: str, *, label: str) -> Path:
    """Return the single regular-file match for basename under root.

    Refuses symlinks and multiple distinct resolved paths.
    """
    matches: list[Path] = []
    for path in root.rglob(basename):
        if _lstat_is_symlink(path):
            raise CeremonyError(f"symlink refused for {label}: {path}")
        try:
            st = os.lstat(os.fspath(path))
        except OSError as exc:
            raise CeremonyError(f"cannot lstat {label} candidate {path}: {exc}") from exc
        if not stat.S_ISREG(st.st_mode):
            continue
        matches.append(path)
    if not matches:
        raise CeremonyError(f"missing {label}: {basename}")
    by_resolved: dict[Path, Path] = {}
    for path in matches:
        resolved = path.resolve()
        if resolved in by_resolved and by_resolved[resolved] != path:
            # Same inode via different non-symlink paths is one artifact.
            continue
        if resolved not in by_resolved:
            by_resolved[resolved] = path
    if len(by_resolved) > 1:
        listed = ", ".join(str(p) for p in sorted(by_resolved.values(), key=str))
        raise CeremonyError(f"ambiguous {label} set ({len(by_resolved)} matches): {listed}")
    return next(iter(by_resolved.values()))


def _validate_checksum_sidecar(
    *,
    root: Path,
    scriptlet: Path,
    version: str,
    expected_sha256: str | None = None,
) -> Path:
    """Require exactly one canonical Install Ignite vX.Y.Z.sha256 sidecar."""
    basename = f"Install Ignite v{version}.sha256"
    sidecar = _unique_named_files(root, basename, label="checksum sidecar")
    if _lstat_is_symlink(sidecar):
        raise CeremonyError(f"symlink refused for checksum sidecar: {sidecar}")
    # Prefer colocated with Scriptlet when both exist as the same resolved file.
    colocated = scriptlet.parent / basename
    if colocated.is_file() and not _lstat_is_symlink(colocated):
        if colocated.resolve() != sidecar.resolve():
            raise CeremonyError(
                "ambiguous checksum sidecar (Scriptlet directory disagrees with tree match)"
            )
        sidecar = colocated
    try:
        raw = sidecar.read_bytes()
    except OSError as exc:
        raise CeremonyError(f"cannot read checksum sidecar: {exc}") from exc
    if not raw or b"\r" in raw or not raw.endswith(b"\n"):
        raise CeremonyError("checksum sidecar must be LF-terminated ASCII")
    if len(raw) > MAX_CHECKSUM_SIDECAR_BYTES:
        raise CeremonyError(
            f"checksum sidecar exceeds {MAX_CHECKSUM_SIDECAR_BYTES} bytes"
        )
    lines = raw.decode("ascii").splitlines()
    if len(lines) != 1:
        raise CeremonyError("checksum sidecar must contain exactly one line")
    line = lines[0]
    # Canonical sha256sum text mode: "<hex><two spaces><filename>"
    parts = line.split("  ")
    if len(parts) != 2:
        raise CeremonyError("checksum sidecar line is not canonical sha256sum format")
    digest, filename = parts
    if not _SHA_RE.fullmatch(digest):
        raise CeremonyError("checksum sidecar digest malformed")
    if filename != scriptlet.name:
        raise CeremonyError(
            f"checksum sidecar filename {filename!r} does not match {scriptlet.name!r}"
        )
    if expected_sha256 is not None and digest != expected_sha256:
        raise CeremonyError("checksum sidecar digest does not match Scriptlet bytes")
    return sidecar


def discover_ci_artifacts(ci_artifacts: Path, *, tag: str) -> DiscoveredInputs:
    """Locate Scriptlet + measurement + profile under a CI artifact directory."""
    if not ci_artifacts.is_dir():
        raise CeremonyError(f"ci-artifacts directory not found: {ci_artifacts}")
    if _lstat_is_symlink(ci_artifacts):
        raise CeremonyError("ci-artifacts directory must not be a symlink")
    try:
        version = version_from_tag(tag)
    except ReleaseSigningError as exc:
        raise CeremonyError(str(exc)) from exc

    scriptlet_name = f"Install Ignite v{version}.sh"
    try:
        scriptlet = _unique_named_files(
            ci_artifacts, scriptlet_name, label="Scriptlet"
        )
    except CeremonyError as exc:
        if "missing" in str(exc):
            others = [
                p
                for p in ci_artifacts.rglob("Install Ignite v*.sh")
                if p.is_file() and not _lstat_is_symlink(p)
            ]
            if len(others) == 1:
                raise CeremonyError(
                    f"tag {tag} expects {scriptlet_name}, found {others[0].name} "
                    "(version/tag mismatch)"
                ) from exc
        raise

    measurement = _unique_named_files(
        ci_artifacts, MEASUREMENT_BASENAME, label="measurement"
    )
    package_profile = _unique_named_files(
        ci_artifacts, PACKAGE_PROFILE_BASENAME, label="package profile"
    )
    checksum = _validate_checksum_sidecar(
        root=ci_artifacts, scriptlet=scriptlet, version=version
    )
    return DiscoveredInputs(
        version=version,
        tag=tag,
        scriptlet=scriptlet,
        measurement=measurement,
        package_profile=package_profile,
        checksum_sidecar=checksum,
    )


def prepare_ceremony(
    *,
    tag: str,
    ci_artifacts: Path,
    expected_source_revision: str | None = None,
) -> PreparedCeremony:
    """Validate CI packaging inputs and build unsigned evidence + binding inputs."""
    discovered = discover_ci_artifacts(ci_artifacts, tag=tag)
    try:
        handoff = verify_handoff_inputs(
            scriptlet=discovered.scriptlet,
            measurement_path=discovered.measurement,
            package_profile_path=discovered.package_profile,
            expected_version=discovered.version,
            expected_source_revision=expected_source_revision,
        )
    except HandoffError as exc:
        raise CeremonyError(str(exc)) from exc
    if not handoff.evidence_ready:
        raise CeremonyError(
            "CI packaging inputs not ready for evidence "
            "(Scriptlet/measurement/profile consistency failed)"
        )

    try:
        evidence = prepare_evidence_from_handoff(
            scriptlet=discovered.scriptlet,
            measurement_path=discovered.measurement,
            package_profile_path=discovered.package_profile,
            expected_version=discovered.version,
        )
    except HandoffError as exc:
        raise CeremonyError(str(exc)) from exc

    identity = measure_scriptlet(
        discovered.scriptlet,
        expected_name=f"Install Ignite v{discovered.version}.sh",
    )
    _validate_checksum_sidecar(
        root=ci_artifacts,
        scriptlet=discovered.scriptlet,
        version=discovered.version,
        expected_sha256=identity.sha256,
    )

    release_manifest = build_release_v1_from_identity(
        version=discovered.version, identity=identity
    )
    try:
        binding = build_publication_binding(
            release_tag=discovered.tag,
            release_version=discovered.version,
            release_manifest=release_manifest,
            scriptlet_sha256=identity.sha256,
            publication_evidence=evidence,
        )
        parse_binding(binding)
    except ProductionTrustError as exc:
        raise CeremonyError(str(exc)) from exc

    assert_scriptlet_unchanged(identity)

    meas_sha = _sha256_file(discovered.measurement)
    profile_sha = _sha256_file(discovered.package_profile)
    readiness = [
        RECEIPT_HEADER,
        f"schema={RECEIPT_SCHEMA}",
        "mode=prepare",
        f"version={discovered.version}",
        f"tag={discovered.tag}",
        f"scriptlet={discovered.scriptlet.name}",
        f"scriptlet_bytes={identity.size}",
        f"scriptlet_sha256={identity.sha256}",
        f"measurement={discovered.measurement.name}",
        f"measurement_sha256={meas_sha}",
        f"package_profile={discovered.package_profile.name}",
        f"package_profile_sha256={profile_sha}",
        f"publication_evidence_sha256={_sha256_bytes(evidence)}",
        f"release_manifest_sha256={_sha256_bytes(release_manifest)}",
        f"publication_binding_sha256={_sha256_bytes(binding)}",
        "offline_evidence_inputs_ready=true",
        "content_integrity_verified=true",
        "publisher_signatures_verified=false",
        "signing_performed=false",
        "cryptographic_integrity=true",
        "ci_provenance_established_by_this_ceremony=false",
        "production_publisher_authority=false",
        "production_promotion_ready=false",
        "github_mutation=false",
        "scriptlet_executed=false",
        "require_trusted_promotion_path_for_ci_provenance=true",
    ]
    readiness.extend(handoff.detail)

    return PreparedCeremony(
        inputs=discovered,
        scriptlet_bytes=identity.size,
        scriptlet_sha256=identity.sha256,
        measurement_sha256=meas_sha,
        package_profile_sha256=profile_sha,
        evidence=evidence,
        release_manifest=release_manifest,
        publication_binding=binding,
        identity=identity,
        handoff_detail=handoff.detail,
        readiness_lines=readiness,
    )


def _write_bytes_exclusive(path: Path, content: bytes) -> None:
    """Create ``path`` exclusively; handle short writes; unlink on failure."""
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists() or _lstat_is_symlink(path):
        raise CeremonyError(f"refusing to overwrite {path.name}")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_BINARY"):
        flags |= os.O_BINARY
    fd = os.open(os.fspath(path), flags, 0o644)
    try:
        try:
            view = memoryview(content)
            written = 0
            while written < len(content):
                n = os.write(fd, view[written:])
                if n <= 0:
                    raise CeremonyError(f"short write while creating {path.name}")
                written += n
        except BaseException:
            os.close(fd)
            fd = -1
            try:
                path.unlink()
            except OSError:
                pass
            raise
    finally:
        if fd >= 0:
            os.close(fd)


def _refuse_standalone_output(path: Path, *, label: str) -> None:
    """Refuse existing files, dangling symlinks, and unsafe parent paths."""
    if path.exists() or _lstat_is_symlink(path):
        raise CeremonyError(
            f"{label} must be a new unused path (refusing existing/symlink)"
        )
    parent = path.parent
    if _lstat_is_symlink(parent):
        raise CeremonyError(f"{label} parent directory must not be a symlink")
    if parent.exists() and not parent.is_dir():
        raise CeremonyError(f"{label} parent path is not a directory")


def _read_contract_bytes(path: Path, *, label: str, max_bytes: int) -> bytes:
    try:
        return read_bounded_plain_file(path, max_bytes=max_bytes, label=label)
    except MeasurementError as exc:
        raise CeremonyError(str(exc)) from exc


def _ceremony_preflight_trust(*, public_key: bytes) -> TrustContext:
    """Select preflight trust from the repository pin vs caller public key.

    Official pin + matching caller key → production-pin verification.
    UNPROVISIONED / unusable pin / alternate key → isolated fixture verification.
    Alternate keys never receive official publisher authority.
    """
    pin_path = _preflight_mod._PRODUCTION_PIN
    try:
        raw = pin_path.read_bytes()
    except OSError:
        return TrustContext.FIXTURE
    if raw == UNPROVISIONED:
        return TrustContext.FIXTURE
    try:
        pin_key = load_public_key(pin_path, official=True)
    except ReleaseSigningError:
        return TrustContext.FIXTURE
    if pin_key != public_key:
        return TrustContext.FIXTURE
    return TrustContext.PRODUCTION


def build_signing_receipt(
    *,
    mode: str,
    version: str,
    tag: str,
    scriptlet_name: str,
    scriptlet_bytes: int,
    scriptlet_sha256: str,
    assets: dict[str, tuple[int, str]],
    public_key_fingerprint: str | None,
    signed: bool,
    signatures_verified: bool,
    production_publisher_authority: bool = False,
    extra: tuple[str, ...] = (),
) -> bytes:
    lines = [
        RECEIPT_HEADER,
        f"schema={RECEIPT_SCHEMA}",
        f"mode={mode}",
        f"version={version}",
        f"tag={tag}",
        f"scriptlet={scriptlet_name}",
        f"scriptlet_bytes={scriptlet_bytes}",
        f"scriptlet_sha256={scriptlet_sha256}",
        f"signing_performed={'true' if signed else 'false'}",
        f"publisher_signatures_verified={'true' if signatures_verified else 'false'}",
        "content_integrity_verified=true",
        "cryptographic_integrity=true",
        "ci_provenance_established_by_this_ceremony=false",
        f"production_publisher_authority={'true' if production_publisher_authority else 'false'}",
        "production_promotion_ready=false",
        "github_mutation=false",
        "scriptlet_executed=false",
        "require_trusted_promotion_path_for_ci_provenance=true",
        "private_key_material_in_output=false",
    ]
    if public_key_fingerprint is not None:
        lines.append(f"publisher_public_key_fingerprint={public_key_fingerprint}")
    for name in sorted(assets):
        size, digest = assets[name]
        lines.append(f"asset={name}|{size}|{digest}")
    lines.extend(extra)
    raw = ("\n".join(lines) + "\n").encode("ascii")
    if len(raw) > MAX_RECEIPT_BYTES:
        raise CeremonyError("signing receipt exceeds size bound")
    return raw


def _refuse_unsafe_paths(
    *,
    output_dir: Path,
    private_key: Path,
    ci_artifacts: Path,
    public_key_path: Path,
) -> None:
    if output_dir.exists() or _lstat_is_symlink(output_dir):
        raise CeremonyError("output destination must be a new unused path")
    out_parent = output_dir.parent
    if not out_parent.is_dir():
        raise CeremonyError("output parent directory does not exist")
    resolved_out = output_dir.resolve()
    for label, path in (
        ("private-key", private_key),
        ("ci-artifacts", ci_artifacts),
        ("public-key", public_key_path),
    ):
        try:
            resolved = path.resolve()
        except OSError as exc:
            raise CeremonyError(f"cannot resolve {label} path: {exc}") from exc
        if resolved == resolved_out:
            raise CeremonyError(f"output destination overlaps {label} path")
        try:
            resolved_out.relative_to(resolved)
            raise CeremonyError(f"output destination is inside {label} path")
        except ValueError:
            pass
        try:
            resolved.relative_to(resolved_out)
            raise CeremonyError(f"{label} path is inside output destination")
        except ValueError:
            pass


def _stage_and_finalize(
    *,
    output_dir: Path,
    writer: Callable[[Path], None],
) -> None:
    """Write into a private staging sibling, then publish only after writer succeeds."""
    token = secrets.token_hex(8)
    staging = output_dir.parent / f".ignite-kindle-ceremony-stage-{os.getpid()}-{token}"
    if staging.exists() or _lstat_is_symlink(staging):
        raise CeremonyError("staging path collision")
    try:
        staging.mkdir(parents=False, exist_ok=False)
        writer(staging)
        if output_dir.exists() or _lstat_is_symlink(output_dir):
            raise CeremonyError("output destination appeared before finalize")
        os.replace(os.fspath(staging), os.fspath(output_dir))
    except BaseException:
        if staging.exists():
            shutil.rmtree(staging, ignore_errors=True)
        raise


def sign_ceremony(
    *,
    tag: str,
    ci_artifacts: Path,
    private_key: Path,
    public_key_path: Path,
    output_dir: Path,
    expected_source_revision: str | None = None,
    hooks: CeremonyHooks | None = None,
) -> dict[str, Path]:
    """Prepare, sign, verify, stage, preflight, and finalize the six-asset bundle."""
    hooks = hooks or CeremonyHooks()
    if not private_key.is_file() or _lstat_is_symlink(private_key):
        raise CeremonyError("offline private key file is unavailable")
    try:
        public_key = load_public_key(public_key_path, official=False)
    except ReleaseSigningError as exc:
        raise CeremonyError(str(exc)) from exc

    _refuse_unsafe_paths(
        output_dir=output_dir,
        private_key=private_key,
        ci_artifacts=ci_artifacts,
        public_key_path=public_key_path,
    )

    prepared = prepare_ceremony(
        tag=tag,
        ci_artifacts=ci_artifacts,
        expected_source_revision=expected_source_revision,
    )
    if hooks.after_prepare is not None:
        hooks.after_prepare()

    # Remeasure before signing — refuse TOCTOU drift after prepare.
    try:
        assert_scriptlet_unchanged(prepared.identity)
    except AuthenticatedAssetError as exc:
        raise CeremonyError(str(exc)) from exc
    current = measure_scriptlet(
        prepared.inputs.scriptlet,
        expected_name=prepared.inputs.scriptlet.name,
    )
    if (
        current.size != prepared.scriptlet_bytes
        or current.sha256 != prepared.scriptlet_sha256
    ):
        raise CeremonyError("Scriptlet changed between verification and signing")
    if _sha256_file(prepared.inputs.measurement) != prepared.measurement_sha256:
        raise CeremonyError("measurement changed between verification and signing")
    if _sha256_file(prepared.inputs.package_profile) != prepared.package_profile_sha256:
        raise CeremonyError("package profile changed between verification and signing")

    if hooks.before_release_sign is not None:
        hooks.before_release_sign()
    try:
        release_sig = sign_release_manifest(
            prepared.release_manifest,
            private_key_path=private_key,
            public_key=public_key,
        )
    except ProductionTrustError as exc:
        raise CeremonyError(str(exc)) from exc
    if hooks.after_release_sign is not None:
        hooks.after_release_sign()

    if hooks.before_binding_sign is not None:
        hooks.before_binding_sign()
    try:
        binding_sig = sign_publication_binding(
            prepared.publication_binding,
            private_key_path=private_key,
            public_key=public_key,
            release_manifest=prepared.release_manifest,
            publication_evidence=prepared.evidence,
        )
    except ProductionTrustError as exc:
        raise CeremonyError(str(exc)) from exc
    if hooks.after_binding_sign is not None:
        hooks.after_binding_sign()

    try:
        authenticate_publication_chain_with_key(
            release_manifest=prepared.release_manifest,
            release_signature=release_sig,
            binding_manifest=prepared.publication_binding,
            binding_signature=binding_sig,
            publication_evidence=prepared.evidence,
            expected_tag=prepared.inputs.tag,
            public_key=public_key,
        )
    except ProductionTrustError as exc:
        raise CeremonyError(
            f"independent signature verification failed: {exc}"
        ) from exc

    try:
        assert_scriptlet_unchanged(prepared.identity)
    except AuthenticatedAssetError as exc:
        raise CeremonyError(str(exc)) from exc
    names = release_asset_basenames(prepared.inputs.version)
    result_paths: dict[str, Path] = {}

    def _write_stage(staging: Path) -> None:
        # Copy Scriptlet and confirm byte identity against signed digest.
        scriptlet_dest = staging / names["scriptlet"]
        shutil.copyfile(prepared.inputs.scriptlet, scriptlet_dest)
        copied = scriptlet_dest.read_bytes()
        if len(copied) != prepared.scriptlet_bytes or _sha256_bytes(copied) != (
            prepared.scriptlet_sha256
        ):
            raise CeremonyError(
                "staged Scriptlet bytes do not match signed Scriptlet identity"
            )

        _write_bytes_exclusive(
            staging / names["release_manifest"], prepared.release_manifest
        )
        _write_bytes_exclusive(staging / names["release_signature"], release_sig)
        _write_bytes_exclusive(
            staging / names["publication_evidence"], prepared.evidence
        )
        _write_bytes_exclusive(
            staging / names["publication_binding"], prepared.publication_binding
        )
        _write_bytes_exclusive(staging / names["binding_signature"], binding_sig)

        promote_dir = staging / PROMOTE_ASSETS_SUBDIR
        promote_dir.mkdir(parents=False, exist_ok=False)
        _write_bytes_exclusive(
            promote_dir / names["publication_evidence"], prepared.evidence
        )
        _write_bytes_exclusive(
            promote_dir / names["publication_binding"], prepared.publication_binding
        )
        _write_bytes_exclusive(promote_dir / names["binding_signature"], binding_sig)

        if hooks.after_stage_write is not None:
            hooks.after_stage_write(staging)

        # Re-verify chain against staged bytes.
        try:
            authenticate_publication_chain_with_key(
                release_manifest=(staging / names["release_manifest"]).read_bytes(),
                release_signature=(staging / names["release_signature"]).read_bytes(),
                binding_manifest=(staging / names["publication_binding"]).read_bytes(),
                binding_signature=(staging / names["binding_signature"]).read_bytes(),
                publication_evidence=(
                    staging / names["publication_evidence"]
                ).read_bytes(),
                expected_tag=prepared.inputs.tag,
                public_key=public_key,
            )
        except ProductionTrustError as exc:
            raise CeremonyError(
                f"staged signature verification failed: {exc}"
            ) from exc

        if hooks.before_preflight is not None:
            hooks.before_preflight(staging)
        trust = _ceremony_preflight_trust(public_key=public_key)
        try:
            report = run_preflight(
                assets_dir=staging,
                expected_tag=prepared.inputs.tag,
                trust=trust,
                public_key_path=(
                    None if trust is TrustContext.PRODUCTION else public_key_path
                ),
            )
        except PreflightError as exc:
            raise CeremonyError(f"six-asset preflight refused: {exc}") from exc
        # Preflight under the official pin may report production_promotion_ready for
        # cryptographic completeness; the ceremony itself never claims promotion
        # readiness — that remains the trusted promote path.
        if (
            trust is TrustContext.FIXTURE
            and report.production_promotion_ready
        ):
            raise CeremonyError(
                "fixture preflight incorrectly claimed production_promotion_ready"
            )
        pin_matched = trust is TrustContext.PRODUCTION
        if pin_matched and report.verification_provenance != (
            "verified-under-production-pin"
        ):
            raise CeremonyError(
                "production-pin preflight did not authenticate under the official pin"
            )

        asset_digests: dict[str, tuple[int, str]] = {}
        for key in (
            "scriptlet",
            "release_manifest",
            "release_signature",
            "publication_evidence",
            "publication_binding",
            "binding_signature",
        ):
            data = (staging / names[key]).read_bytes()
            asset_digests[names[key]] = (len(data), _sha256_bytes(data))

        # Receipt describes validated staged output bytes only.
        staged_scriptlet_sha = asset_digests[names["scriptlet"]][1]
        staged_scriptlet_bytes = asset_digests[names["scriptlet"]][0]
        if (
            staged_scriptlet_sha != prepared.scriptlet_sha256
            or staged_scriptlet_bytes != prepared.scriptlet_bytes
        ):
            raise CeremonyError("staged Scriptlet identity drifted before receipt")

        if pin_matched:
            trust_extra = (
                f"preflight_trust_context={trust.value}",
                f"preflight_verification_provenance={report.verification_provenance}",
                "official_pin_matched_caller_public_key=true",
                "ceremony_success_is_not_production_promotion_readiness=true",
                "ci_provenance_requires_trusted_promotion_path=true",
            )
        else:
            trust_extra = (
                f"preflight_trust_context={trust.value}",
                f"preflight_verification_provenance={report.verification_provenance}",
                "fixture_success_is_not_production_promotion_readiness=true",
                "caller_supplied_key_is_not_production_pin_authority=true",
                "ceremony_success_is_not_production_promotion_readiness=true",
            )

        receipt = build_signing_receipt(
            mode="sign",
            version=prepared.inputs.version,
            tag=prepared.inputs.tag,
            scriptlet_name=names["scriptlet"],
            scriptlet_bytes=staged_scriptlet_bytes,
            scriptlet_sha256=staged_scriptlet_sha,
            assets=asset_digests,
            public_key_fingerprint=_public_key_fingerprint(public_key),
            signed=True,
            signatures_verified=True,
            production_publisher_authority=pin_matched,
            extra=(
                f"measurement_sha256={prepared.measurement_sha256}",
                f"package_profile_sha256={prepared.package_profile_sha256}",
                f"promote_authenticated_assets_dir={PROMOTE_ASSETS_SUBDIR}",
            )
            + trust_extra
            + prepared.handoff_detail,
        )
        receipt_path = staging / RECEIPT_BASENAME_TMPL.format(
            version=prepared.inputs.version
        )
        _write_bytes_exclusive(receipt_path, receipt)

        for path in staging.rglob("*"):
            if not path.is_file():
                continue
            if path.resolve() == private_key.resolve():
                raise CeremonyError("private key path appeared under ceremony output")
            if path.suffix in {".pem", ".key"}:
                raise CeremonyError(
                    f"refusing private-key-like output file: {path.name}"
                )
            if b"PRIVATE KEY" in path.read_bytes()[:64]:
                raise CeremonyError(
                    f"private-key material detected in output: {path.name}"
                )

        if hooks.before_finalize is not None:
            hooks.before_finalize(staging)

        result_paths["receipt"] = Path(
            RECEIPT_BASENAME_TMPL.format(version=prepared.inputs.version)
        )
        result_paths["promote_assets_dir"] = Path(PROMOTE_ASSETS_SUBDIR)
        result_paths["scriptlet"] = Path(names["scriptlet"])

    _stage_and_finalize(output_dir=output_dir, writer=_write_stage)

    return {
        "output_dir": output_dir,
        "receipt": output_dir / result_paths["receipt"],
        "promote_assets_dir": output_dir / result_paths["promote_assets_dir"],
        "scriptlet": output_dir / result_paths["scriptlet"],
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    prepare = sub.add_parser(
        "prepare",
        help="Verify CI packaging inputs and report readiness (no private key)",
    )
    prepare.add_argument("--tag", required=True)
    prepare.add_argument("--ci-artifacts", type=Path, required=True)
    prepare.add_argument(
        "--expected-source-revision",
        default=None,
        help="operator assertion only; not CI provenance proof",
    )
    prepare.add_argument(
        "--receipt-out",
        type=Path,
        default=None,
        help="optional path to write the prepare receipt",
    )

    sign = sub.add_parser(
        "sign",
        help="Verify inputs and sign the authenticated six-asset Kindle bundle",
    )
    sign.add_argument("--tag", required=True)
    sign.add_argument("--ci-artifacts", type=Path, required=True)
    sign.add_argument("--private-key", type=Path, required=True)
    sign.add_argument("--public-key", type=Path, required=True)
    sign.add_argument("--output", type=Path, required=True)
    sign.add_argument("--expected-source-revision", default=None)

    bind = sub.add_parser(
        "sign-binding",
        help="Sign a canonical publication binding with an offline private key",
    )
    bind.add_argument("--binding", type=Path, required=True)
    bind.add_argument("--release-manifest", type=Path, required=True)
    bind.add_argument("--publication-evidence", type=Path, required=True)
    bind.add_argument("--private-key", type=Path, required=True)
    bind.add_argument("--public-key", type=Path, required=True)
    bind.add_argument("--out", type=Path, required=True)

    args = parser.parse_args(argv)
    try:
        if args.command == "prepare":
            prepared = prepare_ceremony(
                tag=args.tag,
                ci_artifacts=args.ci_artifacts,
                expected_source_revision=args.expected_source_revision,
            )
            sys.stdout.write("\n".join(prepared.readiness_lines) + "\n")
            if args.receipt_out is not None:
                _refuse_standalone_output(args.receipt_out, label="receipt-out")
                receipt = build_signing_receipt(
                    mode="prepare",
                    version=prepared.inputs.version,
                    tag=prepared.inputs.tag,
                    scriptlet_name=prepared.inputs.scriptlet.name,
                    scriptlet_bytes=prepared.scriptlet_bytes,
                    scriptlet_sha256=prepared.scriptlet_sha256,
                    assets={
                        prepared.inputs.scriptlet.name: (
                            prepared.scriptlet_bytes,
                            prepared.scriptlet_sha256,
                        ),
                        prepared.inputs.measurement.name: (
                            prepared.inputs.measurement.stat().st_size,
                            prepared.measurement_sha256,
                        ),
                        prepared.inputs.package_profile.name: (
                            prepared.inputs.package_profile.stat().st_size,
                            prepared.package_profile_sha256,
                        ),
                    },
                    public_key_fingerprint=None,
                    signed=False,
                    signatures_verified=False,
                    production_publisher_authority=False,
                    extra=tuple(prepared.handoff_detail),
                )
                _write_bytes_exclusive(args.receipt_out, receipt)
                print(f"receipt={args.receipt_out}")
            return 0
        if args.command == "sign":
            written = sign_ceremony(
                tag=args.tag,
                ci_artifacts=args.ci_artifacts,
                private_key=args.private_key,
                public_key_path=args.public_key,
                output_dir=args.output,
                expected_source_revision=args.expected_source_revision,
            )
            print(f"output_dir={written['output_dir']}")
            print(f"receipt={written['receipt']}")
            print(f"promote_authenticated_assets_dir={written['promote_assets_dir']}")
            print("github_mutation=false")
            print("ci_provenance_established_by_this_ceremony=false")
            print("production_promotion_ready=false")
            print("publisher_signatures_verified=true")
            return 0
        if args.command == "sign-binding":
            _refuse_standalone_output(args.out, label="sign-binding --out")
            try:
                public_key = load_public_key(args.public_key, official=False)
            except ReleaseSigningError as exc:
                raise CeremonyError(str(exc)) from exc
            binding = _read_contract_bytes(
                args.binding, label="binding", max_bytes=MAX_BINDING_BYTES
            )
            release_manifest = _read_contract_bytes(
                args.release_manifest,
                label="release-manifest",
                max_bytes=MAX_RELEASE_MANIFEST_BYTES,
            )
            evidence = _read_contract_bytes(
                args.publication_evidence,
                label="publication-evidence",
                max_bytes=MAX_EVIDENCE_BYTES,
            )
            try:
                envelope = sign_publication_binding(
                    binding,
                    private_key_path=args.private_key,
                    public_key=public_key,
                    release_manifest=release_manifest,
                    publication_evidence=evidence,
                )
            except ProductionTrustError as exc:
                raise CeremonyError(str(exc)) from exc
            _write_bytes_exclusive(args.out, envelope)
            print(f"binding_signature={args.out}")
            print(f"binding_sha256={_sha256_bytes(binding)}")
            print(f"signature_sha256={_sha256_bytes(envelope)}")
            print("github_mutation=false")
            return 0
    except (
        CeremonyError,
        HandoffError,
        MeasurementError,
        ProductionTrustError,
        ReleaseSigningError,
        AuthenticatedAssetError,
        OSError,
    ) as exc:
        print(f"offline_release_ceremony: {exc}", file=sys.stderr)
        return 1
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
