#!/usr/bin/env bash
# Focused safety regressions for accept-disposable ownership + evidence secrets.
# Mock Docker only — never touches a real engine or customer volumes.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/ignite-accept-safe.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

# --- 1) assert-evidence-clean catches secrets ---
POISON="$TMP/poison-evidence"
mkdir -p "$POISON"
echo 'setup_code=K7M4-P2Q9' >"$POISON/leak.txt"
set +e
bash "$DIR/assert-evidence-clean.sh" "$POISON" >/dev/null 2>&1
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "assert-evidence-clean should fail on setup_code"
pass "assert-evidence-clean rejects setup_code in evidence"

CLEAN="$TMP/clean-evidence"
mkdir -p "$CLEAN"
echo 'setup_code=<redacted>' >"$CLEAN/ok.txt"
echo 'claim_http=200 cookie=present' >"$CLEAN/claim.status.txt"
bash "$DIR/assert-evidence-clean.sh" "$CLEAN" >/dev/null
pass "assert-evidence-clean accepts redacted evidence"

# --- 2) Failure after auth material: scratch wiped; evidence has no secrets ---
SCRATCH="$TMP/scratch"
EVIDENCE="$TMP/evidence-fail"
mkdir -p "$SCRATCH" "$EVIDENCE"
chmod 700 "$SCRATCH" "$EVIDENCE"
printf '{"setup_code":"K7M4-P2Q9","display_name":"x"}\n' >"$SCRATCH/claim.json"
cat >"$SCRATCH/claim.headers" <<'EOF'
HTTP/1.1 200 OK
Set-Cookie: scribe_session=SUPERSECRETVALUE; HttpOnly; Path=/
EOF
echo '{"role":"admin"}' >"$SCRATCH/claim.body"
# Sanitized copies only (what accept writes on success/fail diagnostics).
sed -E \
  -e 's/setup_code=[A-Z0-9-]+/setup_code=<redacted>/g' \
  -e 's/[Ss]et-[Cc]ookie:[^\r\n]+/Set-Cookie: <redacted>/g' \
  -e 's/scribe_session=[^;[:space:]]+/scribe_session=<redacted>/g' \
  <"$SCRATCH/claim.headers" >"$EVIDENCE/claim.headers.txt"
echo 'claim_http=200 cookie=present' >"$EVIDENCE/claim.status.txt"
# Simulate deliberate failure AFTER receiving auth material.
rm -f "$SCRATCH"/*
rmdir "$SCRATCH" 2>/dev/null || rm -rf "$SCRATCH"
[ ! -e "$TMP/scratch/claim.json" ] || fail "scratch claim.json survived failure cleanup"
[ ! -d "$SCRATCH" ] || fail "scratch dir survived failure cleanup"
bash "$DIR/assert-evidence-clean.sh" "$EVIDENCE" >/dev/null \
  || fail "evidence after failed-claim path must be clean"
# Ensure raw cookies never landed in evidence
if grep -RIn 'SUPERSECRETVALUE\|K7M4-P2Q9' "$EVIDENCE" >/dev/null 2>&1; then
  fail "raw secret found in evidence after failure simulation"
fi
pass "failure after auth material leaves no secrets in evidence"

# --- 3) Ownership: preexisting volume not adopted/removed ---
mkdir -p "$TMP/bin" "$TMP/state/volumes" "$TMP/log"
: >"$TMP/log/volume.rm"
cat >"$TMP/bin/docker" <<EOF
#!/usr/bin/env bash
set -euo pipefail
STATE="$TMP/state/volumes"
RMLOG="$TMP/log/volume.rm"
cmd="\${1:-}"; shift || true
case "\$cmd" in
  volume)
    sub="\${1:-}"; shift || true
    case "\$sub" in
      inspect)
        name="\${1:-}"
        [ -f "\$STATE/\$name" ] && exit 0
        exit 1
        ;;
      create)
        name="\${1:-}"
        if [ -f "\$STATE/\$name" ]; then
          echo "Error: volume already exists" >&2
          exit 1
        fi
        echo created >"\$STATE/\$name"
        exit 0
        ;;
      rm)
        name="\${1:-}"
        echo "\$name" >>"\$RMLOG"
        rm -f "\$STATE/\$name"
        exit 0
        ;;
      *) exit 2 ;;
    esac
    ;;
  *) exit 2 ;;
esac
EOF
chmod +x "$TMP/bin/docker"
export PATH="$TMP/bin:$PATH"

# Preexisting foreign volume
echo preexisting >"$TMP/state/volumes/keep-me-preexisting"
echo preexisting >"$TMP/state/volumes/ignite-dacc-collision-state"

# refuse_if_exists / create_owned_volume logic (inline copy of accept contract)
refuse_if_exists_volume() {
  local name="$1"
  if docker volume inspect "$name" >/dev/null 2>&1; then
    return 1
  fi
  return 0
}
OWNED=()
create_owned() {
  local name="$1"
  refuse_if_exists_volume "$name" || return 2
  docker volume create "$name"
  OWNED+=("$name")
}

set +e
create_owned "ignite-dacc-collision-state"
crc=$?
set -e
[ "$crc" -eq 2 ] || fail "expected refuse on preexisting colliding volume name"
[ -f "$TMP/state/volumes/keep-me-preexisting" ] || fail "preexisting keep-me removed"
[ -f "$TMP/state/volumes/ignite-dacc-collision-state" ] || fail "collision volume removed on refuse"

# Successful create + cleanup only owned
create_owned "ignite-dacc-owned-only" || fail "create owned failed"
cleanup_owned() {
  local v
  for v in "${OWNED[@]+"${OWNED[@]}"}"; do
    docker volume rm "$v"
  done
}
cleanup_owned
[ ! -f "$TMP/state/volumes/ignite-dacc-owned-only" ] || fail "owned volume not removed"
[ -f "$TMP/state/volumes/keep-me-preexisting" ] || fail "cleanup removed unrelated volume"
[ -f "$TMP/state/volumes/ignite-dacc-collision-state" ] || fail "cleanup removed collision volume"
if grep -qx 'keep-me-preexisting' "$TMP/log/volume.rm" 2>/dev/null; then
  fail "rm log touched keep-me-preexisting"
fi
if grep -qx 'ignite-dacc-collision-state' "$TMP/log/volume.rm" 2>/dev/null; then
  fail "rm log touched collision volume that was never owned"
fi
pass "preexisting volumes never adopted or removed by ownership cleanup"

# --- 4) Compose is image-only (no build context dependency) ---
grep -Eq '^[[:space:]]*build:' "$DIR/docker-compose.yml" \
  && fail "runtime compose must not contain build:" || true
grep -qF 'image: ${IGNITE_IMAGE:-ghcr.io/markrai/ignite-server:latest}' "$DIR/docker-compose.yml" \
  || fail "compose missing the GHCR image"
grep -q 'markrai/ignite' "$DIR/Dockerfile" || fail "OCI source label must be markrai/ignite"
pass "compose is image-only; OCI source label corrected"

# --- Secret scanner must fail without printing credentials ---
LEAK_DIR="$TMP/secret-output-check"
mkdir -p "$LEAK_DIR"

printf '%s\n' \
  'setup_code=K7M4-P2Q9' \
  'Set-Cookie: scribe_session=SUPERSECRETVALUE; HttpOnly' \
  > "$LEAK_DIR/secrets.txt"

SCAN_OUTPUT="$TMP/secret-scan-output.txt"

if bash "$DIR/assert-evidence-clean.sh" "$LEAK_DIR" >"$SCAN_OUTPUT" 2>&1; then
  fail "scanner accepted secret-bearing evidence"
fi

if grep -Eq 'K7M4-P2Q9|SUPERSECRETVALUE' "$SCAN_OUTPUT"; then
  fail "scanner leaked credentials into its output"
fi

pass "scanner fails without echoing secrets"

# --- Launcher (mode/Multipass/cleanup) and round-trip secret handling ---
bash "$DIR/test-accept-launcher.sh"
bash "$DIR/test-roundtrip-secrets.sh"

echo "PASS: accept-safety regressions"
exit 0
