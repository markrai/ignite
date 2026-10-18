#!/usr/bin/env bash
# Server-side LAN discovery diagnostics for host-networked Ignite Server.
#
# Opt-in only. Does not reconfigure firewalls or netpolicy.
# Local avahi sightings are NOT physical Kindle discovery proof.
#
#   IGNITE_LAN_DIAG=1 bash deployment/linux/acceptance/diag-lan-server.sh
#   IGNITE_LAN_DIAG=1 IGNITE_LAN_BASE_URL=http://127.0.0.1:8787 \
#     bash deployment/linux/acceptance/diag-lan-server.sh
#
# Evidence (sanitized) under IGNITE_LAN_EVIDENCE or ./evidence-lan-server-<ts>.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
MODE="${IGNITE_LAN_DIAG:-}"
BASE_URL="${IGNITE_LAN_BASE_URL:-http://127.0.0.1:8787}"
BASE_URL="${BASE_URL%/}"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
EVIDENCE="${IGNITE_LAN_EVIDENCE:-$ROOT/deployment/linux/acceptance/evidence-lan-server-$TS}"

die() { echo "diag-lan-server: ERROR: $*" >&2; exit 1; }
say() { echo "diag-lan-server: $*"; }

[ "$MODE" = "1" ] || die "set IGNITE_LAN_DIAG=1 (explicit opt-in). Refusing to run."

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
  echo "NOTE=local mDNS sighting ≠ physical Kindle discovery (Milestone B)"
} >"$EVIDENCE/report.txt"

say "collecting interface inventory"
{
  echo "=== ip -brief addr ==="
  ip -brief addr 2>/dev/null || ifconfig -a 2>/dev/null || echo "no ip/ifconfig"
  echo
  echo "=== ip route ==="
  ip route 2>/dev/null || echo "no ip route"
} | tee "$EVIDENCE/interfaces.txt" >/dev/null

# Heuristic eligible LAN candidates (non-loopback, non-docker bridge naming hints).
python3 - "$EVIDENCE/interfaces.txt" "$EVIDENCE/eligible-guess.txt" <<'PY' || true
import re, sys
text = open(sys.argv[1], encoding="utf-8", errors="replace").read()
skip = re.compile(r"\b(lo|docker0|br-|veth|virbr|cni|flannel|tun|tap)\b", re.I)
addrs = []
for line in text.splitlines():
    if skip.search(line):
        continue
    for m in re.finditer(r"\b(\d{1,3}(?:\.\d{1,3}){3})/\d+\b", line):
        ip = m.group(1)
        if ip.startswith("127.") or ip.startswith("172.17."):
            continue
        addrs.append((line.split()[0] if line.split() else "?", ip))
out = open(sys.argv[2], "w", encoding="utf-8")
if not addrs:
    out.write("none_guessed\n")
else:
    for iface, ip in addrs:
        out.write(f"{iface}\t{ip}\n")
PY
note "eligible_iface_guess=$(tr '\n' ';' <"$EVIDENCE/eligible-guess.txt" | sed 's/;$//')"

say "control status"
: >"$EVIDENCE/status.raw.json"
STATUS_HTTP="$(curl -sS -o "$EVIDENCE/status.raw.json" -w '%{http_code}' "$BASE_URL/control/v1/status" || true)"
sanitize <"$EVIDENCE/status.raw.json" >"$EVIDENCE/status.json"
rm -f "$EVIDENCE/status.raw.json"
note "status_http=$STATUS_HTTP"
SERVER_ID="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d.get("server_id") or "")' "$EVIDENCE/status.json" 2>/dev/null || true)"
CLAIMED="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d.get("claimed"))' "$EVIDENCE/status.json" 2>/dev/null || true)"
note "server_id=${SERVER_ID:-unknown}"
note "claimed=${CLAIMED:-unknown}"

say "listen port probes"
ss -ltnp 2>/dev/null | tee "$EVIDENCE/ss-listen.txt" >/dev/null || \
  netstat -ltnp 2>/dev/null | tee "$EVIDENCE/ss-listen.txt" >/dev/null || \
  echo "ss/netstat unavailable" >"$EVIDENCE/ss-listen.txt"
if grep -Eq ':8787\b' "$EVIDENCE/ss-listen.txt"; then
  note "tcp_8787=LISTEN_observed"
else
  note "tcp_8787=NOT_OBSERVED (check listen config / host-net)"
fi

say "discovery config hints (compose/image defaults; not authoritative)"
note "expected_service=_scribe._tcp"
note "expected_txt_keys=v,sid,name,cp"
note "compose_default_advertise=true (deployment/linux/docker/config.toml)"
note "network_mode_expect=host"

say "server-local avahi browse (NOT independent LAN-client or Kindle proof)"
# -t: dump the cache and exit; -p: parseable lines. The timeout bounds a hung daemon.
LOCAL_AD="NOT_TESTED"
if command -v avahi-browse >/dev/null 2>&1; then
  set +e
  timeout 10 avahi-browse -rtp _scribe._tcp >"$EVIDENCE/avahi-browse.txt" 2>"$EVIDENCE/avahi-browse.err"
  BR_RC=$?
  set -e
  # Resolved records start with '='; ';'-separated, TXT last. Keep only _scribe._tcp rows.
  grep -E '^=;' "$EVIDENCE/avahi-browse.txt" 2>/dev/null | grep -F '_scribe._tcp' \
    | sanitize >"$EVIDENCE/avahi-scribe.txt" || true
  if [ -s "$EVIDENCE/avahi-scribe.txt" ]; then
    if [ -n "$SERVER_ID" ] && ! grep -qF "sid=$SERVER_ID" "$EVIDENCE/avahi-scribe.txt"; then
      LOCAL_AD="OBSERVED_OTHER_SID"
      note "local_mdns=OBSERVED_OTHER_SID (_scribe._tcp seen, but not sid=$SERVER_ID)"
    else
      LOCAL_AD="OBSERVED"
      note "local_mdns=OBSERVED (server-local only; not independent LAN-client proof)"
    fi
  elif [ "$BR_RC" -eq 124 ]; then
    LOCAL_AD="TIMEOUT"
    note "local_mdns=TIMEOUT (avahi-browse did not finish in 10s; daemon busy or hung)"
  elif [ "$BR_RC" -ne 0 ]; then
    LOCAL_AD="BLOCKED"
    note "local_mdns=BLOCKED (avahi-browse rc=$BR_RC; is avahi-daemon running? see avahi-browse.err)"
  else
    LOCAL_AD="NO_SIGHTING"
    note "local_mdns=NO_SIGHTING (advertise off, multicast blocked, or announce not yet sent)"
  fi
else
  LOCAL_AD="BLOCKED"
  note "local_mdns=BLOCKED (avahi-browse missing; install avahi-utils for local browse)"
  echo "avahi-browse not installed" >"$EVIDENCE/avahi-browse.txt"
fi

say "firewall status (read-only)"
{
  echo "=== ufw ==="
  ufw status verbose 2>/dev/null || echo "ufw unavailable"
  echo
  echo "=== firewall-cmd ==="
  firewall-cmd --list-all 2>/dev/null || echo "firewalld unavailable"
  echo
  echo "=== nft/iptables (truncated) ==="
  nft list ruleset 2>/dev/null | head -80 || iptables -L -n 2>/dev/null | head -40 || echo "none"
} >"$EVIDENCE/firewall.txt" 2>&1 || true
if grep -Eq '8787|mdns|5353' "$EVIDENCE/firewall.txt"; then
  note "firewall_mentions_8787_or_mdns=yes (inspect firewall.txt; no auto-change)"
else
  note "firewall_mentions_8787_or_mdns=no_or_unknown"
fi

{
  echo
  echo "RESULT=REPORT_ONLY"
  echo "SERVER_LOCAL_ADVERTISEMENT=$LOCAL_AD"
  echo "LAN_CLIENT_DISCOVERY=NOT_TESTED"
  echo "KINDLE_DISCOVERY=NOT_TESTED"
  echo "KINDLE_PAIRING_SYNC=NOT_TESTED"
  echo "NEXT=run diag-lan-client.sh from a second physical machine on the same LAN"
} | tee -a "$EVIDENCE/report.txt"

say "done — evidence in $EVIDENCE"
say "Independent LAN-client discovery remains NOT_TESTED until diag-lan-client.sh runs on another machine."
