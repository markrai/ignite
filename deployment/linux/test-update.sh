#!/usr/bin/env bash
# Staging tests of ignite-update (update.sh) against a mock GitHub Releases server.
#   deployment/linux/test-update.sh <bundle-dir>
#
# Everything runs in private --root staging trees and a loopback HTTP server; no host systemd,
# firewall, network or installed service is touched. Older/newer releases are built from the
# given bundle with a version-reporting wrapper around its real binary.
set -euo pipefail
BUNDLE="$(cd "$1" && pwd)"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UPDATE="$HERE/update.sh"
BASE="$(mktemp -d)"
chmod 0755 "$BASE"   # as root, install.sh runs the (wrapped) candidate binary as nobody
SRV_PID=""
SERVER_PID=""
fail() { echo "UPDATE FAIL: $*" >&2; exit 1; }
cleanup() {
  for p in "$SERVER_PID" "$SRV_PID"; do
    [ -n "$p" ] && kill "$p" 2>/dev/null && wait "$p" 2>/dev/null || true
  done
  rm -rf "$BASE"
}
trap cleanup EXIT

free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'
}
ARCH="$(uname -m)"
REAL="$("$BUNDLE/scribe-server" --version | tr -d '[:space:]')"
cp -a "$BUNDLE/scribe-server" "$BASE/real-scribe-server"

# A bundle like $BUNDLE whose binary reports version $1 (and otherwise is the real server).
make_bundle() { # version out-dir
  mkdir -p "$BASE/wrap-$1"
  cat >"$BASE/wrap-$1/scribe-server" <<SH
#!/bin/sh
case "\$1" in --version|-V|version) echo $1; exit 0 ;; esac
exec "$BASE/real-scribe-server" "\$@"
SH
  chmod 0755 "$BASE/wrap-$1/scribe-server"
  bash "$HERE/make-bundle.sh" "$BASE/wrap-$1/scribe-server" "$BUNDLE/web" "$2" >/dev/null
}
asset() { echo "ignite-server-$1-linux-$ARCH.tar.gz"; }
WWW="$BASE/www"
mkdir -p "$WWW"
publish() { # dl-dir version bundle-dir
  mkdir -p "$WWW/$1/v$2"
  bash "$HERE/package-release.sh" "$3" "$WWW/$1/v$2" >/dev/null
}
release_json() { # scenario tag prerelease asset-names...
  local scen="$1" tag="$2" pre="$3" list="" a
  shift 3
  for a in "$@"; do list="$list${list:+,}{\"name\":\"$a\",\"size\":1}"; done
  mkdir -p "$WWW/api/$scen/releases"
  printf '{"tag_name":"%s","name":"Ignite %s","draft":false,"prerelease":%s,"assets":[%s]}' \
    "$tag" "$tag" "$pre" "$list" >"$WWW/api/$scen/releases/latest"
}

# --- fixtures ------------------------------------------------------------------------------
make_bundle 0.0.1 "$BASE/b001"
rm -f "$BASE/b001/ignite-update"                       # an installation from before ignite-update
make_bundle 0.0.2 "$BASE/b002"
echo "# fixture-updater-0.0.2" >>"$BASE/b002/ignite-update"
publish dl 0.0.2 "$BASE/b002"
release_json ok v0.0.2 false "$(asset 0.0.2)" "$(asset 0.0.2).sha256"
publish dl "$REAL" "$BUNDLE"
release_json real "v$REAL" false "$(asset "$REAL")" "$(asset "$REAL").sha256"
release_json pre v0.0.3-rc1 true "$(asset 0.0.3-rc1)"
release_json preflag v0.0.3 true "$(asset 0.0.3)"
release_json noasset v0.0.2 false
mkdir -p "$WWW/dl-empty"
mkdir -p "$WWW/dl-badsum/v0.0.2"
cp "$WWW/dl/v0.0.2/$(asset 0.0.2)" "$WWW/dl-badsum/v0.0.2/"
echo "$(printf '0%.0s' $(seq 64))  $(asset 0.0.2)" >"$WWW/dl-badsum/v0.0.2/$(asset 0.0.2).sha256"
# Version mismatch: the v0.0.3 asset holds the 0.0.2 bundle.
mkdir -p "$BASE/mm/ignite-server-0.0.3-linux-$ARCH" "$WWW/dl/v0.0.3"
cp -R "$BASE/b002/." "$BASE/mm/ignite-server-0.0.3-linux-$ARCH/"
tar -C "$BASE/mm" -czf "$WWW/dl/v0.0.3/$(asset 0.0.3)" "ignite-server-0.0.3-linux-$ARCH"
(cd "$WWW/dl/v0.0.3" && sha256sum "$(asset 0.0.3)" >"$(asset 0.0.3).sha256")
release_json mismatch v0.0.3 false "$(asset 0.0.3)"
# Unsafe archives (valid checksums): a ../ entry, and a symlink.
for kind in dotdot link; do
  mkdir -p "$WWW/dl-$kind/v0.0.2"
  python3 - "$BASE/b002" "$WWW/dl-$kind/v0.0.2/$(asset 0.0.2)" "ignite-server-0.0.2-linux-$ARCH" "$kind" <<'PY'
import io, sys, tarfile
src, out, top, kind = sys.argv[1:]
with tarfile.open(out, "w:gz") as t:
    t.add(src, arcname=top)
    if kind == "dotdot":
        data = b"escaped\n"
        info = tarfile.TarInfo(top + "/../../ignite-update-escape")
        info.size = len(data)
        t.addfile(info, io.BytesIO(data))
    else:
        info = tarfile.TarInfo(top + "/web/evil")
        info.type = tarfile.SYMTYPE
        info.linkname = "/etc/passwd"
        t.addfile(info)
PY
  (cd "$WWW/dl-$kind/v0.0.2" && sha256sum "$(asset 0.0.2)" >"$(asset 0.0.2).sha256")
done
# Server-ID mismatch: a 0.0.4 release whose installer changes the identity after installing.
make_bundle 0.0.4 "$BASE/b004"
mv "$BASE/b004/install.sh" "$BASE/b004/install-real.sh"
cat >"$BASE/b004/install.sh" <<'SH'
#!/usr/bin/env bash
set -e
bash "$(cd "$(dirname "$0")" && pwd)/install-real.sh" "$@"
for a in "$@"; do case "$a" in --root=*) r="${a#--root=}" ;; esac; done
sed -i 's/"server_id"[[:space:]]*:[[:space:]]*"[^"]*"/"server_id": "01900000-0000-7000-8000-000000000099"/' \
  "$r/var/lib/scribe-server/server.json"
SH
chmod 0755 "$BASE/b004/install.sh"
publish dl 0.0.4 "$BASE/b004"
release_json idswap v0.0.4 false "$(asset 0.0.4)"

# A stand-in systemctl for the service steps: records each call; the service is "active".
cat >"$BASE/fake-systemctl" <<SH
#!/bin/sh
echo "\$*" >>"\$FAKE_SYSTEMCTL_LOG"
exit 0
SH
chmod 0755 "$BASE/fake-systemctl"
export FAKE_SYSTEMCTL_LOG="$BASE/systemctl.log"

SRV_PORT="$(free_port)"
python3 -m http.server "$SRV_PORT" --bind 127.0.0.1 --directory "$WWW" >"$BASE/http.log" 2>&1 &
SRV_PID=$!
SRV="http://127.0.0.1:$SRV_PORT"
for _ in $(seq 50); do curl -fs "$SRV/api/ok/releases/latest" >/dev/null && break; sleep 0.1; done

R=""
upd() { # name [VAR=value ...] [-- updater-args]; sets RC
  local name="$1" envs=() args=(--yes)
  shift
  while [ "$#" -gt 0 ]; do
    case "$1" in --) shift; args=("$@"); break ;; *) envs+=("$1"); shift ;; esac
  done
  set +e
  # setsid: no controlling terminal, so a confirmation prompt can never wait on a developer's tty.
  setsid -w env IGNITE_UPDATE_API_URL="$SRV/api/ok" IGNITE_UPDATE_DOWNLOAD_URL="$SRV/dl" "${envs[@]}" \
    bash "${UPDATER:-$UPDATE}" --root="$R" "${args[@]}" >"$BASE/$name.out" 2>"$BASE/$name.err"
  RC=$?
  set -e
}
expect_fail() { # name pattern
  [ "$RC" != 0 ] || fail "$1: reported success"
  grep -qi -- "$2" "$BASE/$1.err" || { cat "$BASE/$1.err" >&2; fail "$1: expected '$2'"; }
}
snapshot() {
  (cd "$R" && find usr etc -type f -print0 | sort -z | xargs -0 sha256sum; sha256sum var/lib/scribe-server/server.json) \
    | sha256sum
}
install_root() { # root bundle
  "$2/install.sh" --root="$1" --port="$(free_port)"
}

# --- refusals (installed 0.0.1, from before ignite-update) ---------------------------------
R="$BASE/root-f"
install_root "$R" "$BASE/b001" >/dev/null
[ ! -e "$R/usr/local/bin/ignite-update" ] || fail "legacy fixture has an updater"
SNAP="$(snapshot)"
unchanged() { [ "$(snapshot)" = "$SNAP" ] || fail "$1 changed the installation"; [ ! -e "$R/var/backups/ignite-server" ] || fail "$1 made a backup"; }

upd check -- --check
[ "$RC" = 10 ] || fail "--check with an update available: rc=$RC"
grep -q "0.0.1 -> 0.0.2" "$BASE/check.out" || fail "--check message"
unchanged "--check"

for scen in pre preflag; do
  upd "$scen" IGNITE_UPDATE_API_URL="$SRV/api/$scen"
  [ "$RC" = 0 ] || fail "$scen: rc=$RC"
  grep -q "not a stable release" "$BASE/$scen.out" || fail "$scen: prerelease not excluded"
  unchanged "$scen"
done

upd arch IGNITE_UPDATE_ARCH=riscv64
expect_fail arch "unsupported architecture riscv64"
unchanged arch
other=aarch64; [ "$ARCH" = aarch64 ] && other=x86_64
upd arch2 IGNITE_UPDATE_ARCH="$other"
expect_fail arch2 "no bundle for Linux $other"
unchanged arch2

upd noasset IGNITE_UPDATE_API_URL="$SRV/api/noasset"
expect_fail noasset "has no bundle"
unchanged noasset

upd download IGNITE_UPDATE_DOWNLOAD_URL="$SRV/dl-empty"
expect_fail download "download failed"
unchanged download

upd badsum IGNITE_UPDATE_DOWNLOAD_URL="$SRV/dl-badsum"
expect_fail badsum "checksum mismatch"
unchanged badsum

upd dotdot IGNITE_UPDATE_DOWNLOAD_URL="$SRV/dl-dotdot"
expect_fail dotdot "unsafe path"
unchanged dotdot
upd link IGNITE_UPDATE_DOWNLOAD_URL="$SRV/dl-link"
expect_fail link "links or special files"
unchanged link
[ -z "$(find "$(dirname "$BASE")" -maxdepth 3 -name ignite-update-escape 2>/dev/null | head -1)" ] \
  || fail "archive entry escaped"

upd mismatch IGNITE_UPDATE_API_URL="$SRV/api/mismatch"
expect_fail mismatch "does not say version 0.0.3"
unchanged mismatch

mkdir -p "$R/var/backups"
: >"$R/var/backups/ignite-server"                    # the backup directory cannot be created
upd backup
expect_fail backup "the backup failed"
grep -qi "nothing was installed" "$BASE/backup.err" || fail "backup failure message"
rm -f "$R/var/backups/ignite-server"
unchanged backup

exec 8>"$R/.ignite-update.lock"
flock -n 8 || fail "could not take the test lock"
upd locked
exec 8>&-
expect_fail locked "already running"
unchanged locked

upd noconfirm --
expect_fail noconfirm "re-run with --yes"
unchanged noconfirm

# --- interrupted while the service is stopped for the backup: restarted, nothing installed --
R="$BASE/root-i"
install_root "$R" "$BASE/b001" >/dev/null
SNAP="$(snapshot)"
mkdir -p "$BASE/tmp-i"
for sig in INT TERM HUP; do
  rm -f "$BASE/paused" "$FAKE_SYSTEMCTL_LOG"
  # Started with default signal dispositions (a background job would otherwise ignore SIGINT).
  env IGNITE_UPDATE_API_URL="$SRV/api/ok" IGNITE_UPDATE_DOWNLOAD_URL="$SRV/dl" TMPDIR="$BASE/tmp-i" \
    IGNITE_UPDATE_SYSTEMCTL="$BASE/fake-systemctl" IGNITE_UPDATE_BACKUP_PAUSE="$BASE/paused" \
    python3 -c 'import os, signal, sys
for s in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP): signal.signal(s, signal.SIG_DFL)
os.execvp(sys.argv[1], sys.argv[1:])' bash "$UPDATE" --root="$R" --yes \
    </dev/null >"$BASE/int-$sig.out" 2>"$BASE/int-$sig.err" &
  pid=$!
  for _ in $(seq 100); do [ -e "$BASE/paused" ] && break; sleep 0.1; done
  [ -e "$BASE/paused" ] || fail "$sig: the backup never paused"
  kill -s "$sig" "$pid"
  set +e; wait "$pid"; RC=$?; set -e
  case "$sig" in INT) want=130 ;; TERM) want=143 ;; HUP) want=129 ;; esac
  [ "$RC" = "$want" ] || { cat "$BASE/int-$sig.err" >&2; fail "$sig: rc=$RC, expected $want"; }
  [ "$(cat "$FAKE_SYSTEMCTL_LOG")" = "$(printf '%s\n' 'is-active --quiet scribe-server.service' \
      'stop scribe-server.service' 'start scribe-server.service')" ] \
    || { cat "$FAKE_SYSTEMCTL_LOG" >&2; fail "$sig: service not restarted exactly once after stop"; }
  grep -q "interrupted ($sig)" "$BASE/int-$sig.err" || fail "$sig: not handled as an interruption"
  grep -q "restarted unchanged" "$BASE/int-$sig.err" || fail "$sig: restart not reported"
  grep -q "is incomplete" "$BASE/int-$sig.err" || fail "$sig: partial backup not flagged"
  [ "$(snapshot)" = "$SNAP" ] || fail "$sig: the installation changed"
  [ -z "$(ls -A "$BASE/tmp-i")" ] || fail "$sig: temporary files left behind"
done

# --- RESTORE.txt, run as pasted: a bad backup or a wrong restored identity never starts the --
# service; a good backup restores and starts. (Staging: the block runs against the root.)
R="$BASE/root-r"
sid_r="$(install_root "$R" "$BASE/b001" | sed -n 's/^ *Server ID: //p')"
upd r1 IGNITE_UPDATE_SYSTEMCTL="$BASE/fake-systemctl"
[ "$RC" = 0 ] || { cat "$BASE/r1.err" >&2; fail "restore fixture update: rc=$RC"; }
BR="$(sed -n 's/^Recovery backup: //p' "$BASE/r1.out")"
sed -n "/^sudo bash -s <<'IGNITE_RESTORE'\$/,/^IGNITE_RESTORE\$/p" "$BR/RESTORE.txt" | sed '1d;$d' >"$BASE/restore.sh"
[ -s "$BASE/restore.sh" ] || fail "RESTORE.txt has no restore block"
cp -a "$BR" "$BASE/backup-good"
run_restore() { # name; sets RC
  rm -f "$FAKE_SYSTEMCTL_LOG"
  set +e; bash "$BASE/restore.sh" </dev/null >"$BASE/$1.out" 2>"$BASE/$1.err"; RC=$?; set -e
}
# (a) Failed archive verification: nothing changes, nothing is stopped or started.
SNAP="$(snapshot)"
printf 'x' >>"$BR/state.tar.gz"
run_restore restore-badsum
[ "$RC" != 0 ] && grep -q "RESTORE FAILED: backup checksums do not match" "$BASE/restore-badsum.err" \
  || { cat "$BASE/restore-badsum.err" >&2; fail "restore with a corrupt archive did not refuse"; }
[ ! -s "$FAKE_SYSTEMCTL_LOG" ] || fail "restore with a corrupt archive touched the service"
[ "$(snapshot)" = "$SNAP" ] || fail "restore with a corrupt archive changed the installation"
# (b) Wrong restored identity (consistent checksums): stopped, moved aside, never started.
rm -rf "$BR" && cp -a "$BASE/backup-good" "$BR"
mkdir "$BASE/tamper" && tar -C "$BASE/tamper" -xzf "$BR/state.tar.gz"
sed -i 's/"server_id"[[:space:]]*:[[:space:]]*"[^"]*"/"server_id": "01900000-0000-7000-8000-000000000099"/' \
  "$BASE/tamper/var/lib/scribe-server/server.json"
tar -C "$BASE/tamper" -czf "$BR/state.tar.gz" var/lib/scribe-server etc/scribe-server
(cd "$BR" && sha256sum state.tar.gz software.tar.gz >SHA256SUMS)
run_restore restore-wrongid
[ "$RC" != 0 ] && grep -Eq "RESTORE FAILED: (restored server ID is|the restored server cannot read)" "$BASE/restore-wrongid.err" \
  || { cat "$BASE/restore-wrongid.err" >&2; fail "restore with a wrong identity did not refuse"; }
grep -q "The service was NOT started" "$BASE/restore-wrongid.err" || fail "wrong-identity restore message"
grep -qx "start scribe-server.service" "$FAKE_SYSTEMCTL_LOG" && fail "wrong-identity restore started the service"
grep -qx "stop scribe-server.service" "$FAKE_SYSTEMCTL_LOG" || fail "wrong-identity restore did not stop first"
[ -n "$(find "$R/var/lib" -maxdepth 1 -name 'scribe-server.before-restore.*' -print -quit)" ] \
  || fail "wrong-identity restore did not keep the replaced state"
# (c) The good backup restores 0.0.1 and starts; earlier set-aside copies do not collide.
rm -rf "$BR" && cp -a "$BASE/backup-good" "$BR"
run_restore restore-good
[ "$RC" = 0 ] || { cat "$BASE/restore-good.err" >&2; fail "restore from a good backup failed: rc=$RC"; }
[ "$(tail -1 "$FAKE_SYSTEMCTL_LOG")" = "start scribe-server.service" ] || fail "good restore did not start the service"
[ "$("$R/usr/local/bin/scribe-server" --version)" = 0.0.1 ] || fail "good restore version"
"$R/usr/local/bin/scribe-server" status --config "$R/etc/scribe-server/config.toml" | grep -cx "server_id=$sid_r" >/dev/null \
  || fail "good restore server id"
[ "$(find "$R/var/lib" -maxdepth 1 -name 'scribe-server.before-restore.*' | wc -l)" = 2 ] \
  || fail "set-aside state copies were not both kept"

# --- installer failure: the installer's rollback is reported, data is not "rolled back" ----
R="$BASE/root-g"
install_root "$R" "$BASE/b001" >/dev/null
bin_before="$(sha256sum <"$R/usr/local/bin/scribe-server")"
upd seam IGNITE_UPDATE_INSTALL_SEAM=fail-restart
expect_fail seam "installation failed"
grep -q "restored:" "$BASE/seam.err" || fail "installer rollback not reported"
grep -q "not rolled back automatically" "$BASE/seam.err" || fail "data rollback over-claimed"
[ "$(sha256sum <"$R/usr/local/bin/scribe-server")" = "$bin_before" ] || fail "binary not restored after installer failure"
ls "$R"/var/backups/ignite-server/*/state.tar.gz >/dev/null 2>&1 || fail "no backup before the failed install"

# --- server-ID mismatch is caught by the updater's own verification -------------------------
R="$BASE/root-h"
install_root "$R" "$BASE/b001" >/dev/null
upd idswap IGNITE_UPDATE_API_URL="$SRV/api/idswap"
expect_fail idswap "verification failed"
grep -qi "server id changed\|server status" "$BASE/idswap.err" || fail "idswap reason"
grep -q "not rolled back automatically" "$BASE/idswap.err" || fail "idswap over-claimed rollback"

# --- HTTP verification: the served version, identity, claim state and manifest must match ---
mock_http() { # dir version server_id claimed manifest-file
  mkdir -p "$WWW/$1/control/v1"
  printf '{"v":1,"mode":"product","server_id":"%s","server_name":"x","version":"%s","claimed":%s,"session":null}' \
    "$3" "$2" "$4" >"$WWW/$1/control/v1/status"
  cp "$5" "$WWW/$1/manifest.webmanifest"
}
for bad in version claimed manifest; do
  R="$BASE/root-http-$bad"
  sid_b="$(install_root "$R" "$BASE/b001" | sed -n 's/^ *Server ID: //p')"
  v=0.0.2; c=false; m="$BUNDLE/web/manifest.webmanifest"
  case "$bad" in
    version) v=0.0.1 ;;
    claimed) c=true ;;
    manifest) echo '{"id": "/something-else"}' >"$BASE/other-manifest"; m="$BASE/other-manifest" ;;
  esac
  mock_http "http-$bad" "$v" "$sid_b" "$c" "$m"
  upd "http-$bad" IGNITE_UPDATE_HTTP_BASE="$SRV/http-$bad"
  expect_fail "http-$bad" "verification failed"
  case "$bad" in
    version) grep -q "version other than 0.0.2" "$BASE/http-$bad.err" || fail "http-version reason" ;;
    claimed) grep -q "claimed state other than false" "$BASE/http-$bad.err" || fail "http-claimed reason" ;;
    manifest) grep -q "ignite-web manifest" "$BASE/http-$bad.err" || fail "http-manifest reason" ;;
  esac
done

# --- success: bootstrap an older install, then the installed updater updates itself ---------
R="$BASE/root-s"
out="$(install_root "$R" "$BASE/b001")"
code="$(echo "$out" | sed -n '/Setup code/{n;s/^ *//;p}')"
sid="$(echo "$out" | sed -n 's/^ *Server ID: //p')"
cfg="$R/etc/scribe-server/config.toml"
port="$(sed -n 's/^listen *= *"[^"]*:\([0-9]*\)".*/\1/p' "$cfg")"
"$R/usr/local/bin/scribe-server" serve --config "$cfg" 2>"$BASE/serve.log" &
SERVER_PID=$!
for _ in $(seq 50); do curl -fs "http://127.0.0.1:$port/control/v1/status" >/dev/null && break; sleep 0.2; done
curl -fs -o /dev/null -H 'X-Scribe-Request: 1' -H 'Content-Type: application/json' \
  --data "{\"setup_code\":\"$code\"}" "http://127.0.0.1:$port/control/v1/claim" || fail "claim"
kill "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
mkdir -p "$R/var/lib/scribe-server/ledger"
echo kept >"$R/var/lib/scribe-server/ledger/marker-from-before-update"
sed -i 's/^firewall=.*/firewall=ufw 80 eth0/' "$R/etc/scribe-server/install-state"
cfg_before="$(sha256sum <"$cfg")"

rm -f "$FAKE_SYSTEMCTL_LOG"
upd s1 IGNITE_UPDATE_SYSTEMCTL="$BASE/fake-systemctl"    # the repository's update.sh (bootstrap)
[ "$RC" = 0 ] || { cat "$BASE/s1.err" >&2; fail "update 0.0.1 -> 0.0.2: rc=$RC"; }
# The updater stops the service for the backup but never starts it after installing (install.sh does).
[ "$(cat "$FAKE_SYSTEMCTL_LOG")" = "$(printf '%s\n' 'is-active --quiet scribe-server.service' 'stop scribe-server.service')" ] \
  || { cat "$FAKE_SYSTEMCTL_LOG" >&2; fail "s1: unexpected service calls"; }
grep -q "updated to 0.0.2" "$BASE/s1.out" || fail "s1 success message"
grep -q "Setup code" "$BASE/s1.out" && fail "s1 printed a setup code for a claimed server"
cmp -s "$R/usr/local/bin/ignite-update" "$BASE/b002/ignite-update" || fail "ignite-update not installed from the release"
[ "$("$R/usr/local/bin/scribe-server" --version)" = 0.0.2 ] || fail "s1 version"
"$R/usr/local/bin/scribe-server" status --config "$cfg" | grep -cx "server_id=$sid" >/dev/null || fail "s1 server id"
"$R/usr/local/bin/scribe-server" status --config "$cfg" | grep -cx "claimed=true" >/dev/null || fail "s1 claim lost"
[ -f "$R/var/lib/scribe-server/ledger/marker-from-before-update" ] || fail "s1 ledger lost"
[ "$(sha256sum <"$cfg")" = "$cfg_before" ] || fail "s1 config changed"
grep -qx "firewall=ufw 80 eth0" "$R/etc/scribe-server/install-state" || fail "s1 firewall record changed"
B1="$(sed -n 's/^Recovery backup: //p' "$BASE/s1.out")"
[ -d "$B1" ] && [ "$(stat -c %a "$B1")" = 700 ] && [ "$(stat -c %a "$(dirname "$B1")")" = 700 ] || fail "backup permissions"
[ "$(stat -c %a "$B1/state.tar.gz")" = 600 ] || fail "backup file permissions"
(cd "$B1" && sha256sum -c --quiet SHA256SUMS) || fail "backup checksums"
tar -tzf "$B1/state.tar.gz" | grep -cx 'var/lib/scribe-server/ledger/marker-from-before-update' >/dev/null || fail "backup lacks the ledger"
tar -tzf "$B1/software.tar.gz" | grep -cx 'usr/local/bin/scribe-server' >/dev/null || fail "backup lacks the binary"
grep -q "0.0.1 -> 0.0.2" "$B1/RESTORE.txt" || fail "RESTORE.txt"
grep -qx "B='$B1'" "$B1/RESTORE.txt" || fail "RESTORE.txt names the backup by absolute path"
grep -qx "set -euo pipefail" "$B1/RESTORE.txt" || fail "RESTORE.txt is not fail-fast"
grep -qx "WANT_SERVER_ID='$sid'" "$B1/RESTORE.txt" && grep -qx "WANT_CLAIMED='true'" "$B1/RESTORE.txt" \
  && grep -qx "WANT_VERSION='0.0.1'" "$B1/RESTORE.txt" || fail "RESTORE.txt expected values"
grep -q 'before-restore.XXXXXX' "$B1/RESTORE.txt" || fail "RESTORE.txt sets aside into unique names"
grep -q "\.after-update" "$B1/RESTORE.txt" && fail "RESTORE.txt uses a fixed set-aside name"

mock_http http-s2 "$REAL" "$sid" true "$BUNDLE/web/manifest.webmanifest"
UPDATER="$R/usr/local/bin/ignite-update" upd s2 IGNITE_UPDATE_API_URL="$SRV/api/real" \
  IGNITE_UPDATE_HTTP_BASE="$SRV/http-s2"
[ "$RC" = 0 ] || { cat "$BASE/s2.err" >&2; fail "update 0.0.2 -> $REAL: rc=$RC"; }
[ "$("$R/usr/local/bin/scribe-server" --version)" = "$REAL" ] || fail "s2 version"
cmp -s "$R/usr/local/bin/ignite-update" "$BUNDLE/ignite-update" || fail "ignite-update not replaced by the newer release"
"$R/usr/local/bin/scribe-server" status --config "$cfg" | grep -cx "server_id=$sid" >/dev/null || fail "s2 server id"
"$R/usr/local/bin/scribe-server" status --config "$cfg" | grep -cx "claimed=true" >/dev/null || fail "s2 claim lost"
[ -d "$B1" ] || fail "an earlier backup was deleted"
[ "$(find "$R/var/backups/ignite-server" -mindepth 1 -maxdepth 1 -type d | wc -l)" = 2 ] || fail "expected two backups"

UPDATER="$R/usr/local/bin/ignite-update" upd current IGNITE_UPDATE_API_URL="$SRV/api/real"
[ "$RC" = 0 ] && grep -q "is up to date" "$BASE/current.out" || fail "no-update case"
UPDATER="$R/usr/local/bin/ignite-update" upd older IGNITE_UPDATE_API_URL="$SRV/api/ok"
[ "$RC" = 0 ] && grep -q "downgrades are refused" "$BASE/older.out" || fail "downgrade not refused"
[ "$("$R/usr/local/bin/scribe-server" --version)" = "$REAL" ] || fail "downgraded"
[ "$(find "$R/var/backups/ignite-server" -mindepth 1 -maxdepth 1 -type d | wc -l)" = 2 ] || fail "no-op made a backup"

grep -rq "$code" "$BASE"/*.out "$BASE"/*.err 2>/dev/null && fail "setup code leaked into updater output"
"$BUNDLE/uninstall.sh" --root="$R" >/dev/null
[ ! -e "$R/usr/local/bin/ignite-update" ] || fail "uninstall left ignite-update"
[ -d "$B1" ] || fail "uninstall removed backups"

echo "UPDATE OK (0.0.1 -> 0.0.2 -> $REAL; refusals, rollback reporting, identity check)"
