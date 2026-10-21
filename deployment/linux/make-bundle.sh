#!/usr/bin/env bash
# Assemble an Ignite Server Linux bundle from build outputs:
#   deployment/linux/make-bundle.sh <scribe-server-binary> <web-dist-dir> <out-dir>
#
# Stages on the destination filesystem, validates fully, then renames into place.
# Never chmod's the input binary. Never replaces arbitrary existing directories.
set -euo pipefail

usage() {
  echo "usage: $0 <scribe-server-binary> <web-dist-dir> <out-dir>" >&2
  exit 2
}
[ "$#" -eq 3 ] || usage
BIN="$1"
WEB="$2"
OUT="$3"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=path-safety.sh
. "$HERE/path-safety.sh"

die() { echo "make-bundle.sh: $*" >&2; exit 1; }

[ -f "$BIN" ] || die "binary not found: $BIN"
[ -f "$WEB/index.html" ] || die "web dist missing index.html: $WEB"
[ -f "$WEB/manifest.webmanifest" ] || die "web dist missing manifest.webmanifest: $WEB"

REPO_ROOT="$(ignite_canonicalize "$HERE/../..")"
HERE_ABS="$(ignite_canonicalize "$HERE")"
BIN_ABS="$(ignite_canonicalize "$BIN")"
WEB_ABS="$(ignite_canonicalize "$WEB")"

case "$OUT" in
  ""|"."|".."|"/" ) die "refusing unsafe out-dir: ${OUT:-<empty>}" ;;
esac

OUT_PARENT="$(dirname -- "$OUT")"
if [ ! -d "$OUT_PARENT" ]; then
  mkdir -p "$OUT_PARENT" || die "cannot create out-dir parent: $OUT_PARENT"
fi
OUT_PARENT_ABS="$(ignite_canonicalize "$OUT_PARENT")"
OUT_ABS="$(ignite_canonicalize "$OUT")"

ignite_reject_bundle_out_dir "$OUT_ABS" "$REPO_ROOT" "$HERE_ABS" "$BIN_ABS" "$WEB_ABS" \
  || die "out-dir rejected"

# Stage beside the destination so rename stays on the same filesystem.
STAGE="$(mktemp -d "$OUT_PARENT_ABS/.ignite-bundle-staging.XXXXXX")"
CLEANUP() { rm -rf "$STAGE"; }
trap CLEANUP EXIT

install -m 0755 "$BIN_ABS" "$STAGE/scribe-server"
mkdir -p "$STAGE/web"
cp -R "$WEB_ABS/." "$STAGE/web/"
if find "$STAGE/web" \( -type l -o -type b -o -type c -o -type p -o -type s \) -print | grep -q .; then
  die "web dist contains symlink or special file (refused)"
fi

install -m 0755 "$HERE_ABS/install.sh" "$HERE_ABS/uninstall.sh" "$STAGE/"
install -m 0755 "$HERE_ABS/update.sh" "$STAGE/ignite-update"
install -m 0644 "$HERE_ABS/firewall.sh" "$HERE_ABS/scribe-server.service" "$HERE_ABS/path-safety.sh" "$STAGE/"

VERSION="$("$STAGE/scribe-server" --version 2>/dev/null || true)"
[ -n "$VERSION" ] || die "bundled binary did not report --version"

MANIFEST_ID="$(sed -n 's/.*"id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$STAGE/web/manifest.webmanifest" | head -1)"
[ "$MANIFEST_ID" = "/ignite-web" ] || die "manifest id must be /ignite-web (got ${MANIFEST_ID:-missing})"

grep -Eqi 'setup_code|Bearer |scribe_session=' "$STAGE/web/index.html" \
  && die "bundle web/index.html contains a secret-looking string"

cat >"$STAGE/README.txt" <<TXT
Ignite Server (Linux portable self-hosting) — version ${VERSION}

  sudo ./install.sh                 install or upgrade; prints the URL and a one-time setup code
  sudo ./uninstall.sh               remove the service (keeps /var/lib/scribe-server)
  sudo ./uninstall.sh --purge       remove everything, including notebook data
  sudo ignite-update                later updates, from the official GitHub Releases

Browser UI: ignite-web (PWA id /ignite-web)
Trusted LAN only: plain HTTP with authenticated devices. Never forward a router port to it.

A binary built on one Linux host is not automatically compatible with every
libc version or CPU architecture — build or obtain a bundle for the target host.
TXT
printf 'ignite-bundle=1\nversion=%s\narch=%s\n' "$VERSION" "$(uname -m)" >"$STAGE/$IGNITE_BUNDLE_MARKER"

# Re-validate existing out-dir immediately before promote (TOCTOU).
if [ -e "$OUT_ABS" ]; then
  ignite_reject_bundle_out_dir "$OUT_ABS" "$REPO_ROOT" "$HERE_ABS" "$BIN_ABS" "$WEB_ABS" \
    || die "out-dir rejected before promote"
  BAK="${OUT_ABS}.prev.$$"
  if ! mv "$OUT_ABS" "$BAK"; then
    die "failed to move existing out-dir aside; left in place"
  fi
  if ! mv "$STAGE" "$OUT_ABS"; then
    mv "$BAK" "$OUT_ABS" || true
    die "failed to promote staged bundle; previous out-dir restored when possible"
  fi
  rm -rf "$BAK"
else
  if ! mv "$STAGE" "$OUT_ABS"; then
    die "failed to promote staged bundle"
  fi
fi
trap - EXIT
# Ensure a failed partial copy cannot look successful: require marker + binary.
[ -f "$OUT_ABS/$IGNITE_BUNDLE_MARKER" ] && [ -f "$OUT_ABS/scribe-server" ] \
  || die "promoted out-dir incomplete (refusing to report success)"
echo "bundle: $OUT_ABS (Ignite Server ${VERSION}, ignite-web id=/ignite-web)"
