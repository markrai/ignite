"""Self-extracting Install Ignite.sh generation and integrity tests."""

from __future__ import annotations

import hashlib
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_SFX = _HERE / "sfx"
for extra in (_HERE, _SFX):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from build_sfx import (  # noqa: E402
    archive_md5,
    build_payload_tgz,
    build_sfx,
    default_template,
    extract_payload,
    split_sfx,
)
from test_validate_stage import FIXTURE_VERSION, make_payload, write_sfx  # noqa: E402
from validate_stage import validate_sfx  # noqa: E402

_GIT_USR = Path(r"C:\Program Files\Git\usr\bin")
_GIT_USR_X86 = Path(r"C:\Program Files (x86)\Git\usr\bin")


def _posix_sh() -> str | None:
    for candidate in (
        shutil.which("sh"),
        shutil.which("bash"),
        str(_GIT_USR / "sh.exe") if (_GIT_USR / "sh.exe").is_file() else None,
        str(_GIT_USR_X86 / "sh.exe") if (_GIT_USR_X86 / "sh.exe").is_file() else None,
    ):
        if candidate:
            return candidate
    return None


def _posix_env(sh: str) -> dict[str, str]:
    env = os.environ.copy()
    sh_dir = str(Path(sh).resolve().parent)
    env["PATH"] = sh_dir + os.pathsep + env.get("PATH", "")
    env["LC_ALL"] = "C"
    return env


def _to_posix_path(path: Path) -> str:
    s = str(path.resolve()).replace("\\", "/")
    if len(s) >= 2 and s[1] == ":":
        return f"/{s[0].lower()}{s[2:]}"
    return s


def _require_sh() -> str:
    sh = _posix_sh()
    if not sh:
        raise AssertionError(
            "POSIX sh not found (Git usr/bin sh.exe or PATH sh). "
            "Installer arithmetic must be executed, not skipped."
        )
    return sh


def test_generate_sfx(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    sfx = write_sfx(payload, tmp_path / "Install Ignite.sh")
    assert sfx.is_file()
    assert sfx.stat().st_size > 2048
    assert validate_sfx(sfx, FIXTURE_VERSION) == []
    blob = sfx.read_bytes()
    assert blob.startswith(b"#!/bin/sh\n")
    assert b"# Name: Install Ignite\n" in blob[:400]
    assert b"\r\n" not in blob[:4096]
    gzip_at = blob.find(b"\x1f\x8b")
    assert gzip_at > 0
    prefix = blob[:gzip_at]
    assert b"not enough free space" in prefix
    assert b"download is incomplete" in prefix


def test_extract_embedded_archive_byte_for_byte(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    expected = {}
    for path in payload.rglob("*"):
        if path.is_file():
            expected[path.relative_to(payload).as_posix()] = path.read_bytes()
    archive = build_payload_tgz(payload, FIXTURE_VERSION)
    sfx = build_sfx(payload, FIXTURE_VERSION, default_template())
    prefix, embedded, meta = split_sfx(sfx)
    assert embedded == archive
    assert hashlib.sha256(embedded).hexdigest() == meta["sha256"]
    extracted = extract_payload(embedded, tmp_path / "out")
    got = {
        p.relative_to(extracted).as_posix(): p.read_bytes()
        for p in extracted.rglob("*")
        if p.is_file()
    }
    assert got == expected
    assert "Install-Ignite.sh" not in got
    assert prefix.endswith(b"\n")


def test_embedded_integrity_matches_stamps(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    sfx = write_sfx(payload, tmp_path / "Install Ignite.sh")
    _prefix, archive, meta = split_sfx(sfx.read_bytes())
    assert hashlib.sha256(archive).hexdigest() == meta["sha256"]
    assert archive_md5(archive) == meta["md5"]
    assert archive[:2] == b"\x1f\x8b"
    assert meta["version"] == FIXTURE_VERSION


def test_truncation_is_rejected(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    blob = write_sfx(payload, tmp_path / "good.sh").read_bytes()
    truncated = tmp_path / "truncated.sh"
    truncated.write_bytes(blob[:-64])
    errors = validate_sfx(truncated, FIXTURE_VERSION)
    assert errors
    assert any("truncated" in e.lower() or "split failed" in e.lower() for e in errors)


def test_corruption_is_rejected(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    blob = bytearray(write_sfx(payload, tmp_path / "good.sh").read_bytes())
    blob[-20] ^= 0xFF
    damaged = tmp_path / "damaged.sh"
    damaged.write_bytes(blob)
    errors = validate_sfx(damaged, FIXTURE_VERSION)
    assert errors
    assert any("mismatch" in e.lower() or "split failed" in e.lower() for e in errors)


def test_required_runtime_files_present(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    (payload / "lib" / "install-runtime.sh").unlink()
    errors = validate_sfx(write_sfx(payload, tmp_path / "Install Ignite.sh"), FIXTURE_VERSION)
    assert any("install-runtime.sh" in e for e in errors)


def test_full_feature_host_policy(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    host = payload / "host" / "scribe-lifecycle-host.sh"
    host.write_text(
        "#!/bin/sh\nexec scribe-app --confirm-write --data-dir /mnt/us/scribe-data\n",
        encoding="utf-8",
        newline="\n",
    )
    errors = validate_sfx(write_sfx(payload, tmp_path / "Install Ignite.sh"), FIXTURE_VERSION)
    assert any("missing --sync-dir" in e or "missing --device-dir" in e for e in errors)


def test_no_user_data_or_state(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    _write = payload.joinpath
    (_write("replica.json")).write_text("{}\n", encoding="utf-8")
    errors = validate_sfx(write_sfx(payload, tmp_path / "Install Ignite.sh"), FIXTURE_VERSION)
    assert any("replica.json" in e for e in errors)


def test_deterministic_generation(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    a = build_sfx(payload, FIXTURE_VERSION, default_template())
    b = build_sfx(payload, FIXTURE_VERSION, default_template())
    assert a == b


def test_prefix_does_not_claim_success_in_conditional_group(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    prefix, _archive, _meta = split_sfx(build_sfx(payload, FIXTURE_VERSION, default_template()))
    text = prefix.decode("utf-8")
    assert '} >>"$LOG" 2>&1 && STATUS=0 || STATUS=$?' not in text
    assert "SUCCESS. Open Ignite from the Library." in text
    success_at = text.index("SUCCESS. Open Ignite from the Library.")
    runtime_at = text.index('sh "$PACKAGE_ROOT/lib/install-runtime.sh"')
    assert runtime_at < success_at
    rm_at = text.index("removed installer from Library")
    assert runtime_at < rm_at
    extract_at = text.index('tar -xzf "$ARCHIVE"')
    diagnose_at = text.index('cp -f "$PACKAGE_ROOT/Diagnose-Ignite.sh"')
    assert extract_at < diagnose_at < runtime_at
    assert "write_usb_fail_report" in text
    assert "Diagnose Ignite is installed only after the payload extracts successfully." in text


def test_set_e_subshell_does_not_print_success_after_false(tmp_path: Path) -> None:
    sh = _require_sh()
    script = tmp_path / "trap.sh"
    script.write_text(
        "#!/bin/sh\n"
        "set +e\n"
        "(\n"
        "  set -e\n"
        "  false\n"
        "  echo SUCCESS\n"
        "  rm -f /tmp/should-not-exist-ignite-test\n"
        ")\n"
        "st=$?\n"
        'echo STATUS=$st\n',
        encoding="utf-8",
        newline="\n",
    )
    ran = subprocess.run(
        [sh, _to_posix_path(script)],
        capture_output=True,
        text=True,
        env=_posix_env(sh),
    )
    assert ran.returncode == 0
    assert "SUCCESS" not in ran.stdout
    assert "STATUS=" in ran.stdout
    assert ran.stdout.strip().splitlines()[-1] != "STATUS=0"


def test_prefix_does_not_arith_padded_fields(tmp_path: Path) -> None:
    """BusyBox ash treats 0nnn as octal; 8/9 in OFFSET/BYTES must not enter $(( ))."""
    payload = make_payload(tmp_path / "src")
    prefix, _archive, meta = split_sfx(build_sfx(payload, FIXTURE_VERSION, default_template()))
    text = prefix.decode("utf-8")
    assert "$((IGNITE_ARCHIVE_OFFSET" not in text
    assert "$((IGNITE_ARCHIVE_BYTES" not in text
    assert "decimal() {" in text
    assert int(meta["offset"]) > 0
    # Real stamps are zero-padded and commonly contain 8/9 (this user's OFFSET was 3954).
    assert meta["offset"].startswith("0") or len(meta["offset"]) == 10


def _decimal_fn_from_prefix(prefix: bytes) -> str:
    text = prefix.decode("utf-8")
    match = re.search(r"^decimal\(\) \{.*?\n\}\n", text, re.M | re.S)
    assert match, "generated prefix is missing decimal()"
    return match.group(0)


def test_decimal_strip_allows_8_and_9(tmp_path: Path) -> None:
    """Reproduce the on-device failure: $((0000003954 + 0003759515)) is invalid octal."""
    payload = make_payload(tmp_path / "src")
    prefix, _archive, _meta = split_sfx(build_sfx(payload, FIXTURE_VERSION, default_template()))
    sh = _require_sh()
    env = _posix_env(sh)
    script = tmp_path / "octal.sh"
    script.write_text(
        "#!/bin/sh\nset -eu\n"
        + _decimal_fn_from_prefix(prefix)
        + "OFFSET_N=$(decimal 0000003954)\n"
        "BYTES_N=$(decimal 0003759515)\n"
        "NEED=$((OFFSET_N + BYTES_N))\n"
        'printf \'%s\\n\' "$NEED"\n',
        encoding="utf-8",
        newline="\n",
    )
    raw = subprocess.run(
        [sh, "-c", "NEED=$((0000003954 + 0003759515)); echo $NEED"],
        capture_output=True,
        text=True,
        env=env,
    )
    assert raw.returncode != 0, "raw padded $(( )) should fail on ash/bash (octal 8/9)"
    combined = (raw.stderr + raw.stdout).lower()
    assert "arithmetic" in combined or "base" in combined or "octal" in combined
    posix_script = _to_posix_path(script)
    fixed = subprocess.run([sh, posix_script], capture_output=True, text=True, env=env)
    assert fixed.returncode == 0, fixed.stderr + fixed.stdout
    assert fixed.stdout.strip() == str(3954 + 3759515)


def test_rehearse_extract_with_posix_sh(tmp_path: Path) -> None:
    """Execute the installer's size math, dd slice, checksum, and tar -tzf on a real SFX."""
    payload = make_payload(tmp_path / "src")
    sfx = write_sfx(payload, tmp_path / "Install Ignite.sh")
    prefix, expected_archive, meta = split_sfx(sfx.read_bytes())
    text = prefix.decode("utf-8")
    assert '[ "$GOT" = "$IGNITE_ARCHIVE_BYTES" ]' not in text
    assert "[ \"$GOT\" -eq \"$BYTES_N\" ]" in text
    assert "OFFSET_N=$(decimal \"$IGNITE_ARCHIVE_OFFSET\")" in text

    sh = _require_sh()
    env = _posix_env(sh)
    for tool in ("dd", "wc", "tr", "tar", "md5sum"):
        which = subprocess.run(
            [sh, "-c", f"command -v {tool}"],
            capture_output=True,
            text=True,
            env=env,
        )
        assert which.returncode == 0, f"missing {tool} on PATH for {sh}: {which.stderr}"

    work = tmp_path / "work"
    work.mkdir()
    script = work / "rehearse.sh"
    posix_sfx = _to_posix_path(sfx)
    posix_work = _to_posix_path(work)
    script.write_text(
        "#!/bin/sh\n"
        "set -eu\n"
        f"IGNITE_ARCHIVE_OFFSET={meta['offset']}\n"
        f"IGNITE_ARCHIVE_BYTES={meta['bytes']}\n"
        f"IGNITE_ARCHIVE_MD5={meta['md5']}\n"
        + _decimal_fn_from_prefix(prefix)
        + f'SELF="{posix_sfx}"\n'
        f'STAGE="{posix_work}/extract"\n'
        "ARCHIVE=\"$STAGE/payload.tgz\"\n"
        "mkdir -p \"$STAGE/root\"\n"
        "OFFSET_N=$(decimal \"$IGNITE_ARCHIVE_OFFSET\")\n"
        "BYTES_N=$(decimal \"$IGNITE_ARCHIVE_BYTES\")\n"
        'TOTAL=$(decimal "$(wc -c < "$SELF" | tr -d \' \')")\n'
        "NEED=$((OFFSET_N + BYTES_N))\n"
        '[ "$TOTAL" -eq "$NEED" ]\n'
        '[ "$TOTAL" -eq "$OFFSET_N" ] && exit 20\n'
        "dd if=\"$SELF\" of=\"$ARCHIVE\" bs=\"$OFFSET_N\" skip=1 2>/dev/null\n"
        'GOT=$(decimal "$(wc -c < "$ARCHIVE" | tr -d \' \')")\n'
        '[ "$GOT" -eq "$BYTES_N" ]\n'
        'SUM=$(md5sum "$ARCHIVE")\n'
        "SUM=${SUM%% *}\n"
        '[ "$SUM" = "$IGNITE_ARCHIVE_MD5" ]\n'
        'tar -tzf "$ARCHIVE" >/dev/null\n'
        'printf "REHEARSE_OK %s %s %s\\n" "$OFFSET_N" "$BYTES_N" "$GOT"\n',
        encoding="utf-8",
        newline="\n",
    )
    ran = subprocess.run(
        [sh, _to_posix_path(script)],
        capture_output=True,
        text=True,
        env=env,
    )
    assert ran.returncode == 0, ran.stderr + ran.stdout
    assert ran.stdout.startswith("REHEARSE_OK ")
    extracted = (work / "extract" / "payload.tgz").read_bytes()
    assert extracted == expected_archive
    parts = ran.stdout.strip().split()
    assert parts[1] == str(int(meta["offset"]))
    assert parts[2] == str(int(meta["bytes"]))
    assert parts[3] == str(len(expected_archive))


def _usb_fail_report_fn_from_prefix(prefix: bytes) -> str:
    text = prefix.decode("utf-8")
    match = re.search(r"^write_usb_fail_report\(\) \{.*?\n\}\n", text, re.M | re.S)
    assert match, "generated prefix is missing write_usb_fail_report()"
    return match.group(0)


def test_early_extract_failure_writes_usb_report_without_diagnose_scriptlet(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    prefix, _archive, _meta = split_sfx(build_sfx(payload, FIXTURE_VERSION, default_template()))
    sh = _require_sh()
    env = _posix_env(sh)
    docs = tmp_path / "documents"
    logs = tmp_path / "logs"
    docs.mkdir()
    logs.mkdir()
    log = logs / "install.log"
    log.write_text("==== Install Ignite\nFAILED: download is incomplete. Copy the file again.\n", encoding="utf-8")
    script = tmp_path / "usb-report.sh"
    script.write_text(
        "#!/bin/sh\nset -eu\n"
        f'DOCS="{_to_posix_path(docs)}"\n'
        f'LOG="{_to_posix_path(log)}"\n'
        "STATUS=1\n"
        "SELF=/mnt/us/documents/Install\\ Ignite.sh\n"
        "IGNITE_PACKAGE_VERSION=0.9.8\n"
        + _usb_fail_report_fn_from_prefix(prefix)
        + "write_usb_fail_report\n",
        encoding="utf-8",
        newline="\n",
    )
    ran = subprocess.run(
        [sh, _to_posix_path(script)],
        capture_output=True,
        text=True,
        env=env,
    )
    assert ran.returncode == 0, ran.stderr + ran.stdout
    report = (docs / "Ignite-Diagnose.txt").read_text(encoding="utf-8")
    assert "install_status=1" in report
    assert "Diagnose Ignite is installed only after the payload extracts successfully." in report
    assert "download is incomplete" in report
    assert not (docs / "Diagnose Ignite.sh").exists()


def test_sfx_prefix_never_parses_gzip(tmp_path: Path) -> None:
    """The shell prefix must `exit` before the gzip bytes so ash never parses them."""
    payload = make_payload(tmp_path / "src")
    prefix, archive, _meta = split_sfx(build_sfx(payload, FIXTURE_VERSION, default_template()))
    text = prefix.decode("utf-8")
    assert "exit \"$STATUS\"" in text
    assert text.rstrip().endswith('exit "$STATUS"') or text.endswith('exit "$STATUS"\n')
    assert archive.startswith(b"\x1f\x8b")
    # gzip must start immediately after the prefix, with no extra ASCII wrapper.
    assert prefix.endswith(b"\n")
    assert b"\x1f\x8b" not in prefix
    prefix_path = tmp_path / "prefix.sh"
    prefix_path.write_bytes(prefix)
    sh = _require_sh()
    checked = subprocess.run(
        [sh, "-n", _to_posix_path(prefix_path)],
        capture_output=True,
        text=True,
        env=_posix_env(sh),
    )
    assert checked.returncode == 0, checked.stderr
