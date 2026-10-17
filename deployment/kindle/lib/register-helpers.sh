#!/bin/sh
# Appreg register/unregister helpers for Ignite external release.
# Canonical handlerId: com.markrai.ignite
# Legacy (pre-0.9.11) handlerId: com.markrai.scribe — retired after verified migration.
# Touches ONLY Ignite-owned handler rows (canonical and/or legacy).
# Never copies appreg.db (or a whole-db backup) over the live registry.
# Concurrent writers: PRAGMA busy_timeout + BEGIN IMMEDIATE.
# Multi-statement scripts always use .bail on so a SQL error cannot COMMIT.
# Never touches /mnt/us/scribe-data.
# shellcheck shell=sh

APPREG="${IGNITE_APPREG:-/var/local/appreg.db}"
BACKUP_DIR="${IGNITE_APP_ROOT:-/mnt/us/scribe-app}/appreg-backups"
HANDLER=com.markrai.ignite
LEGACY_HANDLER=com.markrai.scribe
EXPECTED_COMMAND="${IGNITE_APP_ROOT:-/mnt/us/scribe-app}/bin/scribe-lifecycle-host.sh"
SQLITE3="${IGNITE_SQLITE3:-}"
if [ -z "$SQLITE3" ]; then
  if [ -x /usr/bin/sqlite3 ]; then
    SQLITE3=/usr/bin/sqlite3
  else
    SQLITE3=sqlite3
  fi
fi

ignite_sql() {
  "$SQLITE3" "$APPREG" "$@"
  return $?
}

# Fail closed unless the CLI honors .bail on (error stops later statements, including COMMIT).
ignite_sqlite_require_bail() {
  if [ "${IGNITE_SQLITE_BAIL_OK:-}" = "1" ]; then
    return 0
  fi
  _probe_out=$(
    printf '%s\n' ".bail on" "SELECT ignite_bail_probe();" "SELECT 'BAIL_NOT_HONORED';" \
      | "$SQLITE3" ":memory:" 2>/dev/null || true
  )
  case "$_probe_out" in
    *BAIL_NOT_HONORED*)
      echo "sqlite CLI does not honor .bail on; refusing appreg writes" >&2
      return 1
      ;;
  esac
  if printf '%s\n' ".bail on" "SELECT ignite_bail_probe();" | "$SQLITE3" ":memory:" >/dev/null 2>&1; then
    echo "sqlite CLI .bail on did not fail closed; refusing appreg writes" >&2
    return 1
  fi
  IGNITE_SQLITE_BAIL_OK=1
  export IGNITE_SQLITE_BAIL_OK
  return 0
}

# Stdin: SQL body. Prepends .bail on + busy_timeout. Propagates sqlite exit status.
ignite_sql_script() {
  ignite_sqlite_require_bail || return 1
  {
    echo ".bail on"
    echo "PRAGMA busy_timeout=8000;"
    cat
  } | "$SQLITE3" "$APPREG"
  return $?
}

# Forensic whole-db copy only. NEVER restore by copying this file onto live appreg.
ignite_appreg_backup() {
  mkdir -p "$BACKUP_DIR" || return 1
  TS=$(date -u +%Y%m%dT%H%M%SZ)
  BACKUP="$BACKUP_DIR/appreg.db.bak.$TS"
  cp "$APPREG" "$BACKUP" || return 1
  INTEGRITY=$("$SQLITE3" "$BACKUP" "PRAGMA integrity_check;") || return 1
  case "$INTEGRITY" in
    ok) ;;
    *)
      echo "STOP: appreg backup integrity_check failed: $INTEGRITY" >&2
      return 1
      ;;
  esac
  echo "appreg_backup_forensic=$BACKUP"
}

# Ignite-scoped snapshot with transaction id. One IMMEDIATE txn so the three
# table reads are consistent. SQL errors are not masked.
# Uses current $HANDLER.
ignite_snapshot_handler() {
  TXN=$1
  mkdir -p "$BACKUP_DIR" || return 1
  SNAP="$BACKUP_DIR/handler-$HANDLER.$TXN.sql"
  SNAP_TMP="$SNAP.tmp"
  {
    echo "-- IGNITE_APPREG_SNAP v1"
    echo "-- txn=$TXN"
    echo "-- handler=$HANDLER"
    echo "-- appreg=$APPREG"
    echo "-- captured=$(date -u +%Y%m%dT%H%M%SZ)"
  } >"$SNAP_TMP" || return 1
  if [ "${IGNITE_FAIL_SNAPSHOT_SQL:-}" = "1" ]; then
    if ignite_sql_script <<SQL
SELECT ignite_injected_snapshot_failure();
SQL
    then
      rm -f "$SNAP_TMP"
      return 1
    fi
    rm -f "$SNAP_TMP"
    echo "appreg_snapshot_failed: sqlite" >&2
    return 1
  fi
  DUMP=$(
    ignite_sql_script <<SQL
BEGIN IMMEDIATE;
SELECT 'INSERT INTO handlerIds(handlerId) VALUES(' || quote(handlerId) || ');' FROM handlerIds WHERE handlerId='$HANDLER';
SELECT 'INSERT INTO properties(handlerId,name,value) VALUES(' || quote(handlerId) || ',' || quote(name) || ',' || quote(value) || ');' FROM properties WHERE handlerId='$HANDLER';
SELECT 'INSERT INTO associations(handlerId,interface,contentId,defaultAssoc) VALUES(' || quote(handlerId) || ',' || quote(interface) || ',' || quote(contentId) || ',' || quote(defaultAssoc) || ');' FROM associations WHERE handlerId='$HANDLER';
COMMIT;
SQL
  ) || {
    rm -f "$SNAP_TMP"
    echo "appreg_snapshot_failed: sqlite" >&2
    return 1
  }
  printf '%s\n' "$DUMP" | grep '^INSERT INTO ' >>"$SNAP_TMP" || true
  grep -q "^-- txn=$TXN$" "$SNAP_TMP" || {
    rm -f "$SNAP_TMP"
    return 1
  }
  grep -q "^-- handler=$HANDLER$" "$SNAP_TMP" || {
    rm -f "$SNAP_TMP"
    return 1
  }
  mv -f "$SNAP_TMP" "$SNAP" || return 1
  echo "$SNAP"
}

# Restore only rows for current $HANDLER from a snapshot tied to TXN.
ignite_restore_handler() {
  SNAP=$1
  WANT_TXN=${2:-}
  if [ -z "$SNAP" ] || [ ! -f "$SNAP" ]; then
    echo "appreg_restore_failed: snapshot missing" >&2
    return 1
  fi
  head=$(head -n 5 "$SNAP")
  echo "$head" | grep -q "IGNITE_APPREG_SNAP v1" || {
    echo "appreg_restore_failed: not an Ignite handler snapshot" >&2
    return 1
  }
  echo "$head" | grep -q -- "-- handler=$HANDLER" || {
    echo "appreg_restore_failed: snapshot handler mismatch" >&2
    return 1
  }
  if [ -n "$WANT_TXN" ]; then
    echo "$head" | grep -q -- "-- txn=$WANT_TXN" || {
      echo "appreg_restore_failed: snapshot txn $WANT_TXN not found" >&2
      return 1
    }
  fi
  if ! {
    echo "BEGIN IMMEDIATE;"
    echo "DELETE FROM associations WHERE handlerId='$HANDLER';"
    echo "DELETE FROM properties WHERE handlerId='$HANDLER';"
    echo "DELETE FROM handlerIds WHERE handlerId='$HANDLER';"
    grep -v '^--' "$SNAP" | grep -v '^[[:space:]]*$' || true
    if [ "${IGNITE_FAIL_RESTORE_SQL:-}" = "1" ]; then
      echo "SELECT ignite_injected_restore_failure();"
    fi
    echo "COMMIT;"
  } | ignite_sql_script; then
    echo "appreg_restore_failed: sqlite" >&2
    return 1
  fi
  echo "appreg_restore_ok snap=$SNAP handler=$HANDLER txn=${WANT_TXN:-any}"
}

ignite_register_handler() {
  JSON="$1"
  if [ ! -f "$JSON" ]; then
    echo "missing registration json: $JSON" >&2
    return 1
  fi
  if [ ! -f "$APPREG" ]; then
    echo "missing $APPREG" >&2
    return 1
  fi
  # Refuse JSON that would register a different handler than canonical $HANDLER.
  if ! grep -q "\"handlerId\"[[:space:]]*:[[:space:]]*\"$HANDLER\"" "$JSON"; then
    echo "registration json handlerId must be $HANDLER" >&2
    return 1
  fi
  TXN=$(date -u +%Y%m%dT%H%M%SZ)-$$
  IGNITE_APPREG_TXN=$TXN
  if [ "${IGNITE_SKIP_APPREG_BACKUP:-}" != "1" ]; then
    ignite_appreg_backup || return 1
  fi
  SNAP=$(ignite_snapshot_handler "$TXN") || return 1
  IGNITE_HANDLER_SNAP=$SNAP
  export IGNITE_APPREG_TXN IGNITE_HANDLER_SNAP
  if [ -n "${IGNITE_ACTIVATE_JOURNAL:-}" ] && [ -f "${IGNITE_ACTIVATE_JOURNAL}" ]; then
    echo "# handler_snap=$SNAP" >>"$IGNITE_ACTIVATE_JOURNAL" || return 1
    echo "# handler_txn=$TXN" >>"$IGNITE_ACTIVATE_JOURNAL" || return 1
  fi

  if [ -n "${IGNITE_REGISTER_STUB:-}" ]; then
    if ! sh "$IGNITE_REGISTER_STUB" "$JSON" "$APPREG"; then
      ignite_restore_handler "$SNAP" "$TXN" || return 1
      return 1
    fi
    echo "register_ok handlerId=$HANDLER stub=1 txn=$TXN snap=$SNAP"
    return 0
  fi
  if [ ! -x /usr/bin/register ] && [ -z "${IGNITE_REGISTER_BIN:-}" ]; then
    echo "missing /usr/bin/register" >&2
    return 1
  fi
  REG="${IGNITE_REGISTER_BIN:-/usr/bin/register}"
  if ! ignite_sql_script <<SQL
BEGIN IMMEDIATE;
DELETE FROM associations WHERE handlerId='$HANDLER';
DELETE FROM properties WHERE handlerId='$HANDLER';
DELETE FROM handlerIds WHERE handlerId='$HANDLER';
COMMIT;
SQL
  then
    echo "register_failed: sqlite delete" >&2
    return 1
  fi
  if ! "$REG" -o "$APPREG" "$JSON"; then
    ignite_restore_handler "$SNAP" "$TXN" || return 1
    return 1
  fi
  echo "register_ok handlerId=$HANDLER txn=$TXN snap=$SNAP"
}

ignite_unregister_handler() {
  if [ ! -f "$APPREG" ]; then
    echo "missing $APPREG" >&2
    return 1
  fi
  TXN=$(date -u +%Y%m%dT%H%M%SZ)-$$
  if [ "${IGNITE_SKIP_APPREG_BACKUP:-}" != "1" ]; then
    ignite_appreg_backup || return 1
  fi
  ignite_snapshot_handler "$TXN" >/dev/null || return 1
  if ! ignite_sql_script <<SQL
BEGIN IMMEDIATE;
DELETE FROM associations WHERE handlerId='$HANDLER';
DELETE FROM properties WHERE handlerId='$HANDLER';
DELETE FROM handlerIds WHERE handlerId='$HANDLER';
COMMIT;
SQL
  then
    echo "unregister_failed: sqlite" >&2
    return 1
  fi
  LEFT=$(ignite_sql "SELECT COUNT(*) FROM handlerIds WHERE handlerId='$HANDLER';") || return 1
  echo "unregister_ok handlerId=$HANDLER remaining_rows=$LEFT"
}

ignite_handler_command() {
  ignite_sql "PRAGMA busy_timeout=8000;" >/dev/null || return 1
  ignite_sql "SELECT value FROM properties WHERE handlerId='$HANDLER' AND name='command';"
}

ignite_handler_mark() {
  ignite_sql "PRAGMA busy_timeout=8000;" >/dev/null || return 1
  ignite_sql "SELECT value FROM properties WHERE handlerId='$HANDLER' AND name='igniteMark';"
}

ignite_handler_registered() {
  ignite_sql "PRAGMA busy_timeout=8000;" >/dev/null || return 1
  COUNT=$(ignite_sql "SELECT COUNT(*) FROM handlerIds WHERE handlerId='$HANDLER';") || return 1
  [ "$COUNT" = "1" ]
}

# True if $1 looks like an Ignite-owned lifecycle host command.
ignite_command_is_ours() {
  cmd=$1
  [ -n "$cmd" ] || return 1
  case "$cmd" in
    "$EXPECTED_COMMAND"|*/scribe-app/bin/scribe-lifecycle-host.sh|*/scribe-lifecycle-host.sh)
      return 0
      ;;
  esac
  return 1
}

# Retire legacy com.markrai.scribe only when it clearly belongs to Ignite.
# Never deletes unrelated appreg handlers. Failure is non-fatal for callers
# that already have a working canonical handler.
ignite_retire_legacy_handler() {
  if [ ! -f "$APPREG" ]; then
    return 0
  fi
  _save=$HANDLER
  HANDLER=$LEGACY_HANDLER
  if ! ignite_handler_registered; then
    HANDLER=$_save
    echo "legacy_absent handlerId=$LEGACY_HANDLER"
    return 0
  fi
  LCMD=$(ignite_handler_command 2>/dev/null || true)
  LMARK=$(ignite_handler_mark 2>/dev/null || true)
  if ! ignite_command_is_ours "$LCMD" && [ -z "$LMARK" ]; then
    HANDLER=$_save
    echo "legacy_retire_skipped handlerId=$LEGACY_HANDLER reason=not_ignite_owned command=${LCMD:-unset}" >&2
    return 1
  fi
  if [ "${IGNITE_FAIL_LEGACY_RETIRE:-}" = "1" ]; then
    HANDLER=$_save
    echo "legacy_retire_failed: injected" >&2
    return 1
  fi
  if ! ignite_unregister_handler; then
    HANDLER=$_save
    echo "legacy_retire_failed handlerId=$LEGACY_HANDLER" >&2
    return 1
  fi
  HANDLER=$_save
  echo "legacy_retire_ok handlerId=$LEGACY_HANDLER"
  return 0
}

# Unregister canonical Ignite handler and, if still present, Ignite-owned legacy.
ignite_unregister_all_ignite_handlers() {
  ignite_unregister_handler || return 1
  # Best-effort legacy cleanup; do not fail uninstall if legacy was never present
  # or was already removed. Still fail if legacy looks Ignite-owned and delete fails.
  _save=$HANDLER
  HANDLER=$LEGACY_HANDLER
  if ignite_handler_registered; then
    LCMD=$(ignite_handler_command 2>/dev/null || true)
    LMARK=$(ignite_handler_mark 2>/dev/null || true)
    if ignite_command_is_ours "$LCMD" || [ -n "$LMARK" ]; then
      if ! ignite_unregister_handler; then
        HANDLER=$_save
        return 1
      fi
    else
      echo "legacy_uninstall_skipped handlerId=$LEGACY_HANDLER reason=not_ignite_owned" >&2
    fi
  fi
  HANDLER=$_save
  return 0
}

# Register (or refresh) canonical handler, then retire legacy after verification.
# Always re-registers so install/repair refreshes appreg props; failures restore
# only the canonical handler snapshot and leave legacy untouched.
ignite_register_and_migrate() {
  JSON=$1
  if [ ! -f "$JSON" ]; then
    echo "missing registration json: $JSON" >&2
    return 1
  fi

  if [ "${IGNITE_FAIL_REGISTER_NEW:-}" = "1" ]; then
    echo "register_failed: injected new registration failure" >&2
    return 1
  fi

  # Register canonical. On failure, helpers restore the pre-register snap for
  # $HANDLER only — legacy rows are never deleted in this step.
  if ! ignite_register_handler "$JSON"; then
    echo "register_failed: canonical handler $HANDLER" >&2
    return 1
  fi

  if ! ignite_handler_registered; then
    echo "register_failed: canonical missing after register" >&2
    return 1
  fi
  CMD=$(ignite_handler_command)
  if [ "$CMD" != "$EXPECTED_COMMAND" ]; then
    echo "register_failed: canonical command is '$CMD' (expected $EXPECTED_COMMAND)" >&2
    if [ -n "${IGNITE_HANDLER_SNAP:-}" ] && [ -f "${IGNITE_HANDLER_SNAP}" ]; then
      ignite_restore_handler "$IGNITE_HANDLER_SNAP" "${IGNITE_APPREG_TXN:-}" || true
    fi
    return 1
  fi

  # Canonical is confirmed. Retire legacy; warn-only on cleanup failure so the
  # user keeps a working Ignite launch path (may briefly see two Library entries).
  if ! ignite_retire_legacy_handler; then
    echo "migrate_warn: legacy handler still present; repair by re-running Install Ignite" >&2
  fi
  echo "register_ok handlerId=$HANDLER migrated=1"
  return 0
}

# Diagnose helper: print registration state lines (stdout).
ignite_diagnose_registration() {
  ignite_sql "PRAGMA busy_timeout=8000;" >/dev/null 2>&1 || {
    echo "registration_state=appreg_unavailable"
    return 0
  }
  NEW_N=$(ignite_sql "SELECT COUNT(*) FROM handlerIds WHERE handlerId='$HANDLER';" 2>/dev/null || echo err)
  LEG_N=$(ignite_sql "SELECT COUNT(*) FROM handlerIds WHERE handlerId='$LEGACY_HANDLER';" 2>/dev/null || echo err)
  NEW_CMD=$(ignite_sql "SELECT value FROM properties WHERE handlerId='$HANDLER' AND name='command';" 2>/dev/null || true)
  LEG_CMD=$(ignite_sql "SELECT value FROM properties WHERE handlerId='$LEGACY_HANDLER' AND name='command';" 2>/dev/null || true)
  echo "canonical_handler=$HANDLER"
  echo "legacy_handler=$LEGACY_HANDLER"
  echo "canonical_rows=$NEW_N"
  echo "legacy_rows=$LEG_N"
  echo "canonical_command=${NEW_CMD:-unset}"
  echo "legacy_command=${LEG_CMD:-unset}"
  case "$NEW_N:$LEG_N" in
    1:0) echo "registration_state=canonical_only" ;;
    0:1) echo "registration_state=legacy_only" ;;
    1:1) echo "registration_state=both_present" ;;
    0:0) echo "registration_state=neither" ;;
    *) echo "registration_state=inconsistent" ;;
  esac
  if [ "$NEW_N" = "1" ] && [ "$NEW_CMD" = "$EXPECTED_COMMAND" ]; then
    echo "canonical_command_ok=yes"
  else
    echo "canonical_command_ok=no"
  fi
}
