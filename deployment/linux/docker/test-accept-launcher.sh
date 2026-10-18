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
  transfer)
    case "$1" in
      *:/*)
        # Download (guest → host). Dest is relative to cwd, like multipass.exe.
        echo pull >>"$S/pulls.log"
        src="$S/guest-evidence.tgz"; dst="$2"
        perm() { echo "[error] [sftp] cannot set permissions for local file $dst" >&2; exit 2; }
        case "${FAKE_MP_PULL:-ok}" in
          ok)        cp "$src" "$dst"; exit 0 ;;
          perm)      cp "$src" "$dst"; perm ;;
          truncated) head -c 20 "$src" >"$dst"; perm ;;
          corrupt)   cp "$src" "$dst"; printf 'X' | dd of="$dst" bs=1 seek=12 conv=notrunc 2>/dev/null; exit 0 ;;
          missing)   echo "[error] [sftp] failed to open remote file" >&2; exit 2 ;;
          hang)      head -c 20 "$src" >"$dst"; sleep 30; exit 0 ;;
        esac ;;
      *)
        # Upload: the bytes multipass would read (relative to cwd, like multipass.exe).
        cp "$1" "$S/pushed.bin" || exit 1
        exit 0 ;;
    esac ;;
  delete)
    name="${*: -1}"
    echo "$name" >>"$S/deletes.log"
    rm -f "$S/vms/$name"
    exit 0 ;;
  exec)
    name="$1"; shift; [ "${1:-}" = "--" ] && shift
    all="$*"
    case "$all" in
      *"base64 -d"*)
        # Real multipass.exe forwards no piped stdin: model that (0 bytes, then hang).
        echo stdin-stream >>"$S/stream.log"; : >"$S/pushed.bin"; sleep 30; exit 1 ;;
      "sha256sum /home/ubuntu/evidence.tgz")
        sha256sum "$S/guest-evidence.tgz" | awk -v d="$2" '{print $1"  "d}'
        exit 0 ;;
      "sha256sum "*)
        sha256sum "$S/pushed.bin" | awk -v d="$2" '{print $1"  "d}'
        exit 0 ;;
      *"-czf /home/ubuntu/evidence.tgz"*)
        # Guest packs its evidence (what the guest acceptance run would have written).
        t="$(mktemp -d)"
        mkdir -p "$t/evidence-$IGNITE_DOCKER_RUN_ID"
        printf 'RESULT=%s\nNOTEBOOK_BLOB_ROUNDTRIP=PASS\n' "${FAKE_MP_RESULT:-PASS}" \
          >"$t/evidence-$IGNITE_DOCKER_RUN_ID/summary.txt"
        [ -n "${FAKE_MP_LEAK:-}" ] && echo "Authorization: Bearer leaked-token-value" \
          >"$t/evidence-$IGNITE_DOCKER_RUN_ID/leak.txt"
        tar -C "$t" -czf "$S/guest-evidence.tgz" "evidence-$IGNITE_DOCKER_RUN_ID"
        rm -rf "$t"
        exit 0 ;;
      *accept-disposable.sh*)
        echo guest-run >>"$S/guest.log"
        if [ -n "${FAKE_MP_GUEST_SLEEP:-}" ]; then
          for _ in $(seq 1 "$FAKE_MP_GUEST_SLEEP"); do sleep 1; done
        fi
        exit "${FAKE_MP_GUEST_RC:-0}" ;;
      *"base64 -w0"*)
        # Real multipass.exe wedges streaming base64 back over exec stdout: model the hang.
        echo stdout-stream >>"$S/stream.log"; sleep 30; exit 1 ;;
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

# Windows-side fakes (WSL view): only $FAKE_DRIVE is "on a Windows drive" (C:\fake\...).
WINBIN="$TMP/winbin"
mkdir -p "$WINBIN"
cat >"$WINBIN/wslpath" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  -w)
    p="$2"
    case "$p" in
      "$FAKE_DRIVE"|"$FAKE_DRIVE"/*) r="${p#"$FAKE_DRIVE"}"; printf 'C:\\fake%s\n' "${r//\//\\}" ;;
      *) printf '\\\\wsl.localhost\\Ubuntu%s\n' "${p//\//\\}" ;;
    esac ;;
  -u)
    case "$2" in
      'C:\fake'*) r="${2#'C:\fake'}"; printf '%s%s\n' "$FAKE_DRIVE" "${r//\\//}" ;;
      *) exit 1 ;;
    esac ;;
  *) exit 1 ;;
esac
EOF
cat >"$WINBIN/cmd.exe" <<'EOF'
#!/usr/bin/env bash
[ -z "${FAKE_CMD_FAIL:-}" ] || exit 1
printf 'C:\\fake\\Temp\r\n'
EOF
chmod +x "$WINBIN/wslpath" "$WINBIN/cmd.exe"
# The real C:\Windows\System32\cmd.exe must never be consulted.
export IGNITE_WINDOWS_CMD_CANDIDATES=""

# Hermetic PATH: no real multipass/multipass.exe/docker may leak in from the host.
SYS_PATH="/usr/bin:/bin"
export FAKE_DOCKER_LOG="$TMP/docker.log"

n=0
new_case() {
  n=$((n + 1))
  CASE="$TMP/case$n"
  mkdir -p "$CASE/state/vms" "$CASE/mpbin" "$CASE/stage" "$CASE/drive/Temp"
  export FAKE_DRIVE="$CASE/drive"
  unset FAKE_CMD_FAIL
  : >"$FAKE_DOCKER_LOG"
  export FAKE_MP_STATE="$CASE/state"
  export IGNITE_DOCKER_RUN_ID="t${n}-$$"
  export IGNITE_DOCKER_EVIDENCE="$CASE/evidence"
  export IGNITE_DOCKER_STAGE_DIR="$CASE/stage"
  export IGNITE_DOCKER_BUNDLE="$BUNDLE"
  export IGNITE_MULTIPASS_DELETE_WAIT=0
  export IGNITE_MULTIPASS_DELETE_TRIES=3
  unset IGNITE_DOCKER_ACCEPT IGNITE_MULTIPASS FAKE_MP_GUEST_RC FAKE_MP_GUEST_SLEEP \
    FAKE_MP_RESULT FAKE_MP_LIST_RC FAKE_MP_LEAK IGNITE_MULTIPASS_CANDIDATES FAKE_MP_PULL \
    IGNITE_MULTIPASS_TRANSFER_TIMEOUT
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
[ -s "$CASE/state/pulls.log" ] || fail "evidence not retrieved via multipass transfer"
[ ! -e "$CASE/state/stream.log" ] || fail "evidence/payload streamed as base64 over exec"
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

# --- 9) Windows multipass.exe from WSL: Linux-only TMPDIR → stage moves to Windows %TEMP%;
#        relative transfer from that drive cwd; never a piped-stdin stream ---
new_case
cp "$BIN/fake-multipass" "$CASE/mpbin/multipass.exe"
unset IGNITE_DOCKER_STAGE_DIR
TMPDIR="$CASE/stage" run_accept "$CASE/mpbin:$WINBIN:$BIN:$SYS_PATH"
[ "$RC" -eq 0 ] || fail "multipass.exe discovery run failed: $OUT"
echo "$OUT" | grep -q 'Windows executable' || fail "multipass.exe not treated as Windows executable"
tline="$(grep '|transfer .*:/home/ubuntu/ignite-payload.tgz$' "$CASE/state/calls.log" || true)"
case "$tline" in
  "$CASE/drive/Temp/ignite-dacc-stage-"*"|transfer payload.tgz $(vm_name):/home/ubuntu/ignite-payload.tgz") ;;
  *) fail "Windows transfer not relative from a drive-backed stage: ${tline:-<none>}" ;;
esac
[ -e "$CASE/state/stream.log" ] && fail "payload piped through multipass.exe exec stdin" || true
ls -d "$CASE/drive/Temp"/ignite-dacc-stage-* >/dev/null 2>&1 && fail "Windows stage not removed" || true
grep -qx "$(vm_name)" "$CASE/state/deletes.log" || fail "multipass.exe run did not purge VM"
pass "multipass.exe: stage on Windows %TEMP%, relative transfer, no stdin stream, cleaned up"

# --- 9b) Windows multipass.exe with no drive-backed stage: one actionable error, no VM ---
new_case
cp "$BIN/fake-multipass" "$CASE/mpbin/multipass.exe"
unset IGNITE_DOCKER_STAGE_DIR
export FAKE_CMD_FAIL=1
TMPDIR="$CASE/stage" run_accept "$CASE/mpbin:$WINBIN:$BIN:$SYS_PATH"
[ "$RC" -ne 0 ] && echo "$OUT" | grep -q 'Set IGNITE_DOCKER_STAGE_DIR' || fail "no actionable stage error: $OUT"
grep -q '|launch' "$CASE/state/calls.log" && fail "launched a VM without a usable stage" || true
[ -e "$CASE/state/stream.log" ] && fail "fell back to a stdin stream" || true
pass "multipass.exe without a drive-backed stage: actionable error before launch"

# --- 10) Standard Windows install location found when not on PATH ---
new_case
mkdir -p "$CASE/pf/Multipass/bin"
cp "$BIN/fake-multipass" "$CASE/pf/Multipass/bin/multipass.exe"
export IGNITE_MULTIPASS_CANDIDATES="$CASE/nope/multipass.exe:$CASE/pf/Multipass/bin/multipass.exe"
export IGNITE_DOCKER_STAGE_DIR="$CASE/drive/Temp"
run_accept "$WINBIN:$BIN:$SYS_PATH"
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

# --- 12) Evidence retrieval through Windows multipass.exe: relative download into the
#         drive-backed stage; the post-transfer permission error is accepted only on a hash match ---
use_windows_multipass() {
  cp "$BIN/fake-multipass" "$CASE/mpbin/multipass.exe"
  export IGNITE_DOCKER_STAGE_DIR="$CASE/drive/Temp"
  MP_PATH="$CASE/mpbin:$WINBIN:$BIN:$SYS_PATH"
}
assert_owned_cleaned() { # assert_owned_cleaned <label>
  grep -qx "$(vm_name)" "$CASE/state/deletes.log" || fail "$1: owned VM not purged"
  [ ! -e "$CASE/state/vms/$(vm_name)" ] || fail "$1: owned VM still exists"
  ls -d "$CASE/drive/Temp"/ignite-dacc-stage-* >/dev/null 2>&1 && fail "$1: owned stage not removed" || true
  [ ! -e "$CASE/state/stream.log" ] || fail "$1: base64 streamed over multipass exec"
  assert_foreign_vms_intact "$1"
}
new_case
use_windows_multipass
run_accept "$MP_PATH"
[ "$RC" -eq 0 ] || fail "windows retrieval run failed: $OUT"
grep -q "^$CASE/drive/Temp/ignite-dacc-stage-[^|]*|transfer $(vm_name):/home/ubuntu/evidence.tgz evidence.tgz\$" \
  "$CASE/state/calls.log" || fail "evidence not downloaded by relative name into the drive stage"
grep -q 'RESULT=PASS' "$IGNITE_DOCKER_EVIDENCE/summary.txt" || fail "windows: summary not retrieved"
echo "$OUT" | grep -q 'WARNING' && fail "clean transfer produced a warning" || true
assert_owned_cleaned "windows retrieval"
pass "windows retrieval: native transfer, relative name, verified, cleaned up"

new_case
use_windows_multipass
export FAKE_MP_PULL=perm
run_accept "$MP_PATH"
[ "$RC" -eq 0 ] || fail "complete download with permission error rejected: $OUT"
echo "$OUT" | grep -q 'WARNING: multipass transfer exited 2 after a complete download' || fail "permission error not warned"
echo "$OUT" | grep -q 'cannot set permissions for local file' || fail "multipass message not surfaced"
grep -q 'RESULT=PASS' "$IGNITE_DOCKER_EVIDENCE/summary.txt" || fail "perm: summary not retrieved"
assert_owned_cleaned "permission error"
pass "post-transfer 'cannot set permissions' accepted with a warning when sha256 matches"

# --- 13) Incomplete, corrupt, or missing downloads fail closed (nothing extracted or reported PASS) ---
for mode in truncated corrupt missing; do
  new_case
  use_windows_multipass
  export FAKE_MP_PULL="$mode"
  run_accept "$MP_PATH"
  [ "$RC" -ne 0 ] || fail "$mode download reported success"
  echo "$OUT" | grep -q 'missing, incomplete, or corrupt' || fail "$mode: no integrity error: $OUT"
  echo "$OUT" | grep -q 'PASS — evidence on host' && fail "$mode: reported PASS" || true
  [ ! -e "$IGNITE_DOCKER_EVIDENCE/summary.txt" ] || fail "$mode: unverified evidence extracted"
  assert_owned_cleaned "$mode download"
done
pass "truncated/corrupt/missing evidence downloads fail closed; VM and stage cleaned"

# --- 14) A wedged evidence transfer times out and still cleans up ---
new_case
use_windows_multipass
export FAKE_MP_PULL=hang IGNITE_MULTIPASS_TRANSFER_TIMEOUT=2
start=$SECONDS
run_accept "$MP_PATH"
[ "$RC" -ne 0 ] || fail "hung transfer reported success"
[ $((SECONDS - start)) -lt 25 ] || fail "transfer timeout not enforced"
echo "$OUT" | grep -q 'exceeded 2s' || fail "timeout not reported: $OUT"
assert_owned_cleaned "transfer timeout"
pass "hung evidence transfer bounded by IGNITE_MULTIPASS_TRANSFER_TIMEOUT; VM and stage cleaned"

# --- 15) The transfer bound is unconditional: timeout, else gtimeout, else fail before transferring ---
REAL_TIMEOUT="$(command -v timeout)" || fail "timeout(1) required to run these tests"
# System tools minus timeout/gtimeout, so availability is controlled per case.
NOTMO="$TMP/notimeout-bin"
mkdir -p "$NOTMO"
for f in /usr/bin/* /bin/*; do
  b="$(basename "$f")"
  case "$b" in timeout|gtimeout) continue ;; esac
  [ -e "$NOTMO/$b" ] || ln -s "$f" "$NOTMO/$b"
done
make_timeout_shim() { # make_timeout_shim <name> → $CASE/tmo/<name> logs its argv, then bounds for real
  mkdir -p "$CASE/tmo"
  printf '#!/usr/bin/env bash\necho "%s $*" >>"%s"\nexec "%s" "$@"\n' \
    "$1" "$CASE/state/timeout.log" "$REAL_TIMEOUT" >"$CASE/tmo/$1"
  chmod +x "$CASE/tmo/$1"
}

new_case
use_native_multipass
make_timeout_shim timeout
make_timeout_shim gtimeout
run_accept "$CASE/mpbin:$CASE/tmo:$BIN:$NOTMO"
[ "$RC" -eq 0 ] || fail "run with timeout available failed: $OUT"
grep -q '^timeout -k 10 900 .* transfer ' "$CASE/state/timeout.log" || fail "timeout not used for transfers"
grep -q '^gtimeout ' "$CASE/state/timeout.log" && fail "gtimeout preferred over timeout" || true
pass "transfers bounded by timeout -k 10 900 when timeout is available"

new_case
use_windows_multipass
make_timeout_shim gtimeout
run_accept "$CASE/mpbin:$CASE/tmo:$WINBIN:$BIN:$NOTMO"
[ "$RC" -eq 0 ] || fail "run with only gtimeout failed: $OUT"
[ "$(grep -c '^gtimeout -k 10 900 .* transfer ' "$CASE/state/timeout.log")" -eq 2 ] \
  || fail "gtimeout not used for both upload and download: $(cat "$CASE/state/timeout.log")"
assert_owned_cleaned "gtimeout"
pass "gtimeout accepted when timeout is unavailable (upload and download)"

new_case
use_native_multipass
run_accept "$CASE/mpbin:$BIN:$NOTMO"
[ "$RC" -ne 0 ] || fail "run without a timeout utility reported success"
echo "$OUT" | grep -q "neither 'timeout' nor 'gtimeout' was found" || fail "no actionable timeout error: $OUT"
[ "$(echo "$OUT" | grep -c 'ERROR')" -le 1 ] || fail "more than one error reported: $OUT"
grep -q '|transfer ' "$CASE/state/calls.log" && fail "transfer started without a time bound" || true
grep -q '|launch 24.04 -n ' "$CASE/state/calls.log" || fail "expected the failure after VM launch"
grep -qx "$(vm_name)" "$CASE/state/deletes.log" || fail "no timeout: owned VM not purged"
[ ! -e "$CASE/state/vms/$(vm_name)" ] || fail "no timeout: owned VM still exists"
[ "$(ls -A "$CASE/stage")" = "ignite-dacc-stage-foreign" ] || fail "no timeout: own stage not removed: $(ls -A "$CASE/stage")"
assert_foreign_vms_intact "no timeout"
pass "no timeout utility: fails before any transfer; owned VM and stage cleaned up"

echo "PASS: accept-disposable launcher regressions"
