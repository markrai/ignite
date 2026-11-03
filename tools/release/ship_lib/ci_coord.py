"""Wait for and bind the exact post-merge ci.yml run."""

from __future__ import annotations

import json
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

from .errors import ShipBlockedError, ShipError, ShipRetryableError
from .git_coord import GitHubClient


@dataclass(frozen=True)
class CiRun:
    run_id: int
    head_sha: str
    workflow_path: str
    conclusion: str
    status: str
    url: str


def find_successful_ci_run(
    *,
    repo_root: Path,
    head_sha: str,
    gh: GitHubClient,
    workflow_file: str = "ci.yml",
    repository: str = "markrai/ignite",
) -> CiRun | None:
    completed = gh.run(
        [
            "run",
            "list",
            "--repo",
            repository,
            "--workflow",
            workflow_file,
            "--commit",
            head_sha,
            "--json",
            "databaseId,headSha,conclusion,status,url,workflowDatabaseId",
            "--limit",
            "20",
        ],
        cwd=repo_root,
    )
    if completed.returncode != 0:
        raise ShipRetryableError(
            "gh run list failed: " + (completed.stderr or completed.stdout).strip()
        )
    try:
        rows = json.loads(completed.stdout or "[]")
    except json.JSONDecodeError as exc:
        raise ShipError("malformed gh run list JSON") from exc
    for row in rows:
        if str(row.get("headSha")) != head_sha:
            continue
        if str(row.get("conclusion")) == "success" and str(row.get("status")) == "completed":
            return CiRun(
                run_id=int(row["databaseId"]),
                head_sha=head_sha,
                workflow_path=f".github/workflows/{workflow_file}",
                conclusion="success",
                status="completed",
                url=str(row.get("url") or ""),
            )
        if str(row.get("conclusion")) in {"failure", "cancelled", "timed_out"}:
            raise ShipBlockedError(
                f"ci.yml for {head_sha} concluded {row.get('conclusion')}"
            )
    return None


def wait_for_ci_success(
    *,
    repo_root: Path,
    head_sha: str,
    gh: GitHubClient,
    poll_seconds: int = 30,
    timeout_seconds: int = 3600,
    sleep: Callable[[float], None] = time.sleep,
    repository: str = "markrai/ignite",
) -> CiRun:
    if len(head_sha) != 40:
        raise ShipError("ci wait requires a full 40-character commit SHA")
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        found = find_successful_ci_run(
            repo_root=repo_root,
            head_sha=head_sha,
            gh=gh,
            repository=repository,
        )
        if found is not None:
            return found
        sleep(poll_seconds)
    raise ShipBlockedError(
        f"timed out waiting for successful ci.yml on commit {head_sha}"
    )
