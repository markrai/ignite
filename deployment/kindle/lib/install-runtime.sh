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
UPDATE_DIR=$APP_ROOT/state/update
PENDING_HEALTH=$UPDATE_DIR/pending-health.v1
LKG_INVENTORY_NAME=ignite-lkg-inventory.v1
LKG_INVENTORY=$PREV_DIR/$LKG_INVENTORY_NAME
LKG_HANDLER_NAME=appreg-handler.sql
LKG_HANDLER=$PREV_DIR/$LKG_HANDLER_NAME

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

sha256_file() {
  sha_output=$(sha256sum "$1" 2>/dev/null) || return 1
  sha_value=${sha_output%%[!0123456789abcdef]*}
  sha_rest=${sha_output#"$sha_value"}
  [ "${#sha_value}" -eq 64 ] \
    && printf '%s\n' "$sha_value" | LC_ALL=C grep -Eq '^[0-9a-f]{64}$' \
    || return 1
  case "$sha_rest" in ' '*) ;; *) return 1 ;; esac
  printf '%s\n' "$sha_value"
}

file_bytes() {
  bytes_output=$(wc -c < "$1" 2>/dev/null) || return 1
  bytes_value=$(printf '%s' "$bytes_output" \
    | LC_ALL=C sed 's/^[[:space:]]*//; s/[[:space:]]*$//') || return 1
  case "$bytes_value" in
    0|[1-9]|[1-9][0-9]*) ;;
    *) return 1 ;;
  esac
  case "$bytes_value" in *[!0-9]*) return 1 ;; esac
  printf '%s\n' "$bytes_value"
}

valid_numeric_identifier() {
  identifier=$1
  case "$identifier" in
    ''|*[!0-9]*) return 1 ;;
    0) return 0 ;;
    0*) return 1 ;;
    *) return 0 ;;
  esac
}

# Keep this grammar byte-for-byte aligned with kindle_release_sign.py and the
# Rust release-manifest verifier: SemVer core plus an optional prerelease only.
valid_release_version() {
  version=$1
  [ -n "$version" ] && [ "${#version}" -le 64 ] || return 1
  core=${version%%-*}
  if [ "$core" = "$version" ]; then
    prerelease=
  else
    prerelease=${version#*-}
    [ -n "$prerelease" ] || return 1
  fi
  printf '%s\n' "$core" | LC_ALL=C grep -Eq \
    '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' || return 1
  [ -z "$prerelease" ] && return 0
  printf '%s\n' "$prerelease" | LC_ALL=C grep -Eq \
    '^[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*$' || return 1
  old_ifs=$IFS
  IFS=.
  set -- $prerelease
  IFS=$old_ifs
  for identifier in "$@"; do
    case "$identifier" in
      *[!0-9]*) ;;
      *) valid_numeric_identifier "$identifier" || return 1 ;;
    esac
  done
  return 0
}

refuse_symlink() {
  path=$1
  label=$2
  if [ -L "$path" ]; then
    log "install_integrity: $label is a symlink ($path)"
    return 1
  fi
  return 0
}

ensure_update_dir() {
  refuse_symlink "$APP_ROOT/state" "state directory" || return 1
  refuse_symlink "$UPDATE_DIR" "update state directory" || return 1
  if [ -e "$APP_ROOT/state" ] && [ ! -d "$APP_ROOT/state" ]; then
    log "install_integrity: state path is not a directory"
    return 1
  fi
  if [ -e "$UPDATE_DIR" ] && [ ! -d "$UPDATE_DIR" ]; then
    log "install_integrity: update state path is not a directory"
    return 1
  fi
  mkdir -p "$UPDATE_DIR" || return 1
  chmod 0700 "$UPDATE_DIR" 2>/dev/null || true
  return 0
}

snapshot_name_allowed() {
  case "$1" in
    scribe-app|scribe-lifecycle|scribe-lifecycle-host.sh|Scribe.sh|scribe-icon.png|\
    OFL.txt|DESCRIPTION.en_us.html|upstream_info.md|source-sans-3-OFL.txt|\
    install-runtime.sh|uninstall-runtime.sh|backup-runtime.sh|register-helpers.sh|\
    install-recovery.sh|Uninstall\ Ignite.sh|Backup\ Ignite\ Data.sh|Diagnose\ Ignite.sh|\
    "$LKG_HANDLER_NAME"|"$LKG_INVENTORY_NAME")
      return 0
      ;;
    *) return 1 ;;
  esac
}

validate_snapshot_tree() {
  root=$1
  refuse_symlink "$root" "retained release directory" || return 1
  [ -d "$root" ] || {
    log "lkg_invalid: retained release directory missing"
    return 1
  }
  for path in "$root"/* "$root"/.[!.]* "$root"/..?*; do
    if [ ! -e "$path" ] && [ ! -L "$path" ]; then
      continue
    fi
    name=${path##*/}
    snapshot_name_allowed "$name" || {
      log "lkg_invalid: unexpected retained path $name"
      return 1
    }
    refuse_symlink "$path" "retained path $name" || return 1
    [ -f "$path" ] || {
      log "lkg_invalid: retained path is not a regular file: $name"
      return 1
    }
  done
  for required in scribe-app scribe-lifecycle scribe-lifecycle-host.sh; do
    [ -f "$root/$required" ] && [ ! -L "$root/$required" ] || {
      log "lkg_invalid: required retained file missing: $required"
      return 1
    }
  done
  return 0
}

lkg_inventory_entry() {
  root=$1
  name=$2
  path=$root/$name
  refuse_symlink "$path" "retained inventory entry $name" || return 1
  if [ -f "$path" ]; then
    bytes=$(file_bytes "$path") || return 1
    digest=$(sha256_file "$path") || return 1
    printf 'entry=%s|file|%s|%s\n' "$name" "$bytes" "$digest"
  elif [ -e "$path" ]; then
    log "lkg_invalid: inventory entry is not a regular file: $name"
    return 1
  else
    printf 'entry=%s|absent|0|-\n' "$name"
  fi
}

# Canonical bytes: LF-only fixed header and fixed entry order. Paths are a closed set.
write_lkg_inventory() {
  root=$1
  out=$2
  validate_snapshot_tree "$root" || return 1
  {
    printf '%s\n' 'IGNITE_LKG_INVENTORY_V1'
    printf '%s\n' 'schema=1'
    printf '%s\n' 'kind=ignite-application-release'
    printf '%s\n' 'entry_count=18'
  } >"$out" || return 1
  for name in \
    scribe-app scribe-lifecycle scribe-lifecycle-host.sh Scribe.sh scribe-icon.png \
    OFL.txt DESCRIPTION.en_us.html upstream_info.md source-sans-3-OFL.txt \
    install-runtime.sh uninstall-runtime.sh backup-runtime.sh register-helpers.sh \
    install-recovery.sh "Uninstall Ignite.sh" "Backup Ignite Data.sh" \
    "Diagnose Ignite.sh" "$LKG_HANDLER_NAME"
  do
    lkg_inventory_entry "$root" "$name" >>"$out" || return 1
  done
  return 0
}

validate_lkg_inventory() {
  expected_bytes=$1
  expected_sha=$2
  ensure_update_dir || return 1
  validate_snapshot_tree "$PREV_DIR" || return 1
  refuse_symlink "$LKG_INVENTORY" "retained inventory" || return 1
  [ -f "$LKG_INVENTORY" ] || {
    log "lkg_invalid: inventory missing"
    return 1
  }
  inv_bytes=$(file_bytes "$LKG_INVENTORY") || return 1
  [ "$inv_bytes" -gt 0 ] && [ "$inv_bytes" -le 32768 ] || {
    log "lkg_invalid: inventory size out of bounds ($inv_bytes)"
    return 1
  }
  [ "$inv_bytes" = "$expected_bytes" ] || {
    log "lkg_invalid: inventory byte count mismatch (have $inv_bytes want $expected_bytes)"
    return 1
  }
  inv_sha=$(sha256_file "$LKG_INVENTORY") || return 1
  [ "$inv_sha" = "$expected_sha" ] || {
    log "lkg_invalid: inventory digest mismatch (have $inv_sha want $expected_sha)"
    return 1
  }
  verify_tmp=$UPDATE_DIR/.lkg-verify.$$.tmp
  refuse_symlink "$verify_tmp" "inventory verification temporary" || return 1
  rm -f "$verify_tmp"
  if ! write_lkg_inventory "$PREV_DIR" "$verify_tmp"; then
    rm -f "$verify_tmp"
    return 1
  fi
  verify_sha=$(sha256_file "$verify_tmp") || {
    rm -f "$verify_tmp"
    return 1
  }
  rm -f "$verify_tmp"
  [ "$verify_sha" = "$inv_sha" ] || {
    log "lkg_invalid: retained files do not match canonical inventory"
    return 1
  }
  return 0
}

pending_invalid() {
  log "pending_health_invalid: $*"
  return 1
}

pending_canonical_matches() {
  canonical_tmp=$UPDATE_DIR/.pending-verify.$$.tmp
  refuse_symlink "$canonical_tmp" "pending-health verification temporary" || return 1
  rm -f "$canonical_tmp"
  if ! {
    printf '%s\n' 'IGNITE_PENDING_HEALTH_V1'
    printf '%s\n' 'schema=1'
    printf '%s\n' 'status=pending-health'
    printf 'candidate_version=%s\n' "$PH_CANDIDATE_VERSION"
    printf 'candidate_txn=%s\n' "$PH_CANDIDATE_TXN"
    printf 'candidate_app_bytes=%s\n' "$PH_APP_BYTES"
    printf 'candidate_app_sha256=%s\n' "$PH_APP_SHA"
    printf 'candidate_lifecycle_bytes=%s\n' "$PH_LIFE_BYTES"
    printf 'candidate_lifecycle_sha256=%s\n' "$PH_LIFE_SHA"
    printf '%s\n' 'retained_version=unknown'
    printf 'retained_release_identity=sha256:%s\n' "$PH_LKG_ID"
    printf 'lkg_inventory_bytes=%s\n' "$PH_INV_BYTES"
    printf 'lkg_inventory_sha256=%s\n' "$PH_INV_SHA"
    printf '%s\n' 'data_compatibility=unknown'
  } >"$canonical_tmp"; then
    rm -f "$canonical_tmp"
    return 1
  fi
  if ! cmp -s "$canonical_tmp" "$PENDING_HEALTH"; then
    rm -f "$canonical_tmp"
    pending_invalid "record bytes are not the exact canonical 14-line encoding"
    return 1
  fi
  rm -f "$canonical_tmp"
  return 0
}

validate_pending_health() {
  ensure_update_dir || return 1
  refuse_symlink "$PENDING_HEALTH" "pending-health record" || return 1
  [ -f "$PENDING_HEALTH" ] || return 1
  pending_bytes=$(file_bytes "$PENDING_HEALTH") || return 1
  [ "$pending_bytes" -gt 0 ] && [ "$pending_bytes" -le 4096 ] \
    || pending_invalid "record size out of bounds ($pending_bytes)" || return 1

  ph_1=$(sed -n '1p' "$PENDING_HEALTH")
  ph_2=$(sed -n '2p' "$PENDING_HEALTH")
  ph_3=$(sed -n '3p' "$PENDING_HEALTH")
  ph_4=$(sed -n '4p' "$PENDING_HEALTH")
  ph_5=$(sed -n '5p' "$PENDING_HEALTH")
  ph_6=$(sed -n '6p' "$PENDING_HEALTH")
  ph_7=$(sed -n '7p' "$PENDING_HEALTH")
  ph_8=$(sed -n '8p' "$PENDING_HEALTH")
  ph_9=$(sed -n '9p' "$PENDING_HEALTH")
  ph_10=$(sed -n '10p' "$PENDING_HEALTH")
  ph_11=$(sed -n '11p' "$PENDING_HEALTH")
  ph_12=$(sed -n '12p' "$PENDING_HEALTH")
  ph_13=$(sed -n '13p' "$PENDING_HEALTH")
  ph_14=$(sed -n '14p' "$PENDING_HEALTH")
  [ "$ph_1" = "IGNITE_PENDING_HEALTH_V1" ] || pending_invalid "unsupported header" || return 1
  [ "$ph_2" = "schema=1" ] || pending_invalid "unsupported schema" || return 1
  [ "$ph_3" = "status=pending-health" ] || pending_invalid "unsupported status" || return 1
  case "$ph_4" in candidate_version=*) PH_CANDIDATE_VERSION=${ph_4#candidate_version=} ;; *) pending_invalid "candidate_version missing"; return 1 ;; esac
  case "$ph_5" in candidate_txn=*) PH_CANDIDATE_TXN=${ph_5#candidate_txn=} ;; *) pending_invalid "candidate_txn missing"; return 1 ;; esac
  case "$ph_6" in candidate_app_bytes=*) PH_APP_BYTES=${ph_6#candidate_app_bytes=} ;; *) pending_invalid "candidate_app_bytes missing"; return 1 ;; esac
  case "$ph_7" in candidate_app_sha256=*) PH_APP_SHA=${ph_7#candidate_app_sha256=} ;; *) pending_invalid "candidate_app_sha256 missing"; return 1 ;; esac
  case "$ph_8" in candidate_lifecycle_bytes=*) PH_LIFE_BYTES=${ph_8#candidate_lifecycle_bytes=} ;; *) pending_invalid "candidate_lifecycle_bytes missing"; return 1 ;; esac
  case "$ph_9" in candidate_lifecycle_sha256=*) PH_LIFE_SHA=${ph_9#candidate_lifecycle_sha256=} ;; *) pending_invalid "candidate_lifecycle_sha256 missing"; return 1 ;; esac
  [ "$ph_10" = "retained_version=unknown" ] || pending_invalid "retained_version must remain explicit unknown" || return 1
  case "$ph_11" in retained_release_identity=sha256:*) PH_LKG_ID=${ph_11#retained_release_identity=sha256:} ;; *) pending_invalid "retained release identity missing"; return 1 ;; esac
  case "$ph_12" in lkg_inventory_bytes=*) PH_INV_BYTES=${ph_12#lkg_inventory_bytes=} ;; *) pending_invalid "lkg_inventory_bytes missing"; return 1 ;; esac
  case "$ph_13" in lkg_inventory_sha256=*) PH_INV_SHA=${ph_13#lkg_inventory_sha256=} ;; *) pending_invalid "lkg_inventory_sha256 missing"; return 1 ;; esac
  [ "$ph_14" = "data_compatibility=unknown" ] || pending_invalid "data compatibility must remain unknown" || return 1

  valid_release_version "$PH_CANDIDATE_VERSION" \
    || pending_invalid "candidate version malformed or noncanonical" || return 1
  printf '%s\n' "$PH_CANDIDATE_TXN" | grep -Eq '^[0-9]{8}T[0-9]{6}Z-[0-9]+$' \
    || pending_invalid "candidate transaction malformed" || return 1
  for value in "$PH_APP_BYTES" "$PH_LIFE_BYTES" "$PH_INV_BYTES"; do
    printf '%s\n' "$value" | grep -Eq '^(0|[1-9][0-9]*)$' \
      || pending_invalid "non-canonical byte count" || return 1
  done
  for digest in "$PH_APP_SHA" "$PH_LIFE_SHA" "$PH_LKG_ID" "$PH_INV_SHA"; do
    printf '%s\n' "$digest" | grep -Eq '^[0-9a-f]{64}$' \
      || pending_invalid "malformed SHA-256 digest" || return 1
  done
  [ "$PH_LKG_ID" = "$PH_INV_SHA" ] \
    || pending_invalid "retained release identity does not match inventory identity" || return 1
  pending_canonical_matches || return 1

  for live in "$STABLE_APP" "$STABLE_LIFE"; do
    refuse_symlink "$live" "installed candidate file" || return 1
    [ -f "$live" ] || pending_invalid "installed candidate file missing: $live" || return 1
  done
  live_app_bytes=$(file_bytes "$STABLE_APP") \
    || pending_invalid "cannot measure installed candidate application" || return 1
  live_app_sha=$(sha256_file "$STABLE_APP") \
    || pending_invalid "cannot hash installed candidate application" || return 1
  [ "$live_app_bytes" = "$PH_APP_BYTES" ] \
    && [ "$live_app_sha" = "$PH_APP_SHA" ] \
    || pending_invalid "installed candidate application does not match record" || return 1
  live_life_bytes=$(file_bytes "$STABLE_LIFE") \
    || pending_invalid "cannot measure installed candidate lifecycle" || return 1
  live_life_sha=$(sha256_file "$STABLE_LIFE") \
    || pending_invalid "cannot hash installed candidate lifecycle" || return 1
  [ "$live_life_bytes" = "$PH_LIFE_BYTES" ] \
    && [ "$live_life_sha" = "$PH_LIFE_SHA" ] \
    || pending_invalid "installed candidate lifecycle does not match record" || return 1
  validate_lkg_inventory "$PH_INV_BYTES" "$PH_INV_SHA" || return 1
  return 0
}

guard_pending_health() {
  ensure_update_dir || die "pending health state path is unsafe"
  if [ -e "$PENDING_HEALTH" ] || [ -L "$PENDING_HEALTH" ]; then
    validate_pending_health \
      || die "pending health state is malformed, damaged, or inconsistent; retained recovery evidence was not changed"
    die "update blocked: candidate $PH_CANDIDATE_VERSION transaction $PH_CANDIDATE_TXN is still pending health qualification; retained last-known-good was preserved"
  fi
  if [ -e "$LKG_INVENTORY" ] || [ -L "$LKG_INVENTORY" ] \
    || [ -e "$LKG_HANDLER" ] || [ -L "$LKG_HANDLER" ]; then
    die "pending health state is missing but retained qualification metadata remains; refusing ambiguous installation without deleting evidence"
  fi
  for orphan in "$UPDATE_DIR"/.pending-health.*.tmp \
    "$UPDATE_DIR"/.pending-verify.*.tmp "$UPDATE_DIR"/.lkg-verify.*.tmp; do
    if [ -e "$orphan" ] || [ -L "$orphan" ]; then
      die "pending health state has an orphaned temporary file; refusing ambiguous installation without deleting evidence"
    fi
  done
  return 0
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

cleanup_lkg_temporaries() {
  refuse_symlink "$PREV_DIR" "previous-release directory" || return 1
  refuse_symlink "$UPDATE_DIR" "update state directory" || return 1
  rm -f "$PREV_DIR/.ignite-lkg-inventory."*.tmp \
    "$PREV_DIR/.appreg-handler."*.tmp \
    "$PREV_DIR/.lkg-complete."*.tmp \
    "$UPDATE_DIR/.pending-health."*.tmp \
    "$UPDATE_DIR/.pending-verify."*.tmp \
    "$UPDATE_DIR/.lkg-verify."*.tmp 2>/dev/null || true
  return 0
}

cleanup_committed_leftovers() {
  cleanup_lkg_temporaries || return 1
  rm -f "$JOURNAL" "$JOURNAL.tmp" 2>/dev/null || true
  rm -rf "$PREV_STAGING" || true
  rm -rf "$PREV_OLD" || true
  rm -f "$COMMITTED" "$COMMITTED.tmp" 2>/dev/null || true
  return 0
}

v2_commit_marker_canonical_matches() {
  expected_marker=$1
  marker_bytes=$(file_bytes "$COMMITTED") || {
    log "install_recover: cannot measure activation commit marker"
    return 1
  }
  [ "$marker_bytes" -gt 0 ] && [ "$marker_bytes" -le 256 ] || {
    log "install_recover: activation commit marker size is out of bounds"
    return 1
  }
  marker_tmp=$APP_ROOT/run/.commit-verify.$$.tmp
  refuse_symlink "$marker_tmp" "activation commit verification temporary" || return 1
  rm -f "$marker_tmp"
  if ! printf '%s\n' "$expected_marker" >"$marker_tmp"; then
    rm -f "$marker_tmp"
    return 1
  fi
  if ! cmp -s "$marker_tmp" "$COMMITTED"; then
    rm -f "$marker_tmp"
    log "install_recover: v2 activation commit marker is not canonically encoded"
    return 1
  fi
  rm -f "$marker_tmp"
  return 0
}

validate_committed_transaction() {
  refuse_symlink "$COMMITTED" "activation commit marker" || return 1
  [ -f "$COMMITTED" ] || return 1
  marker=$(sed -n '1p' "$COMMITTED")
  case "$marker" in
    committed\ txn=*)
      # Legacy v1 marker: preserve the existing authoritative cleanup behavior.
      return 0
      ;;
    ignite-activate-commit-v2\ txn=*\ pending_health=0)
      commit_txn=${marker#ignite-activate-commit-v2 txn=}
      commit_txn=${commit_txn% pending_health=0}
      commit_pending=0
      ;;
    ignite-activate-commit-v2\ txn=*\ pending_health=1)
      commit_txn=${marker#ignite-activate-commit-v2 txn=}
      commit_txn=${commit_txn% pending_health=1}
      commit_pending=1
      ;;
    *)
      log "install_recover: malformed or unsupported activation commit marker"
      return 1
      ;;
  esac
  printf '%s\n' "$commit_txn" | grep -Eq '^[0-9]{8}T[0-9]{6}Z-[0-9]+$' || {
    log "install_recover: malformed transaction id in commit marker"
    return 1
  }
  v2_commit_marker_canonical_matches \
    "ignite-activate-commit-v2 txn=$commit_txn pending_health=$commit_pending" || return 1
  if [ "$commit_pending" = "0" ]; then
    if [ -e "$PENDING_HEALTH" ] || [ -L "$PENDING_HEALTH" ]; then
      log "install_recover: committed first install has contradictory pending-health state"
      return 1
    fi
  else
    validate_pending_health || return 1
    [ "$PH_CANDIDATE_TXN" = "$commit_txn" ] || {
      log "install_recover: committed transaction does not match pending-health transaction"
      return 1
    }
  fi
  if [ -f "$JOURNAL" ]; then
    journal_txn=$(grep '^# txn=' "$JOURNAL" 2>/dev/null | tail -n 1 | sed 's/^# txn=//') || true
    [ "$journal_txn" = "$commit_txn" ] || {
      log "install_recover: journal and commit marker transaction ids differ"
      return 1
    }
  fi
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
  cleanup_lkg_temporaries || return 1
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
  # Candidate metadata belongs to the rolled-back transaction. Remove it only
  # after A and its Ignite handler have been restored successfully.
  rm -f "$PENDING_HEALTH" "$LKG_INVENTORY" "$LKG_HANDLER"
  sync
  rm -f "$JOURNAL"
  sync
  return 0
}

recover_install_transaction() {
  if [ -f "$COMMITTED" ]; then
    log "install_recover: committed transaction leftover cleanup (keep installed runtime)"
    validate_committed_transaction || return 1
    cleanup_committed_leftovers
    return 0
  fi
  if [ -f "$JOURNAL" ]; then
    log "install_recover: pending transaction; rolling back to previous complete version"
    restore_pending_transaction || return 1
    return 0
  fi
  rm -rf "$PREV_STAGING" || true
  cleanup_lkg_temporaries || return 1
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
  refuse_symlink "$src" "snapshot source" || return 1
  if [ ! -f "$src" ]; then
    return 0
  fi
  cp -f "$src" "$dest" || return 1
}

snapshot_copy_required() {
  src=$1
  dest=$2
  refuse_symlink "$src" "snapshot source" || return 1
  [ -f "$src" ] || return 1
  cp -f "$src" "$dest" || return 1
}

# Fill staging from live files, then publish as previous-release.
# Does NOT delete .prev: that remains the old recovery base until durable commit.
snapshot_previous_release() {
  refuse_symlink "$PREV_DIR" "previous-release" || return 1
  refuse_symlink "$PREV_OLD" "previous-release.prev" || return 1
  refuse_symlink "$PREV_STAGING" "previous-release.staging" || return 1
  rm -rf "$PREV_STAGING"
  mkdir -p "$PREV_STAGING" || return 1
  snapshot_copy_required "$STABLE_APP" "$PREV_STAGING/scribe-app" || return 1
  snapshot_copy_required "$STABLE_LIFE" "$PREV_STAGING/scribe-lifecycle" || return 1
  snapshot_copy_required "$STABLE_HOST" "$PREV_STAGING/scribe-lifecycle-host.sh" || return 1
  snapshot_copy_optional "$SCRIPTLET" "$PREV_STAGING/Scribe.sh" || return 1
  snapshot_copy_optional "$APP_ROOT/assets/scribe-icon.png" "$PREV_STAGING/scribe-icon.png" || return 1
  snapshot_copy_optional "$APP_ROOT/assets/licenses/noto-emoji/OFL.txt" "$PREV_STAGING/OFL.txt" || return 1
  snapshot_copy_optional "$APP_ROOT/assets/licenses/noto-emoji/DESCRIPTION.en_us.html" \
    "$PREV_STAGING/DESCRIPTION.en_us.html" || return 1
  snapshot_copy_optional "$APP_ROOT/assets/licenses/noto-emoji/upstream_info.md" \
    "$PREV_STAGING/upstream_info.md" || return 1
  snapshot_copy_optional "$APP_ROOT/assets/licenses/source-sans-3/OFL.txt" \
    "$PREV_STAGING/source-sans-3-OFL.txt" || return 1
  for helper in install-runtime.sh uninstall-runtime.sh backup-runtime.sh register-helpers.sh install-recovery.sh; do
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

verify_or_complete_snapshot_entry() {
  name=$1
  live=$2
  retained=$PREV_DIR/$name
  refuse_symlink "$live" "live release file $name" || return 1
  refuse_symlink "$retained" "retained release file $name" || return 1
  if [ -e "$live" ] && [ ! -f "$live" ]; then
    log "lkg_invalid: live release path is not a regular file: $name"
    return 1
  fi
  if [ -e "$retained" ] && [ ! -f "$retained" ]; then
    log "lkg_invalid: retained release path is not a regular file: $name"
    return 1
  fi
  if [ -f "$retained" ]; then
    [ -f "$live" ] || {
      log "lkg_invalid: retained file has no matching live release file: $name"
      return 1
    }
    retained_bytes=$(file_bytes "$retained") || return 1
    live_bytes=$(file_bytes "$live") || return 1
    retained_sha=$(sha256_file "$retained") || return 1
    live_sha=$(sha256_file "$live") || return 1
    [ "$retained_bytes" = "$live_bytes" ] && [ "$retained_sha" = "$live_sha" ] || {
      log "lkg_invalid: existing retained file does not match live release A: $name"
      return 1
    }
    return 0
  fi
  [ -f "$live" ] || return 0

  complete_tmp=$PREV_DIR/.lkg-complete.$$.tmp
  refuse_symlink "$complete_tmp" "retained completion temporary" || return 1
  rm -f "$complete_tmp"
  cp -f "$live" "$complete_tmp" || return 1
  sync
  mv -f "$complete_tmp" "$retained" || return 1
  sync
  log "install_snapshot: completed legacy retained entry $name"
  return 0
}

# A legacy successful install already has previous-release=A, so the ordinary
# snapshot path intentionally does not replace it. Verify that it is the same A
# as the live release and atomically fill any release-owned entries that older
# snapshot code did not retain.
prepare_existing_snapshot() {
  validate_snapshot_tree "$PREV_DIR" || return 1
  verify_or_complete_snapshot_entry scribe-app "$STABLE_APP" || return 1
  verify_or_complete_snapshot_entry scribe-lifecycle "$STABLE_LIFE" || return 1
  verify_or_complete_snapshot_entry scribe-lifecycle-host.sh "$STABLE_HOST" || return 1
  verify_or_complete_snapshot_entry Scribe.sh "$SCRIPTLET" || return 1
  verify_or_complete_snapshot_entry scribe-icon.png "$APP_ROOT/assets/scribe-icon.png" || return 1
  verify_or_complete_snapshot_entry OFL.txt "$APP_ROOT/assets/licenses/noto-emoji/OFL.txt" || return 1
  verify_or_complete_snapshot_entry DESCRIPTION.en_us.html \
    "$APP_ROOT/assets/licenses/noto-emoji/DESCRIPTION.en_us.html" || return 1
  verify_or_complete_snapshot_entry upstream_info.md \
    "$APP_ROOT/assets/licenses/noto-emoji/upstream_info.md" || return 1
  verify_or_complete_snapshot_entry source-sans-3-OFL.txt \
    "$APP_ROOT/assets/licenses/source-sans-3/OFL.txt" || return 1
  verify_or_complete_snapshot_entry install-runtime.sh "$TOOLS_LIB/install-runtime.sh" || return 1
  verify_or_complete_snapshot_entry uninstall-runtime.sh "$TOOLS_LIB/uninstall-runtime.sh" || return 1
  verify_or_complete_snapshot_entry backup-runtime.sh "$TOOLS_LIB/backup-runtime.sh" || return 1
  verify_or_complete_snapshot_entry register-helpers.sh "$TOOLS_LIB/register-helpers.sh" || return 1
  verify_or_complete_snapshot_entry install-recovery.sh "$TOOLS_LIB/install-recovery.sh" || return 1
  verify_or_complete_snapshot_entry "Uninstall Ignite.sh" "$DOCS/Uninstall Ignite.sh" || return 1
  verify_or_complete_snapshot_entry "Backup Ignite Data.sh" "$DOCS/Backup Ignite Data.sh" || return 1
  verify_or_complete_snapshot_entry "Diagnose Ignite.sh" "$DOCS/Diagnose Ignite.sh" || return 1
  return 0
}

retain_lkg_handler_snapshot() {
  [ "${HAD_PREVIOUS:-0}" = "1" ] || return 0
  validate_snapshot_tree "$PREV_DIR" || return 1
  refuse_symlink "$LKG_HANDLER" "retained handler snapshot" || return 1
  handler_tmp=$PREV_DIR/.appreg-handler.${ACTIVATE_TXN}.tmp
  refuse_symlink "$handler_tmp" "retained handler temporary" || return 1
  rm -f "$handler_tmp"
  if [ "${IGNITE_SKIP_REGISTER:-}" = "1" ]; then
    rm -f "$LKG_HANDLER"
    return 0
  fi
  [ -n "${IGNITE_HANDLER_SNAP:-}" ] \
    && [ -f "$IGNITE_HANDLER_SNAP" ] \
    && [ ! -L "$IGNITE_HANDLER_SNAP" ] || {
      log "lkg_invalid: pre-upgrade Ignite handler snapshot is unavailable"
      return 1
    }
  cp -f "$IGNITE_HANDLER_SNAP" "$handler_tmp" || return 1
  chmod 0600 "$handler_tmp" 2>/dev/null || true
  sync
  mv -f "$handler_tmp" "$LKG_HANDLER" || return 1
  sync
  return 0
}

publish_lkg_inventory() {
  [ "${HAD_PREVIOUS:-0}" = "1" ] || return 0
  inventory_tmp=$PREV_DIR/.ignite-lkg-inventory.${ACTIVATE_TXN}.tmp
  refuse_symlink "$inventory_tmp" "retained inventory temporary" || return 1
  rm -f "$inventory_tmp"
  write_lkg_inventory "$PREV_DIR" "$inventory_tmp" || return 1
  chmod 0600 "$inventory_tmp" 2>/dev/null || true
  sync
  publish_crash before-lkg-inventory-publish
  mv -f "$inventory_tmp" "$LKG_INVENTORY" || return 1
  sync
  publish_crash after-lkg-inventory-publish
  LKG_INVENTORY_BYTES=$(file_bytes "$LKG_INVENTORY") || return 1
  LKG_INVENTORY_SHA=$(sha256_file "$LKG_INVENTORY") || return 1
  validate_lkg_inventory "$LKG_INVENTORY_BYTES" "$LKG_INVENTORY_SHA" || return 1
  return 0
}

publish_pending_health() {
  [ "${HAD_PREVIOUS:-0}" = "1" ] || return 0
  ensure_update_dir || return 1
  if [ -e "$PENDING_HEALTH" ] || [ -L "$PENDING_HEALTH" ]; then
    log "pending_health_invalid: refusing to overwrite existing pending-health record"
    return 1
  fi
  pending_tmp=$UPDATE_DIR/.pending-health.${ACTIVATE_TXN}.tmp
  refuse_symlink "$pending_tmp" "pending-health temporary" || return 1
  rm -f "$pending_tmp"
  candidate_app_bytes=$(file_bytes "$STABLE_APP") || return 1
  candidate_app_sha=$(sha256_file "$STABLE_APP") || return 1
  candidate_life_bytes=$(file_bytes "$STABLE_LIFE") || return 1
  candidate_life_sha=$(sha256_file "$STABLE_LIFE") || return 1
  {
    printf '%s\n' 'IGNITE_PENDING_HEALTH_V1'
    printf '%s\n' 'schema=1'
    printf '%s\n' 'status=pending-health'
    printf 'candidate_version=%s\n' "$CANDIDATE_VERSION"
    printf 'candidate_txn=%s\n' "$ACTIVATE_TXN"
    printf 'candidate_app_bytes=%s\n' "$candidate_app_bytes"
    printf 'candidate_app_sha256=%s\n' "$candidate_app_sha"
    printf 'candidate_lifecycle_bytes=%s\n' "$candidate_life_bytes"
    printf 'candidate_lifecycle_sha256=%s\n' "$candidate_life_sha"
    printf '%s\n' 'retained_version=unknown'
    printf 'retained_release_identity=sha256:%s\n' "$LKG_INVENTORY_SHA"
    printf 'lkg_inventory_bytes=%s\n' "$LKG_INVENTORY_BYTES"
    printf 'lkg_inventory_sha256=%s\n' "$LKG_INVENTORY_SHA"
    printf '%s\n' 'data_compatibility=unknown'
  } >"$pending_tmp" || return 1
  chmod 0600 "$pending_tmp" 2>/dev/null || true
  sync
  publish_crash before-pending-health-publish
  mv -f "$pending_tmp" "$PENDING_HEALTH" || return 1
  sync
  publish_crash after-pending-health-publish
  validate_pending_health || return 1
  return 0
}

commit_install_transaction() {
  mkdir -p "$APP_ROOT/run" || return 1
  printf 'ignite-activate-commit-v2 txn=%s pending_health=%s\n' \
    "${ACTIVATE_TXN:-unknown}" "${HAD_PREVIOUS:-0}" >"$COMMITTED.tmp" || return 1
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
    restore_cp_optional "$PREV_DIR/upstream_info.md" \
      "$APP_ROOT/assets/licenses/noto-emoji/upstream_info.md" upstream-info || return 1
    restore_cp_optional "$PREV_DIR/source-sans-3-OFL.txt" \
      "$APP_ROOT/assets/licenses/source-sans-3/OFL.txt" source-sans-ofl || return 1
    for helper in install-runtime.sh uninstall-runtime.sh backup-runtime.sh register-helpers.sh install-recovery.sh; do
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
command -v sha256sum >/dev/null 2>&1 || die "unsupported: sha256sum is required for retained-release integrity"

PACKAGE_NAME=${PACKAGE_ROOT##*/}
case "$PACKAGE_NAME" in
  Ignite-Kindle-*) CANDIDATE_VERSION=${PACKAGE_NAME#Ignite-Kindle-} ;;
  *) die "package directory name is malformed: $PACKAGE_NAME" ;;
esac
valid_release_version "$CANDIDATE_VERSION" \
  || die "package version is malformed or noncanonical: $CANDIDATE_VERSION"

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

# A committed candidate awaiting qualification owns the retained A snapshot.
# No installer invocation is authorized to replace either one in this slice.
guard_pending_health

cleanup_staged_new

# Snapshot last successful live files only if previous-release is empty.
if [ ! -f "$PREV_DIR/scribe-app" ] && [ -f "$STABLE_APP" ]; then
  snapshot_previous_release || die "could not snapshot current release for rollback"
fi
if [ -f "$PREV_DIR/scribe-app" ] && [ -f "$STABLE_APP" ]; then
  prepare_existing_snapshot || die "existing previous-release does not exactly match live release A"
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
  echo "# ignite-activate v2"
  echo "# txn=$ACTIVATE_TXN"
  echo "# package=$PACKAGE_ROOT"
  echo "# phase=pending"
  echo "# had_previous=$HAD_PREVIOUS"
  echo "# pending_health_required=$HAD_PREVIOUS"
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

# The retained application registration is scoped to Ignite only. It is copied
# into A before its canonical inventory is published; notebook/sync/device state
# is never part of this snapshot.
retain_lkg_handler_snapshot || fail_and_rollback "could not retain previous Ignite handler state"

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

# Static application verification has succeeded, but B is not yet transactionally
# committed and is not health-qualified. Any interruption before the commit marker
# remains an ordinary installer rollback to A.
publish_crash after-static-verification

if ! ignite_install_recovery "$PACKAGE_ROOT"; then
  fail_and_rollback "recovery component failed before application commit; retained application was restored"
fi

# For upgrades, keep A in previous-release and publish its exact inventory plus
# B's pending-health record before the durable commit marker. First installs have
# no A and therefore publish neither LKG nor pending-health state.
publish_lkg_inventory || fail_and_rollback "last-known-good inventory publication failed"
publish_pending_health || fail_and_rollback "pending-health publication failed"
publish_crash before-commit
commit_install_transaction || fail_and_rollback "durable commit marker failed"
publish_crash after-commit
publish_crash before-journal-cleanup
rm -f "$JOURNAL"
publish_crash before-prev-cleanup
rm -rf "$PREV_OLD"
rm -f "$COMMITTED" "$COMMITTED.tmp"

log "install_ok profile=full-feature package=$PACKAGE_ROOT recovery=installed spawned=no"
log "notebook data path untouched: $DATA_DIR"
log "Open Ignite from the Kindle Library to launch."
exit 0
