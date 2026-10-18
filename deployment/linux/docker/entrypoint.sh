#!/bin/sh
# Ignite Server container entrypoint.
# - When started as root (image default): chown state volume, then setpriv → 19187:19187.
# - PID 1 after drop is non-root; the brief root phase is for volume init only.
# - Never runs `scribe-server init` implicitly (operators must invoke init explicitly).
# - Does not print setup codes; `serve` never emits them.
set -eu

SCRIBE_UID="${SCRIBE_UID:-19187}"
SCRIBE_GID="${SCRIBE_GID:-19187}"
STATE_DIR="${SCRIBE_STATE_DIR:-/var/lib/scribe-server}"
BIN="/usr/local/bin/scribe-server"

mkdir -p "$STATE_DIR"

if [ "$(id -u)" -eq 0 ]; then
  chown "${SCRIBE_UID}:${SCRIBE_GID}" "$STATE_DIR" || {
    echo "ignite-server: ERROR: cannot chown ${STATE_DIR} to ${SCRIBE_UID}:${SCRIBE_GID}" >&2
    exit 1
  }
  chmod 700 "$STATE_DIR" || true
  if [ "$#" -eq 0 ]; then
    set -- serve --config /etc/scribe-server/config.toml
  fi
  exec setpriv --reuid="${SCRIBE_UID}" --regid="${SCRIBE_GID}" --clear-groups \
    --inh-caps=-all --ambient-caps=-all -- "$BIN" "$@"
fi

# Already non-root (e.g. compose user override).
if [ ! -w "$STATE_DIR" ]; then
  echo "ignite-server: ERROR: state directory not writable: ${STATE_DIR}
Fix ownership for UID ${SCRIBE_UID} (named volumes are often root-owned until first root entrypoint start)." >&2
  exit 1
fi

if [ "$#" -eq 0 ]; then
  set -- serve --config /etc/scribe-server/config.toml
fi
exec "$BIN" "$@"
