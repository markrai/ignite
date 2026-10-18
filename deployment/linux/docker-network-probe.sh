#!/usr/bin/env bash
# Disposable Docker networking probe (host vs bridge observations).
# Does NOT install packages, enable services, or change host firewall/config.
# Not a physical-LAN Kindle proof.
#
# Usage:
#   IGNITE_DISPOSABLE_PROBE=1 bash deployment/linux/docker-network-probe.sh
#
# Optional env:
#   DOCKER          — docker CLI (default: docker)
#   IGNITE_PROBE_HN_PORT / IGNITE_PROBE_BR_PORT — preferred probe ports (must be free)
set -euo pipefail

DOCKER="${DOCKER:-docker}"
SCRIPT_NAME="$(basename "$0")"

die() {
  echo "$SCRIPT_NAME: ERROR: $*" >&2
  exit 1
}

# --- Safety gates (no side effects before these pass) ---

if [ "${IGNITE_DISPOSABLE_PROBE:-}" != "1" ]; then
  die "refusing to create containers without explicit opt-in.
Set IGNITE_DISPOSABLE_PROBE=1 to run this disposable probe.
This script never installs Docker or changes host services."
fi

if ! command -v "$DOCKER" >/dev/null 2>&1 && [ ! -x "$DOCKER" ]; then
  die "Docker CLI not found (DOCKER=$DOCKER).
Install and start Docker yourself, then re-run with IGNITE_DISPOSABLE_PROBE=1.
This script will not run apt-get or systemctl."
fi

if ! "$DOCKER" info >/dev/null 2>&1; then
  die "Docker daemon is not accessible via '$DOCKER info'.
Start the Docker engine (and fix permissions/context) yourself, then re-run.
This script will not enable or reconfigure Docker."
fi

command -v curl >/dev/null 2>&1 || die "curl is required (install it yourself; this script will not)."
command -v ip >/dev/null 2>&1 || die "ip (iproute2) is required (install it yourself; this script will not)."

# --- Invocation-owned resources only ---

INVOCATION_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$-$RANDOM"
PREFIX="ignite-probe-${INVOCATION_ID}"
# Exact container names created by THIS invocation (never pattern-delete).
OWNED_CONTAINERS=()
LAST_OWNED=""

cleanup() {
  local c
  for c in "${OWNED_CONTAINERS[@]+"${OWNED_CONTAINERS[@]}"}"; do
    [ -n "$c" ] || continue
    "$DOCKER" rm -f "$c" >/dev/null 2>&1 || true
  done
}
# Terminate on signals so EXIT runs cleanup exactly once; do not clean from the signal trap.
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

port_listening() {
  local port="$1"
  if command -v ss >/dev/null 2>&1; then
    ss -ltn 2>/dev/null | grep -Eq ":${port}[[:space:]]"
    return $?
  fi
  if (echo >/dev/tcp/127.0.0.1/"$port") >/dev/null 2>&1; then
    return 0
  fi
  return 1
}

pick_free_port() {
  local hint="$1"
  local p
  for p in "$hint" $((hint + 1)) $((hint + 2)) $((hint + 3)) $((hint + 4)); do
    if ! port_listening "$p"; then
      echo "$p"
      return 0
    fi
  done
  die "no free TCP port near $hint on 127.0.0.1 (checked $hint..$((hint + 4))).
Free a port or set IGNITE_PROBE_HN_PORT / IGNITE_PROBE_BR_PORT."
}

# run_owned <suffix> <docker run args...>
# Creates a uniquely named container and records it for trap cleanup (no subshell).
# Name is tracked before `docker run` so an interrupted/failed create is still cleaned.
run_owned() {
  local suffix="$1"
  shift
  local name="${PREFIX}-${suffix}"
  if "$DOCKER" inspect "$name" >/dev/null 2>&1; then
    die "refusing to reuse unexpected existing container name: $name
(This should be unique per invocation; aborting without removing it.)"
  fi
  OWNED_CONTAINERS+=("$name")
  LAST_OWNED="$name"
  if ! "$DOCKER" run -d --name "$name" "$@" >/dev/null; then
    die "docker run failed for $name"
  fi
}

run_ephemeral() {
  # One-shot --rm containers: unique name; track for cleanup if interrupt leaves a remnant.
  local suffix="$1"
  shift
  local name="${PREFIX}-${suffix}"
  if "$DOCKER" inspect "$name" >/dev/null 2>&1; then
    die "refusing to reuse unexpected existing container name: $name"
  fi
  OWNED_CONTAINERS+=("$name")
  "$DOCKER" run --rm --name "$name" "$@"
}

HN_PORT="$(pick_free_port "${IGNITE_PROBE_HN_PORT:-18787}")"
BR_PORT="$(pick_free_port "${IGNITE_PROBE_BR_PORT:-18788}")"
if [ "$HN_PORT" = "$BR_PORT" ]; then
  BR_PORT="$(pick_free_port $((HN_PORT + 1)))"
fi

echo "=== probe invocation ==="
echo "id=$INVOCATION_ID prefix=$PREFIX hn_port=$HN_PORT br_port=$BR_PORT"
echo "docker=$DOCKER"
echo "NOTE: read-only w.r.t. host packages/services/firewall; only creates disposable containers owned by this run."

echo "=== host interfaces ==="
ip -br addr 2>/dev/null || ip addr

echo "=== docker version ==="
"$DOCKER" version --format '{{.Server.Version}}' 2>/dev/null || "$DOCKER" version

echo "=== host-network container interfaces ==="
run_ephemeral hn-iface --network host alpine:3.20 ip addr

echo "=== bridge-network container interfaces ==="
run_ephemeral br-iface alpine:3.20 ip addr

echo "=== NON_LAN prefix simulation (name check only; not full netpolicy) ==="
if command -v python3 >/dev/null 2>&1; then
  python3 - <<'PY'
prefixes=("lo","docker","br-","veth","virbr","vnet","tailscale","tun","tap","wg")
for name in ["eth0","docker0","br-abc","veth123","lo","enp0s1"]:
    non = any(name.startswith(p) for p in prefixes)
    print(f"{name}: name_eligible={not non}")
print("note: ip(8) may show eth0@ifN; if_addrs/app often see ifname eth0 (name-eligible). Accept still needs destination/peer/subnet after Docker NAT.")
PY
else
  for name in eth0 docker0 br-abc veth123 lo enp0s1; do
    case "$name" in
      lo|docker*|br-*|veth*|virbr*|vnet*|tailscale*|tun*|tap*|wg*) echo "$name: name_eligible=False" ;;
      *) echo "$name: name_eligible=True" ;;
    esac
  done
  echo "note: ip(8) may show eth0@ifN; if_addrs/app often see ifname eth0 (name-eligible). Accept still needs destination/peer/subnet after Docker NAT."
fi

echo "=== host-network loopback HTTP ==="
run_owned hn-http --network host python:3.12-alpine \
  python -c "from http.server import HTTPServer,BaseHTTPRequestHandler
class H(BaseHTTPRequestHandler):
  def do_GET(self):
    self.send_response(200); self.end_headers(); self.wfile.write(b'ok-hostnet')
  def log_message(self,*a): pass
HTTPServer(('127.0.0.1',${HN_PORT}),H).serve_forever()"
sleep 1
curl -fsS "http://127.0.0.1:${HN_PORT}/"
echo

echo "=== bridge published-port HTTP (127.0.0.1 only) ==="
run_owned br-http -p "127.0.0.1:${BR_PORT}:8000" python:3.12-alpine \
  python -c "from http.server import HTTPServer,BaseHTTPRequestHandler
class H(BaseHTTPRequestHandler):
  def do_GET(self):
    self.send_response(200); self.end_headers(); self.wfile.write(b'ok-bridge')
  def log_message(self,*a): pass
HTTPServer(('0.0.0.0',8000),H).serve_forever()"
sleep 1
curl -fsS "http://127.0.0.1:${BR_PORT}/"
echo

echo "=== docker0 (if present) ==="
ip -br addr show docker0 2>/dev/null || ip addr show docker0 2>/dev/null || echo "(no docker0)"

echo "PROBE_DONE"
