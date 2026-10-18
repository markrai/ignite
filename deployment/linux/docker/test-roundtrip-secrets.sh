#!/usr/bin/env bash
# Secret-handling regressions for api-notebook-blob-roundtrip.sh against a local mock server.
# Asserts the bearer credential, pairing secret, and admin cookie never reach stdout/stderr,
# curl argv, or evidence — on success and on failures after the credential was issued.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$DIR/../../.." && pwd)"
RT="$DIR/api-notebook-blob-roundtrip.sh"
FIXTURES="$ROOT/ignite/web/tests/fixtures"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/ignite-rt-secrets.XXXXXX")"
SERVER_PID=""
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

command -v python3 >/dev/null 2>&1 || fail "python3 required"
[ -f "$FIXTURES/fixture-manifest.json" ] || fail "fixtures missing: $FIXTURES"

CRED="01a14f57-a6b2-79ed-9103-8fa99942ca20.Zm9vYmFyYmF6cXV4c2VjcmV0dmFsdWUwMTIzNDU2Nzg5"
REQ_SECRET="REQSECRET-9f8e7d6c5b4a39281706f5e4d3c2b1a0"
COOKIE_VAL="COOKIESECRET0123456789abcdef"

# Logs every curl argv, then runs the real curl.
REAL_CURL="$(command -v curl)"
mkdir -p "$TMP/bin"
cat >"$TMP/bin/curl" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$TMP/curl-argv.log"
exec "$REAL_CURL" "\$@"
EOF
chmod +x "$TMP/bin/curl"

cat >"$TMP/mock.py" <<'PY'
import hashlib, json, os, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

fixtures, port_file, cred, req_secret, cookie_val = sys.argv[1:6]
m = json.load(open(os.path.join(fixtures, "fixture-manifest.json")))
blob = open(os.path.join(fixtures, m["blob"]["file"]), "rb").read()
lib = m["library_id"]
fail_at = os.environ.get("FAIL_AT", "")

class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass
    def send(self, code, obj=None, raw=None):
        body = raw if raw is not None else json.dumps(obj or {}).encode()
        self.send_response(code)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""
    def authed(self):
        return self.headers.get("Authorization") == f"Bearer {cred}"
    def do_POST(self):
        b = self.body(); p = self.path
        if p == "/control/v1/pairing":
            if self.headers.get("Cookie") != f"scribe_session={cookie_val}":
                return self.send(401, {"error": "no admin cookie"})
            return self.send(201, {"request_id": "req1", "request_secret": req_secret,
                                   "comparison_code": "1234", "expires_in_s": 300})
        if p == "/control/v1/admin/pairing/req1/approve":
            return self.send(200, {"ok": True})
        if p == "/control/v1/pairing/req1/poll":
            if json.loads(b).get("request_secret") != req_secret:
                return self.send(403, {"error": "bad secret"})
            return self.send(200, {"state": "approved", "device_id": cred.split(".")[0],
                                   "credential": cred, "server_id": "sid"})
        if not self.authed():
            return self.send(401, {"error": "unauthorized"})
        if p == "/v1/libraries":
            return self.send(201, {"library_id": lib})
        if p == f"/v1/libraries/{lib}/operations":
            if fail_at == "ops":
                # Hostile error body that echoes the credential back.
                return self.send(500, raw=f"internal error for Bearer {cred}".encode())
            return self.send(200, {"accepted": 2})
        self.send(404, {})
    def do_PUT(self):
        b = self.body()
        if not self.authed():
            return self.send(401, {})
        if hashlib.sha256(b).hexdigest() != m["blob"]["sha256"]:
            return self.send(400, {})
        self.send(201, {})
    def do_GET(self):
        if not self.authed():
            return self.send(401, {})
        p = self.path.split("?")[0]
        if p == f"/v1/libraries/{lib}":
            return self.send(200, {"library_id": lib, "genesis_sha256": "g", "head_server_seq": 2,
                                   "snapshot_sha256": "x"})
        if p == f"/v1/libraries/{lib}/snapshot":
            return self.send(200, {"library_id": lib, "through_server_seq": 2,
                                   "snapshot_sha256": m["snapshot_sha256_after_live_ops"][0],
                                   "snapshot": {}})
        if p == f"/v1/libraries/{lib}/operations":
            return self.send(200, {"operations": [{}, {}], "head_server_seq": 2})
        if p == f"/v1/blobs/{m['blob']['sha256']}":
            if fail_at == "blob":
                return self.send(200, raw=b"corrupted")
            return self.send(200, raw=blob)
        self.send(404, {})

s = HTTPServer(("127.0.0.1", 0), H)
open(port_file, "w").write(str(s.server_address[1]))
s.serve_forever()
PY

start_server() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  rm -f "$TMP/port"
  FAIL_AT="$1" python3 "$TMP/mock.py" "$FIXTURES" "$TMP/port" "$CRED" "$REQ_SECRET" "$COOKIE_VAL" &
  SERVER_PID=$!
  for _ in $(seq 1 100); do [ -s "$TMP/port" ] && break; sleep 0.05; done
  [ -s "$TMP/port" ] || fail "mock server did not start"
  BASE_URL="http://127.0.0.1:$(cat "$TMP/port")"
}

new_dirs() {
  SCRATCH="$TMP/$1/scratch"; EVIDENCE="$TMP/$1/evidence"
  mkdir -p "$SCRATCH" "$EVIDENCE"
  chmod 700 "$SCRATCH"
  printf 'scribe_session=%s\n' "$COOKIE_VAL" >"$SCRATCH/session.cookie"
  chmod 600 "$SCRATCH/session.cookie"
  : >"$TMP/curl-argv.log"
}

run_rt() { # run_rt <mode> → OUTFILE, RC
  OUTFILE="$TMP/out-$RANDOM.txt"
  set +e
  PATH="$TMP/bin:$PATH" BASE_URL="$BASE_URL" FIXTURES_DIR="$FIXTURES" EVIDENCE="$EVIDENCE" \
    SCRATCH="$SCRATCH" ADMIN_COOKIE_FILE="$SCRATCH/session.cookie" MODE="$1" \
    bash "$RT" >"$OUTFILE" 2>&1
  RC=$?
  set -e
}

assert_no_secrets() { # assert_no_secrets <label>
  local f
  for f in "$OUTFILE" "$TMP/curl-argv.log"; do
    if grep -qF -e "$CRED" -e "${CRED#*.}" -e "$REQ_SECRET" -e "$COOKIE_VAL" "$f"; then
      fail "$1: secret leaked into $(basename "$f")"
    fi
  done
  if grep -rqF -e "$CRED" -e "${CRED#*.}" -e "$REQ_SECRET" -e "$COOKIE_VAL" "$EVIDENCE"; then
    fail "$1: secret leaked into evidence"
  fi
  bash "$DIR/assert-evidence-clean.sh" "$EVIDENCE" >/dev/null || fail "$1: evidence scanner rejected evidence"
}

# --- 1) Seed success: bearer only in the 0600 scratch file ---
start_server ""
new_dirs seed
run_rt seed
[ "$RC" -eq 0 ] || fail "seed against mock failed: $(cat "$OUTFILE")"
assert_no_secrets "seed"
grep -q '^LIBRARY_ID=' "$OUTFILE" || fail "seed did not print LIBRARY_ID"
grep -q 'BEARER' "$OUTFILE" && fail "seed stdout mentions BEARER" || true
[ "$(tr -d '\r\n' <"$SCRATCH/bearer.token")" = "$CRED" ] || fail "bearer file missing or wrong"
perm="$(stat -c '%a' "$SCRATCH/bearer.token" 2>/dev/null || echo 600)"
[ "$perm" = "600" ] || fail "bearer file mode $perm (want 600)"
[ ! -e "$SCRATCH/auth.header" ] && [ ! -e "$SCRATCH/cookie.header" ] || fail "header scratch files left behind"
grep -q '"credential":"<redacted>"' "$EVIDENCE/pair-poll.initial.body.txt" || fail "pair-poll evidence not redacted"
pass "seed: bearer never in stdout/stderr, curl argv, or evidence; stored 0600 in scratch"

# --- 2) Verify reads the bearer from file only ---
run_rt verify
[ "$RC" -eq 0 ] || fail "verify against mock failed: $(cat "$OUTFILE")"
assert_no_secrets "verify"
pass "verify: bearer read from file, never exposed"

# --- 3) Failure after credential issue, with a hostile body echoing the credential ---
start_server ops
new_dirs fail-ops
run_rt seed
[ "$RC" -ne 0 ] || fail "ops failure reported success"
grep -q 'operations post HTTP 500' "$OUTFILE" || fail "ops failure not reported: $(cat "$OUTFILE")"
assert_no_secrets "ops-failure"
pass "failure after pairing (hostile echo): no secret in output or evidence"

# --- 4) Integrity failure (corrupt blob) fails closed, without secrets ---
start_server blob
new_dirs fail-blob
run_rt seed
[ "$RC" -ne 0 ] || fail "corrupt blob reported success"
grep -q 'blob payload sha mismatch' "$OUTFILE" || fail "blob integrity failure not reported"
assert_no_secrets "blob-failure"
pass "blob integrity mismatch fails closed, no secrets exposed"

# --- 5) Scanner catches a raw credential anywhere ---
mkdir -p "$TMP/raw-leak"
echo "token was $CRED" >"$TMP/raw-leak/x.txt"
set +e
SCAN_OUT="$(bash "$DIR/assert-evidence-clean.sh" "$TMP/raw-leak" 2>&1)"
src=$?
set -e
[ "$src" -ne 0 ] || fail "scanner missed a raw credential"
echo "$SCAN_OUT" | grep -qF "${CRED#*.}" && fail "scanner echoed the credential" || true
mkdir -p "$TMP/scratch-leak"
: >"$TMP/scratch-leak/bearer.token"
bash "$DIR/assert-evidence-clean.sh" "$TMP/scratch-leak" >/dev/null 2>&1 && fail "scanner allowed bearer.token in evidence" || true
pass "scanner rejects raw credentials and scratch credential files"

echo "PASS: round-trip secret-handling regressions"
