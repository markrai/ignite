# Installed release slot binding (Slice 3D-B2a.4.3b)

## Status and feasibility boundary

Ignite can now inspect, without mutation, whether explicitly supplied authenticated component bytes
occupy the paths used by the current live-B or retained-A layout. It cannot yet create or verify a
trusted installation binding. The production Ed25519 pin is still `UNPROVISIONED`; the installer
does not consume or durably publish the authenticated release/component records; schema v1 has no
live/retained component profile; and pathname inspection cannot bind a later `exec` to the same
inode.

**B2a.4.3c:** deterministic package / candidate-B / retained-A-future-layout component profiles
derived from the staged release payload are documented in
[`kindle-release-component-profiles.md`](kindle-release-component-profiles.md). Those profiles are
build evidence only; they do not authenticate installation or satisfy first-launch admission.
The retained-A-future-layout profile describes how the *incoming* release would look if later
retained; it does not authenticate a device's currently retained previous release.

**B2a.4.3e:** disposable installer publication / crash-recovery rehearsal is documented in
[`kindle-installer-publication-rehearsal.md`](kindle-installer-publication-rehearsal.md). It exercises
the real installer under path overrides and observes A/B component identity; it does not authorize
promotion or first-launch admission.
Post-authentication closed-directory and transition-marker revalidation in this inspector closes the
TOCTOU gap between observation and provenance verification.

The diagnostic is observational and always non-authorizing. Even a complete test-key fixture
returns `REVIEW_REQUIRED` with nonzero exit status and:

```text
installation_binding=unverified
execution_binding=unverified
first_launch_admission=unavailable
atomic_snapshot=false
data_compatibility=unknown
promotion_authorized=false
inspection_action=report-only
```

This is the deliberate feasibility boundary for this slice. No installer, lifecycle, activation,
recovery, admission, promotion, or rollback behavior changed.

## Actual installed topology

Ignite does not have two independent selectable slots. During an upgrade, the existing stable files
are copied into a flattened `/mnt/us/scribe-app/previous-release` directory, then B replaces stable
paths. `scribe-lifecycle-host.sh` invokes stable `scribe-lifecycle`, which in turn launches stable
`scribe-app`. Retained A is recovery evidence, not a directly selected execution tree.

The installer:

1. extracts the SFX payload into a package/staging root;
2. snapshots existing stable A into `previous-release.staging` and publishes it as
   `previous-release`;
3. stages B as `.new` files and renames them into stable paths under an activation journal;
4. transforms `registration/ignite.json` into app-registry rows and retains A's handler as
   `appreg-handler.sql`;
5. installs shared recovery files under `/var/local/ignite-recovery`;
6. writes the strict 18-entry retained inventory;
7. writes `pending-health.v1`, binding B's app/lifecycle and the retained inventory to the
   transaction;
8. writes a v2 commit marker, then removes the journal, old snapshot, and commit marker.

On a normally completed upgrade the journal and commit marker are both absent. Their absence alone
is not completion proof; it is meaningful only with a valid pending record, matching retained
inventory, exact stable components, and no known staging remnants. Interrupted activation recovery
can restore A before commit or finish committed cleanup. The existing updater transaction lock
serializes cooperating installer mutations, but this standalone inspector does not acquire the
launch/updater/evidence decision boundary and does not treat two observations as atomic.

## Signed component to installed-path mapping

`PKG` is the extracted `Ignite-Kindle-<version>` payload, `APP` is
`/mnt/us/scribe-app`, `DOCS` is `/mnt/us/documents`, `REC` is
`/var/local/ignite-recovery`, and `PREV` is `APP/previous-release`. The diagnostic requires all roots
explicitly; these production values are documentation, not CLI defaults.

| Signed role | Packaged artifact | Candidate-B destination and consumer | Retained-A representation | Binding status |
|---|---|---|---|---|
| `scribe-app` | `PKG/bin/scribe-app` | `APP/bin/scribe-app`; child launched by lifecycle | `PREV/scribe-app` | exact path; pending and inventory identities available |
| `scribe-lifecycle` | `PKG/bin/scribe-lifecycle` | `APP/bin/scribe-lifecycle`; invoked by host wrapper | `PREV/scribe-lifecycle` | exact path; pending and inventory identities available |
| `scribe-lifecycle-host` | `PKG/host/scribe-lifecycle-host.sh` | `APP/bin/scribe-lifecycle-host.sh`; appreg command | `PREV/scribe-lifecycle-host.sh` | exact path; not bound by pending B |
| `launcher` | `PKG/launcher/Scribe.sh` | `DOCS/Scribe.sh`; Kindle library launcher | `PREV/Scribe.sh` | exact path; not bound by pending B |
| `registration-source` | `PKG/registration/ignite.json` | transformed into appreg database rows | no same-byte file; `PREV/appreg-handler.sql` is a different representation | **unmapped** |
| `icon` | `PKG/assets/scribe-icon.png` | `APP/assets/scribe-icon.png`; app registration | `PREV/scribe-icon.png` | exact path |
| `license-noto-emoji` | `PKG/assets/licenses/noto-emoji/OFL.txt` | same relative path under `APP` | `PREV/OFL.txt` | exact path |
| `license-noto-emoji-description` | optional package file | same relative path under `APP` | `PREV/DESCRIPTION.en_us.html` | exact optional path |
| `license-noto-emoji-upstream-info` | not emitted by current packager | supported optional legacy path under `APP` | `PREV/upstream_info.md` | exact optional path; schema ambiguity remains |
| `license-source-sans-3` | `PKG/assets/licenses/source-sans-3/OFL.txt` | same relative path under `APP` | `PREV/source-sans-3-OFL.txt` | exact path |
| `install-runtime` | `PKG/lib/install-runtime.sh` | `APP/tools/ignite-release/lib/install-runtime.sh`; installer/recovery | `PREV/install-runtime.sh` | exact path |
| `uninstall-runtime` | `PKG/lib/uninstall-runtime.sh` | `APP/tools/ignite-release/lib/uninstall-runtime.sh`; uninstall entry | `PREV/uninstall-runtime.sh` | exact path |
| `backup-runtime` | `PKG/lib/backup-runtime.sh` | `APP/tools/ignite-release/lib/backup-runtime.sh`; backup entry | `PREV/backup-runtime.sh` | exact path |
| `register-helpers` | `PKG/lib/register-helpers.sh` | `APP/tools/ignite-release/lib/register-helpers.sh`; install/uninstall | `PREV/register-helpers.sh` | exact path |
| `install-recovery` | `PKG/lib/install-recovery.sh` | `APP/tools/ignite-release/lib/install-recovery.sh`; installer | `PREV/install-recovery.sh` | exact path |
| `uninstall-entry` | `PKG/Uninstall-Ignite.sh` | `DOCS/Uninstall Ignite.sh`; user launcher | `PREV/Uninstall Ignite.sh` | exact path |
| `backup-entry` | `PKG/Backup-Ignite-Data.sh` | `DOCS/Backup Ignite Data.sh`; user launcher | `PREV/Backup Ignite Data.sh` | exact path |
| `diagnose-entry` | `PKG/Diagnose-Ignite.sh` | `DOCS/Diagnose Ignite.sh`; user launcher | `PREV/Diagnose Ignite.sh` | exact path |
| `recovery-dropbear` | `PKG/recovery/bin/dropbearmulti` | `REC/bin/dropbearmulti`; recovery client | shared tree, not retained with A | candidate exact; **A unmapped** |
| `recovery-elf-sha256` | `PKG/recovery/manifest/elf.sha256` | `REC/manifest/elf.sha256` | shared tree, not retained with A | candidate exact; **A unmapped** |
| `recovery-provenance` | `PKG/recovery/manifest/provenance.txt` | `REC/manifest/provenance.txt` | shared tree, not retained with A | candidate exact; **A unmapped** |
| `recovery-version` | no package artifact | installer generates `REC/manifest/version` | shared tree, not retained with A | **generated/ambiguous for B; A unmapped** |

The retained inventory has 18 entries: the 17 retained roles with direct byte mappings plus
`appreg-handler.sql`. It contains no release version and is an accidental-corruption integrity
record, not publisher-authenticated provenance. Recovery is shared and can be replaced by a later
installation independently of retained A. These gaps are why schema-v1 success cannot mean a
complete installed profile.

System-provided execution dependencies are outside the release role schema: `/bin/sh`, the ELF
loader and shared libraries, appmgrd/LIPC, SQLite, `su`, and other firmware utilities. They must be
covered by a separate platform/firmware compatibility contract; authenticating Ignite-owned files
does not authenticate those Kindle objects. The current inspector also observes no appreg database
path, so the selected handler remains a registration-mapping gap rather than an inferred consumer.

## Four separate trust questions

1. **Artifact authenticity:** the Ed25519 release manifest proves that an approved publisher signed
   the exact CI installer. This is unavailable in production until the public pin is provisioned.
2. **Component integrity:** the signed component-closure record and exact hashes prove that supplied
   bytes match publisher claims. This is release-scoped and currently has no production author or
   Release publication contract.
3. **Installation binding:** a trusted transaction must prove that those component identities were
   published into the authoritative B/PREV layout for the exact pending transaction. Filenames and
   caller-selected paths are insufficient. This is not implemented.
4. **Execution binding:** a kernel-backed handoff must prove that lifecycle executes the same opened
   object after assessment. A path hash cannot prevent a later rename. This is not implemented.

The inspector reports these properties separately. A test-key signature is labeled
`test-authenticated`, not production-authenticated. `pending_transaction_binding=observed-match`
means only that strict local records and observed bytes agree.

## Read-only inspector

The manually invoked interface is:

```text
scribe-app --inspect-installed-slot-binding \
  --release-role retained-a|candidate-b \
  --expected-transaction YYYYMMDDTHHMMSSZ-N \
  --app-root ABSOLUTE_APP --documents-root ABSOLUTE_DOCS \
  --recovery-root ABSOLUTE_RECOVERY --expected-tag vX.Y.Z \
  --record ABSOLUTE_RECORD --record-signature ABSOLUTE_SIGNATURE \
  --release-manifest ABSOLUTE_MANIFEST \
  --release-manifest-signature ABSOLUTE_SIGNATURE \
  --capabilities ABSOLUTE_CAPABILITIES \
  --component ROLE=ABSOLUTE_PATH ...exactly 22 roles...
```

There are no production-path defaults. Roots must be absolute, traversal-free, distinct, and
non-overlapping. Every mapped component argument must equal the source-derived destination. The
existing provenance verifier still rejects duplicate roles/paths and authenticates all 22 supplied
claims. Unmapped roles remain explicit gaps rather than being assigned invented destinations.

The inspector strictly reads canonical `pending-health.v1` and the 18-entry inventory, checks their
digest/byte relationships and B app/lifecycle identities, verifies retained files, rejects extra
files in release-owned closed directories, and checks known journal/staging remnants. It uses
no-follow opens, one-link regular-file checks through the shared hashing machinery, bounded reads
and enumeration, and before/after metadata identity. It revalidates pending, inventory, mapped
components, and root identities after provenance verification, then repeats closed-directory and
negative transition-state checks so staged extras or `.new`/journal remnants introduced between
observation and authentication cannot pass. It creates no directory, lock, temporary file, repair,
recovery action, network request, or process.

Outcomes are always nonzero:

| Result | Exit | Meaning |
|---|---:|---|
| `REVIEW_REQUIRED` | 2 | authentication and observations agree, but trusted installation/execution publication is absent |
| `AUTHORITY_UNAVAILABLE` or `INCOMPLETE` | 3 | trust pin/evidence/I/O/bounds/interruption prevented a complete conclusion |
| `INCONSISTENT` or `UNSAFE_INPUT` | 1 or 3 | authenticated claims, topology, transaction, types, or path safety disagree |
| `STATE_CHANGED` | 4 | a protected file or directory identity changed during inspection |

The output is a fixed, bounded `IGNITE_INSTALLED_RELEASE_BINDING_V1` report. It contains no caller
path or file-content echo and cannot be treated as an admission token.

## Proposed trusted installation identity

A future installer-integration slice should publish a strict canonical record only after it has
independently authenticated the release and component closure and completed publication under the
updater transaction boundary. A suitable v1 contract is:

```text
IGNITE_INSTALLED_SLOT_IDENTITY_V1
schema=1
release_role=retained_a|candidate_b
pending_transaction=<canonical transaction>
pending_health_sha256=<digest>
release_manifest_sha256=<digest>
release_manifest_signature_sha256=<digest>
component_closure_sha256=<digest of exact signed record>
component_closure_signature_sha256=<digest>
storage_capabilities_sha256=<digest>
component_profile=retained-a-v1|candidate-b-v1
component_count=<fixed profile count>
component=<fixed role>|<fixed absolute destination>|file|<bytes>|<sha256>
... fixed profile order ...
retained_inventory_sha256=<digest or explicit none>
retained_inventory_bytes=<canonical count or 0>
installer_publication_generation=<canonical positive integer>
publication_state=complete
activation_identity_sha256=<pending/journal completion identity>
data_compatibility=unknown
promotion_authorized=false
```

Exact destination strings are valid only for a fixed production profile. Host-test roots need a
separate test-only schema and can never generate device authority. The component profile must settle
registration and generated/shared recovery explicitly; it must not reuse schema v1's
stage-conditional absence as profile proof.

The record is not publisher-signed anew on device. Its publisher facts are the preserved signed
release/component records. Its installation authority comes from an independently authenticated
installer/coordinator publishing it under the common transaction lock into a qualified,
root-controlled, non-public metadata location such as `/var/local/root/ignite-update`. That path is
only a design candidate: Kindle ownership, mount, symlink, fsync/rename, reboot, firmware-update,
and factory-reset behavior remain unqualified. A copy on `/mnt/us` can aid recovery but cannot be
the authority.

Publication order must be: authenticate immutable source records; prepare A and B identities;
publish all component bytes; validate exact destination objects; publish retained inventory and
pending record; fsync; publish the prepared installation record; publish the transaction completion
generation; fsync parent directories; then remove only transaction-owned staging. Recovery must
select either the complete old generation or complete new generation, never synthesize missing
publisher facts from current files.

## Publication and crash audit

| Interruption | Evidence currently reconstructable | Gap / future rule |
|---|---|---|
| Successful upgrade | B app/lifecycle pending hashes; complete A inventory; stable-path observations | no signed records or trusted publication generation retained |
| During component staging | journal and `.new`/staging artifacts may identify transition | no authenticated component-to-transaction prepared record |
| Partial stable-path publication | journal recovery can restore A | mixed B tree exists temporarily; launch/admission must remain excluded |
| Interrupted retained-A capture | `previous-release.staging`, `.prev`, or partial snapshot may exist | no signed A identity; never certify partial inventory |
| After inventory, before pending | inventory may exist without pending | incomplete; no transaction binding |
| After pending, before commit | strict pending/inventory may agree | still precommit; recovery may roll back |
| After commit, before cleanup | v2 marker and journal identify committed transaction | cleanup is recoverable but no authenticated slot record exists |
| Reboot/process death | installer recovery uses journal/marker rules | a prior diagnostic report is stale and has no authority |
| Later install replaces current B/recovery | previous release and shared recovery change | prior observations/records require exact generation invalidation |

First install publishes no A inventory or pending record and therefore cannot satisfy this A/B
inspector. Legacy installations lack authenticated installed provenance and remain unsupported
unless an exact maintainer-signed historical catalog is established from preserved original
artifacts.

## TOCTOU and execution identity

On Unix, the inspector can prove that a particular regular one-link `(device,inode)` was read and
that the pathname still resolved to the same identity at the final check. It detects symlinked
parents, hard links, special files, atomic rename replacement during the observation window, root
replacement, and stable-file version mismatch. This is not an atomic multi-file snapshot and does
not protect the interval after the final check.

In particular it cannot prove that:

- lifecycle will later resolve the same pathname or that the host/lifecycle/app generation remains
  coherent;
- another installer or noncooperating `/mnt/us`/USB writer will not replace a component;
- an already running process maps the inspected executable rather than an older unlinked inode;
- appreg still selects the inspected host wrapper;
- recovery after reboot preserves an earlier observation; or
- scripts and their interpreter closure execute the same inspected objects.

A future coordinator must hold launch gate → updater transaction lock → evidence lock, open and
verify the exact executable closure under that boundary, and hand the same object to the child.
`fexecve` or `execveat(AT_EMPTY_PATH)` are candidates for ELF executables only; kernel/libc support,
`/proc` behavior, close-on-exec handling, scripts/interpreters, appmgrd, and first-generation Scribe
firmware 5.18.6 all require native qualification. A copied report, pathname, environment variable,
or inherited descriptor number is not proof.

## Verification and next slice

Host fixtures use exclusive owned roots and ownership markers; teardown refuses substituted roots.
They cover valid candidate/retained observations, missing/modified/same-size/mixed components,
extra files, duplicate mappings, transaction/inventory mismatches, untrusted paths, production-pin
refusal, symlink/hard-link/special-file inputs, parent/root/component replacement, limits,
interruption, deterministic output, non-authorization, and byte preservation. Existing provenance
tests provide the lower-level replacement-during-hash hook.

Validation performed on 2026-10-30:

- Windows installed-slot unit tests: 10 passed; CLI tests: 2 passed. The Unix-only inode/link test
  is compiled out on Windows.
- Existing provenance unit/CLI tests: 15/3 passed; release-manifest tests: 14 passed; capability
  golden tests: 6 passed; compatibility-preflight tests: 14 passed.
- Lifecycle tests: 28 passed, including 13 admission-model tests.
- Five focused installer retention, pending, inventory, marker, and interrupted-recovery tests
  passed. The full deployment suite did not produce a result before either the 5-minute or
  15-minute Windows bound and is not reported as passed or failed.
- Windows all-target `cargo check` passed. Ordinary library Clippy completed with only pre-existing
  warnings outside this slice; warnings-as-errors was blocked by existing Rust-1.96 lints outside
  the changed files.
- ARMv7 checking was attempted but the host lacks `arm-linux-gnueabihf-gcc`. Native Dragonfly
  execution was not performed because SSH refused the untrusted/unknown host key. Neither result is
  represented as Kindle or Linux qualification.

The next implementation slice should not be admission. It should define and publish authenticated
profile-specific component closure assets from the exact CI installer, provision the production
public pin through the reviewed release chain, and add crash-safe installer persistence of the
canonical slot identity under the common transaction lock. Only after native durability and
recovery qualification should the admission coordinator consume that record and implement a
kernel-backed execution handoff.
