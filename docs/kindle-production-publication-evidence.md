# Production publication evidence (Slice 3D-B2a.4.3h)

**Status:** canonical production-format evidence and host/device verification are
implemented. Signing for real releases remains blocked while
`tools/release/kindle-release-public-key.hex` is `UNPROVISIONED`. No download,
install, first-launch admission, or promotion authorization is enabled.

Related: [`kindle-production-trust-integration.md`](kindle-production-trust-integration.md)
(3g Option B binding), [`kindle-authenticated-publication-evidence.md`](kindle-authenticated-publication-evidence.md)
(3f test-only evidence), [`kindle-release-manifest.md`](kindle-release-manifest.md).

## 1. Canonical format

Header: `IGNITE_PUBLICATION_EVIDENCE_PRODUCTION_V1`  
Schema: `schema=1`  
Trailer: `publisher_authentication_scope=production-pin-required`

This format is **not** the 3f `IGNITE_PUBLICATION_EVIDENCE_V1` document. The 3f
trailer `publisher_authentication_scope=test-trust-root-only` is refused and
never reinterpreted as production authority.

Exact ASCII/LF field order (then one `component=` line per closed role, then the
scope trailer):

```text
IGNITE_PUBLICATION_EVIDENCE_PRODUCTION_V1
schema=1
version=X.Y.Z
tag=vX.Y.Z
scriptlet_artifact=Install Ignite vX.Y.Z.sh
scriptlet_bytes=<canonical decimal>
scriptlet_sha256=<64 lowercase hex>
payload_dirname=Ignite-Kindle-X.Y.Z
package_profile_sha256=<64 lowercase hex>
component_role_count=22
component=<role>|file|<size>|<sha256>|<class>|<mode>|<mapping>
… (exactly 22 roles, fixed order) …
publisher_authentication_scope=production-pin-required
```

Absent components use `absent|0|-|<class>|-|<mapping>`.

### Mandatory versus optional presence

Presence policy is exported in `publication_component_contract.v1` from
`MANDATORY_PACKAGE_ROLES` / package classifications (not from destination maps):

| Policy | Meaning |
|--------|---------|
| `required` | Packaged `direct-copy` roles must declare `presence=file` |
| `optional` | May be `file` or `absent` |
| `absent-only` | Generated / unpackaged roles (for example `recovery-version`) must be `absent` |

A canonically encoded, correctly signed evidence document that marks a required
role absent fails claim validation in both Python and Rust.

## 2. Component identity semantics

Contracts are owned by `deployment/kindle/release_component_profiles.py` and
exported as `deployment/kindle/publication_component_contract.v1` for the Rust
verifier (`include_str!`). Python regenerates and asserts byte identity; Rust
embeds the same file.

Each role binds:

| Field | Meaning |
|-------|---------|
| presence | `file` or `absent` in the staged package |
| size / sha256 | Exact packaged bytes when present |
| classification | Package-side class (`direct-copy`, `optional`, `generated`, …) |
| mode | Production SFX member mode (`0755` / `0644`) or `-` when absent |
| mapping | Candidate-B destination token from the established map |

Modes follow `expected_member_mode` (executables and `*.sh` → `0755`, else `0644`).

## 3. Production versus test trust

| Path | Provenance | Claims |
|------|------------|--------|
| Official pin entry | `verified-under-production-pin` (unreachable while `UNPROVISIONED`) | Requires production evidence + claim validation |
| Fixture-key seam | `verified-under-test-only-fixture-key` | May report `publication_evidence_claims=validated` |
| 3f test evidence | 3f module only | Never accepted as production evidence |

## 4. Relationship to release-v1 and publication binding

Option B (unchanged):

```text
same Ed25519 publisher
  → release-v1 + signature
  → publication binding + signature
  → production evidence bytes (digest + claim validation)
```

Cross-checks: tag/version, release-manifest digest, Scriptlet SHA-256, evidence
digest, then full production evidence parse against component contracts.

Three distinct states:

1. Cryptographically authenticated bytes (signatures + digests).
2. Validated production component claims (schema/contracts).
3. Installed component observations (still separate; not this slice).

## 5. Build / sign / verify order

1. Stage payload and generate package component profile.
2. Build final customer Scriptlet; take **one** streaming measurement (`size` + SHA-256).
3. Verify SFX ↔ payload ↔ profile equivalence against that path.
4. Remeasure; refuse if identity changed.
5. Build release-v1 and production evidence from that measured identity (no independent rehash without comparison).
6. Build publication binding over release-v1 + evidence digests.
7. Remeasure again; then sign release-v1 and binding with one disposable publisher key.
8. Never rebuild the Scriptlet after step 2.

Residual TOCTOU: remasurement detects mutations across generation steps, but does
not take an exclusive filesystem lock against a concurrent replacer between the
final remasurement and a later independent open.

### Fixture-only packaging invocation

Ordinary `package_release.py` packaging does **not** sign. Explicit opt-in only:

```text
python deployment/kindle/package_release.py \
  --fixture-signing-private-key <disposable.pem> \
  --fixture-signing-public-key <disposable.hex>
```

Both flags are required together. The production pin path is refused. Fixture-
signed metadata is written under `out_root/fixture-authenticated-assets/` and is
never promoted as a customer release asset. The customer Scriptlet and `.sha256`
sidecars remain the ordinary packaging outputs.

Host helper: `deployment/kindle/authenticated_release_assets.py`.

## 6. Release asset names (`vX.Y.Z`)

| Asset | Basename |
|-------|----------|
| Scriptlet | `Install Ignite vX.Y.Z.sh` |
| release-v1 | `ignite-kindle-vX.Y.Z.manifest` |
| release-v1 signature | `ignite-kindle-vX.Y.Z.manifest.sig` |
| publication evidence | `ignite-kindle-vX.Y.Z.publication-evidence` |
| publication binding | `ignite-kindle-vX.Y.Z.publication-binding` |
| binding signature | `ignite-kindle-vX.Y.Z.publication-binding.sig` |

## 7. Cross-language verification

Python builds the asset set. Rust `authenticate_publication_chain_with_key`
consumes those exact host bytes (`IGNITE_PUBLICATION_INTEROP_DIR` harness), then
streams the **actual final Scriptlet file** through
`AuthenticatedReleaseManifest::verify_installer` against release-v1. Both sides
must agree on provenance, digests, validated claims, and Scriptlet identity.
Modified or truncated Scriptlet bytes fail closed.

## 8. Transformed / generated / shared limitations

Evidence records **package-side** classifications and candidate-B destination
tokens. Installed transformed artifacts (for example registration SQL) and
generated on-device files (for example `recovery-version`) are not asserted as
installed observations here. Shared recovery retention semantics remain a later
qualification boundary.

## 9. On-device use prerequisites

- Production pin provisioned offline.
- Release promotion emits the six authenticated assets with exact CI bytes.
- Updater stages assets and verifies **before** any installer execution.
- Separate gates for installation binding, first-launch admission, and promotion.

## 10. Remaining blockers

1. Provision real production Ed25519 public key.
2. Wire promotion CI to emit evidence + binding (still fail-closed today).
3. Kindle download/staging path.
4. Authorization gates (install / launch / promotion / rollback).
5. Hardware ARMv7 confirmation of the production evidence validator.
