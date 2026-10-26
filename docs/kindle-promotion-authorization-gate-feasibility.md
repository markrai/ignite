# Kindle promotion authorization gate feasibility (Slice 3D-B2a)

**Status: blocked; the installer-side lock prerequisite is complete, but the lifecycle launch gate,
worker quiescence, and trusted compatibility evidence remain absent. No authorization interface was
implemented.**

Slice 3D-B1 is a read-only observation. Its `REVIEW_REQUIRED` result is not an authorization and
cannot safely be upgraded into one with the current locking arrangements. B2a requires an exclusive
decision boundary, but no existing primitive excludes both installer mutation and a newer lifecycle
launch. Adding a token or preflight command without that boundary would create a stale-authorization
path, so this slice stops at the required feasibility gate.

## Existing boundaries

### Installer (updated by Slice 3D-B2a.1)

`install-runtime.sh` and `uninstall-runtime.sh` now acquire the fixed kernel-backed updater
transaction lock before installed-state mutation, including `recover_install_transaction`. See
[`kindle-updater-transaction-lock.md`](kindle-updater-transaction-lock.md). This closes the
installer/recovery/uninstall race identified here, but does not by itself exclude a lifecycle child
spawn or drain an already queued evidence worker.

The pending-health guard remains a policy check rather than a mutex. Future assessors must take the
updater lock in the documented global order; the B1 shared evidence lock alone still cannot exclude
installer-owned changes.

### Lifecycle and evidence

The evidence lock serializes publication of `launch-health-owner.v1` and
`launch-health-evidence.v1`. It does not serialize child launch. The lifecycle host deliberately
captures a monotonic launch-order token and calls `command.spawn()` before handing ownership work to
an asynchronous evidence worker.

A stale-authorization interleaving is therefore possible:

1. Durable owner/evidence describe a completed, successful launch L1.
2. A separate preflight process takes the evidence lock exclusively.
3. The lifecycle host spawns L2 and hands its pre-spawn order token to an evidence worker.
4. The worker blocks on the evidence lock, leaving L1 as the durable owner during preflight.
5. Preflight validates L1 and releases the lock.
6. The delayed L2 worker claims ownership and replaces the evidence.

The `scribe-app` singleton at `/tmp/scribe-app.lock` prevents two application bodies from running
simultaneously, but it does not prevent lifecycle from spawning L2 or its evidence worker from later
superseding L1. The lifecycle supervisor mutex is process-local and cannot be acquired by a separate
manual preflight invocation. Evidence workers are detached and expose no cross-process quiescence
barrier.

### Data compatibility

Both current canonical records require `data_compatibility=unknown`. No trusted compatibility record,
capability declaration, or device-local validation result exists. Unknown must remain a hard blocker;
a command-line assertion, `--force`, confirmation prompt, or unsigned sidecar would not resolve it.

Actual compatibility evidence needs a separately specified, strictly validated contract. At minimum
it must:

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

## Boundary required before B2a can continue

A safe implementation needs all of the following cooperating changes:

1. A fixed, no-symlink updater transaction lock honored by every installer, recovery, uninstall, and
   future promotion path **before any mutation or cleanup**. **Completed for current install,
   recovery, and uninstall paths in Slice 3D-B2a.1; future promotion must join it.**
2. A lifecycle-owned launch gate in the long-running host. While a decision is active, `go` must not
   spawn a child. The gate must be established through the host's supervisor serialization rather
   than inferred from `/proc`.
3. A bounded evidence-worker quiescence operation. The host must be able to prove that no earlier or
   later ownership worker is queued before the authorization snapshot is taken.
4. An explicit global lock order covering the launch gate, evidence lock, and updater transaction
   lock, with contention and interrupted-owner behavior defined and tested.
5. Revalidation of the complete B1 snapshot after all boundaries are held and immediately before any
   authorization result is returned or a B2b transaction is prepared.

These are behavioral changes to installer/lifecycle coordination and are outside B2a's stated
boundaries. A double-read, process scan, lock-file existence check, or exclusive evidence lock alone
is not an acceptable substitute.

## Proposed authorization binding after the boundary exists

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
retained_inventory_bytes=<canonical decimal>
retained_inventory_sha256=<64 lowercase hex>
launch_owner_uuid=<32 lowercase hex>
launch_owner_sha256=<64 lowercase hex>
health_evidence_sha256=<64 lowercase hex>
data_compatibility=compatible
data_compatibility_evidence_sha256=<64 lowercase hex>
```

After exclusive revalidation, a non-mutating preflight could compare a manually supplied challenge
digest with the digest of those exact bytes. Any changed transaction, executable, inventory, owner,
evidence, or compatibility result would produce a different challenge and fail closed. Merely
invoking the command or repeating a B1 report would not be sufficient.

No challenge should be emitted as authorizable while compatibility is unknown. No authorization
record or persistence format is introduced in this feasibility slice.

## Required B2a regression matrix

Once the coordination boundary exists, host tests must prove that authorization is refused for a
stale challenge, a changed pending transaction or pending digest, damaged A or B, a replaced owner or
evidence record, incomplete readiness, failed or unrequested exits, and unresolved compatibility.
Process integration tests must also hold each lock in turn, start overlapping lifecycle evidence
workers, and race installer recovery against preflight. A successful case must show that the launch
gate prevents a newer worker from being queued, installer mutation cannot enter the boundary, and an
exact compatibility-bound challenge is revalidated rather than trusted from a prior B1 report.

## B2b transaction prerequisites

B2b must not begin until the boundary and compatibility-evidence work above exists. Its crash-safe
transaction must:

1. acquire the lifecycle launch gate and updater transaction lock in the defined order;
2. drain evidence workers and take the evidence lock exclusively;
3. revalidate the manually supplied authorization binding and every B1 check;
4. durably write a prepared promotion journal that identifies both verified A and live B;
5. commit pending-state removal without deleting either executable set first;
6. recover interruption deterministically to verified A-retained/B-pending or B-promoted; and
7. retire A only after the committed promotion is durably recoverable.

Until then, `pending-health.v1` remains authoritative, A remains retained, and no promotion or
rollback is authorized.
