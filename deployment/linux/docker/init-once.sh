#!/usr/bin/env bash
# Explicit one-off initialization for Compose/Portainer deployments.
#
#   bash deployment/linux/docker/init-once.sh
#
# Runs `scribe-server init` against the stack's persistent volume and config.
# Prints machine-parseable init lines to stdout (may include setup_code=… while unclaimed).
# Safe to re-run after claim (no setup_code; identity preserved) — but while unclaimed,
# each init issues a fresh setup code and invalidates the previous one.
#
# Never writes setup codes to Compose files or evidence logs beyond this command's stdout.
set -euo pipefail

DOCKER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DOCKER_DIR"

COMPOSE=(docker compose -f "$DOCKER_DIR/docker-compose.yml")
if [ -n "${COMPOSE_PROJECT_NAME:-}" ]; then
  COMPOSE=(docker compose -p "$COMPOSE_PROJECT_NAME" -f "$DOCKER_DIR/docker-compose.yml")
fi

die() { echo "init-once.sh: ERROR: $*" >&2; exit 1; }

command -v docker >/dev/null 2>&1 || die "docker CLI not found"
docker info >/dev/null 2>&1 || die "Docker daemon not accessible"

echo "init-once: running scribe-server init (explicit; not part of normal service start)" >&2
echo "init-once: uses the same Compose project volume as 'docker compose up' (image-embedded config unless overridden)" >&2
"${COMPOSE[@]}" run --rm -T --no-deps \
  ignite-server init --config /etc/scribe-server/config.toml
