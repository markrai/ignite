#!/usr/bin/env python3
"""Canonical production publication evidence (Slice 3D-B2a.4.3h).

Schema ``IGNITE_PUBLICATION_EVIDENCE_PRODUCTION_V1`` binds Scriptlet identity and
the closed 22-role component inventory under an explicit production publisher
scope. It is distinct from the 3f ``test-trust-root-only`` evidence format.

Component contracts come exclusively from ``release_component_profiles`` and
SFX member-mode rules — not a second mapping table.
"""

from __future__ import annotations

import hashlib
import re
import sys
from dataclasses import dataclass
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_SFX = _HERE / "sfx"
for extra in (_HERE, _SFX):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from release_component_profiles import (  # noqa: E402
    CANDIDATE_B_DESTINATION,
    COMPONENT_ROLES,
    MANDATORY_PACKAGE_ROLES,
    PACKAGE_CLASS,
    PACKAGE_RELATIVE,
    generate_profiles,
    inspect_package_components,
)
from verify_sfx_payload_equivalence import expected_member_mode  # noqa: E402

EVIDENCE_HEADER = "IGNITE_PUBLICATION_EVIDENCE_PRODUCTION_V1"
EVIDENCE_SCHEMA = "1"
PRODUCTION_SCOPE = "production-pin-required"
TEST_SCOPE_MARKER = "publisher_authentication_scope=test-trust-root-only"
CONTRACT_HEADER = "IGNITE_PUBLICATION_COMPONENT_CONTRACT_V1"
CONTRACT_SCHEMA = "1"

MAX_EVIDENCE_BYTES = 64 * 1024
MAX_COMPONENT_BYTES = 512 * 1024 * 1024
MAX_SCRIPTLET_BYTES = 768 * 1024 * 1024

_VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$")
_SHA_RE = re.compile(r"^[0-9a-f]{64}$")
_BYTES_RE = re.compile(r"^(0|[1-9][0-9]*)$")
_MODE_RE = re.compile(r"^(-|0[0-7]{3})$")
_COMPONENT_RE = re.compile(
    r"^component=(?P<role>[a-z0-9-]+)\|(?P<presence>file|absent)\|"
    r"(?P<size>0|[1-9][0-9]*)\|(?P<digest>-|[0-9a-f]{64})\|"
    r"(?P<klass>direct-copy|optional|transformed|generated|shared)\|"
    r"(?P<mode>-|0[0-7]{3})\|(?P<mapping>[A-Za-z0-9_./ -]+|-)$"
)


class ProductionEvidenceError(ValueError):
    """Fail-closed production evidence refusal."""


@dataclass(frozen=True)
class ComponentClaim:
    role: str
    presence: str
    size: int
    sha256: str
    classification: str
    mode: str
    mapping: str


@dataclass(frozen=True)
class ValidatedProductionEvidence:
    version: str
    tag: str
    scriptlet_artifact: str
    scriptlet_bytes: int
    scriptlet_sha256: str
    payload_dirname: str
    package_profile_sha256: str
    components: tuple[ComponentClaim, ...]
    raw: bytes


def expected_mode_token(role: str, presence: str) -> str:
    if presence != "file":
        return "-"
    rel = PACKAGE_RELATIVE[role]
    if rel is None:
        raise ProductionEvidenceError(f"role {role} cannot be present as a package file")
    return f"{expected_member_mode(rel):04o}"


def package_presence_policy(role: str) -> str:
    """Presence policy derived from release_component_profiles (not destination maps).

    - required: mandatory packaged direct-copy roles
    - optional: optional packaged roles may be absent
    - absent-only: generated / unpackaged roles must not appear as package files
    """
    if role in MANDATORY_PACKAGE_ROLES:
        return "required"
    if PACKAGE_CLASS[role] == "optional":
        return "optional"
    if PACKAGE_RELATIVE[role] is None:
        return "absent-only"
    # Non-mandatory packaged roles with a relative path default to optional-like
    # presence only when classified optional; anything else is a contract bug.
    raise ProductionEvidenceError(f"unresolved package presence policy for {role}")


def enforce_presence_policy(role: str, presence: str) -> None:
    policy = package_presence_policy(role)
    if policy == "required" and presence != "file":
        raise ProductionEvidenceError(
            f"mandatory direct-copy role must be present: {role}"
        )
    if policy == "absent-only" and presence != "absent":
        raise ProductionEvidenceError(
            f"generated/unpackaged role must be absent from package evidence: {role}"
        )


def build_component_contract_bytes() -> bytes:
    """Canonical contract dump shared with the Rust verifier (anti-drift)."""
    lines = [
        CONTRACT_HEADER,
        f"schema={CONTRACT_SCHEMA}",
        f"role_count={len(COMPONENT_ROLES)}",
    ]
    for role in COMPONENT_ROLES:
        rel = PACKAGE_RELATIVE[role]
        rel_token = "-" if rel is None else rel
        # Mode expectation when the role is present as a packaged file.
        mode = "-" if rel is None else f"{expected_member_mode(rel):04o}"
        policy = package_presence_policy(role)
        lines.append(
            f"role={role}|{PACKAGE_CLASS[role]}|{mode}|{CANDIDATE_B_DESTINATION[role]}"
            f"|{rel_token}|{policy}"
        )
    return ("\n".join(lines) + "\n").encode("ascii")


def _mode_for_build(role: str, presence: str, rel: str | None) -> str:
    if presence != "file" or rel is None:
        return "-"
    return f"{expected_member_mode(rel):04o}"


def build_production_publication_evidence(
    *,
    version: str,
    payload: Path,
    scriptlet: Path | None = None,
    scriptlet_bytes: int | None = None,
    scriptlet_sha256: str | None = None,
) -> bytes:
    """Build evidence from staged payload + one authoritative Scriptlet identity.

    Prefer ``scriptlet_bytes`` + ``scriptlet_sha256`` from a single prior measurement.
    Passing ``scriptlet`` alone remeasures for convenience and must not be mixed with
    an independent second measurement elsewhere in the same signing workflow.
    """
    if not _VERSION_RE.fullmatch(version):
        raise ProductionEvidenceError("noncanonical version")
    expected_scriptlet = f"Install Ignite v{version}.sh"
    if payload.name != f"Ignite-Kindle-{version}":
        raise ProductionEvidenceError("payload directory name mismatch")

    if scriptlet_bytes is None or scriptlet_sha256 is None:
        if scriptlet is None:
            raise ProductionEvidenceError("scriptlet identity or path required")
        if scriptlet.name != expected_scriptlet:
            raise ProductionEvidenceError(f"scriptlet basename must be {expected_scriptlet!r}")
        data = scriptlet.read_bytes()
        if not data or len(data) > MAX_SCRIPTLET_BYTES:
            raise ProductionEvidenceError("scriptlet size out of bounds")
        scriptlet_sha256 = hashlib.sha256(data).hexdigest()
        scriptlet_bytes = len(data)
    else:
        if scriptlet is not None and scriptlet.name != expected_scriptlet:
            raise ProductionEvidenceError(f"scriptlet basename must be {expected_scriptlet!r}")
        if scriptlet_bytes <= 0 or scriptlet_bytes > MAX_SCRIPTLET_BYTES:
            raise ProductionEvidenceError("scriptlet size out of bounds")
        if not _SHA_RE.fullmatch(scriptlet_sha256):
            raise ProductionEvidenceError("noncanonical scriptlet_sha256")

    digests = inspect_package_components(payload)
    profiles = generate_profiles(payload, version)
    package_profile = profiles["package"]
    profile_sha = hashlib.sha256(package_profile).hexdigest()

    components: list[ComponentClaim] = []
    for role in COMPONENT_ROLES:
        presence, size, digest = digests[role]
        enforce_presence_policy(role, presence)
        klass = PACKAGE_CLASS[role]
        rel = PACKAGE_RELATIVE[role]
        mapping = CANDIDATE_B_DESTINATION[role]
        mode = _mode_for_build(role, presence, rel)
        if presence == "file":
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

    return build_production_evidence_from_claims(
        version=version,
        scriptlet_bytes=scriptlet_bytes,
        scriptlet_sha256=scriptlet_sha256,
        package_profile_sha256=profile_sha,
        components=components,
    )


def build_production_evidence_from_claims(
    *,
    version: str,
    scriptlet_bytes: int,
    scriptlet_sha256: str,
    package_profile_sha256: str,
    components: list[ComponentClaim] | tuple[ComponentClaim, ...],
) -> bytes:
    tag = f"v{version}"
    lines = [
        EVIDENCE_HEADER,
        f"schema={EVIDENCE_SCHEMA}",
        f"version={version}",
        f"tag={tag}",
        f"scriptlet_artifact=Install Ignite v{version}.sh",
        f"scriptlet_bytes={scriptlet_bytes}",
        f"scriptlet_sha256={scriptlet_sha256}",
        f"payload_dirname=Ignite-Kindle-{version}",
        f"package_profile_sha256={package_profile_sha256}",
        f"component_role_count={len(COMPONENT_ROLES)}",
    ]
    by_role = {c.role: c for c in components}
    if set(by_role) != set(COMPONENT_ROLES):
        raise ProductionEvidenceError("component role set mismatch")
    for role in COMPONENT_ROLES:
        claim = by_role[role]
        enforce_presence_policy(role, claim.presence)
        if claim.presence == "file":
            lines.append(
                f"component={role}|file|{claim.size}|{claim.sha256}|"
                f"{claim.classification}|{claim.mode}|{claim.mapping}"
            )
        else:
            lines.append(
                f"component={role}|absent|0|-|{claim.classification}|-|{claim.mapping}"
            )
    lines.append(f"publisher_authentication_scope={PRODUCTION_SCOPE}")
    raw = ("\n".join(lines) + "\n").encode("ascii")
    if len(raw) > MAX_EVIDENCE_BYTES:
        raise ProductionEvidenceError("publication evidence exceeds size bound")
    return raw


def parse_and_validate_production_evidence(
    raw: bytes,
    *,
    expected_version: str | None = None,
    expected_scriptlet_sha256: str | None = None,
    expected_scriptlet_bytes: int | None = None,
) -> ValidatedProductionEvidence:
    """Strict parse + contract validation. Fail closed on any noncanonical input."""
    if not raw:
        raise ProductionEvidenceError("publication evidence must not be empty")
    if len(raw) > MAX_EVIDENCE_BYTES or b"\r" in raw:
        raise ProductionEvidenceError("publication evidence refused")
    if TEST_SCOPE_MARKER.encode("ascii") in raw:
        raise ProductionEvidenceError(
            "test-trust-root-only evidence is not production evidence"
        )
    text = raw.decode("ascii")
    if not text.endswith("\n"):
        raise ProductionEvidenceError("evidence must end with LF")
    lines = text[:-1].split("\n")
    expected_line_count = 10 + len(COMPONENT_ROLES) + 1
    if len(lines) != expected_line_count:
        raise ProductionEvidenceError("evidence line count mismatch")
    if lines[0] != EVIDENCE_HEADER or lines[1] != f"schema={EVIDENCE_SCHEMA}":
        raise ProductionEvidenceError("unsupported production evidence schema")

    def field(prefix: str, line: str) -> str:
        if not line.startswith(prefix):
            raise ProductionEvidenceError(f"missing field {prefix}")
        return line[len(prefix) :]

    version = field("version=", lines[2])
    tag = field("tag=", lines[3])
    scriptlet_artifact = field("scriptlet_artifact=", lines[4])
    scriptlet_bytes_token = field("scriptlet_bytes=", lines[5])
    scriptlet_sha256 = field("scriptlet_sha256=", lines[6])
    payload_dirname = field("payload_dirname=", lines[7])
    package_profile_sha256 = field("package_profile_sha256=", lines[8])
    role_count = field("component_role_count=", lines[9])

    if not _VERSION_RE.fullmatch(version):
        raise ProductionEvidenceError("noncanonical version")
    if tag != f"v{version}":
        raise ProductionEvidenceError("tag/version mismatch")
    if scriptlet_artifact != f"Install Ignite v{version}.sh":
        raise ProductionEvidenceError("scriptlet artifact mismatch")
    if not _BYTES_RE.fullmatch(scriptlet_bytes_token):
        raise ProductionEvidenceError("noncanonical scriptlet_bytes")
    scriptlet_bytes = int(scriptlet_bytes_token)
    if scriptlet_bytes <= 0 or scriptlet_bytes > MAX_SCRIPTLET_BYTES:
        raise ProductionEvidenceError("scriptlet_bytes out of bounds")
    if not _SHA_RE.fullmatch(scriptlet_sha256):
        raise ProductionEvidenceError("noncanonical scriptlet_sha256")
    if payload_dirname != f"Ignite-Kindle-{version}":
        raise ProductionEvidenceError("payload dirname mismatch")
    if not _SHA_RE.fullmatch(package_profile_sha256):
        raise ProductionEvidenceError("noncanonical package_profile_sha256")
    if role_count != str(len(COMPONENT_ROLES)):
        raise ProductionEvidenceError("component_role_count mismatch")

    if expected_version is not None and version != expected_version:
        raise ProductionEvidenceError("evidence version disagrees with release-v1")
    if expected_scriptlet_sha256 is not None and scriptlet_sha256 != expected_scriptlet_sha256:
        raise ProductionEvidenceError("evidence scriptlet_sha256 disagrees with release-v1")
    if expected_scriptlet_bytes is not None and scriptlet_bytes != expected_scriptlet_bytes:
        raise ProductionEvidenceError("evidence scriptlet_bytes disagrees with release-v1")

    components: list[ComponentClaim] = []
    seen: set[str] = set()
    for idx, role in enumerate(COMPONENT_ROLES):
        match = _COMPONENT_RE.fullmatch(lines[10 + idx])
        if match is None or match.group("role") != role:
            raise ProductionEvidenceError(f"component line malformed for {role}")
        if role in seen:
            raise ProductionEvidenceError(f"duplicate role {role}")
        seen.add(role)
        presence = match.group("presence")
        size = int(match.group("size"))
        digest = match.group("digest")
        klass = match.group("klass")
        mode = match.group("mode")
        mapping = match.group("mapping")
        if klass != PACKAGE_CLASS[role]:
            raise ProductionEvidenceError(f"classification mismatch for {role}")
        if mapping != CANDIDATE_B_DESTINATION[role]:
            raise ProductionEvidenceError(f"mapping mismatch for {role}")
        expected_mode = expected_mode_token(role, presence)
        if mode != expected_mode:
            raise ProductionEvidenceError(f"mode mismatch for {role}")
        enforce_presence_policy(role, presence)
        if presence == "absent":
            if size != 0 or digest != "-" or mode != "-":
                raise ProductionEvidenceError(f"absent component invalid: {role}")
        else:
            if digest == "-" or not _MODE_RE.fullmatch(mode) or mode == "-":
                raise ProductionEvidenceError(f"file component invalid: {role}")
            if size <= 0 or size > MAX_COMPONENT_BYTES:
                raise ProductionEvidenceError(f"component size bound: {role}")
            if PACKAGE_RELATIVE[role] is None:
                raise ProductionEvidenceError(f"role {role} cannot be a packaged file")
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

    if lines[-1] != f"publisher_authentication_scope={PRODUCTION_SCOPE}":
        raise ProductionEvidenceError("production publisher scope marker missing")

    rebuilt = build_production_evidence_from_claims(
        version=version,
        scriptlet_bytes=scriptlet_bytes,
        scriptlet_sha256=scriptlet_sha256,
        package_profile_sha256=package_profile_sha256,
        components=components,
    )
    if rebuilt != raw:
        raise ProductionEvidenceError("evidence is not exact canonical encoding")

    return ValidatedProductionEvidence(
        version=version,
        tag=tag,
        scriptlet_artifact=scriptlet_artifact,
        scriptlet_bytes=scriptlet_bytes,
        scriptlet_sha256=scriptlet_sha256,
        payload_dirname=payload_dirname,
        package_profile_sha256=package_profile_sha256,
        components=tuple(components),
        raw=raw,
    )


def is_production_evidence_header(raw: bytes) -> bool:
    return raw.startswith(f"{EVIDENCE_HEADER}\n".encode("ascii"))
