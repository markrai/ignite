# Kindle candidate health evidence (Slice 3D-A)

**Status:** observation only. This design does not promote or roll back an installation and does not
change `pending-health.v1` or `previous-release`.

## Identity and ownership

`scribe-lifecycle` is the sole writer. The production host re-executes it as root, so it can use the
installer-owned `0700` update directory without a new privilege mechanism. Before spawn, the launch
path captures only a `CLOCK_BOOTTIME` ordering token; it performs no evidence filesystem operation.
After spawn, a per-launch worker claims ownership, validates and hashes the candidate, and publishes
evidence. Lifecycle observations are queued to that worker in memory. Claim or evidence failures are
logged by the worker and never cancel or delay the child spawn or `go` acknowledgment.

The writer requires the exact canonical 14-line `pending-health.v1`, canonical version/transaction
fields, and matching byte counts and SHA-256 digests for live `scribe-app` and `scribe-lifecycle`. The
evidence also carries the SHA-256 digest of the complete pending record. Its fresh launch UUID is the
same capability used by the inherited datagram handshake. A wrong UUID cannot set `ready=true`.
Every publication, including a recorder's first, must match the current durable owner while holding
the publication lock. On the same boot, claims compare the pre-spawn monotonic time, process ID, and
per-process sequence. Therefore, a delayed older worker cannot displace a newer claim merely because
it acquires the filesystem lock later. A different boot ID starts a new ordering epoch.

The owner record is exactly seven LF-terminated lines with no trailing data:

```text
IGNITE_LAUNCH_HEALTH_OWNER_V1
schema=1
boot_id=<32 lowercase hex>
monotonic_ns=<canonical decimal>
process_id=<canonical decimal>
sequence=<canonical decimal>
launch_uuid=<32 lowercase hex>
```

## Exact encoding

`/mnt/us/scribe-app/state/update/launch-health-evidence.v1` is ASCII/UTF-8, exactly 19 LF-terminated
lines, with no BOM, CR, NUL, trailing bytes, reordering, or optional fields:

```text
IGNITE_LAUNCH_HEALTH_EVIDENCE_V1
schema=1
candidate_version=<canonical release version>
candidate_txn=<pending-health candidate_txn>
pending_sha256=<64 lowercase hex>
candidate_app_bytes=<canonical decimal>
candidate_app_sha256=<64 lowercase hex>
candidate_lifecycle_bytes=<canonical decimal>
candidate_lifecycle_sha256=<64 lowercase hex>
launch_uuid=<32 lowercase hex>
outcome=<defined value>
ready=<true|false>
readiness=<awaiting|ready|timed_out|unavailable|interrupted>
process=<not_spawned|running|exited|stopped|unknown>
observation_complete=<true|false>
ready_elapsed_ms=<none|canonical decimal>
post_ready_observed_ms=<none|canonical decimal>
qualification=unqualified
data_compatibility=unknown
```

Outcomes are `spawned`, `ready`, `ready_running`, `spawn_failed`, `exited_success`, `exited_failure`,
`timed_out`, `intentional_stop`, `intentional_stop_failed`, `interrupted`, and `unavailable`.
`intentional_stop` requires the deliberately stopped child to exit successfully; signal termination
or a nonzero status is `intentional_stop_failed` with `process=exited`, never the clean
`process=stopped` combination. Contradictory combinations are invalid. `ready` records the matching
Home acknowledgment. If that process is still present 60 seconds later,
`ready_running` records one monotonic checkpoint using Rust `Instant`, never Kindle wall time. An exit
or intentional stop retains whether readiness occurred and the post-readiness duration. The 60-second
point is evidence collection, not a qualification threshold. Its publication is attempted at most
once per launch: failure disables further evidence for that worker rather than retrying hashing, I/O,
or error logging.

Each publication uses its own fixed temporary, file `fsync`, rename, directory `fsync`, and the shared
fixed advisory lock. Existing path components are checked for symlinks and files use `O_NOFOLLOW`.
The evidence is bounded to 4096 bytes and the owner to 512 bytes. A failed publication leaves the
previous complete record intact when possible; there is no accumulating history or retry log.

Every state says `qualification=unqualified`. Readiness does not establish notebook readability, sync
correctness, ongoing stability, data compatibility, or rollback safety. Missing, damaged, stale,
contradictory, or unwritable evidence cannot authorize a future action.

## Slice 3D-B1 read-only assessment

Run the lifecycle binary manually as root with no other arguments:

```sh
/mnt/us/scribe-app/bin/scribe-lifecycle --assess-promotion
```

The command never loads LIPC and has no write, repair, promotion, rollback, or deletion operation. It
opens the existing evidence lock read-only, holds a shared lock, and performs two complete validation
passes over canonical pending state, both live B executables, the closed 18-entry retained A inventory,
current ownership, and evidence. A change or invalid second pass is reported as a concurrent-change
failure. The report ends with `promotion_authorized=false` in every case.

`BLOCKED` (exit 1) means evidence is insufficient, state is damaged, or the assessment raced a state
change. `REVIEW_REQUIRED` (exit 3) means all runtime checks passed, but compatibility remains an
explicit policy blocker. There is intentionally no zero/eligible result while
`data_compatibility=unknown`.

## Slice 3D-B2 authorization contract

The B2a feasibility review identified installer and lifecycle coordination prerequisites. B2a.1,
B2a.2, and B2a.3 now provide the updater transaction lock, lifecycle launch gate, child/worker
quiescence, exclusive evidence boundary, and complete B1 revalidation inside that boundary. No
authorization command has been added because trusted positive data-compatibility evidence remains
absent. See
[`kindle-promotion-authorization-gate-feasibility.md`](kindle-promotion-authorization-gate-feasibility.md).

1. Promotion should initially require an explicit user/maintainer action after an inspectable report.
   That authorization must bind the pending transaction, pending digest, retained inventory identity,
   and evidence launch UUID, and must include a separately resolved data-compatibility decision.
2. Minimal runtime evidence should be a matching, complete `intentional_stop` with `ready=true`, an
   `readiness=ready`, and at least the chosen monotonic post-ready duration (currently 60 seconds).
   3D-B2 must revalidate pending state, B's binaries, and A's inventory. An active `ready_running` record
   is insufficient because the process may exit immediately afterward. `intentional_stop_failed`
   must never satisfy this contract.
3. Repeated successful launches may add confidence and appear in the report, but cannot silently
   promote B. Any future history must be separately bounded.
4. An unrequested post-ready exit invalidates runtime qualification until a later explicitly reviewed
   successful observation. It does not trigger rollback by itself.
5. Promotion needs its own journal and an exclusive decision boundary: revalidate the authorized
   snapshot, write a prepared marker, durably update pending state, commit, and recover
   deterministically. Interruption must leave verified A or live B intact.
6. A can be retired only after committed transaction recovery and explicit promotion authorization.
7. Data compatibility needs a separate format/capability contract and user-visible validation. Until
   then, `data_compatibility=unknown` prevents claims that executable rollback is data-safe.

## First-generation Scribe qualification plan

Before testing, retain the exact installer and signed metadata off-device, create a separate notebook
backup, confirm USB/SSH recovery access, record firmware/free space, and validate A's retained inventory
and the pending record.

1. Launch B normally. Inspect `lifecycle-host.log` for one UUID progressing through `started`, `ready`,
   and, after 60 seconds, `ready_running`.
2. Read (do not edit) the pending, owner, and evidence files. Compare transaction, pending digest,
   executable hashes, owner/evidence launch UUID, outcome, and `qualification=unqualified`.
3. Return to Kindle Home for intentional pause/unload, confirm `intentional_stop`, relaunch, and confirm
   a new UUID replaces the old observation.
4. Revalidate `previous-release` and its 18-entry inventory; confirm no retained file changed.
5. Verify on real `/mnt/us` that rename is atomic for readers, file/directory `fsync` succeed,
   permissions stay private, and evidence survives a normal reboot. Power-loss testing needs a
   separately approved recovery procedure.

Do **not** remove/edit pending state, delete/replace A, invoke another upgrade, promote, roll back,
corrupt live binaries, or use real notebooks as destructive compatibility fixtures before 3D-B.
