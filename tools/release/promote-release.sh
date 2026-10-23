#!/usr/bin/env bash
# Promote a previously built CI artifact set to a GitHub Release without rebuilding.
# Run only by .github/workflows/release-promote.yml (manual approval from main).
#
#   promote-release.sh guard     repository / event / confirm
#   promote-release.sh resolve   designated CI workflow on main, version match
#   promote-release.sh fetch     download Kindle + Linux artifacts from that run
#   promote-release.sh verify    exact four files, checksums, recorded SHA-256s
#   promote-release.sh publish   create the GitHub Release from those bytes
#   promote-release.sh all       guard → resolve → fetch → verify → publish
#
# Environment (tokens never on argv): IGNITE_PROMOTE_TAG, IGNITE_PROMOTE_RUN_ID,
# IGNITE_PROMOTE_COMMIT, IGNITE_PROMOTE_CONFIRM, IGNITE_PROMOTE_KINDLE_SHA256,
# IGNITE_PROMOTE_LINUX_SHA256, IGNITE_PROMOTE_WORK, GH_TOKEN, GITHUB_REPOSITORY,
# GITHUB_EVENT_NAME, GITHUB_REF.
set -euo pipefail
shopt -s inherit_errexit 2>/dev/null || true

REPO=markrai/ignite
ARCH=x86_64
CI_WORKFLOW=.github/workflows/ci.yml
CI_WORKFLOW_FILE=ci.yml
MAIN_BRANCH=main

die() { echo "promote-release.sh: $*" >&2; exit 1; }

TAG="${IGNITE_PROMOTE_TAG:-}"
RUN_ID="${IGNITE_PROMOTE_RUN_ID:-}"
COMMIT="${IGNITE_PROMOTE_COMMIT:-}"
CONFIRM="${IGNITE_PROMOTE_CONFIRM:-}"
KINDLE_SHA="${IGNITE_PROMOTE_KINDLE_SHA256:-}"
LINUX_SHA="${IGNITE_PROMOTE_LINUX_SHA256:-}"
WORK="${IGNITE_PROMOTE_WORK:-}"

[[ "$TAG" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$ ]] \
  || die "not a release tag: ${TAG:-none}"
VERSION="${TAG#v}"
[[ "$RUN_ID" =~ ^[1-9][0-9]*$ ]] || die "run id must be a positive integer"
[[ "$COMMIT" =~ ^[0-9a-f]{40}$ ]] || die "commit must be a 40-character lowercase SHA-1"
[[ "$KINDLE_SHA" =~ ^[0-9a-f]{64}$ ]] || die "kindle sha256 missing or malformed"
[[ "$LINUX_SHA" =~ ^[0-9a-f]{64}$ ]] || die "linux sha256 missing or malformed"
[ -n "$WORK" ] || die "IGNITE_PROMOTE_WORK is required"

SFX="Install Ignite v$VERSION.sh"
SFX_SUM="Install Ignite v$VERSION.sha256"
LINUX="ignite-server-$VERSION-linux-$ARCH.tar.gz"
LINUX_SUM="$LINUX.sha256"
DIST="$WORK/dist"

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
  local kfiles lfiles have_k have_l
  kfiles="$(_list_files "$WORK/kindle")"
  lfiles="$(_list_files "$WORK/linux")"
  [ "$kfiles" = "$(printf '%s\n' "$SFX" "$SFX_SUM")" ] \
    || die "Kindle artifact file set mismatch: $kfiles"
  [ "$lfiles" = "$(printf '%s\n' "$LINUX" "$LINUX_SUM")" ] \
    || die "Linux artifact file set mismatch: $lfiles"

  (cd "$WORK/kindle" && sha256sum -c "$SFX_SUM")
  (cd "$WORK/linux" && sha256sum -c "$LINUX_SUM")

  have_k="$(sha256sum "$WORK/kindle/$SFX" | awk '{print $1}')"
  have_l="$(sha256sum "$WORK/linux/$LINUX" | awk '{print $1}')"
  [ "$have_k" = "$KINDLE_SHA" ] \
    || die "Kindle installer sha256 $have_k != recorded $KINDLE_SHA (refusing to publish untested bytes)"
  [ "$have_l" = "$LINUX_SHA" ] \
    || die "Linux bundle sha256 $have_l != recorded $LINUX_SHA (refusing to publish untested bytes)"

  python3 "$(cd "$(dirname "$0")/../.." && pwd)/deployment/kindle/validate_stage.py" \
    --sfx "$WORK/kindle/$SFX" --version "$VERSION" \
    || die "Kindle installer failed validate_stage.py"

  tar -xzf "$WORK/linux/$LINUX" -C "$WORK"
  grep -qx "version=$VERSION" "$WORK/${LINUX%.tar.gz}/.ignite-bundle" \
    || die "bundle metadata version is not $VERSION"
  grep -qx "arch=$ARCH" "$WORK/${LINUX%.tar.gz}/.ignite-bundle" \
    || die "bundle metadata arch is not $ARCH"
  [ "$("$WORK/${LINUX%.tar.gz}/scribe-server" --version | tr -d '[:space:]')" = "$VERSION" ] \
    || die "bundle binary is not $VERSION"
  rm -rf "${WORK:?}/${LINUX%.tar.gz}"

  rm -rf "$DIST"
  mkdir -p "$DIST"
  cp -a "$WORK/kindle/$SFX" "$WORK/kindle/$SFX_SUM" "$WORK/linux/$LINUX" "$WORK/linux/$LINUX_SUM" "$DIST/"
  [ "$(ls -1 "$DIST" | LC_ALL=C sort)" = "$(printf '%s\n' "$SFX" "$SFX_SUM" "$LINUX" "$LINUX_SUM")" ] \
    || die "staging dist does not contain exactly the four primary assets"
  echo "verify: four assets match recorded SHA-256s"
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

publish() {
  if _gh_http --jq .tag_name "repos/$REPO/releases/tags/$TAG"; then
    die "release $TAG already exists; its assets are not replaced"
  fi
  if _tag_commit; then
    [ "$TAG_COMMIT" = "$COMMIT" ] \
      || die "git tag $TAG points at $TAG_COMMIT, not recorded commit $COMMIT"
  fi
  pre=()
  case "$TAG" in *-*) pre=(--prerelease) ;; esac
  notes="$WORK/notes.md"
  cat >"$notes" <<EOF
Ignite $VERSION

**Kindle application (primary):** \`$SFX\` and \`$SFX_SUM\`.
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

all() {
  guard
  resolve
  fetch
  verify
  publish
}

case "${1:-}" in
  guard) guard ;;
  resolve) resolve ;;
  fetch) fetch ;;
  verify) verify ;;
  publish) publish ;;
  all) all ;;
  *) die "usage: $0 guard|resolve|fetch|verify|publish|all" ;;
esac
