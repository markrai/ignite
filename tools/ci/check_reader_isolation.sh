#!/usr/bin/env bash
# Fail CI if the experimental reader engine (MuPDF) can reach an ordinary or release build.
# The `reader` feature of scribe-app must stay off by default, no release or packaging script
# may enable it, and builds without it must not depend on mupdf at all.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

fail() { echo "READER ISOLATION FAIL: $*" >&2; exit 1; }

manifest=ignite/kindle/scribe-app/Cargo.toml

# 1) `reader` is not a default feature.
if grep -nE '^default\s*=.*reader' "$manifest" >/dev/null; then
  fail "scribe-app enables reader by default"
fi

# 2) No packaging or release script turns it on.
if grep -rnE 'reader' tools/docker_build_armv7hf.sh scripts/kindle tools/release deployment/kindle \
  --include='*.sh' --include='*.py' --include='*.ps1' 2>/dev/null | grep -E 'features|scribe-app/reader' ; then
  fail "a release or packaging script enables the reader feature"
fi

# 3) Without the feature, mupdf is not in the dependency graph, on the host or the Kindle target
#    (with the official release features).
for args in \
  "" \
  "--target armv7-unknown-linux-gnueabihf --features restore_from_server"
do
  # shellcheck disable=SC2086
  if cargo tree -p scribe-app --locked -e normal,build --prefix none $args | grep -E '^mupdf' ; then
    fail "mupdf is in scribe-app's dependency graph without the reader feature ($args)"
  fi
done

# 4) With the feature it is (so check 3 is not vacuous).
cargo tree -p scribe-app --locked -e normal --prefix none --features reader | grep -qE '^mupdf v' \
  || fail "mupdf is missing from the graph even with --features reader"

echo "reader isolation: ok"
