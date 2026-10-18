#!/usr/bin/env bash
# Fail if an evidence directory contains secret-bearing material.
# Used by CI before artifact upload and by accept-safety regressions.
set -euo pipefail

DIR="${1:-}"
[ -n "$DIR" ] || { echo "usage: assert-evidence-clean.sh <evidence-dir>" >&2; exit 2; }
[ -d "$DIR" ] || { echo "assert-evidence-clean: no directory (ok): $DIR"; exit 0; }

fail() { echo "assert-evidence-clean: FAIL: $*" >&2; exit 1; }

# Patterns that must never appear in uploaded/retained evidence.
if grep -RqE \
  -e 'setup_code=[A-Z0-9]{4}-[A-Z0-9]{4}' \
  -e 'scribe_session=[A-Za-z0-9+/=._-]{8,}' \
  -e 'Authorization:[[:space:]]*Bearer[[:space:]]+[^[:space:]]+' \
  -e '"setup_code"[[:space:]]*:[[:space:]]*"[A-Z0-9]{4}-[A-Z0-9]{4}"' \
  -e 'Set-Cookie:.*scribe_session=[^<]' \
  "$DIR" 2>/dev/null; then
  fail "secret-bearing material found under $DIR"
fi

# Dotfiles that look like claim scratch must not live in evidence.
if find "$DIR" -name '.claim*' -o -name '*.raw' 2>/dev/null | grep -q .; then
  fail "raw/scratch claim artifacts present under $DIR"
fi

echo "assert-evidence-clean: OK $DIR"
exit 0
