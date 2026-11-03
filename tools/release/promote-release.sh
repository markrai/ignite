#!/usr/bin/env bash
# Promote a previously built CI artifact set to a GitHub Release without rebuilding.
# Run only by .github/workflows/release-promote.yml (manual approval from main).
#
#   promote-release.sh guard     repository / event / confirm
#   promote-release.sh resolve   designated CI workflow on main, version match
#   promote-release.sh fetch     download Kindle + Linux artifacts from that run
#   promote-release.sh verify    CI files + release-v1 + authenticated six-asset gate
#   promote-release.sh dry-run   guard → resolve → fetch → verify (no GitHub mutation)
#   promote-release.sh publish   re-verify, then create the GitHub Release
#   promote-release.sh all       guard → resolve → fetch → verify → publish
#
# Authenticated Kindle publication (Slice 3k) additionally requires offline
# ceremony outputs under IGNITE_PROMOTE_AUTHENTICATED_ASSETS_DIR:
#   ignite-kindle-vX.Y.Z.publication-evidence
#   ignite-kindle-vX.Y.Z.publication-binding
#   ignite-kindle-vX.Y.Z.publication-binding.sig
#
# verify() stages the complete nine-file candidate into $DIST, authenticates the
# six Kindle assets from that staged candidate (repository pin only), and writes
# a SHA-256 inventory. publish rechecks the inventory immediately before
# gh release create. Staging+inventory is not race-free against a hostile
# concurrent writer between the final recheck and upload.
#
# Production promotion remains blocked while the public-key pin is UNPROVISIONED.
# Fixture rehearsal is not publication authority. No alternate verifier or
# public-key substitution is accepted on the shipping path.
#
# Environment (tokens never on argv): IGNITE_PROMOTE_TAG, IGNITE_PROMOTE_RUN_ID,
# IGNITE_PROMOTE_COMMIT, IGNITE_PROMOTE_CONFIRM, IGNITE_PROMOTE_KINDLE_SHA256,
# IGNITE_PROMOTE_LINUX_SHA256, IGNITE_PROMOTE_KINDLE_SIGNATURE,
# IGNITE_PROMOTE_AUTHENTICATED_ASSETS_DIR, IGNITE_PROMOTE_WORK, GH_TOKEN,
# GITHUB_REPOSITORY, GITHUB_EVENT_NAME, GITHUB_REF.
# IGNITE_PROMOTE_PUBLIC_KEY_FILE is dry-run rehearsal only (forbidden for
# publish/all). It never substitutes for the promote-gate repository pin.
set -euo pipefail
shopt -s inherit_errexit 2>/dev/null || true

REPO=markrai/ignite
ARCH=x86_64
CI_WORKFLOW=.github/workflows/ci.yml
CI_WORKFLOW_FILE=ci.yml
MAIN_BRANCH=main
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SIGN_TOOL="$ROOT/tools/release/kindle_release_sign.py"
# Default: repository pin only. Alternate-key override is permitted solely for
# the nonpublishing dry-run rehearsal below — never for publish/all.
PUBLIC_KEY_FILE="$ROOT/tools/release/kindle-release-public-key.hex"

die() { echo "promote-release.sh: $*" >&2; exit 1; }

case "${1:-}" in
  dry-run)
    if [ -n "${IGNITE_PROMOTE_PUBLIC_KEY_FILE:-}" ]; then
      [ -f "$IGNITE_PROMOTE_PUBLIC_KEY_FILE" ] \
        || die "IGNITE_PROMOTE_PUBLIC_KEY_FILE not found"
      PUBLIC_KEY_FILE="$IGNITE_PROMOTE_PUBLIC_KEY_FILE"
      echo "promote-rehearsal-public-key-override=true"
      echo "promote-rehearsal-public-key-is-not-production-pin-authority=true"
    fi
    ;;
  publish|all)
    if [ -n "${IGNITE_PROMOTE_PUBLIC_KEY_FILE:-}" ]; then
      die "IGNITE_PROMOTE_PUBLIC_KEY_FILE is forbidden for $1 (dry-run rehearsal only)"
    fi
    ;;
esac

TAG="${IGNITE_PROMOTE_TAG:-}"
RUN_ID="${IGNITE_PROMOTE_RUN_ID:-}"
COMMIT="${IGNITE_PROMOTE_COMMIT:-}"
CONFIRM="${IGNITE_PROMOTE_CONFIRM:-}"
KINDLE_SHA="${IGNITE_PROMOTE_KINDLE_SHA256:-}"
LINUX_SHA="${IGNITE_PROMOTE_LINUX_SHA256:-}"
KINDLE_SIGNATURE="${IGNITE_PROMOTE_KINDLE_SIGNATURE:-}"
AUTH_ASSETS_DIR="${IGNITE_PROMOTE_AUTHENTICATED_ASSETS_DIR:-}"
WORK="${IGNITE_PROMOTE_WORK:-}"

VERSION="$(python3 "$SIGN_TOOL" version --tag "$TAG")" \
  || die "not a canonical release tag: ${TAG:-none}"
[[ "$RUN_ID" =~ ^[1-9][0-9]*$ ]] || die "run id must be a positive integer"
[[ "$COMMIT" =~ ^[0-9a-f]{40}$ ]] || die "commit must be a 40-character lowercase SHA-1"
[[ "$KINDLE_SHA" =~ ^[0-9a-f]{64}$ ]] || die "kindle sha256 missing or malformed"
[[ "$LINUX_SHA" =~ ^[0-9a-f]{64}$ ]] || die "linux sha256 missing or malformed"
[[ "$KINDLE_SIGNATURE" =~ ^[0-9a-f]{128}$ ]] || die "kindle signature missing or malformed"
[ -n "$WORK" ] || die "IGNITE_PROMOTE_WORK is required"

SFX="Install Ignite v$VERSION.sh"
SFX_SUM="Install Ignite v$VERSION.sha256"
LINUX="ignite-server-$VERSION-linux-$ARCH.tar.gz"
LINUX_SUM="$LINUX.sha256"
DIST="$WORK/dist"
MANIFEST="ignite-kindle-v$VERSION.manifest"
MANIFEST_SIG="$MANIFEST.sig"
EVIDENCE="ignite-kindle-v$VERSION.publication-evidence"
BINDING="ignite-kindle-v$VERSION.publication-binding"
BINDING_SIG="$BINDING.sig"
AUTH_DIR="$WORK/authenticated-metadata"
PREFLIGHT="$ROOT/deployment/kindle/authenticated_publication_preflight.py"
INVENTORY="$WORK/release-candidate.sha256"
# Test-harness only: if present, names a deterministic mutation applied after
# staging and before authentication / before publish. Shipping never creates it.
TEST_HOOK_STAGED="$WORK/.promote-test-mutate-staged"
TEST_HOOK_INVENTORY="$WORK/.promote-test-mutate-before-inventory"
TEST_HOOK_PUBLISH="$WORK/.promote-test-mutate-before-publish"

_expected_dist_list() {
  printf '%s\n' \
    "$SFX" "$SFX_SUM" \
    "$MANIFEST" "$MANIFEST_SIG" \
    "$BINDING" "$BINDING_SIG" "$EVIDENCE" \
    "$LINUX" "$LINUX_SUM" | LC_ALL=C sort
}

_refuse_symlinks_in_dist() {
  local f
  for f in "$DIST"/*; do
    [ -e "$f" ] || continue
    if [ -L "$f" ]; then
      die "symlink refused in release candidate: $(basename "$f")"
    fi
    if [ ! -f "$f" ]; then
      die "non-regular file refused in release candidate: $(basename "$f")"
    fi
  done
}

_write_release_inventory() {
  local name
  _refuse_symlinks_in_dist
  [ "$(ls -1 "$DIST" | LC_ALL=C sort)" = "$(_expected_dist_list)" ] \
    || die "release candidate file set mismatch before inventory"
  : >"$INVENTORY" || die "cannot create release candidate inventory"
  while IFS= read -r name; do
    [ -n "$name" ] || continue
    (cd "$DIST" && sha256sum -- "$name") >>"$INVENTORY" \
      || die "inventory hash failed for $name"
  done < <(_expected_dist_list)
  [ -s "$INVENTORY" ] || die "release candidate inventory is empty"
  echo "inventory: wrote $INVENTORY"
}

_recheck_release_inventory() {
  [ -f "$INVENTORY" ] || die "release candidate inventory missing"
  _refuse_symlinks_in_dist
  [ "$(ls -1 "$DIST" | LC_ALL=C sort)" = "$(_expected_dist_list)" ] \
    || die "release candidate file set changed before publication"
  (cd "$DIST" && sha256sum -c "$INVENTORY") \
    || die "release candidate bytes changed since authentication inventory"
}

_apply_test_hook() {
  local hook="$1"
  local action
  [ -f "$hook" ] || return 0
  action="$(tr -d '\r\n' <"$hook")"
  rm -f "$hook"
  case "$action" in
    evidence)
      printf '\n#promote-test-mutate\n' >>"$DIST/$EVIDENCE"
      echo "promote-test-hook: mutated staged evidence"
      ;;
    binding-sig)
      printf '\n#promote-test-mutate\n' >>"$DIST/$BINDING_SIG"
      echo "promote-test-hook: mutated staged binding signature"
      ;;
    scriptlet)
      printf '\n#promote-test-mutate\n' >>"$DIST/$SFX"
      echo "promote-test-hook: mutated staged Scriptlet"
      ;;
    binding)
      printf '\n#promote-test-mutate\n' >>"$DIST/$BINDING"
      echo "promote-test-hook: mutated staged binding"
      ;;
    extra-file)
      printf 'stray\n' >"$DIST/extra-file.txt"
      echo "promote-test-hook: added unexpected staged file"
      ;;
    *)
      die "unknown promote test hook action: ${action:-none}"
      ;;
  esac
}

guard() {
  [ "${GITHUB_REPOSITORY:-}" = "$REPO" ] || die "publishing runs only in $REPO (got ${GITHUB_REPOSITORY:-none})"
  [ "${GITHUB_EVENT_NAME:-}" = workflow_dispatch ] || die "event ${GITHUB_EVENT_NAME:-none} never publishes"
  [ "${GITHUB_REF:-}" = refs/heads/main ] || die "promote-publish runs only from main (ref ${GITHUB_REF:-none})"
  [ "$CONFIRM" = "publish $TAG from run $RUN_ID" ] \
    || die "needs confirm = \"publish $TAG from run $RUN_ID\""
  echo "guard: $TAG run=$RUN_ID ok"
}

# GitHub documents run.path as ".github/workflows/build.yml@main" (file@ref).
# Accept the bare file path or that path plus a non-empty @ref suffix.
_is_ci_run_path() {
  case "$1" in
    "$CI_WORKFLOW" | "$CI_WORKFLOW"@?*) return 0 ;;
    *) return 1 ;;
  esac
}

resolve() {
  local meta conclusion status sha event branch path wf_id names ahead ver want_id
  meta="$(gh api "repos/$REPO/actions/runs/$RUN_ID" \
    --jq '[.conclusion, .status, .head_sha, .event, .head_branch, .path, .workflow_id] | @tsv')" \
    || die "cannot read workflow run $RUN_ID"
  IFS=$'\t' read -r conclusion status sha event branch path wf_id <<<"$meta"
  [ "$status" = completed ] || die "run $RUN_ID is not completed (status ${status:-none})"
  [ "$conclusion" = success ] || die "run $RUN_ID did not succeed (conclusion ${conclusion:-none})"
  [ "$sha" = "$COMMIT" ] || die "run $RUN_ID head_sha $sha != recorded commit $COMMIT"
  want_id="$(gh api "repos/$REPO/actions/workflows/$CI_WORKFLOW_FILE" --jq .id)" \
    || die "cannot read designated CI workflow $CI_WORKFLOW_FILE"
  [[ "$want_id" =~ ^[1-9][0-9]*$ ]] || die "designated CI workflow id is malformed (${want_id:-none})"
  [ "$wf_id" = "$want_id" ] \
    || die "run $RUN_ID is not the designated CI workflow (workflow_id ${wf_id:-none})"
  _is_ci_run_path "$path" \
    || die "run $RUN_ID is not the designated CI workflow (path ${path:-none})"
  case "$event" in
    push | workflow_dispatch) ;;
    pull_request) die "run $RUN_ID is a pull_request; unmerged PR artifacts are not published" ;;
    *) die "run $RUN_ID event $event is not a CI build of release artifacts" ;;
  esac
  [ "$branch" = "$MAIN_BRANCH" ] \
    || die "run $RUN_ID head_branch ${branch:-none} is not $MAIN_BRANCH"
  ahead="$(gh api "repos/$REPO/compare/${MAIN_BRANCH}...$COMMIT" --jq .ahead_by)" \
    || die "cannot verify commit $COMMIT is on $MAIN_BRANCH"
  [ "$ahead" = 0 ] \
    || die "commit $COMMIT is not on $MAIN_BRANCH (ahead_by $ahead)"
  ver="$(gh api -H "Accept: application/vnd.github.raw" \
    "repos/$REPO/contents/ignite/VERSION?ref=$COMMIT")" \
    || die "cannot read ignite/VERSION at $COMMIT"
  ver="$(printf '%s' "$ver" | tr -d '[:space:]')"
  [ "$ver" = "$VERSION" ] \
    || die "ignite/VERSION at $COMMIT is ${ver:-none}, not release $VERSION"
  names="$(gh api "repos/$REPO/actions/runs/$RUN_ID/artifacts" \
    --jq '.artifacts[] | [.name, (.expired|tostring)] | @tsv')" \
    || die "cannot list artifacts for run $RUN_ID"
  echo "$names" | grep -qx $'ignite-kindle-release\tfalse' \
    || die "run $RUN_ID has no current ignite-kindle-release artifact (missing or expired)"
  echo "$names" | grep -qx $'ignite-server-linux-release\tfalse' \
    || die "run $RUN_ID has no current ignite-server-linux-release artifact (missing or expired)"
  echo "resolve: run $RUN_ID success head_sha=$sha event=$event branch=$branch"
}

fetch() {
  [ ! -e "$WORK/kindle" ] || die "work kindle directory already exists: $WORK/kindle"
  [ ! -e "$WORK/linux" ] || die "work linux directory already exists: $WORK/linux"
  mkdir -p "$WORK/kindle" "$WORK/linux" "$DIST"
  gh run download "$RUN_ID" -R "$REPO" -n ignite-kindle-release -D "$WORK/kindle" \
    || die "cannot download ignite-kindle-release from run $RUN_ID"
  gh run download "$RUN_ID" -R "$REPO" -n ignite-server-linux-release -D "$WORK/linux" \
    || die "cannot download ignite-server-linux-release from run $RUN_ID"
  echo "fetch: artifacts from run $RUN_ID"
}

_list_files() {
  (cd "$1" && find . -type f | sed 's|^\./||' | LC_ALL=C sort)
}

verify() {
  # Fail-closed without relying on errexit: Bash suppresses set -e inside
  # functions invoked from conditionals, $(...), or pipelines. Every
  # security-relevant step must use explicit || die / nonzero exit.
  local kfiles lfiles have_k have_l source_hashes
  kfiles="$(_list_files "$WORK/kindle")" \
    || die "cannot list Kindle artifact files"
  lfiles="$(_list_files "$WORK/linux")" \
    || die "cannot list Linux artifact files"
  [ "$kfiles" = "$(printf '%s\n' "$SFX" "$SFX_SUM")" ] \
    || die "Kindle artifact file set mismatch: $kfiles"
  [ "$lfiles" = "$(printf '%s\n' "$LINUX" "$LINUX_SUM")" ] \
    || die "Linux artifact file set mismatch: $lfiles"

  (cd "$WORK/kindle" && sha256sum -c "$SFX_SUM") \
    || die "Kindle Scriptlet checksum sidecar validation failed"
  (cd "$WORK/linux" && sha256sum -c "$LINUX_SUM") \
    || die "Linux archive checksum sidecar validation failed"

  have_k="$(sha256sum "$WORK/kindle/$SFX" | awk '{print $1}')" \
    || die "cannot hash Kindle installer"
  have_l="$(sha256sum "$WORK/linux/$LINUX" | awk '{print $1}')" \
    || die "cannot hash Linux archive"
  [ -n "$have_k" ] || die "Kindle installer sha256 empty"
  [ -n "$have_l" ] || die "Linux archive sha256 empty"
  [ "$have_k" = "$KINDLE_SHA" ] \
    || die "Kindle installer sha256 $have_k != recorded $KINDLE_SHA (refusing to publish untested bytes)"
  [ "$have_l" = "$LINUX_SHA" ] \
    || die "Linux bundle sha256 $have_l != recorded $LINUX_SHA (refusing to publish untested bytes)"
  source_hashes="$(sha256sum \
    "$WORK/kindle/$SFX" "$WORK/kindle/$SFX_SUM" \
    "$WORK/linux/$LINUX" "$WORK/linux/$LINUX_SUM")" \
    || die "cannot fingerprint source CI artifacts"

  python3 "$ROOT/deployment/kindle/validate_stage.py" \
    --sfx "$WORK/kindle/$SFX" --version "$VERSION" \
    || die "Kindle installer failed validate_stage.py"

  # --force-local: paths like C:/... must not be parsed as host:file on Git Bash.
  tar --force-local -xzf "$WORK/linux/$LINUX" -C "$WORK" \
    || die "Linux archive extraction failed"
  grep -qx "version=$VERSION" "$WORK/${LINUX%.tar.gz}/.ignite-bundle" \
    || die "bundle metadata version is not $VERSION"
  grep -qx "arch=$ARCH" "$WORK/${LINUX%.tar.gz}/.ignite-bundle" \
    || die "bundle metadata arch is not $ARCH"
  [ "$("$WORK/${LINUX%.tar.gz}/scribe-server" --version | tr -d '[:space:]')" = "$VERSION" ] \
    || die "bundle binary is not $VERSION"
  rm -rf "${WORK:?}/${LINUX%.tar.gz}"

  rm -rf "$AUTH_DIR"
  python3 "$SIGN_TOOL" verify \
    --installer "$WORK/kindle/$SFX" \
    --tag "$TAG" \
    --expected-sha256 "$KINDLE_SHA" \
    --signature-hex "$KINDLE_SIGNATURE" \
    --public-key "$PUBLIC_KEY_FILE" \
    --output-dir "$AUTH_DIR" \
    || die "Kindle release metadata signature verification failed"

  [ -n "$AUTH_ASSETS_DIR" ] \
    || die "IGNITE_PROMOTE_AUTHENTICATED_ASSETS_DIR is required for authenticated Kindle publication"
  [ -d "$AUTH_ASSETS_DIR" ] \
    || die "authenticated assets directory missing: $AUTH_ASSETS_DIR"
  for req in "$EVIDENCE" "$BINDING" "$BINDING_SIG"; do
    [ -f "$AUTH_ASSETS_DIR/$req" ] \
      || die "missing offline authenticated asset: $req"
    [ ! -L "$AUTH_ASSETS_DIR/$req" ] \
      || die "symlink refused for offline authenticated asset: $req"
  done

  [ "$source_hashes" = "$(sha256sum \
    "$WORK/kindle/$SFX" "$WORK/kindle/$SFX_SUM" \
    "$WORK/linux/$LINUX" "$WORK/linux/$LINUX_SUM")" ] \
    || die "source CI artifacts changed during verification"

  # Stage the complete publication candidate first, then authenticate those bytes.
  rm -rf "$DIST" "$INVENTORY"
  mkdir -p "$DIST" || die "cannot create staging dist directory"
  cp -a \
    "$WORK/kindle/$SFX" "$WORK/kindle/$SFX_SUM" \
    "$AUTH_DIR/$MANIFEST" "$AUTH_DIR/$MANIFEST_SIG" \
    "$AUTH_ASSETS_DIR/$EVIDENCE" \
    "$AUTH_ASSETS_DIR/$BINDING" \
    "$AUTH_ASSETS_DIR/$BINDING_SIG" \
    "$WORK/linux/$LINUX" "$WORK/linux/$LINUX_SUM" \
    "$DIST/" \
    || die "cannot stage nine-file release candidate"
  [ "$(ls -1 "$DIST" | LC_ALL=C sort)" = "$(_expected_dist_list)" ] \
    || die "staging dist does not contain the authenticated Kindle bundle plus Linux pair"

  # Deterministic harness mutation after staging / before authentication.
  _apply_test_hook "$TEST_HOOK_STAGED"

  # Authenticate the staged Kindle six under this tree's repository pin only.
  python3 "$PREFLIGHT" promote-gate \
    --release-candidate-dir "$DIST" \
    --tag "$TAG" \
    --source-revision "$COMMIT" \
    --expected-workflow "$CI_WORKFLOW" \
    || die "authenticated Kindle publication preflight refused"

  # Deterministic harness mutation after authentication / before inventory.
  _apply_test_hook "$TEST_HOOK_INVENTORY"

  _write_release_inventory \
    || die "release inventory creation failed"
  echo "verify: CI assets unchanged; staged candidate authenticated; inventory recorded"
}

# Sets GH_HTTP_CODE / GH_HTTP_BODY from `gh api`. Returns 0 on HTTP 200, 1 on
# HTTP 404. Any transport failure or other status exits the process — callers
# must invoke this directly (never inside $(...)) so die() cannot be swallowed.
_gh_http() {
  local err rc msg
  err="$(mktemp)"
  GH_HTTP_BODY=""
  GH_HTTP_CODE=""
  rc=0
  GH_HTTP_BODY="$(gh api "$@" 2>"$err")" || rc=$?
  if [ "$rc" -eq 0 ]; then
    GH_HTTP_CODE=200
    rm -f "$err"
    return 0
  fi
  GH_HTTP_CODE="$(sed -n 's/.*(HTTP \([0-9][0-9][0-9]\)).*/\1/p' "$err" | tail -n1)"
  msg="$(tr '\n' ' ' <"$err")"
  rm -f "$err"
  case "$GH_HTTP_CODE" in
    404) return 1 ;;
    "") die "GitHub API failed (no HTTP status): ${msg:-unknown error}" ;;
    *) die "GitHub API failed (HTTP $GH_HTTP_CODE): $msg" ;;
  esac
}

# Sets TAG_COMMIT to the peeled commit. Returns 0 if the tag exists, 1 if it is
# genuinely absent (HTTP 404). Lookup/peel failures exit; they are never "missing".
_tag_commit() {
  TAG_COMMIT=""
  local obj type sha
  if _gh_http --jq '[.object.type, .object.sha] | @tsv' "repos/$REPO/git/ref/tags/$TAG"; then
    obj="$GH_HTTP_BODY"
  else
    return 1
  fi
  IFS=$'\t' read -r type sha <<<"$obj"
  case "$type" in
    commit)
      TAG_COMMIT="$sha"
      ;;
    tag)
      TAG_COMMIT="$(gh api --jq .object.sha "repos/$REPO/git/tags/$sha")" \
        || die "cannot peel annotated tag $TAG"
      ;;
    *) die "unexpected tag object type ${type:-none} for $TAG" ;;
  esac
  [[ "$TAG_COMMIT" =~ ^[0-9a-f]{40}$ ]] || die "tag $TAG did not resolve to a commit SHA"
  return 0
}

_require_tag_commit() {
  if _tag_commit; then
    [ "$TAG_COMMIT" = "$COMMIT" ] \
      || die "git tag $TAG points at $TAG_COMMIT, not recorded commit $COMMIT"
    return 0
  fi
  die "$1"
}

_publish_verified() {
  if _gh_http --jq .tag_name "repos/$REPO/releases/tags/$TAG"; then
    die "release $TAG already exists; its assets are not replaced"
  fi
  if _tag_commit; then
    [ "$TAG_COMMIT" = "$COMMIT" ] \
      || die "git tag $TAG points at $TAG_COMMIT, not recorded commit $COMMIT"
  fi
  # Deterministic harness mutation immediately before the inventory recheck.
  _apply_test_hook "$TEST_HOOK_PUBLISH"
  _recheck_release_inventory
  pre=()
  case "$TAG" in *-*) pre=(--prerelease) ;; esac
  notes="$WORK/notes.md"
  cat >"$notes" <<EOF
Ignite $VERSION

**Kindle application (primary):** \`$SFX\` and \`$SFX_SUM\`.
Authenticated updater metadata: \`$MANIFEST\`, \`$MANIFEST_SIG\`,
\`$EVIDENCE\`, \`$BINDING\`, and \`$BINDING_SIG\`.
Copy the installer into the Kindle Documents folder over USB. See [deployment/kindle/README.txt](https://github.com/markrai/ignite/blob/main/deployment/kindle/README.txt).

**Optional Ignite Server (Linux x86_64):** \`$LINUX\` and \`$LINUX_SUM\`.
Self-hosting: [docs/self-hosting.md](https://github.com/markrai/ignite/blob/main/docs/self-hosting.md). Update an installed server with: \`sudo ignite-update\`.

These files are the CI artifacts from run $RUN_ID at commit \`$COMMIT\`, promoted without rebuilding.
EOF
  gh release create "$TAG" "$DIST"/* -R "$REPO" --target "$COMMIT" \
    --title "Ignite $VERSION" --notes-file "$notes" "${pre[@]}" \
    || die "gh release create failed"
  _require_tag_commit "git tag $TAG missing after publish"
  echo "published: $TAG from run $RUN_ID commit $COMMIT"
}

publish() {
  # Direct publication must prove the same authorization and CI-run eligibility as `all`.
  guard
  resolve
  verify
  _publish_verified
}

dry_run() {
  # Read-only against GitHub mutation: resolve/fetch are GETs; never release create.
  # Emit stable phase markers so callers can distinguish pin refusal from earlier failure.
  #
  # Do NOT capture verify via $(verify) / if verify / verify || — those contexts
  # suppress errexit inside the function. verify() is fail-closed via explicit
  # || die; we only disable errexit around the top-level call so we can classify
  # the exit status (pin boundary vs other) without aborting before markers.
  local verify_log verify_rc
  echo "promote-phase=guard"
  guard
  echo "promote-phase=guard-ok"
  echo "promote-phase=resolve"
  resolve
  echo "promote-phase=resolve-ok"
  echo "promote-phase=fetch"
  fetch
  echo "promote-phase=fetch-ok"
  echo "promote-phase=verify"
  verify_log="$(mktemp)" || die "cannot create verify log"
  # Subshell so die/exit inside verify cannot skip refusal classification below.
  # Re-enable errexit inside the subshell; verify() also guards with || die.
  set +e
  (
    set -euo pipefail
    shopt -s inherit_errexit 2>/dev/null || true
    verify
  ) >"$verify_log" 2>&1
  verify_rc=$?
  set -e
  cat "$verify_log"
  if [ "$verify_rc" -ne 0 ]; then
    # Classify only the official-pin unavailable boundary — not a generic substring.
    if grep -Eq \
      'production signing unavailable because the pin is UNPROVISIONED|official release public key is not provisioned' \
      "$verify_log"; then
      echo "promote-shell-refusal=official-pin-unprovisioned"
    else
      echo "promote-shell-refusal=other"
    fi
    echo "promote-phase=verify-failed"
    rm -f "$verify_log"
    exit "$verify_rc"
  fi
  rm -f "$verify_log"
  echo "promote-phase=verify-ok"
  echo "dry-run: would publish $TAG from run $RUN_ID commit $COMMIT with:"
  ls -1 "$DIST" | LC_ALL=C sort | sed 's/^/  /'
  echo "dry-run: no GitHub release create performed"
}

all() {
  guard
  resolve
  fetch
  verify
  _publish_verified
}

case "${1:-}" in
  guard) guard ;;
  resolve) resolve ;;
  fetch) fetch ;;
  verify) verify ;;
  dry-run) dry_run ;;
  publish) publish ;;
  all) all ;;
  *) die "usage: $0 guard|resolve|fetch|verify|dry-run|publish|all" ;;
esac
