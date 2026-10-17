#!/bin/sh
# Install or upgrade Ignite release-owned runtime files (full-feature open-source).
# Expects PACKAGE_ROOT=.../Ignite-Kindle-<version>.
# Never touches /mnt/us/scribe-data contents; never deletes logs/state/appreg-backups.
# Host tests may set IGNITE_* overrides; unset on device.
set -eu

PACKAGE_ROOT="${1:-}"
if [ -z "$PACKAGE_ROOT" ]; then
  echo "usage: install-runtime.sh /mnt/us/Ignite-Kindle-<version>" >&2
  exit 2
fi

APP_ROOT="${IGNITE_APP_ROOT:-/mnt/us/scribe-app}"
DATA_DIR="${IGNITE_DATA_DIR:-/mnt/us/scribe-data}"
DOCS="${IGNITE_DOCS:-/mnt/us/documents}"
US_ROOT="${IGNITE_US_ROOT:-/mnt/us}"
STABLE_APP=$APP_ROOT/bin/scribe-app
STABLE_LIFE=$APP_ROOT/bin/scribe-lifecycle
STABLE_HOST=$APP_ROOT/bin/scribe-lifecycle-host.sh
SCRIPTLET=$DOCS/Scribe.sh
# Canonical appreg identity (sourced helpers also set HANDLER / LEGACY_HANDLER).
HANDLER=com.markrai.ignite
EXPECTED_COMMAND=$STABLE_HOST
PREV_DIR=$APP_ROOT/previous-release
PREV_OLD=$APP_ROOT/previous-release.prev
PREV_STAGING=$APP_ROOT/previous-release.staging
SU_BIN="${IGNITE_SU:-/var/local/kmc/bin/su}"
TOOLS_LIB="$APP_ROOT/tools/ignite-release/lib"
JOURNAL=$APP_ROOT/run/ignite-activate.journal
COMMITTED=$APP_ROOT/run/ignite-activate.committed

LIB_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=register-helpers.sh
. "$LIB_DIR/register-helpers.sh"
# shellcheck source=install-recovery.sh
. "$LIB_DIR/install-recovery.sh"

log() { printf '%s\n' "$*"; }
die() { printf 'install_failed: %s\n' "$*" >&2; exit 1; }

# Packaged binaries are armv7 32-bit little-endian ELF (EM_ARM=40).
ignite_require_arm32le_elf() {
  bin=$1
  [ -f "$bin" ] && [ -s "$bin" ] || return 1
  mag=$(dd if="$bin" bs=1 count=6 2>/dev/null | od -An -t x1 | tr -d ' \n\t')
  mag=$(printf '%s' "$mag" | tr 'A-F' 'a-f')
  [ "$mag" = "7f454c460101" ] || return 1
  mach=$(dd if="$bin" bs=1 skip=18 count=2 2>/dev/null | od -An -t x1 | tr -d ' \n\t')
  mach=$(printf '%s' "$mach" | tr 'A-F' 'a-f')
  [ "$mach" = "2800" ] || return 1
  return 0
}

ignite_require_free_kb() {
  need=$1
  if [ -n "${IGNITE_AVAIL_KB:-}" ]; then
    avail=$IGNITE_AVAIL_KB
  else
    avail=$(df -k "$US_ROOT" 2>/dev/null | awk 'NR==2 {print $4}')
  fi
  [ -n "$avail" ] || die "storage: cannot measure free space on $US_ROOT"
  [ "$avail" -ge "$need" ] || die "storage: not enough free space (need ${need} KB, have ${avail} KB). Delete unused Kindle files and try Install Ignite again."
}

cleanup_staged_new() {
  rm -f "$STABLE_APP.new" "$STABLE_LIFE.new" "$STABLE_HOST.new" "$SCRIPTLET.new" \
    "$APP_ROOT/assets/scribe-icon.png.new" \
    "$APP_ROOT/assets/licenses/noto-emoji/OFL.txt.new" \
    "$APP_ROOT/assets/licenses/noto-emoji/DESCRIPTION.en_us.html.new" \
    "$APP_ROOT/assets/licenses/noto-emoji/upstream_info.md.new" \
    "$APP_ROOT/assets/licenses/source-sans-3/OFL.txt.new" \
    "$DOCS/Uninstall Ignite.sh.new" "$DOCS/Backup Ignite Data.sh.new" \
    "$DOCS/Diagnose Ignite.sh.new"
  rm -f "$TOOLS_LIB/"*.new 2>/dev/null || true
}

# Explicit status: callers invoke this from `||` / `if` which disable set -e.
restore_cp() {
  src=$1
  dest=$2
  name=$3
  if [ "${IGNITE_FAIL_RESTORE_CP:-}" = "$name" ]; then
    log "install_rollback: injected restore cp failure at $name"
    cp -f "$src" "$dest/.ignite-restore-cp-fail" || return 1
    return 1
  fi
  cp -f "$src" "$dest" || return 1
}

restore_cp_optional() {
  src=$1
  dest=$2
  name=$3
  if [ ! -f "$src" ]; then
    return 0
  fi
  restore_cp "$src" "$dest" "$name"
}

publish_crash() {
  point=$1
  if [ "${IGNITE_FAIL_PUBLISH:-}" = "$point" ]; then
    log "install_crash: injected at $point"
    exit 99
  fi
}

load_handler_snap_from_journal() {
  if [ ! -f "$JOURNAL" ]; then
    return 0
  fi
  _snap=$(grep '^# handler_snap=' "$JOURNAL" 2>/dev/null | tail -n 1 | sed 's/^# handler_snap=//') || true
  _txn=$(grep '^# handler_txn=' "$JOURNAL" 2>/dev/null | tail -n 1 | sed 's/^# handler_txn=//') || true
  if [ -n "$_snap" ] && [ -f "$_snap" ]; then
    IGNITE_HANDLER_SNAP=$_snap
    IGNITE_APPREG_TXN=$_txn
    export IGNITE_HANDLER_SNAP IGNITE_APPREG_TXN
  fi
}

journal_had_previous() {
  _had=""
  if [ -f "$JOURNAL" ]; then
    _had=$(grep '^# had_previous=' "$JOURNAL" 2>/dev/null | tail -n 1 | sed 's/^# had_previous=//') || true
  fi
  if [ -n "$_had" ]; then
    echo "$_had"
    return 0
  fi
  if [ -f "$PREV_OLD/scribe-app" ] || [ -f "$PREV_DIR/scribe-app" ]; then
    echo "1"
  else
    echo "0"
  fi
}

cleanup_committed_leftovers() {
  rm -f "$JOURNAL" "$JOURNAL.tmp" 2>/dev/null || true
  rm -rf "$PREV_STAGING" || true
  rm -rf "$PREV_OLD" || true
  rm -f "$COMMITTED" "$COMMITTED.tmp" 2>/dev/null || true
  return 0
}

# PENDING: restore version A (old runtime + matching handler snapshot).
# COMMITTED leftover cleanup must never call this.
restore_pending_transaction() {
  if [ "${IGNITE_FAIL_RECOVERY:-}" = "1" ]; then
    log "install_recover: injected recovery failure"
    return 1
  fi
  had=$(journal_had_previous)
  load_handler_snap_from_journal
  rm -rf "$PREV_STAGING" || true
  if [ "$had" = "0" ]; then
    log "install_recover: pending first-install; discarding incomplete runtime"
    rm -rf "$PREV_DIR" "$PREV_OLD" || true
  else
    log "install_recover: pending upgrade; restoring previous runtime A"
    if [ -f "$PREV_OLD/scribe-app" ]; then
      rm -rf "$PREV_DIR"
      mv "$PREV_OLD" "$PREV_DIR" || return 1
    fi
  fi
  rollback_runtime || return 1
  rm -f "$JOURNAL"
  return 0
}

recover_install_transaction() {
  if [ -f "$COMMITTED" ]; then
    log "install_recover: committed transaction leftover cleanup (keep installed runtime)"
    cleanup_committed_leftovers
    return 0
  fi
  if [ -f "$JOURNAL" ]; then
    log "install_recover: pending transaction; rolling back to previous complete version"
    restore_pending_transaction || return 1
    return 0
  fi
  rm -rf "$PREV_STAGING" || true
  if [ ! -f "$PREV_DIR/scribe-app" ] && [ -f "$PREV_OLD/scribe-app" ]; then
    log "install_recover: restoring recovery base from .prev (no pending journal)"
    rm -rf "$PREV_DIR"
    mv "$PREV_OLD" "$PREV_DIR" || return 1
  fi
  return 0
}

snapshot_copy_optional() {
  src=$1
  dest=$2
  if [ ! -f "$src" ]; then
    return 0
  fi
  cp -f "$src" "$dest" || return 1
}

# Fill staging from live files, then publish as previous-release.
# Does NOT delete .prev: that remains the old recovery base until durable commit.
snapshot_previous_release() {
  rm -rf "$PREV_STAGING"
  mkdir -p "$PREV_STAGING" || return 1
  cp -f "$STABLE_APP" "$PREV_STAGING/scribe-app" || return 1
  cp -f "$STABLE_LIFE" "$PREV_STAGING/scribe-lifecycle" || return 1
  cp -f "$STABLE_HOST" "$PREV_STAGING/scribe-lifecycle-host.sh" || return 1
  snapshot_copy_optional "$SCRIPTLET" "$PREV_STAGING/Scribe.sh" || return 1
  snapshot_copy_optional "$APP_ROOT/assets/scribe-icon.png" "$PREV_STAGING/scribe-icon.png" || return 1
  snapshot_copy_optional "$APP_ROOT/assets/licenses/noto-emoji/OFL.txt" "$PREV_STAGING/OFL.txt" || return 1
  snapshot_copy_optional "$APP_ROOT/assets/licenses/noto-emoji/DESCRIPTION.en_us.html" \
    "$PREV_STAGING/DESCRIPTION.en_us.html" || return 1
  snapshot_copy_optional "$APP_ROOT/assets/licenses/source-sans-3/OFL.txt" \
    "$PREV_STAGING/source-sans-3-OFL.txt" || return 1
  for helper in install-runtime.sh uninstall-runtime.sh backup-runtime.sh register-helpers.sh; do
    snapshot_copy_optional "$TOOLS_LIB/$helper" "$PREV_STAGING/$helper" || return 1
  done
  snapshot_copy_optional "$DOCS/Uninstall Ignite.sh" "$PREV_STAGING/Uninstall Ignite.sh" || return 1
  snapshot_copy_optional "$DOCS/Backup Ignite Data.sh" "$PREV_STAGING/Backup Ignite Data.sh" || return 1
  snapshot_copy_optional "$DOCS/Diagnose Ignite.sh" "$PREV_STAGING/Diagnose Ignite.sh" || return 1
  sync
  publish_crash before-prev-move
  if [ -d "$PREV_DIR" ]; then
    mv "$PREV_DIR" "$PREV_OLD" || return 1
  fi
  publish_crash after-prev-move
  publish_crash before-staging-publish
  if ! mv "$PREV_STAGING" "$PREV_DIR"; then
    if [ -d "$PREV_OLD" ]; then
      mv "$PREV_OLD" "$PREV_DIR" || true
    fi
    return 1
  fi
  sync
  publish_crash after-staging-publish
  return 0
}

commit_install_transaction() {
  mkdir -p "$APP_ROOT/run" || return 1
  printf 'committed txn=%s\n' "${ACTIVATE_TXN:-unknown}" >"$COMMITTED.tmp" || return 1
  sync
  mv -f "$COMMITTED.tmp" "$COMMITTED" || return 1
  sync
  return 0
}

rollback_runtime() {
  if [ "${IGNITE_FAIL_ROLLBACK:-}" = "1" ]; then
    log "install_rollback: injected rollback failure"
    return 1
  fi
  log "install_rollback: restoring previous release-owned files"
  if [ -f "$PREV_DIR/scribe-app" ]; then
    mkdir -p "$APP_ROOT/bin" "$APP_ROOT/assets/licenses/noto-emoji" \
      "$APP_ROOT/assets/licenses/source-sans-3" "$TOOLS_LIB" "$DOCS" || return 1
    restore_cp "$PREV_DIR/scribe-app" "$STABLE_APP" scribe-app || return 1
    restore_cp "$PREV_DIR/scribe-lifecycle" "$STABLE_LIFE" scribe-lifecycle || return 1
    restore_cp "$PREV_DIR/scribe-lifecycle-host.sh" "$STABLE_HOST" host || return 1
    restore_cp_optional "$PREV_DIR/Scribe.sh" "$SCRIPTLET" scriptlet || return 1
    restore_cp_optional "$PREV_DIR/scribe-icon.png" "$APP_ROOT/assets/scribe-icon.png" icon || return 1
    restore_cp_optional "$PREV_DIR/OFL.txt" "$APP_ROOT/assets/licenses/noto-emoji/OFL.txt" ofl || return 1
    restore_cp_optional "$PREV_DIR/DESCRIPTION.en_us.html" \
      "$APP_ROOT/assets/licenses/noto-emoji/DESCRIPTION.en_us.html" description || return 1
    restore_cp_optional "$PREV_DIR/source-sans-3-OFL.txt" \
      "$APP_ROOT/assets/licenses/source-sans-3/OFL.txt" source-sans-ofl || return 1
    for helper in install-runtime.sh uninstall-runtime.sh backup-runtime.sh register-helpers.sh; do
      restore_cp_optional "$PREV_DIR/$helper" "$TOOLS_LIB/$helper" "helper-$helper" || return 1
    done
    restore_cp_optional "$PREV_DIR/Uninstall Ignite.sh" "$DOCS/Uninstall Ignite.sh" uninstall-docs || return 1
    restore_cp_optional "$PREV_DIR/Backup Ignite Data.sh" "$DOCS/Backup Ignite Data.sh" backup-docs || return 1
    restore_cp_optional "$PREV_DIR/Diagnose Ignite.sh" "$DOCS/Diagnose Ignite.sh" diagnose-docs || return 1
  else
    log "install_rollback: no previous snapshot; removing incomplete first install"
    rm -f "$STABLE_APP" "$STABLE_LIFE" "$STABLE_HOST" "$SCRIPTLET" || return 1
    rm -f "$DOCS/Uninstall Ignite.sh" "$DOCS/Backup Ignite Data.sh" || return 1
    # Diagnose Ignite.sh is installed after successful payload extraction (SFX).
    # Keep it so USB diagnose remains available after a failed first runtime install.
  fi
  cleanup_staged_new
  if [ -n "${IGNITE_HANDLER_SNAP:-}" ] && [ -f "${IGNITE_HANDLER_SNAP}" ]; then
    if ! ignite_restore_handler "$IGNITE_HANDLER_SNAP" "${IGNITE_APPREG_TXN:-}"; then
      log "install_rollback: handler restore failed"
      return 1
    fi
  fi
  return 0
}

fail_and_rollback() {
  reason="$*"
  rb=0
  restore_pending_transaction || rb=$?
  if [ "$rb" -ne 0 ]; then
    printf 'install_failed: rollback incomplete (status=%s): %s\n' "$rb" "$reason" >&2
    exit 2
  fi
  printf 'install_failed: %s\n' "$reason" >&2
  exit 1
}

# Named activation: mv staged .new into place, then optional IGNITE_FAIL_AT fault.
# Journal records completed steps so a later install can recover a mixed tree.
activate_mv() {
  name=$1
  src=$2
  dest=$3
  [ -f "$src" ] || fail_and_rollback "missing staged file for $name"
  mv -f "$src" "$dest" || fail_and_rollback "activate mv failed at $name"
  echo "$name" >>"$JOURNAL" || fail_and_rollback "activation journal write failed at $name"
  if [ "${IGNITE_FAIL_AT:-}" = "$name" ]; then
    fail_and_rollback "injected activation fault at $name"
  fi
}

activate_mv_optional() {
  name=$1
  src=$2
  dest=$3
  if [ ! -f "$src" ]; then
    return 0
  fi
  activate_mv "$name" "$src" "$dest"
}

# --- package tree validation ---
[ -d "$PACKAGE_ROOT" ] || die "package directory not found: $PACKAGE_ROOT"
[ -f "$PACKAGE_ROOT/bin/scribe-app" ] || die "missing $PACKAGE_ROOT/bin/scribe-app"
[ -s "$PACKAGE_ROOT/bin/scribe-app" ] || die "empty $PACKAGE_ROOT/bin/scribe-app"
[ -f "$PACKAGE_ROOT/bin/scribe-lifecycle" ] || die "missing $PACKAGE_ROOT/bin/scribe-lifecycle"
[ -s "$PACKAGE_ROOT/bin/scribe-lifecycle" ] || die "empty $PACKAGE_ROOT/bin/scribe-lifecycle"
[ -f "$PACKAGE_ROOT/host/scribe-lifecycle-host.sh" ] || die "missing lifecycle host in package"
[ -f "$PACKAGE_ROOT/launcher/Scribe.sh" ] || die "missing launcher/Scribe.sh"
[ -f "$PACKAGE_ROOT/registration/ignite.json" ] || die "missing registration/ignite.json"
[ -f "$PACKAGE_ROOT/assets/scribe-icon.png" ] || die "missing assets/scribe-icon.png"
[ -f "$PACKAGE_ROOT/assets/licenses/noto-emoji/OFL.txt" ] || die "missing Noto Emoji OFL"
[ -f "$PACKAGE_ROOT/assets/licenses/source-sans-3/OFL.txt" ] || die "missing Source Sans 3 OFL"
[ -f "$PACKAGE_ROOT/Diagnose-Ignite.sh" ] || die "missing Diagnose-Ignite.sh"
[ -f "$PACKAGE_ROOT/recovery/bin/dropbearmulti" ] || die "missing recovery/bin/dropbearmulti"
[ -f "$PACKAGE_ROOT/recovery/manifest/elf.sha256" ] || die "missing recovery/manifest/elf.sha256"
[ -f "$PACKAGE_ROOT/lib/install-recovery.sh" ] || die "missing lib/install-recovery.sh"

# Full-feature host policy (ignore comment lines): sync capability required; optional to use.
_host_active=$(grep -v '^[[:space:]]*#' "$PACKAGE_ROOT/host/scribe-lifecycle-host.sh" || true)
printf '%s\n' "$_host_active" | grep -q -- '--confirm-write' \
  || die "packaged host missing --confirm-write"
printf '%s\n' "$_host_active" | grep -q -- '/mnt/us/scribe-data' \
  || die "packaged host missing /mnt/us/scribe-data"
printf '%s\n' "$_host_active" | grep -q -- '--sync-dir' \
  || die "packaged host missing --sync-dir"
printf '%s\n' "$_host_active" | grep -q -- '/mnt/us/scribe-app/state/sync' \
  || die "packaged host missing sync-dir path"
printf '%s\n' "$_host_active" | grep -q -- '--device-dir' \
  || die "packaged host missing --device-dir"
printf '%s\n' "$_host_active" | grep -q -- '/mnt/us/scribe-app/state/device' \
  || die "packaged host missing device-dir path"

if [ "${IGNITE_SKIP_SU_CHECK:-}" != "1" ]; then
  [ -x "$SU_BIN" ] || die "preparation: missing the root helper at $SU_BIN. This Kindle is not prepared for third-party apps. Ignite cannot jailbreak or prepare it."
fi

# --- SH Integration prerequisite ---
if [ "${IGNITE_SKIP_SH_CHECK:-}" != "1" ]; then
  if [ ! -x /usr/bin/sqlite3 ] || [ ! -r "$APPREG" ]; then
    die "preparation: cannot verify Kindle SH Integration (sqlite3 or appreg missing)"
  fi
  sh_associations=$(
    /usr/bin/sqlite3 -readonly "$APPREG" \
      "SELECT COUNT(*) FROM associations WHERE (interface='application' AND handlerId='tech.hackerdude.shell_integration.launcher' AND contentId='MT:text/x-shellscript') OR (interface='extractor' AND handlerId='tech.hackerdude.shell_integration.extractor' AND contentId='GL:*.sh');"
  )
  [ "$sh_associations" = "2" ] || die "preparation: Kindle SH Integration is not registered. Follow the Kindle preparation instructions, then retry. Ignite cannot install that software for you."
fi

# Fail-fast sqlite is required before any registration or recovery SQL.
if [ "${IGNITE_SKIP_REGISTER:-}" != "1" ]; then
  ignite_sqlite_require_bail || die "preparation: sqlite3 on this Kindle does not honor fail-fast SQL (.bail on). Ignite will not change app registration."
fi

if [ "${IGNITE_SKIP_ELF_CHECK:-}" != "1" ]; then
  ignite_require_arm32le_elf "$PACKAGE_ROOT/bin/scribe-app" \
    || die "unsupported: Install Ignite is built for 32-bit ARM Kindles. This device cannot run these programs."
  ignite_require_arm32le_elf "$PACKAGE_ROOT/bin/scribe-lifecycle" \
    || die "unsupported: Install Ignite is built for 32-bit ARM Kindles. This device cannot run these programs."
fi

if [ "${IGNITE_SKIP_SPACE_CHECK:-}" != "1" ]; then
  _app_b=$(wc -c < "$PACKAGE_ROOT/bin/scribe-app" | tr -d ' ')
  _life_b=$(wc -c < "$PACKAGE_ROOT/bin/scribe-lifecycle" | tr -d ' ')
  _rec_b=0
  if [ -f "$PACKAGE_ROOT/recovery/bin/dropbearmulti" ]; then
    _rec_b=$(wc -c < "$PACKAGE_ROOT/recovery/bin/dropbearmulti" | tr -d ' ')
  fi
  _need_kb=$(( (_app_b + _life_b) * 3 / 1024 + _rec_b / 1024 + 8192 ))
  ignite_require_free_kb "$_need_kb"
fi

# --- refuse while running ---
if [ "${IGNITE_SKIP_PROC_CHECK:-}" != "1" ]; then
  for process_dir in /proc/[0-9]*; do
    process_pid=${process_dir##*/}
    process_exe=$(readlink -f "$process_dir/exe" 2>/dev/null || true)
    case "$process_exe" in
      "$STABLE_APP"|"$STABLE_LIFE"|"$US_ROOT/scribe-app")
        die "running: Ignite is open (PID $process_pid). Go to the Kindle Home screen, wait a few seconds, then open Install Ignite again."
        ;;
    esac
  done
fi

# --- scribe-data snapshot (installer must not mutate) ---
DATA_EXISTED=0
DATA_COUNT=""
if [ -e "$DATA_DIR" ]; then
  DATA_EXISTED=1
  if [ -d "$DATA_DIR" ]; then
    DATA_COUNT=$(find "$DATA_DIR" 2>/dev/null | wc -l | tr -d ' ')
  else
    DATA_COUNT="not-dir"
  fi
fi

# --- prepare release-owned directories only (never wipe logs/state/appreg-backups) ---
mkdir -p "$APP_ROOT/bin" \
  "$APP_ROOT/assets" \
  "$APP_ROOT/assets/licenses/noto-emoji" \
  "$APP_ROOT/assets/licenses/source-sans-3" \
  "$APP_ROOT/logs" \
  "$APP_ROOT/run" \
  "$TOOLS_LIB" \
  "$DOCS"

recover_install_transaction || die "interrupted: could not finish recovery from a previous incomplete Install Ignite. Notebooks were not deleted. Copy Ignite-Diagnose from Documents over USB."

cleanup_staged_new

# Snapshot last successful live files only if previous-release is empty.
if [ ! -f "$PREV_DIR/scribe-app" ] && [ -f "$STABLE_APP" ]; then
  snapshot_previous_release || die "could not snapshot current release for rollback"
fi

# --- stage *.new (release-owned files only) ---
cp -f "$PACKAGE_ROOT/bin/scribe-app" "$STABLE_APP.new"
cp -f "$PACKAGE_ROOT/bin/scribe-lifecycle" "$STABLE_LIFE.new"
cp -f "$PACKAGE_ROOT/host/scribe-lifecycle-host.sh" "$STABLE_HOST.new"
cp -f "$PACKAGE_ROOT/assets/scribe-icon.png" "$APP_ROOT/assets/scribe-icon.png.new"
cp -f "$PACKAGE_ROOT/launcher/Scribe.sh" "$SCRIPTLET.new"
cp -f "$PACKAGE_ROOT/assets/licenses/noto-emoji/OFL.txt" \
  "$APP_ROOT/assets/licenses/noto-emoji/OFL.txt.new"
cp -f "$PACKAGE_ROOT/assets/licenses/source-sans-3/OFL.txt" \
  "$APP_ROOT/assets/licenses/source-sans-3/OFL.txt.new"
if [ -f "$PACKAGE_ROOT/assets/licenses/noto-emoji/DESCRIPTION.en_us.html" ]; then
  cp -f "$PACKAGE_ROOT/assets/licenses/noto-emoji/DESCRIPTION.en_us.html" \
    "$APP_ROOT/assets/licenses/noto-emoji/DESCRIPTION.en_us.html.new"
fi
if [ -f "$PACKAGE_ROOT/assets/licenses/noto-emoji/upstream_info.md" ]; then
  cp -f "$PACKAGE_ROOT/assets/licenses/noto-emoji/upstream_info.md" \
    "$APP_ROOT/assets/licenses/noto-emoji/upstream_info.md.new"
fi

for helper in install-runtime.sh uninstall-runtime.sh backup-runtime.sh register-helpers.sh install-recovery.sh; do
  cp -f "$PACKAGE_ROOT/lib/$helper" "$TOOLS_LIB/$helper.new"
done
cp -f "$PACKAGE_ROOT/Uninstall-Ignite.sh" "$DOCS/Uninstall Ignite.sh.new"
cp -f "$PACKAGE_ROOT/Backup-Ignite-Data.sh" "$DOCS/Backup Ignite Data.sh.new"
cp -f "$PACKAGE_ROOT/Diagnose-Ignite.sh" "$DOCS/Diagnose Ignite.sh.new"

chmod 0755 "$STABLE_APP.new" "$STABLE_LIFE.new" "$STABLE_HOST.new" "$SCRIPTLET.new"
chmod 0755 "$TOOLS_LIB/"*.new
chmod 0755 "$DOCS/"*.new 2>/dev/null || true
chmod 0644 "$APP_ROOT/assets/scribe-icon.png.new"
chmod 0644 "$APP_ROOT/assets/licenses/noto-emoji/OFL.txt.new"
chmod 0644 "$APP_ROOT/assets/licenses/source-sans-3/OFL.txt.new"
sed -i 's/\r$//' "$STABLE_HOST.new" "$SCRIPTLET.new" "$TOOLS_LIB/"*.new "$DOCS/"*.new 2>/dev/null || true

_staged_host=$(grep -v '^[[:space:]]*#' "$STABLE_HOST.new" || true)
printf '%s\n' "$_staged_host" | grep -q -- '--sync-dir' \
  || die "staged host missing --sync-dir"
printf '%s\n' "$_staged_host" | grep -q -- '--device-dir' \
  || die "staged host missing --device-dir"
printf '%s\n' "$_staged_host" | grep -q -- '--confirm-write' \
  || die "staged host missing --confirm-write"

if [ "${IGNITE_SKIP_BIN_SMOKE:-}" != "1" ] && [ -x "$STABLE_LIFE.new" ]; then
  "$STABLE_LIFE.new" --help >/dev/null 2>&1 || true
  # ARM binary may not run on the packager host; on-device a missing interpreter fails later.
fi

# --- commit (rename into place), journaled ---
# Activation boundary names (IGNITE_FAIL_AT): scribe-app, scribe-lifecycle, host,
# icon, scriptlet, ofl, source-sans-ofl, description, upstream_info,
# helper-install-runtime, helper-uninstall-runtime, helper-backup-runtime,
# helper-register-helpers, helper-install-recovery, uninstall-docs, backup-docs,
# diagnose-docs
ACTIVATE_TXN=$(date -u +%Y%m%dT%H%M%SZ)-$$
if [ -f "$PREV_DIR/scribe-app" ]; then
  HAD_PREVIOUS=1
else
  HAD_PREVIOUS=0
fi
{
  echo "# ignite-activate v1"
  echo "# txn=$ACTIVATE_TXN"
  echo "# package=$PACKAGE_ROOT"
  echo "# phase=pending"
  echo "# had_previous=$HAD_PREVIOUS"
} >"$JOURNAL"

activate_mv scribe-app "$STABLE_APP.new" "$STABLE_APP"
activate_mv scribe-lifecycle "$STABLE_LIFE.new" "$STABLE_LIFE"
activate_mv host "$STABLE_HOST.new" "$STABLE_HOST"
activate_mv icon "$APP_ROOT/assets/scribe-icon.png.new" "$APP_ROOT/assets/scribe-icon.png"
activate_mv scriptlet "$SCRIPTLET.new" "$SCRIPTLET"
activate_mv ofl "$APP_ROOT/assets/licenses/noto-emoji/OFL.txt.new" \
  "$APP_ROOT/assets/licenses/noto-emoji/OFL.txt"
activate_mv source-sans-ofl "$APP_ROOT/assets/licenses/source-sans-3/OFL.txt.new" \
  "$APP_ROOT/assets/licenses/source-sans-3/OFL.txt"
activate_mv_optional description \
  "$APP_ROOT/assets/licenses/noto-emoji/DESCRIPTION.en_us.html.new" \
  "$APP_ROOT/assets/licenses/noto-emoji/DESCRIPTION.en_us.html"
activate_mv_optional upstream_info \
  "$APP_ROOT/assets/licenses/noto-emoji/upstream_info.md.new" \
  "$APP_ROOT/assets/licenses/noto-emoji/upstream_info.md"
activate_mv helper-install-runtime "$TOOLS_LIB/install-runtime.sh.new" "$TOOLS_LIB/install-runtime.sh"
activate_mv helper-uninstall-runtime "$TOOLS_LIB/uninstall-runtime.sh.new" "$TOOLS_LIB/uninstall-runtime.sh"
activate_mv helper-backup-runtime "$TOOLS_LIB/backup-runtime.sh.new" "$TOOLS_LIB/backup-runtime.sh"
activate_mv helper-register-helpers "$TOOLS_LIB/register-helpers.sh.new" "$TOOLS_LIB/register-helpers.sh"
activate_mv helper-install-recovery "$TOOLS_LIB/install-recovery.sh.new" "$TOOLS_LIB/install-recovery.sh"
activate_mv uninstall-docs "$DOCS/Uninstall Ignite.sh.new" "$DOCS/Uninstall Ignite.sh"
activate_mv backup-docs "$DOCS/Backup Ignite Data.sh.new" "$DOCS/Backup Ignite Data.sh"
activate_mv diagnose-docs "$DOCS/Diagnose Ignite.sh.new" "$DOCS/Diagnose Ignite.sh"
sync

# Keep the journal until register + verify + previous-release publish complete.
IGNITE_ACTIVATE_JOURNAL=$JOURNAL
export IGNITE_ACTIVATE_JOURNAL

# --- register + legacy migrate (Ignite-scoped; never whole-db restore) ---
# Fresh installs register com.markrai.ignite. Upgrades from com.markrai.scribe
# register the new handler first, verify, then retire the legacy row.
if [ "${IGNITE_SKIP_REGISTER:-}" != "1" ]; then
  if ! ignite_register_and_migrate "$PACKAGE_ROOT/registration/ignite.json"; then
    fail_and_rollback "app registration failed"
  fi
fi

# --- post-install verification ---
verify_fail() { fail_and_rollback "post-install verification failed: $*"; }
if [ "${IGNITE_FORCE_VERIFY_FAIL:-}" = "1" ]; then
  verify_fail "injected"
fi

[ -f "$STABLE_APP" ] && [ -s "$STABLE_APP" ] || verify_fail "scribe-app missing or empty"
[ -f "$STABLE_LIFE" ] && [ -s "$STABLE_LIFE" ] || verify_fail "scribe-lifecycle missing or empty"
[ -f "$STABLE_HOST" ] && [ -s "$STABLE_HOST" ] || verify_fail "lifecycle host missing or empty"
grep -v '^[[:space:]]*#' "$STABLE_HOST" | grep -q -- '--confirm-write' || verify_fail "installed host missing --confirm-write"
grep -v '^[[:space:]]*#' "$STABLE_HOST" | grep -q -- '/mnt/us/scribe-data' || verify_fail "installed host missing data-dir"
grep -v '^[[:space:]]*#' "$STABLE_HOST" | grep -q -- '--sync-dir' || verify_fail "installed host missing --sync-dir"
grep -v '^[[:space:]]*#' "$STABLE_HOST" | grep -q -- '/mnt/us/scribe-app/state/sync' || verify_fail "installed host missing sync-dir path"
grep -v '^[[:space:]]*#' "$STABLE_HOST" | grep -q -- '--device-dir' || verify_fail "installed host missing --device-dir"
grep -v '^[[:space:]]*#' "$STABLE_HOST" | grep -q -- '/mnt/us/scribe-app/state/device' || verify_fail "installed host missing device-dir path"
[ -f "$SCRIPTLET" ] || verify_fail "discovery Scriptlet missing at $SCRIPTLET"
[ -f "$TOOLS_LIB/uninstall-runtime.sh" ] || verify_fail "uninstall helper missing"
[ -f "$TOOLS_LIB/backup-runtime.sh" ] || verify_fail "backup helper missing"
[ -f "$DOCS/Backup Ignite Data.sh" ] || verify_fail "Backup Ignite Data missing"
[ -f "$DOCS/Uninstall Ignite.sh" ] || verify_fail "Uninstall Ignite missing"
[ -f "$DOCS/Diagnose Ignite.sh" ] || verify_fail "Diagnose Ignite missing"
if [ "${IGNITE_SKIP_REGISTER:-}" != "1" ]; then
  ignite_handler_registered || verify_fail "handler $HANDLER not registered"
  CMD=$(ignite_handler_command)
  [ "$CMD" = "$EXPECTED_COMMAND" ] || verify_fail "handler command is '$CMD' (expected $EXPECTED_COMMAND)"
fi

if [ "$DATA_EXISTED" -eq 0 ]; then
  [ ! -e "$DATA_DIR" ] || verify_fail "installer created $DATA_DIR (must not recreate notebook root)"
else
  [ -e "$DATA_DIR" ] || verify_fail "installer removed $DATA_DIR"
  if [ -d "$DATA_DIR" ] && [ "$DATA_COUNT" != "not-dir" ]; then
    AFTER_COUNT=$(find "$DATA_DIR" 2>/dev/null | wc -l | tr -d ' ')
    [ "$AFTER_COUNT" = "$DATA_COUNT" ] || verify_fail "installer changed $DATA_DIR entry count ($DATA_COUNT -> $AFTER_COUNT)"
  fi
fi

# Publish new recovery base B; keep A in .prev until durable commit.
snapshot_previous_release || fail_and_rollback "previous-release snapshot failed"
publish_crash before-commit
commit_install_transaction || fail_and_rollback "durable commit marker failed"
publish_crash after-commit
publish_crash before-journal-cleanup
rm -f "$JOURNAL"
publish_crash before-prev-cleanup
rm -rf "$PREV_OLD"
rm -f "$COMMITTED" "$COMMITTED.tmp"

if ! ignite_install_recovery "$PACKAGE_ROOT"; then
  die "recovery component failed after application files were installed. Notebooks were not deleted. Recovery executable was not published. Copy Ignite-Diagnose from Documents."
fi

log "install_ok profile=full-feature package=$PACKAGE_ROOT recovery=installed spawned=no"
log "notebook data path untouched: $DATA_DIR"
log "Open Ignite from the Kindle Library to launch."
exit 0
