"""GitHub / Git coordination helpers (callable; live use requires allow_live)."""

from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Protocol

from .errors import ShipBlockedError, ShipError


@dataclass(frozen=True)
class GitSnapshot:
    branch: str
    head_sha: str
    dirty: bool
    upstream: str | None


class GitHubClient(Protocol):
    def run(self, args: list[str], *, cwd: Path) -> subprocess.CompletedProcess[str]: ...


class SubprocessGitHub:
    def __init__(self, gh_bin: str = "gh") -> None:
        self._gh = gh_bin

    def run(self, args: list[str], *, cwd: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [self._gh, *args],
            cwd=str(cwd),
            capture_output=True,
            text=True,
            check=False,
        )


def git_snapshot(repo_root: Path) -> GitSnapshot:
    branch = _git(repo_root, ["rev-parse", "--abbrev-ref", "HEAD"]).strip()
    head = _git(repo_root, ["rev-parse", "HEAD"]).strip()
    status = _git(repo_root, ["status", "--porcelain"])
    upstream = None
    try:
        upstream = _git(
            repo_root, ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]
        ).strip()
    except ShipError:
        upstream = None
    return GitSnapshot(
        branch=branch, head_sha=head, dirty=bool(status.strip()), upstream=upstream
    )


def assert_clean_for_live(repo_root: Path) -> GitSnapshot:
    snap = git_snapshot(repo_root)
    if snap.dirty:
        raise ShipError("working tree is dirty; refuse live ship mutation")
    return snap


def prepare_release_commit_plan(
    *,
    version: str,
    changed_paths: list[str],
    pin_provisioned: bool,
) -> dict[str, object]:
    """Describe the signed-off commit that a live ship would create (not executed)."""
    subject = f"release: prepare Ignite {version} authenticated publication"
    body_lines = [
        f"Synchronize version metadata to {version}.",
    ]
    if pin_provisioned:
        body_lines.append(
            "Provision the production Kindle publisher public-key pin (public only)."
        )
    body_lines.append("")
    body_lines.append("Signed-off-by: Ignite ship orchestrator")
    return {
        "subject": subject,
        "body": "\n".join(body_lines),
        "paths": list(changed_paths),
        "signoff": True,
        "executed": False,
    }


def create_or_update_pr_plan(
    *,
    title: str,
    head: str,
    base: str,
    body: str,
) -> dict[str, str]:
    return {
        "title": title,
        "head": head,
        "base": base,
        "body": body,
        "executed": "false",
    }


def wait_for_pr_checks(
    *,
    repo_root: Path,
    pr_number: int,
    gh: GitHubClient,
    poll: Callable[[], None] | None = None,
    timeout_seconds: int = 3600,
) -> dict[str, object]:
    """Poll GitHub checks for a PR. Raises ShipBlockedError on failure/policy."""
    del poll, timeout_seconds  # reserved for live polling loop
    completed = gh.run(
        ["pr", "checks", str(pr_number), "--json", "name,state,bucket"],
        cwd=repo_root,
    )
    if completed.returncode != 0:
        raise ShipBlockedError(
            "unable to read PR checks (branch protection / auth / missing PR): "
            + (completed.stderr or completed.stdout).strip()
        )
    try:
        checks = json.loads(completed.stdout or "[]")
    except json.JSONDecodeError as exc:
        raise ShipError("malformed gh pr checks JSON") from exc
    failing = [
        c
        for c in checks
        if str(c.get("bucket") or "").lower() in {"fail", "cancel"}
        or str(c.get("state") or "").upper() in {"FAILURE", "ERROR", "CANCELLED"}
    ]
    if failing:
        names = ", ".join(str(c.get("name")) for c in failing)
        raise ShipBlockedError(f"required PR checks failing: {names}")
    pending = [
        c
        for c in checks
        if str(c.get("bucket") or "").lower() in {"pending", ""}
        and str(c.get("state") or "").upper() in {"PENDING", "QUEUED", "IN_PROGRESS", ""}
    ]
    return {"checks": checks, "pending": pending, "failing": failing}


def merge_pr_plan(*, pr_number: int, merge_method: str = "squash") -> dict[str, object]:
    return {
        "pr_number": pr_number,
        "merge_method": merge_method,
        "executed": False,
        "note": "live merge requires repository policy compliance; ship does not forge reviews",
    }


def _git(repo_root: Path, args: list[str]) -> str:
    completed = subprocess.run(
        ["git", *args],
        cwd=str(repo_root),
        capture_output=True,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        raise ShipError(
            f"git {' '.join(args)} failed: {(completed.stderr or completed.stdout).strip()}"
        )
    return completed.stdout
