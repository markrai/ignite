#!/bin/sh
# Name: Diagnose Ignite
# Author: Ignite
# DontUseFBInk
#
# USB-visible launch diagnosis. Does not open notebooks or start scribe-app.

set -eu

APP_ROOT="${IGNITE_APP_ROOT:-/mnt/us/scribe-app}"
LOG_DIR="$APP_ROOT/logs"
DOCS="${IGNITE_DOCS:-/mnt/us/documents}"
REPORT="$DOCS/Ignite-Diagnose.txt"
LOG="$LOG_DIR/diagnose.log"
APPREG="${IGNITE_APPREG:-/var/local/appreg.db}"
HANDLER=com.markrai.ignite
LEGACY_HANDLER=com.markrai.scribe
SU="${IGNITE_SU:-/var/local/kmc/bin/su}"
HOST=$APP_ROOT/bin/scribe-lifecycle-host.sh
EXPECTED_COMMAND=$HOST

umask 022
mkdir -p "$LOG_DIR" "$DOCS" 2>/dev/null || true

{
  echo "==== Diagnose Ignite $(date -u +%Y-%m-%dT%H:%M:%SZ) ===="
  echo "uid=$(id -u 2>/dev/null || echo '?')"
  echo "su_path=$SU"
  if [ -x "$SU" ]; then echo "su_executable=yes"; else echo "su_executable=no"; fi

  echo "--- SH Integration ---"
  if [ -x /usr/bin/sqlite3 ] && [ -r "$APPREG" ]; then
    sh_associations=$(
      /usr/bin/sqlite3 -readonly "$APPREG" \
        "SELECT COUNT(*) FROM associations WHERE (interface='application' AND handlerId='tech.hackerdude.shell_integration.launcher' AND contentId='MT:text/x-shellscript') OR (interface='extractor' AND handlerId='tech.hackerdude.shell_integration.extractor' AND contentId='GL:*.sh');" \
        2>/dev/null || echo err
    )
    echo "sh_associations=$sh_associations (need 2)"
  else
    echo "sh_associations=unavailable"
  fi

  echo "--- appreg registration (canonical + legacy) ---"
  echo "canonical_handler=$HANDLER"
  echo "legacy_handler=$LEGACY_HANDLER"
  if [ -x /usr/bin/sqlite3 ] && [ -r "$APPREG" ]; then
    new_n=$(/usr/bin/sqlite3 -readonly "$APPREG" "SELECT COUNT(*) FROM handlerIds WHERE handlerId='$HANDLER';" 2>/dev/null || echo err)
    leg_n=$(/usr/bin/sqlite3 -readonly "$APPREG" "SELECT COUNT(*) FROM handlerIds WHERE handlerId='$LEGACY_HANDLER';" 2>/dev/null || echo err)
    new_cmd=$(/usr/bin/sqlite3 -readonly "$APPREG" "SELECT value FROM properties WHERE handlerId='$HANDLER' AND name='command';" 2>/dev/null || true)
    leg_cmd=$(/usr/bin/sqlite3 -readonly "$APPREG" "SELECT value FROM properties WHERE handlerId='$LEGACY_HANDLER' AND name='command';" 2>/dev/null || true)
    echo "canonical_rows=$new_n"
    echo "legacy_rows=$leg_n"
    echo "canonical_command=${new_cmd:-unset}"
    echo "legacy_command=${leg_cmd:-unset}"
    case "$new_n:$leg_n" in
      1:0) echo "registration_state=canonical_only" ;;
      0:1) echo "registration_state=legacy_only" ;;
      1:1) echo "registration_state=both_present" ;;
      0:0) echo "registration_state=neither" ;;
      *) echo "registration_state=inconsistent" ;;
    esac
    if [ "$new_n" = "1" ] && [ "$new_cmd" = "$EXPECTED_COMMAND" ]; then
      echo "canonical_command_ok=yes"
    else
      echo "canonical_command_ok=no"
    fi
    if [ "$new_n" = "1" ] && [ "$leg_n" != "0" ] && [ "$leg_n" != "err" ]; then
      echo "migration_note=both identities present; re-run Install Ignite to retire legacy"
    fi
    if [ "$new_n" = "0" ] && [ "$leg_n" = "1" ]; then
      echo "migration_note=legacy_only; run Install Ignite v0.9.11+ to migrate to $HANDLER"
    fi
  else
    echo "appreg=unavailable"
    echo "registration_state=appreg_unavailable"
  fi

  echo "--- recovery component (not executed) ---"
  REC="${IGNITE_RECOVERY_ROOT:-/var/local/ignite-recovery}"
  if [ -L "$REC" ]; then
    echo "recovery_root=symlink_refused"
  elif [ -d "$REC" ]; then
    echo "recovery_root=present"
    if [ -f "$REC/bin/dropbearmulti" ]; then
      echo "recovery_elf_bytes=$(wc -c < "$REC/bin/dropbearmulti" | tr -d ' ')"
    else
      echo "recovery_elf=missing"
    fi
    if [ -f "$REC/manifest/elf.sha256" ]; then
      echo "recovery_manifest=present"
    else
      echo "recovery_manifest=missing"
    fi
  else
    echo "recovery_root=absent"
  fi

  echo "--- binaries ---"
  for rel in bin/scribe-app bin/scribe-lifecycle bin/scribe-lifecycle-host.sh; do
    p="$APP_ROOT/$rel"
    if [ -f "$p" ]; then
      sz=$(wc -c < "$p" | tr -d ' ')
      echo "present $rel bytes=$sz"
    else
      echo "missing $rel"
    fi
  done

  echo "--- host profile ---"
  if [ -f "$HOST" ]; then
    if grep -v '^[[:space:]]*#' "$HOST" | grep -q -- '--sync-dir'; then
      echo "host_profile=sync_flags_present"
    else
      echo "host_profile=standalone"
    fi
    grep -v '^[[:space:]]*#' "$HOST" | grep -q -- '--confirm-write' && echo "confirm_write=yes" || echo "confirm_write=no"
    if grep -v '^[[:space:]]*#' "$HOST" | grep -q -- 'com.markrai.ignite'; then
      echo "host_service=com.markrai.ignite"
    elif grep -v '^[[:space:]]*#' "$HOST" | grep -q -- 'com.markrai.scribe'; then
      echo "host_service=com.markrai.scribe"
    else
      echo "host_service=default_or_unset"
    fi
  else
    echo "host_profile=missing"
  fi

  echo "--- recent logs (tails) ---"
  for lf in install.log launcher.log lifecycle-host.log scribe-app.child.log; do
    echo "-- $lf --"
    if [ -f "$LOG_DIR/$lf" ]; then
      tail -n 20 "$LOG_DIR/$lf" 2>/dev/null || true
    else
      echo "(absent)"
    fi
  done

  echo "--- live appmgrd (no notebook open) ---"
  if command -v lipc-get-prop >/dev/null 2>&1; then
    echo "activeApp=$(lipc-get-prop com.lab126.appmgrd activeApp 2>/dev/null || echo err)"
  else
    echo "lipc-get-prop=unavailable"
  fi

  echo "--- device / firmware ---"
  if [ -r /etc/prettyversion.txt ]; then
    echo "prettyversion=$(tr '\n' ' ' < /etc/prettyversion.txt | tr -s ' ')"
  else
    echo "prettyversion=unavailable"
  fi
  if [ -r /mnt/us/system/version.txt ]; then
    echo "system_version=$(head -n 3 /mnt/us/system/version.txt | tr '\n' ' ')"
  else
    echo "system_version=unavailable"
  fi
  if [ -r /proc/device-tree/model ]; then
    echo "dt_model=$(tr -d '\0' < /proc/device-tree/model)"
  else
    echo "dt_model=unavailable"
  fi
  if [ -r /proc/fb ]; then
    echo "proc_fb=$(tr '\n' ' ' < /proc/fb)"
  else
    echo "proc_fb=unavailable"
  fi

  echo "--- framebuffer sysfs (read-only) ---"
  FB0=/sys/class/graphics/fb0
  if [ -d "$FB0" ]; then
    for f in name virtual_size stride bits_per_pixel rotate modes mode pan; do
      if [ -r "$FB0/$f" ]; then
        echo "sysfs_$f=$(tr '\n' ' ' < "$FB0/$f" | tr -s ' ')"
      fi
    done
  else
    echo "sysfs_fb0=unavailable"
  fi

  echo "--- framebuffer ioctl (FBIOGET only) ---"
  APP_BIN="$APP_ROOT/bin/scribe-app"
  if [ -x "$APP_BIN" ]; then
    if [ "$(id -u)" -ne 0 ] && [ -x "$SU" ]; then
      "$SU" -c "$APP_BIN --diagnose-fb" 2>&1 || echo "fb_ioctl=unavailable"
    else
      "$APP_BIN" --diagnose-fb 2>&1 || echo "fb_ioctl=unavailable"
    fi
  else
    echo "fb_ioctl=unavailable (scribe-app missing)"
  fi

  echo "note=this report does not start Ignite, mmap-write the panel, or open /mnt/us/scribe-data"
  echo "==== end ===="
} >"$REPORT" 2>&1

cp -f "$REPORT" "$LOG" 2>/dev/null || true
tail -n 40 "$REPORT" 2>/dev/null || true
exit 0
