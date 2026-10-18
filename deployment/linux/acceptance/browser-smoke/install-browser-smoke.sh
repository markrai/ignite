#!/usr/bin/env bash
# Install Node 20 (if needed), pinned npm deps, and Playwright Chromium.
# Visible progress; bounded timeouts. Keep LF-only.
#
# Env:
#   IGNITE_ACCEPT_NODE_ONLY=1  — only ensure Node >= 20 (root), then exit
set -euo pipefail

SMOKE_DIR="${1:-/home/ubuntu/browser-smoke}"
NPM_TIMEOUT_SEC="${IGNITE_ACCEPT_NPM_TIMEOUT_SEC:-300}"
PW_TIMEOUT_SEC="${IGNITE_ACCEPT_PLAYWRIGHT_TIMEOUT_SEC:-420}"
NODE_SETUP_TIMEOUT_SEC="${IGNITE_ACCEPT_NODE_SETUP_TIMEOUT_SEC:-180}"

cd "$SMOKE_DIR"

run_bounded() {
  local secs="$1"
  shift
  if command -v timeout >/dev/null 2>&1; then
    timeout --foreground "$secs" "$@"
  else
    "$@"
  fi
}

node_major() {
  if ! command -v node >/dev/null 2>&1; then
    echo 0
    return
  fi
  node -p "process.versions.node.split('.')[0]" 2>/dev/null || echo 0
}

ensure_node20() {
  local major
  major="$(node_major)"
  if [ "${major:-0}" -ge 20 ]; then
    echo "browser-smoke: node=$(node -v) (ok)"
    return 0
  fi
  echo "browser-smoke: Node.js $(node -v 2>/dev/null || echo none) < 20; installing Node 20 via NodeSource…"
  if [ "$(id -u)" -ne 0 ]; then
    echo "browser-smoke: Node 20 install requires root" >&2
    exit 1
  fi
  run_bounded "$NODE_SETUP_TIMEOUT_SEC" bash -c \
    'curl -fsSL https://deb.nodesource.com/setup_20.x | bash -'
  run_bounded "$NODE_SETUP_TIMEOUT_SEC" env DEBIAN_FRONTEND=noninteractive \
    apt-get install -y -qq nodejs
  echo "browser-smoke: node=$(node -v) npm=$(npm -v)"
  major="$(node_major)"
  if [ "${major:-0}" -lt 20 ]; then
    echo "browser-smoke: Node >= 20 still unavailable after install" >&2
    exit 1
  fi
}

ensure_node20

if [ "${IGNITE_ACCEPT_NODE_ONLY:-0}" = "1" ]; then
  exit 0
fi

if ! command -v npm >/dev/null 2>&1; then
  echo "browser-smoke: npm not found" >&2
  exit 1
fi
if [ ! -f package-lock.json ]; then
  echo "browser-smoke: package-lock.json missing (required for npm ci)" >&2
  exit 1
fi

echo "browser-smoke: npm ci (timeout ${NPM_TIMEOUT_SEC}s) in $SMOKE_DIR as $(id -un)"
run_bounded "$NPM_TIMEOUT_SEC" npm ci --omit=dev --no-fund --no-audit
echo "browser-smoke: playwright install chromium + OS deps (timeout ${PW_TIMEOUT_SEC}s)"
# OS deps need root; browser download can be user-local.
if [ "$(id -u)" -eq 0 ]; then
  run_bounded "$PW_TIMEOUT_SEC" npx playwright install --with-deps chromium
else
  # Deps may already be present from a prior root install-deps; try user browser install first.
  if ! run_bounded "$PW_TIMEOUT_SEC" npx playwright install chromium; then
    echo "browser-smoke: playwright install chromium failed as $(id -un); retry with sudo install-deps" >&2
    exit 1
  fi
fi
echo "browser-smoke: install complete"
