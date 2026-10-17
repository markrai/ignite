"""Tests for Ignite Kindle package validation (no physical Kindle required)."""

from __future__ import annotations

import hashlib
import struct
import sys
import zipfile
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_SFX = _HERE / "sfx"
for extra in (_HERE, _SFX):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from build_sfx import build_sfx, default_template  # noqa: E402
from validate_stage import (  # noqa: E402
    elf32_section_names,
    elf32_unwanted_sections,
    installer_sfx_name,
    validate_payload,
    validate_root,
    validate_sfx,
    validate_zip,
)

FIXTURE_VERSION = "0.9.8"

ANDROID_README = (
    "Ignite for Kindle\n"
    "Download Install Ignite on the Android phone.\n"
    "Copy it into the Kindle Documents folder.\n"
    "Open Install Ignite, then open Ignite.\n"
)


def _write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text.replace("\r\n", "\n"), encoding="utf-8", newline="\n")


def _write_bin(path: Path, size: int = 2048) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"\x7fELF" + b"\0" * (size - 4))


def _write_arm32le_elf(path: Path, size: int = 2048) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    blob = bytearray(size)
    blob[0:4] = b"\x7fELF"
    blob[4] = 1  # ELFCLASS32
    blob[5] = 1  # ELFDATA2LSB
    blob[18] = 0x28  # EM_ARM
    blob[19] = 0x00
    path.write_bytes(bytes(blob))


def _minimal_elf32_le(section_names: list[str]) -> bytes:
    """Tiny little-endian ELF32 with the given extra section names plus .shstrtab."""
    names = [""] + list(section_names) + [".shstrtab"]
    strtab = b""
    offsets: list[int] = []
    for name in names:
        offsets.append(len(strtab))
        strtab += name.encode("ascii") + b"\x00"
    ehdr_size = 52
    shentsize = 40
    shnum = len(names)
    shoff = ehdr_size
    strtab_off = shoff + shnum * shentsize
    ident = b"\x7fELF" + bytes([1, 1, 1, 0]) + bytes(8)
    ehdr = ident + struct.pack(
        "<HHIIIIIHHHHHH",
        3,  # ET_DYN
        40,  # EM_ARM
        1,
        0,
        0,
        shoff,
        0x5000400,
        52,
        0,
        0,
        shentsize,
        shnum,
        shnum - 1,
    )
    shdrs = []
    for i, name in enumerate(names):
        if name == ".shstrtab":
            sh_type, sh_offset, sh_size = 3, strtab_off, len(strtab)
        elif name == ".symtab":
            sh_type, sh_offset, sh_size = 2, 0, 0
        elif name == ".dynsym":
            sh_type, sh_offset, sh_size = 11, 0, 0
        else:
            sh_type, sh_offset, sh_size = 0, 0, 0
        shdrs.append(
            struct.pack(
                "<IIIIIIIIII",
                offsets[i],
                sh_type,
                0,
                0,
                sh_offset,
                sh_size,
                0,
                0,
                1,
                0,
            )
        )
    return ehdr + b"".join(shdrs) + strtab


def make_payload(parent: Path, version: str = FIXTURE_VERSION) -> Path:
    pkg = parent / f"Ignite-Kindle-{version}"
    _write_bin(pkg / "bin" / "scribe-app")
    _write_bin(pkg / "bin" / "scribe-lifecycle")
    _write(
        pkg / "host" / "scribe-lifecycle-host.sh",
        "#!/bin/sh\n"
        "exec /mnt/us/scribe-app/bin/scribe-lifecycle --host \\\n"
        "  --service com.markrai.ignite \\\n"
        "  --child /mnt/us/scribe-app/bin/scribe-app -- \\\n"
        "  --confirm-write --data-dir /mnt/us/scribe-data \\\n"
        "  --sync-dir /mnt/us/scribe-app/state/sync \\\n"
        "  --device-dir /mnt/us/scribe-app/state/device\n",
    )
    _write(pkg / "launcher" / "Scribe.sh", "#!/bin/sh\n# Name: Ignite\n")
    _write(pkg / "Uninstall-Ignite.sh", "#!/bin/sh\n# Name: Uninstall Ignite\nok\n")
    _write(pkg / "Backup-Ignite-Data.sh", "#!/bin/sh\n# Name: Backup Ignite Data\nok\n")
    _write(pkg / "Diagnose-Ignite.sh", "#!/bin/sh\n# Name: Diagnose Ignite\nok\n")
    _write(
        pkg / "registration" / "ignite.json",
        '{ "def": "interface", "assoc": { "interface": "application" } },\n'
        "{\n"
        '  "def": "handlerId",\n'
        "  \"assoc\": {\n"
        '    "handlerId": "com.markrai.ignite",\n'
        '    "props": {\n'
        '      "lipcId": "com.markrai.ignite",\n'
        '      "command": "/mnt/us/scribe-app/bin/scribe-lifecycle-host.sh"\n'
        "    }\n"
        "  }\n"
        "}\n",
    )
    for name in (
        "assets/licenses/noto-emoji/OFL.txt",
        "assets/licenses/source-sans-3/OFL.txt",
        "lib/install-runtime.sh",
        "lib/backup-runtime.sh",
        "lib/register-helpers.sh",
    ):
        _write(pkg / name, f"# {name}\nok\n")
    _write(
        pkg / "lib" / "uninstall-runtime.sh",
        "# lib/uninstall-runtime.sh\n"
        f'rm -f "/mnt/us/documents/{installer_sfx_name(version)}"\n'
        "ok\n",
    )
    _write(pkg / "lib" / "install-recovery.sh", "# lib/install-recovery.sh\nok\n")
    rec = pkg / "recovery" / "bin" / "dropbearmulti"
    _write_arm32le_elf(rec)
    rec_hash = hashlib.sha256(rec.read_bytes()).hexdigest()
    _write(pkg / "recovery" / "manifest" / "elf.sha256", rec_hash + "\n")
    _write(
        pkg / "recovery" / "manifest" / "provenance.txt",
        "name=dropbearmulti\nrole=test-stub\nupstream_version=2024.85\n",
    )
    _write_bin(pkg / "assets" / "scribe-icon.png", size=1280)
    return pkg


def write_sfx(payload: Path, dest: Path, version: str = FIXTURE_VERSION) -> Path:
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(build_sfx(payload, version, default_template()))
    return dest


def make_valid_tree(root: Path, version: str = FIXTURE_VERSION) -> Path:
    payload = make_payload(root / "payload-src", version)
    dist = root / f"Ignite Kindle {version}"
    dist.mkdir(parents=True, exist_ok=True)
    _write(dist / "README.txt", ANDROID_README)
    write_sfx(payload, dist / installer_sfx_name(version), version)
    return dist


def test_valid_tree_passes(tmp_path: Path) -> None:
    dist = make_valid_tree(tmp_path)
    assert validate_root(dist, FIXTURE_VERSION) == []


def test_valid_payload_passes(tmp_path: Path) -> None:
    payload = make_payload(tmp_path)
    errors = validate_payload(payload, FIXTURE_VERSION)
    assert errors == []


def test_rejects_host_missing_sync(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    host = payload / "host" / "scribe-lifecycle-host.sh"
    # Strip sync capability — full-feature package must retain it.
    lines = []
    for line in host.read_text(encoding="utf-8").splitlines():
        if "--sync-dir" in line or "--device-dir" in line:
            continue
        lines.append(line)
    host.write_text("\n".join(lines) + "\n", encoding="utf-8")
    dist = tmp_path / f"Ignite Kindle {FIXTURE_VERSION}"
    dist.mkdir()
    _write(dist / "README.txt", ANDROID_README)
    write_sfx(payload, dist / installer_sfx_name(FIXTURE_VERSION))
    errors = validate_root(dist, FIXTURE_VERSION)
    assert any("missing --sync-dir" in e or "missing --device-dir" in e for e in errors)


def test_rejects_user_data_dir(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    _write(payload / "scribe-data" / "notebook.meta", "{}")
    dist = tmp_path / f"Ignite Kindle {FIXTURE_VERSION}"
    dist.mkdir()
    _write(dist / "README.txt", ANDROID_README)
    write_sfx(payload, dist / installer_sfx_name(FIXTURE_VERSION))
    errors = validate_root(dist, FIXTURE_VERSION)
    assert any("scribe-data" in e for e in errors)


def test_rejects_mdns_hostname_contamination(tmp_path: Path) -> None:
    dist = make_valid_tree(tmp_path)
    readme = dist / "README.txt"
    readme.write_text(
        readme.read_text(encoding="utf-8") + "\nhttp://build-host.local\n",
        encoding="utf-8",
    )
    errors = validate_root(dist, FIXTURE_VERSION)
    assert any("mDNS .local hostname" in e for e in errors)


def test_rejects_rfc1918_ip_contamination(tmp_path: Path) -> None:
    dist = make_valid_tree(tmp_path)
    readme = dist / "README.txt"
    # RFC 5737 TEST-NET-1 is documentation-only; also exercise RFC1918 detection.
    readme.write_text(
        readme.read_text(encoding="utf-8") + "\nconnect 192.168.200.10\n",
        encoding="utf-8",
    )
    errors = validate_root(dist, FIXTURE_VERSION)
    assert any("RFC1918 private IPv4" in e for e in errors)


def test_rejects_ssh_key_basename_contamination(tmp_path: Path) -> None:
    dist = make_valid_tree(tmp_path)
    readme = dist / "README.txt"
    readme.write_text(
        readme.read_text(encoding="utf-8") + "\nkey path id_ed25519\n",
        encoding="utf-8",
    )
    errors = validate_root(dist, FIXTURE_VERSION)
    assert any("SSH private-key basename" in e for e in errors)


def test_rejects_windows_user_path(tmp_path: Path) -> None:
    dist = make_valid_tree(tmp_path)
    readme = dist / "README.txt"
    readme.write_text(
        readme.read_text(encoding="utf-8") + "\nC:\\Users\\example\\secret\n",
        encoding="utf-8",
    )
    errors = validate_root(dist, FIXTURE_VERSION)
    assert any("Windows user path" in e for e in errors)


def test_rejects_missing_source_sans_license(tmp_path: Path) -> None:
    payload = make_payload(tmp_path)
    (payload / "assets" / "licenses" / "source-sans-3" / "OFL.txt").unlink()
    errors = validate_payload(payload, FIXTURE_VERSION)
    assert any("source-sans-3/OFL.txt" in e for e in errors)


def test_rejects_private_key_material(tmp_path: Path) -> None:
    dist = make_valid_tree(tmp_path)
    _write(dist / "notes.txt", "-----BEGIN OPENSSH PRIVATE KEY-----\nxxx\n")
    errors = validate_root(dist, FIXTURE_VERSION)
    assert any("OpenSSH private key" in e for e in errors)


def test_user_readme_rejects_shell_filename(tmp_path: Path) -> None:
    dist = make_valid_tree(tmp_path)
    readme = dist / "README.txt"
    readme.write_text(readme.read_text(encoding="utf-8") + "\ncopy Install-Ignite.sh\n", encoding="utf-8")
    errors = validate_root(dist, FIXTURE_VERSION)
    assert any(".sh filename" in e for e in errors)


def test_user_readme_rejects_mnt_us(tmp_path: Path) -> None:
    dist = make_valid_tree(tmp_path)
    readme = dist / "README.txt"
    readme.write_text(readme.read_text(encoding="utf-8") + "\n/mnt/us/scribe-data\n", encoding="utf-8")
    errors = validate_root(dist, FIXTURE_VERSION)
    assert any("/mnt/us" in e for e in errors)


def test_zip_validation_roundtrip(tmp_path: Path) -> None:
    dist = make_valid_tree(tmp_path)
    zip_path = tmp_path / f"Ignite-Kindle-{FIXTURE_VERSION}.zip"
    with zipfile.ZipFile(zip_path, "w") as zf:
        for f in dist.rglob("*"):
            if f.is_file():
                zf.write(f, f.relative_to(tmp_path).as_posix())
    assert validate_zip(zip_path, FIXTURE_VERSION) == []


def test_zip_rejects_missing_member(tmp_path: Path) -> None:
    dist = make_valid_tree(tmp_path)
    (dist / "README.txt").unlink()
    zip_path = tmp_path / f"Ignite-Kindle-{FIXTURE_VERSION}.zip"
    with zipfile.ZipFile(zip_path, "w") as zf:
        for f in dist.rglob("*"):
            if f.is_file():
                zf.write(f, f.relative_to(tmp_path).as_posix())
    errors = validate_zip(zip_path, FIXTURE_VERSION)
    assert any("README.txt" in e for e in errors)


def test_real_deployment_host_is_full_feature() -> None:
    host = Path(__file__).resolve().parent / "scribe-lifecycle-host.sh"
    active = "\n".join(
        line
        for line in host.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    )
    assert "--confirm-write" in active
    assert "/mnt/us/scribe-data" in active
    assert "--sync-dir" in active
    assert "/mnt/us/scribe-app/state/sync" in active
    assert "--device-dir" in active
    assert "/mnt/us/scribe-app/state/device" in active


def test_real_external_host_fails_closed_without_su() -> None:
    host = Path(__file__).resolve().parent / "scribe-lifecycle-host.sh"
    text = host.read_text(encoding="utf-8")
    assert "su_missing" in text
    assert "host_abort reason=su_exec_returned" in text


def test_rejects_developer_path_in_binary(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    app = payload / "bin" / "scribe-app"
    app.write_bytes(app.read_bytes() + b"C:\\dev\\project\\scribe\\native\\src\x00")
    errors = validate_sfx(write_sfx(payload, tmp_path / "Install Ignite.sh"), FIXTURE_VERSION)
    assert any("C:\\dev\\project\\scribe" in e or "binary contamination" in e for e in errors)


def test_elf_symtab_fixture_is_rejected() -> None:
    blob = _minimal_elf32_le([".dynsym", ".symtab"])
    names = elf32_section_names(blob)
    assert names is not None
    assert ".dynsym" in names
    assert ".symtab" in names
    unwanted = elf32_unwanted_sections(names)
    assert ".symtab" in unwanted
    assert ".dynsym" not in unwanted


def test_elf_dynsym_only_is_allowed() -> None:
    blob = _minimal_elf32_le([".dynsym"])
    names = elf32_section_names(blob)
    assert names is not None
    assert ".dynsym" in names
    assert ".symtab" not in names
    assert elf32_unwanted_sections(names) == []


def test_payload_rejects_unstripped_symtab_elf(tmp_path: Path) -> None:
    payload = make_payload(tmp_path)
    blob = _minimal_elf32_le([".dynsym", ".symtab"]).ljust(2048, b"\0")
    (payload / "bin" / "scribe-app").write_bytes(blob)
    errors = validate_payload(payload, FIXTURE_VERSION)
    assert any(".symtab" in e for e in errors)


def test_sfx_rejects_unstripped_symtab_elf(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    blob = _minimal_elf32_le([".dynsym", ".symtab"]).ljust(2048, b"\0")
    (payload / "bin" / "scribe-app").write_bytes(blob)
    errors = validate_sfx(write_sfx(payload, tmp_path / "Install Ignite.sh"), FIXTURE_VERSION)
    assert any(".symtab" in e for e in errors)


def test_rejects_upstream_info_md(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    _write(payload / "assets" / "licenses" / "noto-emoji" / "upstream_info.md", "internal investigation\n")
    errors = validate_sfx(write_sfx(payload, tmp_path / "Install Ignite.sh"), FIXTURE_VERSION)
    assert any("upstream_info.md" in e for e in errors)


def test_real_user_readme_has_no_shell_or_paths() -> None:
    text = (Path(__file__).resolve().parent / "README.txt").read_text(encoding="utf-8")
    assert ".sh" not in text
    assert "/mnt/us" not in text
    assert "Scriptlet" not in text
    assert "appreg" not in text
    assert "scribe-app" not in text
    assert "--sync-dir" not in text
    assert "PowerShell" not in text
    assert "Windows PC" not in text
    assert "Android phone is required" not in text
    assert "a phone or computer" in text


def test_install_runtime_does_not_copy_installer() -> None:
    text = (_HERE / "lib" / "install-runtime.sh").read_text(encoding="utf-8")
    assert "Install Ignite.sh" not in text
    assert "Install-Ignite.sh" not in text


def test_installer_sfx_name_includes_version() -> None:
    assert installer_sfx_name("0.9.10") == "Install Ignite v0.9.10.sh"


def test_real_uninstall_removes_versioned_installer_filename() -> None:
    text = (_HERE / "lib" / "uninstall-runtime.sh").read_text(encoding="utf-8")
    assert "Install Ignite v@@IGNITE_PACKAGE_VERSION@@.sh" in text


def test_payload_rejects_unstamped_uninstall(tmp_path: Path) -> None:
    payload = make_payload(tmp_path)
    helper = payload / "lib" / "uninstall-runtime.sh"
    helper.write_text(
        helper.read_text(encoding="utf-8").replace(
            installer_sfx_name(FIXTURE_VERSION),
            "@@IGNITE_PACKAGE_VERSION@@",
        ),
        encoding="utf-8",
    )
    errors = validate_payload(payload, FIXTURE_VERSION)
    assert any("unstamped" in e or installer_sfx_name(FIXTURE_VERSION) in e for e in errors)
