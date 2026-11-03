# Authenticated Kindle release publication readiness (Slices 3D-B2a.4.3k–3o)

**Status:** non-publishing rehearsal. The production public-key pin remains
`UNPROVISIONED`. No GitHub Release is published by this slice. No production
private key is generated or stored in the repository. Slice 3o automates CI
artifact acquisition and local signed-metadata handoff into promote.

Related: [`kindle-ci-packaging-measurement.md`](kindle-ci-packaging-measurement.md),
[`kindle-production-publication-evidence.md`](kindle-production-publication-evidence.md),
[`kindle-production-trust-integration.md`](kindle-production-trust-integration.md),
[`kindle-release-manifest.md`](kindle-release-manifest.md),
[`kindle-verified-update-staging.md`](kindle-verified-update-staging.md),
[`kindle-installation-authorization.md`](kindle-installation-authorization.md).

## 1. Authenticated Kindle bundle

Exact customer-facing Kindle assets for the updater trust chain:

```text
Install Ignite vX.Y.Z.sh
ignite-kindle-vX.Y.Z.manifest
ignite-kindle-vX.Y.Z.manifest.sig
ignite-kindle-vX.Y.Z.publication-evidence
ignite-kindle-vX.Y.Z.publication-binding
ignite-kindle-vX.Y.Z.publication-binding.sig
```

Existing `.sha256` sidecars and the Linux server tarball pair remain separate
GitHub Release companions. They are not substitutes for the authenticated six.

All six Kindle assets must correspond to one release version/tag. The signed
manifest must describe the exact CI-built Scriptlet bytes. Evidence must match
the packaging component contract (22 roles). The binding authenticates manifest,
evidence, and Scriptlet digests under one publisher key.

## 2. Source-of-truth CI path (today)

1. CI (`ci.yml` / `kindle-package`) builds the customer Scriptlet via
   `deployment/kindle/package_release.py` **once** and uploads:
   - Customer artifact `ignite-kindle-release`:
     - `Install Ignite vX.Y.Z.sh`
     - `Install Ignite vX.Y.Z.sha256`
   - Associated build-evidence artifact `ignite-kindle-packaging-measurements`
     (Slice 3l; not a GitHub Release asset):
     - `ignite-kindle-ci-packaging-measurement.v1`
     - `ignite-kindle-component-profile-package.v1`
     - association / equivalence companions
2. Linux job uploads `ignite-server-*-linux-x86_64.tar.gz` (+ `.sha256`).
3. Offline operator validates the measurement against the exact CI Scriptlet
   (`offline_packaging_handoff.py verify`), prepares evidence from those
   measurements, then signs release-v1 + binding under the production key
   (ceremony still blocked while the pin is `UNPROVISIONED`).
4. Promote (`release-promote.yml` → `tools/release/promote-release.sh`)
   resolves the designated CI run, fetches artifacts, verifies identity, and
   stages Scriptlet + checksums + release-v1 pair + Linux pair + authenticated
   evidence/binding.

**Gap closed by 3k preflight/gate:** promotion requires the offline
evidence + binding + binding signature directory and refuses publication while
the pin is `UNPROVISIONED` or the authenticated bundle is incomplete.

**Gap closed by 3l measurements:** CI retains independently verifiable packaging
measurements bound to the same Scriptlet build. CI still does **not** upload
evidence/binding or sign with a production key. See
[`kindle-ci-packaging-measurement.md`](kindle-ci-packaging-measurement.md).

## 3. Signing and promotion trust boundaries

```text
CI builds customer Scriptlet (authoritative bytes)
  → offline measurement + evidence + binding
  → production private key signs release-v1 + binding (same publisher)
  → promote verifies CI provenance + six-asset preflight
  → GitHub Release upload of verified bytes only
  → Kindle verifies against embedded public-key pin
```

Hard rules:

- Production private key never enters the repository, GitHub Release assets,
  build logs, fixture directories, or application packages.
- Fixture keys never substitute for production promotion readiness.
- `UNPROVISIONED` is not production trust.
- Promote must not rebuild or silently replace CI Scriptlet bytes.
- Existing releases are never overwritten; partial upload failure refuses rather
  than leaving an authenticated-looking incomplete release as authoritative.

## 4. Offline ceremony and rehearsal (non-publishing)

### Automated acquire → sign → dry-run (Slice 3o)

Preferred maintainer path. Uses authenticated GitHub API / `gh` for run
identity, then the existing 3n ceremony and Option A local promote handoff
(`IGNITE_PROMOTE_AUTHENTICATED_ASSETS_DIR`). API provenance association is
**not** cryptographic build attestation.

**Release selection policy:** when `--run-id` is omitted, choose the highest
numeric successful completed `ci.yml` run on `main` with event `push` or
`workflow_dispatch`, matching `ignite/VERSION` at `head_sha`, that still
exposes unexpired `ignite-kindle-release`,
`ignite-kindle-packaging-measurements`, and `ignite-server-linux-release`.
Never combine artifacts across runs.

```text
# Networked host (read-only GitHub credentials; no release:write required)
python tools/release/ignite_release.py acquire \
  --tag vX.Y.Z \
  --output acquired-vX.Y.Z
# optional: --run-id <id>

# Transfer only the public acquired/ directory to the offline signing host.
# Do not copy private keys into acquired/.

# Offline signing host (no GitHub network required)
python tools/release/ignite_release.py sign \
  --acquired acquired-vX.Y.Z \
  --private-key <offline.pem> \
  --public-key <publisher.hex> \
  --output signed-vX.Y.Z

# Return signed-vX.Y.Z/promote-authenticated-assets/ (public metadata only).

# Nonpublishing dry run — verifies isolated crypto, requires UNPROVISIONED
# promote-gate refusal, and invokes promote-release.sh dry-run (guard→resolve→
# fetch→verify). Exit 0 only for a complete verified rehearsal.
python tools/release/ignite_release.py dry-run \
  --tag vX.Y.Z \
  --run-id <id> \
  --signed-assets signed-vX.Y.Z \
  --acquired acquired-vX.Y.Z \
  --work dry-run-work \
  --public-key <publisher-or-disposable.hex>
```

Machine-readable `dry_run_outcome=` (and matching exit status):

| Outcome | Exit | Meaning |
| --- | --- | --- |
| `complete-verification-pass` | 0 | Provisioned pin; full shell verify passed |
| `complete-rehearsal-unprovisioned-boundary` | 0 | Shell reached verify; refused at official pin `UNPROVISIONED` |
| `partial-verification` | 2 | Intentional `--skip-promote-shell` (or equivalent partial scope) |
| `verification-failed` | 1 | Resolve/fetch/signature/unrelated shell failure, etc. |
| `verification-not-performed` | 3 | Bash/shell unavailable |

Complete unprovisioned rehearsal requires phase markers
`promote-phase=guard-ok|resolve-ok|fetch-ok|verify` plus
`promote-shell-refusal=official-pin-unprovisioned` — never a generic
substring match on the word `unprovisioned`.

What each check establishes:

- **GitHub API association** — `acquire` / promote `resolve` bind run id, workflow,
  branch/event, and artifact IDs. Not cryptographic build attestation.
- **Internal transfer consistency** — `load_acquired_bundle` revalidates receipts,
  sidecars, measurement/profile, and extracted-tree digests after offline copy.
  Symlinks / non-regular entries in retained trees are refused.
- **Cryptographic integrity** — offline ceremony signatures + isolated six-asset
  preflight under the supplied public key.
- **Official publisher authority** — only the repository pin via production
  promote-gate / `promote-release.sh` verify. While `UNPROVISIONED`, gate refusal
  category must be `official-pin-unprovisioned` (malformed candidates are hard
  failures, not “expected pin refusal”).

`IGNITE_PROMOTE_PUBLIC_KEY_FILE` is dry-run rehearsal only (isolated disposable
key). `publish` / `all` refuse that override; the promote-gate always reads the
repository pin. An inherited override is stripped unless dry-run opts in.

Acquisition emits `acquisition.receipt.v1` / `release-identity.v1`, ceremony
`ci-artifacts/`, `linux/`, and `artifact-trees/` (extracted trees for digest
revalidation — **not** GitHub zip archive digests). Bounded run search
(`MAX_RUN_SEARCH_PAGES`) is recorded when `--run-id` is omitted.

Local promotion handoff (Option A): `dry-run` sets
`IGNITE_PROMOTE_AUTHENTICATED_ASSETS_DIR` and invokes
`tools/release/promote-release.sh dry-run`. Explicit future publication:

```text
# same env as dry-run (without IGNITE_PROMOTE_PUBLIC_KEY_FILE), then:
bash tools/release/promote-release.sh publish   # or all — never automatic
```

**Still blocks production publication today:** repository pin
`UNPROVISIONED`, and the explicit promote publish confirmation.

**Interrupted operations:** acquire and sign use private staging and finalize
only after validation; delete partial staging directories and re-run into a new
unused output path. Do not reuse a truncated destination.

### One-command prepare (no private key)

Place the CI customer Scriptlet and packaging-measurement files under one
directory (flat or nested). Then:

```text
python tools/release/ignite_kindle_offline_sign.py prepare \
  --tag vX.Y.Z \
  --ci-artifacts <dir> \
  --expected-source-revision <40-char sha> \
  --receipt-out prepare.receipt.v1
```

Validates Scriptlet identity, 22-role inventory, canonical package profile, and
the required Scriptlet SHA-256 sidecar. Refuses ambiguous/duplicate CI artifacts
(including symlink aliases). Writes a machine-readable readiness receipt. Does
**not** sign and does **not** establish CI provenance
(`ci_provenance_established_by_this_ceremony=false`;
`publisher_signatures_verified=false`).

### One-command sign (explicit offline private key)

```text
# Optional: export IGNITE_SIGNING_KEY_PASSPHRASE for an encrypted PEM
python tools/release/ignite_kindle_offline_sign.py sign \
  --tag vX.Y.Z \
  --ci-artifacts <dir> \
  --private-key <offline.pem> \
  --public-key <matching.hex> \
  --output <out-dir> \
  --expected-source-revision <40-char sha>
```

Stages the authenticated six-asset bundle privately, independently verifies
signatures and six-asset preflight against staged bytes, then publishes the
completed directory only after validation succeeds. Emits a signing receipt
(`ignite-kindle-vX.Y.Z.signing-receipt.v1`) whose hashes describe those validated
output bytes, plus `promote-authenticated-assets/` containing evidence + binding
+ binding.sig for a future handoff into `IGNITE_PROMOTE_AUTHENTICATED_ASSETS_DIR`.

Requirements:

- Private key must be supplied explicitly; presence on disk alone never signs.
- Binding is parsed/canonicalized before any private-key operation.
- Caller public key must verify both release-v1 and binding signatures.
- Encrypted PEMs use `IGNITE_SIGNING_KEY_PASSPHRASE` (never argv/logs/receipt).
- Output destination must be new/unused; no private-key material in staging.
- Outputs refuse overwrite; no GitHub mutation; Scriptlet is not executed.
- Receipt distinguishes content integrity, signature verification, CI provenance,
  production publisher authority, and production promotion readiness.

Standalone binding signing (same OpenSSL contract; requires digest context):

```text
python tools/release/ignite_kindle_offline_sign.py sign-binding \
  --binding <publication-binding> \
  --release-manifest <release-v1> \
  --publication-evidence <evidence> \
  --private-key <offline.pem> --public-key <matching.hex> --out <binding.sig>
```

Detailed custody / handoff / recovery design:
[`draft/kindle-production-signing-ceremony.md`](draft/kindle-production-signing-ceremony.md).

### Fixture six-asset rehearsal

```text
# After ordinary packaging with --fixture-signing-* (writes fixture-authenticated-assets/)
python deployment/kindle/authenticated_publication_preflight.py rehearse \
  --assets-dir <dir containing the six assets> \
  --expected-tag vX.Y.Z \
  --trust fixture \
  --public-key <disposable.hex> \
  --source-revision <40-char sha>
```

A successful fixture summary may set `fixture_rehearsal_ready=true`. It **must**
keep `production_promotion_ready=false`. Fixture success is never publication
authority. Operator-supplied `--source-revision` / `--expected-workflow` values
are assertions only; CI provenance remains the promote shell resolve/verify path.

### Production promote-gate

```text
python deployment/kindle/authenticated_publication_preflight.py promote-gate \
  --release-candidate-dir <staged nine-file candidate> \
  --tag vX.Y.Z
```

Uses only this tree's repository production pin. No alternate public-key file.
Exits successfully only when `production_promotion_ready=true`. While the pin is
`UNPROVISIONED`, the gate refuses.

### Production status (read-only)

```text
python deployment/kindle/authenticated_publication_preflight.py production-status
```

While the pin is `UNPROVISIONED`, this reports production signing unavailable.

### Promote dry-run

```text
# After fetch inputs are available; performs GitHub reads only, never release create
bash tools/release/promote-release.sh dry-run
```

Requires `IGNITE_PROMOTE_AUTHENTICATED_ASSETS_DIR` pointing at offline
evidence/binding/binding.sig for the same Scriptlet identity. `verify` stages the
complete candidate into `$DIST`, authenticates those staged Kindle bytes, writes
a SHA-256 inventory, and `publish` rechecks the inventory immediately before
`gh release create`.

## 5. Production key custody — decisions requiring maintainer approval

Do **not** treat the following as decided by this slice:

1. **Key generation ceremony** — who generates the Ed25519 keypair, on which
   air-gapped or controlled host, and how the public half is transcribed into
   `tools/release/kindle-release-public-key.hex`.
2. **Private-key custody** — offline HSM vs encrypted disk vs split custody;
   passphrase handling (`IGNITE_SIGNING_KEY_PASSPHRASE` for OpenSSL); backup and
   destruction policy.
3. **Authorized operators** — named roles allowed to sign a release and to run
   promote from `main`.
4. **Evidence measurement inputs** — Slice 3l uploads the compact measurement
   bundle as an associated CI artifact. Whether operators must also retain the
   full staged payload tree beyond that bundle remains a custody preference, not
   a blocker for measurement verification of digests/modes.
5. **How offline evidence/binding files reach promote** — secure transfer path
   into `IGNITE_PROMOTE_AUTHENTICATED_ASSETS_DIR` without placing private keys in
   GitHub Actions secrets. The GitHub workflow still lacks this approved handoff;
   3l deliberately does not wire an unapproved upload/download into promote.
6. **Failure recovery** — revoke/re-pin procedure if a signature or pin is
   compromised; policy for yanking a bad GitHub Release tag.

Until those decisions are explicit, production publication stays blocked.

### Fixture emitters are not the production ceremony

`deployment/kindle/test_emit_fixture_offline_metadata.py` builds synthetic
component claims from `make_payload()` for harness tests. It must not be treated
as verified CI component provenance. Prefer
`deployment/kindle/offline_packaging_handoff.py` with the CI measurement
artifact from the same Scriptlet build. Production evidence requires those
trusted packaging measurements; they are not inferred from the Scriptlet hash
alone.

## 6. GitHub Release publication requirements (when provisioned)

Promote must establish, before `gh release create`:

- Tag/version consistency with `ignite/VERSION` at the recorded commit.
- Designated CI workflow success on `main` for that commit.
- Exact Scriptlet and Linux digests match recorded promote inputs.
- Release-v1 verifies under the official pin for those Scriptlet bytes.
- Offline evidence + binding + binding.sig verify with the same publisher and
  validated production claims.
- Staging directory contains the authenticated Kindle six plus Linux companions.
- No pre-existing release for the tag (assets are not replaced).

Rerun of an identical rehearsal is deterministic and still performs no mutation
in dry-run / preflight modes. A second publish attempt against an existing
release refuses.

## 7. Remaining steps before genuine production signing

1. Maintainer-approved key ceremony and public-key pin replacement.
2. Wire the chosen offline asset handoff into `release-promote.yml` inputs/env
   (still unapproved; promote stays blocked). Local ceremony already emits
   `promote-authenticated-assets/` for that future handoff.
3. CI packaging measurement retention is in place (3l); confirm operator custody
   of the associated artifact alongside the customer Scriptlet.
4. Hardware qualification of on-device verification (tracked separately).
5. Only then: publish a real authenticated GitHub Release.

## 8. Explicit non-goals of this slice

- No production key provisioning.
- This documentation does not authorize changing the production pin; provisioning
  is a separate maintainer action. Test suites must remain compatible once a
  valid official pin is committed.
- No GitHub Actions signing secret.
- No tag push, Release upload, Scriptlet execution, or Kindle install/activation.
