#!/bin/sh
# Read-only archive of /mnt/us/scribe-data into /mnt/us/Ignite-Backups/.
# Never deletes older backups. Never restores. Never modifies the library.
set -eu

DATA_DIR=/mnt/us/scribe-data
BACKUP_ROOT=/mnt/us/Ignite-Backups

die() { printf 'backup_failed: %s\n' "$*" >&2; exit 1; }
log() { printf '%s\n' "$*"; }

[ -d "$DATA_DIR" ] || die "notebook library not found: $DATA_DIR (nothing to back up yet)"

# Ensure we never write the archive inside the library.
case "$BACKUP_ROOT" in
  "$DATA_DIR"|"$DATA_DIR"/*) die "refusing backup destination inside $DATA_DIR" ;;
esac

mkdir -p "$BACKUP_ROOT" || die "cannot create $BACKUP_ROOT"

TS=$(date -u +%Y%m%d-%H%M%S)
OUT_NAME="Ignite-Backup-${TS}.tar.gz"
OUT_PATH="$BACKUP_ROOT/$OUT_NAME"
TMP_PATH="$BACKUP_ROOT/.$OUT_NAME.partial"

rm -f "$TMP_PATH"

# Rough free-space check (KB): need at least library size + 5% headroom.
NEED_KB=$(du -sk "$DATA_DIR" 2>/dev/null | awk '{print $1}')
[ -n "$NEED_KB" ] || die "cannot measure size of $DATA_DIR"
NEED_KB=$((NEED_KB + NEED_KB / 20 + 1024))
AVAIL_KB=$(df -k "$BACKUP_ROOT" 2>/dev/null | awk 'NR==2 {print $4}')
[ -n "$AVAIL_KB" ] || die "cannot measure free space on $BACKUP_ROOT"
[ "$AVAIL_KB" -ge "$NEED_KB" ] || die "insufficient space: need ~${NEED_KB}KB free, have ${AVAIL_KB}KB"

# Archive from parent so the tarball contains scribe-data/ as the top entry.
# Read-only with respect to DATA_DIR: tar -c only.
if command -v tar >/dev/null 2>&1; then
  if tar -czf "$TMP_PATH" -C /mnt/us scribe-data 2>/tmp/ignite-backup-tar.err; then
    :
  else
    rm -f "$TMP_PATH"
    die "tar failed: $(cat /tmp/ignite-backup-tar.err 2>/dev/null || echo unknown)"
  fi
else
  die "tar not available on this Kindle"
fi

[ -f "$TMP_PATH" ] && [ -s "$TMP_PATH" ] || {
  rm -f "$TMP_PATH"
  die "archive missing or empty after tar"
}

mv -f "$TMP_PATH" "$OUT_PATH" || die "rename to $OUT_PATH failed"
sync

log "backup_ok path=$OUT_PATH"
log "Copy $OUT_PATH off the Kindle over USB for safekeeping."
log "Restore is a manual support operation (not automated in this release)."
exit 0
