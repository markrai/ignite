#!/usr/bin/env python3
"""Independent customer-Scriptlet ↔ staged-payload ↔ package-profile equivalence.

Slice 3D-B2a.4.3d. Reads the final Install Ignite vX.Y.Z.sh, stream-inspects its
embedded gzip+ustar payload without executing shell, and compares the complete
embedded inventory to the independently staged payload and the canonical package
component profile.

A PASS means only that the inspected Scriptlet embeds the same release-owned
payload bytes as the staged tree. It does not authenticate the Scriptlet,
installed objects, execution, retained A, data compatibility, or promotion.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import os
import stat
import struct
import sys
import tarfile
import zlib
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_SFX = _HERE / "sfx"
for extra in (_HERE, _SFX):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from build_sfx import SKIP_PAYLOAD_NAMES, split_sfx  # noqa: E402
from release_component_profiles import (  # noqa: E402
    COMPONENT_ROLES,
    MAX_COMPONENT_BYTES,
    MAX_PAYLOAD_FILES,
    PACKAGE_CLASS,
    PACKAGE_RELATIVE,
    ProfileError,
    generate_profiles,
    inspect_package_components,
    sha256_hex,
)

REPORT_HEADER = "IGNITE_SFX_PAYLOAD_EQUIVALENCE_V1"
REPORT_SCHEMA = "1"
MAX_SFX_BYTES = 768 * 1024 * 1024
MAX_ARCHIVE_BYTES = 512 * 1024 * 1024
MAX_UNCOMPRESSED_BYTES = 768 * 1024 * 1024
MAX_MEMBER_PATH_LEN = 200
MAX_REPORT_BYTES = 64 * 1024
READ_CHUNK = 1024 * 1024

# Production builder (_tarinfo / GzipFile mtime=0, compresslevel=9) contract.
EXPECTED_UID = 0
EXPECTED_GID = 0
EXPECTED_MTIME = 0
EXPECTED_UNAME = ""
EXPECTED_GNAME = ""
TAR_BLOCK = 512
USTAR_MAGIC = b"ustar\x00"
USTAR_VERSION = b"00"
# Regular-file typeflags only (production emits ASCII '0').
USTAR_REG_TYPEFLAGS = (b"\x00", b"0")


class EquivalenceError(Exception):
    """Fail-closed equivalence refusal."""


def expected_member_mode(rel: str) -> int:
    """Exact permission mode the production SFX builder stamps (no type/special bits)."""
    name = Path(rel).name
    if name in {"scribe-app", "scribe-lifecycle"} or Path(rel).suffix == ".sh":
        return 0o755
    return 0o644


def _safe_rel_from_member(name: str, version: str) -> str:
    if not name or name.startswith("/") or name.startswith("\\"):
        raise EquivalenceError(f"absolute or empty archive member refused: {name!r}")
    if "\\" in name:
        raise EquivalenceError(f"backslash in archive member refused: {name!r}")
    if len(name) > MAX_MEMBER_PATH_LEN:
        raise EquivalenceError(f"archive member path too long: {name!r}")
    parts = name.split("/")
    if any(p in {"", ".", ".."} for p in parts):
        raise EquivalenceError(f"unsafe archive member path: {name!r}")
    root = f"Ignite-Kindle-{version}"
    if parts[0] != root:
        raise EquivalenceError(
            f"archive member root is {parts[0]!r}, expected {root!r}"
        )
    if len(parts) < 2:
        raise EquivalenceError(f"archive member missing payload-relative path: {name!r}")
    return "/".join(parts[1:])


def _validate_gzip_header(archive: bytes) -> None:
    """Refuse gzip headers that diverge from production GzipFile(mtime=0) output."""
    if len(archive) < 10:
        raise EquivalenceError("gzip header truncated")
    if archive[0:2] != b"\x1f\x8b":
        raise EquivalenceError("embedded archive is not gzip")
    if archive[2] != 8:
        raise EquivalenceError("gzip compression method must be deflate")
    flg = archive[3]
    if flg != 0:
        raise EquivalenceError(f"unexpected gzip header flags: {flg:#x}")
    mtime = struct.unpack_from("<I", archive, 4)[0]
    if mtime != 0:
        raise EquivalenceError(f"unexpected gzip mtime: {mtime}")


def _gunzip_exactly_one_member(archive: bytes) -> bytes:
    """Decompress exactly one gzip member via zlib; refuse leftovers/concat/zeros."""
    if len(archive) > MAX_ARCHIVE_BYTES:
        raise EquivalenceError("embedded archive exceeds size bound")
    _validate_gzip_header(archive)

    # wbits=31: zlib gzip wrapper (header + deflate + CRC32/ISIZE trailer).
    dobj = zlib.decompressobj(wbits=31)
    out = bytearray()
    try:
        chunk = dobj.decompress(archive, max_length=MAX_UNCOMPRESSED_BYTES)
        out.extend(chunk)
        # If output was capped, keep draining until the gzip member ends.
        while not dobj.eof:
            room = MAX_UNCOMPRESSED_BYTES - len(out)
            if room <= 0:
                raise EquivalenceError("uncompressed archive exceeded size bound")
            more = dobj.decompress(dobj.unconsumed_tail, max_length=room)
            if more:
                out.extend(more)
                continue
            # No progress with unconsumed tail: finish the member.
            flushed = dobj.flush()
            if flushed:
                if len(out) + len(flushed) > MAX_UNCOMPRESSED_BYTES:
                    raise EquivalenceError("uncompressed archive exceeded size bound")
                out.extend(flushed)
            break
    except zlib.error as exc:
        raise EquivalenceError(f"gzip validation failed: {exc}") from exc

    if not dobj.eof:
        raise EquivalenceError("gzip stream incomplete (missing trailer/EOF)")
    if dobj.unused_data:
        raise EquivalenceError(
            "unexpected bytes after gzip member "
            f"(leftover={len(dobj.unused_data)} total={len(archive)})"
        )
    if len(out) > MAX_UNCOMPRESSED_BYTES:
        raise EquivalenceError("uncompressed archive exceeded size bound")
    # Single buffer ownership: return bytes view without an extra staging BytesIO.
    return bytes(out)


def _parse_tar_octal(field: bytes, label: str) -> int:
    text = field.split(b"\0", 1)[0].strip()
    if not text:
        return 0
    if any(ch < 0x30 or ch > 0x37 for ch in text):
        # Allow only ASCII octal digits (refuse space-padded garbage mid-field).
        cleaned = text.replace(b" ", b"")
        if not cleaned or any(ch < 0x30 or ch > 0x37 for ch in cleaned):
            raise EquivalenceError(f"invalid tar octal field {label}")
        text = cleaned
    try:
        return int(text, 8)
    except ValueError as exc:
        raise EquivalenceError(f"invalid tar octal field {label}") from exc


def _ustar_header_checksum(header: bytes) -> int:
    """Unsigned POSIX checksum: chksum field treated as eight ASCII spaces."""
    total = 0
    for i, value in enumerate(header):
        if 148 <= i < 156:
            total += 0x20
        else:
            total += value
    return total


def _validate_ustar_framing(ustar: bytes) -> None:
    """Raw structural check for production ustar (magic/version, EOA, padding)."""
    if len(ustar) < TAR_BLOCK * 2:
        raise EquivalenceError("ustar archive too short for end-of-archive blocks")
    if len(ustar) % TAR_BLOCK != 0:
        raise EquivalenceError("ustar length is not 512-byte aligned")

    offset = 0
    members = 0
    length = len(ustar)
    zero_block = b"\0" * TAR_BLOCK

    while offset + TAR_BLOCK <= length:
        header = ustar[offset : offset + TAR_BLOCK]
        if header == zero_block:
            if offset + TAR_BLOCK * 2 > length:
                raise EquivalenceError("ustar missing second end-of-archive block")
            if ustar[offset + TAR_BLOCK : offset + TAR_BLOCK * 2] != zero_block:
                raise EquivalenceError("ustar missing second end-of-archive block")
            rest = ustar[offset + TAR_BLOCK * 2 :]
            if rest and any(rest):
                raise EquivalenceError("nonzero data after tar end-of-archive")
            if members == 0:
                raise EquivalenceError("embedded archive has no file members")
            return

        magic = header[257:263]
        version = header[263:265]
        if magic != USTAR_MAGIC or version != USTAR_VERSION:
            raise EquivalenceError(
                "non-ustar tar header refused "
                f"(magic={magic!r} version={version!r})"
            )

        typeflag = header[156:157]
        if typeflag not in USTAR_REG_TYPEFLAGS:
            raise EquivalenceError(
                f"unexpected tar typeflag {typeflag!r} at offset {offset}"
            )

        stored = _parse_tar_octal(header[148:156], "chksum")
        calculated = _ustar_header_checksum(header)
        if stored != calculated:
            raise EquivalenceError(
                f"tar header checksum mismatch at offset {offset}: "
                f"stored={stored} calculated={calculated}"
            )

        if any(header[345:500]):
            raise EquivalenceError("ustar prefix field refused (long-name form)")
        if any(header[157:257]):
            raise EquivalenceError("unexpected tar linkname in regular member")

        size = _parse_tar_octal(header[124:136], "size")
        if size < 0 or size > MAX_COMPONENT_BYTES:
            raise EquivalenceError(f"ustar member size out of bounds at offset {offset}")

        data_blocks = (size + TAR_BLOCK - 1) // TAR_BLOCK
        next_offset = offset + TAR_BLOCK + data_blocks * TAR_BLOCK
        if next_offset > length:
            raise EquivalenceError("ustar truncated: member extends past archive end")

        members += 1
        if members > MAX_PAYLOAD_FILES:
            raise EquivalenceError("embedded member count exceeded bound")
        offset = next_offset

    raise EquivalenceError("ustar missing end-of-archive blocks")


def _hash_member_incremental(tf: tarfile.TarFile, info: tarfile.TarInfo) -> tuple[int, str]:
    """Hash member bytes without retaining the full payload in memory."""
    if info.size < 0 or info.size > MAX_COMPONENT_BYTES:
        raise EquivalenceError(f"archive member size out of bounds: {info.name}")
    handle = tf.extractfile(info)
    if handle is None:
        raise EquivalenceError(f"cannot read archive member: {info.name}")
    hasher = hashlib.sha256()
    remaining = info.size
    while remaining > 0:
        chunk = handle.read(min(READ_CHUNK, remaining))
        if not chunk:
            raise EquivalenceError(f"truncated archive member: {info.name}")
        hasher.update(chunk)
        remaining -= len(chunk)
    extra = handle.read(1)
    if extra:
        raise EquivalenceError(f"archive member longer than declared size: {info.name}")
    return info.size, hasher.hexdigest()


def _refuse_extension_or_special(info: tarfile.TarInfo) -> None:
    if info.pax_headers:
        raise EquivalenceError(f"PAX extended headers refused: {info.name}")
    if getattr(info, "sparse", None) is not None:
        raise EquivalenceError(f"sparse archive member refused: {info.name}")
    if info.issym() or info.type == tarfile.SYMTYPE:
        raise EquivalenceError(f"symlink archive member refused: {info.name}")
    if info.islnk() or info.type == tarfile.LNKTYPE:
        raise EquivalenceError(f"hardlink archive member refused: {info.name}")
    if info.isdir() or info.type == tarfile.DIRTYPE:
        raise EquivalenceError(f"directory archive member refused: {info.name}")
    if info.isfifo() or info.isblk() or info.ischr() or info.isdev():
        raise EquivalenceError(f"special archive member refused: {info.name}")
    # GNU / PAX extension member types (never emitted by production USTAR builder).
    extension_types = {
        getattr(tarfile, "CONTTYPE", b"1"),
        getattr(tarfile, "GNUTYPE_LONGNAME", b"L"),
        getattr(tarfile, "GNUTYPE_LONGLINK", b"K"),
        getattr(tarfile, "GNUTYPE_SPARSE", b"S"),
        getattr(tarfile, "XHDTYPE", b"x"),
        getattr(tarfile, "XGLTYPE", b"g"),
        getattr(tarfile, "SOLARIS_XHDTYPE", b"X"),
    }
    if info.type in extension_types:
        raise EquivalenceError(
            f"extension/special archive member refused type={info.type!r}: {info.name}"
        )
    if not info.isreg() or info.type not in {tarfile.REGTYPE, tarfile.AREGTYPE}:
        raise EquivalenceError(
            f"unexpected archive member type {info.type!r}: {info.name}"
        )


def _validate_production_metadata(info: tarfile.TarInfo, rel: str) -> None:
    expected_mode = expected_member_mode(rel)
    # Exact mode: reject setuid/setgid/sticky and any other unexpected bits.
    if info.mode != expected_mode:
        raise EquivalenceError(
            f"unexpected tar mode for {rel}: got={oct(info.mode)} expected={oct(expected_mode)}"
        )
    if info.uid != EXPECTED_UID or info.gid != EXPECTED_GID:
        raise EquivalenceError(
            f"unexpected tar ownership for {rel}: uid={info.uid} gid={info.gid}"
        )
    if int(info.mtime) != EXPECTED_MTIME:
        raise EquivalenceError(f"unexpected tar mtime for {rel}: {info.mtime}")
    if info.uname != EXPECTED_UNAME or info.gname != EXPECTED_GNAME:
        raise EquivalenceError(
            f"unexpected tar owner names for {rel}: "
            f"uname={info.uname!r} gname={info.gname!r}"
        )
    if info.linkname:
        raise EquivalenceError(f"unexpected tar linkname for {rel}: {info.linkname!r}")


def inspect_embedded_inventory(
    archive: bytes,
    version: str,
) -> dict[str, tuple[int, str, int]]:
    """Fully validate gzip+ustar; return rel -> (size, sha256, exact mode)."""
    ustar = _gunzip_exactly_one_member(archive)
    # Structural pass before tarfile: genuine ustar magic/version, checksums, EOA.
    _validate_ustar_framing(ustar)

    inventory: dict[str, tuple[int, str, int]] = {}
    bio = io.BytesIO(ustar)
    try:
        with tarfile.open(fileobj=bio, mode="r:") as tf:
            for info in tf:
                _refuse_extension_or_special(info)
                rel = _safe_rel_from_member(info.name, version)
                if rel in SKIP_PAYLOAD_NAMES:
                    raise EquivalenceError(f"forbidden embedded installer member: {rel}")
                if rel in inventory:
                    raise EquivalenceError(f"duplicate archive member: {rel}")
                if len(inventory) >= MAX_PAYLOAD_FILES:
                    raise EquivalenceError("embedded member count exceeded bound")
                _validate_production_metadata(info, rel)
                size, digest = _hash_member_incremental(tf, info)
                inventory[rel] = (size, digest, info.mode)
    except EquivalenceError:
        raise
    except (tarfile.TarError, OSError, EOFError) as exc:
        raise EquivalenceError(f"malformed or truncated ustar archive: {exc}") from exc

    # Framing already required EOA + zero padding; tarfile must not leave unread
    # nonzero content either.
    rest = bio.read()
    if rest and any(byte != 0 for byte in rest):
        raise EquivalenceError("nonzero data after tar end-of-archive")

    if not inventory:
        raise EquivalenceError("embedded archive has no file members")
    return inventory


def _read_plain_file_fd(path: Path, *, min_size: int, max_size: int) -> bytes:
    """Open without following symlinks; read bounded bytes with identity checks."""
    path_str = os.fspath(path)
    try:
        before = os.lstat(path_str)
    except OSError as exc:
        raise EquivalenceError(f"cannot lstat {path}: {exc}") from exc
    if stat.S_ISLNK(before.st_mode):
        raise EquivalenceError(f"symlink refused: {path}")
    if not stat.S_ISREG(before.st_mode):
        raise EquivalenceError(f"not a regular file: {path}")
    if before.st_size < min_size or before.st_size > max_size:
        raise EquivalenceError(f"file size out of bounds: {path} ({before.st_size})")

    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    if hasattr(os, "O_BINARY"):
        flags |= os.O_BINARY
    try:
        fd = os.open(path_str, flags)
    except OSError as exc:
        raise EquivalenceError(f"cannot open {path}: {exc}") from exc
    try:
        opened = os.fstat(fd)
        if stat.S_ISLNK(opened.st_mode) or not stat.S_ISREG(opened.st_mode):
            raise EquivalenceError(f"opened path is not a plain file: {path}")
        if opened.st_size != before.st_size:
            raise EquivalenceError(f"file size changed before read: {path}")
        if sys.platform != "win32":
            if opened.st_dev != before.st_dev or opened.st_ino != before.st_ino:
                raise EquivalenceError(f"file identity changed before read: {path}")

        remaining = opened.st_size
        chunks: list[bytes] = []
        while remaining > 0:
            chunk = os.read(fd, min(READ_CHUNK, remaining))
            if not chunk:
                raise EquivalenceError(f"truncated while reading: {path}")
            remaining -= len(chunk)
            chunks.append(chunk)
        extra = os.read(fd, 1)
        if extra:
            raise EquivalenceError(f"file grew while reading: {path}")
        after = os.fstat(fd)
        if after.st_size != opened.st_size:
            raise EquivalenceError(f"file substituted while reading: {path}")
        data = b"".join(chunks)
        if len(data) != opened.st_size:
            raise EquivalenceError(f"size mismatch while reading: {path}")
        return data
    finally:
        os.close(fd)


def inspect_staged_inventory(payload: Path) -> dict[str, tuple[int, str, int]]:
    """Independently hash staged payload files (hardened package inspection)."""
    try:
        observed = inspect_package_components(payload)
    except ProfileError as exc:
        raise EquivalenceError(f"staged payload inspection failed: {exc}") from exc

    inventory: dict[str, tuple[int, str, int]] = {}
    for role in COMPONENT_ROLES:
        rel = PACKAGE_RELATIVE[role]
        if rel is None:
            continue
        presence, nbytes, digest = observed[role]
        path = payload / rel
        if presence == "absent":
            if path.exists():
                raise EquivalenceError(
                    f"staged path exists but profile observation is absent: {rel}"
                )
            continue
        if not path.is_file():
            raise EquivalenceError(f"staged component missing after observation: {rel}")
        try:
            st = os.lstat(os.fspath(path))
        except OSError as exc:
            raise EquivalenceError(f"cannot lstat staged {rel}: {exc}") from exc
        if stat.S_ISLNK(st.st_mode) or not stat.S_ISREG(st.st_mode):
            raise EquivalenceError(f"staged path is not a plain file: {rel}")
        if st.st_size != nbytes:
            raise EquivalenceError(f"staged size changed after observation: {rel}")
        inventory[rel] = (nbytes, digest, expected_member_mode(rel))
    return inventory


def _compare_inventories(
    staged: dict[str, tuple[int, str, int]],
    embedded: dict[str, tuple[int, str, int]],
) -> list[str]:
    errors: list[str] = []
    staged_keys = set(staged)
    embedded_keys = set(embedded)
    missing = sorted(staged_keys - embedded_keys)
    extra = sorted(embedded_keys - staged_keys)
    if missing:
        errors.append(f"missing embedded components: {missing[:8]}")
    if extra:
        errors.append(f"extra embedded components: {extra[:8]}")
    for rel in sorted(staged_keys & embedded_keys):
        s_size, s_digest, s_mode = staged[rel]
        e_size, e_digest, e_mode = embedded[rel]
        if s_size != e_size:
            errors.append(f"size mismatch {rel}: staged={s_size} embedded={e_size}")
        if s_digest != e_digest:
            errors.append(f"digest mismatch {rel}")
        if e_mode != s_mode:
            errors.append(
                f"mode mismatch {rel}: staged_expected={oct(s_mode)} embedded={oct(e_mode)}"
            )
    return errors


def _verify_package_profile(
    payload: Path,
    version: str,
    package_profile: bytes,
    staged: dict[str, tuple[int, str, int]],
    embedded: dict[str, tuple[int, str, int]],
) -> list[str]:
    errors: list[str] = []
    try:
        regenerated = generate_profiles(payload, version)["package"]
    except ProfileError as exc:
        return [f"cannot regenerate package profile: {exc}"]
    if regenerated != package_profile:
        errors.append("package profile does not match staged payload regeneration")

    text = package_profile.decode("ascii", errors="replace")
    if not text.startswith("IGNITE_RELEASE_COMPONENT_PROFILE_V1\n"):
        errors.append("package profile missing canonical header")
    if "profile=package\n" not in text:
        errors.append("package profile is not profile=package")
    if f"release_version={version}\n" not in text:
        errors.append("package profile release_version mismatch")
    if "promotion_authorized=false\n" not in text:
        errors.append("package profile missing promotion_authorized=false")

    lines = [line for line in text.splitlines() if line.startswith("component=")]
    if len(lines) != len(COMPONENT_ROLES):
        errors.append(
            f"package profile component_count mismatch: {len(lines)} != {len(COMPONENT_ROLES)}"
        )

    for idx, role in enumerate(COMPONENT_ROLES):
        if idx >= len(lines):
            break
        line = lines[idx]
        parts = line.split("|")
        if len(parts) != 6 or not parts[0].startswith(f"component={role}"):
            errors.append(f"package profile line malformed for role {role}")
            continue
        presence, nbytes_s, digest, cls, mapping = parts[1:]
        rel = PACKAGE_RELATIVE[role]
        if rel is None:
            if presence != "absent" or digest != "-" or mapping != "-":
                errors.append(f"generated role {role} must be absent in package profile")
            continue
        if PACKAGE_CLASS[role] == "optional" and presence == "absent":
            if rel in staged or rel in embedded:
                errors.append(f"optional role {role} absent in profile but present on disk")
            continue
        if presence == "file":
            if rel not in staged or rel not in embedded:
                errors.append(f"profile claims file for {role} but inventory missing {rel}")
                continue
            s_size, s_digest, _ = staged[rel]
            e_size, e_digest, _ = embedded[rel]
            try:
                nbytes = int(nbytes_s)
            except ValueError:
                errors.append(f"profile bytes field invalid for {role}")
                continue
            if nbytes != s_size or nbytes != e_size:
                errors.append(f"profile size disagrees with inventories for {role}")
            if digest != s_digest or digest != e_digest:
                errors.append(f"profile digest disagrees with inventories for {role}")
            if cls not in {"direct-copy", "optional"}:
                errors.append(f"unexpected package class for file role {role}: {cls}")
            if mapping != rel:
                errors.append(f"profile mapping mismatch for {role}")
        elif presence == "absent":
            if rel in staged or rel in embedded:
                errors.append(f"profile claims absent for {role} but file is present")
        else:
            errors.append(f"unknown presence {presence!r} for {role}")
    return errors


def render_report(
    *,
    result: str,
    sfx_path: Path,
    payload_path: Path,
    version: str,
    member_count: int,
    archive_sha256: str,
    package_profile_sha256: str,
    errors: list[str],
) -> bytes:
    lines = [
        REPORT_HEADER,
        f"schema={REPORT_SCHEMA}",
        f"result={result}",
        f"release_version={version}",
        f"sfx_path={sfx_path.as_posix()}",
        f"payload_path={payload_path.as_posix()}",
        f"member_count={member_count}",
        f"archive_sha256={archive_sha256}",
        f"package_profile_sha256={package_profile_sha256}",
        "equivalence=byte-identical-inventory" if result == "pass" else "equivalence=failed",
        "authentication=build-evidence-only",
        "installation_binding=unverified",
        "execution_binding=unverified",
        "first_launch_admission=unavailable",
        "data_compatibility=unknown",
        "promotion_authorized=false",
        f"error_count={len(errors)}",
    ]
    for err in errors[:32]:
        safe = err.encode("ascii", errors="replace").decode("ascii")
        if len(safe) > 240:
            safe = safe[:237] + "..."
        lines.append(f"error={safe}")
    raw = ("\n".join(lines) + "\n").encode("ascii")
    if len(raw) > MAX_REPORT_BYTES:
        raise EquivalenceError("equivalence report exceeds size bound")
    return raw


def verify_equivalence(
    *,
    sfx_path: Path,
    payload_path: Path,
    version: str,
    package_profile: bytes,
) -> tuple[bool, bytes]:
    """Return (ok, report_bytes). Never executes the Scriptlet."""
    errors: list[str] = []
    archive_sha256 = "-"
    member_count = 0
    profile_sha = sha256_hex(package_profile)

    try:
        if not version or "/" in version or "\\" in version or "\n" in version:
            raise EquivalenceError("invalid release version")

        # Read customer artifact via O_NOFOLLOW; refuse symlinked SFX paths.
        blob = _read_plain_file_fd(sfx_path, min_size=2048, max_size=MAX_SFX_BYTES)
        try:
            _prefix, archive, meta = split_sfx(blob)
        except ValueError as exc:
            raise EquivalenceError(f"SFX split failed: {exc}") from exc
        if meta.get("version") != version:
            raise EquivalenceError(
                f"SFX version is {meta.get('version')!r}, expected {version!r}"
            )
        archive_sha256 = meta["sha256"]
        if hashlib.sha256(archive).hexdigest() != archive_sha256:
            raise EquivalenceError("embedded archive SHA-256 stamp mismatch")

        embedded = inspect_embedded_inventory(archive, version)
        member_count = len(embedded)
        staged = inspect_staged_inventory(payload_path)
        errors.extend(_compare_inventories(staged, embedded))
        errors.extend(
            _verify_package_profile(
                payload_path, version, package_profile, staged, embedded
            )
        )
    except EquivalenceError as exc:
        errors.append(str(exc))

    ok = not errors
    report = render_report(
        result="pass" if ok else "fail",
        sfx_path=sfx_path,
        payload_path=payload_path,
        version=version,
        member_count=member_count,
        archive_sha256=archive_sha256,
        package_profile_sha256=profile_sha,
        errors=errors,
    )
    return ok, report


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--sfx",
        type=Path,
        required=True,
        help="final customer Install Ignite vVERSION.sh",
    )
    parser.add_argument(
        "--payload",
        type=Path,
        required=True,
        help="independently staged Ignite-Kindle-VERSION directory",
    )
    parser.add_argument("--version", required=True)
    parser.add_argument(
        "--package-profile",
        type=Path,
        required=True,
        help="canonical package component profile bytes",
    )
    parser.add_argument(
        "--report",
        type=Path,
        default=None,
        help="optional path to write the equivalence report",
    )
    args = parser.parse_args(argv)

    # Do not Path.resolve() inputs: that follows symlinks before refusal.
    profile_bytes = _read_plain_file_fd(
        args.package_profile, min_size=1, max_size=MAX_REPORT_BYTES * 4
    )
    ok, report = verify_equivalence(
        sfx_path=args.sfx,
        payload_path=args.payload,
        version=args.version,
        package_profile=profile_bytes,
    )
    if args.report is not None:
        if args.report.is_symlink():
            print("verify_sfx_payload_equivalence: report path is a symlink", file=sys.stderr)
            return 1
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_bytes(report)
    sys.stdout.buffer.write(report)
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
