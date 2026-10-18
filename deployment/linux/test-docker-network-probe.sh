#!/usr/bin/env bash
# Focused safety tests for docker-network-probe.sh — mock Docker only; never touches a real engine.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
PROBE="$ROOT/docker-network-probe.sh"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/ignite-probe-safe.XXXXXX")"
cleanup_tmp() { rm -rf "$TMP"; }
trap cleanup_tmp EXIT

mkdir -p "$TMP/bin" "$TMP/state/containers" "$TMP/log"
: >"$TMP/log/docker.calls"
: >"$TMP/log/docker.rm"

# Minimal host tool stubs (no real network).
cat >"$TMP/bin/ip" <<'EOF'
#!/usr/bin/env bash
echo "lo UNKNOWN 127.0.0.1/8"
echo "eth0 UP 10.0.0.2/24"
exit 0
EOF
cat >"$TMP/bin/curl" <<'EOF'
#!/usr/bin/env bash
echo "ok-mock"
exit 0
EOF
cat >"$TMP/bin/ss" <<'EOF'
#!/usr/bin/env bash
# Report nothing listening so pick_free_port always succeeds.
exit 0
EOF
cat >"$TMP/bin/python3" <<'EOF'
#!/usr/bin/env bash
# Consume stdin heredoc and print fixed eligibility lines.
cat >/dev/null
echo "eth0: name_eligible=True"
echo "docker0: name_eligible=False"
echo "note: mock"
exit 0
EOF
chmod +x "$TMP/bin/ip" "$TMP/bin/curl" "$TMP/bin/ss" "$TMP/bin/python3"

# Mock docker: records calls; stores containers as files under state/containers/<name>.
cat >"$TMP/bin/docker" <<EOF
#!/usr/bin/env bash
set -euo pipefail
STATE="$TMP/state/containers"
LOG="$TMP/log/docker.calls"
RMLOG="$TMP/log/docker.rm"
FAIL_N_FILE="$TMP/state/fail_nth_run_d"
echo "\$*" >>"\$LOG"
cmd="\${1:-}"
shift || true
case "\$cmd" in
  info) exit 0 ;;
  version) echo "0.0.0-mock"; exit 0 ;;
  inspect)
    name="\${1:-}"
    [ -f "\$STATE/\$name" ] && exit 0
    exit 1
    ;;
  rm)
    # docker rm -f NAME
    while [ \$# -gt 0 ]; do
      case "\$1" in
        -f|--force) shift ;;
        *)
          echo "\$1" >>"\$RMLOG"
          rm -f "\$STATE/\$1"
          shift
          ;;
      esac
    done
    exit 0
    ;;
  run)
    # Parse --name and -d / --rm
    name=""
    detached=0
    rmflag=0
    args=("\$@")
    i=0
    while [ \$i -lt \${#args[@]} ]; do
      a="\${args[\$i]}"
      case "\$a" in
        --name)
          i=\$((i+1)); name="\${args[\$i]}" ;;
        --name=*)
          name="\${a#--name=}" ;;
        -d|--detach) detached=1 ;;
        --rm) rmflag=1 ;;
      esac
      i=\$((i+1))
    done
    [ -n "\$name" ] || { echo "mock docker: missing --name" >&2; exit 2; }
    if [ -f "\$STATE/\$name" ]; then
      echo "mock docker: name in use: \$name" >&2
      exit 1
    fi
    if [ "\$detached" -eq 1 ]; then
      COUNT_FILE="$TMP/state/run_d_count"
      cur=0
      [ -f "\$COUNT_FILE" ] && cur=\$(cat "\$COUNT_FILE")
      if [ -f "\$FAIL_N_FILE" ]; then
        n=\$(cat "\$FAIL_N_FILE")
        # Fail when this would be the n-th successful -d create (1-based: n=1 fails the first).
        if [ "\$cur" -ge "\$n" ]; then
          echo "mock docker: simulated run -d failure" >&2
          exit 1
        fi
      fi
      echo "running" >"\$STATE/\$name"
      echo \$((cur + 1)) >"\$COUNT_FILE"
      echo "cid-\$name"
      exit 0
    fi
    # Optional: create a tracked container then TERM the probe (parent) mid-flight.
    # Sending TERM from the mock avoids relying on the host being able to interrupt a
    # blocking child wait (problematic on some Windows/Git-Bash environments).
    if [ -f "$TMP/state/sleep_on_run" ]; then
      echo "running" >"\$STATE/\$name"
      # Parent is the probe script waiting on this `docker run`.
      kill -TERM "\$PPID" 2>/dev/null || true
      # Linger briefly so the probe's wait returns into the signal trap / EXIT cleanup.
      sleep 5
      exit 0
    fi
    # --rm one-shot: do not persist; print fake iface output
    echo "1: lo: mock"
    echo "2: eth0@if9: mock 172.17.0.2/16"
    exit 0
    ;;
  *)
    echo "mock docker: unsupported: \$cmd" >&2
    exit 2
    ;;
esac
EOF
chmod +x "$TMP/bin/docker"

export PATH="$TMP/bin:$PATH"
export DOCKER="$TMP/bin/docker"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

# --- 1) No opt-in ⇒ no containers, docker never called ---
: >"$TMP/log/docker.calls"
set +e
out="$(bash "$PROBE" 2>&1)"
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "expected non-zero without opt-in"
echo "$out" | grep -qi 'IGNITE_DISPOSABLE_PROBE=1' || fail "opt-in message missing"
[ ! -s "$TMP/log/docker.calls" ] || fail "docker was invoked without opt-in: $(cat "$TMP/log/docker.calls")"
[ -z "$(ls -A "$TMP/state/containers" 2>/dev/null || true)" ] || fail "containers created without opt-in"
pass "no opt-in creates no containers"

# --- 2) Missing Docker ⇒ clear failure, no install attempt ---
: >"$TMP/log/docker.calls"
set +e
out="$(env PATH="/usr/bin:/bin" IGNITE_DISPOSABLE_PROBE=1 DOCKER=docker-not-installed \
  bash "$PROBE" 2>&1)"
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "expected failure when docker missing"
echo "$out" | grep -qi 'Docker CLI not found\|not found' || fail "missing-docker message unclear: $out"
echo "$out" | grep -qi 'will not run apt-get\|will not' || fail "should state it will not install"
pass "missing Docker fails clearly without installation"

# --- 3) Simulated probe failure ⇒ cleanup only invocation-owned ---
# Pre-create a preexisting container that must survive.
echo preexisting >"$TMP/state/containers/keep-me-preexisting"
: >"$TMP/log/docker.rm"
: >"$TMP/log/docker.calls"
# Fail the second detached run (after first owned container is created).
echo 1 >"$TMP/state/fail_nth_run_d"
set +e
out="$(IGNITE_DISPOSABLE_PROBE=1 bash "$PROBE" 2>&1)"
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "expected probe failure when mock run -d fails"
# Preexisting must remain
[ -f "$TMP/state/containers/keep-me-preexisting" ] || fail "preexisting container was removed"
# keep-me must not appear in rm log
if grep -qx 'keep-me-preexisting' "$TMP/log/docker.rm" 2>/dev/null; then
  fail "preexisting container was docker-rm'd"
fi
# Any owned containers created should have been cleaned (state files gone except preexisting)
owned_left="$(find "$TMP/state/containers" -type f ! -name 'keep-me-preexisting' ! -name 'fail_nth_run_d' 2>/dev/null | wc -l | tr -d ' ')"
[ "$owned_left" = "0" ] || fail "owned containers left after failed probe: $(ls "$TMP/state/containers")"
# Rm log should only mention ignite-probe-* names if anything
if grep -v 'ignite-probe-' "$TMP/log/docker.rm" 2>/dev/null | grep -q .; then
  fail "rm log touched non-probe names: $(cat "$TMP/log/docker.rm")"
fi
pass "simulated failure cleans only invocation-owned containers"

# --- 4) Preexisting remains untouched on successful path too ---
rm -f "$TMP/state/fail_nth_run_d" "$TMP/state/run_d_count"
echo preexisting >"$TMP/state/containers/keep-me-preexisting"
: >"$TMP/log/docker.rm"
# Clear leftover owned probe containers from prior failure simulation
find "$TMP/state/containers" -type f -name 'ignite-probe-*' -delete 2>/dev/null || true
set +e
out="$(IGNITE_DISPOSABLE_PROBE=1 bash "$PROBE" 2>&1)"
rc=$?
set -e
[ "$rc" -eq 0 ] || fail "expected successful mock probe, got $rc: $out"
echo "$out" | grep -q 'PROBE_DONE' || fail "PROBE_DONE missing"
[ -f "$TMP/state/containers/keep-me-preexisting" ] || fail "preexisting removed on success path"
if grep -qx 'keep-me-preexisting' "$TMP/log/docker.rm" 2>/dev/null; then
  fail "success path rm'd preexisting"
fi
pass "preexisting containers remain untouched"

# --- 5) TERM during active probe ⇒ exit 143, cleanup once, preexisting untouched ---
rm -f "$TMP/state/fail_nth_run_d" "$TMP/state/run_d_count"
find "$TMP/state/containers" -type f -name 'ignite-probe-*' -delete 2>/dev/null || true
echo preexisting >"$TMP/state/containers/keep-me-preexisting"
: >"$TMP/log/docker.rm"
: >"$TMP/log/docker.calls"
# Mock `docker run` (non-detached) creates an owned name then sends TERM to the probe.
: >"$TMP/state/sleep_on_run"
set +e
IGNITE_DISPOSABLE_PROBE=1 bash "$PROBE" >"$TMP/log/term.out" 2>&1
term_rc=$?
set -e
rm -f "$TMP/state/sleep_on_run"
[ "$term_rc" -eq 143 ] || fail "expected exit 143 on TERM, got $term_rc; out=$(cat "$TMP/log/term.out")"
[ -f "$TMP/state/containers/keep-me-preexisting" ] || fail "TERM path removed preexisting"
if grep -qx 'keep-me-preexisting' "$TMP/log/docker.rm" 2>/dev/null; then
  fail "TERM path rm'd preexisting"
fi
owned_left="$(find "$TMP/state/containers" -type f -name 'ignite-probe-*' 2>/dev/null | wc -l | tr -d ' ')"
[ "$owned_left" = "0" ] || fail "owned containers left after TERM: $(ls "$TMP/state/containers")"
if grep -q 'PROBE_DONE' "$TMP/log/term.out" 2>/dev/null; then
  fail "PROBE_DONE after TERM — script did not terminate"
fi
# Ensure docker rm ran for at least one owned probe name (cleanup via EXIT after signal exit).
grep -q 'ignite-probe-' "$TMP/log/docker.rm" || fail "TERM path did not docker-rm owned containers"
pass "TERM during probe cleans owned containers and exits 143"

echo "PASS: docker-network-probe safety regressions"
exit 0
