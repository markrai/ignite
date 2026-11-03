#!/usr/bin/env python3
"""One-shot State B runner: provision a disposable pin in a repo copy and test.

Does not touch the real repository pin. Intended for local 3p validation only.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

_REPO = Path(__file__).resolve().parents[2]
_OVERLAY = (
    "deployment/kindle/production_trust_boundary.py",
    "deployment/kindle/authenticated_publication_evidence.py",
    "deployment/kindle/authenticated_release_assets.py",
    "deployment/kindle/release_orchestrate.py",
    "deployment/kindle/test_provisioned_pin_rehearsal.py",
    "deployment/kindle/test_authenticated_publication_preflight.py",
    "deployment/kindle/test_ci_packaging_measurement.py",
)

_PYTEST_TARGETS = [
    "deployment/kindle/test_provisioned_pin_rehearsal.py",
    "deployment/kindle/test_production_trust_boundary.py",
    "deployment/kindle/test_authenticated_publication_preflight.py",
    "deployment/kindle/test_authenticated_publication_evidence.py",
    "deployment/kindle/test_offline_release_ceremony.py",
    "deployment/kindle/test_production_signing_ceremony_rehearsal.py",
    "deployment/kindle/test_release_acquire.py",
    "deployment/kindle/test_ci_packaging_measurement.py",
    "tools/release/test_kindle_release_sign.py",
]


def assert_not_real_checkout(worktree: Path, real_repo: Path = _REPO) -> None:
    """Refuse to treat the live checkout as a disposable State B destination."""
    wt = worktree.resolve()
    real = real_repo.resolve()
    if wt == real:
        raise ValueError(
            "refusing State B provision against the real repository checkout; "
            "pass a disposable git worktree or copy path"
        )
    real_pin = (real / "tools" / "release" / "kindle-release-public-key.hex").resolve()
    candidate_pin = (wt / "tools" / "release" / "kindle-release-public-key.hex").resolve()
    if candidate_pin == real_pin:
        raise ValueError(
            "refusing State B destination that shares the real production pin path"
        )


def _overlay(src_repo: Path, dest: Path) -> None:
    assert_not_real_checkout(dest, src_repo)
    for rel in _OVERLAY:
        src = src_repo / rel
        dst = dest / rel
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, dst)


def _provision_pin(dest: Path) -> bytes:
    sys.path.insert(0, str(dest / "deployment" / "kindle"))
    sys.path.insert(0, str(dest / "tools" / "release"))
    from authenticated_publication_evidence import generate_test_trust_root

    # Reset pin to UNPROVISIONED before generating disposable keys so fixture
    # helpers accept the new identity, then write the matching public pin.
    pin = dest / "tools" / "release" / "kindle-release-public-key.hex"
    pin.write_bytes(b"UNPROVISIONED\n")
    keys = dest / ".3p-disposable-keys" / f"run-{os.getpid()}"
    if keys.exists():
        shutil.rmtree(keys)
    root = generate_test_trust_root(keys, label="stateb-publisher")
    pin.write_bytes(root.public_key.hex().encode("ascii") + b"\n")
    return root.public_key


def main_from_args(argv: list[str]) -> int:
    worktree = Path(argv[0]).resolve() if argv else None
    if worktree is None or not worktree.is_dir():
        print("usage: _run_stateb_provisioned_copy.py <worktree-path>", file=sys.stderr)
        return 2
    try:
        assert_not_real_checkout(worktree, _REPO)
    except ValueError as exc:
        print(str(exc), file=sys.stderr)
        return 2

    real_pin = (_REPO / "tools" / "release" / "kindle-release-public-key.hex").read_bytes()
    _overlay(_REPO, worktree)
    pub = _provision_pin(worktree)
    copy_pin = (worktree / "tools" / "release" / "kindle-release-public-key.hex").read_bytes()
    if real_pin != b"UNPROVISIONED\n":
        # Soft check: report, do not abort State B if operator already provisioned.
        print(f"NOTE: real repo pin is not UNPROVISIONED ({real_pin!r})")
    assert copy_pin == pub.hex().encode("ascii") + b"\n"
    assert (_REPO / "tools" / "release" / "kindle-release-public-key.hex").read_bytes() == real_pin
    print(f"STATEB_ROOT={worktree}")
    print(f"STATEB_PIN_HEX={pub.hex()}")
    print(f"REAL_PIN_BYTES={real_pin!r}")
    print(f"REAL_PIN_UNCHANGED_PRE={True}")

    env = os.environ.copy()
    env["PYTHONPATH"] = os.pathsep.join(
        [
            str(worktree / "deployment" / "kindle"),
            str(worktree / "tools" / "release"),
            env.get("PYTHONPATH", ""),
        ]
    )
    cmd = [
        sys.executable,
        "-m",
        "pytest",
        *(_PYTEST_TARGETS),
        "-q",
        "--tb=line",
    ]
    print("RUNNING:", " ".join(cmd))
    completed = subprocess.run(cmd, cwd=worktree, env=env)
    # Re-confirm real pin untouched after suite.
    after = (_REPO / "tools" / "release" / "kindle-release-public-key.hex").read_bytes()
    if after != real_pin:
        print("ERROR: real repository pin mutated during State B", file=sys.stderr)
        return 1
    print(f"STATEB_PYTEST_EXIT={completed.returncode}")
    return completed.returncode


def main() -> int:
    return main_from_args(sys.argv[1:])


if __name__ == "__main__":
    raise SystemExit(main())
