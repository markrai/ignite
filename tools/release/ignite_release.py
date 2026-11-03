#!/usr/bin/env python3
"""Ignite release acquire / sign / dry-run orchestration (Slice 3D-B2a.4.3o).

Commands:

  acquire   Download and verify trusted CI artifacts for a release tag
  sign      Offline-sign an acquired bundle (explicit private key; no network)
  dry-run   Nonpublishing end-to-end validation including promote-release.sh dry-run

Does not provision the production pin, create GitHub Releases, or deploy to Kindle.
Private keys and passphrases are never copied into acquisition or handoff outputs.

``IGNITE_RELEASE_GH`` may point at an alternate ``gh`` executable (tests/mocks).
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

_ROOT = Path(__file__).resolve().parents[2]
_KINDLE = _ROOT / "deployment" / "kindle"
if str(_KINDLE) not in sys.path:
    sys.path.insert(0, str(_KINDLE))

from release_acquire import AcquireError, acquire_release  # noqa: E402
from release_orchestrate import (  # noqa: E402
    OrchestrateError,
    dry_run_release,
    sign_acquired,
)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    acquire = sub.add_parser(
        "acquire",
        help="Resolve a trusted ci.yml run and download verified public artifacts",
    )
    acquire.add_argument("--tag", required=True)
    acquire.add_argument("--output", type=Path, required=True)
    acquire.add_argument(
        "--run-id",
        type=int,
        default=None,
        help="explicit Actions run id (otherwise highest eligible within bounded search)",
    )
    acquire.add_argument(
        "--repository",
        default="markrai/ignite",
        help="GitHub repository (default markrai/ignite)",
    )

    sign = sub.add_parser(
        "sign",
        help="Offline-sign an acquired public-input bundle (explicit private key)",
    )
    sign.add_argument("--acquired", type=Path, required=True)
    sign.add_argument("--private-key", type=Path, required=True)
    sign.add_argument("--public-key", type=Path, required=True)
    sign.add_argument("--output", type=Path, required=True)

    dry = sub.add_parser(
        "dry-run",
        help="Nonpublishing acquire+match+nine-file+promote-release.sh dry-run",
    )
    dry.add_argument("--tag", required=True)
    dry.add_argument("--run-id", type=int, required=True)
    dry.add_argument("--signed-assets", type=Path, required=True)
    dry.add_argument(
        "--work",
        type=Path,
        required=True,
        help="unused work directory for acquisition staging and nine-file candidate",
    )
    dry.add_argument(
        "--acquired",
        type=Path,
        default=None,
        help="reuse an existing acquisition bundle instead of downloading",
    )
    dry.add_argument(
        "--public-key",
        type=Path,
        default=None,
        help="publisher/disposable public key (required while pin is UNPROVISIONED)",
    )
    dry.add_argument(
        "--skip-promote-shell",
        action="store_true",
        help="skip promote-release.sh dry-run (report not-performed)",
    )

    args = parser.parse_args(argv)
    try:
        if args.command == "acquire":
            bundle = acquire_release(
                tag=args.tag,
                output_dir=args.output,
                run_id=args.run_id,
                repository=args.repository,
            )
            print(f"acquired_dir={bundle.root}")
            print(f"acquisition_receipt={bundle.receipt_path}")
            print(f"release_identity={bundle.identity_path}")
            print(f"ci_artifacts={bundle.ci_artifacts}")
            print(f"linux_dir={bundle.linux_dir}")
            print(f"ci_run_id={bundle.identity.ci_run_id}")
            print(f"ci_run_attempt={bundle.identity.ci_run_attempt}")
            print(f"ci_head_sha={bundle.identity.ci_head_sha}")
            print(f"scriptlet_sha256={bundle.identity.scriptlet_sha256}")
            print(f"linux_bundle_sha256={bundle.identity.linux_bundle_sha256}")
            print("artifact_digest_kind=extracted-tree-sha256-not-github-archive-digest")
            print("github_api_provenance=associated-not-cryptographic-attestation")
            print("github_mutation=false")
            print("private_key_material=false")
            return 0
        if args.command == "sign":
            signed = sign_acquired(
                acquired=args.acquired,
                private_key=args.private_key,
                public_key=args.public_key,
                output_dir=args.output,
            )
            print(f"signed_dir={signed.root}")
            print(f"receipt={signed.receipt}")
            print(f"promote_authenticated_assets_dir={signed.promote_assets}")
            print("github_mutation=false")
            print("ci_provenance_established_by_this_ceremony=false")
            print("production_promotion_ready=false")
            return 0
        if args.command == "dry-run":
            report = dry_run_release(
                tag=args.tag,
                run_id=args.run_id,
                signed_assets=args.signed_assets,
                work_dir=args.work,
                acquired_dir=args.acquired,
                public_key=args.public_key,
                run_promote_shell=not args.skip_promote_shell,
            )
            sys.stdout.write("\n".join(report.lines) + "\n")
            # Exit status is driven solely by the stable dry_run_outcome field.
            # Partial / failed / not-performed never share complete-success exit 0.
            if report.exit_status != 0:
                print(
                    f"ignite_release: dry-run outcome={report.outcome.value}",
                    file=sys.stderr,
                )
            return report.exit_status
    except (AcquireError, OrchestrateError, OSError) as exc:
        print(f"ignite_release: {exc}", file=sys.stderr)
        return 1
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
