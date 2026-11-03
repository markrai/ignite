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


def github_release_asset_name(logical_name: str) -> str:
    """Map logical release asset names to GitHub's on-wire asset names.

    GitHub Releases substitutes ``.`` for spaces in uploaded asset filenames.
    Local/authenticated inventory keeps the spaced Scriptlet contract names.
    """
    return logical_name.replace(" ", ".")


def resolve_published_asset_name(
    logical_name: str, remote_by_name: Mapping[str, object]
) -> str:
    """Resolve a logical inventory name against a published release asset map.

    Accepts either the logical name (spaces preserved) or GitHub's rewritten
    dotted form so verification works against real GitHub Releases and against
    fixtures that keep spaced names.
    """
    dotted = github_release_asset_name(logical_name)
    if logical_name in remote_by_name:
        return logical_name
    if dotted in remote_by_name:
        return dotted
    raise ShipError(
        f"published release missing asset for {logical_name!r} "
        f"(also looked for {dotted!r})"
    )


def local_asset_digests(asset_dir: Path, names: Sequence[str]) -> dict[str, str]:
    digests: dict[str, str] = {}
    for name in names:
        path = asset_dir / name
        if not path.is_file():
            raise ShipError(f"local authenticated asset missing: {name}")
        digests[name] = hashlib.sha256(path.read_bytes()).hexdigest()
    if sorted(digests) != sorted(names):
        raise ShipError("asset digest map does not match expected inventory")
    return digests


@dataclass(frozen=True)
class NineFileInventory:
    """Immutable expected publication inventory assembled before promote."""

    root: Path
    version: str
    tag: str
    commit: str
    run_id: int
    asset_names: tuple[str, ...]
    asset_digests: dict[str, str]
    linux_bundle_name: str


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
    asset_names: tuple[str, ...] = ()
    recovered_existing: bool = False
    promote_command: str = ""


def assemble_nine_file_inventory(
    *,
    acquired,
    signed,
    dest: Path,
) -> NineFileInventory:
    """Assemble the exact nine-file candidate via ``stage_nine_file_candidate``.

    Digests are taken only from the staged canonical names — never from a casual
    directory listing.
    """
    import sys

    kindle = Path(__file__).resolve().parents[3] / "deployment" / "kindle"
    if str(kindle) not in sys.path:
        sys.path.insert(0, str(kindle))
    from release_orchestrate import stage_nine_file_candidate

    if dest.exists():
        shutil.rmtree(dest)
    staged = stage_nine_file_candidate(
        acquired=acquired, signed=signed, dest=dest
    )
    identity = acquired.identity
    expected = canonical_nine_file_names(
        version=identity.version,
        linux_bundle_name=identity.linux_bundle_name,
    )
    have = sorted(p.name for p in staged.iterdir() if p.is_file())
    if have != expected:
        raise ShipError(
            f"staged nine-file inventory mismatch: got={have!r} expected={expected!r}"
        )
    digests = local_asset_digests(staged, expected)
    return NineFileInventory(
        root=staged,
        version=identity.version,
        tag=identity.tag,
        commit=identity.ci_head_sha,
        run_id=int(identity.ci_run_id),
        asset_names=tuple(expected),
        asset_digests=digests,
        linux_bundle_name=identity.linux_bundle_name,
    )


def seed_promote_work_from_acquired(*, acquired, work_dir: Path) -> None:
    """Place exact acquired CI bytes where ``promote-release.sh verify`` expects them.

    Layout:
      $WORK/kindle/<scriptlet> + .sha256
      $WORK/linux/<linux bundle> + .sha256

    Does not rebuild artifacts. Refuses to overwrite an existing seeded tree with
    different bytes.
    """
    identity = acquired.identity
    kindle_dir = work_dir / "kindle"
    linux_dir = work_dir / "linux"
    scriptlet_name = identity.scriptlet_name
    scriptlet_sum = f"Install Ignite v{identity.version}.sha256"
    linux_name = identity.linux_bundle_name
    linux_sum = f"{linux_name}.sha256"

    src_scriptlet = acquired.ci_artifacts / scriptlet_name
    if not src_scriptlet.is_file():
        matches = list(acquired.ci_artifacts.rglob(scriptlet_name))
        if len(matches) != 1:
            raise ShipError("cannot locate acquired Scriptlet for promote seed")
        src_scriptlet = matches[0]
    src_sum = acquired.ci_artifacts / scriptlet_sum
    if not src_sum.is_file():
        matches = list(acquired.ci_artifacts.rglob(scriptlet_sum))
        if len(matches) != 1:
            raise ShipError("cannot locate acquired Scriptlet checksum for promote seed")
        src_sum = matches[0]
    src_linux = acquired.linux_dir / linux_name
    src_linux_sum = acquired.linux_dir / linux_sum
    if not src_linux.is_file() or not src_linux_sum.is_file():
        raise ShipError("acquired Linux bundle or checksum missing for promote seed")

    mapping = {
        kindle_dir / scriptlet_name: src_scriptlet,
        kindle_dir / scriptlet_sum: src_sum,
        linux_dir / linux_name: src_linux,
        linux_dir / linux_sum: src_linux_sum,
    }
    for dest, src in mapping.items():
        dest.parent.mkdir(parents=True, exist_ok=True)
        if dest.is_file():
            if hashlib.sha256(dest.read_bytes()).hexdigest() != hashlib.sha256(
                src.read_bytes()
            ).hexdigest():
                raise ShipError(
                    f"promote work seed conflict for {dest.name}: "
                    "existing bytes differ from acquired CI artifact"
                )
            continue
        shutil.copy2(src, dest)


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


def inspect_existing_release(
    *,
    repo_root: Path,
    tag: str,
    expected_commit: str,
    expected_asset_names: Sequence[str],
    expected_digests: Mapping[str, str],
    gh: GitHubClient,
) -> dict[str, object] | None:
    """Return verification result if a matching release already exists; else None.

    Raises ShipError when a release exists but identity/bytes differ.
    """
    view = gh.run(
        [
            "release",
            "view",
            tag,
            "--json",
            "tagName,url,isDraft,isPrerelease,assets",
        ],
        cwd=repo_root,
    )
    if view.returncode != 0:
        text = (view.stderr or view.stdout or "").lower()
        if "not found" in text or "could not find" in text or "http 404" in text:
            return None
        # Ambiguous — treat as absent only on clear miss; otherwise fail closed.
        if "release not found" in text:
            return None
        # Try API 404 probe.
        api = gh.run(
            ["api", f"repos/{{owner}}/{{repo}}/releases/tags/{tag}", "--jq", ".tag_name"],
            cwd=repo_root,
        )
        if api.returncode != 0:
            return None
        # Exists via API; fall through to full verify which will raise on mismatch.
    try:
        return verify_published_release(
            repo_root=repo_root,
            tag=tag,
            expected_commit=expected_commit,
            gh=gh,
            expected_asset_names=expected_asset_names,
            expected_digests=expected_digests,
        )
    except ShipError as exc:
        msg = str(exc).lower()
        if "unable to view published release" in msg:
            return None
        raise ShipError(
            f"existing release {tag} does not match authenticated candidate; "
            f"refusing recovery/overwrite: {exc}"
        ) from exc


def publish_release(
    *,
    repo_root: Path,
    env: Mapping[str, str],
    inventory: NineFileInventory,
    bash: str | None = None,
    dry_run_only: bool = False,
    gh: GitHubClient | None = None,
    promote_command: str = "publish",
    allow_recover_existing: bool = True,
) -> PublishResult:
    """Run promote-release.sh after inventory is fixed; verify remote bytes.

    ``promote_command`` should be ``publish`` when the work directory has already
    been seeded with acquired CI artifacts, or ``all`` to let the shell fetch.
    """
    if "IGNITE_PROMOTE_PUBLIC_KEY_FILE" in env:
        raise ShipError("IGNITE_PROMOTE_PUBLIC_KEY_FILE is forbidden for publish")
    if promote_command not in {"publish", "all"}:
        raise ShipError(f"unsupported promote command: {promote_command}")
    script = repo_root / "tools" / "release" / "promote-release.sh"
    if not script.is_file():
        raise ShipError("promote-release.sh missing")
    tag = env["IGNITE_PROMOTE_TAG"]
    run_id = int(env["IGNITE_PROMOTE_RUN_ID"])
    commit = env["IGNITE_PROMOTE_COMMIT"]
    if tag != inventory.tag or commit != inventory.commit or run_id != inventory.run_id:
        raise ShipError("promote env identity does not match staged inventory")
    if dry_run_only:
        return PublishResult(
            tag=tag,
            run_id=run_id,
            commit=commit,
            release_url=None,
            executed=False,
            inventory_ok=True,
            independently_verified=False,
            asset_digests=dict(inventory.asset_digests),
            asset_names=inventory.asset_names,
            promote_command=promote_command,
        )
    if gh is None:
        raise ShipError("GitHub client required for publication")

    # Recover matching existing release before attempting create.
    if allow_recover_existing:
        existing = inspect_existing_release(
            repo_root=repo_root,
            tag=tag,
            expected_commit=commit,
            expected_asset_names=inventory.asset_names,
            expected_digests=inventory.asset_digests,
            gh=gh,
        )
        if existing is not None:
            return PublishResult(
                tag=tag,
                run_id=run_id,
                commit=commit,
                release_url=str(existing.get("release_url") or ""),
                executed=True,
                inventory_ok=True,
                independently_verified=True,
                asset_digests=dict(existing.get("asset_digests") or {}),
                asset_names=tuple(existing.get("asset_names") or inventory.asset_names),
                recovered_existing=True,
                promote_command="recover-existing",
            )

    bash_bin = bash or _resolve_bash()
    if bash_bin is None:
        raise ShipError("bash required for promote-release.sh publish")
    merged = dict(os.environ)
    merged.update(env)
    completed = subprocess.run(
        [bash_bin, str(script).replace("\\", "/"), promote_command],
        cwd=str(repo_root),
        env=merged,
        capture_output=True,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        text = (completed.stderr or "") + (completed.stdout or "")
        # Race: another process created the release between inspect and create.
        if allow_recover_existing and "already exists" in text.lower():
            existing = inspect_existing_release(
                repo_root=repo_root,
                tag=tag,
                expected_commit=commit,
                expected_asset_names=inventory.asset_names,
                expected_digests=inventory.asset_digests,
                gh=gh,
            )
            if existing is not None:
                return PublishResult(
                    tag=tag,
                    run_id=run_id,
                    commit=commit,
                    release_url=str(existing.get("release_url") or ""),
                    executed=True,
                    inventory_ok=True,
                    independently_verified=True,
                    asset_digests=dict(existing.get("asset_digests") or {}),
                    asset_names=tuple(
                        existing.get("asset_names") or inventory.asset_names
                    ),
                    recovered_existing=True,
                    promote_command="recover-existing",
                )
            raise ShipError(
                "promote reported an existing release that does not match the "
                "authenticated candidate; refusing overwrite"
            )
        raise ShipError(
            f"promote-release.sh {promote_command} failed: " + text[-800:]
        )
    url = None
    for line in (completed.stdout or "").splitlines():
        if "https://github.com/" in line and "/releases/" in line:
            url = line.strip()
            break
        if line.startswith("published:"):
            url = url or f"https://github.com/markrai/ignite/releases/tag/{tag}"

    verification = verify_published_release(
        repo_root=repo_root,
        tag=tag,
        expected_commit=commit,
        gh=gh,
        expected_asset_names=inventory.asset_names,
        expected_digests=inventory.asset_digests,
        local_asset_dir=inventory.root,
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
        asset_names=tuple(verification.get("asset_names") or inventory.asset_names),
        recovered_existing=False,
        promote_command=promote_command,
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
    """Independently verify tag/commit/assets by retrieving remote bytes."""
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
    logical_to_remote = {
        name: resolve_published_asset_name(name, remote_by_name) for name in expected
    }
    expected_remote = sorted(logical_to_remote.values())
    names = sorted(remote_by_name)
    if names != expected_remote:
        raise ShipError(
            "published asset inventory mismatch: "
            f"got={names!r} expected={expected_remote!r} "
            f"(logical={expected!r})"
        )

    if local_asset_dir is not None:
        for name in expected:
            local = local_asset_dir / name
            remote = remote_by_name[logical_to_remote[name]]
            remote_size = remote.get("size") if isinstance(remote, dict) else None
            if remote_size is not None and int(remote_size) != local.stat().st_size:
                raise ShipError(f"published asset size mismatch for {name}")

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
            logical_to_remote=logical_to_remote,
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
        "asset_names": expected,
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
    logical_to_remote: Mapping[str, str] | None = None,
) -> dict[str, str]:
    digests: dict[str, str] = {}
    mapping = dict(logical_to_remote or {})
    for name in names:
        if name not in mapping:
            mapping[name] = resolve_published_asset_name(name, remote_by_name)
    pattern_args: list[str] = []
    for remote_name in mapping.values():
        pattern_args.extend(["--pattern", remote_name])
    downloaded = gh.run(
        ["release", "download", tag, "-D", str(dest), *pattern_args, "--clobber"],
        cwd=repo_root,
    )
    if downloaded.returncode != 0:
        for name in names:
            remote_name = mapping[name]
            out = dest / remote_name
            api = gh.run(
                [
                    "api",
                    "-H",
                    "Accept: application/octet-stream",
                    f"repos/{{owner}}/{{repo}}/releases/assets/"
                    f"{_asset_id(remote_by_name[remote_name])}",
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
        remote_name = mapping[name]
        path = dest / remote_name
        if not path.is_file():
            # gh may write the logical name when patterns use spaces; accept either.
            alt = dest / name
            if alt.is_file():
                path = alt
            else:
                raise ShipError(f"downloaded asset missing after retrieval: {name}")
        logical_path = dest / name
        if path != logical_path:
            if logical_path.exists():
                logical_path.unlink()
            path.replace(logical_path)
            path = logical_path
        remote_meta = remote_by_name[remote_name]
        remote_size = (
            remote_meta.get("size") if isinstance(remote_meta, dict) else None
        )
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
