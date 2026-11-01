# Authenticated Kindle release publication readiness (Slice 3D-B2a.4.3k)

**Status:** non-publishing rehearsal. The production public-key pin remains
`UNPROVISIONED`. No GitHub Release is published by this slice. No production
private key is generated or stored in the repository.

Related: [`kindle-production-publication-evidence.md`](kindle-production-publication-evidence.md),
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
   `deployment/kindle/package_release.py` and uploads only:
   - `Install Ignite vX.Y.Z.sh`
   - `Install Ignite vX.Y.Z.sha256`
2. Linux job uploads `ignite-server-*-linux-x86_64.tar.gz` (+ `.sha256`).
3. Offline operator signs release-v1 over the exact Scriptlet digest
   (`tools/release/kindle_release_sign.py sign`) and retains only the
   128-hex signature for promote input.
4. Promote (`release-promote.yml` → `tools/release/promote-release.sh`)
   resolves the designated CI run, fetches artifacts, verifies identity, and
   historically staged Scriptlet + checksums + release-v1 pair + Linux pair.

**Gap closed by 3k preflight/gate:** promotion now also requires the offline
evidence + binding + binding signature directory and refuses publication while
the pin is `UNPROVISIONED` or the authenticated bundle is incomplete.

CI still does **not** upload evidence/binding. Those bytes are produced by the
offline ceremony from exact CI Scriptlet identity plus packaging measurements.
Choosing whether CI should also retain staged payload/profiles for that ceremony
is a maintainer decision (see §5).

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

## 4. Rehearsal procedure (non-publishing)

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
4. **Evidence measurement inputs** — expand the CI Kindle artifact to retain
   staged payload + component profiles, versus requiring the offline operator to
   retain the full package output from the same CI revision.
5. **How offline evidence/binding files reach promote** — secure transfer path
   into `IGNITE_PROMOTE_AUTHENTICATED_ASSETS_DIR` without placing private keys in
   GitHub Actions secrets. The GitHub workflow still lacks this approved handoff.
6. **Failure recovery** — revoke/re-pin procedure if a signature or pin is
   compromised; policy for yanking a bad GitHub Release tag.

Until those decisions are explicit, production publication stays blocked.

### Fixture emitters are not the production ceremony

`deployment/kindle/test_emit_fixture_offline_metadata.py` builds synthetic
component claims from `make_payload()` for harness tests. It must not be treated
as verified CI component provenance. Production evidence requires trusted
packaging measurement inputs corresponding to the exact CI-built Scriptlet;
those measurements are not inferred from the Scriptlet hash alone.

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
2. Wire the chosen offline asset handoff into `release-promote.yml` inputs/env.
3. Expand or document CI retention of packaging measurement inputs if required.
4. Hardware qualification of on-device verification (tracked separately).
5. Only then: publish a real authenticated GitHub Release.

## 8. Explicit non-goals of this slice

- No production key provisioning.
- No change to the `UNPROVISIONED` pin contents.
- No GitHub Actions signing secret.
- No tag push, Release upload, Scriptlet execution, or Kindle install/activation.
