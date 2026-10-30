# Authenticated installed-release provenance feasibility (Slice 3D-B2a.4.3)

> **B2a.4.3b follow-up:** [`kindle-installed-slot-binding.md`](kindle-installed-slot-binding.md)
> now maps authenticated component observations onto the source-derived live-B and retained-A
> paths and checks strict pending/inventory relationships. The diagnostic remains read-only and
> non-authorizing: registration/recovery profile gaps, absent installer publication evidence,
> `UNPROVISIONED` production authority, and pathname-to-exec TOCTOU keep installation and execution
> binding unverified.

**Status: read-only format and verifier implemented; production trust chain blocked.** This slice
adds a closed signed-record contract, strict Ed25519/file verifier, and non-authorizing diagnostic.
The production entry point uses only the compile-time official pin, which remains `UNPROVISIONED`,
so it cannot report an authentic installed release today. No production signer, Release asset,
legacy catalog, installer persistence, authorization, promotion, or rollback behavior was added.

The conservative current result remains:

```text
status=AUTHORITY_UNAVAILABLE
failure=official-key-not-provisioned
installation_binding=unverified
data_compatibility=unknown
promotion_authorized=false
inspection_action=report-only
```

This finding is based only on repository state. No GitHub Release, production service, signing
credential, or Kindle was accessed.

## Audited chain of custody

The current release path has a valid authenticated *artifact* boundary but no authenticated
*installed-release* boundary.

| Link | Current authority and substitution protection | What survives installation | Result |
|---|---|---|---|
| CI source and build | `.github/workflows/ci.yml` builds ARMv7 `scribe-app` and `scribe-lifecycle`, stages the fixed Kindle payload, validates the SFX, and uploads the installer plus checksum sidecar. Promotion verifies run, commit, branch, version, artifact set, and recorded hashes without rebuilding. | The commit/run association exists in promotion checks and logs, not in an installed authenticated record. | Strong pre-publication eligibility; no device-local source binding. |
| Release signature | Offline Ed25519 authorization signs the exact canonical six-line release manifest. That manifest binds tag, installer name, byte count, and installer SHA-256. Promotion publishes the unchanged CI installer and checksum plus the manifest and detached signature. | Neither manifest nor signature is in the CI payload or copied by the installer. | Correct authentication for a future downloader, once the production pin is provisioned. |
| Trust anchor | `tools/release/kindle-release-public-key.hex` is compiled into `scribe-app` and consumed independently by promotion. | The repository value is exactly `UNPROVISIONED`; official verification therefore refuses every input. | No production release or installed provenance can currently authenticate. |
| SFX and payload | The authenticated installer digest transitively covers the shell prefix and deterministic gzip/ustar payload. The prefix checks exact total length and the embedded archive SHA-256 before extraction. | A successful install removes the installer from Documents and deletes the extraction tree. | Payload membership is protected while the exact authenticated installer is present, but there is no retained membership proof afterward. |
| Installed B | `install-runtime.sh` copies application, lifecycle, host, launcher, helpers, assets, and customer scripts to several paths. Registration source is transformed into appreg rows. Recovery files are installed separately under `/var/local/ignite-recovery`. | `pending-health.v1` binds only B's version/transaction and app/lifecycle hashes. It does not bind the host wrapper, launcher, helpers, appreg state, recovery executable, capability declaration, installer, release manifest, signature, CI commit, or signing key. | Exact B app/lifecycle integrity is available, not publisher provenance or complete installed closure. |
| Retained A | The installer snapshots a closed 18-entry release-owned tree and creates a canonical SHA-256 inventory. B1 verifies every listed file against that inventory. | The inventory identity is bound into `pending-health.v1`. `retained_version` remains `unknown`, and neither record is signed. Recovery state is not part of the retained tree. | Strong accidental-corruption detection relative to installation, but an attacker can replace A, the inventory, and pending metadata consistently. |
| Storage capabilities | The exact current `scribe-app` can emit a deterministic 12,772-byte declaration with SHA-256 `ee8be9eb76481492454dac8d83d6121c1f9bf79d93dec28632cf3bbfc976fd44`. | No release signature or installed record binds that digest to the app executable. Running B to ask what B supports would also violate a read-only pre-first-launch provenance check and lets an untrusted candidate describe itself. | Useful source/test fact only; not authenticated release evidence. |
| On-device verifier | `release_manifest.rs` verifies with the compile-time pin and deliberately offers no production caller-selected key. | The verifier lives in the mutable candidate `scribe-app` whose identity is one of the facts under assessment. Existing A releases do not provide this verifier. | A candidate-local report is circular until a previously authenticated A or protected broker authenticates the verifier itself. |
| Legacy release | Tag `v0.9.13` predates the release verifier, public pin, and capability declaration. No exact historical Kindle installer or installed binaries are tracked in the repository. | No signed installed metadata exists for legacy A. | Version strings, source tags, rebuilds, and ordinary checksum sidecars cannot establish legacy publisher identity. |

The ordinary `.sha256` sidecar and SFX-embedded checksum remain integrity mechanisms. They are not
independent publisher authentication and cannot fill any missing link above.

## Installed-component coverage matrix

This matrix is the minimum closure a later design must settle before it can claim complete
installed provenance. “Covered now” means only integrity relative to current installer state; it
does not mean authenticated publisher identity.

| Component | CI/SFX source | Current installed evidence | Authenticated installed status |
|---|---|---|---|
| Live application B | `bin/scribe-app` | Pending byte count and SHA-256 | Missing publisher binding |
| Retained application A | live `bin/scribe-app` snapshot | 18-entry LKG inventory | Missing publisher/version binding |
| Lifecycle executable | `bin/scribe-lifecycle` | Pending hash for B; LKG entry for A | Missing publisher binding |
| Lifecycle host wrapper | `host/scribe-lifecycle-host.sh` | LKG entry for A; static installed checks for B | Missing B hash and publisher binding |
| Discovery launcher | `launcher/Scribe.sh` | LKG entry for A | Missing B hash and publisher binding |
| Installer/helper/customer scripts | `lib/*.sh` and three customer scripts | LKG entries for A | Missing B hashes and publisher binding |
| App registration integration | `registration/ignite.json`, transformed into appreg rows | Retained A handler snapshot only | No authenticated canonical live-B representation |
| Recovery executable and metadata | `recovery/bin` and `recovery/manifest` | Dropbear's separate build pin and installed checksum | Not retained with A and not bound to an installed Ignite release |
| Non-executable packaged assets | icon and licence files | Mostly represented in LKG inventory | Incomplete live-B binding; optional-file semantics need closure |
| Storage capability declaration | compiled Rust declaration | Fixed golden output in tests | No signed executable-to-capability binding |
| Release manifest/signature | promotion-created GitHub assets | Not installed or retained | Missing |
| Package/source identity | installer name/size/hash; promotion run/commit checks | Package path only appears transiently in the activation journal | Missing durable authenticated source identity |
| Legacy installation | historical installed bytes | version may be inferred operationally; hashes may be inventoried later | Unsupported without an exact signed catalog entry |

The application launch closure and the full package closure are not identical. A future normative
record must explicitly classify every release-owned file as executable/launch-critical,
installation/recovery-critical, or non-executable data. It must not silently omit recovery or
appreg integration while claiming to describe the complete installed release.

## Why an implementation would be circular today

Four independent blockers prevent the implemented verifier from becoming production authority:

1. **No production key.** The only production verifier entry point correctly returns
   `OfficialKeyNotProvisioned`. Test keys cannot become device evidence.
2. **No signed component closure.** The v1 release manifest signs only the whole installer. A copied
   component list cannot be proven to have come from that installer after the SFX and extraction
   tree are deleted. A whole-file SHA-256 is not a Merkle membership proof.
3. **No durable handoff.** The installer receives only an extracted package directory and never
   receives, validates, or preserves the release manifest/signature. It therefore cannot bind
   installed files to the authenticated outer artifact. Changing this safely requires a future
   installer transaction design; this slice forbids that behavior change.
4. **No independent installed verifier/bootstrap.** The verifier is inside B, and legacy A
   lacks it. Letting B authenticate B, accepting a key or truth value from the command line, or
   trusting unsigned metadata would make substitution trivial.

There is also no defensible legacy catalog input in this checkout. The repository contains source
tags but not the exact published installer/binary bytes, authenticated historical manifests, or
the cross-version evidence needed to assert reader behavior. Rebuilding a tag would produce a new
artifact, not prove the identity of bytes already installed on a device.

## Authenticated record contract implemented in this slice

`storage_release_provenance.rs` implements a separate, domain-separated Ed25519-signed release
component-set record. The existing release-manifest v1 bytes are unchanged. The record authenticates
claims about supplied bytes; it does not by itself establish that those paths are the lifecycle's
actual installed A or candidate B. Production has no author for this record yet; unit tests use
temporary keys that cannot enter the official pin.

The exact record uses canonical ASCII with LF after every line, fixed field order, a closed
component-role order, canonical decimal integers, lowercase hexadecimal digests, canonical SemVer,
no CR/NUL/BOM/trailing bytes, and a small fixed size bound. At minimum its signed bytes must bind:

```text
IGNITE_INSTALLED_RELEASE_PROVENANCE_V1
schema=1
key_id=ed25519-sha256:<sha256-of-raw-public-key>
platform=kindle
architecture=armv7-unknown-linux-gnueabihf
release_version=<canonical version>
release_tag=v<same version>
source_commit=<40 lowercase hex>
ci_workflow_run_id=<canonical positive decimal>
installer_artifact=Install Ignite v<same version>.sh
installer_bytes=<canonical positive decimal>
installer_sha256=<64 lowercase hex>
release_manifest_bytes=<canonical positive decimal>
release_manifest_sha256=<64 lowercase hex>
capabilities_bytes=<canonical positive decimal>
capabilities_sha256=<64 lowercase hex>
component_count=22
component=<fixed-role>|file|<canonical positive decimal>|<64 lowercase hex>
... one line for every closed component role, in the schema order below ...
data_compatibility=unknown
promotion_authorized=false
```

The actual implementation uses `component=<role>|file|<bytes>|<sha256>` or
`component=<role>|absent|0|-`; actual paths are deliberately not signed and are supplied explicitly
to the verifier. The 22 fixed roles are:

```text
scribe-app
scribe-lifecycle
scribe-lifecycle-host
launcher
registration-source
icon
license-noto-emoji
license-noto-emoji-description
license-noto-emoji-upstream-info
license-source-sans-3
install-runtime
uninstall-runtime
backup-runtime
register-helpers
install-recovery
uninstall-entry
backup-entry
diagnose-entry
recovery-dropbear
recovery-elf-sha256
recovery-provenance
recovery-version
```

Schema v1 has an explicit source-grounded presence policy:

- **Mandatory in every supported component-set view:** `scribe-app`, `scribe-lifecycle`, and
  `scribe-lifecycle-host`. A correctly signed `absent` entry for any of these is invalid.
- **Genuinely optional:** `license-noto-emoji-description`, whose source and package copy are
  conditional, and `license-noto-emoji-upstream-info`, which is supported only as optional legacy
  retained state and is not emitted by the current packager.
- **Stage-conditional:** the remaining 17 roles. The current B installation stages most of them as
  required files, but the retained-A snapshot explicitly copies many only when present;
  `registration-source` exists in the package but is transformed rather than retained, recovery is
  shared rather than retained with A, and `recovery-version` is generated during recovery
  installation rather than packaged. Because schema v1 has no authenticated package/live/retained
  stage binding, their absence cannot prove a valid installed slot. It is accepted only as an
  authenticated claim about the supplied set, with `installation_binding=unverified`.

This is the smallest honest common policy for the current single schema. A later persistence design
must either sign a stage/profile discriminator with complete profile-specific rules or use separate
closed schemas for package, live-B, and retained-A evidence. It must not reinterpret a
stage-conditional absence as proof that a canonical installed component is correctly absent.

The detached signature envelope is exact ASCII/LF:

```text
schema=ignite-installed-release-provenance-signature-v1
signature=<128 lowercase hexadecimal characters>
```

It signs the exact record bytes, not a parsed or reserialized object. `key_id` is derived from the
already pinned key; it never selects a caller-provided key. Optional files use explicit `absent`
entries rather than a variable list. The capability digest must be produced as a CI/release input
and signed; executing B to obtain it is not verification.

The signer must consume the exact successful CI installer and independently validated extraction,
not rebuild any component. Promotion must bind the record to the same eligible run and installer
digest. This requires an explicit decision about the public Release asset contract: the current
authenticated Release has six assets, and neither adding a provenance record/signature nor changing
manifest v1 is authorized by this slice.

## Read-only verification interface and failure taxonomy

The manually invoked interface labels one supplied set as A or B at a time, so reports cannot be
silently conflated:

```text
scribe-app --inspect-release-provenance --subject A|B --expected-tag vX.Y.Z \
  --record ABSOLUTE_PATH --record-signature ABSOLUTE_PATH \
  --release-manifest ABSOLUTE_PATH --release-manifest-signature ABSOLUTE_PATH \
  --capabilities ABSOLUTE_PATH \
  --component ROLE=ABSOLUTE_PATH ...exactly 22 roles...
```

The `--subject` label is not signed and is not active-slot, canonical-root, or pending-transaction
evidence. The library API therefore returns an `AuthenticatedReleaseComponentSet`, not an
authenticated installed-slot object. It accepts the same explicit paths and does not search broad
roots, execute A or B, repair state, or accept a caller-selected production trust key. It
authenticates the provenance
record first, independently authenticates and cross-checks release-manifest v1, then checks the
capability bytes and every fixed component. Unix opens use `O_NOFOLLOW`, require one-link regular
files, stream bounded hashes, and compare file identity before and after reads.
The CLI and Rust verification API use the same pre-open path validator for all five metadata paths
and all 22 component paths: every path must be explicit, absolute, and traversal-free. The official
API checks the immutable production pin before this caller-controlled validation, so an
`UNPROVISIONED` or invalid pin remains deterministically `AUTHORITY_UNAVAILABLE`.

It must distinguish at least:

- `AUTHENTIC_AND_CONSISTENT` — signature, schema, source claims, component closure, and every
  supplied byte agree; this still does not establish installed-slot identity, data compatibility,
  or authorization;
- `PROVENANCE_MISSING` — no signed record/signature is available;
- `SCHEMA_UNSUPPORTED` — signed bytes use an unknown schema/key policy/platform;
- `AUTHORITY_UNAVAILABLE` — the production pin is unprovisioned or the trusted verifier itself
  cannot be established;
- `SIGNATURE_INVALID` — malformed envelope or Ed25519 failure;
- `COMPONENT_MISMATCH`, `COMPONENT_MISSING`, or `COMPONENT_UNEXPECTED`;
- `WRONG_PLATFORM_OR_ARCHITECTURE`;
- `INCONSISTENT_SOURCE` — release tag/version, installer, release manifest, CI source, capability,
  or component identity contradict one another;
- `VERIFICATION_INCOMPLETE` — I/O, bounds, file-identity race, lock, or coordination uncertainty;
  and
- `STATE_CHANGED` — any protected input changes during inspection.

Unknown, missing, unsupported, incomplete, or contradictory state is never “clean.” A diagnostic
report must always retain `data_compatibility=unknown`, `promotion_authorized=false`, and
`inspection_action=report-only` until later slices establish those separate facts. It also always
emits `installation_binding=unverified`: success does not establish actual active-slot ownership,
canonical production paths, an atomic snapshot, pending-transaction identity, or first-launch
admission.

Each invocation reports exactly one explicitly named subject (`A` or `B`); callers must run and
retain two separate reports when comparing slots. Reports have a fixed field set and bounded values,
so their size is bounded by the schema rather than filesystem input. Exit status `0` is reserved for
`AUTHENTIC_AND_CONSISTENT`, `1` denotes authenticated-content or structural rejection, and `3`
denotes missing authority/evidence or an inspection that could not be completed. Output write
failure returns the application's ordinary failure status. No path, file content, or unauthenticated
manifest field is copied into a failure report.

The per-file open and before/after identity checks close symlink, hard-link, replacement-during-read,
and short-read cases, but they do not create a cross-file atomic snapshot. An installer or other
writer can still change one component after it has been checked. Consequently this diagnostic is
not a decision boundary: a future assessor must hold the established launch gate, updater
transaction lock, and evidence lock in their documented order for the entire A/B verification and
must reject any inability to establish writer exclusion.

The signed `source_commit` and `ci_workflow_run_id` are authenticated signer claims, not an
independent query or verification of GitHub CI history. The future offline author and promotion path
must validate them against the eligible workflow run and exact artifact before signing.

The record is release-scoped, not device-transaction-scoped. A future persistence transaction must
bind the digest of its exact signed bytes to `pending-health.v1` and the retained inventory. The
current diagnostic deliberately does not infer that missing binding, so transaction replay remains
a production blocker even when a synthetic record verifies.

## Persistence and transition design (not implemented)

The signed bytes and signature are safe to store on `/mnt/us` only as fail-closed evidence: an
unprivileged deletion or replacement must yield missing/invalid evidence, never approval. A future
transaction should additionally preserve an availability copy in a root-owned location whose
Kindle durability has been qualified. Publication must be temporary-file + fsync + atomic rename,
with no-follow parent/path checks, and must be bound to the exact installer transaction.

When B is installed, the transaction must retain A's previously authenticated record with A and
publish B's record without recasting A's unsigned inventory as signed evidence. Promotion from B
to A-for-the-next-update must preserve B's exact signed bytes and signature; it must never create a
new locally “signed” assertion. Interrupted publication must recover to either the complete prior
binding or the complete new pending binding while retaining both executable trees. Those mechanics
belong to a later installer/provenance transaction slice.

## Legacy bootstrap policy

Legacy classification is exactly one of:

1. **Native signed provenance:** only a future release installed through the complete authenticated
   protocol.
2. **Exact catalog match:** a maintainer-signed, closed entry for one explicitly supported historical
   installer/component set, derived from preserved original artifacts and backed by source and
   cross-version tests. The device files must match every exact hash; partial matches fail.
3. **Unsupported legacy:** missing artifacts, custom/repacked builds, modified files, version-only
   matches, or any unknown component. This is the mandatory current fallback.

There are no wildcard version ranges, filename inferences, best-effort matches, or local manual
assertions. An exact catalog signed now can authenticate a maintainer's current assertion about
preserved historical bytes, but it cannot be generated from a source rebuild or unauthenticated
sidecar. A transitional release T also cannot bootstrap itself on-device: T must first be
authenticated by a pre-existing trusted verifier or by an explicitly documented off-device/manual
installation ceremony that verifies the exact installer before it receives root execution.

No legacy entry was created because the required original artifact bytes and evidence are absent.

## Signing authority and key rotation

The production private key must remain offline and outside the repository, CI variables, release
payload, device, and logs. A future authoring command must take the exact eligible CI installer,
validated extracted closure, exact release-manifest bytes, and exact capability declaration as
inputs; it must never rebuild them. Promotion must verify the resulting record and detached
signature with the repository pin before publication.

Schema v1 permits exactly the one compile-time key whose digest appears in `key_id`; that identifier
is descriptive binding, not key selection. Rotation therefore needs an explicit trust transition,
not a second mutable key file or a manifest-supplied key. The safe choices are a release signed by
the old key that installs a verifier containing the new pin, or a new schema with a maintainer-reviewed
dual-key transition policy. Removal of the old key must not strand retained A metadata needed for a
pending transaction. Compromise, revocation, expiry, and rollback policy remain unresolved and are
production provisioning blockers.

## Adversarial regression coverage

Host tests cover valid signed closure, wrong key, altered signed bytes, wrong-domain/malformed/missing
signature, duplicate/reordered/unknown fields, CRLF/NUL/trailing data, unsupported schema, wrong
platform/architecture, noncanonical version/tag/source/integer fields, policy-field tampering,
release-manifest absence/signature/mismatch, capability mismatch, component map omission/duplicate/
unknown role, missing/replaced/truncated/unexpected components, oversized record, stale A/B tag use,
interrupted signature publication, unprovisioned official key, deterministic negative report, and
strict CLI arguments. Unix-only tests cover symlink, hard-link, and special-file refusal. Test keys
and synthetic fixtures have no production override path.

Transaction replay and durable publication cannot be tested until persistence exists. Linux still
needs the Unix-only cases and concurrent replacement checks; Kindle still needs `/mnt/us` no-follow,
hashing-cost, and durability validation.

## Minimal next slice

Before the verifier can return a production-authentic result, maintainers must add an offline author
and authenticated Release transport for the exact record, provision the genuine public key, and
provide original bytes for any intentionally supported legacy release. A following transaction
slice can pass and preserve those authenticated bytes without altering health, promotion, or
rollback policy. The independently trusted A/broker and device transaction binding remain mandatory
before this diagnostic can participate in admission or promotion assessment.
