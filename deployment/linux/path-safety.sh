# Shared path canonicalization / rejection helpers for Ignite Server Linux scripts.
# Sourced by make-bundle.sh, install.sh, uninstall.sh. Not executed alone.
# shellcheck shell=bash

IGNITE_STAGING_MARKER=".ignite-staging-root"
IGNITE_BUNDLE_MARKER=".ignite-bundle"

# Resolve to an absolute path with symlinks expanded where components exist.
ignite_canonicalize() {
  local raw="$1"
  if command -v realpath >/dev/null 2>&1; then
    realpath -m -- "$raw" 2>/dev/null && return 0
  fi
  local dir base parent
  if [ -d "$raw" ]; then
    (cd "$raw" && pwd -P) && return 0
  fi
  dir="$(dirname -- "$raw")"
  base="$(basename -- "$raw")"
  if [ -d "$dir" ]; then
    parent="$(cd "$dir" && pwd -P)"
    printf '%s/%s\n' "$parent" "$base"
    return 0
  fi
  case "$raw" in
    /*) printf '%s\n' "$raw" ;;
    *) printf '%s/%s\n' "$(pwd -P)" "$raw" ;;
  esac
}

# True if $1 equals $2 or is an ancestor directory of $2 (i.e. $1 contains $2).
ignite_path_contains() {
  local outer="$1" inner="$2"
  [ -n "$outer" ] && [ -n "$inner" ] || return 1
  [ "$outer" = "$inner" ] && return 0
  case "$inner" in
    "$outer"/*) return 0 ;;
  esac
  return 1
}

# Exact protected paths (staging roots / exact out-dir equals).
ignite_is_protected_system_path() {
  local p="$1"
  case "$p" in
    /|/usr|/usr/local|/usr/local/bin|/usr/share|/usr/share/scribe-server|\
    /etc|/etc/scribe-server|/var|/var/lib|/var/lib/scribe-server|\
    /home|/root|/opt|/tmp|/boot|/dev|/proc|/sys|/run|/bin|/sbin|/lib|/lib64|\
    /etc/systemd|/etc/systemd/system)
      return 0 ;;
  esac
  return 1
}

# True if path is inside a hierarchy unsafe for staging roots (prefix match).
# Includes /home and /opt so staging never targets ordinary user/system trees.
# /tmp/<name> is allowed for disposable tests; /tmp itself is not.
# Not used for bundle out-dir validation (see ignite_is_dangerous_bundle_out_hierarchy).
ignite_is_under_system_hierarchy() {
  local p="$1"
  case "$p" in
    /etc|/etc/*|\
    /usr|/usr/*|\
    /var|/var/*|\
    /root|/root/*|\
    /boot|/boot/*|\
    /dev|/dev/*|\
    /proc|/proc/*|\
    /sys|/sys/*|\
    /run|/run/*|\
    /bin|/bin/*|\
    /sbin|/sbin/*|\
    /lib|/lib/*|\
    /lib64|/lib64/*|\
    /opt|/opt/*|\
    /home|/home/*)
      return 0 ;;
    /tmp) return 0 ;;
  esac
  return 1
}

# True if path is a dangerous bundle *output* location (prefix match).
# Unlike staging, ordinary checkouts under /home/... and /opt/... are allowed so
# GitHub Actions (/home/runner/work/...), developer homes, and /opt/ignite/dist
# can receive dist/ignite-server-linux. Exact /home and /opt remain protected
# via ignite_is_protected_system_path.
ignite_is_dangerous_bundle_out_hierarchy() {
  local p="$1"
  case "$p" in
    /etc|/etc/*|\
    /usr|/usr/*|\
    /var|/var/*|\
    /root|/root/*|\
    /boot|/boot/*|\
    /dev|/dev/*|\
    /proc|/proc/*|\
    /sys|/sys/*|\
    /run|/run/*|\
    /bin|/bin/*|\
    /sbin|/sbin/*|\
    /lib|/lib/*|\
    /lib64|/lib64/*)
      return 0 ;;
    /tmp) return 0 ;;
  esac
  return 1
}

# Walk each existing component of $root/$relpath; refuse symlink escapes outside $root.
# Does not create or delete anything. Prints nothing on success.
ignite_assert_path_under_staging_root() {
  local root="$1"
  local relpath="$2"
  local cur="$root"
  local part target resolved
  local IFS='/'
  # shellcheck disable=SC2086
  set -- $relpath
  unset IFS
  for part in "$@"; do
    [ -n "$part" ] || continue
    cur="$cur/$part"
    if [ -L "$cur" ]; then
      # Staging installs do not need symlinks; any symlink is refused.
      echo "refusing staging path is a symlink: $cur" >&2
      return 1
    elif [ -e "$cur" ]; then
      resolved="$(ignite_canonicalize "$cur")"
      if ! ignite_path_contains "$root" "$resolved"; then
        echo "refusing staging path outside root: $cur -> $resolved" >&2
        return 1
      fi
    fi
  done
  # Final resolved destination (including missing leaf) must stay under root.
  resolved="$(ignite_canonicalize "$root/$relpath")"
  if ! ignite_path_contains "$root" "$resolved"; then
    echo "refusing staging destination outside root: $root/$relpath -> $resolved" >&2
    return 1
  fi
  return 0
}

# Refuse any preexisting symlink under a staging root (staging does not require them).
ignite_assert_staging_no_symlinks() {
  local root="$1"
  local found
  [ -d "$root" ] || return 0
  found="$(find "$root" -mindepth 1 -type l -print 2>/dev/null | head -1 || true)"
  if [ -n "$found" ]; then
    echo "refusing staging root contains symlink: $found" >&2
    return 1
  fi
  return 0
}

# Validate all install/uninstall destinations under a staging root before mutation.
ignite_assert_staging_destinations_safe() {
  local root="$1"
  local rel
  ignite_assert_staging_no_symlinks "$root" || return 1
  # Marker must be a regular file when present (never a symlink to an outside target).
  if [ -e "$root/$IGNITE_STAGING_MARKER" ] || [ -L "$root/$IGNITE_STAGING_MARKER" ]; then
    if [ -L "$root/$IGNITE_STAGING_MARKER" ]; then
      echo "refusing staging marker is a symlink: $root/$IGNITE_STAGING_MARKER" >&2
      return 1
    fi
    if [ ! -f "$root/$IGNITE_STAGING_MARKER" ]; then
      echo "refusing staging marker is not a regular file: $root/$IGNITE_STAGING_MARKER" >&2
      return 1
    fi
    ignite_assert_path_under_staging_root "$root" "$IGNITE_STAGING_MARKER" || return 1
  fi
  for rel in \
    "$IGNITE_STAGING_MARKER" \
    usr/local/bin \
    usr/local/bin/scribe-server \
    usr/local/bin/scribe-server.old \
    usr/local/bin/scribe-server.new \
    usr/share/scribe-server \
    usr/share/scribe-server/web \
    usr/share/scribe-server/web.old \
    usr/share/scribe-server/web.new \
    etc/scribe-server \
    etc/systemd/system \
    etc/systemd/system/scribe-server.service \
    etc/systemd/system/scribe-server.service.old \
    etc/systemd/system/scribe-server.service.new \
    var/lib/scribe-server
  do
    ignite_assert_path_under_staging_root "$root" "$rel" || return 1
  done
  return 0
}

ignite_reject_staging_root() {
  local raw="$1"
  local label="${2:-staging root}"
  [ -n "$raw" ] || { echo "refusing empty ${label}" >&2; return 1; }
  case "$raw" in
    .|..|./|../) echo "refusing ambiguous ${label}: $raw" >&2; return 1 ;;
  esac
  local canon
  canon="$(ignite_canonicalize "$raw")"
  [ -n "$canon" ] || { echo "refusing unresolvable ${label}: $raw" >&2; return 1; }
  if ignite_is_protected_system_path "$canon"; then
    echo "refusing protected ${label}: $canon" >&2
    return 1
  fi
  if ignite_is_under_system_hierarchy "$canon"; then
    echo "refusing ${label} under system hierarchy: $canon" >&2
    return 1
  fi
  local live
  for live in /var/lib/scribe-server /etc/scribe-server /usr/share/scribe-server \
              /usr/local/bin/scribe-server /etc/systemd/system/scribe-server.service; do
    if ignite_path_contains "$canon" "$(ignite_canonicalize "$live" 2>/dev/null || echo "$live")"; then
      echo "refusing ${label} that contains live install path $live: $canon" >&2
      return 1
    fi
  done
  [ "$canon" != "/" ] || { echo "refusing ${label}: /" >&2; return 1; }
  printf '%s\n' "$canon"
  return 0
}

ignite_require_staging_marker() {
  local root="$1"
  local marker="$root/$IGNITE_STAGING_MARKER"
  if [ -L "$marker" ]; then
    echo "refusing destructive staging op: $IGNITE_STAGING_MARKER is a symlink in $root" >&2
    return 1
  fi
  [ -f "$marker" ] || {
    echo "refusing destructive staging op without $IGNITE_STAGING_MARKER in $root" >&2
    return 1
  }
}

# True if directory looks like an Ignite Server portable bundle (safe to replace).
# Requires the exact .ignite-bundle marker with valid content (not a symlink).
ignite_is_ignite_bundle_dir() {
  local d="$1"
  local marker="$d/$IGNITE_BUNDLE_MARKER"
  [ -d "$d" ] || return 1
  [ ! -L "$marker" ] || return 1
  [ -f "$marker" ] || return 1
  grep -q '^ignite-bundle=1$' "$marker" || return 1
  [ -f "$d/scribe-server" ] || return 1
  [ ! -L "$d/scribe-server" ] || return 1
  [ -f "$d/install.sh" ] || return 1
  [ -f "$d/uninstall.sh" ] || return 1
  [ -d "$d/web" ] || return 1
  [ -f "$d/web/index.html" ] || return 1
  [ -f "$d/scribe-server.service" ] || return 1
  return 0
}

# Pure (non-mutating) rejection of a proposed bundle out-dir.
# Args: out_abs repo_root here_abs bin_abs web_abs
# Exit 0 if acceptable; 1 with message on stderr if refused.
#
# Staging-root hierarchy rules are intentionally separate: this allows the
# repository dist/ tree (and custom outputs under /home or /opt) while still
# refusing sensitive system destinations.
ignite_reject_bundle_out_dir() {
  local out_abs="$1" repo_root="$2" here_abs="$3" bin_abs="$4" web_abs="$5"
  [ -n "$out_abs" ] || { echo "refusing empty out-dir" >&2; return 1; }
  case "$out_abs" in
    /|.|..) echo "refusing unsafe out-dir: $out_abs" >&2; return 1 ;;
  esac
  if ignite_is_protected_system_path "$out_abs"; then
    echo "refusing protected system out-dir: $out_abs" >&2
    return 1
  fi

  # Legitimate build outputs: repo_root/dist/... even when the checkout lives
  # under /home (GitHub Actions) or /opt. Evaluated before dangerous-hierarchy
  # so ordinary CI paths are not blanket-rejected.
  local in_repo_dist=0
  if ignite_path_contains "$repo_root" "$out_abs"; then
    case "$out_abs" in
      "$repo_root"/dist|"$repo_root"/dist/*) in_repo_dist=1 ;;
      *) echo "refusing out-dir inside repository outside dist/: $out_abs" >&2; return 1 ;;
    esac
  fi

  if [ "$in_repo_dist" != 1 ] && ignite_is_dangerous_bundle_out_hierarchy "$out_abs"; then
    echo "refusing out-dir under sensitive system hierarchy: $out_abs" >&2
    return 1
  fi

  local protected
  for protected in "$repo_root" "$bin_abs" "$web_abs" "$here_abs"; do
    if ignite_path_contains "$out_abs" "$protected"; then
      echo "refusing out-dir that equals or contains protected path $protected: $out_abs" >&2
      return 1
    fi
  done

  local bin_dir
  bin_dir="$(ignite_canonicalize "$(dirname -- "$bin_abs")")"
  if [ "$out_abs" = "$bin_dir" ]; then
    echo "refusing out-dir equal to binary directory: $out_abs" >&2
    return 1
  fi
  if [ -e "$out_abs" ]; then
    [ -d "$out_abs" ] || { echo "refusing out-dir exists and is not a directory: $out_abs" >&2; return 1; }
    if ! ignite_is_ignite_bundle_dir "$out_abs"; then
      echo "refusing to replace existing directory that is not an Ignite Server bundle: $out_abs" >&2
      return 1
    fi
  fi
  return 0
}
