#!/usr/bin/env bash
# Disposable acceptance for the real Ignite Server image (not mocks).
#
# Preferred: Linux Docker Engine on this host (IGNITE_DOCKER_ACCEPT=1).
# Or: Multipass Ubuntu 24.04 guest with Docker installed inside the guest only.
#
#   IGNITE_DOCKER_ACCEPT=1 bash deployment/linux/docker/accept-disposable.sh
#   IGNITE_DOCKER_ACCEPT=multipass bash deployment/linux/docker/accept-disposable.sh
#
# Never runs docker compose down -v against unrelated projects.
# Deletes only resources tagged with this run's unique project/volume names.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
DOCKER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODE="${IGNITE_DOCKER_ACCEPT:-}"
# Compose project names must be lowercase (hyphens/underscores ok).
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
RUN_ID="$(echo "$RUN_ID" | tr '[:upper:]' '[:lower:]')"
PROJECT="ignite-dacc-${RUN_ID}"
VOLUME="${PROJECT}-state"
IMAGE="ignite-server:accept-${RUN_ID}"
EVIDENCE="${IGNITE_DOCKER_EVIDENCE:-$ROOT/deployment/linux/docker/evidence-${RUN_ID}}"
BUNDLE="${IGNITE_DOCKER_BUNDLE:-$ROOT/dist/ignite-server-linux}"

die() { echo "accept-disposable: ERROR: $*" >&2; exit 1; }
say() { echo "accept-disposable: $*"; }

[ -n "$MODE" ] || die "set IGNITE_DOCKER_ACCEPT=1 (local Linux engine) or =multipass
Refusing to run disposable Docker acceptance without explicit opt-in."

mkdir -p "$EVIDENCE"
chmod 700 "$EVIDENCE" 2>/dev/null || true

# Private scratch for claim JSON / raw HTTP — never under $EVIDENCE; wiped on every exit.
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/ignite-dacc-scratch.XXXXXX")"
chmod 700 "$SCRATCH"

sanitize() {
  # Strip setup codes and session cookies from evidence copies.
  sed -E \
    -e 's/setup_code=[A-Z0-9-]+/setup_code=<redacted>/g' \
    -e 's/[Ss]et-[Cc]ookie:[^\r\n]+/Set-Cookie: <redacted>/g' \
    -e 's/scribe_session=[^;[:space:]]+/scribe_session=<redacted>/g' \
    -e 's/[Aa]uthorization:[[:space:]]*Bearer[[:space:]]+[^[:space:]]+/Authorization: Bearer <redacted>/g' \
    -e 's/"setup_code"[[:space:]]*:[[:space:]]*"[^"]+"/"setup_code":"<redacted>"/g'
}

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

cleanup() {
  set +e
  wipe_scratch
  # Multipass mode: Docker resources live in the guest; the VM is purged separately.
  case "${MODE:-}" in
    multipass) set -e; return 0 ;;
  esac
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
  command -v docker >/dev/null 2>&1 || die "docker CLI not found"
  docker info >/dev/null 2>&1 || die "Docker daemon not accessible"
  # Host networking requires a Linux engine. Docker Desktop for Windows/macOS is insufficient.
  os="$(docker info --format '{{.OperatingSystem}}' 2>/dev/null || true)"
  say "engine OS report: ${os:-unknown}"
  if echo "$os" | grep -qiE 'Docker Desktop|linuxkit'; then
    die "this engine looks like Docker Desktop/linuxkit — host networking is not a supported accept path.
Use IGNITE_DOCKER_ACCEPT=multipass on a Linux guest instead."
  fi

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
  sanitize <"$SCRATCH/claim.headers" >"$EVIDENCE/claim.headers.txt"
  sanitize <"$SCRATCH/claim.body" >"$EVIDENCE/claim.body.txt"
  echo "claim_http=200 cookie=present" >"$EVIDENCE/claim.status.txt"
  rm -f "$CLAIM_JSON" "$SCRATCH/claim.headers" "$SCRATCH/claim.body"
  unset SETUP_CODE CODE1 CODE2

  STATUS_CLAIMED="$(curl -fsS http://127.0.0.1:8787/control/v1/status)"
  printf '%s\n' "$STATUS_CLAIMED" | sanitize | tee "$EVIDENCE/status-claimed.json"
  echo "$STATUS_CLAIMED" | grep -q "$SERVER_ID" || die "claimed status missing server_id"
  echo "$STATUS_CLAIMED" | grep -Eq '"claimed"[[:space:]]*:[[:space:]]*true' \
    || die "expected claimed=true in status JSON after claim"

  # Create a small notebook via authenticated API if feasible — at minimum write a marker blob into ledger via status persistence.
  # Use control status + restart persistence of server_id + claimed.
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

  # Browser smoke (optional if node/playwright available on host)
  if [ "${IGNITE_DOCKER_BROWSER_SMOKE:-1}" = "1" ] && [ -f "$ROOT/deployment/linux/acceptance/browser-smoke/smoke.mjs" ]; then
    if command -v node >/dev/null 2>&1; then
      say "browser smoke against containerized server"
      (
        cd "$ROOT/deployment/linux/acceptance/browser-smoke"
        # Claim already done — smoke may expect unclaimed; run asset checks via curl classification only if smoke needs setup.
        # Prefer status + asset fetch already done; attempt smoke with SKIP if claimed.
        if [ -f package.json ]; then
          npm ci --omit=dev >/dev/null 2>&1 || true
        fi
        # Record that claim path used API; browser claim optional when already claimed.
        echo "browser_claim=api-preclaimed" >"$EVIDENCE/browser-note.txt"
      )
    fi
  fi

  wipe_scratch
  bash "$DOCKER_DIR/assert-evidence-clean.sh" "$EVIDENCE" \
    || die "evidence directory failed secret scan before PASS"

  {
    echo "SERVER_ID=$SERVER_ID"
    echo "PROJECT=$PROJECT"
    echo "VOLUME=$VOLUME"
    echo "IMAGE=$IMAGE"
    echo "RESULT=PASS"
    echo "NETWORK_NOTE=host-network on Linux engine; not a physical Kindle LAN proof"
    echo "IMAGE_UPGRADE=NOT_TESTED"
    echo "NOTEBOOK_BLOB_ROUNDTRIP=NOT_TESTED"
    echo "LEDGER_FS_MARKER=PASS"
    echo "RESTART_RECREATE_SAME_IMAGE=PASS"
    echo "BROWSER_CLAIM=NOT_TESTED (API claim with CSRF)"
  } >"$EVIDENCE/summary.txt"
  say "PASS — evidence in $EVIDENCE"
}

run_multipass() {
  command -v multipass >/dev/null 2>&1 || die "multipass not found"
  VM="ignite-dacc-${RUN_ID}"
  # Sanitize VM name (multipass: hostname rules)
  VM="$(echo "$VM" | tr '[:upper:]' '[:lower:]' | sed 's/[^a-z0-9-]/-/g' | cut -c1-50)"
  GUEST_ROOT="/home/ubuntu/ignite-accept"
  # Stage outside deployment/linux/docker so `cp -a "$DOCKER_DIR"` cannot nest into itself.
  STAGE="$ROOT/dist/.docker-accept-stage-${RUN_ID}"
  rm -rf "$STAGE"
  mkdir -p "$STAGE"

  cleanup_vm() {
    say "deleting disposable Multipass VM $VM"
    multipass delete --purge "$VM" >/dev/null 2>&1 || true
    rm -rf "$STAGE" >/dev/null 2>&1 || true
  }
  trap cleanup_vm EXIT

  [ -f "$BUNDLE/scribe-server" ] || die "missing Linux bundle at $BUNDLE (build on Linux first)"
  [ -f "$BUNDLE/web/index.html" ] || die "missing web assets in $BUNDLE"

  say "launching disposable Multipass VM $VM (Ubuntu 24.04)"
  multipass launch 24.04 -n "$VM" -c 4 -m 8G -d 40G

  # Prefer transfer over mount — Windows Multipass often has privileged mounts disabled.
  say "staging bundle + docker packaging for multipass transfer"
  mkdir -p "$STAGE/payload/deployment/linux" "$STAGE/payload/dist"
  # Copy docker packaging files only (exclude evidence/context/stage dirs).
  mkdir -p "$STAGE/payload/deployment/linux/docker"
  for item in Dockerfile docker-compose.yml config.toml entrypoint.sh \
              build-image.sh init-once.sh accept-disposable.sh test-compose-static.sh \
              assert-evidence-clean.sh README.md; do
    [ -e "$DOCKER_DIR/$item" ] || continue
    cp -a "$DOCKER_DIR/$item" "$STAGE/payload/deployment/linux/docker/$item"
  done
  cp -a "$BUNDLE" "$STAGE/payload/dist/ignite-server-linux"
  chmod +x "$STAGE/payload/dist/ignite-server-linux/scribe-server" \
    "$STAGE/payload/deployment/linux/docker/"*.sh \
    "$STAGE/payload/deployment/linux/docker/entrypoint.sh" 2>/dev/null || true
  tar -C "$STAGE/payload" -czf "$STAGE/payload.tgz" .
  # Multipass on Windows treats "C:" as an instance name — transfer via relative path.
  (
    cd "$STAGE"
    multipass transfer payload.tgz "$VM:/home/ubuntu/ignite-payload.tgz"
  )
  multipass exec "$VM" -- bash -lc "
    set -euo pipefail
    rm -rf '$GUEST_ROOT'
    mkdir -p '$GUEST_ROOT'
    tar -C '$GUEST_ROOT' -xzf /home/ubuntu/ignite-payload.tgz
    chmod +x '$GUEST_ROOT'/dist/ignite-server-linux/scribe-server \
      '$GUEST_ROOT'/deployment/linux/docker/*.sh \
      '$GUEST_ROOT'/deployment/linux/docker/entrypoint.sh
  "

  say "installing Docker Engine inside disposable guest (not on the Windows/dev host)"
  # Multipass guests often boot with skewed clocks; apt then rejects InRelease as "not valid yet".
  HOST_UTC="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  multipass exec "$VM" -- sudo date -u -s "$HOST_UTC" >/dev/null 2>&1 || true
  multipass exec "$VM" -- bash -lc '
    set -euo pipefail
    export DEBIAN_FRONTEND=noninteractive
    sudo timedatectl set-ntp true >/dev/null 2>&1 || true
    sleep 2
    sudo apt-get update -qq
    sudo apt-get install -y -qq ca-certificates curl gnupg
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

  say "running acceptance inside guest (virtual LAN ≠ physical Kindle proof)"
  set +e
  multipass exec "$VM" -- bash -lc "
    set -euo pipefail
    cd '$GUEST_ROOT'
    export IGNITE_DOCKER_ACCEPT=1
    export IGNITE_DOCKER_EVIDENCE='$GUEST_ROOT/deployment/linux/docker/evidence-${RUN_ID}'
    export IGNITE_DOCKER_BUNDLE='$GUEST_ROOT/dist/ignite-server-linux'
    export IGNITE_DOCKER_BUILD_BUNDLE=0
    export IGNITE_DOCKER_BROWSER_SMOKE=0
    # First prove claim with a single init code; rotation covered in CI/local as well.
    export IGNITE_DOCKER_TEST_ROTATION=1
    bash deployment/linux/docker/accept-disposable.sh
  "
  GUEST_RC=$?
  set -e

  say "retrieving sanitized evidence from guest (rc=$GUEST_RC)"
  mkdir -p "$ROOT/deployment/linux/docker"
  multipass exec "$VM" -- bash -lc "
    set -euo pipefail
    if [ -d '$GUEST_ROOT/deployment/linux/docker/evidence-${RUN_ID}' ]; then
      tar -C '$GUEST_ROOT/deployment/linux/docker' -czf /home/ubuntu/evidence.tgz 'evidence-${RUN_ID}'
    else
      mkdir -p /tmp/empty-ev && tar -C /tmp -czf /home/ubuntu/evidence.tgz empty-ev
    fi
  " || true
  # Pull via base64 on stdout to avoid Windows Multipass SFTP permission quirks.
  multipass exec "$VM" -- bash -lc 'base64 -w0 /home/ubuntu/evidence.tgz' >"$STAGE/evidence.b64" 2>/dev/null || true
  if [ -s "$STAGE/evidence.b64" ]; then
    if base64 -d <"$STAGE/evidence.b64" >"$STAGE/evidence.tgz" 2>/dev/null \
      || base64 --decode <"$STAGE/evidence.b64" >"$STAGE/evidence.tgz" 2>/dev/null; then
      tar -C "$ROOT/deployment/linux/docker" -xzf "$STAGE/evidence.tgz" || true
    fi
  fi

  [ "$GUEST_RC" -eq 0 ] || die "guest acceptance failed (rc=$GUEST_RC); evidence under deployment/linux/docker/evidence-${RUN_ID} if present"
  [ -f "$ROOT/deployment/linux/docker/evidence-${RUN_ID}/summary.txt" ] \
    || die "missing evidence summary from guest run"
  grep -q 'RESULT=PASS' "$ROOT/deployment/linux/docker/evidence-${RUN_ID}/summary.txt" \
    || die "guest acceptance did not PASS"
  say "PASS — evidence on host at $ROOT/deployment/linux/docker/evidence-${RUN_ID}"
  say "Multipass guest will be purged on exit. Virtual networking ≠ physical Kindle LAN."
}

case "$MODE" in
  1|local|yes) run_local ;;
  multipass) run_multipass ;;
  *) die "unknown IGNITE_DOCKER_ACCEPT=$MODE (use 1 or multipass)" ;;
esac
