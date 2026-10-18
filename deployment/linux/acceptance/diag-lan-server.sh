#!/usr/bin/env bash
# Server-side LAN discovery diagnostic for a host-networked Ignite Server. Read-only: never
# changes firewall, interfaces, routes or system config; never claims, pairs, authenticates or
# writes notebook data. A server-local sighting is NOT independent LAN discovery; run
# diag-lan-client.sh on a second physical machine for that.
#
#   bash deployment/linux/acceptance/diag-lan-server.sh
#   IGNITE_LAN_BASE_URL=http://127.0.0.1:8787 bash deployment/linux/acceptance/diag-lan-server.sh
#
# Optional: IGNITE_LAN_EVIDENCE=<dir>. Evidence (sanitized) defaults to
# deployment/linux/acceptance/evidence-lan-server-<ts>.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
BASE_URL="${IGNITE_LAN_BASE_URL:-http://127.0.0.1:8787}"
BASE_URL="${BASE_URL%/}"
# Test hook only: sysfs net class root used to tell physical NICs from virtual devices.
SYSFS_NET="${IGNITE_LAN_SYSFS_NET:-/sys/class/net}"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
EVIDENCE="${IGNITE_LAN_EVIDENCE:-$ROOT/deployment/linux/acceptance/evidence-lan-server-$TS}"

die() { echo "diag-lan-server: ERROR: $*" >&2; exit 1; }
say() { echo "diag-lan-server: $*"; }

for t in python3 ip curl; do
  command -v "$t" >/dev/null 2>&1 || die "$t is required"
done
PORT="$(python3 -c 'import sys,urllib.parse as u; p=u.urlsplit(sys.argv[1]); print(p.port or (443 if p.scheme=="https" else 80))' "$BASE_URL")" \
  || die "cannot parse IGNITE_LAN_BASE_URL=$BASE_URL"

mkdir -p "$EVIDENCE"
chmod 700 "$EVIDENCE" 2>/dev/null || true

sanitize() {
  sed -E \
    -e 's/setup_code=[A-Z0-9-]+/setup_code=<redacted>/g' \
    -e 's/scribe_session=[^;[:space:]]+/scribe_session=<redacted>/g' \
    -e 's/[Aa]uthorization:[[:space:]]*Bearer[[:space:]]+[^[:space:]]+/Authorization: Bearer <redacted>/g'
}

note() { printf '%s\n' "$*" | tee -a "$EVIDENCE/report.txt"; }

{
  echo "tool=diag-lan-server"
  echo "ts_utc=$TS"
  echo "base_url=$BASE_URL"
  echo "host=$(hostname 2>/dev/null || echo unknown)"
} >"$EVIDENCE/report.txt"

VIRT="unknown"
if command -v systemd-detect-virt >/dev/null 2>&1; then
  VIRT="$(systemd-detect-virt 2>/dev/null || true)"
  VIRT="${VIRT:-unknown}"
elif grep -qiE 'microsoft|wsl' /proc/sys/kernel/osrelease 2>/dev/null; then
  VIRT="wsl"
fi

say "interface inventory"
{
  echo "=== ip -brief addr ==="; ip -brief addr 2>&1 || true
  echo; echo "=== ip route ==="; ip route 2>&1 || true
} >"$EVIDENCE/interfaces.txt"
ip -o addr show 2>/dev/null | awk '{print $2 "\t" $4}' >"$EVIDENCE/local-inventory.tsv" || true
: >"$EVIDENCE/physical-ifaces.txt"
for i in $(cut -f1 "$EVIDENCE/local-inventory.tsv" | sort -u); do
  [ -e "$SYSFS_NET/$i/device" ] && echo "$i" >>"$EVIDENCE/physical-ifaces.txt"
done

say "control status ($BASE_URL)"
STATUS_HTTP="$(curl -sS --noproxy '*' --max-time 5 -o "$EVIDENCE/status.raw.json" -w '%{http_code}' \
  "$BASE_URL/control/v1/status" 2>"$EVIDENCE/status.err" || true)"
touch "$EVIDENCE/status.raw.json"
sanitize <"$EVIDENCE/status.raw.json" >"$EVIDENCE/status.json"
rm -f "$EVIDENCE/status.raw.json"
SERVER_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("server_id") or "")' "$EVIDENCE/status.json" 2>/dev/null || true)"
note "status_http=${STATUS_HTTP:-000} server_id=${SERVER_ID:-unknown}"

say "listening sockets on port $PORT"
{ ss -ltn 2>/dev/null || netstat -ltn 2>/dev/null || echo "ss/netstat unavailable"; } >"$EVIDENCE/listen.txt"
LISTEN_ADDR="$(awk -v p=":$PORT" '{for (i=1;i<=NF;i++) if (substr($i, length($i)-length(p)+1) == p) {print $i; exit}}' "$EVIDENCE/listen.txt")"

BROWSE_RC=127
if command -v avahi-browse >/dev/null 2>&1; then
  say "server-local avahi browse (NOT independent LAN-client proof)"
  set +e
  timeout 10 avahi-browse -rtp _scribe._tcp >"$EVIDENCE/avahi-browse.raw" 2>"$EVIDENCE/avahi-browse.err"
  BROWSE_RC=$?
  set -e
  sanitize <"$EVIDENCE/avahi-browse.raw" >"$EVIDENCE/avahi-browse.txt"
  rm -f "$EVIDENCE/avahi-browse.raw"
else
  echo "avahi-browse not installed" >"$EVIDENCE/avahi-browse.txt"
fi

say "firewall status (read-only)"
FW_IFACE="$(python3 - "$EVIDENCE" <<'PY'
import ipaddress, sys
ev = sys.argv[1]
NON_LAN = ("lo", "docker", "br-", "veth", "virbr", "vnet", "tailscale", "tun", "tap", "wg",
           "zt", "cni", "flannel", "cali", "kube", "lxc", "lxd", "podman", "vmnet", "vboxnet",
           "utun", "llw", "awdl", "ham", "mpqemu")
phys = set(open(f"{ev}/physical-ifaces.txt").read().split())
for line in open(f"{ev}/local-inventory.tsv"):
    p = line.split()
    if len(p) == 2 and not p[0].startswith(NON_LAN) and p[0] in phys:
        print(p[0]); break
PY
)"
{
  if command -v firewall-cmd >/dev/null 2>&1; then
    echo "=== firewalld state ==="; firewall-cmd --state 2>&1 || true
    if [ -n "$FW_IFACE" ]; then
      Z="$(firewall-cmd --get-zone-of-interface="$FW_IFACE" 2>/dev/null || firewall-cmd --get-default-zone 2>/dev/null || true)"
      echo "=== zone for $FW_IFACE: ${Z:-unknown} ==="
      [ -n "$Z" ] && firewall-cmd --zone="$Z" --list-all 2>&1 || true
    fi
  elif command -v ufw >/dev/null 2>&1; then
    echo "=== ufw ==="; ufw status verbose 2>&1 || true
  else
    echo "no firewalld/ufw front-end found (nft/iptables need root; not inspected)"
  fi
} >"$EVIDENCE/firewall.txt"

python3 - "$EVIDENCE" "$SERVER_ID" "$PORT" "$BROWSE_RC" "$VIRT" "${STATUS_HTTP:-000}" "${LISTEN_ADDR:-}" <<'PY'
import ipaddress, re, sys
ev, sid, port, browse_rc, virt, status_http, listen = sys.argv[1:8]
port, browse_rc = int(port), int(browse_rc)
NON_LAN = ("lo", "docker", "br-", "veth", "virbr", "vnet", "tailscale", "tun", "tap", "wg",
           "zt", "cni", "flannel", "cali", "kube", "lxc", "lxd", "podman", "vmnet", "vboxnet",
           "utun", "llw", "awdl", "ham", "mpqemu")
RFC1918 = [ipaddress.ip_network(n) for n in ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16")]
ULA = ipaddress.ip_network("fc00::/7")
def private_lan(ip): return any(ip in n for n in RFC1918) if ip.version == 4 else ip in ULA

phys = set(open(f"{ev}/physical-ifaces.txt").read().split())
lan = []   # (iface, address) the server's netpolicy would advertise on AND backed by a NIC
for line in open(f"{ev}/local-inventory.tsv"):
    p = line.split()
    if len(p) != 2 or p[0].startswith(NON_LAN):
        continue
    try:
        ip = ipaddress.ip_interface(p[1]).ip
    except ValueError:
        continue
    if private_lan(ip) and p[0] in phys:
        lan.append((p[0], str(ip)))
lan_ips = {a for _, a in lan}

own, other = [], 0
for line in open(f"{ev}/avahi-browse.txt", errors="replace"):
    if not line.startswith("=;"):
        continue
    f = line.rstrip("\n").split(";", 9)
    if len(f) < 10 or f[4] != "_scribe._tcp":
        continue
    txt = dict(x.split("=", 1) for x in re.findall(r'"([^"]*)"', f[9]) if "=" in x)
    if sid and txt.get("sid") == sid:
        own.append((f[1], f[7].split("%", 1)[0], int(f[8]) if f[8].isdigit() else None))
    else:
        other += 1

if browse_rc == 127: ad = "BLOCKED"
elif not sid: ad = "UNVERIFIED_SERVER_STATUS_UNAVAILABLE"
elif own: ad = "OBSERVED" if all(p == port for _, _, p in own) else "PORT_MISMATCH"
elif other: ad = "OBSERVED_OTHER_SID_ONLY"
elif browse_rc == 124: ad = "TIMEOUT"
elif browse_rc != 0: ad = "BLOCKED"
else: ad = "NO_SIGHTING"
on_lan = sorted({f"{i}/{a}" for i, a, _ in own if a in lan_ips})

res = {
    "SERVER_RUNNING": "PASS" if status_http == "200" and sid else "FAIL",
    "SERVER_ID": sid or "unknown",
    "LISTEN_PORT": port,
    "LISTENING": f"PASS ({listen})" if listen else "FAIL",
    "PHYSICAL_LAN_ADDRESSES": ",".join(f"{i}/{a}" for i, a in lan) or "none",
    "SERVER_HOST_VIRTUALIZATION": virt,
    "SERVER_LOCAL_ADVERTISEMENT": ad,
    "ADVERTISED_PORTS": ",".join(sorted({str(p) for _, _, p in own})) or "none",
    "ADVERTISED_ON_PHYSICAL_LAN": ",".join(on_lan) or "NO",
    "OTHER_SCRIBE_ADVERTISERS": other,
    "LAN_CLIENT_DISCOVERY": "NOT_TESTED",
    "KINDLE_DISCOVERY": "NOT_TESTED",
    "KINDLE_PAIRING_SYNC": "NOT_TESTED",
}
with open(f"{ev}/verdict.txt", "w") as v:
    for k, val in res.items():
        v.write(f"{k}={val}\n")
PY

{
  echo
  cat "$EVIDENCE/verdict.txt"
  if [ -n "$SERVER_ID" ]; then
    echo "CLIENT_COMMAND=IGNITE_LAN_EXPECT_SID=$SERVER_ID bash deployment/linux/acceptance/diag-lan-client.sh"
  fi
} | tee -a "$EVIDENCE/report.txt"
rm -f "$EVIDENCE/verdict.txt"

say "done — evidence in $EVIDENCE"
say "Independent LAN-client discovery stays NOT_TESTED until diag-lan-client.sh passes on another physical machine."
