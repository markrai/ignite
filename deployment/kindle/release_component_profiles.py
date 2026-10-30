#!/usr/bin/env python3
"""Canonical Ignite Kindle release-component profiles (Slice 3D-B2a.4.3c).

Derives deterministic package / candidate-B / retained-A-future-layout profile
bytes from an actual staged release payload. This is build evidence only: it is
not a signed admission token and does not prove installed or executed objects.

The retained-A-future-layout profile describes how *this* release would look if
it were later retained under APP/previous-release/. It cannot authenticate a
device's currently retained previous release.

Role identifiers match ignite/kindle/scribe-app/src/storage_release_provenance.rs
COMPONENT_ROLES (asserted by tests). Candidate-B destinations match
storage_installed_slot_binding.rs::expected_component_path(CandidateB) using
symbolic roots APP / DOCS / REC (not production absolute paths).
"""

from __future__ import annotations

import argparse
import hashlib
import os
import stat
import sys
from pathlib import Path

PROFILE_HEADER = "IGNITE_RELEASE_COMPONENT_PROFILE_V1"
PROFILE_SCHEMA = "1"
MAX_COMPONENT_BYTES = 512 * 1024 * 1024
MAX_PAYLOAD_FILES = 4096
MAX_PAYLOAD_DIRS = 256
MAX_PROFILE_BYTES = 64 * 1024
READ_CHUNK = 1024 * 1024

PROFILE_KINDS = ("package", "candidate-b", "retained-a-future-layout")

# Exact order matches storage_release_provenance::COMPONENT_ROLES.
COMPONENT_ROLES: tuple[str, ...] = (
    "scribe-app",
    "scribe-lifecycle",
    "scribe-lifecycle-host",
    "launcher",
    "registration-source",
    "icon",
    "license-noto-emoji",
    "license-noto-emoji-description",
    "license-noto-emoji-upstream-info",
    "license-source-sans-3",
    "install-runtime",
    "uninstall-runtime",
    "backup-runtime",
    "register-helpers",
    "install-recovery",
    "uninstall-entry",
    "backup-entry",
    "diagnose-entry",
    "recovery-dropbear",
    "recovery-elf-sha256",
    "recovery-provenance",
    "recovery-version",
)

# Package-relative paths inside Ignite-Kindle-<version>/. None = never packaged.
PACKAGE_RELATIVE: dict[str, str | None] = {
    "scribe-app": "bin/scribe-app",
    "scribe-lifecycle": "bin/scribe-lifecycle",
    "scribe-lifecycle-host": "host/scribe-lifecycle-host.sh",
    "launcher": "launcher/Scribe.sh",
    "registration-source": "registration/ignite.json",
    "icon": "assets/scribe-icon.png",
    "license-noto-emoji": "assets/licenses/noto-emoji/OFL.txt",
    "license-noto-emoji-description": "assets/licenses/noto-emoji/DESCRIPTION.en_us.html",
    "license-noto-emoji-upstream-info": "assets/licenses/noto-emoji/upstream_info.md",
    "license-source-sans-3": "assets/licenses/source-sans-3/OFL.txt",
    "install-runtime": "lib/install-runtime.sh",
    "uninstall-runtime": "lib/uninstall-runtime.sh",
    "backup-runtime": "lib/backup-runtime.sh",
    "register-helpers": "lib/register-helpers.sh",
    "install-recovery": "lib/install-recovery.sh",
    "uninstall-entry": "Uninstall-Ignite.sh",
    "backup-entry": "Backup-Ignite-Data.sh",
    "diagnose-entry": "Diagnose-Ignite.sh",
    "recovery-dropbear": "recovery/bin/dropbearmulti",
    "recovery-elf-sha256": "recovery/manifest/elf.sha256",
    "recovery-provenance": "recovery/manifest/provenance.txt",
    "recovery-version": None,
}

# Closed directory set allowed under the payload root (relative POSIX paths).
ALLOWED_PAYLOAD_DIRS: frozenset[str] = frozenset(
    {
        "bin",
        "host",
        "launcher",
        "registration",
        "assets",
        "assets/licenses",
        "assets/licenses/noto-emoji",
        "assets/licenses/source-sans-3",
        "lib",
        "recovery",
        "recovery/bin",
        "recovery/manifest",
    }
)

# Candidate-B installed destinations (symbolic roots). Verified against
# storage_installed_slot_binding.rs::expected_component_path(CandidateB) and
# install-runtime.sh install destinations. "-" = no authenticated file path.
CANDIDATE_B_DESTINATION: dict[str, str] = {
    "scribe-app": "APP/bin/scribe-app",
    "scribe-lifecycle": "APP/bin/scribe-lifecycle",
    "scribe-lifecycle-host": "APP/bin/scribe-lifecycle-host.sh",
    "launcher": "DOCS/Scribe.sh",
    "registration-source": "-",
    "icon": "APP/assets/scribe-icon.png",
    "license-noto-emoji": "APP/assets/licenses/noto-emoji/OFL.txt",
    "license-noto-emoji-description": "APP/assets/licenses/noto-emoji/DESCRIPTION.en_us.html",
    "license-noto-emoji-upstream-info": "APP/assets/licenses/noto-emoji/upstream_info.md",
    "license-source-sans-3": "APP/assets/licenses/source-sans-3/OFL.txt",
    "install-runtime": "APP/tools/ignite-release/lib/install-runtime.sh",
    "uninstall-runtime": "APP/tools/ignite-release/lib/uninstall-runtime.sh",
    "backup-runtime": "APP/tools/ignite-release/lib/backup-runtime.sh",
    "register-helpers": "APP/tools/ignite-release/lib/register-helpers.sh",
    "install-recovery": "APP/tools/ignite-release/lib/install-recovery.sh",
    "uninstall-entry": "DOCS/Uninstall Ignite.sh",
    "backup-entry": "DOCS/Backup Ignite Data.sh",
    "diagnose-entry": "DOCS/Diagnose Ignite.sh",
    "recovery-dropbear": "REC/bin/dropbearmulti",
    "recovery-elf-sha256": "REC/manifest/elf.sha256",
    "recovery-provenance": "REC/manifest/provenance.txt",
    "recovery-version": "REC/manifest/version",
}

# Future retention layout for *this* release under APP/previous-release/.
# Not a claim about any currently retained previous release on a device.
RETAINED_A_FUTURE_LAYOUT: dict[str, str] = {
    "scribe-app": "APP/previous-release/scribe-app",
    "scribe-lifecycle": "APP/previous-release/scribe-lifecycle",
    "scribe-lifecycle-host": "APP/previous-release/scribe-lifecycle-host.sh",
    "launcher": "APP/previous-release/Scribe.sh",
    "registration-source": "APP/previous-release/appreg-handler.sql",
    "icon": "APP/previous-release/scribe-icon.png",
    "license-noto-emoji": "APP/previous-release/OFL.txt",
    "license-noto-emoji-description": "APP/previous-release/DESCRIPTION.en_us.html",
    "license-noto-emoji-upstream-info": "APP/previous-release/upstream_info.md",
    "license-source-sans-3": "APP/previous-release/source-sans-3-OFL.txt",
    "install-runtime": "APP/previous-release/install-runtime.sh",
    "uninstall-runtime": "APP/previous-release/uninstall-runtime.sh",
    "backup-runtime": "APP/previous-release/backup-runtime.sh",
    "register-helpers": "APP/previous-release/register-helpers.sh",
    "install-recovery": "APP/previous-release/install-recovery.sh",
    "uninstall-entry": "APP/previous-release/Uninstall Ignite.sh",
    "backup-entry": "APP/previous-release/Backup Ignite Data.sh",
    "diagnose-entry": "APP/previous-release/Diagnose Ignite.sh",
    "recovery-dropbear": "-",
    "recovery-elf-sha256": "-",
    "recovery-provenance": "-",
    "recovery-version": "-",
}

PACKAGE_CLASS: dict[str, str] = {
    "scribe-app": "direct-copy",
    "scribe-lifecycle": "direct-copy",
    "scribe-lifecycle-host": "direct-copy",
    "launcher": "direct-copy",
    "registration-source": "direct-copy",
    "icon": "direct-copy",
    "license-noto-emoji": "direct-copy",
    "license-noto-emoji-description": "optional",
    "license-noto-emoji-upstream-info": "optional",
    "license-source-sans-3": "direct-copy",
    "install-runtime": "direct-copy",
    "uninstall-runtime": "direct-copy",
    "backup-runtime": "direct-copy",
    "register-helpers": "direct-copy",
    "install-recovery": "direct-copy",
    "uninstall-entry": "direct-copy",
    "backup-entry": "direct-copy",
    "diagnose-entry": "direct-copy",
    "recovery-dropbear": "direct-copy",
    "recovery-elf-sha256": "direct-copy",
    "recovery-provenance": "direct-copy",
    "recovery-version": "generated",
}

CANDIDATE_B_CLASS: dict[str, str] = {
    **PACKAGE_CLASS,
    "registration-source": "transformed",
    "recovery-version": "generated",
}

RETAINED_A_FUTURE_CLASS: dict[str, str] = {
    "scribe-app": "direct-copy",
    "scribe-lifecycle": "direct-copy",
    "scribe-lifecycle-host": "direct-copy",
    "launcher": "direct-copy",
    "registration-source": "transformed",
    "icon": "direct-copy",
    "license-noto-emoji": "direct-copy",
    "license-noto-emoji-description": "optional",
    "license-noto-emoji-upstream-info": "optional",
    "license-source-sans-3": "direct-copy",
    "install-runtime": "direct-copy",
    "uninstall-runtime": "direct-copy",
    "backup-runtime": "direct-copy",
    "register-helpers": "direct-copy",
    "install-recovery": "direct-copy",
    "uninstall-entry": "direct-copy",
    "backup-entry": "direct-copy",
    "diagnose-entry": "direct-copy",
    "recovery-dropbear": "shared",
    "recovery-elf-sha256": "shared",
    "recovery-provenance": "shared",
    "recovery-version": "shared",
}

MANDATORY_PACKAGE_ROLES = frozenset(
    role
    for role, rel in PACKAGE_RELATIVE.items()
    if rel is not None and PACKAGE_CLASS[role] == "direct-copy"
)


class ProfileError(Exception):
    pass


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _refuse_if_not_plain_file(st: os.stat_result, path: Path) -> None:
    if stat.S_ISLNK(st.st_mode):
        raise ProfileError(f"symlink refused: {path}")
    if not stat.S_ISREG(st.st_mode):
        raise ProfileError(f"special file refused: {path}")
    if st.st_nlink != 1:
        raise ProfileError(f"hard link refused (nlink={st.st_nlink}): {path}")


def _refuse_if_not_plain_dir(st: os.stat_result, path: Path) -> None:
    if stat.S_ISLNK(st.st_mode):
        raise ProfileError(f"symlink directory refused: {path}")
    if not stat.S_ISDIR(st.st_mode):
        raise ProfileError(f"special directory entry refused: {path}")


def _same_plain_file_identity(a: os.stat_result, b: os.stat_result) -> bool:
    """True when a and b describe the same plain file object for substitution checks."""
    if not (stat.S_ISREG(a.st_mode) and stat.S_ISREG(b.st_mode)):
        return False
    if a.st_nlink != 1 or b.st_nlink != 1:
        return False
    if a.st_size != b.st_size:
        return False
    a_mtime = getattr(a, "st_mtime_ns", int(a.st_mtime * 1_000_000_000))
    b_mtime = getattr(b, "st_mtime_ns", int(b.st_mtime * 1_000_000_000))
    if a_mtime != b_mtime:
        return False
    if sys.platform == "win32":
        # Windows inode/dev/ctime are not reliable across lstat vs fstat.
        return True
    a_ctime = getattr(a, "st_ctime_ns", int(a.st_ctime * 1_000_000_000))
    b_ctime = getattr(b, "st_ctime_ns", int(b.st_ctime * 1_000_000_000))
    return (
        a.st_dev == b.st_dev
        and a.st_ino == b.st_ino
        and a_ctime == b_ctime
    )


def _read_bounded_fd(path: Path) -> bytes:
    """Open without following links, hash via fd, re-check identity after read."""
    path_str = os.fspath(path)
    try:
        before = os.lstat(path_str)
    except OSError as exc:
        raise ProfileError(f"cannot lstat {path}: {exc}") from exc
    _refuse_if_not_plain_file(before, path)
    if before.st_size > MAX_COMPONENT_BYTES:
        raise ProfileError(f"component exceeds size bound: {path}")

    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    if hasattr(os, "O_BINARY"):
        flags |= os.O_BINARY
    try:
        fd = os.open(path_str, flags)
    except OSError as exc:
        raise ProfileError(f"cannot open {path}: {exc}") from exc
    try:
        opened = os.fstat(fd)
        _refuse_if_not_plain_file(opened, path)
        if not _same_plain_file_identity(before, opened):
            raise ProfileError(f"component identity changed before read: {path}")
        if opened.st_size > MAX_COMPONENT_BYTES:
            raise ProfileError(f"component exceeds size bound: {path}")

        remaining = opened.st_size
        chunks: list[bytes] = []
        while remaining > 0:
            chunk = os.read(fd, min(READ_CHUNK, remaining))
            if not chunk:
                raise ProfileError(f"component truncated while reading: {path}")
            remaining -= len(chunk)
            chunks.append(chunk)
        # Detect substitution / growth: any extra bytes after declared size.
        extra = os.read(fd, 1)
        if extra:
            raise ProfileError(f"component grew while reading: {path}")

        after = os.fstat(fd)
        _refuse_if_not_plain_file(after, path)
        if not _same_plain_file_identity(opened, after):
            raise ProfileError(f"component substituted while reading: {path}")
        data = b"".join(chunks)
        if len(data) != opened.st_size:
            raise ProfileError(f"component size mismatch while reading: {path}")
        return data
    finally:
        os.close(fd)


def _enumerate_payload_files(payload: Path) -> list[str]:
    try:
        root_st = payload.lstat()
    except OSError as exc:
        raise ProfileError(f"cannot lstat payload root: {exc}") from exc
    _refuse_if_not_plain_dir(root_st, payload)

    found: list[str] = []
    dir_count = 0
    for dirpath, dirnames, filenames in os.walk(payload, followlinks=False, topdown=True):
        base = Path(dirpath)
        try:
            base_st = base.lstat()
        except OSError as exc:
            raise ProfileError(f"cannot lstat {base}: {exc}") from exc
        _refuse_if_not_plain_dir(base_st, base)

        if base != payload:
            rel_dir = base.relative_to(payload).as_posix()
            if rel_dir not in ALLOWED_PAYLOAD_DIRS:
                raise ProfileError(f"unexpected payload directory: {rel_dir}")
            dir_count += 1
            if dir_count > MAX_PAYLOAD_DIRS:
                raise ProfileError("payload directory enumeration exceeded bound")

        # Refuse symlink / unexpected child directories before descending.
        refused_dirs: list[str] = []
        for name in list(dirnames):
            child = base / name
            try:
                child_st = child.lstat()
            except OSError as exc:
                raise ProfileError(f"cannot lstat {child}: {exc}") from exc
            if stat.S_ISLNK(child_st.st_mode):
                raise ProfileError(f"symlink directory refused: {child}")
            if not stat.S_ISDIR(child_st.st_mode):
                raise ProfileError(f"special directory entry refused: {child}")
            child_rel = child.relative_to(payload).as_posix()
            if child_rel not in ALLOWED_PAYLOAD_DIRS:
                raise ProfileError(f"unexpected payload directory: {child_rel}")
            refused_dirs.append(name)
        dirnames[:] = sorted(refused_dirs)

        for name in sorted(filenames):
            path = base / name
            try:
                st = path.lstat()
            except OSError as exc:
                raise ProfileError(f"cannot lstat {path}: {exc}") from exc
            if stat.S_ISLNK(st.st_mode):
                raise ProfileError(f"symlink refused: {path}")
            if not stat.S_ISREG(st.st_mode):
                raise ProfileError(f"special file refused: {path}")
            rel = path.relative_to(payload).as_posix()
            found.append(rel)
            if len(found) > MAX_PAYLOAD_FILES:
                raise ProfileError("payload file enumeration exceeded bound")
    return found


def inspect_package_components(payload: Path) -> dict[str, tuple[str, int, str]]:
    """Return role -> (presence, bytes, sha256_or_dash) for the package view."""
    if set(PACKAGE_RELATIVE) != set(COMPONENT_ROLES):
        raise ProfileError("PACKAGE_RELATIVE keys must equal COMPONENT_ROLES")
    if set(CANDIDATE_B_DESTINATION) != set(COMPONENT_ROLES):
        raise ProfileError("CANDIDATE_B_DESTINATION keys must equal COMPONENT_ROLES")
    if set(RETAINED_A_FUTURE_LAYOUT) != set(COMPONENT_ROLES):
        raise ProfileError("RETAINED_A_FUTURE_LAYOUT keys must equal COMPONENT_ROLES")

    listed = _enumerate_payload_files(payload)
    expected_rels = {rel for rel in PACKAGE_RELATIVE.values() if rel is not None}
    unknown = [rel for rel in listed if rel not in expected_rels]
    if unknown:
        raise ProfileError(f"unknown release-owned payload paths: {unknown[:8]}")

    observed: dict[str, tuple[str, int, str]] = {}
    for role in COMPONENT_ROLES:
        rel = PACKAGE_RELATIVE[role]
        if rel is None:
            observed[role] = ("absent", 0, "-")
            continue
        path = payload / rel
        if not path.exists():
            if PACKAGE_CLASS[role] == "optional":
                observed[role] = ("absent", 0, "-")
                continue
            if role in MANDATORY_PACKAGE_ROLES or PACKAGE_CLASS[role] == "direct-copy":
                raise ProfileError(f"missing mandatory package component: {role} ({rel})")
            observed[role] = ("absent", 0, "-")
            continue
        data = _read_bounded_fd(path)
        observed[role] = ("file", len(data), sha256_hex(data))

    for role in MANDATORY_PACKAGE_ROLES:
        if observed[role][0] != "file":
            raise ProfileError(f"missing mandatory package component: {role}")
    return observed


def _mapping_path(profile: str, role: str) -> str:
    if profile == "package":
        rel = PACKAGE_RELATIVE[role]
        return rel if rel is not None else "-"
    if profile == "candidate-b":
        return CANDIDATE_B_DESTINATION[role]
    if profile == "retained-a-future-layout":
        return RETAINED_A_FUTURE_LAYOUT[role]
    raise ProfileError(f"unknown profile kind: {profile}")


def _class_for(profile: str, role: str) -> str:
    if profile == "package":
        return PACKAGE_CLASS[role]
    if profile == "candidate-b":
        return CANDIDATE_B_CLASS[role]
    if profile == "retained-a-future-layout":
        return RETAINED_A_FUTURE_CLASS[role]
    raise ProfileError(f"unknown profile kind: {profile}")


def render_profile(
    *,
    profile: str,
    version: str,
    observed: dict[str, tuple[str, int, str]],
    capabilities_sha256: str | None = None,
) -> bytes:
    """Render canonical profile bytes. Digests always come from the package observation."""
    if profile not in PROFILE_KINDS:
        raise ProfileError(f"unsupported profile: {profile}")
    if not version or "/" in version or "\\" in version or "\n" in version:
        raise ProfileError("invalid release version")

    lines: list[str] = [
        PROFILE_HEADER,
        f"schema={PROFILE_SCHEMA}",
        f"profile={profile}",
        f"release_version={version}",
        "authentication=build-evidence-only",
        "installation_binding=unverified",
        "execution_binding=unverified",
        "first_launch_admission=unavailable",
        "data_compatibility=unknown",
        "promotion_authorized=false",
    ]
    if profile == "retained-a-future-layout":
        lines.extend(
            [
                "layout_subject=this-release-if-retained",
                "device_retained_a_identity=unauthenticated",
            ]
        )
    lines.append(f"component_count={len(COMPONENT_ROLES)}")
    if capabilities_sha256 is not None:
        if len(capabilities_sha256) != 64 or any(
            ch not in "0123456789abcdef" for ch in capabilities_sha256
        ):
            raise ProfileError("capabilities_sha256 must be 64 lowercase hex chars")
        lines.append(f"capabilities_sha256={capabilities_sha256}")

    for role in COMPONENT_ROLES:
        presence, nbytes, digest = observed[role]
        cls = _class_for(profile, role)
        mapped = _mapping_path(profile, role)
        if profile == "retained-a-future-layout" and RETAINED_A_FUTURE_CLASS[role] == "shared":
            # Shared recovery is not part of a retained previous-release layout.
            presence, nbytes, digest = "absent", 0, "-"
        elif profile == "candidate-b" and role == "recovery-version":
            presence, nbytes, digest = "absent", 0, "-"
        elif profile == "candidate-b" and role == "registration-source":
            # Transformed into appreg; package digest is not the live registration.
            presence, nbytes, digest = "absent", 0, "-"
        elif profile == "retained-a-future-layout" and role == "registration-source":
            # Transformed SQL snapshot; package digest is not that representation.
            presence, nbytes, digest = "absent", 0, "-"
        if presence == "file":
            lines.append(
                f"component={role}|file|{nbytes}|{digest}|{cls}|{mapped}"
            )
        else:
            lines.append(f"component={role}|absent|0|-|{cls}|{mapped}")

    text = "\n".join(lines) + "\n"
    raw = text.encode("ascii")
    if len(raw) > MAX_PROFILE_BYTES:
        raise ProfileError("profile exceeds size bound")
    return raw


def generate_profiles(
    payload: Path,
    version: str,
    *,
    capabilities_sha256: str | None = None,
) -> dict[str, bytes]:
    observed = inspect_package_components(payload)
    return {
        kind: render_profile(
            profile=kind,
            version=version,
            observed=observed,
            capabilities_sha256=capabilities_sha256,
        )
        for kind in PROFILE_KINDS
    }


def verify_deterministic(payload: Path, version: str) -> dict[str, bytes]:
    first = generate_profiles(payload, version)
    second = generate_profiles(payload, version)
    if first != second:
        raise ProfileError("non-deterministic profile generation")
    return first


def verify_against_payload(payload: Path, version: str, package_profile: bytes) -> None:
    regenerated = generate_profiles(payload, version)["package"]
    if regenerated != package_profile:
        raise ProfileError("package profile does not match payload")


def write_profiles(out_dir: Path, profiles: dict[str, bytes]) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    for kind, raw in profiles.items():
        path = out_dir / f"ignite-kindle-component-profile-{kind}.v1"
        path.write_bytes(raw)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--payload", type=Path, required=True, help="Ignite-Kindle-VERSION directory")
    parser.add_argument("--version", required=True, help="X.Y.Z release version")
    parser.add_argument(
        "--out-dir",
        type=Path,
        help="write profile files (optional; default print package profile to stdout)",
    )
    parser.add_argument(
        "--capabilities-sha256",
        default=None,
        help="optional lowercase hex digest of storage-capabilities declaration",
    )
    parser.add_argument(
        "--check-deterministic",
        action="store_true",
        help="generate twice and require identical bytes",
    )
    args = parser.parse_args(argv)

    try:
        if args.check_deterministic:
            profiles = verify_deterministic(args.payload, args.version)
        else:
            profiles = generate_profiles(
                args.payload,
                args.version,
                capabilities_sha256=args.capabilities_sha256,
            )
        if args.out_dir is not None:
            write_profiles(args.out_dir, profiles)
        else:
            sys.stdout.buffer.write(profiles["package"])
    except ProfileError as exc:
        print(f"release_component_profiles: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
