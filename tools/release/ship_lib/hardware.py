"""Bounded Kindle hardware acceptance harness and live SSH probe."""

from __future__ import annotations

import re
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

from .errors import ShipBlockedError, ShipError

_SUPPORTED_FIRMWARE_PREFIXES: tuple[str, ...] = ("Kindle 5.18.6",)

RemoteScriptRunner = Callable[[str], str]


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
    pre_release_ready: bool = False
    embedded_pin_state: str = "unknown"
    installed_version: str = ""
    case_results: tuple[str, ...] = ()

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
        del signed_root, version
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
            pre_release_ready=False,
            embedded_pin_state="mock",
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
            pre_release_ready=ok,
            embedded_pin_state="scripted",
        )


def device_id_from_usid_sha256(usid_sha256: str) -> str:
    digest = usid_sha256.strip().lower()
    if not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise ShipError("Kindle usid sha256 must be 64 lowercase hex characters")
    return f"kindle-usid-sha256:{digest}"


def parse_usid_sha256_from_device_id(device_id: str) -> str | None:
    prefix = "kindle-usid-sha256:"
    if not device_id.startswith(prefix):
        return None
    digest = device_id[len(prefix) :].strip().lower()
    if not re.fullmatch(r"[0-9a-f]{64}", digest):
        return None
    return digest


def _kv(text: str, key: str) -> str | None:
    for line in text.splitlines():
        if line.startswith(f"{key}="):
            return line.split("=", 1)[1].strip()
    return None


def _count_grep(text: str, key: str) -> int:
    raw = _kv(text, key)
    if raw is None:
        return 0
    try:
        return int(raw)
    except ValueError:
        return 0


def run_ssh_script(
    *,
    host: str,
    script: str,
    timeout_seconds: float,
) -> str:
    """Execute a remote sh script via OpenSSH BatchMode (stdin), Host alias only."""
    # Dropbear/BusyBox reject CRLF (`set -eu\\r` → illegal option).
    normalized = script.replace("\r\n", "\n").replace("\r", "\n").lstrip("\ufeff")
    argv = [
        "ssh",
        "-o",
        "BatchMode=yes",
        "-o",
        f"ConnectTimeout={max(1, int(timeout_seconds))}",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "IdentitiesOnly=yes",
        host,
        "sh -s",
    ]
    try:
        # Binary stdin avoids Windows text-mode LF→CRLF translation that breaks Dropbear.
        proc = subprocess.run(
            argv,
            input=normalized.encode("utf-8"),
            check=False,
            capture_output=True,
            timeout=timeout_seconds + 5.0,
        )
    except subprocess.TimeoutExpired as exc:
        raise ShipBlockedError(
            f"release-blocked: Kindle SSH timed out ({host})"
        ) from exc
    stdout = (proc.stdout or b"").decode("utf-8", errors="replace")
    stderr = (proc.stderr or b"").decode("utf-8", errors="replace")
    if proc.returncode != 0:
        err = (stderr or stdout).strip()[:300]
        raise ShipBlockedError(
            f"release-blocked: Kindle SSH failed rc={proc.returncode} {err}"
        )
    return stdout


@dataclass
class KindleSshHardwareProbe(HardwareProbe):
    """Live, non-destructive Kindle acceptance over an existing SSH Host alias.

    Never installs, never executes an installer, never mutates notebooks, and
    never sets ``production_evidence`` while the repository pin is unprovisioned
    or the installed ARM build lacks a verified production-signed OTA path.
    """

    ssh_host: str
    evidence_dir: Path
    timeout_seconds: float = 45.0
    repo_pin_hex: str = "UNPROVISIONED"
    run_script: RemoteScriptRunner | None = None
    supported_firmware_prefixes: tuple[str, ...] = _SUPPORTED_FIRMWARE_PREFIXES

    def _remote(self, script: str) -> str:
        runner = self.run_script
        if runner is not None:
            return runner(script)
        return run_ssh_script(
            host=self.ssh_host,
            script=script,
            timeout_seconds=self.timeout_seconds,
        )

    def evaluate(
        self, *, device_id: str, public_key_hex: str
    ) -> HardwareAcceptanceReport:
        del public_key_hex
        expected = parse_usid_sha256_from_device_id(device_id)
        if expected is None:
            raise ShipError(
                "IGNITE_SHIP_KINDLE_DEVICE_ID must be kindle-usid-sha256:<64-hex> "
                "derived from the designated Kindle /proc/usid"
            )
        self.evidence_dir.mkdir(parents=True, exist_ok=True)
        stamp = time.strftime("%Y%m%dT%H%M%S")
        cases: list[str] = []
        details: list[str] = []

        identity = self._remote(
            """
set -eu
USID=""
if [ -r /proc/usid ]; then USID=$(cat /proc/usid | tr -d '\\000\\n\\r'); fi
printf 'usid_sha256=%s\\n' "$(printf '%s' "$USID" | sha256sum | awk '{print $1}')"
printf 'usid_prefix=%s\\n' "${USID%????????}"
printf 'hostname=%s\\n' "$(hostname 2>/dev/null || true)"
printf 'arch=%s\\n' "$(uname -m 2>/dev/null || true)"
printf 'prettyversion=%s\\n' "$(cat /etc/prettyversion.txt 2>/dev/null | tr '\\n' ' ')"
printf 'version_txt=%s\\n' "$(head -1 /etc/version.txt 2>/dev/null || true)"
"""
        )
        usid_sha = _kv(identity, "usid_sha256") or ""
        firmware = (_kv(identity, "prettyversion") or "").strip()
        arch = _kv(identity, "arch") or ""
        if usid_sha != expected:
            cases.append(
                f"T01|FAIL|device identity mismatch expected={expected[:12]}… "
                f"observed={usid_sha[:12]}…"
            )
            raise ShipError(
                "refusing hardware acceptance on unexpected Kindle device identity"
            )
        if arch != "armv7l":
            raise ShipBlockedError(
                f"release-blocked: unsupported Kindle architecture {arch!r}"
            )
        firmware_ok = any(
            firmware.startswith(prefix) for prefix in self.supported_firmware_prefixes
        )
        if not firmware_ok:
            raise ShipBlockedError(
                f"release-blocked: unsupported Kindle firmware {firmware!r}"
            )
        cases.append(f"T01|PASS|ssh_ok firmware={firmware!r} arch={arch}")

        inventory = self._remote(
            """
set -eu
BIN=/mnt/us/scribe-app/bin/scribe-app
LC=/mnt/us/scribe-app/bin/scribe-lifecycle
NB=/mnt/us/scribe-data/notebooks
printf 'scribe_app_present=%s\\n' "$([ -x "$BIN" ] && echo yes || echo no)"
if [ -x "$BIN" ]; then
  printf 'scribe_app_sha256=%s\\n' "$(sha256sum "$BIN" | awk '{print $1}')"
  printf 'scribe_app_size=%s\\n' "$(wc -c < "$BIN")"
fi
printf 'lifecycle_present=%s\\n' "$([ -x "$LC" ] && echo yes || echo no)"
HELP=$("$BIN" --help 2>&1 || true)
if printf '%s' "$HELP" | grep -q 'stage-verified-update'; then
  printf 'updater_cli=present\\n'
else
  printf 'updater_cli=absent\\n'
fi
printf 'UNPROVISIONED_count=%s\\n' "$(grep -a -o 'UNPROVISIONED' "$BIN" 2>/dev/null | wc -l | tr -d ' ')"
printf 'stage_cli_count=%s\\n' "$(grep -a -o 'stage-verified-update' "$BIN" 2>/dev/null | wc -l | tr -d ' ')"
printf 'manifest_schema_count=%s\\n' "$(grep -a -o 'ignite-kindle-release-v1' "$BIN" 2>/dev/null | wc -l | tr -d ' ')"
printf 'notebook_dirs=%s\\n' "$(find "$NB" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d ' ')"
printf 'notebook_files=%s\\n' "$(find "$NB" -type f 2>/dev/null | wc -l | tr -d ' ')"
if [ -f /mnt/us/scribe-data/library.meta ]; then
  printf 'library_meta_sha256=%s\\n' "$(sha256sum /mnt/us/scribe-data/library.meta | awk '{print $1}')"
  printf 'library_meta_size=%s\\n' "$(wc -c < /mnt/us/scribe-data/library.meta | tr -d ' ')"
fi
printf 'prod_stage_exists=%s\\n' "$([ -e /mnt/us/scribe-app/state/verified-update ] && echo yes || echo no)"
if [ -f /mnt/us/scribe-app/logs/install.log ]; then
  printf 'install_last=%s\\n' "$(grep -E '^==== Install Ignite ' /mnt/us/scribe-app/logs/install.log | tail -1)"
fi
ps 2>/dev/null | grep -E 'scribe-app|scribe-lifecycle' | grep -v grep >/dev/null \
  && printf 'ignite_proc=present\\n' || printf 'ignite_proc=none_matched\\n'
"""
        )
        evidence_path = self.evidence_dir / f"kindle-ssh-probe-{stamp}.txt"
        evidence_path.write_text(
            "IGNITE_3R_SSH_PROBE_EVIDENCE_V1\n" + identity + "\n" + inventory + "\n",
            encoding="utf-8",
        )
        details.append(f"evidence={evidence_path}")

        if _kv(inventory, "scribe_app_present") != "yes":
            cases.append("T02|FAIL|scribe-app missing")
            raise ShipBlockedError(
                "release-blocked: installed Ignite binary missing on designated Kindle"
            )
        cases.append(
            "T02|PASS|scribe-app " f"sha256={_kv(inventory, 'scribe_app_sha256')}"
        )
        if _kv(inventory, "lifecycle_present") != "yes":
            cases.append("T02b|FAIL|scribe-lifecycle missing")
        else:
            cases.append("T02b|PASS|scribe-lifecycle present")

        updater_cli = _kv(inventory, "updater_cli") == "present"
        if updater_cli:
            cases.append("T03|PASS|updater CLI present on installed ARM binary")
        else:
            cases.append(
                "T03|SKIP|installed ARM binary lacks stage-verified-update "
                "(predates updater; host-only negative tests required)"
            )

        unprov_count = _count_grep(inventory, "UNPROVISIONED_count")
        schema_count = _count_grep(inventory, "manifest_schema_count")
        if unprov_count > 0:
            pin_state = "UNPROVISIONED"
            cases.append("T04|PASS|installed binary embeds UNPROVISIONED sentinel")
        elif schema_count > 0:
            pin_state = "PIN_SCHEMA_PRESENT_STATE_UNKNOWN"
            cases.append(
                "T04|SKIP|release schema strings present but UNPROVISIONED literal absent"
            )
        else:
            pin_state = "ABSENT_PREDATES_PIN"
            cases.append(
                "T04|PASS|observation=installed binary predates embedded publisher pin"
            )

        cases.append(
            "T06|NOT_RUN_SAFETY_BOUNDARY|"
            "production matching-release acceptance deferred until pin activation "
            "+ post-provision ARM install"
        )
        cases.append(
            "T07|NOT_RUN_SAFETY_BOUNDARY|"
            "authorize-verified-update on production assets deferred "
            "(would not authorize install; still skipped while pin UNPROVISIONED)"
        )
        if updater_cli:
            cases.append(
                "T08|SKIP|fixture negative paths require explicit isolated fixture upload"
            )
            cases.append(
                "T10|NOT_RUN_SAFETY_BOUNDARY|"
                "interrupted multi-MB download recovery not exercised on user device"
            )
        else:
            cases.append(
                "T08|NOT_RUN_SAFETY_BOUNDARY|"
                "malformed/wrong-key/corrupt rejection not executable on this ARM build"
            )
            cases.append(
                "T10|NOT_RUN_SAFETY_BOUNDARY|interrupted download recovery requires updater"
            )
        cases.append(
            "T11|NOT_RUN_SAFETY_BOUNDARY|"
            "no production-signed OTA acceptance while repository pin is UNPROVISIONED"
        )
        cases.append(
            "T12|NOT_RUN_SAFETY_BOUNDARY|install/execute downloaded installer forbidden"
        )

        if _kv(inventory, "prod_stage_exists") == "no":
            cases.append("T09|PASS|production verified-update staging absent")
        else:
            cases.append(
                "T09|SKIP|production staging directory exists; left untouched"
            )

        nb_files = _kv(inventory, "notebook_files") or ""
        lib_hash = _kv(inventory, "library_meta_sha256") or ""
        notebooks_ok = bool(nb_files.isdigit() and lib_hash)
        if notebooks_ok:
            cases.append(
                f"T13|PASS|notebook_files={nb_files} library_meta_sha256={lib_hash}"
            )
        else:
            cases.append("T13|FAIL|notebook integrity markers unavailable")

        install_last = _kv(inventory, "install_last") or ""
        installed_version = ""
        match = re.search(r"Install Ignite ([^ ]+)", install_last)
        if match:
            installed_version = match.group(1)

        pin_is_unprovisioned = self.repo_pin_hex.strip().upper() == "UNPROVISIONED"
        pre_release = (
            firmware_ok
            and notebooks_ok
            and _kv(inventory, "scribe_app_present") == "yes"
            and usid_sha == expected
        )
        details.append(f"ssh_host={self.ssh_host}")
        details.append(f"embedded_pin_state={pin_state}")
        details.append(f"installed_version={installed_version or 'unknown'}")
        details.append(f"pre_release_ready={str(pre_release).lower()}")
        details.append("production_evidence=false")
        if pin_is_unprovisioned:
            details.append(
                "blocker=repository pin UNPROVISIONED; official OTA acceptance impossible"
            )
        if pin_state == "ABSENT_PREDATES_PIN":
            details.append(
                "blocker=installed ARM build predates updater; "
                "trusted out-of-band reinstall required after pin activation"
            )

        return HardwareAcceptanceReport(
            device_id=device_id,
            firmware=firmware or "unknown",
            arm_package_embeds_pin=False,
            matching_release_accepted=False,
            wrong_key_refused=False,
            corrupted_metadata_refused=False,
            version_rules_ok=False,
            staging_interrupt_ok=False,
            authorization_behavior_ok=False,
            notebooks_intact=notebooks_ok,
            production_evidence=False,
            details=details,
            pre_release_ready=pre_release,
            embedded_pin_state=pin_state,
            installed_version=installed_version,
            case_results=tuple(cases),
        )

    def doctor_connectivity(self, *, device_id: str) -> list[str]:
        lines: list[str] = [f"hardware_ssh_host={self.ssh_host}"]
        expected = parse_usid_sha256_from_device_id(device_id)
        if expected is None:
            lines.append("hardware_probe=invalid-device-id-format")
            return lines
        try:
            out = self._remote(
                """
set -eu
USID=""
if [ -r /proc/usid ]; then USID=$(cat /proc/usid | tr -d '\\000\\n\\r'); fi
printf 'usid_sha256=%s\\n' "$(printf '%s' "$USID" | sha256sum | awk '{print $1}')"
printf 'prettyversion=%s\\n' "$(cat /etc/prettyversion.txt 2>/dev/null | tr '\\n' ' ')"
printf 'arch=%s\\n' "$(uname -m)"
"""
            )
        except ShipBlockedError as exc:
            lines.append(f"hardware_probe=unreachable:{exc}")
            return lines
        observed = _kv(out, "usid_sha256") or ""
        if observed != expected:
            lines.append("hardware_probe=device-mismatch")
            return lines
        lines.append("hardware_probe=reachable")
        lines.append(f"hardware_firmware={_kv(out, 'prettyversion') or 'unknown'}")
        lines.append("hardware_production_evidence=false")
        return lines


def build_default_kindle_probe(
    *,
    ssh_host: str,
    evidence_dir: Path,
    repo_pin_hex: str,
    timeout_seconds: float = 45.0,
) -> KindleSshHardwareProbe:
    return KindleSshHardwareProbe(
        ssh_host=ssh_host,
        evidence_dir=evidence_dir,
        timeout_seconds=timeout_seconds,
        repo_pin_hex=repo_pin_hex,
    )
