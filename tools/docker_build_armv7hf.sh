#!/usr/bin/env bash
set -euo pipefail

apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq gcc-arm-linux-gnueabihf
rustup target add armv7-unknown-linux-gnueabihf

# Ignite workspace Cargo.toml lives at the repository root (mounted at /src).
cd /src
# Remap container paths so panic locations cannot embed the Docker mount or cargo home.
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=/src=. --remap-path-prefix=/usr/local/cargo/registry=deps --remap-path-prefix=/usr/local/cargo/git=deps"
# Official customer build: Restore from Server is part of the release (its Vega-sized hardware
# memory test passed). No other optional feature (e.g. the supervisor test harness) is enabled.
cargo build -p ink-probe -p scribe-app -p scribe-lifecycle --locked --release --target armv7-unknown-linux-gnueabihf   --features scribe-app/restore_from_server

for name in ink-probe scribe-app scribe-lifecycle; do
  BIN=target/armv7-unknown-linux-gnueabihf/release/$name
  file "$BIN"
  arm-linux-gnueabihf-readelf -h "$BIN" | sed -n '1,20p'
  echo '--- interpreter ---'
  arm-linux-gnueabihf-readelf -l "$BIN" | grep -E 'INTERP|Requesting'
  echo '--- glibc needed ---'
  arm-linux-gnueabihf-objdump -T "$BIN" 2>/dev/null | grep -oE 'GLIBC_[0-9.]+' | sort -u
  echo '--- abi check (cannot exec on amd64; print expected sizes from build) ---'
  echo "binary=$BIN"
  ls -la "$BIN"
  if file "$BIN" | grep -q 'not stripped'; then
    echo "refusing unstripped release binary: $BIN" >&2
    exit 1
  fi
done
