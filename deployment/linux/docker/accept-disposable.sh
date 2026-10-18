#!/usr/bin/env bash
# Disposable acceptance for the real Ignite Server image (not mocks).
#
# Default: a fresh Multipass Ubuntu 24.04 VM with Docker installed inside the guest only.
# Works from Linux, macOS, Git Bash, and WSL (Windows CMD: `bash ...` → WSL), using the
# Windows multipass.exe through WSL interop when no native multipass exists.
#
#   bash deployment/linux/docker/accept-disposable.sh
#
# Explicit local mode: this host's Linux Docker Engine (CI runners, Linux dev boxes).
#
#   IGNITE_DOCKER_ACCEPT=local bash deployment/linux/docker/accept-disposable.sh
#
# Optional: IGNITE_DOCKER_BROWSER_SMOKE=1 runs the Playwright claim on the unclaimed server
# (installed inside the disposable guest in Multipass mode; never on this host).
#
# Safety comes from isolation, unique names, and ownership-aware cleanup: the VM, compose
# project, volume, and image are named for this run, refused if they already exist, and only
# resources this invocation created are deleted. Never falls back from Multipass to the host's
# Docker Engine. Never runs docker compose down -v against unrelated projects.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
DOCKER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODE="${IGNITE_DOCKER_ACCEPT:-multipass}"
# Compose project names must be lowercase (hyphens/underscores ok).
RUN_ID="${IGNITE_DOCKER_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
RUN_ID="$(echo "$RUN_ID" | tr '[:upper:]' '[:lower:]')"
case "$RUN_ID" in
  *[!a-z0-9-]*|"") echo "accept-disposable: ERROR: IGNITE_DOCKER_RUN_ID must be [a-z0-9-]" >&2; exit 1 ;;
esac
PROJECT="ignite-dacc-${RUN_ID}"
VOLUME="${PROJECT}-state"
IMAGE="ignite-server:accept-${RUN_ID}"
EVIDENCE="${IGNITE_DOCKER_EVIDENCE:-$ROOT/deployment/linux/docker/evidence-${RUN_ID}}"
BUNDLE="${IGNITE_DOCKER_BUNDLE:-$ROOT/dist/ignite-server-linux}"
BROWSER_REQUESTED="${IGNITE_DOCKER_BROWSER_SMOKE:-0}"

die() { echo "accept-disposable: ERROR: $*" >&2; exit 1; }
say() { echo "accept-disposable: $*"; }

case "$MODE" in
  1|local|yes) MODE=local ;;
  multipass) ;;
  *) die "unknown IGNITE_DOCKER_ACCEPT=$MODE (unset for Multipass, or 'local' for this host's Linux engine)" ;;
esac

# Private scratch for claim JSON / raw HTTP — never under $EVIDENCE; wiped on every exit.
# Created by run_local only (Multipass mode keeps every secret inside the guest).
SCRATCH=""

sanitize() {
  # Strip setup codes, session cookies, pairing secrets, and bearer tokens from evidence.
  sed -E \
    -e 's/setup_code=[A-Z0-9-]+/setup_code=<redacted>/g' \
    -e 's/[Ss]et-[Cc]ookie:[^\r\n]+/Set-Cookie: <redacted>/g' \
    -e 's/scribe_session=[^;[:space:]]+/scribe_session=<redacted>/g' \
    -e 's/[Aa]uthorization:[[:space:]]*Bearer[[:space:]]+[^[:space:]]+/Authorization: Bearer <redacted>/g' \
    -e 's/"setup_code"[[:space:]]*:[[:space:]]*"[^"]+"/"setup_code":"<redacted>"/g' \
    -e 's/"request_secret"[[:space:]]*:[[:space:]]*"[^"]+"/"request_secret":"<redacted>"/g' \
    -e 's/"credential"[[:space:]]*:[[:space:]]*"[^"]+"/"credential":"<redacted>"/g' \
    -e 's/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\.[A-Za-z0-9_-]{16,}/<redacted-credential>/g'
}

FIXTURES_DIR="${IGNITE_DOCKER_FIXTURES:-$ROOT/ignite/web/tests/fixtures}"
ROUNDTRIP_SH="$DOCKER_DIR/api-notebook-blob-roundtrip.sh"

wipe_scratch() {
  [ -n "${SCRATCH:-}" ] && [ -d "$SCRATCH" ] || return 0
  find "$SCRATCH" -type f -exec rm -f {} + 2>/dev/null || true
  rm -rf "$SCRATCH" 2>/dev/null || true
  SCRATCH=""
}

# Only resources successfully created by THIS invocation (never pattern-matched names).
OWNED_CONTAINERS=()
OWNED_VOLUMES=()
OWNED_IMAGE=""
COMPOSE_OWNED=0

refuse_if_exists() {
  local kind="$1" name="$2"
  case "$kind" in
    volume)
      if docker volume inspect "$name" >/dev/null 2>&1; then
        die "refusing to adopt preexisting volume: $name"
      fi
      ;;
    container)
      if docker inspect "$name" >/dev/null 2>&1; then
        die "refusing to reuse preexisting container name: $name"
      fi
      ;;
    image)
      if docker image inspect "$name" >/dev/null 2>&1; then
        die "refusing to overwrite preexisting image tag: $name"
      fi
      ;;
  esac
}

create_owned_volume() {
  local name="$1"
  refuse_if_exists volume "$name"
  docker volume create "$name" >/dev/null
  OWNED_VOLUMES+=("$name")
}

# Multipass mode ownership: set only after we verified the name was free and called launch.
VM=""
VM_OWNED=0
STAGE=""
STAGE_OWNED=0

cleanup() {
  local rc=$?
  set +e
  wipe_scratch
  # Multipass mode: Docker resources live in the guest; purge the VM we launched.
  if [ "$MODE" = "multipass" ]; then
    cleanup_vm
    # A leaked VM must not hide behind a successful exit status.
    if [ "$VM_CLEANUP" = "FAIL" ] && [ "$rc" -eq 0 ]; then
      exit 1
    fi
    return 0
  fi
  if [ "${COMPOSE_OWNED:-0}" = "1" ] && [ -n "${COMPOSE_PROJECT_NAME:-}" ] \
    && [ -f "$DOCKER_DIR/docker-compose.yml" ]; then
    # Remove containers/networks for THIS project only — never -v here.
    docker compose -p "$COMPOSE_PROJECT_NAME" -f "$DOCKER_DIR/docker-compose.yml" \
      down --remove-orphans >/dev/null 2>&1
  fi
  local c v
  for c in "${OWNED_CONTAINERS[@]+"${OWNED_CONTAINERS[@]}"}"; do
    [ -n "$c" ] || continue
    docker rm -f "$c" >/dev/null 2>&1
  done
  for v in "${OWNED_VOLUMES[@]+"${OWNED_VOLUMES[@]}"}"; do
    [ -n "$v" ] || continue
    docker volume rm "$v" >/dev/null 2>&1
  done
  if [ -n "${OWNED_IMAGE:-}" ]; then
    docker image rm -f "$OWNED_IMAGE" >/dev/null 2>&1
  fi
  set -e
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

run_local() {
  command -v docker >/dev/null 2>&1 || die "docker CLI not found (IGNITE_DOCKER_ACCEPT=local needs a Linux Docker Engine on this host)"
  docker info >/dev/null 2>&1 || die "Docker daemon not accessible"
  # Host networking requires a Linux engine. Docker Desktop for Windows/macOS is insufficient.
  os="$(docker info --format '{{.OperatingSystem}}' 2>/dev/null || true)"
  say "engine OS report: ${os:-unknown}"
  if echo "$os" | grep -qiE 'Docker Desktop|linuxkit'; then
    die "this engine looks like Docker Desktop/linuxkit — host networking is not a supported accept path.
Run without IGNITE_DOCKER_ACCEPT to use a disposable Multipass guest instead."
  fi

  mkdir -p "$EVIDENCE"
  chmod 700 "$EVIDENCE" 2>/dev/null || true
  SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/ignite-dacc-scratch.XXXXXX")"
  chmod 700 "$SCRATCH"

  if [ ! -f "$BUNDLE/scribe-server" ]; then
    say "building Linux bundle via build-bundle.sh"
    bash "$ROOT/deployment/linux/build-bundle.sh" "$BUNDLE"
  fi

  refuse_if_exists image "$IMAGE"
  export IGNITE_DOCKER_IMAGE="$IMAGE"
  bash "$DOCKER_DIR/build-image.sh" "$BUNDLE"
  docker image inspect "$IMAGE" >/dev/null 2>&1 || die "image $IMAGE missing after build-image"
  OWNED_IMAGE="$IMAGE"

  export COMPOSE_PROJECT_NAME="$PROJECT"

  # Override compose volume name + image via a disposable overlay (image-only; no build).
  OVERLAY="$EVIDENCE/compose.override.yml"
  cat >"$OVERLAY" <<YAML
services:
  ignite-server:
    image: ${IMAGE}
    container_name: ${PROJECT}
volumes:
  ignite-state:
    name: ${VOLUME}
    external: true
YAML
  COMPOSE=(docker compose -p "$PROJECT" -f "$DOCKER_DIR/docker-compose.yml" -f "$OVERLAY")

  say "init (first-run)"
  export COMPOSE_ANSI=never
  export DOCKER_CLI_HINTS=false
  create_owned_volume "$VOLUME"
  run_init() {
    local name="$1"
    refuse_if_exists container "$name"
    docker run --rm --network host --name "$name" \
      -v "${VOLUME}:/var/lib/scribe-server" \
      --entrypoint /usr/local/bin/docker-entrypoint.sh \
      "$IMAGE" init --config /etc/scribe-server/config.toml 2>/dev/null \
      | grep -E '^(server_id|server_name|identity|claimed|setup_code)='
  }
  INIT_OUT="$(run_init "${PROJECT}-init")" || die "init failed"
  printf '%s\n' "$INIT_OUT" | sanitize | tee "$EVIDENCE/init-1.txt"
  # Prove control DB landed on the named volume before we start serve.
  docker run --rm --entrypoint /bin/sh -v "${VOLUME}:/state:ro" "$IMAGE" \
    -c 'set -e; ls -la /state; test -f /state/control.sqlite; test -f /state/server.json' \
    >"$EVIDENCE/volume-after-init.txt" 2>&1 \
    || die "state volume missing control.sqlite/server.json after init (see volume-after-init.txt)"
  echo "$INIT_OUT" | grep -q '^server_id=' || die "init missing server_id"
  echo "$INIT_OUT" | grep -q '^claimed=false' || die "expected unclaimed after first init"
  echo "$INIT_OUT" | grep -q '^setup_code=' || die "expected setup_code while unclaimed"
  SERVER_ID="$(printf '%s\n' "$INIT_OUT" | sed -n 's/^server_id=//p' | head -1 | tr -d '\r')"
  CODE1="$(printf '%s\n' "$INIT_OUT" | sed -n 's/^setup_code=//p' | head -1 | tr -d '\r')"
  [ -n "$SERVER_ID" ] && [ -n "$CODE1" ] || die "parse init fields"
  echo "$CODE1" | grep -Eq '^[A-Z0-9]{4}-[A-Z0-9]{4}$' \
    || die "unexpected setup_code format (len=${#CODE1})"

  # Use the first init's code for claim. Rotation is verified after a successful claim
  # is not required for first-run; we still document rotation with a dedicated check
  # against a throwaway init only when IGNITE_DOCKER_TEST_ROTATION=1 (default on).
  SETUP_CODE="$CODE1"
  echo "setup_code_len=${#SETUP_CODE}" >"$EVIDENCE/setup-code-meta.txt"

  if [ "${IGNITE_DOCKER_TEST_ROTATION:-1}" = "1" ]; then
    INIT2="$(run_init "${PROJECT}-init2")" || die "second init failed"
    printf '%s\n' "$INIT2" | sanitize | tee "$EVIDENCE/init-2-rotate.txt"
    CODE2="$(printf '%s\n' "$INIT2" | sed -n 's/^setup_code=//p' | head -1 | tr -d '\r')"
    [ -n "$CODE2" ] || die "second init missing setup_code"
    [ "$CODE2" != "$CODE1" ] || die "expected setup code rotation on re-init while unclaimed"
    echo "$CODE2" | grep -Eq '^[A-Z0-9]{4}-[A-Z0-9]{4}$' \
      || die "unexpected rotated setup_code format (len=${#CODE2})"
    SID2="$(printf '%s\n' "$INIT2" | sed -n 's/^server_id=//p' | head -1 | tr -d '\r')"
    [ "$SID2" = "$SERVER_ID" ] || die "server_id changed on re-init ($SERVER_ID -> $SID2)"
    # Current valid code is the rotated one.
    SETUP_CODE="$CODE2"
    echo "setup_code_len=${#SETUP_CODE} rotated=1" >"$EVIDENCE/setup-code-meta.txt"
  fi

  say "compose up"
  refuse_if_exists container "$PROJECT"
  "${COMPOSE[@]}" up -d --no-build
  COMPOSE_OWNED=1
  OWNED_CONTAINERS+=("$PROJECT")
  # Wait for health / status
  ok=0
  for i in $(seq 1 60); do
    body="$(curl -fsS http://127.0.0.1:8787/control/v1/status 2>/dev/null || true)"
    if printf '%s' "$body" | grep -q "$SERVER_ID"; then
      ok=1
      break
    fi
    sleep 0.5
  done
  [ "$ok" = 1 ] || die "status endpoint did not become healthy with server_id=$SERVER_ID"
  printf '%s\n' "$body" | sanitize | tee "$EVIDENCE/status-1.json"

  # Privilege boundary evidence (PID 1 after setpriv drop; image Config.User may be root).
  uid="$("${COMPOSE[@]}" exec -T ignite-server sh -c 'awk "/^Uid:/{print \$2; exit}" /proc/1/status')"
  gid="$("${COMPOSE[@]}" exec -T ignite-server sh -c 'awk "/^Gid:/{print \$2; exit}" /proc/1/status')"
  cape="$("${COMPOSE[@]}" exec -T ignite-server sh -c 'awk "/^CapEff:/{print \$2; exit}" /proc/1/status')"
  capa="$("${COMPOSE[@]}" exec -T ignite-server sh -c 'awk "/^CapAmb:/{print \$2; exit}" /proc/1/status')"
  img_user="$(docker inspect "$PROJECT" --format '{{.Config.User}}')"
  nnp="$(docker inspect "$PROJECT" --format '{{.HostConfig.SecurityOpt}}')"
  {
    echo "pid1_uid=$uid pid1_gid=$gid"
    echo "pid1_CapEff=$cape pid1_CapAmb=$capa"
    echo "image_Config.User=${img_user:-<empty=root>}"
    echo "HostConfig.SecurityOpt=$nnp"
    echo "note=entrypoint may briefly run as root for volume chown before setpriv drop"
  } | tee "$EVIDENCE/identity-uid.txt"
  [ "$uid" = "19187" ] && [ "$gid" = "19187" ] || die "expected pid1 uid/gid 19187, got $uid:$gid"
  # CapAmb should be zero after setpriv --ambient-caps=-all
  echo "$capa" | grep -Eqi '^0+$' || die "expected CapAmb all-zero, got $capa"
  echo "$nnp" | grep -qi 'no-new-privileges' || die "expected no-new-privileges in SecurityOpt"

  docker inspect "$PROJECT" --format 'Privileged={{.HostConfig.Privileged}} NetworkMode={{.HostConfig.NetworkMode}} Binds={{json .HostConfig.Binds}} ReadonlyRootfs={{.HostConfig.ReadonlyRootfs}}' \
    | tee "$EVIDENCE/inspect-security.txt"
  docker inspect "$PROJECT" --format '{{.HostConfig.Privileged}}' | grep -qi true \
    && die "container is privileged" || true
  docker inspect "$PROJECT" --format '{{.HostConfig.NetworkMode}}' | grep -qx 'host' \
    || die "network mode is not host"
  docker inspect "$PROJECT" --format '{{.HostConfig.ReadonlyRootfs}}' | grep -qi true \
    || die "expected ReadonlyRootfs=true"

  # Web assets
  curl -fsS -o "$EVIDENCE/index.html" http://127.0.0.1:8787/
  grep -qi 'html' "$EVIDENCE/index.html" || die "index.html not served"
  curl -fsS -o "$EVIDENCE/manifest.webmanifest" http://127.0.0.1:8787/manifest.webmanifest
  grep -q '"/ignite-web"' "$EVIDENCE/manifest.webmanifest" || die "PWA id /ignite-web missing"

  # Claim via control API — secrets only in $SCRATCH (never under $EVIDENCE).
  CLAIM_JSON="$SCRATCH/claim.json"
  printf '{"setup_code":"%s","display_name":"accept-admin"}\n' "$SETUP_CODE" >"$CLAIM_JSON"
  chmod 600 "$CLAIM_JSON"

  # Rotated-away code must fail (only when we actually rotated).
  if [ -n "${CODE2:-}" ]; then
    OLD_JSON="$SCRATCH/claim-old.json"
    printf '{"setup_code":"%s","display_name":"accept-admin"}\n' "$CODE1" >"$OLD_JSON"
    chmod 600 "$OLD_JSON"
    OLD_HTTP="$(curl -sS -o "$SCRATCH/claim-old.body" -w '%{http_code}' \
      -X POST http://127.0.0.1:8787/control/v1/claim \
      -H 'Content-Type: application/json' \
      -H 'X-Scribe-Request: 1' \
      --data-binary @"$OLD_JSON" || true)"
    sanitize <"$SCRATCH/claim-old.body" >"$EVIDENCE/claim-old.body.txt"
    rm -f "$OLD_JSON" "$SCRATCH/claim-old.body"
    [ "$OLD_HTTP" = "403" ] || die "expected HTTP 403 claiming with rotated-away code, got $OLD_HTTP"
  fi

  BROWSER_CLAIM="NOT_TESTED"
  BROWSER_NOTE="IGNITE_DOCKER_BROWSER_SMOKE!=1"
  CLAIM_VIA="api"
  SMOKE_DIR="${IGNITE_DOCKER_SMOKE_DIR:-$ROOT/deployment/linux/acceptance/browser-smoke}"

  # Optional Playwright claim on an *unclaimed* server (must not run against a claimed host).
  # Dependencies are never installed here: missing ones make the browser result BLOCKED.
  if [ "$BROWSER_REQUESTED" = "1" ]; then
    if [ ! -f "$SMOKE_DIR/smoke.mjs" ]; then
      BROWSER_CLAIM="BLOCKED"; BROWSER_NOTE="smoke.mjs missing"
    elif ! command -v node >/dev/null 2>&1; then
      BROWSER_CLAIM="BLOCKED"; BROWSER_NOTE="node missing"
    elif [ ! -d "$SMOKE_DIR/node_modules/playwright" ]; then
      BROWSER_CLAIM="BLOCKED"; BROWSER_NOTE="playwright not installed (install-browser-smoke.sh)"
    elif ! (cd "$SMOKE_DIR" && node --input-type=module -e \
        'import { chromium } from "playwright"; import { existsSync } from "node:fs"; process.exit(existsSync(chromium.executablePath()) ? 0 : 1);') \
        >/dev/null 2>&1; then
      BROWSER_CLAIM="BLOCKED"; BROWSER_NOTE="playwright chromium not installed"
    fi
    [ "$BROWSER_CLAIM" = "BLOCKED" ] && say "browser claim BLOCKED: $BROWSER_NOTE (continuing with API claim)"
  fi

  if [ "$BROWSER_REQUESTED" = "1" ] && [ "$BROWSER_CLAIM" = "NOT_TESTED" ]; then
    say "browser claim smoke (unclaimed server)"
    set +e
    (
      cd "$SMOKE_DIR"
      export IGNITE_ACCEPT_SETUP_CODE="$SETUP_CODE"
      export IGNITE_ACCEPT_BASE_URL="http://127.0.0.1:8787"
      export IGNITE_ACCEPT_EVIDENCE="$EVIDENCE"
      export IGNITE_ACCEPT_FIXTURES="$FIXTURES_DIR"
      export IGNITE_ACCEPT_SESSION_FILE="$SCRATCH/session.cookie"
      node smoke.mjs
    )
    SMOKE_RC=$?
    set -e
    if [ "$SMOKE_RC" -eq 0 ] && [ -f "$SCRATCH/session.cookie" ]; then
      BROWSER_CLAIM="PASS"
      BROWSER_NOTE="claim_via=browser"
      CLAIM_VIA="browser"
      unset SETUP_CODE
    else
      BROWSER_CLAIM="FAIL"
      BROWSER_NOTE="rc=$SMOKE_RC"
      rm -f "$SCRATCH/session.cookie"
      # If smoke claimed the server before failing later, API claim cannot recover.
      pre="$(curl -fsS http://127.0.0.1:8787/control/v1/status 2>/dev/null || true)"
      if printf '%s' "$pre" | grep -Eq '"claimed"[[:space:]]*:[[:space:]]*true'; then
        echo "browser_claim=FAIL $BROWSER_NOTE claimed_by_browser=true" >"$EVIDENCE/browser-note.txt"
        wipe_scratch
        die "browser smoke failed after claiming server (rc=$SMOKE_RC); refuse API claim on claimed host"
      fi
      say "browser smoke failed before claim (rc=$SMOKE_RC); continuing with API claim (overall result will be FAIL)"
    fi
  fi
  echo "browser_claim=$BROWSER_CLAIM ($BROWSER_NOTE)" >"$EVIDENCE/browser-note.txt"

  if [ "$CLAIM_VIA" = "api" ]; then
    CLAIM_HTTP="$(curl -sS -D "$SCRATCH/claim.headers" -o "$SCRATCH/claim.body" -w '%{http_code}' \
      -X POST http://127.0.0.1:8787/control/v1/claim \
      -H 'Content-Type: application/json' \
      -H 'X-Scribe-Request: 1' \
      --data-binary @"$CLAIM_JSON" || true)"
    if [ "$CLAIM_HTTP" != "200" ]; then
      sanitize <"$SCRATCH/claim.body" >"$EVIDENCE/claim.body.txt" || true
      echo "claim_http=$CLAIM_HTTP" >"$EVIDENCE/claim.status.txt"
      wipe_scratch
      die "claim HTTP failed (status=$CLAIM_HTTP); see claim.body.txt (sanitized)"
    fi
    grep -qi 'scribe_session' "$SCRATCH/claim.headers" \
      || { wipe_scratch; die "claim did not set session cookie"; }
    # Admin cookie goes straight from headers to a 0600 scratch file (never a shell variable,
    # argv, or stdout); sanitized copy only under evidence.
    (umask 077; python3 - "$SCRATCH/claim.headers" "$SCRATCH/session.cookie" <<'PY'
import sys
cookie = ""
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    if line.lower().startswith("set-cookie:") and "scribe_session=" in line:
        part = line.split(":", 1)[1].strip()
        # first cookie pair
        for seg in part.split(";"):
            seg = seg.strip()
            if seg.startswith("scribe_session="):
                cookie = seg
                break
if not cookie:
    raise SystemExit("no scribe_session in Set-Cookie")
open(sys.argv[2], "w", encoding="utf-8").write(cookie + "\n")
PY
    ) || { wipe_scratch; die "could not extract session cookie from claim response"; }
    sanitize <"$SCRATCH/claim.headers" >"$EVIDENCE/claim.headers.txt"
    sanitize <"$SCRATCH/claim.body" >"$EVIDENCE/claim.body.txt"
    echo "claim_http=200 cookie=present claim_via=api" >"$EVIDENCE/claim.status.txt"
    rm -f "$CLAIM_JSON" "$SCRATCH/claim.headers" "$SCRATCH/claim.body"
    unset SETUP_CODE CODE1 CODE2
  else
    rm -f "$CLAIM_JSON"
    unset CODE1 CODE2
    echo "claim_via=browser cookie=present" >"$EVIDENCE/claim.status.txt"
  fi

  STATUS_CLAIMED="$(curl -fsS http://127.0.0.1:8787/control/v1/status)"
  printf '%s\n' "$STATUS_CLAIMED" | sanitize | tee "$EVIDENCE/status-claimed.json"
  echo "$STATUS_CLAIMED" | grep -q "$SERVER_ID" || die "claimed status missing server_id"
  echo "$STATUS_CLAIMED" | grep -Eq '"claimed"[[:space:]]*:[[:space:]]*true' \
    || die "expected claimed=true in status JSON after claim"

  # Product API notebook + blob round-trip (pairing ≠ admin claim).
  [ -f "$ROUNDTRIP_SH" ] || die "missing $ROUNDTRIP_SH"
  [ -d "$FIXTURES_DIR" ] || die "fixtures missing: $FIXTURES_DIR"
  command -v python3 >/dev/null 2>&1 || die "python3 required for notebook/blob round-trip"
  say "notebook/blob API round-trip (seed)"
  # Bearer travels only via $SCRATCH/bearer.token (0600); stdout carries ids only.
  NOTEBOOK_BLOB_ROUNDTRIP="FAIL"
  ROUNDTRIP_OUT="$SCRATCH/roundtrip.out"
  BASE_URL="http://127.0.0.1:8787" \
    ADMIN_COOKIE_FILE="$SCRATCH/session.cookie" \
    FIXTURES_DIR="$FIXTURES_DIR" \
    EVIDENCE="$EVIDENCE" \
    SCRATCH="$SCRATCH" \
    PHASE_LABEL="initial" \
    MODE="seed" \
    bash "$ROUNDTRIP_SH" | tee "$ROUNDTRIP_OUT"
  RT_LIBRARY_ID="$(sed -n 's/^LIBRARY_ID=//p' "$ROUNDTRIP_OUT" | head -1)"
  RT_BLOB_SHA="$(sed -n 's/^BLOB_SHA=//p' "$ROUNDTRIP_OUT" | head -1)"
  [ -s "$SCRATCH/bearer.token" ] && [ -n "$RT_LIBRARY_ID" ] && [ -n "$RT_BLOB_SHA" ] \
    || die "round-trip seed did not produce bearer file / LIBRARY_ID / BLOB_SHA"
  rm -f "$ROUNDTRIP_OUT" "$SCRATCH/session.cookie"
  NOTEBOOK_BLOB_ROUNDTRIP="PASS"
  RESTART_PERSISTENCE="FAIL"
  RECREATE_PERSISTENCE="FAIL"

  say "restart persistence"
  "${COMPOSE[@]}" restart ignite-server
  ok=0
  for i in $(seq 1 60); do
    body="$(curl -fsS http://127.0.0.1:8787/control/v1/status 2>/dev/null || true)"
    if printf '%s' "$body" | grep -q "$SERVER_ID" \
      && printf '%s' "$body" | grep -Eq '"claimed"[[:space:]]*:[[:space:]]*true'; then
      ok=1
      break
    fi
    sleep 0.5
  done
  [ "$ok" = 1 ] || die "post-restart status failed"
  printf '%s\n' "$body" | sanitize | tee "$EVIDENCE/status-after-restart.json"
  echo "$body" | grep -q "$SERVER_ID" || die "server_id lost after restart"

  say "notebook/blob verify after restart"
  BASE_URL="http://127.0.0.1:8787" \
    BEARER_FILE="$SCRATCH/bearer.token" \
    FIXTURES_DIR="$FIXTURES_DIR" \
    EVIDENCE="$EVIDENCE" \
    SCRATCH="$SCRATCH" \
    PHASE_LABEL="after-restart" \
    MODE="verify" \
    bash "$ROUNDTRIP_SH"
  RESTART_PERSISTENCE="PASS"

  # Recreate container (same image + volume) — NOT an image-upgrade proof.
  say "recreate container (same image + volume)"
  "${COMPOSE[@]}" up -d --force-recreate --no-build
  ok=0
  for i in $(seq 1 60); do
    body="$(curl -fsS http://127.0.0.1:8787/control/v1/status 2>/dev/null || true)"
    if printf '%s' "$body" | grep -q "$SERVER_ID"; then ok=1; break; fi
    sleep 0.5
  done
  [ "$ok" = 1 ] || die "status failed after recreate"
  printf '%s\n' "$body" | sanitize | tee "$EVIDENCE/status-after-recreate.json"
  echo "$body" | grep -q "$SERVER_ID" || die "server_id lost after recreate"
  echo "$body" | grep -Eq '"claimed"[[:space:]]*:[[:space:]]*true' \
    || die "expected claimed persistence after recreate (init must not auto-run)"

  say "notebook/blob verify after recreate"
  BASE_URL="http://127.0.0.1:8787" \
    BEARER_FILE="$SCRATCH/bearer.token" \
    FIXTURES_DIR="$FIXTURES_DIR" \
    EVIDENCE="$EVIDENCE" \
    SCRATCH="$SCRATCH" \
    PHASE_LABEL="after-recreate" \
    MODE="verify" \
    bash "$ROUNDTRIP_SH"
  RECREATE_PERSISTENCE="PASS"

  # Init after claim: no setup_code, same server_id
  INIT3="$(run_init "${PROJECT}-init3")" || die "post-claim init failed"
  printf '%s\n' "$INIT3" | sanitize | tee "$EVIDENCE/init-after-claim.txt"
  echo "$INIT3" | grep -q '^claimed=true' || die "expected claimed=true on init after claim"
  echo "$INIT3" | grep -q 'setup_code=' && die "setup_code must not appear after claim" || true
  echo "$INIT3" | grep -q "server_id=$SERVER_ID" || die "server_id mismatch after claim init"

  # Filesystem marker in ledger/ — not a notebook/blob API round trip.
  say "ledger dir filesystem marker (not notebook/blob round-trip)"
  "${COMPOSE[@]}" exec -T ignite-server sh -c \
    'mkdir -p /var/lib/scribe-server/ledger && echo accept-marker-'"$RUN_ID"' > /var/lib/scribe-server/ledger/.accept-marker'
  MARKER="$("${COMPOSE[@]}" exec -T ignite-server cat /var/lib/scribe-server/ledger/.accept-marker | tr -d '\r')"
  echo "$MARKER" | tee "$EVIDENCE/ledger-marker.txt"
  echo "$MARKER" | grep -q "accept-marker-$RUN_ID" || die "marker missing"

  "${COMPOSE[@]}" up -d --force-recreate --no-build
  sleep 2
  MARKER2="$("${COMPOSE[@]}" exec -T ignite-server cat /var/lib/scribe-server/ledger/.accept-marker | tr -d '\r')"
  echo "$MARKER2" | tee "$EVIDENCE/ledger-marker-after-recreate.txt"
  [ "$MARKER2" = "$MARKER" ] || die "ledger marker lost after recreate"

  # Failure: bad config should fail visibly
  say "invalid config failure"
  BADCFG="$SCRATCH/bad-config.toml"
  echo 'this is not valid toml [[[[' >"$BADCFG"
  refuse_if_exists container "${PROJECT}-badcfg"
  set +e
  BAD_OUT="$(docker run --rm --network host \
    -v "${VOLUME}:/var/lib/scribe-server" \
    -v "${BADCFG}:/etc/scribe-server/config.toml:ro" \
    --name "${PROJECT}-badcfg" \
    --entrypoint /usr/local/bin/docker-entrypoint.sh \
    "$IMAGE" serve --config /etc/scribe-server/config.toml 2>&1)"
  BAD_RC=$?
  set -e
  printf '%s\n' "$BAD_OUT" | sanitize | tee "$EVIDENCE/bad-config.out"
  [ "$BAD_RC" -ne 0 ] || die "expected nonzero exit for bad config"

  # Serve logs must not contain setup_code
  LOGS="$("${COMPOSE[@]}" logs --no-color 2>&1 || true)"
  printf '%s\n' "$LOGS" | sanitize | tee "$EVIDENCE/serve.logs.txt"
  echo "$LOGS" | grep -qi 'setup_code=' && die "setup_code leaked into serve logs" || true

  wipe_scratch
  bash "$DOCKER_DIR/assert-evidence-clean.sh" "$EVIDENCE" \
    || die "evidence directory failed secret scan before PASS"

  # API acceptance passed if we got here. An explicitly requested browser claim that did not
  # pass keeps the overall result from being PASS; its own line says FAIL or BLOCKED.
  RESULT="PASS"
  if [ "$BROWSER_REQUESTED" = "1" ] && [ "$BROWSER_CLAIM" != "PASS" ]; then
    RESULT="$BROWSER_CLAIM"
  fi
  {
    echo "SERVER_ID=$SERVER_ID"
    echo "PROJECT=$PROJECT"
    echo "VOLUME=$VOLUME"
    echo "IMAGE=$IMAGE"
    echo "RESULT=$RESULT"
    echo "API_ACCEPTANCE=PASS"
    echo "NETWORK_NOTE=host-network on Linux engine; not a physical Kindle LAN proof"
    echo "IMAGE_UPGRADE=NOT_TESTED"
    echo "NOTEBOOK_BLOB_ROUNDTRIP=$NOTEBOOK_BLOB_ROUNDTRIP"
    echo "RESTART_PERSISTENCE=$RESTART_PERSISTENCE"
    echo "RECREATE_PERSISTENCE=$RECREATE_PERSISTENCE"
    echo "LEDGER_FS_MARKER=PASS"
    echo "API_CLAIM=$([ "$CLAIM_VIA" = "api" ] && echo PASS || echo NOT_TESTED)"
    echo "CLAIM_VIA=$CLAIM_VIA"
    echo "BROWSER_CLAIM=$BROWSER_CLAIM"
    echo "EVIDENCE_SCAN=PASS"
    echo "MILESTONE_A_APP_DATA=PASS"
    echo "MILESTONE_B_PHYSICAL_LAN=NOT_TESTED"
    echo "MILESTONE_C_KINDLE_E2E=NOT_TESTED"
  } >"$EVIDENCE/summary.txt"
  if [ "$RESULT" != "PASS" ]; then
    die "API acceptance passed but requested browser claim is $BROWSER_CLAIM ($BROWSER_NOTE); evidence in $EVIDENCE"
  fi
  say "PASS — evidence in $EVIDENCE"
}


# --- Multipass discovery -------------------------------------------------------------------
# Native multipass (Linux/macOS), or the Windows multipass.exe reached through WSL interop or
# Git Bash. Users never need a second Multipass inside WSL.
MP=""
MP_WINDOWS=0
VM_CLEANUP="NOT_RUN"
MP_DEFAULT_CANDIDATES="/mnt/c/Program Files/Multipass/bin/multipass.exe:/c/Program Files/Multipass/bin/multipass.exe"

resolve_multipass() {
  local c found=""
  if [ -n "${IGNITE_MULTIPASS:-}" ]; then
    [ -x "$IGNITE_MULTIPASS" ] || die "IGNITE_MULTIPASS=$IGNITE_MULTIPASS is not an executable file"
    found="$IGNITE_MULTIPASS"
  elif c="$(command -v multipass 2>/dev/null)"; then
    found="$c"
  elif c="$(command -v multipass.exe 2>/dev/null)"; then
    found="$c"
  else
    # Windows PATH may not be appended inside WSL (appendWindowsPath=false); try the
    # standard installer location. IGNITE_MULTIPASS_CANDIDATES overrides (tests use it).
    local IFS=':'
    for c in ${IGNITE_MULTIPASS_CANDIDATES-$MP_DEFAULT_CANDIDATES}; do
      [ -n "$c" ] && [ -x "$c" ] && { found="$c"; break; }
    done
  fi
  [ -n "$found" ] || die "Multipass not found.
  Install Multipass (https://multipass.run/install). On Windows the standard installer is enough:
  WSL and Git Bash use C:\\Program Files\\Multipass\\bin\\multipass.exe automatically.
  For a nonstandard location set IGNITE_MULTIPASS=/path/to/multipass[.exe].
  To use this host's Linux Docker Engine instead, run with IGNITE_DOCKER_ACCEPT=local.
  (No fallback to the host Docker Engine is attempted.)"
  MP="$found"
  case "$MP" in *.exe) MP_WINDOWS=1 ;; esac
  case "$(uname -s 2>/dev/null)" in MINGW*|MSYS*|CYGWIN*) MP_WINDOWS=1 ;; esac
  "$MP" version >/dev/null 2>&1 || die "Multipass found at '$MP' but 'multipass version' failed.
  Check that the Multipass service (multipassd) is running$([ "$MP_WINDOWS" = 1 ] && echo ' and that WSL interop is enabled')."
  say "multipass: $MP$([ "$MP_WINDOWS" = 1 ] && echo ' (Windows executable)')"
}

# MSYS_NO_PATHCONV: stop Git Bash rewriting guest paths like /home/ubuntu into C:/... args.
mp() { MSYS_NO_PATHCONV=1 "$MP" "$@"; }

# 0 = VM exists, 1 = absent, 2 = could not list (never guess).
vm_exists() {
  local out
  out="$(mp list --format csv 2>/dev/null)" || return 2
  printf '%s\n' "$out" | tr -d '\r' | cut -d, -f1 | grep -qxF "$VM"
}

# A Windows multipass.exe parses "C:\..." / "C:/..." as instance "C", so it gets a path
# relative to a cwd on a Windows drive. Linux-only filesystems (e.g. WSL ext4) are unusable:
# multipass.exe cannot read them, and it does not forward piped stdin to `exec` (a base64
# stream delivers 0 bytes and hangs), so on Windows the stage itself lives on a drive.
windows_drive_dir() {
  local w=""
  if command -v wslpath >/dev/null 2>&1; then
    w="$(wslpath -w "$1" 2>/dev/null || true)"
  elif command -v cygpath >/dev/null 2>&1; then
    w="$(cygpath -w "$1" 2>/dev/null || true)"
  fi
  case "$w" in [A-Za-z]:\\*) return 0 ;; esac
  return 1
}

MP_TRANSFER_LIMIT="${IGNITE_MULTIPASS_TRANSFER_TIMEOUT:-900}"

# Bounded `multipass transfer`: a wedged transfer must fail (and reach VM cleanup), not hang.
# Exit 124/137 = timed out. The bound is unconditional: no timeout utility, no transfer.
# Called in the caller's shell (not the subshells below) so `die` stops the run.
mp_select_timeout() {
  if command -v timeout >/dev/null 2>&1; then
    MP_TIMEOUT=timeout
  elif command -v gtimeout >/dev/null 2>&1; then
    MP_TIMEOUT=gtimeout
  else
    die "multipass transfers need a timeout utility, and neither 'timeout' nor 'gtimeout' was found.
  Install GNU coreutils (Linux/WSL: coreutils package; macOS: brew install coreutils)."
  fi
}
mp_transfer() {
  MSYS_NO_PATHCONV=1 "$MP_TIMEOUT" -k 10 "$MP_TRANSFER_LIMIT" "$MP" transfer "$@"
}

host_sha256() { (sha256sum "$1" 2>/dev/null || shasum -a 256 "$1") | awk '{print $1}'; }
guest_sha256() { mp exec "$VM" -- sha256sum "$1" 2>/dev/null | tr -d '\r' | awk '{print $1}'; }

# Upload a stage file. Windows: relative name from the drive-backed stage dir.
mp_push() {
  local src="$1" dst="$2" dir base
  dir="$(cd "$(dirname "$src")" && pwd)"
  base="$(basename "$src")"
  mp_select_timeout
  if [ "$MP_WINDOWS" = 0 ]; then
    mp_transfer "$src" "$VM:$dst" \
      || die "multipass transfer to $VM:$dst failed or exceeded ${MP_TRANSFER_LIMIT}s"
  else
    windows_drive_dir "$dir" || die "internal: Windows stage $dir is not on a Windows drive"
    (cd "$dir" && mp_transfer "$base" "$VM:$dst") \
      || die "multipass transfer to $VM:$dst failed or exceeded ${MP_TRANSFER_LIMIT}s"
  fi
  [ "$(host_sha256 "$src")" = "$(guest_sha256 "$dst")" ] \
    || die "payload integrity mismatch after transfer to $VM:$dst"
}

# Download a guest file to $STAGE/<name>, verified against the guest's sha256 before use.
# Windows multipass.exe may exit nonzero with "cannot set permissions for local file" after
# writing the complete file: accept that (with a warning) only when the hash matches.
mp_pull() {
  local src="$1" name="$2" want got rc=0 err
  mp_select_timeout
  want="$(guest_sha256 "$src")"
  [[ "$want" =~ ^[0-9a-f]{64}$ ]] || die "could not hash $VM:$src in the guest"
  rm -f "$STAGE/$name"
  err="$STAGE/$name.transfer-err"
  if [ "$MP_WINDOWS" = 0 ]; then
    mp_transfer "$VM:$src" "$STAGE/$name" 2>"$err" || rc=$?
  else
    windows_drive_dir "$STAGE" || die "internal: Windows stage $STAGE is not on a Windows drive"
    (cd "$STAGE" && mp_transfer "$VM:$src" "$name") 2>"$err" || rc=$?
  fi
  case "$rc" in
    124|137) die "multipass transfer from $VM:$src exceeded ${MP_TRANSFER_LIMIT}s" ;;
  esac
  got=""
  [ -f "$STAGE/$name" ] && got="$(host_sha256 "$STAGE/$name")"
  if [ "$got" != "$want" ]; then
    sed 's/^/  multipass: /' "$err" >&2
    die "evidence download from $VM:$src is missing, incomplete, or corrupt (transfer rc=$rc; sha256 mismatch)"
  fi
  if [ "$rc" != 0 ]; then
    echo "accept-disposable: WARNING: multipass transfer exited $rc after a complete download (sha256 verified); accepting:" >&2
    sed 's/^/  multipass: /' "$err" >&2
  fi
}

# WSL: the Windows user's %TEMP% (TEMP is not forwarded into WSL without WSLENV).
windows_temp_dir() {
  command -v wslpath >/dev/null 2>&1 || return 1
  local c w u IFS=':'
  for c in cmd.exe ${IGNITE_WINDOWS_CMD_CANDIDATES-/mnt/c/Windows/System32/cmd.exe}; do
    [ -n "$c" ] && command -v "$c" >/dev/null 2>&1 || continue
    w="$(cd / && "$c" /d /c 'echo %TEMP%' 2>/dev/null | tr -d '\r' | head -n 1)" || continue
    case "$w" in [A-Za-z]:\\*) ;; *) continue ;; esac
    u="$(wslpath -u "$w" 2>/dev/null)" || continue
    [ -d "$u" ] && [ -w "$u" ] && { printf '%s\n' "$u"; return 0; }
  done
  return 1
}

host_path_note() {
  if [ "$MP_WINDOWS" = 1 ] && command -v wslpath >/dev/null 2>&1; then
    printf ' (%s)' "$(wslpath -w "$1" 2>/dev/null)"
  fi
}

cleanup_vm() {
  if [ "$VM_OWNED" = 1 ] && [ -n "$VM" ]; then
    say "deleting disposable Multipass VM $VM"
    local i st
    # Retry: an interrupted launch can leave the instance busy for a while.
    for i in $(seq 1 "${IGNITE_MULTIPASS_DELETE_TRIES:-12}"); do
      mp delete --purge "$VM" >/dev/null 2>&1
      st=0; vm_exists || st=$?
      [ "$st" = 1 ] && break
      sleep "${IGNITE_MULTIPASS_DELETE_WAIT:-5}"
    done
    if [ "$st" = 1 ]; then
      VM_CLEANUP="PASS"
    else
      VM_CLEANUP="FAIL"
      echo "accept-disposable: ERROR: could not confirm deletion of Multipass VM $VM; remove it with: multipass delete --purge $VM" >&2
    fi
    VM_OWNED=0
    if [ -d "$EVIDENCE" ]; then
      echo "MULTIPASS_CLEANUP=$VM_CLEANUP vm=$VM" >"$EVIDENCE/host-cleanup.txt"
    fi
    say "MULTIPASS_CLEANUP=$VM_CLEANUP"
  fi
  if [ "$STAGE_OWNED" = 1 ] && [ -n "$STAGE" ]; then
    rm -rf "$STAGE" >/dev/null 2>&1
    STAGE_OWNED=0
  fi
}

run_multipass() {
  resolve_multipass

  [ -f "$BUNDLE/scribe-server" ] || die "missing Linux bundle at $BUNDLE (build it on Linux first: bash deployment/linux/build-bundle.sh)"
  [ -f "$BUNDLE/web/index.html" ] || die "missing web assets in $BUNDLE"
  [ -d "$FIXTURES_DIR" ] || die "fixtures missing: $FIXTURES_DIR"
  local t
  for t in tar; do
    command -v "$t" >/dev/null 2>&1 || die "$t is required on this host"
  done

  VM="ignite-dacc-${RUN_ID}"
  # Sanitize VM name (multipass: hostname rules)
  VM="$(echo "$VM" | tr '[:upper:]' '[:lower:]' | sed 's/[^a-z0-9-]/-/g' | cut -c1-50)"
  GUEST_ROOT="/home/ubuntu/ignite-accept"
  GUEST_EVIDENCE="$GUEST_ROOT/deployment/linux/docker/evidence-${RUN_ID}"
  # Private, fresh stage outside the repo (the checkout may not be writable from WSL, and
  # copying packaging files must not nest into itself).
  local stage_parent="${IGNITE_DOCKER_STAGE_DIR:-${TMPDIR:-/tmp}}"
  if [ "$MP_WINDOWS" = 1 ]; then
    # multipass.exe only reads files on a Windows drive (see windows_drive_dir).
    if [ -z "${IGNITE_DOCKER_STAGE_DIR:-}" ] && ! windows_drive_dir "$stage_parent"; then
      stage_parent="$(windows_temp_dir)" || stage_parent=""
    fi
    { [ -n "$stage_parent" ] && windows_drive_dir "$stage_parent"; } \
      || die "Windows multipass.exe can only transfer files that live on a Windows drive,
  and no writable drive-backed temp directory was found${stage_parent:+ ($stage_parent is not on one)}.
  Set IGNITE_DOCKER_STAGE_DIR to a writable directory on a Windows drive,
  e.g. IGNITE_DOCKER_STAGE_DIR=/mnt/c/Users/<you>/AppData/Local/Temp"
  fi
  STAGE="$(mktemp -d "$stage_parent/ignite-dacc-stage-${RUN_ID}.XXXXXX")" \
    || die "cannot create a private stage directory under $stage_parent"
  STAGE_OWNED=1
  # DrvFs may ignore modes; Windows profile ACLs keep %TEMP% private. No secrets are staged.
  chmod 700 "$STAGE" 2>/dev/null || true

  # Never adopt (or later delete) an instance this run did not create.
  local st=0
  vm_exists || st=$?
  case "$st" in
    0) die "refusing to adopt preexisting Multipass instance: $VM" ;;
    2) die "could not list Multipass instances (is multipassd running?)" ;;
  esac

  say "launching disposable Multipass VM $VM (Ubuntu 24.04)"
  VM_OWNED=1
  mp launch 24.04 -n "$VM" -c 4 -m 8G -d 40G

  # Prefer transfer over mount — Windows Multipass often has privileged mounts disabled.
  say "staging bundle + docker packaging for multipass transfer"
  mkdir -p "$STAGE/payload/deployment/linux/docker" "$STAGE/payload/dist"
  # Copy docker packaging files only (exclude evidence/context/stage dirs).
  for item in Dockerfile docker-compose.yml config.toml entrypoint.sh \
              build-image.sh init-once.sh accept-disposable.sh \
              api-notebook-blob-roundtrip.sh test-compose-static.sh \
              assert-evidence-clean.sh README.md; do
    [ -e "$DOCKER_DIR/$item" ] || continue
    cp -a "$DOCKER_DIR/$item" "$STAGE/payload/deployment/linux/docker/$item"
  done
  mkdir -p "$STAGE/payload/ignite/web/tests"
  cp -a "$FIXTURES_DIR" "$STAGE/payload/ignite/web/tests/fixtures"
  cp -a "$BUNDLE" "$STAGE/payload/dist/ignite-server-linux"
  if [ "$BROWSER_REQUESTED" = "1" ]; then
    local smoke_src="$ROOT/deployment/linux/acceptance/browser-smoke"
    local smoke_dst="$STAGE/payload/deployment/linux/acceptance/browser-smoke"
    mkdir -p "$smoke_dst"
    # Sources + lockfile only; dependencies are installed inside the guest.
    for item in "$smoke_src"/*.mjs "$smoke_src"/*.sh "$smoke_src"/package.json \
                "$smoke_src"/package-lock.json "$smoke_src"/fixtures; do
      if [ -e "$item" ]; then cp -a "$item" "$smoke_dst/"; fi
    done
  fi
  chmod +x "$STAGE/payload/dist/ignite-server-linux/scribe-server" \
    "$STAGE/payload/deployment/linux/docker/"*.sh 2>/dev/null || true
  tar -C "$STAGE/payload" -czf "$STAGE/payload.tgz" .
  mp_push "$STAGE/payload.tgz" /home/ubuntu/ignite-payload.tgz
  mp exec "$VM" -- bash -lc "
    set -euo pipefail
    rm -rf '$GUEST_ROOT'
    mkdir -p '$GUEST_ROOT'
    tar -C '$GUEST_ROOT' -xzf /home/ubuntu/ignite-payload.tgz
    chmod +x '$GUEST_ROOT'/dist/ignite-server-linux/scribe-server \
      '$GUEST_ROOT'/deployment/linux/docker/*.sh
  "

  say "installing Docker Engine inside disposable guest (not on this host)"
  # Multipass guests often boot with skewed clocks; apt then rejects InRelease as "not valid yet".
  HOST_UTC="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  mp exec "$VM" -- sudo date -u -s "$HOST_UTC" >/dev/null 2>&1 || true
  mp exec "$VM" -- bash -lc '
    set -euo pipefail
    export DEBIAN_FRONTEND=noninteractive
    sudo timedatectl set-ntp true >/dev/null 2>&1 || true
    sleep 2
    sudo apt-get update -qq
    sudo apt-get install -y -qq ca-certificates curl gnupg python3
    sudo install -m 0755 -d /etc/apt/keyrings
    curl -fsSL https://download.docker.com/linux/ubuntu/gpg | sudo gpg --dearmor -o /etc/apt/keyrings/docker.gpg
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo $VERSION_CODENAME) stable" | sudo tee /etc/apt/sources.list.d/docker.list >/dev/null
    sudo apt-get update -qq
    sudo apt-get install -y -qq docker-ce docker-ce-cli containerd.io docker-compose-plugin
    sudo usermod -aG docker ubuntu || true
    sudo systemctl enable --now docker
    # Disposable guest only: allow non-root docker without re-login.
    sudo chmod 666 /var/run/docker.sock
  '

  if [ "$BROWSER_REQUESTED" = "1" ]; then
    say "installing Node + Playwright Chromium inside disposable guest (browser claim requested)"
    # A failed install leaves the guest without playwright; the guest run then reports BLOCKED.
    mp exec "$VM" -- sudo env PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright \
      bash "$GUEST_ROOT/deployment/linux/acceptance/browser-smoke/install-browser-smoke.sh" \
      "$GUEST_ROOT/deployment/linux/acceptance/browser-smoke" \
      || say "browser dependency install failed in guest; browser claim will be BLOCKED"
    mp exec "$VM" -- sudo bash -c 'chmod -R a+rX /opt/ms-playwright 2>/dev/null || true'
  fi

  say "running acceptance inside guest (virtual LAN ≠ physical Kindle proof)"
  set +e
  mp exec "$VM" -- bash -lc "
    set -euo pipefail
    cd '$GUEST_ROOT'
    export IGNITE_DOCKER_ACCEPT=local
    export IGNITE_DOCKER_EVIDENCE='$GUEST_EVIDENCE'
    export IGNITE_DOCKER_BUNDLE='$GUEST_ROOT/dist/ignite-server-linux'
    export IGNITE_DOCKER_BUILD_BUNDLE=0
    export IGNITE_DOCKER_BROWSER_SMOKE='$BROWSER_REQUESTED'
    export PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright
    export IGNITE_DOCKER_TEST_ROTATION=1
    bash deployment/linux/docker/accept-disposable.sh
  "
  GUEST_RC=$?
  set -e

  say "retrieving sanitized evidence from guest (rc=$GUEST_RC)"
  mp exec "$VM" -- bash -lc "
    set -euo pipefail
    if [ -d '$GUEST_EVIDENCE' ]; then
      tar -C '$(dirname "$GUEST_EVIDENCE")' -czf /home/ubuntu/evidence.tgz '$(basename "$GUEST_EVIDENCE")'
    else
      mkdir -p /tmp/empty-ev && tar -C /tmp -czf /home/ubuntu/evidence.tgz empty-ev
    fi
  " || die "could not package evidence inside the guest (guest rc=$GUEST_RC)"
  # Native transfer (no base64 over exec stdio: Windows multipass.exe wedges on it), verified
  # by sha256 before extraction.
  mp_pull /home/ubuntu/evidence.tgz evidence.tgz
  mkdir -p "$STAGE/evidence"
  tar -C "$STAGE/evidence" --no-same-owner -xzf "$STAGE/evidence.tgz" \
    || die "could not extract the retrieved evidence archive"
  if [ -d "$STAGE/evidence/evidence-${RUN_ID}" ]; then
    mkdir -p "$EVIDENCE"
    chmod 700 "$EVIDENCE" 2>/dev/null || true
    cp -R "$STAGE/evidence/evidence-${RUN_ID}/." "$EVIDENCE/"
    # Defense in depth: the guest scans only on its PASS path; scan everything retrieved.
    bash "$DOCKER_DIR/assert-evidence-clean.sh" "$EVIDENCE" \
      || die "retrieved evidence failed the secret scan: $EVIDENCE"
  elif [ "$GUEST_RC" -eq 0 ]; then
    die "retrieved evidence archive has no evidence-${RUN_ID} directory"
  fi

  if [ -f "$EVIDENCE/summary.txt" ]; then
    say "guest summary:"
    sed 's/^/  /' "$EVIDENCE/summary.txt"
  fi
  [ "$GUEST_RC" -eq 0 ] || die "guest acceptance failed (rc=$GUEST_RC); evidence under $EVIDENCE$(host_path_note "$EVIDENCE") if present"
  [ -f "$EVIDENCE/summary.txt" ] || die "missing evidence summary from guest run"
  grep -qx 'RESULT=PASS' "$EVIDENCE/summary.txt" || die "guest acceptance did not PASS"
  say "PASS — evidence on host at $EVIDENCE$(host_path_note "$EVIDENCE")"
  say "Multipass guest will be purged on exit. Virtual networking ≠ physical Kindle LAN."
}

case "$MODE" in
  local) run_local ;;
  multipass) run_multipass ;;
esac
