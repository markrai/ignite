"""Option A local publication via promote-release.sh."""

from __future__ import annotations

import os
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping

from .errors import ShipError


@dataclass(frozen=True)
class PublishResult:
    tag: str
    run_id: int
    commit: str
    release_url: str | None
    executed: bool
    inventory_ok: bool


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
    # Hard refuse fixture override for publication.
    env.pop("IGNITE_PROMOTE_PUBLIC_KEY_FILE", None)
    return env


def publish_release(
    *,
    repo_root: Path,
    env: Mapping[str, str],
    bash: str | None = None,
    dry_run_only: bool = False,
) -> PublishResult:
    """Run promote-release.sh publish (or refuse when dry_run_only)."""
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
    return PublishResult(
        tag=tag,
        run_id=run_id,
        commit=commit,
        release_url=url,
        executed=True,
        inventory_ok=True,
    )


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
