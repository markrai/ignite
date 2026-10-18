#!/usr/bin/env bash
# Status-handling regressions for diag-lan-client.sh / diag-lan-server.sh (fake avahi-browse).
# Proves: server-local sightings never count as independent LAN discovery, timeouts and daemon
# failures are distinct from "no advertiser", and Kindle results are never PASS.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/ignite-diag-lan.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

command -v python3 >/dev/null 2>&1 || fail "python3 required"
command -v timeout >/dev/null 2>&1 || fail "timeout(1) required"

mkdir -p "$TMP/bin"
cat >"$TMP/bin/avahi-browse" <<'EOF'
#!/usr/bin/env bash
case "${FAKE_AVAHI:-empty}" in
  local)
    addr="$(hostname -I 2>/dev/null | awk '{print $1}')"; addr="${addr:-127.0.0.1}"
    echo "+;eth0;IPv4;Ignite\\032Server;_scribe._tcp;local"
    echo "=;eth0;IPv4;Ignite\\032Server;_scribe._tcp;local;$(hostname).local;$addr;8787;\"v=1\" \"sid=sid-local\" \"cp=/control/v1\""
    ;;
  remote-unreachable)
    # TEST-NET-1 (RFC 5737): never routable, so the HTTP probe must fail.
    echo "=;eth0;IPv4;Ignite\\032Server;_scribe._tcp;local;elsewhere.local;192.0.2.10;8787;\"v=1\" \"sid=sid-remote\""
    ;;
  timeout) sleep 30 ;;
  daemon) echo "Failed to create client object: Daemon not running" >&2; exit 1 ;;
  empty) : ;;
esac
EOF
chmod +x "$TMP/bin/avahi-browse"

run_client() { # run_client <fake-mode> → REPORT
  local ev="$TMP/client-$1"
  FAKE_AVAHI="$1" PATH="$TMP/bin:$PATH" IGNITE_LAN_DIAG=1 IGNITE_LAN_EVIDENCE="$ev" \
    IGNITE_LAN_BROWSE_TIMEOUT=2 bash "$DIR/diag-lan-client.sh" >/dev/null 2>&1 \
    || fail "client diag exited nonzero for $1"
  REPORT="$ev/report.txt"
}
field() { sed -n "s/^$1=//p" "$REPORT" | tail -1; }
never_kindle_pass() {
  [ "$(field KINDLE_DISCOVERY)" = "NOT_TESTED" ] && [ "$(field KINDLE_PAIRING_SYNC)" = "NOT_TESTED" ] \
    || fail "$1: Kindle result must stay NOT_TESTED"
}

run_client local
[ "$(field BROWSE)" = "PASS" ] || fail "local: browse should see the record"
[ "$(field SERVER_LOCAL_ADVERTISEMENT)" = "OBSERVED" ] || fail "local: should be server-local"
[ "$(field LAN_CLIENT_DISCOVERY)" = "NOT_TESTED" ] || fail "local: own advertisement counted as LAN-client discovery"
never_kindle_pass local
pass "own advertisement is server-local only, never independent LAN discovery"

run_client remote-unreachable
[ "$(field SERVER_LOCAL_ADVERTISEMENT)" = "NOT_OBSERVED" ] || fail "remote: misclassified as local"
[ "$(field HTTP)" = "FAIL" ] || fail "remote: unreachable HTTP should FAIL"
[ "$(field LAN_CLIENT_DISCOVERY)" = "FAIL" ] || fail "remote: unreachable advertiser must not PASS"
never_kindle_pass remote
pass "remote advertisement without HTTP reachability is FAIL"

run_client timeout
[ "$(field BROWSE)" = "TIMEOUT" ] || fail "timeout: got BROWSE=$(field BROWSE)"
grep -q '^browse=PASS' "$REPORT" && fail "timeout: conflicting browse=PASS line" || true
[ "$(field LAN_CLIENT_DISCOVERY)" = "FAIL" ] || fail "timeout: LAN_CLIENT_DISCOVERY should be FAIL"
pass "browse timeout reported as TIMEOUT (single, consistent status)"

run_client daemon
[ "$(field BROWSE)" = "BLOCKED" ] || fail "daemon: got BROWSE=$(field BROWSE)"
[ "$(field LAN_CLIENT_DISCOVERY)" = "BLOCKED" ] || fail "daemon: should be BLOCKED, not FAIL"
pass "avahi daemon failure reported as BLOCKED"

run_client empty
[ "$(field BROWSE)" = "NO_SIGHTING" ] || fail "empty: got BROWSE=$(field BROWSE)"
pass "completed browse with no advertiser reported as NO_SIGHTING"

# Server side: classify the same states; never claims LAN-client or Kindle results.
run_server() {
  local ev="$TMP/server-$1"
  FAKE_AVAHI="$1" PATH="$TMP/bin:$PATH" IGNITE_LAN_DIAG=1 IGNITE_LAN_EVIDENCE="$ev" \
    IGNITE_LAN_BASE_URL="http://127.0.0.1:9" bash "$DIR/diag-lan-server.sh" >/dev/null 2>&1 \
    || fail "server diag exited nonzero for $1"
  REPORT="$ev/report.txt"
}
for m in local daemon empty; do
  run_server "$m"
  [ "$(field LAN_CLIENT_DISCOVERY)" = "NOT_TESTED" ] || fail "server $m: claimed LAN-client result"
  never_kindle_pass "server $m"
done
run_server local;  [ "$(field SERVER_LOCAL_ADVERTISEMENT)" = "OBSERVED" ] || fail "server local: not OBSERVED"
run_server daemon; [ "$(field SERVER_LOCAL_ADVERTISEMENT)" = "BLOCKED" ] || fail "server daemon: not BLOCKED"
run_server empty;  [ "$(field SERVER_LOCAL_ADVERTISEMENT)" = "NO_SIGHTING" ] || fail "server empty: not NO_SIGHTING"
pass "server diag reports server-local advertisement only"

echo "PASS: LAN diagnostic status regressions"
