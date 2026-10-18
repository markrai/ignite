#!/usr/bin/env bash
# Regressions for diag-lan-client.sh / diag-lan-server.sh with hermetic fakes (avahi-browse,
# ip, curl, ss, systemd-detect-virt, sysfs). Proves independent discovery PASSes only for a
# remote, separately identified target on a physical on-link interface whose TXT sid matches
# its HTTP server_id — and never for server-local, virtual-only, unreachable, mismatched, or
# unidentified sightings. Kindle results are never PASS. No real network traffic.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/ignite-diag-lan.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

command -v python3 >/dev/null 2>&1 || fail "python3 required"
command -v timeout >/dev/null 2>&1 || fail "timeout(1) required"

BIN="$TMP/bin"; AV="$TMP/avahi"; SYS="$TMP/sys"
mkdir -p "$BIN" "$AV" "$SYS/eth0/device" "$SYS/docker0" "$SYS/mpqemubr0" "$SYS/lo"
export IGNITE_LAN_SYSFS_NET="$SYS"

# This machine: eth0 is the physical LAN NIC; docker0 / mpqemubr0 are virtual bridges.
cat >"$BIN/ip" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  "-o addr show")
    echo "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever"
    echo "2: eth0    inet 192.168.77.10/24 brd 192.168.77.255 scope global eth0\\       valid_lft forever"
    echo "2: eth0    inet6 fe80::10/64 scope link \\       valid_lft forever"
    echo "3: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0\\       valid_lft forever"
    echo "4: mpqemubr0    inet 10.97.0.1/24 brd 10.97.0.255 scope global mpqemubr0\\       valid_lft forever" ;;
  *) echo "eth0 UP 192.168.77.10/24"; echo "docker0 DOWN 172.17.0.1/16"; echo "mpqemubr0 UP 10.97.0.1/24" ;;
esac
EOF
# HTTP table: "<host:port> <code> <server_id>"; anything else is connection refused.
cat >"$BIN/curl" <<'EOF'
#!/usr/bin/env bash
out="" fmt="" url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift ;;
    -w) fmt="$2"; shift ;;
    --max-time|--noproxy) shift ;;
    -*) ;;
    *) url="$1" ;;
  esac
  shift
done
echo "$url" >>"$FAKE_STATE/curl.log"
hp="${url#http://}"; hp="${hp%%/*}"
row="$(grep -F "$hp " "$FAKE_STATE/http-table" 2>/dev/null | head -1 || true)"
if [ -z "$row" ]; then
  echo "curl: (7) Failed to connect to $hp: Connection refused" >&2
  code=000; body=""; rc=7
else
  code="$(echo "$row" | awk '{print $2}')"; sid="$(echo "$row" | awk '{print $3}')"
  body="{\"v\":1,\"server_id\":\"$sid\",\"claimed\":false,\"session\":null}"; rc=0
fi
w="${fmt//%\{http_code\}/$code}"
if [ -n "$out" ]; then printf '%s' "$body" >"$out"; printf '%b' "$w"; else printf '%s%b' "$body" "$w"; fi
exit "$rc"
EOF
cat >"$BIN/systemd-detect-virt" <<'EOF'
#!/usr/bin/env bash
echo "${FAKE_VIRT:-none}"; [ "${FAKE_VIRT:-none}" != none ]
EOF
cat >"$BIN/ss" <<'EOF'
#!/usr/bin/env bash
echo "State  Recv-Q Send-Q Local Address:Port Peer Address:Port"
echo "LISTEN 0      128    ${FAKE_LISTEN:-0.0.0.0:8787}       0.0.0.0:*"
EOF
cat >"$AV/avahi-browse" <<'EOF'
#!/usr/bin/env bash
case "${FAKE_AVAHI:-rows}" in
  rows) cat "$FAKE_STATE/rows" 2>/dev/null || true ;;
  timeout) sleep 30 ;;
  daemon) echo "Failed to create client object: Daemon not running" >&2; exit 1 ;;
esac
EOF
chmod +x "$BIN"/* "$AV/avahi-browse"

# System tools without any avahi-browse, for the "missing Avahi" cases.
NOAV="$TMP/noavahi"; mkdir -p "$NOAV"
for f in /usr/bin/* /bin/*; do
  b="$(basename "$f")"; [ "$b" = avahi-browse ] && continue
  [ -e "$NOAV/$b" ] || ln -s "$f" "$NOAV/$b"
done
WITH_AVAHI="$BIN:$AV:/usr/bin:/bin"
NO_AVAHI="$BIN:$NOAV"

ad() { # ad <iface> <address> <port> <sid> → one resolved avahi -p row
  echo "+;$1;IPv4;Ignite\\032Server;_scribe._tcp;local"
  echo "=;$1;IPv4;Ignite\\032Server\\032on\\032akuma;_scribe._tcp;local;akuma.local;$2;$3;\"v=1\" \"sid=$4\" \"name=Ignite Server on akuma\" \"cp=/control/v1\""
}

n=0
setup() { # setup → fresh state; then append rows with `ad ... >>"$FAKE_STATE/rows"`
  n=$((n + 1))
  export FAKE_STATE="$TMP/s$n"
  mkdir -p "$FAKE_STATE"; : >"$FAKE_STATE/rows"; : >"$FAKE_STATE/http-table"; : >"$FAKE_STATE/curl.log"
  unset FAKE_AVAHI FAKE_VIRT FAKE_LISTEN
  export IGNITE_LAN_EXPECT_SID=sid-target
}
http() { echo "$1 $2 $3" >>"$FAKE_STATE/http-table"; }
run_client() { # run_client [PATH]
  local ev="$FAKE_STATE/client-ev"
  PATH="${1:-$WITH_AVAHI}" IGNITE_LAN_EVIDENCE="$ev" IGNITE_LAN_BROWSE_TIMEOUT=2 \
    bash "$DIR/diag-lan-client.sh" >"$FAKE_STATE/client.out" 2>&1 \
    || fail "client diag exited nonzero: $(cat "$FAKE_STATE/client.out")"
  REPORT="$ev/report.txt"
}
field() { sed -n "s/^$1=//p" "$REPORT" | tail -1; }
expect() { # expect <FIELD> <value> <label>
  [ "$(field "$1")" = "$2" ] || fail "$3: $1=$(field "$1") (want $2); report:
$(cat "$REPORT")"
}
never_kindle_pass() {
  expect KINDLE_DISCOVERY NOT_TESTED "$1"; expect KINDLE_PAIRING_SYNC NOT_TESTED "$1"
}

# --- client ------------------------------------------------------------------------------------
setup; ad eth0 192.168.77.20 8787 sid-target >>"$FAKE_STATE/rows"; http 192.168.77.20:8787 200 sid-target
run_client
expect LAN_CLIENT_DISCOVERY PASS "target"; expect SERVER_ID_MATCH PASS "target"; expect HTTP PASS "target"
expect TARGET_ENDPOINT 192.168.77.20:8787 "target"; expect TARGET_INTERFACE eth0 "target"
expect SERVER_LOCAL_ADVERTISEMENT NOT_OBSERVED "target"; never_kindle_pass "target"
pass "remote target on physical on-link interface with matching TXT/HTTP identity → PASS (no opt-in flag)"

setup
ad eth0 192.168.77.21 8787 sid-other >>"$FAKE_STATE/rows"; http 192.168.77.21:8787 200 sid-other
ad eth0 192.168.77.20 8787 sid-target >>"$FAKE_STATE/rows"; http 192.168.77.20:8787 200 sid-target
run_client
expect LAN_CLIENT_DISCOVERY PASS "multi"; expect DISTINCT_ADVERTISED_SIDS 2 "multi"
expect TARGET_ENDPOINT 192.168.77.20:8787 "multi"
pass "multiple advertisers: the target is selected by sid, not by first sighting"

setup; unset IGNITE_LAN_EXPECT_SID
ad eth0 192.168.77.21 8787 sid-other >>"$FAKE_STATE/rows"; http 192.168.77.21:8787 200 sid-other
run_client
expect LAN_CLIENT_DISCOVERY NOT_TESTED "no expect"; expect TARGET_ADVERTISEMENT NOT_TESTED "no expect"
grep -q 'set_IGNITE_LAN_EXPECT_SID' "$REPORT" || fail "no-expect reason missing"
pass "reachable but unidentified advertiser is never PASS without IGNITE_LAN_EXPECT_SID"

setup; ad eth0 192.168.77.21 8787 sid-other >>"$FAKE_STATE/rows"; http 192.168.77.21:8787 200 sid-other
run_client
expect LAN_CLIENT_DISCOVERY FAIL "other only"; expect TARGET_ADVERTISEMENT NOT_FOUND "other only"
pass "only unrelated advertisers → FAIL target_sid_not_advertised"

setup; ad eth0 192.168.77.20 8787 sid-target >>"$FAKE_STATE/rows"; http 192.168.77.20:8787 200 sid-imposter
run_client
expect LAN_CLIENT_DISCOVERY FAIL "txt≠http"; expect SERVER_ID_MATCH FAIL "txt≠http"
grep -q 'target_txt_sid_differs_from_http_server_id' "$REPORT" || fail "mismatch reason missing"
setup; ad eth0 192.168.77.20 8787 sid-stale >>"$FAKE_STATE/rows"; http 192.168.77.20:8787 200 sid-target
run_client
expect LAN_CLIENT_DISCOVERY FAIL "http=target, txt=other"
pass "TXT sid ≠ HTTP server_id for the target is FAIL (both directions), not a warning"

setup; ad eth0 192.168.77.30 8787 sid-target >>"$FAKE_STATE/rows"
run_client
expect LAN_CLIENT_DISCOVERY FAIL "unreachable"; expect HTTP FAIL "unreachable"
grep -q 'target_unreachable_over_http' "$REPORT" || fail "unreachable reason missing"
grep -q 'Connection refused' "$FAKE_STATE/client-ev/http-probe.txt" || fail "probe error not recorded"
pass "advertised but unreachable target → FAIL with the HTTP error recorded"

setup
ad mpqemubr0 10.97.0.5 8787 sid-target >>"$FAKE_STATE/rows"; http 10.97.0.5:8787 200 sid-target
ad docker0 172.17.0.2 8787 sid-target >>"$FAKE_STATE/rows"; http 172.17.0.2:8787 200 sid-target
run_client
expect LAN_CLIENT_DISCOVERY FAIL "virtual only"
grep -q 'target_only_via_virtual_interface_or_off_subnet_address' "$REPORT" || fail "virtual reason missing"
[ ! -s "$FAKE_STATE/curl.log" ] || fail "probed a virtual-interface address: $(cat "$FAKE_STATE/curl.log")"
setup; ad eth0 192.168.99.5 8787 sid-target >>"$FAKE_STATE/rows"; http 192.168.99.5:8787 200 sid-target
run_client
expect LAN_CLIENT_DISCOVERY FAIL "off subnet"
pass "target seen only via Multipass/Docker bridges or off-subnet → FAIL, never probed as LAN"

setup; ad eth0 192.168.77.10 8787 sid-target >>"$FAKE_STATE/rows"; http 192.168.77.10:8787 200 sid-target
run_client
expect LAN_CLIENT_DISCOVERY NOT_TESTED "self"; expect SERVER_LOCAL_ADVERTISEMENT OBSERVED "self"
pass "target resolving to this machine is server-local, never independent discovery"

setup; export FAKE_VIRT=wsl
ad eth0 192.168.77.20 8787 sid-target >>"$FAKE_STATE/rows"; http 192.168.77.20:8787 200 sid-target
run_client
expect LAN_CLIENT_DISCOVERY NOT_TESTED "wsl client"; expect CLIENT_HOST_VIRTUALIZATION wsl "wsl client"
setup; export FAKE_VIRT=kvm
ad eth0 192.168.77.20 8787 sid-target >>"$FAKE_STATE/rows"; http 192.168.77.20:8787 200 sid-target
run_client
expect LAN_CLIENT_DISCOVERY NOT_TESTED "vm client"
pass "a WSL or VM client is never an independent physical client"

setup; export FAKE_AVAHI=timeout
run_client
expect BROWSE TIMEOUT "timeout"; expect LAN_CLIENT_DISCOVERY FAIL "timeout"
grep -q '^browse=PASS' "$REPORT" && fail "timeout: conflicting browse=PASS line" || true
setup; export FAKE_AVAHI=daemon
run_client
expect BROWSE BLOCKED "daemon"; expect LAN_CLIENT_DISCOVERY BLOCKED "daemon"
setup
run_client
expect BROWSE NO_SIGHTING "empty"; expect LAN_CLIENT_DISCOVERY FAIL "empty"
setup
run_client "$NO_AVAHI"
expect BROWSE BLOCKED "no avahi"; expect LAN_CLIENT_DISCOVERY BLOCKED "no avahi"; never_kindle_pass "no avahi"
pass "browse timeout / daemon failure / no advertiser / missing Avahi are distinct, never PASS"

# --- server ------------------------------------------------------------------------------------
run_server() { # run_server [PATH]
  local ev="$FAKE_STATE/server-ev"
  PATH="${1:-$WITH_AVAHI}" IGNITE_LAN_EVIDENCE="$ev" \
    bash "$DIR/diag-lan-server.sh" >"$FAKE_STATE/server.out" 2>&1 \
    || fail "server diag exited nonzero: $(cat "$FAKE_STATE/server.out")"
  REPORT="$ev/report.txt"
}
server_common() {
  expect LAN_CLIENT_DISCOVERY NOT_TESTED "server $1"; never_kindle_pass "server $1"
}

setup; http 127.0.0.1:8787 200 sid-target
ad eth0 192.168.77.10 8787 sid-target >>"$FAKE_STATE/rows"
ad eth0 192.168.77.21 8787 sid-other >>"$FAKE_STATE/rows"
run_server
expect SERVER_RUNNING PASS "server ok"; expect SERVER_ID sid-target "server ok"
expect SERVER_LOCAL_ADVERTISEMENT OBSERVED "server ok"; expect ADVERTISED_PORTS 8787 "server ok"
expect ADVERTISED_ON_PHYSICAL_LAN eth0/192.168.77.10 "server ok"; expect OTHER_SCRIBE_ADVERTISERS 1 "server ok"
expect PHYSICAL_LAN_ADDRESSES eth0/192.168.77.10 "server ok"; expect LISTENING "PASS (0.0.0.0:8787)" "server ok"
grep -q '^CLIENT_COMMAND=IGNITE_LAN_EXPECT_SID=sid-target bash ' "$REPORT" || fail "client command not emitted"
server_common ok
pass "server: own advertisement by sid, port matches listener, physical address only (no docker0/mpqemubr0)"

setup; http 127.0.0.1:8787 200 sid-target; ad eth0 192.168.77.10 9999 sid-target >>"$FAKE_STATE/rows"
run_server
expect SERVER_LOCAL_ADVERTISEMENT PORT_MISMATCH "port"; server_common port
setup; http 127.0.0.1:8787 200 sid-target; ad eth0 192.168.77.21 8787 sid-other >>"$FAKE_STATE/rows"
run_server
expect SERVER_LOCAL_ADVERTISEMENT OBSERVED_OTHER_SID_ONLY "other"; server_common other
setup; ad eth0 192.168.77.10 8787 sid-target >>"$FAKE_STATE/rows"
run_server
expect SERVER_RUNNING FAIL "down"; expect SERVER_LOCAL_ADVERTISEMENT UNVERIFIED_SERVER_STATUS_UNAVAILABLE "down"
grep -q '^CLIENT_COMMAND=' "$REPORT" && fail "client command emitted without a server id" || true
pass "server: port mismatch, other-sid-only and unreachable status are never OBSERVED"

setup; http 127.0.0.1:8787 200 sid-target; export FAKE_AVAHI=daemon
run_server; expect SERVER_LOCAL_ADVERTISEMENT BLOCKED "server daemon"; server_common daemon
setup; http 127.0.0.1:8787 200 sid-target
run_server; expect SERVER_LOCAL_ADVERTISEMENT NO_SIGHTING "server empty"
setup; http 127.0.0.1:8787 200 sid-target
run_server "$NO_AVAHI"; expect SERVER_LOCAL_ADVERTISEMENT BLOCKED "server no avahi"; server_common noavahi
setup; http 127.0.0.1:8787 200 sid-target; export FAKE_LISTEN=0.0.0.0:9000
run_server; expect LISTENING FAIL "not listening"
pass "server: daemon failure, no sighting, missing Avahi, and missing listener reported distinctly"

echo "PASS: LAN diagnostic regressions"
