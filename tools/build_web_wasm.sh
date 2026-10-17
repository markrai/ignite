#!/usr/bin/env bash
# Thin wrapper around the authoritative Node WASM builder.
# Prefer:  cd ignite/web && npm run build:wasm
# Or from repo root with Node available:
#   node ignite/web/scripts/build-wasm.mjs
#
# Docker (needs Node + Rust with wasm32), example:
#   docker run --rm -v "$PWD":/src -w /src node:20-bookworm bash -lc '
#     apt-get update -qq && apt-get install -y -qq curl build-essential
#     curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
#     . "$HOME/.cargo/env" && rustup target add wasm32-unknown-unknown
#     node ignite/web/scripts/build-wasm.mjs'
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if ! command -v node >/dev/null 2>&1; then
  echo "REFUSE: node is required to build ignite-web WASM (authoritative script is Node)." >&2
  echo "Install Node 20+ or use the Docker example in this script's header." >&2
  exit 1
fi
exec node "$ROOT/ignite/web/scripts/build-wasm.mjs"
