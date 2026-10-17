"""Mock-tree tests for install-runtime rollback and Diagnose (no personal Kindle)."""

from __future__ import annotations

import os
import sqlite3
import stat
import subprocess
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

from test_sfx import _posix_env, _require_sh, _to_posix_path  # noqa: E402
from test_validate_stage import FIXTURE_VERSION, make_payload  # noqa: E402

# Must match activate_mv names in lib/install-runtime.sh (optional: description).
REQUIRED_ACTIVATION_BOUNDARIES = [
    "scribe-app",
    "scribe-lifecycle",
    "host",
    "icon",
    "scriptlet",
    "ofl",
    "helper-install-runtime",
    "helper-uninstall-runtime",
    "helper-backup-runtime",
    "helper-register-helpers",
    "helper-install-recovery",
    "uninstall-docs",
    "backup-docs",
    "diagnose-docs",
]
OPTIONAL_ACTIVATION_BOUNDARIES = ["description"]
ALL_ACTIVATION_BOUNDARIES = REQUIRED_ACTIVATION_BOUNDARIES + OPTIONAL_ACTIVATION_BOUNDARIES


def _env_for(kindle: Path, extra: dict[str, str] | None = None) -> dict[str, str]:
    sh = _require_sh()
    env = _posix_env(sh)
    app = kindle / "scribe-app"
    env.update(
        {
            "IGNITE_APP_ROOT": _to_posix_path(app),
            "IGNITE_DATA_DIR": _to_posix_path(kindle / "scribe-data"),
            "IGNITE_DOCS": _to_posix_path(kindle / "documents"),
            "IGNITE_US_ROOT": _to_posix_path(kindle),
            "IGNITE_SKIP_SH_CHECK": "1",
            "IGNITE_SKIP_PROC_CHECK": "1",
            "IGNITE_SKIP_BIN_SMOKE": "1",
            "IGNITE_SKIP_SU_CHECK": "1",
            "IGNITE_SKIP_REGISTER": "1",
            "IGNITE_SKIP_APPREG_BACKUP": "1",
            "IGNITE_SKIP_ELF_CHECK": "1",
            "IGNITE_SKIP_SPACE_CHECK": "1",
            "IGNITE_RECOVERY_ALLOW_NONROOT": "1",
            "IGNITE_RECOVERY_SKIP_CHOWN": "1",
            "IGNITE_RECOVERY_ROOT": _to_posix_path(kindle / "var" / "local" / "ignite-recovery"),
        }
    )
    if extra:
        env.update(extra)
    return env


def _run_install(payload: Path, kindle: Path, extra: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    sh = _require_sh()
    script = _HERE / "lib" / "install-runtime.sh"
    return subprocess.run(
        [sh, _to_posix_path(script), _to_posix_path(payload)],
        capture_output=True,
        text=True,
        env=_env_for(kindle, extra),
    )


def test_missing_su_fails_closed(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "notebook-1").mkdir(parents=True)
    (kindle / "scribe-data" / "notebook-1" / "page.meta").write_text("keep\n", encoding="utf-8")
    result = _run_install(
        payload,
        kindle,
        {"IGNITE_SKIP_SU_CHECK": "0", "IGNITE_SU": _to_posix_path(tmp_path / "no-such-su")},
    )
    assert result.returncode != 0
    combined = result.stdout + result.stderr
    assert "preparation:" in combined
    assert "root helper" in combined
    assert (kindle / "scribe-data" / "notebook-1" / "page.meta").read_text(encoding="utf-8") == "keep\n"
    assert not (kindle / "scribe-app" / "bin" / "scribe-app").exists()


def test_first_install_then_repeat(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    assert (kindle / "scribe-app" / "bin" / "scribe-app").is_file()
    assert (kindle / "documents" / "Diagnose Ignite.sh").is_file()
    second = _run_install(payload, kindle)
    assert second.returncode == 0, second.stdout + second.stderr
    assert (kindle / "scribe-data" / "n" / "page.meta").read_text(encoding="utf-8") == "keep\n"


def test_interrupted_new_files_are_cleared(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    app_bin = kindle / "scribe-app" / "bin"
    app_bin.mkdir(parents=True)
    leftover = app_bin / "scribe-app.new"
    leftover.write_bytes(b"stale")
    result = _run_install(payload, kindle)
    assert result.returncode == 0, result.stdout + result.stderr
    assert not leftover.exists()
    assert (app_bin / "scribe-app").is_file()


def test_failed_register_restores_previous_runtime(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    marker = kindle / "scribe-data" / "n" / "page.meta"
    marker.write_text("keep\n", encoding="utf-8")
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    old_app = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    (payload / "bin" / "scribe-app").write_bytes(old_app + b"NEW")
    stub = tmp_path / "fail-register.sh"
    stub.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8", newline="\n")
    failed = _run_install(
        payload,
        kindle,
        {
            "IGNITE_SKIP_REGISTER": "0",
            "IGNITE_REGISTER_STUB": _to_posix_path(stub),
            "IGNITE_SKIP_APPREG_BACKUP": "1",
        },
    )
    assert failed.returncode != 0
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == old_app
    assert marker.read_text(encoding="utf-8") == "keep\n"


def test_failed_verify_restores_and_preserves_data(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    old_app = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    (payload / "bin" / "scribe-app").write_bytes(old_app + b"NEW")
    failed = _run_install(payload, kindle, {"IGNITE_FORCE_VERIFY_FAIL": "1"})
    assert failed.returncode != 0
    assert "post-install verification failed" in (failed.stdout + failed.stderr)
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == old_app
    assert (kindle / "scribe-data" / "n" / "page.meta").read_text(encoding="utf-8") == "keep\n"


def test_diagnose_writes_usb_report_without_notebooks(tmp_path: Path) -> None:
    kindle = tmp_path / "kindle"
    docs = kindle / "documents"
    app = kindle / "scribe-app"
    docs.mkdir(parents=True)
    (app / "bin").mkdir(parents=True)
    (app / "logs").mkdir(parents=True)
    host = app / "bin" / "scribe-lifecycle-host.sh"
    host.write_text(
        "#!/bin/sh\nexec /x --confirm-write --data-dir /mnt/us/scribe-data "
        "--sync-dir /mnt/us/scribe-app/state/sync "
        "--device-dir /mnt/us/scribe-app/state/device\n",
        encoding="utf-8",
        newline="\n",
    )
    (app / "bin" / "scribe-app").write_bytes(b"\x7fELF" + b"\0" * 20)
    (app / "bin" / "scribe-lifecycle").write_bytes(b"\x7fELF" + b"\0" * 20)
    sh = _require_sh()
    env = _posix_env(sh)
    env.update(
        {
            "IGNITE_APP_ROOT": _to_posix_path(app),
            "IGNITE_DOCS": _to_posix_path(docs),
            "IGNITE_SU": _to_posix_path(tmp_path / "no-su"),
        }
    )
    ran = subprocess.run(
        [sh, _to_posix_path(_HERE / "Diagnose-Ignite.sh")],
        capture_output=True,
        text=True,
        env=env,
    )
    assert ran.returncode == 0, ran.stdout + ran.stderr
    report = (docs / "Ignite-Diagnose.txt").read_text(encoding="utf-8")
    assert "su_executable=no" in report
    assert "host_profile=sync_flags_present" in report
    assert "canonical_handler=com.markrai.ignite" in report
    assert "legacy_handler=com.markrai.scribe" in report
    assert "--- framebuffer sysfs (read-only) ---" in report
    assert "--- framebuffer ioctl (FBIOGET only) ---" in report
    assert "does not start Ignite" in report
    assert not (kindle / "scribe-data").exists()


def _make_sqlite3_wrapper(tmp_path: Path) -> str:
    tmp_path.mkdir(parents=True, exist_ok=True)
    wrapper = tmp_path / "ignite-sqlite3.sh"
    py = _to_posix_path(Path(sys.executable))
    shim = _to_posix_path(_HERE / "test_sqlite3_shim.py")
    wrapper.write_text(
        f'#!/bin/sh\nexec "{py}" "{shim}" "$@"\n',
        encoding="utf-8",
        newline="\n",
    )
    wrapper.chmod(wrapper.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return _to_posix_path(wrapper)


def _seed_appreg(
    path: Path,
    *,
    ignite_command: str | None = "/old/host",
    handler_id: str = "com.markrai.ignite",
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    con = sqlite3.connect(path)
    con.executescript(
        """
        CREATE TABLE IF NOT EXISTS handlerIds (handlerId TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS properties (handlerId TEXT, name TEXT, value TEXT);
        CREATE TABLE IF NOT EXISTS associations (
          handlerId TEXT, interface TEXT, contentId TEXT, defaultAssoc TEXT
        );
        CREATE TABLE IF NOT EXISTS unrelated_lab126 (x TEXT);
        INSERT INTO unrelated_lab126(x) VALUES ('keep-system');
        INSERT INTO handlerIds(handlerId) VALUES ('system.other');
        INSERT INTO properties(handlerId,name,value)
          VALUES ('system.other','command','/usr/bin/other');
        INSERT INTO associations(handlerId,interface,contentId,defaultAssoc)
          VALUES ('system.other','application','MT:other','Y');
        """
    )
    if ignite_command is not None:
        con.execute("INSERT INTO handlerIds(handlerId) VALUES (?)", (handler_id,))
        con.execute(
            "INSERT INTO properties(handlerId,name,value) VALUES (?,'command',?)",
            (handler_id, ignite_command),
        )
        con.execute(
            "INSERT INTO associations(handlerId,interface,contentId,defaultAssoc) "
            "VALUES (?,'application','MT:scribe','Y')",
            (handler_id,),
        )
    con.commit()
    con.close()


def _appreg_query(path: Path, sql: str) -> str:
    con = sqlite3.connect(path)
    try:
        row = con.execute(sql).fetchone()
        return "" if row is None or row[0] is None else str(row[0])
    finally:
        con.close()


def _register_env(tmp_path: Path, kindle: Path, extra: dict[str, str] | None = None) -> dict[str, str]:
    env = {
        "IGNITE_SKIP_REGISTER": "0",
        "IGNITE_SKIP_APPREG_BACKUP": "0",
        "IGNITE_SQLITE3": _make_sqlite3_wrapper(tmp_path / "sqlite3-bin"),
        "IGNITE_APPREG": _to_posix_path(kindle / "var" / "local" / "appreg.db"),
    }
    if extra:
        env.update(extra)
    return env


def _write_register_stub(path: Path, *, succeed: bool) -> None:
    if succeed:
        body = """#!/bin/sh
APPREG=$2
"$IGNITE_SQLITE3" "$APPREG" "INSERT OR IGNORE INTO handlerIds(handlerId) VALUES('com.markrai.ignite');"
"$IGNITE_SQLITE3" "$APPREG" "DELETE FROM properties WHERE handlerId='com.markrai.ignite' AND name='command';"
"$IGNITE_SQLITE3" "$APPREG" "INSERT INTO properties(handlerId,name,value) VALUES('com.markrai.ignite','command','$IGNITE_APP_ROOT/bin/scribe-lifecycle-host.sh');"
if [ -n "${IGNITE_REG_MARK:-}" ]; then
  "$IGNITE_SQLITE3" "$APPREG" "DELETE FROM properties WHERE handlerId='com.markrai.ignite' AND name='igniteMark';"
  "$IGNITE_SQLITE3" "$APPREG" "INSERT INTO properties(handlerId,name,value) VALUES('com.markrai.ignite','igniteMark','$IGNITE_REG_MARK');"
fi
exit 0
"""
    else:
        body = """#!/bin/sh
APPREG=$2
"$IGNITE_SQLITE3" "$APPREG" "INSERT OR IGNORE INTO handlerIds(handlerId) VALUES('com.markrai.ignite');"
"$IGNITE_SQLITE3" "$APPREG" "DELETE FROM properties WHERE handlerId='com.markrai.ignite' AND name='command';"
"$IGNITE_SQLITE3" "$APPREG" "INSERT INTO properties(handlerId,name,value) VALUES('com.markrai.ignite','command','/broken');"
exit 1
"""
    path.write_text(body, encoding="utf-8", newline="\n")


def test_packaged_host_enables_sync_like_legacy_product_host() -> None:
    """Single open-source distribution: packaged host must include sync paths."""
    dev = _HERE.parent.parent / "tools" / "kindle_app" / "bin" / "scribe-lifecycle-host.sh"
    text = dev.read_text(encoding="utf-8")
    assert "--sync-dir" in text
    assert "--device-dir" in text
    ext = (_HERE / "scribe-lifecycle-host.sh").read_text(encoding="utf-8")
    active = "\n".join(
        line for line in ext.splitlines() if line.strip() and not line.lstrip().startswith("#")
    )
    assert "--sync-dir" in active
    assert "/mnt/us/scribe-app/state/sync" in active
    assert "--device-dir" in active
    assert "/mnt/us/scribe-app/state/device" in active
    assert "--confirm-write" in active
    assert "--service com.markrai.ignite" in active
    assert "su_missing" in ext
    assert "--service com.markrai.ignite" in "\n".join(
        line for line in text.splitlines() if line.strip() and not line.lstrip().startswith("#")
    )


def test_real_helpers_never_restore_whole_appreg() -> None:
    install = (_HERE / "lib" / "install-runtime.sh").read_text(encoding="utf-8")
    helpers = (_HERE / "lib" / "register-helpers.sh").read_text(encoding="utf-8")
    assert "rollback_runtime || true" not in install
    assert "ls -1t" not in install
    assert 'cp -f "$APPREG_ROLLBACK" "$APPREG"' not in install
    assert "IGNITE_FAIL_AT" in install
    assert "ignite-activate.journal" in install
    assert "rollback incomplete" in install
    assert "IGNITE_FAIL_RESTORE_CP" in install
    assert "previous-release.staging" in install
    assert "ignite-activate.committed" in install
    assert "had_previous" in install
    assert "phase=pending" in install
    assert "IGNITE_FAIL_RESTORE_SQL" in helpers
    assert ".bail on" in helpers
    assert "BEGIN IMMEDIATE" in helpers
    assert "busy_timeout" in helpers
    assert "IGNITE_APPREG_SNAP" in helpers
    assert "Never copies appreg.db" in helpers or "Never copies appreg.db" in helpers.replace("\n", " ")
    combined = install + "\n" + helpers
    assert 'cp "$BACKUP" "$APPREG"' not in combined
    assert 'cp -f "$BACKUP" "$APPREG"' not in combined


def test_failed_register_restores_only_ignite_handler(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    marker = kindle / "scribe-data" / "n" / "page.meta"
    marker.write_text("keep\n", encoding="utf-8")
    appreg = kindle / "var" / "local" / "appreg.db"
    _seed_appreg(appreg, ignite_command="/old/host")
    stub_ok = tmp_path / "ok-register.sh"
    _write_register_stub(stub_ok, succeed=True)
    first = _run_install(payload, kindle, _register_env(tmp_path, kindle, {
        "IGNITE_REGISTER_STUB": _to_posix_path(stub_ok),
    }))
    assert first.returncode == 0, first.stdout + first.stderr
    old_app = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    (payload / "bin" / "scribe-app").write_bytes(old_app + b"NEW")

    decoy = kindle / "scribe-app" / "appreg-backups" / "appreg.db.bak.ZZZ"
    decoy.parent.mkdir(parents=True, exist_ok=True)
    decoy.write_bytes(b"NOT-A-DATABASE-SHOULD-NEVER-BE-COPIED")
    os.utime(decoy, (2_000_000_000, 2_000_000_000))

    stub_fail = tmp_path / "fail-register.sh"
    _write_register_stub(stub_fail, succeed=False)
    failed = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {"IGNITE_REGISTER_STUB": _to_posix_path(stub_fail)}),
    )
    assert failed.returncode != 0
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == old_app
    assert marker.read_text(encoding="utf-8") == "keep\n"
    assert appreg.read_bytes()[:15] != b"NOT-A-DATABASE"
    assert _appreg_query(appreg, "SELECT x FROM unrelated_lab126") == "keep-system"
    assert _appreg_query(appreg, "SELECT value FROM properties WHERE handlerId='system.other'") == "/usr/bin/other"
    ignite_cmd = _appreg_query(
        appreg,
        "SELECT value FROM properties WHERE handlerId='com.markrai.ignite' AND name='command'",
    )
    expect = _to_posix_path(kindle / "scribe-app" / "bin" / "scribe-lifecycle-host.sh")
    assert ignite_cmd == expect
    snaps = list((kindle / "scribe-app" / "appreg-backups").glob("handler-com.markrai.ignite.*.sql"))
    assert snaps
    assert any("IGNITE_APPREG_SNAP v1" in p.read_text(encoding="utf-8") for p in snaps)


def test_appreg_restore_does_not_expand_shell_in_values(tmp_path: Path) -> None:
    kindle = tmp_path / "kindle"
    appreg = kindle / "var" / "local" / "appreg.db"
    _seed_appreg(appreg, ignite_command=None)
    con = sqlite3.connect(appreg)
    con.execute("INSERT INTO handlerIds(handlerId) VALUES ('com.markrai.ignite')")
    con.execute(
        "INSERT INTO properties(handlerId,name,value) VALUES ('com.markrai.ignite','command',?)",
        ("/bin/ignite$'HOME'`uname`",),
    )
    con.commit()
    con.close()
    sh = _require_sh()
    env = _posix_env(sh)
    env.update(
        {
            "IGNITE_APPREG": _to_posix_path(appreg),
            "IGNITE_APP_ROOT": _to_posix_path(kindle / "scribe-app"),
            "IGNITE_SQLITE3": _make_sqlite3_wrapper(tmp_path / "sqlite3-bin"),
            "IGNITE_SKIP_APPREG_BACKUP": "1",
        }
    )
    script = tmp_path / "restore-literal.sh"
    helpers = _to_posix_path(_HERE / "lib" / "register-helpers.sh")
    script.write_text(
        "#!/bin/sh\nset -eu\n"
        f'. "{helpers}"\n'
        'SNAP=$(ignite_snapshot_handler txn-lit)\n'
        "$IGNITE_SQLITE3 \"$IGNITE_APPREG\" \"UPDATE properties SET value='/broken' WHERE handlerId='com.markrai.ignite';\"\n"
        'ignite_restore_handler "$SNAP" txn-lit\n',
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
    got = _appreg_query(
        appreg,
        "SELECT value FROM properties WHERE handlerId='com.markrai.ignite' AND name='command'",
    )
    assert got == "/bin/ignite$'HOME'`uname`"
    assert _appreg_query(appreg, "SELECT x FROM unrelated_lab126") == "keep-system"


def test_appreg_restore_rejects_wrong_txn(tmp_path: Path) -> None:
    kindle = tmp_path / "kindle"
    appreg = kindle / "var" / "local" / "appreg.db"
    _seed_appreg(appreg)
    sh = _require_sh()
    env = _posix_env(sh)
    sqlite = _make_sqlite3_wrapper(tmp_path / "sqlite3-bin")
    env.update(
        {
            "IGNITE_APPREG": _to_posix_path(appreg),
            "IGNITE_APP_ROOT": _to_posix_path(kindle / "scribe-app"),
            "IGNITE_SQLITE3": sqlite,
            "IGNITE_SKIP_APPREG_BACKUP": "1",
        }
    )
    script = tmp_path / "restore-wrong.sh"
    helpers = _to_posix_path(_HERE / "lib" / "register-helpers.sh")
    script.write_text(
        "#!/bin/sh\nset -eu\n"
        f'. "{helpers}"\n'
        'SNAP=$(ignite_snapshot_handler txn-good)\n'
        "ignite_restore_handler \"$SNAP\" txn-other\n",
        encoding="utf-8",
        newline="\n",
    )
    ran = subprocess.run(
        [sh, _to_posix_path(script)],
        capture_output=True,
        text=True,
        env=env,
    )
    assert ran.returncode != 0
    assert "snapshot txn" in (ran.stdout + ran.stderr)
    assert _appreg_query(appreg, "SELECT x FROM unrelated_lab126") == "keep-system"


@pytest.mark.parametrize("boundary", ALL_ACTIVATION_BOUNDARIES)
def test_activation_fault_restores_previous_release(tmp_path: Path, boundary: str) -> None:
    payload = make_payload(tmp_path / "src")
    (payload / "assets" / "licenses" / "noto-emoji" / "DESCRIPTION.en_us.html").write_text(
        "<html>ofl</html>\n", encoding="utf-8", newline="\n"
    )
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    marker = kindle / "scribe-data" / "n" / "page.meta"
    marker.write_text("keep\n", encoding="utf-8")
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    old_app = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    old_host = (kindle / "scribe-app" / "bin" / "scribe-lifecycle-host.sh").read_bytes()
    (payload / "bin" / "scribe-app").write_bytes(old_app + b"NEW")
    failed = _run_install(payload, kindle, {"IGNITE_FAIL_AT": boundary})
    assert failed.returncode == 1, failed.stdout + failed.stderr
    assert f"injected activation fault at {boundary}" in (failed.stdout + failed.stderr)
    assert "rollback incomplete" not in (failed.stdout + failed.stderr)
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == old_app
    assert (kindle / "scribe-app" / "bin" / "scribe-lifecycle-host.sh").read_bytes() == old_host
    assert marker.read_text(encoding="utf-8") == "keep\n"
    assert not (kindle / "scribe-app" / "run" / "ignite-activate.journal").exists()
    assert not (kindle / "scribe-app" / "bin" / "scribe-app.new").exists()


def test_first_install_activation_fault_removes_incomplete(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    failed = _run_install(payload, kindle, {"IGNITE_FAIL_AT": "host"})
    assert failed.returncode == 1
    assert not (kindle / "scribe-app" / "bin" / "scribe-app").exists()
    assert not (kindle / "scribe-app" / "bin" / "scribe-lifecycle-host.sh").exists()
    assert (kindle / "scribe-data" / "n" / "page.meta").read_text(encoding="utf-8") == "keep\n"


def test_rollback_failure_is_surfaced(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    old_app = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    (payload / "bin" / "scribe-app").write_bytes(old_app + b"NEW")
    failed = _run_install(
        payload,
        kindle,
        {"IGNITE_FAIL_AT": "scriptlet", "IGNITE_FAIL_ROLLBACK": "1"},
    )
    assert failed.returncode == 2
    assert "rollback incomplete" in (failed.stdout + failed.stderr)
    assert (kindle / "scribe-app" / "run" / "ignite-activate.journal").is_file()
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == old_app + b"NEW"


def test_journal_recover_after_partial_rollback_failure(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    old_app = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    (payload / "bin" / "scribe-app").write_bytes(old_app + b"NEW")
    broken = _run_install(
        payload,
        kindle,
        {"IGNITE_FAIL_AT": "host", "IGNITE_FAIL_ROLLBACK": "1"},
    )
    assert broken.returncode == 2
    recovered = _run_install(payload, kindle)
    assert recovered.returncode == 0, recovered.stdout + recovered.stderr
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == old_app + b"NEW"
    assert not (kindle / "scribe-app" / "run" / "ignite-activate.journal").exists()
    assert (kindle / "scribe-data" / "n" / "page.meta").read_text(encoding="utf-8") == "keep\n"


def test_restore_cp_failure_reports_incomplete_and_keeps_journal(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    old_app = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    prev = (kindle / "scribe-app" / "previous-release" / "scribe-app").read_bytes()
    (payload / "bin" / "scribe-app").write_bytes(old_app + b"NEW")
    failed = _run_install(
        payload,
        kindle,
        {"IGNITE_FAIL_AT": "host", "IGNITE_FAIL_RESTORE_CP": "scribe-app"},
    )
    assert failed.returncode == 2
    assert "rollback incomplete" in (failed.stdout + failed.stderr)
    journal = kindle / "scribe-app" / "run" / "ignite-activate.journal"
    assert journal.is_file()
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == old_app + b"NEW"
    assert (kindle / "scribe-app" / "previous-release" / "scribe-app").read_bytes() == prev


def test_restore_sql_failure_reports_incomplete_and_keeps_journal(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    appreg = kindle / "var" / "local" / "appreg.db"
    _seed_appreg(appreg, ignite_command="/old/host")
    stub = tmp_path / "ok-register.sh"
    _write_register_stub(stub, succeed=True)
    first = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {"IGNITE_REGISTER_STUB": _to_posix_path(stub)}),
    )
    assert first.returncode == 0, first.stdout + first.stderr
    old_app = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    (payload / "bin" / "scribe-app").write_bytes(old_app + b"NEW")
    failed = _run_install(
        payload,
        kindle,
        _register_env(
            tmp_path,
            kindle,
            {
                "IGNITE_REGISTER_STUB": _to_posix_path(stub),
                "IGNITE_FORCE_VERIFY_FAIL": "1",
                "IGNITE_FAIL_RESTORE_SQL": "1",
            },
        ),
    )
    assert failed.returncode == 2
    assert "rollback incomplete" in (failed.stdout + failed.stderr)
    assert (kindle / "scribe-app" / "run" / "ignite-activate.journal").is_file()
    recovered = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {"IGNITE_REGISTER_STUB": _to_posix_path(stub)}),
    )
    assert recovered.returncode == 0, recovered.stdout + recovered.stderr
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == old_app + b"NEW"
    assert not (kindle / "scribe-app" / "run" / "ignite-activate.journal").exists()
    assert _appreg_query(appreg, "SELECT x FROM unrelated_lab126") == "keep-system"


def test_abrupt_termination_journal_recovers_without_fail_and_rollback(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    old_app = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    live = kindle / "scribe-app" / "bin" / "scribe-app"
    live.write_bytes(old_app + b"CRASH")
    journal = kindle / "scribe-app" / "run" / "ignite-activate.journal"
    journal.parent.mkdir(parents=True, exist_ok=True)
    journal.write_text(
        "# ignite-activate v1\n# txn=abrupt-test\nscribe-app\n",
        encoding="utf-8",
        newline="\n",
    )
    (payload / "bin" / "scribe-app").write_bytes(old_app + b"NEW")
    recovered = _run_install(payload, kindle)
    assert recovered.returncode == 0, recovered.stdout + recovered.stderr
    assert live.read_bytes() == old_app + b"NEW"
    assert not journal.exists()
    assert (kindle / "scribe-data" / "n" / "page.meta").read_text(encoding="utf-8") == "keep\n"


def test_partial_previous_release_staging_does_not_replace_recovery_base(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    old_app = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    staging = kindle / "scribe-app" / "previous-release.staging"
    staging.mkdir(parents=True)
    (staging / "scribe-app").write_bytes(b"PARTIAL")
    prev_app = kindle / "scribe-app" / "previous-release" / "scribe-app"
    assert prev_app.read_bytes() == old_app
    live = kindle / "scribe-app" / "bin" / "scribe-app"
    live.write_bytes(old_app + b"CRASH")
    journal = kindle / "scribe-app" / "run" / "ignite-activate.journal"
    journal.write_text("# ignite-activate v1\nscribe-app\n", encoding="utf-8", newline="\n")
    recovered = _run_install(payload, kindle)
    assert recovered.returncode == 0, recovered.stdout + recovered.stderr
    assert not staging.exists()
    assert prev_app.read_bytes() == (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()


def test_interrupted_prev_publish_reuses_prev_dir(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    old_app = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    prev = kindle / "scribe-app" / "previous-release"
    prev_old = kindle / "scribe-app" / "previous-release.prev"
    prev.rename(prev_old)
    live = kindle / "scribe-app" / "bin" / "scribe-app"
    live.write_bytes(old_app + b"CRASH")
    journal = kindle / "scribe-app" / "run" / "ignite-activate.journal"
    journal.parent.mkdir(parents=True, exist_ok=True)
    journal.write_text("# ignite-activate v1\nscribe-app\n", encoding="utf-8", newline="\n")
    recovered = _run_install(payload, kindle)
    assert recovered.returncode == 0, recovered.stdout + recovered.stderr
    assert (kindle / "scribe-app" / "previous-release" / "scribe-app").is_file()
    assert not prev_old.exists()


def test_handler_snapshot_sql_failure_does_not_publish(tmp_path: Path) -> None:
    kindle = tmp_path / "kindle"
    appreg = kindle / "var" / "local" / "appreg.db"
    _seed_appreg(appreg)
    sh = _require_sh()
    env = _posix_env(sh)
    env.update(
        {
            "IGNITE_APPREG": _to_posix_path(appreg),
            "IGNITE_APP_ROOT": _to_posix_path(kindle / "scribe-app"),
            "IGNITE_SQLITE3": _make_sqlite3_wrapper(tmp_path / "sqlite3-bin"),
            "IGNITE_SKIP_APPREG_BACKUP": "1",
            "IGNITE_FAIL_SNAPSHOT_SQL": "1",
        }
    )
    helpers = _to_posix_path(_HERE / "lib" / "register-helpers.sh")
    script = tmp_path / "snap-fail.sh"
    script.write_text(
        "#!/bin/sh\nset -eu\n"
        f'. "{helpers}"\n'
        "ignite_snapshot_handler txn-fail\n",
        encoding="utf-8",
        newline="\n",
    )
    ran = subprocess.run(
        [sh, _to_posix_path(script)],
        capture_output=True,
        text=True,
        env=env,
    )
    assert ran.returncode != 0
    backups = kindle / "scribe-app" / "appreg-backups"
    snaps = list(backups.glob("handler-com.markrai.ignite.txn-fail.sql*")) if backups.exists() else []
    assert not any(p.suffix == ".sql" and p.is_file() for p in snaps)


PUBLISH_PENDING = (
    "before-prev-move",
    "after-prev-move",
    "before-staging-publish",
    "after-staging-publish",
    "before-commit",
)
PUBLISH_COMMITTED = (
    "after-commit",
    "before-journal-cleanup",
    "before-prev-cleanup",
)


def _mark(kindle: Path) -> str:
    appreg = kindle / "var" / "local" / "appreg.db"
    return _appreg_query(
        appreg,
        "SELECT value FROM properties WHERE handlerId='com.markrai.ignite' AND name='igniteMark'",
    )


def _data_ok(kindle: Path) -> None:
    assert (kindle / "scribe-data" / "n" / "page.meta").read_text(encoding="utf-8") == "keep\n"


def _unrelated_ok(kindle: Path) -> None:
    appreg = kindle / "var" / "local" / "appreg.db"
    assert _appreg_query(appreg, "SELECT x FROM unrelated_lab126") == "keep-system"
    assert _appreg_query(appreg, "SELECT value FROM properties WHERE handlerId='system.other'") == "/usr/bin/other"


@pytest.mark.parametrize("point", PUBLISH_PENDING + PUBLISH_COMMITTED)
def test_publish_crash_boundaries_keep_one_complete_version(tmp_path: Path, point: str) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    appreg = kindle / "var" / "local" / "appreg.db"
    _seed_appreg(appreg, ignite_command="/old/host")
    stub = tmp_path / "ok-register.sh"
    _write_register_stub(stub, succeed=True)
    bytes_a = (payload / "bin" / "scribe-app").read_bytes()
    first = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {
            "IGNITE_REGISTER_STUB": _to_posix_path(stub),
            "IGNITE_REG_MARK": "A",
        }),
    )
    assert first.returncode == 0, first.stdout + first.stderr
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == bytes_a
    assert _mark(kindle) == "A"
    bytes_b = bytes_a + b"VERSION-B"
    (payload / "bin" / "scribe-app").write_bytes(bytes_b)
    crashed = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {
            "IGNITE_REGISTER_STUB": _to_posix_path(stub),
            "IGNITE_REG_MARK": "B",
            "IGNITE_FAIL_PUBLISH": point,
        }),
    )
    assert crashed.returncode == 99, crashed.stdout + crashed.stderr
    assert "install_crash: injected at " + point in (crashed.stdout + crashed.stderr)
    _data_ok(kindle)
    _unrelated_ok(kindle)
    journal = kindle / "scribe-app" / "run" / "ignite-activate.journal"
    committed = kindle / "scribe-app" / "run" / "ignite-activate.committed"
    if point in PUBLISH_PENDING:
        assert not committed.exists()
        assert journal.is_file()
    else:
        assert committed.is_file()

    probe = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {
            "IGNITE_REGISTER_STUB": _to_posix_path(stub),
            "IGNITE_REG_MARK": "B",
            "IGNITE_FAIL_AT": "scribe-app",
        }),
    )
    assert probe.returncode == 1, probe.stdout + probe.stderr
    assert "rollback incomplete" not in (probe.stdout + probe.stderr)
    live = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    if point in PUBLISH_PENDING:
        assert live == bytes_a
        assert _mark(kindle) == "A"
    else:
        assert live == bytes_b
        assert _mark(kindle) == "B"
    _data_ok(kindle)
    _unrelated_ok(kindle)

    finished = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {
            "IGNITE_REGISTER_STUB": _to_posix_path(stub),
            "IGNITE_REG_MARK": "B",
        }),
    )
    assert finished.returncode == 0, finished.stdout + finished.stderr
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == bytes_b
    assert _mark(kindle) == "B"
    assert not journal.exists()
    assert not committed.exists()
    assert not (kindle / "scribe-app" / "previous-release.prev").exists()
    _data_ok(kindle)
    _unrelated_ok(kindle)
    again = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {
            "IGNITE_REGISTER_STUB": _to_posix_path(stub),
            "IGNITE_REG_MARK": "B",
        }),
    )
    assert again.returncode == 0, again.stdout + again.stderr
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == bytes_b
    assert _mark(kindle) == "B"


def test_recovery_failure_stays_pending_and_retryable(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    appreg = kindle / "var" / "local" / "appreg.db"
    _seed_appreg(appreg, ignite_command="/old/host")
    stub = tmp_path / "ok-register.sh"
    _write_register_stub(stub, succeed=True)
    bytes_a = (payload / "bin" / "scribe-app").read_bytes()
    assert _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {
            "IGNITE_REGISTER_STUB": _to_posix_path(stub),
            "IGNITE_REG_MARK": "A",
        }),
    ).returncode == 0
    (payload / "bin" / "scribe-app").write_bytes(bytes_a + b"VERSION-B")
    crashed = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {
            "IGNITE_REGISTER_STUB": _to_posix_path(stub),
            "IGNITE_REG_MARK": "B",
            "IGNITE_FAIL_PUBLISH": "after-staging-publish",
        }),
    )
    assert crashed.returncode == 99
    failed = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {
            "IGNITE_REGISTER_STUB": _to_posix_path(stub),
            "IGNITE_REG_MARK": "B",
            "IGNITE_FAIL_RECOVERY": "1",
        }),
    )
    assert failed.returncode != 0
    assert "install_ok" not in (failed.stdout + failed.stderr)
    assert (kindle / "scribe-app" / "run" / "ignite-activate.journal").is_file()
    assert (kindle / "scribe-app" / "previous-release.prev" / "scribe-app").is_file()
    _data_ok(kindle)
    finished = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {
            "IGNITE_REGISTER_STUB": _to_posix_path(stub),
            "IGNITE_REG_MARK": "B",
        }),
    )
    assert finished.returncode == 0, finished.stdout + finished.stderr
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == bytes_a + b"VERSION-B"
    assert _mark(kindle) == "B"
    _data_ok(kindle)
    _unrelated_ok(kindle)


def test_first_install_pending_publish_crash_discards_incomplete(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    appreg = kindle / "var" / "local" / "appreg.db"
    _seed_appreg(appreg, ignite_command=None)
    stub = tmp_path / "ok-register.sh"
    _write_register_stub(stub, succeed=True)
    crashed = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {
            "IGNITE_REGISTER_STUB": _to_posix_path(stub),
            "IGNITE_REG_MARK": "A",
            "IGNITE_FAIL_PUBLISH": "before-commit",
        }),
    )
    assert crashed.returncode == 99
    probe = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {
            "IGNITE_REGISTER_STUB": _to_posix_path(stub),
            "IGNITE_REG_MARK": "A",
            "IGNITE_FAIL_AT": "scribe-app",
        }),
    )
    assert probe.returncode == 1
    assert not (kindle / "scribe-app" / "bin" / "scribe-app").exists()
    _data_ok(kindle)
    finished = _run_install(
        payload,
        kindle,
        _register_env(tmp_path, kindle, {
            "IGNITE_REGISTER_STUB": _to_posix_path(stub),
            "IGNITE_REG_MARK": "A",
        }),
    )
    assert finished.returncode == 0, finished.stdout + finished.stderr
    assert (kindle / "scribe-app" / "bin" / "scribe-app").is_file()
    assert _mark(kindle) == "A"
    _data_ok(kindle)
    _unrelated_ok(kindle)


def _arm32le_elf_stub(size: int = 2048) -> bytes:
    ident = b"\x7fELF" + bytes([1, 1, 1, 0]) + bytes(8)
    hdr = ident + bytes([2, 0, 0x28, 0])
    return hdr + b"\0" * (size - len(hdr))


def test_scenario_a_clean_compatible_device(tmp_path: Path) -> None:
    """No prior runtime, registration, or notebook library."""
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    result = _run_install(payload, kindle)
    assert result.returncode == 0, result.stdout + result.stderr
    host = (kindle / "scribe-app" / "bin" / "scribe-lifecycle-host.sh").read_text(encoding="utf-8")
    active = "\n".join(
        line for line in host.splitlines() if line.strip() and not line.lstrip().startswith("#")
    )
    assert "--confirm-write" in active
    assert "--sync-dir" in active
    assert "--device-dir" in active
    assert "/mnt/us/scribe-app/state/sync" in active
    assert "/mnt/us/scribe-app/state/device" in active
    assert (kindle / "scribe-app" / "bin" / "scribe-app").is_file()
    assert (kindle / "documents" / "Scribe.sh").is_file()
    assert not (kindle / "scribe-data").exists()


def test_scenario_b_upgrade_preserves_notebooks(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    note = kindle / "scribe-data" / "notebooks" / "notebook-1" / "page-1" / "strokes.jsonl"
    note.parent.mkdir(parents=True)
    note.write_text("stroke-keep\n", encoding="utf-8")
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    (payload / "bin" / "scribe-app").write_bytes(
        (payload / "bin" / "scribe-app").read_bytes() + b"UPGRADED"
    )
    second = _run_install(payload, kindle)
    assert second.returncode == 0, second.stdout + second.stderr
    assert note.read_text(encoding="utf-8") == "stroke-keep\n"
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes().endswith(b"UPGRADED")
    assert (kindle / "scribe-app" / "previous-release" / "scribe-app").is_file()


def test_scenario_c_wrong_elf_leaves_runtime_and_data(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    marker = kindle / "scribe-data" / "n" / "page.meta"
    marker.parent.mkdir(parents=True)
    marker.write_text("keep\n", encoding="utf-8")
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    old = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    (payload / "bin" / "scribe-app").write_bytes(b"\x7fELF" + b"\0" * 64)
    failed = _run_install(payload, kindle, {"IGNITE_SKIP_ELF_CHECK": "0"})
    assert failed.returncode != 0
    assert "unsupported:" in (failed.stdout + failed.stderr)
    assert "install_ok" not in (failed.stdout + failed.stderr)
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == old
    assert marker.read_text(encoding="utf-8") == "keep\n"


def test_scenario_c_insufficient_space_does_not_install(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    marker = kindle / "scribe-data" / "n" / "page.meta"
    marker.parent.mkdir(parents=True)
    marker.write_text("keep\n", encoding="utf-8")
    failed = _run_install(
        payload,
        kindle,
        {"IGNITE_SKIP_SPACE_CHECK": "0", "IGNITE_AVAIL_KB": "16"},
    )
    assert failed.returncode != 0
    assert "storage:" in (failed.stdout + failed.stderr)
    assert "install_ok" not in (failed.stdout + failed.stderr)
    assert not (kindle / "scribe-app" / "bin" / "scribe-app").exists()
    assert marker.read_text(encoding="utf-8") == "keep\n"


def test_arm32le_elf_stub_is_accepted(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    stub = _arm32le_elf_stub()
    (payload / "bin" / "scribe-app").write_bytes(stub)
    (payload / "bin" / "scribe-lifecycle").write_bytes(stub)
    # Recovery ELF check runs when IGNITE_SKIP_ELF_CHECK=0; keep stub + digest aligned.
    rec = payload / "recovery" / "bin" / "dropbearmulti"
    rec.write_bytes(stub)
    import hashlib

    (payload / "recovery" / "manifest" / "elf.sha256").write_text(
        hashlib.sha256(stub).hexdigest() + "\n", encoding="utf-8"
    )
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    result = _run_install(payload, kindle, {"IGNITE_SKIP_ELF_CHECK": "0"})
    assert result.returncode == 0, result.stdout + result.stderr
