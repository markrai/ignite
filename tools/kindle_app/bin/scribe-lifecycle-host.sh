#!/bin/sh
# appmgrd command entry for com.markrai.ignite
# appmgrd launches as framework; /dev/fb0 is root:video mode 660 → need root.
if [ "$(id -u)" -ne 0 ]; then
  exec /var/local/kmc/bin/su -c /mnt/us/scribe-app/bin/scribe-lifecycle-host.sh
fi
# D1 product mode: sync state + paired-server profile live outside the library.
# --service must match appreg lipcId / handlerId (com.markrai.ignite).
exec /mnt/us/scribe-app/bin/scribe-lifecycle --host \
  --service com.markrai.ignite \
  --child /mnt/us/scribe-app/bin/scribe-app -- \
  --confirm-write --data-dir /mnt/us/scribe-data \
  --sync-dir /mnt/us/scribe-app/state/sync \
  --device-dir /mnt/us/scribe-app/state/device
