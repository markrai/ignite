#!/usr/bin/env python3
"""Reusable Ignite release qualification CLI.

Commands:

  qualify     Run selected automated groups; write JSON + summary
  preflight   Read-only Kindle inspection; never launches the app
  status      Recompute release states from existing evidence files

Does not install toolchains, mutate the Kindle, enable Vega, publish, tag, or
commit. Human approval remains required for device install and publication.

Examples:

  python tools/qa/ignite_qa.py qualify --groups identity
  python tools/qa/ignite_qa.py qualify --groups identity,rust,armv7,wasm,web,e2e
  python tools/qa/ignite_qa.py preflight --ssh-host kindle
  python tools/qa/ignite_qa.py status --qualify-json target/qa-evidence/qualify.json
"""

from __future__ import annotations

import argparse
import json
import shutil
import sys
from datetime import datetime, timezone
from pathlib import Path

_QA = Path(__file__).resolve().parent
if str(_QA) not in sys.path:
    sys.path.insert(0, str(_QA))

from env import git_identity, repo_root_from_here  # noqa: E402
from evidence import evaluate_states, human_summary  # noqa: E402
from preflight import run_preflight  # noqa: E402
from qualify import GROUPS, Qualify  # noqa: E402


def _parse_groups(raw: str | None) -> tuple[str, ...]:
    if not raw or raw.strip() == "all":
        return GROUPS
    names = tuple(part.strip() for part in raw.split(",") if part.strip())
    unknown = [n for n in names if n not in GROUPS]
    if unknown:
        raise SystemExit(f"unknown groups {unknown}; choose from {', '.join(GROUPS)}")
    return names


def _write(path: Path, payload: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


def _run_stamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def _preserve_evidence(out_dir: Path, name: str, payload: dict, summary: str, run_dir: Path) -> Path:
    """Write timestamped evidence first so a later run cannot clobber a failure."""
    json_stamped = run_dir / f"{name}.json"
    txt_stamped = run_dir / f"{name}.txt"
    _write(json_stamped, payload)
    txt_stamped.write_text(summary, encoding="utf-8")
    latest_json = out_dir / f"{name}.json"
    latest_txt = out_dir / f"{name}.txt"
    shutil.copyfile(json_stamped, latest_json)
    shutil.copyfile(txt_stamped, latest_txt)
    return latest_json


def _load_json(path: Path | None) -> dict | None:
    if path is None:
        return None
    return json.loads(path.read_text(encoding="utf-8"))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--repo-root", type=Path, default=None)
    parser.add_argument(
        "--out-dir",
        type=Path,
        default=None,
        help="evidence directory (default: <repo>/target/qa-evidence)",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    q = sub.add_parser("qualify", help="run automated qualification groups")
    q.add_argument(
        "--groups",
        default="all",
        help=f"comma-separated subset of {','.join(GROUPS)} (default all)",
    )

    p = sub.add_parser("preflight", help="read-only Kindle preflight (never launches Ignite)")
    p.add_argument("--ssh-host", default="kindle")
    p.add_argument(
        "--qualify-json",
        type=Path,
        default=None,
        help="prior qualify JSON whose artifact hashes should be compared",
    )

    s = sub.add_parser("status", help="evaluate release states from evidence files")
    s.add_argument("--qualify-json", type=Path, required=True)
    s.add_argument("--preflight-json", type=Path, default=None)
    s.add_argument("--physical-json", type=Path, default=None)

    args = parser.parse_args(argv)
    root = (args.repo_root or repo_root_from_here()).resolve()
    out_dir = (args.out_dir or (root / "target" / "qa-evidence")).resolve()
    run_dir = out_dir / _run_stamp()
    run_dir.mkdir(parents=True, exist_ok=True)

    if args.command == "qualify":
        groups = _parse_groups(args.groups)
        sys.stdout.write(f"evidence_run_dir={run_dir}\n")
        sys.stdout.flush()
        report = Qualify(root, out_dir=run_dir, groups=groups).run()
        summary = human_summary(report)
        json_path = _preserve_evidence(out_dir, "qualify", report, summary, run_dir)
        sys.stdout.write(summary)
        sys.stdout.write(f"evidence_json={json_path}\n")
        sys.stdout.write(f"evidence_run_dir={run_dir}\n")
        return int(report.get("exit_code") or 0)

    if args.command == "preflight":
        qualified = _load_json(args.qualify_json)
        report = run_preflight(ssh_host=args.ssh_host, qualified_artifacts=qualified)
        summary = human_summary(report)
        json_path = _preserve_evidence(out_dir, "preflight", report, summary, run_dir)
        sys.stdout.write(summary)
        sys.stdout.write(f"evidence_json={json_path}\n")
        sys.stdout.write(f"evidence_run_dir={run_dir}\n")
        if report.get("refuse_physical_qa"):
            sys.stdout.write("PHYSICAL_QA_ISOLATION_BLOCKED\n")
        return int(report.get("exit_code") or 0)

    if args.command == "status":
        qualify = _load_json(args.qualify_json) or {}
        preflight = _load_json(args.preflight_json)
        physical = _load_json(args.physical_json)
        qualify["states"] = evaluate_states(
            report=qualify,
            preflight=preflight,
            physical_pass=physical,
            live_source=git_identity(root),
        )
        summary = human_summary(qualify)
        json_path = _preserve_evidence(out_dir, "status", qualify, summary, run_dir)
        sys.stdout.write(summary)
        sys.stdout.write(f"evidence_json={json_path}\n")
        sys.stdout.write(f"evidence_run_dir={run_dir}\n")
        return 0 if qualify.get("states", {}).get("RELEASE_CANDIDATE", {}).get("status") == "PASS" else 1

    raise SystemExit("unknown command")


if __name__ == "__main__":
    raise SystemExit(main())
