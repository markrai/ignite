#!/usr/bin/env python3
"""Trusted CI packaging measurement artifact (Slice 3D-B2a.4.3l).

Captures the packaging-time observation of the exact CI-built Kindle Scriptlet
and its 22-role component inventory. This is build evidence for the offline
signing ceremony — not publisher authentication and not a GitHub Release asset.

Independently recoverable from Scriptlet bytes (without executing the Scriptlet):
  - Scriptlet size / SHA-256
  - Per-member size / SHA-256 / tar mode (via gzip+ustar inventory)

Requires packaging contracts / packaging-time observation:
  - Role↔path binding, classifications, candidate-B mappings
  - Package profile framing / package_profile_sha256
  - Explicit association with CI run / source revision (operator or workflow)

Never infer component identities from the Scriptlet hash alone.
Never claim production publisher authority from this artifact alone.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import stat
import sys
from dataclasses import dataclass
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_SFX = _HERE / "sfx"
_RELEASE = _HERE.parents[1] / "tools" / "release"
for extra in (_HERE, _SFX, str(_RELEASE)):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from build_sfx import split_sfx  # noqa: E402
from production_publication_evidence import (  # noqa: E402
    MAX_SCRIPTLET_BYTES,
    ComponentClaim,
    build_production_evidence_from_claims,
    build_production_publication_evidence,
    enforce_presence_policy,
    expected_mode_token,
    parse_and_validate_production_evidence,
)
from release_component_profiles import (  # noqa: E402
    CANDIDATE_B_DESTINATION,
    COMPONENT_ROLES,
    PACKAGE_CLASS,
    PACKAGE_RELATIVE,
    ProfileError,
    render_profile,
)
from verify_sfx_payload_equivalence import (  # noqa: E402
    EquivalenceError,
    inspect_embedded_inventory,
)

MEASUREMENT_HEADER = "IGNITE_CI_PACKAGING_MEASUREMENT_V1"
MEASUREMENT_SCHEMA = "1"
MEASUREMENT_SCOPE = "ci-build-evidence-only"
MEASUREMENT_BASENAME = "ignite-kindle-ci-packaging-measurement.v1"
PACKAGE_PROFILE_BASENAME = "ignite-kindle-component-profile-package.v1"
EQUIVALENCE_BASENAME = "sfx-payload-equivalence.v1"
MEASUREMENTS_SUBDIR = "packaging-measurements"

MAX_MEASUREMENT_BYTES = 64 * 1024
MAX_PROFILE_BYTES = 64 * 1024
READ_CHUNK = 1024 * 1024

_SHA_RE = re.compile(r"^[0-9a-f]{64}$")
_BYTES_RE = re.compile(r"^(0|[1-9][0-9]*)$")
_VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$")
_COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
_RUN_RE = re.compile(r"^[1-9][0-9]*$")
_COMPONENT_RE = re.compile(
    r"^component=(?P<role>[a-z0-9-]+)\|(?P<presence>file|absent)\|"
    r"(?P<size>0|[1-9][0-9]*)\|(?P<digest>-|[0-9a-f]{64})\|"
    r"(?P<klass>direct-copy|optional|transformed|generated|shared)\|"
    r"(?P<mode>-|0[0-7]{3})\|(?P<mapping>[A-Za-z0-9_./ -]+|-)$"
)


class MeasurementError(ValueError):
    """Fail-closed CI packaging measurement refusal."""


@dataclass(frozen=True)
class MeasurementVerification:
    """Separated verification outcomes — never collapsed into verified=true."""

    version: str
    tag: str
    scriptlet_integrity: bool
    scriptlet_identity_match: bool
    component_consistency: bool
    package_profile_match: bool
    operator_asserted_source_revision: str | None
    operator_asserted_workflow_run_id: str | None
    ci_provenance_established_by_this_verifier: bool
    production_publisher_authority: bool
    measurement_scope: str
    detail: tuple[str, ...]

    def summary_lines(self) -> list[str]:
        lines = [
            "IGNITE_CI_PACKAGING_MEASUREMENT_VERIFY_V1",
            "schema=1",
            f"version={self.version}",
            f"tag={self.tag}",
            f"scriptlet_integrity={'true' if self.scriptlet_integrity else 'false'}",
            f"scriptlet_identity_match={'true' if self.scriptlet_identity_match else 'false'}",
            f"component_consistency={'true' if self.component_consistency else 'false'}",
            f"package_profile_match={'true' if self.package_profile_match else 'false'}",
            f"measurement_scope={self.measurement_scope}",
            "ci_provenance_established_by_this_verifier=false",
            "production_publisher_authority=false",
            "measurement_is_not_publisher_signed=true",
            "github_mutation=false",
            "scriptlet_executed=false",
        ]
        if self.operator_asserted_source_revision is not None:
            lines.append(
                f"operator_asserted_source_revision={self.operator_asserted_source_revision}"
            )
            lines.append(
                "operator_asserted_source_revision_is_not_ci_provenance_proof=true"
            )
        if self.operator_asserted_workflow_run_id is not None:
            lines.append(
                f"operator_asserted_workflow_run_id={self.operator_asserted_workflow_run_id}"
            )
            lines.append(
                "operator_asserted_workflow_run_id_is_not_ci_provenance_proof=true"
            )
        lines.extend(self.detail)
        ready = (
            self.scriptlet_integrity
            and self.scriptlet_identity_match
            and self.component_consistency
            and self.package_profile_match
        )
        lines.append(f"offline_evidence_inputs_ready={'true' if ready else 'false'}")
        lines.append("production_promotion_ready=false")
        return lines


def _sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def read_bounded_plain_file(
    path: Path,
    *,
    max_bytes: int,
    label: str,
    min_bytes: int = 1,
) -> bytes:
    """Length-limited O_NOFOLLOW read of an untrusted evidence metadata file.

    Refuses empty, oversized, symlinked, and non-regular inputs.
    """
    path_str = os.fspath(path)
    try:
        before = os.lstat(path_str)
    except OSError as exc:
        raise MeasurementError(f"cannot lstat {label}: {exc}") from exc
    if stat.S_ISLNK(before.st_mode):
        raise MeasurementError(f"symlink refused: {label}")
    if not stat.S_ISREG(before.st_mode):
        raise MeasurementError(f"not a regular file: {label}")
    if before.st_size < min_bytes or before.st_size > max_bytes:
        raise MeasurementError(
            f"size out of bounds for {label}: {before.st_size} "
            f"(allowed {min_bytes}..{max_bytes})"
        )

    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    if hasattr(os, "O_BINARY"):
        flags |= os.O_BINARY
    try:
        fd = os.open(path_str, flags)
    except OSError as exc:
        raise MeasurementError(f"cannot open {label}: {exc}") from exc
    try:
        opened = os.fstat(fd)
        if stat.S_ISLNK(opened.st_mode) or not stat.S_ISREG(opened.st_mode):
            raise MeasurementError(f"opened path is not a plain file: {label}")
        if opened.st_size != before.st_size:
            raise MeasurementError(f"size changed before read: {label}")
        if sys.platform != "win32":
            if opened.st_dev != before.st_dev or opened.st_ino != before.st_ino:
                raise MeasurementError(f"identity changed before read: {label}")
        remaining = opened.st_size
        chunks: list[bytes] = []
        while remaining > 0:
            chunk = os.read(fd, min(READ_CHUNK, remaining))
            if not chunk:
                raise MeasurementError(f"truncated while reading: {label}")
            remaining -= len(chunk)
            chunks.append(chunk)
        if os.read(fd, 1):
            raise MeasurementError(f"file grew while reading: {label}")
        after = os.fstat(fd)
        if after.st_size != opened.st_size:
            raise MeasurementError(f"file substituted while reading: {label}")
        data = b"".join(chunks)
        if len(data) != opened.st_size:
            raise MeasurementError(f"size mismatch while reading: {label}")
        if not data:
            raise MeasurementError(f"empty {label}")
        return data
    finally:
        os.close(fd)


def observed_from_embedded_inventory(
    embedded: dict[str, tuple[int, str, int]],
) -> dict[str, tuple[str, int, str]]:
    """Map Scriptlet inventory + packaging contracts to profile observation tuples."""
    observed: dict[str, tuple[str, int, str]] = {}
    for role in COMPONENT_ROLES:
        rel = PACKAGE_RELATIVE[role]
        if rel is None or rel not in embedded:
            presence = "absent"
            observed[role] = ("absent", 0, "-")
        else:
            size, digest, _mode = embedded[rel]
            presence = "file"
            observed[role] = ("file", size, digest)
        enforce_presence_policy(role, presence)
    return observed


def reconstruct_canonical_package_profile(
    *,
    version: str,
    embedded: dict[str, tuple[int, str, int]],
) -> bytes:
    """Rebuild package profile bytes from Scriptlet inventory + release contracts."""
    observed = observed_from_embedded_inventory(embedded)
    try:
        return render_profile(profile="package", version=version, observed=observed)
    except ProfileError as exc:
        raise MeasurementError(f"cannot reconstruct package profile: {exc}") from exc


def claims_from_payload_and_contracts(
    *,
    version: str,
    payload: Path,
) -> list[ComponentClaim]:
    """Build 22 role claims using packaging contracts + staged payload observation."""
    from release_component_profiles import inspect_package_components

    if payload.name != f"Ignite-Kindle-{version}":
        raise MeasurementError("payload directory name mismatch")
    digests = inspect_package_components(payload)
    components: list[ComponentClaim] = []
    for role in COMPONENT_ROLES:
        presence, size, digest = digests[role]
        enforce_presence_policy(role, presence)
        klass = PACKAGE_CLASS[role]
        mapping = CANDIDATE_B_DESTINATION[role]
        if presence == "file":
            mode = expected_mode_token(role, "file")
            components.append(
                ComponentClaim(
                    role=role,
                    presence="file",
                    size=size,
                    sha256=digest,
                    classification=klass,
                    mode=mode,
                    mapping=mapping,
                )
            )
        else:
            components.append(
                ComponentClaim(
                    role=role,
                    presence="absent",
                    size=0,
                    sha256="-",
                    classification=klass,
                    mode="-",
                    mapping=mapping,
                )
            )
    return components


def build_ci_packaging_measurement(
    *,
    version: str,
    scriptlet: Path,
    payload: Path,
    package_profile: bytes,
    source_revision: str | None = None,
    workflow_run_id: str | None = None,
    archive_sha256: str | None = None,
    equivalence_result: str = "pass",
) -> bytes:
    """Build the canonical measurement artifact from real packaging inputs."""
    if not _VERSION_RE.fullmatch(version):
        raise MeasurementError("noncanonical version")
    if source_revision is not None and not _COMMIT_RE.fullmatch(source_revision):
        raise MeasurementError("source_revision must be 40 lowercase hex characters")
    if workflow_run_id is not None and not _RUN_RE.fullmatch(workflow_run_id):
        raise MeasurementError("workflow_run_id must be a positive integer")
    if equivalence_result not in {"pass", "fail"}:
        raise MeasurementError("equivalence_result must be pass or fail")
    if equivalence_result != "pass":
        raise MeasurementError("refusing to emit measurement for failed equivalence")

    expected_name = f"Install Ignite v{version}.sh"
    if scriptlet.name != expected_name:
        raise MeasurementError(f"scriptlet basename must be {expected_name!r}")
    blob = read_bounded_plain_file(
        scriptlet,
        max_bytes=MAX_SCRIPTLET_BYTES,
        label="scriptlet",
        min_bytes=1,
    )
    scriptlet_bytes = len(blob)
    scriptlet_sha256 = _sha256_bytes(blob)
    if len(package_profile) == 0 or len(package_profile) > MAX_PROFILE_BYTES:
        raise MeasurementError("package profile size refused")
    if b"\r" in package_profile:
        raise MeasurementError("package profile must be LF-only")
    profile_sha = _sha256_bytes(package_profile)

    # Derive archive digest from Scriptlet when not supplied.
    if archive_sha256 is None:
        try:
            _prefix, archive, meta = split_sfx(blob)
        except ValueError as exc:
            raise MeasurementError(f"SFX split failed: {exc}") from exc
        archive_sha256 = meta["sha256"]
        if _sha256_bytes(archive) != archive_sha256:
            raise MeasurementError("embedded archive SHA-256 stamp mismatch")
    if not _SHA_RE.fullmatch(archive_sha256):
        raise MeasurementError("archive_sha256 malformed")

    components = claims_from_payload_and_contracts(version=version, payload=payload)
    # Cross-check: production evidence builder must accept the same observation.
    evidence = build_production_publication_evidence(
        version=version,
        payload=payload,
        scriptlet_bytes=scriptlet_bytes,
        scriptlet_sha256=scriptlet_sha256,
    )
    validated = parse_and_validate_production_evidence(
        evidence,
        expected_version=version,
        expected_scriptlet_sha256=scriptlet_sha256,
        expected_scriptlet_bytes=scriptlet_bytes,
    )
    if validated.package_profile_sha256 != profile_sha:
        raise MeasurementError(
            "package profile sha256 does not match payload observation"
        )

    tag = f"v{version}"
    lines = [
        MEASUREMENT_HEADER,
        f"schema={MEASUREMENT_SCHEMA}",
        f"version={version}",
        f"tag={tag}",
        f"scriptlet_artifact=Install Ignite v{version}.sh",
        f"scriptlet_bytes={scriptlet_bytes}",
        f"scriptlet_sha256={scriptlet_sha256}",
        f"payload_dirname=Ignite-Kindle-{version}",
        f"package_profile_sha256={profile_sha}",
        f"archive_sha256={archive_sha256}",
        f"equivalence_result={equivalence_result}",
        f"component_role_count={len(COMPONENT_ROLES)}",
    ]
    if source_revision is not None:
        lines.append(f"source_revision={source_revision}")
    if workflow_run_id is not None:
        lines.append(f"workflow_run_id={workflow_run_id}")
    by_role = {c.role: c for c in components}
    for role in COMPONENT_ROLES:
        claim = by_role[role]
        if claim.presence == "file":
            lines.append(
                f"component={role}|file|{claim.size}|{claim.sha256}|"
                f"{claim.classification}|{claim.mode}|{claim.mapping}"
            )
        else:
            lines.append(
                f"component={role}|absent|0|-|{claim.classification}|-|{claim.mapping}"
            )
    lines.append(f"measurement_scope={MEASUREMENT_SCOPE}")
    raw = ("\n".join(lines) + "\n").encode("ascii")
    if len(raw) > MAX_MEASUREMENT_BYTES:
        raise MeasurementError("measurement exceeds size bound")
    return raw


def parse_ci_packaging_measurement(raw: bytes) -> dict:
    """Strict parse of the measurement artifact (unsigned build evidence)."""
    if not raw:
        raise MeasurementError("measurement must not be empty")
    if len(raw) > MAX_MEASUREMENT_BYTES or b"\r" in raw:
        raise MeasurementError("measurement refused")
    text = raw.decode("ascii")
    if not text.endswith("\n"):
        raise MeasurementError("measurement must end with LF")
    lines = text[:-1].split("\n")
    if len(lines) < 13 + len(COMPONENT_ROLES):
        raise MeasurementError("measurement line count too short")
    if lines[0] != MEASUREMENT_HEADER or lines[1] != f"schema={MEASUREMENT_SCHEMA}":
        raise MeasurementError("unsupported measurement schema")

    def field(prefix: str, line: str) -> str:
        if not line.startswith(prefix):
            raise MeasurementError(f"missing field {prefix}")
        return line[len(prefix) :]

    version = field("version=", lines[2])
    tag = field("tag=", lines[3])
    scriptlet_artifact = field("scriptlet_artifact=", lines[4])
    scriptlet_bytes_token = field("scriptlet_bytes=", lines[5])
    scriptlet_sha256 = field("scriptlet_sha256=", lines[6])
    payload_dirname = field("payload_dirname=", lines[7])
    package_profile_sha256 = field("package_profile_sha256=", lines[8])
    archive_sha256 = field("archive_sha256=", lines[9])
    equivalence_result = field("equivalence_result=", lines[10])
    role_count = field("component_role_count=", lines[11])

    if not _VERSION_RE.fullmatch(version):
        raise MeasurementError("noncanonical version")
    if tag != f"v{version}":
        raise MeasurementError("tag/version mismatch")
    if scriptlet_artifact != f"Install Ignite v{version}.sh":
        raise MeasurementError("scriptlet artifact mismatch")
    if payload_dirname != f"Ignite-Kindle-{version}":
        raise MeasurementError("payload dirname mismatch")
    if not _BYTES_RE.fullmatch(scriptlet_bytes_token):
        raise MeasurementError("noncanonical scriptlet_bytes")
    if not _SHA_RE.fullmatch(scriptlet_sha256):
        raise MeasurementError("scriptlet_sha256 malformed")
    if not _SHA_RE.fullmatch(package_profile_sha256):
        raise MeasurementError("package_profile_sha256 malformed")
    if not _SHA_RE.fullmatch(archive_sha256):
        raise MeasurementError("archive_sha256 malformed")
    if equivalence_result != "pass":
        raise MeasurementError("equivalence_result must be pass")
    if role_count != str(len(COMPONENT_ROLES)):
        raise MeasurementError("component_role_count mismatch")

    idx = 12
    source_revision = None
    workflow_run_id = None
    while idx < len(lines) and not lines[idx].startswith("component="):
        if lines[idx].startswith("source_revision="):
            source_revision = field("source_revision=", lines[idx])
            if not _COMMIT_RE.fullmatch(source_revision):
                raise MeasurementError("source_revision malformed")
        elif lines[idx].startswith("workflow_run_id="):
            workflow_run_id = field("workflow_run_id=", lines[idx])
            if not _RUN_RE.fullmatch(workflow_run_id):
                raise MeasurementError("workflow_run_id malformed")
        else:
            raise MeasurementError(f"unexpected field before components: {lines[idx]}")
        idx += 1

    components: list[ComponentClaim] = []
    for role in COMPONENT_ROLES:
        if idx >= len(lines):
            raise MeasurementError("missing component lines")
        match = _COMPONENT_RE.fullmatch(lines[idx])
        if match is None:
            raise MeasurementError(f"malformed component line: {lines[idx]}")
        if match.group("role") != role:
            raise MeasurementError(f"component role order mismatch at {role}")
        presence = match.group("presence")
        enforce_presence_policy(role, presence)
        klass = match.group("klass")
        if klass != PACKAGE_CLASS[role]:
            raise MeasurementError(f"classification mismatch for {role}")
        mapping = match.group("mapping")
        if mapping != CANDIDATE_B_DESTINATION[role]:
            raise MeasurementError(f"mapping mismatch for {role}")
        size = int(match.group("size"))
        digest = match.group("digest")
        mode = match.group("mode")
        if presence == "file":
            if digest == "-" or size <= 0 or mode == "-":
                raise MeasurementError(f"file claim malformed for {role}")
            if mode != expected_mode_token(role, "file"):
                raise MeasurementError(f"mode mismatch for {role}")
        else:
            if digest != "-" or size != 0 or mode != "-":
                raise MeasurementError(f"absent claim malformed for {role}")
        components.append(
            ComponentClaim(
                role=role,
                presence=presence,
                size=size,
                sha256=digest,
                classification=klass,
                mode=mode,
                mapping=mapping,
            )
        )
        idx += 1

    if idx >= len(lines) or lines[idx] != f"measurement_scope={MEASUREMENT_SCOPE}":
        raise MeasurementError("measurement_scope missing or incorrect")
    idx += 1
    if idx != len(lines):
        raise MeasurementError("unexpected trailing measurement lines")

    return {
        "version": version,
        "tag": tag,
        "scriptlet_artifact": scriptlet_artifact,
        "scriptlet_bytes": int(scriptlet_bytes_token),
        "scriptlet_sha256": scriptlet_sha256,
        "payload_dirname": payload_dirname,
        "package_profile_sha256": package_profile_sha256,
        "archive_sha256": archive_sha256,
        "equivalence_result": equivalence_result,
        "source_revision": source_revision,
        "workflow_run_id": workflow_run_id,
        "components": tuple(components),
        "raw": raw,
    }


def verify_measurement_against_scriptlet(
    *,
    measurement: bytes,
    scriptlet: Path,
    package_profile: bytes | None = None,
    expected_version: str | None = None,
    expected_source_revision: str | None = None,
) -> MeasurementVerification:
    """Independently verify measurement claims against Scriptlet bytes.

    Does not execute the Scriptlet. Does not establish GitHub CI provenance or
    production publisher authority.
    """
    parsed = parse_ci_packaging_measurement(measurement)
    version = parsed["version"]
    detail: list[str] = []

    if expected_version is not None and expected_version != version:
        raise MeasurementError(
            f"version mismatch: measurement {version} vs expected {expected_version}"
        )
    if expected_source_revision is not None:
        if parsed["source_revision"] is None:
            raise MeasurementError("measurement missing source_revision")
        if parsed["source_revision"] != expected_source_revision:
            raise MeasurementError("source_revision mismatch")

    if scriptlet.name != parsed["scriptlet_artifact"]:
        raise MeasurementError(
            f"scriptlet basename must be {parsed['scriptlet_artifact']!r}"
        )
    blob = read_bounded_plain_file(
        scriptlet,
        max_bytes=MAX_SCRIPTLET_BYTES,
        label="scriptlet",
        min_bytes=1,
    )
    scriptlet_size = len(blob)
    scriptlet_sha = _sha256_bytes(blob)
    scriptlet_integrity = True
    scriptlet_identity_match = (
        scriptlet_size == parsed["scriptlet_bytes"]
        and scriptlet_sha == parsed["scriptlet_sha256"]
    )
    if not scriptlet_identity_match:
        raise MeasurementError(
            "scriptlet identity does not match measurement "
            f"(have {scriptlet_size}/{scriptlet_sha}, "
            f"measurement {parsed['scriptlet_bytes']}/{parsed['scriptlet_sha256']})"
        )

    try:
        _prefix, archive, meta = split_sfx(blob)
        embedded = inspect_embedded_inventory(archive, version)
    except (ValueError, EquivalenceError) as exc:
        raise MeasurementError(f"cannot inspect Scriptlet payload: {exc}") from exc
    if meta["sha256"] != parsed["archive_sha256"]:
        raise MeasurementError("archive_sha256 does not match Scriptlet stamp")
    if _sha256_bytes(archive) != parsed["archive_sha256"]:
        raise MeasurementError("archive bytes do not match archive_sha256")

    # Compare each file claim to embedded inventory via PACKAGE_RELATIVE.
    for claim in parsed["components"]:
        rel = PACKAGE_RELATIVE[claim.role]
        if claim.presence == "absent":
            if rel is not None and rel in embedded:
                raise MeasurementError(
                    f"role {claim.role} claimed absent but present in Scriptlet"
                )
            continue
        if rel is None:
            raise MeasurementError(f"role {claim.role} cannot be a packaged file")
        if rel not in embedded:
            raise MeasurementError(
                f"role {claim.role} missing from Scriptlet inventory ({rel})"
            )
        size, digest, mode = embedded[rel]
        mode_token = f"{mode:04o}"
        if size != claim.size or digest != claim.sha256 or mode_token != claim.mode:
            raise MeasurementError(
                f"role {claim.role} Scriptlet inventory mismatch "
                f"(embedded {size}/{digest}/{mode_token} vs "
                f"measurement {claim.size}/{claim.sha256}/{claim.mode})"
            )

    # Extra archive members that map to no role must still be accounted for:
    # every embedded member must be a known PACKAGE_RELATIVE path.
    known_rels = {p for p in PACKAGE_RELATIVE.values() if p is not None}
    extras = sorted(set(embedded) - known_rels)
    if extras:
        raise MeasurementError(
            "Scriptlet contains unmapped members: " + ", ".join(extras)
        )

    component_consistency = True
    detail.append("component_digests_recovered_from_scriptlet_bytes=true")
    detail.append("component_classifications_from_packaging_contract=true")

    # Independently reconstruct the canonical package profile from Scriptlet
    # inventory + release_component_profiles contracts. Digest agreement with the
    # unsigned measurement is necessary but never sufficient.
    canonical_profile = reconstruct_canonical_package_profile(
        version=version, embedded=embedded
    )
    canonical_sha = _sha256_bytes(canonical_profile)
    package_profile_match = True
    if package_profile is not None:
        if len(package_profile) == 0 or len(package_profile) > MAX_PROFILE_BYTES:
            raise MeasurementError("package profile size refused")
        if b"\r" in package_profile:
            raise MeasurementError("package profile must be LF-only")
        if package_profile != canonical_profile:
            raise MeasurementError(
                "package profile contents do not match canonical reconstruction "
                "from Scriptlet inventory and packaging contracts"
            )
        if _sha256_bytes(package_profile) != parsed["package_profile_sha256"]:
            raise MeasurementError("package profile bytes do not match measurement")
        if canonical_sha != parsed["package_profile_sha256"]:
            raise MeasurementError(
                "measurement package_profile_sha256 does not match "
                "canonical reconstructed profile"
            )
        detail.append("package_profile_contents_match_canonical_reconstruction=true")
        detail.append("package_profile_digest_matches_measurement=true")
    else:
        detail.append("package_profile_bytes_not_supplied=true")
        package_profile_match = False
        detail.append(
            "package_profile_match_requires_attached_profile_and_canonical_reconstruction=true"
        )
        if canonical_sha != parsed["package_profile_sha256"]:
            raise MeasurementError(
                "measurement package_profile_sha256 does not match "
                "canonical reconstruction from Scriptlet inventory"
            )
        detail.append(
            "measurement_profile_digest_matches_canonical_without_attached_bytes=true"
        )

    return MeasurementVerification(
        version=version,
        tag=parsed["tag"],
        scriptlet_integrity=scriptlet_integrity,
        scriptlet_identity_match=scriptlet_identity_match,
        component_consistency=component_consistency,
        package_profile_match=package_profile_match,
        operator_asserted_source_revision=parsed["source_revision"],
        operator_asserted_workflow_run_id=parsed["workflow_run_id"],
        ci_provenance_established_by_this_verifier=False,
        production_publisher_authority=False,
        measurement_scope=MEASUREMENT_SCOPE,
        detail=tuple(detail),
    )


def build_production_evidence_from_measurement(
    measurement: bytes,
    *,
    scriptlet: Path,
    package_profile: bytes,
) -> bytes:
    """Produce production-format evidence bytes from a validated measurement.

    Caller must still sign under a disposable or production key separately.
    Requires attached package profile bytes and Scriptlet revalidation.
    """
    result = verify_measurement_against_scriptlet(
        measurement=measurement,
        scriptlet=scriptlet,
        package_profile=package_profile,
    )
    if not (
        result.scriptlet_identity_match
        and result.component_consistency
        and result.package_profile_match
    ):
        raise MeasurementError("measurement not ready for evidence generation")
    parsed = parse_ci_packaging_measurement(measurement)
    return build_production_evidence_from_claims(
        version=parsed["version"],
        scriptlet_bytes=parsed["scriptlet_bytes"],
        scriptlet_sha256=parsed["scriptlet_sha256"],
        package_profile_sha256=parsed["package_profile_sha256"],
        components=list(parsed["components"]),
    )


def write_packaging_measurements(
    *,
    out_dir: Path,
    measurement: bytes,
    package_profile: bytes,
    equivalence_report: bytes | None = None,
) -> dict[str, Path]:
    """Write measurement bundle; refuse overwrites."""
    out_dir.mkdir(parents=True, exist_ok=True)
    written: dict[str, Path] = {}
    mapping = {
        MEASUREMENT_BASENAME: measurement,
        PACKAGE_PROFILE_BASENAME: package_profile,
    }
    if equivalence_report is not None:
        mapping[EQUIVALENCE_BASENAME] = equivalence_report
    for name, content in mapping.items():
        path = out_dir / name
        if path.exists():
            raise MeasurementError(f"refusing to overwrite {path}")
        path.write_bytes(content)
        written[name] = path
    return written


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="CI packaging measurement build/verify (unsigned build evidence)"
    )
    sub = parser.add_subparsers(dest="command", required=True)

    build = sub.add_parser("build", help="Emit measurement from packaging outputs")
    build.add_argument("--version", required=True)
    build.add_argument("--scriptlet", type=Path, required=True)
    build.add_argument("--payload", type=Path, required=True)
    build.add_argument("--package-profile", type=Path, required=True)
    build.add_argument("--out-dir", type=Path, required=True)
    build.add_argument("--equivalence-report", type=Path, default=None)
    build.add_argument("--source-revision", default=None)
    build.add_argument("--workflow-run-id", default=None)

    verify = sub.add_parser(
        "verify",
        help="Verify measurement against Scriptlet bytes (no Scriptlet execution)",
    )
    verify.add_argument("--measurement", type=Path, required=True)
    verify.add_argument("--scriptlet", type=Path, required=True)
    verify.add_argument("--package-profile", type=Path, default=None)
    verify.add_argument("--expected-version", default=None)
    verify.add_argument("--expected-source-revision", default=None)

    args = parser.parse_args(argv)
    try:
        if args.command == "build":
            package_profile = read_bounded_plain_file(
                args.package_profile,
                max_bytes=MAX_PROFILE_BYTES,
                label="package-profile",
            )
            measurement = build_ci_packaging_measurement(
                version=args.version,
                scriptlet=args.scriptlet,
                payload=args.payload,
                package_profile=package_profile,
                source_revision=args.source_revision,
                workflow_run_id=args.workflow_run_id,
            )
            eq = None
            if args.equivalence_report is not None:
                eq = read_bounded_plain_file(
                    args.equivalence_report,
                    max_bytes=MAX_MEASUREMENT_BYTES,
                    label="equivalence-report",
                )
            written = write_packaging_measurements(
                out_dir=args.out_dir,
                measurement=measurement,
                package_profile=package_profile,
                equivalence_report=eq,
            )
            print(f"wrote={written[MEASUREMENT_BASENAME]}")
            print(f"measurement_sha256={_sha256_bytes(measurement)}")
            return 0
        if args.command == "verify":
            measurement = read_bounded_plain_file(
                args.measurement,
                max_bytes=MAX_MEASUREMENT_BYTES,
                label="measurement",
            )
            profile = None
            if args.package_profile is not None:
                profile = read_bounded_plain_file(
                    args.package_profile,
                    max_bytes=MAX_PROFILE_BYTES,
                    label="package-profile",
                )
            result = verify_measurement_against_scriptlet(
                measurement=measurement,
                scriptlet=args.scriptlet,
                package_profile=profile,
                expected_version=args.expected_version,
                expected_source_revision=args.expected_source_revision,
            )
            sys.stdout.write("\n".join(result.summary_lines()) + "\n")
            if not (result.scriptlet_identity_match and result.component_consistency):
                return 1
            return 0
    except (MeasurementError, OSError) as exc:
        print(f"ci_packaging_measurement: {exc}", file=sys.stderr)
        return 1
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
