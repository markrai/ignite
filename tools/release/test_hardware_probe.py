"""Unit tests for Kindle SSH hardware probe transport and evidence integrity."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from ship_lib.errors import ShipBlockedError, ShipError
from ship_lib.hardware import (
    HardwareAcceptanceHarness,
    KindleSshHardwareProbe,
    MockHardwareProbe,
    device_id_from_usid_sha256,
    parse_usid_sha256_from_device_id,
)

_USID_SHA = "1f62a2356af655489f9f8072e5bf3ad5ca2308ab73f7864b871df792b073f0d2"
_DEVICE = f"kindle-usid-sha256:{_USID_SHA}"


def _identity_block(
    *,
    usid: str = _USID_SHA,
    firmware: str = "Kindle 5.18.6 (~~otaVersion~~)",
    arch: str = "armv7l",
) -> str:
    return (
        f"usid_sha256={usid}\n"
        f"usid_prefix=G0922710\n"
        f"hostname=kindle\n"
        f"arch={arch}\n"
        f"prettyversion={firmware}\n"
        f"version_txt=System Software Version: 041-juno\n"
    )


def _inventory_block(
    *,
    updater_cli: str = "absent",
    unprov: int = 0,
    schema: int = 0,
    notebooks: int = 437,
    lib_hash: str = "3b78114bd3cb70447eba9fd2372802bad8710429fc69c94e9880a7018cb65f3d",
) -> str:
    return (
        "scribe_app_present=yes\n"
        "scribe_app_sha256=46dfb73906e107da548463a6e5526d31b3ac40aa8465d493b0d8763d88f0c9db\n"
        "scribe_app_size=7204604\n"
        "lifecycle_present=yes\n"
        f"updater_cli={updater_cli}\n"
        f"UNPROVISIONED_count={unprov}\n"
        "stage_cli_count=0\n"
        f"manifest_schema_count={schema}\n"
        "notebook_dirs=26\n"
        f"notebook_files={notebooks}\n"
        f"library_meta_sha256={lib_hash}\n"
        "library_meta_size=596\n"
        "prod_stage_exists=no\n"
        "install_last==== Install Ignite 0.9.13-diag19b3d3 2026-10-22T21:55:04Z ====\n"
        "ignite_proc=none_matched\n"
    )


class HardwareProbeUnitTests(unittest.TestCase):
    def test_device_id_roundtrip(self) -> None:
        self.assertEqual(_DEVICE, device_id_from_usid_sha256(_USID_SHA))
        self.assertEqual(_USID_SHA, parse_usid_sha256_from_device_id(_DEVICE))
        self.assertIsNone(parse_usid_sha256_from_device_id("qa-kindle-1"))

    def test_mock_never_production_evidence(self) -> None:
        report = MockHardwareProbe(pass_all=True).evaluate(
            device_id="qa-kindle-1", public_key_hex="ab" * 32
        )
        self.assertTrue(report.passed)
        self.assertFalse(report.production_evidence)
        self.assertFalse(report.authorizes_publication)

    def test_connection_failure(self) -> None:
        def boom(_script: str) -> str:
            raise ShipBlockedError("release-blocked: Kindle SSH failed rc=255")

        with tempfile.TemporaryDirectory() as tmp:
            probe = KindleSshHardwareProbe(
                ssh_host="kindle",
                evidence_dir=Path(tmp),
                run_script=boom,
            )
            with self.assertRaises(ShipBlockedError):
                probe.evaluate(device_id=_DEVICE, public_key_hex="UNPROVISIONED")

    def test_incorrect_device_selection(self) -> None:
        calls = {"n": 0}

        def runner(script: str) -> str:
            calls["n"] += 1
            if "prettyversion" in script and "scribe_app_present" not in script:
                return _identity_block(usid="0" * 64)
            return _inventory_block()

        with tempfile.TemporaryDirectory() as tmp:
            probe = KindleSshHardwareProbe(
                ssh_host="kindle",
                evidence_dir=Path(tmp),
                run_script=runner,
            )
            with self.assertRaises(ShipError) as ctx:
                probe.evaluate(device_id=_DEVICE, public_key_hex="ab" * 32)
            self.assertIn("unexpected Kindle device identity", str(ctx.exception))

    def test_harness_rejects_wrong_connected_id(self) -> None:
        harness = HardwareAcceptanceHarness(required_device_id=_DEVICE)
        with self.assertRaises(ShipError):
            harness.run(
                signed_root=Path("."),
                public_key_hex="ab" * 32,
                version="0.9.15",
                connected_device_id="kindle-usid-sha256:" + ("11" * 32),
                probe=MockHardwareProbe(),
            )

    def test_malformed_remote_output_unsupported_firmware(self) -> None:
        def runner(script: str) -> str:
            if "scribe_app_present" in script:
                return _inventory_block()
            return _identity_block(firmware="Kindle 5.12.0")

        with tempfile.TemporaryDirectory() as tmp:
            probe = KindleSshHardwareProbe(
                ssh_host="kindle",
                evidence_dir=Path(tmp),
                run_script=runner,
            )
            with self.assertRaises(ShipBlockedError) as ctx:
                probe.evaluate(device_id=_DEVICE, public_key_hex="ab" * 32)
            self.assertIn("unsupported Kindle firmware", str(ctx.exception))

    def test_timeout_mapped_to_blocked(self) -> None:
        def runner(_script: str) -> str:
            raise ShipBlockedError("release-blocked: Kindle SSH timed out (kindle)")

        with tempfile.TemporaryDirectory() as tmp:
            probe = KindleSshHardwareProbe(
                ssh_host="kindle",
                evidence_dir=Path(tmp),
                run_script=runner,
            )
            with self.assertRaises(ShipBlockedError) as ctx:
                probe.evaluate(device_id=_DEVICE, public_key_hex="ab" * 32)
            self.assertIn("timed out", str(ctx.exception))

    def test_pre_release_ready_without_production_evidence(self) -> None:
        def runner(script: str) -> str:
            if "scribe_app_present" in script:
                return _inventory_block()
            return _identity_block()

        with tempfile.TemporaryDirectory() as tmp:
            probe = KindleSshHardwareProbe(
                ssh_host="kindle",
                evidence_dir=Path(tmp),
                repo_pin_hex="UNPROVISIONED",
                run_script=runner,
            )
            report = probe.evaluate(device_id=_DEVICE, public_key_hex="ab" * 32)
            self.assertTrue(report.pre_release_ready)
            self.assertTrue(report.notebooks_intact)
            self.assertFalse(report.production_evidence)
            self.assertFalse(report.authorizes_publication)
            self.assertEqual("ABSENT_PREDATES_PIN", report.embedded_pin_state)
            self.assertEqual("0.9.13-diag19b3d3", report.installed_version)
            self.assertTrue(
                any(p.name.startswith("kindle-ssh-probe-") for p in Path(tmp).iterdir())
            )
            self.assertTrue(
                any(c.startswith("T01|PASS|") for c in report.case_results)
            )
            self.assertTrue(
                any("NOT_RUN_SAFETY_BOUNDARY" in c for c in report.case_results)
            )

    def test_doctor_connectivity_mismatch(self) -> None:
        def runner(_script: str) -> str:
            return _identity_block(usid="a" * 64)

        with tempfile.TemporaryDirectory() as tmp:
            probe = KindleSshHardwareProbe(
                ssh_host="kindle",
                evidence_dir=Path(tmp),
                run_script=runner,
            )
            lines = probe.doctor_connectivity(device_id=_DEVICE)
            self.assertIn("hardware_probe=device-mismatch", lines)

    def test_invalid_device_id_format(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            probe = KindleSshHardwareProbe(
                ssh_host="kindle",
                evidence_dir=Path(tmp),
                run_script=lambda _s: "",
            )
            with self.assertRaises(ShipError):
                probe.evaluate(device_id="qa-kindle-1", public_key_hex="ab" * 32)


if __name__ == "__main__":
    unittest.main()
