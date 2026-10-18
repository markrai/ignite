#!/usr/bin/env bash
# Independent LAN-client discovery diagnostics for Ignite Server (_scribe._tcp).
#
# Run this on a **second physical machine** on the same trusted LAN as a
# host-networked Ignite Server. Multipass / HCS / container-only networking
# must NOT be labeled physical-LAN PASS.
#
#   IGNITE_LAN_DIAG=1 bash deployment/linux/acceptance/diag-lan-client.sh
#   IGNITE_LAN_DIAG=1 IGNITE_LAN_EXPECT_SID=<server_id> \
#     bash deployment/linux/acceptance/diag-lan-client.sh
#
# Optional: IGNITE_LAN_HTTP_PROBE=1 also fetches http://<resolved>:<port>/control/v1/status
# (default 1). Does not pair, claim, or send credentials.
#
# Reports four separate things; this tool can only ever establish the first two:
#   SERVER_LOCAL_ADVERTISEMENT — the advertisement resolved to an address of THIS machine
#   LAN_CLIENT_DISCOVERY       — resolved to another machine and reachable over HTTP
#   KINDLE_DISCOVERY / KINDLE_PAIRING_SYNC — always NOT_TESTED (needs the physical device)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
MODE="${IGNITE_LAN_DIAG:-}"
EXPECT_SID="${IGNITE_LAN_EXPECT_SID:-}"
HTTP_PROBE="${IGNITE_LAN_HTTP_PROBE:-1}"
BROWSE_TIMEOUT="${IGNITE_LAN_BROWSE_TIMEOUT:-12}"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
EVIDENCE="${IGNITE_LAN_EVIDENCE:-$ROOT/deployment/linux/acceptance/evidence-lan-client-$TS}"

die() { echo "diag-lan-client: ERROR: $*" >&2; exit 1; }
say() { echo "diag-lan-client: $*"; }

[ "$MODE" = "1" ] || die "set IGNITE_LAN_DIAG=1 (explicit opt-in). Refusing to run."

mkdir -p "$EVIDENCE"
chmod 700 "$EVIDENCE" 2>/dev/null || true

sanitize() {
  sed -E \
    -e 's/setup_code=[A-Z0-9-]+/setup_code=<redacted>/g' \
    -e 's/scribe_session=[^;[:space:]]+/scribe_session=<redacted>/g'
}

note() { printf '%s\n' "$*" | tee -a "$EVIDENCE/report.txt"; }

{
  echo "tool=diag-lan-client"
  echo "ts_utc=$TS"
  echo "host=$(hostname 2>/dev/null || echo unknown)"
  echo "expect_sid=${EXPECT_SID:-<none>}"
  echo "NOTE=independent discovery requires a resolved address that is NOT on this machine"
} >"$EVIDENCE/report.txt"

say "client interface inventory"
{
  echo "=== ip -brief addr ==="
  ip -brief addr 2>/dev/null || ifconfig -a 2>/dev/null || echo "no ip/ifconfig"
} >"$EVIDENCE/interfaces.txt"
# This machine's own addresses (to tell server-local sightings from independent ones).
{
  ip -o addr show 2>/dev/null | awk '{print $4}' | cut -d/ -f1
  hostname -I 2>/dev/null | tr ' ' '\n'
  echo 127.0.0.1
  echo ::1
} | sed '/^$/d' | sort -u >"$EVIDENCE/local-addrs.txt"

BROWSE="NOT_TESTED"   # PASS | NO_SIGHTING | TIMEOUT | BLOCKED
echo '{"advertisements":[],"count":0}' >"$EVIDENCE/resolved.json"

if command -v avahi-browse >/dev/null 2>&1; then
  say "DNS-SD browse + resolve _scribe._tcp (timeout ${BROWSE_TIMEOUT}s)"
  set +e
  timeout "$BROWSE_TIMEOUT" avahi-browse -rtp _scribe._tcp \
    >"$EVIDENCE/avahi-scribe-resolve.txt" 2>"$EVIDENCE/avahi-browse.err"
  BR_RC=$?
  set -e
  # Parseable resolved rows: =;iface;proto;name;type;domain;host;address;port;txt
  python3 - "$EVIDENCE/avahi-scribe-resolve.txt" "$EVIDENCE/local-addrs.txt" \
    "$(hostname 2>/dev/null || true)" "$EVIDENCE/resolved.json" <<'PY'
import json, re, sys
rows = open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines()
local = {l.strip() for l in open(sys.argv[2], encoding="utf-8") if l.strip()}
me = (sys.argv[3] or "").lower().split(".")[0]
ads = []
for line in rows:
    if not line.startswith("=;"):
        continue
    f = line.split(";", 9)
    if len(f) < 10 or f[4] != "_scribe._tcp":
        continue
    addr = f[7].split("%", 1)[0]
    txt = dict(p.split("=", 1) for p in re.findall(r'"([^"]*)"', f[9]) if "=" in p)
    host = f[6].lower()
    is_local = addr in local or addr.startswith("127.") or addr == "::1" \
        or (me and host.split(".")[0] == me)
    ads.append({"iface": f[1], "family": f[2], "host": f[6], "address": addr,
                "port": int(f[8]) if f[8].isdigit() else None, "txt": txt,
                "local": is_local})
json.dump({"advertisements": ads, "count": len(ads)},
          open(sys.argv[4], "w", encoding="utf-8"), indent=2)
open(sys.argv[4], "a", encoding="utf-8").write("\n")
PY
  ADS="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["count"])' "$EVIDENCE/resolved.json")"
  if [ "$ADS" -gt 0 ]; then
    BROWSE="PASS"
  elif [ "$BR_RC" -eq 124 ]; then
    BROWSE="TIMEOUT"
  elif [ "$BR_RC" -ne 0 ]; then
    BROWSE="BLOCKED"
  else
    BROWSE="NO_SIGHTING"
  fi
  case "$BROWSE" in
    PASS) note "browse=PASS resolved_advertisements=$ADS" ;;
    TIMEOUT) note "browse=TIMEOUT (no resolved record within ${BROWSE_TIMEOUT}s: multicast isolation, firewall, or no advertiser)" ;;
    BLOCKED) note "browse=BLOCKED (avahi-browse rc=$BR_RC; is avahi-daemon running? see avahi-browse.err)" ;;
    NO_SIGHTING) note "browse=NO_SIGHTING (browse completed; no _scribe._tcp advertiser visible)" ;;
  esac
else
  BROWSE="BLOCKED"
  note "browse=BLOCKED (avahi-browse missing — install avahi-utils)"
  echo "avahi-browse not installed" >"$EVIDENCE/avahi-scribe-resolve.txt"
fi

LOCAL_ADS="$(python3 -c 'import json,sys; print(sum(1 for a in json.load(open(sys.argv[1]))["advertisements"] if a["local"]))' "$EVIDENCE/resolved.json")"
REMOTE_ADS="$(python3 -c 'import json,sys; print(sum(1 for a in json.load(open(sys.argv[1]))["advertisements"] if not a["local"]))' "$EVIDENCE/resolved.json")"
note "local_advertisements=$LOCAL_ADS remote_advertisements=$REMOTE_ADS"

HTTP="NOT_TESTED"
SID_MATCH="NOT_TESTED"
if [ "$HTTP_PROBE" = "1" ] && [ "$REMOTE_ADS" -gt 0 ]; then
  say "HTTP connectivity to resolved non-local addresses"
  python3 - "$EVIDENCE/resolved.json" "$EVIDENCE/http-probe.txt" "$EXPECT_SID" <<'PY'
import json, sys, urllib.request
data = json.load(open(sys.argv[1], encoding="utf-8"))
out = open(sys.argv[2], "w", encoding="utf-8")
expect = sys.argv[3] or ""
any_ok = False
sid_match = "NOT_TESTED"
for ad in data.get("advertisements") or []:
    addr = ad.get("address")
    if not addr or ad.get("local"):
        continue
    port = ad.get("port") or 8787
    host = f"[{addr}]" if ":" in addr else addr
    url = f"http://{host}:{port}/control/v1/status"
    out.write(f"probe {url}\n")
    try:
        with urllib.request.urlopen(url, timeout=5) as r:
            j = json.loads(r.read().decode("utf-8", "replace"))
    except Exception as e:
        out.write(f"  error={e}\n")
        continue
    sid = j.get("server_id") or ""
    out.write(f"  http=200 server_id={sid} claimed={j.get('claimed')}\n")
    txt_sid = (ad.get("txt") or {}).get("sid") or ""
    if txt_sid and sid and txt_sid != sid:
        out.write(f"  WARN txt_sid_mismatch txt={txt_sid} status={sid}\n")
    any_ok = True
    if expect:
        if sid == expect:
            sid_match = "PASS"
        elif sid_match != "PASS":
            sid_match = "FAIL"
out.write(f"any_http_ok={any_ok}\n")
out.write(f"sid_match={sid_match}\n")
PY
  sanitize <"$EVIDENCE/http-probe.txt" >"$EVIDENCE/http-probe.sanitized.txt"
  mv "$EVIDENCE/http-probe.sanitized.txt" "$EVIDENCE/http-probe.txt"
  if grep -q 'any_http_ok=True' "$EVIDENCE/http-probe.txt"; then HTTP="PASS"; else HTTP="FAIL"; fi
  SID_MATCH="$(sed -n 's/^sid_match=//p' "$EVIDENCE/http-probe.txt" | head -1)"
  note "http_probe=$HTTP server_id_consistency=${SID_MATCH:-NOT_TESTED}"
fi

if [ "${ADS:-0}" -gt 1 ]; then
  note "WARN=multiple _scribe advertisements visible; Kindle endpoint selection may vary"
fi

VIRTUAL="no"
if grep -qiE 'multipass|docker0|hyper-v|vboxnet|vEthernet|mpqemubr|virbr' "$EVIDENCE/interfaces.txt" 2>/dev/null; then
  VIRTUAL="yes"
  note "WARN=virtual_or_docker_iface_names_seen — results may reflect virtual networking"
fi

# Server-local sightings never count as independent discovery.
SERVER_LOCAL="NOT_OBSERVED"
[ "$LOCAL_ADS" -gt 0 ] && SERVER_LOCAL="OBSERVED"
LAN_CLIENT="NOT_TESTED"
case "$BROWSE" in
  BLOCKED) LAN_CLIENT="BLOCKED" ;;
  TIMEOUT|NO_SIGHTING) LAN_CLIENT="FAIL" ;;
  PASS)
    if [ "$REMOTE_ADS" -eq 0 ]; then
      LAN_CLIENT="NOT_TESTED"   # only this machine's own advertisement was seen
    elif [ "$HTTP_PROBE" != "1" ]; then
      LAN_CLIENT="NOT_TESTED"
    elif [ "$HTTP" != "PASS" ]; then
      LAN_CLIENT="FAIL"
    elif [ -n "$EXPECT_SID" ] && [ "$SID_MATCH" != "PASS" ]; then
      LAN_CLIENT="FAIL"
    else
      LAN_CLIENT="PASS"
    fi
    ;;
esac

{
  echo
  echo "BROWSE=$BROWSE"
  echo "HTTP=$HTTP"
  echo "SERVER_ID_MATCH=$SID_MATCH"
  echo "SERVER_LOCAL_ADVERTISEMENT=$SERVER_LOCAL"
  echo "LAN_CLIENT_DISCOVERY=$LAN_CLIENT"
  echo "VIRTUAL_NETWORK_SUSPECTED=$VIRTUAL"
  echo "KINDLE_DISCOVERY=NOT_TESTED"
  echo "KINDLE_PAIRING_SYNC=NOT_TESTED"
} | tee -a "$EVIDENCE/report.txt"

say "done — evidence in $EVIDENCE"
case "$LAN_CLIENT" in
  PASS) say "independent LAN-client discovery PASS on this client$([ "$VIRTUAL" = yes ] && echo ' (virtual interfaces present — confirm the path is physical)'); Kindle discovery and pairing remain NOT_TESTED." ;;
  NOT_TESTED) say "no independent discovery shown (only server-local advertisement, or HTTP probe disabled)." ;;
  *) say "independent LAN-client discovery $LAN_CLIENT (see report.txt)." ;;
esac
