#!/usr/bin/env bash
# Build Ignite Server + ignite-web from this repository and assemble a Linux bundle.
#
#   deployment/linux/build-bundle.sh [out-dir]
#
# Honors CARGO_TARGET_DIR when set (absolute or relative to the repo root).
# Uses the authoritative `npm run build` for ignite-web (Cargo WASM + hash verify).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="${1:-$ROOT/dist/ignite-server-linux}"
cd "$ROOT"

VERSION="$(tr -d '[:space:]' <ignite/VERSION)"
[ -n "$VERSION" ] || { echo "build-bundle.sh: ignite/VERSION missing" >&2; exit 1; }

# Resolve Cargo target dir the same way Cargo does when CARGO_TARGET_DIR is set.
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
  case "$CARGO_TARGET_DIR" in
    /*) TARGET_DIR="$CARGO_TARGET_DIR" ;;
    *) TARGET_DIR="$ROOT/$CARGO_TARGET_DIR" ;;
  esac
else
  TARGET_DIR="$ROOT/target"
fi
# Normalize for existence checks after build.
mkdir -p "$TARGET_DIR"

echo "build-bundle: Ignite Server / ignite-web ${VERSION}"
echo "build-bundle: cargo build -p scribe-server --release --locked (target dir: $TARGET_DIR)"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build -p scribe-server --release --locked

BIN="$TARGET_DIR/release/scribe-server"
if [ ! -f "$BIN" ] && [ -f "$TARGET_DIR/release/scribe-server.exe" ]; then
  BIN="$TARGET_DIR/release/scribe-server.exe"
fi
[ -f "$BIN" ] || {
  echo "build-bundle.sh: binary missing after cargo build: $BIN" >&2
  echo "build-bundle.sh: (CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-<unset>})" >&2
  exit 1
}

BIN_VER="$("$BIN" --version | tr -d '[:space:]')"
[ "$BIN_VER" = "$VERSION" ] || {
  echo "build-bundle.sh: binary version $BIN_VER != ignite/VERSION $VERSION" >&2
  exit 1
}

PKG_VER="$(node -p "require('./ignite/web/package.json').version" | tr -d '[:space:]')"
[ "$PKG_VER" = "$VERSION" ] || {
  echo "build-bundle.sh: ignite/web package.json version $PKG_VER != $VERSION" >&2
  exit 1
}

echo "build-bundle: npm ci && npm run build (authoritative WASM)"
(
  cd ignite/web
  if [ ! -d node_modules ]; then npm ci; else npm ci --prefer-offline; fi
  npm run build
)

WEB_DIST="$ROOT/ignite/web/dist"
[ -f "$WEB_DIST/index.html" ] || { echo "build-bundle.sh: web dist missing" >&2; exit 1; }
MANIFEST_ID="$(sed -n 's/.*"id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$WEB_DIST/manifest.webmanifest" | head -1)"
[ "$MANIFEST_ID" = "/ignite-web" ] || {
  echo "build-bundle.sh: dist manifest id must be /ignite-web (got ${MANIFEST_ID:-missing})" >&2
  exit 1
}

echo "build-bundle: make-bundle → $OUT"
bash "$ROOT/deployment/linux/make-bundle.sh" "$BIN" "$WEB_DIST" "$OUT"
echo "build-bundle: OK $OUT (binary from $BIN)"
