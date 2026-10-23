#!/usr/bin/env bash
# Official Linux / CI Kindle packaging wrapper.
# Always rebuilds ARMv7hf customer binaries with the digest-pinned rust image,
# builds Dropbear via tools/dropbear/docker_build.sh (PINNED.txt enforced),
# then calls the shared Python packager. There is no SkipBuild on this path.
set -euo pipefail
shopt -s inherit_errexit 2>/dev/null || true

die() { echo "package-release.sh: $*" >&2; exit 1; }

# Create dir as this user so a later root Docker run cannot leave a root-owned
# target/ that the runner cannot mkdir into. Never chmod 777, chown, or sudo.
ensure_host_owned_dir() {
  local dir="$1"
  if [ -e "$dir" ]; then
    [ -d "$dir" ] || die "$dir exists and is not a directory"
    if [ ! -w "$dir" ]; then
      die "$dir is not writable by $(id -un) (likely created by Docker as root). Remove $dir and retry. Do not chmod 777 or chown the tree."
    fi
    return 0
  fi
  mkdir -p "$dir" || die "cannot create $dir"
  [ -w "$dir" ] || die "created $dir but it is not writable by $(id -un)"
}

require_host_writable_dir() {
  local dir="$1"
  local when="$2"
  [ -d "$dir" ] || die "$dir is missing ${when}"
  if [ ! -w "$dir" ]; then
    die "$dir is not writable by $(id -un) ${when} (likely created by Docker as root). Remove $dir and retry. Do not chmod 777 or chown the tree."
  fi
}

if [ "${BASH_SOURCE[0]}" != "$0" ]; then
  return 0
fi

ROOT="$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)"
IMAGES="$ROOT/tools/pinned-docker-images.txt"

command -v docker >/dev/null 2>&1 || die "docker not found"
command -v python3 >/dev/null 2>&1 || die "python3 not found"
[ -f "$IMAGES" ] || die "missing $IMAGES"

RUST_IMAGE="$(sed -n 's/^rust_armv7hf=//p' "$IMAGES" | tr -d '\r')"
[ -n "$RUST_IMAGE" ] || die "rust_armv7hf pin missing"
[[ "$RUST_IMAGE" == *@sha256:* ]] || die "rust image must be digest-pinned (got $RUST_IMAGE)"

# Host user must own target/ before the first root Docker invocation.
ensure_host_owned_dir "$ROOT/target"

echo "== ARM release build via Docker ($RUST_IMAGE)"
docker run --rm -v "$ROOT:/src" -w /src "$RUST_IMAGE" bash /src/tools/docker_build_armv7hf.sh

require_host_writable_dir "$ROOT/target" "after the ARM Docker build"

echo "== Dropbear ARMv7hf client"
bash "$ROOT/tools/dropbear/docker_build.sh"

echo "== Shared Python packager"
# CI uploads only the customer Scriptlet + checksum; still emit support ZIP locally.
python3 "$ROOT/deployment/kindle/package_release.py" --repo-root "$ROOT"
