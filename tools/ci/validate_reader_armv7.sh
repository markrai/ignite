#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET=armv7-unknown-linux-gnueabihf
ORDINARY_TARGET_DIR="${IGNITE_READER_ORDINARY_TARGET_DIR:-/tmp/ignite-reader-ordinary-target}"
READER_TARGET_DIR="${IGNITE_READER_TARGET_DIR:-/tmp/ignite-reader-armv7-target}"
cd "$ROOT"

if [ "$(id -u)" -ne 0 ]; then
  echo "validate_reader_armv7.sh must run as root inside the pinned build container" >&2
  exit 1
fi

apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
  clang file g++-arm-linux-gnueabihf gcc-arm-linux-gnueabihf libclang-dev make python3
rustup target add "$TARGET"

export CARGO_TERM_COLOR=never
if [ "${IGNITE_READER_RUN_HOST_TESTS:-0}" = 1 ]; then
  echo "== Reader engine tests (Linux host)"
  CARGO_TARGET_DIR="${IGNITE_READER_HOST_TARGET_DIR:-/tmp/ignite-reader-host-target}" cargo test \
    -p scribe-app \
    --lib \
    --locked \
    --features reader \
    reader_engine
  echo "reader_host_tests=passed"
fi

export CROSS_COMPILE=arm-linux-gnueabihf-
export CC=arm-linux-gnueabihf-gcc
export CXX=arm-linux-gnueabihf-g++
export LD=arm-linux-gnueabihf-ld
export AR=arm-linux-gnueabihf-ar
export RANLIB=arm-linux-gnueabihf-ranlib
export CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc
export CXX_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-g++
export AR_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-ar
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_RUSTFLAGS="-C link-arg=-static-libstdc++"
export BINDGEN_EXTRA_CLANG_ARGS_armv7_unknown_linux_gnueabihf="--target=arm-linux-gnueabihf --sysroot=/usr/arm-linux-gnueabihf"

echo "== Ordinary ARMv7 scribe-app link without reader"
CARGO_TARGET_DIR="$ORDINARY_TARGET_DIR" cargo build \
  -p scribe-app \
  --locked \
  --target "$TARGET" \
  --features scribe-app/restore_from_server \
  --bin scribe-app

ORDINARY_BIN="$ORDINARY_TARGET_DIR/$TARGET/debug/scribe-app"
test -f "$ORDINARY_BIN"
if find "$ORDINARY_TARGET_DIR" -iname '*mupdf*' -print -quit | grep -q .; then
  echo "ordinary ARM target directory contains MuPDF output" >&2
  exit 1
fi
if arm-linux-gnueabihf-nm -C --defined-only "$ORDINARY_BIN" \
  | grep -E 'scribe_app::reader_engine|mupdf::|[[:space:]][Tt][[:space:]]+fz_' ; then
  echo "ordinary ARM scribe-app contains reader or MuPDF symbols" >&2
  exit 1
fi
echo "ordinary_arm_reader_symbols=absent"
echo "ordinary_arm_mupdf_outputs=absent"

echo "== Reader ARMv7 linked test executable"
BUILD_LOG="$(mktemp)"
trap 'rm -f "$BUILD_LOG"' EXIT
CARGO_TARGET_DIR="$READER_TARGET_DIR" cargo test \
  -p scribe-app \
  --lib \
  --locked \
  --no-run \
  --target "$TARGET" \
  --features scribe-app/restore_from_server,scribe-app/reader \
  reader_engine 2>&1 | tee "$BUILD_LOG"

READER_TEST_BIN="$(
  sed -n 's/^[[:space:]]*Executable unittests src\/lib.rs (\(.*\))$/\1/p' "$BUILD_LOG" \
    | tail -n 1
)"
if [ -z "$READER_TEST_BIN" ]; then
  echo "could not locate linked scribe-app test executable in Cargo output" >&2
  exit 1
fi
case "$READER_TEST_BIN" in
  /*) ;;
  *) READER_TEST_BIN="$ROOT/$READER_TEST_BIN" ;;
esac

python3 tools/ci/verify_reader_armv7_elf.py \
  "$READER_TEST_BIN" \
  --baseline-binary "$ORDINARY_BIN" \
  --target-dir "$READER_TARGET_DIR" \
  --target "$TARGET"
