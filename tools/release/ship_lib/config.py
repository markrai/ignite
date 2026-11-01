"""Ship configuration — paths outside the repository; no secrets inline."""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

from .errors import ShipConfigError

_REPO = Path(__file__).resolve().parents[3]
_DEFAULT_HOME_WIN = Path(os.environ.get("LOCALAPPDATA", "")) / "Ignite" / "ship"
_DEFAULT_HOME_POSIX = Path.home() / ".local" / "share" / "ignite" / "ship"


def default_ship_home() -> Path:
    if os.name == "nt":
        base = _DEFAULT_HOME_WIN
        if not str(base).strip("\\/"):
            raise ShipConfigError(
                "LOCALAPPDATA is unset; cannot locate a secure Ignite ship home"
            )
        return base
    return _DEFAULT_HOME_POSIX


@dataclass(frozen=True)
class ShipConfig:
    """Operator-facing ship configuration.

    Private-key material never lives under ``repo_root``. Recovery backups must
    use a destination distinct from ``ship_home``.
    """

    repo_root: Path
    ship_home: Path
    recovery_dir: Path | None
    repository: str = "markrai/ignite"
    feature_branch: str = "feat/kindle-auto-updater"
    base_branch: str = "main"
    allow_live: bool = False
    allow_insecure_store: bool = False
    hardware_device_id: str | None = None
    ci_poll_seconds: int = 30
    ci_timeout_seconds: int = 3600
    require_hardware: bool = True

    @property
    def authority_dir(self) -> Path:
        return self.ship_home / "authority"

    @property
    def journal_dir(self) -> Path:
        return self.ship_home / "journal"

    @property
    def work_dir(self) -> Path:
        return self.ship_home / "work"

    @property
    def pin_path(self) -> Path:
        return self.repo_root / "tools" / "release" / "kindle-release-public-key.hex"

    @property
    def public_key_path(self) -> Path:
        return self.authority_dir / "publisher-public.hex"

    @property
    def fingerprint_path(self) -> Path:
        return self.authority_dir / "publisher-fingerprint.sha256"

    @property
    def sealed_private_path(self) -> Path:
        return self.authority_dir / "publisher-private.sealed"

    @property
    def authority_meta_path(self) -> Path:
        return self.authority_dir / "authority.meta.v1"


def load_ship_config(
    *,
    repo_root: Path | None = None,
    ship_home: Path | None = None,
    recovery_dir: Path | None = None,
    allow_live: bool = False,
    hardware_device_id: str | None = None,
    require_hardware: bool | None = None,
) -> ShipConfig:
    root = (repo_root or _REPO).resolve()
    home = (ship_home or Path(os.environ.get("IGNITE_SHIP_HOME", default_ship_home()))).resolve()
    recovery_env = os.environ.get("IGNITE_SHIP_RECOVERY_DIR")
    recovery = recovery_dir
    if recovery is None and recovery_env:
        recovery = Path(recovery_env)
    if recovery is not None:
        recovery = recovery.resolve()
        if recovery == home or home in recovery.parents or recovery in home.parents:
            raise ShipConfigError(
                "IGNITE_SHIP_RECOVERY_DIR must be independent of IGNITE_SHIP_HOME "
                "(off-machine / separate durable recovery destination required)"
            )
        if root in recovery.parents or recovery == root or recovery in root.parents:
            raise ShipConfigError(
                "recovery destination must not be inside the Git repository"
            )

    insecure = os.environ.get("IGNITE_SHIP_ALLOW_INSECURE_STORE", "").strip() == "1"
    live = allow_live or os.environ.get("IGNITE_SHIP_ALLOW_LIVE", "").strip() == "1"
    hw = hardware_device_id or os.environ.get("IGNITE_SHIP_KINDLE_DEVICE_ID")
    req_hw = (
        require_hardware
        if require_hardware is not None
        else os.environ.get("IGNITE_SHIP_REQUIRE_HARDWARE", "1").strip() != "0"
    )
    return ShipConfig(
        repo_root=root,
        ship_home=home,
        recovery_dir=recovery,
        allow_live=live,
        allow_insecure_store=insecure,
        hardware_device_id=hw,
        require_hardware=req_hw,
    )
