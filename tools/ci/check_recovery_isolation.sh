#!/usr/bin/env bash
# Fail CI if experimental recovery or the Kindle supervisor harness can leak
# into customer packaging or become enabled by default.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

fail() { echo "ISOLATION FAIL: $*" >&2; exit 1; }

# Resolve ripgrep for GitHub Actions (apt `rg`), macOS/Linux PATH, and common
# Windows editor layouts where `rg.exe` exists but is not bare `rg`.
ignite_find_rg() {
  if command -v rg >/dev/null 2>&1; then
    command -v rg
    return 0
  fi
  if command -v rg.exe >/dev/null 2>&1; then
    command -v rg.exe
    return 0
  fi
  local old_ifs p cand
  old_ifs=$IFS
  IFS=:
  # shellcheck disable=SC2086
  set -- ${PATH:-}
  IFS=$old_ifs
  for p in "$@"; do
    [ -n "$p" ] || continue
    for cand in "$p/rg" "$p/rg.exe"; do
      if [ -x "$cand" ]; then
        printf '%s\n' "$cand"
        return 0
      fi
    done
  done
  for cand in \
    "${LOCALAPPDATA:-}/Programs/cursor/resources/app/node_modules/@vscode/ripgrep/bin/rg.exe" \
    "${USERPROFILE:-}/AppData/Local/Programs/cursor/resources/app/node_modules/@vscode/ripgrep/bin/rg.exe"
  do
    if [ -n "$cand" ] && [ -f "$cand" ]; then
      printf '%s\n' "$cand"
      return 0
    fi
  done
  return 1
}

RG="$(ignite_find_rg)" || fail "rg (ripgrep) not found on PATH; install ripgrep (CI: apt install ripgrep)"

# 1) Live non-loopback POST must stay compile-time false.
if ! "$RG" -n 'pub const LIVE_NON_LOOPBACK_POST_ENABLED:\s*bool\s*=\s*false\s*;' \
  ignite/kindle/scribe-app/src/sync_recovery_policy.rs >/dev/null; then
  fail "LIVE_NON_LOOPBACK_POST_ENABLED is not false in sync_recovery_policy.rs"
fi

# 2) Production spawn entry must remain a hard refuse.
if ! "$RG" -n 'child spawn is not enabled' ignite/kindle/scribe-app/src/sync_recovery_stream.rs >/dev/null; then
  fail "spawn_recovery_stream_child refuse string missing"
fi

# 3) Customer ARM build must not enable the harness feature.
if "$RG" -n 'kindle_supervisor_harness' tools/docker_build_armv7hf.sh >/dev/null; then
  fail "docker_build_armv7hf.sh must not reference kindle_supervisor_harness"
fi

# 4) package-release (Python core + wrappers) must not stage the harness binary.
for packager in \
  deployment/kindle/package_release.py \
  scripts/kindle/package-release.sh \
  scripts/kindle/package-release.ps1
do
  if "$RG" -n 'kindle-supervisor-harness|kindle_supervisor_harness' "$packager" >/dev/null; then
    fail "$packager must not package the supervisor harness"
  fi
done

# 5) Ordinary main must not wire recovery submit or the harness.
if "$RG" -n 'submit_recovery|SupervisedRecoveryStream|kindle_supervisor_harness|run_kindle_supervisor_harness' \
  ignite/kindle/scribe-app/src/main.rs >/dev/null; then
  fail "scribe-app main.rs must not reference recovery submit/harness"
fi

# 6) Harness binary requires the explicit feature.
if ! "$RG" -n 'required-features\s*=\s*\["kindle_supervisor_harness"\]' \
  ignite/kindle/scribe-app/Cargo.toml >/dev/null; then
  fail "kindle-supervisor-harness bin missing required-features gate"
fi

# 7) No allow_live / skip_checks escape hatches in the executor.
if "$RG" -n 'allow_live|skip_checks|trusted_local' ignite/kindle/scribe-app/src/sync_recovery_exec.rs >/dev/null; then
  fail "forbidden escape-hatch identifier in sync_recovery_exec.rs"
fi

# 8) Customer packaging must keep secret/credential contamination gates.
if ! "$RG" -n 'CONTAMINATION_PATTERNS' deployment/kindle/validate_stage.py >/dev/null; then
  fail "validate_stage.py missing CONTAMINATION_PATTERNS (credential gates)"
fi
if ! "$RG" -n '\.pem|FORBIDDEN_FILENAMES|BINARY_FORBIDDEN_SUBSTRINGS' deployment/kindle/validate_stage.py >/dev/null; then
  fail "validate_stage.py missing secret file / binary contamination gates"
fi

echo "ISOLATION OK: recovery disabled; harness excluded from customer packaging"
echo "ISOLATION OK: rg=$RG"
