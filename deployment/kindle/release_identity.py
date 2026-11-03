"""Canonical Ignite release identity and acquisition receipt (Slice 3D-B2a.4.3o).

The acquisition receipt records GitHub Actions API association for a designated
``ci.yml`` run together with digests of downloaded public artifacts. API
provenance association is **not** cryptographic build attestation.

Signed release-v1 / publication-binding formats are unchanged.
"""

from __future__ import annotations

import hashlib
import re
from dataclasses import dataclass, field
from pathlib import Path

RECEIPT_HEADER = "IGNITE_RELEASE_ACQUISITION_RECEIPT_V1"
RECEIPT_SCHEMA = "1"
IDENTITY_HEADER = "IGNITE_RELEASE_IDENTITY_V1"
IDENTITY_SCHEMA = "1"
RECEIPT_BASENAME = "acquisition.receipt.v1"
IDENTITY_BASENAME = "release-identity.v1"

EXPECTED_REPOSITORY = "markrai/ignite"
EXPECTED_WORKFLOW = ".github/workflows/ci.yml"
EXPECTED_WORKFLOW_FILE = "ci.yml"
EXPECTED_BRANCH = "main"
KINDLE_ARTIFACT = "ignite-kindle-release"
MEASUREMENTS_ARTIFACT = "ignite-kindle-packaging-measurements"
LINUX_ARTIFACT = "ignite-server-linux-release"

MAX_RECEIPT_BYTES = 64 * 1024
_SHA_RE = re.compile(r"^[0-9a-f]{64}$")
_COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
_RUN_RE = re.compile(r"^[1-9][0-9]*$")
_VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$")


class ReleaseIdentityError(ValueError):
    """Fail-closed release identity / acquisition receipt refusal."""


@dataclass(frozen=True)
class ArtifactRecord:
    name: str
    artifact_id: int
    # SHA-256 over the extracted artifact tree (sorted path+bytes), NOT the
    # original GitHub Actions zip archive digest.
    digest_sha256: str
    expired: bool = False


@dataclass
class ReleaseIdentity:
    """Single canonical release identity across acquire → sign → promote."""

    repository: str
    workflow: str
    tag: str
    version: str
    ci_run_id: int
    ci_run_attempt: int
    ci_head_sha: str
    ci_event: str
    ci_head_branch: str
    kindle_artifact: ArtifactRecord
    measurements_artifact: ArtifactRecord
    linux_artifact: ArtifactRecord
    scriptlet_name: str
    scriptlet_bytes: int
    scriptlet_sha256: str
    scriptlet_checksum_sha256: str
    measurement_sha256: str
    package_profile_sha256: str
    linux_bundle_name: str
    linux_bundle_bytes: int
    linux_bundle_sha256: str
    linux_checksum_sha256: str
    association_cross_checked: bool = False
    selection_policy: str = "explicit-run-id"
    extra: tuple[str, ...] = field(default_factory=tuple)

    def validate(self) -> None:
        if self.repository != EXPECTED_REPOSITORY:
            raise ReleaseIdentityError(f"repository refused: {self.repository}")
        if self.workflow != EXPECTED_WORKFLOW:
            raise ReleaseIdentityError(f"workflow refused: {self.workflow}")
        if not self.tag.startswith("v") or self.tag[1:] != self.version:
            raise ReleaseIdentityError("tag/version inconsistent")
        if not _VERSION_RE.fullmatch(self.version):
            raise ReleaseIdentityError("version noncanonical")
        if not _RUN_RE.fullmatch(str(self.ci_run_id)):
            raise ReleaseIdentityError("ci_run_id malformed")
        if self.ci_run_attempt < 1:
            raise ReleaseIdentityError("ci_run_attempt must be >= 1")
        if not _COMMIT_RE.fullmatch(self.ci_head_sha):
            raise ReleaseIdentityError("ci_head_sha malformed")
        if self.ci_head_branch != EXPECTED_BRANCH:
            raise ReleaseIdentityError(f"branch refused: {self.ci_head_branch}")
        if self.ci_event not in {"push", "workflow_dispatch"}:
            raise ReleaseIdentityError(f"event refused: {self.ci_event}")
        for label, digest in (
            ("scriptlet", self.scriptlet_sha256),
            ("measurement", self.measurement_sha256),
            ("package_profile", self.package_profile_sha256),
            ("linux_bundle", self.linux_bundle_sha256),
            ("scriptlet_checksum", self.scriptlet_checksum_sha256),
            ("linux_checksum", self.linux_checksum_sha256),
            ("kindle_artifact", self.kindle_artifact.digest_sha256),
            ("measurements_artifact", self.measurements_artifact.digest_sha256),
            ("linux_artifact", self.linux_artifact.digest_sha256),
        ):
            if not _SHA_RE.fullmatch(digest):
                raise ReleaseIdentityError(f"{label} digest malformed")
        if self.kindle_artifact.name != KINDLE_ARTIFACT:
            raise ReleaseIdentityError("kindle artifact name mismatch")
        if self.measurements_artifact.name != MEASUREMENTS_ARTIFACT:
            raise ReleaseIdentityError("measurements artifact name mismatch")
        if self.linux_artifact.name != LINUX_ARTIFACT:
            raise ReleaseIdentityError("linux artifact name mismatch")
        ids = {
            self.kindle_artifact.artifact_id,
            self.measurements_artifact.artifact_id,
            self.linux_artifact.artifact_id,
        }
        if len(ids) != 3:
            raise ReleaseIdentityError("duplicate GitHub artifact identities")
        if self.scriptlet_name != f"Install Ignite v{self.version}.sh":
            raise ReleaseIdentityError("scriptlet name mismatch")
        if self.linux_bundle_name != (
            f"ignite-server-{self.version}-linux-x86_64.tar.gz"
        ):
            raise ReleaseIdentityError("linux bundle name mismatch")

    def to_lines(self, *, header: str, schema: str) -> list[str]:
        self.validate()
        lines = [
            header,
            f"schema={schema}",
            f"repository={self.repository}",
            f"workflow={self.workflow}",
            f"tag={self.tag}",
            f"version={self.version}",
            f"ci_run_id={self.ci_run_id}",
            f"ci_run_attempt={self.ci_run_attempt}",
            f"ci_head_sha={self.ci_head_sha}",
            f"ci_event={self.ci_event}",
            f"ci_head_branch={self.ci_head_branch}",
            f"selection_policy={self.selection_policy}",
            (
                f"artifact={self.kindle_artifact.name}|id={self.kindle_artifact.artifact_id}"
                f"|extracted_tree_sha256={self.kindle_artifact.digest_sha256}"
            ),
            (
                f"artifact={self.measurements_artifact.name}"
                f"|id={self.measurements_artifact.artifact_id}"
                f"|extracted_tree_sha256={self.measurements_artifact.digest_sha256}"
            ),
            (
                f"artifact={self.linux_artifact.name}|id={self.linux_artifact.artifact_id}"
                f"|extracted_tree_sha256={self.linux_artifact.digest_sha256}"
            ),
            f"scriptlet={self.scriptlet_name}",
            f"scriptlet_bytes={self.scriptlet_bytes}",
            f"scriptlet_sha256={self.scriptlet_sha256}",
            f"scriptlet_checksum_sha256={self.scriptlet_checksum_sha256}",
            f"measurement_sha256={self.measurement_sha256}",
            f"package_profile_sha256={self.package_profile_sha256}",
            f"linux_bundle={self.linux_bundle_name}",
            f"linux_bundle_bytes={self.linux_bundle_bytes}",
            f"linux_bundle_sha256={self.linux_bundle_sha256}",
            f"linux_checksum_sha256={self.linux_checksum_sha256}",
            f"association_cross_checked={'true' if self.association_cross_checked else 'false'}",
            "artifact_digest_kind=extracted-tree-sha256-not-github-archive-digest",
            "github_api_provenance=associated-not-cryptographic-attestation",
            "ci_provenance_requires_trusted_promotion_path=true",
            "filename_is_not_ci_provenance_proof=true",
            "operator_supplied_sha_is_not_ci_provenance_proof=true",
            "private_key_material=false",
            "github_mutation=false",
        ]
        lines.extend(self.extra)
        return lines

    def encode_receipt(self) -> bytes:
        raw = ("\n".join(self.to_lines(header=RECEIPT_HEADER, schema=RECEIPT_SCHEMA)) + "\n").encode(
            "ascii"
        )
        if len(raw) > MAX_RECEIPT_BYTES:
            raise ReleaseIdentityError("acquisition receipt exceeds size bound")
        return raw

    def encode_identity(self) -> bytes:
        raw = (
            "\n".join(self.to_lines(header=IDENTITY_HEADER, schema=IDENTITY_SCHEMA)) + "\n"
        ).encode("ascii")
        if len(raw) > MAX_RECEIPT_BYTES:
            raise ReleaseIdentityError("release identity exceeds size bound")
        return raw


def _field(prefix: str, line: str) -> str:
    if not line.startswith(prefix):
        raise ReleaseIdentityError(f"missing field {prefix}")
    return line[len(prefix) :]


def _parse_artifact_line(line: str) -> ArtifactRecord:
    # artifact=name|id=N|extracted_tree_sha256=hex
    if not line.startswith("artifact="):
        raise ReleaseIdentityError("artifact line malformed")
    body = line[len("artifact=") :]
    parts = body.split("|")
    if len(parts) != 3:
        raise ReleaseIdentityError("artifact line field count refused")
    name = parts[0]
    if not parts[1].startswith("id="):
        raise ReleaseIdentityError("artifact id missing")
    if not parts[2].startswith("extracted_tree_sha256="):
        raise ReleaseIdentityError(
            "artifact extracted_tree_sha256 missing "
            "(not a GitHub archive digest)"
        )
    try:
        artifact_id = int(parts[1][3:])
    except ValueError as exc:
        raise ReleaseIdentityError("artifact id malformed") from exc
    digest = parts[2][len("extracted_tree_sha256=") :]
    if artifact_id < 1 or not _SHA_RE.fullmatch(digest):
        raise ReleaseIdentityError("artifact identity malformed")
    return ArtifactRecord(name=name, artifact_id=artifact_id, digest_sha256=digest)


def parse_acquisition_receipt(raw: bytes) -> ReleaseIdentity:
    if not raw or b"\r" in raw or len(raw) > MAX_RECEIPT_BYTES:
        raise ReleaseIdentityError("acquisition receipt refused")
    text = raw.decode("ascii")
    if not text.endswith("\n"):
        raise ReleaseIdentityError("acquisition receipt must end with LF")
    lines = text[:-1].split("\n")
    if len(lines) < 28 or lines[0] != RECEIPT_HEADER or lines[1] != f"schema={RECEIPT_SCHEMA}":
        raise ReleaseIdentityError("unsupported acquisition receipt schema")

    by_key: dict[str, str] = {}
    artifacts: list[ArtifactRecord] = []
    extras: list[str] = []
    known_prefixes = (
        "repository=",
        "workflow=",
        "tag=",
        "version=",
        "ci_run_id=",
        "ci_run_attempt=",
        "ci_head_sha=",
        "ci_event=",
        "ci_head_branch=",
        "selection_policy=",
        "scriptlet=",
        "scriptlet_bytes=",
        "scriptlet_sha256=",
        "scriptlet_checksum_sha256=",
        "measurement_sha256=",
        "package_profile_sha256=",
        "linux_bundle=",
        "linux_bundle_bytes=",
        "linux_bundle_sha256=",
        "linux_checksum_sha256=",
        "association_cross_checked=",
    )
    # Emitted by to_lines(); must not be double-counted via extras.
    constant_trailers = {
        "artifact_digest_kind=extracted-tree-sha256-not-github-archive-digest",
        "github_api_provenance=associated-not-cryptographic-attestation",
        "ci_provenance_requires_trusted_promotion_path=true",
        "filename_is_not_ci_provenance_proof=true",
        "operator_supplied_sha_is_not_ci_provenance_proof=true",
        "private_key_material=false",
        "github_mutation=false",
    }
    for line in lines[2:]:
        if line.startswith("artifact="):
            artifacts.append(_parse_artifact_line(line))
            continue
        if line in constant_trailers:
            continue
        matched = False
        for prefix in known_prefixes:
            if line.startswith(prefix):
                key = prefix[:-1]
                if key in by_key:
                    raise ReleaseIdentityError(f"duplicate field {key}")
                by_key[key] = line[len(prefix) :]
                matched = True
                break
        if not matched:
            extras.append(line)

    if len(artifacts) != 3:
        raise ReleaseIdentityError("acquisition receipt must list exactly three artifacts")
    by_name = {a.name: a for a in artifacts}
    for required in (KINDLE_ARTIFACT, MEASUREMENTS_ARTIFACT, LINUX_ARTIFACT):
        if required not in by_name:
            raise ReleaseIdentityError(f"missing artifact record: {required}")

    identity = ReleaseIdentity(
        repository=by_key["repository"],
        workflow=by_key["workflow"],
        tag=by_key["tag"],
        version=by_key["version"],
        ci_run_id=int(by_key["ci_run_id"]),
        ci_run_attempt=int(by_key["ci_run_attempt"]),
        ci_head_sha=by_key["ci_head_sha"],
        ci_event=by_key["ci_event"],
        ci_head_branch=by_key["ci_head_branch"],
        kindle_artifact=by_name[KINDLE_ARTIFACT],
        measurements_artifact=by_name[MEASUREMENTS_ARTIFACT],
        linux_artifact=by_name[LINUX_ARTIFACT],
        scriptlet_name=by_key["scriptlet"],
        scriptlet_bytes=int(by_key["scriptlet_bytes"]),
        scriptlet_sha256=by_key["scriptlet_sha256"],
        scriptlet_checksum_sha256=by_key["scriptlet_checksum_sha256"],
        measurement_sha256=by_key["measurement_sha256"],
        package_profile_sha256=by_key["package_profile_sha256"],
        linux_bundle_name=by_key["linux_bundle"],
        linux_bundle_bytes=int(by_key["linux_bundle_bytes"]),
        linux_bundle_sha256=by_key["linux_bundle_sha256"],
        linux_checksum_sha256=by_key["linux_checksum_sha256"],
        association_cross_checked=by_key["association_cross_checked"] == "true",
        selection_policy=by_key["selection_policy"],
        extra=tuple(extras),
    )
    identity.validate()
    # Require exact canonical encoding (including constant trailer lines).
    if identity.encode_receipt() != raw:
        raise ReleaseIdentityError("acquisition receipt is not exact canonical encoding")
    return identity


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()
