#!/bin/sh
# Name: Ignite
# Author: Mark Rai
# Icon: /mnt/us/scribe-app/assets/scribe-icon.png
# DontUseFBInk
#
# Discovery-only entry: request the registered appmgrd application, then exit.
# Must NOT exec scribe-app directly (that recreates the unmanaged Scriptlet-child
# architecture rejected by F10.6).

APP_ROOT=/mnt/us/scribe-app
LOG_DIR="$APP_ROOT/logs"
CURRENT_LOG="$LOG_DIR/launcher.log"
PREVIOUS_LOG="$LOG_DIR/launcher.previous.log"
HANDLER=com.markrai.ignite
URI="app://$HANDLER"

umask 022

if ! mkdir -p "$LOG_DIR"; then
    exit 1
fi

timestamp() {
    date -u '+%Y-%m-%dT%H:%M:%SZ'
}

log_line() {
    printf '%s %s\n' "$(timestamp)" "$*" >>"$CURRENT_LOG" 2>/dev/null
}

if [ -f "$CURRENT_LOG" ]; then
    mv -f "$CURRENT_LOG" "$PREVIOUS_LOG" 2>/dev/null || true
fi
: >"$CURRENT_LOG" 2>/dev/null || true

log_line "discovery_start uri=$URI"

# If Scribe is already the foreground app, do nothing (blocks double-tap twins).
active=$(lipc-get-prop com.lab126.appmgrd activeApp 2>/dev/null || true)
if [ "$active" = "$HANDLER" ]; then
    log_line "discovery_skipped reason=already_activeApp activeApp=$active"
    exit 0
fi

if ! lipc-set-prop com.lab126.appmgrd start "$URI" 2>>"$CURRENT_LOG"; then
    log_line "discovery_failed reason=lipc_set_prop_start uri=$URI"
    exit 1
fi

log_line "discovery_requested uri=$URI"
# Exit immediately so SH Integration unload cannot kill the real Scribe host.
exit 0
