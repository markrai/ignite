#!/usr/bin/env python3
"""Offline packaging measurement handoff (Slice 3D-B2a.4.3l).

Public CI measurement material → independent offline validation → disposable
fixture signing rehearsal. Production private-key custody, operator identities,
and promote upload transport are **not** chosen here.

Never:
  - mutates GitHub Releases / tags
  - provisions or reads a production private key
  - changes the UNPROVISIONED production pin
  - executes the Scriptlet
  - claims CI provenance solely from operator-asserted revision fields
  - claims production publisher authority from fixture keys
"""

from __future__ import annotations

import argparse
import hashlib
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_SFX = _HERE / "sfx"
_RELEASE = _HERE.parents[1] / "tools" / "release"
for extra in (_HERE, _SFX, str(_RELEASE)):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from authenticated_publication_preflight import (  # noqa: E402
    PreflightError,
    TrustContext,
    run_preflight,
)
from authenticated_release_assets import (  # noqa: E402
    AuthenticatedAssetError,
    AuthenticatedReleaseAssetSet,
    assert_scriptlet_unchanged,
    build_release_v1_from_identity,
    load_fixture_public_key,
    measure_scriptlet,
    release_asset_basenames,
    write_authenticated_release_assets,
)
from ci_packaging_measurement import (  # noqa: E402
    MAX_MEASUREMENT_BYTES,
    MAX_PROFILE_BYTES,
    MEASUREMENT_SCOPE,
    MeasurementError,
    build_production_evidence_from_measurement,
    parse_ci_packaging_measurement,
    read_bounded_plain_file,
    verify_measurement_against_scriptlet,
)
from production_trust_boundary import (  # noqa: E402
    ProductionTrustError,
    build_publication_binding,
    refuse_fixture_use_of_production_pin,
    sign_binding_for_test,
    sign_release_for_test,
)

HANDOFF_HEADER = "IGNITE_OFFLINE_PACKAGING_HANDOFF_V1"
HANDOFF_SCHEMA = "1"


class HandoffError(ValueError):
    """Fail-closed offline handoff refusal."""


@dataclass(frozen=True)
class HandoffVerifyResult:
    measurement_ok: bool
    evidence_ready: bool
    ci_provenance_established: bool
    production_authority: bool
    github_mutation: bool
    scriptlet_executed: bool
    version: str
    tag: str
    detail: tuple[str, ...]
    measurement_bytes: bytes | None = None
    package_profile_bytes: bytes | None = None

    def summary_lines(self) -> list[str]:
        lines = [
            HANDOFF_HEADER,
            f"schema={HANDOFF_SCHEMA}",
            f"version={self.version}",
            f"tag={self.tag}",
            f"measurement_ok={'true' if self.measurement_ok else 'false'}",
            f"offline_evidence_inputs_ready={'true' if self.evidence_ready else 'false'}",
            "ci_provenance_established_by_this_verifier=false",
            "production_publisher_authority=false",
            "measurement_is_not_publisher_signed=true",
            "fixture_signing_is_not_production_authority=true",
            "github_mutation=false",
            "scriptlet_executed=false",
            f"measurement_scope={MEASUREMENT_SCOPE}",
            "production_promotion_ready=false",
            "promote_handoff_configured=false",
            "unsigned_ops_independent_of_production_pin=true",
        ]
        lines.extend(self.detail)
        return lines


def verify_handoff_inputs(
    *,
    scriptlet: Path,
    measurement_path: Path,
    package_profile_path: Path | None,
    expected_version: str | None = None,
    expected_source_revision: str | None = None,
) -> HandoffVerifyResult:
    """Read-only verification of Scriptlet ↔ measurement ↔ optional profile.

    Does not depend on production pin provisioning state. Never grants publisher
    authority or production promotion readiness.
    """
    try:
        measurement = read_bounded_plain_file(
            measurement_path,
            max_bytes=MAX_MEASUREMENT_BYTES,
            label="measurement",
        )
    except MeasurementError as exc:
        if "cannot lstat" in str(exc) or "cannot open" in str(exc):
            raise HandoffError("missing measurement artifact") from exc
        raise HandoffError(str(exc)) from exc
    profile: bytes | None = None
    if package_profile_path is not None:
        try:
            profile = read_bounded_plain_file(
                package_profile_path,
                max_bytes=MAX_PROFILE_BYTES,
                label="package-profile",
            )
        except MeasurementError as exc:
            raise HandoffError(str(exc)) from exc
    try:
        result = verify_measurement_against_scriptlet(
            measurement=measurement,
            scriptlet=scriptlet,
            package_profile=profile,
            expected_version=expected_version,
            expected_source_revision=expected_source_revision,
        )
    except MeasurementError as exc:
        raise HandoffError(str(exc)) from exc

    evidence_ready = (
        result.scriptlet_integrity
        and result.scriptlet_identity_match
        and result.component_consistency
        and result.package_profile_match
    )
    return HandoffVerifyResult(
        measurement_ok=result.scriptlet_identity_match and result.component_consistency,
        evidence_ready=evidence_ready,
        ci_provenance_established=False,
        production_authority=False,
        github_mutation=False,
        scriptlet_executed=False,
        version=result.version,
        tag=result.tag,
        detail=result.detail
        + (
            "require_trusted_promotion_path_for_ci_provenance=true",
            "require_maintainer_key_ceremony_for_production_authority=true",
        ),
        measurement_bytes=measurement,
        package_profile_bytes=profile,
    )


def prepare_evidence_from_handoff(
    *,
    scriptlet: Path,
    measurement_path: Path,
    package_profile_path: Path,
    expected_version: str | None = None,
) -> bytes:
    """Build production-format evidence bytes from validated CI measurements.

    Independent of production pin provisioning. Does not sign and does not grant
    publisher authority. Reads measurement/profile once and reuses those bytes.
    """
    handoff = verify_handoff_inputs(
        scriptlet=scriptlet,
        measurement_path=measurement_path,
        package_profile_path=package_profile_path,
        expected_version=expected_version,
    )
    if not handoff.evidence_ready:
        raise HandoffError(
            "measurement not ready for evidence generation "
            "(package profile required and must match)"
        )
    if handoff.measurement_bytes is None or handoff.package_profile_bytes is None:
        raise HandoffError("internal handoff missing validated measurement bytes")
    try:
        return build_production_evidence_from_measurement(
            handoff.measurement_bytes,
            scriptlet=scriptlet,
            package_profile=handoff.package_profile_bytes,
        )
    except MeasurementError as exc:
        raise HandoffError(str(exc)) from exc


def rehearse_fixture_signing_from_measurement(
    *,
    version: str,
    scriptlet: Path,
    measurement_path: Path,
    package_profile_path: Path,
    private_key_path: Path,
    public_key_path: Path,
    output_dir: Path,
) -> AuthenticatedReleaseAssetSet:
    """Full fixture six-asset rehearsal from validated CI measurement inputs.

    Disposable keys only. Never production authority. Never GitHub mutation.
    """
    # Fixture signing stays isolated from production authority without requiring
    # the repository pin to remain permanently UNPROVISIONED.
    try:
        refuse_fixture_use_of_production_pin(
            private_key_path=private_key_path, public_key_path=public_key_path
        )
    except ProductionTrustError as exc:
        raise HandoffError(str(exc)) from exc
    if (
        private_key_path.resolve()
        == (_RELEASE / "kindle-release-public-key.hex").resolve()
    ):
        raise HandoffError("refusing production pin path as fixture private key")
    public_key = load_fixture_public_key(public_key_path)

    handoff = verify_handoff_inputs(
        scriptlet=scriptlet,
        measurement_path=measurement_path,
        package_profile_path=package_profile_path,
        expected_version=version,
    )
    if not handoff.evidence_ready:
        raise HandoffError(
            "measurement not ready for evidence generation "
            "(package profile required and must match)"
        )
    if handoff.measurement_bytes is None or handoff.package_profile_bytes is None:
        raise HandoffError("internal handoff missing validated measurement bytes")
    try:
        evidence = build_production_evidence_from_measurement(
            handoff.measurement_bytes,
            scriptlet=scriptlet,
            package_profile=handoff.package_profile_bytes,
        )
    except MeasurementError as exc:
        raise HandoffError(str(exc)) from exc
    parsed = parse_ci_packaging_measurement(handoff.measurement_bytes)
    identity = measure_scriptlet(
        scriptlet, expected_name=f"Install Ignite v{version}.sh"
    )
    if (
        identity.size != parsed["scriptlet_bytes"]
        or identity.sha256 != parsed["scriptlet_sha256"]
    ):
        raise HandoffError("scriptlet mutated after measurement verification")

    release_manifest = build_release_v1_from_identity(
        version=version, identity=identity
    )
    binding = build_publication_binding(
        release_tag=f"v{version}",
        release_version=version,
        release_manifest=release_manifest,
        scriptlet_sha256=identity.sha256,
        publication_evidence=evidence,
    )
    assert_scriptlet_unchanged(identity)
    try:
        release_sig = sign_release_for_test(
            release_manifest, private_key_path=private_key_path, public_key=public_key
        )
        binding_sig = sign_binding_for_test(
            binding, private_key_path=private_key_path, public_key=public_key
        )
    except Exception as exc:  # noqa: BLE001 — surface signer refusals as handoff errors
        raise HandoffError(f"fixture signing failed: {exc}") from exc
    assert_scriptlet_unchanged(identity)

    assets = AuthenticatedReleaseAssetSet(
        version=version,
        tag=f"v{version}",
        scriptlet_path=scriptlet,
        scriptlet_sha256=identity.sha256,
        scriptlet_bytes=identity.size,
        release_manifest=release_manifest,
        release_signature=release_sig,
        publication_evidence=evidence,
        publication_binding=binding,
        binding_signature=binding_sig,
        package_profile_sha256=parsed["package_profile_sha256"],
    )
    output_dir.mkdir(parents=True, exist_ok=True)
    # Copy Scriptlet into the output bundle so preflight sees all six colocated.
    names = release_asset_basenames(version)
    scriptlet_dest = output_dir / names["scriptlet"]
    if scriptlet_dest.resolve() != scriptlet.resolve():
        if scriptlet_dest.exists():
            raise HandoffError(f"refusing to overwrite {scriptlet_dest.name}")
        shutil.copy2(scriptlet, scriptlet_dest)
        assets = AuthenticatedReleaseAssetSet(
            version=assets.version,
            tag=assets.tag,
            scriptlet_path=scriptlet_dest,
            scriptlet_sha256=assets.scriptlet_sha256,
            scriptlet_bytes=assets.scriptlet_bytes,
            release_manifest=assets.release_manifest,
            release_signature=assets.release_signature,
            publication_evidence=assets.publication_evidence,
            publication_binding=assets.publication_binding,
            binding_signature=assets.binding_signature,
            package_profile_sha256=assets.package_profile_sha256,
        )
    try:
        write_authenticated_release_assets(assets, output_dir)
    except AuthenticatedAssetError as exc:
        raise HandoffError(str(exc)) from exc
    return assets


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    verify = sub.add_parser("verify", help="Read-only measurement handoff verification")
    verify.add_argument("--scriptlet", type=Path, required=True)
    verify.add_argument("--measurement", type=Path, required=True)
    verify.add_argument("--package-profile", type=Path, default=None)
    verify.add_argument("--expected-version", default=None)
    verify.add_argument("--expected-source-revision", default=None)

    prepare = sub.add_parser(
        "prepare-evidence",
        help="Build production-format evidence from validated measurements",
    )
    prepare.add_argument("--scriptlet", type=Path, required=True)
    prepare.add_argument("--measurement", type=Path, required=True)
    prepare.add_argument("--package-profile", type=Path, required=True)
    prepare.add_argument("--expected-version", default=None)
    prepare.add_argument("--out", type=Path, required=True)

    rehearse = sub.add_parser(
        "rehearse-fixture",
        help="Fixture six-asset signing from measurements (disposable keys only)",
    )
    rehearse.add_argument("--version", required=True)
    rehearse.add_argument("--scriptlet", type=Path, required=True)
    rehearse.add_argument("--measurement", type=Path, required=True)
    rehearse.add_argument("--package-profile", type=Path, required=True)
    rehearse.add_argument("--fixture-private-key", type=Path, required=True)
    rehearse.add_argument("--fixture-public-key", type=Path, required=True)
    rehearse.add_argument("--out-dir", type=Path, required=True)
    rehearse.add_argument(
        "--run-preflight",
        action="store_true",
        help="Also run fixture authenticated publication preflight",
    )
    rehearse.add_argument("--source-revision", default=None)

    args = parser.parse_args(argv)
    try:
        if args.command == "verify":
            result = verify_handoff_inputs(
                scriptlet=args.scriptlet,
                measurement_path=args.measurement,
                package_profile_path=args.package_profile,
                expected_version=args.expected_version,
                expected_source_revision=args.expected_source_revision,
            )
            sys.stdout.write("\n".join(result.summary_lines()) + "\n")
            return 0 if result.measurement_ok else 1
        if args.command == "prepare-evidence":
            evidence = prepare_evidence_from_handoff(
                scriptlet=args.scriptlet,
                measurement_path=args.measurement,
                package_profile_path=args.package_profile,
                expected_version=args.expected_version,
            )
            if args.out.exists():
                raise HandoffError(f"refusing to overwrite {args.out}")
            args.out.parent.mkdir(parents=True, exist_ok=True)
            args.out.write_bytes(evidence)
            print(f"wrote={args.out}")
            print(f"evidence_sha256={hashlib.sha256(evidence).hexdigest()}")
            print("production_publisher_authority=false")
            print("github_mutation=false")
            return 0
        if args.command == "rehearse-fixture":
            assets = rehearse_fixture_signing_from_measurement(
                version=args.version,
                scriptlet=args.scriptlet,
                measurement_path=args.measurement,
                package_profile_path=args.package_profile,
                private_key_path=args.fixture_private_key,
                public_key_path=args.fixture_public_key,
                output_dir=args.out_dir,
            )
            print(f"fixture_assets_dir={args.out_dir}")
            print(f"scriptlet_sha256={assets.scriptlet_sha256}")
            print(f"package_profile_sha256={assets.package_profile_sha256}")
            print("fixture_signing_is_not_production_authority=true")
            print("production_promotion_ready=false")
            print("github_mutation=false")
            if args.run_preflight:
                try:
                    report = run_preflight(
                        assets_dir=args.out_dir,
                        expected_tag=f"v{args.version}",
                        trust=TrustContext.FIXTURE,
                        public_key_path=args.fixture_public_key,
                        source_revision=args.source_revision,
                        expected_workflow=None,
                    )
                except PreflightError as exc:
                    raise HandoffError(f"fixture preflight refused: {exc}") from exc
                sys.stdout.write("\n".join(report.summary_lines()) + "\n")
                if report.production_promotion_ready:
                    raise HandoffError(
                        "fixture rehearsal incorrectly claimed production_promotion_ready"
                    )
            return 0
    except (HandoffError, MeasurementError, AuthenticatedAssetError, OSError) as exc:
        print(f"offline_packaging_handoff: {exc}", file=sys.stderr)
        return 1
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
