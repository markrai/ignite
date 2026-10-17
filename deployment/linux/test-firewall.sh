#!/usr/bin/env bash
# Real ufw / firewalld ownership tests for firewall.sh, run inside a DISPOSABLE container with
# NET_ADMIN (the container's own network namespace; never a host):
#   deployment/linux/test-firewall.sh ufw|firewalld
# Invariant: rules only on the given LAN interface(s), no duplicates, remove ONLY owned rules;
# unrelated rules survive every step.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=firewall.sh
. "$HERE/firewall.sh"
fail() { echo "FIREWALL TEST FAIL ($MODE): $*" >&2; exit 1; }
MODE="$1"
IFC=eth0

case "$MODE" in
  ufw)
    ufw --force reset >/dev/null
    ufw default deny incoming >/dev/null
    ufw --force enable >/dev/null
    ufw allow 80/tcp >/dev/null
    ufw allow proto udp to 224.0.0.251 port 5353 >/dev/null
    ufw allow in on "$IFC" proto tcp to any port 80 comment "admin-web" >/dev/null
    ufw allow 22/tcp comment ssh >/dev/null
    before="$(ufw status numbered | grep -v "# scribe-server" | sed 's/^\[ *[0-9]*\]//')"
    owned() { ufw status numbered | grep -c "# scribe-server\$" || true; }

    rec="$(fw_open 80 "$IFC")"
    [ "$rec" = "ufw 80 $IFC" ] || fail "record $rec"
    [ "$(owned)" -ge 1 ] || fail "owned mDNS rule not added"
    ufw status numbered | grep -q "80/tcp on $IFC .*# admin-web" || fail "foreign identical rule was re-tagged"
    ufw status numbered | grep -q "80/tcp on $IFC .*# scribe-server" && fail "took over an identical foreign rule"
    ufw status numbered | grep -Eq "224.0.0.251 5353/udp on $IFC .*# scribe-server" || fail "mdns rule not scoped"
    ufw status | grep -Eiq "tailscale|wg0|docker" && fail "rule on a non-LAN interface"
    n1="$(owned)"
    fw_open 80 "$IFC" >/dev/null
    fw_open 80 "$IFC" >/dev/null
    [ "$(owned)" = "$n1" ] || fail "repeated install duplicated owned rules ($(owned) vs $n1)"

    fw_close "$rec"
    [ "$(owned)" = 0 ] || fail "owned rules left after uninstall"
    after="$(ufw status numbered | sed 's/^\[ *[0-9]*\]//')"
    [ "$before" = "$after" ] || { echo "before:"; echo "$before"; echo "after:"; echo "$after"; fail "unrelated rules changed"; }
    fw_close "$rec"
    fw_close "$rec"
    [ "$(ufw status numbered | sed 's/^\[ *[0-9]*\]//')" = "$before" ] || fail "repeated uninstall changed rules"
    ufw status | grep -q "^Status: active" || fail "ufw was disabled"
    ufw --force delete "$(ufw status numbered | sed -n 's/^\[ *\([0-9]*\)\].*# admin-web$/\1/p' | head -1)" >/dev/null
    before2="$(ufw status numbered | sed 's/^\[ *[0-9]*\]//')"
    rec="$(fw_open 8080 "$IFC")"
    ufw status numbered | grep -q "8080/tcp on $IFC .*# scribe-server" || fail "tcp rule not interface-scoped"
    fw_close "$rec"
    [ "$(owned)" = 0 ] && [ "$(ufw status numbered | sed 's/^\[ *[0-9]*\]//')" = "$before2" ] || fail "phase 2 cleanup"
    echo "UFW OK owned_rules_per_install=$n1 unrelated_rules_preserved=$(echo "$before" | grep -c ALLOW)"
    ;;

  firewalld)
    mkdir -p /run/dbus
    dbus-daemon --system --fork
    firewalld --nofork --nopid >/tmp/firewalld.log 2>&1 &
    for _ in $(seq 60); do firewall-cmd --state >/dev/null 2>&1 && break; sleep 0.5; done
    firewall-cmd --state >/dev/null 2>&1 || { tail -20 /tmp/firewalld.log; fail "firewalld did not start"; }
    firewall-cmd --permanent --zone=public --change-interface="$IFC" >/dev/null
    firewall-cmd --permanent --zone=public --add-service=ssh >/dev/null
    firewall-cmd --permanent --zone=public --add-port=80/tcp >/dev/null
    firewall-cmd --permanent --zone=public --add-service=mdns >/dev/null
    firewall-cmd --reload >/dev/null
    snap() { firewall-cmd --permanent --zone=public --list-all | grep -E "services:|ports:" | tr -s ' '; }
    before="$(snap | sed 's/ scribe-server//')"

    rec="$(fw_open 80 "$IFC")"
    [ "$rec" = "firewalld 80 public" ] || fail "record $rec"
    firewall-cmd --permanent --zone=public --query-service=scribe-server >/dev/null || fail "service not enabled"
    firewall-cmd --permanent --info-service=scribe-server | grep -q "80/tcp 5353/udp" || fail "service ports"
    fw_open 80 "$IFC" >/dev/null
    [ "$(firewall-cmd --permanent --zone=public --list-services | tr ' ' '\n' | grep -c '^scribe-server$')" = 1 ] || fail "duplicated"

    fw_close "$rec"
    firewall-cmd --permanent --zone=public --query-service=scribe-server >/dev/null 2>&1 && fail "service still enabled"
    [ ! -e /etc/firewalld/services/scribe-server.xml ] || fail "service definition left"
    [ "$(snap)" = "$before" ] || { echo "before: $before"; echo "after: $(snap)"; fail "unrelated config changed"; }
    fw_close "$rec"
    fw_open 80 "$IFC" >/dev/null && fw_close "firewalld 80 public"
    [ "$(snap)" = "$before" ] || fail "repeat cycle changed unrelated config"
    firewall-cmd --state >/dev/null || fail "firewalld stopped"
    echo "FIREWALLD OK preserved: $before"
    ;;
  *) echo "usage: $0 ufw|firewalld" >&2; exit 2 ;;
esac
