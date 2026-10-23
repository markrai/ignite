#!/usr/bin/env python3
"""Fail-closed fixture for tools/ci/check_version_sync.py (no pytest required)."""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CHECKER = Path(__file__).resolve().parent / "check_version_sync.py"


def _write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def _tree(root: Path, version: str) -> None:
    _write(root / "ignite" / "VERSION", version + "\n")
    _write(
        root / "Cargo.toml",
        "[workspace]\nmembers = [\"pkg\"]\n\n[workspace.package]\n"
        f'version = "{version}"\n',
    )
    _write(
        root / "pkg" / "Cargo.toml",
        '[package]\nname = "pkg"\nversion.workspace = true\nedition = "2021"\n',
    )
    _write(
        root / "Cargo.lock",
        "version = 3\n\n[[package]]\nname = \"pkg\"\n"
        f'version = "{version}"\n',
    )
    _write(root / "ignite" / "web" / "package.json", f'{{"name": "web", "version": "{version}"}}\n')
    _write(
        root / "ignite" / "web" / "package-lock.json",
        f'{{"name": "web", "version": "{version}", "packages": {{"": {{"version": "{version}"}}}}}} \n',
    )


def _run(root: Path) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env["IGNITE_VERSION_CHECK_ROOT"] = str(root)
    return subprocess.run(
        [sys.executable, str(CHECKER)],
        cwd=str(ROOT),
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )


def main() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        aligned = Path(tmp) / "ok"
        _tree(aligned, "0.9.14")
        ok = _run(aligned)
        if ok.returncode != 0:
            print(ok.stderr, file=sys.stderr)
            print("test_check_version_sync.py: aligned fixture must pass", file=sys.stderr)
            return 1

        broken = Path(tmp) / "bad"
        _tree(broken, "0.9.14")
        (broken / "ignite" / "VERSION").write_text("0.9.13\n", encoding="utf-8")
        bad = _run(broken)
        if bad.returncode == 0:
            print("test_check_version_sync.py: mismatch fixture must fail", file=sys.stderr)
            return 1
        if "ignite/VERSION" not in bad.stderr and "Cargo.toml" not in bad.stderr:
            print(bad.stderr, file=sys.stderr)
            print("test_check_version_sync.py: expected a VERSION/Cargo mismatch", file=sys.stderr)
            return 1

    print("VERSION SYNC FIXTURE OK: mismatch fails closed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
