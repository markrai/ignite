#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET=armv7-unknown-linux-gnueabihf
ORDINARY_TARGET_DIR="${IGNITE_READER_ORDINARY_TARGET_DIR:-/tmp/ignite-reader-ordinary-target}"
BASELINE_TARGET_DIR="${IGNITE_READER_BASELINE_TARGET_DIR:-/tmp/ignite-reader-baseline-target}"
READER_TARGET_DIR="${IGNITE_READER_TARGET_DIR:-/tmp/ignite-reader-armv7-target}"
DEBIAN_SNAPSHOT=20260824T000000Z
DEBIAN_SNAPSHOT_SOURCES=/tmp/ignite-reader-debian-snapshot.list
DEBIAN_SNAPSHOT_LISTS=/tmp/ignite-reader-debian-snapshot-lists
MUPDF_DIR=vendor/mupdf-sys/mupdf
MUPDF_NATIVE_REVISION=da5f5eb890d7b3fb76903160b5f7f48ba1301492
MUPDF_VERSION=1.28.6
cd "$ROOT"

if [ "$(id -u)" -ne 0 ]; then
  echo "validate_reader_armv7.sh must run as root inside the pinned build container" >&2
  exit 1
fi

git config --global --add safe.directory "$ROOT"
while IFS= read -r repository; do
  git config --global --add safe.directory "$ROOT/$repository"
done < <(find "$MUPDF_DIR" -name .git -type f -printf '%h\n')

if [ ! -d "$MUPDF_DIR/.git" ] && [ ! -f "$MUPDF_DIR/.git" ]; then
  echo "MuPDF native submodule is not initialized; run git submodule update --init --recursive" >&2
  exit 1
fi
ACTUAL_MUPDF_REVISION="$(git -C "$MUPDF_DIR" rev-parse HEAD)"
if [ "$ACTUAL_MUPDF_REVISION" != "$MUPDF_NATIVE_REVISION" ]; then
  echo "MuPDF native revision mismatch: expected $MUPDF_NATIVE_REVISION, got $ACTUAL_MUPDF_REVISION" >&2
  exit 1
fi
MUPDF_SUBMODULE_STATUS="$(git submodule status --recursive "$MUPDF_DIR")"
if grep -E '^[+-U]' <<<"$MUPDF_SUBMODULE_STATUS"; then
  echo "MuPDF native submodule tree is incomplete or does not match its recorded revisions" >&2
  exit 1
fi
if ! grep -Fxq "#define FZ_VERSION \"$MUPDF_VERSION\"" \
  "$MUPDF_DIR/include/mupdf/fitz/version.h"; then
  echo "MuPDF version header does not report $MUPDF_VERSION" >&2
  exit 1
fi
printf 'mupdf_native_revision=%s\nmupdf_version=%s\n' \
  "$ACTUAL_MUPDF_REVISION" "$MUPDF_VERSION"
printf '%s\n' "$MUPDF_SUBMODULE_STATUS"

printf '%s\n' \
  "deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/$DEBIAN_SNAPSHOT bullseye main" \
  "deb [check-valid-until=no] http://snapshot.debian.org/archive/debian-security/$DEBIAN_SNAPSHOT bullseye-security main" \
  "deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/$DEBIAN_SNAPSHOT bullseye-updates main" \
  >"$DEBIAN_SNAPSHOT_SOURCES"
mkdir -p "$DEBIAN_SNAPSHOT_LISTS/partial"
APT_SNAPSHOT_OPTIONS=(
  -o "Dir::Etc::sourcelist=$DEBIAN_SNAPSHOT_SOURCES"
  -o "Dir::Etc::sourceparts=-"
  -o "Dir::State::lists=$DEBIAN_SNAPSHOT_LISTS"
)
echo "== Debian snapshot provisioning ($DEBIAN_SNAPSHOT)"
if ! apt-get "${APT_SNAPSHOT_OPTIONS[@]}" update -qq; then
  echo "verified Debian snapshot $DEBIAN_SNAPSHOT is unavailable" >&2
  exit 1
fi
if ! DEBIAN_FRONTEND=noninteractive apt-get "${APT_SNAPSHOT_OPTIONS[@]}" install -y -qq \
  clang file g++-arm-linux-gnueabihf gcc-arm-linux-gnueabihf libclang-dev make python3 time; then
  echo "could not provision ARMv7 validation tools from Debian snapshot $DEBIAN_SNAPSHOT" >&2
  exit 1
fi
rustup target add "$TARGET"

RUSTC_VERSION="$(rustc --version)"
CARGO_VERSION="$(cargo --version)"
ARM_GCC_VERSION="$(arm-linux-gnueabihf-gcc --version | sed -n '1p')"
ARM_LD_VERSION="$(arm-linux-gnueabihf-ld --version | sed -n '1p')"
CLANG_VERSION="$(clang --version | sed -n '1p')"
printf 'rustc=%s\ncargo=%s\narm_gcc=%s\narm_ld=%s\nclang=%s\n' \
  "$RUSTC_VERSION" "$CARGO_VERSION" "$ARM_GCC_VERSION" "$ARM_LD_VERSION" "$CLANG_VERSION"

export CARGO_TERM_COLOR=never
if [ "${IGNITE_READER_RUN_HOST_TESTS:-0}" = 1 ]; then
  echo "== Reader engine tests (Linux host)"
  HOST_TARGET_DIR="${IGNITE_READER_HOST_TARGET_DIR:-/tmp/ignite-reader-host-target}"
  HOST_BUILD_LOG="$(mktemp)"
  HOST_RSS_LOG="$(mktemp)"
  CARGO_TARGET_DIR="$HOST_TARGET_DIR" cargo test \
    -p scribe-app \
    --lib \
    --locked \
    --features reader \
    --no-run \
    reader_engine 2>&1 | tee "$HOST_BUILD_LOG"
  HOST_TEST_BIN="$(
    sed -n 's/^[[:space:]]*Executable unittests src\/lib.rs (\(.*\))$/\1/p' \
      "$HOST_BUILD_LOG" | tail -n 1
  )"
  if [ -z "$HOST_TEST_BIN" ]; then
    echo "could not locate linked Linux reader test executable in Cargo output" >&2
    exit 1
  fi
  case "$HOST_TEST_BIN" in
    /*) ;;
    *) HOST_TEST_BIN="$ROOT/$HOST_TEST_BIN" ;;
  esac
  test -f "$HOST_TEST_BIN"
  timeout --signal=KILL 120s /usr/bin/time -v -o "$HOST_RSS_LOG" \
    "$HOST_TEST_BIN" reader_engine --nocapture
  HOST_PEAK_RSS_KIB="$(
    sed -n 's/^[[:space:]]*Maximum resident set size (kbytes):[[:space:]]*//p' \
      "$HOST_RSS_LOG"
  )"
  if ! [[ "$HOST_PEAK_RSS_KIB" =~ ^[0-9]+$ ]]; then
    echo "could not read Linux reader test peak RSS" >&2
    exit 1
  fi
  printf 'reader_host_peak_rss_kib=%s\nreader_host_native_crash=absent\nreader_host_oom=absent\n' \
    "$HOST_PEAK_RSS_KIB"
  rm -f "$HOST_BUILD_LOG" "$HOST_RSS_LOG"
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
ORDINARY_MUPDF_OUTPUT="$(find "$ORDINARY_TARGET_DIR" -iname '*mupdf*' -print -quit)"
if [ -n "$ORDINARY_MUPDF_OUTPUT" ]; then
  echo "ordinary ARM target directory contains MuPDF output" >&2
  exit 1
fi
ORDINARY_SYMBOLS="$(arm-linux-gnueabihf-nm --defined-only "$ORDINARY_BIN")"
if grep -E 'scribe_app(13|::)reader_engine|mupdf|[[:space:]][Tt][[:space:]]+fz_' \
  <<<"$ORDINARY_SYMBOLS"; then
  echo "ordinary ARM scribe-app contains reader or MuPDF symbols" >&2
  exit 1
fi
echo "ordinary_arm_reader_symbols=absent"
echo "ordinary_arm_mupdf_outputs=absent"

BASELINE_BUILD_LOG="$(mktemp)"
READER_BUILD_LOG="$(mktemp)"
trap 'rm -f "$BASELINE_BUILD_LOG" "$READER_BUILD_LOG"' EXIT

echo "== Reader-disabled ARMv7 linked test executable"
CARGO_TARGET_DIR="$BASELINE_TARGET_DIR" cargo test \
  -p scribe-app \
  --lib \
  --locked \
  --no-run \
  --target "$TARGET" \
  --features scribe-app/restore_from_server \
  reader_engine 2>&1 | tee "$BASELINE_BUILD_LOG"

BASELINE_TEST_BIN="$(
  sed -n 's/^[[:space:]]*Executable unittests src\/lib.rs (\(.*\))$/\1/p' \
    "$BASELINE_BUILD_LOG" | tail -n 1
)"
if [ -z "$BASELINE_TEST_BIN" ]; then
  echo "could not locate linked reader-disabled scribe-app test executable in Cargo output" >&2
  exit 1
fi
case "$BASELINE_TEST_BIN" in
  /*) ;;
  *) BASELINE_TEST_BIN="$ROOT/$BASELINE_TEST_BIN" ;;
esac
test -f "$BASELINE_TEST_BIN"

echo "== Reader-enabled ARMv7 linked test executable"
CARGO_TARGET_DIR="$READER_TARGET_DIR" cargo test \
  -p scribe-app \
  --lib \
  --locked \
  --no-run \
  --target "$TARGET" \
  --features scribe-app/restore_from_server,scribe-app/reader \
  reader_engine 2>&1 | tee "$READER_BUILD_LOG"

READER_TEST_BIN="$(
  sed -n 's/^[[:space:]]*Executable unittests src\/lib.rs (\(.*\))$/\1/p' \
    "$READER_BUILD_LOG" | tail -n 1
)"
if [ -z "$READER_TEST_BIN" ]; then
  echo "could not locate linked scribe-app test executable in Cargo output" >&2
  exit 1
fi
case "$READER_TEST_BIN" in
  /*) ;;
  *) READER_TEST_BIN="$ROOT/$READER_TEST_BIN" ;;
esac
test -f "$READER_TEST_BIN"

python3 tools/ci/verify_reader_armv7_elf.py \
  "$READER_TEST_BIN" \
  --baseline-binary "$BASELINE_TEST_BIN" \
  --ordinary-binary "$ORDINARY_BIN" \
  --target-dir "$READER_TARGET_DIR" \
  --target "$TARGET"
