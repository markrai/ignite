#!/usr/bin/env bash
# Host-transferred helper for Windows Multipass (run-host.ps1).
# Normalizes the uploaded bundle layout, strips CRLF from scripts, then runs
# guest-acceptance.sh. Keep this file LF-only.
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

file /home/ubuntu/ignite-bundle/scribe-server | grep -qi ELF || {
  echo "bundle binary is not ELF" >&2
  exit 1
}

export IGNITE_ACCEPT_EVIDENCE=/home/ubuntu/evidence
export IGNITE_ACCEPT_PLATFORM_CLASS="${IGNITE_ACCEPT_PLATFORM_CLASS:-true-vm}"
export IGNITE_ACCEPT_PLATFORM_DRIVER="${IGNITE_ACCEPT_PLATFORM_DRIVER:-multipass-24.04}"
# IGNITE_ACCEPT_MULTIPASS_BACKEND is optional; set by the host wrapper when known.

sudo -E bash /home/ubuntu/guest-acceptance.sh /home/ubuntu/ignite-bundle
