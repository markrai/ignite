#!/usr/bin/env bash
# Promote-publish guards: mocked gh, no network, nothing is published.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$DIR/../.." && pwd)"
PUB="$DIR/promote-release.sh"
WF="$ROOT/.github/workflows/release-promote.yml"
CI="$ROOT/.github/workflows/ci.yml"
PYTHON_COMMAND="${PYTHON:-python3}"
PYTHON_REAL="$(command -v "$PYTHON_COMMAND")"
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

block() { awk -v pat="$2" '
  !on && $0 ~ pat { on = 1; match($0, /^ */); ind = RLENGTH; next }
  on { match($0, /^ */); if ($0 !~ /^[[:space:]]*$/ && RLENGTH <= ind) exit; print }' "$1"; }

# --- workflow static guards ----------------------------------------------------
[ -f "$WF" ] || fail "missing $WF"
grep -qx 'permissions: {}' "$WF" || fail "release-promote.yml top-level permissions must be {}"
grep -Eq '^[[:space:]]*(pull_request|push|schedule|release):' "$WF" && fail "promote workflow has a forbidden trigger"
grep -q "github.repository == 'markrai/ignite'" "$WF" || fail "promote must not run in forks"
grep -q "github.ref == 'refs/heads/main'" "$WF" || fail "promote must be limited to main"
grep -q 'actions: read' "$WF" || fail "promote job needs actions: read to download CI artifacts"
grep -q 'contents: write' "$WF" || fail "promote job needs contents: write to create the Release"
grep -Eq 'secrets\.|\bPAT\b|personal' "$WF" && fail "promote must use only GITHUB_TOKEN"
grep -Eq 'run: *[|>]' "$WF" && fail "promote run steps must be single-line"
[ -z "$(grep -E 'run:' "$WF" | grep '\${{')" ] || fail "promote run steps must not interpolate \${{ }}"
grep -q 'uses: ./.github/workflows/ghcr.yml' "$WF" || fail "stable promote must call ghcr.yml"
grep -q 'cancel-in-progress: false' "$WF" || fail "promote publishes must not be cancelled mid-flight"
block "$WF" 'kindle_signature:' | grep -q 'required: true' \
  || fail "offline Kindle signature must be a required dispatch input"
grep -q 'IGNITE_PROMOTE_KINDLE_SIGNATURE:.*inputs.kindle_signature' "$WF" \
  || fail "signature input must reach promotion through the environment"
grep -qi 'hardware' "$WF" && fail "hardware testing must not be described as a publication gate"
grep -q 'name: Promote authenticated CI artifacts' "$WF" \
  || fail "promotion job must describe authenticated CI artifacts"
grep -q 'IGNITE_PROMOTE_TEST_' "$WF" && fail "test trust-root overrides must not be exposed by the workflow"
grep -q 'IGNITE_PROMOTE_TEST_' "$PUB" && fail "promotion must not contain a test trust-root override"
block "$WF" '^  ghcr:' | grep -q 'kindle_signature' \
  && fail "GHCR job must remain independent of Kindle signing metadata"
pass "release-promote.yml: dispatch from main, narrow permissions, GHCR after promote"

# ci.yml must not auto-publish on tag push (no second Kindle/Linux rebuild as Release source)
grep -q 'gh release create' "$CI" && fail "ci.yml must not gh release create (promotion owns publish)"
grep -qE '^  release:' "$CI" && fail "ci.yml must not keep a tag-push release job"
grep -qE '^  ghcr:' "$CI" && fail "ci.yml must not call GHCR (promotion owns GHCR)"
[ "$(grep -c 'packages: write' "$CI")" = 0 ] || fail "ci.yml must not grant packages: write"
grep -qE '^  kindle-package:' "$CI" || fail "ci.yml missing kindle-package job"
grep -q 'name: ignite-kindle-release' "$CI" || fail "ci.yml must upload ignite-kindle-release"
grep -q 'retention-days: 30' "$CI" || fail "release artifacts must be retained for hardware qualification"
grep -q 'python3 tools/release/test_kindle_release_sign.py' "$CI" \
  || fail "CI must run the offline signing helper tests"
pass "ci.yml: builds artifacts, does not publish Releases or GHCR"

T="$(mktemp -d)"
DBT=""
trap 'rm -rf "$T"; [ -n "$DBT" ] && rm -rf "$DBT"' EXIT
SECRET="ghs_test-token-must-never-reach-argv"
V=0.9.13
TAG="v$V"
RUN=424242
COMMIT="$(printf 'a%.0s' {1..40})"
KINDLE_SHA="$(printf 'b%.0s' {1..64})"
LINUX_SHA="$(printf 'c%.0s' {1..64})"
MOCK="$T/bin"
mkdir -p "$MOCK"

# One disposable key per harness run. Only its public half reaches the promotion
# verifier; the real repository pin remains UNPROVISIONED.
TEST_PRIVATE_KEY="$T/disposable-private.pem"
TEST_PUBLIC_DER="$T/disposable-public.der"
TEST_PUBLIC_PIN="$T/disposable-public.hex"
openssl genpkey -algorithm ED25519 -out "$TEST_PRIVATE_KEY" 2>/dev/null
openssl pkey -in "$TEST_PRIVATE_KEY" -pubout -outform DER -out "$TEST_PUBLIC_DER" 2>/dev/null
{ tail -c 32 "$TEST_PUBLIC_DER" | od -An -v -tx1 | tr -d ' \n'; printf '\n'; } >"$TEST_PUBLIC_PIN"
TEST_REPOSITORY="$T/repository"
PUB_RUN="$TEST_REPOSITORY/tools/release/promote-release.sh"
mkdir -p "$TEST_REPOSITORY/tools/release"
cp "$PUB" "$PUB_RUN"
cp "$ROOT/tools/release/kindle_release_sign.py" "$TEST_REPOSITORY/tools/release/"
cp "$TEST_PUBLIC_PIN" "$TEST_REPOSITORY/tools/release/kindle-release-public-key.hex"

cat >"$MOCK/gh" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "gh $*" >>"$MOCK_STATE/argv.log"
http_err() { echo "gh: $2 (HTTP $1)" >&2; exit 1; }
orig="$*"
target=""
args=("$@")
i=0
while [ "$i" -lt "${#args[@]}" ]; do
  [ "${args[i]}" = --target ] && target="${args[i+1]}"
  i=$((i + 1))
done
case "$orig" in
  *"actions/workflows/ci.yml"*)
    [ -f "$MOCK_STATE/workflow.err" ] && { echo "$(cat "$MOCK_STATE/workflow.err")" >&2; exit 1; }
    cat "$MOCK_STATE/workflow.id" ;;
  *"actions/runs/"*"artifacts"*) cat "$MOCK_STATE/artifacts.tsv" ;;
  *"actions/runs/"*) cat "$MOCK_STATE/run.tsv" ;;
  *"/compare/"*)
    [ -f "$MOCK_STATE/compare.err" ] && { echo "$(cat "$MOCK_STATE/compare.err")" >&2; exit 1; }
    cat "$MOCK_STATE/compare.ahead" ;;
  *"contents/ignite/VERSION"*)
    [ -f "$MOCK_STATE/version.err" ] && { echo "$(cat "$MOCK_STATE/version.err")" >&2; exit 1; }
    cat "$MOCK_STATE/version.txt" ;;
  *"/git/ref/tags/"*)
    if [ -f "$MOCK_STATE/tag.network" ]; then
      echo "error connecting to api.github.com" >&2
      exit 1
    fi
    if [ -f "$MOCK_STATE/tag.http" ]; then
      http_err "$(cat "$MOCK_STATE/tag.http")" "tag lookup failed"
    fi
    if [ -f "$MOCK_STATE/tag.tsv" ]; then cat "$MOCK_STATE/tag.tsv"; exit 0; fi
    http_err 404 "Not Found" ;;
  *"/git/tags/"*)
    [ -f "$MOCK_STATE/tagobj.err" ] && { echo "$(cat "$MOCK_STATE/tagobj.err")" >&2; exit 1; }
    cat "$MOCK_STATE/tagobj" ;;
  *"/releases/tags/"*)
    [ -f "$MOCK_STATE/release-exists" ] && { echo "v-exists"; exit 0; }
    http_err 404 "Not Found" ;;
  *"run download"*)
    dest=""; name=""
    while [ "$#" -gt 0 ]; do
      [ "$1" = -D ] && dest="$2"
      [ "$1" = -n ] && name="$2"
      shift
    done
    mkdir -p "$dest"
    cp "$MOCK_STATE/arts/$name/"* "$dest/" ;;
  *"release create"*)
    printf '%s\n' "create $*" >>"$MOCK_STATE/publish.log"
    if [ -f "$MOCK_STATE/tag.after.tsv" ]; then
      cp "$MOCK_STATE/tag.after.tsv" "$MOCK_STATE/tag.tsv"
    elif [ -f "$MOCK_STATE/tag.no-create" ]; then
      rm -f "$MOCK_STATE/tag.tsv"
    else
      printf 'commit\t%s\n' "$target" >"$MOCK_STATE/tag.tsv"
    fi
    exit 0 ;;
  *) echo "unexpected gh $*" >&2; exit 9 ;;
esac
SH
cat >"$MOCK/python3" <<'SH'
#!/usr/bin/env bash
case " $* " in
  *validate_stage.py*)
    [ "${MOCK_VALIDATE:-ok}" = ok ] || { echo "validate failed" >&2; exit 1; }
    exit 0 ;;
  *kindle_release_sign.py*" version "*" --tag v0.9.13 "*)
    printf '0.9.13\n'
    exit 0 ;;
esac
exec "$PYTHON_REAL" "$@"
SH
chmod +x "$MOCK"/*

make_linux() {
  local st="$T/stage" top="ignite-server-$V-linux-x86_64"
  rm -rf "$st"
  mkdir -p "$st/$top"
  printf 'version=%s\narch=x86_64\n' "$V" >"$st/$top/.ignite-bundle"
  printf '#!/bin/sh\necho %s\n' "$V" >"$st/$top/scribe-server"
  chmod +x "$st/$top/scribe-server"
  tar -C "$st" -czf "$T/arts/ignite-server-linux-release/$top.tar.gz" "$top"
  (cd "$T/arts/ignite-server-linux-release" && sha256sum "$top.tar.gz" >"$top.tar.gz.sha256")
}

reset() {
  rm -rf "$T/work" "$T/arts" "$T/signing" "$T/publish.log" "$T/release-exists" "$T/tag.tsv" \
    "$T/tag.http" "$T/tag.network" "$T/tag.after.tsv" "$T/tag.no-create" \
    "$T/tagobj" "$T/tagobj.err" "$T/compare.err" "$T/version.err" "$T/reg-status" \
    "$T/workflow.err"
  mkdir -p "$T/arts/ignite-kindle-release" "$T/arts/ignite-server-linux-release" "$T/work"
  printf '161335\n' >"$T/workflow.id"
  printf 'success\tcompleted\t%s\tpush\tmain\t.github/workflows/ci.yml\t161335\n' "$COMMIT" >"$T/run.tsv"
  printf '0\n' >"$T/compare.ahead"
  printf '%s\n' "$V" >"$T/version.txt"
  printf 'ignite-kindle-release\tfalse\nignite-server-linux-release\tfalse\n' >"$T/artifacts.tsv"
  printf 'stub installer\n' >"$T/arts/ignite-kindle-release/Install Ignite v$V.sh"
  (cd "$T/arts/ignite-kindle-release" && sha256sum "Install Ignite v$V.sh" >"Install Ignite v$V.sha256")
  make_linux
  KINDLE_SHA="$(sha256sum "$T/arts/ignite-kindle-release/Install Ignite v$V.sh" | awk '{print $1}')"
  LINUX_SHA="$(sha256sum "$T/arts/ignite-server-linux-release/ignite-server-$V-linux-x86_64.tar.gz" | awk '{print $1}')"
  sign_output="$("$PYTHON_REAL" "$ROOT/tools/release/kindle_release_sign.py" sign \
    --installer "$T/arts/ignite-kindle-release/Install Ignite v$V.sh" \
    --tag "$TAG" --expected-sha256 "$KINDLE_SHA" \
    --private-key "$TEST_PRIVATE_KEY" --public-key "$TEST_PUBLIC_PIN" \
    --output-dir "$T/signing")" || fail "could not create disposable promotion signature"
  KINDLE_SIGNATURE="$(printf '%s\n' "$sign_output" | sed -n 's/^signature_hex=//p')"
  [[ "$KINDLE_SIGNATURE" =~ ^[0-9a-f]{128}$ ]] || fail "disposable signature is malformed"
}

run() {
  local name="$1" cmd="$2"; shift 2
  local BASE=(
    PATH="$MOCK:$PATH"
    PYTHON_REAL="$PYTHON_REAL"
    MOCK_STATE="$T"
    MOCK_VALIDATE="${MOCK_VALIDATE:-ok}"
    GITHUB_REPOSITORY=markrai/ignite
    GITHUB_EVENT_NAME=workflow_dispatch
    GITHUB_REF=refs/heads/main
    GH_TOKEN="$SECRET"
    IGNITE_PROMOTE_TAG="$TAG"
    IGNITE_PROMOTE_RUN_ID="$RUN"
    IGNITE_PROMOTE_COMMIT="$COMMIT"
    IGNITE_PROMOTE_CONFIRM="publish $TAG from run $RUN"
    IGNITE_PROMOTE_KINDLE_SHA256="$KINDLE_SHA"
    IGNITE_PROMOTE_KINDLE_SIGNATURE="$KINDLE_SIGNATURE"
    IGNITE_PROMOTE_LINUX_SHA256="$LINUX_SHA"
    IGNITE_PROMOTE_WORK="$T/work"
  )
  set +e
  env "${BASE[@]}" "$@" bash "$PUB_RUN" "$cmd" >"$T/$name.out" 2>"$T/$name.err"
  RC=$?
  set -e
}
ok() { [ "$RC" = 0 ] || { cat "$T/$1.out" "$T/$1.err" >&2; fail "$1: expected success (rc=$RC)"; }; }
refused() {
  [ "$RC" != 0 ] || fail "$1: expected a refusal"
  grep -qF -- "$2" "$T/$1.err" || { cat "$T/$1.err" >&2; fail "$1: expected refusal \"$2\""; }
}

reset
run g-ok guard; ok g-ok
run g-pr guard GITHUB_EVENT_NAME=pull_request; refused g-pr "never publishes"
run g-branch guard GITHUB_REF=refs/heads/feature; refused g-branch "runs only from main"
run g-fork guard GITHUB_REPOSITORY=someone/ignite; refused g-fork "publishing runs only in markrai/ignite"
run g-confirm guard IGNITE_PROMOTE_CONFIRM="publish $TAG"; refused g-confirm "needs confirm"
run g-no-signature guard IGNITE_PROMOTE_KINDLE_SIGNATURE=; refused g-no-signature "signature missing or malformed"
run g-bad-signature guard IGNITE_PROMOTE_KINDLE_SIGNATURE=abcd; refused g-bad-signature "signature missing or malformed"
run g-bad-tag guard IGNITE_PROMOTE_TAG=v01.2.3; refused g-bad-tag "not a canonical release tag"
pass "guard: dispatch from main with exact confirm only"

reset
run r-ok resolve; ok r-ok
# GitHub REST example: path is file@ref (e.g. .github/workflows/build.yml@main)
printf 'success\tcompleted\t%s\tpush\tmain\t.github/workflows/ci.yml@main\t161335\n' "$COMMIT" >"$T/run.tsv"
run r-path-at-main resolve; ok r-path-at-main
printf 'success\tcompleted\t%s\tpush\tmain\t.github/workflows/ci.yml@refs/heads/main\t161335\n' "$COMMIT" >"$T/run.tsv"
run r-path-at-refs resolve; ok r-path-at-refs
printf 'success\tcompleted\t%s\tworkflow_dispatch\tmain\t.github/workflows/ci.yml@main\t161335\n' "$COMMIT" >"$T/run.tsv"
run r-path-dispatch resolve; ok r-path-dispatch
printf 'failure\tcompleted\t%s\tpush\n' "$COMMIT" >"$T/run.tsv"
run r-fail resolve; refused r-fail "did not succeed"
printf 'cancelled\tcompleted\t%s\tpush\n' "$COMMIT" >"$T/run.tsv"
run r-cancel resolve; refused r-cancel "did not succeed"
printf 'success\tin_progress\t%s\tpush\n' "$COMMIT" >"$T/run.tsv"
run r-pend resolve; refused r-pend "is not completed"
printf 'success\tcompleted\t%s\tpush\n' "$(printf 'd%.0s' {1..40})" >"$T/run.tsv"
run r-sha resolve; refused r-sha "head_sha"
printf 'success\tcompleted\t%s\tschedule\tmain\t.github/workflows/ci.yml\t161335\n' "$COMMIT" >"$T/run.tsv"
run r-ev resolve; refused r-ev "is not a CI build"
printf 'success\tcompleted\t%s\tpull_request\tfeature\t.github/workflows/ci.yml\t161335\n' "$COMMIT" >"$T/run.tsv"
run r-pr resolve; refused r-pr "unmerged PR artifacts"
printf 'success\tcompleted\t%s\tpush\tmain\t.github/workflows/nightly.yml\t999001\n' "$COMMIT" >"$T/run.tsv"
run r-wf resolve; refused r-wf "not the designated CI workflow"
printf 'success\tcompleted\t%s\tpush\tmain\t.github/workflows/nightly.yml@main\t999001\n' "$COMMIT" >"$T/run.tsv"
run r-wf-at resolve; refused r-wf-at "not the designated CI workflow"
printf 'success\tcompleted\t%s\tpush\tmain\t.github/workflows/ci.yml@main\t999001\n' "$COMMIT" >"$T/run.tsv"
run r-wf-id resolve; refused r-wf-id "workflow_id 999001"
printf 'success\tcompleted\t%s\tpush\tmain\t.github/workflows/nightly.yml@main\t161335\n' "$COMMIT" >"$T/run.tsv"
run r-wf-path resolve; refused r-wf-path "path .github/workflows/nightly.yml@main"
printf 'success\tcompleted\t%s\tpush\tfeature\t.github/workflows/ci.yml\t161335\n' "$COMMIT" >"$T/run.tsv"
run r-branch resolve; refused r-branch "is not main"
printf 'success\tcompleted\t%s\tpush\tv0.9.13\t.github/workflows/ci.yml\t161335\n' "$COMMIT" >"$T/run.tsv"
run r-tagpush resolve; refused r-tagpush "is not main"
printf 'success\tcompleted\t%s\tpush\tmain\t.github/workflows/ci.yml@main\t161335\n' "$COMMIT" >"$T/run.tsv"
printf '3\n' >"$T/compare.ahead"
run r-ahead resolve; refused r-ahead "is not on main"
printf '0\n' >"$T/compare.ahead"
printf '0.0.0\n' >"$T/version.txt"
run r-ver resolve; refused r-ver "not release"
printf '%s\n' "$V" >"$T/version.txt"
echo "gh: compare failed (HTTP 500)" >"$T/compare.err"
run r-cmp resolve; refused r-cmp "cannot verify commit"
rm -f "$T/compare.err"
printf 'ignite-kindle-release\ttrue\nignite-server-linux-release\tfalse\n' >"$T/artifacts.tsv"
run r-exp resolve; refused r-exp "missing or expired"
printf 'ignite-server-linux-release\tfalse\n' >"$T/artifacts.tsv"
run r-miss resolve; refused r-miss "ignite-kindle-release"
pass "resolve: designated CI on main, file@ref paths, version match, current Kindle+Linux artifacts only"

reset
run f-ok fetch; ok f-ok
run f-again fetch; refused f-again "already exists"
pass "fetch: downloads both artifacts once"

reset
run f-ok fetch; ok f-ok
source_before="$(sha256sum \
  "$T/work/kindle/Install Ignite v$V.sh" \
  "$T/work/kindle/Install Ignite v$V.sha256" \
  "$T/work/linux/ignite-server-$V-linux-x86_64.tar.gz" \
  "$T/work/linux/ignite-server-$V-linux-x86_64.tar.gz.sha256")"
run v-ok verify; ok v-ok
source_after="$(sha256sum \
  "$T/work/kindle/Install Ignite v$V.sh" \
  "$T/work/kindle/Install Ignite v$V.sha256" \
  "$T/work/linux/ignite-server-$V-linux-x86_64.tar.gz" \
  "$T/work/linux/ignite-server-$V-linux-x86_64.tar.gz.sha256")"
[ "$source_before" = "$source_after" ] || fail "verification changed an original CI artifact"
[ "$(find "$T/work/dist" -maxdepth 1 -type f | wc -l | tr -d ' ')" = 6 ] \
  || fail "verification must stage exactly six release assets"
for asset in \
  "Install Ignite v$V.sh" \
  "Install Ignite v$V.sha256" \
  "ignite-kindle-v$V.manifest" \
  "ignite-kindle-v$V.manifest.sig" \
  "ignite-server-$V-linux-x86_64.tar.gz" \
  "ignite-server-$V-linux-x86_64.tar.gz.sha256"; do
  [ -f "$T/work/dist/$asset" ] || fail "staged release is missing $asset"
done
echo extra >"$T/work/kindle/stray.txt"
run v-extra verify; refused v-extra "Kindle artifact file set mismatch"
reset; run f-ok fetch; ok f-ok
echo different >"$T/work/kindle/Install Ignite v$V.sh"
(cd "$T/work/kindle" && sha256sum "Install Ignite v$V.sh" >"Install Ignite v$V.sha256")
run v-ksha verify; refused v-ksha "refusing to publish untested bytes"
reset; run f-ok fetch; ok f-ok
# half-missing linux checksum
rm -f "$T/work/linux/ignite-server-$V-linux-x86_64.tar.gz.sha256"
run v-half verify; refused v-half "Linux artifact file set mismatch"
reset; run f-ok fetch; ok f-ok
printf 'UNPROVISIONED\n' >"$TEST_REPOSITORY/tools/release/kindle-release-public-key.hex"
run v-unprovisioned verify; \
  refused v-unprovisioned "official release public key is not provisioned"
cp "$TEST_PUBLIC_PIN" "$TEST_REPOSITORY/tools/release/kindle-release-public-key.hex"
reset; run f-ok fetch; ok f-ok
case "${KINDLE_SIGNATURE: -1}" in
  0) BAD_SIGNATURE="${KINDLE_SIGNATURE:0:127}1" ;;
  *) BAD_SIGNATURE="${KINDLE_SIGNATURE:0:127}0" ;;
esac
run v-bad-auth verify IGNITE_PROMOTE_KINDLE_SIGNATURE="$BAD_SIGNATURE"; \
  refused v-bad-auth "signature verification failed"
pass "verify: original four CI files are unchanged; only valid authenticated metadata makes six assets"

reset
run f-ok fetch; ok f-ok
run v-ok verify; ok v-ok
run p-ineligible-repo publish GITHUB_REPOSITORY=someone/ignite; \
  refused p-ineligible-repo "publishing runs only in markrai/ignite"
run p-ineligible-branch publish GITHUB_REF=refs/heads/feature; \
  refused p-ineligible-branch "runs only from main"
run p-ineligible-confirm publish IGNITE_PROMOTE_CONFIRM="publish $TAG"; \
  refused p-ineligible-confirm "needs confirm"
printf 'failure\tcompleted\t%s\tpush\tmain\t.github/workflows/ci.yml\t161335\n' "$COMMIT" >"$T/run.tsv"
run p-ineligible-ci publish; refused p-ineligible-ci "did not succeed"
[ ! -f "$T/publish.log" ] \
  || fail "direct publish reached gh release create with invalid eligibility"
pass "direct publish: repository, branch, confirmation, and CI-run eligibility are mandatory"

reset
run p-no-fetch publish
[ "$RC" != 0 ] || fail "direct publish without verified inputs must fail"
[ ! -f "$T/publish.log" ] || fail "direct publish bypass reached gh release create"
run f-ok fetch; ok f-ok
case "${KINDLE_SIGNATURE: -1}" in
  0) BAD_SIGNATURE="${KINDLE_SIGNATURE:0:127}1" ;;
  *) BAD_SIGNATURE="${KINDLE_SIGNATURE:0:127}0" ;;
esac
run p-bad-auth publish IGNITE_PROMOTE_KINDLE_SIGNATURE="$BAD_SIGNATURE"; \
  refused p-bad-auth "signature verification failed"
[ ! -f "$T/publish.log" ] || fail "invalid authentication reached gh release create"
pass "publish subcommand re-verifies and fails before any GitHub publication call"

reset
run f-ok fetch; ok f-ok
run p-ok publish; ok p-ok
grep -q "create $TAG" "$T/publish.log" || { cat "$T/publish.log" >&2; fail "publish did not create the release"; }
grep -q -- "--target $COMMIT" "$T/publish.log" || fail "publish must target the recorded commit"
for asset in \
  "Install Ignite v$V.sh" \
  "Install Ignite v$V.sha256" \
  "ignite-kindle-v$V.manifest" \
  "ignite-kindle-v$V.manifest.sig" \
  "ignite-server-$V-linux-x86_64.tar.gz" \
  "ignite-server-$V-linux-x86_64.tar.gz.sha256"; do
  grep -qF "$asset" "$T/publish.log" || fail "published release is missing $asset"
done
[ -f "$T/tag.tsv" ] || fail "publish must record the created tag"
touch "$T/release-exists"
run p-exists publish; refused p-exists "already exists; its assets are not replaced"
pass "publish: creates release at recorded commit; never replaces"

reset
run f-ok fetch; ok f-ok
printf 'commit\t%s\n' "$COMMIT" >"$T/tag.tsv"
run p-tag-ok publish; ok p-tag-ok
printf 'commit\t%s\n' "$(printf 'e%.0s' {1..40})" >"$T/tag.tsv"
run p-tag-wrong publish; refused p-tag-wrong "not recorded commit"
rm -f "$T/tag.tsv"
printf '500\n' >"$T/tag.http"
run p-tag-500 publish; refused p-tag-500 "HTTP 500"
rm -f "$T/tag.http"
touch "$T/tag.network"
run p-tag-net publish; refused p-tag-net "no HTTP status"
rm -f "$T/tag.network"
touch "$T/tag.no-create"
run p-tag-after-404 publish; refused p-tag-after-404 "missing after publish"
rm -f "$T/tag.no-create" "$T/tag.tsv"
printf 'commit\t%s\n' "$(printf 'f%.0s' {1..40})" >"$T/tag.after.tsv"
run p-tag-after-wrong publish; refused p-tag-after-wrong "not recorded commit"
printf 'tag\t%s\n' "$(printf 'b%.0s' {1..40})" >"$T/tag.tsv"
echo "gh: peel failed (HTTP 500)" >"$T/tagobj.err"
run p-peel publish; refused p-peel "cannot peel annotated tag"
pass "publish: tag lookup is fail-closed; tag must point at the approved commit"

reset
run all-ok all; ok all-ok
grep -q "release create" "$T/publish.log" || fail "all did not publish"
pass "all: guard → resolve → fetch → verify → publish"

grep -qF "$SECRET" "$T/argv.log" && fail "the workflow token reached a command line"
while IFS= read -r private_line; do
  [ -n "$private_line" ] || continue
  if grep -qF -- "$private_line" "$T/argv.log" "$T"/*.out "$T"/*.err; then
    fail "private-key contents reached a command line or promotion log"
  fi
done <"$TEST_PRIVATE_KEY"
pass "workflow token and private-key contents never appear in mocked GitHub argv"

"$PYTHON_REAL" "$ROOT/tools/ci/check_version_sync.py" || fail "version sync on this tree"
"$PYTHON_REAL" "$ROOT/tools/ci/test_check_version_sync.py" || fail "version mismatch fixture"
pass "version files agree; mismatch fixture fails closed"

grep -vE '^\s*#' "$ROOT/scripts/kindle/package-release.sh" | grep -q SkipBuild \
  && fail "package-release.sh must not offer SkipBuild"
grep -qE '>\s*"?(\$PINNED|.*PINNED\.txt)|tee +.*PINNED|sed -i.*PINNED' \
  "$ROOT/tools/dropbear/docker_build.sh" \
  && fail "docker_build.sh must not rewrite PINNED.txt"
if grep -n '2>/dev/null' "$PUB" | grep -v 'inherit_errexit' | grep -q .; then
  fail "promote-release.sh must not hide GitHub API errors"
fi
grep -q 'if existing=' "$PUB" && fail "tag lookup must not run inside a swallowing if-assignment"
grep -q 'CI_WORKFLOW=.github/workflows/ci.yml' "$PUB" || fail "promote must pin the designated CI workflow"
grep -q '_is_ci_run_path' "$PUB" || fail "promote must accept GitHub file@ref run paths"
grep -q 'actions/workflows/$CI_WORKFLOW_FILE' "$PUB" || fail "promote must resolve the designated workflow by id"
grep -q 'IGNITE_DROPBEAR_FORCE' "$CI" || fail "CI Kindle packaging must force a Dropbear rebuild"
pass "official Kindle path: no SkipBuild; Dropbear pins are read-only"

# docker_build.sh cache hit must not invoke docker (SkipBuild / local reuse).
# Output must live under the repo: docker_build.sh rejects paths outside it.
DBT="$ROOT/target/dropbear-cache-test"
rm -rf "$DBT"
mkdir -p "$DBT/bin"
printf 'stub-dropbear' >"$DBT/dropbearmulti"
chmod +x "$DBT/dropbearmulti"
db_sha="$(sha256sum "$DBT/dropbearmulti" | awk '{print $1}')"
db_bytes="$(wc -c <"$DBT/dropbearmulti" | tr -d ' ')"
printf 'elf_sha256=%s\nelf_bytes=%s\n' "$db_sha" "$db_bytes" >"$DBT/PINNED.txt"
printf '%s\n' "$db_sha" >"$DBT/elf.sha256"
printf 'name=dropbearmulti\nrole=test-stub\n' >"$DBT/provenance.txt"
cat >"$DBT/bin/docker" <<STUB
#!/bin/sh
echo docker-invoked >>"$DBT/docker.log"
echo unexpected-docker >&2
exit 90
STUB
chmod +x "$DBT/bin/docker"
PATH="$DBT/bin:$PATH" IGNITE_DROPBEAR_FORCE='' IGNITE_DROPBEAR_OUT="$DBT" \
  IGNITE_DROPBEAR_PINNED="$DBT/PINNED.txt" \
  bash "$ROOT/tools/dropbear/docker_build.sh" >/dev/null \
  || fail "docker_build.sh cache hit must succeed without invoking docker"
[ -f "$DBT/docker.log" ] && fail "cache hit must not invoke docker"
PATH="$DBT/bin:$PATH" IGNITE_DROPBEAR_FORCE=1 IGNITE_DROPBEAR_OUT="$DBT" \
  IGNITE_DROPBEAR_PINNED="$DBT/PINNED.txt" \
  bash "$ROOT/tools/dropbear/docker_build.sh" >/dev/null 2>"$DBT/force.err" \
  && fail "FORCE=1 must invoke docker (stub refuses)"
grep -q docker-invoked "$DBT/docker.log" || fail "FORCE=1 must invoke docker"
pass "Dropbear pin cache hits without docker; FORCE=1 rebuilds"

echo "PASS: promote-release guards"
