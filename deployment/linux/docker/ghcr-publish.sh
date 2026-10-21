#!/usr/bin/env bash
# Publish an already published, CI-built Ignite Server GitHub Release to
# ghcr.io/markrai/ignite-server as tag v<X.Y.Z>, and as latest when it is the newest stable release.
# Run only by .github/workflows/ghcr.yml. Rust and ignite-web are never rebuilt: the image is
# the release's own Linux asset, packaged by the release tag's build-image.sh/Dockerfile.
#
#   ghcr-publish.sh guard          trigger, repository and GitHub Release checks (no checkout)
#   ghcr-publish.sh source         IGNITE_RELEASE_SRC is a checkout of the tag at that version
#   ghcr-publish.sh fetch          download + verify the release asset into IGNITE_GHCR_WORK
#   ghcr-publish.sh decide-latest  prints latest=true|false (fails if latest would move backward)
#   ghcr-publish.sh publish        build, verify and push v<X.Y.Z>, then latest when decided;
#                                    if v<X.Y.Z> already exists, verify the registry image
#                                    against the release asset and only complete latest
#                                    (the existing version tag is never replaced)
#
# Environment: IGNITE_GHCR_TAG (vX.Y.Z), IGNITE_RELEASE_SRC, IGNITE_GHCR_WORK, GH_TOKEN, and the
# Actions GITHUB_REPOSITORY / GITHUB_EVENT_NAME / GITHUB_REF / GITHUB_ACTOR / GITHUB_TOKEN.
# A manual backfill also needs IGNITE_GHCR_CONFIRM="publish vX.Y.Z". Tokens never reach argv.
set -euo pipefail
shopt -s inherit_errexit

REPO=markrai/ignite
IMAGE=ghcr.io/markrai/ignite-server
REGISTRY_NAME="${IMAGE#ghcr.io/}"
ARCH=x86_64
RELEASER='github-actions[bot]'
GHCR=https://ghcr.io

die() { echo "ghcr-publish.sh: $*" >&2; exit 1; }

TAG="${IGNITE_GHCR_TAG:-}"
[[ "$TAG" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
  || die "not a stable release tag: ${TAG:-none} (prereleases are never published)"
VERSION="${TAG#v}"
TOP="ignite-server-$VERSION-linux-$ARCH"
CANDIDATE="ignite-server:ghcr-candidate-$VERSION"

version_gt() { # a b → a > b, both X.Y.Z
  local IFS=. i x y
  read -ra x <<<"$1"
  read -ra y <<<"$2"
  for i in 0 1 2; do
    ((10#${x[i]} > 10#${y[i]})) && return 0
    ((10#${x[i]} < 10#${y[i]})) && return 1
  done
  return 1
}

guard() {
  [ "${GITHUB_REPOSITORY:-}" = "$REPO" ] || die "publishing runs only in $REPO (got ${GITHUB_REPOSITORY:-none})"
  case "${GITHUB_EVENT_NAME:-}" in
    push)
      [ "${GITHUB_REF:-}" = "refs/tags/$TAG" ] \
        || die "a tag push publishes only its own tag (ref ${GITHUB_REF:-none}, tag $TAG)" ;;
    workflow_dispatch)
      [ "${GITHUB_REF:-}" = refs/heads/main ] || die "a manual backfill runs only from main (ref ${GITHUB_REF:-none})"
      [ "${IGNITE_GHCR_CONFIRM:-}" = "publish $TAG" ] || die "a manual backfill needs confirm = \"publish $TAG\"" ;;
    *) die "event ${GITHUB_EVENT_NAME:-none} never publishes" ;;
  esac
  # Only CI's `release` job creates releases as github-actions[bot], and it runs only after
  # every CI gate passed for the tag; a hand-made, draft or prerelease release is refused.
  local meta
  meta="$(gh api "repos/$REPO/releases/tags/$TAG" --jq '[.draft, .prerelease, .author.login, .tag_name] | @tsv')" \
    || die "no GitHub Release $TAG"
  [ "$meta" = "$(printf 'false\tfalse\t%s\t%s' "$RELEASER" "$TAG")" ] \
    || die "release $TAG is not a published stable release created by CI (draft, prerelease, author, tag: $meta)"
  echo "guard: $TAG ok (${GITHUB_EVENT_NAME})"
}

source_check() {
  local src="${IGNITE_RELEASE_SRC:?}"
  [ "$(git -C "$src" rev-parse HEAD)" = "$(git -C "$src" rev-parse "refs/tags/$TAG^{commit}")" ] \
    || die "release source is not a checkout of $TAG"
  [ "$(tr -d '[:space:]' <"$src/ignite/VERSION")" = "$VERSION" ] || die "ignite/VERSION at $TAG is not $VERSION"
  [ -f "$src/deployment/linux/docker/build-image.sh" ] || die "$TAG has no Docker packaging"
  echo "source: $TAG at $(git -C "$src" rev-parse HEAD)"
}

fetch() {
  local work="${IGNITE_GHCR_WORK:?}" dl sum name extra digest
  [ ! -e "$work" ] || die "work directory already exists: $work"
  dl="$work/download"
  mkdir -p "$dl" "$work/x"
  gh release download "$TAG" -R "$REPO" -D "$dl" -p "$TOP.tar.gz" -p "$TOP.tar.gz.sha256" \
    || die "cannot download the $TAG release asset"
  [ "$(ls -A "$dl")" = "$(printf '%s\n' "$TOP.tar.gz" "$TOP.tar.gz.sha256")" ] || die "unexpected download: $(ls -A "$dl")"
  [ "$(wc -l <"$dl/$TOP.tar.gz.sha256")" -eq 1 ] || die "checksum file must have exactly one line"
  read -r sum name extra <"$dl/$TOP.tar.gz.sha256"
  [[ "$sum" =~ ^[0-9a-f]{64}$ ]] && [ "$name" = "$TOP.tar.gz" ] && [ -z "${extra:-}" ] \
    || die "checksum file does not name $TOP.tar.gz"
  [ "$(sha256sum "$dl/$TOP.tar.gz" | cut -d' ' -f1)" = "$sum" ] || die "release asset checksum mismatch"
  # GitHub records its own digest of the uploaded asset; it must agree as well.
  digest="$(gh api "repos/$REPO/releases/tags/$TAG" --jq ".assets[] | select(.name == \"$TOP.tar.gz\") | .digest")"
  [ "$digest" = "sha256:$sum" ] || die "GitHub asset digest ${digest:-missing} != sha256:$sum"

  # Only regular files and directories under the one top-level directory, no "..". The
  # listings are read whole (no grep -q) so SIGPIPE under pipefail cannot hide a match.
  local n
  [ -z "$(tar -tvzf "$dl/$TOP.tar.gz" | cut -c1 | grep -v '^[-d]$')" ] || die "archive holds links or special files"
  while IFS= read -r n; do
    case "$n" in "$TOP" | "$TOP/"*) ;; *) die "archive entry outside $TOP/: $n" ;; esac
    case "/$n/" in */../*) die "archive has a .. entry: $n" ;; esac
  done <<<"$(tar -tzf "$dl/$TOP.tar.gz")"
  tar -xzf "$dl/$TOP.tar.gz" --no-same-owner -C "$work/x"

  local b="$work/x/$TOP"
  grep -qx "version=$VERSION" "$b/.ignite-bundle" || die "bundle metadata version is not $VERSION"
  grep -qx "arch=$ARCH" "$b/.ignite-bundle" || die "bundle metadata arch is not $ARCH"
  [ "$("$b/scribe-server" --version | tr -d '[:space:]')" = "$VERSION" ] || die "bundle binary is not $VERSION"
  [ -f "$b/web/index.html" ] && [ -f "$b/web/manifest.webmanifest" ] || die "bundle has no web assets"
  echo "fetch: $TOP.tar.gz sha256=$sum verified"
}

# HTTP status of a manifest in GHCR (200 present, 404 absent). The workflow token reaches
# curl through stdin; any other status stops the run (fail closed).
manifest_status() {
  local token code
  token="$(printf 'user = "%s:%s"\n' "${GITHUB_ACTOR:?}" "${GITHUB_TOKEN:?}" \
    | curl -fsS -K - "$GHCR/token?service=ghcr.io&scope=repository:$REGISTRY_NAME:pull,push" \
    | sed -n 's/.*"token"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')" || die "cannot get a registry token"
  [ -n "$token" ] || die "registry token response had no token"
  code="$(printf 'header = "Authorization: Bearer %s"\n' "$token" \
    | curl -sS -o /dev/null -w '%{http_code}' -I -K - \
        -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json' \
        "$GHCR/v2/$REGISTRY_NAME/manifests/$1")" || die "cannot query $IMAGE:$1"
  case "$code" in 200 | 404) echo "$code" ;; *) die "unexpected registry status $code for $IMAGE:$1" ;; esac
}

# Statuses are always assigned before use: a die inside "$(...)" in a test or `if` would
# only end that subshell and fail open.
decide_latest() {
  local newest current status
  newest="$(gh api "repos/$REPO/releases/latest" --jq .tag_name)" || die "cannot read the latest release"
  if [ "$newest" != "$TAG" ]; then
    echo "latest=false ($TAG is not the latest stable release $newest)"
    return
  fi
  status="$(manifest_status latest)"
  if [ "$status" = 200 ]; then
    docker pull -q "$IMAGE:latest" >/dev/null
    current="$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.version"}}' "$IMAGE:latest")"
    [[ "$current" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "$IMAGE:latest has no release version label"
    ! version_gt "$current" "$VERSION" || die "$IMAGE:latest is $current, newer than $VERSION; latest never moves backward"
  fi
  echo "latest=true"
}

tree_sums() { # dir → sorted sha256 list of its regular files
  (cd "$1" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum)
}

# Full release-identity check of an image (built locally or pulled from the registry):
# version + revision labels, platform, entrypoint, binary version and SHA-256, and the
# exact web file set with a hash per file. Any mismatch refuses.
verify_image() { # image-ref revision bundle-dir
  local ref="$1" rev="$2" b="$3" got
  got="$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.version"}}' "$ref")" \
    || die "cannot inspect version label of $ref"
  [ "$got" = "$VERSION" ] || die "image $ref version label is ${got:-missing}, not $VERSION"
  got="$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$ref")" \
    || die "cannot inspect revision label of $ref"
  [ "$got" = "$rev" ] || die "image $ref revision label is ${got:-missing}, not $rev"
  [ "$(docker image inspect -f '{{.Os}}/{{.Architecture}}' "$ref")" = linux/amd64 ] || die "image is not linux/amd64"
  [ "$(docker image inspect -f '{{json .Config.Entrypoint}}' "$ref")" = '["/usr/local/bin/docker-entrypoint.sh"]' ] \
    || die "image entrypoint changed"
  [ "$(docker run --rm --network none --entrypoint /usr/local/bin/scribe-server "$ref" --version | tr -d '[:space:]')" = "$VERSION" ] \
    || die "image binary is not $VERSION"
  [ "$(docker run --rm --network none --entrypoint sha256sum "$ref" /usr/local/bin/scribe-server | cut -d' ' -f1)" \
    = "$(sha256sum "$b/scribe-server" | cut -d' ' -f1)" ] || die "image binary differs from the release asset"
  [ "$(docker run --rm --network none --entrypoint sh "$ref" -c \
      'cd /usr/share/scribe-server/web && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum')" \
    = "$(tree_sums "$b/web")" ] || die "image web assets differ from the release asset"
}

# Immutable digest ($IMAGE@sha256:…) of a local image; refuses when absent or malformed.
repo_digest() { # image-ref
  local ref="$1" digests d
  digests="$(docker image inspect -f '{{join .RepoDigests " "}}' "$ref")" || die "cannot inspect digests of $ref"
  for d in $digests; do
    case "$d" in
      "$IMAGE@"*)
        d="${d#*@}"
        [[ "$d" =~ ^sha256:[0-9a-f]{64}$ ]] || die "malformed repo digest for $ref: $d"
        echo "$d"
        return ;;
    esac
  done
  die "image $ref has no $IMAGE repo digest"
}

pull_digest() { # registry-ref → digest of what the registry actually serves for it
  docker pull -q "$1" >/dev/null || die "cannot pull $1"
  repo_digest "$1"
}

# Point latest at the verified version digest, or do nothing / refuse:
# absent or older-version latest is completed by digest (never by re-pushing the
# version tag); the same digest is a no-op; a newer version, or the same version
# at a different digest, is ambiguous and refuses.
complete_latest() { # verified version digest
  local vdigest="$1" status ldigest lver
  status="$(manifest_status latest)"
  if [ "$status" = 404 ]; then
    docker tag "$IMAGE@$vdigest" "$IMAGE:latest"
    docker push "$IMAGE:latest"
    echo "latest: pushed $IMAGE@$vdigest"
    return
  fi
  ldigest="$(pull_digest "$IMAGE:latest")"
  if [ "$ldigest" = "$vdigest" ]; then
    echo "latest: already $IMAGE@$vdigest"
    return
  fi
  lver="$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.version"}}' "$IMAGE@$ldigest")" \
    || die "cannot inspect version label of $IMAGE:latest"
  [[ "$lver" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "$IMAGE:latest has no release version label"
  ! version_gt "$lver" "$VERSION" || die "$IMAGE:latest is $lver, newer than $VERSION; latest never moves backward"
  [ "$lver" != "$VERSION" ] \
    || die "$IMAGE:latest claims $VERSION at $ldigest, not the verified $vdigest; refusing ambiguous latest"
  docker tag "$IMAGE@$vdigest" "$IMAGE:latest"
  docker push "$IMAGE:latest"
  echo "latest: moved $lver -> $VERSION ($IMAGE@$vdigest)"
}

publish() {
  local work="${IGNITE_GHCR_WORK:?}" src="${IGNITE_RELEASE_SRC:?}" b ref latest rev status
  b="$work/x/$TOP"
  ref="$IMAGE:$TAG"
  rev="$(git -C "$src" rev-parse HEAD)"
  status="$(manifest_status "$TAG")"
  if [ "$status" = 200 ]; then
    # Recovery: the version tag exists (e.g. latest failed after it was pushed). It is
    # never replaced. Verify the registry image itself, by immutable digest, against
    # this release asset; only then may a missing/outdated latest be completed, and
    # only when this is the newest stable GitHub Release (complete_latest also
    # refuses any backward or ambiguous move).
    local vdigest vdigest2 newest
    vdigest="$(pull_digest "$ref")"
    echo "recovery: $ref exists at $IMAGE@$vdigest; verifying it against the $TAG release asset"
    verify_image "$IMAGE@$vdigest" "$rev" "$b"
    vdigest2="$(pull_digest "$ref")"
    [ "$vdigest2" = "$vdigest" ] || die "$ref changed while it was verified ($vdigest -> $vdigest2); not recovered"
    newest="$(gh api "repos/$REPO/releases/latest" --jq .tag_name)" || die "cannot read the latest release"
    if [ "$newest" != "$TAG" ]; then
      echo "recovered: $ref verified at $IMAGE@$vdigest; latest untouched ($TAG is not the latest stable release $newest)"
      return
    fi
    complete_latest "$vdigest"
    echo "recovered: $ref verified at $IMAGE@$vdigest (never replaced)"
    return
  fi

  latest="$(decide_latest)"
  echo "decide: $latest"

  # The release tag's own packaging (Dockerfile, entrypoint, config) around the release asset.
  IGNITE_DOCKER_IMAGE="$CANDIDATE" bash "$src/deployment/linux/docker/build-image.sh" "$b"
  printf 'FROM %s\n' "$CANDIDATE" | docker build --pull=false \
    --label "org.opencontainers.image.version=$VERSION" \
    --label "org.opencontainers.image.revision=$rev" \
    -t "$ref" -

  verify_image "$ref" "$rev" "$b"

  status="$(manifest_status "$TAG")"
  [ "$status" = 404 ] || die "$ref appeared while building; not replaced"
  docker push "$ref"
  if [ "$latest" = latest=true ]; then
    complete_latest "$(repo_digest "$ref")"
  fi
  echo "published: $ref ($(docker image inspect -f '{{join .RepoDigests " "}}' "$ref")) $latest"
}

case "${1:-}" in
  guard) guard ;;
  source) source_check ;;
  fetch) fetch ;;
  decide-latest) decide_latest ;;
  publish) publish ;;
  *) die "usage: $0 guard|source|fetch|decide-latest|publish" ;;
esac
