#!/usr/bin/env bash
# Live-systemd acceptance checks for Ignite Server on a disposable Ubuntu guest.
# Evidence from this script is LIVE systemd (PID 1), not staging --root seams.
#
#   guest-acceptance.sh <bundle-dir>
#
# Evidence is sanitized: setup codes and session cookies are never written to disk.
# Temporary upgrade fixtures and binary copies stay under /tmp (not the evidence tree).
set -euo pipefail

BUNDLE="$(cd "$1" && pwd)"
EVIDENCE="${IGNITE_ACCEPT_EVIDENCE:-/tmp/ignite-acceptance-evidence}"
SCRATCH="${IGNITE_ACCEPT_SCRATCH:-$(mktemp -d /tmp/ignite-accept-scratch.XXXXXX)}"
mkdir -p "$EVIDENCE"
fail() { echo "ACCEPT FAIL: $*" | tee -a "$EVIDENCE/summary.txt" >&2; exit 1; }
note() { echo "$*" | tee -a "$EVIDENCE/summary.txt"; }

# Prefer sudo when available (Multipass ubuntu user); otherwise require root.
as_root() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    fail "need root or sudo to run: $*"
  fi
}

# Redact setup codes / session cookies / Authorization from text before persisting.
sanitize_text() {
  awk '
    BEGIN { redact_next = 0 }
    /^Setup code/ {
      print
      redact_next = 1
      next
    }
    redact_next && $0 ~ /^[[:space:]]*[A-Za-z0-9]{4}-[A-Za-z0-9]{4}[[:space:]]*$/ {
      print "  [REDACTED]"
      redact_next = 0
      next
    }
    {
      redact_next = 0
      line = $0
      gsub(/[Ss]et-[Cc]ookie:[[:space:]]*scribe_session=[^;]+/, "Set-Cookie: scribe_session=[REDACTED]", line)
      gsub(/scribe_session=[^;[:space:]]+/, "scribe_session=[REDACTED]", line)
      gsub(/"setup_code"[[:space:]]*:[[:space:]]*"[^"]+"/, "\"setup_code\":\"[REDACTED]\"", line)
      gsub(/Bearer [A-Za-z0-9._~+\/=-]+/, "Bearer [REDACTED]", line)
      print line
    }
  '
}

write_sanitized() {
  local dest="$1"
  sanitize_text >"$dest"
}

note "=== Ignite Server live-systemd acceptance ==="
note "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
note "host: $(hostname)"
note "os: $(. /etc/os-release && echo "$PRETTY_NAME")"
note "pid1: $(ps -p 1 -o comm=)"
note "platform_class=${IGNITE_ACCEPT_PLATFORM_CLASS:-unspecified}"
note "platform_driver=${IGNITE_ACCEPT_PLATFORM_DRIVER:-unspecified}"
note "multipass_backend=${IGNITE_ACCEPT_MULTIPASS_BACKEND:-n/a}"
note "bundle: $BUNDLE"
note "evidence_policy=no_setup_codes_no_session_cookies_no_bundle_copies"

[ "$(ps -p 1 -o comm=)" = "systemd" ] || fail "PID 1 is not systemd (got $(ps -p 1 -o comm=))"
command -v systemctl >/dev/null || fail "systemctl missing"
command -v curl >/dev/null || fail "curl missing"

# Avoid touching the host firewall of the developer machine: guest-local only.
INSTALL_FLAGS=(--firewall=none --port=8787)

# ---------------------------------------------------------------------------
# 1) Fresh install
# ---------------------------------------------------------------------------
note "--- install ---"
set +e
INSTALL_OUT="$(cd "$BUNDLE" && as_root ./install.sh "${INSTALL_FLAGS[@]}" 2>"$SCRATCH/install.err")"
rc=$?
set -e
printf '%s\n' "$INSTALL_OUT" | write_sanitized "$EVIDENCE/install.out"
if [ -s "$SCRATCH/install.err" ]; then
  sanitize_text <"$SCRATCH/install.err" >"$EVIDENCE/install.err"
fi
[ "$rc" -eq 0 ] || fail "install.sh exited $rc (see install.err)"
echo "$INSTALL_OUT" | grep -q "Ignite Server installed." || fail "missing install banner"

# Sensitive values stay in process memory only.
SETUP_CODE="$(echo "$INSTALL_OUT" | sed -n '/Setup code/{n;s/^ *//;p}')"
SID="$(echo "$INSTALL_OUT" | sed -n 's/^ *Server ID: //p')"
[ -n "$SETUP_CODE" ] || fail "no setup code"
[ -n "$SID" ] || fail "no server id"
note "server_id=$SID"
note "setup_code=present_in_memory_not_persisted"

systemctl is-active scribe-server.service | tee "$EVIDENCE/service-active-1.txt" \
  | grep -qx active || fail "service not active after install"
systemctl show -p User -p MainPID -p FragmentPath scribe-server.service \
  | tee "$EVIDENCE/service-show-1.txt"
systemctl show -p User scribe-server.service | grep -qx 'User=scribe' \
  || fail "service User is not scribe"
MAINPID="$(systemctl show -p MainPID --value scribe-server.service)"
[ -n "$MAINPID" ] && [ "$MAINPID" != 0 ] || fail "no MainPID"
ps -o user= -p "$MAINPID" | tr -d ' ' | grep -qx scribe \
  || fail "MainPID not running as scribe (pid=$MAINPID)"

[ "$(stat -c %a /var/lib/scribe-server)" = 700 ] || fail "state dir mode"
[ "$(stat -c %a /var/lib/scribe-server/server.json)" = 600 ] || fail "server.json mode"
[ "$(stat -c %a /var/lib/scribe-server/control.sqlite)" = 600 ] || fail "control.sqlite mode"
grep -q 'listen = "0.0.0.0:8787"' /etc/scribe-server/config.toml || fail "listen binding"
grep -q '0.0.0.0' /etc/scribe-server/config.toml || fail "expected non-loopback bind"
note "state_perms=ok listen=0.0.0.0:8787"

# ---------------------------------------------------------------------------
# 2) Browser / PWA / claim / session
#     - Playwright Chromium smoke when available (real JS/WASM/auth)
#     - curl claim fallback only when IGNITE_ACCEPT_REQUIRE_BROWSER is unset
# ---------------------------------------------------------------------------
note "--- browser / claim ---"
for _ in $(seq 40); do
  curl -fsS "http://127.0.0.1:8787/control/v1/status" >/dev/null && break
  sleep 0.25
done
HTML="$(curl -fsS "http://127.0.0.1:8787/")"
echo "$HTML" | grep -qi '<html' || fail "ignite-web index not served"
note "ignite_web_html=ok"
MANIFEST="$(curl -fsS "http://127.0.0.1:8787/manifest.webmanifest")"
echo "$MANIFEST" | grep -q '"id": "/ignite-web"' || fail "PWA id is not /ignite-web"
# Persist only the PWA id line, not the full asset tree.
echo "$MANIFEST" | grep -E '"id"|"name"' | head -5 | tee "$EVIDENCE/manifest-id.txt" >/dev/null
STATUS="$(curl -fsS "http://127.0.0.1:8787/control/v1/status")"
echo "$STATUS" | grep -q "$SID" || fail "status missing server id"
echo "$STATUS" | write_sanitized "$EVIDENCE/status-1.json"
note "status_unclaimed=ok"

REQUIRE_BROWSER="${IGNITE_ACCEPT_REQUIRE_BROWSER:-0}"
SMOKE_DIR="${IGNITE_ACCEPT_BROWSER_SMOKE:-/home/ubuntu/browser-smoke}"
BROWSER_NOTE=""
BROWSER_CLASS=""
COOKIE=""

run_browser_smoke() {
  local smoke_js="$SMOKE_DIR/smoke.mjs"
  [ -f "$smoke_js" ] || return 2
  command -v node >/dev/null 2>&1 || return 3
  [ -d "$SMOKE_DIR/node_modules/playwright" ] || return 4
  # Browsers are installed as the ubuntu user; keep PLAYWRIGHT_BROWSERS_PATH aligned.
  export PLAYWRIGHT_BROWSERS_PATH="${PLAYWRIGHT_BROWSERS_PATH:-/home/ubuntu/.cache/ms-playwright}"
  mkdir -p "$EVIDENCE"
  # Evidence dir must be writable by the smoke user.
  if [ "$(id -u)" -eq 0 ] && id ubuntu >/dev/null 2>&1; then
    chown -R ubuntu:ubuntu "$EVIDENCE" 2>/dev/null || true
  fi
  echo "browser-smoke: starting Playwright Chromium smoke…"
  local -a run_cmd=(env
    "IGNITE_ACCEPT_SETUP_CODE=$SETUP_CODE"
    "IGNITE_ACCEPT_BASE_URL=http://127.0.0.1:8787"
    "IGNITE_ACCEPT_EVIDENCE=$EVIDENCE"
    "IGNITE_ACCEPT_FIXTURES=${IGNITE_ACCEPT_FIXTURES:-$SMOKE_DIR/fixtures}"
    "PLAYWRIGHT_BROWSERS_PATH=$PLAYWRIGHT_BROWSERS_PATH"
    node "$smoke_js"
  )
  set +e
  if [ "$(id -u)" -eq 0 ] && id ubuntu >/dev/null 2>&1; then
    sudo -u ubuntu -- "${run_cmd[@]}"
  else
    "${run_cmd[@]}"
  fi
  local rc=$?
  set -e
  return "$rc"
}

if [ "$REQUIRE_BROWSER" = "1" ]; then
  # Capture status before `if` — `if ! cmd; then rc=$?` is often 0 in bash.
  set +e
  run_browser_smoke
  smoke_rc=$?
  set -e
  if [ "$smoke_rc" -ne 0 ]; then
    case "$smoke_rc" in
      2) fail "IGNITE_ACCEPT_REQUIRE_BROWSER=1 but browser-smoke missing at $SMOKE_DIR" ;;
      3) fail "IGNITE_ACCEPT_REQUIRE_BROWSER=1 but node is not installed" ;;
      4) fail "IGNITE_ACCEPT_REQUIRE_BROWSER=1 but playwright deps missing (run install-browser-smoke.sh)" ;;
      *) fail "Playwright browser smoke failed (exit $smoke_rc); see browser-smoke.json — no HTTP-only fallback" ;;
    esac
  fi
  if [ ! -f "$EVIDENCE/browser-smoke.json" ]; then
    fail "browser smoke produced no browser-smoke.json"
  fi
  grep -q '"browser_class": "playwright-chromium"' "$EVIDENCE/browser-smoke.json" \
    || fail "browser smoke did not report browser_class=playwright-chromium"
  grep -q '"ok": true' "$EVIDENCE/browser-smoke.json" \
    || fail "browser-smoke.json ok!=true"
  # Server is claimed by the browser; confirm without persisting cookies.
  CLAIMED="$(curl -fsS "http://127.0.0.1:8787/control/v1/status")"
  echo "$CLAIMED" | grep -q "$SID" || fail "status missing server id after browser claim"
  echo "$CLAIMED" | grep -Eq '"claimed"[[:space:]]*:[[:space:]]*true' \
    || fail "server not claimed after browser smoke"
  echo "$CLAIMED" | write_sanitized "$EVIDENCE/status-after-browser-claim.json"
  BROWSER_CLASS="playwright-chromium"
  BROWSER_NOTE="playwright chromium: UI claim, HttpOnly session, authed API, reload, WASM init + canvas project"
  note "claim=ok session_verified=playwright_browser_context"
  note "authed_status=ok_via_playwright"
else
  # Optional browser: curl claim only when smoke was NOT attempted (missing deps).
  # If Playwright runs and fails after (or during) claim, do not curl-claim again.
  set +e
  run_browser_smoke
  smoke_rc=$?
  set -e
  if [ "$smoke_rc" -eq 0 ] \
     && grep -q '"browser_class": "playwright-chromium"' "$EVIDENCE/browser-smoke.json" 2>/dev/null \
     && grep -q '"ok": true' "$EVIDENCE/browser-smoke.json" 2>/dev/null; then
    CLAIMED="$(curl -fsS "http://127.0.0.1:8787/control/v1/status")"
    echo "$CLAIMED" | write_sanitized "$EVIDENCE/status-after-browser-claim.json"
    BROWSER_CLASS="playwright-chromium"
    BROWSER_NOTE="playwright chromium: UI claim, HttpOnly session, authed API, reload, WASM init + canvas project"
    note "claim=ok session_verified=playwright_browser_context"
    note "authed_status=ok_via_playwright"
  elif [ "$smoke_rc" -eq 2 ] || [ "$smoke_rc" -eq 3 ] || [ "$smoke_rc" -eq 4 ]; then
    note "browser_smoke_skipped=not_attempted rc=$smoke_rc (curl claim fallback)"
    CLAIM_HDRS="$(curl -sS -D - -o /dev/null \
      -H 'X-Scribe-Request: 1' -H 'Content-Type: application/json' \
      --data "{\"setup_code\":\"$SETUP_CODE\"}" \
      "http://127.0.0.1:8787/control/v1/claim")"
    echo "$CLAIM_HDRS" | grep -qi 'set-cookie: scribe_session=' || fail "claim did not set session cookie"
    COOKIE="$(echo "$CLAIM_HDRS" | tr -d '\r' | sed -n 's/[Ss]et-[Cc]ookie: \(scribe_session=[^;]*\).*/\1/p' | head -1)"
    [ -n "$COOKIE" ] || fail "could not parse session cookie"
    note "claim=ok session_cookie=present_in_memory_not_persisted"
    printf '%s\n' "$CLAIM_HDRS" | write_sanitized "$EVIDENCE/claim.headers.sanitized"
    AUTHED="$(curl -fsS -H "Cookie: $COOKIE" -H 'X-Scribe-Request: 1' \
      "http://127.0.0.1:8787/control/v1/status")"
    echo "$AUTHED" | grep -q "$SID" || fail "authed status failed"
    echo "$AUTHED" | write_sanitized "$EVIDENCE/status-authed.json"
    note "authed_status=ok"
    BROWSER_NOTE="curl HTTP client (claim + session + HTML/manifest); not a GUI browser"
    BROWSER_CLASS="http-client"
    if command -v chromium-browser >/dev/null 2>&1 || command -v chromium >/dev/null 2>&1 || command -v google-chrome >/dev/null 2>&1; then
      CHROME="$(command -v chromium-browser || command -v chromium || command -v google-chrome)"
      if "$CHROME" --headless --disable-gpu --dump-dom "http://127.0.0.1:8787/" \
          >"$SCRATCH/chrome-dom.html" 2>"$SCRATCH/chrome.err"; then
        if grep -qi '<html' "$SCRATCH/chrome-dom.html" 2>/dev/null; then
          BROWSER_NOTE="headless chromium DOM dump + curl claim/session"
          BROWSER_CLASS="headless-chromium"
          echo "chromium_dom=html_ok" >"$EVIDENCE/browser-note.txt"
        fi
      fi
    fi
  else
    fail "Playwright browser smoke was attempted and failed (exit $smoke_rc); refusing curl re-claim — see browser-smoke.json"
  fi
fi
note "browser_evidence=$BROWSER_NOTE"
note "browser_class=$BROWSER_CLASS"
[ "$BROWSER_CLASS" = "playwright-chromium" ] || [ "$REQUIRE_BROWSER" != "1" ] \
  || fail "REQUIRE_BROWSER set but browser_class is not playwright-chromium"

# Discovery (best-effort; multicast may be unavailable in some guests)
DISCOVERY_NOTE="not verified"
DISCOVERY_CLASS="unverified"
if command -v avahi-browse >/dev/null 2>&1; then
  if timeout 5 avahi-browse -atr 2>/dev/null | tee "$SCRATCH/avahi.txt" | grep -qi scribe; then
    DISCOVERY_NOTE="avahi saw _scribe service"
    DISCOVERY_CLASS="mdns-observed"
    grep -i scribe "$SCRATCH/avahi.txt" | head -20 >"$EVIDENCE/avahi-scribe.txt" || true
  else
    DISCOVERY_NOTE="avahi available but no _scribe sighting (multicast may be blocked)"
    DISCOVERY_CLASS="mdns-unavailable"
    echo "avahi_ran=yes scribe_sighting=no" >"$EVIDENCE/discovery-note.txt"
  fi
else
  IFACES="$(/usr/local/bin/scribe-server lan-interfaces 2>/dev/null || true)"
  if [ -n "$IFACES" ]; then
    DISCOVERY_NOTE="lan-interfaces non-empty; mDNS browse not installed"
    DISCOVERY_CLASS="interfaces-only"
    printf '%s\n' "$IFACES" >"$EVIDENCE/lan-interfaces.txt"
  else
    DISCOVERY_NOTE="no eligible LAN interfaces reported (typical in isolated bridge nets)"
    DISCOVERY_CLASS="no-lan-interfaces"
  fi
fi
note "discovery=$DISCOVERY_NOTE"
note "discovery_class=$DISCOVERY_CLASS"

as_root install -d -m 0700 -o scribe -g scribe /var/lib/scribe-server/ledger
echo "ledger-before-upgrade" | as_root tee /var/lib/scribe-server/ledger/accept-marker >/dev/null
as_root chown scribe:scribe /var/lib/scribe-server/ledger/accept-marker
# Hashes in scratch only — do not archive executable binaries into evidence.
BIN_HASH_BEFORE="$(sha256sum /usr/local/bin/scribe-server | awk '{print $1}')"
UNIT_HASH_BEFORE="$(sha256sum /etc/systemd/system/scribe-server.service | awk '{print $1}')"
printf 'bin_sha256=%s\nunit_sha256=%s\n' "$BIN_HASH_BEFORE" "$UNIT_HASH_BEFORE" \
  >"$EVIDENCE/component-hashes-before-upgrade.txt"
# Unit text is non-secret diagnostic (no cookies); strip nothing sensitive.
cp -a /etc/systemd/system/scribe-server.service "$EVIDENCE/unit.before.txt"

# ---------------------------------------------------------------------------
# 3) Successful upgrade (same bundle → upgrade path)
# ---------------------------------------------------------------------------
note "--- upgrade ---"
set +e
UP_OUT="$(cd "$BUNDLE" && as_root ./install.sh "${INSTALL_FLAGS[@]}" 2>"$SCRATCH/upgrade.err")"
rc=$?
set -e
printf '%s\n' "$UP_OUT" | write_sanitized "$EVIDENCE/upgrade.out"
[ "$rc" -eq 0 ] || fail "upgrade exited $rc"
echo "$UP_OUT" | grep -q "Ignite Server upgraded." || fail "missing upgrade banner"
echo "$UP_OUT" | grep -q "Setup code" && fail "upgrade printed setup code"
[ "$(echo "$UP_OUT" | sed -n 's/^ *Server ID: //p')" = "$SID" ] || fail "ServerId changed on upgrade"
systemctl is-active scribe-server.service | grep -qx active || fail "service inactive after upgrade"
[ -f /var/lib/scribe-server/ledger/accept-marker ] || fail "ledger marker lost"
/usr/local/bin/scribe-server status --config /etc/scribe-server/config.toml \
  | tee "$EVIDENCE/status-after-upgrade.txt" | grep -q 'claimed=true' || fail "claim lost"
curl -fsS -H "Cookie: $COOKIE" -H 'X-Scribe-Request: 1' \
  "http://127.0.0.1:8787/control/v1/status" | grep -q "$SID" || fail "session/status broken after upgrade"
note "upgrade preserved server_id + claim + ledger"

# ---------------------------------------------------------------------------
# 4) Failed upgrade → real systemd rollback (isolated copy under scratch)
# ---------------------------------------------------------------------------
note "--- failed-upgrade rollback (live systemd) ---"
BAD="$SCRATCH/bad-upgrade-bundle"
rm -rf "$BAD"
cp -a "$BUNDLE" "$BAD"
# Passes preflight; fails after promotion (broken unit).
sed -i 's|^ExecStart=.*|ExecStart=/bin/false|' "$BAD/scribe-server.service"
BIN_HASH_RB="$(sha256sum /usr/local/bin/scribe-server | awk '{print $1}')"
UNIT_HASH_RB="$(sha256sum /etc/systemd/system/scribe-server.service | awk '{print $1}')"
cp -a /etc/systemd/system/scribe-server.service "$EVIDENCE/unit.before-rollback.txt"
set +e
BAD_OUT="$(cd "$BAD" && as_root ./install.sh "${INSTALL_FLAGS[@]}" 2>"$SCRATCH/bad-upgrade.err")"
rc=$?
set -e
printf '%s\n' "$BAD_OUT" | write_sanitized "$EVIDENCE/bad-upgrade.out"
sanitize_text <"$SCRATCH/bad-upgrade.err" >"$EVIDENCE/bad-upgrade.err"
# Record only that the bad unit differed (do not archive the whole temp bundle).
echo "bad_unit_execstart=/bin/false" >"$EVIDENCE/bad-upgrade-fixture.txt"
[ "$rc" -ne 0 ] || fail "bad upgrade unexpectedly succeeded"
grep -qi 'install/upgrade failed' "$EVIDENCE/bad-upgrade.err" \
  || fail "bad upgrade missing failure message"
grep -qi 'restored:' "$EVIDENCE/bad-upgrade.err" || fail "bad upgrade did not report restore"
grep -qi 'recovery restart=' "$EVIDENCE/bad-upgrade.err" \
  || fail "bad upgrade missing recovery restart distinction"
[ "$(sha256sum /usr/local/bin/scribe-server | awk '{print $1}')" = "$BIN_HASH_RB" ] \
  || fail "binary not restored after failed upgrade"
[ "$(sha256sum /etc/systemd/system/scribe-server.service | awk '{print $1}')" = "$UNIT_HASH_RB" ] \
  || fail "unit not restored after failed upgrade"
systemctl is-active scribe-server.service | tee "$EVIDENCE/service-active-after-rollback.txt" \
  | grep -qx active || fail "service not active after rollback recovery"
[ "$(cat /var/lib/scribe-server/ledger/accept-marker)" = "ledger-before-upgrade" ] \
  || fail "ledger damaged by failed upgrade"
/usr/local/bin/scribe-server status --config /etc/scribe-server/config.toml \
  | grep -q 'claimed=true' || fail "claim lost after rollback"
curl -fsS "http://127.0.0.1:8787/control/v1/status" | grep -q "$SID" \
  || fail "HTTP status broken after rollback"
note "live systemd rollback restored previous unit/binary; service active; data preserved"
# Drop temp bad bundle (never part of evidence).
rm -rf "$BAD"

# ---------------------------------------------------------------------------
# 5) Uninstall (data-preserving) + reinstall
# ---------------------------------------------------------------------------
note "--- uninstall / reinstall ---"
as_root "$BUNDLE/uninstall.sh" 2>"$SCRATCH/uninstall.err" | write_sanitized "$EVIDENCE/uninstall.out"
systemctl is-active scribe-server.service >/dev/null 2>&1 && fail "service still active after uninstall"
[ ! -e /usr/local/bin/scribe-server ] || fail "binary remains after uninstall"
[ -f /var/lib/scribe-server/server.json ] || fail "uninstall deleted state"
[ -f /var/lib/scribe-server/ledger/accept-marker ] || fail "uninstall deleted ledger"

set +e
RE_OUT="$(cd "$BUNDLE" && as_root ./install.sh "${INSTALL_FLAGS[@]}" 2>"$SCRATCH/reinstall.err")"
rc=$?
set -e
printf '%s\n' "$RE_OUT" | write_sanitized "$EVIDENCE/reinstall.out"
[ "$rc" -eq 0 ] || fail "reinstall exited $rc"
[ "$(echo "$RE_OUT" | sed -n 's/^ *Server ID: //p')" = "$SID" ] || fail "ServerId changed on reinstall"
echo "$RE_OUT" | grep -q "Setup code" && fail "reinstall reopened claim"
systemctl is-active scribe-server.service | grep -qx active || fail "service inactive after reinstall"
curl -fsS "http://127.0.0.1:8787/manifest.webmanifest" | grep -q '"id": "/ignite-web"' \
  || fail "manifest broken after reinstall"

# Clear secrets from this shell before exit.
SETUP_CODE=""
COOKIE=""
rm -rf "$SCRATCH"

note "ACCEPT PASS server_id=$SID"
note "evidence_dir=$EVIDENCE"
echo "ACCEPT PASS server_id=$SID"
