# Verified Kindle update staging (Slice 3D-B2a.4.3i)

**Status:** download, durable staging, and publisher authentication only.
No Scriptlet execution, installer activation, first-launch admission, promotion,
or automatic updates.

Related: [`kindle-production-trust-integration.md`](kindle-production-trust-integration.md),
[`kindle-production-publication-evidence.md`](kindle-production-publication-evidence.md),
[`kindle-release-manifest.md`](kindle-release-manifest.md).

## 1. Device baseline (TITAN → `ssh root@kindle`)

| Fact | Observed |
|------|----------|
| Device | Kindle Scribe (`armv7l`), Linux 4.9.77-lab126 |
| Firmware | 5.18.6 (`041-juno_1806_barolo_bellatrix3-458622`) |
| RAM | ~964 MiB total; no swap |
| User store | `fsp` on `/mnt/us` (~11.9 GiB, ~8.7 GiB free) over VFAT `/mnt/base-us` |
| Local RW | `/var/local` ext3 (~405 MiB free) — recovery lives here |
| TLS tools | `curl` 7.86.0 + OpenSSL 3.0.8; `wget`; CA bundle present |
| Ignite | `/mnt/us/scribe-app` (binaries, state, previous-release); data `/mnt/us/scribe-data` |
| Symlinks on `/mnt/us` | Not reliable (VFAT/FSP) — staging refuses symlink roots/ancestors/assets |

## 2. Selected staging location

**Production staging root:** `/mnt/us/scribe-app/state/verified-update`

**Fixture rehearsal root:** `/mnt/us/scribe-app/state/verified-update-fixture-3i`

### Why this path

- Under Ignite `state/`, so it is not the live `bin/`, `assets/`, `tools/`, or
  `previous-release` trees and is outside `/mnt/us/scribe-data` and
  `/mnt/us/documents` (notebooks / customer Scriptlets).
- `/mnt/us` has multi-gigabyte headroom suitable for a customer Scriptlet plus
  small metadata; `/var/local` is reserved for recovery and is comparatively small.
- Cleanup is hard-scoped to the staging root and refuses protected prefixes.
- Incomplete work lives in `work/` with an `INCOMPLETE` marker; a published
  `STAGING_RECORD` appears only after authentication + Scriptlet verification.

## 3. Release acquisition origin

Configured origin form: `--github-release owner/repo` (no arbitrary URLs).

Asset URL shape:

```text
https://github.com/<owner>/<repo>/releases/download/<tag>/<asset>
```

Rules:

- HTTPS only (`--proto =https`, `--proto-redir =https`).
- Redirect host allowlist: `github.com`, `objects.githubusercontent.com`,
  `release-assets.githubusercontent.com`, `github-releases.githubusercontent.com`.
- Max redirects: 3. Connect/transfer timeouts enforced. Per-asset size caps
  (metadata 64 KiB; Scriptlet ≤ 768 MiB). Short reads / empty outputs refuse.
- Exactly the six publication assets; duplicates and unexpected names refuse.
- GitHub metadata is **not** publisher authentication.

Production GitHub acquisition remains fail-closed while the pin is
`UNPROVISIONED`. Promote now requires the authenticated six-asset Kindle bundle
(see [`kindle-authenticated-release-publication.md`](kindle-authenticated-release-publication.md))
but will not publish until the pin and offline ceremony are provisioned. Fixture
rehearsal uses `--fixture-assets-dir` + explicit `--fixture-public-key` only.

## 4. Verification sequence

1. Acquire six assets into `work/assets/` (temp `.part` files, then rename).
2. Mark `download_complete` in a draft record under `work/` (still incomplete).
3. `authenticate_official_publication_chain` **or** explicit fixture-key seam.
4. Require release-v1 + binding signatures, same publisher key, tag/version
   consistency, release-manifest digest, evidence digest, production evidence
   schema + 22 component claims when production evidence is present.
5. Stream-verify Scriptlet bytes via `AuthenticatedReleaseManifest::verify_installer`.
6. Publish `assets/` + `STAGING_RECORD` to the staging root; delete `work/`.

Observation flags (not install permission):

```text
download_complete
publisher_authenticated
publication_claims_validated
scriptlet_verified
staging_complete
```

Always retained:

```text
installation_binding=unverified
execution_binding=unverified
first_launch_admission=unavailable
data_compatibility=unknown
promotion_authorized=false
authorization=none
```

## 5. Atomicity and crash recovery

| Failure point | Durable result |
|---------------|----------------|
| Mid-download | `work/` + `INCOMPLETE`; no `STAGING_RECORD` at root |
| Auth/Scriptlet fail | Incomplete work only; `load_staging_record` refuses |
| Crash after draft, before publish | `assets.next` / `STAGING_RECORD.next` leftovers; incomplete |
| Success | Root `assets/` + `STAGING_RECORD` with all five observations true |

`STAGING_RECORD` is an observation hint only. Returning success for an existing
directory always re-verifies the signed assets and Scriptlet under the
**requested** trust mode. A fixture record never satisfies a production request.
Placeholder / digest-only evidence cannot publish a complete record.

Replacement of a completed **different** tag is refused (conservative FSP-safe
policy). Same-tag retry revalidates in place. Publish stages through
`assets.next` / `STAGING_RECORD.next` and publishes the record last.

FSP/VFAT cannot guarantee multi-rename transactions; the readiness gate is the
final `STAGING_RECORD` plus mandatory asset revalidation.

Cleanup deletes only the two exact approved updater roots
(`/mnt/us/scribe-app/state/verified-update` and
`…/verified-update-fixture-3i`) — never nested descendants,
`/mnt/us/scribe-app/state` itself, live binaries, notebooks, or recovery.
Existing ancestors are checked with non-following metadata before staging,
recovery, and cleanup. Path validation alone is not race-free against a
concurrent symlink substitution between check and use.

## 6. Test-only versus production trust

| Mode | CLI | Provenance |
|------|-----|------------|
| Production | `--github-release owner/repo` | Official pin; refuses while `UNPROVISIONED` |
| Fixture harness | `--fixture-assets-dir` **and** `--fixture-public-key` | `verified-under-test-only-fixture-key` |

There is no environment-variable fixture-key override and no user-facing
production option that injects a test key. Fixture success never sets
`promotion_authorized=true` or installation/execution binding.

## 7. CLI

```text
scribe-app --stage-verified-update --expected-tag vX.Y.Z \
  --github-release owner/repo \
  [--staging-root /mnt/us/scribe-app/state/verified-update]

scribe-app --stage-verified-update --expected-tag vX.Y.Z \
  --fixture-assets-dir DIR \
  --fixture-public-key DIR/public_key.hex \
  [--staging-root /mnt/us/scribe-app/state/verified-update-fixture-3i]
```

Defaults: production requests use `/mnt/us/scribe-app/state/verified-update`;
fixture requests use `/mnt/us/scribe-app/state/verified-update-fixture-3i`.
Explicit cross-mode roots are refused. Must be the first (only) diagnostic
command. Does not take the app singleton lock and does not launch the notebook UI.

## 8. Reproducing the Kindle fixture rehearsal

1. Build ARMv7 `scribe-app` (`tools/docker_build_armv7hf.sh` or equivalent).
2. On the host, package fixture-authenticated assets (3h
   `fixture-authenticated-assets/` + disposable public key hex).
3. Copy the **harness binary** (not replacing live `bin/scribe-app`) and the six
   assets into an isolated directory under
   `/mnt/us/scribe-app/state/verified-update-fixture-3i/`.
4. Run `--stage-verified-update` with `--fixture-assets-dir` and
   `--fixture-public-key`.
5. Confirm `STAGING_RECORD` shows fixture provenance and
   `promotion_authorized=false`.
6. Corrupt/truncate an asset and confirm refusal with no complete record.
7. Delete only the fixture staging root.

SCP of assets alone is a transport smoke aid; network acquisition proof requires
the HTTPS GitHub path (or a controlled HTTPS fixture origin using the same
policy). Production downloads remain blocked on the unprovisioned pin even if
bytes transfer.

## 9. Hardware findings and limitations

- ARMv7 verifier runs on-device when a cross-built binary is present (confirmed
  2026-10-31 on this Scribe with a Docker-cross-built harness binary under the
  fixture staging root — live `/mnt/us/scribe-app/bin/scribe-app` untouched).
- Fixture rehearsal produced `staging_complete=true` with
  `publisher_authentication=verified-under-test-only-fixture-key`,
  `publication_evidence_claims=validated`, and `promotion_authorized=false`.
- Corrupt Scriptlet / truncated manifest failed closed with no complete record.
- `/mnt/us` VFAT/FSP lacks robust symlink semantics; staging refuses symlinks.
- Directory rename atomicity is best-effort on FSP; the published `STAGING_RECORD`
  is the readiness gate.
- Official GitHub publication of the full six authenticated assets remains blocked
  on pin provisioning; promote preflight/gate is covered by slice 3k.
  HTTPS hop-by-hop redirect policy is covered by host transport-decision tests
  (allowed CDN hops, forbidden hosts, HTTP downgrade, loops). A live GitHub
  download was not exercised because authenticated assets are not yet promoted.
  Unrestricted `curl --location` is not used.

## 10. Remaining blockers before installation authorization

1. Provision production Ed25519 pin and complete the offline six-asset ceremony
   (see [`kindle-authenticated-release-publication.md`](kindle-authenticated-release-publication.md)).
2. Installation authorization gate: see
   [`kindle-installation-authorization.md`](kindle-installation-authorization.md)
   (3j). Staging observations alone still never authorize install.
3. A/B activation, Scriptlet execution, and recovery remain unchanged and unused
   by this slice.
