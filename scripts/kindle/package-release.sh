#!/usr/bin/env bash
# Official Linux / CI Kindle packaging wrapper.
# Always rebuilds ARMv7hf customer binaries with the digest-pinned rust image,
# builds Dropbear via tools/dropbear/docker_build.sh (PINNED.txt enforced),
# then calls the shared Python packager. There is no SkipBuild on this path.
set -euo pipefail
shopt -s inherit_errexit 2>/dev/null || true

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
IMAGES="$ROOT/tools/pinned-docker-images.txt"
die() { echo "package-release.sh: $*" >&2; exit 1; }

command -v docker >/dev/null 2>&1 || die "docker not found"
command -v python3 >/dev/null 2>&1 || die "python3 not found"
[ -f "$IMAGES" ] || die "missing $IMAGES"

RUST_IMAGE="$(sed -n 's/^rust_armv7hf=//p' "$IMAGES" | tr -d '\r')"
[ -n "$RUST_IMAGE" ] || die "rust_armv7hf pin missing"
[[ "$RUST_IMAGE" == *@sha256:* ]] || die "rust image must be digest-pinned (got $RUST_IMAGE)"

echo "== ARM release build via Docker ($RUST_IMAGE)"
docker run --rm -v "$ROOT:/src" -w /src "$RUST_IMAGE" bash /src/tools/docker_build_armv7hf.sh

echo "== Dropbear ARMv7hf client"
bash "$ROOT/tools/dropbear/docker_build.sh"

echo "== Shared Python packager"
# CI uploads only the customer Scriptlet + checksum; still emit support ZIP locally.
python3 "$ROOT/deployment/kindle/package_release.py" --repo-root "$ROOT"
