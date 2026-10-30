#!/usr/bin/env python3
"""Disposable installer publication and crash-recovery rehearsal (Slice 3D-B2a.4.3e).

Exercises the real `install-runtime.sh` against exclusively owned disposable trees
(via IGNITE_APP_ROOT / DOCS / REC overrides). Observes what the installer publishes,
distinguishes retained A from candidate B with independent component evidence, and
classifies interruption outcomes.

This is observational and non-authorizing. It is not production trusted publication,
automatic installation, promotion, rollback, or first-launch admission.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import secrets
import shutil
import stat
import sys
from dataclasses import dataclass
from pathlib import Path

_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

from release_component_profiles import (  # noqa: E402
    CANDIDATE_B_CLASS,
    CANDIDATE_B_DESTINATION,
    COMPONENT_ROLES,
    PACKAGE_RELATIVE,
    RETAINED_A_FUTURE_CLASS,
    RETAINED_A_FUTURE_LAYOUT,
    generate_profiles,
    inspect_package_components,
)

REPORT_HEADER = "IGNITE_INSTALLER_PUBLICATION_REHEARSAL_V1"
REPORT_SCHEMA = "1"
MAX_REPORT_BYTES = 96 * 1024
MAX_COMPONENT_READ = 512 * 1024 * 1024
READ_CHUNK = 1024 * 1024
OWNER_FILE = ".ignite-rehearsal-owner"

PUBLICATION_STATES = (
    "no-active-release",
    "a-active-no-pending",
    "b-staging-present",
    "a-retention-incomplete",
    "b-publication-partial",
    "b-present-without-pending-health",
    "b-published-with-pending-health",
    "incomplete-or-inconsistent",
)

PUBLISH_INTERRUPT_POINTS = (
    "before-prev-move",
    "after-prev-move",
    "before-staging-publish",
    "after-staging-publish",
    "after-static-verification",
    "before-lkg-inventory-publish",
    "after-lkg-inventory-publish",
    "before-pending-health-publish",
    "after-pending-health-publish",
    "before-commit",
    "after-commit",
    "before-journal-cleanup",
    "before-prev-cleanup",
)

ACTIVATION_INTERRUPT_POINTS = (
    "scribe-app",
    "scribe-lifecycle",
    "host",
    "icon",
    "scriptlet",
    "ofl",
    "helper-install-runtime",
    "uninstall-docs",
    "backup-docs",
    "diagnose-docs",
)

_VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$")
_TXN_RE = re.compile(r"^[0-9]{8}T[0-9]{6}Z-[0-9]+$")
_SHA_RE = re.compile(r"^[0-9a-f]{64}$")
_BYTES_RE = re.compile(r"^(0|[1-9][0-9]*)$")


class RehearsalError(Exception):
    pass


@dataclass(frozen=True)
class ReleaseEvidence:
    version: str
    payload: Path
    package_profile: bytes
    digests: dict[str, tuple[str, int, str]]


@dataclass(frozen=True)
class ComponentObservation:
    role: str
    classification: str
    expected_mapping: str
    presence: str
    size: int
    sha256: str
    digest_match: str


@dataclass
class FixtureRoot:
    """Exclusive disposable root bound to create-time identity (mirrors OwnedTestRoot)."""

    path: Path
    token: str
    device: int
    inode: int

    def assert_owned(self) -> None:
        try:
            st = os.lstat(os.fspath(self.path))
        except OSError as exc:
            raise RehearsalError(f"cannot lstat fixture root: {exc}") from exc
        if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
            raise RehearsalError("fixture root must be a real directory")
        if st.st_dev != self.device or st.st_ino != self.inode:
            raise RehearsalError("fixture root identity does not match create-time owner")
        marker = self.path / OWNER_FILE
        try:
            mst = os.lstat(os.fspath(marker))
        except OSError as exc:
            raise RehearsalError(f"fixture ownership marker missing: {exc}") from exc
        if stat.S_ISLNK(mst.st_mode) or not stat.S_ISREG(mst.st_mode):
            raise RehearsalError("fixture ownership marker must be a plain file")
        if mst.st_nlink != 1:
            raise RehearsalError("fixture ownership marker must not be hard-linked")
        # Read marker via fd with identity checks (copied markers cannot rebind ownership).
        try:
            _size, _digest = _hash_file_fd(marker)
            flags = os.O_RDONLY
            if hasattr(os, "O_NOFOLLOW"):
                flags |= os.O_NOFOLLOW
            if hasattr(os, "O_BINARY"):
                flags |= os.O_BINARY
            fd = os.open(os.fspath(marker), flags)
            try:
                text = os.read(fd, 4096).decode("ascii")
            finally:
                os.close(fd)
        except (RehearsalError, OSError, UnicodeError) as exc:
            raise RehearsalError(f"fixture ownership marker unreadable: {exc}") from exc
        if f"token={self.token}\n" not in text:
            raise RehearsalError("fixture ownership marker token mismatch")
        if f"dev={self.device}\n" not in text or f"ino={self.inode}\n" not in text:
            raise RehearsalError("fixture ownership marker identity mismatch")
        # Marker still encodes the create-time root identity; a copied marker on a
        # substituted directory cannot match that directory's live identity.
        if f"dev={st.st_dev}\n" not in text or f"ino={st.st_ino}\n" not in text:
            raise RehearsalError("fixture ownership marker does not bind this directory")


def _same_identity(before: os.stat_result, after: os.stat_result) -> bool:
    if not (stat.S_ISREG(before.st_mode) and stat.S_ISREG(after.st_mode)):
        return False
    if before.st_nlink != 1 or after.st_nlink != 1:
        return False
    if before.st_size != after.st_size:
        return False
    if sys.platform == "win32":
        return True
    return before.st_dev == after.st_dev and before.st_ino == after.st_ino


def _hash_file_fd(path: Path) -> tuple[int, str]:
    """O_NOFOLLOW open, refuse hardlinks/symlinks, hash via fd with identity checks."""
    path_str = os.fspath(path)
    try:
        before = os.lstat(path_str)
    except OSError as exc:
        raise RehearsalError(f"cannot lstat {path}: {exc}") from exc
    if stat.S_ISLNK(before.st_mode):
        raise RehearsalError(f"symlink refused: {path}")
    if not stat.S_ISREG(before.st_mode):
        raise RehearsalError(f"special file refused: {path}")
    if before.st_nlink != 1:
        raise RehearsalError(f"hard link refused (nlink={before.st_nlink}): {path}")
    if before.st_size > MAX_COMPONENT_READ:
        raise RehearsalError(f"component exceeds size bound: {path}")

    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    if hasattr(os, "O_BINARY"):
        flags |= os.O_BINARY
    try:
        fd = os.open(path_str, flags)
    except OSError as exc:
        raise RehearsalError(f"cannot open {path}: {exc}") from exc
    try:
        opened = os.fstat(fd)
        if not _same_identity(before, opened):
            raise RehearsalError(f"component identity changed before read: {path}")
        hasher = hashlib.sha256()
        remaining = opened.st_size
        while remaining > 0:
            chunk = os.read(fd, min(READ_CHUNK, remaining))
            if not chunk:
                raise RehearsalError(f"truncated while reading: {path}")
            hasher.update(chunk)
            remaining -= len(chunk)
        if os.read(fd, 1):
            raise RehearsalError(f"file grew while reading: {path}")
        after = os.fstat(fd)
        if not _same_identity(opened, after):
            raise RehearsalError(f"component substituted while reading: {path}")
        return opened.st_size, hasher.hexdigest()
    finally:
        os.close(fd)


def _lstat_regular_or_none(path: Path) -> os.stat_result | None:
    try:
        st = os.lstat(os.fspath(path))
    except OSError:
        return None
    if stat.S_ISLNK(st.st_mode):
        raise RehearsalError(f"symlink refused: {path}")
    if not stat.S_ISREG(st.st_mode):
        raise RehearsalError(f"special file refused: {path}")
    if st.st_nlink != 1:
        raise RehearsalError(f"hard link refused (nlink={st.st_nlink}): {path}")
    return st


def build_release_evidence(payload: Path, version: str) -> ReleaseEvidence:
    if payload.name != f"Ignite-Kindle-{version}":
        raise RehearsalError(f"payload directory name mismatch: {payload.name}")
    observed = inspect_package_components(payload)
    profiles = generate_profiles(payload, version)
    return ReleaseEvidence(
        version=version,
        payload=payload,
        package_profile=profiles["package"],
        digests=observed,
    )


def _resolve_symbolic(kindle: Path, mapping: str) -> Path | None:
    if mapping == "-":
        return None
    if mapping.startswith("APP/"):
        return kindle / "scribe-app" / mapping[4:]
    if mapping.startswith("DOCS/"):
        return kindle / "documents" / mapping[5:]
    if mapping.startswith("REC/"):
        return kindle / "var" / "local" / "ignite-recovery" / mapping[4:]
    raise RehearsalError(f"unknown symbolic mapping: {mapping}")


def _digest_match(
    presence: str,
    digest: str,
    evidence_a: ReleaseEvidence | None,
    evidence_b: ReleaseEvidence | None,
    role: str,
    *,
    prefer: str,
) -> str:
    if presence != "file":
        return "absent"
    if prefer == "a" and evidence_a is not None:
        a_pres, _, a_dig = evidence_a.digests[role]
        if a_pres == "file" and digest == a_dig:
            return "match-a"
    if prefer == "b" and evidence_b is not None:
        b_pres, _, b_dig = evidence_b.digests[role]
        if b_pres == "file" and digest == b_dig:
            return "match-b"
    if evidence_a is not None:
        a_pres, _, a_dig = evidence_a.digests[role]
        if a_pres == "file" and digest == a_dig:
            return "match-a"
    if evidence_b is not None:
        b_pres, _, b_dig = evidence_b.digests[role]
        if b_pres == "file" and digest == b_dig:
            return "match-b"
    if CANDIDATE_B_CLASS[role] in {"transformed", "generated", "shared"}:
        return "not-applicable"
    return "mismatch"


def observe_candidate_b(
    kindle: Path,
    evidence_b: ReleaseEvidence,
    evidence_a: ReleaseEvidence | None = None,
) -> list[ComponentObservation]:
    rows: list[ComponentObservation] = []
    for role in COMPONENT_ROLES:
        cls = CANDIDATE_B_CLASS[role]
        mapping = CANDIDATE_B_DESTINATION[role]
        path = _resolve_symbolic(kindle, mapping)
        if path is None or cls in {"transformed", "generated"}:
            rows.append(
                ComponentObservation(
                    role=role,
                    classification=cls,
                    expected_mapping=mapping,
                    presence="absent",
                    size=0,
                    sha256="-",
                    digest_match="not-applicable",
                )
            )
            continue
        st = _lstat_regular_or_none(path)
        if st is None:
            rows.append(
                ComponentObservation(
                    role=role,
                    classification=cls,
                    expected_mapping=mapping,
                    presence="absent",
                    size=0,
                    sha256="-",
                    digest_match="absent",
                )
            )
            continue
        size, digest = _hash_file_fd(path)
        rows.append(
            ComponentObservation(
                role=role,
                classification=cls,
                expected_mapping=mapping,
                presence="file",
                size=size,
                sha256=digest,
                digest_match=_digest_match(
                    "file", digest, evidence_a, evidence_b, role, prefer="b"
                ),
            )
        )
    return rows


def observe_retained_a(
    kindle: Path,
    evidence_a: ReleaseEvidence,
    evidence_b: ReleaseEvidence | None = None,
) -> list[ComponentObservation]:
    rows: list[ComponentObservation] = []
    for role in COMPONENT_ROLES:
        cls = RETAINED_A_FUTURE_CLASS[role]
        mapping = RETAINED_A_FUTURE_LAYOUT[role]
        path = _resolve_symbolic(kindle, mapping)
        if path is None or cls in {"shared", "transformed"}:
            if cls == "transformed":
                sql = kindle / "scribe-app" / "previous-release" / "appreg-handler.sql"
                try:
                    st = _lstat_regular_or_none(sql)
                except RehearsalError:
                    raise
                if st is not None:
                    size, digest = _hash_file_fd(sql)
                    rows.append(
                        ComponentObservation(
                            role=role,
                            classification=cls,
                            expected_mapping=mapping,
                            presence="file",
                            size=size,
                            sha256=digest,
                            digest_match="not-applicable",
                        )
                    )
                    continue
            rows.append(
                ComponentObservation(
                    role=role,
                    classification=cls,
                    expected_mapping=mapping,
                    presence="absent",
                    size=0,
                    sha256="-",
                    digest_match="not-applicable",
                )
            )
            continue
        st = _lstat_regular_or_none(path)
        if st is None:
            rows.append(
                ComponentObservation(
                    role=role,
                    classification=cls,
                    expected_mapping=mapping,
                    presence="absent",
                    size=0,
                    sha256="-",
                    digest_match="absent",
                )
            )
            continue
        size, digest = _hash_file_fd(path)
        rows.append(
            ComponentObservation(
                role=role,
                classification=cls,
                expected_mapping=mapping,
                presence="file",
                size=size,
                sha256=digest,
                digest_match=_digest_match(
                    "file", digest, evidence_a, evidence_b, role, prefer="a"
                ),
            )
        )
    return rows


def _has_new_staging(kindle: Path) -> bool:
    app = kindle / "scribe-app"
    docs = kindle / "documents"
    patterns = [
        app / "bin" / "scribe-app.new",
        app / "bin" / "scribe-lifecycle.new",
        app / "bin" / "scribe-lifecycle-host.sh.new",
        docs / "Scribe.sh.new",
    ]
    return any(p.exists() or p.is_symlink() for p in patterns)


def _retention_leftovers(kindle: Path) -> bool:
    app = kindle / "scribe-app"
    return (app / "previous-release.staging").exists() or (
        app / "previous-release.prev"
    ).exists()


def _pending_health_path(kindle: Path) -> Path:
    return kindle / "scribe-app" / "state" / "update" / "pending-health.v1"


def _journal_path(kindle: Path) -> Path:
    return kindle / "scribe-app" / "run" / "ignite-activate.journal"


def _stable_app(kindle: Path) -> Path:
    return kindle / "scribe-app" / "bin" / "scribe-app"


def _stable_life(kindle: Path) -> Path:
    return kindle / "scribe-app" / "bin" / "scribe-lifecycle"


def inspect_pending_health(kindle: Path) -> tuple[str, dict[str, str] | None]:
    """Return (absent|invalid|valid, fields). Mirrors install-runtime validate_pending_health."""
    path = _pending_health_path(kindle)
    try:
        st = os.lstat(os.fspath(path))
    except OSError:
        return "absent", None
    if stat.S_ISLNK(st.st_mode):
        return "invalid", None
    if not stat.S_ISREG(st.st_mode) or st.st_nlink != 1:
        return "invalid", None
    if st.st_size <= 0 or st.st_size > 4096:
        return "invalid", None
    try:
        size, _digest = _hash_file_fd(path)
        # Re-open via fd for exact bytes (same identity constraints).
        flags = os.O_RDONLY
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        if hasattr(os, "O_BINARY"):
            flags |= os.O_BINARY
        fd = os.open(os.fspath(path), flags)
        try:
            raw = os.read(fd, size + 1)
        finally:
            os.close(fd)
        if len(raw) != size:
            return "invalid", None
        text = raw.decode("ascii")
    except (RehearsalError, OSError, UnicodeError):
        return "invalid", None
    if "\r" in text or not text.endswith("\n"):
        return "invalid", None
    lines = text[:-1].split("\n")
    if len(lines) != 14:
        return "invalid", None
    if lines[:3] != [
        "IGNITE_PENDING_HEALTH_V1",
        "schema=1",
        "status=pending-health",
    ]:
        return "invalid", None
    if lines[9] != "retained_version=unknown":
        return "invalid", None
    if lines[13] != "data_compatibility=unknown":
        return "invalid", None

    def _req(prefix: str, line: str) -> str | None:
        if not line.startswith(prefix):
            return None
        return line[len(prefix) :]

    fields: dict[str, str] = {}
    mapping = [
        ("candidate_version", "candidate_version=", 3),
        ("candidate_txn", "candidate_txn=", 4),
        ("candidate_app_bytes", "candidate_app_bytes=", 5),
        ("candidate_app_sha256", "candidate_app_sha256=", 6),
        ("candidate_lifecycle_bytes", "candidate_lifecycle_bytes=", 7),
        ("candidate_lifecycle_sha256", "candidate_lifecycle_sha256=", 8),
        ("retained_release_identity", "retained_release_identity=sha256:", 10),
        ("lkg_inventory_bytes", "lkg_inventory_bytes=", 11),
        ("lkg_inventory_sha256", "lkg_inventory_sha256=", 12),
    ]
    for key, prefix, idx in mapping:
        value = _req(prefix, lines[idx])
        if value is None:
            return "invalid", None
        fields[key] = value

    if not _VERSION_RE.fullmatch(fields["candidate_version"]):
        return "invalid", None
    if not _TXN_RE.fullmatch(fields["candidate_txn"]):
        return "invalid", None
    for key in (
        "candidate_app_bytes",
        "candidate_lifecycle_bytes",
        "lkg_inventory_bytes",
    ):
        if not _BYTES_RE.fullmatch(fields[key]):
            return "invalid", None
    for key in (
        "candidate_app_sha256",
        "candidate_lifecycle_sha256",
        "retained_release_identity",
        "lkg_inventory_sha256",
    ):
        if not _SHA_RE.fullmatch(fields[key]):
            return "invalid", None
    if fields["retained_release_identity"] != fields["lkg_inventory_sha256"]:
        return "invalid", None

    # Canonical 14-line re-encoding must match exactly (install-runtime contract).
    canonical = (
        "\n".join(
            [
                "IGNITE_PENDING_HEALTH_V1",
                "schema=1",
                "status=pending-health",
                f"candidate_version={fields['candidate_version']}",
                f"candidate_txn={fields['candidate_txn']}",
                f"candidate_app_bytes={fields['candidate_app_bytes']}",
                f"candidate_app_sha256={fields['candidate_app_sha256']}",
                f"candidate_lifecycle_bytes={fields['candidate_lifecycle_bytes']}",
                f"candidate_lifecycle_sha256={fields['candidate_lifecycle_sha256']}",
                "retained_version=unknown",
                f"retained_release_identity=sha256:{fields['retained_release_identity']}",
                f"lkg_inventory_bytes={fields['lkg_inventory_bytes']}",
                f"lkg_inventory_sha256={fields['lkg_inventory_sha256']}",
                "data_compatibility=unknown",
            ]
        )
        + "\n"
    )
    if text != canonical:
        return "invalid", None

    # Live candidate app/lifecycle must match recorded digests.
    try:
        app_size, app_sha = _hash_file_fd(_stable_app(kindle))
        life_size, life_sha = _hash_file_fd(_stable_life(kindle))
    except RehearsalError:
        return "invalid", None
    if str(app_size) != fields["candidate_app_bytes"] or app_sha != fields["candidate_app_sha256"]:
        return "invalid", None
    if (
        str(life_size) != fields["candidate_lifecycle_bytes"]
        or life_sha != fields["candidate_lifecycle_sha256"]
    ):
        return "invalid", None
    return "valid", fields


def _candidate_matches_release(
    candidate_rows: list[ComponentObservation],
    evidence: ReleaseEvidence,
) -> bool:
    """True when live candidate slots match the given release evidence contract.

    Compares live digests to the evidence map (not match-a/match-b labels) so
    identical cross-release package bytes (e.g. recovery fixtures) do not flip
    first-install classification. Mandatory direct-copy roles must be present and
    digest-equal. Optional roles expected from the evidence must match;
    unexpected optional presence fails. Shared/transformed/generated roles do
    not decide completeness here.
    """
    by_role = {row.role: row for row in candidate_rows}
    for role in COMPONENT_ROLES:
        row = by_role[role]
        cls = row.classification
        exp_presence, _exp_size, exp_digest = evidence.digests[role]
        if cls == "direct-copy":
            if exp_presence != "file":
                # Package evidence must list mandatory roles as files.
                return False
            if row.presence != "file" or row.sha256 != exp_digest:
                return False
        elif cls == "optional":
            if exp_presence == "file":
                if row.presence != "file" or row.sha256 != exp_digest:
                    return False
            elif row.presence != "absent":
                return False
    return True


def classify_publication_state(
    kindle: Path,
    *,
    candidate_rows: list[ComponentObservation],
    retained_rows: list[ComponentObservation],
    evidence_a: ReleaseEvidence,
    evidence_b: ReleaseEvidence,
    had_upgrade: bool,
) -> str:
    pending_status, _fields = inspect_pending_health(kindle)
    journal = _journal_path(kindle)
    staging = _has_new_staging(kindle)
    retention_messy = _retention_leftovers(kindle)

    try:
        stable = _lstat_regular_or_none(_stable_app(kindle))
    except RehearsalError:
        return "incomplete-or-inconsistent"

    if (
        stable is None
        and not staging
        and not journal.is_file()
        and not retention_messy
        and pending_status == "absent"
    ):
        return "no-active-release"

    b_complete = _candidate_matches_release(candidate_rows, evidence_b)
    a_active_complete = _candidate_matches_release(candidate_rows, evidence_a)

    # Partial B: some mandatory B-unique bytes present, but not a complete B tree.
    # Ignore roles whose A and B package digests are identical (shared fixture bytes).
    b_partial = False
    if had_upgrade and not b_complete:
        for row in candidate_rows:
            if row.classification != "direct-copy" or row.presence != "file":
                continue
            a_pres, _, a_dig = evidence_a.digests[row.role]
            b_pres, _, b_dig = evidence_b.digests[row.role]
            if b_pres != "file" or row.sha256 != b_dig:
                continue
            if a_pres == "file" and a_dig == b_dig:
                continue
            b_partial = True
            break

    retained_direct = [
        row
        for row in retained_rows
        if row.classification == "direct-copy" and row.expected_mapping != "-"
    ]
    retained_a_complete = (
        all(row.presence == "file" and row.digest_match == "match-a" for row in retained_direct)
        if had_upgrade
        else True
    )

    # Surface interrupted publication leftovers before claiming completeness.
    if staging:
        return "b-staging-present"
    if retention_messy:
        return "a-retention-incomplete"
    if journal.is_file() or b_partial:
        return "b-publication-partial"
    if pending_status == "invalid":
        return "incomplete-or-inconsistent"

    if (
        had_upgrade
        and b_complete
        and retained_a_complete
        and pending_status == "valid"
    ):
        # Valid pending-health is publication evidence only — never qualification.
        return "b-published-with-pending-health"
    if had_upgrade and b_complete and pending_status == "absent":
        return "b-present-without-pending-health"

    if not had_upgrade and pending_status == "absent":
        if (kindle / "scribe-app" / "previous-release" / "scribe-app").is_file():
            return "incomplete-or-inconsistent"
        if a_active_complete:
            return "a-active-no-pending"
        if stable is None:
            return "no-active-release"
        return "incomplete-or-inconsistent"

    if not had_upgrade and stable is None and pending_status == "absent":
        return "no-active-release"
    return "incomplete-or-inconsistent"


def observe_external_state(sentinels: dict[Path, bytes]) -> list[str]:
    errors: list[str] = []
    for path, expected in sentinels.items():
        expected_digest = hashlib.sha256(expected).hexdigest()
        try:
            size, digest = _hash_file_fd(path)
        except RehearsalError as exc:
            errors.append(f"external unsafe/missing: {path.as_posix()} ({exc})")
            continue
        if size != len(expected) or digest != expected_digest:
            errors.append(f"external mutated: {path.as_posix()}")
    return errors


def render_report(
    *,
    state: str,
    version_a: str,
    version_b: str,
    install_rc: int | None,
    interrupt_point: str | None,
    candidate_rows: list[ComponentObservation],
    retained_rows: list[ComponentObservation],
    external_errors: list[str],
    notes: list[str],
) -> bytes:
    lines = [
        REPORT_HEADER,
        f"schema={REPORT_SCHEMA}",
        f"publication_state={state}",
        f"release_a_version={version_a}",
        f"release_b_version={version_b}",
        f"install_rc={install_rc if install_rc is not None else '-'}",
        f"interrupt_point={interrupt_point or '-'}",
        "harness=real-install-runtime-disposable-root",
        "publisher_authentication=unavailable",
        "installation_binding=unverified",
        "execution_binding=unverified",
        "first_launch_admission=unavailable",
        "data_compatibility=unknown",
        "promotion_authorized=false",
        f"candidate_component_count={len(candidate_rows)}",
        f"retained_component_count={len(retained_rows)}",
        f"external_error_count={len(external_errors)}",
    ]
    for row in candidate_rows:
        lines.append(
            "candidate="
            f"{row.role}|{row.classification}|{row.presence}|{row.size}|"
            f"{row.sha256}|{row.digest_match}|{row.expected_mapping}"
        )
    for row in retained_rows:
        lines.append(
            "retained="
            f"{row.role}|{row.classification}|{row.presence}|{row.size}|"
            f"{row.sha256}|{row.digest_match}|{row.expected_mapping}"
        )
    for err in external_errors[:32]:
        safe = err.encode("ascii", errors="replace").decode("ascii")
        lines.append(f"external_error={safe[:240]}")
    for note in notes[:32]:
        safe = note.encode("ascii", errors="replace").decode("ascii")
        lines.append(f"note={safe[:240]}")
    raw = ("\n".join(lines) + "\n").encode("ascii")
    if len(raw) > MAX_REPORT_BYTES:
        raise RehearsalError("rehearsal report exceeds size bound")
    return raw


def seed_external_sentinels(kindle: Path) -> dict[Path, bytes]:
    files = {
        kindle / "scribe-data" / "notebooks" / "n" / "page" / "strokes.jsonl": b"stroke-sentinel\n",
        kindle / "scribe-app" / "state" / "sync" / "outbox.jsonl": b'{"sync":"sentinel"}\n',
        kindle / "scribe-app" / "state" / "device" / "identity.json": b'{"device":"sentinel"}\n',
        kindle / "scribe-app" / "state" / "settings.json": b'{"settings":"sentinel"}\n',
        kindle / "documents" / "UserNote.txt": b"user-document-sentinel\n",
    }
    for path, content in files.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
    return files


def init_fixture_root(root: Path) -> FixtureRoot:
    """Create an exclusive disposable root; ownership is the returned FixtureRoot."""
    root.mkdir(parents=True, exist_ok=False)
    try:
        st = os.lstat(os.fspath(root))
        if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
            raise RehearsalError("fixture root must be a real directory")
        token = secrets.token_hex(16)
        marker = root / OWNER_FILE
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
        if hasattr(os, "O_BINARY"):
            flags |= os.O_BINARY
        fd = os.open(os.fspath(marker), flags, 0o644)
        try:
            os.write(
                fd,
                f"ignite-rehearsal\ntoken={token}\ndev={st.st_dev}\nino={st.st_ino}\n".encode(
                    "ascii"
                ),
            )
        finally:
            os.close(fd)
        (root / "documents").mkdir()
        (root / "scribe-data").mkdir()
        (root / "var" / "local").mkdir(parents=True)
        owned = FixtureRoot(path=root, token=token, device=st.st_dev, inode=st.st_ino)
        owned.assert_owned()
        return owned
    except Exception:
        shutil.rmtree(root, ignore_errors=True)
        raise


def safe_cleanup_fixture(owned: FixtureRoot) -> None:
    """Delete only the create-time owned root. Copied markers cannot authorize cleanup."""
    owned.assert_owned()
    # Re-validate immediately before unlink tree.
    owned.assert_owned()
    shutil.rmtree(owned.path)


def summarize_identity(
    candidate_rows: list[ComponentObservation],
    retained_rows: list[ComponentObservation],
) -> list[str]:
    notes: list[str] = []
    b_matches = sum(1 for row in candidate_rows if row.digest_match == "match-b")
    a_as_b = sum(1 for row in candidate_rows if row.digest_match == "match-a")
    a_matches = sum(1 for row in retained_rows if row.digest_match == "match-a")
    b_as_a = sum(1 for row in retained_rows if row.digest_match == "match-b")
    notes.append(f"candidate_match_b={b_matches}")
    notes.append(f"candidate_incorrectly_match_a={a_as_b}")
    notes.append(f"retained_match_a={a_matches}")
    notes.append(f"retained_incorrectly_match_b={b_as_a}")
    return notes


def observe_rehearsal(
    kindle: Path,
    evidence_a: ReleaseEvidence,
    evidence_b: ReleaseEvidence,
    *,
    had_upgrade: bool,
    install_rc: int | None = None,
    interrupt_point: str | None = None,
    sentinels: dict[Path, bytes] | None = None,
) -> bytes:
    candidate = observe_candidate_b(kindle, evidence_b, evidence_a)
    retained = observe_retained_a(kindle, evidence_a, evidence_b)
    state = classify_publication_state(
        kindle,
        candidate_rows=candidate,
        retained_rows=retained,
        evidence_a=evidence_a,
        evidence_b=evidence_b,
        had_upgrade=had_upgrade,
    )
    external_errors = observe_external_state(sentinels or {})
    notes = summarize_identity(candidate, retained)
    pending_status, _ = inspect_pending_health(kindle)
    notes.append(f"pending_health={pending_status}")
    if _journal_path(kindle).is_file():
        notes.append("activate_journal=present")
    notes.append(
        "recovery_version=generated-by-install-recovery-not-package-direct-copy"
    )
    notes.append("registration_source=transformed-when-register-enabled")
    notes.append("pending_health_never_implies_qualification=true")
    return render_report(
        state=state,
        version_a=evidence_a.version,
        version_b=evidence_b.version,
        install_rc=install_rc,
        interrupt_point=interrupt_point,
        candidate_rows=candidate,
        retained_rows=retained,
        external_errors=external_errors,
        notes=notes,
    )


def distinguish_payload_bytes(payload: Path, tag: bytes) -> None:
    """Make release-owned direct-copy bytes unique without breaking recovery digests."""
    skip_prefix = ("recovery/",)
    for role, rel in PACKAGE_RELATIVE.items():
        if rel is None:
            continue
        if any(rel.startswith(prefix) for prefix in skip_prefix):
            continue
        path = payload / rel
        if not path.is_file():
            continue
        if role in {"scribe-app", "scribe-lifecycle"} or rel.endswith(".png"):
            path.write_bytes(path.read_bytes() + tag)
        else:
            path.write_bytes(path.read_bytes() + b"\n# " + tag + b"\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kindle-root", type=Path, required=True)
    parser.add_argument("--payload-a", type=Path, required=True)
    parser.add_argument("--version-a", required=True)
    parser.add_argument("--payload-b", type=Path, required=True)
    parser.add_argument("--version-b", required=True)
    parser.add_argument("--had-upgrade", action="store_true")
    parser.add_argument("--install-rc", type=int, default=None)
    parser.add_argument("--interrupt-point", default=None)
    parser.add_argument("--report", type=Path, default=None)
    args = parser.parse_args(argv)

    evidence_a = build_release_evidence(args.payload_a, args.version_a)
    evidence_b = build_release_evidence(args.payload_b, args.version_b)
    report = observe_rehearsal(
        args.kindle_root,
        evidence_a,
        evidence_b,
        had_upgrade=args.had_upgrade,
        install_rc=args.install_rc,
        interrupt_point=args.interrupt_point,
    )
    if args.report is not None:
        args.report.write_bytes(report)
    sys.stdout.buffer.write(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
