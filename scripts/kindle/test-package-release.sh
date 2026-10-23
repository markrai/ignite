#!/usr/bin/env bash
# Directory-order and host-writability checks for package-release.sh.
# Does not run Docker, Dropbear, or the packager.
set -euo pipefail
shopt -s inherit_errexit 2>/dev/null || true

ROOT="$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)"
WRAP="$ROOT/scripts/kindle/package-release.sh"
fail() { echo "test-package-release.sh: $*" >&2; exit 1; }

[ -f "$WRAP" ] || fail "missing $WRAP"

# Strip comments for order checks.
code="$(grep -vE '^\s*#' "$WRAP")"

echo "$code" | grep -q SkipBuild && fail "package-release.sh must not offer SkipBuild"
# Error text may mention chmod/chown/sudo; those commands must not run.
cmds="$(echo "$code" | grep -vE 'die[[:space:]]')"
echo "$cmds" | grep -qE '(^|[[:space:]])sudo[[:space:]]' && fail "package-release.sh must not use sudo"
echo "$cmds" | grep -qE '(^|[[:space:]])chmod[[:space:]]' && fail "package-release.sh must not chmod"
echo "$cmds" | grep -qE '(^|[[:space:]])chown[[:space:]]' && fail "package-release.sh must not chown"

# First docker run must follow ensure_host_owned_dir of $ROOT/target.
awk '
  /ensure_host_owned_dir[[:space:]]+"\$ROOT\/target"/ { ensure=NR }
  /docker[[:space:]]+run/ && !docker { docker=NR }
  /require_host_writable_dir[[:space:]]+"\$ROOT\/target"/ { require=NR }
  /docker_build\.sh/ && !dropbear { dropbear=NR }
  END {
    if (!ensure) { print "missing ensure_host_owned_dir of $ROOT/target"; exit 1 }
    if (!docker) { print "missing docker run"; exit 1 }
    if (!require) { print "missing require_host_writable_dir of $ROOT/target"; exit 1 }
    if (!dropbear) { print "missing docker_build.sh"; exit 1 }
    if (!(ensure < docker)) { print "target/ must be created before the first docker run"; exit 1 }
    if (!(docker < require)) { print "writability must be checked after the ARM docker run"; exit 1 }
    if (!(require < dropbear)) { print "writability must be checked before Dropbear"; exit 1 }
  }
' <<<"$code" || fail "directory-creation order is wrong"

# shellcheck disable=SC1090
# Helpers only; does not invoke Docker.
# shellcheck source=package-release.sh
. "$WRAP"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/ignite-pkg-target.XXXXXX")"
trap 'chmod -f u+w "$TMP/target" "$TMP/blocked" 2>/dev/null || true; rm -rf "$TMP"' EXIT

ensure_host_owned_dir "$TMP/target"
[ -d "$TMP/target" ] || fail "ensure_host_owned_dir did not create the directory"
[ -w "$TMP/target" ] || fail "created target/ is not writable"
probe="$TMP/target/host-sibling"
mkdir "$probe" || fail "host must be able to create a sibling under target/"
rmdir "$probe"
require_host_writable_dir "$TMP/target" "in the writable fixture"

mkdir -p "$TMP/blocked"
chmod a-w "$TMP/blocked"
if [ -w "$TMP/blocked" ]; then
  echo "SKIP: this filesystem ignores chmod a-w (cannot simulate a root-owned target/)"
else
  if out="$(ensure_host_owned_dir "$TMP/blocked" 2>&1)"; then
    fail "ensure_host_owned_dir must refuse an unwritable existing directory"
  fi
  echo "$out" | grep -q "not writable" || fail "unwritable error must mention not writable (got: $out)"
  echo "$out" | grep -qiE 'chmod 777|chown' || fail "unwritable error must tell the operator not to chmod 777 or chown (got: $out)"
  echo "$out" | grep -q "Remove" || fail "unwritable error must tell the operator to remove the directory (got: $out)"
  if out="$(require_host_writable_dir "$TMP/blocked" "after the ARM Docker build" 2>&1)"; then
    fail "require_host_writable_dir must refuse after ARM when target/ is not writable"
  fi
  echo "$out" | grep -q "after the ARM Docker build" || fail "post-ARM error must include the when= text (got: $out)"
fi

echo "PASS: package-release.sh host-owned target/ order and writability"
