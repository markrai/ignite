# Kindle promotion authorization gate feasibility (Slice 3D-B2a)

**Status: coordination and boundary-aware B1 revalidation are complete through Slice 3D-B2a.3;
trusted compatibility evidence and pre-first-launch admission remain absent, so authorization is
still blocked. No authorization interface was implemented.**

Slice 3D-B1 is a read-only observation. Its `REVIEW_REQUIRED` result is not an authorization and
cannot be upgraded into one by a later caller. The original feasibility review found no primitive
that excluded both installer mutation and a newer lifecycle launch. B2a.1 and B2a.2 added that
boundary; B2a.3 now reruns B1 inside it. Adding an authorization token still remains unsafe until a
trusted positive compatibility decision exists and is bound to the same snapshot.

## Existing boundaries

### Installer (updated by Slice 3D-B2a.1)

`install-runtime.sh` and `uninstall-runtime.sh` now acquire the fixed kernel-backed updater
transaction lock before installed-state mutation, including `recover_install_transaction`. See
[`kindle-updater-transaction-lock.md`](kindle-updater-transaction-lock.md). This closes the
installer/recovery/uninstall race identified here, but does not by itself exclude a lifecycle child
spawn or drain an already queued evidence worker.

The pending-health guard remains a policy check rather than a mutex. Exclusive assessments use the
manual/internal decision boundary in
[`kindle-lifecycle-decision-boundary.md`](kindle-lifecycle-decision-boundary.md), which takes the
updater lock in the documented global order; the B1 shared evidence lock alone still cannot exclude
installer-owned changes.

### Lifecycle and evidence (updated by Slice 3D-B2a.2)

The lifecycle host now admits `go` under a shared cross-process launch gate and acquires child and
worker activity tokens before spawning. A decision takes the launch gate exclusively, excludes a
running or orphaned child, drains worker tokens with a bounded deadline, confirms a completed worker
from the current live host generation, and then takes the evidence lock. The updater transaction
lock is acquired between the launch gate and worker/evidence boundaries.

A stale-authorization interleaving was previously possible:

1. Durable owner/evidence describe a completed, successful launch L1.
2. A separate preflight process takes the evidence lock exclusively.
3. The lifecycle host spawns L2 and hands its pre-spawn order token to an evidence worker.
4. The worker blocks on the evidence lock, leaving L1 as the durable owner during preflight.
5. Preflight validates L1 and releases the lock.
6. The delayed L2 worker claims ownership and replaces the evidence.

This interleaving is now closed: L2 must hold shared launch admission until its worker token is
queued, so an exclusive decision cannot enter between spawn and handoff. A queued worker retains its
token while blocked on the evidence lock. Host death cannot hide a live child because the child
inherits its activity token, and a restarted host refuses decisions until a launch in that new host
generation has completed its terminal evidence publication.

The `scribe-app` singleton remains independent and is not used as promotion evidence. The new gate,
activity tokens, and host-generation endpoint provide the cross-process boundary; the process-local
supervisor mutex alone is still not treated as authority.

### Data compatibility

Both current canonical records require `data_compatibility=unknown`. No trusted compatibility record,
capability declaration, or device-local validation result exists. Unknown must remain a hard blocker;
a command-line assertion, `--force`, confirmation prompt, or unsigned sidecar would not resolve it.

The source-grounded contract is specified in
[`kindle-data-compatibility-contract.md`](kindle-data-compatibility-contract.md). Slice 3D-B2a.4.1
implements an incomplete, unauthenticated compiled capability declaration and documents its source
inventory in
[`kindle-storage-capability-inventory.md`](kindle-storage-capability-inventory.md). It is not
compatibility evidence. Data-writer exclusion, probes, authenticated provenance, and stable-state
evidence remain unimplemented.
Slice 3D-B2a.4.3's repository audit additionally found that authenticated installed provenance
cannot yet be established: the production pin is `UNPROVISIONED`, installation preserves neither
the signed release assets nor a signed component closure, and no exact legacy artifact catalog can
be derived from this checkout. See
[`kindle-installed-release-provenance-feasibility.md`](kindle-installed-release-provenance-feasibility.md).
No authorization input may substitute the unsigned pending record or LKG inventory for that missing
publisher identity.
The Slice 3D-B2a.4.3 verifier is report-only and always uses the compile-time pin; temporary test
keys are not an authorization path. It authenticates a caller-supplied component set, not lifecycle
slot ownership, canonical production paths, or a pending transaction, and always reports
`installation_binding=unverified`. Its successful test-fixture result, if any, is not compatibility
evidence, and its current production result is `AUTHORITY_UNAVAILABLE`.
At minimum the eventual evidence must:

- identify the compatibility-policy/schema version;
- bind the exact pending transaction and pending-record digest;
- bind B's version and both executable identities;
- bind A's retained inventory identity;
- state which notebook, sync, settings, and migration invariants were evaluated;
- identify the device-local data snapshot or complete bounded scan that was evaluated;
- record a positive result from a trusted checker, not an operator assertion; and
- define whether the claim covers B operation only, A rollback readability, or both.

Maintainer-authenticated release capabilities may be one input, but they cannot replace device-local
checks when B may already have written data. The evidence format and checker are separate future work.

### Pre-first-launch correction (Slice 3D-B2a.4.0)

Post-launch promotion assessment is not the first compatibility decision. Current `scribe-app`
startup can complete adoption recovery, migrate a library, reconcile sync state, and repair import
or page state before or after Home readiness. If B performs a write A cannot read, later
`promotion_authorized=false` preserves A's files but does not make executable rollback safe.

The corrected design in
[`kindle-data-compatibility-contract.md`](kindle-data-compatibility-contract.md) therefore requires a
positive bidirectional result before B is selected for normal execution. The authoritative operation
is a future pre-activation candidate-admission transaction: launch gate first, updater lock second,
then child/worker and data-writer exclusion, exact A/B probes and stable roots, durable binding,
activation, pending publication, and commit under one boundary. Lifecycle admission and a
kernel-backed lifecycle-to-application capability enforce the result after reboot and prevent direct
or mixed-tree B execution. A flag, environment variable, process scan, or unsigned marker alone is
not sufficient.

The existing installer cannot provide that boundary: it takes the updater lock without the launch
gate, performs a racy `/proc` scan, and selects B before publishing pending state. B2a.3 also cannot
be reused unchanged because it requires health evidence from a B launch. The required pre-launch
coordinator, data-writer boundary, authenticated capabilities/provenance, no-write probes, stable
root, durable admission binding, and child handoff remain future implementation slices. Until they
exist, `data_compatibility=unknown` blocks both safe first-launch approval and promotion
authorization; this document's authorization binding remains a post-launch contract only.

Read-only firmware-5.18.6 inspection on 2026-10-27 confirmed that `/mnt/us` exposes mode `0777`
through `fuse.fsp`, so it cannot permission-isolate authoritative admission state. Writable ext3
`/var/local/root` is not writable by the framework user and is the current candidate parent for a
future root-owned record. The same inspection confirmed appmgrd's 60-second `go` deadline and
exclusive lock contention/release on existing `/tmp` and `/mnt/us` files. No Slice code was deployed,
no admission state was created, and reboot/power-loss durability remains untested.

## Boundary required before B2a can continue

A safe implementation needs all of the following cooperating changes:

1. A fixed, no-symlink updater transaction lock honored by every installer, recovery, uninstall, and
   future promotion path **before any mutation or cleanup**. **Completed for current install,
   recovery, and uninstall paths in Slice 3D-B2a.1; future promotion must join it.**
2. **Completed in Slice 3D-B2a.2.** A lifecycle-owned launch gate prevents child spawn and worker
   queueing while a decision is active.
3. **Completed in Slice 3D-B2a.2.** Per-worker activity tokens, running-child exclusion, and the
   current host-generation endpoint provide bounded quiescence without relying on `/proc` scans.
4. **Completed in Slice 3D-B2a.2.** The global order is launch gate, updater transaction lock,
   child/worker quiescence, then evidence lock; contention and process death fail closed.
5. **Completed in Slice 3D-B2a.3.** Revalidation of the complete B1 snapshot after all boundaries are
   held, including an exact binding between the live host's latest launch UUID and durable
   owner/evidence state. The host endpoint generation is checked before and after validation.

Items 1–5 are now implemented without authorization or promotion behavior. The manual
`--assess-promotion-with-boundary` interface remains non-authorizing and reports
`data_compatibility=unknown`. A double-read, process scan, lock-file existence check, or exclusive
evidence lock alone is not an acceptable substitute for this complete boundary.

### Data-writer exclusion diagnostic (Slice 3D-B2a.4.4)

The source-grounded writer audit and read-only diagnostic are documented in
[`kindle-data-writer-exclusion.md`](kindle-data-writer-exclusion.md). The diagnostic can acquire the
existing launch/updater/activity/application/adoption/sync/replica/identity locks in the proposed
order without creating them. This proves useful contention behavior, not a promotion boundary:
recovery coordination is partly in-process, lifecycle-host startup precedes `go`, and no continuous
USB or noncooperating-writer gate exists. It therefore never reports complete exclusion and does
not change the blocked B2a authorization finding.

## Proposed authorization binding on the established boundary

The eventual manual authorization challenge should use one canonical, versioned byte sequence such
as:

```text
IGNITE_PROMOTION_AUTHORIZATION_V1
schema=1
pending_txn=<canonical transaction>
pending_sha256=<64 lowercase hex>
candidate_version=<canonical version>
candidate_app_bytes=<canonical decimal>
candidate_app_sha256=<64 lowercase hex>
candidate_lifecycle_bytes=<canonical decimal>
candidate_lifecycle_sha256=<64 lowercase hex>
candidate_host_bytes=<canonical decimal>
candidate_host_sha256=<64 lowercase hex>
retained_inventory_bytes=<canonical decimal>
retained_inventory_sha256=<64 lowercase hex>
launch_owner_uuid=<32 lowercase hex>
launch_owner_sha256=<64 lowercase hex>
health_evidence_sha256=<64 lowercase hex>
data_compatibility=compatible
data_compatibility_evidence_sha256=<64 lowercase hex>
prelaunch_admission_sha256=<64 lowercase hex>
```

After exclusive revalidation, a non-mutating preflight could compare a manually supplied challenge
digest with the digest of those exact bytes. For candidates installed under the corrected protocol,
the binding must also identify the transaction-specific pre-first-launch compatibility/admission
result. Any changed transaction, executable, inventory, owner, evidence, or compatibility result
would produce a different challenge and fail closed. Merely invoking the command or repeating a B1
report would not be sufficient.

No challenge should be emitted as authorizable while compatibility is unknown. No authorization
record or persistence format is introduced in this feasibility slice.

## Remaining authorization regression matrix

Boundary assessment tests now cover changed pending identity, damaged A/B, stale or malformed
owner/evidence, incomplete readiness, installer contention, queued workers, launch exclusion, and
host disappearance/restart. Once trusted compatibility evidence and an explicit authorization
contract exist, additional tests must refuse stale challenges and changed compatibility bindings.
An exact compatibility-bound challenge must be revalidated inside this boundary rather than trusted
from a prior B1 or B2a.3 report.

## B2b transaction prerequisites

B2b must not begin until compatibility evidence, the pre-first-launch gate for supported candidate
installs, and explicit authorization are added on top of this boundary. A candidate installed by an
older path must be classified from its current data and cannot be presumed to have passed that gate.
Its crash-safe transaction must:

1. acquire the lifecycle launch gate and updater transaction lock in the defined order;
2. drain evidence workers and take the evidence lock exclusively;
3. revalidate the manually supplied authorization binding and every B1 check;
4. durably write a prepared promotion journal that identifies both verified A and live B;
5. commit pending-state removal without deleting either executable set first;
6. recover interruption deterministically to verified A-retained/B-pending or B-promoted; and
7. retire A only after the committed promotion is durably recoverable.

Until then, `pending-health.v1` remains authoritative, A remains retained, and no promotion or
rollback is authorized.
