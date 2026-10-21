#!/usr/bin/env bash
# ignite-update — update Ignite Server from the official GitHub Releases of markrai/ignite.
#
#   sudo ignite-update            check, confirm, back up, install, verify
#   ignite-update --check         only report whether an update is available (exit 10 if so)
#   sudo ignite-update --yes      no confirmation prompt (every check and the backup still run)
#
# Installs only the latest stable release, never a prerelease or an older version. The bundle is
# verified before anything runs from it; the server state and installed software are backed up
# (service stopped) to /var/backups/ignite-server/ before the bundle's own install.sh runs.
#
# Staging-only (tests): --root=DIR (an installer staging root), with IGNITE_UPDATE_API_URL,
# IGNITE_UPDATE_DOWNLOAD_URL, IGNITE_UPDATE_ARCH, IGNITE_UPDATE_INSTALL_SEAM (an install.sh
# --test-seam), IGNITE_UPDATE_SYSTEMCTL (a stand-in systemctl for the service steps),
# IGNITE_UPDATE_BACKUP_PAUSE (a file created when the backup pauses, so a test can interrupt it)
# and IGNITE_UPDATE_HTTP_BASE (where the HTTP verification looks). These variables are ignored
# without --root.
set -euo pipefail

REPO="markrai/ignite"
API_URL="https://api.github.com/repos/$REPO"
DOWNLOAD_URL="https://github.com/$REPO/releases/download"

ROOT=""
CHECK=0
YES=0
for arg in "$@"; do
  case "$arg" in
    --check) CHECK=1 ;;
    --yes|-y) YES=1 ;;
    --root=*) ROOT="${arg#--root=}" ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "ignite-update: unknown option $arg" >&2; exit 2 ;;
  esac
done

say() { printf '%s\n' "$*"; }
die() { printf 'ignite-update: %s\n' "$*" >&2; exit 1; }

MACHINE="$(uname -m)"
SEAM=""
PROTO="=https"
SYSTEMCTL=systemctl
BACKUP_PAUSE=""
HTTP_BASE=""
if [ -n "$ROOT" ]; then
  [ -d "$ROOT" ] && [ ! -L "$ROOT" ] && [ -f "$ROOT/.ignite-staging-root" ] \
    || die "--root must be an existing installer staging root"
  ROOT="$(cd "$ROOT" && pwd -P)"
  API_URL="${IGNITE_UPDATE_API_URL:-$API_URL}"
  DOWNLOAD_URL="${IGNITE_UPDATE_DOWNLOAD_URL:-$DOWNLOAD_URL}"
  MACHINE="${IGNITE_UPDATE_ARCH:-$MACHINE}"
  SEAM="${IGNITE_UPDATE_INSTALL_SEAM:-}"
  PROTO="=http,https"
  SYSTEMCTL="${IGNITE_UPDATE_SYSTEMCTL:-}"
  BACKUP_PAUSE="${IGNITE_UPDATE_BACKUP_PAUSE:-}"
  HTTP_BASE="${IGNITE_UPDATE_HTTP_BASE:-}"
fi

BIN="$ROOT/usr/local/bin/scribe-server"
UPDATER="$ROOT/usr/local/bin/ignite-update"
CONFIG="$ROOT/etc/scribe-server/config.toml"
STATE="$ROOT/var/lib/scribe-server"
WEB="$ROOT/usr/share/scribe-server/web"
BACKUP_BASE="$ROOT/var/backups/ignite-server"

for tool in curl tar gzip sha256sum; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
done
case "$MACHINE" in
  x86_64|aarch64|armv7l) ;;
  *) die "unsupported architecture $MACHINE" ;;
esac
[ -x "$BIN" ] || die "Ignite Server is not installed ($BIN missing); install it from a release bundle first"
CURRENT="$("$BIN" --version 2>/dev/null | tr -d '[:space:]')"
[[ "$CURRENT" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || die "cannot read the installed version"

# -1, 0 or 1 comparing two X.Y.Z[-pre] versions; a prerelease sorts before its release.
version_cmp() {
  local a="${1%%-*}" b="${2%%-*}" i
  local -a x y
  IFS=. read -r -a x <<<"$a"
  IFS=. read -r -a y <<<"$b"
  for i in 0 1 2; do
    if ((10#${x[i]} > 10#${y[i]})); then echo 1; return; fi
    if ((10#${x[i]} < 10#${y[i]})); then echo -1; return; fi
  done
  if [ "$1" = "$a" ] && [ "$2" != "$b" ]; then echo 1; return; fi
  if [ "$1" != "$a" ] && [ "$2" = "$b" ]; then echo -1; return; fi
  echo 0
}

fetch() { # url out max-seconds
  curl --proto "$PROTO" --proto-redir "$PROTO" -fsSL --retry 2 --connect-timeout 15 \
    --max-time "$3" -H 'Accept: application/vnd.github+json' -o "$2" "$1"
}

WORK=""
STOPPED=0
INSTALL_RAN=0
BACKUP=""
BACKUP_DONE=0
PAUSE_PID=""
INTERRUPTED=""
# Runs on every exit, signals included. Before installation starts, a stopped service is started
# again unchanged; once the installer has run, the service is never restarted or touched here.
cleanup() {
  local rc=$?
  trap '' INT TERM HUP
  [ -n "$PAUSE_PID" ] && kill "$PAUSE_PID" 2>/dev/null
  [ -n "$WORK" ] && rm -rf -- "$WORK"
  if [ "$INSTALL_RAN" = 0 ]; then
    if [ -n "$BACKUP" ] && [ "$BACKUP_DONE" = 0 ]; then
      say "ignite-update: the backup in $BACKUP is incomplete; do not restore from it." >&2
    fi
    if [ "$STOPPED" = 1 ]; then
      if "$SYSTEMCTL" start scribe-server.service; then
        say "ignite-update: the update stopped before installation; Ignite Server was restarted unchanged." >&2
      else
        say "ignite-update: the update stopped before installation and Ignite Server did not restart; run: sudo systemctl start scribe-server" >&2
      fi
    fi
  elif [ -n "$INTERRUPTED" ]; then
    say "ignite-update: interrupted after the installer ran; the service was left as the installer left it. Check: systemctl status scribe-server. Backup: $BACKUP" >&2
  fi
  exit "$rc"
}
on_signal() {
  INTERRUPTED="$1"
  say "ignite-update: interrupted ($1)" >&2
  exit "$2"
}
trap cleanup EXIT
trap 'on_signal INT 130' INT
trap 'on_signal TERM 143' TERM
trap 'on_signal HUP 129' HUP
WORK="$(mktemp -d "${TMPDIR:-/tmp}/ignite-update.XXXXXX")"
chmod 0700 "$WORK"

# --- latest stable release -------------------------------------------------------------------
fetch "$API_URL/releases/latest" "$WORK/release.json" 60 \
  || die "could not query the latest release from $API_URL"
RELEASE="$(tr -d '\r\n' <"$WORK/release.json")"
json_string() { sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" <<<"$RELEASE" | head -1; }
json_bool() { grep -o "\"$1\"[[:space:]]*:[[:space:]]*[a-z]*" <<<"$RELEASE" | head -1 | sed 's/.*:[[:space:]]*//'; }
TAG="$(json_string tag_name)"
[ -n "$TAG" ] || die "the release response has no tag"
if [ "$(json_bool draft)" != false ] || [ "$(json_bool prerelease)" != false ] \
    || ! [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  say "Ignite Server $CURRENT: the latest published release ($TAG) is not a stable release; nothing to do."
  exit 0
fi
LATEST="${TAG#v}"
case "$(version_cmp "$LATEST" "$CURRENT")" in
  0) say "Ignite Server $CURRENT is up to date."; exit 0 ;;
  -1) say "Ignite Server $CURRENT is newer than the latest release ($LATEST); downgrades are refused."; exit 0 ;;
esac
TOP="ignite-server-$LATEST-linux-$MACHINE"
ASSET="$TOP.tar.gz"
grep -q "\"name\"[[:space:]]*:[[:space:]]*\"${ASSET//./\\.}\"" <<<"$RELEASE" \
  || die "release $TAG has no bundle for Linux $MACHINE ($ASSET); this architecture is not supported by that release"

if [ "$CHECK" = 1 ]; then
  say "Update available: Ignite Server $CURRENT -> $LATEST (run: sudo ignite-update)"
  exit 10
fi

# --- exclusive, confirmed -------------------------------------------------------------------
if [ -z "$ROOT" ]; then
  [ "$(id -u)" -eq 0 ] || die "run as root: sudo ignite-update"
  for tool in systemctl flock runuser; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
  done
  LOCK=/run/ignite-update.lock
else
  LOCK="$ROOT/.ignite-update.lock"
fi
exec 9>"$LOCK"
flock -n 9 || die "another ignite-update is already running"

if [ "$YES" != 1 ]; then
  (: </dev/tty >/dev/tty) 2>/dev/null || die "no terminal to confirm on; re-run with --yes"
  printf 'Update Ignite Server %s -> %s? A backup is taken first. [y/N] ' "$CURRENT" "$LATEST" >/dev/tty
  read -r answer </dev/tty || answer=""
  case "$answer" in y|Y|yes|YES) ;; *) say "Cancelled; nothing was changed."; exit 0 ;; esac
fi

# --- download + verify (the service is untouched until all of this has passed) --------------
say "Downloading $ASSET ..."
fetch "$DOWNLOAD_URL/$TAG/$ASSET" "$WORK/$ASSET" 900 || die "download failed: $ASSET"
fetch "$DOWNLOAD_URL/$TAG/$ASSET.sha256" "$WORK/$ASSET.sha256" 60 || die "download failed: $ASSET.sha256"
read -r want name <"$WORK/$ASSET.sha256" || true
[ "${name#\*}" = "$ASSET" ] && [[ "$want" =~ ^[0-9a-f]{64}$ ]] || die "malformed checksum file for $ASSET"
got="$(sha256sum "$WORK/$ASSET" | cut -d' ' -f1)"
[ "$got" = "$want" ] || die "checksum mismatch for $ASSET (refusing to use it)"

# Only regular files and directories, all under $TOP/, nothing absolute or with "..".
tar -tvzf "$WORK/$ASSET" >"$WORK/types" 2>/dev/null || die "the bundle archive is unreadable"
tar -tzf "$WORK/$ASSET" >"$WORK/names" 2>/dev/null || die "the bundle archive is unreadable"
if cut -c1 "$WORK/types" | grep -cv '^[-d]$' >/dev/null; then
  die "the bundle archive contains links or special files (refused)"
fi
while IFS= read -r entry; do
  case "$entry" in
    "$TOP"/|"$TOP") ;;
    /*|*..*) die "unsafe path in the bundle archive: $entry" ;;
    "$TOP"/*) ;;
    *) die "unexpected path in the bundle archive: $entry" ;;
  esac
done <"$WORK/names"
mkdir -m 0700 "$WORK/x"
tar -xzf "$WORK/$ASSET" -C "$WORK/x" --no-same-owner || die "could not extract the bundle"
BUNDLE="$WORK/x/$TOP"
if [ -n "$(find "$WORK/x" \( -type l -o -type b -o -type c -o -type p -o -type s \) -print -quit)" ]; then
  die "the bundle contains links or special files (refused)"
fi
for f in scribe-server install.sh uninstall.sh firewall.sh path-safety.sh scribe-server.service \
    ignite-update web/index.html web/manifest.webmanifest .ignite-bundle; do
  [ -f "$BUNDLE/$f" ] || die "the bundle is incomplete (missing $f)"
done
grep -qx "version=$LATEST" "$BUNDLE/.ignite-bundle" || die "bundle metadata does not say version $LATEST"
grep -qx "arch=$MACHINE" "$BUNDLE/.ignite-bundle" || die "bundle metadata does not say arch $MACHINE"

# The candidate binary must run here; as root, only ever as nobody from a reachable copy.
probe_version() {
  if [ -n "$ROOT" ]; then "$1" --version; return; fi
  local d rc=0
  d="$(mktemp -d /tmp/ignite-update-probe.XXXXXX)"
  chmod 0755 "$d" && install -m 0755 "$1" "$d/scribe-server" \
    && runuser -u nobody -- "$d/scribe-server" --version || rc=1
  rm -rf -- "$d"
  return "$rc"
}
CANDIDATE="$(probe_version "$BUNDLE/scribe-server" 2>/dev/null | tr -d '[:space:]')" \
  || die "the $LATEST server binary does not run on this machine (architecture/glibc)"
[ "$CANDIDATE" = "$LATEST" ] || die "the bundled binary reports version ${CANDIDATE:-none}, expected $LATEST"

# --- what must survive ---------------------------------------------------------------------
status_of() {
  if [ -z "$ROOT" ]; then runuser -u scribe -- "$BIN" status --config "$CONFIG"; else "$BIN" status --config "$CONFIG"; fi
}
[ -f "$STATE/server.json" ] && [ -f "$CONFIG" ] || die "no server state or config to update"
BEFORE="$(status_of)" || die "could not read the current server status"
SERVER_ID="$(sed -n 's/^server_id=//p' <<<"$BEFORE")"
CLAIMED="$(sed -n 's/^claimed=//p' <<<"$BEFORE")"
[ -n "$SERVER_ID" ] && [ -n "$CLAIMED" ] || die "could not read the server identity"

need_kb=$(( $(du -sk "$STATE" "$ROOT/etc/scribe-server" "$WEB" "$BIN" 2>/dev/null | awk '{s+=$1} END {print s+0}') + 65536 ))
mkdir -p "$(dirname "$BACKUP_BASE")" 2>/dev/null || true
free_kb="$(df -Pk "$(dirname "$BACKUP_BASE")" 2>/dev/null | awk 'NR==2 {print $4}')"
[ -n "$free_kb" ] && [ "$free_kb" -ge "$need_kb" ] \
  || die "not enough free space for the backup in $(dirname "$BACKUP_BASE") (need ${need_kb} KiB)"

say ""
say "Ignite Server $CURRENT -> $LATEST"
say "  Server ID: $SERVER_ID (claimed=$CLAIMED)"

# --- stop, back up --------------------------------------------------------------------------
if [ -n "$SYSTEMCTL" ] && "$SYSTEMCTL" is-active --quiet scribe-server.service; then
  say "Stopping Ignite Server for a consistent backup ..."
  STOPPED=1
  "$SYSTEMCTL" stop scribe-server.service || die "could not stop scribe-server.service"
fi

make_backup() {
  local base="$BACKUP_BASE" sw=() rel n_src n_tar
  install -d -m 0700 "$base" && [ -d "$base" ] && [ ! -L "$base" ] && chmod 0700 "$base" || return 1
  BACKUP="$(mktemp -d "$base/$(date -u +%Y%m%dT%H%M%SZ)-$CURRENT-to-$LATEST.XXXX")" || return 1
  chmod 0700 "$BACKUP" || return 1
  for rel in usr/local/bin/scribe-server usr/share/scribe-server \
      etc/systemd/system/scribe-server.service usr/local/bin/ignite-update; do
    [ -e "$ROOT/$rel" ] && sw+=("$rel")
  done
  (
    umask 077
    tar -C "${ROOT:-/}" --numeric-owner -czf "$BACKUP/state.tar.gz" var/lib/scribe-server etc/scribe-server \
      && tar -C "${ROOT:-/}" --numeric-owner -czf "$BACKUP/software.tar.gz" "${sw[@]}" \
      && (cd "$BACKUP" && sha256sum state.tar.gz software.tar.gz >SHA256SUMS)
  ) || return 1
  if [ -n "$BACKUP_PAUSE" ]; then
    : >"$BACKUP_PAUSE"
    sleep 60 &
    PAUSE_PID=$!
    wait "$PAUSE_PID" || true
    PAUSE_PID=""
  fi
  (cd "$BACKUP" && sha256sum -c --quiet SHA256SUMS) || return 1
  tar -tzf "$BACKUP/state.tar.gz" | grep -cx 'var/lib/scribe-server/server.json' >/dev/null || return 1
  n_src="$(cd "${ROOT:-/}" && find var/lib/scribe-server etc/scribe-server -type f | wc -l)"
  n_tar="$(tar -tvzf "$BACKUP/state.tar.gz" | grep -c '^[-h]')"
  [ "$n_src" = "$n_tar" ] || return 1
  n_src="$(cd "${ROOT:-/}" && find "${sw[@]}" -type f | wc -l)"
  n_tar="$(tar -tvzf "$BACKUP/software.tar.gz" | grep -c '^[-h]')"
  [ "$n_src" = "$n_tar" ] || return 1
  local port as_scribe="" svc="systemctl"
  port="$(sed -n 's/^listen *= *"[^"]*:\([0-9]*\)".*/\1/p' "$CONFIG")"
  if [ -z "$ROOT" ]; then as_scribe="runuser -u scribe --"; else svc="${SYSTEMCTL:-true}"; fi
  {
    cat <<TXT
Ignite Server backup taken by ignite-update before updating $CURRENT -> $LATEST
(server $SERVER_ID, claimed=$CLAIMED; the service was stopped while it was taken).

  $BACKUP/state.tar.gz
      /var/lib/scribe-server and /etc/scribe-server
  $BACKUP/software.tar.gz
      scribe-server, ignite-web, the systemd unit and ignite-update

To return to Ignite Server $CURRENT by hand, paste the whole block below (from "sudo bash" to
"IGNITE_RESTORE") into a terminal on the server. It stops at the first failure:
  - the backup is verified first; if that fails, nothing is changed;
  - the current state, configuration and web assets are moved aside into new uniquely named
    directories (never deleted or overwritten); anything written after the update stays there;
  - the restored version, server ID and claimed state must equal the values below before the
    service is started; if any step after stopping fails, the service is left stopped and the
    message says where everything is. This backup is never modified.

sudo bash -s <<'IGNITE_RESTORE'
set -euo pipefail
B='$BACKUP'
WANT_VERSION='$CURRENT'
WANT_SERVER_ID='$SERVER_ID'
WANT_CLAIMED='$CLAIMED'
R='$ROOT'                      # installation root ('' = /)
SVC='$svc'
AS_SCRIBE='$as_scribe'
PORT='${port:-80}'
TXT
    cat <<'TXT'
MOVED=""
fail() {
  echo "RESTORE FAILED: $*" >&2
  echo "The backup in $B is unchanged." >&2
  [ -n "$MOVED" ] && echo "The service was NOT started. The installation from before this attempt was moved to:$MOVED" >&2
  exit 1
}
# 1. Verify the backup; nothing has been changed yet.
(cd "$B" && sha256sum -c --quiet SHA256SUMS) || fail "backup checksums do not match (SHA256SUMS); nothing was changed"
tar -tzf "$B/state.tar.gz" >/dev/null || fail "state.tar.gz is unreadable; nothing was changed"
tar -tzf "$B/software.tar.gz" >/dev/null || fail "software.tar.gz is unreadable; nothing was changed"
# 2. Stop the service; move the current installation aside (collision-free names).
$SVC stop scribe-server.service || fail "could not stop scribe-server; nothing was changed"
for d in "$R/var/lib/scribe-server" "$R/etc/scribe-server" "$R/usr/share/scribe-server/web"; do
  [ -e "$d" ] || continue
  aside="$(mktemp -d "$d.before-restore.XXXXXX")" || fail "could not create a directory beside $d"
  mv "$d" "$aside/" || fail "could not move $d aside into $aside"
  MOVED="$MOVED $aside/"
  echo "moved $d to $aside/"
done
# 3. Restore.
tar -C "${R:-/}" -xzpf "$B/software.tar.gz" || fail "extracting software.tar.gz failed"
tar -C "${R:-/}" -xzpf "$B/state.tar.gz" || fail "extracting state.tar.gz failed"
$SVC daemon-reload || fail "systemctl daemon-reload failed"
# 4. Assert the restored version, identity and claim state before starting anything.
got="$("$R/usr/local/bin/scribe-server" --version)" || fail "the restored scribe-server does not run"
[ "$got" = "$WANT_VERSION" ] || fail "restored version is $got, expected $WANT_VERSION"
status="$($AS_SCRIBE "$R/usr/local/bin/scribe-server" status --config "$R/etc/scribe-server/config.toml")" \
  || fail "the restored server cannot read its state"
got="$(sed -n 's/^server_id=//p' <<<"$status")"
[ "$got" = "$WANT_SERVER_ID" ] || fail "restored server ID is ${got:-missing}, expected $WANT_SERVER_ID"
got="$(sed -n 's/^claimed=//p' <<<"$status")"
[ "$got" = "$WANT_CLAIMED" ] || fail "restored claimed state is ${got:-missing}, expected $WANT_CLAIMED"
# 5. Start.
$SVC start scribe-server.service || fail "the restored service did not start (check: journalctl -u scribe-server)"
echo "Restored Ignite Server $WANT_VERSION (server $WANT_SERVER_ID, claimed=$WANT_CLAIMED)."
echo "Check: curl -fsS http://127.0.0.1:$PORT/control/v1/status"
[ -z "$MOVED" ] || echo "The installation it replaced is kept in:$MOVED"
IGNITE_RESTORE
TXT
  } >"$BACKUP/RESTORE.txt" || return 1
  chmod 0600 "$BACKUP"/*
  BACKUP_DONE=1
}
say "Backing up server state and installed software ..."
make_backup || die "the backup failed${BACKUP:+ ($BACKUP is incomplete)}; nothing was installed"
say "  Backup: $BACKUP"

# --- install (the bundle's own installer, with its binary/web/unit rollback) -----------------
INSTALL_ARGS=(--firewall=none)
[ -n "$ROOT" ] && INSTALL_ARGS+=("--root=$ROOT")
[ -n "$SEAM" ] && INSTALL_ARGS+=("--test-seam=$SEAM")
INSTALL_RAN=1
if ! bash "$BUNDLE/install.sh" "${INSTALL_ARGS[@]}"; then
  running="not running"
  if [ -z "$ROOT" ]; then
    systemctl is-active --quiet scribe-server.service || systemctl start scribe-server.service || true
    systemctl is-active --quiet scribe-server.service && running="running"
    say "Ignite Server is $running ($("$BIN" --version 2>/dev/null || echo unknown))." >&2
  fi
  die "the $LATEST installation failed; the installer's message above says which program files it restored. Server data was not rolled back automatically; the backup is in $BACKUP (see RESTORE.txt)."
fi

# --- verify ---------------------------------------------------------------------------------
fail_verify() {
  die "installed $LATEST but verification failed: $1. Server data was not rolled back automatically; the backup is in $BACKUP (see RESTORE.txt)."
}
[ "$("$BIN" --version 2>/dev/null | tr -d '[:space:]')" = "$LATEST" ] || fail_verify "the installed binary is not $LATEST"
AFTER="$(status_of)" || fail_verify "could not read the server status"
[ "$(sed -n 's/^server_id=//p' <<<"$AFTER")" = "$SERVER_ID" ] || fail_verify "the server ID changed"
[ "$(sed -n 's/^claimed=//p' <<<"$AFTER")" = "$CLAIMED" ] || fail_verify "the claimed state changed"
[ -f "$WEB/index.html" ] && grep -q '"id": "/ignite-web"' "$WEB/manifest.webmanifest" \
  && [ -n "$(find "$WEB" -name '*.wasm' -type f -print -quit)" ] || fail_verify "ignite-web assets are missing"
[ -x "$UPDATER" ] || fail_verify "ignite-update was not installed"
if [ -z "$ROOT" ]; then
  systemctl is-active --quiet scribe-server.service || fail_verify "scribe-server.service is not running"
  HTTP_BASE="http://127.0.0.1:$(sed -n 's/^listen *= *"[^"]*:\([0-9]*\)".*/\1/p' "$CONFIG")"
fi
# What the running server serves: this version, identity and claim state, and this ignite-web.
if [ -n "$HTTP_BASE" ]; then
  served=""
  for _ in $(seq 25); do
    served="$(curl -fsS --max-time 5 "$HTTP_BASE/control/v1/status" 2>/dev/null)" && break
    served=""
    sleep 0.2
  done
  [ -n "$served" ] || fail_verify "the server does not answer at $HTTP_BASE/control/v1/status"
  served="$(tr -d '\r\n' <<<"$served")"
  serves() { grep -Eq "\"$1\"[[:space:]]*:[[:space:]]*$2[[:space:]]*[,}]" <<<"$served"; }
  serves version "\"${LATEST//./\\.}\"" || fail_verify "the server reports a version other than $LATEST"
  serves server_id "\"$SERVER_ID\"" || fail_verify "the server reports a server ID other than $SERVER_ID"
  serves claimed "$CLAIMED" || fail_verify "the server reports claimed state other than $CLAIMED"
  curl -fsS --max-time 10 -o "$WORK/served-manifest" "$HTTP_BASE/manifest.webmanifest" 2>/dev/null \
    && cmp -s "$WORK/served-manifest" "$WEB/manifest.webmanifest" \
    || fail_verify "the server does not serve the installed ignite-web manifest"
fi

say ""
say "Ignite Server updated to $LATEST. Server ID, pairing and notebooks were kept."
say "Recovery backup: $BACKUP"
