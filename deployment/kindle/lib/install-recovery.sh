#!/bin/sh
# Target A: install the provenance-verified Dropbear client under
# /var/local/ignite-recovery. Does not execute the client. Does not
# change appreg or ordinary launch. Does not chmod /var/local itself.
#
# Sourced from install-runtime.sh. Host tests set IGNITE_RECOVERY_*.
# shellcheck shell=sh

IGNITE_RECOVERY_ROOT="${IGNITE_RECOVERY_ROOT:-/var/local/ignite-recovery}"
IGNITE_RECOVERY_PARENT=$(dirname "$IGNITE_RECOVERY_ROOT")

ignite_recovery_die() {
  printf 'recovery_install_failed: %s\n' "$*" >&2
  return 1
}

ignite_recovery_log() {
  printf '%s\n' "$*"
}

# Refuse symlink names anywhere on the install path. Do not follow them.
ignite_recovery_refuse_symlink() {
  path=$1
  label=$2
  if [ -L "$path" ]; then
    ignite_recovery_die "$label is a symlink ($path); refusing to follow it"
    return 1
  fi
  return 0
}

ignite_recovery_sha256() {
  sha256sum "$1" | awk '{print $1}'
}

ignite_recovery_apply_modes() {
  root=$1
  if [ "${IGNITE_RECOVERY_SKIP_CHOWN:-}" != "1" ]; then
    if [ "${IGNITE_RECOVERY_FAIL_CHOWN:-}" = "1" ]; then
      ignite_recovery_die "injected ownership failure"
      return 1
    fi
    chown -R root:root "$root" || {
      ignite_recovery_die "chown root:root failed"
      return 1
    }
  fi
  if [ "${IGNITE_RECOVERY_FAIL_CHMOD:-}" = "1" ]; then
    ignite_recovery_die "injected permission failure"
    return 1
  fi
  # 0700 plus a-s: strip inherited setgid from a 2775 /var/local parent.
  chmod a-s "$root" \
    "$root/bin" "$root/trust" "$root/identity" "$root/manifest" "$root/previous" 2>/dev/null || true
  chmod 0700 "$root" \
    "$root/bin" "$root/trust" "$root/identity" "$root/manifest" "$root/previous" || {
    ignite_recovery_die "chmod 0700 failed"
    return 1
  }
  chmod 0755 "$root/bin/dropbearmulti" 2>/dev/null || true
  chmod 0600 "$root/manifest/"* 2>/dev/null || true
  return 0
}

ignite_recovery_verify_tree() {
  root=$1
  want=$2
  ignite_recovery_refuse_symlink "$root" "recovery root" || return 1
  ignite_recovery_refuse_symlink "$root/bin" "recovery bin" || return 1
  ignite_recovery_refuse_symlink "$root/bin/dropbearmulti" "recovery executable" || return 1
  [ -f "$root/bin/dropbearmulti" ] && [ -s "$root/bin/dropbearmulti" ] || {
    ignite_recovery_die "published executable missing"
    return 1
  }
  have=$(ignite_recovery_sha256 "$root/bin/dropbearmulti")
  [ "$have" = "$want" ] || {
    ignite_recovery_die "published digest mismatch (have $have want $want)"
    return 1
  }
  [ -f "$root/manifest/elf.sha256" ] || {
    ignite_recovery_die "manifest missing"
    return 1
  }
  if [ "${IGNITE_RECOVERY_SKIP_CHOWN:-}" != "1" ]; then
    mode=$(stat -c '%a' "$root")
    low=$mode
    while [ "${#low}" -gt 3 ]; do
      low=${low#?}
    done
    [ "$low" = "700" ] || {
      ignite_recovery_die "recovery root mode is $mode (want 0700)"
      return 1
    }
    owner=$(stat -c '%u' "$root")
    [ "$owner" = "0" ] || {
      ignite_recovery_die "recovery root uid is $owner (want 0)"
      return 1
    }
  fi
  return 0
}

ignite_recovery_remove_staging() {
  staging=$1
  if [ -L "$staging" ]; then
    ignite_recovery_log "recovery_cleanup: refusing to follow staging symlink $staging"
    return 0
  fi
  [ -d "$staging" ] || return 0
  rm -f "$staging/bin/dropbearmulti" \
    "$staging/bin/dropbearmulti.new" \
    "$staging/manifest/elf.sha256" \
    "$staging/manifest/provenance.txt" \
    "$staging/manifest/version"
  rmdir "$staging/bin" 2>/dev/null || true
  rmdir "$staging/trust" 2>/dev/null || true
  rmdir "$staging/identity" 2>/dev/null || true
  rmdir "$staging/manifest" 2>/dev/null || true
  rmdir "$staging/previous" 2>/dev/null || true
  rmdir "$staging" 2>/dev/null || true
}

# Remove only known recovery artifacts. Never rm -rf through a symlink.
ignite_recovery_uninstall() {
  root="${1:-$IGNITE_RECOVERY_ROOT}"
  if [ -L "$root" ]; then
    ignite_recovery_die "uninstall refused: $root is a symlink"
    return 1
  fi
  [ -e "$root" ] || {
    ignite_recovery_log "recovery_uninstall: absent"
    return 0
  }
  [ -d "$root" ] || {
    ignite_recovery_die "uninstall refused: $root is not a directory"
    return 1
  }
  ignite_recovery_refuse_symlink "$root/bin" "bin" || return 1
  ignite_recovery_refuse_symlink "$root/bin/dropbearmulti" "executable" || return 1
  rm -f "$root/bin/dropbearmulti" "$root/bin/dropbearmulti.new"
  rm -f "$root/previous/dropbearmulti" "$root/previous/elf.sha256"
  rm -f "$root/manifest/elf.sha256" "$root/manifest/provenance.txt" "$root/manifest/version"
  rmdir "$root/bin" 2>/dev/null || true
  rmdir "$root/trust" 2>/dev/null || true
  rmdir "$root/manifest" 2>/dev/null || true
  rmdir "$root/previous" 2>/dev/null || true
  # Preserve identity/ if a future install stored keys there.
  if [ -d "$root/identity" ]; then
    if [ -z "$(ls -A "$root/identity" 2>/dev/null || true)" ]; then
      rmdir "$root/identity" 2>/dev/null || true
    else
      ignite_recovery_log "recovery_uninstall: preserved non-empty identity/"
    fi
  fi
  rmdir "$root" 2>/dev/null || true
  ignite_recovery_log "recovery_uninstall_ok"
  return 0
}

ignite_install_recovery() {
  package_root=$1
  src_bin="$package_root/recovery/bin/dropbearmulti"
  src_sum="$package_root/recovery/manifest/elf.sha256"
  src_prov="$package_root/recovery/manifest/provenance.txt"

  umask 077

  if [ "$(id -u)" -ne 0 ] && [ "${IGNITE_RECOVERY_ALLOW_NONROOT:-}" != "1" ]; then
    ignite_recovery_die "requires the existing privileged installer context"
    return 1
  fi

  [ -f "$src_bin" ] && [ -s "$src_bin" ] || {
    ignite_recovery_die "package missing recovery/bin/dropbearmulti"
    return 1
  }
  [ -f "$src_sum" ] && [ -s "$src_sum" ] || {
    ignite_recovery_die "package missing recovery/manifest/elf.sha256"
    return 1
  }
  want=$(awk '{print $1; exit}' "$src_sum")
  [ -n "$want" ] || {
    ignite_recovery_die "empty packaged digest"
    return 1
  }
  got=$(ignite_recovery_sha256 "$src_bin")
  [ "$got" = "$want" ] || {
    ignite_recovery_die "source artifact digest mismatch"
    return 1
  }
  if [ "${IGNITE_SKIP_ELF_CHECK:-}" != "1" ]; then
    ignite_require_arm32le_elf "$src_bin" || {
      ignite_recovery_die "packaged recovery ELF is not ARM 32-bit little-endian"
      return 1
    }
  fi

  ignite_recovery_refuse_symlink "$IGNITE_RECOVERY_PARENT" "parent" || return 1
  ignite_recovery_refuse_symlink "$IGNITE_RECOVERY_ROOT" "recovery root" || return 1

  if [ -e "$IGNITE_RECOVERY_ROOT" ] && [ ! -d "$IGNITE_RECOVERY_ROOT" ]; then
    ignite_recovery_die "recovery path exists and is not a directory"
    return 1
  fi

  need_bytes=$(wc -c < "$src_bin" | tr -d ' ')
  need_kb=$((need_bytes / 1024 + 256))
  if [ "${IGNITE_SKIP_SPACE_CHECK:-}" != "1" ]; then
    if [ -n "${IGNITE_RECOVERY_AVAIL_KB:-}" ]; then
      avail=$IGNITE_RECOVERY_AVAIL_KB
    else
      avail=$(df -k "$IGNITE_RECOVERY_PARENT" 2>/dev/null | awk 'NR==2 {print $4}')
    fi
    [ -n "$avail" ] || {
      ignite_recovery_die "cannot measure free space"
      return 1
    }
    [ "$avail" -ge "$need_kb" ] || {
      ignite_recovery_die "not enough free space (need ${need_kb} KB, have ${avail} KB)"
      return 1
    }
  fi

  live="$IGNITE_RECOVERY_ROOT/bin/dropbearmulti"
  if [ -f "$live" ] && [ ! -L "$live" ]; then
    live_sum=$(ignite_recovery_sha256 "$live")
    if [ "$live_sum" = "$want" ]; then
      ignite_recovery_log "recovery_install: already current digest=$want"
      ignite_recovery_apply_modes "$IGNITE_RECOVERY_ROOT" || return 1
      ignite_recovery_verify_tree "$IGNITE_RECOVERY_ROOT" "$want" || return 1
      ignite_recovery_log "recovery_install_ok state=current"
      return 0
    fi
  fi

  staging="${IGNITE_RECOVERY_ROOT}.staging"
  if [ -L "$staging" ]; then
    ignite_recovery_die "staging path is a symlink"
    return 1
  fi
  ignite_recovery_remove_staging "$staging"
  mkdir -p "$staging/bin" "$staging/trust" "$staging/identity" "$staging/manifest" "$staging/previous" || {
    ignite_recovery_die "cannot create staging directories"
    return 1
  }

  cp -f "$src_bin" "$staging/bin/dropbearmulti.new" || {
    ignite_recovery_die "failed to stage executable"
    return 1
  }
  staged_sum=$(ignite_recovery_sha256 "$staging/bin/dropbearmulti.new")
  [ "$staged_sum" = "$want" ] || {
    ignite_recovery_remove_staging "$staging"
    ignite_recovery_die "staged copy digest mismatch"
    return 1
  }
  awk '{print $1; exit}' "$src_sum" >"$staging/manifest/elf.sha256"
  if [ -f "$src_prov" ]; then
    cp -f "$src_prov" "$staging/manifest/provenance.txt"
  fi
  printf '%s\n' "dropbear-2024.85" >"$staging/manifest/version"

  if [ "${IGNITE_RECOVERY_FAIL_AT:-}" = "truncated" ]; then
    dd if="$staging/bin/dropbearmulti.new" of="$staging/bin/dropbearmulti.new.trunc" bs=64 count=1 2>/dev/null || true
    mv -f "$staging/bin/dropbearmulti.new.trunc" "$staging/bin/dropbearmulti.new"
    ignite_recovery_die "injected truncated executable"
    return 1
  fi

  # Snapshot a known-good live binary into staging/previous before publish.
  if [ -f "$live" ] && [ ! -L "$live" ]; then
    cp -f "$live" "$staging/previous/dropbearmulti" || {
      ignite_recovery_remove_staging "$staging"
      ignite_recovery_die "failed to snapshot previous executable"
      return 1
    }
    ignite_recovery_sha256 "$live" >"$staging/previous/elf.sha256"
  fi

  if [ "${IGNITE_RECOVERY_FAIL_AT:-}" = "before-publish" ]; then
    ignite_recovery_log "recovery_install: injected interrupt before publish"
    return 1
  fi

  if [ ! -d "$IGNITE_RECOVERY_ROOT" ]; then
    if [ "${IGNITE_RECOVERY_FAIL_PUBLISH:-}" = "1" ]; then
      ignite_recovery_die "injected atomic publication failure"
      return 1
    fi
    mv -f "$staging/bin/dropbearmulti.new" "$staging/bin/dropbearmulti" || {
      ignite_recovery_die "failed to commit staged executable"
      return 1
    }
    mv -f "$staging" "$IGNITE_RECOVERY_ROOT" || {
      ignite_recovery_die "failed to publish recovery directory"
      return 1
    }
  else
    mkdir -p "$IGNITE_RECOVERY_ROOT/bin" "$IGNITE_RECOVERY_ROOT/trust" \
      "$IGNITE_RECOVERY_ROOT/identity" "$IGNITE_RECOVERY_ROOT/manifest" \
      "$IGNITE_RECOVERY_ROOT/previous"
    if [ -f "$staging/previous/dropbearmulti" ]; then
      cp -f "$staging/previous/dropbearmulti" "$IGNITE_RECOVERY_ROOT/previous/dropbearmulti"
      cp -f "$staging/previous/elf.sha256" "$IGNITE_RECOVERY_ROOT/previous/elf.sha256"
    fi
    cp -f "$staging/manifest/elf.sha256" "$IGNITE_RECOVERY_ROOT/manifest/elf.sha256.new"
    if [ -f "$staging/manifest/provenance.txt" ]; then
      cp -f "$staging/manifest/provenance.txt" "$IGNITE_RECOVERY_ROOT/manifest/provenance.txt.new"
    fi
    cp -f "$staging/manifest/version" "$IGNITE_RECOVERY_ROOT/manifest/version.new"
    if [ "${IGNITE_RECOVERY_FAIL_PUBLISH:-}" = "1" ]; then
      ignite_recovery_die "injected atomic publication failure"
      return 1
    fi
    mv -f "$staging/bin/dropbearmulti.new" "$IGNITE_RECOVERY_ROOT/bin/dropbearmulti" || {
      ignite_recovery_die "failed to publish executable"
      return 1
    }
    mv -f "$IGNITE_RECOVERY_ROOT/manifest/elf.sha256.new" "$IGNITE_RECOVERY_ROOT/manifest/elf.sha256"
    mv -f "$IGNITE_RECOVERY_ROOT/manifest/version.new" "$IGNITE_RECOVERY_ROOT/manifest/version"
    if [ -f "$IGNITE_RECOVERY_ROOT/manifest/provenance.txt.new" ]; then
      mv -f "$IGNITE_RECOVERY_ROOT/manifest/provenance.txt.new" "$IGNITE_RECOVERY_ROOT/manifest/provenance.txt"
    fi
    ignite_recovery_remove_staging "$staging"
  fi

  ignite_recovery_apply_modes "$IGNITE_RECOVERY_ROOT" || return 1
  ignite_recovery_verify_tree "$IGNITE_RECOVERY_ROOT" "$want" || {
    # Restore previous known-good if present.
    if [ -f "$IGNITE_RECOVERY_ROOT/previous/dropbearmulti" ]; then
      cp -f "$IGNITE_RECOVERY_ROOT/previous/dropbearmulti" "$IGNITE_RECOVERY_ROOT/bin/dropbearmulti" || true
    fi
    return 1
  }
  ignite_recovery_log "recovery_install_ok digest=$want root=$IGNITE_RECOVERY_ROOT spawned=no"
  return 0
}
