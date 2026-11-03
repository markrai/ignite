"""Host regressions for verified update staging helpers (Slice 3D-B2a.4.3i)."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parents[1]
_SFX = _HERE / "sfx"
for extra in (_HERE, _SFX):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from authenticated_release_assets import (  # noqa: E402
    FIXTURE_AUTH_SUBDIR,
    build_authenticated_release_assets,
    release_asset_basenames,
    write_authenticated_release_assets,
)
from authenticated_publication_evidence import generate_test_trust_root  # noqa: E402
from installer_publication_rehearsal import distinguish_payload_bytes  # noqa: E402
from release_component_profiles import generate_profiles  # noqa: E402
from test_validate_stage import make_payload, write_sfx  # noqa: E402
from validate_stage import installer_sfx_name  # noqa: E402

VERSION = "3.3.1"
TAG = f"v{VERSION}"


@pytest.fixture
def disposable_root(tmp_path: Path):
    return generate_test_trust_root(tmp_path / "keys", label="slice-3i-publisher")


def _build_fixture(tmp_path: Path):
    payload = make_payload(tmp_path / "src", VERSION)
    distinguish_payload_bytes(payload, b"STAGE-3I")
    scriptlet = tmp_path / installer_sfx_name(VERSION)
    write_sfx(payload, scriptlet, VERSION)
    profiles = generate_profiles(payload, VERSION)
    return payload, scriptlet, profiles["package"]


def test_fixture_assets_ready_for_staging_harness(tmp_path: Path, disposable_root) -> None:
    """Produce the six-asset fixture set the Rust staging harness consumes."""
    payload, scriptlet, profile = _build_fixture(tmp_path)
    assets = build_authenticated_release_assets(
        version=VERSION,
        payload=payload,
        scriptlet=scriptlet,
        package_profile=profile,
        private_key_path=disposable_root.private_key_path,
        public_key=disposable_root.public_key,
    )
    out = tmp_path / "fixture-assets"
    written = write_authenticated_release_assets(assets, out)
    names = release_asset_basenames(VERSION)
    for key in (
        "release_manifest",
        "release_signature",
        "publication_evidence",
        "publication_binding",
        "binding_signature",
    ):
        assert written[key].is_file()
        assert written[key].name == names[key]
    # Scriptlet remains the ordinary customer artifact (not rewritten by asset emit).
    assert scriptlet.is_file()
    assert scriptlet.name == names["scriptlet"]
    # Copy Scriptlet beside metadata for the on-device/local staging harness layout.
    staged_scriptlet = out / names["scriptlet"]
    staged_scriptlet.write_bytes(scriptlet.read_bytes())
    pub = out / "public_key.hex"
    pub.write_bytes(disposable_root.public_key.hex().encode("ascii") + b"\n")
    assert len(pub.read_text(encoding="ascii").strip()) == 64
    assert FIXTURE_AUTH_SUBDIR  # packaging still isolates fixture emit under this name


def test_staging_asset_basenames_are_exact_six() -> None:
    names = release_asset_basenames(VERSION)
    assert set(names) == {
        "scriptlet",
        "release_manifest",
        "release_signature",
        "publication_evidence",
        "publication_binding",
        "binding_signature",
    }
    assert names["scriptlet"] == f"Install Ignite v{VERSION}.sh"
    assert TAG == f"v{VERSION}"
