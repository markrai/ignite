"""Focused tests for com.markrai.scribe → com.markrai.ignite appreg migration."""

from __future__ import annotations

import sqlite3
import subprocess
import sys
from pathlib import Path

_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

from test_install_runtime import (  # noqa: E402
    _appreg_query,
    _register_env,
    _run_install,
    _seed_appreg,
    _to_posix_path,
    _write_register_stub,
)
from test_sfx import _posix_env, _require_sh  # noqa: E402
from test_validate_stage import make_payload  # noqa: E402

CANONICAL = "com.markrai.ignite"
LEGACY = "com.markrai.scribe"


def _handler_count(appreg: Path, handler: str) -> str:
    return _appreg_query(appreg, f"SELECT COUNT(*) FROM handlerIds WHERE handlerId='{handler}'")


def _handler_cmd(appreg: Path, handler: str) -> str:
    return _appreg_query(
        appreg,
        f"SELECT value FROM properties WHERE handlerId='{handler}' AND name='command'",
    )


def _seed_legacy(appreg: Path, command: str) -> None:
    _seed_appreg(appreg, ignite_command=command, handler_id=LEGACY)


def test_fresh_install_registers_canonical_only(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    appreg = kindle / "var" / "local" / "appreg.db"
    _seed_appreg(appreg, ignite_command=None)
    stub = tmp_path / "ok-register.sh"
    _write_register_stub(stub, succeed=True)
    result = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {"IGNITE_REGISTER_STUB": _to_posix_path(stub)}),
    )
    assert result.returncode == 0, result.stdout + result.stderr
    expect = _to_posix_path(kindle / "scribe-app" / "bin" / "scribe-lifecycle-host.sh")
    assert _handler_count(appreg, CANONICAL) == "1"
    assert _handler_cmd(appreg, CANONICAL) == expect
    assert _handler_count(appreg, LEGACY) == "0"
    assert (kindle / "scribe-data" / "n" / "page.meta").read_text(encoding="utf-8") == "keep\n"
    assert _appreg_query(appreg, "SELECT x FROM unrelated_lab126") == "keep-system"


def test_upgrade_from_legacy_registers_new_and_retires_old(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    note = kindle / "scribe-data" / "notebooks" / "n1" / "page.meta"
    note.parent.mkdir(parents=True)
    note.write_text("legacy-notebook\n", encoding="utf-8")
    sync_id = kindle / "scribe-app" / "state" / "sync" / "identity.json"
    sync_id.parent.mkdir(parents=True)
    sync_id.write_text('{"device":"keep-me"}\n', encoding="utf-8")
    appreg = kindle / "var" / "local" / "appreg.db"
    host = _to_posix_path(kindle / "scribe-app" / "bin" / "scribe-lifecycle-host.sh")
    # Pre-create host path string used in legacy command (file may not exist yet).
    _seed_legacy(appreg, host)
    stub = tmp_path / "ok-register.sh"
    _write_register_stub(stub, succeed=True)
    result = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {"IGNITE_REGISTER_STUB": _to_posix_path(stub)}),
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "legacy_retire_ok" in (result.stdout + result.stderr) or _handler_count(appreg, LEGACY) == "0"
    assert _handler_count(appreg, CANONICAL) == "1"
    assert _handler_cmd(appreg, CANONICAL) == host
    assert _handler_count(appreg, LEGACY) == "0"
    assert note.read_text(encoding="utf-8") == "legacy-notebook\n"
    assert sync_id.read_text(encoding="utf-8") == '{"device":"keep-me"}\n'


def test_idempotent_reinstall_after_migration(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    appreg = kindle / "var" / "local" / "appreg.db"
    host = _to_posix_path(kindle / "scribe-app" / "bin" / "scribe-lifecycle-host.sh")
    _seed_legacy(appreg, host)
    stub = tmp_path / "ok-register.sh"
    _write_register_stub(stub, succeed=True)
    env = _register_env(tmp_path, kindle, {"IGNITE_REGISTER_STUB": _to_posix_path(stub)})
    first = _run_install(payload, kindle, env)
    assert first.returncode == 0, first.stdout + first.stderr
    second = _run_install(payload, kindle, env)
    assert second.returncode == 0, second.stdout + second.stderr
    assert _handler_count(appreg, CANONICAL) == "1"
    assert _handler_count(appreg, LEGACY) == "0"
    assert (kindle / "scribe-data" / "n" / "page.meta").read_text(encoding="utf-8") == "keep\n"


def test_failed_new_registration_leaves_legacy_intact(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    appreg = kindle / "var" / "local" / "appreg.db"
    legacy_cmd = "/mnt/us/scribe-app/bin/scribe-lifecycle-host.sh"
    _seed_legacy(appreg, legacy_cmd)
    stub = tmp_path / "fail-register.sh"
    _write_register_stub(stub, succeed=False)
    failed = _run_install(
        payload,
        kindle,
        _register_env(
            tmp_path,
            kindle,
            {
                "IGNITE_REGISTER_STUB": _to_posix_path(stub),
                "IGNITE_FAIL_REGISTER_NEW": "1",
            },
        ),
    )
    assert failed.returncode != 0
    assert _handler_count(appreg, LEGACY) == "1"
    assert _handler_cmd(appreg, LEGACY) == legacy_cmd
    # Injected failure should not create a working canonical row.
    assert _handler_count(appreg, CANONICAL) in ("0", "")
    assert (kindle / "scribe-data" / "n" / "page.meta").read_text(encoding="utf-8") == "keep\n"


def test_legacy_retire_failure_keeps_working_canonical(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    appreg = kindle / "var" / "local" / "appreg.db"
    host = _to_posix_path(kindle / "scribe-app" / "bin" / "scribe-lifecycle-host.sh")
    _seed_legacy(appreg, host)
    stub = tmp_path / "ok-register.sh"
    _write_register_stub(stub, succeed=True)
    result = _run_install(
        payload,
        kindle,
        _register_env(
            tmp_path,
            kindle,
            {
                "IGNITE_REGISTER_STUB": _to_posix_path(stub),
                "IGNITE_FAIL_LEGACY_RETIRE": "1",
            },
        ),
    )
    # Install succeeds: user still has a working Ignite launch path.
    assert result.returncode == 0, result.stdout + result.stderr
    assert "migrate_warn" in (result.stdout + result.stderr)
    assert _handler_count(appreg, CANONICAL) == "1"
    assert _handler_cmd(appreg, CANONICAL) == host
    assert _handler_count(appreg, LEGACY) == "1"
    assert (kindle / "scribe-data" / "n" / "page.meta").read_text(encoding="utf-8") == "keep\n"


def test_repair_clears_duplicate_after_partial_migration(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    appreg = kindle / "var" / "local" / "appreg.db"
    host = _to_posix_path(kindle / "scribe-app" / "bin" / "scribe-lifecycle-host.sh")
    # Simulate interrupted migration: both handlers present, canonical already correct.
    _seed_appreg(appreg, ignite_command=host, handler_id=CANONICAL)
    con = sqlite3.connect(appreg)
    con.execute("INSERT INTO handlerIds(handlerId) VALUES (?)", (LEGACY,))
    con.execute(
        "INSERT INTO properties(handlerId,name,value) VALUES (?,'command',?)",
        (LEGACY, host),
    )
    con.commit()
    con.close()
    stub = tmp_path / "ok-register.sh"
    _write_register_stub(stub, succeed=True)
    repaired = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {"IGNITE_REGISTER_STUB": _to_posix_path(stub)}),
    )
    assert repaired.returncode == 0, repaired.stdout + repaired.stderr
    assert _handler_count(appreg, CANONICAL) == "1"
    assert _handler_count(appreg, LEGACY) == "0"


def test_uninstall_after_migration_removes_both_keeps_data(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    note = kindle / "scribe-data" / "n" / "page.meta"
    note.parent.mkdir(parents=True)
    note.write_text("keep\n", encoding="utf-8")
    appreg = kindle / "var" / "local" / "appreg.db"
    host = _to_posix_path(kindle / "scribe-app" / "bin" / "scribe-lifecycle-host.sh")
    _seed_legacy(appreg, host)
    stub = tmp_path / "ok-register.sh"
    _write_register_stub(stub, succeed=True)
    installed = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {"IGNITE_REGISTER_STUB": _to_posix_path(stub)}),
    )
    assert installed.returncode == 0, installed.stdout + installed.stderr
    # Force both present to exercise dual unregister (repair-like leftover).
    con = sqlite3.connect(appreg)
    con.execute("INSERT OR IGNORE INTO handlerIds(handlerId) VALUES (?)", (LEGACY,))
    con.execute(
        "INSERT INTO properties(handlerId,name,value) VALUES (?,'command',?)",
        (LEGACY, host),
    )
    con.commit()
    con.close()

    sh = _require_sh()
    env = _posix_env(sh)
    env.update(
        {
            "IGNITE_APP_ROOT": _to_posix_path(kindle / "scribe-app"),
            "IGNITE_DATA_DIR": _to_posix_path(kindle / "scribe-data"),
            "IGNITE_DOCS": _to_posix_path(kindle / "documents"),
            "IGNITE_APPREG": _to_posix_path(appreg),
            "IGNITE_SQLITE3": _register_env(tmp_path, kindle)["IGNITE_SQLITE3"],
            "IGNITE_SKIP_APPREG_BACKUP": "1",
            "IGNITE_RECOVERY_ALLOW_NONROOT": "1",
            "IGNITE_RECOVERY_SKIP_CHOWN": "1",
            "IGNITE_RECOVERY_ROOT": _to_posix_path(kindle / "var" / "local" / "ignite-recovery"),
            "IGNITE_SKIP_PROC_CHECK": "1",
        }
    )
    # install-recovery sourced by uninstall needs helpers present
    tools = kindle / "scribe-app" / "tools" / "ignite-release" / "lib"
    # uninstall uses LIB_DIR of the script itself
    ran = subprocess.run(
        [sh, _to_posix_path(_HERE / "lib" / "uninstall-runtime.sh")],
        capture_output=True,
        text=True,
        env=env,
    )
    assert ran.returncode == 0, ran.stdout + ran.stderr
    assert _handler_count(appreg, CANONICAL) == "0"
    assert _handler_count(appreg, LEGACY) == "0"
    assert note.read_text(encoding="utf-8") == "keep\n"
    assert _appreg_query(appreg, "SELECT value FROM properties WHERE handlerId='system.other'") == "/usr/bin/other"
    del tools


def test_unrelated_handler_not_removed_on_legacy_retire(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    appreg = kindle / "var" / "local" / "appreg.db"
    host = _to_posix_path(kindle / "scribe-app" / "bin" / "scribe-lifecycle-host.sh")
    _seed_legacy(appreg, host)
    # Add a non-Ignite handler that happens to share the legacy id shape? Use foreign command.
    con = sqlite3.connect(appreg)
    con.execute("INSERT INTO handlerIds(handlerId) VALUES ('com.example.other')")
    con.execute(
        "INSERT INTO properties(handlerId,name,value) VALUES ('com.example.other','command','/usr/bin/other-app')"
    )
    con.commit()
    con.close()
    stub = tmp_path / "ok-register.sh"
    _write_register_stub(stub, succeed=True)
    result = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {"IGNITE_REGISTER_STUB": _to_posix_path(stub)}),
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert _handler_count(appreg, "com.example.other") == "1"
    assert _handler_count(appreg, CANONICAL) == "1"
    assert _handler_count(appreg, LEGACY) == "0"


def test_diagnose_reports_registration_states(tmp_path: Path) -> None:
    kindle = tmp_path / "kindle"
    app = kindle / "scribe-app"
    appreg = kindle / "var" / "local" / "appreg.db"
    host_posix = _to_posix_path(app / "bin" / "scribe-lifecycle-host.sh")
    _seed_appreg(appreg, ignite_command=host_posix, handler_id=CANONICAL)
    con = sqlite3.connect(appreg)
    con.execute("INSERT INTO handlerIds(handlerId) VALUES (?)", (LEGACY,))
    con.execute(
        "INSERT INTO properties(handlerId,name,value) VALUES (?,'command',?)",
        (LEGACY, host_posix),
    )
    con.commit()
    con.close()

    from test_install_runtime import _make_sqlite3_wrapper

    sqlite_wrapper = _make_sqlite3_wrapper(tmp_path / "sqlite3-bin")
    sh = _require_sh()
    env = _posix_env(sh)
    env.update(
        {
            "IGNITE_APP_ROOT": _to_posix_path(app),
            "IGNITE_APPREG": _to_posix_path(appreg),
            "IGNITE_SQLITE3": sqlite_wrapper,
            "IGNITE_SKIP_APPREG_BACKUP": "1",
        }
    )
    helpers = _to_posix_path(_HERE / "lib" / "register-helpers.sh")
    script = tmp_path / "diag.sh"
    script.write_text(
        "#!/bin/sh\nset -eu\n"
        f'. "{helpers}"\n'
        "ignite_diagnose_registration\n",
        encoding="utf-8",
        newline="\n",
    )
    ran = subprocess.run(
        [sh, _to_posix_path(script)],
        capture_output=True,
        text=True,
        env=env,
    )
    assert ran.returncode == 0, ran.stdout + ran.stderr
    out = ran.stdout + ran.stderr
    assert "registration_state=both_present" in out
    assert f"canonical_handler={CANONICAL}" in out
    assert f"legacy_handler={LEGACY}" in out


def test_migration_does_not_touch_recovery_flags() -> None:
    helpers = (_HERE / "lib" / "register-helpers.sh").read_text(encoding="utf-8")
    install = (_HERE / "lib" / "install-runtime.sh").read_text(encoding="utf-8")
    assert "LIVE_NON_LOOPBACK_POST_ENABLED" not in helpers
    assert "ignite_register_and_migrate" in helpers
    assert "LEGACY_HANDLER=com.markrai.scribe" in helpers
    assert "HANDLER=com.markrai.ignite" in helpers
    assert "ignite_register_and_migrate" in install
    assert "registration/ignite.json" in install
