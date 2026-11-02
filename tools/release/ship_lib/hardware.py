"""Bounded Kindle hardware acceptance harness interface."""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

from .errors import ShipBlockedError, ShipError


@dataclass(frozen=True)
class HardwareAcceptanceReport:
    device_id: str
    firmware: str
    arm_package_embeds_pin: bool
    matching_release_accepted: bool
    wrong_key_refused: bool
    corrupted_metadata_refused: bool
    version_rules_ok: bool
    staging_interrupt_ok: bool
    authorization_behavior_ok: bool
    notebooks_intact: bool
    production_evidence: bool
    details: list[str] = field(default_factory=list)

    @property
    def passed(self) -> bool:
        return all(
            (
                self.arm_package_embeds_pin,
                self.matching_release_accepted,
                self.wrong_key_refused,
                self.corrupted_metadata_refused,
                self.version_rules_ok,
                self.staging_interrupt_ok,
                self.authorization_behavior_ok,
                self.notebooks_intact,
            )
        )

    @property
    def authorizes_publication(self) -> bool:
        return self.passed and self.production_evidence


class HardwareAcceptanceHarness:
    """Accepts only an explicitly configured QA/test Kindle device id."""

    def __init__(self, *, required_device_id: str | None) -> None:
        self._required_device_id = required_device_id

    def run(
        self,
        *,
        signed_root: Path,
        public_key_hex: str,
        version: str,
        connected_device_id: str | None = None,
        probe: HardwareProbe | None = None,
    ) -> HardwareAcceptanceReport:
        del signed_root, version  # used by live probe implementations
        if not self._required_device_id:
            raise ShipBlockedError(
                "release-blocked: hardware-acceptance-unavailable "
                "(set IGNITE_SHIP_KINDLE_DEVICE_ID to a designated QA Kindle)"
            )
        device = connected_device_id or self._required_device_id
        if device != self._required_device_id:
            raise ShipError(
                "refusing hardware acceptance on unexpected Kindle device identity"
            )
        probe = probe or NullHardwareProbe()
        return probe.evaluate(
            device_id=device,
            public_key_hex=public_key_hex,
        )


class HardwareProbe:
    def evaluate(
        self, *, device_id: str, public_key_hex: str
    ) -> HardwareAcceptanceReport:  # pragma: no cover - interface
        raise NotImplementedError


class NullHardwareProbe(HardwareProbe):
    """Host placeholder — never claims ARM/device success."""

    def evaluate(
        self, *, device_id: str, public_key_hex: str
    ) -> HardwareAcceptanceReport:
        del public_key_hex
        raise ShipBlockedError(
            "release-blocked: hardware-acceptance-unavailable "
            f"(no live Kindle probe configured for device {device_id})"
        )


class MockHardwareProbe(HardwareProbe):
    """Deterministic host mock — never production evidence; cannot authorize publish."""

    def __init__(self, *, pass_all: bool = True) -> None:
        self._pass_all = pass_all

    def evaluate(
        self, *, device_id: str, public_key_hex: str
    ) -> HardwareAcceptanceReport:
        ok = self._pass_all
        return HardwareAcceptanceReport(
            device_id=device_id,
            firmware="5.18.6-mock",
            arm_package_embeds_pin=ok and len(public_key_hex) == 64,
            matching_release_accepted=ok,
            wrong_key_refused=ok,
            corrupted_metadata_refused=ok,
            version_rules_ok=ok,
            staging_interrupt_ok=ok,
            authorization_behavior_ok=ok,
            notebooks_intact=ok,
            production_evidence=False,
            details=["mock-host-probe-not-arm-device"],
        )


class ScriptedHardwareProbe(HardwareProbe):
    """Test seam simulating a live-capable probe (not used by default ship).

    Production publication still requires ``production_evidence=True``. Unit
    tests that exercise the verified-publish path may inject this seam; the
    default CLI never installs it.
    """

    def __init__(
        self,
        *,
        pass_all: bool = True,
        production_evidence: bool = True,
        firmware: str = "5.18.6-qa",
    ) -> None:
        self._pass_all = pass_all
        self._production_evidence = production_evidence
        self._firmware = firmware

    def evaluate(
        self, *, device_id: str, public_key_hex: str
    ) -> HardwareAcceptanceReport:
        ok = self._pass_all
        details = (
            ["scripted-live-capable-probe"]
            if self._production_evidence
            else ["scripted-probe-without-production-evidence"]
        )
        return HardwareAcceptanceReport(
            device_id=device_id,
            firmware=self._firmware,
            arm_package_embeds_pin=ok and len(public_key_hex) == 64,
            matching_release_accepted=ok,
            wrong_key_refused=ok,
            corrupted_metadata_refused=ok,
            version_rules_ok=ok,
            staging_interrupt_ok=ok,
            authorization_behavior_ok=ok,
            notebooks_intact=ok,
            production_evidence=bool(self._production_evidence and ok),
            details=details,
        )
