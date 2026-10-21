#!/usr/bin/env bash
# Staging validation of the Linux installer (--root mode).
#   deployment/linux/test-staging.sh <bundle-dir>
set -euo pipefail
BUNDLE="$(cd "$1" && pwd)"
BASE="$(mktemp -d)"
ROOT="$BASE/stage"
SERVER_PID=""
fail() { echo "STAGING FAIL: $*" >&2; exit 1; }
cleanup() {
  if [ -n "${SERVER_PID:-}" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  # Sweep any leftover servers we started under BASE.
  if [ -d "$BASE" ]; then
    while read -r pid; do
      [ -n "$pid" ] || continue
      kill "$pid" 2>/dev/null || true
    done < <(pgrep -f "$BASE" 2>/dev/null || true)
  fi
  rm -rf "$BASE"
}
trap cleanup EXIT

free_port() {
  # Bind to an ephemeral port and print it.
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

PORT="$(free_port)"
PORT_UP="$(free_port)"

# --- happy path ----------------------------------------------------------------------------
out1="$("$BUNDLE/install.sh" --root="$ROOT" --port="$PORT")"
echo "$out1" | grep -q "Ignite Server installed." || fail "first install banner"
[ -f "$ROOT/.ignite-staging-root" ] || fail "staging marker missing"
code="$(echo "$out1" | sed -n '/Setup code/{n;s/^ *//;p}')"
[ -n "$code" ] || fail "no setup code on first install"
sid1="$(echo "$out1" | sed -n 's/^ *Server ID: //p')"
[ -n "$sid1" ] || fail "no server id"
cfg="$ROOT/etc/scribe-server/config.toml"
grep -q "listen = \"0.0.0.0:$PORT\"" "$cfg" || fail "config port"
[ "$(stat -c %a "$ROOT/var/lib/scribe-server")" = 700 ] || fail "state dir must be 0700"
[ "$(stat -c %a "$ROOT/var/lib/scribe-server/server.json")" = 600 ] || fail "server.json must be 0600"
[ "$(stat -c %a "$ROOT/var/lib/scribe-server/control.sqlite")" = 600 ] || fail "control.sqlite must be 0600"
grep -q "$code" -r "$ROOT/etc" "$ROOT/usr" 2>/dev/null && fail "setup code persisted in plaintext"
grep -q "$code" -r "$ROOT/var/lib/scribe-server" 2>/dev/null && fail "setup code persisted in plaintext (state)"
grep -q '"id": "/ignite-web"' "$ROOT/usr/share/scribe-server/web/manifest.webmanifest" \
  || fail "installed manifest id is not /ignite-web"

"$ROOT/usr/local/bin/scribe-server" serve --config "$cfg" 2>"$ROOT/server.log" &
SERVER_PID=$!
for _ in $(seq 50); do curl -fs "http://127.0.0.1:$PORT/control/v1/status" >/dev/null && break; sleep 0.2; done
curl -fs "http://127.0.0.1:$PORT/" | grep -qi "<html" || fail "ignite-web not served"
curl -fs "http://127.0.0.1:$PORT/manifest.webmanifest" | grep -q '"id": "/ignite-web"' \
  || fail "served manifest id is not /ignite-web"
status="$(curl -fs "http://127.0.0.1:$PORT/control/v1/status")"
echo "$status" | grep -q "$sid1" || fail "status missing server id"
claim="$(curl -s -D - -o /dev/null -H 'X-Scribe-Request: 1' -H 'Content-Type: application/json' \
  --data "{\"setup_code\":\"$code\"}" "http://127.0.0.1:$PORT/control/v1/claim")"
echo "$claim" | grep -qi "set-cookie: scribe_session=" || fail "claim did not set a session"
grep -q "$code" "$ROOT/server.log" && fail "setup code appeared in the service log"
kill "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
mkdir -p "$ROOT/var/lib/scribe-server/ledger"
touch "$ROOT/var/lib/scribe-server/ledger/marker-from-before-upgrade"

# Upgrade: same ServerId, still claimed, no new code, data preserved.
out2="$("$BUNDLE/install.sh" --root="$ROOT" --port="$PORT")"
echo "$out2" | grep -q "Ignite Server upgraded." || fail "upgrade banner"
echo "$out2" | grep -q "Setup code" && fail "upgrade printed a setup code for a claimed server"
[ "$(echo "$out2" | sed -n 's/^ *Server ID: //p')" = "$sid1" ] || fail "ServerId changed on upgrade"
[ -f "$ROOT/var/lib/scribe-server/ledger/marker-from-before-upgrade" ] || fail "ledger lost on upgrade"
"$ROOT/usr/local/bin/scribe-server" status --config "$cfg" | grep -q "claimed=true" || fail "claim lost on upgrade"

# Uninstall keeps data; reinstall resumes it.
"$BUNDLE/uninstall.sh" --root="$ROOT" >/dev/null
[ ! -e "$ROOT/usr/local/bin/scribe-server" ] || fail "binary not removed"
[ -f "$ROOT/var/lib/scribe-server/server.json" ] || fail "uninstall deleted data"
out3="$("$BUNDLE/install.sh" --root="$ROOT" --port="$PORT")"
[ "$(echo "$out3" | sed -n 's/^ *Server ID: //p')" = "$sid1" ] || fail "ServerId changed after reinstall"
echo "$out3" | grep -q "Setup code" && fail "reinstall reopened the claim window"

# --- failure injection ---------------------------------------------------------------------
BAD="$BASE/bad-bundle"
cp -a "$BUNDLE" "$BAD"

printf '#!/bin/sh\nexit 1\n' >"$BAD/scribe-server"
chmod +x "$BAD/scribe-server"
ROOT_BAD="$BASE/stage-bad-bin"
if "$BAD/install.sh" --root="$ROOT_BAD" --port="$(free_port)" >"$BASE/bad-bin.out" 2>"$BASE/bad-bin.err"; then
  fail "bad binary install reported success"
fi
[ ! -f "$ROOT_BAD/var/lib/scribe-server/server.json" ] || fail "bad binary created state"

BAD2="$BASE/bad-web"
cp -a "$BUNDLE" "$BAD2"
rm -f "$BAD2/web/index.html"
if "$BAD2/install.sh" --root="$BASE/stage-bad-web" --port="$(free_port)" \
    >"$BASE/bad-web.out" 2>"$BASE/bad-web.err"; then
  fail "incomplete web install reported success"
fi

BAD3="$BASE/bad-link"
cp -a "$BUNDLE" "$BAD3"
ln -s /etc/passwd "$BAD3/web/evil-link" 2>/dev/null || ln -s index.html "$BAD3/web/evil-link"
ROOT_UP="$BASE/stage-upgrade-fail"
"$BUNDLE/install.sh" --root="$ROOT_UP" --port="$PORT_UP" >/dev/null
if "$BAD3/install.sh" --root="$ROOT_UP" --port="$PORT_UP" >"$BASE/bad-link.out" 2>"$BASE/bad-link.err"; then
  fail "symlink web upgrade reported success"
fi
[ -f "$ROOT_UP/usr/share/scribe-server/web/manifest.webmanifest" ] \
  || fail "failed upgrade removed web assets"
grep -q '"id": "/ignite-web"' "$ROOT_UP/usr/share/scribe-server/web/manifest.webmanifest" \
  || fail "failed upgrade corrupted manifest"
[ -f "$ROOT_UP/var/lib/scribe-server/server.json" ] || fail "failed upgrade destroyed state"

BAD4="$BASE/no-bin"
cp -a "$BUNDLE" "$BAD4"
rm -f "$BAD4/scribe-server"
if "$BAD4/install.sh" --root="$BASE/stage-no-bin" --port="$(free_port)" \
    >"$BASE/no-bin.out" 2>"$BASE/no-bin.err"; then
  fail "missing binary install reported success"
fi

# --- rollback seams (post-promotion failure; not merely refused malformed bundles) ---------
ROOT_RB="$BASE/stage-rollback"
PORT_RB="$(free_port)"
"$BUNDLE/install.sh" --root="$ROOT_RB" --port="$PORT_RB" >/dev/null
sid_rb="$("$ROOT_RB/usr/local/bin/scribe-server" status --config "$ROOT_RB/etc/scribe-server/config.toml" | sed -n 's/^server_id=//p')"
[ -n "$sid_rb" ] || fail "rollback fixture missing server id"
echo "UNIT_MARK_V1" | tee -a "$ROOT_RB/etc/systemd/system/scribe-server.service" >/dev/null
echo "WEB_MARK_V1" >"$ROOT_RB/usr/share/scribe-server/web/ROLLBACK_MARK"
cp -a "$ROOT_RB/usr/local/bin/scribe-server" "$BASE/bin.before"
cp -a "$ROOT_RB/etc/systemd/system/scribe-server.service" "$BASE/unit.before"
cp -a "$ROOT_RB/usr/share/scribe-server/web/ROLLBACK_MARK" "$BASE/web.before"

# Prepare an upgrade bundle that would replace unit/web/binary contents.
UP_BUNDLE="$BASE/upgrade-bundle"
cp -a "$BUNDLE" "$UP_BUNDLE"
echo "UNIT_MARK_V2_SHOULD_NOT_STICK" >>"$UP_BUNDLE/scribe-server.service"
echo "WEB_MARK_V2" >"$UP_BUNDLE/web/ROLLBACK_MARK"

if "$UP_BUNDLE/install.sh" --root="$ROOT_RB" --port="$PORT_RB" --test-seam=fail-restart \
    >"$BASE/seam-restart.out" 2>"$BASE/seam-restart.err"; then
  fail "fail-restart seam reported success"
fi
grep -qi "install/upgrade failed" "$BASE/seam-restart.err" || fail "fail-restart missing failure message"
grep -qi "restored:" "$BASE/seam-restart.err" || fail "fail-restart did not claim restore"
cmp -s "$BASE/bin.before" "$ROOT_RB/usr/local/bin/scribe-server" || fail "binary not restored after fail-restart"
cmp -s "$BASE/unit.before" "$ROOT_RB/etc/systemd/system/scribe-server.service" || fail "unit not restored after fail-restart"
cmp -s "$BASE/web.before" "$ROOT_RB/usr/share/scribe-server/web/ROLLBACK_MARK" || fail "web mark not restored after fail-restart"
[ -f "$ROOT_RB/var/lib/scribe-server/server.json" ] || fail "state lost after fail-restart"
sid_after="$("$ROOT_RB/usr/local/bin/scribe-server" status --config "$ROOT_RB/etc/scribe-server/config.toml" | sed -n 's/^server_id=//p')"
[ "$sid_after" = "$sid_rb" ] || fail "server id changed after fail-restart rollback"

# Refresh "before" snapshots then exercise health-fail seam.
cp -a "$ROOT_RB/usr/local/bin/scribe-server" "$BASE/bin.before"
cp -a "$ROOT_RB/etc/systemd/system/scribe-server.service" "$BASE/unit.before"
cp -a "$ROOT_RB/usr/share/scribe-server/web/ROLLBACK_MARK" "$BASE/web.before"
if "$UP_BUNDLE/install.sh" --root="$ROOT_RB" --port="$PORT_RB" --test-seam=fail-health \
    >"$BASE/seam-health.out" 2>"$BASE/seam-health.err"; then
  fail "fail-health seam reported success"
fi
grep -qi "install/upgrade failed" "$BASE/seam-health.err" || fail "fail-health missing failure message"
cmp -s "$BASE/unit.before" "$ROOT_RB/etc/systemd/system/scribe-server.service" || fail "unit not restored after fail-health"
cmp -s "$BASE/web.before" "$ROOT_RB/usr/share/scribe-server/web/ROLLBACK_MARK" || fail "web not restored after fail-health"
sid_after2="$("$ROOT_RB/usr/local/bin/scribe-server" status --config "$ROOT_RB/etc/scribe-server/config.toml" | sed -n 's/^server_id=//p')"
[ "$sid_after2" = "$sid_rb" ] || fail "server id changed after fail-health rollback"

# daemon-reload / enable seams (staging only; host systemd is never invoked).
for seam in fail-daemon-reload fail-enable; do
  cp -a "$ROOT_RB/usr/local/bin/scribe-server" "$BASE/bin.before"
  cp -a "$ROOT_RB/etc/systemd/system/scribe-server.service" "$BASE/unit.before"
  cp -a "$ROOT_RB/usr/share/scribe-server/web/ROLLBACK_MARK" "$BASE/web.before"
  if "$UP_BUNDLE/install.sh" --root="$ROOT_RB" --port="$PORT_RB" --test-seam="$seam" \
      >"$BASE/seam-$seam.out" 2>"$BASE/seam-$seam.err"; then
    fail "$seam seam reported success"
  fi
  grep -qi "install/upgrade failed" "$BASE/seam-$seam.err" || fail "$seam missing failure message"
  grep -qi "restored:" "$BASE/seam-$seam.err" || fail "$seam did not claim restore"
  grep -Eqi "daemon-reload|enable" "$BASE/seam-$seam.err" || fail "$seam reason unclear"
  cmp -s "$BASE/bin.before" "$ROOT_RB/usr/local/bin/scribe-server" || fail "binary not restored after $seam"
  cmp -s "$BASE/unit.before" "$ROOT_RB/etc/systemd/system/scribe-server.service" || fail "unit not restored after $seam"
  cmp -s "$BASE/web.before" "$ROOT_RB/usr/share/scribe-server/web/ROLLBACK_MARK" || fail "web not restored after $seam"
  [ -f "$ROOT_RB/var/lib/scribe-server/server.json" ] || fail "state lost after $seam"
  sid_seam="$("$ROOT_RB/usr/local/bin/scribe-server" status --config "$ROOT_RB/etc/scribe-server/config.toml" | sed -n 's/^server_id=//p')"
  [ "$sid_seam" = "$sid_rb" ] || fail "server id changed after $seam rollback"
done

# Updater promotion failure (binary, web and unit already promoted): everything rolls back,
# the updater included.
echo "# updater-v1" >>"$ROOT_RB/usr/local/bin/ignite-update"
echo "# updater-v2" >>"$UP_BUNDLE/ignite-update"
cp -a "$ROOT_RB/usr/local/bin/scribe-server" "$BASE/bin.before"
cp -a "$ROOT_RB/etc/systemd/system/scribe-server.service" "$BASE/unit.before"
cp -a "$ROOT_RB/usr/share/scribe-server/web/ROLLBACK_MARK" "$BASE/web.before"
cp -a "$ROOT_RB/usr/local/bin/ignite-update" "$BASE/updater.before"
if "$UP_BUNDLE/install.sh" --root="$ROOT_RB" --port="$PORT_RB" --test-seam=fail-updater \
    >"$BASE/seam-updater.out" 2>"$BASE/seam-updater.err"; then
  fail "fail-updater seam reported success"
fi
grep -q "updater promote" "$BASE/seam-updater.err" || fail "fail-updater reason unclear"
grep -q "restored: binary,web,unit,updater" "$BASE/seam-updater.err" || fail "fail-updater restore report"
cmp -s "$BASE/bin.before" "$ROOT_RB/usr/local/bin/scribe-server" || fail "binary not restored after fail-updater"
cmp -s "$BASE/unit.before" "$ROOT_RB/etc/systemd/system/scribe-server.service" || fail "unit not restored after fail-updater"
cmp -s "$BASE/web.before" "$ROOT_RB/usr/share/scribe-server/web/ROLLBACK_MARK" || fail "web not restored after fail-updater"
cmp -s "$BASE/updater.before" "$ROOT_RB/usr/local/bin/ignite-update" || fail "updater not restored after fail-updater"
for left in usr/local/bin/ignite-update.new usr/local/bin/ignite-update.old usr/local/bin/scribe-server.old; do
  [ ! -e "$ROOT_RB/$left" ] || fail "fail-updater left $left"
done
# The same bundle then installs completely, updater included.
"$UP_BUNDLE/install.sh" --root="$ROOT_RB" --port="$PORT_RB" >/dev/null || fail "install after fail-updater"
cmp -s "$UP_BUNDLE/ignite-update" "$ROOT_RB/usr/local/bin/ignite-update" || fail "updater not replaced"
[ ! -e "$ROOT_RB/usr/local/bin/ignite-update.old" ] || fail "updater rollback copy left after success"

# Purge deletes everything (isolated marked root only).
"$BUNDLE/uninstall.sh" --root="$ROOT" --purge >/dev/null
[ ! -e "$ROOT/var/lib/scribe-server" ] && [ ! -e "$ROOT/etc/scribe-server" ] || fail "purge left data"

# Private 0700 parent of the source bundle: root must still preflight via accessible copy.
# (Exercises the nobody-reachable temp staging path; does not touch host systemd.)
if [ "$(id -u)" -eq 0 ]; then
  PRIV_PARENT="$BASE/private-home"
  PRIV_BUNDLE="$PRIV_PARENT/ignite-bundle"
  mkdir -p "$PRIV_PARENT"
  cp -a "$BUNDLE" "$PRIV_BUNDLE"
  chmod 0700 "$PRIV_PARENT"
  # Confirm nobody cannot traverse the parent (the bug condition).
  if runuser -u nobody -- test -x "$PRIV_BUNDLE/scribe-server" 2>/dev/null; then
    fail "0700 parent unexpectedly traversable by nobody (test setup invalid)"
  fi
  ROOT_PRIV="$BASE/stage-private-src"
  out_priv="$("$PRIV_BUNDLE/install.sh" --root="$ROOT_PRIV" --port="$(free_port)")"
  echo "$out_priv" | grep -q "Ignite Server installed." || fail "0700-parent bundle install failed"
  [ -f "$ROOT_PRIV/var/lib/scribe-server/server.json" ] || fail "0700-parent install missing state"
  # Failed preflight must not write marker or install layout.
  BAD_PRIV="$BASE/private-home-bad"
  BAD_BUNDLE="$BAD_PRIV/ignite-bundle"
  mkdir -p "$BAD_PRIV"
  cp -a "$BUNDLE" "$BAD_BUNDLE"
  printf '#!/bin/sh\nexit 1\n' >"$BAD_BUNDLE/scribe-server"
  chmod 0755 "$BAD_BUNDLE/scribe-server"
  chmod 0700 "$BAD_PRIV"
  ROOT_BAD_PRIV="$BASE/stage-private-bad"
  if "$BAD_BUNDLE/install.sh" --root="$ROOT_BAD_PRIV" --port="$(free_port)" \
      >"$BASE/priv-bad.out" 2>"$BASE/priv-bad.err"; then
    fail "bad binary under 0700 parent reported success"
  fi
  [ ! -f "$ROOT_BAD_PRIV/.ignite-staging-root" ] || fail "failed preflight wrote staging marker"
  [ ! -e "$ROOT_BAD_PRIV/usr" ] || fail "failed preflight created usr layout"
  [ ! -e "$ROOT_BAD_PRIV/var" ] || fail "failed preflight created var layout"
else
  echo "STAGING NOTE: skipped 0700-parent preflight (not root)" >&2
fi

echo "STAGING OK server_id=$sid1"
