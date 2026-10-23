#!/usr/bin/env bash
# GHCR publishing: static workflow guards, image reference consistency, and the guards of
# ghcr-publish.sh exercised against mocked gh / curl / docker (real git, tar, sha256sum).
# No network, no Docker daemon, nothing is published.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$DIR/../../.." && pwd)"
PUB="$DIR/ghcr-publish.sh"
WF="$ROOT/.github/workflows/ghcr.yml"
CI="$ROOT/.github/workflows/ci.yml"
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

# Lines of the block that follows the first line matching $2 (until indentation drops back).
block() { awk -v pat="$2" '
  !on && $0 ~ pat { on = 1; match($0, /^ */); ind = RLENGTH; next }
  on { match($0, /^ */); if ($0 !~ /^[[:space:]]*$/ && RLENGTH <= ind) exit; print }' "$1"; }

# --- 1) ghcr.yml: triggers, permissions, guard order, no secrets ------------------------------
[ "$(block "$WF" '^on:$' | grep -E '^  [a-z_]+:' | tr -d ' :' | sort | tr '\n' ' ')" \
  = "workflow_call workflow_dispatch " ] || fail "ghcr.yml may only be called or dispatched"
grep -qx 'permissions: {}' "$WF" || fail "ghcr.yml top-level permissions must be {}"
[ "$(block "$WF" '^    permissions:$' | tr -d ' ' | sort | tr '\n' ' ')" = "contents:read packages:write " ] \
  || fail "ghcr.yml job permissions must be exactly contents: read, packages: write"
[ "$(grep -c 'permissions:' "$WF")" = 2 ] || fail "ghcr.yml grants permissions elsewhere"
grep -Eq 'secrets\.|\bPAT\b|personal' "$WF" && fail "ghcr.yml must use only the workflow GITHUB_TOKEN"
grep -Eq '^[[:space:]]*(pull_request|push|schedule|release):' "$WF" && fail "ghcr.yml has a forbidden trigger"
grep -Eq 'run: *[|>]' "$WF" && fail "ghcr.yml run steps must be single-line"
[ -z "$(grep -E 'run:' "$WF" | grep '\${{')" ] || fail "ghcr.yml run steps must not interpolate \${{ }}"
grep -q "github.repository == 'markrai/ignite'" "$WF" || fail "ghcr.yml must not run in forks"
grep -q "github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main'" "$WF" \
  || fail "manual backfill must be limited to main"
grep -q "cancel-in-progress: false" "$WF" || fail "publishes must be serialized, never cancelled"
[ "$(grep -c 'persist-credentials: false' "$WF")" = 2 ] || fail "checkouts must not persist credentials"
order="$(grep -nE 'PUBLISH" (guard|source|fetch|publish)|ref: refs/tags/|docker login' "$WF" \
  | sed -E 's/.*(PUBLISH" [a-z]+|ref: refs\/tags|docker login).*/\1/' | tr '\n' ',')"
[ "$order" = 'PUBLISH" guard,ref: refs/tags,PUBLISH" source,PUBLISH" fetch,docker login,PUBLISH" publish,' ] \
  || fail "ghcr.yml step order (guard before checkout, verify before login/push): $order"
pass "ghcr.yml: call/dispatch only, GITHUB_TOKEN with contents:read + packages:write, guarded order"

# --- 2) ci.yml builds artifacts; publication is promote-publish, then GHCR ------
PROMOTE_WF="$ROOT/.github/workflows/release-promote.yml"
grep -qE '^  ghcr:' "$CI" && fail "ci.yml must not call GHCR (promotion owns GHCR)"
grep -q 'gh release create' "$CI" && fail "ci.yml must not create GitHub Releases"
[ "$(grep -c 'packages: write' "$CI")" = 0 ] || fail "ci.yml must not grant packages: write"
grep -qE '^  kindle-package:' "$CI" || fail "ci.yml missing kindle-package"
grep -q 'uses: ./.github/workflows/ghcr.yml' "$PROMOTE_WF" || fail "release-promote.yml must call ghcr.yml"
grep -q 'needs: promote' "$PROMOTE_WF" || fail "GHCR must run after promote"
pass "ci.yml: artifacts only; GHCR follows promote-publish"

# --- 3) image references agree everywhere ------------------------------------------------------
IMAGE="$(sed -n 's/^IMAGE=//p' "$PUB")"
REPO="$(sed -n 's/^REPO=//p' "$PUB")"
[ "$IMAGE" = ghcr.io/markrai/ignite-server ] && [ "$REPO" = markrai/ignite ] || fail "publisher image/repo"
grep -q "org.opencontainers.image.source=\"https://github.com/$REPO\"" "$DIR/Dockerfile" \
  || fail "Dockerfile source label must be https://github.com/$REPO (links the GHCR package)"
grep -qxF "    image: \${IGNITE_IMAGE:-$IMAGE:latest}" "$DIR/docker-compose.yml" || fail "compose default image"
refs="$(grep -rhoE 'ghcr\.io/[A-Za-z0-9._/-]+(:[A-Za-z0-9._<>-]+)?' "$ROOT/README.md" "$ROOT/docs" \
  "$ROOT/.github/workflows" "$DIR"/*.yml "$DIR"/*.md "$PUB" | sort -u)"
[ -n "$refs" ] || fail "no GHCR references found"
while IFS= read -r r; do
  [[ "$r" =~ ^ghcr\.io/markrai/ignite-server(:(latest|v\<version\>|v[0-9]+\.[0-9]+\.[0-9]+))?$ ]] || fail "stray image reference: $r"
done <<<"$refs"
for doc in "$ROOT/README.md" "$ROOT/docs/self-hosting.md" "$DIR/README.md"; do
  grep -qF "$IMAGE" "$doc" || fail "$doc does not name $IMAGE"
done
grep -qi 'not yet published' "$ROOT/README.md" && fail "README still says the image is unpublished"
grep -q 'ignite-server:local' "$ROOT/README.md" && fail "README quick start must not need a local build"
url="https://raw.githubusercontent.com/$REPO/main/deployment/linux/docker/docker-compose.yml"
grep -qF "$url" "$ROOT/docs/self-hosting.md" && grep -qF "$url" "$DIR/README.md" || fail "docs must fetch compose from $url"
grep -q 'docker compose pull' "$ROOT/docs/self-hosting.md" || fail "self-hosting.md must show docker compose pull"
pass "image references: $IMAGE everywhere, compose default :latest, docs copy-pasteable"

# --- 4) ghcr-publish.sh behaviour against mocks -------------------------------------------------
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
SECRET="ghs_test-token-must-never-reach-argv"
V=0.9.13
TOP="ignite-server-$V-linux-x86_64"
MOCK="$T/bin"
mkdir -p "$MOCK" "$T/assets" "$T/reg"

cat >"$MOCK/gh" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "gh $*" >>"$MOCK_STATE/argv.log"
case "$1 $2" in
  "api repos/markrai/ignite/releases/latest") cat "$MOCK_STATE/gh-latest" ;;
  "api repos/markrai/ignite/releases/tags/"*)
    case "$*" in
      *@tsv*) cat "$MOCK_STATE/gh-meta" ;;
      *.digest*) cat "$MOCK_STATE/gh-digest" ;;
      *) exit 9 ;;
    esac ;;
  "release download")
    shift 2; dest=""
    while [ "$#" -gt 0 ]; do [ "$1" = -D ] && dest="$2"; shift; done
    cp "$MOCK_STATE/assets/"* "$dest/" ;;
  *) echo "unexpected gh $*" >&2; exit 9 ;;
esac
SH
cat >"$MOCK/curl" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "curl $*" >>"$MOCK_STATE/argv.log"
cfg="$(cat)"
url="${*: -1}"
case "$url" in
  https://ghcr.io/token?*)
    grep -q '^user = "ci-bot:' <<<"$cfg" || exit 22
    echo '{"token":"registry-bearer"}' ;;
  https://ghcr.io/v2/markrai/ignite-server/manifests/*)
    grep -qx 'header = "Authorization: Bearer registry-bearer"' <<<"$cfg" || exit 22
    tag="${url##*/}"
    if [ -f "$MOCK_STATE/reg-status" ]; then cat "$MOCK_STATE/reg-status"
    elif [ -f "$MOCK_STATE/reg/$tag" ] || [ -d "$MOCK_STATE/regimg/$tag" ]; then printf 200
    else printf 404; fi ;;
  *) exit 9 ;;
esac
SH
cat >"$MOCK/docker" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "docker $*" >>"$MOCK_STATE/argv.log"
# Mock image model: a local image is $MOCK_STATE/img/<key>/ and a registry image is
# $MOCK_STATE/regimg/<tag>/, each holding version, revision, binversion, osarch,
# entrypoint, bin, web/ and digest (sha256:…). Pull copies registry → local, tag copies
# local → local, push copies local → registry. Legacy seeds ($MOCK_STATE/reg/<tag> =
# version label only) synthesize a release-identical image on pull.
key() { printf '%s' "$1" | tr -c 'A-Za-z0-9_.-' '_'; }
digest_dir() { # image-dir → sha256:… over labels, binary and web tree
  { cat "$1/version" "$1/revision" "$1/binversion" "$1/osarch" "$1/entrypoint"
    sha256sum "$1/bin"
    (cd "$1/web" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum)
  } | sha256sum | sed 's/^/sha256:/; s/ .*//'
}
resolve() { # ref → local image dir (by tag key, or by @digest); empty if unknown
  local ref="$1" d
  case "$ref" in
    *@sha256:*)
      for d in "$MOCK_STATE"/img/*/; do
        [ -f "$d/digest" ] && [ "$(cat "$d/digest")" = "${ref##*@}" ] && { printf '%s' "${d%/}"; return; }
      done ;;
    *) local k; k="$(key "$ref")"
       [ -d "$MOCK_STATE/img/$k" ] && printf '%s' "$MOCK_STATE/img/$k" ;;
  esac
}
synth() { # tag → local image dir synthesized from a legacy reg/<tag> seed
  local tag="$1" d="$MOCK_STATE/img/$(key "ghcr.io/markrai/ignite-server:$tag")"
  mkdir -p "$d/web"
  printf '%s' "$(cat "$MOCK_STATE/reg/$tag")" >"$d/version"
  printf '%s' "$(cat "$MOCK_STATE/image-revision" 2>/dev/null || echo unknown)" >"$d/revision"
  printf '%s' "$(cat "$MOCK_STATE/reg/$tag")" >"$d/binversion"
  printf 'linux/amd64' >"$d/osarch"
  printf '["/usr/local/bin/docker-entrypoint.sh"]' >"$d/entrypoint"
  cp "${MOCK_IMAGE_BIN:-$MOCK_BUNDLE/scribe-server}" "$d/bin"
  cp -a "$MOCK_BUNDLE/web/." "$d/web/"
  digest_dir "$d" >"$d/digest"
  printf '%s' "$d"
}
img_of() { # ref → local image dir, synthesizing a legacy seed when needed
  local d; d="$(resolve "$1")"
  if [ -z "$d" ]; then
    case "$1" in
      ghcr.io/markrai/ignite-server:*)
        local tag="${1##*:}"
        [ -f "$MOCK_STATE/reg/$tag" ] && d="$(synth "$tag")" ;;
    esac
  fi
  printf '%s' "$d"
}
case "$1" in
  pull)
    ref="${@: -1}"; tag="${ref##*:}"; d=""
    if [ -d "$MOCK_STATE/regimg/$tag" ]; then
      d="$MOCK_STATE/img/$(key "$ref")"; rm -rf "$d"; cp -a "$MOCK_STATE/regimg/$tag" "$d"
    elif [ -f "$MOCK_STATE/reg/$tag" ]; then synth "$tag" >/dev/null
    else echo "mock pull: no such image $ref" >&2; exit 1; fi ;;
  build)
    printf '%s\n' "build $*" >>"$MOCK_STATE/docker.log"
    cat >/dev/null
    tref=""; ver=""; rev=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        -t) tref="$2"; shift 2 ;;
        --label)
          case "$2" in
            org.opencontainers.image.version=*) ver="${2##*=}" ;;
            org.opencontainers.image.revision=*) rev="${2##*=}" ;;
          esac; shift 2 ;;
        *) shift ;;
      esac
    done
    d="$MOCK_STATE/img/$(key "$tref")"; rm -rf "$d"; mkdir -p "$d/web"
    printf '%s' "${ver:-$(cat "$MOCK_STATE/image-version")}" >"$d/version"
    printf '%s' "${rev:-unknown}" >"$d/revision"
    printf '%s' "$(cat "$MOCK_STATE/image-version")" >"$d/binversion"
    printf 'linux/amd64' >"$d/osarch"
    printf '["/usr/local/bin/docker-entrypoint.sh"]' >"$d/entrypoint"
    cp "${MOCK_IMAGE_BIN:-$MOCK_BUNDLE/scribe-server}" "$d/bin"
    cp -a "${MOCK_IMAGE_WEB:-$MOCK_BUNDLE/web}/." "$d/web/"
    digest_dir "$d" >"$d/digest" ;;
  tag)
    d="$(img_of "$2")"; [ -n "$d" ] || { echo "mock tag: unknown source $2" >&2; exit 1; }
    nd="$MOCK_STATE/img/$(key "$3")"; rm -rf "$nd"; cp -a "$d" "$nd" ;;
  push)
    printf '%s\n' "push $2" >>"$MOCK_STATE/docker.log"
    d="$(img_of "$2")"; [ -n "$d" ] || { echo "mock push: unknown image $2" >&2; exit 1; }
    tag="${2##*:}"; rm -rf "$MOCK_STATE/regimg/$tag"; cp -a "$d" "$MOCK_STATE/regimg/$tag"
    printf '%s\n' "$(cat "$d/version")" >"$MOCK_STATE/reg/$tag" ;;
  image)
    fmt="$4"; d="$(img_of "$5")"
    case "$fmt" in
      *Labels*version*) if [ -n "$d" ]; then cat "$d/version"; else cat "$MOCK_STATE/reg/latest"; fi ;;
      *Labels*revision*) if [ -n "$d" ]; then cat "$d/revision"; else echo unknown; fi ;;
      *Labels*) cat "$MOCK_STATE/reg/latest" ;;
      *Os*) if [ -n "$d" ]; then cat "$d/osarch"; else echo linux/amd64; fi ;;
      *Entrypoint*) if [ -n "$d" ]; then cat "$d/entrypoint"; else echo '["/usr/local/bin/docker-entrypoint.sh"]'; fi ;;
      *RepoDigests*) [ -n "$d" ] && printf 'ghcr.io/markrai/ignite-server@%s' "$(cat "$d/digest")" ;;
    esac ;;
  run)
    while [ "$1" != --entrypoint ]; do shift; done
    ep="$2"; ref="$3"; d="$(img_of "$ref")"
    case "$ep" in
      */scribe-server) if [ -n "$d" ]; then cat "$d/binversion"; else cat "$MOCK_STATE/image-version"; fi ;;
      sha256sum) if [ -n "$d" ]; then sha256sum "$d/bin"; else sha256sum "${MOCK_IMAGE_BIN:-$MOCK_BUNDLE/scribe-server}"; fi ;;
      sh) if [ -n "$d" ]; then cd "$d/web"; else cd "$MOCK_BUNDLE/web"; fi
          find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum ;;
    esac ;;
  *) echo "unexpected docker $*" >&2; exit 9 ;;
esac
SH
chmod +x "$MOCK"/*

# Release source: a tagged checkout whose build-image.sh records how it was called.
mkdir -p "$T/src/ignite" "$T/src/deployment/linux/docker"
echo "$V" >"$T/src/ignite/VERSION"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "build-image $IGNITE_DOCKER_IMAGE $1" >>"$MOCK_STATE/docker.log"\n' \
  >"$T/src/deployment/linux/docker/build-image.sh"
git -C "$T/src" init -q
git -C "$T/src" -c user.name=t -c user.email=t@t add -A
git -C "$T/src" -c user.name=t -c user.email=t@t commit -qm release
git -C "$T/src" -c user.name=t -c user.email=t@t tag -a "v$V" -m "v$V"

make_asset() { # [marker-version] [binary-version] [extra: symlink|outside]
  local mv="${1:-$V}" bv="${2:-$V}" st="$T/stage"
  rm -rf "$st" "$T/assets"/* && mkdir -p "$st/$TOP/web"
  printf 'version=%s\narch=x86_64\n' "$mv" >"$st/$TOP/.ignite-bundle"
  printf '#!/bin/sh\necho %s\n' "$bv" >"$st/$TOP/scribe-server" && chmod +x "$st/$TOP/scribe-server"
  echo '<!doctype html>' >"$st/$TOP/web/index.html"
  echo '{"id":"/ignite-web"}' >"$st/$TOP/web/manifest.webmanifest"
  case "${3:-}" in
    symlink) ln -s index.html "$st/$TOP/web/link.html" ;;
    outside) echo x >"$st/stray" ;;
  esac
  local extra=(); [ "${3:-}" = outside ] && extra=(stray)
  tar -C "$st" -czf "$T/assets/$TOP.tar.gz" "$TOP" "${extra[@]}"
  (cd "$T/assets" && sha256sum "$TOP.tar.gz" >"$TOP.tar.gz.sha256")
  echo "sha256:$(cut -d' ' -f1 "$T/assets/$TOP.tar.gz.sha256")" >"$T/gh-digest"
}

reset() {
  rm -rf "$T/work" "$T/reg"/* "$T/regimg" "$T/img" "$T/reg-status" "$T/docker.log"
  mkdir -p "$T/regimg" "$T/img"
  printf 'false\tfalse\tgithub-actions[bot]\tv%s\n' "$V" >"$T/gh-meta"
  echo "v$V" >"$T/gh-latest"
  echo "$V" >"$T/image-version"
  git -C "$T/src" rev-parse HEAD >"$T/image-revision" 2>/dev/null || echo unknown >"$T/image-revision"
  make_asset
}

BASE=(PATH="$MOCK:$PATH" MOCK_STATE="$T" MOCK_BUNDLE="$T/work/x/$TOP"
  GITHUB_REPOSITORY=markrai/ignite GITHUB_EVENT_NAME=push GITHUB_REF="refs/tags/v$V"
  GITHUB_ACTOR=ci-bot GITHUB_TOKEN="$SECRET" GH_TOKEN="$SECRET" IGNITE_GHCR_TAG="v$V"
  IGNITE_GHCR_CONFIRM= IGNITE_RELEASE_SRC="$T/src" IGNITE_GHCR_WORK="$T/work")
run() { # name cmd [VAR=value...]; sets RC
  local name="$1" cmd="$2"; shift 2
  set +e; env "${BASE[@]}" "$@" bash "$PUB" "$cmd" >"$T/$name.out" 2>"$T/$name.err"; RC=$?; set -e
}
ok() { [ "$RC" = 0 ] || { cat "$T/$1.err" >&2; fail "$1: expected success (rc=$RC)"; }; }
refused() { # name message
  [ "$RC" != 0 ] || fail "$1: expected a refusal"
  grep -qF -- "$2" "$T/$1.err" || { cat "$T/$1.err" >&2; fail "$1: expected refusal \"$2\""; }
}
no_push() { [ ! -f "$T/docker.log" ] || ! grep -q '^push' "$T/docker.log" || fail "$1 pushed an image"; }

# guard: trigger, repository and release provenance
reset
run g-push guard; ok g-push
run g-pr guard GITHUB_EVENT_NAME=pull_request GITHUB_REF=refs/pull/7/merge; refused g-pr "event pull_request never publishes"
run g-main guard GITHUB_REF=refs/heads/main; refused g-main "publishes only its own tag"
run g-other-tag guard GITHUB_REF=refs/tags/v0.9.12; refused g-other-tag "publishes only its own tag"
run g-sched guard GITHUB_EVENT_NAME=schedule; refused g-sched "never publishes"
run g-fork guard GITHUB_REPOSITORY=someone/ignite; refused g-fork "publishing runs only in markrai/ignite"
D=(GITHUB_EVENT_NAME=workflow_dispatch GITHUB_REF=refs/heads/main)
run g-d-noconfirm guard "${D[@]}"; refused g-d-noconfirm 'needs confirm = "publish v0.9.13"'
run g-d-badconfirm guard "${D[@]}" IGNITE_GHCR_CONFIRM="publish v0.9.12"; refused g-d-badconfirm "needs confirm"
run g-d-branch guard "${D[@]}" GITHUB_REF=refs/heads/feature IGNITE_GHCR_CONFIRM="publish v$V"
refused g-d-branch "runs only from main"
run g-d-ok guard "${D[@]}" IGNITE_GHCR_CONFIRM="publish v$V"; ok g-d-ok
for bad in v0.9.14-rc.1 0.9.13 v0.9 v01.9.13 'v0.9.13;x' ''; do
  run g-tag guard IGNITE_GHCR_TAG="$bad" GITHUB_REF="refs/tags/$bad"; refused g-tag "not a stable release tag"
done
for meta in 'true\tfalse\tgithub-actions[bot]' 'false\ttrue\tgithub-actions[bot]' 'false\tfalse\tmarkrai'; do
  printf "$meta\\tv%s\\n" "$V" >"$T/gh-meta"
  run g-meta guard; refused g-meta "not a published stable release created by CI"
done
pass "guard: tag push of its own tag or confirmed backfill from main; CI-made stable release only"

# source: the checkout is the tag, at that version
reset
run s-ok source; ok s-ok
run s-notag source IGNITE_GHCR_TAG=v0.9.14; refused s-notag "not a checkout of v0.9.14"
git -C "$T/src" tag v0.9.14
run s-ver source IGNITE_GHCR_TAG=v0.9.14; refused s-ver "ignite/VERSION at v0.9.14 is not 0.9.14"
git -C "$T/src" tag -d v0.9.14 >/dev/null
git -C "$T/src" -c user.name=t -c user.email=t@t commit -q --allow-empty -m after
run s-moved source; refused s-moved "not a checkout of v$V"
git -C "$T/src" reset -q --hard "v$V"
pass "source: HEAD is the tag and ignite/VERSION matches"

# fetch: checksum, GitHub digest, archive shape, metadata and binary version
reset
run f-ok fetch; ok f-ok
[ -x "$T/work/x/$TOP/scribe-server" ] || fail "fetch did not extract the bundle"
run f-again fetch; refused f-again "work directory already exists"
reset; printf 'x' >>"$T/assets/$TOP.tar.gz"
run f-sum fetch; refused f-sum "release asset checksum mismatch"
reset; echo "sha256:$(printf '%064d' 0)" >"$T/gh-digest"
run f-digest fetch; refused f-digest "GitHub asset digest"
reset; (cd "$T/assets" && cp "$TOP.tar.gz" other.tar.gz && sha256sum other.tar.gz >"$TOP.tar.gz.sha256" && rm other.tar.gz)
run f-name fetch; refused f-name "checksum file does not name"
reset; make_asset 0.9.12
run f-marker fetch; refused f-marker "bundle metadata version is not $V"
reset; make_asset "$V" 0.9.12
run f-binary fetch; refused f-binary "bundle binary is not $V"
reset; make_asset "$V" "$V" symlink
run f-link fetch; refused f-link "archive holds links or special files"
reset; make_asset "$V" "$V" outside
run f-outside fetch; refused f-outside "archive entry outside $TOP/"
pass "fetch: checksum + GitHub digest + archive shape + metadata + binary version"

# decide-latest: only the newest stable release, never backward (numeric, fail closed)
reset
echo v0.9.14 >"$T/gh-latest"
run l-older decide-latest; ok l-older; grep -q '^latest=false' "$T/l-older.out" || fail "older release took latest"
reset
run l-first decide-latest; ok l-first; grep -qx 'latest=true' "$T/l-first.out" || fail "first publish latest"
for cur in 0.9.12 0.9.9 0.9.13; do
  reset; echo "$cur" >"$T/reg/latest"
  run l-fwd decide-latest; ok l-fwd; grep -qx 'latest=true' "$T/l-fwd.out" || fail "latest $cur -> $V"
done
for cur in 0.9.14 0.10.0 1.0.0; do
  reset; echo "$cur" >"$T/reg/latest"
  run l-back decide-latest; refused l-back "latest never moves backward"
done
reset; echo unlabeled >"$T/reg/latest"
run l-nolabel decide-latest; refused l-nolabel "has no release version label"
reset; echo 403 >"$T/reg-status"
run l-403 decide-latest; refused l-403 "unexpected registry status 403"
reset; run p-fetch fetch; ok p-fetch; echo 403 >"$T/reg-status"
run p-403 publish; refused p-403 "unexpected registry status 403"; no_push p-403
pass "decide-latest: newest stable release only; never backward; registry errors fail closed"

# publish: immutable version tags, release-identical image, latest only when decided.
# seed_image plants a registry image with explicit content so recovery can be tested
# against correct and tampered registry state (defaults: release-identical).
seed_image() { # tag version [revision] [bin] [web-dir] [bin-version]
  local tag="$1" ver="$2"
  local d="$T/regimg/$tag"
  rm -rf "$d"; mkdir -p "$d/web"
  printf '%s' "$ver" >"$d/version"
  printf '%s' "${3:-$(git -C "$T/src" rev-parse HEAD)}" >"$d/revision"
  printf '%s' "${6:-$ver}" >"$d/binversion"
  printf 'linux/amd64' >"$d/osarch"
  printf '["/usr/local/bin/docker-entrypoint.sh"]' >"$d/entrypoint"
  cp "${4:-$T/work/x/$TOP/scribe-server}" "$d/bin"
  cp -a "${5:-$T/work/x/$TOP/web}/." "$d/web/"
  { cat "$d/version" "$d/revision" "$d/binversion" "$d/osarch" "$d/entrypoint"
    sha256sum "$d/bin"
    (cd "$d/web" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum)
  } | sha256sum | sed 's/^/sha256:/; s/ .*//' >"$d/digest"
  printf '%s\n' "$ver" >"$T/reg/$tag"
}
no_version_push() { # existing v$V must never be (re)pushed in this scenario
  [ ! -f "$T/docker.log" ] || ! grep -qx "push ghcr.io/markrai/ignite-server:v$V" "$T/docker.log" \
    || fail "$1 pushed the existing version tag v$V"
}
version_digest() { cat "$T/regimg/v$V/digest"; }

# 1) first publication of a previously absent version
reset; run p-fetch fetch; ok p-fetch
run p-latest publish; ok p-latest
[ "$(grep -E '^(build-image|push)' "$T/docker.log")" = "$(printf '%s\n' \
  "build-image ignite-server:ghcr-candidate-$V $T/work/x/$TOP" \
  "push ghcr.io/markrai/ignite-server:v$V" "push ghcr.io/markrai/ignite-server:latest")" ] \
  || { cat "$T/docker.log" >&2; fail "p-latest build/push sequence"; }
grep -q -- "--label org.opencontainers.image.version=$V --label org.opencontainers.image.revision=$(git -C "$T/src" rev-parse HEAD)" \
  "$T/docker.log" || fail "image labels"

reset; run p-fetch fetch; ok p-fetch
echo v0.9.14 >"$T/gh-latest"; echo 0.9.14 >"$T/reg/latest"
run p-backfill publish; ok p-backfill
[ "$(grep '^push' "$T/docker.log")" = "push ghcr.io/markrai/ignite-server:v$V" ] || fail "backfill of an older release moved latest"
[ "$(cat "$T/reg/latest")" = 0.9.14 ] || fail "latest changed"

reset; run p-fetch fetch; ok p-fetch
echo different >"$T/other-bin"
run p-bin publish MOCK_IMAGE_BIN="$T/other-bin"; refused p-bin "image binary differs from the release asset"; no_push p-bin
reset; run p-fetch fetch; ok p-fetch
echo 0.9.12 >"$T/image-version"
run p-ver publish; refused p-ver "image binary is not $V"; no_push p-ver
reset; run p-fetch fetch; ok p-fetch
echo 0.10.0 >"$T/reg/latest"
run p-back publish; refused p-back "latest never moves backward"; no_push p-back
pass "publish (absent version): first publish, backfill, built-image checks, latest only forward"

# Recovery: v$V already in the registry. It is never rebuilt or re-pushed; the registry
# image is verified by digest against the release asset, then latest may be completed.
# 2)+3) version pushed, latest push failed → retry publishes only latest
reset; run r-fetch fetch; ok r-fetch
seed_image "v$V" "$V"
before="$(version_digest)"
run r-retry publish; ok r-retry
[ "$(grep '^push' "$T/docker.log")" = "push ghcr.io/markrai/ignite-server:latest" ] \
  || { cat "$T/docker.log" >&2; fail "r-retry must push only latest"; }
no_version_push r-retry
[ ! -f "$T/docker.log" ] || ! grep -q '^build' "$T/docker.log" || fail "r-retry rebuilt the image"
[ "$(version_digest)" = "$before" ] || fail "r-retry changed the version image digest"
[ "$(cat "$T/regimg/latest/digest")" = "$before" ] || fail "r-retry latest does not point at the verified digest"
pass "recovery: partial publish (version ok, latest failed) retries with only a latest push"

# outdated latest (older version) is completed to the verified digest, nothing else
reset; run r-fetch fetch; ok r-fetch
seed_image "v$V" "$V"; seed_image latest 0.9.12
before="$(version_digest)"
run r-outdated publish; ok r-outdated
[ "$(grep '^push' "$T/docker.log")" = "push ghcr.io/markrai/ignite-server:latest" ] || fail "r-outdated pushes"
no_version_push r-outdated
[ "$(cat "$T/regimg/latest/digest")" = "$before" ] || fail "r-outdated latest digest"
[ "$(version_digest)" = "$before" ] || fail "r-outdated changed the version image"

# 8) latest already points at the verified image → success, no pushes at all
reset; run r-fetch fetch; ok r-fetch
seed_image "v$V" "$V"; cp -a "$T/regimg/v$V" "$T/regimg/latest"; echo "$V" >"$T/reg/latest"
before="$(version_digest)"
run r-current publish; ok r-current
no_push r-current; no_version_push r-current
grep -q 'latest: already' "$T/r-current.out" || fail "r-current did not report latest already correct"
[ "$(version_digest)" = "$before" ] || fail "r-current changed the version image"
pass "recovery: outdated latest completed by digest; correct latest is a no-op"

# 4) mismatched binary in the existing version image → refuse, push nothing
reset; run r-fetch fetch; ok r-fetch
echo different >"$T/other-bin"
seed_image "v$V" "$V" "" "$T/other-bin"
before="$(version_digest)"
run r-bin publish; refused r-bin "image binary differs from the release asset"
no_push r-bin; [ "$(version_digest)" = "$before" ] || fail "r-bin changed the version image"

# 5)+6) mismatched web assets: altered, missing and extra files → refuse, push nothing
for variant in altered missing extra; do
  reset; run r-fetch fetch; ok r-fetch
  rm -rf "$T/web-$variant"; cp -a "$T/work/x/$TOP/web" "$T/web-$variant"
  case "$variant" in
    altered) echo tampered >>"$T/web-$variant/index.html" ;;
    missing) rm "$T/web-$variant/manifest.webmanifest" ;;
    extra) echo stray >"$T/web-$variant/stray.txt" ;;
  esac
  seed_image "v$V" "$V" "" "" "$T/web-$variant"
  before="$(version_digest)"
  run r-web publish; refused r-web "image web assets differ from the release asset"
  no_push r-web; [ "$(version_digest)" = "$before" ] || fail "r-web ($variant) changed the version image"
done
pass "recovery: mismatched binary and altered/missing/extra web files all refuse"

# 7) mismatched version / revision labels (and platform / entrypoint) → refuse
reset; run r-fetch fetch; ok r-fetch
seed_image "v$V" 0.9.12
run r-vlabel publish; refused r-vlabel "version label is 0.9.12, not $V"; no_push r-vlabel
reset; run r-fetch fetch; ok r-fetch
seed_image "v$V" "$V" 0000000000000000000000000000000000000000
run r-rlabel publish; refused r-rlabel "revision label"; no_push r-rlabel
reset; run r-fetch fetch; ok r-fetch
seed_image "v$V" "$V"; printf 'linux/arm64' >"$T/regimg/v$V/osarch"
run r-arch publish; refused r-arch "image is not linux/amd64"; no_push r-arch
reset; run r-fetch fetch; ok r-fetch
seed_image "v$V" "$V"; printf '["/bin/sh"]' >"$T/regimg/v$V/entrypoint"
run r-entry publish; refused r-entry "image entrypoint changed"; no_push r-entry
pass "recovery: version/revision label, architecture and entrypoint mismatches refuse"

# 9) latest newer than the requested release → refuse the downgrade, push nothing
reset; run r-fetch fetch; ok r-fetch
seed_image "v$V" "$V"; seed_image latest 0.9.14
before="$(version_digest)"
run r-newer publish; refused r-newer "latest never moves backward"
no_push r-newer; [ "$(version_digest)" = "$before" ] || fail "r-newer changed the version image"
[ "$(cat "$T/reg/latest")" = 0.9.14 ] || fail "r-newer moved latest"

# recovery of a release that is not the newest stable → verified, latest untouched
reset; run r-fetch fetch; ok r-fetch
seed_image "v$V" "$V"; echo v0.9.14 >"$T/gh-latest"
run r-old publish; ok r-old
no_push r-old; no_version_push r-old

# 10) registry errors / authorization failure during recovery → refuse, push nothing
reset; run r-fetch fetch; ok r-fetch
seed_image "v$V" "$V"; echo 401 >"$T/reg-status"
run r-401 publish; refused r-401 "unexpected registry status 401"; no_push r-401
reset; run r-fetch fetch; ok r-fetch
seed_image "v$V" "$V"; echo 403 >"$T/reg-status"
run r-403 publish; refused r-403 "unexpected registry status 403"; no_push r-403
pass "recovery: newer latest refuses; non-newest release leaves latest; registry errors fail closed"
# 11) is asserted throughout above: no_version_push + unchanged version digest in every
# recovery scenario, and the only version-tag push in this suite is the first publish.

grep -qF "$SECRET" "$T/argv.log" && fail "the workflow token reached a command line"
pass "the workflow token never appears in argv"

echo "PASS: GHCR publishing guards and image references"
