"""Real sqlite3 CLI fail-fast tests. Do not substitute the Python shim."""

from __future__ import annotations

import shutil
import sqlite3
import subprocess
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

from test_install_runtime import (  # noqa: E402
    _appreg_query,
    _run_install,
    _seed_appreg,
    _write_register_stub,
)
from test_sfx import _to_posix_path  # noqa: E402
from test_validate_stage import make_payload  # noqa: E402

_GIT_SQLITE = (
    Path(r"C:\Program Files\Git\usr\bin\sqlite3.exe"),
    Path(r"C:\Program Files\Git\mingw64\bin\sqlite3.exe"),
    Path(r"C:\Program Files (x86)\Git\usr\bin\sqlite3.exe"),
)


def _real_sqlite3() -> str:
    found: list[str] = []
    which = shutil.which("sqlite3")
    if which:
        found.append(which)
    for candidate in _GIT_SQLITE:
        if candidate.is_file():
            found.append(str(candidate))
    for path in found:
        resolved = Path(path).resolve()
        if resolved.name.lower().startswith("ignite-sqlite3"):
            continue
        if "test_sqlite3_shim" in resolved.name.lower():
            continue
        probed = subprocess.run(
            [path, "-version"],
            capture_output=True,
            text=True,
        )
        if probed.returncode != 0:
            continue
        version = (probed.stdout or probed.stderr).strip()
        if not version:
            continue
        bail = subprocess.run(
            [path, ":memory:"],
            input=".bail on\nSELECT ignite_bail_probe();\nSELECT 'BAIL_NOT_HONORED';\n",
            capture_output=True,
            text=True,
        )
        if "BAIL_NOT_HONORED" in (bail.stdout or ""):
            continue
        if bail.returncode == 0:
            continue
        return path
    pytest.fail(
        "real sqlite3 CLI is required for this regression test and was not found "
        "(Git usr/bin sqlite3.exe or PATH sqlite3 with working .bail on)"
    )


def test_real_sqlite3_bail_rolls_back_before_commit(tmp_path: Path) -> None:
    sqlite = _real_sqlite3()
    db = tmp_path / "appreg.db"
    _seed_appreg(db, ignite_command="/old/host")
    con = sqlite3.connect(db)
    con.execute(
        "INSERT INTO properties(handlerId,name,value) VALUES ('com.markrai.ignite','igniteMark','A')"
    )
    con.commit()
    con.close()
    script = tmp_path / "txn.sql"
    script.write_text(
        ".bail on\n"
        "PRAGMA busy_timeout=8000;\n"
        "BEGIN IMMEDIATE;\n"
        "DELETE FROM properties WHERE handlerId='com.markrai.ignite' AND name='command';\n"
        "INSERT INTO properties(handlerId,name,value) "
        "VALUES ('com.markrai.ignite','command','/broken');\n"
        "SELECT ignite_injected_restore_failure();\n"
        "COMMIT;\n",
        encoding="utf-8",
        newline="\n",
    )
    ran = subprocess.run(
        [sqlite, str(db)],
        input=script.read_text(encoding="utf-8"),
        capture_output=True,
        text=True,
    )
    assert ran.returncode != 0
    assert "BAIL_NOT_HONORED" not in (ran.stdout + ran.stderr)
    assert _appreg_query(db, "SELECT value FROM properties WHERE handlerId='com.markrai.ignite' AND name='command'") == "/old/host"
    assert _appreg_query(db, "SELECT value FROM properties WHERE handlerId='com.markrai.ignite' AND name='igniteMark'") == "A"
    assert _appreg_query(db, "SELECT x FROM unrelated_lab126") == "keep-system"
    assert _appreg_query(db, "SELECT value FROM properties WHERE handlerId='system.other'") == "/usr/bin/other"


def test_real_sqlite3_restore_helper_fail_fast_and_keeps_journal(tmp_path: Path) -> None:
    sqlite = _real_sqlite3()
    payload = make_payload(tmp_path / "src")
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "scribe-data" / "n").mkdir(parents=True)
    (kindle / "scribe-data" / "n" / "page.meta").write_text("keep\n", encoding="utf-8")
    appreg = kindle / "var" / "local" / "appreg.db"
    _seed_appreg(appreg, ignite_command="/old/host")
    stub = tmp_path / "ok-register.sh"
    _write_register_stub(stub, succeed=True)
    posix_sqlite = _to_posix_path(Path(sqlite))
    extra = {
        "IGNITE_SKIP_REGISTER": "0",
        "IGNITE_SKIP_APPREG_BACKUP": "0",
        "IGNITE_SQLITE3": posix_sqlite,
        "IGNITE_APPREG": _to_posix_path(appreg),
        "IGNITE_REGISTER_STUB": _to_posix_path(stub),
        "IGNITE_REG_MARK": "A",
    }
    first = _run_install(payload, kindle, extra)
    assert first.returncode == 0, first.stdout + first.stderr
    old_app = (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes()
    (payload / "bin" / "scribe-app").write_bytes(old_app + b"NEW")
    extra_fail = dict(extra)
    extra_fail.update({"IGNITE_REG_MARK": "B", "IGNITE_FORCE_VERIFY_FAIL": "1", "IGNITE_FAIL_RESTORE_SQL": "1"})
    failed = _run_install(payload, kindle, extra_fail)
    assert failed.returncode == 2
    assert "rollback incomplete" in (failed.stdout + failed.stderr)
    assert "install_ok" not in (failed.stdout + failed.stderr)
    assert (kindle / "scribe-app" / "run" / "ignite-activate.journal").is_file()
    assert _appreg_query(appreg, "SELECT x FROM unrelated_lab126") == "keep-system"
    assert _appreg_query(appreg, "SELECT value FROM properties WHERE handlerId='system.other'") == "/usr/bin/other"
    extra_ok = dict(extra)
    extra_ok["IGNITE_REG_MARK"] = "B"
    recovered = _run_install(payload, kindle, extra_ok)
    assert recovered.returncode == 0, recovered.stdout + recovered.stderr
    assert (kindle / "scribe-app" / "bin" / "scribe-app").read_bytes() == old_app + b"NEW"
    assert not (kindle / "scribe-app" / "run" / "ignite-activate.journal").exists()
    assert (kindle / "scribe-data" / "n" / "page.meta").read_text(encoding="utf-8") == "keep\n"


def test_helpers_source_contains_bail_on() -> None:
    text = (_HERE / "lib" / "register-helpers.sh").read_text(encoding="utf-8")
    assert ".bail on" in text
    assert "ignite_sql_script" in text
    assert "ignite_sqlite_require_bail" in text
