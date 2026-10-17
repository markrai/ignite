#!/bin/sh
# Name: Backup Ignite Data
# Author: Ignite
# DontUseFBInk
#
# Creates a timestamped archive of /mnt/us/scribe-data under /mnt/us/Ignite-Backups/.
# Read-only with respect to the library. Never restores. Never deletes older backups.

set -eu

LOG_DIR=/mnt/us/scribe-app/logs
LOG="$LOG_DIR/backup.log"
HELPERS=/mnt/us/scribe-app/tools/ignite-release/lib/backup-runtime.sh
STATUS=0

umask 022
mkdir -p "$LOG_DIR" 2>/dev/null || true

if {
  echo "==== Backup Ignite Data $(date -u +%Y-%m-%dT%H:%M:%SZ) ===="
  if [ ! -f "$HELPERS" ]; then
    echo "Backup Ignite Data: helpers not found at $HELPERS"
    echo "Run Install Ignite once so backup tools are installed, then try again."
    echo "FAILED. Notebooks were not modified."
    false
  else
    if [ "$(id -u)" -ne 0 ] && [ -x /var/local/kmc/bin/su ]; then
      /var/local/kmc/bin/su -c "sh '$HELPERS'"
    else
      sh "$HELPERS"
    fi
    echo "SUCCESS. Copy the new file from Ignite-Backups off the Kindle over USB."
  fi
} >>"$LOG" 2>&1; then
  STATUS=0
else
  STATUS=$?
fi

tail -n 40 "$LOG" 2>/dev/null || true
exit "$STATUS"
