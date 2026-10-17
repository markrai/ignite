#!/bin/sh
# Name: Uninstall Ignite
# Author: Ignite
# DontUseFBInk
#
# Removes Ignite Library integration and release-owned runtime files.
# Preserves /mnt/us/scribe-data (notebooks) and logs/state/appreg-backups.
# Does NOT purge notebooks. Destructive data deletion is not offered from this Scriptlet.

set -eu

LOG_DIR=/mnt/us/scribe-app/logs
LOG="$LOG_DIR/uninstall.log"
HELPERS=/mnt/us/scribe-app/tools/ignite-release/lib/uninstall-runtime.sh
STATUS=0

umask 022
mkdir -p "$LOG_DIR" 2>/dev/null || true

if {
  echo "==== Uninstall Ignite $(date -u +%Y-%m-%dT%H:%M:%SZ) ===="
  if [ ! -f "$HELPERS" ]; then
    echo "Uninstall Ignite: helpers not found at $HELPERS"
    echo "Re-run Install Ignite from a release package first."
    echo "FAILED."
    false
  else
    if [ "$(id -u)" -ne 0 ] && [ -x /var/local/kmc/bin/su ]; then
      /var/local/kmc/bin/su -c "sh '$HELPERS'"
    else
      sh "$HELPERS"
    fi
    echo "SUCCESS. Notebooks at /mnt/us/scribe-data were preserved."
  fi
} >>"$LOG" 2>&1; then
  STATUS=0
else
  STATUS=$?
fi

tail -n 40 "$LOG" 2>/dev/null || true
exit "$STATUS"
