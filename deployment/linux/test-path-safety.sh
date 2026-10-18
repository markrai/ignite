#!/usr/bin/env bash
# Path-safety regression tests.
#
# Destructive / mutating exercises use ONLY synthetic paths under a temp directory.
# Rejection of the real repository root and filesystem root is proven with pure
# (non-mutating) validation helpers — those paths are never passed to make-bundle
# or install/uninstall as mutation targets.
#
#   deployment/linux/test-path-safety.sh <bundle-dir>
set -euo pipefail
BUNDLE="$(cd "$1" && pwd)"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REAL_REPO="$(cd "$HERE/../.." && pwd)"
# shellcheck source=path-safety.sh
. "$HERE/path-safety.sh"

BASE="$(mktemp -d)"
trap 'rm -rf "$BASE"' EXIT
fail() { echo "PATH-SAFETY FAIL: $*" >&2; exit 1; }

FAKE="$BASE/inputs"
mkdir -p "$FAKE"
cp -a "$BUNDLE/scribe-server" "$FAKE/scribe-server"
cp -a "$BUNDLE/web" "$FAKE/web"
FAKE_BIN="$(ignite_canonicalize "$FAKE/scribe-server")"
FAKE_WEB="$(ignite_canonicalize "$FAKE/web")"
HERE_ABS="$(ignite_canonicalize "$HERE")"

# ---------------------------------------------------------------------------
# Pure (non-mutating) validation — may mention REAL_REPO and "/" as strings only
# ---------------------------------------------------------------------------
assert_pure_reject_out() {
  local out="$1" label="$2"
  local repo="${3:-$REAL_REPO}"
  local here="${4:-$HERE_ABS}"
  if ignite_reject_bundle_out_dir "$out" "$repo" "$here" "$FAKE_BIN" "$FAKE_WEB" \
      2>"$BASE/pure.err"; then
    fail "pure out-dir validation accepted ($label): $out"
  fi
  grep -Eqi "refus|refusing" "$BASE/pure.err" || fail "pure reject missing message ($label)"
}

assert_pure_accept_out() {
  local out="$1" label="$2"
  local repo="${3:-$REAL_REPO}"
  local here="${4:-$HERE_ABS}"
  if ! ignite_reject_bundle_out_dir "$out" "$repo" "$here" "$FAKE_BIN" "$FAKE_WEB" \
      2>"$BASE/pure.err"; then
    fail "pure out-dir validation rejected ($label): $out ($(cat "$BASE/pure.err"))"
  fi
}

assert_pure_reject_staging() {
  local root="$1" label="$2"
  if ignite_reject_staging_root "$root" "staging root" >/dev/null 2>"$BASE/pure.err"; then
    fail "pure staging validation accepted ($label): $root"
  fi
  grep -Eqi "refus|refusing" "$BASE/pure.err" || fail "pure staging reject missing message ($label)"
}

assert_pure_reject_out "$REAL_REPO" "real repo root (pure)"
assert_pure_reject_out "$REAL_REPO/ignite" "real repo subtree (pure)"
assert_pure_reject_out "$HERE_ABS" "deployment/linux (pure)"
assert_pure_reject_out "/" "filesystem root (pure)"
assert_pure_reject_out "/usr" "exact /usr (pure)"
assert_pure_reject_out "/usr/local/share/ignite-bundle" "under /usr (pure)"
assert_pure_reject_out "/etc/ignite-out" "under /etc (pure)"
assert_pure_reject_out "/var/tmp/ignite-out" "under /var (pure)"
assert_pure_reject_out "/root/ignite-out" "under /root (pure)"
assert_pure_reject_out "/home" "exact /home (pure)"
assert_pure_reject_out "/opt" "exact /opt (pure)"

# GitHub Actions / developer /opt checkouts: standard dist output must be accepted.
# (Modeled paths only — no mutation of real homes or the live checkout.)
for modeled in \
  "/home/runner/work/ignite/ignite" \
  "/home/user/dev/ignite" \
  "/opt/ignite"
do
  assert_pure_accept_out \
    "$modeled/dist/ignite-server-linux" \
    "modeled dist under $modeled" \
    "$modeled" \
    "$modeled/deployment/linux"
  assert_pure_reject_out \
    "$modeled" \
    "modeled repo root $modeled" \
    "$modeled" \
    "$modeled/deployment/linux"
  assert_pure_reject_out \
    "$modeled/deployment/linux" \
    "modeled deployment scripts $modeled" \
    "$modeled" \
    "$modeled/deployment/linux"
  assert_pure_reject_out \
    "$modeled/ignite/server" \
    "modeled source tree $modeled" \
    "$modeled" \
    "$modeled/deployment/linux"
done

# Custom output under a user home (outside the repo) is allowed when unused.
assert_pure_accept_out \
  "/home/user/bundles/ignite-server-linux" \
  "custom home bundle out" \
  "/home/user/dev/ignite" \
  "/home/user/dev/ignite/deployment/linux"

# Staging roots remain blocked under /home and /opt (unchanged stricter policy).
assert_pure_reject_staging "/home/user/stage-root" "staging under /home (pure)"
assert_pure_reject_staging "/opt/ignite-stage" "staging under /opt (pure)"

assert_pure_reject_staging "/" "filesystem root staging (pure)"
assert_pure_reject_staging "/etc" "etc staging (pure)"
assert_pure_reject_staging "/var/lib" "var/lib staging (pure)"

# Symlink-to-repo string: canonicalize then pure-reject (no mutation of repo).
ln -s "$REAL_REPO" "$BASE/link-to-repo"
LINK_REPO="$(ignite_canonicalize "$BASE/link-to-repo")"
assert_pure_reject_out "$LINK_REPO" "symlink to repo resolves to repo (pure)"

# Symlink alias of a dangerous system path cannot bypass bundle-out protection.
ln -s /etc/scribe-server "$BASE/link-to-etc-scribe"
LINK_ETC="$(ignite_canonicalize "$BASE/link-to-etc-scribe")"
assert_pure_reject_out "$LINK_ETC" "symlink alias to /etc/scribe-server (pure)"

# Current checkout's expected dist *path shape* (use a nonexistent probe so an
# unreadable root-owned dist/ from prior privileged builds cannot false-fail).
assert_pure_accept_out \
  "$REAL_REPO/dist/ignite-server-linux-pathprobe" \
  "real checkout dist path shape (pure)"
# When the real dist exists and is a readable Ignite bundle, replacement is allowed.
if [ -d "$REAL_REPO/dist/ignite-server-linux" ] \
    && ignite_is_ignite_bundle_dir "$REAL_REPO/dist/ignite-server-linux"; then
  assert_pure_accept_out \
    "$REAL_REPO/dist/ignite-server-linux" \
    "real checkout existing bundle dist (pure)"
fi

# Explicit GitHub Actions workspace pattern used by ubuntu-latest checkout@v4.
GHA_REPO="/home/runner/work/ignite/ignite"
GHA_OUT="$GHA_REPO/dist/ignite-server-linux"
assert_pure_accept_out "$GHA_OUT" "GitHub Actions dist path" "$GHA_REPO" "$GHA_REPO/deployment/linux"

# ---------------------------------------------------------------------------
# Mutating tests — synthetic paths only under $BASE
# ---------------------------------------------------------------------------
MARKER="$BASE/marker-must-survive"
echo "alive" >"$MARKER"
DECOY="$BASE/decoy-tree"
mkdir -p "$DECOY/sub"
echo "decoy" >"$DECOY/sub/keep.txt"

# Refuse replacing an arbitrary nonempty directory that is not a bundle.
ARBITRARY="$BASE/not-a-bundle"
mkdir -p "$ARBITRARY"
echo "keep-me" >"$ARBITRARY/important.txt"
if bash "$HERE/make-bundle.sh" "$FAKE/scribe-server" "$FAKE/web" "$ARBITRARY" \
    >"$BASE/out.log" 2>"$BASE/err.log"; then
  fail "make-bundle replaced arbitrary nonempty directory"
fi
grep -Eqi "refus|refusing|not an Ignite" "$BASE/err.log" || fail "arbitrary dir reject unclear"
[ -f "$ARBITRARY/important.txt" ] || fail "arbitrary dir contents were damaged"
[ "$(cat "$ARBITRARY/important.txt")" = "keep-me" ] || fail "arbitrary dir contents changed"

# Safe out-dir create + replace (must look like a bundle after first build).
SAFE_OUT="$BASE/safe-bundle-out"
bash "$HERE/make-bundle.sh" "$FAKE/scribe-server" "$FAKE/web" "$SAFE_OUT" >/dev/null
[ -x "$SAFE_OUT/scribe-server" ] || fail "safe out missing binary"
[ -f "$SAFE_OUT/$IGNITE_BUNDLE_MARKER" ] || fail "bundle marker missing"
grep -q '^ignite-bundle=1$' "$SAFE_OUT/$IGNITE_BUNDLE_MARKER" || fail "bundle marker content invalid"
bash "$HERE/make-bundle.sh" "$FAKE/scribe-server" "$FAKE/web" "$SAFE_OUT" >/dev/null
[ -x "$SAFE_OUT/scribe-server" ] || fail "safe replace lost binary"
[ -f "$DECOY/sub/keep.txt" ] || fail "decoy tree was damaged"

# Actual make-bundle.sh against a disposable synthetic repo under a simulated home
# layout (does not touch real $HOME or the live checkout).
SYN_REPO="$BASE/home/runner/work/ignite/ignite"
mkdir -p "$SYN_REPO/deployment/linux" "$SYN_REPO/dist" "$SYN_REPO/build"
cp -a "$HERE/make-bundle.sh" "$HERE/path-safety.sh" \
  "$HERE/install.sh" "$HERE/uninstall.sh" "$HERE/firewall.sh" \
  "$HERE/scribe-server.service" "$SYN_REPO/deployment/linux/"
cp -a "$FAKE/scribe-server" "$SYN_REPO/build/scribe-server"
cp -a "$FAKE/web" "$SYN_REPO/build/web"
SYN_OUT="$SYN_REPO/dist/ignite-server-linux"
bash "$SYN_REPO/deployment/linux/make-bundle.sh" \
  "$SYN_REPO/build/scribe-server" "$SYN_REPO/build/web" "$SYN_OUT" >/dev/null
[ -x "$SYN_OUT/scribe-server" ] || fail "synthetic-home make-bundle missing binary"
[ -f "$SYN_OUT/$IGNITE_BUNDLE_MARKER" ] || fail "synthetic-home make-bundle missing marker"
# Repeatable replace of the legitimate bundle output.
bash "$SYN_REPO/deployment/linux/make-bundle.sh" \
  "$SYN_REPO/build/scribe-server" "$SYN_REPO/build/web" "$SYN_OUT" >/dev/null
[ -x "$SYN_OUT/scribe-server" ] || fail "synthetic-home replace lost binary"
# Unrelated directory beside dist must remain untouched / not replaceable as out.
SYN_UNRELATED="$SYN_REPO/dist/unrelated-dir"
mkdir -p "$SYN_UNRELATED"
echo "keep-unrelated" >"$SYN_UNRELATED/keep.txt"
if bash "$SYN_REPO/deployment/linux/make-bundle.sh" \
    "$SYN_REPO/build/scribe-server" "$SYN_REPO/build/web" "$SYN_UNRELATED" \
    >"$BASE/syn.out" 2>"$BASE/syn.err"; then
  fail "synthetic-home make-bundle replaced unrelated dist sibling"
fi
[ -f "$SYN_UNRELATED/keep.txt" ] || fail "synthetic-home unrelated dir damaged"
[ "$(cat "$SYN_UNRELATED/keep.txt")" = "keep-unrelated" ] || fail "synthetic-home unrelated dir altered"

# Lookalike directory (all filenames, no valid .ignite-bundle marker) must not be replaced.
LOOKALIKE="$BASE/lookalike-bundle"
cp -a "$SAFE_OUT" "$LOOKALIKE"
rm -f "$LOOKALIKE/$IGNITE_BUNDLE_MARKER"
echo "lookalike-keep" >"$LOOKALIKE/IMPORTANT.txt"
if bash "$HERE/make-bundle.sh" "$FAKE/scribe-server" "$FAKE/web" "$LOOKALIKE" \
    >"$BASE/look.out" 2>"$BASE/look.err"; then
  fail "make-bundle replaced lookalike dir without bundle marker"
fi
grep -Eqi "refus|not an Ignite" "$BASE/look.err" || fail "lookalike reject unclear"
[ -f "$LOOKALIKE/IMPORTANT.txt" ] || fail "lookalike contents damaged"
[ "$(cat "$LOOKALIKE/IMPORTANT.txt")" = "lookalike-keep" ] || fail "lookalike contents changed"

# Malformed marker content.
MALFORMED="$BASE/malformed-marker"
cp -a "$SAFE_OUT" "$MALFORMED"
printf 'ignite-bundle=0\n' >"$MALFORMED/$IGNITE_BUNDLE_MARKER"
echo "malformed-keep" >"$MALFORMED/IMPORTANT.txt"
if bash "$HERE/make-bundle.sh" "$FAKE/scribe-server" "$FAKE/web" "$MALFORMED" \
    >"$BASE/mal.out" 2>"$BASE/mal.err"; then
  fail "make-bundle replaced dir with malformed bundle marker"
fi
[ -f "$MALFORMED/IMPORTANT.txt" ] || fail "malformed-marker dir damaged"

# Symlinked bundle marker.
SYMMARK="$BASE/symlink-marker-bundle"
VICTIM_BM="$BASE/victim-bundle-marker"
cp -a "$SAFE_OUT" "$SYMMARK"
echo "bundle-marker-victim" >"$VICTIM_BM"
rm -f "$SYMMARK/$IGNITE_BUNDLE_MARKER"
ln -s "$VICTIM_BM" "$SYMMARK/$IGNITE_BUNDLE_MARKER"
if bash "$HERE/make-bundle.sh" "$FAKE/scribe-server" "$FAKE/web" "$SYMMARK" \
    >"$BASE/sm.out" 2>"$BASE/sm.err"; then
  fail "make-bundle replaced dir with symlinked bundle marker"
fi
[ -f "$VICTIM_BM" ] || fail "bundle marker symlink damaged victim"
[ "$(cat "$VICTIM_BM")" = "bundle-marker-victim" ] || fail "bundle marker victim altered"

# Pure identity helper: positive on real bundle, negative on lookalike.
ignite_is_ignite_bundle_dir "$SAFE_OUT" || fail "ignite_is_ignite_bundle_dir rejected real bundle"
ignite_is_ignite_bundle_dir "$LOOKALIKE" && fail "ignite_is_ignite_bundle_dir accepted lookalike"
ignite_is_ignite_bundle_dir "$MALFORMED" && fail "ignite_is_ignite_bundle_dir accepted malformed marker"
ignite_is_ignite_bundle_dir "$SYMMARK" && fail "ignite_is_ignite_bundle_dir accepted symlinked marker"

# Input binary mode must not change.
chmod 0644 "$FAKE/scribe-server"
bash "$HERE/make-bundle.sh" "$FAKE/scribe-server" "$FAKE/web" "$BASE/mode-out" >/dev/null
[ "$(stat -c %a "$FAKE/scribe-server")" = "644" ] || fail "make-bundle changed input binary mode"

# Staging root: nonempty unmarked directory must not be adopted / marked.
NONEMPTY="$BASE/nonempty-unmarked"
mkdir -p "$NONEMPTY/leftover"
echo y >"$NONEMPTY/leftover/x"
if "$BUNDLE/install.sh" --root="$NONEMPTY" --port=19993 >"$BASE/nm.out" 2>"$BASE/nm.err"; then
  fail "install adopted nonempty unmarked staging root"
fi
[ ! -f "$NONEMPTY/$IGNITE_STAGING_MARKER" ] || fail "marker written onto nonempty unmarked root"
[ -f "$NONEMPTY/leftover/x" ] || fail "nonempty unmarked root was modified"

# Purge without marker fails; data remains.
EMPTY="$BASE/empty-stage"
mkdir -p "$EMPTY/var/lib/scribe-server"
echo x >"$EMPTY/var/lib/scribe-server/server.json"
if "$BUNDLE/uninstall.sh" --root="$EMPTY" --purge >"$BASE/p.log" 2>"$BASE/p.err"; then
  fail "purge without staging marker succeeded"
fi
[ -f "$EMPTY/var/lib/scribe-server/server.json" ] || fail "purge without marker deleted data"

# ---------------------------------------------------------------------------
# Symlink escape under staging root (Finding 1 reproduction, disposable victim)
# ---------------------------------------------------------------------------
VICTIM="$BASE/victim-outside"
mkdir -p "$VICTIM"
echo "victim-alive" >"$VICTIM/service.unit"
STAGE_ESC="$BASE/stage-escape"
mkdir -p "$STAGE_ESC"
# Build a marked staging install first (safe).
"$BUNDLE/install.sh" --root="$STAGE_ESC" --port=19994 >/dev/null
# Replace etc with a symlink escaping to the victim directory.
rm -rf "$STAGE_ESC/etc"
ln -s "$VICTIM" "$STAGE_ESC/etc"
mkdir -p "$VICTIM/systemd/system"
echo "dummy-unit" >"$VICTIM/systemd/system/scribe-server.service"

if "$BUNDLE/uninstall.sh" --root="$STAGE_ESC" >"$BASE/esc.out" 2>"$BASE/esc.err"; then
  fail "uninstall accepted staging root with escaping symlink"
fi
grep -Eqi "refus|symlink|outside" "$BASE/esc.err" || fail "symlink escape reject unclear"
[ -f "$VICTIM/service.unit" ] || fail "victim file deleted via symlink escape"
[ "$(cat "$VICTIM/service.unit")" = "victim-alive" ] || fail "victim file altered"
[ -f "$VICTIM/systemd/system/scribe-server.service" ] || fail "victim unit deleted via symlink escape"

# Marked staging root whose usr/ is replaced by an escaping symlink: upgrade must refuse.
STAGE_ESC2="$BASE/stage-escape-upgrade"
VICTIM2="$BASE/victim2"
mkdir -p "$VICTIM2/share/scribe-server"
echo "v2" >"$VICTIM2/share/scribe-server/KEEP"
"$BUNDLE/install.sh" --root="$STAGE_ESC2" --port=19995 >/dev/null
rm -rf "$STAGE_ESC2/usr"
ln -s "$VICTIM2" "$STAGE_ESC2/usr"
if "$BUNDLE/install.sh" --root="$STAGE_ESC2" --port=19995 >"$BASE/esc2.out" 2>"$BASE/esc2.err"; then
  fail "upgrade accepted staging root with escaping usr symlink"
fi
grep -Eqi "refus|symlink|outside" "$BASE/esc2.err" || fail "upgrade symlink escape reject unclear"
[ -f "$VICTIM2/share/scribe-server/KEEP" ] || fail "upgrade symlink escape damaged victim"
[ "$(cat "$VICTIM2/share/scribe-server/KEEP")" = "v2" ] || fail "upgrade symlink escape altered victim"

# State-dir symlink escape (misspelled destination previously missed this).
STAGE_STATE="$BASE/stage-state-symlink"
VICTIM_STATE="$BASE/victim-state"
mkdir -p "$VICTIM_STATE"
echo "state-alive" >"$VICTIM_STATE/KEEP"
"$BUNDLE/install.sh" --root="$STAGE_STATE" --port=19996 >/dev/null
rm -rf "$STAGE_STATE/var/lib/scribe-server"
ln -s "$VICTIM_STATE" "$STAGE_STATE/var/lib/scribe-server"
if "$BUNDLE/install.sh" --root="$STAGE_STATE" --port=19996 >"$BASE/st.out" 2>"$BASE/st.err"; then
  fail "upgrade accepted staging root with escaping state symlink"
fi
grep -Eqi "refus|symlink" "$BASE/st.err" || fail "state symlink escape reject unclear"
[ -f "$VICTIM_STATE/KEEP" ] || fail "state symlink escape deleted victim"
[ "$(cat "$VICTIM_STATE/KEEP")" = "state-alive" ] || fail "state symlink escape altered victim"
if "$BUNDLE/uninstall.sh" --root="$STAGE_STATE" --purge >"$BASE/st-purge.out" 2>"$BASE/st-purge.err"; then
  fail "purge accepted staging root with escaping state symlink"
fi
[ -f "$VICTIM_STATE/KEEP" ] || fail "purge via state symlink deleted victim"

# Staging marker that is a symlink to an outside file must not be rewritten.
STAGE_MARK="$BASE/stage-marker-symlink"
VICTIM_MARK="$BASE/victim-marker-file"
echo "marker-victim-alive" >"$VICTIM_MARK"
mkdir -p "$STAGE_MARK"
ln -s "$VICTIM_MARK" "$STAGE_MARK/$IGNITE_STAGING_MARKER"
if "$BUNDLE/install.sh" --root="$STAGE_MARK" --port=19997 >"$BASE/mk.out" 2>"$BASE/mk.err"; then
  fail "install accepted staging root with symlinked marker"
fi
grep -Eqi "refus|symlink" "$BASE/mk.err" || fail "marker symlink reject unclear"
[ -f "$VICTIM_MARK" ] || fail "marker symlink install deleted victim"
[ "$(cat "$VICTIM_MARK")" = "marker-victim-alive" ] || fail "marker symlink install overwrote victim"

# Incomplete removal must not print unconditional success.
STAGE_RM="$BASE/stage-rm-fail"
"$BUNDLE/install.sh" --root="$STAGE_RM" --port=19998 >/dev/null
RM_TARGET="$STAGE_RM/usr/local/bin/scribe-server"
FORCE_RM_FAIL=0
cleanup_rm_attr() { :; }
if [ "$(id -u)" -ne 0 ]; then
  chmod a-w "$STAGE_RM/usr/local/bin"
  cleanup_rm_attr() { chmod u+w "$STAGE_RM/usr/local/bin" 2>/dev/null || true; }
  FORCE_RM_FAIL=1
elif command -v chattr >/dev/null 2>&1 && chattr +i "$RM_TARGET" 2>/dev/null; then
  cleanup_rm_attr() { chattr -i "$RM_TARGET" 2>/dev/null || true; }
  FORCE_RM_FAIL=1
fi
if [ "$FORCE_RM_FAIL" = 1 ]; then
  if "$BUNDLE/uninstall.sh" --root="$STAGE_RM" >"$BASE/rm.out" 2>"$BASE/rm.err"; then
    cleanup_rm_attr
    fail "uninstall claimed success despite removal failure"
  fi
  cleanup_rm_attr
  grep -Eqi "incomplete|failed to remove" "$BASE/rm.err" || fail "uninstall removal failure not reported"
  grep -Eqi "Ignite Server removed" "$BASE/rm.out" && fail "uninstall printed success after incomplete removal"
else
  echo "PATH-SAFETY NOTE: skipped incomplete-removal assert (could not force rm failure in this environment)" >&2
fi

# Deliberately broken validators in a disposable copy under $BASE only.
# Assert helpers are neutered; safe_rm containment still refuses outside targets.
# Checkout/host are never mutation arguments of this harness.
BROKEN="$BASE/broken-scripts"
mkdir -p "$BROKEN"
cp -a "$HERE/uninstall.sh" "$HERE/firewall.sh" "$BROKEN/"
{
  cat "$HERE/path-safety.sh"
  echo 'ignite_assert_path_under_staging_root() { return 0; }'
  echo 'ignite_assert_staging_destinations_safe() { return 0; }'
} >"$BROKEN/path-safety.sh"
STAGE_BRK="$BASE/stage-broken-validator"
VICTIM_BRK="$BASE/victim-broken-validator"
mkdir -p "$VICTIM_BRK/systemd/system" "$STAGE_BRK"
echo "brk-alive" >"$VICTIM_BRK/keep.unit"
printf 'ignite-staging-root=1\ncreated_by=test\n' >"$STAGE_BRK/$IGNITE_STAGING_MARKER"
ln -s "$VICTIM_BRK" "$STAGE_BRK/etc"
echo "dummy" >"$VICTIM_BRK/systemd/system/scribe-server.service"
# Repo-tracked scripts are often mode 0644; invoke via bash so CI exercises
# safe_rm containment rather than failing on the missing executable bit.
if bash "$BROKEN/uninstall.sh" --root="$STAGE_BRK" >"$BASE/brk.out" 2>"$BASE/brk.err"; then
  fail "broken-validator uninstall unexpectedly succeeded without containment"
fi
grep -Eqi "refus|outside" "$BASE/brk.err" || fail "broken-validator uninstall missing containment refuse"
[ -f "$VICTIM_BRK/keep.unit" ] || fail "broken-validator deleted victim"
[ "$(cat "$VICTIM_BRK/keep.unit")" = "brk-alive" ] || fail "broken-validator altered victim"
[ -f "$VICTIM_BRK/systemd/system/scribe-server.service" ] || fail "broken-validator deleted victim unit"
[ -f "$HERE/path-safety.sh" ] || fail "checkout path-safety.sh missing after tests"
[ -f "$REAL_REPO/Cargo.toml" ] || [ -f "$REAL_REPO/Cargo.lock" ] || fail "checkout root damaged"
if grep -nE 'make-bundle\.sh[[:space:]].*\$REAL_REPO|install\.sh[[:space:]]+--root=/|uninstall\.sh[[:space:]]+--root=/' "$0" \
    | grep -vE 'assert_pure|#|pure' >/dev/null; then
  fail "test harness still mutates REAL_REPO or /"
fi

[ -f "$MARKER" ] && [ "$(cat "$MARKER")" = "alive" ] || fail "global marker modified"
echo "PATH-SAFETY OK"
