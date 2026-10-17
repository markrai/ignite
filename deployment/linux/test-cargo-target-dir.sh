#!/usr/bin/env bash
# Prove the release binary is taken from CARGO_TARGET_DIR, not a stale default target/.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
CUSTOM="$(mktemp -d)"
OUT_PARENT="$(mktemp -d)"
OUT="$OUT_PARENT/bundle"
trap 'rm -rf "$CUSTOM" "$OUT_PARENT"' EXIT

VERSION="$(tr -d '[:space:]' <ignite/VERSION)"
echo "test-cargo-target-dir: CARGO_TARGET_DIR=$CUSTOM"
CARGO_TARGET_DIR="$CUSTOM" cargo build -p scribe-server --release --locked
BIN="$CUSTOM/release/scribe-server"
[ -f "$BIN" ] || { echo "FAIL: missing $BIN" >&2; exit 1; }
[ "$("$BIN" --version | tr -d '[:space:]')" = "$VERSION" ] || {
  echo "FAIL: version mismatch" >&2
  exit 1
}

WEB_DIST="$ROOT/ignite/web/dist"
if [ ! -f "$WEB_DIST/index.html" ]; then
  echo "test-cargo-target-dir: building web dist"
  (cd ignite/web && npm ci && npm run build)
fi

# Mirror build-bundle.sh locator rules.
TARGET_DIR="$CUSTOM"
LOCATED="$TARGET_DIR/release/scribe-server"
[ -f "$LOCATED" ] || { echo "FAIL: build-bundle locator would miss binary" >&2; exit 1; }

bash "$ROOT/deployment/linux/make-bundle.sh" "$LOCATED" "$WEB_DIST" "$OUT"
cmp -s "$LOCATED" "$OUT/scribe-server" || {
  echo "FAIL: bundled binary differs from CARGO_TARGET_DIR artifact" >&2
  exit 1
}
echo "CARGO_TARGET_DIR OK bin=$LOCATED"
