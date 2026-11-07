#!/usr/bin/env python3
"""Deterministic fixtures for the standalone Ignite version tool."""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

_RELEASE = Path(__file__).resolve().parent
_REPO = _RELEASE.parents[1]
CLI = _RELEASE / "ignite_version.py"
if str(_RELEASE) not in sys.path:
    sys.path.insert(0, str(_RELEASE))

from ship_lib.errors import ShipError  # noqa: E402
from ship_lib.version_sync import apply_version  # noqa: E402
from ship_lib import version_sync as version_sync_mod  # noqa: E402


OLD = "0.9.21"
NEW = "0.9.22"


def _write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def _complete_tree(root: Path, version: str) -> None:
    _write(root / "ignite" / "VERSION", version + "\n")
    _write(
        root / "Cargo.toml",
        "[workspace]\n"
        'members = ["pkg", "other"]\n\n'
        "[workspace.package]\n"
        f'version = "{version}"\n'
        'edition = "2021"\n',
    )
    _write(
        root / "pkg" / "Cargo.toml",
        '[package]\nname = "pkg"\nversion.workspace = true\nedition = "2021"\n',
    )
    _write(
        root / "other" / "Cargo.toml",
        "[package]\n"
        'name = "other"\n'
        f'version = "{version}"\n'
        'edition = "2021"\n',
    )
    _write(
        root / "Cargo.lock",
        "version = 3\n\n"
        "[[package]]\n"
        'name = "other"\n'
        f'version = "{version}"\n\n'
        "[[package]]\n"
        'name = "pkg"\n'
        f'version = "{version}"\n\n'
        "[[package]]\n"
        'name = "serde"\n'
        'version = "1.0.200"\n'
        'source = "registry+https://github.com/rust-lang/crates.io-index"\n',
    )
    _write(
        root / "ignite" / "web" / "package.json",
        "{\n"
        '  "name": "web",\n'
        f'  "version": "{version}",\n'
        '  "dependencies": {\n'
        '    "left-pad": "1.3.0"\n'
        "  }\n"
        "}\n",
    )
    _write(
        root / "ignite" / "web" / "package-lock.json",
        "{\n"
        '  "name": "web",\n'
        f'  "version": "{version}",\n'
        '  "lockfileVersion": 3,\n'
        '  "packages": {\n'
        '    "": {\n'
        '      "name": "web",\n'
        f'      "version": "{version}"\n'
        "    },\n"
        '    "node_modules/left-pad": {\n'
        '      "version": "1.3.0"\n'
        "    }\n"
        "  }\n"
        "}\n",
    )
    _write(root / "CHANGELOG.md", "# Changelog\n\nMaintainer-owned notes.\n")


def _snapshot(root: Path) -> dict[str, bytes]:
    out: dict[str, bytes] = {}
    for path in root.rglob("*"):
        if path.is_file():
            out[path.relative_to(root).as_posix()] = path.read_bytes()
    return out


def _run_cli(root: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(CLI), "--repo-root", str(root), *args],
        cwd=str(_REPO),
        text=True,
        capture_output=True,
        check=False,
    )


class CompleteTreeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="ignite-version-test-"))
        self.root = self.tmp / "repo"
        _complete_tree(self.root, OLD)

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_bump_0_9_21_to_0_9_22(self) -> None:
        result = _run_cli(self.root, "set", NEW)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertEqual(NEW, (self.root / "ignite" / "VERSION").read_text(encoding="utf-8").strip())
        cargo = (self.root / "Cargo.toml").read_text(encoding="utf-8")
        self.assertIn(f'version = "{NEW}"', cargo)
        self.assertIn('edition = "2021"', cargo)
        other = (self.root / "other" / "Cargo.toml").read_text(encoding="utf-8")
        self.assertIn(f'version = "{NEW}"', other)
        self.assertTrue((self.root / "pkg" / "Cargo.toml").read_text(encoding="utf-8").count("version.workspace = true"))
        lock = (self.root / "Cargo.lock").read_text(encoding="utf-8")
        self.assertIn(f'name = "pkg"\nversion = "{NEW}"', lock)
        self.assertIn(f'name = "other"\nversion = "{NEW}"', lock)
        web = json.loads((self.root / "ignite" / "web" / "package.json").read_text(encoding="utf-8"))
        self.assertEqual(NEW, web["version"])
        self.assertEqual("1.3.0", web["dependencies"]["left-pad"])
        web_lock = json.loads(
            (self.root / "ignite" / "web" / "package-lock.json").read_text(encoding="utf-8")
        )
        self.assertEqual(NEW, web_lock["version"])
        self.assertEqual(NEW, web_lock["packages"][""]["version"])
        self.assertIn("VERSION SET:", result.stdout)

    def test_second_identical_set_is_byte_identical(self) -> None:
        first = _run_cli(self.root, "set", NEW)
        self.assertEqual(0, first.returncode, first.stderr)
        after_first = _snapshot(self.root)
        second = _run_cli(self.root, "set", NEW)
        self.assertEqual(0, second.returncode, second.stderr)
        self.assertIn("changed=none", second.stdout)
        self.assertEqual(after_first, _snapshot(self.root))

    def test_cargo_lock_workspace_members_and_third_party(self) -> None:
        apply_version(self.root, NEW, require_complete=True)
        lock = (self.root / "Cargo.lock").read_text(encoding="utf-8")
        self.assertRegex(lock, r'name = "pkg"\nversion = "0\.9\.22"')
        self.assertRegex(lock, r'name = "other"\nversion = "0\.9\.22"')
        self.assertIn('name = "serde"\nversion = "1.0.200"', lock)
        self.assertIn(
            'source = "registry+https://github.com/rust-lang/crates.io-index"',
            lock,
        )
        web_lock = json.loads(
            (self.root / "ignite" / "web" / "package-lock.json").read_text(encoding="utf-8")
        )
        self.assertEqual("1.3.0", web_lock["packages"]["node_modules/left-pad"]["version"])
        self.assertEqual(
            "# Changelog\n\nMaintainer-owned notes.\n",
            (self.root / "CHANGELOG.md").read_text(encoding="utf-8"),
        )

    def test_plan_is_read_only_and_lists_fields(self) -> None:
        before = _snapshot(self.root)
        result = _run_cli(self.root, "plan", NEW)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertEqual(before, _snapshot(self.root))
        self.assertIn("VERSION PLAN: 0.9.21 -> 0.9.22", result.stdout)
        self.assertIn("ignite/VERSION: value 0.9.21 -> 0.9.22", result.stdout)
        self.assertIn("Cargo.toml: workspace.package.version 0.9.21 -> 0.9.22", result.stdout)
        self.assertIn("other/Cargo.toml: package.version 0.9.21 -> 0.9.22", result.stdout)
        self.assertIn("Cargo.lock: package[pkg].version 0.9.21 -> 0.9.22", result.stdout)
        self.assertIn("Cargo.lock: package[other].version 0.9.21 -> 0.9.22", result.stdout)
        self.assertIn("ignite/web/package.json: version 0.9.21 -> 0.9.22", result.stdout)
        self.assertIn(
            'ignite/web/package-lock.json: packages[""].version 0.9.21 -> 0.9.22',
            result.stdout,
        )
        self.assertNotIn("serde", result.stdout)

    def test_check_is_read_only(self) -> None:
        before = _snapshot(self.root)
        result = _run_cli(self.root, "check")
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("VERSION SYNC OK: 0.9.21", result.stdout)
        self.assertEqual(before, _snapshot(self.root))

    def test_check_reports_mismatch_without_writing(self) -> None:
        (self.root / "ignite" / "VERSION").write_text("0.9.20\n", encoding="utf-8")
        before = _snapshot(self.root)
        result = _run_cli(self.root, "check")
        self.assertNotEqual(0, result.returncode)
        self.assertEqual(before, _snapshot(self.root))

    def test_mismatched_versions_refuse_set_and_plan(self) -> None:
        (self.root / "ignite" / "VERSION").write_text("0.9.20\n", encoding="utf-8")
        before = _snapshot(self.root)
        planned = _run_cli(self.root, "plan", NEW)
        self.assertNotEqual(0, planned.returncode)
        self.assertTrue(planned.stderr.strip())
        applied = _run_cli(self.root, "set", NEW)
        self.assertNotEqual(0, applied.returncode)
        self.assertEqual(before, _snapshot(self.root))

    def test_malformed_manifests_refuse(self) -> None:
        (self.root / "ignite" / "web" / "package.json").write_text(
            "{not json\n", encoding="utf-8"
        )
        before = _snapshot(self.root)
        result = _run_cli(self.root, "set", NEW)
        self.assertNotEqual(0, result.returncode)
        self.assertIn("malformed JSON", result.stderr)
        self.assertEqual(before, _snapshot(self.root))

    def test_missing_required_file_refuses(self) -> None:
        (self.root / "Cargo.lock").unlink()
        before = _snapshot(self.root)
        for command in (("check",), ("plan", NEW), ("set", NEW)):
            result = _run_cli(self.root, *command)
            self.assertNotEqual(0, result.returncode, command)
            self.assertIn("missing required version files: Cargo.lock", result.stderr)
        self.assertEqual(before, _snapshot(self.root))

    def test_inconsistent_workspace_member_refuses(self) -> None:
        shutil.rmtree(self.root / "pkg")
        before = _snapshot(self.root)
        for command in (("check",), ("plan", NEW), ("set", NEW)):
            result = _run_cli(self.root, *command)
            self.assertNotEqual(0, result.returncode, command)
            self.assertIn("inconsistent workspace members", result.stderr)
        self.assertEqual(before, _snapshot(self.root))

    def test_invalid_version_string_refuses(self) -> None:
        before = _snapshot(self.root)
        result = _run_cli(self.root, "set", "v0.9.22")
        self.assertNotEqual(0, result.returncode)
        self.assertIn("malformed release version", result.stderr)
        self.assertEqual(before, _snapshot(self.root))

    def test_write_failure_rolls_back_every_affected_file(self) -> None:
        before = _snapshot(self.root)
        orig = version_sync_mod._write_text
        calls = {"n": 0}

        def boom(path: Path, text: str) -> None:
            calls["n"] += 1
            if calls["n"] >= 2:
                raise OSError("simulated write failure")
            orig(path, text)

        with mock.patch.object(version_sync_mod, "_write_text", boom):
            with self.assertRaises(ShipError) as ctx:
                apply_version(self.root, NEW, require_complete=True)
        self.assertIn("rolled back", str(ctx.exception))
        self.assertGreaterEqual(calls["n"], 2)
        self.assertEqual(before, _snapshot(self.root))

    def test_keyboard_interrupt_during_update_restores_bytes(self) -> None:
        before = _snapshot(self.root)
        orig = version_sync_mod._write_text
        calls = {"n": 0}

        def interrupt_after_first(path: Path, text: str) -> None:
            calls["n"] += 1
            orig(path, text)
            if calls["n"] >= 2:
                raise KeyboardInterrupt()

        with mock.patch.object(version_sync_mod, "_write_text", interrupt_after_first):
            with self.assertRaises(KeyboardInterrupt):
                apply_version(self.root, NEW, require_complete=True)
        self.assertGreaterEqual(calls["n"], 2)
        self.assertEqual(before, _snapshot(self.root))

    def test_interrupt_restore_failure_is_reported_without_claiming_rollback(
        self,
    ) -> None:
        orig = version_sync_mod._write_text
        calls = {"n": 0}

        def interrupt_after_first(path: Path, text: str) -> None:
            calls["n"] += 1
            orig(path, text)
            if calls["n"] >= 2:
                raise KeyboardInterrupt()

        def fail_restore(_originals: dict[Path, bytes]) -> None:
            raise ShipError("version file restoration failed: simulated restore failure")

        with mock.patch.object(version_sync_mod, "_write_text", interrupt_after_first):
            with mock.patch.object(
                version_sync_mod, "_restore_original_bytes", fail_restore
            ):
                with self.assertRaises(ShipError) as ctx:
                    apply_version(self.root, NEW, require_complete=True)
        message = str(ctx.exception)
        self.assertIn("restoration failed", message)
        self.assertIn("KeyboardInterrupt", message)
        self.assertNotIn("rolled back", message)
        self.assertGreaterEqual(calls["n"], 2)

    def test_plan_same_version_lists_no_changes(self) -> None:
        result = _run_cli(self.root, "plan", OLD)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("(no file changes)", result.stdout)


class ShipCompatibilityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="ignite-version-ship-"))
        self.repo = self.tmp / "repo"
        (self.repo / "tools" / "release").mkdir(parents=True)
        (self.repo / "ignite" / "web").mkdir(parents=True)
        (self.repo / "ignite" / "VERSION").write_text("0.9.14\n", encoding="utf-8")
        (self.repo / "Cargo.toml").write_text(
            '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "0.9.14"\n',
            encoding="utf-8",
        )
        (self.repo / "ignite" / "web" / "package.json").write_text(
            json.dumps({"name": "ignite-web", "version": "0.9.14"}) + "\n",
            encoding="utf-8",
        )
        (self.repo / "ignite" / "web" / "package-lock.json").write_text(
            json.dumps(
                {"version": "0.9.14", "packages": {"": {"version": "0.9.14"}}}
            )
            + "\n",
            encoding="utf-8",
        )

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_apply_version_on_mini_tree_matches_ship_contract(self) -> None:
        compact_web = (self.repo / "ignite" / "web" / "package.json").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("\n  ", compact_web)
        changed = apply_version(self.repo, "0.9.15")
        self.assertIn("ignite/VERSION", changed)
        self.assertEqual(
            "0.9.15",
            (self.repo / "ignite" / "VERSION").read_text(encoding="utf-8").strip(),
        )
        self.assertIn("Cargo.toml", changed)
        self.assertIn("ignite/web/package.json", changed)
        web = (self.repo / "ignite" / "web" / "package.json").read_text(encoding="utf-8")
        self.assertIn('"version": "0.9.15"', web)
        self.assertNotIn("\n  ", web)
        again = apply_version(self.repo, "0.9.15")
        self.assertEqual([], again)

    def test_cli_requires_complete_tree_unlike_ship_apply_version(self) -> None:
        result = _run_cli(self.repo, "set", "0.9.15")
        self.assertNotEqual(0, result.returncode)
        self.assertIn("missing required version files: Cargo.lock", result.stderr)
        self.assertEqual(
            "0.9.14",
            (self.repo / "ignite" / "VERSION").read_text(encoding="utf-8").strip(),
        )


class LiveRepoReadOnlyTests(unittest.TestCase):
    def test_check_current_repository(self) -> None:
        result = subprocess.run(
            [sys.executable, str(CLI), "check"],
            cwd=str(_REPO),
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        version = (_REPO / "ignite" / "VERSION").read_text(encoding="utf-8").strip()
        self.assertIn(f"VERSION SYNC OK: {version}", result.stdout)

    def test_plan_current_version_is_noop_on_this_tree(self) -> None:
        version = (_REPO / "ignite" / "VERSION").read_text(encoding="utf-8").strip()
        before_version = (_REPO / "ignite" / "VERSION").read_bytes()
        before_cargo = (_REPO / "Cargo.toml").read_bytes()
        result = subprocess.run(
            [sys.executable, str(CLI), "plan", version],
            cwd=str(_REPO),
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("(no file changes)", result.stdout)
        self.assertEqual(before_version, (_REPO / "ignite" / "VERSION").read_bytes())
        self.assertEqual(before_cargo, (_REPO / "Cargo.toml").read_bytes())


if __name__ == "__main__":
    unittest.main()
