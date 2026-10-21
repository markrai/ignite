#!/usr/bin/env bash
# Ignite Server — uninstall (Linux portable self-hosting).
set -euo pipefail

BUNDLE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=path-safety.sh
. "$BUNDLE/path-safety.sh"

ROOT=""
PURGE=0
for arg in "$@"; do
  case "$arg" in
    --root=*) ROOT="${arg#--root=}" ;;
    --purge) PURGE=1 ;;
    -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
    *) echo "uninstall.sh: unknown option $arg" >&2; exit 2 ;;
  esac
done

die() { echo "uninstall.sh: $*" >&2; exit 1; }

if [ -n "$ROOT" ]; then
  ROOT="$(ignite_reject_staging_root "$ROOT" "staging root")" || exit 1
  ignite_assert_staging_destinations_safe "$ROOT" || exit 1
  if [ "$PURGE" = 1 ]; then
    ignite_require_staging_marker "$ROOT" || exit 1
  fi
fi

if [ -z "$ROOT" ] && [ "$(id -u)" -ne 0 ]; then die "run as root (sudo)"; fi

# shellcheck source=firewall.sh
. "$BUNDLE/firewall.sh"

ETC="$ROOT/etc/scribe-server"
STATE="$ROOT/var/lib/scribe-server"
REMOVAL_FAILED=0

# Remove a path when present. Nonexistent paths are OK (idempotent).
# Required removals that fail are recorded; success is not claimed later.
safe_rm() {
  local path="$1"
  local flags="${2:-}"
  if [ ! -e "$path" ] && [ ! -L "$path" ]; then
    return 0
  fi
  if [ -n "$ROOT" ]; then
    local resolved
    resolved="$(ignite_canonicalize "$path")"
    ignite_path_contains "$ROOT" "$resolved" || die "refusing to remove path outside staging root: $path -> $resolved"
    if [ -L "$path" ]; then
      die "refusing to remove staging symlink: $path"
    fi
  fi
  # shellcheck disable=SC2086
  if ! rm $flags -- "$path"; then
    echo "uninstall.sh: failed to remove $path" >&2
    REMOVAL_FAILED=1
    return 1
  fi
  return 0
}

if [ -z "$ROOT" ] && command -v systemctl >/dev/null 2>&1; then
  systemctl disable --now scribe-server.service >/dev/null 2>&1 || true
fi

# Refuse symlink escape on unit path before rm.
if [ -n "$ROOT" ]; then
  ignite_assert_path_under_staging_root "$ROOT" "etc/systemd/system/scribe-server.service" || exit 1
fi

safe_rm "$ROOT/etc/systemd/system/scribe-server.service" -f || true
safe_rm "$ROOT/etc/systemd/system/scribe-server.service.old" -f || true
safe_rm "$ROOT/etc/systemd/system/scribe-server.service.new" -f || true
[ -z "$ROOT" ] && command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload || true
if [ -z "$ROOT" ]; then
  FW_RECORD="$(sed -n 's/^firewall=//p' "$ETC/install-state" 2>/dev/null || true)"
  [ -n "$FW_RECORD" ] && fw_close "$FW_RECORD"
fi
safe_rm "$ROOT/usr/local/bin/scribe-server" -f || true
safe_rm "$ROOT/usr/local/bin/scribe-server.old" -f || true
safe_rm "$ROOT/usr/local/bin/scribe-server.new" -f || true
safe_rm "$ROOT/usr/local/bin/ignite-update" -f || true
safe_rm "$ROOT/usr/local/bin/ignite-update.new" -f || true
safe_rm "$ROOT/usr/local/bin/ignite-update.old" -f || true
if [ -n "$ROOT" ]; then
  if [ -e "$ROOT/usr/share/scribe-server" ] || [ -L "$ROOT/usr/share/scribe-server" ]; then
    ignite_assert_path_under_staging_root "$ROOT" "usr/share/scribe-server" || exit 1
  fi
fi
safe_rm "$ROOT/usr/share/scribe-server" -rf || true
safe_rm "$ETC/install-state" -f || true

if [ "$PURGE" = 1 ]; then
  if [ -n "$ROOT" ]; then
    ignite_assert_path_under_staging_root "$ROOT" "var/lib/scribe-server" || exit 1
    ignite_assert_path_under_staging_root "$ROOT" "etc/scribe-server" || exit 1
  fi
  safe_rm "$STATE" -rf || true
  safe_rm "$ETC" -rf || true
  if [ -n "$ROOT" ]; then
    if [ -L "$ROOT/$IGNITE_STAGING_MARKER" ]; then
      die "refusing to remove symlinked staging marker: $ROOT/$IGNITE_STAGING_MARKER"
    fi
    safe_rm "$ROOT/$IGNITE_STAGING_MARKER" -f || true
  fi
  if [ -z "$ROOT" ] && id scribe >/dev/null 2>&1; then userdel scribe 2>/dev/null || true; fi
  if [ "$REMOVAL_FAILED" = 1 ]; then
    die "removal incomplete; not claiming full purge success"
  fi
  echo "Ignite Server removed, including all of its data (--purge)."
else
  if [ "$REMOVAL_FAILED" = 1 ]; then
    die "removal incomplete; data kept in $STATE"
  fi
  echo "Ignite Server removed. Data kept in $STATE (reinstall resumes it; --purge deletes it)."
fi
