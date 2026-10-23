#!/usr/bin/env bash
# Build the pinned Dropbear ARMv7hf client into target/dropbear-kindle/.
# Official / CI path: always uses the digest-pinned rust:1-bullseye image from
# tools/pinned-docker-images.txt (same pin as Kindle ARMv7hf Rust). Do not
# substitute debian:bookworm: that toolchain requires GLIBC_2.34 and will not
# match PINNED.txt. Bookworm strings in PINNED.txt are signing-key sources
# only, not the compile OS. The resulting ELF must match PINNED.txt
# elf_sha256 and elf_bytes — this script never rewrites those pins.
set -euo pipefail
shopt -s inherit_errexit 2>/dev/null || true

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PINNED="${IGNITE_DROPBEAR_PINNED:-$SCRIPT_DIR/PINNED.txt}"
IMAGES="$ROOT/tools/pinned-docker-images.txt"
OUT="${IGNITE_DROPBEAR_OUT:-$ROOT/target/dropbear-kindle}"

die() { echo "dropbear/docker_build.sh: $*" >&2; exit 1; }

[ -f "$PINNED" ] || die "missing $PINNED"
[ -f "$IMAGES" ] || die "missing $IMAGES"

IMAGE="$(sed -n 's/^dropbear_armv7hf=//p' "$IMAGES" | tr -d '\r')"
[ -n "$IMAGE" ] || die "dropbear_armv7hf pin missing in $IMAGES"
[[ "$IMAGE" == *@sha256:* ]] || die "dropbear image must be digest-pinned (got $IMAGE)"

want_sha="$(sed -n 's/^elf_sha256=//p' "$PINNED" | tr -d '\r')"
want_bytes="$(sed -n 's/^elf_bytes=//p' "$PINNED" | tr -d '\r')"
[[ "$want_sha" =~ ^[0-9a-f]{64}$ ]] || die "PINNED.txt elf_sha256 missing or malformed"
[[ "$want_bytes" =~ ^[1-9][0-9]*$ ]] || die "PINNED.txt elf_bytes missing or malformed"

pin_mismatch() {
  echo "dropbear/docker_build.sh: Dropbear ELF does not match tools/dropbear/PINNED.txt" >&2
  echo "  have sha256=$1 bytes=$2" >&2
  echo "  want sha256=$want_sha bytes=$want_bytes" >&2
  echo "  image=$IMAGE" >&2
  echo "Do not update PINNED.txt to silence this. Investigate the toolchain image," >&2
  echo "upstream tarball, configure flags, and strip step, then record findings." >&2
  exit 1
}

check_out() {
  local have_sha have_bytes
  [ -f "$OUT/dropbearmulti" ] && [ -f "$OUT/elf.sha256" ] && [ -f "$OUT/provenance.txt" ] || return 1
  have_sha="$(sha256sum "$OUT/dropbearmulti" | awk '{print $1}')"
  have_bytes="$(wc -c <"$OUT/dropbearmulti" | tr -d ' ')"
  [ "$have_sha" = "$want_sha" ] && [ "$have_bytes" = "$want_bytes" ] || return 1
  [ "$(tr -d '[:space:]' <"$OUT/elf.sha256")" = "$want_sha" ] || return 1
  return 0
}

if [ "${IGNITE_DROPBEAR_FORCE:-}" != "1" ] && check_out; then
  echo "dropbear/docker_build.sh: existing $OUT/dropbearmulti matches PINNED.txt"
  exit 0
fi

command -v docker >/dev/null 2>&1 || die "docker not found"

mkdir -p "$OUT"
echo "dropbear/docker_build.sh: building with $IMAGE"

# Single repo mount (OUT is under the repo). Do not mark the tree read-only.
rel_out="${OUT#"$ROOT"/}"
case "$rel_out" in
  "$OUT"|"") die "Dropbear output must be inside the repository ($OUT)" ;;
esac

docker run --rm \
  -v "$ROOT:/src" \
  -w /src \
  -e "IGNITE_DROPBEAR_REL=$rel_out" \
  "$IMAGE" \
  bash -c 'set -euo pipefail
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      ca-certificates curl gnupg make gcc-arm-linux-gnueabihf \
      libc6-dev-armhf-cross binutils-arm-linux-gnueabihf \
      bzip2 xz-utils file
    mkdir -p /work/dropbear-src /work/dropbear-out "/src/$IGNITE_DROPBEAR_REL"
    bash /src/tools/dropbear/build-armv7hf-client.sh /work/dropbear-src /work/dropbear-out
    cp -a /work/dropbear-out/dropbearmulti "/src/$IGNITE_DROPBEAR_REL/dropbearmulti"
    cp -a /work/dropbear-out/elf.sha256 "/src/$IGNITE_DROPBEAR_REL/elf.sha256"
    cp -a /work/dropbear-out/provenance.txt "/src/$IGNITE_DROPBEAR_REL/provenance.txt"
    chmod 0755 "/src/$IGNITE_DROPBEAR_REL/dropbearmulti"
  '

have_sha="$(sha256sum "$OUT/dropbearmulti" | awk '{print $1}')"
have_bytes="$(wc -c <"$OUT/dropbearmulti" | tr -d ' ')"
[ "$have_sha" = "$want_sha" ] && [ "$have_bytes" = "$want_bytes" ] \
  || pin_mismatch "$have_sha" "$have_bytes"
[ "$(tr -d '[:space:]' <"$OUT/elf.sha256")" = "$want_sha" ] \
  || die "elf.sha256 sidecar does not match the ELF"

echo "dropbear/docker_build.sh: OK $OUT/dropbearmulti sha256=$have_sha bytes=$have_bytes"
