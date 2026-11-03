"""Release version synchronization helpers."""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path

from .errors import ShipError

_VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$")


def validate_version_string(version: str) -> None:
    if not _VERSION_RE.fullmatch(version):
        raise ShipError(f"malformed release version: {version!r}")


def tag_for_version(version: str) -> str:
    validate_version_string(version)
    return f"v{version}"


def read_repo_version(repo_root: Path) -> str:
    raw = (repo_root / "ignite" / "VERSION").read_text(encoding="utf-8").strip()
    validate_version_string(raw)
    return raw


def apply_version(repo_root: Path, version: str) -> list[str]:
    """Write version across VERSION / Cargo workspace / web package files.

    Returns relative paths modified. Does not create a Git commit.
    """
    validate_version_string(version)
    changed: list[str] = []

    version_path = repo_root / "ignite" / "VERSION"
    if version_path.read_text(encoding="utf-8").strip() != version:
        version_path.write_text(version + "\n", encoding="utf-8")
        changed.append("ignite/VERSION")

    cargo_toml = repo_root / "Cargo.toml"
    text = cargo_toml.read_text(encoding="utf-8")
    marker = "[workspace.package]"
    if marker not in text:
        raise ShipError("Cargo.toml missing [workspace.package]")
    head, rest = text.split(marker, 1)
    # Replace only the first version = "..." inside workspace.package section
    # (until the next [section]).
    section, sep, tail = rest.partition("\n[")
    new_section, count = re.subn(
        r'(?m)^(version\s*=\s*")([^"]+)(")\s*$',
        rf'\g<1>{version}\g<3>',
        section,
        count=1,
    )
    if count != 1:
        raise ShipError("Cargo.toml workspace.package.version not found")
    new_text = head + marker + new_section + (sep + tail if sep else "")
    if new_text != text:
        cargo_toml.write_text(new_text, encoding="utf-8")
        changed.append("Cargo.toml")

    web = repo_root / "ignite" / "web" / "package.json"
    if web.is_file():
        data = json.loads(web.read_text(encoding="utf-8"))
        if data.get("version") != version:
            data["version"] = version
            web.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
            changed.append("ignite/web/package.json")

    lock = repo_root / "ignite" / "web" / "package-lock.json"
    if lock.is_file():
        lock_data = json.loads(lock.read_text(encoding="utf-8"))
        lock_changed = False
        if lock_data.get("version") != version:
            lock_data["version"] = version
            lock_changed = True
        packages = lock_data.setdefault("packages", {})
        root_pkg = packages.setdefault("", {})
        if root_pkg.get("version") != version:
            root_pkg["version"] = version
            lock_changed = True
        if lock_changed:
            lock.write_text(json.dumps(lock_data, indent=2) + "\n", encoding="utf-8")
            changed.append("ignite/web/package-lock.json")

    if (repo_root / "Cargo.lock").is_file():
        if _rewrite_cargo_lock_workspace_versions(repo_root, version):
            changed.append("Cargo.lock")

    run_version_sync_check(repo_root)
    return sorted(set(changed))


def run_version_sync_check(repo_root: Path) -> None:
    script = repo_root / "tools" / "ci" / "check_version_sync.py"
    if not script.is_file():
        # Disposable test trees may omit the checker; require VERSION grammar only.
        read_repo_version(repo_root)
        return
    env = dict(os.environ)
    env["IGNITE_VERSION_CHECK_ROOT"] = str(repo_root)
    completed = subprocess.run(
        [sys.executable, str(script)],
        cwd=str(repo_root),
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        raise ShipError(
            "version synchronization check failed: "
            + (completed.stderr or completed.stdout).strip()
        )


def _rewrite_cargo_lock_workspace_versions(repo_root: Path, version: str) -> bool:
    """Update Cargo.lock versions for workspace members that use workspace version."""
    try:
        import tomllib
    except ImportError:  # pragma: no cover
        return False
    cargo = tomllib.loads((repo_root / "Cargo.toml").read_text(encoding="utf-8"))
    members = cargo.get("workspace", {}).get("members", [])
    names: set[str] = set()
    for member in members:
        pkg_toml = repo_root / member / "Cargo.toml"
        if not pkg_toml.is_file():
            continue
        pkg = tomllib.loads(pkg_toml.read_text(encoding="utf-8"))
        name = pkg.get("package", {}).get("name")
        if isinstance(name, str):
            names.add(name)
    if not names:
        return False
    lock_path = repo_root / "Cargo.lock"
    original = lock_path.read_text(encoding="utf-8")
    blocks = re.split(r"(?m)(?=^\[\[package\]\])", original)
    out: list[str] = []
    changed = False
    for block in blocks:
        if not block.startswith("[[package]]"):
            out.append(block)
            continue
        name_m = re.search(r'(?m)^name = "([^"]+)"', block)
        if name_m and name_m.group(1) in names:
            new_block, n = re.subn(
                r'(?m)^(version = ")([^"]+)(")\s*$',
                rf'\g<1>{version}\g<3>',
                block,
                count=1,
            )
            if n:
                block = new_block
                changed = True
        out.append(block)
    if not changed:
        return False
    lock_path.write_text("".join(out), encoding="utf-8")
    return True
