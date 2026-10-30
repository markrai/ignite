# Kindle stable-state inventory and compatibility preflight

**Status:** Slice 3D-B2a.4.5 read-only foundation. This document does not approve a first launch,
promotion, rollback, migration, or candidate execution. `data_compatibility=unknown` remains a hard
blocker.

## Feasibility decision

The repository can now produce a bounded byte inventory of the closed production data layout, run
the two existing read-only semantic inspectors, and compare a second inventory. Equal inventories
mean only that the two observations had identical canonical bytes. They do not prove that nothing
changed and changed back between observations, and they are not a filesystem-wide atomic snapshot.

A trustworthy bidirectional A/B result is still unavailable:

- B2a.4.4 cannot exclude every writer or continuously exclude USB access;
- B2a.4.3 authenticates a caller-supplied component closure but reports
  `installation_binding=unverified`, and the production public key remains `UNPROVISIONED`;
- no authenticated exact historical A capability/probe catalog exists;
- the retained A and candidate B probe interfaces are not bound to the installed slots or pending
  transaction; and
- B's complete possible write/migration/crash-state closure has not been proven readable and
  recoverable by A.

The production preflight therefore has no `BIDIRECTIONAL_COMPATIBLE` state and never exits zero.

## Fixed-root inventory v1

The CLI receives five explicit absolute paths and requires their fixed relationship:

```text
scribe-app --inspect-compatibility-preflight \
  --app-root APP \
  --data-dir LIBRARY \
  --sync-dir APP/state/sync \
  --device-dir APP/state/device \
  --state-dir APP/state
```

All paths must be traversal-free and distinct. `LIBRARY` may not overlap `APP`. The engine neither
discovers a replacement path nor falls back to production defaults.

| Inventory role | Exact path or closed namespace | Presence | Traversal and allowed objects | Writer and inspector coverage |
|---|---|---|---|---|
| `library` | `LIBRARY` | Required | Recursive; singly linked regular files and directories only; known top-level library namespaces are closed | Runtime, import, inbound materialization, recovery and adoption write it; B2a.4.2a semantically inspects the native library |
| `sync` | `APP/state/sync` | Optional/explicitly absent | Recursive; active sync files, queues, confirmed state, checkpoints and blobs | Sync/adoption/recovery write it; B2a.4.2b inspects it |
| `device` | `APP/state/device` | Optional/explicitly absent | Recursive; device identity, server profile and preferences | Pairing/preferences write it; B2a.4.2b inspects it |
| `state` | `APP/state` | Required | Recursive except registered nested roles; `sync`, `device`, adoption and retired sync roots are represented as delegated entries | Preferences, replica and updater state writers; B2a.4.2b reports uninterpreted state conservatively |
| `activation_run` | `APP/run` | Optional/explicitly absent | Recursive; activation journal/commit marker names are closed | Installer/recovery write it; semantic validation remains an installer/B1 responsibility |
| `library_adopt` | sibling `LIBRARY.adopt` | Optional/explicitly absent | Recursive staged library | Adoption writer; B2a.4.2b recognizes the adoption transaction |
| `sync_adopt` | sibling `APP/state/sync.adopt` | Optional/explicitly absent | Recursive plan, staged sync, and staged profile | Adoption writer; B2a.4.2b recognizes the transaction |
| `retired_library:<uuid>` | siblings `LIBRARY.retired-<canonical UUID>` | Conditional | Recursive; discovered only inside the reserved exact-name namespace | Adoption publication; B2a.4.2b pairs and inspects retired epochs |
| `retired_sync:<uuid>` | siblings `APP/state/sync.retired-<canonical UUID>` | Conditional | Recursive; must pair with its library epoch | Adoption publication; B2a.4.2b pairs and inspects retired epochs |

The inventory excludes installed binaries, `previous-release` executable components, logs,
diagnostic output, and lock-file contents from compatibility semantics. Executable identities are
the provenance verifier's responsibility. Existing lock path presence can be represented as a
delegated/excluded coordination entry, but lock bytes carry no authority.

Only reserved sibling names are enumerated; unrelated siblings are not traversed. A malformed
reserved name, unpaired retired epoch, unknown top-level entry, unsafe filename, missing required
root, or duplicate/conflicting root definition makes coverage incomplete. Unknown bytes are
preserved and hashed where safe; they are never silently discarded.

### Bounds and filesystem rules

`IGNITE_FIXED_ROOT_INVENTORY_V1` uses:

- at most 200,000 entries;
- maximum recursive depth 8;
- maximum individual file size 2 GiB;
- maximum 16 GiB hashed per pass;
- maximum relative path length 512 bytes; and
- at most 512 reported findings.

Traversal order is byte-stable after safe component validation. Files are opened read-only with
the existing no-follow helper. On Unix the inventory requires one hard link, validates descriptor
and pathname device/inode identity, size, link count, mtime and ctime before/after hashing, and
refuses symlinks and special files. Directories are enumerated before and after recursive scanning;
any observed addition, removal, rename, type change, or metadata identity change invalidates the
pass. There is no optimistic retry. Windows exercises portable logic with disposable fixtures;
Unix hard-link and special-file authority is validated natively on Linux.

These checks detect observed races. Without the B2a.4.4 writer boundary, a noncooperating writer
could still change and restore data between observations. The report therefore always retains
`atomic_snapshot=false`.

## Preflight sequence and normalized results

The pre-initialization command performs exactly:

1. fixed-root inventory R1;
2. existing B2a.4.2a library inspection;
3. existing B2a.4.2b external sync/device inspection;
4. fixed-root inventory R2; and
5. canonical digest comparison.

It does not initialize runtime services, take production writer locks, run recovery/adoption,
create a missing root, start sync, use the network, or execute A or B. The orchestration normalizes
each underlying inspector as `passed`, `issues`, or `incomplete` without erasing its original result
token. A partial inspector cannot become complete through composition.

`observational_stability=equal` requires two complete inventory passes and equal canonical SHA-256
digests. Complete unequal passes report `STATE_CHANGED`. Any incomplete pass reports stability as
`unknown` unless it already contains a detected change.

## Canonical report contract

The report begins:

```text
IGNITE_COMPATIBILITY_PREFLIGHT_V1
schema=1
result=REVIEW_REQUIRED|STATE_CHANGED|UNSAFE_INPUT|INTERRUPTED|LIMIT_EXCEEDED|INCOMPLETE
```

It includes both inventory results, digests, entry/byte counts, observational stability, original
and normalized inspector outcomes, and bounded findings. Exit codes are:

- 2: `REVIEW_REQUIRED` or `INCOMPLETE`;
- 3: unsafe input, interruption, or a resource limit; and
- 4: state changed.

There is intentionally no exit 0. Every report contains:

```text
writer_exclusion=unverified
installation_binding=unverified
exact_a_probe=unavailable
exact_b_probe=unavailable
future_write_compatibility=unverified
bidirectional_compatibility=unavailable
atomic_snapshot=false
data_compatibility=unknown
promotion_authorized=false
inspection_action=report-only
```

## Exact A/B reader gate

A future positive internal type must be constructible only inside a trusted coordinator holding the
complete writer/USB boundary across R1, exact A probe, exact B probe, and R2. It must additionally
bind authenticated component/capability closures, installed A/B slot identities, the exact pending
transaction, and the future-write closure required by Section 6.4 of the compatibility contract.

Version comparisons, a valid current-reader result, equal inventory digests, or an operator
confirmation are not substitutes. The current diagnostic stops before executing either retained A
or candidate B.

## Remaining work

Before first-launch admission can consume compatibility evidence, later slices still need:

1. source-complete writer and continuous USB exclusion;
2. authenticated installed-slot provenance and a provisioned production pin;
3. authenticated exact-version capability closures for A and B;
4. non-migrating exact A/B probes and historical cross-version fixtures;
5. a model of every durable B output, migration and crash state readable by A;
6. continuous lifecycle admission and transaction binding; and
7. native Kindle performance/durability qualification within the appmgrd deadline.

No inventory or preflight output is durable authorization. If retained for debugging, it remains
untrusted diagnostic material and must never bypass authoritative revalidation.
