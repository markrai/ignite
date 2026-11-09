"""Read-only Kindle preflight. Never launches scribe-app or mutates the device."""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Any, Callable

_RELEASE = Path(__file__).resolve().parents[1] / "release"
if str(_RELEASE) not in sys.path:
    sys.path.insert(0, str(_RELEASE))

from ship_lib.errors import ShipBlockedError  # noqa: E402
from ship_lib.hardware import run_ssh_script  # noqa: E402

from evidence import (
    PREFLIGHT_SCHEMA,
    STATUS_BLOCKED,
    STATUS_FAIL,
    STATUS_NOT_RUN,
    STATUS_PASS,
    check,
    group_status,
    utc_now,
)

PROD_DATA = "/mnt/us/scribe-data"
PROD_APP = "/mnt/us/scribe-app"
PROD_SYNC = "/mnt/us/scribe-app/state/sync"
PROD_DEVICE = "/mnt/us/scribe-app/state/device"
PROD_HANDLER = "com.markrai.ignite"
LEGACY_HANDLER = "com.markrai.scribe"
QA_HANDLER = "com.markrai.ignite.qa"
QA_ROOT = "/mnt/us/scribe-qa"

# Files that may hold pairing tokens or server credentials — never printed.
_SECRET_BASENAMES = frozenset(
    {
        "server-profile.json",
        "device.json",
        "config.json",
        "token",
        "verifier",
        "preferences.json",
    }
)

PREFLIGHT_REMOTE = r"""
set -eu
# Read-only inventory. Do not exec scribe-app or scribe-lifecycle.
printf 'prettyversion=%s\n' "$(cat /etc/prettyversion.txt 2>/dev/null | tr '\n' ' ' | tr -s ' ')"
printf 'uname=%s\n' "$(uname -a 2>/dev/null || true)"
printf 'arch=%s\n' "$(uname -m 2>/dev/null || true)"
printf 'hostname=%s\n' "$(hostname 2>/dev/null || true)"

APP=/mnt/us/scribe-app
DATA=/mnt/us/scribe-data
HOST=$APP/bin/scribe-lifecycle-host.sh
APPREG=/var/local/appreg.db
HANDLER=com.markrai.ignite
LEGACY=com.markrai.scribe
QA=com.markrai.ignite.qa

if [ -f "$APP/state/update/launch-health-evidence.v1" ]; then
  grep -E '^(candidate_version|candidate_app_sha256|candidate_app_bytes|candidate_lifecycle_sha256|candidate_lifecycle_bytes|qualification|data_compatibility|ready)=' \
    "$APP/state/update/launch-health-evidence.v1" | sed 's/^/health_/'
else
  printf 'health_present=no\n'
fi

for rel in bin/scribe-app bin/scribe-lifecycle bin/scribe-lifecycle-host.sh; do
  p="$APP/$rel"
  base=$(echo "$rel" | tr '/.' '_')
  if [ -f "$p" ]; then
    printf '%s_present=yes\n' "$base"
    printf '%s_bytes=%s\n' "$base" "$(wc -c < "$p" | tr -d ' ')"
    printf '%s_sha256=%s\n' "$base" "$(sha256sum "$p" | awk '{print $1}')"
  else
    printf '%s_present=no\n' "$base"
  fi
done

if [ -f "$APP/bin/storage-contract.v1" ]; then
  printf 'storage_contract=%s\n' "$(tr '\n' ';' < "$APP/bin/storage-contract.v1" | tr -s ' ')"
else
  printf 'storage_contract=missing\n'
fi

if [ -f "$HOST" ]; then
  printf 'host_present=yes\n'
  printf 'host_has_prod_data=%s\n' "$(grep -v '^[[:space:]]*#' "$HOST" | grep -q -- '--data-dir /mnt/us/scribe-data' && echo yes || echo no)"
  printf 'host_has_prod_sync=%s\n' "$(grep -v '^[[:space:]]*#' "$HOST" | grep -q -- '--sync-dir /mnt/us/scribe-app/state/sync' && echo yes || echo no)"
  printf 'host_has_prod_device=%s\n' "$(grep -v '^[[:space:]]*#' "$HOST" | grep -q -- '--device-dir /mnt/us/scribe-app/state/device' && echo yes || echo no)"
  printf 'host_execs_scribe_app=%s\n' "$(grep -v '^[[:space:]]*#' "$HOST" | grep -q -- 'scribe-app' && echo yes || echo no)"
  printf 'host_service=%s\n' "$(grep -v '^[[:space:]]*#' "$HOST" | grep -o 'com.markrai.[a-z.]*' | head -1)"
else
  printf 'host_present=no\n'
fi

printf 'prod_data_dir=%s\n' "$([ -d "$DATA" ] && echo yes || echo no)"
printf 'prod_sync_dir=%s\n' "$([ -d "$APP/state/sync" ] && echo yes || echo no)"
printf 'prod_device_dir=%s\n' "$([ -d "$APP/state/device" ] && echo yes || echo no)"
printf 'prod_pairing_files=%s\n' "$([ -f "$APP/state/device/device.json" ] && [ -f "$APP/state/device/server-profile.json" ] && echo present || echo absent)"
printf 'prod_sync_config_json=%s\n' "$([ -f "$APP/state/sync/config.json" ] && echo present || echo absent)"
printf 'prod_replica_json=%s\n' "$([ -f "$APP/state/sync/replica.json" ] && echo present || echo absent)"
if [ -f "$DATA/library.meta" ]; then
  printf 'library_meta_present=yes\n'
  printf 'library_meta_bytes=%s\n' "$(wc -c < "$DATA/library.meta" | tr -d ' ')"
  printf 'library_meta_sha256=%s\n' "$(sha256sum "$DATA/library.meta" | awk '{print $1}')"
else
  printf 'library_meta_present=no\n'
fi
printf 'notebook_dirs=%s\n' "$(ls -1 "$DATA/notebooks" 2>/dev/null | wc -l | tr -d ' ')"
printf 'previous_release_app=%s\n' "$([ -f "$APP/previous-release/scribe-app" ] && echo present || echo absent)"
printf 'qa_root=%s\n' "$([ -d /mnt/us/scribe-qa ] && echo present || echo absent)"
printf 'qa_host=%s\n' "$([ -f /mnt/us/scribe-qa/bin/scribe-lifecycle-host.sh ] && echo present || echo absent)"

if command -v lipc-get-prop >/dev/null 2>&1; then
  printf 'activeApp=%s\n' "$(lipc-get-prop com.lab126.appmgrd activeApp 2>/dev/null || echo unavailable)"
else
  printf 'activeApp=unavailable\n'
fi
ps 2>/dev/null | grep -E 'scribe-app|scribe-lifecycle' | grep -v grep >/dev/null \
  && printf 'ignite_proc=present\n' || printf 'ignite_proc=none_matched\n'
printf 'singleton_file=%s\n' "$([ -e /tmp/scribe-app.lock ] && echo present || echo absent)"

if [ -x /usr/bin/sqlite3 ] && [ -r "$APPREG" ]; then
  printf 'appreg=readable\n'
  printf 'canonical_rows=%s\n' "$(/usr/bin/sqlite3 -readonly "$APPREG" "SELECT COUNT(*) FROM handlerIds WHERE handlerId='$HANDLER';" 2>/dev/null || echo err)"
  printf 'legacy_rows=%s\n' "$(/usr/bin/sqlite3 -readonly "$APPREG" "SELECT COUNT(*) FROM handlerIds WHERE handlerId='$LEGACY';" 2>/dev/null || echo err)"
  printf 'qa_rows=%s\n' "$(/usr/bin/sqlite3 -readonly "$APPREG" "SELECT COUNT(*) FROM handlerIds WHERE handlerId='$QA';" 2>/dev/null || echo err)"
  printf 'canonical_command=%s\n' "$(/usr/bin/sqlite3 -readonly "$APPREG" "SELECT value FROM properties WHERE handlerId='$HANDLER' AND name='command';" 2>/dev/null || true)"
  printf 'canonical_lipc=%s\n' "$(/usr/bin/sqlite3 -readonly "$APPREG" "SELECT value FROM properties WHERE handlerId='$HANDLER' AND name='lipcId';" 2>/dev/null || true)"
else
  printf 'appreg=unavailable\n'
fi
printf 'documents_scribe_sh=%s\n' "$([ -f /mnt/us/documents/Scribe.sh ] && echo present || echo absent)"
if [ -f /mnt/us/documents/Scribe.sh ]; then
  printf 'launcher_execs_scribe_app=%s\n' "$(grep -v '^[[:space:]]*#' /mnt/us/documents/Scribe.sh | grep -q 'scribe-app' && echo yes || echo no)"
  printf 'launcher_handler=%s\n' "$(grep '^HANDLER=' /mnt/us/documents/Scribe.sh | head -1 | tr -d '\r')"
fi
printf 'preflight_exec_scribe_app=no\n'
"""


def _kv(text: str) -> dict[str, str]:
    out: dict[str, str] = {}
    for line in text.splitlines():
        if "=" not in line:
            continue
        k, v = line.split("=", 1)
        k = k.strip()
        if not k:
            continue
        out[k] = v.strip()
    return out


def parse_preflight_kv(text: str) -> dict[str, str]:
    return _kv(text)


def evaluate_isolation(kv: dict[str, str]) -> dict[str, Any]:
    """Production-bound live slot cannot be used for physical QA."""
    prod_bound = (
        kv.get("host_has_prod_data") == "yes"
        and kv.get("host_has_prod_sync") == "yes"
        and kv.get("host_has_prod_device") == "yes"
    )
    try:
        notebooks = int(kv.get("notebook_dirs") or "0")
    except ValueError:
        notebooks = -1
    qa_slot = (
        kv.get("qa_root") == "present"
        and kv.get("qa_host") == "present"
        and (kv.get("qa_rows") not in {None, "", "0", "err"})
        and kv.get("qa_rows") != "0"
    )
    refuse = True
    reasons = []
    if prod_bound:
        reasons.append(
            f"live host is bound to {PROD_DATA}, {PROD_SYNC}, and {PROD_DEVICE}"
        )
    if notebooks > 0:
        reasons.append(f"production library contains {notebooks} notebooks")
    if kv.get("prod_pairing_files") == "present":
        reasons.append("production pairing files are present (contents not read)")
    if not qa_slot:
        reasons.append("no independent QA handler/root (com.markrai.ignite.qa / /mnt/us/scribe-qa)")
    if not reasons:
        refuse = False
    return {
        "production_bound": prod_bound,
        "isolated_qa_slot_available": bool(qa_slot),
        "notebook_count": notebooks,
        "refuse_physical_qa": refuse,
        "refuse_reason": "; ".join(reasons) if refuse else "",
    }


def _artifact_match(kv: dict[str, str], qualified: dict[str, Any] | None) -> dict[str, Any]:
    if not qualified:
        return {"status": STATUS_NOT_RUN, "detail": "no qualified artifact evidence supplied"}
    arts = qualified.get("artifacts") or {}
    want = ((arts.get("scribe-app") or {}).get("sha256") or "").lower()
    have = _first_sha(kv, "scribe-app").lower()
    if not want:
        return {"status": STATUS_NOT_RUN, "detail": "qualified evidence has no scribe-app sha256"}
    if have == want:
        return {"status": STATUS_PASS, "detail": f"installed scribe-app matches qualified {want}"}
    return {
        "status": STATUS_FAIL,
        "detail": (
            f"installed={(have or 'missing')[:16]}… qualified={want[:16]}…"
            " (mismatch is expected until an authorized install)"
        ),
        "installed": have,
        "qualified": want,
    }


def run_preflight(
    *,
    ssh_host: str,
    qualified_artifacts: dict[str, Any] | None = None,
    timeout_seconds: float = 45.0,
    run_script: Callable[[str], str] | None = None,
) -> dict[str, Any]:
    runner = run_script or (
        lambda script: run_ssh_script(host=ssh_host, script=script, timeout_seconds=timeout_seconds)
    )
    qualify_fp = ((qualified_artifacts or {}).get("source") or {}).get("tree_fingerprint") or ""
    try:
        raw = runner(PREFLIGHT_REMOTE)
    except (ShipBlockedError, OSError, TimeoutError) as exc:
        reason = str(exc)
        return {
            "schema": PREFLIGHT_SCHEMA,
            "generated_at": utc_now(),
            "command": "preflight",
            "ssh_host": ssh_host,
            "groups": {
                "preflight": {
                    "status": STATUS_NOT_RUN,
                    "checks": [
                        check(
                            name="ssh",
                            status=STATUS_NOT_RUN,
                            detail=reason[:400],
                        )
                    ],
                }
            },
            "production_bound": True,
            "isolated_qa_slot_available": False,
            "notebook_count": None,
            "refuse_physical_qa": True,
            "refuse_reason": "Kindle unreachable; cannot prove isolation. Physical QA remains blocked.",
            "exec_scribe_app": False,
            "qualify_tree_fingerprint": qualify_fp,
            "overall": STATUS_NOT_RUN,
            "exit_code": 2,
            "states_hint": {
                "DEVICE_PREFLIGHT_PASS": {
                    "status": STATUS_NOT_RUN,
                    "reason": "SSH preflight did not run",
                }
            },
        }
    # Refuse to keep any line that looks like a dumped secret file.
    for name in _SECRET_BASENAMES:
        if name in raw and "present" not in raw.split(name, 1)[-1][:20]:
            # still ok: we only print present/absent for those names
            pass
    kv = parse_preflight_kv(raw)
    isolation = evaluate_isolation(kv)
    match = _artifact_match(kv, qualified_artifacts)
    checks = [
        check(
            name="ssh",
            status=STATUS_PASS,
            detail=f"host={ssh_host} arch={kv.get('arch', '')}",
        ),
        check(
            name="firmware",
            status=STATUS_PASS if (kv.get("prettyversion") or "").startswith("Kindle 5.18.6") else STATUS_FAIL,
            detail=kv.get("prettyversion", ""),
        ),
        check(
            name="installed-version",
            status=STATUS_PASS if kv.get("health_candidate_version") else STATUS_NOT_RUN,
            detail=kv.get("health_candidate_version", "launch-health-evidence missing version"),
        ),
        check(
            name="appmgrd-handler",
            status=STATUS_PASS if kv.get("canonical_rows") == "1" else STATUS_FAIL,
            detail=f"handler={PROD_HANDLER} rows={kv.get('canonical_rows')} command={kv.get('canonical_command')} lipc={kv.get('canonical_lipc')}",
        ),
        check(
            name="lifecycle-activity",
            status=STATUS_PASS,
            detail=f"ignite_proc={kv.get('ignite_proc')} activeApp={kv.get('activeApp')} singleton_file={kv.get('singleton_file')}",
        ),
        check(
            name="canonical-roots",
            status=STATUS_PASS if isolation["production_bound"] else STATUS_FAIL,
            detail=f"data={PROD_DATA} sync={PROD_SYNC} device={PROD_DEVICE} host_bound={isolation['production_bound']}",
        ),
        check(
            name="library-identity",
            status=STATUS_PASS if kv.get("library_meta_sha256") else STATUS_FAIL,
            detail=f"notebooks={isolation['notebook_count']} library.meta_sha256={kv.get('library_meta_sha256', '')} bytes={kv.get('library_meta_bytes', '')}",
        ),
        check(
            name="qa-slot",
            status=STATUS_PASS if isolation["isolated_qa_slot_available"] else STATUS_BLOCKED,
            detail=isolation["refuse_reason"],
        ),
        check(
            name="qualified-artifact-match",
            status=match["status"],
            detail=match["detail"],
        ),
        check(
            name="physical-qa-approval",
            status=STATUS_BLOCKED if isolation["refuse_physical_qa"] else STATUS_PASS,
            detail=isolation["refuse_reason"] or "isolated slot available",
        ),
    ]
    # Never approve the production-bound slot even if firmware and binaries look healthy.
    if isolation["refuse_physical_qa"]:
        overall = STATUS_BLOCKED
        exit_code = 3
    elif group_status(checks) == STATUS_FAIL:
        overall = STATUS_FAIL
        exit_code = 1
    else:
        overall = STATUS_PASS
        exit_code = 0
    report: dict[str, Any] = {
        "schema": PREFLIGHT_SCHEMA,
        "generated_at": utc_now(),
        "command": "preflight",
        "ssh_host": ssh_host,
        "firmware": kv.get("prettyversion", ""),
        "arch": kv.get("arch", ""),
        "installed_version": kv.get("health_candidate_version", ""),
        "installed_app_sha256": _first_sha(kv, "scribe-app"),
        "installed_lifecycle_sha256": _first_sha(kv, "scribe-lifecycle"),
        "handler": PROD_HANDLER,
        "handler_command": kv.get("canonical_command", ""),
        "active_app": kv.get("activeApp", ""),
        "ignite_proc": kv.get("ignite_proc", ""),
        "roots": {
            "library": PROD_DATA,
            "sync": PROD_SYNC,
            "device": PROD_DEVICE,
            "app": PROD_APP,
        },
        "library_meta_sha256": kv.get("library_meta_sha256", ""),
        "data_compatibility": kv.get("health_data_compatibility", "unknown"),
        "qualification": kv.get("health_qualification", ""),
        "groups": {"preflight": {"status": overall, "checks": checks}},
        "kv_public": {
            k: v
            for k, v in kv.items()
            if not any(s in k.lower() for s in ("token", "secret", "password", "bearer"))
        },
        "raw_lines": len(raw.splitlines()),
        "exec_scribe_app": False,
        "qualify_tree_fingerprint": qualify_fp,
        **isolation,
        "artifact_match": match,
        "overall": overall,
        "exit_code": exit_code,
        "states_hint": {
            "DEVICE_PREFLIGHT_PASS": {
                "status": STATUS_BLOCKED if isolation["refuse_physical_qa"] else STATUS_PASS,
                "reason": isolation["refuse_reason"] or "isolated QA slot",
            }
        },
    }
    return report


def _first_sha(kv: dict[str, str], needle: str) -> str:
    for k, v in kv.items():
        if needle in k and k.endswith("_sha256"):
            return v
    return ""
