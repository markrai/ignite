# Production trust integration (Slice 3D-B2a.4.3g)

**Status:** fail-closed verification boundary only. The production pin remains
`UNPROVISIONED`. No automatic updates, installer activation, first-launch
admission, promotion, or rollback authorization are enabled.

Related: [`kindle-release-manifest.md`](kindle-release-manifest.md) (release-v1),
[`kindle-authenticated-publication-evidence.md`](kindle-authenticated-publication-evidence.md)
(3f test-root evidence).

## 1. Existing signing architecture

| Piece | Contract |
|-------|----------|
| Production pin | `tools/release/kindle-release-public-key.hex` — currently `UNPROVISIONED\n` |
| Host signing | `tools/release/kindle_release_sign.py` (OpenSSL Ed25519, offline private key) |
| On-device verifier | `ed25519-dalek` + `sha2` in `scribe-app` (`release_manifest.rs`) |
| Release-v1 | Fixed six-line ASCII/LF manifest + detached `ignite-kindle-release-signature-v1` |
| 3f evidence | `IGNITE_PUBLICATION_EVIDENCE_V1` signed under `PublicationTestTrustRoot` only |

Release-v1 authenticates the exact customer Scriptlet selected by a future
updater. It does not currently carry component-profile or installed-publication
claims. Slice 3f proved those claims under disposable test keys; this slice
binds them to the **same** production publisher identity without introducing a
second trust root.

## 2. Selected single-authority design

**Chosen: Option B — adjacent publication binding signed by the production pin.**

```text
Production Ed25519 pin
  → release-v1 + detached release signature
  → publication binding + detached binding signature  (same pin)
  → publication-evidence bytes (digest-bound only; claims unverified)
```

Digest authentication of opaque evidence bytes is **not** publication-evidence
schema or component-claim validation. Reports emit
`publication_evidence_digest=authenticated` together with
`publication_evidence_claims=unverified` until a later slice defines a
production evidence format.

The binding message is canonical ASCII/LF:

```text
IGNITE_PUBLICATION_BINDING_V1
schema=1
release_tag=vX.Y.Z
release_version=X.Y.Z
release_manifest_sha256=<64 lowercase hex>
scriptlet_sha256=<64 lowercase hex>
publication_evidence_sha256=<64 lowercase hex>
publisher_authentication_scope=production-pin-required
```

Detached signature schema:
`ignite-kindle-publication-binding-signature-v1`.

### Why Option A (extend release-v1) was rejected

Release-v1 is a **fixed six-line** contract. Existing host and Kindle parsers
reject additional fields, reordered keys, and non-canonical encodings. Extending
the signed message would:

- invalidate every currently signed release-v1 asset shape,
- require a coordinated schema migration for promotion and device verifiers,
- risk changing customer Scriptlet packaging contracts that already hash the
  installer identity declared by release-v1.

An adjacent binding keeps release-v1 bytes stable, reuses the same pin, and
avoids a second publisher root.

## 3. Relationship between release-v1 and publication evidence

| Artifact | Role under production pin |
|----------|---------------------------|
| release-v1 | Authenticates Scriptlet name/bytes/SHA-256 for tag `vX.Y.Z` |
| Publication binding | Authenticates digests of release-v1 bytes, Scriptlet SHA-256, and evidence bytes |
| Publication evidence | Opaque bytes whose digest is bound. Schema/component claims remain unverified in 3g |

Cross-checks after both signatures verify:

1. Binding `release_tag` / `release_version` match authenticated release-v1.
2. Binding `release_manifest_sha256` equals SHA-256 of the exact release-v1 bytes.
3. Binding `scriptlet_sha256` equals release-v1 `sha256`.
4. Binding `publication_evidence_sha256` equals SHA-256 of the evidence bytes.

### Evidence eligibility for production claims

Any entry that claims **production publication-evidence validation** refuses:

- empty evidence
- the disposable placeholder header `IGNITE_PUBLICATION_EVIDENCE_PROD_PLACEHOLDER_V1`
- unknown schemas (no production schema exists yet in this slice)
- 3f `publisher_authentication_scope=test-trust-root-only`

The isolated fixture seam may still digest-bind placeholder bytes under
**test-only** provenance to exercise the binding contract. That outcome never
reports validated component claims and never reports production-pin authentication.

### 3f format migration note

`IGNITE_PUBLICATION_EVIDENCE_V1` trails with
`publisher_authentication_scope=test-trust-root-only`. That marker is **test
scope**. The production boundary **refuses** any evidence containing it and does
not reinterpret it as production authority.

A future production evidence version is required before component claims can be
validated under the pin.

## 4. Production versus test trust roots

| Path | Trust root | Provenance on success | Accepts 3f test evidence? |
|------|------------|------------------------|---------------------------|
| `authenticate_official_publication_chain` | Repository pin only | `verified-under-production-pin` | No |
| `authenticate_publication_chain_with_key` | Explicit fixture key (tests) | `verified-under-test-only-fixture-key` | No |
| 3f `PublicationTestTrustRoot` | Ephemeral OpenSSL keys | 3f report only | Yes, in 3f module only |

Rules:

- Provenance is carried on the verification result and copied into reports.
  Callers cannot supply a production-pin label over a fixture-key outcome.
- No environment-variable key substitution on the production path.
- Known RFC test vectors remain rejected as the official pin.
- Test keys are never written into `kindle-release-public-key.hex`.
- While the pin is `UNPROVISIONED`, production reports
  `publisher_authentication=unavailable`.

## 5. On-device verifier feasibility

**Feasible with existing Kindle dependencies.**

| Concern | Finding |
|---------|---------|
| Runtime crypto | `ed25519-dalek` and `sha2` already linked into `scribe-app` for release-v1 |
| Desktop Python / OpenSSL on device | Not required and not assumed |
| Binary size | No new crypto subsystem; binding verification reuses dalek |
| Pre-install use | Verification operates on byte slices; does not execute update code |
| Streaming SHA-256 | Scriptlet measurement continues via `AuthenticatedReleaseManifest::verify_installer` (chunked). Binding digests cover already-measured identities |
| Offline | All inputs are local bytes; no network |
| Path / link safety | Out of scope for this verification-only boundary; installation observation remains separate (3e/3f) |

ARMv7 Kindle hardware validation of this new module is not claimed by this slice;
host `cargo test` covers the cryptographic contracts. Device confirmation remains
a later activation prerequisite.

## 6. Required release assets (future, after pin provisioning)

For tag `vX.Y.Z`, authenticated publication would add (in addition to release-v1):

- `ignite-kindle-vX.Y.Z.publication-binding`
- `ignite-kindle-vX.Y.Z.publication-binding.sig`
- publication-evidence bytes (production-scoped format; not the 3f test trailer)

This slice does **not** wire promotion, packaging, or download of those assets.

## 7. Exact verification order

Fail closed at the first refusal:

1. Load production pin → refuse if `UNPROVISIONED` / malformed / known test key.
2. Verify release-v1 signature under that pin; parse canonical six-line manifest.
3. Refuse publication evidence containing `test-trust-root-only`.
4. Verify publication-binding signature under the **same** pin; parse eight-line binding.
5. Cross-check tag/version and the three digests.
6. Emit observation-only report. Do **not** authorize install, launch, promotion,
   or rollback.

Success report fields remain non-authorizing. Provenance is taken from the
result (`verified-under-production-pin` only for the official entry;
fixture-key success reports `verified-under-test-only-fixture-key`):

```text
installation_binding=unverified
execution_binding=unverified
first_launch_admission=unavailable
data_compatibility=unknown
promotion_authorized=false
publication_evidence_digest=authenticated
publication_evidence_claims=unverified
```

## 8. Remaining blockers to production activation

1. Provision a real production Ed25519 public key into the pin (offline ceremony).
2. Define and ship a production-scoped publication-evidence format (no test trailer).
3. Extend release packaging/promotion to emit binding + evidence assets.
4. Wire Kindle download/staging to feed the production verifier **before** any
   installer execution.
5. Separate authorization gates: installation binding, first-launch admission,
   promotion, rollback — still unimplemented as authorizing paths.
6. Hardware ARMv7 confirmation of the binding verifier on-device.

## 9. Rollback / key rotation (not implemented)

- Key rotation requires a deliberate pin update in a new application build; old
  signatures will not verify under a new pin.
- Dual-pin windows, emergency revoke lists, and automatic rollback authorization
  are explicitly out of scope for 3g.
- Verification success must never be treated as rollback permission.

## 10. Implementation map

| Surface | Location |
|---------|----------|
| Rust production boundary | `ignite/kindle/scribe-app/src/publication_trust_boundary.rs` |
| Python host boundary / tests | `deployment/kindle/production_trust_boundary.py` |
| Regressions | `deployment/kindle/test_production_trust_boundary.py` |
| Existing release-v1 | `release_manifest.rs`, `kindle_release_sign.py` |
| 3f test-only path | `authenticated_publication_evidence.py` |
