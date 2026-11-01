#!/usr/bin/env python3
"""Authenticated Kindle release publication preflight (Slice 3D-B2a.4.3k).

Validates the six-asset authenticated Kindle bundle without publishing,
without rebuilding customer Scriptlet bytes, and without treating fixture
success as production-promotion readiness.

Trust contexts:

  rehearse --trust fixture  — disposable public key; fixture_rehearsal_ready only
  rehearse --trust production / promote-gate
                            — repository pin only; refuses while UNPROVISIONED

``promote-gate`` authenticates a completed release-candidate directory (the
files that would be published). It never accepts an alternate public-key path.
CLI success requires ``production_promotion_ready=true``.

Operator-supplied ``--source-revision`` / ``--expected-workflow`` labels are
recorded as assertions only. CI provenance confirmation remains the shell
resolve/verify path.

This module performs no GitHub mutation and never loads a production private key.

Remaining check-to-use limitation: filesystem staging + inventory recheck is
not race-free against a hostile concurrent writer between the final inventory
recheck and ``gh release create``.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import stat
import sys
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parents[1]
_RELEASE_TOOLS = _REPO / "tools" / "release"
_PRODUCTION_PIN = _RELEASE_TOOLS / "kindle-release-public-key.hex"
for extra in (_HERE, str(_RELEASE_TOOLS)):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from authenticated_release_assets import (  # noqa: E402
    measure_scriptlet,
    release_asset_basenames,
)
from kindle_release_sign import (  # noqa: E402
    KNOWN_TEST_PUBLIC_KEYS,
    UNPROVISIONED,
    ReleaseSigningError,
    load_public_key,
    version_from_tag,
)
from production_trust_boundary import (  # noqa: E402
    EvidenceClaimStatus,
    ProductionTrustError,
    VerificationProvenance,
    authenticate_official_publication_chain,
    authenticate_publication_chain_with_key,
    refuse_for_production_publication_evidence,
)

READ_CHUNK = 1024 * 1024
MAX_RELEASE_MANIFEST_BYTES = 1024
MAX_SIGNATURE_BYTES = 512
MAX_EVIDENCE_BYTES = 64 * 1024
MAX_BINDING_BYTES = 2048


class PreflightError(RuntimeError):
    """Fail-closed preflight refusal."""


class TrustContext(Enum):
    FIXTURE = "fixture"
    PRODUCTION = "production"


@dataclass(frozen=True)
class AssetDigest:
    name: str
    bytes_count: int
    sha256: str


@dataclass
class PreflightReport:
    version: str
    tag: str
    trust_context: TrustContext
    verification_provenance: str
    production_signing: str
    fixture_rehearsal_ready: bool
    production_promotion_ready: bool
    publisher_authenticated: bool = False
    operator_asserted_source_revision: str | None = None
    operator_asserted_workflow: str | None = None
    ci_provenance_verified_by_preflight: bool = False
    assets: list[AssetDigest] = field(default_factory=list)
    detail: list[str] = field(default_factory=list)

    def summary_lines(self) -> list[str]:
        lines = [
            "IGNITE_AUTHENTICATED_PUBLICATION_PREFLIGHT_V1",
            "schema=1",
            f"version={self.version}",
            f"tag={self.tag}",
            f"trust_context={self.trust_context.value}",
            f"verification_provenance={self.verification_provenance}",
            f"production_signing={self.production_signing}",
            f"publisher_authenticated={'true' if self.publisher_authenticated else 'false'}",
            f"fixture_rehearsal_ready={'true' if self.fixture_rehearsal_ready else 'false'}",
            f"production_promotion_ready={'true' if self.production_promotion_ready else 'false'}",
            "ci_provenance_verified_by_preflight=false",
            "github_mutation=false",
            "scriptlet_executed=false",
            "customer_artifact_rebuilt=false",
        ]
        if self.operator_asserted_source_revision is not None:
            lines.append(
                "operator_asserted_source_revision="
                f"{self.operator_asserted_source_revision}"
            )
            lines.append(
                "operator_asserted_source_revision_is_not_ci_provenance_proof=true"
            )
        if self.operator_asserted_workflow is not None:
            lines.append(
                f"operator_asserted_workflow={self.operator_asserted_workflow}"
            )
            lines.append("operator_asserted_workflow_is_not_ci_provenance_proof=true")
        for asset in self.assets:
            lines.append(
                f"asset={asset.name}|bytes={asset.bytes_count}|sha256={asset.sha256}"
            )
        lines.extend(self.detail)
        return lines


def _refuse_symlink_or_nonfile(path: Path, *, label: str) -> None:
    try:
        meta = path.lstat()
    except OSError as exc:
        raise PreflightError(f"cannot stat {label}: {exc}") from exc
    if stat.S_ISLNK(meta.st_mode):
        raise PreflightError(f"symlink refused for authenticated asset {label}")
    if not stat.S_ISREG(meta.st_mode):
        raise PreflightError(f"authenticated asset {label} is not a regular file")


def _read_bounded(path: Path, *, label: str, max_bytes: int) -> bytes:
    """Length-limited, non-following read of an authenticated metadata file."""
    _refuse_symlink_or_nonfile(path, label=label)
    size = 0
    chunks: list[bytes] = []
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(min(READ_CHUNK, max_bytes - size + 1))
            if not chunk:
                break
            size += len(chunk)
            if size > max_bytes:
                raise PreflightError(
                    f"oversized required asset: {label} exceeds {max_bytes} bytes"
                )
            chunks.append(chunk)
    data = b"".join(chunks)
    if not data:
        raise PreflightError(f"empty required asset: {label}")
    return data


def _digest_bytes(name: str, data: bytes) -> AssetDigest:
    return AssetDigest(
        name=name, bytes_count=len(data), sha256=hashlib.sha256(data).hexdigest()
    )


def _production_signing_status(pin_path: Path = _PRODUCTION_PIN) -> str:
    try:
        raw = pin_path.read_bytes()
    except OSError:
        return "unavailable"
    if raw == UNPROVISIONED:
        return "unavailable"
    try:
        key = load_public_key(pin_path, official=True)
    except ReleaseSigningError:
        return "unavailable"
    if key in KNOWN_TEST_PUBLIC_KEYS:
        return "unavailable"
    return "pin-present"


def load_bundle_bytes(
    *,
    assets_dir: Path,
    version: str,
    scriptlet_path: Path | None = None,
) -> tuple[dict[str, bytes | Path], list[AssetDigest]]:
    """Load the six authenticated Kindle assets; digests describe authenticated bytes."""
    names = release_asset_basenames(version)
    scriptlet = scriptlet_path or (assets_dir / names["scriptlet"])
    _refuse_symlink_or_nonfile(scriptlet, label=names["scriptlet"])

    found = {p.name for p in assets_dir.iterdir() if p.is_file() or p.is_symlink()}
    required_meta = {
        names["release_manifest"],
        names["release_signature"],
        names["publication_evidence"],
        names["publication_binding"],
        names["binding_signature"],
    }
    missing = sorted(required_meta - found)
    if missing:
        raise PreflightError(
            "missing required authenticated assets: " + ", ".join(missing)
        )

    release_manifest = _read_bounded(
        assets_dir / names["release_manifest"],
        label=names["release_manifest"],
        max_bytes=MAX_RELEASE_MANIFEST_BYTES,
    )
    release_signature = _read_bounded(
        assets_dir / names["release_signature"],
        label=names["release_signature"],
        max_bytes=MAX_SIGNATURE_BYTES,
    )
    evidence = _read_bounded(
        assets_dir / names["publication_evidence"],
        label=names["publication_evidence"],
        max_bytes=MAX_EVIDENCE_BYTES,
    )
    binding = _read_bounded(
        assets_dir / names["publication_binding"],
        label=names["publication_binding"],
        max_bytes=MAX_BINDING_BYTES,
    )
    binding_sig = _read_bounded(
        assets_dir / names["binding_signature"],
        label=names["binding_signature"],
        max_bytes=MAX_SIGNATURE_BYTES,
    )
    identity = measure_scriptlet(
        scriptlet, expected_name=f"Install Ignite v{version}.sh"
    )
    digests = [
        AssetDigest(
            name=names["scriptlet"], bytes_count=identity.size, sha256=identity.sha256
        ),
        _digest_bytes(names["release_manifest"], release_manifest),
        _digest_bytes(names["release_signature"], release_signature),
        _digest_bytes(names["publication_evidence"], evidence),
        _digest_bytes(names["publication_binding"], binding),
        _digest_bytes(names["binding_signature"], binding_sig),
    ]
    return (
        {
            "scriptlet_path": scriptlet,
            "scriptlet_identity": identity,
            "release_manifest": release_manifest,
            "release_signature": release_signature,
            "publication_evidence": evidence,
            "publication_binding": binding,
            "binding_signature": binding_sig,
        },
        digests,
    )


def run_preflight(
    *,
    assets_dir: Path,
    expected_tag: str,
    trust: TrustContext,
    public_key_path: Path | None = None,
    scriptlet_path: Path | None = None,
    source_revision: str | None = None,
    expected_workflow: str | None = None,
    require_production_claims: bool = True,
) -> PreflightReport:
    """Validate the six-asset bundle under the selected trust context."""
    try:
        version = version_from_tag(expected_tag)
    except ReleaseSigningError as exc:
        raise PreflightError(str(exc)) from exc

    production_signing = _production_signing_status()
    bundle, assets = load_bundle_bytes(
        assets_dir=assets_dir, version=version, scriptlet_path=scriptlet_path
    )
    identity = bundle["scriptlet_identity"]
    release_manifest = bundle["release_manifest"]
    release_signature = bundle["release_signature"]
    evidence = bundle["publication_evidence"]
    binding = bundle["publication_binding"]
    binding_sig = bundle["binding_signature"]
    assert not isinstance(identity, (bytes, Path))
    assert isinstance(release_manifest, bytes)
    assert isinstance(release_signature, bytes)
    assert isinstance(evidence, bytes)
    assert isinstance(binding, bytes)
    assert isinstance(binding_sig, bytes)

    detail: list[str] = []
    try:
        if trust is TrustContext.FIXTURE:
            if public_key_path is None:
                raise PreflightError("fixture trust requires --public-key")
            if public_key_path.resolve() == _PRODUCTION_PIN.resolve():
                raise PreflightError(
                    "refusing to use the production public-key pin as a fixture key"
                )
            public_key = load_public_key(public_key_path, official=False)
            if public_key in KNOWN_TEST_PUBLIC_KEYS:
                detail.append("fixture_key_class=known-test-vector")
            auth = authenticate_publication_chain_with_key(
                release_manifest=release_manifest,
                release_signature=release_signature,
                binding_manifest=binding,
                binding_signature=binding_sig,
                publication_evidence=evidence,
                expected_tag=expected_tag,
                public_key=public_key,
            )
            provenance = "verified-under-test-only-fixture-key"
        else:
            if production_signing == "unavailable":
                raise PreflightError(
                    "production signing unavailable because the pin is UNPROVISIONED "
                    "or otherwise unusable"
                )
            refuse_for_production_publication_evidence(evidence)
            auth = authenticate_official_publication_chain(
                release_manifest=release_manifest,
                release_signature=release_signature,
                binding_manifest=binding,
                binding_signature=binding_sig,
                publication_evidence=evidence,
                expected_tag=expected_tag,
            )
            provenance = "verified-under-production-pin"
    except (ProductionTrustError, ReleaseSigningError) as exc:
        raise PreflightError(str(exc)) from exc

    if (
        auth.release.sha256 != identity.sha256
        or auth.release.bytes_count != identity.size
    ):
        raise PreflightError(
            "authenticated release-v1 identity does not match Scriptlet bytes on disk"
        )
    if require_production_claims:
        if auth.evidence_claims is not EvidenceClaimStatus.PRODUCTION_CLAIMS_VALIDATED:
            raise PreflightError("production evidence claims are not validated")

    detail.append(f"evidence_claims={auth.evidence_claims.value}")
    detail.append(f"publisher_provenance={auth.provenance.value}")
    if auth.provenance is VerificationProvenance.TEST_ONLY_FIXTURE_KEY:
        detail.append("fixture_success_is_not_production_promotion_readiness=true")
        detail.append(
            "synthetic_or_fixture_payload_is_not_verified_ci_component_provenance=true"
        )

    fixture_ready = trust is TrustContext.FIXTURE and (
        auth.evidence_claims is EvidenceClaimStatus.PRODUCTION_CLAIMS_VALIDATED
    )
    production_ready = (
        trust is TrustContext.PRODUCTION
        and auth.provenance is VerificationProvenance.OFFICIAL_PRODUCTION_PIN
        and auth.evidence_claims is EvidenceClaimStatus.PRODUCTION_CLAIMS_VALIDATED
        and production_signing == "pin-present"
    )

    return PreflightReport(
        version=version,
        tag=expected_tag,
        trust_context=trust,
        verification_provenance=provenance,
        production_signing=production_signing,
        fixture_rehearsal_ready=fixture_ready,
        production_promotion_ready=production_ready,
        publisher_authenticated=True,
        operator_asserted_source_revision=source_revision,
        operator_asserted_workflow=expected_workflow,
        assets=assets,
        detail=detail,
    )


def run_promote_gate(
    *,
    release_candidate_dir: Path,
    tag: str,
    source_revision: str | None = None,
    expected_workflow: str | None = None,
) -> PreflightReport:
    """Authenticate the six Kindle assets inside a completed release candidate.

    Uses only this tree's repository production pin. No alternate public key.
    Succeeds only when ``production_promotion_ready`` is true.
    """
    # Refuse shipping-path verifier substitution before any trust work.
    if os.environ.get("IGNITE_PROMOTE_PREFLIGHT"):
        raise PreflightError(
            "IGNITE_PROMOTE_PREFLIGHT is not accepted by the production promotion gate"
        )

    pin = _PRODUCTION_PIN.read_bytes()
    if pin == UNPROVISIONED:
        raise PreflightError(
            "production signing unavailable because the pin is UNPROVISIONED; "
            "authenticated six-asset publication remains blocked"
        )

    if not release_candidate_dir.is_dir():
        raise PreflightError(
            f"release candidate directory missing: {release_candidate_dir}"
        )

    report = run_preflight(
        assets_dir=release_candidate_dir,
        expected_tag=tag,
        trust=TrustContext.PRODUCTION,
        scriptlet_path=None,
        source_revision=source_revision,
        expected_workflow=expected_workflow,
        require_production_claims=True,
    )
    if not report.production_promotion_ready:
        raise PreflightError(
            "promote-gate refused: production_promotion_ready=false "
            "(fixture rehearsal is not publication authority)"
        )
    report.detail.append("authenticated_from_release_candidate_directory=true")
    return report


def _print_report(report: PreflightReport) -> None:
    sys.stdout.write("\n".join(report.summary_lines()) + "\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Non-publishing authenticated Kindle release publication preflight"
    )
    sub = parser.add_subparsers(dest="command", required=True)

    rehearse = sub.add_parser("rehearse", help="Validate a local six-asset directory")
    rehearse.add_argument("--assets-dir", type=Path, required=True)
    rehearse.add_argument("--expected-tag", required=True)
    rehearse.add_argument(
        "--trust",
        choices=("fixture", "production"),
        required=True,
        help="fixture never reports production_promotion_ready",
    )
    rehearse.add_argument("--public-key", type=Path, default=None)
    rehearse.add_argument("--scriptlet", type=Path, default=None)
    rehearse.add_argument(
        "--source-revision",
        default=None,
        help="operator assertion only; not CI provenance proof",
    )
    rehearse.add_argument(
        "--expected-workflow",
        default=None,
        help="operator assertion only; not CI provenance proof",
    )

    gate = sub.add_parser(
        "promote-gate",
        help="Authenticate a staged release candidate under the repository pin",
    )
    gate.add_argument(
        "--release-candidate-dir",
        type=Path,
        required=True,
        help="Directory containing the exact files intended for publication",
    )
    gate.add_argument("--tag", required=True)
    gate.add_argument("--source-revision", default=None)
    gate.add_argument("--expected-workflow", default=None)

    sub.add_parser(
        "production-status",
        help="Report whether production signing is available (read-only)",
    )

    args = parser.parse_args(argv)
    try:
        if args.command == "production-status":
            signing = _production_signing_status()
            print("IGNITE_PRODUCTION_SIGNING_STATUS_V1")
            print("schema=1")
            print(f"production_signing={signing}")
            print("production_promotion_ready=false")
            print("github_mutation=false")
            return 0
        if args.command == "rehearse":
            report = run_preflight(
                assets_dir=args.assets_dir,
                expected_tag=args.expected_tag,
                trust=TrustContext.FIXTURE
                if args.trust == "fixture"
                else TrustContext.PRODUCTION,
                public_key_path=args.public_key,
                scriptlet_path=args.scriptlet,
                source_revision=args.source_revision,
                expected_workflow=args.expected_workflow,
            )
            _print_report(report)
            # Fixture rehearsal exits 0 on cryptographic success even when
            # production_promotion_ready is false.
            if args.trust == "production" and not report.production_promotion_ready:
                raise PreflightError(
                    "production rehearsal refused: production_promotion_ready=false"
                )
            return 0
        if args.command == "promote-gate":
            report = run_promote_gate(
                release_candidate_dir=args.release_candidate_dir,
                tag=args.tag,
                source_revision=args.source_revision,
                expected_workflow=args.expected_workflow,
            )
            _print_report(report)
            return 0
    except PreflightError as exc:
        print(f"authenticated_publication_preflight: {exc}", file=sys.stderr)
        return 1
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
