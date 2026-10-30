# Kindle data-compatibility contract and pre-first-launch gate (Slices 3D-B2a.4–4.5)

**Status:** feasibility/design, the incomplete compiled capability inventory from Slice
3D-B2a.4.1, the library-only probe from Slice 3D-B2a.4.2a, the external sync/device probe from
Slice 3D-B2a.4.2b, the read-only provenance foundation from Slice 3D-B2a.4.3, the fail-closed
writer-exclusion diagnostic from Slice 3D-B2a.4.4, and the observational fixed-root preflight from
Slice 3D-B2a.4.5. The probes are not a complete A/B compatibility check. No compatibility approval, full-device probe, launch
admission, authorization, promotion, rollback, storage migration, or persistent data format is
implemented. `data_compatibility=unknown` remains a hard blocker.

## Decision

Ignite cannot infer data safety from Home readiness, a successful 60-second observation, or the
fact that A and B use the same directories. It also cannot wait until promotion assessment to
discover incompatibility: startup can already have replaced or migrated shared data. A safe
initial candidate policy needs all of these **before B's first data-mutating execution**:

1. exact, authenticated A and B executable identities;
2. a versioned storage contract from each release that declares exact reader, writer, automatic
   mutation, and migration capabilities;
3. release-time cross-version tests supporting those declarations;
4. a read-only, full-data validation by the exact retained A and candidate B implementations;
5. a stable device-data identity produced while every cooperating Ignite data writer is excluded;
6. a pre-activation/pre-first-launch transaction that admits only that verified B; and
7. after observation, a fresh revalidation of all of the above inside the B2a.3 decision boundary.

For the first implementation, B should be allowed to launch only when B can read current data **and**
every durable format B may write is readable by A. The same property must be re-established before
promotion. Forward-only migrations remain blocked. A
pre-upgrade snapshot is useful recovery material, but restoring it after B has accepted edits would
discard those edits; it is therefore not, by itself, proof of safe rollback.

This is the rule throughout this document:

> No authenticated contract plus current device proof, no first launch and no authorization.

## 1. Source-grounded storage model

### 1.1 Paths shared by A and B

The production host invokes every release with the same paths:

- canonical library: `/mnt/us/scribe-data` (`store::DEFAULT_DATA_DIR` and
  `deployment/kindle/scribe-lifecycle-host.sh`);
- sync/replica/preferences state: `/mnt/us/scribe-app/state/sync` (`--sync-dir`);
- pairing and device state: `/mnt/us/scribe-app/state/device` (`--device-dir`); and
- updater state, including pending and health records: `/mnt/us/scribe-app/state/update`.

`deployment/kindle/lib/install-runtime.sh` replaces release-owned runtime files and explicitly does
not mutate `/mnt/us/scribe-data`. Its retained `previous-release` is an executable snapshot only.
It does not retain an A-era notebook or sync-state snapshot. Consequently, switching executables
back to A is not a data rollback.

### 1.2 Canonical and user-significant data

The live library is a directory tree, not a database:

| Area | Durable files | Role |
|---|---|---|
| Library | `library.meta` | Live notebook order/identity and allocation state. `library::validate_library_manifest` requires schema v1. |
| Notebook | `notebooks/<id>/notebook.info`, `notebook.meta` | Title, page order/identity, and allocation state. `library::read_notebook_info` and `notebook::validate_manifest` require v1. |
| Page | `page.meta`, `background.raw`, `strokes.jsonl`, `edits.jsonl`, optional `texts.jsonl`, optional `links.jsonl` | User-visible page content. `store.rs` currently writes page format v5, stroke records v1/v2, and edit records v1/v2. |
| Folders | `.scribe-folders/folders.json` | User-visible folder titles and membership. `folders::read_folders` requires v1 and sync includes folders in the semantic snapshot. |
| Import provenance | `.scribe-import/receipts.json`, notebook `import-origin.json` | Prevents duplicate Amazon imports and records provenance. `nbk_import::Ledger` uses pending/complete/failed receipts. |
| Deleted/orphaned content | `.notebook-trash`, per-notebook `.page-trash`, and unreferenced notebook/page directories | Not in the live semantic snapshot, but may be the only recoverable copy after a failed quarantine or user deletion. A lossless recovery design must preserve them. |
| Sync binding in library | `.scribe-sync/identity.json` | Stable mapping between local notebook/page names and global sync identities. `sync_local::ensure_sync_identity` creates and updates it atomically. |

The files above are not all equivalent. Live manifests and page content are canonical user data.
Folder state and import receipts are also user-significant. Sync identity is app-owned metadata, but
losing or mismatching it can fork identities or break continuity, so it must be included in an
update-compatibility decision. Trash and orphans are not live state, yet an updater must not delete
or overwrite them.

The library also contains coordination and convenience state:

- `.scribe-sync/local-mutation-gen`, `page-ledger-v1.jsonl`, verified-baseline files, and the
  recovery-maintenance marker support invalidation, diagnostics, and recovery coordination;
- `.scribe-sync/navigation-v1.json` and `active-notebook-v1.json` are explicitly replica-local,
  lossy convenience state (`nav_state.rs`); and
- lock files carry no durable authority; only their kernel locks matter.

Some of this state can be rebuilt or safely defaulted, but an older A must still tolerate the files
that B leaves in the shared tree. The compatibility scanner must classify every accepted path as
canonical, recovery-significant, rebuildable, or an allowed lock/temp path. An unknown path cannot
be silently ignored when making a rollback claim.

### 1.3 Sync and device state outside the library

`sync_outbound.rs` documents the active sync epoch under `--sync-dir`:

- `sync-state.json` (currently schema v2), `genesis.json`, and legacy `base-state.json`;
- `outbox.jsonl`, `accepted.jsonl`, and `inbox.jsonl`;
- `apply.json` for an in-flight materialization;
- `confirmed.log` and `checkpoint/{manifest.json,pages/*}`;
- `blobs/<sha256>`, `replica.json`, optional `config.json`, preferences, and lock files.

This is not merely a cache. In particular, `outbox.jsonl` can be the only durable record of edits
not accepted by a server, while the accepted/inbox/apply/cursor files jointly describe crash-safe
progress. `sync_inbound.rs` advances the server cursor only after local materialization and
verification. `sync_confirmed.rs` uses a framed log plus a page-chunked v3 checkpoint. Treating the
whole sync directory as disposable could lose unsent work or replay the wrong history.

`server_link.rs` stores a server profile and a per-device credential under `--device-dir`.
`prefs::preferences_path` places `preferences.json` in the sync directory, device directory, or
fallback app state. Pairing and preferences are not notebook content, but compatibility should
report them separately because losing them changes sync availability and user settings.

Retired sync epochs (`state/sync-retired-*`) may retain unsent operations. They are not read by the
active session, but they are recovery-significant and must be preserved even if the active A/B
compatibility probe does not parse them.

### 1.4 Writers and concurrency

The normal writer is one `scribe-app` process. `main.rs` holds the `/tmp/scribe-app.lock` singleton
for its full session. Within that process:

- the runtime thread performs pen and structural writes;
- `FsPageStore::append_committed_stroke`, `append_delete_stroke`, and
  `append_replace_strokes` append and fsync page journals;
- notebook/library/folder helpers publish metadata with temp-file, fsync, and rename patterns;
- the runtime thread applies inbound sync at an idle safe point through
  `OutboundSync::apply_inbound` and `LocalMaterializer`;
- background journal, upload, confirmed-state loader, and checkpoint threads mutate sync state;
  the module contract says the runtime thread alone mutates library files; and
- the server-link and NBK-import flows can start library adoption or create native notebooks.

`sync_adopt::adopt_library` is a whole-library writer. It stages a new library and sync epoch, then
publishes them by directory rename after a durable plan. `recover_interrupted_adoption` runs before
the library opens and either discards an uncommitted preparation or rolls a committed adoption
forward. It is intentionally crash-safe, but it means application startup itself can replace the
live library.

Amazon notebook sources under `/mnt/us/.notebooks` and `/var/local/ksdk.content.db` are read-only to
Ignite (`nbk_import.rs`). The optional Linux Ignite Server never directly opens the Kindle library;
its effects arrive through the Kindle sync client.

The updater lock excludes installer, recovery, uninstall, and future promotion mutation. It does
not exclude notebook writes. The B2a.3 launch gate excludes a lifecycle-supervised child and drains
health workers, but it does not presently acquire the app singleton or the sync/adoption locks. A
manually started `scribe-app`, an SSH maintenance process, USB-side modification, or any other
non-cooperating writer is outside the current proof.

### 1.5 Opening is not read-only

Several ordinary open paths can change durable data:

- `library::open_or_create_library_shell_with_hooks` creates a missing library, performs the
  legacy flat-layout migration, and removes a completed migration record;
- `notebook::open_or_create_notebook` calls `store::migrate_all_pages`, which upgrades every
  referenced page v2→v3→v4→v5 before opening the selected page;
- `store::load_existing_page` truncates a torn stroke or edit journal to its verified LF boundary;
- `notebook::repair_background_chrome` can replace `background.raw` and `page.meta`;
- `sync_local::ensure_sync_identity` creates or extends `.scribe-sync/identity.json`;
- `sync_local::build_snapshot` may seal a missing background identity into `page.meta`;
- `OutboundSync::open_with` creates replica/sync state, reconciles journals, recovers inbound
  transactions, and can convert legacy confirmed state; and
- runtime startup calls `recover_interrupted_adoption` and NBK-import recovery before Home becomes
  interactive.

Launch readiness is acknowledged after Home is rendered and a complete event-loop iteration, while
notebook binding is deferred. That milestone is valuable runtime evidence but neither visits all
notebooks nor proves that startup made no compatibility-relevant write.

### 1.6 Actual install-to-first-launch sequence

The following order is supported by the current source. It matters because the durable pending
record is not itself a pre-launch barrier.

1. `install-runtime.sh` obtains the updater transaction lock at lines 19–43. It then uses a `/proc`
   executable scan to refuse an already-running Ignite process (`install-runtime.sh:1069-1080`).
   The scan is not atomic with a later lifecycle `go`; the installer does not take the launch gate.
2. Under that updater lock it runs `recover_install_transaction`, refuses an already-committed
   pending candidate, and snapshots live A into `previous-release`
   (`install-runtime.sh:1104-1118`). The snapshot includes release files and app registration, not
   notebook, sync, or device data.
3. It copies B to `.new` paths, creates an activation journal and transaction identifier, then
   renames B's `scribe-app`, `scribe-lifecycle`, host wrapper, and helpers into their stable paths
   (`install-runtime.sh:1120-1211`). At the first rename B is selectable by an independently started
   process even though the transaction is not committed.
4. It registers the stable host path with appmgrd and verifies the mixed release tree
   (`install-runtime.sh:1217-1266`). It then publishes A's inventory and B's canonical
   `pending-health.v1`, writes the commit marker, removes transaction leftovers, and exits with
   `spawned=no` (`install-runtime.sh:1277-1293`). Successful completion therefore does not itself
   start B, but neither the `/proc` scan nor the updater lock prevents a concurrent app request.
5. The discovery Scriptlet (`tools/kindle_launcher/Scribe.sh`) asks appmgrd to start
   `app://com.markrai.ignite`; it deliberately does not execute `scribe-app`. App registration maps
   that URI to `/mnt/us/scribe-app/bin/scribe-lifecycle-host.sh`
   (`tools/kindle_app/registration/ignite.json`).
6. The installed host wrapper appends a lifecycle log, creates the sync and device directories, and
   execs the stable `scribe-lifecycle --host` as root. These are durable app-state writes by B's
   release-owned host before `go`, although the current wrapper does not open notebook data.
7. The lifecycle host publishes `runresult` and waits for LIPC. On `go`, `dispatch_go` calls
   `start_child_if_configured` (`scribe-lifecycle/src/main.rs:807-833`). The production path takes
   the launch gate shared, prepares running-child and evidence-worker activity tokens, spawns the
   stable `scribe-app`, queues the asynchronous evidence worker, and acknowledges `go` only after
   admission succeeds (`main.rs:434-665`). This gate excludes a promotion decision, not data
   incompatibility.
8. `scribe-app::main` claims the health descriptor and takes `/tmp/scribe-app.lock` before runtime
   setup (`scribe-app/src/main.rs:39-66`). There is currently no compatibility check between that
   singleton acquisition and `runtime::run_page_session`.
9. Runtime initializes display/input and then calls `recover_interrupted_adoption` before opening
   the library (`runtime.rs:3718-3735`). If an adoption workspace or staged library exists, that
   call can delete an uncommitted preparation or publish a committed whole-library replacement
   (`sync_adopt.rs:814-853`). **This is the earliest current call that can semantically replace
   canonical library/sync/device state.**
10. If no adoption recovery is present, `open_or_create_library_shell` is the next mutation point:
    it creates a missing root, removes a completed migration record, migrates a legacy layout, or
    creates a library (`library.rs:942-965`). `OutboundSync::open_with` then creates/locks sync
    state, allocates replica state, establishes an epoch, and may reconcile or recover durable sync
    state (`runtime.rs:3770-3798`, `sync_outbound.rs:1182-1258`). NBK import recovery runs before
    Home (`runtime.rs:3834-3840`).
11. Home is rendered and readiness is acknowledged only on entry to the second event-loop
    iteration (`runtime.rs:3874-3966`). Durable health evidence is produced later by the lifecycle
    worker. Thus readiness occurs after all startup mutations above.
12. Notebook binding is deferred until a notebook is opened, but that later path calls
    `migrate_all_pages`, loads/truncates recoverable journals, and may repair page background/meta
    (`notebook.rs:300-327`). Deferral narrows startup work; it does not establish rollback safety.

There are paths around the normal Scriptlet request. The runtime explicitly supports an
SSH-launched session by starting its own powerd watcher (`runtime.rs:3699-3706`), so an operator can
execute the stable app directly today. A caller can also invoke the lifecycle host or binary
directly. A library restore restarts `run_page_session` inside the same process and singleton
(`scribe-app/src/main.rs:74-83`) without another appmgrd `go`. Reboot or host restart loses only
in-memory coordination; the next app request starts the registered stable host again. The recovery
component installer states that it does not execute the client, and `install-runtime.sh` reports
`spawned=no`, so neither is a normal first-launch source. Whether appmgrd can automatically replay a
pending `go` or restart a previously active app after a real Kindle reboot remains a hardware
validation item.

Items 1–12 above are source-code findings. A read-only first-generation Scribe inspection on
2026-10-27 confirmed firmware 5.18.6, the appmgrd registration values, and the current filesystem
and lock facts recorded in Section 10.2. Appmgrd reboot/replay behavior, USB exclusion,
directory-fsync/power-loss durability, and realistic full-scan time remain unverified; the design
does not treat them as established facts.

The first semantically dangerous mutation is conditional adoption recovery, not Home readiness.
Even earlier, B's installed host can write application state. Therefore a gate only in the current
`go` callback is too late to protect every state path, while a gate only in `scribe-app` is too late
to prevent release-owned host behavior. Positive compatibility must be established before B is
selected, with lifecycle and application checks as defense in depth.

### 1.7 Crash consistency and the absence of a notebook WAL

Ignite's native library has no SQLite database and no WAL. It uses append-only LF-framed journals,
atomic metadata replacement, manifest-first structural commits, quarantine directories, and
explicit fsyncs. For example:

- page and notebook metadata use temp → fsync → rename;
- strokes/edits append complete lines and fsync; a torn final line is recoverable;
- page/notebook delete commits the manifest before moving the unreferenced directory to trash;
- sync inbound uses `apply.json`, durable reducer state, verification, cursor ordering, and
  idempotent replay; and
- library adoption uses a durable plan and idempotent directory swaps.

Those mechanisms protect the formats that implement them. They do not prove that a future format
or migration is understood by A. The sync server has SQLite/WAL internally, and Amazon notebook
import reads Kindle SQLite/NBK files, but neither is the storage engine for Ignite's local library.

### 1.8 Existing backup and restore guarantees

`deployment/kindle/lib/backup-runtime.sh` creates a timestamped tar.gz of only
`/mnt/us/scribe-data`. It is read-only with respect to the library, refuses a destination inside the
library, stages a partial archive, and renames it on success. It does **not**:

- stop or lock `scribe-app`;
- prove that a multi-file snapshot was quiescent;
- include `/mnt/us/scribe-app/state/sync` or device state;
- validate every archived record; or
- implement restore.

It is useful operator recovery material when Ignite has first been closed, but it is not currently
an updater transaction primitive.

The portable backup crates provide a stronger logical server-side backup: canonical sync snapshots
and content-addressed blobs are fully verified, and server restore is transactional. Their declared
scope is current synchronized state only; device credentials, replica state, history, deleted
objects, and edits not synchronized to the source server are excluded. They therefore cannot prove
lossless rollback of the current Kindle on their own.

## 2. Compatibility properties

The promotion decision must keep three properties distinct:

**Forward compatibility** means the exact B reader can validate and safely operate on the complete
current A-era library, active sync epoch, and required settings. It includes preexisting corruption
handling and any automatic mutation B performs before the user edits anything.

**Backward compatibility** means that after any supported B operation—including notebook edits,
inbound sync, import, recovery, metadata repair, and a crash at a documented write boundary—the
exact retained A reader can either open the resulting state without loss or run an A-defined,
non-destructive recovery to a state it can open. It is a writer-to-reader property, not merely an
overlap in nominal schema numbers.

**Recoverability** means that when direct backward compatibility is absent, a durable mechanism can
restore a complete consistent state. A byte snapshot of pre-upgrade data recovers old state but not
B-era edits. Lossless recoverability after B writes additionally requires a reversible migration,
dual-write representation, or a proven conversion/replay path that preserves those edits.

The repository currently enforces strict parsing and crash rules for many individual formats, and
it has explicit page and legacy-layout migrations. Slice 3D-B2a.4.1 adds a deterministic compiled
reader/writer inventory, but that declaration is explicitly incomplete and unauthenticated. The
repository still does not authenticate a pairwise compatibility decision, scan all data with both
exact releases, or retain a pre-B data snapshot. Therefore none of the three properties is
currently established for an arbitrary A→B update.

## 3. Upgrade and rollback threat matrix

| Scenario | Current behavior and risk | Evidence needed | Safe policy |
|---|---|---|---|
| 1. No persistent format change | Existing atomic/journal rules still apply, but equality of paths does not prove equal semantics. | Authenticated A/B contracts, both full read-only probes, stable data identity, and cross-version fixtures. | Admit B only if both read the current state and B's write closure is within A's reader contract. Revalidate before promotion. |
| 2. Backward-compatible extension | Older serde readers do not uniformly deny unknown fields, but relying on ignored fields can silently lose meaning when A rewrites a file. | Field-level writer/read semantics plus a round-trip test proving A preserves or safely ignores the extension. | Admit only when the extension is explicitly lossless for A; numeric version overlap alone is insufficient. |
| 3. Forward-only migration | B may rewrite all page metadata on notebook open. Current v2→v5 migrations have no general reverse transaction. | Authenticated migration edge, precondition scan, crash tests, output identity, and a reverse/lossless recovery proof. | Block B's first launch and promotion under the initial policy. A snapshot permits intervention, not automatic lossless rollback after edits. |
| 4. B writes data A cannot understand | A can refuse a page/notebook/sync epoch or, worse, ignore meaning it later overwrites. | B writer set must be a subset of A's semantically validated reader set; exact A probe after representative B writes. | Block B's first launch, promotion, and rollback to A. Preserve both binaries and data for intervention if B already ran. |
| 5. B crashes during mutation/migration | Current known writes often leave an old file, a complete new file, or a recoverable marker/torn tail. A future migration may not. | Per-migration crash-state grammar and recovery owner; power-cut tests at every durable step; both probes on recovered output. | Recovery must be completed by the release that owns the marker before compatibility can be decided. Unknown markers block. |
| 6. B is promoted, then later fails | Retaining A's executable does not freeze data. Later B writes may invalidate an earlier rollback conclusion. | Release-wide B-writer→A-reader guarantee and a fresh current-state A probe before rollback. | Never auto-rollback from old evidence. Reassess current data under the same writer freeze. |
| 7. Rollback after B creates/edits notebooks | A may not know B record types, sync operations, IDs, or metadata. Restoring an A-era snapshot loses B work. | Exact A probe of current state, including sync queues, or a proven lossless reverse/conversion path. | Direct rollback only when backward compatibility is freshly proven. Otherwise preserve both states and require recovery tooling. |
| 8. Sync modifies data while B runs | Inbound materialization changes library files and coupled sync state; outbound/checkpoint threads change sync state. | App/sync quiescence, sync/adoption locks, stable two-pass root, and contracts covering protocol plus materialized formats. | Any active or uncertain sync writer blocks. The final scan occurs with no app process. |
| 9. Data was partially corrupt before upgrade | Validators often refuse; some open paths repair/truncate. A candidate must not turn baseline corruption into apparent incompatibility or silently repair it during assessment. | Read-only baseline classification, exact damaged paths, and no-write probes. | Block B's first launch and classify as corruption/intervention, not compatible. Preserve bytes; normal explicit recovery must precede reassessment. |
| 10. Reboot/power loss interrupts a sensitive operation | Existing markers cover known inbound/adoption/import operations, but no generic migration journal exists. | Durable prepared/committed states, idempotent recovery, directory durability tests, and post-reboot A/B probes. | An incomplete or unrecognized transaction blocks promotion and rollback. Never delete A or recovery material. |

## 4. Feasible contracts compared

| Approach | Value | Limitation / failure behavior | Recommendation |
|---|---|---|---|
| Manual “compatible” declaration | Low implementation cost. | Not runtime proof; easy to replay or mismatch; cannot detect corrupt or already-migrated device data. | Never sufficient alone. |
| Exact versioned reader/writer capabilities | Makes release behavior inspectable and pairwise evaluation deterministic. | Must describe semantics, auto-writes, sync state, and migrations—not just min/max numbers. | Required foundation. Use exact named format sets/edges. |
| Reader/writer numeric ranges | Compact. | Incorrect if versions are non-monotonic or an older reader drops new fields. | Only derived display information, never authority. |
| Migration manifest/state | Gives crash and reversibility rules for a real migration. | A broad framework adds complexity when no migration exists. | Require one only for each compatibility-changing migration; do not build a generic engine preemptively. |
| Candidate-specific and cross-version tests | Catches real parser/writer regressions before signing. | Test fixtures cannot describe the user's current state and do not authenticate device evidence. | Required release evidence, alongside device probes. |
| Immutable pre-upgrade snapshot | Preserves A-era bytes and supports forensic/manual recovery. | Storage and fsync cost; current backup omits sync state; restoration loses later B edits. | Prerequisite for any forward-only trial, not proof of safe automatic rollback. |
| Reversible migration / dual write | Can preserve rollback after B writes. | High complexity and crash-state surface; must prove old and new views remain equivalent. | Add only when an actual format change needs it. |
| Conservative block | No hidden data risk. | Delays promotion for old releases without capabilities. | Required fallback and recommended initial policy. |

## 5. Recommended minimal contract

### 5.1 Release storage capabilities

`scribe-app --storage-capabilities` now emits one canonical, bounded
`IGNITE_STORAGE_CAPABILITIES_V1` document before launch timing, singleton, device, library, sync,
or pairing initialization. The implemented V1 foundation is documented in
[`kindle-storage-capability-inventory.md`](kindle-storage-capability-inventory.md). It contains
sorted exact format records with reader/writer, unknown-field preservation, automatic mutation,
recovery-owner, and confidence facts, plus mandatory
`coverage=incomplete`, `data_compatibility=unknown`, and `promotion_authorized=false` lines.

The current contract deliberately does **not** include a release version, an invented fixture
digest, or an attestation claim. A later authenticated release binding must associate its exact
canonical bytes with the exact executable hash and real reproducible cross-version test evidence.
The complete release contract eventually needs:

- contract and policy version;
- release version, build identity, and exact executable binding outside or above the current
  compiled declaration;
- exact accepted format identifiers for every library, page, folder, import, identity, sync,
  checkpoint, navigation, preference, and device-state component;
- exact formats and record variants the release may emit;
- every automatic startup/open repair or migration, with input, output, trigger, crash states,
  recovery owner, and whether it is semantically reversible by A;
- whether unknown fields/records are rejected, preserved byte-for-byte, or dropped on rewrite;
- the closure of formats reachable after ordinary authoring, inbound sync, import, and recovery;
  and
- a digest of a real reproducible compatibility fixture suite or release attestation that approved
  the contract, once such an artifact exists.

Capabilities use sorted exact identifiers such as `page-header-v5` and
`confirmed-checkpoint-manifest-v3`; an interval such as “2 through 5” is not proof that every
intermediate format has the same semantics. A durable representation without an in-band version
is named `-unversioned`, not assigned an invented version.

The capability bytes are trusted only when bound to an authenticated official executable. For B,
that binding must descend from the Ed25519-authenticated release and the pending executable hashes.
For A, the updater must retain authenticated installed-release provenance, not merely an inventory
that proves its files have not changed since installation. Predecessors are classified explicitly:

- **Native-contract A** has authenticated executable provenance, a compiled capability contract,
  and an exact-A read-only probe once that probe exists.
- **Cataloged legacy A** matches a signed catalog entry by exact official executable identities,
  has audited capabilities backed by cross-version tests, and is scanned by a separate trusted
  version-neutral reader. It must never claim that the original A executable ran a future probe.
- **Unsupported A** has missing/untrusted provenance, modified or unknown executables, or missing
  supported evidence. Automatic candidate activation is blocked.

This avoids a second release-signing key. If capabilities are distributed outside the exact binary,
their digest must instead be included in authenticated release metadata with a domain-separated
signature. The transport and persistence choice is still an implementation decision; an unsigned
release sidecar is not acceptable.

### 5.2 Exact-binary read-only probes

Capabilities are claims, so the exact A and B binaries must also implement a bounded read-only
`--inspect-storage-compatibility` operation. Under a stable data boundary, each probe must:

- traverse all live manifests and every referenced page, not just Home or the active notebook;
- strictly parse and semantically validate every supported record and cross-file invariant;
- inspect active sync, pairing, preference, import, and migration/recovery state applicable to its
  contract;
- report observed format identifiers and any operation it would perform on normal open;
- refuse incomplete journals/markers instead of repairing them;
- perform no create, truncate, rename, cleanup, migration, identity allocation, reconciliation,
  network request, framebuffer/input access, or sync thread startup; and
- emit bounded canonical output and a semantic state digest.

The boundary holder, not a shell wrapper, should spawn only the already-hashed A and B paths with a
fixed argument vector, empty/minimal environment, bounded stdout/stderr, and a deadline. The parent
continues to hold every lock; lock descriptors are `CLOEXEC` and the child is not asked to preserve
them. Exit, signal, timeout, extra output, or noncanonical output blocks the decision.

This is a narrowly cooperating internal executor, not the rejected arbitrary-program wrapper from
B2a.2.

#### 5.2.1 Implemented library-only inspection (Slice 3D-B2a.4.2a)

The exact `scribe-app` binary now exposes this isolated diagnostic before launch timing, the
application singleton, runtime configuration, devices, sync, recovery, or networking:

```text
scribe-app --inspect-storage-library --data-dir PATH
```

The callable API is
`storage_inspect_library::inspect_library(&Path) -> LibraryInspectionReport`. It inspects one
explicit existing native library. It does not create a missing root. Its fixed report begins with
`IGNITE_STORAGE_LIBRARY_INSPECTION_V1`, states `scope=library-only`, `coverage=partial`,
`writer_exclusion=false`, `atomic_snapshot=false`, `data_compatibility=unknown`, and
`promotion_authorized=false`, and ends with sorted bounded findings. Exit 0 means the implemented
library surface was completely inspected without a non-`VALID` diagnostic. Exit 2 means the scan
completed but detected unsupported, corrupt, incomplete, missing, or unverified state. Exit 1
means invocation or inspection itself was incomplete. None of those results is an authorization.

A clean one-notebook example is intentionally diagnostic rather than compatibility evidence:

```text
IGNITE_STORAGE_LIBRARY_INSPECTION_V1
schema=1
scope=library-only
coverage=partial
writer_exclusion=false
atomic_snapshot=false
external_sync=out-of-scope
device_state=out-of-scope
data_compatibility=unknown
promotion_authorized=false
inspection_action=report-only
completed=true
result=VALID
libraries=1
notebooks=1
pages=1
...
observed_page_formats=v5
...
finding_count=0
```

The implemented validation surface is:

- strict duplicate-key-free JSON for `library.meta`, every referenced `notebook.info` and
  `notebook.meta`, and cross-file notebook/page identities and order;
- page headers v2, v3, v4, and v5, render profiles v1/v2, background byte count/checksum/geometry,
  and a present background identity when it is valid;
- bounded streaming validation of stroke records v1/v2 and edit delete-v1/replace-v2 records,
  including identity uniqueness, coordinates, profile/style compatibility, delete targets,
  replacement targets, fragment collisions, and torn final records;
- strict optional text and link records, including source identity checks when the library-local
  identity exists;
- folders v1, import receipts v1, notebook import-origin v1, and their live-layout/provenance
  relationships; and
- strict library-local sync identity v1 plus detection of the recovery-maintenance marker,
  migration record/staging, temporary files, trash, and live-layout orphans.

Historical page formats are validated in place and reported with the action normal open would
take, such as `normal_open_action=migrate-to-v5`; no migration is run. A missing or invalid
background identity is likewise reported as an unverified repair/seal requirement after the
background bytes themselves have been checked. Torn journals are
`INCOMPLETE_TRANSACTION` and remain byte-for-byte unchanged. Trash and orphan directories are
identified as preserved `UNVERIFIED` recovery material rather than corruption because Ignite has
no complete semantic reader for them. Library-local navigation and page-ledger files are also
explicitly unverified in this partial probe rather than silently accepted.

The implementation deliberately avoids these normal-open functions because their call graphs can
write: `open_or_create_library_shell_with_hooks`, `open_or_create_notebook`,
`migrate_all_pages`, `load_existing_page`, `repair_background_chrome`,
`ensure_sync_identity`, `build_snapshot` (the writing variant), import recovery, and adoption
recovery. It reuses only pure validators and parsers: the library/notebook manifest validators,
render-profile validators, stroke/edit record parsers and semantic helpers, strict
`sync-protocol` content/identity types, and the read-only checksum rules. Files are opened directly
read-only; the probe never opens an append handle.

Resource limits are part of V1: traversal depth 8 below the root, 200,000 tree entries, 10,000
notebooks, 100,000 pages, 2,000,000 content records, 4 MiB ordinary metadata, 256 KiB
`page.meta`, 64 MiB `background.raw`, 2 GiB per journal, 16 MiB per JSONL record, and 16 GiB per
tree-census pass. JSON is limited to depth 64 and 1,000,000 values. Relative diagnostic paths are
limited to 512 ASCII bytes, findings to 512, and canonical output to 512 KiB. Exceeding a limit is
an explicit incomplete inspection, never silent truncation.

Before semantic validation and afterward, the probe enumerates sorted path/type/size/SHA-256
records for the complete root and requires equality. This detects ordinary concurrent changes but
does not make an atomic snapshot: no writer-exclusion boundary exists yet, and an uncooperative
writer could change and restore bytes between observations. On Unix, regular files are opened with
`O_NOFOLLOW`, kernel identity is compared with the preceding metadata, and unexpected hard links
are refused. Every relative component and the supplied root's path chain is checked for symlinks;
special files and noncanonical names fail closed. The same API remains buildable on Windows, where
tests exercise the portable metadata/census behavior; the Unix-specific descriptor guarantees
require native Linux execution.

This slice does not inspect active external sync/outbox/inbox/checkpoint state, device/pairing
state, preferences outside the library, retired sync epochs, or a complete adoption workspace.
Slice 3D-B2a.4.2b adds that separate external-state surface below; neither report alone nor their
composition establishes writer exclusion, an authoritative stable root, release provenance,
pairwise A-writer/B-reader behavior, or rollback readability.

#### 5.2.2 Implemented external sync/device inspection (Slice 3D-B2a.4.2b)

The separately callable API is
`storage_inspect_external::inspect_external(&ExternalInspectionConfig) -> ExternalInspectionReport`.
The exact binary exposes it before launch timing, singleton, runtime, hardware, recovery, and
network initialization:

```text
scribe-app --inspect-storage-external \
  --data-dir LIBRARY --sync-dir SYNC --device-dir DEVICE --state-dir STATE
```

All four paths are mandatory and positional. The command never substitutes live Kindle defaults.
Its report starts with `IGNITE_STORAGE_EXTERNAL_INSPECTION_V1` and fixes
`scope=external-sync-and-device`, `coverage=partial`, `writer_exclusion=false`,
`atomic_snapshot=false`, `data_compatibility=unknown`, `promotion_authorized=false`, and
`inspection_action=report-only`. Findings distinguish `VALID`, `VALID_PENDING`,
`UNSUPPORTED_FORMAT`, `CORRUPT`, `MISSING_REQUIRED`, `INCOMPLETE_TRANSACTION`, `UNVERIFIED`, and
`INSPECTION_ERROR`. Exit 0 means the implemented structural scan completed (`VALID` or ordinary
`VALID_PENDING`); exit 2 means completed with a problem or unverified claim; exit 3 means the
inspection was incomplete. CLI misuse/output failure remains exit 1. No result authorizes launch,
promotion, rollback, repair, or deletion.

The library root is not optional: it must be an existing directory containing a strict,
source-valid `library.meta` v1 manifest. A missing library root or manifest is
`MISSING_REQUIRED`; a non-directory or inaccessible root makes the inspection incomplete. The
sync, device, and state roots may be absent for a real fresh/unpaired library, but become required
field-by-field once a recorded epoch or server binding refers to them. Consequently four missing
arbitrary paths can never produce `VALID`.

The active-epoch scan validates library identity v1, replica identity v1, sync-state v1/v2,
canonical genesis snapshot v1 and digest, config v1 without exposing its token, outbox transaction
ordering, inbound inbox/accepted sequencing, apply markers, legacy base v2, checkpoint manifest
v3/content-addressed pages, binary `SCL1` confirmed log framing and reducer replay, referenced
blobs, recovery intent/completion, verified stamp v3, mutation generation, page-ledger syntax, and
external preferences v1. Device identity and server profile v1 are strictly parsed; profile,
config, and staged-profile credentials remain `UNVERIFIED` because the diagnostic neither contacts
the server nor treats possession of a bearer secret as proof of server identity.

An LF-complete, canonical queued operation is `VALID_PENDING`, not corruption. A trailing outbox
`prep`, inbound apply marker, torn journal suffix, recovery intent/claim, maintenance hold, or
adoption plan remains byte-for-byte present and is reported as an incomplete transaction. No
cursor is advanced, no operation is acknowledged, no journal is compacted/truncated, no blob is
removed, and no recovery is run. The scanner inspects staged adoption sync state and pairing when
a committed plan exists, inventories uncommitted staging, and scans paired retired library/sync
siblings as preserved recovery material. Missing pairs and uninterpretable retired objects are
visible; retired epochs are never called disposable.

Journal lines are read incrementally from the buffered file descriptor. The scanner refuses a
record before appending bytes beyond 16 MiB and caps the input represented by retained parsed
records at 64 MiB; outbox, inbox, and accepted state therefore cannot accumulate an unbounded
parsed vector. The outbox diagnostic feeds each strict record into the same pure transaction-state
core as the production reader and never rereads the file. Empty lines and CRLF are historical
production-reader inputs, so they are preserved and reported `UNVERIFIED` as noncanonical rather
than normalized or silently called clean. An unterminated final record remains an incomplete torn
tail. Duplicate keys and malformed JSON are corrupt; unknown fields/variants and future record
versions are unsupported.

Large structured inputs do not materialize raw bytes, a generic `serde_json::Value`, a typed value,
and canonical output simultaneously. Genesis, legacy base, and checkpoint pages use separate
streaming strict-validation, typed-deserialization, and canonical-comparison passes; JSONL retains
only the current bounded line alongside its typed record. Checkpoint reconstruction still retains
the resulting typed `LibraryState`, and journal transaction validation retains its bounded semantic
state. These source-byte limits are therefore allocation controls, not a claim that peak RSS equals
the serialized byte count.

All blob declarations share one constraint set. Repeating the same digest and byte length is
idempotent; declaring different lengths for one digest is `CORRUPT` and does not allow a later
reference to replace the earlier expectation. This applies across genesis, outbound, inbound,
accepted, checkpoint, confirmed-log, and recovery evidence. Retired and adoption diagnostics carry
their actual namespace, for example
`retired/<id>/library/.scribe-sync/identity.json`, rather than reusing the active `library/`
prefix.

The implementation deliberately avoids `OutboundSync::open_with`, `establish_epoch`,
`load_base_store`, `ConfirmedLog::open`, `finish_apply`, `ensure_sync_identity`,
`ensure_replica_state`, `client_id`, `recover_interrupted_adoption`, `adopt_library`, recovery
prepare/reconcile/release functions, `load_side_button`'s lossy fallback, and all server-link HTTP
or worker paths. It reuses the source-owned identity/snapshot/envelope/reducer validators, the
outbox transaction parser, a factored read-only confirmed-log scanner, checkpoint DTOs, recovery
DTOs, page-ledger record validator, and 4.2a's strict duplicate-key JSON and safe file-open/hash
primitives.

V1 limits are: depth 8; 200,000 aggregate census entries and 200,000 entries per directory before a
name vector can grow further; 16 GiB per census pass; 200,000 journal or diagnostic confirmed-log
records; 2 GiB per journal/log; 16 MiB per JSONL, confirmed-log payload, or checkpoint-page record;
64 MiB of input represented by retained parsed journal records; 64 MiB cumulative confirmed replay
payload for this diagnostic; 128 MiB aggregate source bytes for one reconstructed checkpoint state,
genesis, or legacy base; 128 MiB per blob; 4 MiB ordinary metadata; JSON depth 64 and one million
values; 4,096 keys per object; 512 findings; 512-byte safe relative paths; and a 512 KiB report. The
object-key cap bounds the duplicate-detection set for malformed input without limiting array members
used by actual notebook/page collections. Journals and confirmed logs are streamed. The diagnostic
confirmed-log reader does not retain the production compaction index. Blobs are hashed from
read-only file descriptors. Checkpoint pages are read one content-addressed object at a time while
the reconstructed typed state remains resident.

Every directory iterator result is checked. An unreadable entry, unsafe name, or count overflow is
`INSPECTION_ERROR`, makes `completed=false`, and returns exit 3; no partial enumeration is treated as
coverage. Active device/state, sync-root, blobs, checkpoint artifacts/pages, adoption, and retired
namespaces all follow this rule. Retired sibling names are collected once for each semantic pass
rather than rescanned inside the pairing loop. The before/after censuses remain intentionally
separate so concurrent mutation can be detected.

The Linux-only stress fixture
`cargo test -p scribe-app storage_inspect_external::tests::linux_large_structured_input_streaming_stress_fixture -- --exact`
constructs a 24 MiB structured file without a same-sized fixture buffer and exercises the streaming
strict/typed/canonical passes. Run it under `/usr/bin/time -v` on the target-class Linux runner to
record peak RSS. Windows tests cover the same parser and reduced-limit behavior, but only native
Linux and Kindle measurements can establish an acceptable memory margin on the 5.18.6 device.

Before and after semantic work the scanner hashes the explicit library, sync, device, and state
roots plus the precise `.adopt`, `.adopt.lock`, and `.retired-*` sibling namespaces used by
`sync_adopt`. Equality detects ordinary concurrent changes. It remains neither a lock nor an
atomic snapshot and cannot detect a writer that changes and restores bytes between observations.
On Unix it inherits 4.2a's `O_NOFOLLOW`, descriptor identity, single-link, path-chain, and special
file refusals. Directory-component race closure and `/mnt/us` behavior still require a future
writer-exclusion boundary and Kindle validation.

Remaining partial areas are deliberate: server-side acceptance/authentication cannot be proven
offline; local verified stamps and mutation generation are cache/invalidation diagnostics rather
than state authority; retired library semantics are not fully replayed; and no historical exact-A
fixtures were available for A-writer/B-reader and B-writer/A-reader testing. Same-release fixtures
exercise parser and preservation behavior only. Therefore even two structurally clean reports
leave data compatibility `unknown`.

### 5.3 Stable device-data identity

The assessor should compute a canonical `IGNITE_DATA_STATE_V1` root at this manual decision point,
not on every launch. It should cover fixed logical roots:

1. `library` = `/mnt/us/scribe-data`, including live data, sync binding, import receipts, trash, and
   orphans;
2. `active_sync` = `/mnt/us/scribe-app/state/sync`;
3. `device_state` = compatibility-relevant files under `/mnt/us/scribe-app/state/device`; and
4. the names and identities of preserved retired sync epochs, at minimum proving they remain
   unchanged and are not deleted by promotion.

For each root, enumerate sorted relative byte paths and file type, length, and SHA-256. Hash all
regular-file bytes, including backgrounds and journals. Reject symlinks, path escapes, duplicate or
noncanonical names, special files, unexpected hard links, count/size overflow, unreadable files, and
unknown transaction markers. Fixed lock files may be identified but their bytes carry no authority.
Known disposable temp files should either be absent or cause an explicit incomplete-transaction
result; they must not be silently omitted.

Compute the root before and after both probes and require equality. The existing local mutation
generation and page ledger are useful diagnostics and future optimization hints, but they are not
complete authority. `note_library_mutation_scoped` calls `invalidate_durable` only for the first
successful cooperating admission associated with a library's in-process session cell. That call
publishes a saturating increment and clears the verified stamp; later admitted writes using the same
cell do not increment again, although they remain fenced and may add page-ledger scopes.
`clear_registry_for` removes the cell during sync teardown and adoption, so another session in the
same process can increment again.

Current library/notebook/page/folder writers, local-identity writes, ordinary import, library/page
migration, sync materialization, and torn-journal recovery use those admission paths. This is not
universal write coverage: active sync and device/pairing state live outside the library counter;
adoption publishes directory swaps; some import-receipt recovery/carry writes are not independently
admitted; and older, manual, external, or otherwise uncooperating writers may not record anything.
The verified-startup census can detect some missed library changes, but hashes some large files by
size rather than content and does not cover the complete device state. Neither the counter nor the
page ledger can establish that the proposed device-data root remained unchanged.

A complete byte hash is justified here because this is a rare manual authorization operation that
binds a specific data snapshot. It is not required on ordinary startup. If hardware measurements
later show it is too slow, a content-addressed tree cache may optimize repeat scans only when every
writer updates it transactionally; cache uncertainty falls back to a full scan or blocks.

### 5.4 Canonical compatibility result

The internal assessor should construct canonical LF-terminated bytes with no optional spellings,
duplicate fields, CR, NUL, or trailing data. The following is the proposed logical content; exact
field ordering becomes normative only when implemented:

```text
IGNITE_DATA_COMPATIBILITY_EVIDENCE_V1
schema=1
policy=bidirectional-no-data-loss-v1
pending_txn=<canonical pending transaction>
pending_sha256=<64 lowercase hex>
candidate_version=<canonical version>
candidate_app_sha256=<64 lowercase hex>
candidate_lifecycle_sha256=<64 lowercase hex>
candidate_host_sha256=<64 lowercase hex>
retained_inventory_sha256=<64 lowercase hex>
retained_app_sha256=<64 lowercase hex>
retained_lifecycle_sha256=<64 lowercase hex>
retained_host_sha256=<64 lowercase hex>
a_capabilities_sha256=<64 lowercase hex>
b_capabilities_sha256=<64 lowercase hex>
data_state_sha256=<64 lowercase hex>
a_probe_sha256=<64 lowercase hex>
b_probe_sha256=<64 lowercase hex>
forward_compatibility=compatible
backward_compatibility=compatible
recoverability=not_required
result=compatible
```

The complete binding must also include the launch owner/evidence digests already used by B2a.3 and
the authenticated installed-release provenance for A and B. Those existing fields can remain in
the eventual authorization challenge rather than being duplicated in a standalone compatibility
record.

The device has no signing private key. A locally persisted result therefore cannot authenticate
itself. If a result is cached under `state/update`, use the existing no-symlink, bounded,
temp→fsync→rename→directory-fsync pattern, but treat it only as a cache. The authoritative decision
reruns the probes and state-root comparison inside the exclusive boundary and binds the resulting
digest into the manual authorization challenge. A copied, forged, stale, mismatched, or partially
written cache can never authorize promotion.

### 5.5 Validity after B writes

There are two different lifetimes:

- The **release-pair contract** may remain true after B writes only if authenticated B code can emit
  no format or migration result outside A's semantic reader/recovery set.
- The **device scan** describes one exact point in time and becomes stale after any write.

Therefore promotion authorization must scan the final current state. Any later rollback must repeat
the A probe on the then-current state. If B has a forward-only writer or migration, compatibility
evidence expires at its first possible write; a previous positive scan cannot be reused.

## 6. Pre-first-launch compatibility gate

### 6.1 Safety invariant and feasibility result

The required invariant is:

> Until a positive pre-activation decision is durable, no B-owned normal execution may create,
> repair, migrate, reconcile, recover, or otherwise mutate canonical library, sync, pairing,
> preference, import, or recovery-significant data. After exact B is activated, its pre-`go` broker
> remains read-only for those roots and `scribe-app` cannot enter runtime until it receives a
> one-launch admission capability for the authenticated B/A pair whose current device state passed
> the bidirectional policy under writer exclusion.

“B-owned” includes the release's host wrapper and lifecycle binary, not just `scribe-app`. “Current
device state” includes the library, active sync epoch, device state, retired recovery-significant
epochs, and incomplete transaction markers. A successful launch-health acknowledgment is not an
input to this preflight because it cannot exist safely until after admission.

The current architecture does **not** enforce this invariant. The updater lock does not exclude a
new `go`, the installer acquires that lock before it could acquire the launch gate, B is renamed and
registered before `pending-health.v1` is published, and neither lifecycle nor application startup
checks compatibility. B2a.3 is deliberately post-launch and requires a completed current-host
evidence worker. Reusing it unchanged would be circular.

The guarantee is feasible with a bounded extension, but not with documentation, a process scan, or
a marker alone. The smallest prerequisite is an internal **candidate admission transaction** that
reuses the existing launch/updater locks, adds the planned data-writer boundary, and owns a
kernel-backed handoff to normal application startup. It must have a pre-launch mode that does not
require prior B health evidence.

### 6.2 Authoritative owner and defense-in-depth enforcement

The authoritative decision belongs at installer activation, while A is still the selected live
release and B exists only in authenticated staging:

1. derive the exact future pending transaction, retained inventory, and B executable identities;
2. exclude launches and every cooperating data writer;
3. run the complete preflight against stable data;
4. if and only if it is bidirectionally compatible, durably bind that decision to the exact
   prepared transaction; and
5. while the same boundary is held, activate/register B, publish exactly the bound pending bytes,
   and commit the installer transaction.

If preflight is unknown or fails, the installer aborts before the first stable B rename and A
remains the registered executable. This is materially safer than committing a B that the user can
no longer open.

Two enforcement points are still required:

- **Lifecycle admission.** The registered host must refuse `go` unless it can validate the exact
  committed candidate, pending digest, retained A, compatibility binding, and absence of an
  incomplete activation. It must not acknowledge a refused `go`. Host restart and reboot rebuild
  this decision from durable state; no in-memory “already checked” bit is authority.
- **Application self-enforcement.** Normal `scribe-app` startup must require a one-launch kernel
  capability from that lifecycle admission, and consume/validate it after taking the singleton but
  before `default_config` or `run_page_session`. A private Unix socket/descriptor exchange bound to
  a fresh launch UUID is suitable. An environment variable, CLI option, inherited descriptor
  number, or mutable file by itself is not. Direct normal execution over SSH must therefore be
  refused or routed through a lifecycle-owned manual-launch interface. Side-effect-free capability,
  probe, and help operations can remain directly executable because they never open canonical data.

The application check closes mixed-tree and direct-execution cases: B `scribe-app` paired with an
old lifecycle cannot receive the new capability; a copied B binary cannot gain access by naming a
file or setting an environment variable; and a host that dies before completing the handshake
cannot leave an admitted child. The lifecycle check avoids an unnecessary child spawn and protects
the normal appmgrd route. The installer check remains authoritative because the current
release-owned host performs app-state writes before `go`.

The current host wrapper's `mkdir -p state/sync state/device` must move into the already-approved
activation transaction or be proven to create only absent, empty directories before they enter the
compatibility roots. The safer initial rule is that the registered broker performs no covered-state
write before admission; lifecycle logging may remain outside the compatibility roots. Future host
behavior is part of B's authenticated capability contract.

The existing in-process library-restore restart may reuse the already validated candidate and
singleton, but its re-entry into `run_page_session` remains within B's declared automatic recovery
and writer closure. A new process always needs a new lifecycle capability.

### 6.3 Durable blocked and approved state

The initial state is fail-closed: a candidate transaction with absent, malformed, mismatched, or
unreadable compatibility state is **blocked before activation**. If a partial legacy activation has
already selected B, lifecycle and application enforcement refuse normal B startup. This state must
not be encoded by changing `pending-health.v1`; that record remains the installation/health source
of truth.

A future, separate canonical compatibility-admission record may bridge install time and user launch.
Its exact format is out of scope here, but its logical binding must include:

- policy/schema and a state such as prepared or first-launch-admitted;
- installer transaction plus the SHA-256 of the exact pending bytes that will be or were published;
- B version and exact `scribe-app`, `scribe-lifecycle`, and host-wrapper identities;
- A authenticated provenance, retained inventory identity, and exact executable identities;
- both authenticated capability digests and release-test attestation identities;
- the stable data-state root and exact A/B probe-result digests; and
- the identity/version of the trusted checker that produced the result.

It must be published with no-symlink, bounded, temp-file, file-fsync, rename, and directory-fsync
rules and protected from ordinary USB/user writes. Device inspection rejects
`/mnt/us/scribe-app/state/update` as the authority location: `/mnt/us` is `fuse.fsp` over a vfat
backing store with modes exposed as `0777`. `/var/local/root` is on writable ext3, owned by root:root,
and not writable by framework; a dedicated future `/var/local/root/ignite-update` mode-0700
directory is the current candidate authority location. Its reboot, firmware-update, factory-reset,
and power-loss semantics still require destructive hardware qualification. An optional USB-visible
copy may be diagnostic only.

The record is local coordination evidence, not a new publisher signature: the Kindle has no
release signing private key. Consequently, it never authorizes by itself. The trusted broker
revalidates its canonical bytes, all authenticated immutable inputs, and current state under kernel
locks before issuing a launch capability. A hash of an unsigned marker, a user confirmation, or
file ownership alone is not positive compatibility evidence. Missing root-owned state after reset
fails closed.

Candidate replacement changes the transaction/pending digest or an executable identity and makes
the binding unusable. Installer recovery removes or invalidates a prepared record only after it has
restored A; it never “repairs” a mismatch into approval.

For the conservative first policy, compatibility is checked on **every normal launch while B is
pending**. The initial data root must still equal the pre-activation root before first admission.
After B writes, root equality with that old snapshot is neither expected nor useful, so later
admission reruns the exact A/B read-only probes and current-state root under writer exclusion. This
also catches a crash that left an incomplete or A-unreadable state. A release whose crash recovery
requires letting B make a change that A cannot understand does not satisfy the initial policy; it
needs a separately designed recovery transaction. Caching may improve diagnostics but cannot skip
the authoritative revalidation. Promotion later performs its own full B2a.3 revalidation.

App registration currently gives `go` a 60-second maximum. If a complete root and both probes
cannot finish within that real appmgrd deadline, the broker must not acknowledge early or admit B
from cached evidence. A separately validated longer appmgrd contract or a precomputed tree whose
updates are transactionally maintained by every writer would be required. This is a performance
feasibility gate, not grounds to weaken revalidation.

### 6.4 Positive preflight requirements

All of these checks must pass before the first stable B rename:

| Check | Positive requirement |
|---|---|
| Transaction | Canonical prepared transaction; exact future `pending-health.v1` bytes/digest; no existing pending candidate or inconsistent activation marker. |
| A identity | Exact live A equals the retained 18-entry inventory and authenticated installed provenance, or an explicitly supported exact-hash legacy catalog entry. Integrity alone is not publisher identity. |
| B identity | Installer and release manifest signature valid under the provisioned production pin; staged app/lifecycle/host bytes exactly match the authenticated artifact. |
| Capabilities | Strict authenticated contracts for exact A and B; every reader, writer, startup repair, migration, sync materialization, import, and crash state is named. |
| Current readability | Exact A and B read-only probes both validate the complete current library, sync/device state, and relevant retired epochs without writes. |
| Future rollback readability | B's complete durable writer/automatic-migration/crash-state closure is a subset of A's semantic reader/recovery closure, supported by signed release claims and cross-version tests. |
| Transactions | No incomplete adoption, inbound apply, import, page/library migration, installer, or unknown recovery transaction. Read-only preflight does not silently finish one. |
| Stable state | Launches, child/workers, app singleton, adoption, active sync, and other cooperating writers are excluded; equal before/after full roots surround both probes. |
| External state | USB export or other supported external writer is proven absent, or the operation blocks. Arbitrary root compromise remains outside the guarantee. |
| Policy | Result is `BIDIRECTIONAL_COMPATIBLE`. Forward-only, snapshot-required, unknown, corrupt, or inconsistent results do not activate B. |

Today the repository can authenticate future B metadata once the production pin is provisioned,
hash staged/live executables, validate pending/inventory formats, and acquire the launch gate,
updater lock, activity tokens, and evidence lock. It cannot yet produce a positive row for A
provenance, release capabilities, exact no-write A/B probes, the complete data root, data-writer
exclusion, USB exclusion, or the application admission capability. Source audit is evidence for
designing those mechanisms, not a substitute for them.

### 6.5 Atomicity and lock order

Preflight through activation requires one continuous exclusive boundary. Publishing an approval,
releasing locks, and later invoking today's installer would let A, sync, or another `go` change the
data that was assessed. Activating B first and assessing later recreates the original defect.

The extended global order is:

1. lifecycle launch gate, exclusive;
2. updater transaction lock;
3. lifecycle running-child exclusion and bounded evidence-worker quiescence;
4. application singleton, exclusive;
5. adoption lock, then active-sync lock, in the application's established order;
6. launch-health evidence lock if the operation also reads health evidence; and
7. stable-root scans and exact probes while all applicable guards remain held.

The pre-first-launch path does not require health evidence, but it must not acquire an earlier lock
after a later one. Ordinary app execution already takes the singleton before adoption/sync work.
Normal evidence workers take their activity token before the evidence lock. The final implemented
order must be checked against every adoption/sync call path, and contention or uncertainty has a
bounded fail-closed result.

Today's shell installer enters through the updater helper and therefore owns item 2 without item 1;
it cannot safely acquire the launch gate afterward. A future activation must enter through a
narrow, trusted internal coordinator that obtains item 1 first and retains the guards while the
exact installer operation runs. It must not revive the rejected arbitrary-program wrapper. Either
the installer transaction becomes an internal cooperating operation or a guardian supervises the
fixed, verified installer child, retains the kernel guards itself, and guarantees that the child
cannot survive guardian death. Merely passing descriptor numbers to an arbitrary shell command is
not a decision boundary.

The continuous boundary ends only after the compatibility binding, B files/registration,
retained-A inventory, exact pending record, and activation commit are durable. Power loss before
the commit follows the existing journal recovery to A; the compatibility binding is transaction-
specific and cannot authorize a mixed tree. Power loss after commit leaves B pending plus the
matching binding.

Install-to-user-launch can be bridged rather than holding locks indefinitely, provided the first
launch re-establishes the launch/data-writer boundary, validates the durable exact binding, confirms
the current root still equals the approved pre-B root, and hands a kernel admission capability to
the exact B child without a writer gap. The lifecycle parent must retain the boundary until the
child has adopted the same kernel-locked singleton open-file description and consumed the capability
before entering runtime. The parent keeps a duplicate through acknowledgment, eliminating an unlock
window; the child does not “prove” ownership with an environment variable or descriptor number.
Parent death before handoff, timeout, missing acknowledgment, or descriptor/socket setup failure
terminates or refuses the child before canonical data access; it must not fall back to an unobserved
launch.

The existing B2a.3 boundary is insufficient on its own: it requires post-launch evidence, does not
take the app singleton or data locks, and does not supervise the transition into runtime. The
candidate-admission transaction is the narrowly scoped additional primitive.

### 6.6 Failure and recovery policy

| Condition | Required behavior |
|---|---|
| Compatibility is unknown, or a capability/probe/provenance input is missing | Do not activate B. Leave A registered and data untouched. No confirmation or `--force` conversion exists. |
| A cannot inspect current data, current data is corrupt, or an incomplete transaction exists | Block and report the exact class/path. A normal, separately chosen A recovery may be run before retry; preflight performs no repair. |
| B declares a forward-only migration or A cannot read a possible B output | Block B's first launch under the initial policy. A pre-B copy is useful recovery material but is not lossless rollback for later B edits. |
| Power loss during read-only preflight | Kernel locks release; no approval is published. A remains selected. A temporary result is non-authoritative. |
| Power loss during approved activation | Existing installer journal restores A before commit or retains committed B afterward. A transaction-mismatched compatibility record is ignored. B self-enforcement refuses a mixed/incomplete tree. |
| Preflight succeeds but B or pending bytes change | Exact hashes/digest no longer match; activation or launch is refused. The old result is never rebound to new bytes. |
| Reboot/host restart before first B launch | Durable state is re-read and fully validated; current-host memory is not trusted. Missing host/gate state blocks B. |
| B's first launch fails | Pending and retained A stay unchanged; no promotion follows. A later B attempt requires a fresh current-state admission. A failed launch is not compatibility revocation by itself, but damaged/incomplete state blocks. |
| B already modified data under an older updater | Classify the current state with exact probes. If A readability is not proven, report `INCOMPATIBLE_DATA_ALREADY_WRITTEN`; preserve A, B, data, and backups, and do not auto-rollback. |
| The compatibility gate is unavailable | Fail closed for B. Before activation A remains normally usable. After an old/partial committed activation, preserve A and expose diagnostics/backup; restoring or running A needs a future verified recovery operation, not an ad-hoc bypass. |

First installation has no retained A rollback claim. An empty/new data root may be initialized by B
only after B authenticity and its own capability are valid. A first install over preexisting Ignite
data still requires a read-only B probe and explicit classification; absence of retained A must not
be misrepresented as rollback safety.

Legitimate A/recovery execution is not granted by weakening B's gate. If preflight fails before
activation, the already registered A continues normally. Exact retained-A probes run only through
the read-only internal executor. A future executable rollback or temporary A session must first
prove A can read the then-current data and must use the updater transaction boundary; neither is
implemented here. The installed recovery Dropbear component is not an automatic rollback path and
must not be started merely to bypass compatibility.

### 6.7 Legacy A bootstrap

| Strategy | Feasibility and trust | Cost / user effect | Conclusion |
|---|---|---|---|
| A. Native-contract A via a transitional release | Establishes authenticated provenance, a compiled contract, and an exact executable probe for later upgrades. By itself it says nothing trustworthy about the older A it replaces, so T's first launch can still be unsafe. | One manual upgrade and a release held back from auto-promotion. Lowest long-term maintenance. | Required foundation, insufficient alone for its own bootstrap. |
| B. Cataloged legacy A | A signed catalog can bind a finite official A executable/host set to audited reader behavior and cross-version tests. A separate trusted version-neutral scanner validates current data; the original A is never represented as having run a future probe. | Per-release audit/test burden; cannot cover custom/repacked binaries; catalog errors are security-sensitive. | Suitable only for a small supported legacy set and only with exact hashes. |
| C. Unsupported A / conservative manual path | Makes no invented claim for missing provenance, modified or unknown executables, or incomplete evidence. | Some users need a documented USB/off-device backup and manual transition; automatic upgrade is unavailable. Manual acknowledgment still cannot create rollback proof. | Mandatory fallback for every noncataloged or mismatched A. |

The recommended bootstrap is **A plus a narrow B, with C as the fallback**:

1. publish a transitional release T that implements capabilities, no-write probes, authenticated
   installed provenance, pre-first-launch enforcement, and the post-launch assessment, but does not
   enable promotion;
2. include in T's authenticated release inputs a small exact-hash catalog for explicitly supported
   official legacy A releases, backed by retained source, A-reader fixtures, T-writer→A-reader tests,
   and crash-state tests;
3. require the current live/retained A files to match one catalog entry exactly and require the
   device scan to remain within that entry's proven reader set;
4. require T to emit no durable format outside that legacy A reader closure; otherwise block T's
   first launch; and
5. after T is installed and admitted, preserve its authenticated provenance so subsequent T→B
   upgrades use the native protocol rather than the catalog.

Missing provenance is never inferred from a version string. Custom builds, modified files,
uncataloged releases, and any device state outside the cataloged grammar take the conservative
manual path. The production release key pin must be provisioned before a real catalog or T
provenance can be trusted.

### 6.8 Complete lifecycle after this correction

1. **Pre-activation/pre-first-launch:** authenticate A/B, freeze data writers, validate current data
   and the complete B-writer→A-reader closure, then atomically approve and activate exact B. Recheck
   and issue a kernel capability before B enters runtime.
2. **Candidate observation:** B runs normally and collects the existing candidate-specific launch
   evidence. Readiness and 60 seconds of monotonic observation remain runtime evidence only.
3. **Pre-promotion:** B2a.3 reacquires its exclusive decision boundary, adds the data-writer layer,
   revalidates pending B, retained A, host generation, health evidence, current data, capabilities,
   and compatibility, and still grants no authorization by itself.
4. **Authorization/promotion:** a later explicit manual authorization binds that fresh snapshot. A
   separate crash-safe transaction may promote B but does not retire A automatically.
5. **Future rollback:** freeze writers and run a fresh exact A probe on the then-current state.
   Neither pre-first-launch approval nor promotion-time evidence alone authorizes rollback.

## 7. Promotion and rollback policy

The assessor should produce one of these explicit outcomes:

| Outcome | Meaning | Promotion / rollback consequence |
|---|---|---|
| `BIDIRECTIONAL_COMPATIBLE` | Authenticated contracts, stable state, and both exact probes prove B can read current data and A can read every state B may write. | May activate/admit exact B. After observation, a fresh result may proceed to the separate authorization gate. No authorization is implied. Re-probe before rollback. |
| `FORWARD_COMPATIBLE_SNAPSHOT_REQUIRED` | B can read current data; A cannot read all B outputs; a complete pre-B snapshot and recovery design exist. | Initial conservative policy blocks B's first launch and promotion. A future policy may permit a clearly labeled trial, but never silent automatic rollback or A retirement. |
| `FORWARD_ONLY_NO_LOSSLESS_ROLLBACK` | B can proceed, but no mechanism preserves B-era edits for A. | B's first launch, promotion, and automatic rollback are blocked; retain A and all data for intervention. |
| `COMPATIBILITY_UNKNOWN` | Capability, provenance, format, scan, race, or test evidence is missing. | B's first launch is blocked. This is the current system state. |
| `INCONSISTENT_OR_CORRUPT` | Parsing, transaction state, hashes, or the two data roots disagree. | Blocked as corruption/concurrent change. Do not repair during assessment. |
| `INCOMPATIBLE_DATA_ALREADY_WRITTEN` | Current data is outside A's contract. | Executable rollback blocked. Preserve B, A, current data, and any pre-B snapshot; require explicit recovery tooling. |

On a device where an older updater already allowed B to run, a user may independently choose to
keep using it despite unknown or irreversible compatibility, but acknowledgment is an acceptance of
risk, not technical evidence. The new gate has no confirmation bypass: acknowledgment cannot turn
an `UNKNOWN` or forward-only result into `compatible`, mint an authorization challenge, admit a new
B, or enable automatic rollback.

## 8. Coordination with B2a.3

The current B2a.3 order is launch gate → updater transaction lock → child/worker quiescence →
evidence lock. Both pre-launch and post-launch compatibility operations need the data-writer layer
defined in Section 6.5 while retaining that prefix. The post-launch assessment order is:

1. lifecycle launch gate, exclusive;
2. updater transaction lock;
3. lifecycle child and evidence-worker quiescence;
4. app singleton lock, exclusive, to exclude supervised **and manually launched** `scribe-app`;
5. adoption and active-sync locks in the same order used by application startup, with no pending
   adoption/apply/recovery transaction;
6. evidence lock, exclusive (preserving the existing relative order); and
7. two data roots with A/B probes while all guards remain held.

No ordinary writer may acquire these locks in the opposite order. Because the app singleton is
acquired before any library/sync work in `main.rs`, holding it is the simplest umbrella for normal
writers; the lower-level locks additionally prove that a non-session recovery/adoption operation is
not active. The exact adoption/sync order and all error paths still require implementation audit and
Linux/Kindle validation.

`--assess-promotion-with-boundary` should continue to run both complete B1 passes and verify the
host generation. Compatibility becomes another check within the same private guard lifetime. A
prior report, cached compatibility record, old launch-health record, or shared-lock B1 result is
never accepted in place of this revalidation.

The stable compatibility result is then bound to:

- pending transaction and pending-record digest;
- B version and live app/lifecycle/host identities;
- A inventory and app/lifecycle/host identities;
- current launch owner and completed health evidence;
- A/B capability and probe digests; and
- current device-data root.

Only the eventual explicit authorization challenge may combine these inputs. B2b promotion must
revalidate the same binding before its prepared journal. Promotion does not delete A. Retirement of
A requires a separate policy and cannot occur while rollback/data recovery obligations remain.

### 8.1 Residual writer boundary

The proposed locks coordinate Ignite. They cannot exclude a malicious root process and may not
exclude USB-host writes or ad-hoc shell modification. At minimum the two full roots detect changes
during assessment. Kindle hardware validation must determine `/mnt/us` behavior during USB export
and whether the decision command can reliably refuse while external access is possible. If not, a
device/USB-state gate is a prerequisite. The threat model should state explicitly that arbitrary
root compromise is outside the updater's guarantee; accidental independent modification still
fails closed through stable-root comparison.

### 8.2 Implemented writer-exclusion diagnostic (Slice 3D-B2a.4.4)

`scribe-app --inspect-writer-exclusion --data-dir LIBRARY --sync-dir SYNC` is dispatched before
normal initialization. It opens only pre-existing lock files read-only, takes their kernel locks in
the extended hierarchy, and reports missing, contended, unsafe, unsupported, or cancelled checks.
It never creates a coordination resource or starts recovery. Transaction markers are sampled only
while every known lock is held.

The source audit and complete writer/lock matrices are in
[`kindle-data-writer-exclusion.md`](kindle-data-writer-exclusion.md). Recovery has no complete
cross-process boundary, lifecycle-host writes precede `go`, and USB/manual writers do not
cooperate. Therefore the command can report only `writer_exclusion=partial` or `unverified`; it has
no successful authorization exit and can never report `established`. Its fixed safety fields retain
`atomic_snapshot=false`, `data_compatibility=unknown`, `promotion_authorized=false`, and
`inspection_action=report-only`.

### 8.3 Implemented stable-state preflight (Slice 3D-B2a.4.5)

`scribe-app --inspect-compatibility-preflight` now collects a bounded canonical inventory of the
explicit library, sync, device, application-state, activation, adoption, and retired-epoch roots.
It runs the existing library and external-state inspectors between inventory R1 and R2 and reports
equal, changed, or unknown observational stability. The exact manifest, bounds, coverage mapping,
and report schema are documented in
[`kindle-stable-state-preflight.md`](kindle-stable-state-preflight.md).

This closes the reproducible byte-inventory foundation, not Section 6.4. Equal roots are not an
atomic snapshot, B2a.4.4 still cannot establish complete writer/USB exclusion, B2a.4.3 still reports
unverified installed-slot binding, and exact authenticated A/B reader plus future-writer evidence is
unavailable. The production preflight therefore has no `BIDIRECTIONAL_COMPATIBLE` state, never exits
zero, and always keeps `data_compatibility=unknown` and `promotion_authorized=false`.

## 9. Revised implementation slices

1. **3D-B2a.4.1 — Capability inventory (implemented, not authoritative).** Canonical
   `StorageCapabilitiesV1`, the source audit, and the no-initialization `scribe-app` command now
   exist. The host/lifecycle pre-`go` writes and unresolved format behavior are inventoried in
   `kindle-storage-capability-inventory.md`. The output remains incomplete and unauthenticated;
   missing or unverified capability remains unknown.
2. **3D-B2a.4.2 — Read-only probes (4.2a and 4.2b implemented, still partial).** The exact binary
   now has separate strict bounded native-library and external sync/device scanners that never
   invoke migrating/repairing opens. Exact historical A/B writer→reader fixtures, stable writer
   exclusion, authenticated probe provenance, a composed canonical state root, and admission
   integration remain separate work before this becomes a full compatibility probe.
3. **3D-B2a.4.3 — Authenticated installed provenance and bootstrap (feasibility blocked).** The
   repository audit in
   [`kindle-installed-release-provenance-feasibility.md`](kindle-installed-release-provenance-feasibility.md)
   found that the production pin is still `UNPROVISIONED`, the signed v1 release manifest binds only
   the whole installer, successful installation deletes that artifact and preserves no signed
   component/capability closure, and no exact historical artifacts exist locally for a legacy
   catalog. A strict read-only signed-record verifier and fail-closed diagnostic were added with
   temporary-key tests, but production always selects the unprovisioned repository pin and therefore
   reports `AUTHORITY_UNAVAILABLE`. Even with a provisioned key, the diagnostic authenticates only
   the supplied component set and reports `installation_binding=unverified`; it does not bind those
   paths to lifecycle A/B ownership or the pending transaction. A future offline-authoring/
   release-metadata and persistence slice must close those links before installed A/B provenance can
   be used.
4. **3D-B2a.4.4 — Data-writer boundary diagnostic.** **Implemented as a conservative diagnostic.**
   It audits singleton/sync/adoption and related locks, uses a proven inspection order with bounded
   contention, reports USB/recovery/noncooperating gaps, and defines the narrowly cooperating A/B
   probe-executor boundary. Complete exclusion remains infeasible without separately reviewed
   writer and lifecycle integration; launch behavior remains unchanged.
5. **3D-B2a.4.5 — Stable root and preflight engine.** **Implemented as a non-authorizing
   observational diagnostic.** The bounded fixed-root inventory surrounds both existing probes and
   detects changed or incomplete state. Because the remaining Section 6.4 prerequisites cannot
   pass, no production-reachable `BIDIRECTIONAL_COMPATIBLE` result exists.
6. **3D-B2a.4.6 — Pre-activation and first-launch admission.** Add the restricted activation
   coordinator that acquires launch gate before updater lock, transaction-bound durable admission
   state, lifecycle validation, and the lifecycle→app kernel handshake. Make normal B startup refuse
   before `run_page_session` without it. Test transitional/legacy behavior before enabling use in an
   installer.
7. **3D-B2a.4.7 — Post-launch assessment integration.** Add current-state compatibility to
   `--assess-promotion-with-boundary`; retain both B1 passes, host-generation validation, and
   `promotion_authorized=false`.
8. **3D-B2a authorization, then B2b promotion.** Bind only a fresh post-launch compatibility digest
   into explicit manual authorization, then implement a separate crash-safe promotion journal.
   Forward-only snapshots, reverse migration, automatic rollback, and A retirement remain separate
   projects.

## 10. Regression and acceptance plan

### 10.1 Host-testable matrix

- exact compatible A/B contracts and a valid multi-notebook tree pass both probes;
- missing, unauthenticated, malformed, duplicate-field, noncanonical, oversized, or replayed
  capability/evidence bytes fail closed;
- changed pending transaction, B executable, A executable/inventory, launch UUID, or capability
  digest invalidates the result;
- every supported current page version and migration precondition is represented, including
  v2→v3→v4→v5, both stroke record versions, both edit record versions, texts, links, folders,
  imports, identities, and sync checkpoint/log formats;
- a B-only field that A drops on rewrite is incompatible even when A parses it;
- B writer output outside A's reader set is incompatible;
- forward-only, interrupted, reversible, and falsely declared migration fixtures produce distinct
  outcomes;
- torn JSONL/binary-log tails, incomplete apply/adoption/import markers, preexisting corruption,
  unknown paths, symlinks, path escapes, and special files block without mutation;
- writes between the two roots, including same-length content replacement, are detected;
- an active app singleton, sync/adoption holder, inbound materializer, checkpoint worker, or manual
  app launch prevents assessment;
- probe timeout, signal, early exit, output overflow, and attempted mutation block;
- a write after cached approval makes the cache stale; final assessment re-runs rather than trusts
  it;
- snapshot restoration tests prove whether B-era changes are preserved; a pre-B-only restore is
  never labeled lossless rollback;
- assessment leaves library, sync/device state, pending B, retained A, and all evidence byte-for-byte
  unchanged;
- unknown compatibility aborts before the first stable B rename and leaves A's binary and appreg
  selection unchanged;
- a `go` during preflight/activation is refused, is not acknowledged, and cannot queue a B child or
  evidence worker;
- an A writer, direct SSH launch, sync/adoption holder, installer recovery, and concurrent preflight
  each contend at the documented boundary without a lock-order cycle;
- missing, forged, replayed, truncated, or transaction-mismatched admission state never yields the
  kernel launch capability;
- setting an environment variable, passing a flag, copying a descriptor number, or invoking B
  directly cannot enter `run_page_session`, and the complete data root remains unchanged;
- B receives admission only after exact first-launch root/probe revalidation; parent death, child
  timeout, or handshake failure closes the child before data access;
- power-cut injection before/after compatibility publication, every B rename, appreg publication,
  pending publication, and commit recovers to A or exact pending B without a reusable stale result;
- reboot/host-restart fixtures discard in-memory admission and revalidate durable state;
- changed B app/lifecycle/host bytes, changed A/catalog identity, or a new pending transaction
  invalidates an old result;
- first-launch failure preserves pending B and A, produces no promotion, and requires fresh current-
  state compatibility before retry; and
- a simulated older-updater B mutation that A cannot read reports
  `INCOMPATIBLE_DATA_ALREADY_WRITTEN` and never starts A or deletes either release.

Cross-version CI should retain golden A fixtures and run both directions: B reads A data; B performs
every declared write/recovery; then A reads and round-trips the result without semantic loss. Crash
injection belongs at every migration and multi-file commit point.

### 10.2 Physical Kindle observations and remaining acceptance

Read-only inspection of a first-generation Scribe on firmware 5.18.6 on 2026-10-27 established:

- ARMv7 Linux 4.9.77 and approximately 8.7 GiB free under `/mnt/us`;
- `/mnt/us` is `fuse.fsp` over a vfat backing store with `allow_other`; Ignite app/data/state
  directories report mode `0777`, so Unix mode bits there cannot isolate admission authority from
  ordinary device/USB-side writers;
- `/var/local` is a writable ext3 mount with approximately 406 MiB free. `/var/local/root` is
  root:root mode `2775` (not writable by framework), so a mode-0700 child is a plausible protected
  home for a tiny admission record. No directory was created during this inspection;
- `/tmp` is the expected 64 MiB tmpfs. The existing singleton file remained after the app exited,
  confirming that file existence is not lock ownership;
- transient exclusive-lock tests on existing files under both `/tmp` and `/mnt/us` excluded a
  nested contender and allowed immediate reacquisition after release. They changed no file bytes;
- app registration points at the stable lifecycle host and sets `maxGoTime=60`, along with 60-second
  load/pause/unload deadlines and `unloadOnPause`;
- `sha256sum`, `wc`, `readlink`, `sync`, SQLite, `flock`, and curl exist; Python does not; and
- Ignite was not running, the Kindle main app was active, and `state/update` was absent. The device
  therefore does not contain the current Slice implementation, so no end-to-end gate claim was
  made.

The following physical acceptance remains, using disposable copied data and off-device backups:

- measure full-root traversal/hash and A/B probe time, memory, battery, and `/mnt/us` I/O;
- prove the Rust singleton, sync, adoption, launch, updater, and evidence locks on their exact
  production paths, including abrupt holder death (the read-only inspection used system `flock` on
  existing files only);
- verify `CLOEXEC`, bounded child capture, signal/timeout cleanup, and no framebuffer/input access;
- exercise launch, sync inbound/outbound, unload, assessment contention, reboot, and power loss at
  every new durable publication point;
- test `/mnt/us` visibility and mutation behavior during USB connection/export;
- compare a large real-world library, folders, imports, pending outbox, inbox/apply recovery,
  checkpoint state, and retired epochs before and after assessment;
- demonstrate that both probe commands perform zero writes using before/after full roots;
- observe actual appmgrd host/load/go/reboot ordering and whether a previous active app is restarted
  without a fresh user request;
- prove the launch gate prevents production `go` during an activation coordinator, not only a test
  fixture, and that appmgrd receives no successful result;
- validate the lifecycle→app kernel capability, singleton handoff, parent-death cleanup, and direct
  SSH refusal before any canonical-data system call;
- validate file and directory fsync durability for admission state under the root-owned location,
  plus symlink/permissions behavior after reboot, firmware update, and factory reset;
- measure repeated pending-launch root/probe cost and establish a safe UI timeout/battery budget;
- attempt rollback only in a laboratory fixture after a fresh A probe. Do not use production
  notebooks and do not retire A.

## 11. Blockers and unresolved decisions

- Existing A releases do not expose authenticated storage capabilities or a read-only full-state
  probe. The current exact binary exposes only the library portion. The first compatible
  predecessor policy needs either a signed exact-hash legacy catalog or must block until both
  adjacent releases implement the complete contract.
- The current retained inventory proves integrity relative to installation, not by itself publisher
  authenticity. Installed-release provenance must survive promotion for the next A identity.
- The exact active-sync compatibility surface must be finalized. Treating sync state as rebuildable
  is unacceptable while it may contain unsent or partially applied work.
- The data-writer lock order and USB-state exclusion require Linux/Kindle validation. The present
  B2a.3 boundary alone is insufficient for a data snapshot.
- The current installer takes updater lock before any launch gate and uses a racy `/proc` scan. A
  restricted activation coordinator and crash-safe parent/child lock lifetime are prerequisites;
  acquiring launch gate from inside the current locked script would invert the global order.
- Normal `scribe-app` accepts direct SSH execution today. The admission protocol must either route
  that use through lifecycle or intentionally remove data-mutating direct launch; a CLI flag cannot
  preserve the guarantee.
- No durable, permission-isolated local admission record or lifecycle→application capability exists.
  `/var/local/root/ignite-update` is now the hardware-supported candidate location, but its fsync,
  reboot/update/reset behavior still requires proof and it cannot be sole compatibility evidence.
- The production app registration allows 60 seconds for `go`. Full current-state roots and two
  exact probes may exceed that on real libraries; timing must be measured before choosing between a
  longer validated deadline and a complete tree transactionally maintained by every writer.
- Current page/open migrations are hard-coded and lack a release-level reversibility declaration.
  They must be inventoried before any positive claim.
- A pre-B immutable snapshot does not solve lossless rollback after B edits. Supporting
  forward-only releases requires an explicit product decision and additional recovery design.
- Production release authentication must be provisioned before any real compatibility provenance
  can be trusted; test keys are never production evidence.

Until these are resolved, the only defensible result is `data_compatibility=unknown`, overall
`REVIEW_REQUIRED` when runtime evidence is otherwise valid, and `promotion_authorized=false`.

## Reviewed implementation sources

- `ignite/kindle/scribe-app/src/main.rs`: singleton lifetime and session restart.
- `runtime.rs`: startup order, Home readiness, sync start, safe-point inbound application.
- `store.rs`: page formats, migrations, journal repair, background repair, and page writes.
- `notebook.rs`: notebook manifest, open-time migration, structural commits, and trash.
- `library.rs`: library schemas, legacy migration, notebook structural commits, and trash.
- `folders.rs`, `nav_state.rs`, `prefs.rs`, `nbk_import.rs`: auxiliary user and replica state.
- `storage_inspect_library.rs`: bounded library-only read-only inspection and canonical diagnostic
  report (Slice 3D-B2a.4.2a).
- `storage_inspect_external.rs`: bounded active/retired sync, recovery, device, pairing,
  preferences, and adoption inspection plus canonical external report (Slice 3D-B2a.4.2b).
- `sync_local.rs`, `sync_outbound.rs`, `sync_inbound.rs`, `sync_confirmed.rs`,
  `sync_materialize.rs`, `sync_adopt.rs`, `sync_verified.rs`, and `page_ledger.rs`: identity,
  replication, materialization, adoption, crash recovery, invalidation, and diagnostics.
- `ignite/core/sync-protocol`, `sync-engine`, `portable-backup`, and `backup-archive`: canonical sync
  formats and verified logical backup scope.
- `deployment/kindle/lib/install-runtime.sh`, `uninstall-runtime.sh`, `backup-runtime.sh`, and
  `deployment/kindle/scribe-lifecycle-host.sh`: release/data boundaries and manual backup.
- `tools/kindle_launcher/Scribe.sh`, `tools/kindle_app/registration/ignite.json`, and
  `deployment/kindle/lib/register-helpers.sh`: discovery, appmgrd selection, and handler ownership.
- `ignite/kindle/scribe-lifecycle/src/candidate_health/assessment.rs`, `launch_gate.rs`, and
  `docs/kindle-lifecycle-decision-boundary.md`: current B1/B2a.3 validation and exclusive boundary.
