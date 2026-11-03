#!/usr/bin/env python3
"""FIXTURE-ONLY helper: emit synthetic evidence+binding for promote harness tests.

WARNING: This is not the production offline signing ceremony.

It builds publication-evidence component claims from ``test_validate_stage.make_payload()``,
which is a disposable synthetic payload unrelated to trusted CI packaging measurements.
Scriptlet identity (size/sha256) is taken from the supplied installer bytes, but the
22-role component inventory is fabricated for tests.

Never present these bytes as verified production CI component provenance.
Prefer deployment/kindle/offline_packaging_handoff.py with the CI measurement
artifact from the same Scriptlet build (Slice 3l). See
docs/kindle-ci-packaging-measurement.md.

Disposable keys only. Never touches the production private key.
"""

from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parents[1]
_SFX = _HERE / "sfx"
_RELEASE = _REPO / "tools" / "release"
for extra in (_HERE, _SFX, str(_RELEASE)):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from authenticated_release_assets import (  # noqa: E402
    build_release_v1_from_identity,
    measure_scriptlet,
    release_asset_basenames,
)
from kindle_release_sign import version_from_tag  # noqa: E402
from production_publication_evidence import (  # noqa: E402
    build_production_publication_evidence,
)
from production_trust_boundary import (  # noqa: E402
    assert_production_pin_well_formed,
    build_publication_binding,
    sign_binding_for_test,
)
from test_validate_stage import make_payload  # noqa: E402


def emit_fixture_offline_metadata(
    *,
    installer: Path,
    tag: str,
    private_key: Path,
    public_key_hex: Path,
    output_dir: Path,
    work_dir: Path,
) -> None:
    """Write fixture-only evidence/binding/binding.sig for harness tests."""
    assert_production_pin_well_formed()
    version = version_from_tag(tag)
    identity = measure_scriptlet(
        installer, expected_name=f"Install Ignite v{version}.sh"
    )
    # Synthetic payload: test contract shape only — not CI component provenance.
    payload = make_payload(work_dir / "payload-src", version)
    evidence = build_production_publication_evidence(
        version=version,
        payload=payload,
        scriptlet_bytes=identity.size,
        scriptlet_sha256=identity.sha256,
    )
    release = build_release_v1_from_identity(version=version, identity=identity)
    binding = build_publication_binding(
        release_tag=tag,
        release_version=version,
        release_manifest=release,
        scriptlet_sha256=identity.sha256,
        publication_evidence=evidence,
    )
    public_raw = public_key_hex.read_bytes()
    if not public_raw.endswith(b"\n") or b"\r" in public_raw:
        raise SystemExit("fixture public key must be 64 lowercase hex bytes plus LF")
    public_key = bytes.fromhex(public_raw[:-1].decode("ascii"))
    binding_sig = sign_binding_for_test(
        binding, private_key_path=private_key, public_key=public_key
    )
    names = release_asset_basenames(version)
    output_dir.mkdir(parents=True, exist_ok=True)
    mapping = {
        names["publication_evidence"]: evidence,
        names["publication_binding"]: binding,
        names["binding_signature"]: binding_sig,
    }
    for name, content in mapping.items():
        path = output_dir / name
        if path.exists():
            raise SystemExit(f"refusing to overwrite {path}")
        path.write_bytes(content)
    print("fixture_only=true")
    print("not_production_ci_component_provenance=true")
    print(f"release_manifest_sha256={hashlib.sha256(release).hexdigest()}")
    print(f"scriptlet_sha256={identity.sha256}")
    print(f"publication_evidence_sha256={hashlib.sha256(evidence).hexdigest()}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="FIXTURE-ONLY synthetic offline metadata emitter (not production ceremony)"
    )
    parser.add_argument("--installer", type=Path, required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--private-key", type=Path, required=True)
    parser.add_argument("--public-key", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    args = parser.parse_args(argv)
    emit_fixture_offline_metadata(
        installer=args.installer,
        tag=args.tag,
        private_key=args.private_key,
        public_key_hex=args.public_key,
        output_dir=args.output_dir,
        work_dir=args.work_dir,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
