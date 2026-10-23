#!/usr/bin/env python3
"""Fail if Ignite version files disagree (VERSION, Cargo, lockfiles, web)."""

from __future__ import annotations

import json
import os
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def die(msg: str) -> None:
    print(f"check_version_sync.py: {msg}", file=sys.stderr)
    sys.exit(1)


def cargo_lock_version(lock_text: str, name: str) -> str | None:
    blocks = re.split(r"\n\[\[package\]\]\n", lock_text)
    for block in blocks:
        n = re.search(r'^name = "([^"]+)"', block, re.M)
        v = re.search(r'^version = "([^"]+)"', block, re.M)
        if n and v and n.group(1) == name:
            return v.group(1)
    return None


def main() -> int:
    root = Path(os.environ.get("IGNITE_VERSION_CHECK_ROOT", ROOT))
    version = (root / "ignite" / "VERSION").read_text(encoding="utf-8").strip()
    if not re.match(r"^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$", version):
        die(f"ignite/VERSION is malformed: {version!r}")

    cargo = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    ws_ver = cargo["workspace"]["package"]["version"]
    if ws_ver != version:
        die(f"Cargo.toml workspace.package.version {ws_ver!r} != ignite/VERSION {version!r}")

    lock_text = (root / "Cargo.lock").read_text(encoding="utf-8")
    for member in cargo["workspace"]["members"]:
        pkg = tomllib.loads((root / member / "Cargo.toml").read_text(encoding="utf-8"))
        name = pkg["package"]["name"]
        ver_field = pkg["package"].get("version")
        if isinstance(ver_field, dict) and ver_field.get("workspace") is True:
            pass
        elif ver_field == version:
            pass
        else:
            die(f"{member} package.version {ver_field!r} is not workspace={version!r}")
        locked = cargo_lock_version(lock_text, name)
        if locked != version:
            die(f"Cargo.lock {name} version {locked!r} != {version!r}")

    web = json.loads((root / "ignite" / "web" / "package.json").read_text(encoding="utf-8"))
    if web.get("version") != version:
        die(f"ignite/web/package.json version {web.get('version')!r} != {version!r}")
    lock = json.loads((root / "ignite" / "web" / "package-lock.json").read_text(encoding="utf-8"))
    if lock.get("version") != version:
        die(f"ignite/web/package-lock.json version {lock.get('version')!r} != {version!r}")
    root_pkg = (lock.get("packages") or {}).get("") or {}
    if root_pkg.get("version") != version:
        die(
            f'ignite/web/package-lock.json packages[""].version '
            f"{root_pkg.get('version')!r} != {version!r}"
        )

    print(f"VERSION SYNC OK: {version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
