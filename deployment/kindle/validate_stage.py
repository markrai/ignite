#!/usr/bin/env python3
"""Validate an Ignite Kindle release (payload, SFX, support folder, or ZIP).

Usage:
  validate_stage.py --payload <Ignite-Kindle-VERSION> --version VERSION
  validate_stage.py --sfx <Install Ignite vVERSION.sh> --version VERSION
  validate_stage.py --root <Ignite Kindle VERSION> --version VERSION
  validate_stage.py --zip <Ignite-Kindle-VERSION.zip> --version VERSION
"""

from __future__ import annotations

import argparse
import hashlib
import re
import struct
import sys
import tempfile
import zipfile
from pathlib import Path

PAYLOAD_REQUIRED_RELATIVE = [
    "Uninstall-Ignite.sh",
    "Backup-Ignite-Data.sh",
    "Diagnose-Ignite.sh",
    "bin/scribe-app",
    "bin/scribe-lifecycle",
    "host/scribe-lifecycle-host.sh",
    "launcher/Scribe.sh",
    "registration/ignite.json",
    "assets/scribe-icon.png",
    "assets/licenses/noto-emoji/OFL.txt",
    "assets/licenses/source-sans-3/OFL.txt",
    "lib/install-runtime.sh",
    "lib/uninstall-runtime.sh",
    "lib/backup-runtime.sh",
    "lib/register-helpers.sh",
    "lib/install-recovery.sh",
    "recovery/bin/dropbearmulti",
    "recovery/manifest/elf.sha256",
    "recovery/manifest/provenance.txt",
]

FORBIDDEN_FILENAMES = {
    "replica.json",
    "device.json",
    "identity.json",
    "config.json",
    "upstream_info.md",
}

FORBIDDEN_PATH_PARTS = {
    "scribe-data",
    ".git",
    "node_modules",
}

USER_README_FORBIDDEN = [
    (re.compile(r"\.sh\b"), ".sh filename"),
    (re.compile(r"/mnt/us"), "/mnt/us"),
    (re.compile(r"Scriptlet", re.I), "Scriptlet"),
    (re.compile(r"appreg", re.I), "appreg"),
    (re.compile(r"scribe-app"), "scribe-app"),
    (re.compile(r"--sync-dir"), "--sync-dir"),
    (re.compile(r"--device-dir"), "--device-dir"),
    (re.compile(r"\bSSH\b"), "SSH"),
    (re.compile(r"\bterminal\b", re.I), "terminal"),
]

LIBRARY_NAME_HEADERS = {
    "launcher/Scribe.sh": "Ignite",
    "Uninstall-Ignite.sh": "Uninstall Ignite",
    "Backup-Ignite-Data.sh": "Backup Ignite Data",
    "Diagnose-Ignite.sh": "Diagnose Ignite",
}

TEXT_SUFFIXES = {
    ".sh",
    ".txt",
    ".md",
    ".json",
    ".html",
    ".py",
    ".bat",
    ".ps1",
    ".service",
    ".conf",
    ".cfg",
    ".env",
    ".yml",
    ".yaml",
    ".toml",
    ".csv",
}

# Generalized contamination detectors. Fixtures must use synthetic examples only
# (RFC 5737 TEST-NET, example/*.invalid hostnames, etc.) — never real infra.
CONTAMINATION_PATTERNS: list[tuple[re.Pattern[str], str]] = [
    (
        re.compile(
            r"(?<!\d)(?:10(?:\.\d{1,3}){3}|172\.(?:1[6-9]|2\d|3[0-1])(?:\.\d{1,3}){2}|192\.168(?:\.\d{1,3}){2})(?!\d)"
        ),
        "RFC1918 private IPv4",
    ),
    (re.compile(r"\b[A-Za-z0-9][A-Za-z0-9._-]*\.local\b", re.I), "mDNS .local hostname"),
    (re.compile(r"\.ts\.net\b", re.I), "Tailscale MagicDNS hostname"),
    (
        re.compile(r"(?:\bid_|[A-Za-z0-9._-]+_)(?:ed25519|rsa)\b", re.I),
        "SSH private-key basename",
    ),
    (re.compile(r"C:\\Users\\", re.I), "Windows user path"),
    (re.compile(r"[A-Za-z]:\\dev\\project\\", re.I), "repo-local Windows path"),
    (re.compile(r"/home/[A-Za-z0-9._-]+"), "Unix home path"),
    (re.compile(r"BEGIN OPENSSH PRIVATE KEY"), "OpenSSH private key"),
    (re.compile(r"BEGIN PRIVATE KEY"), "PEM private key"),
    (re.compile(r"BEGIN RSA PRIVATE KEY"), "RSA private key"),
    (
        re.compile(
            r"""(["']?(?:token|password|secret|api[_-]?key|authorization)["']?\s*[:=]\s*["'][^"']{8,}["'])""",
            re.I,
        ),
        "credential-like field",
    ),
    (
        re.compile(
            r"https?://(?:127\.0\.0\.1|localhost)(?::\d+)?/(?:v1|control)/",
            re.I,
        ),
        "developer localhost API URL",
    ),
]

# Full-feature open-source host must expose sync/pairing paths (optional to use).
HOST_REQUIRED_TOKENS = (
    "--confirm-write",
    "/mnt/us/scribe-data",
    "--sync-dir",
    "/mnt/us/scribe-app/state/sync",
    "--device-dir",
    "/mnt/us/scribe-app/state/device",
)

# Byte needles for packaged ELF/runtime files (ASCII substrings; class-based only).
BINARY_FORBIDDEN_SUBSTRINGS = [
    b"C:\\dev\\project\\",
    b"C:/dev/project/",
    b".ts.net",
    b"BEGIN OPENSSH PRIVATE KEY",
    b"BEGIN PRIVATE KEY",
]


def dist_name(version: str) -> str:
    return f"Ignite Kindle {version}"


def payload_name(version: str) -> str:
    return f"Ignite-Kindle-{version}"


def installer_sfx_name(version: str) -> str:
    """Customer installer filename, e.g. 'Install Ignite v0.9.10.sh'."""
    return f"Install Ignite v{version}.sh"


def is_customer_installer_name(name: str) -> bool:
    if name in {"Install Ignite.sh", "Install-Ignite.sh"}:
        return True
    return bool(re.fullmatch(r"Install Ignite v.+\.sh", name))


def dist_required_relative(version: str) -> list[str]:
    return ["README.txt", installer_sfx_name(version)]


def elf32_section_names(blob: bytes) -> list[str] | None:
    """Return section names for a little-endian ELF32, or None if not parseable."""
    if len(blob) < 52 or blob[:4] != b"\x7fELF":
        return None
    if blob[4] != 1 or blob[5] != 1:
        return None
    e_shoff = struct.unpack_from("<I", blob, 32)[0]
    e_shentsize = struct.unpack_from("<H", blob, 46)[0]
    e_shnum = struct.unpack_from("<H", blob, 48)[0]
    e_shstrndx = struct.unpack_from("<H", blob, 50)[0]
    if e_shentsize < 40 or e_shnum == 0 or e_shstrndx >= e_shnum:
        return None
    sh_off = e_shoff + e_shstrndx * e_shentsize
    if sh_off + 16 > len(blob):
        return None
    strtab_off, strtab_size = struct.unpack_from("<II", blob, sh_off + 16)
    if strtab_off + strtab_size > len(blob):
        return None
    strtab = blob[strtab_off : strtab_off + strtab_size]
    names: list[str] = []
    for i in range(e_shnum):
        ent = e_shoff + i * e_shentsize
        if ent + 4 > len(blob):
            return None
        name_off = struct.unpack_from("<I", blob, ent)[0]
        if name_off >= len(strtab):
            continue
        end = strtab.find(b"\x00", name_off)
        raw = strtab[name_off:end] if end >= 0 else strtab[name_off:]
        names.append(raw.decode("latin-1", errors="replace"))
    return names


def elf32_unwanted_sections(names: list[str]) -> list[str]:
    """DWARF/debug sections and a full symbol table must not ship. .dynsym is allowed."""
    bad: list[str] = []
    for name in names:
        if name.startswith(".debug") or name.startswith(".zdebug"):
            bad.append(name)
        elif name == ".symtab":
            bad.append(name)
    return bad


def _is_text_path(path: Path) -> bool:
    if path.suffix.lower() in TEXT_SUFFIXES:
        return True
    return path.name.lower() in {"readme", "license", "licence"}


def _iter_files(root: Path):
    for p in sorted(root.rglob("*")):
        if p.is_file():
            yield p


def _scan_contamination(root: Path, errors: list[str]) -> None:
    for path in _iter_files(root):
        if is_customer_installer_name(path.name):
            continue
        rel = path.relative_to(root).as_posix()
        parts = set(path.relative_to(root).parts)
        if parts & FORBIDDEN_PATH_PARTS:
            errors.append(f"forbidden path in package: {rel}")
        if path.name in FORBIDDEN_FILENAMES:
            errors.append(f"forbidden state/config file: {rel}")
        if path.suffix.lower() in {".pem", ".key"} or path.name.endswith("_ed25519"):
            errors.append(f"forbidden key-like file: {rel}")
        if path.name == "Cargo.toml" or path.suffix == ".rs":
            errors.append(f"source file must not be packaged: {rel}")
        if path.suffix.lower() == ".pdb":
            errors.append(f"debug artifact must not be packaged: {rel}")
        if "target" in parts and "debug" in parts:
            errors.append(f"debug build path: {rel}")
        if path.name.endswith(".pdb") or path.name.endswith(".dSYM"):
            errors.append(f"debug companion must not be packaged: {rel}")

    for path in _iter_files(root):
        if is_customer_installer_name(path.name):
            continue
        if path.stat().st_size > 32_000_000:
            continue
        try:
            blob = path.read_bytes()
        except OSError:
            continue
        rel = path.relative_to(root).as_posix()
        for needle in BINARY_FORBIDDEN_SUBSTRINGS:
            if needle in blob:
                errors.append(
                    f"{rel}: binary contamination ({needle.decode('latin-1', errors='replace')})"
                )

    for path in _iter_files(root):
        if is_customer_installer_name(path.name):
            continue
        if not _is_text_path(path):
            continue
        if path.stat().st_size > 2_000_000:
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError as exc:
            errors.append(f"cannot read {path.relative_to(root)}: {exc}")
            continue
        rel = path.relative_to(root).as_posix()
        for pattern, label in CONTAMINATION_PATTERNS:
            if pattern.search(text):
                errors.append(f"{rel}: contamination ({label})")


def validate_payload(payload: Path, version: str) -> list[str]:
    errors: list[str] = []
    expected = payload_name(version)
    if payload.name != expected:
        errors.append(f"payload directory name is {payload.name!r}, expected {expected!r}")

    for rel in PAYLOAD_REQUIRED_RELATIVE:
        path = payload / rel
        if not path.is_file():
            errors.append(f"missing required payload file: {rel}")
        elif path.stat().st_size == 0:
            errors.append(f"required payload file is empty: {rel}")

    reg = payload / "registration" / "ignite.json"
    if reg.is_file():
        rtext = reg.read_text(encoding="utf-8", errors="replace")
        if '"handlerId": "com.markrai.ignite"' not in rtext and '"handlerId":"com.markrai.ignite"' not in rtext:
            errors.append("registration/ignite.json must declare handlerId com.markrai.ignite")
        if '"handlerId": "com.markrai.scribe"' in rtext or '"handlerId":"com.markrai.scribe"' in rtext:
            errors.append("registration/ignite.json must not declare legacy handlerId com.markrai.scribe")

    uninstall = payload / "lib" / "uninstall-runtime.sh"
    if uninstall.is_file():
        utext = uninstall.read_text(encoding="utf-8", errors="replace")
        expected_installer = installer_sfx_name(version)
        if expected_installer not in utext:
            errors.append(
                f"uninstall-runtime.sh must remove {expected_installer!r}"
            )
        if "@@IGNITE_PACKAGE_VERSION@@" in utext:
            errors.append("uninstall-runtime.sh still contains an unstamped placeholder")

    rec_elf = payload / "recovery" / "bin" / "dropbearmulti"
    rec_sum = payload / "recovery" / "manifest" / "elf.sha256"
    if rec_elf.is_file() and rec_sum.is_file():
        have = hashlib.sha256(rec_elf.read_bytes()).hexdigest()
        want = rec_sum.read_text(encoding="utf-8", errors="replace").split()[0].strip()
        if have != want:
            errors.append("recovery/manifest/elf.sha256 does not match recovery/bin/dropbearmulti")

    for rel in ("bin/scribe-app", "bin/scribe-lifecycle", "recovery/bin/dropbearmulti"):
        path = payload / rel
        if path.is_file() and path.stat().st_size < 1024:
            errors.append(f"{rel} suspiciously small ({path.stat().st_size} bytes)")
        if path.is_file():
            blob = path.read_bytes()
            if blob[:4] != b"\x7fELF":
                errors.append(f"{rel} is not an ELF binary")
            else:
                sections = elf32_section_names(blob)
                if sections:
                    bad_secs = elf32_unwanted_sections(sections)
                    debug_secs = [n for n in bad_secs if n.startswith(".debug") or n.startswith(".zdebug")]
                    if debug_secs:
                        errors.append(f"{rel} ships DWARF/debug sections: {debug_secs[:8]}")
                    if ".symtab" in bad_secs:
                        errors.append(f"{rel} still contains .symtab (release binaries must be stripped)")
        pdb = payload / f"{rel}.pdb"
        if pdb.is_file():
            errors.append(f"debug companion must not be packaged: {rel}.pdb")

    host = payload / "host" / "scribe-lifecycle-host.sh"
    if host.is_file():
        text = host.read_text(encoding="utf-8", errors="replace")
        active = "\n".join(
            line for line in text.splitlines() if line.strip() and not line.lstrip().startswith("#")
        )
        for token in HOST_REQUIRED_TOKENS:
            if token not in active:
                errors.append(f"full-feature host missing {token}")

    for rel, name in LIBRARY_NAME_HEADERS.items():
        path = payload / rel
        if path.is_file():
            first_lines = path.read_text(encoding="utf-8", errors="replace").splitlines()[:12]
            header = f"# Name: {name}"
            if header not in first_lines:
                errors.append(f"{rel} missing Library name header {header!r}")

    return errors


def validate_sfx(sfx_path: Path, version: str) -> list[str]:
    """Validate the customer-facing self-extracting Install Ignite vVERSION.sh."""
    errors: list[str] = []
    if not sfx_path.is_file():
        return [f"SFX not found: {sfx_path}"]
    if sfx_path.stat().st_size < 2048:
        errors.append("SFX is suspiciously small")
    blob = sfx_path.read_bytes()

    sfx_dir = str(Path(__file__).resolve().parent / "sfx")
    if sfx_dir not in sys.path:
        sys.path.insert(0, sfx_dir)
    from build_sfx import extract_payload, split_sfx  # noqa: WPS433

    try:
        prefix, archive, meta = split_sfx(blob)
    except ValueError as exc:
        return [f"SFX split failed: {exc}"]

    # Only the shell prefix must be LF-only; gzip payload bytes may coincidentally
    # contain 0x0d 0x0a and must not be scanned as text.
    if b"\r" in prefix:
        errors.append("SFX prefix contains CRLF")

    if meta.get("version") != version:
        errors.append(f"SFX version is {meta.get('version')!r}, expected {version!r}")
    text = prefix.decode("utf-8", errors="replace")
    first = text.splitlines()[:12]
    if "# Name: Install Ignite" not in first:
        errors.append("SFX missing Library name header '# Name: Install Ignite'")
    if installer_sfx_name(version) not in text:
        errors.append(
            f"SFX must name/remove the versioned installer {installer_sfx_name(version)!r}"
        )
    if "@@IGNITE_" in text:
        errors.append("SFX still contains an unstamped placeholder")
    if "ignite-extract-${IGNITE_PACKAGE_VERSION}-$$" not in text:
        errors.append("SFX extraction stage must be process-unique before lock acquisition")
    if "IGNITE_ARCHIVE_OFFSET=0000000000" in text:
        errors.append("SFX OFFSET was not stamped")
    if "$((IGNITE_ARCHIVE_OFFSET" in text or "$((IGNITE_ARCHIVE_BYTES" in text:
        errors.append("SFX must not use zero-padded OFFSET/BYTES in $((...)) (BusyBox octal)")
    extract_at = text.find('tar -xzf "$ARCHIVE"')
    diagnose_at = text.find("IGNITE_INSTALL_DIAGNOSE_EARLY=1")
    runtime_at = text.find('sh "$PACKAGE_ROOT/lib/install-runtime.sh"')
    if extract_at < 0:
        errors.append("SFX missing payload extract")
    if diagnose_at < 0:
        errors.append("SFX missing locked Diagnose Ignite publication request")
    elif extract_at >= 0 and diagnose_at < extract_at:
        errors.append("Diagnose Ignite must be published only after successful payload extract")
    if runtime_at < 0:
        errors.append("SFX missing install-runtime invocation")
    elif diagnose_at >= 0 and runtime_at < diagnose_at:
        errors.append("Diagnose Ignite publication request must happen before install-runtime")
    if "write_usb_fail_report" not in text or 'Ignite-Diagnose.txt' not in text:
        errors.append("SFX must write USB Ignite-Diagnose.txt on install failure")
    if "install-runtime publishes Diagnose after it has acquired" not in text:
        errors.append("SFX must state Diagnose Ignite publication is transaction-locked")
    for pattern, label in CONTAMINATION_PATTERNS:
        if pattern.search(text):
            errors.append(f"SFX prefix: contamination ({label})")

    with tempfile.TemporaryDirectory(prefix="ignite-sfx-val-") as tmp:
        try:
            payload = extract_payload(archive, Path(tmp) / "root")
        except Exception as exc:  # noqa: BLE001
            errors.append(f"SFX extract failed: {exc}")
            return errors
        if payload.name != payload_name(version):
            errors.append(f"embedded payload root is {payload.name!r}")
        errors.extend(validate_payload(payload, version))
        _scan_contamination(payload, errors)
        if (payload / "Install-Ignite.sh").is_file():
            errors.append("embedded payload must not include a second installer")
    return errors


def validate_root(root: Path, version: str) -> list[str]:
    """Validate a support folder that contains README + Install Ignite vVERSION.sh."""
    errors: list[str] = []
    expected = dist_name(version)
    if root.name != expected:
        errors.append(f"root directory name is {root.name!r}, expected {expected!r}")

    for rel in dist_required_relative(version):
        path = root / rel
        if not path.is_file():
            errors.append(f"missing required file: {rel}")
        elif path.stat().st_size == 0:
            errors.append(f"required file is empty: {rel}")

    readme = root / "README.txt"
    if readme.is_file():
        text = readme.read_text(encoding="utf-8", errors="replace")
        for pattern, label in USER_README_FORBIDDEN:
            if pattern.search(text):
                errors.append(f"README.txt must not mention {label} (user-facing)")

    sfx = root / installer_sfx_name(version)
    if sfx.is_file():
        errors.extend(validate_sfx(sfx, version))

    # Support folder should not ship a payload tree or PC tooling at the top level.
    if (root / "payload").exists():
        errors.append("support folder must not contain payload/")
    _scan_contamination(root, errors)
    return errors


def validate_zip(zip_path: Path, version: str) -> list[str]:
    errors: list[str] = []
    expected_root = dist_name(version)
    expected_prefix = expected_root + "/"
    if not zip_path.is_file():
        return [f"zip not found: {zip_path}"]
    if zip_path.stat().st_size < 1024:
        errors.append("zip is suspiciously small")

    with zipfile.ZipFile(zip_path, "r") as zf:
        names = [n.replace("\\", "/") for n in zf.namelist()]
        if not names:
            return ["zip is empty"]

        roots = set()
        for n in names:
            cleaned = n.strip("/")
            if cleaned:
                roots.add(cleaned.split("/")[0])
        if roots != {expected_root}:
            errors.append(f"zip root(s) {sorted(roots)!r}, expected exact {expected_root!r}")

        for rel in dist_required_relative(version):
            member = expected_prefix + rel
            if member not in names:
                errors.append(f"zip missing member: {member}")

        for name in names:
            parts = [p for p in name.split("/") if p]
            if any(p in FORBIDDEN_PATH_PARTS for p in parts):
                errors.append(f"zip contains forbidden path: {name}")
            base = parts[-1] if parts else ""
            if base in FORBIDDEN_FILENAMES:
                errors.append(f"zip contains forbidden file: {name}")

        with tempfile.TemporaryDirectory(prefix="ignite-zip-val-") as tmp:
            zf.extractall(tmp)
            root = Path(tmp) / expected_root
            if not root.is_dir():
                errors.append("extracted zip root directory missing")
                return errors
            errors.extend(validate_root(root, version))

    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, help="support folder (README + Install Ignite vVERSION.sh)")
    parser.add_argument("--payload", type=Path, help="staged Ignite-Kindle-<version> payload directory")
    parser.add_argument("--sfx", type=Path, help="customer Install Ignite vVERSION.sh")
    parser.add_argument("--zip", type=Path, dest="zip_path", help="developer/support ZIP")
    parser.add_argument("--version", required=True, help="expected version string")
    args = parser.parse_args(argv)

    modes = [args.root, args.payload, args.sfx, args.zip_path]
    if sum(x is not None for x in modes) != 1:
        parser.error("exactly one of --root, --payload, --sfx, or --zip is required")

    if args.payload:
        errors = validate_payload(args.payload.resolve(), args.version)
        _scan_contamination(args.payload.resolve(), errors)
    elif args.sfx:
        errors = validate_sfx(args.sfx.resolve(), args.version)
    elif args.root:
        errors = validate_root(args.root.resolve(), args.version)
    else:
        errors = validate_zip(args.zip_path.resolve(), args.version)

    if errors:
        print("VALIDATION FAILED:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        return 1
    print("validation_ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
