#!/usr/bin/env bash
# Cross-compile the test-only Kindle supervisor harness (ARMv7).
# Not part of the customer Scriptlet / package-release path.
set -euo pipefail

apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq gcc-arm-linux-gnueabihf
rustup target add armv7-unknown-linux-gnueabihf

cd /src
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=/src=. --remap-path-prefix=/usr/local/cargo/registry=deps --remap-path-prefix=/usr/local/cargo/git=deps"
cargo build -p scribe-app --locked --release --target armv7-unknown-linux-gnueabihf \
  --bin kindle-supervisor-harness --features kindle_supervisor_harness

BIN=target/armv7-unknown-linux-gnueabihf/release/kindle-supervisor-harness
file "$BIN"
arm-linux-gnueabihf-readelf -h "$BIN" | sed -n '1,20p'
ls -la "$BIN"
if file "$BIN" | grep -q 'not stripped'; then
  echo "refusing unstripped harness binary: $BIN" >&2
  exit 1
fi
