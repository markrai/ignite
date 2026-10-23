# Ignite public release packaging

This document is the maintainer procedure for a complete GitHub Release
(Kindle installer + Linux server). It does **not** replace hardware
qualification.

## Artifacts (per version `X.Y.Z`)

Primary GitHub Release assets (exactly these four):

| Artifact | Builder |
|----------|---------|
| `Install Ignite vX.Y.Z.sh` | CI `kindle-package` → `scripts/kindle/package-release.sh` |
| `Install Ignite vX.Y.Z.sha256` | Same |
| `ignite-server-X.Y.Z-linux-x86_64.tar.gz` | CI `ignite-server-linux` |
| `ignite-server-X.Y.Z-linux-x86_64.tar.gz.sha256` | Same |

GitHub also attaches source archives. The developer/support ZIP may be produced
locally; it is **not** a Release asset.

Version authority: `ignite/VERSION`, kept in sync with the Cargo workspace,
`Cargo.lock`, `ignite/web/package.json`, and `package-lock.json`
(`tools/ci/check_version_sync.py`).

## Build ≠ publish (artifact promotion)

CI **builds** Kindle and Linux artifacts on `main`, pull requests, and manual
dispatch (`ignite-kindle-release`, `ignite-server-linux-release`, 30-day
retention). Only a successful run of `.github/workflows/ci.yml` whose
`head_sha` is on `main` and whose `ignite/VERSION` matches the release tag is
eligible to promote. Unmerged PR artifacts, other workflows, tag-push rebuilds,
and commits not on `main` are refused. Tag-push CI must **not** be the source
of published bytes.

Publication is [`.github/workflows/release-promote.yml`](../.github/workflows/release-promote.yml):

1. Freeze the release-candidate commit (docs and authentic screenshots already on it).
2. Download the CI artifacts from that commit’s successful run; record
   `{commit_sha, workflow_run_id, kindle_sha256, linux_sha256}`.
3. Hardware-accept the **exact** Kindle installer bytes (and exercise sync against
   the **same** Linux artifact if sync is advertised).
4. From `main`, dispatch **Promote release** with those recorded values and
   confirm `publish vX.Y.Z from run <id>`.
5. The workflow refuses missing, expired, mismatched, failed, or unconfirmed
   artifacts; never replaces an existing Release; never rebuilds.
6. Stable tags then run GHCR against the **published** Linux tarball (existing
   GHCR guards). `ghcr-publish.sh` still downloads only the Linux pair.

**Never** publish first and compare checksums afterward to discover a mismatch.

Promote-publish permissions are the minimum that can download another run’s
artifacts and create the Release: job-level `contents: write` and `actions: read`
(`GITHUB_TOKEN` only). GHCR keeps `contents: read` + `packages: write` as today.
Do not add a PAT.

### Qualification record (fill before dispatch)

```text
commit_sha=
workflow_run_id=
kindle_artifact=ignite-kindle-release
linux_artifact=ignite-server-linux-release
kindle_sha256=
linux_sha256=
```

CI logs print `kindle_sha256=` and `linux_sha256=` after each successful artifact
job. Retention is 30 days — qualify and promote before the artifacts expire.

## Kindle packaging (CI and local)

Shared staging lives in [`deployment/kindle/package_release.py`](../deployment/kindle/package_release.py).
Thin wrappers:

- Linux / CI: `bash scripts/kindle/package-release.sh` (always rebuilds)
- Windows: `.\scripts\kindle\package-release.ps1` (`-SkipBuild` is **not**
  provenance-safe and is forbidden for official artifacts)

Dropbear is built with [`tools/dropbear/docker_build.sh`](../tools/dropbear/docker_build.sh)
using the **same** digest-pinned `rust:1-bullseye` image as the Kindle Rust
build in [`tools/pinned-docker-images.txt`](../tools/pinned-docker-images.txt)
(gcc-10 / glibc 2.31 armhf-cross; `PINNED.txt` `max_glibc=GLIBC_2.28`). Do not
use `debian:bookworm` for this compile: it emits `__libc_start_main@GLIBC_2.34`
and will not match the pin. `signing_key_sources` in
[`tools/dropbear/PINNED.txt`](../tools/dropbear/PINNED.txt) naming Debian
bookworm is GPG-key provenance only, not the compilation environment.
`elf_sha256` and `elf_bytes` in that file are hard checks. **Do not** update
those pins to silence CI; investigate the toolchain image, upstream tarball,
configure flags, and strip step.

Rust ARMv7hf uses that same digest-pinned `rust:1-bullseye` image and
[`tools/docker_build_armv7hf.sh`](../tools/docker_build_armv7hf.sh)
(`scribe-app/restore_from_server`). `ink-probe` is not packaged.
`kindle-supervisor-harness` must never be staged.

## CI vs hardware

**Automated (CI):** workspace tests, recovery isolation, armv7 check, installer
pytest, shared packager tests, version sync, Linux bundle tests, Docker static
and GHCR mocks, promote-publish failure-path tests, full Kindle Scriptlet build.

**Hardware (required before promote-publish, not CI):**

| Check | Requirement |
|-------|-------------|
| Fresh install | Clean install of the **CI** Scriptlet on a prepared Scribe |
| Upgrade | Upgrade from an existing Ignite install |
| Notebook preservation | Notebooks survive upgrade (`/mnt/us/scribe-data`) |
| Handwriting + restart | Write strokes; restart app/device; content persists |
| Backup verification | Backup helper produces a usable backup; spot-check |
| Diagnostics | Diagnose Ignite shows basic health |
| Pairing + sync | Pair with Ignite Server from the **same** RC Linux artifact; sync round-trip |

Device class: **Verified** matrix only (Scribe 1st gen, firmware **5.18.6**)
unless new evidence is gathered. Do not claim unverified firmware. See
[`docs/compatibility.md`](compatibility.md).

## Maintainer sequence (vX.Y.Z)

1. Finish packaging, promote-publish, docs, Kindle-first README, and authentic
   screenshots on `main`. Any later source change invalidates the candidate.
2. Bump `ignite/VERSION`, Cargo workspace version, `Cargo.lock`,
   `ignite/web/package.json`, and `package-lock.json` together; CI version-sync
   must pass.
3. From that commit’s CI run, download both artifacts; record identity.
4. Complete the hardware table on those exact bytes; capture screenshots if not
   already on the commit (if you add files, that is a new commit → new CI run →
   re-qualify).
5. Dispatch **Promote release** from `main` with recorded identity. That
   confirmation is the publication approval.
6. Confirm the Release assets’ SHA-256s equal the recorded qualification
   digests (promotion correctness, not a substitute for pre-publish checks).
7. GHCR: `docker pull ghcr.io/markrai/ignite-server:v0.9.13` (example version). First-time package
   visibility may still need a manual Public toggle.
8. Update the compatibility matrix with the published Scriptlet hash.
9. Do **not** replace Release assets. Fix forward with a new version.

## Forbidden

- `-SkipBuild` for official / CI artifacts
- Auto-updating Dropbear or Docker image pins to make a build green
- Uploading the support ZIP as a Release asset
- Replacing assets on an existing GitHub Release
- Tag-push rebuild as the source of published bytes
- Publishing before hardware sign-off on the CI Scriptlet
- Generated / AI screenshots in the README
