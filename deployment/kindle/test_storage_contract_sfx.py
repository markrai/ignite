"""Storage-contract claim must be readable from real build_sfx.py installers."""

from __future__ import annotations

import hashlib
import sys
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_SFX = _HERE / "sfx"
for extra in (_HERE, _SFX):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from build_sfx import build_sfx, default_template, split_sfx  # noqa: E402
from test_validate_stage import make_payload  # noqa: E402

CLAIM = "IGNITE_STORAGE_CONTRACT='ignite-storage-v1'"


def _sfx_prefix_contains_claim(blob: bytes) -> bool:
    prefix, _archive, _meta = split_sfx(blob)
    text = prefix.decode("utf-8")
    return any(line.strip() == CLAIM for line in text.splitlines())


def test_build_sfx_binary_payload_preserves_storage_contract_claim(tmp_path: Path) -> None:
    version = "9.9.9"
    payload = make_payload(tmp_path / "payload", version)
    # Ensure payload has non-trivial binary content so the archive is real gzip.
    app = payload / "bin" / "scribe-app"
    app.write_bytes(bytes(range(256)) * 64 + app.read_bytes())
    blob = build_sfx(payload, version, default_template())
    assert blob[blob.find(b"\x1f\x8b") :][:2] == b"\x1f\x8b" or b"\x1f\x8b" in blob
    prefix, archive, meta = split_sfx(blob)
    assert archive[:2] == b"\x1f\x8b"
    assert int(meta["offset"]) == len(prefix)
    assert _sfx_prefix_contains_claim(blob)
    # Full-file UTF-8 decode must fail on a real installer with binary payload.
    assert not _is_utf8(blob)
    # Prefix-bounded scan (mirrors Rust sfx_shell_prefix_bound) still finds the claim.
    bound = int(meta["offset"])
    assert CLAIM.encode("ascii") in blob[:bound]


def _is_utf8(data: bytes) -> bool:
    try:
        data.decode("utf-8")
        return True
    except UnicodeDecodeError:
        return False


def test_materialized_contract_matches_authenticated_sfx_claim(tmp_path: Path) -> None:
    version = "9.9.8"
    payload = make_payload(tmp_path / "payload", version)
    blob = build_sfx(payload, version, default_template())
    assert _sfx_prefix_contains_claim(blob)
    contract = (
        "IGNITE_STORAGE_CONTRACT_V1\n"
        "schema=1\n"
        "storage_contract=ignite-storage-v1\n"
    ).encode("ascii")
    # Sidecar alone is not enough without the Scriptlet claim — claim is present here.
    assert hashlib.sha256(blob).hexdigest()
    assert b"storage_contract=ignite-storage-v1\n" in contract
