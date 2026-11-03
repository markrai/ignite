"""Public pin provisioning into the repository pin file."""

from __future__ import annotations

import sys

from .authority import PublisherAuthority, load_authority
from .config import ShipConfig
from .errors import ShipError


def provision_public_pin(config: ShipConfig, *, authority: PublisherAuthority | None = None) -> str:
    """Replace UNPROVISIONED with the authority public key (public material only).

    Refuses unexpected changes to an already-matching provisioned pin.
    Never writes private-key material.
    """
    auth = authority or load_authority(config)
    pin_path = config.pin_path
    desired = auth.public_key.hex().encode("ascii") + b"\n"
    current = pin_path.read_bytes()
    if current == desired:
        _validate_pin_module(config)
        return auth.fingerprint_sha256
    if current != b"UNPROVISIONED\n":
        raise ShipError(
            "repository pin is already provisioned with different material; "
            "key rotation is not supported by ignite_ship"
        )
    pin_path.write_bytes(desired)
    if pin_path.read_bytes() != desired:
        raise ShipError("failed to write production public-key pin")
    _validate_pin_module(config)
    # Kindle embeds the pin at compile time via include_str! of this exact file.
    embed_path = (
        config.repo_root
        / "ignite"
        / "kindle"
        / "scribe-app"
        / "src"
        / "release_manifest.rs"
    )
    if embed_path.is_file():
        text = embed_path.read_text(encoding="utf-8")
        if "kindle-release-public-key.hex" not in text or "include_str!" not in text:
            raise ShipError(
                "scribe-app release_manifest.rs does not include the repository pin"
            )
    return auth.fingerprint_sha256


def _validate_pin_module(config: ShipConfig) -> None:
    kindle = config.repo_root / "deployment" / "kindle"
    release = config.repo_root / "tools" / "release"
    for path in (str(kindle), str(release)):
        if path not in sys.path:
            sys.path.insert(0, path)
    import production_trust_boundary as ptb

    # Point the module at this repo's pin (important for disposable test trees).
    ptb._PRODUCTION_PIN = config.pin_path  # noqa: SLF001 — intentional test/ship wiring
    ptb.assert_production_pin_well_formed()
    if ptb.production_pin_is_unprovisioned():
        raise ShipError("pin still reports UNPROVISIONED after provisioning")
    loaded = ptb.load_production_public_key()
    auth = load_authority(config)
    if loaded != auth.public_key:
        raise ShipError("loaded production pin does not match authority public key")
