#!/usr/bin/env bash
# Run firewall ownership tests inside disposable Docker containers (never the host firewall).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

docker run --rm --cap-add=NET_ADMIN \
  -v "$HERE:/linux:ro" \
  ubuntu:24.04 bash -lc '
    set -euo pipefail
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq ufw iproute2 >/dev/null
    bash /linux/test-firewall.sh ufw
  '

docker run --rm --privileged \
  -v "$HERE:/linux:ro" \
  ubuntu:24.04 bash -lc '
    set -euo pipefail
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq firewalld dbus iproute2 >/dev/null
    bash /linux/test-firewall.sh firewalld
  '

echo "FIREWALL CI OK"
