#!/usr/bin/env python3
"""Ignite autonomous release orchestrator (Slice 3D-B2a.4.3q).

Commands:

  status   Read-only signing authority / pin / readiness report
  doctor   Verify local dependencies and configuration
  ship     Run the phased release workflow for a version
  resume   Resume an interrupted ship journal for a version

Live GitHub mutation, production-key activation against a maintainer host, and
Kindle publication require explicit configuration (IGNITE_SHIP_HOME,
IGNITE_SHIP_RECOVERY_DIR, optional IGNITE_SHIP_ALLOW_LIVE). This engineering
checkout leaves changes unstaged and does not execute live publish.

Security tradeoff: unattended signing requires a connected, controlled signing
host. A compromised host or authorized signing-service identity can compromise
publisher authority. The system is not air-gapped.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

_RELEASE = Path(__file__).resolve().parent
if str(_RELEASE) not in sys.path:
    sys.path.insert(0, str(_RELEASE))

from ship_lib.config import load_ship_config  # noqa: E402
from ship_lib.orchestrator import ShipHooks, ShipOrchestrator  # noqa: E402
from ship_lib.outcomes import SHIP_EXIT  # noqa: E402


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=None,
        help="repository root (default: inferred from this file)",
    )
    parser.add_argument(
        "--ship-home",
        type=Path,
        default=None,
        help="override IGNITE_SHIP_HOME (outside the repository)",
    )
    parser.add_argument(
        "--recovery-dir",
        type=Path,
        default=None,
        help="override IGNITE_SHIP_RECOVERY_DIR (independent durable backup root)",
    )
    parser.add_argument(
        "--allow-live",
        action="store_true",
        help="permit live GitHub/CI/publish mutation when fully configured",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("status", help="read-only readiness report")
    sub.add_parser("doctor", help="verify local ship prerequisites")

    ship = sub.add_parser("ship", help="autonomous release workflow for a version")
    ship.add_argument("--version", required=True, help="release version e.g. 0.9.15")
    ship.add_argument(
        "--execute-live",
        action="store_true",
        help="execute live git/CI/publish phases (default: plan/defer non-local phases)",
    )
    ship.add_argument(
        "--require-hardware",
        action=argparse.BooleanOptionalAction,
        default=None,
        help="require designated Kindle hardware acceptance before publish",
    )

    resume = sub.add_parser("resume", help="resume an interrupted ship journal")
    resume.add_argument("--version", required=True)
    resume.add_argument("--execute-live", action="store_true")

    args = parser.parse_args(argv)
    config = load_ship_config(
        repo_root=args.repo_root,
        ship_home=args.ship_home,
        recovery_dir=args.recovery_dir,
        allow_live=bool(args.allow_live),
        require_hardware=args.require_hardware
        if getattr(args, "require_hardware", None) is not None
        else None,
    )
    hooks = ShipHooks(
        execute_live_mutations=bool(getattr(args, "execute_live", False))
        and config.allow_live
    )
    orch = ShipOrchestrator(config, hooks=hooks)

    if args.command == "status":
        report = orch.status()
    elif args.command == "doctor":
        report = orch.doctor()
    elif args.command == "ship":
        report = orch.ship(args.version, resume=False)
    elif args.command == "resume":
        report = orch.ship(args.version, resume=True)
    else:  # pragma: no cover
        parser.error(f"unknown command {args.command}")
        return 2

    for line in report.lines:
        print(line)
    print(f"ship_outcome={report.outcome.value}")
    return SHIP_EXIT[report.outcome]


if __name__ == "__main__":
    raise SystemExit(main())
