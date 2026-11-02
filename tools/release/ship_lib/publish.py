"""Option A local publication via promote-release.sh + independent verification."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping, Sequence

from .errors import ShipError
from .git_coord import GitHubClient


def canonical_nine_file_names(
    *,
    version: str,
    linux_bundle_name: str,
) -> list[str]:
    """Exact nine-file GitHub Release inventory for a Kindle authenticated publish."""
    return sorted(
        [
            f"Install Ignite v{version}.sh",
            f"Install Ignite v{version}.sha256",
            f"ignite-kindle-v{version}.manifest",
            f"ignite-kindle-v{version}.manifest.sig",
            f"ignite-kindle-v{version}.publication-binding",
            f"ignite-kindle-v{version}.publication-binding.sig",
            f"ignite-kindle-v{version}.publication-evidence",
            linux_bundle_name,
            f"{linux_bundle_name}.sha256",
        ]
    )


def local_asset_digests(asset_dir: Path, names: Sequence[str]) -> dict[str, str]:
    digests: dict[str, str] = {}
    for name in names:
        path = asset_dir / name
        if not path.is_file():
            raise ShipError(f"local authenticated asset missing: {name}")
        digests[name] = hashlib.sha256(path.read_bytes()).hexdigest()
    return digests


@dataclass(frozen=True)
class PublishResult:
    tag: str
    run_id: int
    commit: str
    release_url: str | None
    executed: bool
    inventory_ok: bool
    independently_verified: bool
    asset_digests: dict[str, str]


def build_publish_env(
    *,
    acquired,
    signed,
    work_dir: Path,
    confirm: str | None = None,
) -> dict[str, str]:
    import sys

    kindle = Path(__file__).resolve().parents[3] / "deployment" / "kindle"
    if str(kindle) not in sys.path:
        sys.path.insert(0, str(kindle))
    from release_orchestrate import promote_env_from_bundles

    env = promote_env_from_bundles(
        acquired=acquired,
        signed=signed,
        work_dir=work_dir,
        confirm=confirm,
        public_key_file=None,
    )
    env.pop("IGNITE_PROMOTE_PUBLIC_KEY_FILE", None)
    return env


def publish_release(
    *,
    repo_root: Path,
    env: Mapping[str, str],
    bash: str | None = None,
    dry_run_only: bool = False,
    gh: GitHubClient | None = None,
    expected_assets: Sequence[str] | None = None,
    local_asset_dir: Path | None = None,
    expected_digests: Mapping[str, str] | None = None,
) -> PublishResult:
    """Run promote-release.sh publish and independently verify the GitHub Release."""
    if "IGNITE_PROMOTE_PUBLIC_KEY_FILE" in env:
        raise ShipError("IGNITE_PROMOTE_PUBLIC_KEY_FILE is forbidden for publish")
    script = repo_root / "tools" / "release" / "promote-release.sh"
    if not script.is_file():
        raise ShipError("promote-release.sh missing")
    tag = env["IGNITE_PROMOTE_TAG"]
    run_id = int(env["IGNITE_PROMOTE_RUN_ID"])
    commit = env["IGNITE_PROMOTE_COMMIT"]
    if dry_run_only:
        return PublishResult(
            tag=tag,
            run_id=run_id,
            commit=commit,
            release_url=None,
            executed=False,
            inventory_ok=False,
            independently_verified=False,
            asset_digests={},
        )
    bash_bin = bash or _resolve_bash()
    if bash_bin is None:
        raise ShipError("bash required for promote-release.sh publish")
    merged = dict(os.environ)
    merged.update(env)
    completed = subprocess.run(
        [bash_bin, str(script).replace("\\", "/"), "publish"],
        cwd=str(repo_root),
        env=merged,
        capture_output=True,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        raise ShipError(
            "promote-release.sh publish failed: "
            + (completed.stderr or completed.stdout)[-800:]
        )
    url = None
    for line in (completed.stdout or "").splitlines():
        if "https://github.com/" in line and "/releases/" in line:
            url = line.strip()
            break

    if expected_assets is None and local_asset_dir is not None:
        expected_assets = sorted(
            p.name for p in local_asset_dir.iterdir() if p.is_file()
        )
    if expected_digests is None and local_asset_dir is not None and expected_assets:
        expected_digests = local_asset_digests(local_asset_dir, expected_assets)

    verification = verify_published_release(
        repo_root=repo_root,
        tag=tag,
        expected_commit=commit,
        gh=gh,
        expected_asset_names=expected_assets,
        expected_digests=expected_digests,
        local_asset_dir=local_asset_dir,
    )
    return PublishResult(
        tag=tag,
        run_id=run_id,
        commit=commit,
        release_url=url or str(verification.get("release_url") or ""),
        executed=True,
        inventory_ok=bool(verification.get("inventory_ok")),
        independently_verified=bool(verification.get("verified")),
        asset_digests=dict(verification.get("asset_digests") or {}),
    )


def verify_published_release(
    *,
    repo_root: Path,
    tag: str,
    expected_commit: str,
    gh: GitHubClient | None,
    expected_asset_names: Sequence[str] | None = None,
    expected_digests: Mapping[str, str] | None = None,
    local_asset_dir: Path | None = None,
    download_dir: Path | None = None,
) -> dict[str, object]:
    """Independently verify tag/commit/assets by retrieving remote bytes.

    A local hash without a remote comparison is not sufficient.
    """
    if gh is None:
        raise ShipError(
            "independent publication verification requires a GitHub client"
        )
    if not expected_asset_names:
        raise ShipError(
            "exact expected asset inventory is required for publication verification"
        )
    expected = sorted(expected_asset_names)
    if len(expected) != 9:
        raise ShipError(
            f"expected exactly nine published assets, got {len(expected)}: {expected}"
        )
    if expected_digests is None and local_asset_dir is not None:
        expected_digests = local_asset_digests(local_asset_dir, expected)
    if not expected_digests:
        raise ShipError(
            "expected asset digests required; cannot verify remote publication bytes"
        )
    for name in expected:
        if name not in expected_digests:
            raise ShipError(f"missing expected digest for {name}")

    view = gh.run(
        [
            "release",
            "view",
            tag,
            "--json",
            "tagName,targetCommitish,url,isDraft,isPrerelease,assets",
        ],
        cwd=repo_root,
    )
    if view.returncode != 0:
        raise ShipError(
            "unable to view published release for verification: "
            + (view.stderr or view.stdout).strip()
        )
    try:
        data = json.loads(view.stdout or "{}")
    except json.JSONDecodeError as exc:
        raise ShipError("malformed gh release view JSON") from exc
    if str(data.get("tagName")) != tag:
        raise ShipError("published tag mismatch")
    if bool(data.get("isDraft")):
        raise ShipError("release is still draft; not verified published")

    tag_sha = _resolve_tag_commit(repo_root, tag, gh)
    if tag_sha != expected_commit:
        raise ShipError(
            f"release tag {tag} points at {tag_sha}, expected {expected_commit}"
        )

    assets = list(data.get("assets") or [])
    remote_by_name = {str(a.get("name") or ""): a for a in assets}
    names = sorted(remote_by_name)
    if names != expected:
        raise ShipError(
            "published asset inventory mismatch: "
            f"got={names!r} expected={expected!r}"
        )

    # Size check against local authenticated candidate when available.
    if local_asset_dir is not None:
        for name in expected:
            local = local_asset_dir / name
            remote_size = remote_by_name[name].get("size")
            if remote_size is not None and int(remote_size) != local.stat().st_size:
                raise ShipError(f"published asset size mismatch for {name}")

    # Independently retrieve remote bytes and hash them.
    own_tmpdir = download_dir is None
    work = Path(download_dir) if download_dir is not None else Path(
        tempfile.mkdtemp(prefix="ignite-ship-release-verify-")
    )
    try:
        if own_tmpdir:
            work.mkdir(parents=True, exist_ok=True)
        remote_digests = _download_and_hash_assets(
            repo_root=repo_root,
            tag=tag,
            names=expected,
            dest=work,
            gh=gh,
            remote_by_name=remote_by_name,
        )
        for name in expected:
            if remote_digests[name] != expected_digests[name]:
                raise ShipError(
                    f"published asset digest mismatch for {name}: "
                    "remote bytes do not match authenticated release candidate"
                )
    finally:
        if own_tmpdir and work.exists():
            shutil.rmtree(work, ignore_errors=True)

    return {
        "verified": True,
        "inventory_ok": True,
        "release_url": str(data.get("url") or ""),
        "tag": tag,
        "commit": tag_sha,
        "asset_names": names,
        "asset_digests": dict(remote_digests),
        "is_prerelease": bool(data.get("isPrerelease")),
    }


def _download_and_hash_assets(
    *,
    repo_root: Path,
    tag: str,
    names: Sequence[str],
    dest: Path,
    gh: GitHubClient,
    remote_by_name: Mapping[str, object],
) -> dict[str, str]:
    """Download each published asset and return SHA-256 digests of remote bytes."""
    digests: dict[str, str] = {}
    # Prefer a single gh release download into dest, then hash.
    pattern_args: list[str] = []
    for name in names:
        pattern_args.extend(["--pattern", name])
    downloaded = gh.run(
        ["release", "download", tag, "-D", str(dest), *pattern_args, "--clobber"],
        cwd=repo_root,
    )
    if downloaded.returncode != 0:
        # Fallback: per-asset API download via gh api (raw bytes to file).
        for name in names:
            out = dest / name
            api = gh.run(
                [
                    "api",
                    "-H",
                    "Accept: application/octet-stream",
                    f"repos/{{owner}}/{{repo}}/releases/assets/"
                    f"{_asset_id(remote_by_name[name])}",
                    "--output",
                    str(out),
                ],
                cwd=repo_root,
            )
            if api.returncode != 0 or not out.is_file():
                raise ShipError(
                    f"unable to download published asset {name} for verification: "
                    + (api.stderr or api.stdout or downloaded.stderr or "").strip()
                )
    for name in names:
        path = dest / name
        if not path.is_file():
            raise ShipError(f"downloaded asset missing after retrieval: {name}")
        remote_size = remote_by_name[name].get("size") if isinstance(remote_by_name[name], dict) else None
        if remote_size is not None and int(remote_size) != path.stat().st_size:
            raise ShipError(f"downloaded asset size mismatch for {name}")
        digests[name] = hashlib.sha256(path.read_bytes()).hexdigest()
    return digests


def _asset_id(asset: object) -> str:
    if isinstance(asset, dict):
        for key in ("id", "databaseId", "url"):
            if asset.get(key) is not None:
                value = str(asset[key])
                if key == "url" and "/assets/" in value:
                    return value.rsplit("/", 1)[-1]
                if key != "url":
                    return value
    raise ShipError("published asset missing id for download")


def _resolve_tag_commit(repo_root: Path, tag: str, gh: GitHubClient) -> str:
    api = gh.run(
        ["api", f"repos/{{owner}}/{{repo}}/git/ref/tags/{tag}", "--jq", ".object"],
        cwd=repo_root,
    )
    if api.returncode != 0:
        api2 = gh.run(
            ["api", f"repos/{{owner}}/{{repo}}/commits/{tag}", "--jq", ".sha"],
            cwd=repo_root,
        )
        if api2.returncode != 0:
            raise ShipError(f"unable to resolve commit for tag {tag}")
        return api2.stdout.strip()[:40]
    try:
        obj = json.loads(api.stdout or "{}")
    except json.JSONDecodeError as exc:
        raise ShipError("malformed tag object JSON") from exc
    sha = str(obj.get("sha") or "")
    obj_type = str(obj.get("type") or "")
    if obj_type == "commit" and len(sha) >= 40:
        return sha[:40]
    if obj_type == "tag":
        peel = gh.run(
            ["api", f"repos/{{owner}}/{{repo}}/git/tags/{sha}", "--jq", ".object.sha"],
            cwd=repo_root,
        )
        if peel.returncode == 0 and len(peel.stdout.strip()) >= 40:
            return peel.stdout.strip()[:40]
    api2 = gh.run(
        ["api", f"repos/{{owner}}/{{repo}}/commits/{tag}", "--jq", ".sha"],
        cwd=repo_root,
    )
    if api2.returncode != 0 or len(api2.stdout.strip()) < 40:
        raise ShipError(f"unable to peel tag {tag} to a commit")
    return api2.stdout.strip()[:40]


def _resolve_bash() -> str | None:
    import sys

    kindle = Path(__file__).resolve().parents[3] / "deployment" / "kindle"
    if str(kindle) not in sys.path:
        sys.path.insert(0, str(kindle))
    try:
        import release_orchestrate as ro

        return ro._resolve_bash()
    except Exception:
        return None
