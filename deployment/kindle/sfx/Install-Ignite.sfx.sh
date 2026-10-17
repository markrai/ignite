#!/bin/sh
# Name: Install Ignite
# Author: Ignite
# DontUseFBInk
#
# Self-extracting Ignite installer (full-feature open-source).
# Shell prefix ends at EOF of this script; a gzip+tar payload is appended.
# BusyBox ash executes sequentially and never parses bytes after `exit`.

set -eu

# Stamped at packaging time. Fixed-width numeric fields so OFFSET can be patched
# in place without changing the prefix length.
IGNITE_PACKAGE_VERSION='@@IGNITE_PACKAGE_VERSION@@'
IGNITE_ARCHIVE_OFFSET=0000000000
IGNITE_ARCHIVE_BYTES=0000000000
IGNITE_ARCHIVE_SHA256=0000000000000000000000000000000000000000000000000000000000000000
IGNITE_ARCHIVE_MD5=00000000000000000000000000000000

LOG_DIR="${IGNITE_LOG_DIR:-/mnt/us/scribe-app/logs}"
LOG="$LOG_DIR/install.log"
STAGE="${IGNITE_STAGE:-/mnt/us/scribe-app/run/ignite-extract-${IGNITE_PACKAGE_VERSION}}"
PACKAGE_ROOT="${STAGE}/root/Ignite-Kindle-${IGNITE_PACKAGE_VERSION}"
DOCS="${IGNITE_DOCS:-/mnt/us/documents}"
STATUS=1

umask 022

resolve_self() {
  _self=$0
  case "$_self" in
    /*) ;;
    *)
      _self="$(CDPATH= cd -- "$(dirname -- "$_self")" && pwd)/$(basename -- "$_self")"
      ;;
  esac
  printf '%s\n' "$_self"
}

die() {
  printf '%s\n' "$*" >&2
  exit 1
}

# BusyBox ash treats $((0nnn)) as octal. Fixed-width stamps like 0000003954
# contain 8/9 and abort with "arithmetic syntax error". Strip zeros first.
decimal() {
  v=$1
  while [ "$v" != "${v#0}" ] && [ "$v" != "0" ]; do
    v=${v#0}
  done
  [ -n "$v" ] || v=0
  printf '%s\n' "$v"
}

cleanup_stage() {
  rm -rf "$STAGE" 2>/dev/null || true
}

# USB-visible report on any prefix failure, including before the payload extracts.
# Diagnose Ignite (Library) is copied only after tar extract succeeds.
write_usb_fail_report() {
  mkdir -p "$DOCS" 2>/dev/null || true
  {
    echo "==== Diagnose Ignite (install prefix) $(date -u +%Y-%m-%dT%H:%M:%SZ) ===="
    echo "install_status=${STATUS:-1}"
    echo "self=${SELF:-unknown}"
    echo "package_version=$IGNITE_PACKAGE_VERSION"
    if [ -x /var/local/kmc/bin/su ]; then echo "su_executable=yes"; else echo "su_executable=no"; fi
    echo "note=Diagnose Ignite is installed only after the payload extracts successfully."
    echo "note=This report is written on any install failure so Documents still has a diagnosis over USB."
    echo "--- install.log (tail) ---"
    if [ -f "$LOG" ]; then
      tail -n 80 "$LOG" 2>/dev/null || echo "(unreadable install.log)"
    else
      echo "(no install.log)"
    fi
  } >"$DOCS/Ignite-Diagnose.txt" 2>/dev/null || true
}

mkdir -p "$LOG_DIR" 2>/dev/null || true

SELF=$(resolve_self)

# Elevate once; keep the same file path (spaces-safe).
if [ "$(id -u)" -ne 0 ] && [ -x /var/local/kmc/bin/su ]; then
  exec /var/local/kmc/bin/su -c "sh \"$SELF\""
fi

# Subshell + set -e: a failed command exits the subshell before SUCCESS / rm.
# Do not wrap as `{ ... } && STATUS=0 || STATUS=$?` — that disables set -e.
set +e
(
  set -e
  echo "==== Install Ignite ${IGNITE_PACKAGE_VERSION} $(date -u +%Y-%m-%dT%H:%M:%SZ) ===="
  echo "self=$SELF"

  command -v tar >/dev/null 2>&1 || die "FAILED: this Kindle is missing tar (unsupported environment)."
  command -v dd >/dev/null 2>&1 || die "FAILED: this Kindle is missing dd (unsupported environment)."
  command -v md5sum >/dev/null 2>&1 || die "FAILED: this Kindle is missing md5sum (unsupported environment)."

  if [ "${IGNITE_SKIP_SPACE_CHECK:-}" != "1" ]; then
    SPACE_ROOT="${IGNITE_US_ROOT:-/mnt/us}"
    if [ -n "${IGNITE_AVAIL_KB:-}" ]; then
      AVAIL_KB=$IGNITE_AVAIL_KB
    else
      AVAIL_KB=$(df -k "$SPACE_ROOT" 2>/dev/null | awk 'NR==2 {print $4}')
    fi
    [ -n "$AVAIL_KB" ] || die "FAILED: cannot measure free space on the Kindle storage."
    # Unpacked payload + runtime copies + previous-release snapshot. 32 MB floor.
    NEED_KB=32768
    [ "$AVAIL_KB" -ge "$NEED_KB" ] || die "FAILED: not enough free space (need 32 MB). Delete unused files and copy Install Ignite again."
  fi

  [ -f "$SELF" ] && [ -s "$SELF" ] || die "FAILED: installer file is missing."
  OFFSET_N=$(decimal "$IGNITE_ARCHIVE_OFFSET")
  BYTES_N=$(decimal "$IGNITE_ARCHIVE_BYTES")
  TOTAL=$(decimal "$(wc -c < "$SELF" | tr -d ' ')")
  NEED=$((OFFSET_N + BYTES_N))
  [ "$TOTAL" -ge "$NEED" ] || die "FAILED: download is incomplete. Copy Install Ignite into Documents again."
  [ "$TOTAL" -eq "$NEED" ] || die "FAILED: installer file is damaged (unexpected extra data). Download Install Ignite again."

  cleanup_stage
  mkdir -p "$STAGE/root" || die "FAILED: cannot create a temporary install folder."
  ARCHIVE="$STAGE/payload.tgz"

  # One-block skip: bs=OFFSET skip=1 copies everything after the shell prefix.
  dd if="$SELF" of="$ARCHIVE" bs="$OFFSET_N" skip=1 2>/dev/null || die "FAILED: could not read the installer payload."
  GOT=$(decimal "$(wc -c < "$ARCHIVE" | tr -d ' ')")
  [ "$GOT" -eq "$BYTES_N" ] || die "FAILED: download is incomplete or damaged."

  MAG=$(dd if="$ARCHIVE" bs=2 count=1 2>/dev/null | wc -c | tr -d ' ')
  [ "$MAG" = "2" ] || die "FAILED: installer payload is empty."
  SUM=$(md5sum "$ARCHIVE")
  SUM=${SUM%% *}
  [ "$SUM" = "$IGNITE_ARCHIVE_MD5" ] || die "FAILED: download is damaged. Copy the file again."
  if command -v sha256sum >/dev/null 2>&1; then
    SHA=$(sha256sum "$ARCHIVE")
    SHA=${SHA%% *}
    [ "$SHA" = "$IGNITE_ARCHIVE_SHA256" ] || die "FAILED: download is damaged. Copy the file again."
  fi

  tar -tzf "$ARCHIVE" >/dev/null 2>&1 || die "FAILED: installer payload is damaged."
  tar -xzf "$ARCHIVE" -C "$STAGE/root" || die "FAILED: could not unpack Ignite."
  [ -f "$PACKAGE_ROOT/lib/install-runtime.sh" ] || die "FAILED: installer payload is incomplete."

  # Diagnose Ignite is installed only after successful payload extraction.
  mkdir -p "$DOCS" 2>/dev/null || true
  if [ -f "$PACKAGE_ROOT/Diagnose-Ignite.sh" ]; then
    cp -f "$PACKAGE_ROOT/Diagnose-Ignite.sh" "$DOCS/Diagnose Ignite.sh" || true
    chmod 0755 "$DOCS/Diagnose Ignite.sh" 2>/dev/null || true
    sed -i 's/\r$//' "$DOCS/Diagnose Ignite.sh" 2>/dev/null || true
  fi

  sh "$PACKAGE_ROOT/lib/install-runtime.sh" "$PACKAGE_ROOT"

  cleanup_stage
  sync

  # Only after install-runtime succeeded (set -e).
  case "$SELF" in
    "$DOCS/Install Ignite v@@IGNITE_PACKAGE_VERSION@@.sh"|"$DOCS/Install Ignite.sh"|"$DOCS/Install-Ignite.sh")
      rm -f "$SELF" || true
      echo "removed installer from Library"
      ;;
  esac

  echo "SUCCESS. Open Ignite from the Library."
) >>"$LOG" 2>&1
STATUS=$?
set -e

if [ "$STATUS" -ne 0 ]; then
  echo "FAILED. Install Ignite was not removed. Copy Ignite-Diagnose from Documents over USB." >>"$LOG" 2>/dev/null || true
  write_usb_fail_report
  cleanup_stage
fi

tail -n 40 "$LOG" 2>/dev/null || true
exit "$STATUS"
