#!/bin/sh
# Remove Ignite release-owned runtime integration. Preserves notebooks by default.
# Never deletes /mnt/us/scribe-data unless --purge-data is passed explicitly.
# Preserves logs/, state/, appreg-backups/.
set -eu

PURGE_DATA=0
for arg in "$@"; do
  case "$arg" in
    --purge-data) PURGE_DATA=1 ;;
    -h|--help)
      echo "usage: uninstall-runtime.sh [--purge-data]" >&2
      echo "  default: unregister Ignite, remove release-owned files, keep notebooks" >&2
      echo "  --purge-data: ALSO delete /mnt/us/scribe-data (destructive; irreversible)" >&2
      exit 0
      ;;
  esac
done

APP_ROOT="${IGNITE_APP_ROOT:-/mnt/us/scribe-app}"
DATA_DIR="${IGNITE_DATA_DIR:-/mnt/us/scribe-data}"
DOCS="${IGNITE_DOCS:-/mnt/us/documents}"
SCRIPTLET=$DOCS/Scribe.sh
# Stamped at packaging (Install Ignite v<version>.sh).
IGNITE_PACKAGE_VERSION='@@IGNITE_PACKAGE_VERSION@@'

# Uninstall shares the same nonblocking kernel transaction lock as install and recovery. The
# installed lifecycle binary is safe to unlink after it execs this shell: the inherited descriptor,
# not the binary pathname, owns the lock for the complete operation.
UPDATER_LOCK_HELPER="${IGNITE_UPDATER_LOCK_HELPER:-$APP_ROOT/bin/scribe-lifecycle}"
if [ "${IGNITE_UPDATER_LOCK_HELD:-}" != "1" ]; then
  if [ ! -f "$UPDATER_LOCK_HELPER" ] || [ ! -x "$UPDATER_LOCK_HELPER" ]; then
    echo "uninstall_failed: coordination: updater transaction lock helper is unavailable" >&2
    exit 1
  fi
  exec "$UPDATER_LOCK_HELPER" --with-updater-transaction-lock -- \
    /bin/sh "$0" "$@"
fi
case "${IGNITE_UPDATER_LOCK_FD:-}" in
  ''|*[!0-9]*)
    echo "uninstall_failed: coordination: updater transaction lock handoff is invalid" >&2
    exit 1
    ;;
esac
if "$UPDATER_LOCK_HELPER" --verify-updater-transaction-lock "$IGNITE_UPDATER_LOCK_FD"; then
  :
else
  lock_status=$?
  echo "uninstall_failed: coordination: inherited descriptor does not hold the updater transaction lock" >&2
  exit "$lock_status"
fi

LIB_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=register-helpers.sh
. "$LIB_DIR/register-helpers.sh"
# shellcheck source=install-recovery.sh
. "$LIB_DIR/install-recovery.sh"

log() { printf '%s\n' "$*"; }
die() { printf 'uninstall_failed: %s\n' "$*" >&2; exit 1; }

# Refuse while running
for process_dir in /proc/[0-9]*; do
  process_pid=${process_dir##*/}
  process_exe=$(readlink -f "$process_dir/exe" 2>/dev/null || true)
  case "$process_exe" in
    "$APP_ROOT/bin/scribe-app"|"$APP_ROOT/bin/scribe-lifecycle"|"/mnt/us/scribe-app")
      die "Ignite is running as PID $process_pid. Open Home, then run Uninstall Ignite again."
      ;;
  esac
done

if [ -f "$APPREG" ]; then
  # Unregister canonical com.markrai.ignite and Ignite-owned legacy com.markrai.scribe.
  ignite_unregister_all_ignite_handlers || die "unregister failed"
else
  log "appreg missing; skipping unregister"
fi

# Release-owned files only (never wipe logs/, state/, appreg-backups/)
rm -f "$APP_ROOT/bin/scribe-app" \
  "$APP_ROOT/bin/scribe-lifecycle" \
  "$APP_ROOT/bin/scribe-lifecycle-host.sh" \
  "$APP_ROOT/assets/scribe-icon.png" \
  "$APP_ROOT/assets/licenses/noto-emoji/OFL.txt" \
  "$APP_ROOT/assets/licenses/noto-emoji/DESCRIPTION.en_us.html" \
  "$APP_ROOT/assets/licenses/noto-emoji/upstream_info.md" \
  "$APP_ROOT/assets/licenses/source-sans-3/OFL.txt" \
  "$SCRIPTLET" \
  "$DOCS/Install Ignite v@@IGNITE_PACKAGE_VERSION@@.sh" \
  "$DOCS/Install-Ignite.sh" \
  "$DOCS/Install Ignite.sh" \
  "$DOCS/Uninstall-Ignite.sh" \
  "$DOCS/Uninstall Ignite.sh" \
  "$DOCS/Backup-Ignite-Data.sh" \
  "$DOCS/Backup Ignite Data.sh" \
  "$DOCS/Diagnose Ignite.sh" \
  "$DOCS/Diagnose-Ignite.sh"

# Remove installed release helper tree (not logs/state/appreg-backups)
rm -rf "$APP_ROOT/tools/ignite-release"

# Recovery client is not on FSP. Never follow a symlink. Preserve notebooks.
if [ -L "${IGNITE_RECOVERY_ROOT:-/var/local/ignite-recovery}" ]; then
  log "recovery_uninstall: refused symlink at ${IGNITE_RECOVERY_ROOT:-/var/local/ignite-recovery}"
elif ! ignite_recovery_uninstall "${IGNITE_RECOVERY_ROOT:-/var/local/ignite-recovery}"; then
  die "recovery uninstall failed"
fi

# Do not remove logs/, state/, appreg-backups/, or the scribe-app directory itself.
log "preserved: $APP_ROOT/logs $APP_ROOT/state $APP_ROOT/appreg-backups (if present)"

if [ "$PURGE_DATA" -eq 1 ]; then
  if [ -e "$DATA_DIR" ]; then
    log "WARNING: deleting notebook library $DATA_DIR"
    rm -rf "$DATA_DIR"
    log "purge_data_ok path=$DATA_DIR"
  else
    log "purge_data: $DATA_DIR already absent"
  fi
else
  log "notebooks preserved at $DATA_DIR (pass --purge-data to delete)"
fi

sync
log "uninstall_ok"
exit 0
