"""Wait for and bind the exact post-merge ci.yml run."""

from __future__ import annotations

import base64
import json
import re
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

from .errors import ShipBlockedError, ShipError, ShipRetryableError
from .git_coord import GitHubClient

_COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
EXPECTED_WORKFLOW_FILE = "ci.yml"
EXPECTED_WORKFLOW = f".github/workflows/{EXPECTED_WORKFLOW_FILE}"
EXPECTED_BRANCH = "main"


@dataclass(frozen=True)
class CiRun:
    run_id: int
    head_sha: str
    workflow_path: str
    conclusion: str
    status: str
    url: str
    workflow_id: int = 0


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


def _is_designated_ci_run_path(path: str, *, workflow_file: str = EXPECTED_WORKFLOW_FILE) -> bool:
    expected = f".github/workflows/{workflow_file}"
    return path == expected or path.startswith(expected + "@")


def _designated_workflow_id(
    *,
    repo_root: Path,
    repository: str,
    gh: GitHubClient,
    workflow_file: str = EXPECTED_WORKFLOW_FILE,
) -> int:
    completed = gh.run(
        ["api", f"repos/{repository}/actions/workflows/{workflow_file}"],
        cwd=repo_root,
    )
    if completed.returncode != 0:
        raise ShipBlockedError(
            f"cannot read designated CI workflow {workflow_file}: "
            + (completed.stderr or completed.stdout).strip()
        )
    try:
        data = json.loads(completed.stdout or "{}")
    except json.JSONDecodeError as exc:
        raise ShipError("malformed designated CI workflow JSON") from exc
    if not isinstance(data, dict):
        raise ShipError("designated CI workflow payload malformed")
    try:
        workflow_id = int(data.get("id"))
    except (TypeError, ValueError) as exc:
        raise ShipError("designated CI workflow id is malformed") from exc
    if workflow_id < 1:
        raise ShipError("designated CI workflow id is malformed")
    return workflow_id


def _assert_commit_on_main(
    *,
    repo_root: Path,
    repository: str,
    commit: str,
    gh: GitHubClient,
    branch: str = EXPECTED_BRANCH,
) -> None:
    """Refuse commits not contained in main (same ahead_by=0 guard as promote-release)."""
    completed = gh.run(
        [
            "api",
            f"repos/{repository}/compare/{branch}...{commit}",
            "--jq",
            ".ahead_by",
        ],
        cwd=repo_root,
    )
    if completed.returncode != 0:
        raise ShipBlockedError(
            f"cannot verify commit {commit} is on {branch}: "
            + (completed.stderr or completed.stdout).strip()
        )
    ahead_raw = (completed.stdout or "").strip()
    try:
        ahead = int(ahead_raw)
    except ValueError as exc:
        raise ShipError(
            f"malformed compare ahead_by for {branch}...{commit}: {ahead_raw!r}"
        ) from exc
    if ahead != 0:
        raise ShipBlockedError(
            f"commit {commit} is not on {branch} (ahead_by {ahead})"
        )


def _version_at_commit(
    *,
    repo_root: Path,
    repository: str,
    commit: str,
    gh: GitHubClient,
) -> str:
    completed = gh.run(
        [
            "api",
            "-H",
            "Accept: application/vnd.github.raw",
            f"repos/{repository}/contents/ignite/VERSION?ref={commit}",
        ],
        cwd=repo_root,
    )
    if completed.returncode != 0:
        # Fallback for fixtures that return base64 content JSON / jq .content.
        alt = gh.run(
            [
                "api",
                f"repos/{repository}/contents/ignite/VERSION?ref={commit}",
                "--jq",
                ".content",
            ],
            cwd=repo_root,
        )
        if alt.returncode != 0:
            raise ShipError(
                "unable to read ignite/VERSION at commit: "
                + (completed.stderr or completed.stdout or alt.stderr or "").strip()
            )
        raw = (alt.stdout or "").strip().replace("\n", "")
        try:
            decoded = base64.b64decode(raw, validate=False).decode("utf-8")
        except (ValueError, UnicodeDecodeError) as exc:
            raise ShipError("ignite/VERSION at commit is not valid UTF-8") from exc
        version = decoded.strip()
    else:
        version = (completed.stdout or "").strip()
    if not version:
        raise ShipError("ignite/VERSION empty at commit")
    return version


def bind_existing_ci_run(
    *,
    repo_root: Path,
    version: str,
    run_id: int,
    commit: str,
    gh: GitHubClient,
    repository: str = "markrai/ignite",
    workflow_file: str = EXPECTED_WORKFLOW_FILE,
) -> CiRun:
    """Validate an already-successful main ci.yml run for publish-from-existing-CI.

    Fail-closed on branch/event/workflow-id/path/SHA/main-containment/version
    mismatch. Does not wait and does not start a new CI run.
    """
    if run_id < 1:
        raise ShipError("ci run id must be a positive integer")
    if not _COMMIT_RE.fullmatch(commit):
        raise ShipError("commit must be a full 40-character lowercase hex SHA")

    want_workflow_id = _designated_workflow_id(
        repo_root=repo_root,
        repository=repository,
        gh=gh,
        workflow_file=workflow_file,
    )

    completed = gh.run(
        ["api", f"repos/{repository}/actions/runs/{run_id}"],
        cwd=repo_root,
    )
    if completed.returncode != 0:
        raise ShipBlockedError(
            f"unable to load CI run {run_id}: "
            + (completed.stderr or completed.stdout).strip()
        )
    try:
        run = json.loads(completed.stdout or "{}")
    except json.JSONDecodeError as exc:
        raise ShipError("malformed GitHub Actions run JSON") from exc
    if not isinstance(run, dict):
        raise ShipError("GitHub Actions run payload malformed")

    status = str(run.get("status") or "")
    conclusion = str(run.get("conclusion") or "")
    if status != "completed" or conclusion != "success":
        raise ShipBlockedError(
            f"CI run {run_id} is not successful completed "
            f"(status={status} conclusion={conclusion})"
        )

    head_sha = str(run.get("head_sha") or "")
    if head_sha != commit:
        raise ShipBlockedError(
            f"CI run {run_id} head_sha {head_sha} does not match commit {commit}"
        )

    head_branch = str(run.get("head_branch") or "")
    if head_branch != EXPECTED_BRANCH:
        raise ShipBlockedError(
            f"CI run {run_id} branch refused: {head_branch!r} (required {EXPECTED_BRANCH})"
        )

    event = str(run.get("event") or "")
    if event == "pull_request":
        raise ShipBlockedError("pull_request CI artifacts are not eligible for publication")
    if event not in {"push", "workflow_dispatch"}:
        raise ShipBlockedError(f"CI run event refused: {event}")

    path = str(run.get("path") or "")
    if not _is_designated_ci_run_path(path, workflow_file=workflow_file):
        raise ShipBlockedError(
            f"CI run {run_id} is not the designated CI workflow "
            f"(path {path!r}; required {EXPECTED_WORKFLOW} or {EXPECTED_WORKFLOW}@ref)"
        )

    try:
        got_workflow_id = int(run.get("workflow_id"))
    except (TypeError, ValueError) as exc:
        raise ShipBlockedError(
            f"CI run {run_id} workflow_id missing or malformed"
        ) from exc
    if got_workflow_id != want_workflow_id:
        raise ShipBlockedError(
            f"CI run {run_id} is not the designated CI workflow "
            f"(workflow_id {got_workflow_id}; required {want_workflow_id})"
        )

    _assert_commit_on_main(
        repo_root=repo_root,
        repository=repository,
        commit=commit,
        gh=gh,
        branch=EXPECTED_BRANCH,
    )

    at_commit = _version_at_commit(
        repo_root=repo_root,
        repository=repository,
        commit=commit,
        gh=gh,
    )
    if at_commit != version:
        raise ShipBlockedError(
            f"ignite/VERSION at {commit} is {at_commit!r}, expected {version!r}"
        )

    return CiRun(
        run_id=run_id,
        head_sha=commit,
        workflow_path=EXPECTED_WORKFLOW
        if workflow_file == EXPECTED_WORKFLOW_FILE
        else f".github/workflows/{workflow_file}",
        conclusion="success",
        status="completed",
        url=str(run.get("html_url") or run.get("url") or ""),
        workflow_id=want_workflow_id,
    )
