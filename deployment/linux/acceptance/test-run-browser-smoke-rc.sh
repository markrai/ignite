#!/usr/bin/env bash
# Behavioral regression: run_browser_smoke status capture under set -e.
# Uses disposable mocks — does not launch Chromium or Multipass.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/ignite-smoke-rc.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

# --- Reproduce the fixed invoke pattern (mirrors guest-acceptance.sh) ---
SMOKE_DIR="$TMP/browser-smoke"
EVIDENCE="$TMP/evidence"
SETUP_CODE="ab12-cd34"
mkdir -p "$EVIDENCE"

run_browser_smoke() {
  local smoke_js="$SMOKE_DIR/smoke.mjs"
  [ -f "$smoke_js" ] || return 2
  command -v node >/dev/null 2>&1 || return 3
  [ -d "$SMOKE_DIR/node_modules/playwright" ] || return 4
  export PLAYWRIGHT_BROWSERS_PATH="${PLAYWRIGHT_BROWSERS_PATH:-$TMP/ms-playwright}"
  mkdir -p "$EVIDENCE"
  local -a run_cmd=(env
    "IGNITE_ACCEPT_SETUP_CODE=$SETUP_CODE"
    "IGNITE_ACCEPT_BASE_URL=http://127.0.0.1:8787"
    "IGNITE_ACCEPT_EVIDENCE=$EVIDENCE"
    "IGNITE_ACCEPT_FIXTURES=$SMOKE_DIR/fixtures"
    "PLAYWRIGHT_BROWSERS_PATH=$PLAYWRIGHT_BROWSERS_PATH"
    node "$smoke_js"
  )
  # No set +e / set -e inside the function.
  "${run_cmd[@]}" || return $?
  return 0
}

fail_msg=""
fail() { fail_msg="$*"; return 1; }

classify_optional() {
  # Same branching as guest-acceptance optional path.
  local smoke_rc=0
  run_browser_smoke || smoke_rc=$?
  if [ "$smoke_rc" -eq 0 ]; then
    echo "playwright-ok"
  elif [ "$smoke_rc" -eq 2 ] || [ "$smoke_rc" -eq 3 ] || [ "$smoke_rc" -eq 4 ]; then
    echo "not-attempted:$smoke_rc"
  else
    echo "attempted-failed:$smoke_rc"
  fi
}

# 1) Missing smoke → not attempted (2)
rm -rf "$SMOKE_DIR"
out="$(classify_optional)"
[ "$out" = "not-attempted:2" ] || { echo "FAIL: expected not-attempted:2 got $out" >&2; exit 1; }

# 2) Missing playwright deps → not attempted (4)
mkdir -p "$SMOKE_DIR"
printf '%s\n' 'process.exit(0)' >"$SMOKE_DIR/smoke.mjs"
out="$(classify_optional)"
[ "$out" = "not-attempted:4" ] || { echo "FAIL: expected not-attempted:4 got $out" >&2; exit 1; }

# 3) Attempted failure: mock smoke exits 7; under set -e caller must capture, not abort.
mkdir -p "$SMOKE_DIR/node_modules/playwright"
printf '%s\n' 'console.error("mock fail"); process.exit(7)' >"$SMOKE_DIR/smoke.mjs"
# Prove set -e does not kill the capture path:
set -e
smoke_rc=0
run_browser_smoke || smoke_rc=$?
[ "$smoke_rc" -eq 7 ] || { echo "FAIL: expected smoke_rc=7 got $smoke_rc" >&2; exit 1; }
out="$(classify_optional)"
[ "$out" = "attempted-failed:7" ] || { echo "FAIL: expected attempted-failed:7 got $out" >&2; exit 1; }

# 4) Success path
printf '%s\n' 'process.exit(0)' >"$SMOKE_DIR/smoke.mjs"
out="$(classify_optional)"
[ "$out" = "playwright-ok" ] || { echo "FAIL: expected playwright-ok got $out" >&2; exit 1; }

# 5) guest-acceptance.sh must not toggle set -e inside run_browser_smoke
guest="$ROOT/guest-acceptance.sh"
# Extract the function body roughly and reject set +/-e inside it.
awk '
  /^run_browser_smoke\(\)/ { in_fn=1; depth=0 }
  in_fn {
    print
    gsub(/[^\{\}]/, "")
    for (i=1;i<=length($0);i++) {
      c=substr($0,i,1)
      if (c=="{") depth++
      if (c=="}") { depth--; if (depth==0) { in_fn=0; exit } }
    }
  }
' "$guest" >"$TMP/fn.txt"
# Match actual errexit toggles only (not the word in comments).
if grep -E '^[[:space:]]*set [+-]e([[:space:]]|$)' "$TMP/fn.txt" >/dev/null; then
  echo "FAIL: run_browser_smoke still manipulates errexit (set +/-e)" >&2
  cat "$TMP/fn.txt" >&2
  exit 1
fi
# Callers must use || smoke_rc=$? capture form (not bare assignment after set +e).
grep -q 'run_browser_smoke || smoke_rc=\$?' "$guest" \
  || { echo "FAIL: guest-acceptance.sh must capture via run_browser_smoke || smoke_rc=\$?" >&2; exit 1; }

echo "PASS: run_browser_smoke rc capture behavioral regression"
exit 0
