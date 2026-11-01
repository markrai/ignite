# Trusted CI packaging measurement and offline handoff (Slice 3D-B2a.4.3l)

**Status:** build-evidence + offline rehearsal only. Not publisher authentication.
The production public-key pin remains `UNPROVISIONED`. No GitHub Release is
published by this slice. No production private key is generated or stored.

Related: [`kindle-authenticated-release-publication.md`](kindle-authenticated-release-publication.md),
[`kindle-release-component-profiles.md`](kindle-release-component-profiles.md),
[`kindle-production-publication-evidence.md`](kindle-production-publication-evidence.md).

## 1. Goal

Establish a trustworthy, reproducible path:

```text
CI-built Scriptlet
  → verified packaging measurements (same build)
  → offline evidence inputs
  → fixture authenticated publication rehearsal
```

This eliminates dependence on synthetic fixture inventories (for example
`test_emit_fixture_offline_metadata.py` / `make_payload` claims that are not
bound to the CI Scriptlet) without provisioning production signing.

## 2. What is independently recoverable from the Scriptlet

Without executing the Scriptlet, stream-inspecting the SFX (prefix ‖ gzip ‖
ustar) recovers:

| Claim | Recoverable? |
| --- | --- |
| Scriptlet filename / byte count / SHA-256 | Yes (file identity) |
| Embedded archive SHA-256 stamp | Yes |
| Per-member size / SHA-256 / tar mode | Yes (`inspect_embedded_inventory`) |
| Role ↔ path binding | **No** — packaging contract |
| Classification / candidate-B mapping | **No** — packaging contract |
| Package profile framing / `package_profile_sha256` | **No** — packaging-time observation (or attached profile bytes) |
| CI run / source revision provenance | **No** — GitHub workflow identity via promote |

**Do not** infer component identities from the Scriptlet SHA-256 alone. A
measurement that only restates the Scriptlet digest, without verifiable
component claims bound to the embedded inventory and packaging contracts, does
not qualify.

## 3. Measurement artifact format

Header: `IGNITE_CI_PACKAGING_MEASUREMENT_V1` / `schema=1`

Canonical LF-only ASCII. Unsigned build evidence. Scope:
`measurement_scope=ci-build-evidence-only`.

Required fields:

- `version`, `tag` (`v` + version)
- `scriptlet_artifact`, `scriptlet_bytes`, `scriptlet_sha256`
- `payload_dirname`, `package_profile_sha256`, `archive_sha256`
- `equivalence_result=pass`
- `component_role_count=22`
- Exactly 22 `component=` lines in `COMPONENT_ROLES` order
- Optional operator/CI fields: `source_revision` (40 hex), `workflow_run_id`

Bundle directory (not a GitHub Release asset):

```text
packaging-measurements/
  ignite-kindle-ci-packaging-measurement.v1
  ignite-kindle-component-profile-package.v1
  sfx-payload-equivalence.v1          # optional companion
  artifact-association.v1             # CI upload association record
```

Modules:

- `deployment/kindle/ci_packaging_measurement.py` — build / parse / verify
- `deployment/kindle/offline_packaging_handoff.py` — offline verify / evidence /
  fixture rehearsal

## 4. CI integration

`package_release.package()` emits the measurement after SFX↔payload↔profile
equivalence passes, from the **same** staged payload and customer Scriptlet.
Customer Scriptlet bytes are unchanged.

`ci.yml` / `kindle-package`:

1. Builds the Scriptlet once (`package-release.sh`).
2. Copies the measurement bundle to `dist/kindle-packaging-measurements/`.
3. Verifies the measurement against the upload-candidate Scriptlet (no execution).
4. Uploads associated artifact `ignite-kindle-packaging-measurements` alongside
   existing `ignite-kindle-release`.

The user-facing GitHub Release asset contract is unchanged. Measurements are
**not** Release assets.

## 5. Offline verification — separated outcomes

The verifier never collapses trust into a single `verified=true`.

| Outcome | Meaning |
| --- | --- |
| `scriptlet_integrity` | File readable / measurable |
| `scriptlet_identity_match` | Scriptlet size+SHA match measurement |
| `component_consistency` | Embedded inventory matches measured role digests/modes under packaging contracts |
| `package_profile_match` | Attached profile equals the canonical reconstruction from Scriptlet inventory + packaging contracts **and** its digest matches the measurement field |
| `ci_provenance_established_by_this_verifier` | Always `false` offline; require promote resolve/verify |
| `production_publisher_authority` | Always `false` here; requires keyed ceremony + pin |
| `measurement_is_not_publisher_signed` | Always `true` for this artifact |
| `offline_evidence_inputs_ready` | Identity + consistency + profile match |
| `production_promotion_ready` | Always `false` in this slice |

A modified profile whose SHA-256 is also updated in the unsigned measurement
**must refuse**. Digest agreement alone is never sufficient.

Unsigned `verify` / `prepare-evidence` do **not** depend on the production pin
remaining `UNPROVISIONED`. Fixture signing and the production promote gate keep
their pin protections. Operator-asserted `source_revision` / `workflow_run_id`
fields are **not** CI provenance proof.

Measurement and package-profile inputs are read with length-limited
non-following readers (64 KiB each); empty, oversized, symlinked, and
non-regular files are refused.

## 6. Offline handoff contract (non-secret)

Approved direction: production private key stays outside GitHub Actions.

```text
Export public CI measurement material
  → independent offline validation
  → prepare production-format evidence from validated measurements
  → offline sign release-v1 + publication binding
    (tools/release/ignite_kindle_offline_sign.py)
  → return only public signed metadata to promote
```

Preferred one-command path (Slice 3n):

```text
python tools/release/ignite_kindle_offline_sign.py prepare --tag vX.Y.Z --ci-artifacts <dir>
python tools/release/ignite_kindle_offline_sign.py sign \
  --tag vX.Y.Z --ci-artifacts <dir> \
  --private-key <offline.pem> --public-key <matching.hex> --output <out>
```

CI artifacts must include the Scriptlet, its canonical `.sha256` sidecar, the
packaging measurement, and the package profile exactly once under the tree
(flat or nested layouts are fine; duplicates and symlink aliases are refused).
Sign stages outputs privately and finalizes only after signature verification
and six-asset preflight succeed against the staged candidate.

Lower-level rehearsal entry points:

```text
python deployment/kindle/offline_packaging_handoff.py verify \
  --scriptlet Install\ Ignite\ vX.Y.Z.sh \
  --measurement ignite-kindle-ci-packaging-measurement.v1 \
  --package-profile ignite-kindle-component-profile-package.v1 \
  --expected-version X.Y.Z

python deployment/kindle/offline_packaging_handoff.py prepare-evidence \
  --scriptlet ... --measurement ... --package-profile ... --out evidence.bin

python deployment/kindle/offline_packaging_handoff.py rehearse-fixture \
  --version X.Y.Z --scriptlet ... --measurement ... --package-profile ... \
  --fixture-private-key <disposable.pem> --fixture-public-key <disposable.hex> \
  --out-dir <six-asset-dir> --run-preflight
```

**Not decided by this slice** (still maintainer approval):

1. Production key ceremony and pin transcription.
2. Private-key custody / operator identities.
3. How signed evidence/binding reach `IGNITE_PROMOTE_AUTHENTICATED_ASSETS_DIR`.
4. Wiring any external upload/download into `release-promote.yml`.

Until those are approved, production promote remains blocked (`UNPROVISIONED`
and/or missing handoff configuration). Fixture rehearsal never sets
`production_promotion_ready=true`.

## 7. Fixture-versus-production separation

- Fixture keys never substitute for production authority.
- Measurement scope is `ci-build-evidence-only`, not release authentication.
- `test_emit_fixture_offline_metadata.py` remains a harness helper with synthetic
  inventories; prefer this measurement path for ceremony-shaped rehearsals.
- Production pin must stay `UNPROVISIONED` through this slice.

## 8. Explicit non-goals

- No production key generation or pin change.
- No GitHub Actions signing secrets.
- No Release publish / tag create / asset upload.
- No Scriptlet execution, install, A/B activation, or recovery changes.
- No change to the Kindle on-device trust contract or competing signing formats.
