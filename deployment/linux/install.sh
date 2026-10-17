#!/usr/bin/env bash
# Ignite Server — Linux portable installer. Idempotent: first install OR upgrade.
#
#   sudo ./install.sh                       install/upgrade, start the service, print URL + setup code
#   sudo ./install.sh --firewall=dry-run    show the firewall change without making it
#   ./install.sh --root=DIR                 staging install into DIR (tests: no host systemd/firewall)
#
# Staging-only test seams (require --root=):
#   --test-seam=fail-daemon-reload|fail-enable|fail-restart|fail-health
set -euo pipefail

BUNDLE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=path-safety.sh
. "$BUNDLE/path-safety.sh"

ROOT=""
FIREWALL="auto"
PORT=""
SYSTEMD=1
TEST_SEAM=""
for arg in "$@"; do
  case "$arg" in
    --root=*) ROOT="${arg#--root=}" ;;
    --bundle=*) BUNDLE="${arg#--bundle=}" ;;
    --firewall=*) FIREWALL="${arg#--firewall=}" ;;
    --port=*) PORT="${arg#--port=}" ;;
    --no-systemd) SYSTEMD=0 ;;
    --test-seam=*) TEST_SEAM="${arg#--test-seam=}" ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "install.sh: unknown option $arg" >&2; exit 2 ;;
  esac
done

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

if [ -n "$TEST_SEAM" ]; then
  [ -n "$ROOT" ] || die "--test-seam requires --root= (staging only)"
  case "$TEST_SEAM" in
    fail-daemon-reload|fail-enable|fail-restart|fail-health) ;;
    *) die "--test-seam must be fail-daemon-reload, fail-enable, fail-restart, or fail-health" ;;
  esac
fi

STAGING_IS_FRESH=0
if [ -n "$ROOT" ]; then
  ROOT="$(ignite_reject_staging_root "$ROOT" "staging root")" || exit 1
  if [ ! -e "$ROOT" ]; then
    mkdir -p "$ROOT"
    STAGING_IS_FRESH=1
  fi
  ROOT="$(ignite_canonicalize "$ROOT")"
  ROOT="$(ignite_reject_staging_root "$ROOT" "staging root")" || exit 1
  # Refuse symlink escapes in any pre-existing staging layout before we touch it.
  ignite_assert_staging_destinations_safe "$ROOT" || exit 1
  # Marker policy: only mark empty roots or roots we already marked. Never adopt
  # an arbitrary nonempty directory as purge-safe.
  if [ ! -f "$ROOT/$IGNITE_STAGING_MARKER" ]; then
    if [ "$STAGING_IS_FRESH" != 1 ] && [ -n "$(find "$ROOT" -mindepth 1 -maxdepth 1 2>/dev/null | head -1)" ]; then
      die "staging root is nonempty without $IGNITE_STAGING_MARKER; refusing to adopt it"
    fi
  fi
  SYSTEMD=0
  FIREWALL="none"
  if [ -n "$TEST_SEAM" ]; then
    SYSTEMD=1
  fi
fi

BIN_DST="$ROOT/usr/local/bin/scribe-server"
SHARE="$ROOT/usr/share/scribe-server"
WEB_DST="$SHARE/web"
ETC="$ROOT/etc/scribe-server"
CONFIG="$ETC/config.toml"
INSTALL_STATE="$ETC/install-state"
STATE="$ROOT/var/lib/scribe-server"
UNIT_DST="$ROOT/etc/systemd/system/scribe-server.service"
STATE_RUNTIME="${ROOT:+$ROOT}/var/lib/scribe-server"
WEB_RUNTIME="${ROOT:+$ROOT}/usr/share/scribe-server/web"

validate_bundle_web() {
  local web="$1"
  [ -f "$web/index.html" ] || die "bundle web missing index.html"
  [ -f "$web/manifest.webmanifest" ] || die "bundle web missing manifest.webmanifest"
  local id
  id="$(sed -n 's/.*"id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$web/manifest.webmanifest" | head -1)"
  [ "$id" = "/ignite-web" ] || die "ignite-web manifest id must be /ignite-web (got ${id:-missing})"
  if find "$web" \( -type l -o -type b -o -type c -o -type p -o -type s \) -print | grep -q .; then
    die "bundle web contains symlink or special file (refused)"
  fi
  [ -n "$(find "$web" -name '*.wasm' -type f | head -1)" ] \
    || die "bundle web has no WASM asset (run authoritative npm run build)"
}

# Run a binary without keeping root privileges on live installs.
# Staging (--root=) keeps the calling uid so private temp roots remain usable.
run_binary_preflight() {
  local bin="$1"
  shift
  if [ -z "$ROOT" ] && [ "$(id -u)" -eq 0 ]; then
    if command -v runuser >/dev/null 2>&1 && id -u nobody >/dev/null 2>&1; then
      runuser -u nobody -- "$bin" "$@"
      return $?
    fi
    if command -v su >/dev/null 2>&1; then
      su -s /bin/sh nobody -c "$(printf '%q ' "$bin" "$@")" </dev/null
      return $?
    fi
  fi
  "$bin" "$@"
}

# Unprivileged preflight of the bundle candidate.
# When root: copy into a world-traversable temp dir first so a 0700 parent of the
# source bundle cannot block nobody (Permission denied). Never runs the untrusted
# candidate as root. Does not touch the live installation.
preflight_bundle_binary() {
  local src="$1"
  local tmp=""
  local rc=0
  if [ "$(id -u)" -eq 0 ]; then
    tmp="$(mktemp -d /tmp/ignite-preflight.XXXXXX)" || die "cannot create preflight staging dir"
    chmod 0755 "$tmp" || { rm -rf "$tmp"; die "cannot chmod preflight staging dir"; }
    if ! install -m 0755 "$src" "$tmp/scribe-server"; then
      rm -rf "$tmp"
      die "cannot stage candidate binary for preflight"
    fi
    if command -v runuser >/dev/null 2>&1 && id -u nobody >/dev/null 2>&1; then
      runuser -u nobody -- "$tmp/scribe-server" --help >/dev/null 2>&1 || rc=1
      [ "$rc" = 0 ] && runuser -u nobody -- "$tmp/scribe-server" --version >/dev/null 2>&1 || rc=1
    elif command -v su >/dev/null 2>&1; then
      su -s /bin/sh nobody -c "$(printf '%q ' "$tmp/scribe-server" --help)" </dev/null >/dev/null 2>&1 || rc=1
      [ "$rc" = 0 ] && su -s /bin/sh nobody -c "$(printf '%q ' "$tmp/scribe-server" --version)" </dev/null >/dev/null 2>&1 || rc=1
    else
      rm -rf "$tmp"
      die "cannot drop privileges for binary preflight (need runuser or su)"
    fi
    rm -rf "$tmp"
    [ "$rc" = 0 ] || die "the bundled scribe-server does not run on this machine (architecture/glibc)"
    return 0
  fi
  run_binary_preflight "$src" --help >/dev/null 2>&1 \
    || die "the bundled scribe-server does not run on this machine (architecture/glibc)"
  run_binary_preflight "$src" --version >/dev/null 2>&1 \
    || die "the bundled scribe-server did not report --version"
}

# --- preflight (before any promotion) -------------------------------------------------------
[ -x "$BUNDLE/scribe-server" ] || [ -f "$BUNDLE/scribe-server" ] \
  || die "bundle has no scribe-server binary ($BUNDLE)"
validate_bundle_web "$BUNDLE/web"
if [ -z "$ROOT" ] && [ "$(id -u)" -ne 0 ]; then die "run as root (sudo) — or use --root=DIR for a staging install"; fi
case "$(uname -m)" in
  x86_64|aarch64|armv7l) ;;
  *) die "unsupported architecture $(uname -m)" ;;
esac
if [ "$SYSTEMD" = 1 ] && [ -z "$TEST_SEAM" ]; then
  command -v curl >/dev/null 2>&1 || die "curl is required for post-install health checks"
  command -v systemctl >/dev/null 2>&1 || die "systemctl is required for service install"
fi
if [ -n "$TEST_SEAM" ]; then
  command -v curl >/dev/null 2>&1 || true
fi

preflight_bundle_binary "$BUNDLE/scribe-server"

# shellcheck source=firewall.sh
. "$BUNDLE/firewall.sh"

UPGRADE=0
[ -f "$STATE/server.json" ] && UPGRADE=1
EXPECTED_SERVER_ID=""
if [ "$UPGRADE" = 1 ]; then
  EXPECTED_SERVER_ID="$(sed -n 's/.*"server_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$STATE/server.json" | head -1)"
fi

# --- account + directories -------------------------------------------------------------------
RUN_AS=()
if [ -z "$ROOT" ]; then
  if ! id scribe >/dev/null 2>&1; then
    useradd --system --home-dir /var/lib/scribe-server --no-create-home --shell /usr/sbin/nologin scribe
  fi
  RUN_AS=(runuser -u scribe --)
fi
if [ -n "$ROOT" ]; then
  ignite_assert_staging_destinations_safe "$ROOT" || exit 1
  # Write marker before creating install layout. Only empty/fresh roots or
  # already-marked trees are eligible (never adopt arbitrary nonempty dirs).
  # Never write through a symlinked marker (would overwrite an outside target).
  if [ -L "$ROOT/$IGNITE_STAGING_MARKER" ]; then
    die "refusing to write staging marker: $ROOT/$IGNITE_STAGING_MARKER is a symlink"
  fi
  if [ -f "$ROOT/$IGNITE_STAGING_MARKER" ] || [ "$STAGING_IS_FRESH" = 1 ] \
      || [ -z "$(find "$ROOT" -mindepth 1 -maxdepth 1 2>/dev/null | head -1)" ]; then
    printf 'ignite-staging-root=1\ncreated_by=install.sh\n' >"$ROOT/$IGNITE_STAGING_MARKER" \
      || die "failed to write staging marker"
  else
    die "refusing to write staging marker into nonempty unmarked root"
  fi
  ignite_assert_staging_destinations_safe "$ROOT" || exit 1
fi
install -d -m 0755 "$ROOT/usr/local/bin" "$SHARE" "$ETC"
install -d -m 0700 "$STATE"
[ -z "$ROOT" ] && chown scribe:scribe "$STATE"
if [ -n "$ROOT" ]; then
  ignite_assert_staging_destinations_safe "$ROOT" || exit 1
fi

# --- stage binary + web + unit; promote only after validation -------------------------------
BIN_NEW="$BIN_DST.new"
BIN_OLD="$BIN_DST.old"
WEB_NEW="$WEB_DST.new"
WEB_OLD="$WEB_DST.old"
UNIT_NEW="$UNIT_DST.new"
UNIT_OLD="$UNIT_DST.old"

RESTORED_BIN=0
RESTORED_WEB=0
RESTORED_UNIT=0
RECOVERY_RESTART_OK=""

restore_previous() {
  RESTORED_BIN=0
  RESTORED_WEB=0
  RESTORED_UNIT=0
  if [ -n "$ROOT" ]; then
    if ! ignite_assert_staging_destinations_safe "$ROOT"; then
      echo "install.sh: rollback skipped; staging root unsafe (symlink or escape)" >&2
      return 1
    fi
  fi
  if [ -f "$BIN_OLD" ]; then
    if mv -f "$BIN_OLD" "$BIN_DST"; then RESTORED_BIN=1; fi
  fi
  if [ -d "$WEB_OLD" ]; then
    rm -rf "$WEB_DST"
    if mv "$WEB_OLD" "$WEB_DST"; then RESTORED_WEB=1; fi
  fi
  if [ -f "$UNIT_OLD" ]; then
    if mv -f "$UNIT_OLD" "$UNIT_DST"; then RESTORED_UNIT=1; fi
  elif [ ! -f "$UNIT_DST" ] && [ -f "$UNIT_NEW" ]; then
    rm -f "$UNIT_NEW"
  fi
  rm -f "$BIN_NEW" "$UNIT_NEW"
  rm -rf "$WEB_NEW"
}

describe_restore() {
  local parts=()
  [ "$RESTORED_BIN" = 1 ] && parts+=("binary")
  [ "$RESTORED_WEB" = 1 ] && parts+=("web")
  [ "$RESTORED_UNIT" = 1 ] && parts+=("unit")
  if [ "${#parts[@]}" -eq 0 ]; then
    echo "no prior components were available to restore"
  else
    echo "restored: $(IFS=,; echo "${parts[*]}")"
  fi
}

fail_upgrade() {
  local reason="$1"
  restore_previous
  local restore_msg
  restore_msg="$(describe_restore)"
  if [ -z "$ROOT" ] && [ -z "$TEST_SEAM" ] && command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
    if systemctl restart scribe-server.service >/dev/null 2>&1; then
      RECOVERY_RESTART_OK=1
    else
      RECOVERY_RESTART_OK=0
    fi
    die "install/upgrade failed ($reason); $restore_msg; recovery restart=$RECOVERY_RESTART_OK"
  fi
  die "install/upgrade failed ($reason); $restore_msg"
}

install -m 0755 "$BUNDLE/scribe-server" "$BIN_NEW"
# Staged binary preflight: as nobody when root (not the live path yet).
run_binary_preflight "$BIN_NEW" --help >/dev/null 2>&1 || { rm -f "$BIN_NEW"; die "staged binary failed --help"; }

rm -rf "$WEB_NEW"
cp -R "$BUNDLE/web" "$WEB_NEW"
chmod -R u=rwX,go=rX "$WEB_NEW"
validate_bundle_web "$WEB_NEW"

install -d -m 0755 "$(dirname "$UNIT_DST")"
install -m 0644 "$BUNDLE/scribe-server.service" "$UNIT_NEW"

# Save previous components before any live replacement.
if [ -f "$BIN_DST" ]; then cp -a "$BIN_DST" "$BIN_OLD"; fi
if [ -d "$WEB_DST" ]; then
  rm -rf "$WEB_OLD"
  cp -a "$WEB_DST" "$WEB_OLD"
fi
if [ -f "$UNIT_DST" ]; then cp -a "$UNIT_DST" "$UNIT_OLD"; fi

mv -f "$BIN_NEW" "$BIN_DST"
rm -rf "$WEB_DST"
if ! mv "$WEB_NEW" "$WEB_DST"; then
  fail_upgrade "web promote"
fi
mv -f "$UNIT_NEW" "$UNIT_DST"
[ -f "$BUNDLE/README.txt" ] && install -m 0644 "$BUNDLE/README.txt" "$SHARE/README.txt"

# --- config (first install only) ------------------------------------------------------------
port_in_use() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
if [ ! -f "$CONFIG" ]; then
  if [ -z "$PORT" ]; then
    if [ -z "$ROOT" ] && port_in_use 80; then PORT=8787; else PORT=80; fi
    if [ "$PORT" = 8787 ] && port_in_use 8787; then
      fail_upgrade "ports 80 and 8787 both in use"
    fi
  fi
  umask 022
  cat >"$CONFIG" <<TOML || fail_upgrade "write config.toml"
# Ignite Server configuration (no secrets live here).
[server]
listen = "0.0.0.0:${PORT}"
state_dir = "${STATE_RUNTIME}"
web_root = "${WEB_RUNTIME}"
# Display name on the LAN; empty = "Ignite Server on <Hostname>".
name = ""

[discovery]
advertise = true
TOML
  chmod 0644 "$CONFIG" || fail_upgrade "chmod config.toml"
fi
PORT="$(sed -n 's/^listen *= *"[^"]*:\([0-9]*\)".*/\1/p' "$CONFIG" | head -1)"
[ -n "$PORT" ] || fail_upgrade "missing listen port in config.toml"

# --- identity + one-time setup code ---------------------------------------------------------
INIT_OUT="$("${RUN_AS[@]}" "$BIN_DST" init --config "$CONFIG")" || fail_upgrade "scribe-server init"
SERVER_ID="$(sed -n 's/^server_id=//p' <<<"$INIT_OUT")"
SERVER_NAME="$(sed -n 's/^server_name=//p' <<<"$INIT_OUT")"
SETUP_CODE="$(sed -n 's/^setup_code=//p' <<<"$INIT_OUT")"
[ -n "$SERVER_ID" ] || fail_upgrade "missing server_id from init"
if [ -n "$EXPECTED_SERVER_ID" ] && [ "$SERVER_ID" != "$EXPECTED_SERVER_ID" ]; then
  fail_upgrade "server_id changed during upgrade ($EXPECTED_SERVER_ID -> $SERVER_ID)"
fi

# --- systemd / test seams -------------------------------------------------------------------
health_check() {
  local body="" i
  for i in $(seq 40); do
    body="$(curl -fsS "http://127.0.0.1:${PORT}/control/v1/status" 2>/dev/null || true)"
    if [ -n "$body" ]; then
      # Must be our server identity, not an arbitrary HTTP responder on the port.
      if printf '%s' "$body" | grep -q "$SERVER_ID"; then
        return 0
      fi
    fi
    sleep 0.2
  done
  return 1
}

if [ -n "$TEST_SEAM" ]; then
  case "$TEST_SEAM" in
    fail-daemon-reload) fail_upgrade "simulated systemd daemon-reload failure" ;;
    fail-enable) fail_upgrade "simulated systemd enable failure" ;;
    fail-restart) fail_upgrade "simulated systemd restart failure" ;;
    fail-health) fail_upgrade "simulated health-check failure" ;;
  esac
fi

if [ "$SYSTEMD" = 1 ] && [ -z "$TEST_SEAM" ]; then
  # Every fallible step after live promotion must roll back.
  systemctl daemon-reload || fail_upgrade "systemd daemon-reload"
  systemctl enable scribe-server.service >/dev/null 2>&1 || fail_upgrade "systemd enable"
  if ! systemctl restart scribe-server.service; then
    fail_upgrade "systemd restart"
  fi
  if ! health_check; then
    fail_upgrade "health check (/control/v1/status for server_id=$SERVER_ID)"
  fi
fi

# Success — drop rollback copies only after verification.
rm -rf "$WEB_OLD"
rm -f "$BIN_OLD" "$UNIT_OLD"

# --- firewall -------------------------------------------------------------------------------
FW_RECORD="$(sed -n 's/^firewall=//p' "$INSTALL_STATE" 2>/dev/null || true)"
# shellcheck disable=SC2207
LAN_IFACES=($("$BIN_DST" lan-interfaces 2>/dev/null || true))
case "$FIREWALL" in
  auto)
    if [ -n "$FW_RECORD" ] && [ "${FW_RECORD%% *}" != "$(fw_detect)" ]; then fw_close "$FW_RECORD"; fi
    FW_RECORD="$(fw_open "$PORT" "${LAN_IFACES[@]}")"
    ;;
  dry-run) say "firewall plan (LAN interfaces: ${LAN_IFACES[*]:-none}):"; fw_plan "$PORT" "${LAN_IFACES[@]}" ;;
  none) ;;
  *) die "--firewall must be auto, dry-run or none" ;;
esac
{
  echo "version=$("$BIN_DST" --version 2>/dev/null || echo unknown)"
  echo "port=$PORT"
  echo "firewall=${FW_RECORD}"
} >"$INSTALL_STATE"

# --- result ---------------------------------------------------------------------------------
HOST="$(tr 'A-Z' 'a-z' </proc/sys/kernel/hostname 2>/dev/null || hostname | tr 'A-Z' 'a-z')"
HOST="${HOST%%.*}"
URL="http://${HOST}.local/"
[ "$PORT" != 80 ] && URL="http://${HOST}.local:${PORT}/"
say ""
if [ "$UPGRADE" = 1 ]; then say "Ignite Server upgraded."; else say "Ignite Server installed."; fi
say ""
say "  ${SERVER_NAME}"
say "  Server ID: ${SERVER_ID}"
say ""
say "Open:"
say "  ${URL}"
if [ -n "$SETUP_CODE" ]; then
  say ""
  say "Setup code (one-time):"
  say "  ${SETUP_CODE}"
else
  say ""
  say "This server is already set up. Existing devices and data were kept."
fi
say ""
say "Trusted LAN only (plain HTTP). Do not expose Ignite Server to the Internet."
