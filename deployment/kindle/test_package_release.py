"""Tests for the shared Kindle packager (no Docker, no physical Kindle)."""

from __future__ import annotations

import hashlib
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parents[1]
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

from package_release import (  # noqa: E402
    PackageError,
    enforce_dropbear_pin,
    package,
    sha256_file,
)
from test_validate_stage import _write_arm32le_elf, make_payload  # noqa: E402
from validate_stage import installer_sfx_name, validate_sfx  # noqa: E402


def _pinned(path: Path, elf: Path) -> Path:
    digest = hashlib.sha256(elf.read_bytes()).hexdigest()
    path.write_text(
        f"elf_sha256={digest}\nelf_bytes={elf.stat().st_size}\n",
        encoding="utf-8",
    )
    return path


def _dropbear_dir(tmp: Path, size: int = 2048) -> Path:
    d = tmp / "dropbear"
    d.mkdir()
    elf = d / "dropbearmulti"
    _write_arm32le_elf(elf, size=size)
    digest = hashlib.sha256(elf.read_bytes()).hexdigest()
    (d / "elf.sha256").write_text(digest + "\n", encoding="utf-8")
    (d / "provenance.txt").write_text(
        "name=dropbearmulti\nrole=test-stub\nupstream_version=2024.85\n",
        encoding="utf-8",
    )
    return d


def test_enforce_dropbear_pin_ok(tmp_path: Path) -> None:
    d = _dropbear_dir(tmp_path)
    pin = _pinned(tmp_path / "PINNED.txt", d / "dropbearmulti")
    sha, n = enforce_dropbear_pin(d / "dropbearmulti", pin)
    assert sha == sha256_file(d / "dropbearmulti")
    assert n == (d / "dropbearmulti").stat().st_size


def test_enforce_dropbear_pin_mismatch(tmp_path: Path) -> None:
    d = _dropbear_dir(tmp_path)
    pin = tmp_path / "PINNED.txt"
    pin.write_text(
        "elf_sha256=" + ("ab" * 32) + "\nelf_bytes=145544\n",
        encoding="utf-8",
    )
    with pytest.raises(PackageError, match="Do not update PINNED.txt"):
        enforce_dropbear_pin(d / "dropbearmulti", pin)


def test_package_emits_customer_sfx(tmp_path: Path) -> None:
    version = "0.9.8"
    bins = tmp_path / "bins"
    bins.mkdir()
    _write_arm32le_elf(bins / "scribe-app", size=4096)
    _write_arm32le_elf(bins / "scribe-lifecycle", size=4096)
    drop = _dropbear_dir(tmp_path)
    pin = _pinned(tmp_path / "PINNED.txt", drop / "dropbearmulti")
    out = tmp_path / "out"
    result = package(
        repo=_REPO,
        version=version,
        release_bin_dir=bins,
        dropbear_dir=drop,
        out_root=out,
        pinned=pin,
        support_zip=True,
    )
    sfx = Path(str(result["sfx"]))
    assert sfx.name == installer_sfx_name(version)
    assert not validate_sfx(sfx, version)
    sha_file = Path(str(result["sfx_checksum"]))
    listed = sha_file.read_text(encoding="utf-8").split()[0]
    assert listed == result["sfx_sha256"] == sha256_file(sfx)
    payload = out / "staging" / f"Ignite-Kindle-{version}"
    names = {p.name for p in payload.rglob("*") if p.is_file()}
    assert "kindle-supervisor-harness" not in names
    assert "dropbearmulti" in names
    assert Path(str(result["support_zip"])).is_file()


def test_package_pin_mismatch_refuses(tmp_path: Path) -> None:
    bins = tmp_path / "bins"
    bins.mkdir()
    _write_arm32le_elf(bins / "scribe-app", size=4096)
    _write_arm32le_elf(bins / "scribe-lifecycle", size=4096)
    drop = _dropbear_dir(tmp_path)
    pin = tmp_path / "PINNED.txt"
    pin.write_text("elf_sha256=" + ("cd" * 32) + "\nelf_bytes=12\n", encoding="utf-8")
    with pytest.raises(PackageError, match="PINNED.txt"):
        package(
            repo=_REPO,
            version="0.9.8",
            release_bin_dir=bins,
            dropbear_dir=drop,
            out_root=tmp_path / "out",
            pinned=pin,
            support_zip=False,
        )


def test_package_missing_provenance_refuses(tmp_path: Path) -> None:
    bins = tmp_path / "bins"
    bins.mkdir()
    _write_arm32le_elf(bins / "scribe-app", size=4096)
    _write_arm32le_elf(bins / "scribe-lifecycle", size=4096)
    drop = _dropbear_dir(tmp_path)
    (drop / "provenance.txt").unlink()
    pin = _pinned(tmp_path / "PINNED.txt", drop / "dropbearmulti")
    with pytest.raises(PackageError, match="provenance"):
        package(
            repo=_REPO,
            version="0.9.8",
            release_bin_dir=bins,
            dropbear_dir=drop,
            out_root=tmp_path / "out",
            pinned=pin,
            support_zip=False,
        )


def test_make_payload_fixture_still_used_by_validator(tmp_path: Path) -> None:
    # Shared inventory stays compatible with existing validate_stage fixtures.
    pkg = make_payload(tmp_path)
    assert (pkg / "bin" / "scribe-app").is_file()
