#!/bin/sh
# appmgrd command entry for com.markrai.ignite (full-feature open-source profile).
# Sync/pairing are available via --sync-dir/--device-dir; offline notebooks work
# without a server and without requiring pair/connect on first launch.
# appmgrd launches as framework; /dev/fb0 is root:video mode 660 → need root.
LOG=/mnt/us/scribe-app/logs/lifecycle-host.log
SU=/var/local/kmc/bin/su
HOST=/mnt/us/scribe-app/bin/scribe-lifecycle-host.sh
APP_ROOT=/mnt/us/scribe-app

mkdir -p "$APP_ROOT/logs" \
  "$APP_ROOT/state/sync" \
  "$APP_ROOT/state/device" 2>/dev/null || true
echo "host_start uid=$(id -u) su_exists=$([ -x "$SU" ] && echo yes || echo no) $(date -u +%Y-%m-%dT%H:%M:%SZ)" >>"$LOG" 2>/dev/null || true

if [ "$(id -u)" -ne 0 ]; then
  if [ ! -x "$SU" ]; then
    echo "host_abort reason=su_missing path=$SU" >>"$LOG" 2>/dev/null || true
    exit 1
  fi
  exec "$SU" -c "$HOST"
  echo "host_abort reason=su_exec_returned" >>"$LOG" 2>/dev/null || true
  exit 1
fi
# --service must match appreg lipcId / handlerId (com.markrai.ignite).
exec /mnt/us/scribe-app/bin/scribe-lifecycle --host \
  --service com.markrai.ignite \
  --child /mnt/us/scribe-app/bin/scribe-app -- \
  --confirm-write --data-dir /mnt/us/scribe-data \
  --sync-dir /mnt/us/scribe-app/state/sync \
  --device-dir /mnt/us/scribe-app/state/device
