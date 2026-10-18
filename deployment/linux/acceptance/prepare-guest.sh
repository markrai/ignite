#!/usr/bin/env bash
# Host-transferred helper for Windows Multipass (run-host.ps1).
# Normalizes the uploaded bundle layout, strips CRLF from scripts, installs
# browser-smoke when present, then runs guest-acceptance.sh. Keep LF-only.
set -euo pipefail

UPLOAD=/home/ubuntu/bundle-upload
if [ -f "$UPLOAD/install.sh" ]; then
  BUNDLE_DIR=$UPLOAD
elif [ -f "$UPLOAD/ignite-server-linux/install.sh" ]; then
  BUNDLE_DIR=$UPLOAD/ignite-server-linux
else
  BUNDLE_DIR="$(find "$UPLOAD" -maxdepth 2 -name install.sh -printf '%h\n' | head -1 || true)"
fi

if [ -z "${BUNDLE_DIR:-}" ] || [ ! -f "$BUNDLE_DIR/install.sh" ]; then
  echo "bundle layout not found under $UPLOAD" >&2
  ls -laR "$UPLOAD" >&2 || true
  exit 1
fi

sudo mkdir -p /home/ubuntu/ignite-bundle
sudo cp -a "$BUNDLE_DIR/." /home/ubuntu/ignite-bundle/
sudo chmod +x \
  /home/ubuntu/guest-acceptance.sh \
  /home/ubuntu/prepare-guest.sh \
  /home/ubuntu/ignite-bundle/install.sh \
  /home/ubuntu/ignite-bundle/uninstall.sh \
  /home/ubuntu/ignite-bundle/scribe-server

# Strip CRLF if transferred from Windows.
sudo sed -i 's/\r$//' /home/ubuntu/guest-acceptance.sh /home/ubuntu/prepare-guest.sh
sudo find /home/ubuntu/ignite-bundle -name '*.sh' -exec sed -i 's/\r$//' {} +
if [ -f /home/ubuntu/browser-smoke/install-browser-smoke.sh ]; then
  sudo sed -i 's/\r$//' /home/ubuntu/browser-smoke/install-browser-smoke.sh \
    /home/ubuntu/browser-smoke/*.mjs 2>/dev/null || true
  sudo chmod +x /home/ubuntu/browser-smoke/install-browser-smoke.sh
fi

file /home/ubuntu/ignite-bundle/scribe-server | grep -qi ELF || {
  echo "bundle binary is not ELF" >&2
  exit 1
}

if [ "${IGNITE_ACCEPT_REQUIRE_BROWSER:-0}" = "1" ]; then
  if [ ! -d /home/ubuntu/browser-smoke ]; then
    echo "IGNITE_ACCEPT_REQUIRE_BROWSER=1 but /home/ubuntu/browser-smoke missing" >&2
    exit 1
  fi
  echo "prepare-guest: installing browser-smoke dependencies…"
  export PLAYWRIGHT_BROWSERS_PATH="${PLAYWRIGHT_BROWSERS_PATH:-/home/ubuntu/.cache/ms-playwright}"
  sudo mkdir -p "$PLAYWRIGHT_BROWSERS_PATH"
  sudo chown -R ubuntu:ubuntu /home/ubuntu/browser-smoke "$PLAYWRIGHT_BROWSERS_PATH"
  # Root: Node 20 + Playwright OS shared libs. Ubuntu: npm ci + Chromium browser cache.
  echo "prepare-guest: ensure Node >= 20…"
  sudo env IGNITE_ACCEPT_NODE_ONLY=1 \
    bash /home/ubuntu/browser-smoke/install-browser-smoke.sh /home/ubuntu/browser-smoke
  NPM_TIMEOUT_SEC="${IGNITE_ACCEPT_NPM_TIMEOUT_SEC:-300}"
  PW_TIMEOUT_SEC="${IGNITE_ACCEPT_PLAYWRIGHT_TIMEOUT_SEC:-420}"
  run_bounded() {
    local secs="$1"; shift
    if command -v timeout >/dev/null 2>&1; then timeout --foreground "$secs" "$@"; else "$@"; fi
  }
  echo "prepare-guest: npm ci + playwright install-deps (root, timeout ${NPM_TIMEOUT_SEC}s / ${PW_TIMEOUT_SEC}s)…"
  run_bounded "$NPM_TIMEOUT_SEC" sudo bash -c 'cd /home/ubuntu/browser-smoke && npm ci --omit=dev --no-fund --no-audit'
  run_bounded "$PW_TIMEOUT_SEC" sudo bash -c 'cd /home/ubuntu/browser-smoke && npx playwright install-deps chromium'
  sudo chown -R ubuntu:ubuntu /home/ubuntu/browser-smoke
  echo "prepare-guest: playwright install chromium (ubuntu, timeout ${PW_TIMEOUT_SEC}s)…"
  run_bounded "$PW_TIMEOUT_SEC" sudo -u ubuntu env PLAYWRIGHT_BROWSERS_PATH="$PLAYWRIGHT_BROWSERS_PATH" \
    bash -c 'cd /home/ubuntu/browser-smoke && npx playwright install chromium'
  echo "prepare-guest: browser-smoke ready"
fi

export IGNITE_ACCEPT_EVIDENCE=/home/ubuntu/evidence
export IGNITE_ACCEPT_PLATFORM_CLASS="${IGNITE_ACCEPT_PLATFORM_CLASS:-true-vm}"
export IGNITE_ACCEPT_PLATFORM_DRIVER="${IGNITE_ACCEPT_PLATFORM_DRIVER:-multipass-24.04}"
export IGNITE_ACCEPT_BROWSER_SMOKE="${IGNITE_ACCEPT_BROWSER_SMOKE:-/home/ubuntu/browser-smoke}"
# IGNITE_ACCEPT_MULTIPASS_BACKEND / IGNITE_ACCEPT_REQUIRE_BROWSER set by host wrapper.

sudo -E bash /home/ubuntu/guest-acceptance.sh /home/ubuntu/ignite-bundle
