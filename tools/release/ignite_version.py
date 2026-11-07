#!/usr/bin/env python3
"""Standalone Ignite release-version tool.

Commands:

  check           Read-only consistency check (tools/ci/check_version_sync.py)
  plan VERSION    Show files and fields that would change (no writes)
  set VERSION     Apply VERSION across release-version files

``ignite/VERSION`` is the single authoritative version. This tool reuses
``ship_lib.version_sync`` and ``tools/ci/check_version_sync.py``. It does not
commit, tag, push, publish, sign, write changelog entries, or touch hardware.

Examples (Windows PowerShell and Linux):

  python tools/release/ignite_version.py check
  python tools/release/ignite_version.py plan 0.9.22
  python tools/release/ignite_version.py set 0.9.22
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

_RELEASE = Path(__file__).resolve().parent
if str(_RELEASE) not in sys.path:
    sys.path.insert(0, str(_RELEASE))

from ship_lib.errors import ShipError  # noqa: E402
from ship_lib.version_sync import (  # noqa: E402
    apply_version,
    check_version,
    format_plan,
    plan_version,
)


def _repo_root_from_cli(path: Path | None) -> Path:
    if path is not None:
        return path.resolve()
    return Path(__file__).resolve().parents[2]


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
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("check", help="read-only version consistency check")

    plan = sub.add_parser(
        "plan",
        help="show exact file/field changes without modifying anything",
    )
    plan.add_argument("version", help="target release version e.g. 0.9.22")

    set_cmd = sub.add_parser(
        "set",
        help="write the target version across release-version files",
    )
    set_cmd.add_argument("version", help="target release version e.g. 0.9.22")

    args = parser.parse_args(argv)
    repo_root = _repo_root_from_cli(args.repo_root)

    try:
        if args.command == "check":
            check_version(repo_root, passthrough=True)
            return 0
        if args.command == "plan":
            plan_result = plan_version(
                repo_root, args.version, require_complete=True
            )
            for line in format_plan(plan_result):
                print(line)
            return 0
        if args.command == "set":
            plan_result = plan_version(
                repo_root, args.version, require_complete=True
            )
            for line in format_plan(plan_result):
                print(line)
            changed = apply_version(
                repo_root, args.version, require_complete=True
            )
            label = ",".join(changed) if changed else "none"
            print(f"VERSION SET: changed={label}")
            return 0
    except ShipError as exc:
        print(f"ignite_version.py: {exc}", file=sys.stderr)
        return 1

    parser.error(f"unknown command {args.command}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
