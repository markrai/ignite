#!/usr/bin/env bash
# Stage the Linux release bundle and build ignite-server:local.
#
#   bash deployment/linux/docker/build-image.sh [bundle-dir]
#
# Defaults to <repo>/dist/ignite-server-linux. Does not publish the image.
# Requires: Docker Engine (Linux amd64), a Linux ELF scribe-server bundle.
#
# Optional: IGNITE_DOCKER_BUILD_BUNDLE=1 runs build-bundle.sh first (Linux host/CI only).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
DOCKER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUNDLE="${1:-$ROOT/dist/ignite-server-linux}"
IMAGE="${IGNITE_DOCKER_IMAGE:-ignite-server:local}"
CTX="$DOCKER_DIR/context"

die() { echo "build-image.sh: ERROR: $*" >&2; exit 1; }

command -v docker >/dev/null 2>&1 || die "docker CLI not found"
docker info >/dev/null 2>&1 || die "Docker daemon not accessible"

if [ "${IGNITE_DOCKER_BUILD_BUNDLE:-}" = "1" ]; then
  echo "build-image: IGNITE_DOCKER_BUILD_BUNDLE=1 → build-bundle.sh"
  bash "$ROOT/deployment/linux/build-bundle.sh" "$BUNDLE"
fi

[ -x "$BUNDLE/scribe-server" ] || [ -f "$BUNDLE/scribe-server" ] \
  || die "missing $BUNDLE/scribe-server (run build-bundle.sh on Linux, or pass bundle dir)"
[ -f "$BUNDLE/web/index.html" ] || die "missing $BUNDLE/web/index.html"
[ -f "$BUNDLE/web/manifest.webmanifest" ] || die "missing web manifest"

# Refuse Windows PE / non-ELF payloads.
if command -v file >/dev/null 2>&1; then
  file "$BUNDLE/scribe-server" | grep -qi 'ELF' \
    || die "scribe-server is not a Linux ELF binary (build the bundle on Linux/amd64)"
else
  # Minimal magic check when `file` is absent.
  hdr="$(od -An -t x1 -N 4 "$BUNDLE/scribe-server" | tr -d ' \n')"
  [ "$hdr" = "7f454c46" ] || die "scribe-server missing ELF magic (not a Linux binary)"
fi

MANIFEST_ID="$(sed -n 's/.*"id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
  "$BUNDLE/web/manifest.webmanifest" | head -1)"
[ "$MANIFEST_ID" = "/ignite-web" ] || die "manifest id must be /ignite-web (got ${MANIFEST_ID:-missing})"

echo "build-image: staging context → $CTX"
rm -rf "$CTX"
mkdir -p "$CTX/web"
cp -a "$BUNDLE/scribe-server" "$CTX/scribe-server"
cp -a "$BUNDLE/web/." "$CTX/web/"
cp -a "$DOCKER_DIR/entrypoint.sh" "$CTX/entrypoint.sh"
cp -a "$DOCKER_DIR/config.toml" "$CTX/config.toml"
chmod 0755 "$CTX/scribe-server" "$CTX/entrypoint.sh"

echo "build-image: docker build -t $IMAGE (linux/amd64)"
docker build \
  --platform linux/amd64 \
  -f "$DOCKER_DIR/Dockerfile" \
  -t "$IMAGE" \
  "$CTX"

# Verify dynamic linker deps inside the *built* runtime image (authoritative).
echo "build-image: verifying libraries + --version inside $IMAGE"
docker run --rm --entrypoint /bin/bash "$IMAGE" -c 'set -euo pipefail
  if ! ldd /usr/local/bin/scribe-server >/tmp/ldd.out 2>/tmp/ldd.err; then
    echo "ldd failed:" >&2
    cat /tmp/ldd.err >&2
    exit 1
  fi
  if grep -qi "not found" /tmp/ldd.out; then
    echo "missing shared libraries:" >&2
    cat /tmp/ldd.out >&2
    exit 1
  fi
  /usr/local/bin/scribe-server --version
  echo "ldd-ok"
  cat /tmp/ldd.out
' || die "binary is not compatible with the built ubuntu:24.04 runtime image"

echo "build-image: OK $IMAGE"
docker image inspect "$IMAGE" --format '{{.Id}} {{.Architecture}} {{.Os}}'
