"""Adversarial tests for SFX ↔ staged-payload ↔ package-profile equivalence."""

from __future__ import annotations

import gzip
import io
import sys
import tarfile
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
_SFX = _HERE / "sfx"
for extra in (_HERE, _SFX):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from build_sfx import (  # noqa: E402
    build_payload_tgz,
    build_sfx,
    default_template,
    render_prefix,
)
from release_component_profiles import (  # noqa: E402
    PACKAGE_RELATIVE,
    generate_profiles,
    sha256_hex,
)
from test_validate_stage import FIXTURE_VERSION, make_payload  # noqa: E402
from verify_sfx_payload_equivalence import (  # noqa: E402
    inspect_embedded_inventory,
    verify_equivalence,
)


def _package_profile(payload: Path, version: str = FIXTURE_VERSION) -> bytes:
    return generate_profiles(payload, version)["package"]


def _wrap_sfx(archive: bytes, version: str = FIXTURE_VERSION) -> bytes:
    return render_prefix(default_template(), version, archive) + archive


def _member_arcname(rel: str, version: str = FIXTURE_VERSION) -> str:
    return f"Ignite-Kindle-{version}/{rel}"


def _expected_mode(rel: str) -> int:
    name = Path(rel).name
    if name in {"scribe-app", "scribe-lifecycle"} or Path(rel).suffix == ".sh":
        return 0o755
    return 0o644


def _gzip_ustar(ustar: bytes) -> bytes:
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", compresslevel=9, mtime=0) as gz:
        gz.write(ustar)
    return buf.getvalue()


def _custom_ustar(
    members: list[tuple[str, bytes]],
    version: str = FIXTURE_VERSION,
    *,
    mode_override: dict[str, int] | None = None,
    uid: int = 0,
    gid: int = 0,
    mtime: int = 0,
    uname: str = "",
    gname: str = "",
) -> bytes:
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tf:
        for rel, data in members:
            info = tarfile.TarInfo(name=_member_arcname(rel, version))
            info.size = len(data)
            info.mtime = mtime
            info.uid = uid
            info.gid = gid
            info.uname = uname
            info.gname = gname
            info.mode = (mode_override or {}).get(rel, _expected_mode(rel))
            tf.addfile(info, io.BytesIO(data))
    return raw.getvalue()


def _custom_tgz(
    members: list[tuple[str, bytes]],
    version: str = FIXTURE_VERSION,
    **meta: object,
) -> bytes:
    return _gzip_ustar(_custom_ustar(members, version, **meta))  # type: ignore[arg-type]


def _assert_stamp_ok_but_equivalence_fails(sfx_path: Path, payload: Path, needle: bytes) -> None:
    """Prove refusal is from archive inspection, not merely SFX stamp mismatch."""
    from build_sfx import split_sfx

    split_sfx(sfx_path.read_bytes())  # must succeed
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert needle in report
    assert b"SFX split failed" not in report


def _inventory_from_payload(payload: Path) -> list[tuple[str, bytes]]:
    items: list[tuple[str, bytes]] = []
    for rel in PACKAGE_RELATIVE.values():
        if rel is None:
            continue
        path = payload / rel
        if path.is_file():
            items.append((rel, path.read_bytes()))
    return sorted(items, key=lambda item: item[0])


def test_valid_exact_equivalence(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(build_sfx(payload, FIXTURE_VERSION, default_template()))
    profile = _package_profile(payload)
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=profile,
    )
    assert ok
    text = report.decode("ascii")
    assert text.startswith("IGNITE_SFX_PAYLOAD_EQUIVALENCE_V1\n")
    assert "result=pass\n" in text
    assert "promotion_authorized=false\n" in text
    assert "installation_binding=unverified\n" in text
    assert "equivalence=byte-identical-inventory\n" in text


def test_repeated_verification_identical(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(build_sfx(payload, FIXTURE_VERSION, default_template()))
    profile = _package_profile(payload)
    first = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=profile,
    )
    second = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=profile,
    )
    assert first == second
    assert first[0] is True


def test_missing_embedded_component(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = [(rel, data) for rel, data in _inventory_from_payload(payload) if rel != "bin/scribe-app"]
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_custom_tgz(members)))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"missing embedded components" in report


def test_extra_embedded_component(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload) + [("lib/evil.sh", b"#!/bin/sh\nevil\n")]
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_custom_tgz(members)))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"extra embedded components" in report


def test_changed_component_bytes(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = []
    for rel, data in _inventory_from_payload(payload):
        if rel == "assets/scribe-icon.png":
            data = data + b"X"
        members.append((rel, data))
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_custom_tgz(members)))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"digest mismatch" in report or b"size mismatch" in report


def test_same_size_mutation(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = []
    for rel, data in _inventory_from_payload(payload):
        if rel == "bin/scribe-app":
            assert len(data) > 4
            data = b"X" + data[1:]
            assert len(data) == payload.joinpath(rel).stat().st_size
        members.append((rel, data))
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_custom_tgz(members)))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"digest mismatch" in report


def test_duplicate_archive_entries(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload)
    # Duplicate bin/scribe-app with different bytes; still structurally gzip+ustar.
    app = next(data for rel, data in members if rel == "bin/scribe-app")
    members = members + [("bin/scribe-app", app[:-1] + b"Z")]
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_custom_tgz(members)))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"duplicate archive member" in report


def test_path_traversal_member_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tf:
        for rel, data in _inventory_from_payload(payload):
            info = tarfile.TarInfo(name=_member_arcname(rel))
            info.size = len(data)
            info.mtime = 0
            info.mode = _expected_mode(rel)
            tf.addfile(info, io.BytesIO(data))
        evil = tarfile.TarInfo(name=f"Ignite-Kindle-{FIXTURE_VERSION}/../evil")
        evil.size = 4
        evil.mtime = 0
        evil.mode = 0o644
        tf.addfile(evil, io.BytesIO(b"evil"))
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", compresslevel=9, mtime=0) as gz:
        gz.write(raw.getvalue())
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(buf.getvalue()))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"unsafe archive member path" in report


def test_absolute_member_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tf:
        info = tarfile.TarInfo(name="/tmp/evil")
        info.size = 4
        info.mtime = 0
        info.mode = 0o644
        tf.addfile(info, io.BytesIO(b"evil"))
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", compresslevel=9, mtime=0) as gz:
        gz.write(raw.getvalue())
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(buf.getvalue()))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"absolute" in report or b"archive member root" in report


@pytest.mark.skipif(sys.platform == "win32", reason="symlink tar members exercised on Unix")
def test_symlink_member_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload)
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tf:
        for rel, data in members:
            info = tarfile.TarInfo(name=_member_arcname(rel))
            info.size = len(data)
            info.mtime = 0
            info.mode = _expected_mode(rel)
            tf.addfile(info, io.BytesIO(data))
        link = tarfile.TarInfo(name=_member_arcname("bin/scribe-app.link"))
        link.type = tarfile.SYMTYPE
        link.linkname = "scribe-app"
        link.mtime = 0
        link.mode = 0o777
        tf.addfile(link)
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", compresslevel=9, mtime=0) as gz:
        gz.write(raw.getvalue())
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(buf.getvalue()))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"symlink archive member" in report or b"unexpected tar typeflag" in report


@pytest.mark.skipif(sys.platform == "win32", reason="hardlink tar members exercised on Unix")
def test_hardlink_member_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload)
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tf:
        for rel, data in members:
            info = tarfile.TarInfo(name=_member_arcname(rel))
            info.size = len(data)
            info.mtime = 0
            info.mode = _expected_mode(rel)
            tf.addfile(info, io.BytesIO(data))
        link = tarfile.TarInfo(name=_member_arcname("bin/scribe-app.alias"))
        link.type = tarfile.LNKTYPE
        link.linkname = _member_arcname("bin/scribe-app")
        link.mtime = 0
        link.mode = 0o644
        tf.addfile(link)
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", compresslevel=9, mtime=0) as gz:
        gz.write(raw.getvalue())
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(buf.getvalue()))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"hardlink archive member" in report or b"unexpected tar typeflag" in report


def test_fifo_member_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload)
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tf:
        for rel, data in members:
            info = tarfile.TarInfo(name=_member_arcname(rel))
            info.size = len(data)
            info.mtime = 0
            info.mode = _expected_mode(rel)
            tf.addfile(info, io.BytesIO(data))
        fifo = tarfile.TarInfo(name=_member_arcname("bin/pipe"))
        fifo.type = tarfile.FIFOTYPE
        fifo.mtime = 0
        fifo.mode = 0o644
        tf.addfile(fifo)
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", compresslevel=9, mtime=0) as gz:
        gz.write(raw.getvalue())
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(buf.getvalue()))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"special archive member" in report or b"unexpected tar typeflag" in report


def test_directory_member_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload)
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tf:
        for rel, data in members:
            info = tarfile.TarInfo(name=_member_arcname(rel))
            info.size = len(data)
            info.mtime = 0
            info.mode = _expected_mode(rel)
            tf.addfile(info, io.BytesIO(data))
        directory = tarfile.TarInfo(name=_member_arcname("extras"))
        directory.type = tarfile.DIRTYPE
        directory.mtime = 0
        directory.mode = 0o755
        tf.addfile(directory)
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", compresslevel=9, mtime=0) as gz:
        gz.write(raw.getvalue())
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(buf.getvalue()))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert (
        b"directory archive member" in report
        or b"extension/special" in report
        or b"unexpected tar typeflag" in report
    )


def test_truncated_payload_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    archive = build_payload_tgz(payload, FIXTURE_VERSION)
    truncated = archive[: max(32, len(archive) // 4)]
    # Re-stamp so split_sfx integrity checks pass; equivalence must still fail on decode.
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(truncated))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"malformed" in report or b"truncated" in report


def test_wrong_version_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(build_sfx(payload, FIXTURE_VERSION, default_template()))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version="9.9.9",
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"SFX version" in report or b"expected '9.9.9'" in report


def test_incorrect_optional_presence(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    # Staged lacks optional upstream_info; embed it anyway.
    members = _inventory_from_payload(payload) + [
        ("assets/licenses/noto-emoji/upstream_info.md", b"optional\n")
    ]
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_custom_tgz(members)))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"extra embedded components" in report


def test_incorrect_staged_component_profile(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(build_sfx(payload, FIXTURE_VERSION, default_template()))
    bad_profile = _package_profile(payload).replace(
        b"promotion_authorized=false",
        b"promotion_authorized=true ",
        1,
    )
    # Keep length if possible; if replace changes length, still a wrong profile.
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=bad_profile,
    )
    assert not ok
    assert b"package profile does not match" in report or b"promotion_authorized" in report


def test_modified_sfx_after_profile_generation(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    profile = _package_profile(payload)
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(build_sfx(payload, FIXTURE_VERSION, default_template()))
    # Mutate staged file after profile captured; SFX still has original bytes.
    target = payload / "bin/scribe-lifecycle"
    original = target.read_bytes()
    target.write_bytes(b"Y" + original[1:])
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=profile,
    )
    assert not ok
    # Either inventory mismatch or profile regeneration mismatch.
    assert (
        b"digest mismatch" in report
        or b"package profile does not match" in report
        or b"profile digest disagrees" in report
    )


def test_oversized_member_refused(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import verify_sfx_payload_equivalence as mod

    monkeypatch.setattr(mod, "MAX_COMPONENT_BYTES", 64)
    payload = make_payload(tmp_path / "src")
    members = []
    for rel, data in _inventory_from_payload(payload):
        if rel == "bin/scribe-app":
            data = b"A" * 128
        members.append((rel, data))
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_custom_tgz(members)))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"size out of bounds" in report or b"size mismatch" in report


def test_member_count_bound(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import verify_sfx_payload_equivalence as mod
    from release_component_profiles import MAX_PAYLOAD_FILES as real_max

    monkeypatch.setattr(mod, "MAX_PAYLOAD_FILES", 3)
    payload = make_payload(tmp_path / "src")
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(build_sfx(payload, FIXTURE_VERSION, default_template()))
    assert real_max > 3
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"member count exceeded" in report


def test_inspect_embedded_matches_builder_inventory(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    archive = build_payload_tgz(payload, FIXTURE_VERSION)
    embedded = inspect_embedded_inventory(archive, FIXTURE_VERSION)
    for rel, data in _inventory_from_payload(payload):
        assert rel in embedded
        size, digest, mode = embedded[rel]
        assert size == len(data)
        assert digest == sha256_hex(data)
        assert mode == _expected_mode(rel)


def test_split_contract_still_required(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    sfx = build_sfx(payload, FIXTURE_VERSION, default_template())
    # Corrupt trailing byte after valid stamps by extending blob.
    broken = bytearray(sfx)
    broken.append(0x00)
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(bytes(broken))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"SFX split failed" in report


def test_pass_does_not_claim_authentication(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(build_sfx(payload, FIXTURE_VERSION, default_template()))
    _ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    text = report.decode("ascii")
    assert "authentication=build-evidence-only" in text
    assert "execution_binding=unverified" in text
    assert "first_launch_admission=unavailable" in text
    assert "data_compatibility=unknown" in text
    assert "promotion_authorized=true" not in text


def test_corrupted_gzip_crc_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    archive = bytearray(build_payload_tgz(payload, FIXTURE_VERSION))
    # CRC32 is the 4 bytes immediately before the 4-byte ISIZE trailer.
    archive[-8] ^= 0xFF
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(bytes(archive)))
    _assert_stamp_ok_but_equivalence_fails(sfx_path, payload, b"gzip validation failed")


def test_truncated_gzip_trailer_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    archive = build_payload_tgz(payload, FIXTURE_VERSION)
    truncated = archive[:-3]  # clip CRC/ISIZE trailer
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(truncated))
    from build_sfx import split_sfx

    split_sfx(sfx_path.read_bytes())
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"SFX split failed" not in report
    assert b"gzip validation failed" in report or b"gzip stream incomplete" in report


def test_appended_bytes_after_gzip_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    archive = build_payload_tgz(payload, FIXTURE_VERSION) + b"\x00\xffEXTRA"
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(archive))
    from build_sfx import split_sfx

    split_sfx(sfx_path.read_bytes())
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"SFX split failed" not in report
    assert b"unexpected bytes after gzip" in report or b"gzip validation failed" in report


def test_concatenated_gzip_members_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    first = build_payload_tgz(payload, FIXTURE_VERSION)
    # Non-empty second member so silent multi-member decompression cannot look like
    # a clean single ustar.
    second = _gzip_ustar(b"NOT-A-TAR-PAYLOAD")
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(first + second))
    _assert_stamp_ok_but_equivalence_fails(sfx_path, payload, b"unexpected bytes after gzip")


def test_empty_concatenated_gzip_member_refused(tmp_path: Path) -> None:
    """Empty second gzip member must not pass (GzipFile historically accepted this)."""
    payload = make_payload(tmp_path / "src")
    first = build_payload_tgz(payload, FIXTURE_VERSION)
    second = _gzip_ustar(b"")
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(first + second))
    _assert_stamp_ok_but_equivalence_fails(sfx_path, payload, b"unexpected bytes after gzip")


def test_trailing_zeros_after_gzip_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    archive = build_payload_tgz(payload, FIXTURE_VERSION) + (b"\x00" * 32)
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(archive))
    _assert_stamp_ok_but_equivalence_fails(sfx_path, payload, b"unexpected bytes after gzip")


def test_nonzero_after_tar_termination_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    ustar = _custom_ustar(_inventory_from_payload(payload))
    # Production tar ends with zero blocks; append nonzero content afterward.
    poisoned = ustar + b"HIDDEN" + (b"\0" * (512 - 6))
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_gzip_ustar(poisoned)))
    _assert_stamp_ok_but_equivalence_fails(
        sfx_path, payload, b"nonzero data after tar end-of-archive"
    )


def test_pax_extension_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload)
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as tf:
        for rel, data in members:
            info = tarfile.TarInfo(name=_member_arcname(rel))
            info.size = len(data)
            info.mtime = 0
            info.uid = 0
            info.gid = 0
            info.mode = _expected_mode(rel)
            info.pax_headers = {"comment": "not-production"}
            tf.addfile(info, io.BytesIO(data))
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_gzip_ustar(raw.getvalue())))
    from build_sfx import split_sfx

    split_sfx(sfx_path.read_bytes())
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"SFX split failed" not in report
    assert b"PAX" in report or b"unexpected tar typeflag" in report


def test_gnu_format_tar_refused(tmp_path: Path) -> None:
    """GNU magic/version must fail raw ustar framing even when tarfile can parse it."""
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload)
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.GNU_FORMAT) as tf:
        for rel, data in members:
            info = tarfile.TarInfo(name=_member_arcname(rel))
            info.size = len(data)
            info.mtime = 0
            info.uid = 0
            info.gid = 0
            info.uname = ""
            info.gname = ""
            info.mode = _expected_mode(rel)
            tf.addfile(info, io.BytesIO(data))
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_gzip_ustar(raw.getvalue())))
    _assert_stamp_ok_but_equivalence_fails(sfx_path, payload, b"non-ustar tar header")


def test_missing_tar_end_blocks_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    ustar = _custom_ustar(_inventory_from_payload(payload))
    # Walk block-aligned headers to the first end-of-archive zero block, then drop it.
    zero = b"\0" * 512
    offset = 0
    while offset + 512 <= len(ustar):
        block = ustar[offset : offset + 512]
        if block == zero:
            break
        size_field = block[124:136].split(b"\0", 1)[0].strip() or b"0"
        size = int(size_field, 8)
        offset += 512 + ((size + 511) // 512) * 512
    else:
        raise AssertionError("EOA not found in fixture ustar")
    truncated = ustar[:offset]
    assert len(truncated) % 512 == 0
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_gzip_ustar(truncated)))
    _assert_stamp_ok_but_equivalence_fails(
        sfx_path, payload, b"ustar missing end-of-archive"
    )


def test_gnu_longname_extension_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload)
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.GNU_FORMAT) as tf:
        for rel, data in members:
            info = tarfile.TarInfo(name=_member_arcname(rel))
            info.size = len(data)
            info.mtime = 0
            info.uid = 0
            info.gid = 0
            info.mode = _expected_mode(rel)
            tf.addfile(info, io.BytesIO(data))
        # Explicit GNU long-name record (never emitted by production USTAR builder).
        longname = tarfile.TarInfo(name="././@LongLink")
        longname.type = tarfile.GNUTYPE_LONGNAME
        payload_name = (_member_arcname("bin/" + ("a" * 120)) + "\0").encode("utf-8")
        longname.size = len(payload_name)
        longname.mtime = 0
        longname.mode = 0o644
        tf.addfile(longname, io.BytesIO(payload_name))
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_gzip_ustar(raw.getvalue())))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert not ok
    assert b"SFX split failed" not in report
    assert (
        b"extension/special" in report
        or b"GNU" in report
        or b"non-ustar tar header" in report
        or b"unexpected archive member type" in report
        or b"LongLink" in report
        or b"malformed or truncated ustar" in report
    )


def test_setuid_mode_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload)
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(
        _wrap_sfx(_custom_tgz(members, mode_override={"bin/scribe-app": 0o4755}))
    )
    _assert_stamp_ok_but_equivalence_fails(sfx_path, payload, b"unexpected tar mode")


def test_setgid_mode_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload)
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(
        _wrap_sfx(_custom_tgz(members, mode_override={"bin/scribe-lifecycle": 0o2755}))
    )
    _assert_stamp_ok_but_equivalence_fails(sfx_path, payload, b"unexpected tar mode")


def test_sticky_mode_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload)
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(
        _wrap_sfx(_custom_tgz(members, mode_override={"lib/install-runtime.sh": 0o1755}))
    )
    _assert_stamp_ok_but_equivalence_fails(sfx_path, payload, b"unexpected tar mode")


def test_unexpected_uid_gid_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload)
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_custom_tgz(members, uid=1, gid=0)))
    _assert_stamp_ok_but_equivalence_fails(sfx_path, payload, b"unexpected tar ownership")


def test_unexpected_mtime_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    members = _inventory_from_payload(payload)
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(_custom_tgz(members, mtime=1)))
    _assert_stamp_ok_but_equivalence_fails(sfx_path, payload, b"unexpected tar mtime")


@pytest.mark.skipif(not hasattr(__import__("os"), "symlink"), reason="no symlink support")
@pytest.mark.skipif(sys.platform == "win32", reason="symlink privileges vary on Windows")
def test_cli_symlink_sfx_refused(tmp_path: Path) -> None:
    import os

    from verify_sfx_payload_equivalence import main as eq_main

    payload = make_payload(tmp_path / "src")
    real_sfx = tmp_path / "Install Ignite.sh"
    real_sfx.write_bytes(build_sfx(payload, FIXTURE_VERSION, default_template()))
    profile_path = tmp_path / "package.v1"
    profile_path.write_bytes(_package_profile(payload))
    link = tmp_path / "linked-install.sh"
    os.symlink(real_sfx.name, link)
    rc = eq_main(
        [
            "--sfx",
            str(link),
            "--payload",
            str(payload),
            "--version",
            FIXTURE_VERSION,
            "--package-profile",
            str(profile_path),
        ]
    )
    assert rc == 1


def test_valid_production_sfx_with_normal_padding(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    archive = build_payload_tgz(payload, FIXTURE_VERSION)
    # Production gzip+ustar includes tar end blocks / zero padding; must still PASS.
    embedded = inspect_embedded_inventory(archive, FIXTURE_VERSION)
    assert embedded
    sfx_path = tmp_path / "Install Ignite.sh"
    sfx_path.write_bytes(_wrap_sfx(archive))
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert ok, report.decode("ascii")
    assert b"result=pass\n" in report


def test_ci_dist_copy_path_still_passes(tmp_path: Path) -> None:
    """Copied upload-candidate Scriptlet must pass the same gate as the builder output."""
    payload = make_payload(tmp_path / "src")
    blob = build_sfx(payload, FIXTURE_VERSION, default_template())
    dist = tmp_path / "dist" / "kindle-release"
    dist.mkdir(parents=True)
    sfx_path = dist / f"Install Ignite v{FIXTURE_VERSION}.sh"
    sfx_path.write_bytes(blob)
    ok, report = verify_equivalence(
        sfx_path=sfx_path,
        payload_path=payload,
        version=FIXTURE_VERSION,
        package_profile=_package_profile(payload),
    )
    assert ok, report.decode("ascii")
    assert sfx_path.read_bytes() == blob
