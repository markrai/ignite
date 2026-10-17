#!/usr/bin/env python3
"""Build a self-extracting Install Ignite.sh (shell prefix + gzip tar)."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import sys
import tarfile
from pathlib import Path

OFFSET_PLACEHOLDER = "IGNITE_ARCHIVE_OFFSET=0000000000"
BYTES_PLACEHOLDER = "IGNITE_ARCHIVE_BYTES=0000000000"
SHA_PLACEHOLDER = "IGNITE_ARCHIVE_SHA256=" + ("0" * 64)
MD5_PLACEHOLDER = "IGNITE_ARCHIVE_MD5=" + ("0" * 32)

SKIP_PAYLOAD_NAMES = {
    "Install-Ignite.sh",
}


def _tarinfo(arcname: str, path: Path) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name=arcname.replace("\\", "/"))
    info.size = path.stat().st_size
    info.mtime = 0
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    info.mode = 0o755 if path.suffix == ".sh" or path.name in {"scribe-app", "scribe-lifecycle"} else 0o644
    if path.name in {"scribe-app", "scribe-lifecycle"} or path.suffix == ".sh":
        info.mode = 0o755
    return info


def build_payload_tgz(payload_dir: Path, version: str) -> bytes:
    """Return gzip+ustar bytes of Ignite-Kindle-<version>/..."""
    payload_dir = payload_dir.resolve()
    prefix = f"Ignite-Kindle-{version}"
    buf = io.BytesIO()
    # mtime=0 gzip header via GzipFile
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tf:
        files = sorted(p for p in payload_dir.rglob("*") if p.is_file())
        for path in files:
            rel = path.relative_to(payload_dir).as_posix()
            if rel in SKIP_PAYLOAD_NAMES:
                continue
            arcname = f"{prefix}/{rel}"
            info = _tarinfo(arcname, path)
            with path.open("rb") as fh:
                tf.addfile(info, fh)
    ustar = raw.getvalue()
    gz = gzip.GzipFile(fileobj=buf, mode="wb", compresslevel=9, mtime=0)
    gz.write(ustar)
    gz.close()
    return buf.getvalue()


def archive_md5(data: bytes) -> str:
    try:
        return hashlib.md5(data, usedforsecurity=False).hexdigest()
    except TypeError:
        return hashlib.md5(data).hexdigest()


def render_prefix(template: str, version: str, archive: bytes) -> bytes:
    sha = hashlib.sha256(archive).hexdigest()
    md5 = archive_md5(archive)
    text = template.replace("@@IGNITE_PACKAGE_VERSION@@", version)
    if BYTES_PLACEHOLDER not in text:
        raise ValueError("template missing IGNITE_ARCHIVE_BYTES placeholder")
    if OFFSET_PLACEHOLDER not in text:
        raise ValueError("template missing IGNITE_ARCHIVE_OFFSET placeholder")
    if SHA_PLACEHOLDER not in text:
        raise ValueError("template missing IGNITE_ARCHIVE_SHA256 placeholder")
    if MD5_PLACEHOLDER not in text:
        raise ValueError("template missing IGNITE_ARCHIVE_MD5 placeholder")
    text = text.replace(BYTES_PLACEHOLDER, f"IGNITE_ARCHIVE_BYTES={len(archive):010d}")
    text = text.replace(SHA_PLACEHOLDER, f"IGNITE_ARCHIVE_SHA256={sha}")
    text = text.replace(MD5_PLACEHOLDER, f"IGNITE_ARCHIVE_MD5={md5}")
    if not text.endswith("\n"):
        text += "\n"
    prefix = text.encode("utf-8")
    if b"\r" in prefix:
        raise ValueError("SFX prefix must be LF-only")
    offset = len(prefix)
    patched = prefix.replace(
        OFFSET_PLACEHOLDER.encode("ascii"),
        f"IGNITE_ARCHIVE_OFFSET={offset:010d}".encode("ascii"),
        1,
    )
    if len(patched) != offset:
        raise ValueError("OFFSET patch changed prefix length")
    return patched


def build_sfx(payload_dir: Path, version: str, template: str) -> bytes:
    archive = build_payload_tgz(payload_dir, version)
    prefix = render_prefix(template, version, archive)
    return prefix + archive


def split_sfx(blob: bytes) -> tuple[bytes, bytes, dict[str, str]]:
    """Split prefix/archive using the stamped OFFSET. Raises ValueError if malformed."""
    # Parse stamped assignments from the prefix text (ASCII).
    head = blob[:4096]
    meta: dict[str, str] = {}
    for line in head.split(b"\n"):
        if line.startswith(b"IGNITE_ARCHIVE_OFFSET="):
            meta["offset"] = line.split(b"=", 1)[1].decode("ascii")
        elif line.startswith(b"IGNITE_ARCHIVE_BYTES="):
            meta["bytes"] = line.split(b"=", 1)[1].decode("ascii")
        elif line.startswith(b"IGNITE_ARCHIVE_SHA256="):
            meta["sha256"] = line.split(b"=", 1)[1].decode("ascii")
        elif line.startswith(b"IGNITE_ARCHIVE_MD5="):
            meta["md5"] = line.split(b"=", 1)[1].decode("ascii")
        elif line.startswith(b"IGNITE_PACKAGE_VERSION="):
            meta["version"] = line.split(b"=", 1)[1].decode("ascii").strip("'\"")
        if len(meta) >= 5:
            break
    for key in ("offset", "bytes", "sha256", "md5"):
        if key not in meta:
            raise ValueError(f"SFX missing {key}")
    offset = int(meta["offset"])
    size = int(meta["bytes"])
    if offset < 1 or size < 1:
        raise ValueError("SFX offset/size invalid")
    if len(blob) < offset + size:
        raise ValueError("SFX truncated")
    if len(blob) != offset + size:
        raise ValueError("SFX has trailing garbage")
    prefix = blob[:offset]
    archive = blob[offset : offset + size]
    if archive_md5(archive) != meta["md5"]:
        raise ValueError("SFX archive MD5 mismatch")
    if hashlib.sha256(archive).hexdigest() != meta["sha256"]:
        raise ValueError("SFX archive SHA-256 mismatch")
    if archive[:2] != b"\x1f\x8b":
        raise ValueError("SFX archive is not gzip")
    return prefix, archive, meta


def extract_payload(archive: bytes, dest: Path) -> Path:
    dest.mkdir(parents=True, exist_ok=True)
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as tf:
        kwargs: dict[str, object] = {}
        if sys.version_info >= (3, 12):
            kwargs["filter"] = "data"
        tf.extractall(dest, **kwargs)  # type: ignore[arg-type]
    roots = [p for p in dest.iterdir() if p.is_dir()]
    if len(roots) != 1:
        raise ValueError(f"expected one payload root, got {roots!r}")
    return roots[0]


def default_template() -> str:
    path = Path(__file__).resolve().parent / "Install-Ignite.sfx.sh"
    return path.read_text(encoding="utf-8").replace("\r\n", "\n").replace("\r", "\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--payload", type=Path, required=True, help="Ignite-Kindle-<version> directory")
    parser.add_argument("--version", required=True)
    parser.add_argument("--out", type=Path, required=True, help="Install Ignite vVERSION.sh output path")
    parser.add_argument("--template", type=Path, default=None)
    args = parser.parse_args(argv)
    template = args.template.read_text(encoding="utf-8") if args.template else default_template()
    blob = build_sfx(args.payload, args.version, template)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_bytes(blob)
    print(f"sfx_ok bytes={len(blob)} path={args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
