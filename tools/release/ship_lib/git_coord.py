"""GitHub / Git coordination — executable behind the live-execution boundary."""

from __future__ import annotations

import json
import re
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Protocol, Sequence

from .config import OWNED_RELEASE_PATHS
from .errors import ShipBlockedError, ShipError

_PIN_REL = "tools/release/kindle-release-public-key.hex"
_VERSION_REL = "ignite/VERSION"
_PR_URL_RE = re.compile(r"/pull/(\d+)\b")


@dataclass(frozen=True)
class GitSnapshot:
    branch: str
    head_sha: str
    dirty: bool
    dirty_paths: tuple[str, ...]
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


class GitRunner(Protocol):
    def run(self, args: list[str], *, cwd: Path) -> subprocess.CompletedProcess[str]: ...


class SubprocessGit:
    def run(self, args: list[str], *, cwd: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["git", *args],
            cwd=str(cwd),
            capture_output=True,
            text=True,
            check=False,
        )


def git_snapshot(repo_root: Path, *, git: GitRunner | None = None) -> GitSnapshot:
    git = git or SubprocessGit()
    branch = _git_ok(git, repo_root, ["rev-parse", "--abbrev-ref", "HEAD"]).strip()
    head = _git_ok(git, repo_root, ["rev-parse", "HEAD"]).strip()
    status = _git_ok(git, repo_root, ["status", "--porcelain"])
    dirty_paths = tuple(
        line[3:].strip() for line in status.splitlines() if line.strip()
    )
    upstream = None
    up = git.run(
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        cwd=repo_root,
    )
    if up.returncode == 0:
        upstream = up.stdout.strip()
    return GitSnapshot(
        branch=branch,
        head_sha=head,
        dirty=bool(dirty_paths),
        dirty_paths=dirty_paths,
        upstream=upstream,
    )


def assert_only_owned_dirty(
    repo_root: Path,
    *,
    owned_paths: Sequence[str] = OWNED_RELEASE_PATHS,
    git: GitRunner | None = None,
) -> GitSnapshot:
    """Refuse live mutation when unrelated (non-owned) paths are dirty."""
    snap = git_snapshot(repo_root, git=git)
    owned = {p.replace("\\", "/") for p in owned_paths}
    unexpected = [
        p for p in snap.dirty_paths if p.replace("\\", "/") not in owned
    ]
    if unexpected:
        raise ShipError(
            "working tree has unrelated dirty paths; refuse live ship mutation: "
            + ", ".join(unexpected[:12])
        )
    return snap


def release_commit_paths(
    *,
    changed_paths: Sequence[str],
    pin_provisioned: bool,
    owned_paths: Sequence[str] | None = None,
) -> list[str]:
    """Owned paths that must be staged for the release preparation commit."""
    allowed = {p.replace("\\", "/") for p in OWNED_RELEASE_PATHS}
    paths = {
        p.replace("\\", "/")
        for p in changed_paths
        if p.replace("\\", "/") in allowed
    }
    if owned_paths:
        for p in owned_paths:
            norm = p.replace("\\", "/")
            if norm in allowed and norm in {
                *(x.replace("\\", "/") for x in changed_paths),
                _PIN_REL if pin_provisioned else "",
            }:
                paths.add(norm)
    if pin_provisioned:
        paths.add(_PIN_REL)
    return sorted(paths)


def prepare_release_commit_plan(
    *,
    version: str,
    changed_paths: list[str],
    pin_provisioned: bool,
) -> dict[str, object]:
    paths = release_commit_paths(
        changed_paths=changed_paths, pin_provisioned=pin_provisioned
    )
    subject = f"release: prepare Ignite {version} authenticated publication"
    body_lines = [
        f"Synchronize version metadata to {version}.",
    ]
    if pin_provisioned:
        body_lines.append(
            "Provision the production Kindle publisher public-key pin (public only)."
        )
    body_lines.append("")
    body_lines.append(
        "Users with UNPROVISIONED builds require a trusted manual reinstall "
        "to establish the new publisher key."
    )
    return {
        "subject": subject,
        "body": "\n".join(body_lines),
        "paths": paths,
        "signoff": True,
        "executed": False,
    }


def commit_release_preparation(
    *,
    repo_root: Path,
    version: str,
    paths: Sequence[str],
    pin_provisioned: bool,
    expected_public_key_hex: str | None = None,
    git: GitRunner | None = None,
) -> dict[str, object]:
    """Stage owned paths and create a signed-off release preparation commit."""
    git = git or SubprocessGit()
    stage_paths = release_commit_paths(
        changed_paths=paths, pin_provisioned=pin_provisioned
    )
    assert_only_owned_dirty(
        repo_root,
        owned_paths=list(OWNED_RELEASE_PATHS),
        git=git,
    )
    plan = prepare_release_commit_plan(
        version=version,
        changed_paths=list(stage_paths),
        pin_provisioned=pin_provisioned,
    )
    if not stage_paths:
        head = _git_ok(git, repo_root, ["rev-parse", "HEAD"]).strip()
        return {
            **plan,
            "executed": True,
            "commit_sha": head,
            "note": "no paths changed; reusing HEAD",
        }
    for rel in stage_paths:
        completed = git.run(["add", "--", rel], cwd=repo_root)
        if completed.returncode != 0:
            raise ShipError(f"git add failed for {rel}")
    message = f"{plan['subject']}\n\n{plan['body']}\n"
    completed = git.run(
        ["commit", "-s", "-m", message],
        cwd=repo_root,
    )
    if completed.returncode != 0:
        raise ShipError(
            "git commit -s failed: "
            + (completed.stderr or completed.stdout).strip()
        )
    sha = _git_ok(git, repo_root, ["rev-parse", "HEAD"]).strip()
    verify_release_tree(
        repo_root=repo_root,
        commit_sha=sha,
        version=version,
        expected_public_key_hex=expected_public_key_hex,
        require_pin=pin_provisioned,
        git=git,
    )
    return {**plan, "executed": True, "commit_sha": sha, "paths": stage_paths}


def verify_release_tree(
    *,
    repo_root: Path,
    commit_sha: str,
    version: str,
    expected_public_key_hex: str | None,
    require_pin: bool,
    git: GitRunner | None = None,
) -> None:
    """Confirm the commit tree has the expected pin/version and no private keys."""
    git = git or SubprocessGit()
    version_blob = _git_show(git, repo_root, commit_sha, _VERSION_REL)
    if version_blob.strip() != version:
        raise ShipError(
            f"commit {commit_sha[:12]} VERSION is {version_blob.strip()!r}, "
            f"expected {version!r}"
        )
    if require_pin:
        if not expected_public_key_hex:
            raise ShipError("expected public key hex required to verify pin in commit")
        pin_blob = _git_show(git, repo_root, commit_sha, _PIN_REL)
        if pin_blob.strip() == "UNPROVISIONED":
            raise ShipError(
                f"commit {commit_sha[:12]} still has UNPROVISIONED pin"
            )
        if pin_blob.strip().lower() != expected_public_key_hex.strip().lower():
            raise ShipError(
                f"commit {commit_sha[:12]} does not contain the expected "
                "production public-key pin"
            )
    # Refuse private-key-like blobs in owned release paths.
    for rel in OWNED_RELEASE_PATHS:
        try:
            blob = _git_show(git, repo_root, commit_sha, rel)
        except ShipError:
            continue
        upper = blob.upper()
        if "BEGIN" in upper and "PRIVATE" in upper:
            raise ShipError(
                f"refusing commit {commit_sha[:12]}: private-key material in {rel}"
            )
        if rel.endswith(".sealed") or "recovery-authority" in rel:
            raise ShipError(
                f"refusing commit {commit_sha[:12]}: sealed/recovery material in {rel}"
            )


def push_feature_branch(
    *,
    repo_root: Path,
    branch: str,
    git: GitRunner | None = None,
) -> dict[str, object]:
    git = git or SubprocessGit()
    completed = git.run(
        ["push", "-u", "origin", branch],
        cwd=repo_root,
    )
    if completed.returncode != 0:
        raise ShipError(
            "git push failed: " + (completed.stderr or completed.stdout).strip()
        )
    return {"executed": True, "branch": branch}


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


def create_or_update_pr(
    *,
    repo_root: Path,
    title: str,
    head: str,
    base: str,
    body: str,
    expected_head_sha: str,
    gh: GitHubClient,
) -> dict[str, object]:
    """Create a PR or reuse an existing open PR for head→base.

    Resolves the PR number without unsupported ``gh pr view --head``.
    Verifies head/base branches and expected head commit.
    """
    listed = gh.run(
        [
            "pr",
            "list",
            "--head",
            head,
            "--base",
            base,
            "--state",
            "open",
            "--json",
            "number,url,title,headRefName,baseRefName,headRefOid",
        ],
        cwd=repo_root,
    )
    if listed.returncode != 0:
        raise ShipBlockedError(
            "unable to list PRs: " + (listed.stderr or listed.stdout).strip()
        )
    try:
        rows = json.loads(listed.stdout or "[]")
    except json.JSONDecodeError as exc:
        raise ShipError("malformed gh pr list JSON") from exc
    if rows:
        pr = rows[0]
        number = int(pr["number"])
        edit = gh.run(
            ["pr", "edit", str(number), "--title", title, "--body", body],
            cwd=repo_root,
        )
        if edit.returncode != 0:
            raise ShipBlockedError(
                "unable to update PR: " + (edit.stderr or edit.stdout).strip()
            )
        verified = verify_pr_identity(
            repo_root=repo_root,
            pr_number=number,
            expected_head=head,
            expected_base=base,
            expected_head_sha=expected_head_sha,
            gh=gh,
        )
        return {
            "executed": True,
            "pr_number": number,
            "pr_url": str(pr.get("url") or verified.get("url") or ""),
            "reused": True,
            "head_sha": str(verified["head_sha"]),
        }

    created = gh.run(
        [
            "pr",
            "create",
            "--title",
            title,
            "--body",
            body,
            "--head",
            head,
            "--base",
            base,
        ],
        cwd=repo_root,
    )
    if created.returncode != 0:
        raise ShipBlockedError(
            "unable to create PR (branch protection / auth): "
            + (created.stderr or created.stdout).strip()
        )
    url = (created.stdout or "").strip().splitlines()[-1] if created.stdout else ""
    number = _pr_number_from_url(url)
    if number <= 0:
        # Re-list open PRs for head/base — never use unsupported --head on view.
        relist = gh.run(
            [
                "pr",
                "list",
                "--head",
                head,
                "--base",
                base,
                "--state",
                "open",
                "--json",
                "number,url",
            ],
            cwd=repo_root,
        )
        if relist.returncode == 0:
            try:
                found = json.loads(relist.stdout or "[]")
            except json.JSONDecodeError:
                found = []
            if found:
                number = int(found[0]["number"])
                url = str(found[0].get("url") or url)
    if number <= 0:
        raise ShipError("unable to resolve created PR number")
    verified = verify_pr_identity(
        repo_root=repo_root,
        pr_number=number,
        expected_head=head,
        expected_base=base,
        expected_head_sha=expected_head_sha,
        gh=gh,
    )
    return {
        "executed": True,
        "pr_number": number,
        "pr_url": url or str(verified.get("url") or ""),
        "reused": False,
        "head_sha": str(verified["head_sha"]),
    }


def verify_pr_identity(
    *,
    repo_root: Path,
    pr_number: int,
    expected_head: str,
    expected_base: str,
    expected_head_sha: str,
    gh: GitHubClient,
) -> dict[str, object]:
    view = gh.run(
        [
            "pr",
            "view",
            str(pr_number),
            "--json",
            "number,url,headRefName,baseRefName,headRefOid,state,mergeable,isDraft",
        ],
        cwd=repo_root,
    )
    if view.returncode != 0:
        raise ShipBlockedError(
            f"unable to view PR #{pr_number}: "
            + (view.stderr or view.stdout).strip()
        )
    try:
        data = json.loads(view.stdout or "{}")
    except json.JSONDecodeError as exc:
        raise ShipError("malformed gh pr view JSON") from exc
    if str(data.get("headRefName")) != expected_head:
        raise ShipError(
            f"PR #{pr_number} head branch is {data.get('headRefName')!r}, "
            f"expected {expected_head!r}"
        )
    if str(data.get("baseRefName")) != expected_base:
        raise ShipError(
            f"PR #{pr_number} base branch is {data.get('baseRefName')!r}, "
            f"expected {expected_base!r}"
        )
    head_sha = str(data.get("headRefOid") or "")
    if expected_head_sha and head_sha != expected_head_sha:
        raise ShipError(
            f"PR #{pr_number} head commit is {head_sha}, expected {expected_head_sha}"
        )
    return {
        "pr_number": pr_number,
        "url": str(data.get("url") or ""),
        "head_sha": head_sha,
        "state": str(data.get("state") or ""),
    }


def wait_for_pr_checks(
    *,
    repo_root: Path,
    pr_number: int,
    gh: GitHubClient,
    poll_seconds: int = 30,
    timeout_seconds: int = 3600,
    sleep: Callable[[float], None] = time.sleep,
) -> dict[str, object]:
    """Poll required PR checks until green, failed, or timeout.

    Ordinary pending checks are waited out — not treated as immediate failure.
    """
    deadline = time.monotonic() + timeout_seconds
    last: dict[str, object] = {"checks": [], "pending": [], "failing": []}
    while time.monotonic() < deadline:
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
        pending = [c for c in checks if _check_is_pending(c)]
        last = {"checks": checks, "pending": pending, "failing": failing}
        if not pending:
            return last
        sleep(poll_seconds)
    raise ShipBlockedError(
        f"timed out waiting for PR #{pr_number} required checks to become green"
    )


def merge_pr(
    *,
    repo_root: Path,
    pr_number: int,
    gh: GitHubClient,
    merge_method: str = "squash",
    git: GitRunner | None = None,
    base: str = "main",
    expected_head_sha: str | None = None,
    version: str | None = None,
    expected_public_key_hex: str | None = None,
    require_pin: bool = False,
) -> dict[str, object]:
    """Merge when policy permits, then confirm merged state and resulting main SHA.

    A successful merge command alone does not establish that the PR is merged.
    """
    # Preflight: still open / not already merged with wrong head.
    pre = gh.run(
        [
            "pr",
            "view",
            str(pr_number),
            "--json",
            "state,mergedAt,mergeCommit,headRefOid,statusCheckRollup",
        ],
        cwd=repo_root,
    )
    if pre.returncode == 0:
        try:
            pre_data = json.loads(pre.stdout or "{}")
        except json.JSONDecodeError:
            pre_data = {}
        if str(pre_data.get("state") or "").upper() == "MERGED":
            raise ShipError(
                f"PR #{pr_number} is already merged; refusing to re-associate blindly"
            )
        if expected_head_sha and str(pre_data.get("headRefOid") or "") != expected_head_sha:
            raise ShipError(
                f"PR #{pr_number} head moved before merge "
                f"({pre_data.get('headRefOid')} != {expected_head_sha})"
            )

    completed = gh.run(
        [
            "pr",
            "merge",
            str(pr_number),
            f"--{merge_method}",
            "--delete-branch=false",
        ],
        cwd=repo_root,
    )
    text = (completed.stderr or "") + (completed.stdout or "")
    if completed.returncode != 0:
        lowered = text.lower()
        if (
            "review" in lowered
            or "approval" in lowered
            or "protected" in lowered
            or "required" in lowered
            or "merge queue" in lowered
        ):
            raise ShipBlockedError(
                "merge blocked by branch protection / independent review / merge queue: "
                + text.strip()[:500]
            )
        raise ShipBlockedError("gh pr merge failed: " + text.strip()[:500])

    confirmed = confirm_pr_merged(
        repo_root=repo_root,
        pr_number=pr_number,
        gh=gh,
        expected_head_sha=expected_head_sha,
    )
    merge_commit = str(confirmed["merge_commit"])
    main_sha = resolve_base_sha(repo_root=repo_root, base=base, gh=gh, git=git)
    # The resulting main tip must be the merge commit (or contain it for merge commits).
    if main_sha != merge_commit:
        # For squash merges, mergeCommit.oid should equal origin/main.
        raise ShipError(
            f"origin/{base} is {main_sha}, but PR merge commit is {merge_commit}; "
            "refusing to associate the release with an unrelated main tip"
        )
    if version:
        verify_release_tree(
            repo_root=repo_root,
            commit_sha=main_sha,
            version=version,
            expected_public_key_hex=expected_public_key_hex,
            require_pin=require_pin,
            git=git,
        )
    return {
        "executed": True,
        "pr_number": pr_number,
        "merge_method": merge_method,
        "main_sha": main_sha,
        "merge_commit": merge_commit,
        "merged": True,
    }


def confirm_pr_merged(
    *,
    repo_root: Path,
    pr_number: int,
    gh: GitHubClient,
    expected_head_sha: str | None = None,
    attempts: int = 10,
    sleep: Callable[[float], None] = time.sleep,
) -> dict[str, object]:
    """Poll until PR reports merged with a resolvable merge commit OID."""
    last_err = "PR not yet merged"
    for _ in range(max(1, attempts)):
        view = gh.run(
            [
                "pr",
                "view",
                str(pr_number),
                "--json",
                "state,mergedAt,mergeCommit,headRefOid,baseRefName",
            ],
            cwd=repo_root,
        )
        if view.returncode != 0:
            last_err = (view.stderr or view.stdout).strip()
            sleep(2)
            continue
        try:
            data = json.loads(view.stdout or "{}")
        except json.JSONDecodeError:
            last_err = "malformed pr view JSON"
            sleep(2)
            continue
        state = str(data.get("state") or "").upper()
        merged_at = data.get("mergedAt")
        merge_obj = data.get("mergeCommit") or {}
        merge_oid = ""
        if isinstance(merge_obj, dict):
            merge_oid = str(merge_obj.get("oid") or "")
        elif isinstance(merge_obj, str):
            merge_oid = merge_obj
        if state == "MERGED" and merged_at and len(merge_oid) >= 40:
            if expected_head_sha:
                # Squash merges replace the head; headRefOid may still be pre-merge.
                pass
            return {
                "merged": True,
                "merge_commit": merge_oid[:40],
                "merged_at": str(merged_at),
                "state": state,
            }
        last_err = f"state={state} mergedAt={merged_at} mergeCommit={merge_oid}"
        sleep(2)
    raise ShipError(
        f"PR #{pr_number} merge command returned success but merged state "
        f"was not confirmed: {last_err}"
    )


def merge_pr_plan(*, pr_number: int, merge_method: str = "squash") -> dict[str, object]:
    return {
        "pr_number": pr_number,
        "merge_method": merge_method,
        "executed": False,
        "note": "live merge requires repository policy compliance; ship does not forge reviews",
    }


def resolve_base_sha(
    *,
    repo_root: Path,
    base: str,
    gh: GitHubClient | None = None,
    git: GitRunner | None = None,
) -> str:
    git = git or SubprocessGit()
    git.run(["fetch", "origin", base], cwd=repo_root)
    rev = git.run(["rev-parse", f"origin/{base}"], cwd=repo_root)
    if rev.returncode == 0 and len(rev.stdout.strip()) == 40:
        return rev.stdout.strip()
    if gh is not None:
        api = gh.run(
            ["api", f"repos/{{owner}}/{{repo}}/commits/{base}", "--jq", ".sha"],
            cwd=repo_root,
        )
        if api.returncode == 0 and len(api.stdout.strip()) >= 40:
            return api.stdout.strip()[:40]
    raise ShipError(f"unable to resolve exact {base} SHA after merge")


def _check_is_pending(check: dict) -> bool:
    bucket = str(check.get("bucket") or "").lower()
    state = str(check.get("state") or "").upper()
    if bucket in {"pass", "skip"} or state in {"SUCCESS", "NEUTRAL", "SKIPPED"}:
        return False
    if bucket in {"fail", "cancel"} or state in {"FAILURE", "ERROR", "CANCELLED"}:
        return False
    return True


def _pr_number_from_url(url: str) -> int:
    match = _PR_URL_RE.search(url or "")
    if not match:
        return 0
    return int(match.group(1))


def _git_show(git: GitRunner, repo_root: Path, commit_sha: str, rel: str) -> str:
    completed = git.run(["show", f"{commit_sha}:{rel}"], cwd=repo_root)
    if completed.returncode != 0:
        raise ShipError(
            f"git show {commit_sha[:12]}:{rel} failed: "
            + (completed.stderr or completed.stdout).strip()
        )
    return completed.stdout


def _git_ok(git: GitRunner, repo_root: Path, args: list[str]) -> str:
    completed = git.run(args, cwd=repo_root)
    if completed.returncode != 0:
        raise ShipError(
            f"git {' '.join(args)} failed: "
            + (completed.stderr or completed.stdout).strip()
        )
    return completed.stdout
