#!/usr/bin/env bash
# Static checks for Dockerfile / Compose (no Docker daemon, no image build).
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

f="$DIR/docker-compose.yml"
[ -f "$f" ] || fail "missing docker-compose.yml"

grep -q 'network_mode: host' "$f" || fail "compose must use network_mode: host"
if grep -Eq '^[[:space:]]*privileged:[[:space:]]*true' "$f"; then
  fail "privileged: true must not appear as a compose key"
fi
if grep -Eq '^[[:space:]]*ports:' "$f"; then
  fail "ports: mapping must not appear with host networking"
fi
if grep -Eiq 'docker\.sock|/var/run/docker\.sock' "$f"; then
  fail "Docker socket must not be mounted"
fi
if grep -Eq '^[[:space:]]*build:' "$f"; then
  fail "runtime compose must be image-only (no build:)"
fi
grep -q 'image: ignite-server:local' "$f" || fail "missing image: ignite-server:local"
grep -q 'ignite-state:/var/lib/scribe-server' "$f" || fail "missing state volume mount"
# Config bind is optional (image embeds default); must not require ./context
if grep -q './context' "$f"; then
  fail "compose must not reference generated ./context"
fi
grep -q 'restart: unless-stopped' "$f" || fail "missing restart policy"
grep -q 'read_only: true' "$f" || fail "expected read_only rootfs"
grep -q '8787' "$f" || fail "expected port 8787 in healthcheck"
grep -q '/control/v1/status' "$f" || fail "healthcheck must hit /control/v1/status"
grep -q 'no-new-privileges' "$f" || fail "expected no-new-privileges"
pass "compose host-network image-only + volume + healthcheck shape"

df="$DIR/Dockerfile"
grep -q 'ubuntu:24.04' "$df" || fail "runtime base should be ubuntu:24.04"
grep -q '19187' "$df" || fail "expected deterministic UID 19187"
grep -q 'github.com/markrai/ignite' "$df" || fail "OCI source must be github.com/markrai/ignite"
grep -qi 'systemd' "$df" && fail "Dockerfile must not install/use systemd" || true
grep -q 'HEALTHCHECK' "$df" || fail "Dockerfile missing HEALTHCHECK"
grep -q 'curl' "$df" || fail "healthcheck tool (curl) must be installed"
pass "Dockerfile runtime identity + healthcheck"

ep="$DIR/entrypoint.sh"
grep -q 'setpriv' "$ep" || fail "entrypoint must drop privileges via setpriv"
if grep -Eq '^[^#]*\binit\b' "$ep"; then
  fail "entrypoint must not invoke init outside comments"
fi
grep -q 'serve --config' "$ep" || fail "entrypoint default should run serve --config"
pass "entrypoint does not auto-init"

cfg="$DIR/config.toml"
grep -q '0.0.0.0:8787' "$cfg" || fail "default listen must be 0.0.0.0:8787"
grep -q 'state_dir = "/var/lib/scribe-server"' "$cfg" || fail "state_dir path mismatch"
grep -q 'web_root = "/usr/share/scribe-server/web"' "$cfg" || fail "web_root path mismatch"
pass "config.toml paths"

echo "PASS: docker compose/Dockerfile static checks"
exit 0
