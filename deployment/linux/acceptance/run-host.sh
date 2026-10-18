#!/usr/bin/env bash
# Host driver: disposable Ubuntu 24.04 guest with systemd as PID 1.
#
# Platform selection (never mislabeled):
#   - multipass → platform_class=true-vm
#   - docker systemd container → platform_class=systemd-containe
# Set IGNITE_ACCEPT_FORCE_DOCKER=1 to skip Multipass even when installed.
#
# Does NOT run install.sh on the developer host.
#
#   deployment/linux/acceptance/run-host.sh [bundle-dir]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUNDLE="${1:-$ROOT/dist/ignite-server-linux}"
NAME="ignite-linux-accept-$$"
EVIDENCE_HOST="$ROOT/deployment/linux/acceptance/evidence-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$EVIDENCE_HOST"

log() { printf '%s\n' "$*"; }
die() { printf 'run-host: %s\n' "$*" >&2; exit 1; }

cleanup() {
  if [ "${PLATFORM:-}" = multipass ] && command -v multipass >/dev/null 2>&1; then
    multipass delete --purge "$NAME" >/dev/null 2>&1 || true
  fi
  if [ "${PLATFORM:-}" = docker-systemd ] && docker inspect "$NAME" >/dev/null 2>&1; then
    docker rm -f "$NAME" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

[ -x "$BUNDLE/scribe-server" ] || [ -f "$BUNDLE/scribe-server" ] \
  || die "bundle missing scribe-server at $BUNDLE (build with deployment/linux/build-bundle.sh)"
[ -f "$BUNDLE/install.sh" ] || die "bundle missing install.sh"
[ -f "$HERE/guest-acceptance.sh" ] || die "guest-acceptance.sh missing"

PLATFORM=""
PLATFORM_CLASS=""
PLATFORM_DRIVER=""
if [ "${IGNITE_ACCEPT_FORCE_DOCKER:-}" = 1 ]; then
  command -v docker >/dev/null 2>&1 || die "IGNITE_ACCEPT_FORCE_DOCKER=1 but docker not found"
  PLATFORM=docker-systemd
  PLATFORM_CLASS=systemd-containe
  PLATFORM_DRIVER=docker-jrei-systemd-ubuntu-24.04
elif command -v multipass >/dev/null 2>&1; then
  PLATFORM=multipass
  PLATFORM_CLASS=true-vm
  PLATFORM_DRIVER=multipass-24.04
elif command -v docker >/dev/null 2>&1; then
  PLATFORM=docker-systemd
  PLATFORM_CLASS=systemd-containe
  PLATFORM_DRIVER=docker-jrei-systemd-ubuntu-24.04
else
  die "need multipass (true-vm) or docker (systemd-container)"
fi

{
  echo "platform_class=$PLATFORM_CLASS"
  echo "platform_driver=$PLATFORM_DRIVER"
  echo "host_driver=run-host.sh"
} | tee "$EVIDENCE_HOST/platform.txt"
log "platform_class=$PLATFORM_CLASS platform_driver=$PLATFORM_DRIVER"
log "bundle=$BUNDLE"
log "evidence=$EVIDENCE_HOST"

run_multipass() {
  multipass launch 24.04 --name "$NAME" --cpus 2 --memory 2G --disk 10G
  multipass transfer -r "$BUNDLE" "$NAME":/home/ubuntu/ignite-bundle
  multipass transfer "$HERE/guest-acceptance.sh" "$NAME":/home/ubuntu/guest-acceptance.sh
  multipass exec "$NAME" -- sudo apt-get update -qq
  multipass exec "$NAME" -- sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq curl avahi-utils >/dev/null \
    || multipass exec "$NAME" -- sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq curl
  multipass exec "$NAME" -- bash -lc "
    set -euo pipefail
    sudo chmod +x /home/ubuntu/guest-acceptance.sh /home/ubuntu/ignite-bundle/install.sh /home/ubuntu/ignite-bundle/uninstall.sh
    export IGNITE_ACCEPT_EVIDENCE=/home/ubuntu/evidence
    export IGNITE_ACCEPT_PLATFORM_CLASS=$PLATFORM_CLASS
    export IGNITE_ACCEPT_PLATFORM_DRIVER=$PLATFORM_DRIVER
    sudo -E bash /home/ubuntu/guest-acceptance.sh /home/ubuntu/ignite-bundle
  "
  multipass transfer -r "$NAME":/home/ubuntu/evidence "$EVIDENCE_HOST/guest"
}

run_docker_systemd() {
  docker pull jrei/systemd-ubuntu:24.04 >/dev/null
  # Privileged + cgroup: systemd as PID 1 inside a disposable container.
  # Labeled systemd-container — not a Multipass / true-vm result.
  docker run -d --name "$NAME" --privileged \
    --cgroupns=host \
    -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
    jrei/systemd-ubuntu:24.04 >/dev/null

  for _ in $(seq 60); do
    state="$(docker exec "$NAME" systemctl is-system-running 2>/dev/null || true)"
    case "$state" in
      running|degraded) break ;;
    esac
    sleep 1
  done
  pid1="$(docker exec "$NAME" ps -p 1 -o comm= | tr -d '\r')"
  [ "$pid1" = "systemd" ] || die "container PID1 is not systemd (got $pid1)"

  docker exec "$NAME" bash -lc '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq curl ca-certificates python3 avahi-utils >/dev/null
  '

  docker cp "$BUNDLE/." "$NAME:/opt/ignite-bundle"
  docker cp "$HERE/guest-acceptance.sh" "$NAME:/opt/guest-acceptance.sh"
  docker exec -e IGNITE_ACCEPT_PLATFORM_CLASS="$PLATFORM_CLASS" \
    -e IGNITE_ACCEPT_PLATFORM_DRIVER="$PLATFORM_DRIVER" \
    "$NAME" bash -lc '
    set -euo pipefail
    chmod +x /opt/guest-acceptance.sh /opt/ignite-bundle/install.sh /opt/ignite-bundle/uninstall.sh /opt/ignite-bundle/scribe-server
    find /opt/ignite-bundle -name "*.sh" -exec sed -i "s/\r$//" {} +
    sed -i "s/\r$//" /opt/guest-acceptance.sh
    export IGNITE_ACCEPT_EVIDENCE=/opt/evidence
    bash /opt/guest-acceptance.sh /opt/ignite-bundle
  '
  mkdir -p "$EVIDENCE_HOST/guest"
  docker cp "$NAME:/opt/evidence/." "$EVIDENCE_HOST/guest/"
}

case "$PLATFORM" in
  multipass) run_multipass ;;
  docker-systemd) run_docker_systemd ;;
esac

SUMMARY=""
if [ -f "$EVIDENCE_HOST/guest/summary.txt" ]; then
  SUMMARY="$EVIDENCE_HOST/guest/summary.txt"
elif [ -f "$EVIDENCE_HOST/guest/evidence/summary.txt" ]; then
  SUMMARY="$EVIDENCE_HOST/guest/evidence/summary.txt"
else
  find "$EVIDENCE_HOST" -type f | head -50
  die "missing guest summary.txt"
fi

log "--- guest summary ---"
cat "$SUMMARY"
grep -q 'ACCEPT PASS' "$SUMMARY" || die "guest did not report ACCEPT PASS"
# Refuse mislabeling: docker path must not claim true-vm.
if [ "$PLATFORM_CLASS" = systemd-container ]; then
  grep -q 'platform_class=systemd-container' "$SUMMARY" \
    || die "systemd-container run missing platform_class label in summary"
fi

log "HOST ACCEPT COMPLETE evidence=$EVIDENCE_HOST platform_class=$PLATFORM_CLASS"
