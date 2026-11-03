"""Integration tests for Option A publish path (Slice 3q.2).

Exercises assemble → seed → promote-release.sh command boundary without a
mocked ``publish_fn``. Real repository pin is never modified.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

_RELEASE = Path(__file__).resolve().parent
_REPO = _RELEASE.parents[1]
_KINDLE = _REPO / "deployment" / "kindle"
for path in (_RELEASE, _KINDLE):
    if str(path) not in sys.path:
        sys.path.insert(0, str(path))

from ship_lib.errors import ShipError  # noqa: E402
from ship_lib.publish import (  # noqa: E402
    assemble_nine_file_inventory,
    build_publish_env,
    canonical_nine_file_names,
    inspect_existing_release,
    publish_release,
    seed_promote_work_from_acquired,
)


def _bash() -> str | None:
    try:
        import release_orchestrate as ro

        return ro._resolve_bash()
    except Exception:
        return None


def _load_acquire_fixtures(tmp: Path):
    """Build disposable acquired+signed bundles via the 3o test helpers."""
    import importlib.util

    helper = _KINDLE / "test_release_acquire.py"
    spec = importlib.util.spec_from_file_location("ignite_test_release_acquire", helper)
    assert spec and spec.loader
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod._signed_bundle_for_shell(tmp)


class NineFileAssembleTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="ignite-ship-nine-"))

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_assemble_hashes_staged_canonical_names_only(self) -> None:
        _state, acquired, _root, _pub, signed = _load_acquire_fixtures(self.tmp)
        inventory = assemble_nine_file_inventory(
            acquired=acquired,
            signed=signed,
            dest=self.tmp / "nine",
        )
        expected = canonical_nine_file_names(
            version=acquired.identity.version,
            linux_bundle_name=acquired.identity.linux_bundle_name,
        )
        self.assertEqual(tuple(expected), inventory.asset_names)
        self.assertEqual(9, len(inventory.asset_digests))
        for name in expected:
            path = inventory.root / name
            self.assertTrue(path.is_file(), name)
            self.assertEqual(
                hashlib.sha256(path.read_bytes()).hexdigest(),
                inventory.asset_digests[name],
            )
        # Extra junk in another directory must not affect inventory.
        junk = self.tmp / "junk"
        junk.mkdir()
        (junk / "extra.bin").write_bytes(b"nope")
        self.assertNotIn("extra.bin", inventory.asset_names)

    def test_seed_populate_fresh_work_for_promote_verify(self) -> None:
        bash = _bash()
        if bash is None:
            self.skipTest("bash required")
        _state, acquired, _root, pub, signed = _load_acquire_fixtures(self.tmp)
        work = self.tmp / "promote-work"
        work.mkdir()
        seed_promote_work_from_acquired(acquired=acquired, work_dir=work)
        kindle_files = sorted(p.name for p in (work / "kindle").iterdir())
        linux_files = sorted(p.name for p in (work / "linux").iterdir())
        version = acquired.identity.version
        self.assertEqual(
            [
                f"Install Ignite v{version}.sh",
                f"Install Ignite v{version}.sha256",
            ],
            kindle_files,
        )
        self.assertEqual(
            [
                acquired.identity.linux_bundle_name,
                f"{acquired.identity.linux_bundle_name}.sha256",
            ],
            linux_files,
        )

        inventory = assemble_nine_file_inventory(
            acquired=acquired, signed=signed, dest=self.tmp / "nine"
        )
        env = build_publish_env(
            acquired=acquired, signed=signed, work_dir=work
        )
        # Official pin remains UNPROVISIONED — verify should reach CI artifact
        # checks then refuse at the pin/signature gate, not "missing kindle dir".
        script = (_REPO / "tools" / "release" / "promote-release.sh").as_posix()
        completed = subprocess.run(
            [bash, script, "verify"],
            cwd=str(_REPO),
            env={**os.environ, **env},
            capture_output=True,
            text=True,
            check=False,
        )
        combined = (completed.stderr or "") + (completed.stdout or "")
        self.assertNotIn("work kindle directory already exists", combined)
        self.assertNotIn("cannot list Kindle artifact files", combined)
        self.assertNotIn("Kindle artifact file set mismatch", combined)
        # Must not succeed publication while pin is UNPROVISIONED / fixture key.
        self.assertNotEqual(0, completed.returncode)
        self.assertTrue(
            "UNPROVISIONED" in combined
            or "signature" in combined.lower()
            or "public key" in combined.lower()
            or "production" in combined.lower()
            or "verify" in combined.lower(),
            combined[-1200:],
        )
        del inventory, pub  # fixtures used


class PublishRecoveryAndShellBoundaryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="ignite-ship-pub-"))

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_matching_existing_release_recovers_without_create(self) -> None:
        _state, acquired, _root, _pub, signed = _load_acquire_fixtures(self.tmp)
        inventory = assemble_nine_file_inventory(
            acquired=acquired, signed=signed, dest=self.tmp / "nine"
        )
        names = list(inventory.asset_names)
        store = self.tmp / "remote-release"
        store.mkdir()
        for name in names:
            shutil.copy2(inventory.root / name, store / name)

        class FakeGH:
            def __init__(self) -> None:
                self.create_calls = 0

            def run(self, args, *, cwd):
                if args[:2] == ["release", "view"]:
                    assets = [
                        {
                            "name": n,
                            "size": (store / n).stat().st_size,
                            "id": i + 1,
                        }
                        for i, n in enumerate(names)
                    ]
                    return subprocess.CompletedProcess(
                        args,
                        0,
                        json.dumps(
                            {
                                "tagName": inventory.tag,
                                "url": f"https://github.com/markrai/ignite/releases/tag/{inventory.tag}",
                                "isDraft": False,
                                "isPrerelease": False,
                                "assets": assets,
                            }
                        ),
                        "",
                    )
                if args[:1] == ["api"] and "git/ref/tags" in " ".join(args):
                    return subprocess.CompletedProcess(
                        args,
                        0,
                        json.dumps({"sha": inventory.commit, "type": "commit"}),
                        "",
                    )
                if args[:2] == ["release", "download"]:
                    dest = Path(args[args.index("-D") + 1])
                    dest.mkdir(parents=True, exist_ok=True)
                    for name in names:
                        shutil.copy2(store / name, dest / name)
                    return subprocess.CompletedProcess(args, 0, "", "")
                if args[:2] == ["release", "create"]:
                    self.create_calls += 1
                    return subprocess.CompletedProcess(args, 1, "", "should not create")
                return subprocess.CompletedProcess(args, 0, "", "")

        gh = FakeGH()
        env = build_publish_env(
            acquired=acquired,
            signed=signed,
            work_dir=self.tmp / "work",
        )
        # bash never invoked when existing release matches.
        result = publish_release(
            repo_root=_REPO,
            env=env,
            inventory=inventory,
            bash="/bin/false",
            gh=gh,
            promote_command="publish",
            allow_recover_existing=True,
        )
        self.assertTrue(result.recovered_existing)
        self.assertTrue(result.independently_verified)
        self.assertEqual(0, gh.create_calls)

    def test_mismatched_existing_release_refuses(self) -> None:
        _state, acquired, _root, _pub, signed = _load_acquire_fixtures(self.tmp)
        inventory = assemble_nine_file_inventory(
            acquired=acquired, signed=signed, dest=self.tmp / "nine"
        )
        names = list(inventory.asset_names)

        class FakeGH:
            def run(self, args, *, cwd):
                if args[:2] == ["release", "view"]:
                    assets = [
                        {"name": n, "size": 1, "id": i + 1} for i, n in enumerate(names)
                    ]
                    return subprocess.CompletedProcess(
                        args,
                        0,
                        json.dumps(
                            {
                                "tagName": inventory.tag,
                                "url": "https://example.invalid/r",
                                "isDraft": False,
                                "isPrerelease": False,
                                "assets": assets,
                            }
                        ),
                        "",
                    )
                if args[:1] == ["api"] and "git/ref/tags" in " ".join(args):
                    return subprocess.CompletedProcess(
                        args,
                        0,
                        json.dumps({"sha": inventory.commit, "type": "commit"}),
                        "",
                    )
                if args[:2] == ["release", "download"]:
                    dest = Path(args[args.index("-D") + 1])
                    dest.mkdir(parents=True, exist_ok=True)
                    for name in names:
                        (dest / name).write_bytes(b"tampered-" + name.encode())
                    return subprocess.CompletedProcess(args, 0, "", "")
                return subprocess.CompletedProcess(args, 0, "", "")

        with self.assertRaises(ShipError) as ctx:
            inspect_existing_release(
                repo_root=_REPO,
                tag=inventory.tag,
                expected_commit=inventory.commit,
                expected_asset_names=inventory.asset_names,
                expected_digests=inventory.asset_digests,
                gh=FakeGH(),
            )
        self.assertIn("does not match", str(ctx.exception).lower())

    def test_shell_boundary_receives_seeded_work_and_publish_command(self) -> None:
        """Real publish_release → bash argv boundary with seeded WORK layout."""
        _state, acquired, _root, _pub, signed = _load_acquire_fixtures(self.tmp)
        inventory = assemble_nine_file_inventory(
            acquired=acquired, signed=signed, dest=self.tmp / "nine"
        )
        work = self.tmp / "promote-work"
        work.mkdir()
        seed_promote_work_from_acquired(acquired=acquired, work_dir=work)
        env = build_publish_env(acquired=acquired, signed=signed, work_dir=work)
        self.assertNotIn("IGNITE_PROMOTE_PUBLIC_KEY_FILE", env)

        log = self.tmp / "bash-log.json"
        shim = self.tmp / "shim.py"
        shim.write_text(
            "\n".join(
                [
                    "import json, os, sys",
                    "from pathlib import Path",
                    f"log = Path(r'{log}')",
                    "argv = sys.argv[1:]",
                    "work = Path(os.environ['IGNITE_PROMOTE_WORK'])",
                    "payload = {",
                    "  'argv': argv,",
                    "  'command': argv[-1] if argv else '',",
                    "  'script': argv[0] if argv else '',",
                    "  'kindle': sorted(p.name for p in (work/'kindle').iterdir()),",
                    "  'linux': sorted(p.name for p in (work/'linux').iterdir()),",
                    "  'has_override': 'IGNITE_PROMOTE_PUBLIC_KEY_FILE' in os.environ,",
                    "}",
                    "log.write_text(json.dumps(payload), encoding='utf-8')",
                    "print('published: ' + os.environ['IGNITE_PROMOTE_TAG'])",
                    "print('https://github.com/markrai/ignite/releases/tag/' + os.environ['IGNITE_PROMOTE_TAG'])",
                ]
            )
            + "\n",
            encoding="utf-8",
        )
        if os.name == "nt":
            bash = self.tmp / "promote-bash.cmd"
            bash.write_text(
                f'@echo off\r\n"{sys.executable}" "{shim}" %*\r\n',
                encoding="utf-8",
            )
        else:
            bash = self.tmp / "promote-bash"
            bash.write_text(
                f"#!/bin/sh\nexec '{sys.executable}' '{shim}' \"$@\"\n",
                encoding="utf-8",
                newline="\n",
            )
            bash.chmod(0o755)

        names = list(inventory.asset_names)
        store = {n: (inventory.root / n).read_bytes() for n in names}

        class FakeGH:
            def run(self, args, *, cwd):
                joined = " ".join(args)
                if args[:2] == ["release", "view"]:
                    if not log.is_file():
                        return subprocess.CompletedProcess(
                            args, 1, "", "release not found"
                        )
                    assets = [
                        {"name": n, "size": len(store[n]), "id": i + 1}
                        for i, n in enumerate(names)
                    ]
                    return subprocess.CompletedProcess(
                        args,
                        0,
                        json.dumps(
                            {
                                "tagName": inventory.tag,
                                "url": (
                                    "https://github.com/markrai/ignite/releases/tag/"
                                    + inventory.tag
                                ),
                                "isDraft": False,
                                "isPrerelease": False,
                                "assets": assets,
                            }
                        ),
                        "",
                    )
                if args[:1] == ["api"] and "releases/tags" in joined:
                    return subprocess.CompletedProcess(args, 1, "", "HTTP 404")
                if args[:1] == ["api"] and "git/ref/tags" in joined:
                    return subprocess.CompletedProcess(
                        args,
                        0,
                        json.dumps({"sha": inventory.commit, "type": "commit"}),
                        "",
                    )
                if args[:2] == ["release", "download"]:
                    dest = Path(args[args.index("-D") + 1])
                    dest.mkdir(parents=True, exist_ok=True)
                    for name, data in store.items():
                        (dest / name).write_bytes(data)
                    return subprocess.CompletedProcess(args, 0, "", "")
                return subprocess.CompletedProcess(args, 0, "", "")

        result = publish_release(
            repo_root=_REPO,
            env=env,
            inventory=inventory,
            bash=str(bash),
            gh=FakeGH(),
            promote_command="publish",
            allow_recover_existing=True,
        )
        self.assertTrue(result.independently_verified, result)
        self.assertFalse(result.recovered_existing)
        self.assertEqual("publish", result.promote_command)
        self.assertTrue(log.is_file())
        payload = json.loads(log.read_text(encoding="utf-8"))
        self.assertEqual("publish", payload["command"])
        self.assertIn("promote-release.sh", payload["script"].replace("\\", "/"))
        self.assertEqual(2, len(payload["kindle"]))
        self.assertEqual(2, len(payload["linux"]))
        self.assertFalse(payload["has_override"])

    def test_missing_ci_artifact_refuses_seed(self) -> None:
        _state, acquired, _root, _pub, signed = _load_acquire_fixtures(self.tmp)
        # Remove linux bundle from acquired tree.
        linux = acquired.linux_dir / acquired.identity.linux_bundle_name
        linux.unlink()
        with self.assertRaises(ShipError):
            seed_promote_work_from_acquired(
                acquired=acquired, work_dir=self.tmp / "work"
            )


class FixtureKeyCannotAuthorizeTests(unittest.TestCase):
    def test_publish_env_strips_fixture_key_override(self) -> None:
        tmp = Path(tempfile.mkdtemp(prefix="ignite-ship-fix-"))
        try:
            _state, acquired, _root, pub, signed = _load_acquire_fixtures(tmp)
            os.environ["IGNITE_PROMOTE_PUBLIC_KEY_FILE"] = str(pub)
            try:
                env = build_publish_env(
                    acquired=acquired,
                    signed=signed,
                    work_dir=tmp / "work",
                )
            finally:
                os.environ.pop("IGNITE_PROMOTE_PUBLIC_KEY_FILE", None)
            self.assertNotIn("IGNITE_PROMOTE_PUBLIC_KEY_FILE", env)
        finally:
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
