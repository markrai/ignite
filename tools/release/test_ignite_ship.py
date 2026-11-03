"""Deterministic tests for ignite_ship orchestration (Slice 3q.1)."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

_RELEASE = Path(__file__).resolve().parent
_REPO = _RELEASE.parents[1]
if str(_RELEASE) not in sys.path:
    sys.path.insert(0, str(_RELEASE))

from ship_lib.authority import (  # noqa: E402
    authority_present,
    initialize_authority,
    load_authority,
)
from ship_lib.config import ShipConfig, load_ship_config  # noqa: E402
from ship_lib.errors import ShipBlockedError, ShipConfigError, ShipError  # noqa: E402
from ship_lib.hardware import (  # noqa: E402
    HardwareAcceptanceHarness,
    MockHardwareProbe,
    ScriptedHardwareProbe,
)
from ship_lib.journal import ShipJournal  # noqa: E402
from ship_lib.orchestrator import ShipHooks, ShipOrchestrator, ShipReport  # noqa: E402
from ship_lib.outcomes import (  # noqa: E402
    PHASE_AUTHORITY,
    PHASE_DONE,
    PHASE_PUBLISH,
    PhaseState,
    ShipOutcome,
)
from ship_lib.pin_provision import provision_public_pin  # noqa: E402
from ship_lib.git_coord import (  # noqa: E402
    commit_release_preparation,
    create_or_update_pr,
    merge_pr,
    release_commit_paths,
    verify_release_tree,
    wait_for_pr_checks,
)
from ship_lib.publish import (  # noqa: E402
    canonical_nine_file_names,
    verify_published_release,
)
from ship_lib.recovery import (  # noqa: E402
    assert_dpapi_alone_insufficient_label,
    ensure_recovery_key_destination,
    verify_independent_restore,
)
from ship_lib.secret_store import InsecureFileStore, select_secret_store  # noqa: E402
from ship_lib.signing_service import RestrictedSigningService, SignReleaseRequest  # noqa: E402
from ship_lib.version_sync import apply_version, validate_version_string  # noqa: E402


def _openssl_available() -> bool:
    return shutil.which("openssl") is not None


def _init_mini_repo(root: Path, *, version: str = "0.9.14") -> None:
    (root / "tools" / "release").mkdir(parents=True)
    (root / "ignite" / "web").mkdir(parents=True)
    (root / "ignite" / "kindle" / "scribe-app" / "src").mkdir(parents=True)
    (root / "deployment" / "kindle").mkdir(parents=True)
    (root / "ignite" / "VERSION").write_text(version + "\n", encoding="utf-8")
    (root / "Cargo.toml").write_text(
        f'[workspace]\nmembers = []\n\n[workspace.package]\nversion = "{version}"\n',
        encoding="utf-8",
    )
    (root / "ignite" / "web" / "package.json").write_text(
        json.dumps({"name": "ignite-web", "version": version}) + "\n",
        encoding="utf-8",
    )
    (root / "ignite" / "web" / "package-lock.json").write_text(
        json.dumps(
            {"version": version, "packages": {"": {"version": version}}}
        )
        + "\n",
        encoding="utf-8",
    )
    (root / "tools" / "release" / "kindle-release-public-key.hex").write_bytes(
        b"UNPROVISIONED\n"
    )
    (
        root
        / "ignite"
        / "kindle"
        / "scribe-app"
        / "src"
        / "release_manifest.rs"
    ).write_text(
        'include_str!("../../../../tools/release/kindle-release-public-key.hex");\n',
        encoding="utf-8",
    )


@unittest.skipUnless(_openssl_available(), "openssl required")
class ShipAuthorityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="ignite-ship-test-"))
        self.repo = self.tmp / "repo"
        self.home = self.tmp / "ship-home"
        self.recovery = self.tmp / "recovery-backup"
        self.key_dir = self.tmp / "recovery-keys"
        _init_mini_repo(self.repo)
        self.config = ShipConfig(
            repo_root=self.repo,
            ship_home=self.home,
            recovery_dir=self.recovery,
            recovery_key_dir=self.key_dir,
            allow_insecure_store=True,
            require_hardware=True,
        )
        self.store = InsecureFileStore(allow_insecure=True)

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_init_authority_and_refuse_regen(self) -> None:
        auth = initialize_authority(self.config, store=self.store)
        self.assertTrue(authority_present(self.config))
        self.assertEqual(64, len(auth.public_key.hex()))
        self.assertTrue(auth.recovery_verified)
        self.assertTrue(
            (self.recovery / "publisher-private.recovery.enc").is_file()
        )
        self.assertTrue((self.key_dir / "recovery-authority.key").is_file())
        self.assertFalse((self.recovery / "recovery-authority.key").exists())
        with self.assertRaises(ShipError):
            initialize_authority(self.config, store=self.store)

    def test_missing_recovery_dir_refused(self) -> None:
        cfg = ShipConfig(
            repo_root=self.repo,
            ship_home=self.home,
            recovery_dir=None,
            recovery_key_dir=self.key_dir,
            allow_insecure_store=True,
        )
        with self.assertRaises(ShipConfigError):
            initialize_authority(cfg, store=self.store)

    def test_missing_recovery_key_dir_refused(self) -> None:
        cfg = ShipConfig(
            repo_root=self.repo,
            ship_home=self.home,
            recovery_dir=self.recovery,
            recovery_key_dir=None,
            allow_insecure_store=True,
        )
        with self.assertRaises(ShipConfigError):
            initialize_authority(cfg, store=self.store)

    def test_colocated_recovery_key_domain_refused(self) -> None:
        with self.assertRaises(ShipConfigError):
            ensure_recovery_key_destination(
                self.recovery,  # same path as backup domain
                recovery_dir=self.recovery,
                ship_home=self.home,
                repo_root=self.repo,
            )

    def test_recovery_inside_repo_refused(self) -> None:
        with self.assertRaises(ShipConfigError):
            load_ship_config(
                repo_root=self.repo,
                ship_home=self.home,
                recovery_dir=self.repo / "backup",
                allow_live=False,
            )

    def test_independent_recovery_without_secret_store(self) -> None:
        auth = initialize_authority(self.config, store=self.store)
        ok = verify_independent_restore(
            recovery_dir=self.recovery,
            recovery_key_dir=self.key_dir,
            expected_public=auth.public_key,
            expected_fingerprint=auth.fingerprint_sha256,
        )
        self.assertTrue(ok)
        # Backup destination alone must not decrypt.
        self.assertFalse(
            verify_independent_restore(
                recovery_dir=self.recovery,
                recovery_key_dir=self.recovery,  # wrong domain / missing key
                expected_public=auth.public_key,
                expected_fingerprint=auth.fingerprint_sha256,
            )
        )

    def test_dpapi_alone_is_labeled_insufficient(self) -> None:
        label = assert_dpapi_alone_insufficient_label()
        self.assertIn("NOT independent recovery", label)
        self.assertIn("separated", label)

    def test_insecure_store_refused_by_default(self) -> None:
        with self.assertRaises(ShipConfigError):
            InsecureFileStore(allow_insecure=False).seal(b"x")

    def test_insecure_store_forbidden_for_production_select(self) -> None:
        with self.assertRaises(ShipConfigError):
            select_secret_store(allow_insecure=True, production=True)

    def test_insecure_forbidden_for_production_authority(self) -> None:
        with self.assertRaises(ShipConfigError):
            initialize_authority(
                self.config, store=self.store, production=True
            )

    def test_provision_pin_and_persist(self) -> None:
        auth = initialize_authority(self.config, store=self.store)
        real_kindle = _REPO / "deployment" / "kindle"
        real_release = _REPO / "tools" / "release"
        sys.path.insert(0, str(real_kindle))
        sys.path.insert(0, str(real_release))
        fp = provision_public_pin(self.config, authority=auth)
        self.assertEqual(fp, auth.fingerprint_sha256)
        pin = (
            self.repo / "tools" / "release" / "kindle-release-public-key.hex"
        ).read_bytes()
        self.assertEqual(pin, auth.public_key.hex().encode("ascii") + b"\n")
        fp2 = provision_public_pin(self.config, authority=auth)
        self.assertEqual(fp, fp2)

    def test_journal_refuses_secrets(self) -> None:
        journal = ShipJournal.open_for_version(self.home / "journal", "0.9.15")
        with self.assertRaises(ShipError):
            journal.mark_completed(PHASE_AUTHORITY, private_key="secret")

    def test_journal_planned_is_not_complete(self) -> None:
        journal = ShipJournal.open_for_version(self.home / "journal", "0.9.15")
        journal.mark_planned(PHASE_PUBLISH, note="plan-only")
        self.assertFalse(journal.is_complete(PHASE_PUBLISH))
        self.assertTrue(journal.is_planned(PHASE_PUBLISH))
        with self.assertRaises(ShipError):
            journal.mark_completed(PHASE_PUBLISH, executed=False)
        with self.assertRaises(ShipError):
            journal.mark_completed(PHASE_DONE, published=True)

    def test_partial_journal_write_recovery(self) -> None:
        journal = ShipJournal.open_for_version(self.home / "journal", "0.9.15")
        journal.mark_deferred(PHASE_AUTHORITY, reason="test")
        # Corrupt a temp sibling then ensure load of real journal still works.
        tmp = journal.path.with_suffix(journal.path.suffix + ".tmp")
        tmp.write_text("{not-json", encoding="ascii")
        loaded = ShipJournal.load(journal.path)
        self.assertEqual(PhaseState.DEFERRED, loaded.state_of(PHASE_AUTHORITY))
        tmp.unlink(missing_ok=True)


@unittest.skipUnless(_openssl_available(), "openssl required")
class ShipOrchestratorBoundaryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="ignite-ship-orch-"))
        self.repo = self.tmp / "repo"
        self.home = self.tmp / "ship-home"
        self.recovery = self.tmp / "recovery-backup"
        self.key_dir = self.tmp / "recovery-keys"
        _init_mini_repo(self.repo)
        self.config = ShipConfig(
            repo_root=self.repo,
            ship_home=self.home,
            recovery_dir=self.recovery,
            recovery_key_dir=self.key_dir,
            allow_insecure_store=True,
            allow_live=False,
            require_hardware=True,
            hardware_device_id="qa-kindle-1",
        )
        self.store = InsecureFileStore(allow_insecure=True)
        self.real_pin_before = (
            _REPO / "tools" / "release" / "kindle-release-public-key.hex"
        ).read_bytes()

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        # Real pin must remain untouched by every test.
        real_pin = (
            _REPO / "tools" / "release" / "kindle-release-public-key.hex"
        ).read_bytes()
        self.assertEqual(self.real_pin_before, real_pin)

    def test_non_live_ship_does_not_mutate_pin_or_authority(self) -> None:
        hooks = ShipHooks(
            secret_store=self.store,
            execute_live_mutations=False,
            hardware_probe=MockHardwareProbe(pass_all=True),
        )
        orch = ShipOrchestrator(self.config, hooks=hooks)
        report = orch.ship("0.9.15")
        self.assertNotEqual(ShipOutcome.SUCCESS, report.outcome)
        self.assertIn(
            report.outcome,
            {ShipOutcome.PLANNED, ShipOutcome.NOT_PERFORMED, ShipOutcome.PARTIAL},
        )
        self.assertFalse(authority_present(self.config))
        pin = (
            self.repo / "tools" / "release" / "kindle-release-public-key.hex"
        ).read_bytes()
        self.assertEqual(b"UNPROVISIONED\n", pin)
        self.assertEqual(
            "0.9.14", (self.repo / "ignite" / "VERSION").read_text().strip()
        )
        self.assertIn("published=false", report.lines)
        journal = ShipJournal.open_for_version(self.config.journal_dir, "0.9.15")
        self.assertFalse(journal.is_complete(PHASE_AUTHORITY))
        self.assertFalse(journal.is_complete(PHASE_PUBLISH))
        self.assertFalse(journal.published())

    def test_plan_command_is_non_mutating(self) -> None:
        orch = ShipOrchestrator(
            self.config,
            hooks=ShipHooks(secret_store=self.store, execute_live_mutations=False),
        )
        report = orch.plan("0.9.15")
        self.assertEqual(ShipOutcome.PLANNED, report.outcome)
        self.assertFalse(authority_present(self.config))
        self.assertEqual(
            b"UNPROVISIONED\n",
            (
                self.repo / "tools" / "release" / "kindle-release-public-key.hex"
            ).read_bytes(),
        )

    def test_resume_does_not_treat_planned_as_completed(self) -> None:
        orch = ShipOrchestrator(
            self.config,
            hooks=ShipHooks(secret_store=self.store, execute_live_mutations=False),
        )
        first = orch.ship("0.9.15")
        self.assertNotEqual(ShipOutcome.SUCCESS, first.outcome)
        journal = ShipJournal.open_for_version(self.config.journal_dir, "0.9.15")
        self.assertTrue(
            journal.is_deferred(PHASE_AUTHORITY) or journal.is_planned(PHASE_AUTHORITY)
        )
        second = orch.ship("0.9.15", resume=True)
        self.assertNotEqual(ShipOutcome.SUCCESS, second.outcome)
        self.assertFalse(journal.published())
        # Reloaded journal still not complete for publish.
        journal2 = ShipJournal.open_for_version(self.config.journal_dir, "0.9.15")
        self.assertFalse(journal2.is_complete(PHASE_PUBLISH))

    def test_mock_hardware_cannot_authorize_publication(self) -> None:
        harness = HardwareAcceptanceHarness(required_device_id="qa-kindle-1")
        report = harness.run(
            signed_root=self.tmp,
            public_key_hex="ab" * 32,
            version="0.9.15",
            probe=MockHardwareProbe(pass_all=True),
        )
        self.assertTrue(report.passed)
        self.assertFalse(report.production_evidence)
        self.assertFalse(report.authorizes_publication)

        # Orchestrator live path with mock probe must block.
        cfg = ShipConfig(
            repo_root=self.repo,
            ship_home=self.home,
            recovery_dir=self.recovery,
            recovery_key_dir=self.key_dir,
            allow_insecure_store=True,
            allow_live=True,
            require_hardware=True,
            hardware_device_id="qa-kindle-1",
        )
        # Pre-complete prior phases via local test mutations then hit hardware.
        hooks = ShipHooks(
            secret_store=self.store,
            execute_live_mutations=True,
            allow_local_mutations_for_tests=True,
            hardware_probe=MockHardwareProbe(pass_all=True),
        )
        orch = ShipOrchestrator(cfg, hooks=hooks)
        # Seed journal as if earlier phases completed enough to reach hardware.
        real_kindle = _REPO / "deployment" / "kindle"
        sys.path.insert(0, str(real_kindle))
        sys.path.insert(0, str(_REPO / "tools" / "release"))
        initialize_authority(cfg, store=self.store)
        provision_public_pin(cfg)
        apply_version(self.repo, "0.9.15")
        journal = ShipJournal.open_for_version(cfg.journal_dir, "0.9.15")
        for phase in (
            "authority-init",
            "public-pin-provision",
            "version-sync",
            "git-prepare",
            "git-push-pr",
            "git-merge",
            "ci-wait",
            "acquire",
            "sign",
            "verify-dry-run",
        ):
            journal.mark_completed(
                phase,
                identity={
                    "repository": cfg.repository,
                    "version": "0.9.15",
                    "tag": "v0.9.15",
                    "fingerprint_sha256": load_authority(cfg).fingerprint_sha256,
                    "main_sha": "a" * 40,
                    "source_revision": "a" * 40,
                    "ci_run_id": 1,
                    "scriptlet_sha256": "ab" * 32,
                    "scriptlet_bytes": 1,
                },
                main_sha="a" * 40,
                source_revision="a" * 40,
                ci_run_id=1,
                scriptlet_sha256="ab" * 32,
                scriptlet_bytes=1,
                acquired_dir=str(self.tmp / "acq"),
                signed_dir=str(self.tmp / "signed"),
                dry_run_outcome="complete-verification-pass",
                fingerprint_sha256=load_authority(cfg).fingerprint_sha256,
            )
        report = orch.ship("0.9.15", resume=True)
        self.assertEqual(ShipOutcome.BLOCKED_HARDWARE, report.outcome)
        self.assertIn("published=false", report.lines)
        self.assertFalse(
            ShipJournal.open_for_version(cfg.journal_dir, "0.9.15").published()
        )

    def test_hardware_unavailable_blocks(self) -> None:
        harness = HardwareAcceptanceHarness(required_device_id=None)
        with self.assertRaises(ShipBlockedError) as ctx:
            harness.run(
                signed_root=self.tmp,
                public_key_hex="ab" * 32,
                version="0.9.15",
            )
        self.assertIn("hardware-acceptance-unavailable", str(ctx.exception))

    def test_verified_publish_path_with_scripted_probe(self) -> None:
        """End-to-end success only when publish is independently verified."""
        cfg = ShipConfig(
            repo_root=self.repo,
            ship_home=self.home,
            recovery_dir=self.recovery,
            recovery_key_dir=self.key_dir,
            allow_insecure_store=True,
            allow_live=True,
            require_hardware=True,
            hardware_device_id="qa-kindle-1",
        )
        real_kindle = _REPO / "deployment" / "kindle"
        sys.path.insert(0, str(real_kindle))
        sys.path.insert(0, str(_REPO / "tools" / "release"))

        class FakeGH:
            def run(self, args: list[str], *, cwd: Path):
                del cwd
                stdout = ""
                if args[:2] == ["release", "view"]:
                    stdout = json.dumps(
                        {
                            "tagName": "v0.9.15",
                            "targetCommitish": "a" * 40,
                            "url": "https://github.com/markrai/ignite/releases/tag/v0.9.15",
                            "isDraft": False,
                            "isPrerelease": False,
                            "assets": [
                                {"name": f"asset-{i}.bin", "size": 1}
                                for i in range(9)
                            ],
                        }
                    )
                elif "git/ref/tags" in " ".join(args):
                    stdout = json.dumps({"sha": "a" * 40, "type": "commit"})
                elif args[:1] == ["api"] and "commits/" in " ".join(args):
                    stdout = "a" * 40
                return subprocess.CompletedProcess(args, 0, stdout, "")

        def fake_publish(*, journal: ShipJournal, gh: Any) -> dict[str, Any]:
            del journal, gh
            names = [f"asset-{i}.bin" for i in range(9)]
            digests = {n: "ab" * 32 for n in names}
            return {
                "executed": True,
                "independently_verified": True,
                "release_url": "https://github.com/markrai/ignite/releases/tag/v0.9.15",
                "asset_digests": digests,
                "asset_names": names,
            }

        @dataclass
        class _Ident:
            ci_head_sha: str
            scriptlet_sha256: str
            scriptlet_bytes: int

        @dataclass
        class _Bundle:
            root: Path
            identity: _Ident

        def fake_acquire(*, tag: str, output_dir: Path, run_id: int) -> _Bundle:
            del tag, run_id
            output_dir.mkdir(parents=True, exist_ok=True)
            (output_dir / "release-identity.v1").write_text(
                "ci_head_sha=" + ("a" * 40) + "\n"
                "scriptlet_sha256=" + ("cd" * 32) + "\n"
                "tag=v0.9.15\n",
                encoding="ascii",
            )
            return _Bundle(
                root=output_dir,
                identity=_Ident("a" * 40, "cd" * 32, 12),
            )

        class _Dry:
            outcome = type("O", (), {"value": "complete-verification-pass"})()

        hooks = ShipHooks(
            secret_store=self.store,
            execute_live_mutations=True,
            allow_local_mutations_for_tests=True,
            hardware_probe=ScriptedHardwareProbe(
                pass_all=True, production_evidence=True
            ),
            gh=FakeGH(),
            acquire_fn=fake_acquire,
            dry_run_fn=lambda **kwargs: _Dry(),
            publish_fn=fake_publish,
        )
        # Avoid real RestrictedSigningService by pre-completing sign via hooks:
        # use a thin wrapper that marks acquire then we stub sign by completing
        # journal mid-flight — instead, patch sign by providing acquired and
        # monkeypatching phase through publish_fn only after seeding.
        orch = ShipOrchestrator(cfg, hooks=hooks)

        # Seed authority/pin/version via local mutation path first piece-wise.
        initialize_authority(cfg, store=self.store)
        provision_public_pin(cfg)
        apply_version(self.repo, "0.9.15")
        journal = ShipJournal.open_for_version(cfg.journal_dir, "0.9.15")
        fp = load_authority(cfg).fingerprint_sha256
        for phase in (
            "authority-init",
            "public-pin-provision",
            "version-sync",
            "git-prepare",
            "git-push-pr",
            "git-merge",
            "ci-wait",
        ):
            journal.mark_completed(
                phase,
                identity={
                    "repository": cfg.repository,
                    "version": "0.9.15",
                    "tag": "v0.9.15",
                    "fingerprint_sha256": fp,
                    "main_sha": "a" * 40,
                    "source_revision": "a" * 40,
                    "ci_run_id": 42,
                },
                fingerprint_sha256=fp,
                main_sha="a" * 40,
                source_revision="a" * 40,
                ci_run_id=42,
                commit_sha="b" * 40,
                pr_number=7,
                changed_paths=["ignite/VERSION"],
                owned_paths=["ignite/VERSION", "tools/release/kindle-release-public-key.hex"],
            )

        # Provide a no-op signing service by completing acquire+sign through hooks:
        # acquire_fn works; for sign we need acquired dir + identity files that
        # satisfy policy — signing still needs real openssl ceremony. Stub sign
        # by completing PHASE_SIGN before ship resume after acquire.
        # Simpler: mark acquire/sign/verify complete then let hardware+publish run.
        acq = self.home / "work" / "acquired-v0.9.15"
        acq.mkdir(parents=True, exist_ok=True)
        (acq / "release-identity.v1").write_text(
            "ci_head_sha=" + ("a" * 40) + "\n"
            "scriptlet_sha256=" + ("cd" * 32) + "\n"
            "tag=v0.9.15\n",
            encoding="ascii",
        )
        signed = self.home / "work" / "signed-v0.9.15"
        signed.mkdir(parents=True, exist_ok=True)
        for phase in ("acquire", "sign", "verify-dry-run"):
            journal.mark_completed(
                phase,
                identity={
                    "repository": cfg.repository,
                    "version": "0.9.15",
                    "tag": "v0.9.15",
                    "fingerprint_sha256": fp,
                    "main_sha": "a" * 40,
                    "source_revision": "a" * 40,
                    "ci_run_id": 42,
                    "scriptlet_sha256": "cd" * 32,
                    "scriptlet_bytes": 12,
                },
                acquired_dir=str(acq),
                signed_dir=str(signed),
                scriptlet_sha256="cd" * 32,
                scriptlet_bytes=12,
                source_revision="a" * 40,
                main_sha="a" * 40,
                ci_run_id=42,
                dry_run_outcome="complete-verification-pass",
                fingerprint_sha256=fp,
            )

        report = orch.ship("0.9.15", resume=True)
        self.assertEqual(ShipOutcome.SUCCESS, report.outcome)
        self.assertIn("published=true", report.lines)
        self.assertTrue(
            ShipJournal.open_for_version(cfg.journal_dir, "0.9.15").published()
        )

    def test_failed_publish_cannot_record_success(self) -> None:
        cfg = ShipConfig(
            repo_root=self.repo,
            ship_home=self.home,
            recovery_dir=self.recovery,
            recovery_key_dir=self.key_dir,
            allow_insecure_store=True,
            allow_live=True,
            hardware_device_id="qa-kindle-1",
        )
        real_kindle = _REPO / "deployment" / "kindle"
        sys.path.insert(0, str(real_kindle))
        sys.path.insert(0, str(_REPO / "tools" / "release"))
        initialize_authority(cfg, store=self.store)
        provision_public_pin(cfg)
        journal = ShipJournal.open_for_version(cfg.journal_dir, "0.9.15")
        fp = load_authority(cfg).fingerprint_sha256
        for phase in (
            "authority-init",
            "public-pin-provision",
            "version-sync",
            "git-prepare",
            "git-push-pr",
            "git-merge",
            "ci-wait",
            "acquire",
            "sign",
            "verify-dry-run",
            "hardware-acceptance",
        ):
            journal.mark_completed(
                phase,
                identity={
                    "repository": cfg.repository,
                    "version": "0.9.15",
                    "tag": "v0.9.15",
                    "fingerprint_sha256": fp,
                    "main_sha": "a" * 40,
                    "source_revision": "a" * 40,
                    "ci_run_id": 1,
                    "scriptlet_sha256": "ab" * 32,
                    "scriptlet_bytes": 1,
                },
                fingerprint_sha256=fp,
                main_sha="a" * 40,
                source_revision="a" * 40,
                ci_run_id=1,
                scriptlet_sha256="ab" * 32,
                scriptlet_bytes=1,
                acquired_dir=str(self.tmp / "acq"),
                signed_dir=str(self.tmp / "signed"),
                dry_run_outcome="complete-verification-pass",
                production_evidence=True,
                device_id="qa-kindle-1",
            )

        def boom_publish(*, journal: ShipJournal, gh: Any) -> dict[str, Any]:
            del journal, gh
            raise ShipError("promote failed")

        hooks = ShipHooks(
            secret_store=self.store,
            execute_live_mutations=True,
            allow_local_mutations_for_tests=True,
            hardware_probe=ScriptedHardwareProbe(production_evidence=True),
            publish_fn=boom_publish,
            gh=object(),  # unused due to boom
        )
        # replace gh with minimal stub
        class FakeGH:
            def run(self, args, *, cwd):
                return subprocess.CompletedProcess(args, 1, "", "no release")

        hooks.gh = FakeGH()
        orch = ShipOrchestrator(cfg, hooks=hooks)
        report = orch.ship("0.9.15", resume=True)
        self.assertNotEqual(ShipOutcome.SUCCESS, report.outcome)
        self.assertIn("published=false", report.lines)
        self.assertFalse(
            ShipJournal.open_for_version(cfg.journal_dir, "0.9.15").published()
        )

    def test_cli_status_doctor_plan(self) -> None:
        env = os.environ.copy()
        env["IGNITE_SHIP_HOME"] = str(self.home)
        env["IGNITE_SHIP_RECOVERY_DIR"] = str(self.recovery)
        env["IGNITE_SHIP_RECOVERY_KEY_DIR"] = str(self.key_dir)
        env["IGNITE_SHIP_ALLOW_INSECURE_STORE"] = "1"
        script = _RELEASE / "ignite_ship.py"
        status = subprocess.run(
            [sys.executable, str(script), "--repo-root", str(self.repo), "status"],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(0, status.returncode, status.stdout + status.stderr)
        self.assertIn("authority=absent", status.stdout)
        self.assertIn("NOT independent recovery", status.stdout)
        plan = subprocess.run(
            [
                sys.executable,
                str(script),
                "--repo-root",
                str(self.repo),
                "plan",
                "--version",
                "0.9.15",
            ],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(0, plan.returncode, plan.stdout + plan.stderr)
        self.assertIn("ship_outcome=planned", plan.stdout)
        self.assertFalse(authority_present(self.config))
        self.assertEqual(
            b"UNPROVISIONED\n",
            (
                self.repo / "tools" / "release" / "kindle-release-public-key.hex"
            ).read_bytes(),
        )

    def test_cli_init_authority_requires_recovery_domains(self) -> None:
        env = os.environ.copy()
        env["IGNITE_SHIP_HOME"] = str(self.home)
        env.pop("IGNITE_SHIP_RECOVERY_DIR", None)
        env.pop("IGNITE_SHIP_RECOVERY_KEY_DIR", None)
        env["IGNITE_SHIP_ALLOW_INSECURE_STORE"] = "1"
        script = _RELEASE / "ignite_ship.py"
        result = subprocess.run(
            [
                sys.executable,
                str(script),
                "--repo-root",
                str(self.repo),
                "init-authority",
            ],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(0, result.returncode, result.stdout + result.stderr)
        self.assertIn("ship_outcome=failed", result.stdout)
        self.assertFalse(authority_present(self.config))
        pin_result = subprocess.run(
            [
                sys.executable,
                str(script),
                "--repo-root",
                str(self.repo),
                "provision-pin",
            ],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(0, pin_result.returncode)
        self.assertEqual(
            b"UNPROVISIONED\n",
            (
                self.repo / "tools" / "release" / "kindle-release-public-key.hex"
            ).read_bytes(),
        )

    def test_version_validation(self) -> None:
        validate_version_string("0.9.15")
        with self.assertRaises(ShipError):
            validate_version_string("v0.9.15")

    def test_apply_version_on_mini_tree(self) -> None:
        changed = apply_version(self.repo, "0.9.15")
        self.assertIn("ignite/VERSION", changed)
        self.assertEqual(
            "0.9.15", (self.repo / "ignite" / "VERSION").read_text().strip()
        )


class ShipSigningPolicyTests(unittest.TestCase):
    def test_sign_refuses_unprovisioned_pin(self) -> None:
        tmp = Path(tempfile.mkdtemp(prefix="ignite-ship-sign-"))
        try:
            repo = tmp / "repo"
            _init_mini_repo(repo)
            cfg = ShipConfig(
                repo_root=repo,
                ship_home=tmp / "home",
                recovery_dir=tmp / "recovery",
                recovery_key_dir=tmp / "recovery-keys",
                allow_insecure_store=True,
            )
            store = InsecureFileStore(allow_insecure=True)
            if not _openssl_available():
                self.skipTest("openssl required")
            initialize_authority(cfg, store=store)
            # Pin still UNPROVISIONED.
            service = RestrictedSigningService(cfg, store=store, production=False)
            req = SignReleaseRequest(
                tag="v0.9.15",
                version="0.9.15",
                source_revision="a" * 40,
                repository="markrai/ignite",
                workflow=".github/workflows/ci.yml",
                scriptlet_sha256="ab" * 32,
                scriptlet_bytes=10,
                acquired_root=tmp / "missing",
                output_dir=tmp / "out",
                expected_main_sha="a" * 40,
                ci_run_id=1,
            )
            with self.assertRaises(ShipError) as ctx:
                service.sign_release_candidate(req)
            self.assertIn("UNPROVISIONED", str(ctx.exception))
        finally:
            shutil.rmtree(tmp, ignore_errors=True)

    def test_sign_refuses_source_sha_mismatch(self) -> None:
        tmp = Path(tempfile.mkdtemp(prefix="ignite-ship-sign2-"))
        try:
            if not _openssl_available():
                self.skipTest("openssl required")
            repo = tmp / "repo"
            _init_mini_repo(repo)
            cfg = ShipConfig(
                repo_root=repo,
                ship_home=tmp / "home",
                recovery_dir=tmp / "recovery",
                recovery_key_dir=tmp / "recovery-keys",
                allow_insecure_store=True,
            )
            store = InsecureFileStore(allow_insecure=True)
            auth = initialize_authority(cfg, store=store)
            real_kindle = _REPO / "deployment" / "kindle"
            sys.path.insert(0, str(real_kindle))
            sys.path.insert(0, str(_REPO / "tools" / "release"))
            provision_public_pin(cfg, authority=auth)
            acq = tmp / "acq"
            acq.mkdir()
            (acq / "release-identity.v1").write_text(
                "ci_head_sha=" + ("b" * 40) + "\n"
                "scriptlet_sha256=" + ("ab" * 32) + "\n"
                "tag=v0.9.15\n",
                encoding="ascii",
            )
            (acq / "ci-artifacts").mkdir()
            scriptlet = acq / "ci-artifacts" / "Install Ignite v0.9.15.sh"
            scriptlet.write_bytes(b"x" * 10)
            digest = __import__("hashlib").sha256(b"x" * 10).hexdigest()
            (acq / "release-identity.v1").write_text(
                "ci_head_sha=" + ("b" * 40) + "\n"
                f"scriptlet_sha256={digest}\n"
                "tag=v0.9.15\n",
                encoding="ascii",
            )
            service = RestrictedSigningService(cfg, store=store)
            req = SignReleaseRequest(
                tag="v0.9.15",
                version="0.9.15",
                source_revision="b" * 40,
                repository="markrai/ignite",
                workflow=".github/workflows/ci.yml",
                scriptlet_sha256=digest,
                scriptlet_bytes=10,
                acquired_root=acq,
                output_dir=tmp / "out",
                expected_main_sha="a" * 40,  # mismatch vs source
                ci_run_id=1,
            )
            with self.assertRaises(ShipError) as ctx:
                service.sign_release_candidate(req)
            self.assertIn("does not match merged main", str(ctx.exception))
        finally:
            shutil.rmtree(tmp, ignore_errors=True)


class GitCoordMockTests(unittest.TestCase):
    def test_commit_and_pr_via_mocked_executables(self) -> None:
        calls: list[list[str]] = []

        class FakeGit:
            def run(self, args, *, cwd):
                calls.append(["git", *args])
                stdout = ""
                if args[:2] == ["rev-parse", "--abbrev-ref"]:
                    stdout = "feat/kindle-auto-updater\n"
                elif args[:2] == ["rev-parse", "HEAD"]:
                    stdout = "c" * 40 + "\n"
                elif args[:2] == ["status", "--porcelain"]:
                    stdout = (
                        " M ignite/VERSION\n"
                        " M tools/release/kindle-release-public-key.hex\n"
                    )
                elif args[:1] == ["add"]:
                    stdout = ""
                elif args[:1] == ["commit"]:
                    stdout = "[feat 1] ok\n"
                elif args[:1] == ["show"] and args[1].endswith("VERSION"):
                    stdout = "0.9.15\n"
                elif args[:1] == ["show"] and "kindle-release-public-key.hex" in args[1]:
                    stdout = "ab" * 32 + "\n"
                elif args[:1] == ["show"]:
                    return subprocess.CompletedProcess(args, 1, "", "missing")
                elif args[:2] == ["rev-parse", "origin/main"]:
                    stdout = "d" * 40 + "\n"
                elif args[:1] == ["fetch"]:
                    stdout = ""
                else:
                    stdout = ""
                return subprocess.CompletedProcess(args, 0, stdout, "")

        class FakeGH:
            def __init__(self) -> None:
                self.merged = False

            def run(self, args, *, cwd):
                calls.append(["gh", *args])
                if args[:2] == ["pr", "list"]:
                    return subprocess.CompletedProcess(args, 0, "[]", "")
                if args[:2] == ["pr", "create"]:
                    return subprocess.CompletedProcess(
                        args,
                        0,
                        "https://github.com/markrai/ignite/pull/9\n",
                        "",
                    )
                if args[:2] == ["pr", "view"]:
                    if self.merged:
                        payload = {
                            "number": 9,
                            "url": "https://github.com/markrai/ignite/pull/9",
                            "headRefName": "feat/kindle-auto-updater",
                            "baseRefName": "main",
                            "headRefOid": "c" * 40,
                            "state": "MERGED",
                            "mergedAt": "2026-01-01T00:00:00Z",
                            "mergeCommit": {"oid": "d" * 40},
                        }
                    else:
                        payload = {
                            "number": 9,
                            "url": "https://github.com/markrai/ignite/pull/9",
                            "headRefName": "feat/kindle-auto-updater",
                            "baseRefName": "main",
                            "headRefOid": "c" * 40,
                            "state": "OPEN",
                            "mergedAt": None,
                            "mergeCommit": None,
                        }
                    return subprocess.CompletedProcess(
                        args, 0, json.dumps(payload), ""
                    )
                if args[:2] == ["pr", "merge"]:
                    self.merged = True
                    return subprocess.CompletedProcess(args, 0, "merged\n", "")
                if args[:1] == ["api"]:
                    return subprocess.CompletedProcess(args, 0, "d" * 40, "")
                return subprocess.CompletedProcess(args, 0, "", "")

        tmp = Path(tempfile.mkdtemp(prefix="ignite-ship-git-"))
        try:
            result = commit_release_preparation(
                repo_root=tmp,
                version="0.9.15",
                paths=["ignite/VERSION"],
                pin_provisioned=True,
                expected_public_key_hex="ab" * 32,
                git=FakeGit(),
            )
            self.assertTrue(result["executed"])
            self.assertEqual("c" * 40, result["commit_sha"])
            self.assertIn(
                "tools/release/kindle-release-public-key.hex", result["paths"]
            )
            gh = FakeGH()
            git = FakeGit()
            pr = create_or_update_pr(
                repo_root=tmp,
                title="release: Ignite 0.9.15",
                head="feat/kindle-auto-updater",
                base="main",
                body="body",
                expected_head_sha="c" * 40,
                gh=gh,
            )
            self.assertEqual(9, pr["pr_number"])
            merged = merge_pr(
                repo_root=tmp,
                pr_number=9,
                gh=gh,
                git=git,
                expected_head_sha="c" * 40,
            )
            self.assertTrue(merged["executed"])
            self.assertTrue(merged["merged"])
            self.assertEqual("d" * 40, merged["main_sha"])
        finally:
            shutil.rmtree(tmp, ignore_errors=True)


class StateBHelperSafetyTests(unittest.TestCase):
    def test_helper_refuses_real_repo_as_worktree(self) -> None:
        helper = _REPO / "deployment" / "kindle" / "_run_stateb_provisioned_copy.py"
        if not helper.is_file():
            self.skipTest("stateb helper absent")
        import importlib.util

        spec = importlib.util.spec_from_file_location("stateb_helper", helper)
        assert spec and spec.loader
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        rc = mod.main_from_args([str(_REPO)]) if hasattr(mod, "main_from_args") else None
        if rc is None:
            if hasattr(mod, "assert_not_real_checkout"):
                with self.assertRaises(ValueError):
                    mod.assert_not_real_checkout(_REPO, _REPO)
            else:
                self.skipTest("helper guard not yet exposed")
        else:
            self.assertNotEqual(0, rc)


if __name__ == "__main__":
    unittest.main()


class FinalGateCorrectionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="ignite-ship-gate-"))
        self.repo = self.tmp / "repo"
        self.home = self.tmp / "ship-home"
        self.recovery = self.tmp / "recovery-backup"
        self.key_dir = self.tmp / "recovery-keys"
        _init_mini_repo(self.repo)

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_release_commit_paths_include_pin_and_version(self) -> None:
        paths = release_commit_paths(
            changed_paths=["ignite/VERSION", "Cargo.toml"],
            pin_provisioned=True,
        )
        self.assertIn("tools/release/kindle-release-public-key.hex", paths)
        self.assertIn("ignite/VERSION", paths)

    def test_verify_release_tree_requires_pin(self) -> None:
        class FakeGit:
            def __init__(self) -> None:
                self.pin = "ab" * 32 + "\n"
                self.version = "0.9.15\n"

            def run(self, args, *, cwd):
                if args[:1] == ["show"] and args[1].endswith("VERSION"):
                    return subprocess.CompletedProcess(args, 0, self.version, "")
                if args[:1] == ["show"] and "kindle-release-public-key.hex" in args[1]:
                    return subprocess.CompletedProcess(args, 0, self.pin, "")
                if args[:1] == ["show"]:
                    return subprocess.CompletedProcess(args, 1, "", "missing")
                return subprocess.CompletedProcess(args, 0, "", "")

        git = FakeGit()
        verify_release_tree(
            repo_root=self.repo,
            commit_sha="c" * 40,
            version="0.9.15",
            expected_public_key_hex="ab" * 32,
            require_pin=True,
            git=git,
        )
        git.pin = "UNPROVISIONED\n"
        with self.assertRaises(ShipError):
            verify_release_tree(
                repo_root=self.repo,
                commit_sha="c" * 40,
                version="0.9.15",
                expected_public_key_hex="ab" * 32,
                require_pin=True,
                git=git,
            )

    def test_publish_verify_requires_remote_digest_match(self) -> None:
        names = canonical_nine_file_names(
            version="0.9.15",
            linux_bundle_name="ignite-server-0.9.15-linux-x86_64.tar.gz",
        )
        local = self.tmp / "local-assets"
        local.mkdir()
        digests = {}
        for name in names:
            data = f"body-{name}".encode()
            (local / name).write_bytes(data)
            digests[name] = __import__("hashlib").sha256(data).hexdigest()

        class FakeGH:
            def __init__(self, mutate: bool = False) -> None:
                self.mutate = mutate

            def run(self, args, *, cwd):
                if args[:2] == ["release", "view"]:
                    assets = [
                        {"name": n, "size": (local / n).stat().st_size, "id": i + 1}
                        for i, n in enumerate(names)
                    ]
                    body = json.dumps(
                        {
                            "tagName": "v0.9.15",
                            "url": "https://github.com/markrai/ignite/releases/tag/v0.9.15",
                            "isDraft": False,
                            "isPrerelease": False,
                            "assets": assets,
                        }
                    )
                    return subprocess.CompletedProcess(args, 0, body, "")
                if args[:1] == ["api"] and "git/ref/tags" in " ".join(args):
                    return subprocess.CompletedProcess(
                        args, 0, json.dumps({"sha": "a" * 40, "type": "commit"}), ""
                    )
                if args[:2] == ["release", "download"]:
                    dest = Path(args[args.index("-D") + 1])
                    dest.mkdir(parents=True, exist_ok=True)
                    for name in names:
                        data = (local / name).read_bytes()
                        if self.mutate:
                            data = data + b"-tampered"
                        (dest / name).write_bytes(data)
                    return subprocess.CompletedProcess(args, 0, "", "")
                return subprocess.CompletedProcess(args, 0, "", "")

        ok = verify_published_release(
            repo_root=self.repo,
            tag="v0.9.15",
            expected_commit="a" * 40,
            gh=FakeGH(mutate=False),
            expected_asset_names=names,
            expected_digests=digests,
            local_asset_dir=local,
        )
        self.assertTrue(ok["verified"])
        with self.assertRaises(ShipError):
            verify_published_release(
                repo_root=self.repo,
                tag="v0.9.15",
                expected_commit="a" * 40,
                gh=FakeGH(mutate=True),
                expected_asset_names=names,
                expected_digests=digests,
                local_asset_dir=local,
            )

    def test_wait_for_pr_checks_polls_pending(self) -> None:
        calls = {"n": 0}

        class FakeGH:
            def run(self, args, *, cwd):
                calls["n"] += 1
                if calls["n"] == 1:
                    checks = [{"name": "ci", "state": "IN_PROGRESS", "bucket": "pending"}]
                else:
                    checks = [{"name": "ci", "state": "SUCCESS", "bucket": "pass"}]
                return subprocess.CompletedProcess(
                    args, 0, json.dumps(checks), ""
                )

        sleeps: list[float] = []
        result = wait_for_pr_checks(
            repo_root=self.repo,
            pr_number=3,
            gh=FakeGH(),
            poll_seconds=0,
            timeout_seconds=10,
            sleep=lambda s: sleeps.append(s),
        )
        self.assertEqual([], result["pending"])
        self.assertGreaterEqual(calls["n"], 2)

    def test_create_pr_resolves_number_without_view_head(self) -> None:
        seen: list[list[str]] = []

        class FakeGH:
            def run(self, args, *, cwd):
                seen.append(list(args))
                if args[:2] == ["pr", "list"]:
                    return subprocess.CompletedProcess(args, 0, "[]", "")
                if args[:2] == ["pr", "create"]:
                    return subprocess.CompletedProcess(
                        args,
                        0,
                        "https://github.com/markrai/ignite/pull/42\n",
                        "",
                    )
                if args[:2] == ["pr", "view"]:
                    assert args[2] == "42"
                    assert "--head" not in args
                    return subprocess.CompletedProcess(
                        args,
                        0,
                        json.dumps(
                            {
                                "number": 42,
                                "url": "https://github.com/markrai/ignite/pull/42",
                                "headRefName": "feat/kindle-auto-updater",
                                "baseRefName": "main",
                                "headRefOid": "c" * 40,
                                "state": "OPEN",
                            }
                        ),
                        "",
                    )
                return subprocess.CompletedProcess(args, 0, "", "")

        pr = create_or_update_pr(
            repo_root=self.repo,
            title="t",
            head="feat/kindle-auto-updater",
            base="main",
            body="b",
            expected_head_sha="c" * 40,
            gh=FakeGH(),
        )
        self.assertEqual(42, pr["pr_number"])
        self.assertTrue(all("--head" not in a[2:] or a[:2] != ["pr", "view"] for a in seen))

    def test_merge_requires_confirmed_merged_state(self) -> None:
        class FakeGH:
            def __init__(self) -> None:
                self.n = 0

            def run(self, args, *, cwd):
                if args[:2] == ["pr", "view"] and "mergedAt" in "".join(args):
                    self.n += 1
                    if self.n == 1:
                        # preflight open
                        return subprocess.CompletedProcess(
                            args,
                            0,
                            json.dumps(
                                {
                                    "state": "OPEN",
                                    "mergedAt": None,
                                    "mergeCommit": None,
                                    "headRefOid": "c" * 40,
                                }
                            ),
                            "",
                        )
                    # post-merge still not confirmed
                    return subprocess.CompletedProcess(
                        args,
                        0,
                        json.dumps(
                            {
                                "state": "OPEN",
                                "mergedAt": None,
                                "mergeCommit": None,
                                "headRefOid": "c" * 40,
                            }
                        ),
                        "",
                    )
                if args[:2] == ["pr", "merge"]:
                    return subprocess.CompletedProcess(args, 0, "ok\n", "")
                return subprocess.CompletedProcess(args, 0, "", "")

        class FakeGit:
            def run(self, args, *, cwd):
                return subprocess.CompletedProcess(args, 0, "d" * 40 + "\n", "")

        # Force confirm_pr_merged to fail quickly by returning never-merged views.
        import ship_lib.git_coord as gc

        original = gc.confirm_pr_merged

        def boom(**kwargs):
            raise ShipError("PR #9 merge command returned success but merged state was not confirmed")

        gc.confirm_pr_merged = boom  # type: ignore[assignment]
        try:
            with self.assertRaises(ShipError) as ctx:
                merge_pr(
                    repo_root=self.repo,
                    pr_number=9,
                    gh=FakeGH(),
                    git=FakeGit(),
                    expected_head_sha="c" * 40,
                )
            self.assertIn("merged state", str(ctx.exception).lower())
        finally:
            gc.confirm_pr_merged = original  # type: ignore[assignment]



class PrepareFromCiTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="ignite-prepare-ci-"))
        self.repo = self.tmp / "repo"
        self.home = self.tmp / "ship-home"
        self.recovery = self.tmp / "recovery"
        self.key_dir = self.tmp / "recovery-key"
        _init_mini_repo(self.repo, version="0.9.16")
        # Pin validation loads trust-boundary + release sign helpers from the mini tree.
        shutil.copy(
            _REPO / "deployment" / "kindle" / "production_trust_boundary.py",
            self.repo / "deployment" / "kindle" / "production_trust_boundary.py",
        )
        shutil.copy(
            _REPO / "tools" / "release" / "kindle_release_sign.py",
            self.repo / "tools" / "release" / "kindle_release_sign.py",
        )
        self.store = InsecureFileStore(self.home / "secrets")
        self.cfg = ShipConfig(
            repo_root=self.repo,
            ship_home=self.home,
            recovery_dir=self.recovery,
            recovery_key_dir=self.key_dir,
            allow_insecure_store=True,
            allow_live=True,
            require_hardware=True,
            hardware_device_id="qa-kindle-1",
        )
        self.home.mkdir(parents=True)
        self.recovery.mkdir(parents=True)
        self.key_dir.mkdir(parents=True)
        initialize_authority(self.cfg, store=self.store)
        provision_public_pin(self.cfg)
        self.commit = "5c01d29b41f86c94e10f5b43eef222c7f17d30b6"
        self.run_id = 41705371651

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    DESIGNATED_WORKFLOW_ID = 424242

    def _gh_for_bind(
        self,
        *,
        head_sha: str | None = None,
        conclusion: str = "success",
        path: str = ".github/workflows/ci.yml",
        workflow_id: int | None = None,
        designated_workflow_id: int | None = None,
        ahead_by: int = 0,
        version_at_commit: str = "0.9.16",
    ):
        sha = head_sha or self.commit
        run_id = self.run_id
        run_workflow_id = (
            self.DESIGNATED_WORKFLOW_ID if workflow_id is None else workflow_id
        )
        want_workflow_id = (
            self.DESIGNATED_WORKFLOW_ID
            if designated_workflow_id is None
            else designated_workflow_id
        )

        class FakeGH:
            def run(self, args: list[str], *, cwd: Path):
                del cwd
                joined = " ".join(args)
                if args[:1] == ["api"] and "actions/workflows/ci.yml" in joined:
                    return subprocess.CompletedProcess(
                        args,
                        0,
                        json.dumps({"id": want_workflow_id, "path": "ci.yml"}),
                        "",
                    )
                if args[:1] == ["api"] and f"actions/runs/{run_id}" in joined:
                    return subprocess.CompletedProcess(
                        args,
                        0,
                        json.dumps(
                            {
                                "id": run_id,
                                "status": "completed",
                                "conclusion": conclusion,
                                "head_sha": sha,
                                "head_branch": "main",
                                "event": "push",
                                "path": path,
                                "workflow_id": run_workflow_id,
                                "html_url": "https://example.test/run",
                            }
                        ),
                        "",
                    )
                if args[:1] == ["api"] and "compare/main..." in joined:
                    if "--jq" in args and args[args.index("--jq") + 1] == ".ahead_by":
                        return subprocess.CompletedProcess(
                            args, 0, f"{ahead_by}\n", ""
                        )
                    return subprocess.CompletedProcess(
                        args, 0, json.dumps({"ahead_by": ahead_by}), ""
                    )
                if "contents/ignite/VERSION" in joined:
                    if "application/vnd.github.raw" in joined:
                        return subprocess.CompletedProcess(
                            args, 0, version_at_commit + "\n", ""
                        )
                    import base64

                    body = base64.b64encode(
                        (version_at_commit + "\n").encode("utf-8")
                    ).decode("ascii")
                    return subprocess.CompletedProcess(args, 0, body + "\n", "")
                return subprocess.CompletedProcess(args, 1, "", "unexpected")

        return FakeGH()

    def test_bind_existing_ci_run_accepts_matching_success(self) -> None:
        from ship_lib.ci_coord import bind_existing_ci_run

        bound = bind_existing_ci_run(
            repo_root=self.repo,
            version="0.9.16",
            run_id=self.run_id,
            commit=self.commit,
            gh=self._gh_for_bind(),
            repository=self.cfg.repository,
        )
        self.assertEqual(bound.run_id, self.run_id)
        self.assertEqual(bound.head_sha, self.commit)
        self.assertEqual(bound.workflow_id, self.DESIGNATED_WORKFLOW_ID)
        self.assertEqual(bound.workflow_path, ".github/workflows/ci.yml")

    def test_bind_existing_ci_run_rejects_sha_mismatch(self) -> None:
        from ship_lib.ci_coord import bind_existing_ci_run

        with self.assertRaises(ShipBlockedError) as ctx:
            bind_existing_ci_run(
                repo_root=self.repo,
                version="0.9.16",
                run_id=self.run_id,
                commit=self.commit,
                gh=self._gh_for_bind(head_sha="a" * 40),
                repository=self.cfg.repository,
            )
        self.assertIn("does not match commit", str(ctx.exception))

    def test_bind_existing_ci_run_rejects_wrong_workflow_path(self) -> None:
        from ship_lib.ci_coord import bind_existing_ci_run

        with self.assertRaises(ShipBlockedError) as ctx:
            bind_existing_ci_run(
                repo_root=self.repo,
                version="0.9.16",
                run_id=self.run_id,
                commit=self.commit,
                gh=self._gh_for_bind(path="tools/release/ci.yml"),
                repository=self.cfg.repository,
            )
        self.assertIn("not the designated CI workflow", str(ctx.exception))
        self.assertIn("path", str(ctx.exception))

    def test_bind_existing_ci_run_rejects_wrong_workflow_id(self) -> None:
        from ship_lib.ci_coord import bind_existing_ci_run

        with self.assertRaises(ShipBlockedError) as ctx:
            bind_existing_ci_run(
                repo_root=self.repo,
                version="0.9.16",
                run_id=self.run_id,
                commit=self.commit,
                gh=self._gh_for_bind(workflow_id=999001),
                repository=self.cfg.repository,
            )
        self.assertIn("not the designated CI workflow", str(ctx.exception))
        self.assertIn("workflow_id", str(ctx.exception))

    def test_bind_existing_ci_run_rejects_commit_not_on_main(self) -> None:
        from ship_lib.ci_coord import bind_existing_ci_run

        with self.assertRaises(ShipBlockedError) as ctx:
            bind_existing_ci_run(
                repo_root=self.repo,
                version="0.9.16",
                run_id=self.run_id,
                commit=self.commit,
                gh=self._gh_for_bind(ahead_by=1),
                repository=self.cfg.repository,
            )
        self.assertIn("not on main", str(ctx.exception))
        self.assertIn("ahead_by", str(ctx.exception))

    def test_prepare_from_ci_never_publishes_without_execute_live(self) -> None:
        hooks = ShipHooks(
            secret_store=self.store,
            execute_live_mutations=False,
            gh=self._gh_for_bind(),
            publish_fn=lambda **kwargs: (_ for _ in ()).throw(
                AssertionError("publish must not run during prepare")
            ),
        )
        orch = ShipOrchestrator(self.cfg, hooks=hooks)
        report = orch.prepare_from_ci(
            "0.9.16",
            run_id=self.run_id,
            commit=self.commit,
            plan_only=True,
        )
        self.assertEqual(report.outcome, ShipOutcome.PLANNED)
        self.assertIn("published=false", report.lines)
        self.assertIn("publish=forbidden-in-prepare-from-ci", report.lines)
        self.assertIn("journal=unmodified-non-live", report.lines)
        journal_path = self.cfg.journal_dir / "ship-0.9.16.journal.v2.json"
        self.assertFalse(
            journal_path.exists(),
            "plan/non-live prepare-from-ci must not create a journal",
        )

    def test_prepare_from_ci_plan_mode_does_not_alter_existing_journal(self) -> None:
        journal = ShipJournal.open_for_version(self.cfg.journal_dir, "0.9.16")
        journal.mark_completed(
            "authority-init",
            note="pre-existing-snapshot",
            fingerprint_sha256="ab" * 32,
        )
        journal_path = journal.path
        before = journal_path.read_bytes()
        before_mtime = journal_path.stat().st_mtime_ns

        hooks = ShipHooks(
            secret_store=self.store,
            execute_live_mutations=False,
            gh=self._gh_for_bind(),
            publish_fn=lambda **kwargs: (_ for _ in ()).throw(
                AssertionError("publish must not run during prepare")
            ),
        )
        orch = ShipOrchestrator(self.cfg, hooks=hooks)
        report = orch.prepare_from_ci(
            "0.9.16",
            run_id=self.run_id,
            commit=self.commit,
            plan_only=True,
        )
        self.assertEqual(report.outcome, ShipOutcome.PLANNED)
        self.assertIn("journal=unmodified-non-live", report.lines)
        after = journal_path.read_bytes()
        self.assertEqual(before, after)
        self.assertEqual(before_mtime, journal_path.stat().st_mtime_ns)
        reloaded = ShipJournal.load(journal_path)
        self.assertTrue(reloaded.is_complete("authority-init"))
        self.assertFalse(reloaded.is_complete("ci-wait"))
        self.assertFalse(reloaded.is_complete("version-sync"))
        self.assertFalse(reloaded.published())

    def test_prepare_then_publish_prepared_uses_hardware_evidence(self) -> None:
        @dataclass
        class _Ident:
            ci_head_sha: str
            scriptlet_sha256: str
            scriptlet_bytes: int
            ci_run_id: int
            tag: str

        @dataclass
        class _Bundle:
            root: Path
            identity: _Ident

        def fake_acquire(*, tag: str, output_dir: Path, run_id: int) -> _Bundle:
            output_dir.mkdir(parents=True, exist_ok=True)
            return _Bundle(
                root=output_dir,
                identity=_Ident(
                    ci_head_sha=self.commit,
                    scriptlet_sha256="cd" * 32,
                    scriptlet_bytes=12,
                    ci_run_id=run_id,
                    tag=tag,
                ),
            )

        class _Dry:
            outcome = type("O", (), {"value": "complete-verification-pass"})()

        publish_calls: list[str] = []

        def fake_publish(*, journal: ShipJournal, gh: Any) -> dict[str, Any]:
            del gh
            publish_calls.append(journal.version)
            names = [f"asset-{i}.bin" for i in range(9)]
            return {
                "executed": True,
                "independently_verified": True,
                "release_url": "https://github.com/markrai/ignite/releases/tag/v0.9.16",
                "asset_digests": {n: "ab" * 32 for n in names},
                "asset_names": names,
            }

        # Sign without RestrictedSigningService: complete sign after acquire via hook seam.
        hooks = ShipHooks(
            secret_store=self.store,
            execute_live_mutations=True,
            allow_local_mutations_for_tests=True,
            gh=self._gh_for_bind(),
            hardware_probe=ScriptedHardwareProbe(
                pass_all=True, production_evidence=True
            ),
            acquire_fn=fake_acquire,
            dry_run_fn=lambda **kwargs: _Dry(),
            publish_fn=fake_publish,
        )
        orch = ShipOrchestrator(self.cfg, hooks=hooks)

        # Monkeypatch sign to complete without openssl ceremony.
        original_sign = ShipOrchestrator._phase_sign

        commit = self.commit
        run_id = self.run_id

        def stub_sign(orch_self, journal, report, version, tag, *, mutate):
            if journal.is_complete("sign"):
                report.emit("phase=sign:skipped-complete")
                return
            if not mutate:
                journal.mark_deferred("sign", reason="non-live")
                report.emit("phase=sign:deferred-non-live")
                return
            signed = orch_self.config.work_dir / f"signed-{tag}"
            signed.mkdir(parents=True, exist_ok=True)
            journal.mark_completed(
                "sign",
                identity={
                    "repository": orch_self.config.repository,
                    "version": version,
                    "tag": tag,
                    "main_sha": commit,
                    "source_revision": commit,
                    "ci_run_id": run_id,
                },
                signed_dir=str(signed),
                fingerprint_sha256=load_authority(orch_self.config).fingerprint_sha256,
                signed_scriptlet_sha256="cd" * 32,
            )
            report.emit("phase=sign:ok")

        ShipOrchestrator._phase_sign = stub_sign  # type: ignore[method-assign]
        try:
            prep = orch.prepare_from_ci(
                "0.9.16",
                run_id=self.run_id,
                commit=self.commit,
                plan_only=False,
            )
            self.assertEqual(prep.outcome, ShipOutcome.READY)
            self.assertIn("prepared_for_publish=true", prep.lines)
            self.assertIn("published=false", prep.lines)
            self.assertEqual(publish_calls, [])

            journal = ShipJournal.open_for_version(self.cfg.journal_dir, "0.9.16")
            self.assertTrue(journal.is_complete("hardware-acceptance"))
            self.assertTrue(
                journal.phases["hardware-acceptance"].fields.get("production_evidence")
            )

            pub = orch.publish_prepared("0.9.16", plan_only=False)
            self.assertEqual(pub.outcome, ShipOutcome.SUCCESS)
            self.assertIn("published=true", pub.lines)
            self.assertEqual(publish_calls, ["0.9.16"])
        finally:
            ShipOrchestrator._phase_sign = original_sign  # type: ignore[method-assign]

    def test_publish_prepared_refuses_without_hardware_evidence(self) -> None:
        journal = ShipJournal.open_for_version(self.cfg.journal_dir, "0.9.16")
        for phase in (
            "authority-init",
            "public-pin-provision",
            "version-sync",
            "git-prepare",
            "git-push-pr",
            "git-merge",
            "ci-wait",
            "acquire",
            "sign",
            "verify-dry-run",
            "hardware-acceptance",
        ):
            fields = {
                "main_sha": self.commit,
                "source_revision": self.commit,
                "ci_run_id": self.run_id,
                "acquired_dir": str(self.tmp / "acq"),
                "signed_dir": str(self.tmp / "sig"),
                "scriptlet_sha256": "cd" * 32,
                "scriptlet_bytes": 12,
            }
            if phase == "hardware-acceptance":
                fields["production_evidence"] = False
            journal.mark_completed(phase, **fields)
        hooks = ShipHooks(
            secret_store=self.store,
            execute_live_mutations=True,
            publish_fn=lambda **kwargs: (_ for _ in ()).throw(
                AssertionError("must not publish")
            ),
        )
        orch = ShipOrchestrator(self.cfg, hooks=hooks)
        report = orch.publish_prepared("0.9.16", plan_only=False)
        self.assertEqual(report.outcome, ShipOutcome.BLOCKED_HARDWARE)
        self.assertIn("published=false", report.lines)
        self.assertIn("hardware-acceptance-unavailable", "\n".join(report.lines))


class PublishWorkspaceIsolationTests(unittest.TestCase):
    """Regression: version-scoped promote work + original publish error preservation."""

    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="ignite-publish-iso-"))
        self.repo = self.tmp / "repo"
        self.home = self.tmp / "ship-home"
        self.recovery = self.tmp / "recovery"
        self.key_dir = self.tmp / "recovery-key"
        _init_mini_repo(self.repo, version="0.9.16")
        shutil.copy(
            _REPO / "deployment" / "kindle" / "production_trust_boundary.py",
            self.repo / "deployment" / "kindle" / "production_trust_boundary.py",
        )
        shutil.copy(
            _REPO / "tools" / "release" / "kindle_release_sign.py",
            self.repo / "tools" / "release" / "kindle_release_sign.py",
        )
        # Real promote shell path needs promote-release.sh under the mini repo.
        shutil.copy(
            _REPO / "tools" / "release" / "promote-release.sh",
            self.repo / "tools" / "release" / "promote-release.sh",
        )
        self.store = InsecureFileStore(self.home / "secrets")
        self.cfg = ShipConfig(
            repo_root=self.repo,
            ship_home=self.home,
            recovery_dir=self.recovery,
            recovery_key_dir=self.key_dir,
            allow_insecure_store=True,
            allow_live=True,
            require_hardware=True,
            hardware_device_id="qa-kindle-1",
        )
        self.home.mkdir(parents=True)
        self.recovery.mkdir(parents=True)
        self.key_dir.mkdir(parents=True)
        initialize_authority(self.cfg, store=self.store)
        provision_public_pin(self.cfg)
        self.commit = "5c01d29b41f86c94e10f5b43eef222c7f17d30b6"
        self.run_id = 41705371651

        import importlib.util

        helper = _REPO / "deployment" / "kindle" / "test_release_acquire.py"
        spec = importlib.util.spec_from_file_location(
            "ignite_test_release_acquire_iso", helper
        )
        assert spec and spec.loader
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        bundles = self.tmp / "bundles"
        bundles.mkdir(parents=True, exist_ok=True)
        _state, self.acquired, _root, _pub, self.signed = mod._signed_bundle_for_shell(
            bundles
        )
        self.commit = str(self.acquired.identity.ci_head_sha)
        self.run_id = int(self.acquired.identity.ci_run_id)
        self.version = str(self.acquired.identity.version)
        self.tag = f"v{self.version}"

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _prepare_journal(self) -> ShipJournal:
        journal = ShipJournal.open_for_version(self.cfg.journal_dir, self.version)
        for phase in (
            "authority-init",
            "public-pin-provision",
            "version-sync",
            "git-prepare",
            "git-push-pr",
            "git-merge",
            "ci-wait",
            "acquire",
            "sign",
            "verify-dry-run",
            "hardware-acceptance",
        ):
            fields: dict[str, Any] = {
                "main_sha": self.commit,
                "source_revision": self.commit,
                "ci_run_id": self.run_id,
                "acquired_dir": str(self.acquired.root),
                "signed_dir": str(self.signed.root),
                "scriptlet_sha256": "cd" * 32,
                "scriptlet_bytes": 12,
            }
            if phase == "hardware-acceptance":
                fields["production_evidence"] = True
            journal.mark_completed(phase, **fields)
        return journal

    def test_phase_publish_uses_version_specific_workspace(self) -> None:
        import ship_lib.orchestrator as orch_mod
        from ship_lib.publish import promote_publish_workspace

        journal = self._prepare_journal()
        captured: dict[str, Path] = {}

        def capture_seed(*, acquired: Any, work_dir: Path) -> None:
            del acquired
            captured["work_dir"] = work_dir

        def fake_publish(**kwargs: Any) -> Any:
            del kwargs

            @dataclass
            class _Result:
                independently_verified: bool = True
                release_url: str = "https://example.test/release"
                asset_digests: dict[str, str] = field(default_factory=dict)
                asset_names: tuple[str, ...] = ()
                recovered_existing: bool = False
                executed: bool = True

            digests = {
                n: "ab" * 32
                for n in (
                    f"Install Ignite v{self.version}.sh",
                    f"Install Ignite v{self.version}.sha256",
                )
            }
            return _Result(
                asset_digests=digests,
                asset_names=tuple(digests),
            )

        original_seed = orch_mod.seed_promote_work_from_acquired
        original_publish = orch_mod.publish_release
        original_assemble = orch_mod.assemble_nine_file_inventory
        original_env = orch_mod.build_publish_env
        try:
            orch_mod.seed_promote_work_from_acquired = capture_seed  # type: ignore[assignment]

            def fake_assemble(*, acquired: Any, signed: Any, dest: Path):
                del signed
                dest.mkdir(parents=True, exist_ok=True)
                names = (
                    f"Install Ignite v{acquired.identity.version}.sh",
                    f"Install Ignite v{acquired.identity.version}.sha256",
                    f"ignite-kindle-v{acquired.identity.version}.manifest",
                    f"ignite-kindle-v{acquired.identity.version}.manifest.sig",
                    f"ignite-kindle-v{acquired.identity.version}.publication-binding",
                    f"ignite-kindle-v{acquired.identity.version}.publication-binding.sig",
                    f"ignite-kindle-v{acquired.identity.version}.publication-evidence",
                    acquired.identity.linux_bundle_name,
                    f"{acquired.identity.linux_bundle_name}.sha256",
                )
                digests = {}
                for name in names:
                    path = dest / name
                    path.write_bytes(b"x")
                    digests[name] = "ab" * 32
                from ship_lib.publish import NineFileInventory

                return NineFileInventory(
                    root=dest,
                    version=acquired.identity.version,
                    tag=f"v{acquired.identity.version}",
                    commit=self.commit,
                    run_id=self.run_id,
                    asset_names=tuple(sorted(names)),
                    asset_digests=digests,
                    linux_bundle_name=acquired.identity.linux_bundle_name,
                )

            orch_mod.assemble_nine_file_inventory = fake_assemble  # type: ignore[assignment]
            orch_mod.publish_release = fake_publish  # type: ignore[assignment]
            orch_mod.build_publish_env = lambda **kwargs: {  # type: ignore[assignment]
                "IGNITE_PROMOTE_TAG": self.tag,
                "IGNITE_PROMOTE_RUN_ID": str(self.run_id),
                "IGNITE_PROMOTE_COMMIT": self.commit,
            }

            hooks = ShipHooks(
                secret_store=self.store,
                execute_live_mutations=True,
                allow_local_mutations_for_tests=True,
                gh=type("GH", (), {"run": lambda *a, **k: None})(),
            )
            orch = ShipOrchestrator(self.cfg, hooks=hooks)
            report = ShipReport(
                outcome=ShipOutcome.PARTIAL, version=self.version, tag=self.tag
            )
            orch._phase_publish(journal, report, mutate=True)
            expected = promote_publish_workspace(self.cfg.work_dir, self.tag)
            self.assertEqual(captured.get("work_dir"), expected)
            self.assertIn(f"promote_work_dir={expected}", report.lines)
            self.assertNotEqual(
                captured["work_dir"], self.cfg.work_dir / "promote-publish"
            )
            # Prior shared workspace contamination must not be the publish target.
            shared = self.cfg.work_dir / "promote-publish"
            shared.mkdir(parents=True, exist_ok=True)
            (shared / "kindle").mkdir(exist_ok=True)
            (shared / "kindle" / "Install Ignite v0.9.15.sh").write_bytes(b"old")
            self.assertNotEqual(captured["work_dir"], shared)
        finally:
            orch_mod.seed_promote_work_from_acquired = original_seed  # type: ignore[assignment]
            orch_mod.publish_release = original_publish  # type: ignore[assignment]
            orch_mod.assemble_nine_file_inventory = original_assemble  # type: ignore[assignment]
            orch_mod.build_publish_env = original_env  # type: ignore[assignment]

    def test_phase_publish_preserves_original_promote_error(self) -> None:
        import ship_lib.orchestrator as orch_mod

        journal = self._prepare_journal()
        promote_msg = (
            "promote-release.sh publish failed: Kindle artifact file set mismatch: "
            "Install Ignite v0.9.15.sh"
        )
        recover_msg = "unable to view published release for verification: not found"

        def boom_publish(**kwargs: Any) -> Any:
            del kwargs
            raise ShipError(promote_msg)

        def boom_verify(**kwargs: Any) -> Any:
            del kwargs
            raise ShipError(recover_msg)

        def fake_assemble(*, acquired: Any, signed: Any, dest: Path):
            del signed
            dest.mkdir(parents=True, exist_ok=True)
            from ship_lib.publish import NineFileInventory

            names = (
                f"Install Ignite v{self.version}.sh",
                f"Install Ignite v{self.version}.sha256",
                f"ignite-kindle-v{self.version}.manifest",
                f"ignite-kindle-v{self.version}.manifest.sig",
                f"ignite-kindle-v{self.version}.publication-binding",
                f"ignite-kindle-v{self.version}.publication-binding.sig",
                f"ignite-kindle-v{self.version}.publication-evidence",
                acquired.identity.linux_bundle_name,
                f"{acquired.identity.linux_bundle_name}.sha256",
            )
            for name in names:
                (dest / name).write_bytes(b"x")
            return NineFileInventory(
                root=dest,
                version=self.version,
                tag=self.tag,
                commit=self.commit,
                run_id=self.run_id,
                asset_names=names,
                asset_digests={n: "ab" * 32 for n in names},
                linux_bundle_name=acquired.identity.linux_bundle_name,
            )

        original_publish = orch_mod.publish_release
        original_verify = orch_mod.verify_published_release
        original_assemble = orch_mod.assemble_nine_file_inventory
        original_seed = orch_mod.seed_promote_work_from_acquired
        original_env = orch_mod.build_publish_env
        try:
            orch_mod.publish_release = boom_publish  # type: ignore[assignment]
            orch_mod.verify_published_release = boom_verify  # type: ignore[assignment]
            orch_mod.assemble_nine_file_inventory = fake_assemble  # type: ignore[assignment]
            orch_mod.seed_promote_work_from_acquired = lambda **kwargs: None  # type: ignore[assignment]
            orch_mod.build_publish_env = lambda **kwargs: {  # type: ignore[assignment]
                "IGNITE_PROMOTE_TAG": self.tag,
                "IGNITE_PROMOTE_RUN_ID": str(self.run_id),
                "IGNITE_PROMOTE_COMMIT": self.commit,
            }
            hooks = ShipHooks(
                secret_store=self.store,
                execute_live_mutations=True,
                allow_local_mutations_for_tests=True,
                gh=type("GH", (), {"run": lambda *a, **k: None})(),
            )
            orch = ShipOrchestrator(self.cfg, hooks=hooks)
            report = ShipReport(
                outcome=ShipOutcome.PARTIAL, version=self.version, tag=self.tag
            )
            with self.assertRaises(ShipError) as ctx:
                orch._phase_publish(journal, report, mutate=True)
            msg = str(ctx.exception)
            self.assertIn("Kindle artifact file set mismatch", msg)
            self.assertIn("Install Ignite v0.9.15.sh", msg)
            self.assertIn("recovery verification also failed", msg)
            self.assertIn("unable to view published release", msg)
            phase = journal.phases[PHASE_PUBLISH]
            self.assertEqual(phase.state, PhaseState.FAILED)
            self.assertIn(
                "Kindle artifact file set mismatch", str(phase.fields.get("error"))
            )
        finally:
            orch_mod.publish_release = original_publish  # type: ignore[assignment]
            orch_mod.verify_published_release = original_verify  # type: ignore[assignment]
            orch_mod.assemble_nine_file_inventory = original_assemble  # type: ignore[assignment]
            orch_mod.seed_promote_work_from_acquired = original_seed  # type: ignore[assignment]
            orch_mod.build_publish_env = original_env  # type: ignore[assignment]


if __name__ == "__main__":
    unittest.main()
