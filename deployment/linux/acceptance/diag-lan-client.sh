#!/usr/bin/env bash
# Independent LAN-client discovery diagnostic for Ignite Server (_scribe._tcp). Read-only:
# never changes firewall, interfaces, routes or system config; never claims, pairs, sends
# credentials or writes notebook data.
#
# Run on a SECOND PHYSICAL machine on the same trusted LAN as the server:
#
#   IGNITE_LAN_EXPECT_SID=<server_id from diag-lan-server.sh> \
#     bash deployment/linux/acceptance/diag-lan-client.sh
#
# Optional: IGNITE_LAN_EVIDENCE=<dir>, IGNITE_LAN_BROWSE_TIMEOUT=<s> (default 12),
# IGNITE_LAN_HTTP_PROBE=0 (skip HTTP). Needs avahi-browse (avahi-tools / avahi-utils),
# python3, curl, ip.
#
# LAN_CLIENT_DISCOVERY=PASS only when ALL hold for one advertisement:
#   * this client is not a VM / WSL guest (systemd-detect-virt),
#   * TXT sid equals IGNITE_LAN_EXPECT_SID (the separately identified target),
#   * it resolved to an address NOT on this machine,
#   * it arrived on a physical, LAN-eligible interface (same name policy as the server's
#     netpolicy, plus a real device in sysfs) and resolved to a private address on that
#     interface's subnet,
#   * GET /control/v1/status at that address:port answered 200 with server_id == TXT sid.
# A TXT/HTTP identity mismatch for the target is FAIL, never a warning.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
EXPECT_SID="${IGNITE_LAN_EXPECT_SID:-}"
HTTP_PROBE="${IGNITE_LAN_HTTP_PROBE:-1}"
BROWSE_TIMEOUT="${IGNITE_LAN_BROWSE_TIMEOUT:-12}"
# Test hook only: sysfs net class root used to tell physical NICs from virtual devices.
SYSFS_NET="${IGNITE_LAN_SYSFS_NET:-/sys/class/net}"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
EVIDENCE="${IGNITE_LAN_EVIDENCE:-$ROOT/deployment/linux/acceptance/evidence-lan-client-$TS}"

die() { echo "diag-lan-client: ERROR: $*" >&2; exit 1; }
say() { echo "diag-lan-client: $*"; }

for t in python3 ip; do
  command -v "$t" >/dev/null 2>&1 || die "$t is required"
done

mkdir -p "$EVIDENCE"
chmod 700 "$EVIDENCE" 2>/dev/null || true

sanitize() {
  sed -E \
    -e 's/setup_code=[A-Z0-9-]+/setup_code=<redacted>/g' \
    -e 's/scribe_session=[^;[:space:]]+/scribe_session=<redacted>/g' \
    -e 's/[Aa]uthorization:[[:space:]]*Bearer[[:space:]]+[^[:space:]]+/Authorization: Bearer <redacted>/g'
}

ME="$(hostname 2>/dev/null || echo unknown)"
{
  echo "tool=diag-lan-client"
  echo "ts_utc=$TS"
  echo "host=$ME"
  echo "expect_sid=${EXPECT_SID:-<none>}"
} >"$EVIDENCE/report.txt"

# Is this client itself virtualized? A VM / WSL guest is never an independent physical client.
VIRT="unknown"
if command -v systemd-detect-virt >/dev/null 2>&1; then
  VIRT="$(systemd-detect-virt 2>/dev/null || true)"
  VIRT="${VIRT:-unknown}"
elif grep -qiE 'microsoft|wsl' /proc/sys/kernel/osrelease 2>/dev/null; then
  VIRT="wsl"
fi

say "client interface inventory"
ip -brief addr >"$EVIDENCE/interfaces.txt" 2>&1 || true
# iface<TAB>cidr for every address on this machine.
ip -o addr show 2>/dev/null | awk '{print $2 "\t" $4}' >"$EVIDENCE/local-inventory.tsv" || true
# Interfaces backed by a real device (virtual bridges/veth/tun/docker have none in sysfs).
: >"$EVIDENCE/physical-ifaces.txt"
for i in $(cut -f1 "$EVIDENCE/local-inventory.tsv" | sort -u); do
  [ -e "$SYSFS_NET/$i/device" ] && echo "$i" >>"$EVIDENCE/physical-ifaces.txt"
done

BROWSE_RC=127
if command -v avahi-browse >/dev/null 2>&1; then
  say "DNS-SD browse + resolve _scribe._tcp (timeout ${BROWSE_TIMEOUT}s)"
  set +e
  timeout "$BROWSE_TIMEOUT" avahi-browse -rtp _scribe._tcp \
    >"$EVIDENCE/avahi-browse.raw" 2>"$EVIDENCE/avahi-browse.err"
  BROWSE_RC=$?
  set -e
  sanitize <"$EVIDENCE/avahi-browse.raw" >"$EVIDENCE/avahi-browse.txt"
  rm -f "$EVIDENCE/avahi-browse.raw"
else
  echo "avahi-browse not installed" >"$EVIDENCE/avahi-browse.txt"
fi

[ "$HTTP_PROBE" = 1 ] && ! command -v curl >/dev/null 2>&1 && die "curl is required for the HTTP probe"

say "classifying advertisements$([ "$HTTP_PROBE" = 1 ] && echo ' and probing HTTP identity')"
python3 - "$EVIDENCE" "$ME" "$EXPECT_SID" "$BROWSE_RC" "$HTTP_PROBE" "$VIRT" <<'PY'
import ipaddress, json, re, subprocess, sys

ev, me, expect, browse_rc, http_probe, virt = sys.argv[1:7]
browse_rc = int(browse_rc)
me = me.lower().split(".")[0]

# Mirrors ignite/server/scribe-server/src/netpolicy.rs (NON_LAN_PREFIXES, private addresses),
# plus Multipass's bridge.
NON_LAN = ("lo", "docker", "br-", "veth", "virbr", "vnet", "tailscale", "tun", "tap", "wg",
           "zt", "cni", "flannel", "cali", "kube", "lxc", "lxd", "podman", "vmnet", "vboxnet",
           "utun", "llw", "awdl", "ham", "mpqemu")
RFC1918 = [ipaddress.ip_network(n) for n in ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16")]
ULA = ipaddress.ip_network("fc00::/7")

def lan_name(n): return not n.startswith(NON_LAN)
def private_lan(ip):
    return any(ip in n for n in RFC1918) if ip.version == 4 else ip in ULA

inv = []
for line in open(f"{ev}/local-inventory.tsv", encoding="utf-8"):
    parts = line.split()
    if len(parts) == 2:
        try:
            inv.append((parts[0], ipaddress.ip_interface(parts[1])))
        except ValueError:
            pass
physical = set(open(f"{ev}/physical-ifaces.txt", encoding="utf-8").read().split())
local_ips = {i.ip for _, i in inv}

def on_link(iface, ip):
    """ip is on the subnet of a physical, LAN-eligible, private address of the receiving iface."""
    return any(n == iface and lan_name(n) and n in physical and private_lan(i.ip)
               and ip.version == i.version and ip in i.network for n, i in inv)

ads = []
for line in open(f"{ev}/avahi-browse.txt", encoding="utf-8", errors="replace"):
    if not line.startswith("=;"):
        continue
    f = line.rstrip("\n").split(";", 9)
    if len(f) < 10 or f[4] != "_scribe._tcp":
        continue
    txt = dict(p.split("=", 1) for p in re.findall(r'"([^"]*)"', f[9]) if "=" in p)
    addr_s = f[7].split("%", 1)[0]
    try:
        ip = ipaddress.ip_address(addr_s)
    except ValueError:
        ip = None
    iface = f[1]
    ad = {
        "iface": iface, "family": f[2], "instance": f[3], "host": f[6], "address": addr_s,
        "port": int(f[8]) if f[8].isdigit() else None, "txt_sid": txt.get("sid", ""),
        "local": bool(ip is not None and (ip in local_ips or ip.is_loopback))
                 or (bool(me) and f[6].lower().split(".")[0] == me),
        "iface_physical": lan_name(iface) and iface in physical,
        "lan_address": bool(ip is not None and private_lan(ip) and on_link(iface, ip)),
        "http": "NOT_TESTED", "http_server_id": "", "identity": "UNVERIFIED",
    }
    ads.append(ad)

# HTTP identity probe: only remote ads on a physical on-link LAN address (direct, no proxy).
probes = {}
out = open(f"{ev}/http-probe.txt", "w", encoding="utf-8")
for ad in ads:
    if ad["local"]:
        ad["http"] = "SKIPPED_LOCAL"; continue
    if not (ad["iface_physical"] and ad["lan_address"]):
        ad["http"] = "SKIPPED_NOT_PHYSICAL_LAN"; continue
    if http_probe != "1" or ad["port"] is None:
        continue
    key = (ad["address"], ad["port"])
    if key not in probes:
        host = f"[{ad['address']}]" if ":" in ad["address"] else ad["address"]
        url = f"http://{host}:{ad['port']}/control/v1/status"
        r = subprocess.run(["curl", "-sS", "--noproxy", "*", "--max-time", "5",
                            "-w", "\n%{http_code}", url], capture_output=True, text=True)
        body, _, code = r.stdout.rpartition("\n")
        sid = ""
        if code == "200":
            try:
                sid = json.loads(body).get("server_id") or ""
            except ValueError:
                code = "200_BAD_JSON"
        probes[key] = (code, sid, (r.stderr or "").strip().splitlines()[-1:] or [""])
        out.write(f"probe {url} iface={ad['iface']}\n  http={code} server_id={sid}"
                  + (f" error={probes[key][2][0]}" if code != "200" else "") + "\n")
    code, sid, _ = probes[key]
    ad["http"] = "PASS" if code == "200" and sid else "FAIL"
    ad["http_server_id"] = sid
    if ad["http"] == "PASS":
        ad["identity"] = "CONSISTENT" if ad["txt_sid"] and ad["txt_sid"] == sid else "INCONSISTENT"
out.close()

browse = ("BLOCKED" if browse_rc == 127 else "PASS" if ads else "TIMEOUT" if browse_rc == 124
          else "BLOCKED" if browse_rc != 0 else "NO_SIGHTING")
for ad in ads:
    ad["target"] = bool(expect) and expect in (ad["txt_sid"], ad["http_server_id"])
targets = [a for a in ads if a["target"]]
remote_t = [a for a in targets if not a["local"]]
good = [a for a in remote_t if a["iface_physical"] and a["lan_address"] and a["http"] == "PASS"
        and a["txt_sid"] == expect and a["http_server_id"] == expect]
inconsistent_t = [a for a in targets if a["identity"] == "INCONSISTENT"]

def verdict():
    if browse == "BLOCKED":
        return "BLOCKED", "avahi_unavailable"
    if http_probe != "1":
        return "NOT_TESTED", "http_probe_disabled"
    if not expect:
        return "NOT_TESTED", "set_IGNITE_LAN_EXPECT_SID_to_identify_the_target"
    if not ads:
        return "FAIL", f"browse_{browse.lower()}"
    if not targets:
        return "FAIL", "target_sid_not_advertised"
    if inconsistent_t:
        return "FAIL", "target_txt_sid_differs_from_http_server_id"
    if not remote_t:
        return "NOT_TESTED", "target_only_seen_as_this_machine"
    if good:
        return "PASS", "target_discovered_resolved_reached_identity_verified"
    if not any(a["iface_physical"] and a["lan_address"] for a in remote_t):
        return "FAIL", "target_only_via_virtual_interface_or_off_subnet_address"
    return "FAIL", "target_unreachable_over_http"

lan, reason = verdict()
if lan == "PASS" and virt != "none":
    lan, reason = "NOT_TESTED", f"client_is_virtualized_{virt}"

best = (good or remote_t or targets or [None])[0]
json.dump({"advertisements": ads, "count": len(ads)},
          open(f"{ev}/resolved.json", "w", encoding="utf-8"), indent=2)
res = {
    "BROWSE": browse,
    "ADVERTISEMENTS": len(ads),
    "DISTINCT_ADVERTISED_SIDS": len({a["txt_sid"] for a in ads}),
    "TARGET_ADVERTISEMENT": ("NOT_TESTED" if not expect else "FOUND" if targets else "NOT_FOUND"),
    "TARGET_INTERFACE": best["iface"] if best else "",
    "TARGET_ENDPOINT": f"{best['address']}:{best['port']}" if best else "",
    "HTTP": (best["http"] if best and best["http"] in ("PASS", "FAIL") else
             "FAIL" if remote_t and any(a["http"] == "FAIL" for a in remote_t) else "NOT_TESTED"),
    "SERVER_ID_MATCH": ("NOT_TESTED" if not expect or not any(a["http"] == "PASS" for a in targets)
                        else "FAIL" if inconsistent_t or not good else "PASS"),
    "CLIENT_HOST_VIRTUALIZATION": virt,
    "SERVER_LOCAL_ADVERTISEMENT": "OBSERVED" if any(a["local"] for a in ads) else "NOT_OBSERVED",
    "LAN_CLIENT_DISCOVERY": lan,
    "LAN_CLIENT_DISCOVERY_REASON": reason,
    "KINDLE_DISCOVERY": "NOT_TESTED",
    "KINDLE_PAIRING_SYNC": "NOT_TESTED",
}
with open(f"{ev}/verdict.txt", "w", encoding="utf-8") as v:
    for k, val in res.items():
        v.write(f"{k}={val}\n")
PY

{
  echo
  cat "$EVIDENCE/verdict.txt"
} | tee -a "$EVIDENCE/report.txt"
rm -f "$EVIDENCE/verdict.txt"

say "done — evidence in $EVIDENCE"
case "$(sed -n 's/^LAN_CLIENT_DISCOVERY=//p' "$EVIDENCE/report.txt" | tail -1)" in
  PASS) say "independent physical-LAN discovery PASS. Kindle discovery and pairing remain NOT_TESTED." ;;
  *) say "independent physical-LAN discovery not shown — see LAN_CLIENT_DISCOVERY_REASON and http-probe.txt." ;;
esac
