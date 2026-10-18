#!/usr/bin/env bash
# Launcher regressions for accept-disposable.sh: mode selection, Multipass discovery,
# no fallback to the host Docker Engine, and ownership-aware VM cleanup.
# Fake multipass/docker only — never touches a real VM, engine, or volume.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
ACCEPT="$DIR/accept-disposable.sh"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/ignite-accept-launcher.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

command -v python3 >/dev/null 2>&1 || fail "python3 required"

# Fake bundle (never executed; Multipass mode only transfers it).
BUNDLE="$TMP/bundle"
mkdir -p "$BUNDLE/web"
echo '#!/bin/sh' >"$BUNDLE/scribe-server"
echo '<html></html>' >"$BUNDLE/web/index.html"

# --- fakes ------------------------------------------------------------------------------
BIN="$TMP/bin"
mkdir -p "$BIN"
cat >"$BIN/fake-multipass" <<'EOF'
#!/usr/bin/env bash
S="$FAKE_MP_STATE"
mkdir -p "$S/vms"
echo "$PWD|$*" >>"$S/calls.log"
cmd="${1:-}"; shift || true
case "$cmd" in
  version) echo "multipass 0.0.0-fake"; exit 0 ;;
  list)
    [ "${FAKE_MP_LIST_RC:-0}" = 0 ] || exit "$FAKE_MP_LIST_RC"
    echo "Name,State,IPv4,IPv6,Release,AllIPv4"
    for f in "$S"/vms/*; do [ -e "$f" ] && echo "$(basename "$f"),Running,,,Ubuntu 24.04 LTS,"; done
    exit 0 ;;
  launch)
    name=""
    while [ $# -gt 0 ]; do [ "$1" = "-n" ] && name="$2"; shift; done
    touch "$S/vms/$name"
    exit "${FAKE_MP_LAUNCH_RC:-0}" ;;
  transfer) exit 0 ;;
  delete)
    name="${*: -1}"
    echo "$name" >>"$S/deletes.log"
    rm -f "$S/vms/$name"
    exit 0 ;;
  exec)
    name="$1"; shift; [ "${1:-}" = "--" ] && shift
    all="$*"
    case "$all" in
      *"base64 -d"*) base64 -d >"$S/pushed.bin"; exit 0 ;;
      "sha256sum "*)
        # Fake guest file: streamed bytes, or (transfer path) the host stage payload.
        f="$S/pushed.bin"
        [ -s "$f" ] || f="$(ls -d "$IGNITE_DOCKER_STAGE_DIR"/ignite-dacc-stage-*/payload.tgz | head -1)"
        sha256sum "$f" | awk -v d="$2" '{print $1"  "d}'
        exit 0 ;;
      *accept-disposable.sh*)
        echo guest-run >>"$S/guest.log"
        if [ -n "${FAKE_MP_GUEST_SLEEP:-}" ]; then
          for _ in $(seq 1 "$FAKE_MP_GUEST_SLEEP"); do sleep 1; done
        fi
        exit "${FAKE_MP_GUEST_RC:-0}" ;;
      *"base64 -w0"*)
        t="$(mktemp -d)"
        mkdir -p "$t/evidence-$IGNITE_DOCKER_RUN_ID"
        printf 'RESULT=%s\nNOTEBOOK_BLOB_ROUNDTRIP=PASS\n' "${FAKE_MP_RESULT:-PASS}" \
          >"$t/evidence-$IGNITE_DOCKER_RUN_ID/summary.txt"
        [ -n "${FAKE_MP_LEAK:-}" ] && echo "Authorization: Bearer leaked-token-value" \
          >"$t/evidence-$IGNITE_DOCKER_RUN_ID/leak.txt"
        tar -C "$t" -czf "$t/e.tgz" "evidence-$IGNITE_DOCKER_RUN_ID"
        base64 -w0 "$t/e.tgz"
        rm -rf "$t"
        exit 0 ;;
    esac
    exit 0 ;;
esac
exit 0
EOF
cat >"$BIN/docker" <<'EOF'
#!/usr/bin/env bash
echo "$*" >>"$FAKE_DOCKER_LOG"
[ "${1:-}" = "info" ] && exit "${FAKE_DOCKER_INFO_RC:-1}"
exit 0
EOF
chmod +x "$BIN/fake-multipass" "$BIN/docker"

# Hermetic PATH: no real multipass/multipass.exe/docker may leak in from the host.
SYS_PATH="/usr/bin:/bin"
export FAKE_DOCKER_LOG="$TMP/docker.log"

n=0
new_case() {
  n=$((n + 1))
  CASE="$TMP/case$n"
  mkdir -p "$CASE/state/vms" "$CASE/mpbin" "$CASE/stage"
  : >"$FAKE_DOCKER_LOG"
  export FAKE_MP_STATE="$CASE/state"
  export IGNITE_DOCKER_RUN_ID="t${n}-$$"
  export IGNITE_DOCKER_EVIDENCE="$CASE/evidence"
  export IGNITE_DOCKER_STAGE_DIR="$CASE/stage"
  export IGNITE_DOCKER_BUNDLE="$BUNDLE"
  export IGNITE_MULTIPASS_DELETE_WAIT=0
  export IGNITE_MULTIPASS_DELETE_TRIES=3
  unset IGNITE_DOCKER_ACCEPT IGNITE_MULTIPASS FAKE_MP_GUEST_RC FAKE_MP_GUEST_SLEEP \
    FAKE_MP_RESULT FAKE_MP_LIST_RC FAKE_MP_LEAK IGNITE_MULTIPASS_CANDIDATES
  # Pre-existing, unrelated instances that must never be touched.
  touch "$CASE/state/vms/primary" "$CASE/state/vms/ignite-dacc-someone-else"
  # A stage dir from some other run must survive this run's cleanup.
  mkdir -p "$CASE/stage/ignite-dacc-stage-foreign"
}
vm_name() { echo "ignite-dacc-$IGNITE_DOCKER_RUN_ID"; }
assert_foreign_vms_intact() {
  [ -e "$CASE/state/vms/primary" ] && [ -e "$CASE/state/vms/ignite-dacc-someone-else" ] \
    || fail "$1: a preexisting instance was deleted"
  if grep -qxE 'primary|ignite-dacc-someone-else' "$CASE/state/deletes.log" 2>/dev/null; then
    fail "$1: delete was called on a preexisting instance"
  fi
}
run_accept() { # run_accept <PATH> ; captures rc/out
  set +e
  OUT="$(PATH="$1" bash "$ACCEPT" 2>&1)"
  RC=$?
  set -e
}
use_native_multipass() {
  cp "$BIN/fake-multipass" "$CASE/mpbin/multipass"
  MP_PATH="$CASE/mpbin:$BIN:$SYS_PATH"
}

# --- 1) Default mode is Multipass; missing Multipass is one actionable error; no fallback ---
new_case
export IGNITE_MULTIPASS_CANDIDATES=""
run_accept "$BIN:$SYS_PATH"
[ "$RC" -ne 0 ] || fail "missing multipass should fail"
echo "$OUT" | grep -q 'Multipass not found' || fail "missing multipass: no actionable error: $OUT"
echo "$OUT" | grep -q 'IGNITE_DOCKER_ACCEPT=local' || fail "missing multipass: error should name the explicit local mode"
[ ! -s "$FAKE_DOCKER_LOG" ] || fail "missing multipass fell back to host docker: $(cat "$FAKE_DOCKER_LOG")"
[ ! -e "$IGNITE_DOCKER_EVIDENCE" ] || fail "missing multipass left an empty evidence directory"
[ "$(echo "$OUT" | grep -c 'ERROR')" -eq 1 ] || fail "expected exactly one error line block"
pass "default mode selects Multipass; missing Multipass → one actionable error, no Docker fallback"

# --- 2) Explicit local mode uses the host engine path and never Multipass ---
new_case
use_native_multipass
export IGNITE_DOCKER_ACCEPT=local
run_accept "$MP_PATH"
[ "$RC" -ne 0 ] || fail "local mode with unavailable daemon should fail"
echo "$OUT" | grep -q 'Docker daemon not accessible' || fail "local mode did not take the Docker path: $OUT"
[ ! -s "$CASE/state/calls.log" ] || fail "local mode invoked multipass"
grep -qx 'info' "$FAKE_DOCKER_LOG" || fail "local mode did not probe docker"
export IGNITE_DOCKER_ACCEPT=1
run_accept "$MP_PATH"
echo "$OUT" | grep -q 'Docker daemon not accessible' || fail "IGNITE_DOCKER_ACCEPT=1 (CI) no longer maps to local"
export IGNITE_DOCKER_ACCEPT=bogus
run_accept "$MP_PATH"
[ "$RC" -ne 0 ] && echo "$OUT" | grep -q 'unknown IGNITE_DOCKER_ACCEPT' || fail "unknown mode not rejected"
pass "explicit local mode (local/1) stays available and never touches Multipass"

# --- 3) Success: owned VM launched, evidence retrieved, VM purged, others untouched ---
new_case
use_native_multipass
run_accept "$MP_PATH"
[ "$RC" -eq 0 ] || fail "fake multipass success run failed: $OUT"
grep -q "^.*|launch 24.04 -n $(vm_name) " "$CASE/state/calls.log" || fail "VM not launched with run name"
grep -qx "$(vm_name)" "$CASE/state/deletes.log" || fail "owned VM not deleted after success"
[ ! -e "$CASE/state/vms/$(vm_name)" ] || fail "owned VM still exists after success"
grep -q 'RESULT=PASS' "$IGNITE_DOCKER_EVIDENCE/summary.txt" || fail "evidence summary not retrieved"
grep -q 'MULTIPASS_CLEANUP=PASS' "$IGNITE_DOCKER_EVIDENCE/host-cleanup.txt" || fail "cleanup result not recorded"
[ "$(ls -A "$CASE/stage")" = "ignite-dacc-stage-foreign" ] || fail "own stage not removed, or foreign stage touched: $(ls -A "$CASE/stage")"
[ ! -s "$FAKE_DOCKER_LOG" ] || fail "multipass mode touched host docker"
assert_foreign_vms_intact "success"
pass "success path: owned VM purged, evidence retrieved, stage removed, foreign VMs intact"

# --- 4) Guest failure: VM still purged, nonzero exit, never PASS ---
new_case
use_native_multipass
export FAKE_MP_GUEST_RC=7 FAKE_MP_RESULT=FAIL
run_accept "$MP_PATH"
[ "$RC" -ne 0 ] || fail "guest failure reported success"
echo "$OUT" | grep -q 'guest acceptance failed (rc=7)' || fail "guest failure not reported: $OUT"
grep -qx "$(vm_name)" "$CASE/state/deletes.log" || fail "owned VM not deleted after failure"
assert_foreign_vms_intact "failure"
pass "failure path: owned VM purged, exit nonzero"

# --- 5) Launch failure (instance half-created) is still cleaned up ---
new_case
use_native_multipass
export FAKE_MP_LAUNCH_RC=3
run_accept "$MP_PATH"
unset FAKE_MP_LAUNCH_RC
[ "$RC" -ne 0 ] || fail "launch failure reported success"
grep -qx "$(vm_name)" "$CASE/state/deletes.log" || fail "half-launched VM not deleted"
assert_foreign_vms_intact "launch-failure"
pass "launch failure: half-created VM purged"

# --- 6) Interruption (SIGINT during guest run): VM purged ---
new_case
use_native_multipass
export FAKE_MP_GUEST_SLEEP=60
set +e
PATH="$MP_PATH" python3 - "$ACCEPT" "$CASE/state/guest.log" "$CASE/out.txt" <<'PY'
import os, signal, subprocess, sys, time
accept, guest_log, out = sys.argv[1:4]
with open(out, "wb") as f:
    p = subprocess.Popen(["bash", accept], stdout=f, stderr=subprocess.STDOUT,
                         start_new_session=True,
                         preexec_fn=lambda: signal.signal(signal.SIGINT, signal.SIG_DFL))
    for _ in range(300):
        if os.path.exists(guest_log):
            break
        time.sleep(0.1)
    else:
        p.kill(); sys.exit("guest run never started")
    os.killpg(p.pid, signal.SIGINT)
    sys.exit(0 if p.wait(timeout=60) != 0 else "interrupted run exited 0")
PY
prc=$?
set -e
[ "$prc" -eq 0 ] || fail "interrupt harness failed (rc=$prc): $(cat "$CASE/out.txt")"
grep -qx "$(vm_name)" "$CASE/state/deletes.log" || fail "owned VM not deleted after SIGINT: $(cat "$CASE/out.txt")"
[ ! -e "$CASE/state/vms/$(vm_name)" ] || fail "owned VM survived SIGINT"
assert_foreign_vms_intact "interrupt"
pass "interrupt path: SIGINT during guest run still purges the owned VM"

# --- 7) Preexisting instance with this run's name: refused, never adopted or deleted ---
new_case
use_native_multipass
touch "$CASE/state/vms/$(vm_name)"
run_accept "$MP_PATH"
[ "$RC" -ne 0 ] && echo "$OUT" | grep -q 'refusing to adopt preexisting Multipass instance' \
  || fail "colliding instance not refused: $OUT"
[ -e "$CASE/state/vms/$(vm_name)" ] || fail "colliding preexisting instance was deleted"
[ ! -s "$CASE/state/deletes.log" ] || fail "delete called after refusing collision"
grep -q '|launch' "$CASE/state/calls.log" && fail "launched despite collision" || true
pass "preexisting instance with the run name is refused and left intact"

# --- 8) Unlistable daemon: refuse rather than guess ---
new_case
use_native_multipass
export FAKE_MP_LIST_RC=1
run_accept "$MP_PATH"
[ "$RC" -ne 0 ] && echo "$OUT" | grep -q 'could not list Multipass instances' || fail "list failure not reported"
grep -q '|launch' "$CASE/state/calls.log" && fail "launched without a successful ownership check" || true
pass "unlistable Multipass: refuses before launch"

# --- 9) Windows multipass.exe (WSL / Git Bash): discovered by name; transfer uses no drive path ---
new_case
cp "$BIN/fake-multipass" "$CASE/mpbin/multipass.exe"
run_accept "$CASE/mpbin:$BIN:$SYS_PATH"
[ "$RC" -eq 0 ] || fail "multipass.exe discovery run failed: $OUT"
echo "$OUT" | grep -q 'Windows executable' || fail "multipass.exe not treated as Windows executable"
tline="$(grep '|transfer ' "$CASE/state/calls.log" || true)"
if [ -n "$tline" ]; then
  # Drive-backed stage: relative basename from the stage cwd (no C:\ or /mnt/c source).
  echo "$tline" | grep -qE '\|transfer payload\.tgz ' || fail "Windows transfer used a non-relative source: $tline"
else
  # Linux-only stage (no drive path): payload streamed through exec instead.
  grep -q "|exec $(vm_name) -- bash -c base64 -d > '/home/ubuntu/ignite-payload.tgz'" "$CASE/state/calls.log" \
    || fail "no transfer and no exec stream for Windows multipass"
fi
grep -qx "$(vm_name)" "$CASE/state/deletes.log" || fail "multipass.exe run did not purge VM"
pass "multipass.exe discovered; payload never passed as a drive-letter path"

# --- 10) Standard Windows install location found when not on PATH ---
new_case
mkdir -p "$CASE/pf/Multipass/bin"
cp "$BIN/fake-multipass" "$CASE/pf/Multipass/bin/multipass.exe"
export IGNITE_MULTIPASS_CANDIDATES="$CASE/nope/multipass.exe:$CASE/pf/Multipass/bin/multipass.exe"
run_accept "$BIN:$SYS_PATH"
[ "$RC" -eq 0 ] || fail "candidate-path discovery failed: $OUT"
echo "$OUT" | grep -q "multipass: $CASE/pf/Multipass/bin/multipass.exe" || fail "candidate path not used"
pass "multipass.exe found at the standard install location without PATH"

# --- 11) Retrieved evidence is secret-scanned on the host ---
new_case
use_native_multipass
export FAKE_MP_LEAK=1
run_accept "$MP_PATH"
[ "$RC" -ne 0 ] && echo "$OUT" | grep -q 'failed the secret scan' || fail "leaky guest evidence not rejected: $OUT"
echo "$OUT" | grep -q 'leaked-token-value' && fail "scanner echoed the secret" || true
grep -qx "$(vm_name)" "$CASE/state/deletes.log" || fail "VM not purged after evidence rejection"
pass "host rescans retrieved evidence; leak fails the run without echoing it"

echo "PASS: accept-disposable launcher regressions"
