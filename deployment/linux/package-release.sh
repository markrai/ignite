#!/usr/bin/env bash
# Package an assembled, tested Ignite Server Linux bundle as a GitHub Release asset:
#   deployment/linux/package-release.sh <bundle-dir> <out-dir>
#     → <out-dir>/ignite-server-<version>-linux-<arch>.tar.gz (+ .sha256, sha256sum format)
#
# The version and arch come from the bundle metadata and must agree with its binary. The
# archive holds one top-level directory with regular files and directories only; this is
# exactly what ignite-update accepts.
set -euo pipefail

[ "$#" -eq 2 ] || { echo "usage: $0 <bundle-dir> <out-dir>" >&2; exit 2; }
BUNDLE="$(cd "$1" && pwd)"
OUT="$2"
die() { echo "package-release.sh: $*" >&2; exit 1; }

MARKER="$BUNDLE/.ignite-bundle"
[ -f "$MARKER" ] && [ ! -L "$MARKER" ] || die "not an Ignite Server bundle: $BUNDLE"
VERSION="$(sed -n 's/^version=//p' "$MARKER")"
ARCH="$(sed -n 's/^arch=//p' "$MARKER")"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || die "bundle version missing or malformed: ${VERSION:-none}"
case "$ARCH" in x86_64|aarch64|armv7l) ;; *) die "bundle arch missing or unsupported: ${ARCH:-none}" ;; esac
BIN_VER="$("$BUNDLE/scribe-server" --version | tr -d '[:space:]')"
[ "$BIN_VER" = "$VERSION" ] || die "binary version $BIN_VER != bundle version $VERSION"
[ -x "$BUNDLE/ignite-update" ] || die "bundle has no ignite-update"
if [ -n "$(find "$BUNDLE" \( -type l -o -type b -o -type c -o -type p -o -type s \) -print -quit)" ]; then
  die "bundle contains symlinks or special files"
fi

TOP="ignite-server-$VERSION-linux-$ARCH"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp -R "$BUNDLE" "$STAGE/$TOP"
tar -C "$STAGE" --sort=name --owner=0 --group=0 --numeric-owner -czf "$OUT/$TOP.tar.gz" "$TOP"
(cd "$OUT" && sha256sum "$TOP.tar.gz" >"$TOP.tar.gz.sha256")
echo "release asset: $OUT/$TOP.tar.gz ($(cut -d' ' -f1 "$OUT/$TOP.tar.gz.sha256"))"
