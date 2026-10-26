"""Mock-tree tests for install-runtime rollback and Diagnose (no personal Kindle)."""

from __future__ import annotations

import hashlib
import os
import shutil
import sqlite3
import stat
import subprocess
import sys
import time
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


def _write_passthrough_lock_helper(kindle: Path) -> Path:
    """Host-only stand-in; kernel lock behavior has separate Unix regression coverage."""
    helper = kindle / ".test-bin" / "updater-lock-helper.sh"
    helper.parent.mkdir(parents=True, exist_ok=True)
    helper.write_text(
        "#!/bin/sh\n"
        'if test "$1" = "--verify-updater-transaction-lock"; then exit 0; fi\n'
        'test "$1" = "--with-updater-transaction-lock" || exit 64\n'
        "shift\n"
        'test "$1" = "--" || exit 64\n'
        "shift\n"
        "IGNITE_UPDATER_LOCK_HELD=1\n"
        "IGNITE_UPDATER_LOCK_FD=0\n"
        "export IGNITE_UPDATER_LOCK_HELD IGNITE_UPDATER_LOCK_FD\n"
        'exec "$@"\n',
        encoding="utf-8",
        newline="\n",
    )
    helper.chmod(helper.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return helper


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
            "IGNITE_UPDATER_LOCK_HELPER": _to_posix_path(
                _write_passthrough_lock_helper(kindle)
            ),
        }
    )
    if extra:
        env.update(extra)
    return env


def _run_install(
    payload: Path,
    kindle: Path,
    extra: dict[str, str] | None = None,
    pass_fds: tuple[int, ...] = (),
) -> subprocess.CompletedProcess[str]:
    sh = _require_sh()
    script = _HERE / "lib" / "install-runtime.sh"
    options: dict[str, object] = {}
    if pass_fds:
        options["pass_fds"] = pass_fds
    return subprocess.run(  # type: ignore[arg-type]
        [sh, _to_posix_path(script), _to_posix_path(payload)],
        capture_output=True,
        text=True,
        env=_env_for(kindle, extra),
        **options,
    )


@pytest.fixture(scope="session")
def compiled_rust_lock_helper(tmp_path_factory: pytest.TempPathFactory) -> Path:
    if not sys.platform.startswith("linux"):
        pytest.skip("compiled updater-lock integration requires Linux")
    cargo = shutil.which("cargo")
    assert cargo is not None, "Linux updater-lock integration requires cargo"
    target_dir = tmp_path_factory.mktemp("rust-updater-lock-target")
    built = subprocess.run(
        [cargo, "build", "-p", "scribe-lifecycle", "--target-dir", str(target_dir)],
        cwd=_HERE.parents[1],
        capture_output=True,
        text=True,
    )
    assert built.returncode == 0, built.stdout + built.stderr
    helper = target_dir / "debug" / "scribe-lifecycle"
    assert helper.is_file()
    return helper


def _start_rust_lock_holder(helper: Path, lock_path: Path, ready: Path) -> subprocess.Popen[str]:
    env = os.environ.copy()
    env.update(
        {
            "IGNITE_TEST_UPDATER_LOCK_PATH": str(lock_path),
            "IGNITE_TEST_LOCK_READY": str(ready),
        }
    )
    code = (
        "import os,time; "
        "open(os.environ['IGNITE_TEST_LOCK_READY'],'w',encoding='ascii').write('ready\\n'); "
        "time.sleep(60)"
    )
    return subprocess.Popen(
        [
            str(helper),
            "--with-updater-transaction-lock",
            "--",
            sys.executable,
            "-c",
            code,
        ],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        text=True,
        env=env,
    )


def _run_uninstall_runtime(
    kindle: Path,
    extra: dict[str, str],
    pass_fds: tuple[int, ...] = (),
) -> subprocess.CompletedProcess[str]:
    sh = _require_sh()
    options: dict[str, object] = {}
    if pass_fds:
        options["pass_fds"] = pass_fds
    return subprocess.run(  # type: ignore[arg-type]
        [sh, _to_posix_path(_HERE / "lib" / "uninstall-runtime.sh")],
        capture_output=True,
        text=True,
        env=_env_for(kindle, extra),
        **options,
    )


def _wait_for_file(path: Path, process: subprocess.Popen[str]) -> None:
    deadline = time.monotonic() + 5
    while not path.exists():
        assert process.poll() is None, "lock holder exited before readiness"
        assert time.monotonic() < deadline, "lock holder readiness timed out"
        time.sleep(0.01)


def _pending_path(kindle: Path) -> Path:
    return kindle / "scribe-app" / "state" / "update" / "pending-health.v1"


def _inventory_path(kindle: Path) -> Path:
    return kindle / "scribe-app" / "previous-release" / "ignite-lkg-inventory.v1"


def _read_record(path: Path) -> dict[str, str]:
    lines = path.read_text(encoding="utf-8").splitlines()
    return dict(line.split("=", 1) for line in lines[1:] if "=" in line)


def _path_with_command_stub(tmp_path: Path, command: str, script: str) -> str:
    test_bin = tmp_path / f"fail-{command}"
    test_bin.mkdir(exist_ok=True)
    stub = test_bin / command
    stub.write_text(f"#!/bin/sh\n{script}\n", encoding="utf-8", newline="\n")
    stub.chmod(stub.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    sh_dir = str(Path(_require_sh()).resolve().parent)
    return os.pathsep.join((str(test_bin), sh_dir, os.environ.get("PATH", "")))


def _seed_persistent_state(kindle: Path) -> dict[Path, bytes]:
    files = {
        kindle / "scribe-data" / "notebooks" / "n" / "page" / "strokes.jsonl": b"stroke\x00bytes\n",
        kindle / "scribe-app" / "state" / "sync" / "outbox.jsonl": b'{"sync":"keep"}\n',
        kindle / "scribe-app" / "state" / "device" / "identity.json": b'{"device":"keep"}\n',
        kindle / "scribe-app" / "state" / "settings.json": b'{"settings":"keep"}\n',
    }
    for path, content in files.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
    return files


def _assert_persistent_state(files: dict[Path, bytes]) -> None:
    for path, content in files.items():
        assert path.read_bytes() == content


def _install_pending_candidate(tmp_path: Path) -> tuple[Path, Path]:
    payload_a = make_payload(tmp_path / "src-a", "7.8.0")
    payload_b = make_payload(tmp_path / "src-b", "7.8.1")
    app_b = payload_b / "bin" / "scribe-app"
    app_b.write_bytes(app_b.read_bytes() + b"candidate-B")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    installed = _run_install(payload_a, kindle)
    assert installed.returncode == 0, installed.stdout + installed.stderr
    upgraded = _run_install(payload_b, kindle)
    assert upgraded.returncode == 0, upgraded.stdout + upgraded.stderr
    return payload_b, kindle


def _seed_legacy_previous_release(kindle: Path) -> Path:
    app = kindle / "scribe-app"
    previous = app / "previous-release"
    previous.mkdir(parents=True, exist_ok=True)
    for source, name in (
        (app / "bin" / "scribe-app", "scribe-app"),
        (app / "bin" / "scribe-lifecycle", "scribe-lifecycle"),
        (app / "bin" / "scribe-lifecycle-host.sh", "scribe-lifecycle-host.sh"),
    ):
        shutil.copyfile(source, previous / name)
    return previous


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
    assert _pending_path(kindle).is_file()
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
    assert "IGNITE_PENDING_HEALTH_V1" in install
    assert "IGNITE_LKG_INVENTORY_V1" in install
    assert "data_compatibility=unknown" in install
    assert "update blocked:" in install
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


def test_lkg_inventory_retains_pre_upgrade_ignite_handler_only(tmp_path: Path) -> None:
    payload_a = make_payload(tmp_path / "src-a", "3.0.0")
    payload_b = make_payload(tmp_path / "src-b", "3.0.1")
    (payload_b / "bin" / "scribe-app").write_bytes(
        (payload_b / "bin" / "scribe-app").read_bytes() + b"B"
    )
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    appreg = kindle / "var" / "local" / "appreg.db"
    _seed_appreg(appreg, ignite_command="/old/host")
    stub = tmp_path / "ok-register.sh"
    _write_register_stub(stub, succeed=True)
    env_a = _register_env(
        tmp_path,
        kindle,
        {"IGNITE_REGISTER_STUB": _to_posix_path(stub), "IGNITE_REG_MARK": "A"},
    )
    env_b = _register_env(
        tmp_path,
        kindle,
        {"IGNITE_REGISTER_STUB": _to_posix_path(stub), "IGNITE_REG_MARK": "B"},
    )
    assert _run_install(payload_a, kindle, env_a).returncode == 0
    upgraded = _run_install(payload_b, kindle, env_b)
    assert upgraded.returncode == 0, upgraded.stdout + upgraded.stderr

    handler = kindle / "scribe-app" / "previous-release" / "appreg-handler.sql"
    text = handler.read_text(encoding="utf-8")
    assert "IGNITE_APPREG_SNAP v1" in text
    assert "com.markrai.ignite" in text
    assert "igniteMark" in text
    assert "'A'" in text
    assert "system.other" not in text
    digest = hashlib.sha256(handler.read_bytes()).hexdigest()
    assert f"entry=appreg-handler.sql|file|{handler.stat().st_size}|{digest}\n" in _inventory_path(
        kindle
    ).read_text(encoding="utf-8")


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
    assert (kindle / "scribe-app" / "previous-release" / "scribe-app").read_bytes() == old_app


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
    _seed_legacy_previous_release(kindle)
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
    prev = _seed_legacy_previous_release(kindle)
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


SNAPSHOT_PRE_JOURNAL = (
    "before-prev-move",
    "after-prev-move",
    "before-staging-publish",
    "after-staging-publish",
)
PUBLISH_PENDING = (
    "after-static-verification",
    "before-lkg-inventory-publish",
    "after-lkg-inventory-publish",
    "before-pending-health-publish",
    "after-pending-health-publish",
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


@pytest.mark.parametrize("point", SNAPSHOT_PRE_JOURNAL + PUBLISH_PENDING + PUBLISH_COMMITTED)
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
    if point in SNAPSHOT_PRE_JOURNAL:
        assert not committed.exists()
        assert not journal.exists()
    elif point in PUBLISH_PENDING:
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
    if point in SNAPSHOT_PRE_JOURNAL + PUBLISH_PENDING:
        assert live == bytes_a
        assert _mark(kindle) == "A"
    else:
        assert live == bytes_b
        assert _mark(kindle) == "B"
    _data_ok(kindle)
    _unrelated_ok(kindle)

    if point in SNAPSHOT_PRE_JOURNAL + PUBLISH_PENDING:
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
    assert _pending_path(kindle).is_file()
    assert (kindle / "scribe-app" / "previous-release" / "scribe-app").read_bytes() == bytes_a
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
    assert again.returncode != 0, again.stdout + again.stderr
    assert "update blocked" in (again.stdout + again.stderr)
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
            "IGNITE_FAIL_PUBLISH": "after-pending-health-publish",
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
    assert (kindle / "scribe-app" / "previous-release" / "scribe-app").read_bytes() == bytes_a
    assert _pending_path(kindle).is_file()
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
    assert _pending_path(kindle).is_file()
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
    assert not (kindle / "scribe-app" / "previous-release").exists()
    assert not _pending_path(kindle).exists()


def test_scenario_b_upgrade_preserves_notebooks(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    note = kindle / "scribe-data" / "notebooks" / "notebook-1" / "page-1" / "strokes.jsonl"
    note.parent.mkdir(parents=True)
    note.write_text("stroke-keep\n", encoding="utf-8")
    first = _run_install(payload, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    bytes_a = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    (payload / "bin" / "scribe-app").write_bytes(
        (payload / "bin" / "scribe-app").read_bytes() + b"UPGRADED"
    )
    second = _run_install(payload, kindle)
    assert second.returncode == 0, second.stdout + second.stderr
    assert note.read_text(encoding="utf-8") == "stroke-keep\n"
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes().endswith(b"UPGRADED")
    assert (kindle / "scribe-app" / "previous-release" / "scribe-app").read_bytes() == bytes_a
    assert _pending_path(kindle).is_file()
    assert _inventory_path(kindle).is_file()


def test_upgrade_retains_a_and_publishes_strict_pending_health(tmp_path: Path) -> None:
    payload_a = make_payload(tmp_path / "src-a", "1.2.3")
    payload_b = make_payload(tmp_path / "src-b", "1.2.4")
    (payload_b / "bin" / "scribe-app").write_bytes(
        (payload_b / "bin" / "scribe-app").read_bytes() + b"candidate-B"
    )
    (payload_b / "bin" / "scribe-lifecycle").write_bytes(
        (payload_b / "bin" / "scribe-lifecycle").read_bytes() + b"candidate-B"
    )
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    persistent = _seed_persistent_state(kindle)

    installed_a = _run_install(payload_a, kindle)
    assert installed_a.returncode == 0, installed_a.stdout + installed_a.stderr
    app_a = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    life_a = (kindle / "scribe-app" / "bin" / "scribe-lifecycle").read_bytes()
    upgraded = _run_install(payload_b, kindle)
    assert upgraded.returncode == 0, upgraded.stdout + upgraded.stderr

    live_app = kindle / "scribe-app" / "bin" / "scribe-app"
    live_life = kindle / "scribe-app" / "bin" / "scribe-lifecycle"
    retained = kindle / "scribe-app" / "previous-release"
    assert live_app.read_bytes().endswith(b"candidate-B")
    assert live_life.read_bytes().endswith(b"candidate-B")
    assert (retained / "scribe-app").read_bytes() == app_a
    assert (retained / "scribe-lifecycle").read_bytes() == life_a
    for required in (
        "scribe-lifecycle-host.sh",
        "Scribe.sh",
        "scribe-icon.png",
        "install-runtime.sh",
        "uninstall-runtime.sh",
        "backup-runtime.sh",
        "register-helpers.sh",
        "install-recovery.sh",
        "Uninstall Ignite.sh",
        "Backup Ignite Data.sh",
        "Diagnose Ignite.sh",
    ):
        assert (retained / required).is_file(), required

    pending = _pending_path(kindle)
    inventory = _inventory_path(kindle)
    assert pending.read_bytes().endswith(b"\n")
    assert len(pending.read_bytes()) <= 4096
    assert len(pending.read_text(encoding="utf-8").splitlines()) == 14
    record = _read_record(pending)
    assert record["schema"] == "1"
    assert record["status"] == "pending-health"
    assert record["candidate_version"] == "1.2.4"
    assert record["retained_version"] == "unknown"
    assert record["data_compatibility"] == "unknown"
    assert record["candidate_app_sha256"] == hashlib.sha256(live_app.read_bytes()).hexdigest()
    assert record["candidate_lifecycle_sha256"] == hashlib.sha256(live_life.read_bytes()).hexdigest()
    inventory_bytes = inventory.read_bytes()
    inventory_sha = hashlib.sha256(inventory_bytes).hexdigest()
    assert record["lkg_inventory_bytes"] == str(len(inventory_bytes))
    assert record["lkg_inventory_sha256"] == inventory_sha
    assert record["retained_release_identity"] == f"sha256:{inventory_sha}"
    inventory_text = inventory.read_text(encoding="utf-8")
    assert inventory_text.startswith(
        "IGNITE_LKG_INVENTORY_V1\nschema=1\nkind=ignite-application-release\nentry_count=18\n"
    )
    assert "entry=appreg-handler.sql|absent|0|-\n" in inventory_text
    _assert_persistent_state(persistent)


def test_legacy_previous_release_is_completed_before_upgrade(tmp_path: Path) -> None:
    payload_a = make_payload(tmp_path / "src-a", "1.3.0")
    payload_b = make_payload(tmp_path / "src-b", "1.3.1")
    (payload_b / "bin" / "scribe-app").write_bytes(
        (payload_b / "bin" / "scribe-app").read_bytes() + b"candidate-B"
    )
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    assert _run_install(payload_a, kindle).returncode == 0
    legacy = _seed_legacy_previous_release(kindle)
    live_helper = kindle / "scribe-app" / "tools" / "ignite-release" / "lib" / "install-recovery.sh"
    helper_a = live_helper.read_bytes()
    assert not (legacy / "install-recovery.sh").exists()

    upgraded = _run_install(payload_b, kindle)
    assert upgraded.returncode == 0, upgraded.stdout + upgraded.stderr
    assert (legacy / "install-recovery.sh").read_bytes() == helper_a
    helper_digest = hashlib.sha256(helper_a).hexdigest()
    assert (
        f"entry=install-recovery.sh|file|{len(helper_a)}|{helper_digest}\n"
        in _inventory_path(kindle).read_text(encoding="utf-8")
    )


def test_mismatched_existing_recovery_base_fails_before_activation(tmp_path: Path) -> None:
    payload_a = make_payload(tmp_path / "src-a", "1.4.0")
    payload_b = make_payload(tmp_path / "src-b", "1.4.1")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    assert _run_install(payload_a, kindle).returncode == 0
    live_a = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    legacy = _seed_legacy_previous_release(kindle)
    (legacy / "scribe-app").write_bytes(b"different-release")

    refused = _run_install(payload_b, kindle)
    assert refused.returncode != 0
    assert "does not exactly match live release A" in (refused.stdout + refused.stderr)
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == live_a
    assert (legacy / "scribe-app").read_bytes() == b"different-release"
    assert not _pending_path(kindle).exists()


def test_pending_health_blocks_reinstall_and_stacked_upgrade(tmp_path: Path) -> None:
    payload_a = make_payload(tmp_path / "src-a", "2.0.0")
    payload_b = make_payload(tmp_path / "src-b", "2.0.1")
    payload_c = make_payload(tmp_path / "src-c", "2.0.2")
    (payload_b / "bin" / "scribe-app").write_bytes(b"B" + (payload_b / "bin" / "scribe-app").read_bytes())
    (payload_c / "bin" / "scribe-app").write_bytes(b"C" + (payload_c / "bin" / "scribe-app").read_bytes())
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    live_b = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    retained_a = (kindle / "scribe-app" / "previous-release" / "scribe-app").read_bytes()
    pending = _pending_path(kindle).read_bytes()
    inventory = _inventory_path(kindle).read_bytes()
    pending_fields = _read_record(_pending_path(kindle))
    evidence_path = kindle / "scribe-app" / "state" / "update" / "launch-health-evidence.v1"
    evidence = (
        "IGNITE_LAUNCH_HEALTH_EVIDENCE_V1\n"
        "schema=1\n"
        f"candidate_version={pending_fields['candidate_version']}\n"
        f"candidate_txn={pending_fields['candidate_txn']}\n"
        f"pending_sha256={hashlib.sha256(pending).hexdigest()}\n"
        f"candidate_app_bytes={pending_fields['candidate_app_bytes']}\n"
        f"candidate_app_sha256={pending_fields['candidate_app_sha256']}\n"
        f"candidate_lifecycle_bytes={pending_fields['candidate_lifecycle_bytes']}\n"
        f"candidate_lifecycle_sha256={pending_fields['candidate_lifecycle_sha256']}\n"
        "launch_uuid=00112233445566778899aabbccddeeff\n"
        "outcome=ready_running\n"
        "ready=true\n"
        "readiness=ready\n"
        "process=running\n"
        "observation_complete=false\n"
        "ready_elapsed_ms=250\n"
        "post_ready_observed_ms=60000\n"
        "qualification=unqualified\n"
        "data_compatibility=unknown\n"
    ).encode("ascii")
    evidence_path.write_bytes(evidence)

    for attempted in (payload_b, payload_c):
        refused = _run_install(attempted, kindle)
        assert refused.returncode != 0
        assert "update blocked" in (refused.stdout + refused.stderr)
        assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == live_b
        assert (kindle / "scribe-app" / "previous-release" / "scribe-app").read_bytes() == retained_a
        assert _pending_path(kindle).read_bytes() == pending
        assert _inventory_path(kindle).read_bytes() == inventory
        assert evidence_path.read_bytes() == evidence


def test_pending_health_record_damage_fails_closed(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    assert _run_install(payload, kindle).returncode == 0
    (payload / "bin" / "scribe-app").write_bytes(
        (payload / "bin" / "scribe-app").read_bytes() + b"B"
    )
    assert _run_install(payload, kindle).returncode == 0
    pending = _pending_path(kindle)
    retained = (kindle / "scribe-app" / "previous-release" / "scribe-app").read_bytes()
    original = pending.read_text(encoding="utf-8")

    for damaged in (
        original.replace("schema=1", "schema=2"),
        original.replace("status=pending-health", "status=ready"),
        original.replace("candidate_app_sha256=", "candidate_app_sha256=xyz", 1),
        original + "unexpected=field\n",
    ):
        pending.write_text(damaged, encoding="utf-8", newline="\n")
        refused = _run_install(payload, kindle)
        assert refused.returncode != 0
        assert "malformed, damaged, or inconsistent" in (refused.stdout + refused.stderr)
        assert pending.read_text(encoding="utf-8") == damaged
        assert (kindle / "scribe-app" / "previous-release" / "scribe-app").read_bytes() == retained
    pending.write_text(original, encoding="utf-8", newline="\n")

    live = kindle / "scribe-app" / "bin" / "scribe-app"
    live_bytes = live.read_bytes()
    live.write_bytes(live_bytes + b"stale-or-modified")
    refused = _run_install(payload, kindle)
    assert refused.returncode != 0
    assert "installed candidate application does not match record" in (refused.stdout + refused.stderr)
    live.write_bytes(live_bytes)

    pending.unlink()
    refused = _run_install(payload, kindle)
    assert refused.returncode != 0
    assert "pending health state is missing" in (refused.stdout + refused.stderr)
    assert _inventory_path(kindle).is_file()


def test_pending_health_record_requires_exact_canonical_bytes(tmp_path: Path) -> None:
    payload, kindle = _install_pending_candidate(tmp_path)
    pending = _pending_path(kindle)
    original = pending.read_bytes()
    retained = (kindle / "scribe-app" / "previous-release" / "scribe-app").read_bytes()

    valid = _run_install(payload, kindle)
    assert valid.returncode != 0
    assert "update blocked" in (valid.stdout + valid.stderr)

    damaged_records = (
        original + b"unterminated-trailing-data",
        original.replace(b"schema=1", b"schema=1\x00", 1),
        original + b"unexpected=field\n",
        original[:-1],
        original.replace(b"\n", b"\r\n"),
    )
    for damaged in damaged_records:
        pending.write_bytes(damaged)
        refused = _run_install(payload, kindle)
        assert refused.returncode != 0
        assert "malformed, damaged, or inconsistent" in (refused.stdout + refused.stderr)
        assert pending.read_bytes() == damaged
        assert (kindle / "scribe-app" / "previous-release" / "scribe-app").read_bytes() == retained
    pending.write_bytes(original)


def test_pending_health_rejects_noncanonical_versions(tmp_path: Path) -> None:
    payload, kindle = _install_pending_candidate(tmp_path)
    pending = _pending_path(kindle)
    original = pending.read_bytes()
    version_line = next(line for line in original.splitlines(keepends=True) if line.startswith(b"candidate_version="))

    invalid_versions = (
        b"01.2.3",
        b"1.02.3",
        b"1.2.03",
        b"1.2.3-01",
        b"1.2.3-alpha..1",
        b"1.2.3-.rc",
        b"1.2.3-rc.",
        b"1.2.3-alpha_1",
        b"1.2.3+build.1",
        b"1.2.3-rc+build",
        b"1.2.3-rc/1",
        b"1.2",
        b"1.2.3-",
        b"1.2.3-" + b"a" * 59,
    )
    for version in invalid_versions:
        damaged = original.replace(version_line, b"candidate_version=" + version + b"\n", 1)
        pending.write_bytes(damaged)
        refused = _run_install(payload, kindle)
        assert refused.returncode != 0
        assert "candidate version malformed or noncanonical" in (refused.stdout + refused.stderr)
        assert pending.read_bytes() == damaged
    pending.write_bytes(original)


def test_integrity_command_failures_and_invalid_outputs_fail_closed(tmp_path: Path) -> None:
    payload, kindle = _install_pending_candidate(tmp_path)
    pending = _pending_path(kindle)
    original_pending = pending.read_bytes()
    inventory = _inventory_path(kindle)
    original_inventory = inventory.read_bytes()
    cases = (
        ("sha256sum", "exit 23"),
        ("sha256sum", "printf '%s\\n' not-a-digest; exit 0"),
        ("sha256sum", f"printf '%s\\n' {'0' * 64}; exit 0"),
        ("wc", "exit 23"),
        ("wc", "printf '%s\\n' not-a-count; exit 0"),
        ("wc", "printf '%s\\n' '1 2'; exit 0"),
    )
    for command, script in cases:
        path = _path_with_command_stub(tmp_path, command, script)
        refused = _run_install(payload, kindle, {"PATH": path})
        assert refused.returncode != 0
        assert "malformed, damaged, or inconsistent" in (refused.stdout + refused.stderr)
        assert pending.read_bytes() == original_pending
        assert inventory.read_bytes() == original_inventory


def test_v2_committed_marker_requires_exact_canonical_bytes(tmp_path: Path) -> None:
    payload_a = make_payload(tmp_path / "src-a", "8.0.0")
    payload_b = make_payload(tmp_path / "src-b", "8.0.1")
    app_b = payload_b / "bin" / "scribe-app"
    app_b.write_bytes(app_b.read_bytes() + b"candidate-B")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    assert _run_install(payload_a, kindle).returncode == 0
    crashed = _run_install(payload_b, kindle, {"IGNITE_FAIL_PUBLISH": "after-commit"})
    assert crashed.returncode == 99

    committed = kindle / "scribe-app" / "run" / "ignite-activate.committed"
    original = committed.read_bytes()
    malformed_markers = (
        original + b"unterminated-trailing-data",
        original.replace(b" pending_health=1", b"\x00 pending_health=1", 1),
        original + b"unexpected=field\n",
        original[:-1],
        original.replace(b"\n", b"\r\n"),
        original.replace(b"pending_health=1", b"pending_health=01", 1),
        original.replace(b"txn=", b"txn=not-a-transaction-", 1),
    )
    for malformed in malformed_markers:
        committed.write_bytes(malformed)
        refused = _run_install(payload_b, kindle)
        assert refused.returncode != 0
        assert "install_ok" not in (refused.stdout + refused.stderr)
        assert committed.read_bytes() == malformed

    committed.write_bytes(original)
    valid = _run_install(payload_b, kindle)
    assert valid.returncode != 0
    assert "update blocked" in (valid.stdout + valid.stderr)
    assert not committed.exists()


def test_legacy_committed_marker_recovery_remains_supported(tmp_path: Path) -> None:
    payload_a = make_payload(tmp_path / "src-a", "8.1.0")
    payload_b = make_payload(tmp_path / "src-b", "8.1.1")
    app_b = payload_b / "bin" / "scribe-app"
    app_b.write_bytes(app_b.read_bytes() + b"candidate-B")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    assert _run_install(payload_a, kindle).returncode == 0
    committed = kindle / "scribe-app" / "run" / "ignite-activate.committed"
    committed.write_bytes(b"committed txn=legacy-v1\n")

    upgraded = _run_install(payload_b, kindle)
    assert upgraded.returncode == 0, upgraded.stdout + upgraded.stderr
    assert not committed.exists()
    assert _pending_path(kindle).is_file()


def test_lkg_inventory_damage_and_unexpected_paths_fail_closed(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    assert _run_install(payload, kindle).returncode == 0
    (payload / "bin" / "scribe-app").write_bytes(
        (payload / "bin" / "scribe-app").read_bytes() + b"B"
    )
    assert _run_install(payload, kindle).returncode == 0
    retained = kindle / "scribe-app" / "previous-release"
    app = retained / "scribe-app"
    app_bytes = app.read_bytes()

    app.write_bytes(app_bytes + b"modified")
    refused = _run_install(payload, kindle)
    assert refused.returncode != 0
    assert "lkg_invalid" in (refused.stdout + refused.stderr)
    app.write_bytes(app_bytes)

    life = retained / "scribe-lifecycle"
    life_bytes = life.read_bytes()
    life.unlink()
    refused = _run_install(payload, kindle)
    assert refused.returncode != 0
    assert "lkg_invalid" in (refused.stdout + refused.stderr)
    life.write_bytes(life_bytes)

    unexpected = retained / "unexpected-file"
    unexpected.write_bytes(b"not release inventory")
    refused = _run_install(payload, kindle)
    assert refused.returncode != 0
    assert "unexpected retained path" in (refused.stdout + refused.stderr)
    unexpected.unlink()

    inventory = _inventory_path(kindle)
    inventory_bytes = inventory.read_bytes()
    inventory.write_bytes(inventory_bytes.replace(b"entry_count=18", b"entry_count=17"))
    refused = _run_install(payload, kindle)
    assert refused.returncode != 0
    assert "inventory digest mismatch" in (refused.stdout + refused.stderr)
    inventory.write_bytes(inventory_bytes)

    scriptlet = retained / "Scribe.sh"
    scriptlet_bytes = scriptlet.read_bytes()
    scriptlet.unlink()
    try:
        os.symlink(app, scriptlet)
    except OSError:
        scriptlet.write_bytes(scriptlet_bytes)
        return
    refused = _run_install(payload, kindle)
    assert refused.returncode != 0
    assert "retained path Scribe.sh is a symlink" in (refused.stdout + refused.stderr)
    assert scriptlet.is_symlink()


@pytest.mark.parametrize(
    "version",
    (
        "",
        "v1.2.3",
        "1.2.3.4",
        "01.2.3",
        "1.02.3",
        "1.2.03",
        "1.2.3-01",
        "1.2.3-.rc",
        "1.2.3-rc.",
        "1.2.3-alpha..1",
        "1.2.3-alpha_1",
        "1.2.3+build.1",
        "1.2.3-rc+build",
        "1.2.3-rc/1",
        "1.2",
        "1.2.3-",
        "1.2.3-" + "a" * 59,
    ),
)
def test_noncanonical_package_version_fails_closed(tmp_path: Path, version: str) -> None:
    payload = make_payload(tmp_path / "src", version)
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)

    refused = _run_install(payload, kindle)
    assert refused.returncode != 0
    output = refused.stdout + refused.stderr
    if version in ("1.2.3-rc.", "1.2.3-rc/1"):
        assert (
            "package directory" in output
            or "package version is malformed or noncanonical" in output
        )
    else:
        assert "package version is malformed or noncanonical" in output
    assert not (kindle / "scribe-app" / "bin" / "scribe-app").exists()


@pytest.mark.parametrize(
    "version", ("0.0.0", "1.2.3", "10.20.30-rc.1", "1.2.3-alpha-beta.0")
)
def test_canonical_package_versions_remain_supported(tmp_path: Path, version: str) -> None:
    payload = make_payload(tmp_path / "src", version)
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)

    installed = _run_install(payload, kindle)
    assert installed.returncode == 0, installed.stdout + installed.stderr


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


def test_missing_transaction_lock_helper_refuses_before_mutation(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)

    refused = _run_install(
        payload,
        kindle,
        {"IGNITE_UPDATER_LOCK_HELPER": _to_posix_path(tmp_path / "missing-helper")},
    )

    assert refused.returncode != 0
    assert "coordination: updater transaction lock helper is unavailable" in refused.stderr
    assert not (kindle / "scribe-app").exists()


def test_transaction_lock_precedes_install_recovery_and_uninstall_mutation() -> None:
    install = (_HERE / "lib" / "install-runtime.sh").read_text(encoding="utf-8")
    uninstall = (_HERE / "lib" / "uninstall-runtime.sh").read_text(encoding="utf-8")

    install_lock = install.index("--with-updater-transaction-lock")
    install_verify = install.index("--verify-updater-transaction-lock")
    assert install_lock < install.index("recover_install_transaction")
    assert install_verify < install.index("recover_install_transaction")
    assert install_lock < install.index('mkdir -p "$APP_ROOT/bin"')
    assert install_lock < install.index("guard_pending_health")

    uninstall_lock = uninstall.index("--with-updater-transaction-lock")
    uninstall_verify = uninstall.index("--verify-updater-transaction-lock")
    assert uninstall_lock < uninstall.index("# Refuse while running")
    assert uninstall_verify < uninstall.index("# Refuse while running")
    assert uninstall_lock < uninstall.index('rm -f "$APP_ROOT/bin/scribe-app"')
    assert uninstall_lock < uninstall.index("ignite_recovery_uninstall")


@pytest.mark.skipif(
    not sys.platform.startswith("linux"), reason="requires Linux flock semantics"
)
def test_compiled_rust_handoff_rejects_forgery_and_releases_after_holder_death(
    tmp_path: Path, compiled_rust_lock_helper: Path
) -> None:
    payload = make_payload(tmp_path / "src")
    packaged_helper = payload / "bin" / "scribe-lifecycle"
    shutil.copyfile(compiled_rust_lock_helper, packaged_helper)
    packaged_helper.chmod(
        packaged_helper.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH
    )
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    lock_path = tmp_path / "updater.lock"
    rust_env = {
        "IGNITE_UPDATER_LOCK_HELPER": str(packaged_helper),
        "IGNITE_TEST_UPDATER_LOCK_PATH": str(lock_path),
    }

    installed = _run_install(payload, kindle, rust_env)
    assert installed.returncode == 0, installed.stdout + installed.stderr
    live = kindle / "scribe-app" / "bin" / "scribe-app"
    installed_helper = kindle / "scribe-app" / "bin" / "scribe-lifecycle"
    uninstall_env = {
        "IGNITE_UPDATER_LOCK_HELPER": str(installed_helper),
        "IGNITE_TEST_UPDATER_LOCK_PATH": str(lock_path),
    }
    live_before = live.read_bytes()
    stale = kindle / "scribe-app" / "bin" / "scribe-app.new"
    stale.write_bytes(b"recovery-must-not-run")

    forged_fd = os.open(lock_path, os.O_RDWR)
    ready = tmp_path / "holder.ready"
    holder = _start_rust_lock_holder(
        compiled_rust_lock_helper,
        lock_path,
        ready,
    )
    try:
        _wait_for_file(ready, holder)
        forged_install_env = {
            **rust_env,
            "IGNITE_UPDATER_LOCK_HELD": "1",
            "IGNITE_UPDATER_LOCK_FD": str(forged_fd),
        }
        forged_install = _run_install(
            payload,
            kindle,
            forged_install_env,
            pass_fds=(forged_fd,),
        )
        assert forged_install.returncode == 75
        assert "already in progress" in forged_install.stderr
        assert stale.read_bytes() == b"recovery-must-not-run"
        assert live.read_bytes() == live_before

        forged_uninstall_env = {
            **uninstall_env,
            "IGNITE_UPDATER_LOCK_HELD": "1",
            "IGNITE_UPDATER_LOCK_FD": str(forged_fd),
        }
        forged_uninstall = _run_uninstall_runtime(
            kindle,
            forged_uninstall_env,
            pass_fds=(forged_fd,),
        )
        assert forged_uninstall.returncode == 75
        assert "already in progress" in forged_uninstall.stderr
        assert stale.read_bytes() == b"recovery-must-not-run"
        assert live.read_bytes() == live_before
    finally:
        os.close(forged_fd)
        holder.kill()
        holder.wait(timeout=5)

    recovered = _run_install(
        payload,
        kindle,
        rust_env,
    )
    assert recovered.returncode == 0, recovered.stdout + recovered.stderr
    assert not stale.exists()

    uninstalled = _run_uninstall_runtime(kindle, uninstall_env)
    assert uninstalled.returncode == 0, uninstalled.stdout + uninstalled.stderr
    assert not live.exists()


@pytest.mark.skipif(
    not sys.platform.startswith("linux"), reason="requires Linux flock semantics"
)
def test_compiled_rust_lock_contention_preserves_pending_b_and_retained_a(
    tmp_path: Path, compiled_rust_lock_helper: Path
) -> None:
    payload_b, kindle = _install_pending_candidate(tmp_path)
    app = kindle / "scribe-app"
    protected_roots = [
        app / "bin",
        app / "previous-release",
        app / "state" / "update",
        app / "run",
    ]

    def snapshot() -> dict[str, bytes]:
        return {
            str(path.relative_to(app)): path.read_bytes()
            for root in protected_roots
            if root.exists()
            for path in root.rglob("*")
            if path.is_file()
        }

    before = snapshot()
    lock_path = tmp_path / "updater.lock"
    rust_env = {
        "IGNITE_UPDATER_LOCK_HELPER": str(compiled_rust_lock_helper),
        "IGNITE_TEST_UPDATER_LOCK_PATH": str(lock_path),
    }
    ready = tmp_path / "holder.ready"
    holder = _start_rust_lock_holder(
        compiled_rust_lock_helper,
        lock_path,
        ready,
    )
    try:
        _wait_for_file(ready, holder)
        install = _run_install(payload_b, kindle, rust_env)
        assert install.returncode == 75
        assert "already in progress" in install.stderr
        uninstall = _run_uninstall_runtime(kindle, rust_env)
        assert uninstall.returncode == 75
        assert "already in progress" in uninstall.stderr
        assert snapshot() == before
    finally:
        holder.kill()
        holder.wait(timeout=5)
