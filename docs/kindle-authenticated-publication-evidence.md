# Authenticated release and publication evidence (Slice 3D-B2a.4.3f)

**Status:** verification-only observation under an explicit **test trust root**.
Production `publisher_authentication` remains `unavailable`. The production
public-key pin stays `UNPROVISIONED`. This slice does not authorize installation,
execution, promotion, rollback, or first-launch admission.

Related: [`kindle-release-manifest.md`](kindle-release-manifest.md) (outer
installer release-v1), [`kindle-installer-publication-rehearsal.md`](kindle-installer-publication-rehearsal.md)
(3e), [`kindle-release-component-profiles.md`](kindle-release-component-profiles.md),
[`kindle-sfx-payload-equivalence.md`](kindle-sfx-payload-equivalence.md) (if present).

## 1. Chain

```text
Test trust-root Ed25519 signature
  → publication-evidence manifest (detached)
  → customer Scriptlet identity + payload/profile equivalence
  → installed B identity (real install-runtime.sh disposable root)
  → independently authenticated retained A identity
```

Both conditions must hold independently before reporting authenticated
publication evidence:

- A valid release signature does **not** override an incomplete installation.
- A complete installation does **not** override an invalid release signature.

## 2. Manifest and signature formats

### Publication evidence (`IGNITE_PUBLICATION_EVIDENCE_V1`)

Canonical ASCII/LF, fixed order, exact line count, no CR/BOM/padding. Built from
verified release artifacts (staged payload + customer Scriptlet), never from an
installed Kindle tree.

Binds:

- release `version` / `tag`
- exact Scriptlet artifact name, bytes, SHA-256
- payload directory name
- package component-profile SHA-256
- closed 22-role set with presence, size, SHA-256, package classification,
  production member mode (`0755`/`0644`/`-`), and candidate-B mapping

Trailer: `publisher_authentication_scope=test-trust-root-only`

### Detached signature

```text
schema=ignite-kindle-publication-evidence-signature-v1
signature=<128 lowercase hex characters>
```

Ed25519 over the exact evidence bytes. Signature verification (OpenSSL
`pkeyutl -verify -rawin`, same helper path as `kindle_release_sign.py`) must
succeed **before** evidence fields are treated as authenticated claims.

The signature is detached so Scriptlet bytes are not circularly hashed into
themselves.

## 3. Trusted versus observed

| Item | Role |
|------|------|
| Signed publication-evidence manifest | Authority for release claims under the **test** trust root |
| Local Scriptlet/payload hashes | Measurements compared to that authority |
| Component profiles / SFX equivalence | Build evidence; not independent authentication |
| Pending-health / LKG inventory | Installation consistency evidence; not publisher authentication |
| Installed path observation (3e) | Measurements of disposable trees |

## 4. Test-key isolation

- Ephemeral Ed25519 keys are generated under test temp dirs via OpenSSL.
- They are never written to `tools/release/kindle-release-public-key.hex`.
- Success paths require an explicit `TestTrustRoot` object; there is no
  environment-variable override that substitutes a test key into production
  verification.
- Reports may say `signature_status=verified-under-test-trust-root:<label>` while
  still forcing `publisher_authentication=unavailable`.

## 5. Independent retained A

Release A and release B each carry their own signed evidence. After A→B upgrade,
retained A is matched against **A's** authenticated component digests with
bidirectional presence checks (expected file must be present; expected absence
must remain absent). B's manifest cannot authenticate retained A — including when
some component bytes are identical across releases (candidate-version collision
and release-bound evidence). Observer `ReleaseEvidence` is derived from signed
claims; caller-supplied evidence is validated against those claims and never
fills identity gaps. LKG inventory consistency alone is not publisher
authentication. An unsigned legacy installation remains unauthenticated even
when its files can be hashed.

## 6. Unverifiable / limited components

- `registration-source` / `appreg-handler.sql`: transformed / register-gated.
- `recovery-version`: generated at install time.
- Shared recovery roles under retained-A layout: not retained as A-private copies.

## 7. Remaining production requirements

1. Maintainer provisions a genuine offline Ed25519 key and replaces
   `UNPROVISIONED` in the shared pin (see release-manifest docs).
2. Publish complementary publication-evidence assets for releases (or extend the
   outer release-v1 story without breaking Scriptlet bytes).
3. Device-side verifier must consume the production pin only — never test roots.
4. First-launch admission and automatic updating remain out of scope.
