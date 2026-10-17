"""Recovery-component install/rollback tests (no physical Kindle)."""

from __future__ import annotations

import hashlib
import os
import stat
import subprocess
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

from test_install_runtime import _env_for, _run_install  # noqa: E402
from test_sfx import _posix_env, _require_sh, _to_posix_path  # noqa: E402
from test_validate_stage import FIXTURE_VERSION, make_payload  # noqa: E402


def _recovery_root(kindle: Path) -> Path:
    return kindle / "var" / "local" / "ignite-recovery"


def _run_uninstall(kindle: Path) -> subprocess.CompletedProcess[str]:
    sh = _require_sh()
    script = _HERE / "lib" / "uninstall-runtime.sh"
    env = _env_for(kindle)
    env["IGNITE_SKIP_REGISTER"] = "1"
    return subprocess.run(
        [sh, _to_posix_path(script)],
        capture_output=True,
        text=True,
        env=env,
    )


def test_first_install_and_idempotent_reinstall(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    rec = _recovery_root(kindle) / "bin" / "dropbearmulti"
    assert rec.is_file()
    want = (payload / "recovery" / "manifest" / "elf.sha256").read_text().split()[0]
    have = hashlib.sha256(rec.read_bytes()).hexdigest()
    assert have == want
    second = _run_install(payload, kindle)
    assert second.returncode == 0, second.stdout + second.stderr
    assert "already current" in second.stdout


def test_upgrade_keeps_previous(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    assert _run_install(payload, kindle).returncode == 0
    old = (_recovery_root(kindle) / "bin" / "dropbearmulti").read_bytes()
    new = old + b"UP"
    (payload / "recovery" / "bin" / "dropbearmulti").write_bytes(new)
    (payload / "recovery" / "manifest" / "elf.sha256").write_text(
        hashlib.sha256(new).hexdigest() + "\n", encoding="utf-8"
    )
    upgraded = _run_install(payload, kindle)
    assert upgraded.returncode == 0, upgraded.stdout + upgraded.stderr
    prev = _recovery_root(kindle) / "previous" / "dropbearmulti"
    assert prev.read_bytes() == old
    assert (_recovery_root(kindle) / "bin" / "dropbearmulti").read_bytes() == new


def test_digest_mismatch_refuses(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    (payload / "recovery" / "manifest" / "elf.sha256").write_text(
        "0" * 64 + "\n", encoding="utf-8"
    )
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    failed = _run_install(payload, kindle)
    assert failed.returncode != 0
    assert "digest mismatch" in (failed.stdout + failed.stderr)
    assert not (_recovery_root(kindle) / "bin" / "dropbearmulti").exists()


def test_truncated_payload_refuses(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    failed = _run_install(payload, kindle, {"IGNITE_RECOVERY_FAIL_AT": "truncated"})
    assert failed.returncode != 0
    assert "truncated" in (failed.stdout + failed.stderr)


def test_insufficient_space_refuses(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    failed = _run_install(
        payload,
        kindle,
        {"IGNITE_SKIP_SPACE_CHECK": "0", "IGNITE_RECOVERY_AVAIL_KB": "1", "IGNITE_AVAIL_KB": "999999"},
    )
    # App space check uses IGNITE_AVAIL_KB; recovery uses IGNITE_RECOVERY_AVAIL_KB.
    # SKIP_SPACE_CHECK=0 re-enables both; set app avail high.
    assert failed.returncode != 0
    assert "not enough free space" in (failed.stdout + failed.stderr)


def test_interrupted_upgrade_preserves_live(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    assert _run_install(payload, kindle).returncode == 0
    live = (_recovery_root(kindle) / "bin" / "dropbearmulti").read_bytes()
    new = live + b"NEWER"
    (payload / "recovery" / "bin" / "dropbearmulti").write_bytes(new)
    (payload / "recovery" / "manifest" / "elf.sha256").write_text(
        hashlib.sha256(new).hexdigest() + "\n", encoding="utf-8"
    )
    failed = _run_install(payload, kindle, {"IGNITE_RECOVERY_FAIL_AT": "before-publish"})
    assert failed.returncode != 0
    assert (_recovery_root(kindle) / "bin" / "dropbearmulti").read_bytes() == live
    recovered = _run_install(payload, kindle)
    assert recovered.returncode == 0, recovered.stdout + recovered.stderr
    assert (_recovery_root(kindle) / "bin" / "dropbearmulti").read_bytes() == new


def test_symlink_root_refused(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    parent = kindle / "var" / "local"
    parent.mkdir(parents=True)
    target = parent / "other"
    target.mkdir()
    root = parent / "ignite-recovery"
    try:
        os.symlink(target, root, target_is_directory=True)
    except OSError as exc:
        pytest.skip(f"symlinks unavailable: {exc}")
    failed = _run_install(payload, kindle)
    assert failed.returncode != 0
    assert "symlink" in (failed.stdout + failed.stderr)


def test_chown_failure_refuses(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    failed = _run_install(
        payload,
        kindle,
        {"IGNITE_RECOVERY_SKIP_CHOWN": "0", "IGNITE_RECOVERY_FAIL_CHOWN": "1"},
    )
    assert failed.returncode != 0
    assert "ownership" in (failed.stdout + failed.stderr)


def test_chmod_failure_refuses(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    failed = _run_install(payload, kindle, {"IGNITE_RECOVERY_FAIL_CHMOD": "1"})
    assert failed.returncode != 0
    assert "permission" in (failed.stdout + failed.stderr)


def test_publish_failure_refuses(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    failed = _run_install(payload, kindle, {"IGNITE_RECOVERY_FAIL_PUBLISH": "1"})
    assert failed.returncode != 0
    assert "publication" in (failed.stdout + failed.stderr)


def test_uninstall_removes_recovery_keeps_notebooks(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    notebooks = kindle / "scribe-data"
    notebooks.mkdir()
    (notebooks / "keep.txt").write_text("notebook", encoding="utf-8")
    assert _run_install(payload, kindle).returncode == 0
    result = _run_uninstall(kindle)
    assert result.returncode == 0, result.stdout + result.stderr
    assert not (_recovery_root(kindle) / "bin" / "dropbearmulti").exists()
    assert (notebooks / "keep.txt").read_text(encoding="utf-8") == "notebook"


def test_unexpected_preexisting_file_replaced_after_snapshot(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    rec_bin = _recovery_root(kindle) / "bin"
    rec_bin.mkdir(parents=True)
    (rec_bin / "dropbearmulti").write_bytes(b"\x7fELF" + b"OLD" * 100)
    result = _run_install(payload, kindle)
    assert result.returncode == 0, result.stdout + result.stderr
    want = hashlib.sha256(
        (payload / "recovery" / "bin" / "dropbearmulti").read_bytes()
    ).hexdigest()
    have = hashlib.sha256(
        (_recovery_root(kindle) / "bin" / "dropbearmulti").read_bytes()
    ).hexdigest()
    assert have == want
    assert (_recovery_root(kindle) / "previous" / "dropbearmulti").is_file()
