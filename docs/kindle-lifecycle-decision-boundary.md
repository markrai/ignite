# Kindle lifecycle decision boundary and assessment

Slice 3D-B2a.2 provides lifecycle coordination. Slice 3D-B2a.3 runs the existing B1 read-only
assessment inside that boundary. Neither slice authorizes, promotes, rolls back, or retires a
release.

## Manual/internal interface

```text
scribe-lifecycle --check-promotion-decision-boundary
scribe-lifecycle --assess-promotion-with-boundary
```

The coordination-only command acquires every boundary, reports the current host generation's latest completed launch
UUID as coordination context, and releases the boundary only when the in-process check returns. It
does not execute an arbitrary external program and never reports authorization. Its output is:

```text
IGNITE_PROMOTION_DECISION_BOUNDARY_V1
status=ready
launch_uuid=<32 lowercase hexadecimal characters>
authorization=not_granted
```

The former arbitrary `PROGRAM` wrapper is intentionally unsupported. Passing lock descriptors to
an arbitrary program could not guarantee the boundary: that program could close a descriptor and
continue running after releasing one lock. Environment variables and descriptor numbers are not
proof of lock ownership. A future assessor must therefore be an internal, cooperating operation
that retains the private `DecisionBoundary` guard for its complete revalidation. The current check
is only a manually invocable coordination diagnostic.

The command refuses when the lifecycle host is missing, its current generation has no completed
evidence worker, a child is running, a worker is queued or incomplete, any lock is contended, the
control endpoint is malformed, or coordination cannot be proven before the bounded deadline.

`--assess-promotion-with-boundary` acquires the same private boundary and then executes both complete
B1 validation passes in process. The validator receives a reference to the already-held private
exclusive evidence guard; it does not recursively acquire the evidence lock or launch another CLI.
Its report uses `mode=boundary-read-only`, separates `decision_boundary` from candidate checks, and
always ends with `promotion_authorized=false`. A healthy runtime candidate is
`REVIEW_REQUIRED` (exit 3), because `data_compatibility=unknown` remains a policy blocker. Candidate,
coordination, corruption, or stale-generation failures are `BLOCKED` (exit 1). Initial lock
contention retains the boundary's existing refusal status, including exit 75 where applicable.

The original `--assess-promotion` remains available. It still takes the evidence lock shared and is
useful as a standalone B1 diagnostic, but it is not an exclusive decision snapshot.

## Launch admission and worker quiescence

The production lifecycle host owns an exclusive host lease and a mode-0600 Unix control socket
under `/tmp`. Its `go` path:

1. takes the launch gate shared and nonblocking;
2. prepares a shared running-child token and evidence-worker token;
3. records the launch UUID as in progress in the current host generation;
4. spawns the application and queues the evidence worker; and
5. releases launch admission.

The child inherits the running-child token. If the host dies, an orphan application therefore
continues blocking a decision until the application exits. The worker token is acquired before
spawn and moved into the evidence thread, where it remains held through ownership claiming,
hashing, publication, and the terminal report. A worker blocked on the evidence lock remains
visible. A publication failure marks the current launch incomplete. Worker-token acquisition or
thread-coordination failure poisons the current host generation: neither a successful evidence
report nor a later launch can repair missing coordination or establish quiescence. Host-process
exit is the boundary that proves any untracked threads are gone.

The control endpoint reports ready only for the latest launch UUID whose terminal evidence
publication succeeded in the current host process. Late reports from earlier rapid launches are
ignored. A restarted host begins with no completed launch, so evidence from a prior host generation
cannot be silently accepted as latest.

The boundary-aware assessor samples the live host before taking the evidence lock and again after
both B1 passes. It requires the same completed launch UUID and the same owned control-socket device
and inode, while the launch gate remains exclusive. Durable owner and evidence UUIDs must match that
host UUID. Host disappearance, endpoint replacement, or restart therefore blocks the report even if
a test host reports the same UUID after restarting.

While a decision owns the gate exclusively, `go` cannot spawn a child or queue a worker. The host
does not send a successful `goresult` for a refused launch.

## Global lock order

The complete order is:

1. lifecycle launch gate, exclusive for a decision and shared for launch admission;
2. updater transaction lock;
3. running-child exclusion and bounded evidence-worker quiescence;
4. launch-health evidence lock, exclusive for the decision snapshot and both B1 validation passes.

Normal install and uninstall take only item 2. A lifecycle launch takes item 1 shared, prepares
items 3 as shared activity tokens, then releases item 1. Evidence publication later takes item 4
while retaining its worker token. No normal lifecycle path takes the updater transaction lock.

Decision contention is bounded and fails closed. Installer contention retains exit 75. All
decision locks remain private to the in-process operation, are released in reverse acquisition
order after the report has been flushed, and release on error or process death; lock files must
never be unlinked by cooperating code.

## Availability and scope

Coordination setup failure disables decision boundaries. A degraded host holds the launch gate
shared for its lifetime while continuing ordinary launches, so a stale or different host lease
cannot satisfy a decision. If even that blocker cannot be obtained because a decision is already
active, the degraded host refuses `go` until restart rather than launching outside the boundary.
Evidence persistence remains asynchronous, and the launch path adds only small `/tmp` open/flock
operations and thread handoff—no candidate hashing, fsync, update-state reads, or evidence
publication.

The existing `pending-health.v1`, retained A inventory, LKG state, and evidence formats are
unchanged. Runtime readiness is still not promotion authorization, and
`data_compatibility=unknown` remains a hard blocker.
