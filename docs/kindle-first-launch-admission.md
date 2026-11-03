# Kindle first-launch admission architecture (Slice 3D-B2a.4.6a)

The read-only slot-mapping foundation is documented in
[`kindle-installed-slot-binding.md`](kindle-installed-slot-binding.md). It eliminates ambiguity
about current stable and retained paths but does not satisfy admission: no trusted installer
publication record or kernel-backed verified-object handoff exists, and the current component
profile has explicit registration/recovery gaps.

## Status and feasibility decision

This slice defines a source-grounded architecture and a test-only reference model. It does not
change installation, activation, lifecycle admission, application startup, promotion, rollback, or
data formats. The current device path still cannot produce a positive admission result.

The architecture is feasible, but only as one coordinated future change. No existing component
owns the whole interval from selecting B through B's first compatibility-relevant data access. The
future owner must be a restricted internal coordinator that acquires the launch gate before the
updater lock, validates the exact pending transaction and A/B closure, excludes every relevant
writer, and retains kernel authority through a one-shot handoff to the exact B process. Extending
the current installer from inside its updater lock would invert the established order and is not
safe.

Today the required inputs are incomplete: production release authentication is unprovisioned,
installed A/B binding is unverified, writer and USB exclusion are incomplete, and the stable-state
preflight cannot prove bidirectional compatibility. Therefore:

```text
first_launch_admission=unavailable
data_compatibility=unknown
promotion_authorized=false
```

## 1. Current topology and the admission boundary

The current source path is:

```text
install-runtime.sh
  updater lock -> recovery -> retained A -> stable-path B renames -> appreg -> pending-health
      |
      +-- independently selectable stable B paths begin at the first rename

appmgrd -> scribe-lifecycle-host.sh
  creates sync/device directories and appends a log
    -> scribe-lifecycle --host
      publishes its endpoint and accepts go
        -> shared launch gate -> child/worker tokens -> spawn stable scribe-app
          -> health descriptor -> application singleton
            -> display/input setup
              -> adoption recovery
                -> library open/create/migration
                  -> sync open/recovery/workers -> import recovery -> Home ready
```

The installer obtains the updater lock but not the launch gate. Its process scan is not atomic with
a later `go`. It makes B selectable before publishing the pending record. Interrupted activation
recovery can restore A before commit, but committed B remains pending; neither path establishes a
pre-first-launch data decision.

There is no general manual executable rollback transaction in the current tree. The installer can
restore A while recovering an interrupted activation, and it retains A after a committed upgrade,
but nothing safely reselects retained A after B has operated on shared data. The packaged recovery
component and successful installer also do not launch the application.

The installed host wrapper writes logs and may create application-state directories before the
lifecycle `go` callback. Those writes are not notebook edits, but they are release-owned durable
activity and cannot be silently excluded from a compatibility root. The future path must either
move them after admission, use a separately classified non-authoritative log area, or bind and
prove them harmless.

The lifecycle `go` callback already provides launch/evidence coordination, but it checks no
compatibility authorization. In the child, the earliest useful defense-in-depth point is after the
application singleton has been acquired and before `default_config` and
`runtime::run_page_session`. The latter can run adoption recovery, create or migrate a library,
repair journals, establish sync state, publish identities, and start asynchronous writers before
Home readiness. A gate at Home, or only before notebook binding, is too late.

There are therefore two mandatory checks:

1. **Selection check:** the trusted coordinator must authorize exact B before any B-owned host or
   application code is selected for normal execution.
2. **Process check:** exact B must consume a live, one-shot capability before any
   compatibility-relevant persistent access. Direct execution without it must fail closed.

Read-only diagnostics may remain available only if dispatch is proven to precede all persistent
initialization and the diagnostic cannot enter normal runtime.

## 2. Candidate-launch bypass audit

Every route below must converge on the same admission contract in 3D-B2a.4.6b. None does today.

| Route | Current behavior | Required future rule |
|---|---|---|
| Launcher Scriptlet/appmgrd | Starts the stable host path | Coordinator owns selection; host cannot reach normal `go` without current admission |
| Direct lifecycle host or `--child` | Can be invoked outside appmgrd | Normal child spawn requires the same coordinator-held capability |
| Direct `scribe-app`/SSH execution | Takes only the app singleton | Normal runtime refuses without a live handoff; no command-line or environment override |
| Reboot or host restart | Loses in-memory guards and can start the stable host again | Reacquire all locks and revalidate; a durable record alone is never authority |
| Old host after stable-path replacement | May have opened or cached old release state | Bind host generation and exact host/lifecycle/app closure; mixed trees refuse |
| Duplicate or rapid `go` | Suppresses a running child but has no compatibility gate | One assessment and handoff per exact transaction/generation; duplicates refuse |
| In-process library-restore restart | Re-enters `run_page_session` with the singleton | Remains within the consumed process capability; a new process needs a new capability |
| Interrupted install/recovery | Can restore precommit A or finish cleanup | Recover under coordinator order; never select transitional B |
| Uninstall or future promotion | Mutates state under updater lock | Join the coordinator before mutation; promotion remains separately authorized |
| Manual/public storage API caller | Can bypass the singleton | Outside the proof until a continuous platform/storage exclusion exists |
| USB or another `/mnt/us` writer | Does not cooperate with Ignite locks | Continuously exclude with a hardware-qualified primitive; sampling is insufficient |
| Stale/orphan B process | May outlive pathname changes | Child activity, singleton, process identity, and parent-death state must agree |
| Copied or partially replaced binaries | Filename/version may look correct | Require exact authenticated closure and opened-file identity |
| Side-effect-free diagnostics | Dispatch before ordinary runtime | May bypass only while strictly read-only and unable to enter normal runtime |

Legacy releases that cannot participate cannot be silently grandfathered. They require an
authenticated legacy catalog plus exact non-mutating probes and a specifically reviewed bootstrap
policy, or the candidate remains blocked.

## 3. Trusted coordinator and ownership

The future authoritative owner is a small internal activation/admission coordinator. It must not
be an arbitrary-program wrapper and must not accept environment variables, descriptor numbers,
paths, PID files, or copied reports as proof. It either internalizes the fixed activation steps or
supervises a fixed, verified helper protocol while retaining the actual kernel guards.

For a candidate first launch it must:

1. acquire the exclusive lifecycle launch gate;
2. acquire the updater transaction lock;
3. drain child and evidence-worker activity with the bounded lifecycle protocol;
4. establish application/adoption/sync/replica/identity and continuous USB exclusion;
5. re-read and authenticate the exact pending transaction, retained A, installed B, capabilities,
   provenance, inventory, and compatibility inputs under those guards;
6. publish only a canonical, permission-isolated authorization record;
7. open and verify the exact B executable closure without following substituted paths;
8. spawn that exact process and complete the kernel handoff while retaining every guard; and
9. release or transfer guards only after B consumes the capability before persistent access.

The coordinator does not decide promotion or runtime health. First-launch admission says only that
this exact candidate may begin operating on this exact compatible state under the declared
contract. `pending-health.v1`, retained A, and later health qualification remain unchanged.

The proposed authoritative record belongs under a root-controlled, non-public location such as
`/var/local/root/ignite-update`, not `/mnt/us`. The location is only a candidate until ownership,
no-follow handling, fsync/rename durability, reboot, firmware-update, and factory-reset behavior
are qualified on Kindle. `/mnt/us` records remain diagnostic because its observed permissions and
external visibility cannot isolate authority.

## 4. Versioned state machine

These states refine the existing pending transaction; they do not replace `pending-health.v1` or
create another release identity. `CANDIDATE_PREPARED` is a phase of the existing activation journal
and authenticated candidate binding, not a second pending record. Every transition that changes
durable admission state is coordinator-owned under launch-gate then updater-lock order; assessment
and handoff additionally retain the full guards described in Section 6.

| State | Owner and transition requirements | Crash/retry behavior | May execute |
|---|---|---|---|
| `NO_PENDING_CANDIDATE` | Updater proves no pending candidate | Idempotent | Selected healthy release under ordinary policy |
| `CANDIDATE_PREPARED` | Coordinator has authenticated/staged exact B while A remains selected; launch gate then updater lock held | Crash leaves A selected; recover only transaction-owned staging | A only, if no transition guard is held |
| `ADMISSION_REQUIRED` | Exact transaction/pending digest/A/B/inventory are durable; no valid authorization | Reboot stays required; retry starts a new assessment | A only; an A write invalidates prior snapshot evidence |
| `ASSESSMENT_IN_PROGRESS` | Full ordered boundary held; exact probes and stable-root checks running | Crash/timeout invalidates; retry from required | Neither normal A nor B during boundary |
| `ADMISSION_REFUSED` | Canonical refusal binds candidate and reasons | Reboot/retry cannot approve it; a complete reassessment is required | A only if independently safe; B blocked |
| `ADMISSION_AUTHORIZED` | Complete trusted evidence and canonical authorization published | Not executable authority; restart/generation change requires revalidation | Neither until one-shot handoff |
| `FIRST_LAUNCH_IN_PROGRESS` | Exact B has a live parent-owned capability; guards and identity remain verified | Death, timeout, or ambiguous acknowledgement invalidates | Only exact capability-bearing B |
| `FIRST_LAUNCH_CONSUMED` | Exact B acknowledged before persistent access; nonce consumed | Cannot authorize another process; does not prove health or promotion | That process under the declared contract |

`ADMISSION_REFUSED` and `FIRST_LAUNCH_CONSUMED` are facts, not reusable permissions. Candidate byte,
pending digest, retained inventory, capability, lifecycle generation, or stable-data changes return
the current candidate to `ADMISSION_REQUIRED` or block it as conflicting. Unknown or missing state
always refuses B.

Transitions must be idempotent by exact transaction and publication generation. Retries may repeat
validation; they may never reinterpret partial bytes, an interrupted record, or old authorization
as success.

After consumption, every new B process still needs a fresh lifecycle-issued process capability so
direct execution cannot bypass coordination. It does not reuse the first-launch nonce. Whether the
coordinator can issue that later capability from the consumed contract or must repeat compatibility
assessment depends on the proven future-write closure and current binding; any identity, policy,
pending-state, or uncovered-data change forces reassessment. That later-launch policy is not
implemented or modeled in this slice.

## 5. Canonical authorization binding

A future versioned record must bind at least:

- schema and issuance context;
- pending transaction identifier and SHA-256 of canonical `pending-health.v1`;
- authenticated installed-release closure for A and B: app, lifecycle, host, helpers, exact sizes
  and digests, plus the signed release/capability closure identity;
- complete retained-A inventory and exact retained executable identity;
- verified installation/slot binding rather than caller-supplied paths;
- stable inventories before and after exact probes, including every declared library, sync,
  device, recovery, retired, preference, and settings root;
- complete writer-exclusion and continuous USB-exclusion proof identities;
- exact A-on-current-state and B-on-current-state results;
- exact future B-write/A-read closure, including migrations and documented crash states;
- checker/coordinator implementation identity and policy version;
- lifecycle host generation and control endpoint identity; and
- a unique assessment/publication generation.

Authenticated provenance and capabilities are trusted only after the production key and installed
closure are established. Current diagnostic reports, path arguments, B1/B2a output, writer-lock
samples, and preflight inventories are inputs for review, not authority. A release signature proves
publisher origin; it does not prove compatibility with current device data.

The record must use one strict bounded encoding, exclusive creation, no-follow and file-identity
checks, safe temporary publication, file and parent-directory durability, and reject all trailing,
duplicate, or noncanonical bytes. Its digest is part of the one-shot handoff. Record possession or
valid syntax alone never authorizes execution.

The architecture-level positive byte contract is the following fixed-order LF-only record. Every
digest is 64 lowercase hexadecimal characters; counters are canonical unsigned decimal without a
leading zero; `transaction` uses the existing canonical pending grammar; and no field, whitespace,
CR, NUL, duplicate, unknown line, or trailing byte is permitted. The production field-size and
total-size constants must be frozen before 4.6b implementation.

```text
IGNITE_FIRST_LAUNCH_ADMISSION_V1
schema=1
state=admission_authorized
issuance_context=kindle-first-launch
transaction=<pending transaction>
pending_sha256=<digest>
retained_a_release_closure_sha256=<digest>
retained_a_app_sha256=<digest>
retained_a_lifecycle_sha256=<digest>
retained_a_host_sha256=<digest>
candidate_b_release_closure_sha256=<digest>
candidate_b_app_sha256=<digest>
candidate_b_lifecycle_sha256=<digest>
candidate_b_host_sha256=<digest>
retained_inventory_sha256=<digest>
retained_a_provenance_sha256=<digest>
candidate_b_provenance_sha256=<digest>
retained_a_capabilities_sha256=<digest>
candidate_b_capabilities_sha256=<digest>
stable_inventory_before_sha256=<digest>
stable_inventory_after_sha256=<same digest>
writer_exclusion_sha256=<digest>
usb_exclusion_sha256=<digest>
compatibility_evidence_sha256=<digest>
future_write_compatibility_sha256=<digest>
coordinator_policy_sha256=<digest>
assessment_generation=<decimal>
lifecycle_host_lease_sha256=<digest>
control_socket_device=<decimal>
control_socket_inode=<decimal>
```

The authorization identity is SHA-256 of exactly those bytes, including the final LF. A refusal is
a different fixed variant: the same magic/schema, `state=admission_refused`, the transaction,
pending/A/B/inventory binding, `reason_count=<decimal>`, then exactly that many sorted
`reason_<n>=<closed-enum-token>` lines. Missing evidence is a refusal reason, never an omitted
positive field. Transient `assessment_in_progress` and `first_launch_in_progress` records contain
only the candidate binding, generation, and a digest of the parent-held capability; after restart
they are always invalidated to required/refused and can never be upgraded from durable bytes.
`first_launch_consumed` records the authorization digest and consumed capability digest but is not
permission for another process.

## 6. Lock hierarchy and deadlock assessment

The global order is:

1. lifecycle launch gate, exclusive;
2. updater transaction lock;
3. lifecycle child lock, then evidence-worker lock, exclusive with a bounded deadline;
4. application singleton;
5. adoption lock;
6. active-sync lock;
7. replica lock, then library-identity lock;
8. launch-health evidence lock only for later health assessment; and
9. protected state checks and exact probes while required guards remain held.

Release is reverse order. Acquisition is not reentrant: trusted code retains and hands over the
actual guard/open-file description rather than reopening the path. Contention, missing resources,
uncertain host state, queued workers, and timeouts refuse. Process death releases kernel locks but
does not make durable state valid.

The current installer starts at item 2. It must not attempt item 1 while holding item 2. The future
coordinator obtains item 1 first and either calls a fixed activation operation that adopts the
already-held updater guard or internalizes activation. Likewise, an app cannot reopen the singleton
after the parent already owns it through another open-file description; it must adopt the actual
guard under a verified protocol.

This order excludes cooperating Ignite writers only. Recovery maintenance is partly in-process,
device/preferences have no narrower independent lock, public APIs can bypass the singleton, and
USB/noncooperating writers remain gaps. Positive admission is unavailable until those gaps have a
continuous source- and hardware-verified boundary.

## 7. Lifecycle-to-application kernel handoff

The handoff must combine durable binding with live kernel ownership:

1. The coordinator retains every guard and opens exact verified B without path substitution.
   Production must qualify exact-inode execution (`fexecve`, `execveat`, or an equivalent platform
   primitive); spawning a stable pathname leaves a replacement interval.
2. It creates a private Unix channel and unpredictable one-use nonce bound to the authorization
   digest, transaction, exact executable closure, lifecycle generation, and child creation.
3. It spawns B and verifies peer/process identity using Kindle kernel facts (for example peer
   credentials, process start identity, and `/proc` executable identity). The exact primitives need
   device qualification.
4. B adopts the inherited singleton and, before `default_config` or runtime, recomputes its
   executable/pending binding and answers the parent's challenge.
5. The parent verifies the response and only then sends final proceed. B treats EOF, timeout, wrong
   generation/digest, or parent death as refusal and exits before persistent access.
6. The parent consumes the nonce and retains duplicate-start exclusion until the child crosses the
   boundary. No later process can replay the channel or record.

This particular capability is the one-shot first-launch capability. Subsequent processes require
new nonces and live lifecycle handoffs; `FIRST_LAUNCH_CONSUMED` can never be presented as a process
capability.

An environment variable may locate an inherited channel descriptor, but neither is proof. Proof is
the live kernel connection to the guard-owning coordinator, non-replayable challenge, exact
process/executable identity, and matching canonical record. Arbitrary external programs cannot hold
the boundary because they could close descriptors while continuing.

Mixed-generation host/app trees refuse. A stale endpoint, coordinator restart, or host lease change
invalidates an unconsumed authorization. A running child or uncertain worker prevents quiescence.

## 8. Crash and reboot matrix

| Interruption or race | Durable interpretation | Safe next action |
|---|---|---|
| Before candidate preparation | No candidate authority | A may run under ordinary policy |
| During staging, before stable publication | Transaction-owned staging may exist | Recover/discard staging; A remains selected |
| During B publication/app registration | Mixed selection is possible today | Block launch; verify journal; restore A or finish exact B preparation |
| After pending B, before assessment | B is pending but unauthorized | Block B; require fresh assessment |
| During evidence collection | Partial output is not evidence | Ignore transaction-owned temporary output; retry from required |
| During authorization publication | Torn/missing/noncanonical record | Refuse; recover publication by exact transaction |
| Authorized, before spawn | Record has no live capability | Restart invalidates issuance; fresh revalidation |
| During spawn/handoff | Capability consumption is uncertain | Close channel, kill/reap if possible, invalidate nonce, reassess |
| B starts, before first persistent access | No final proceed means no permission | Terminate/refuse and verify roots unchanged |
| During B's first persistent write | Data may be B-era | Never assume A safe; exact A must re-probe before executable rollback |
| During recovery/migration | State may be a B crash form | Only declared idempotent B recovery; A requires explicit read proof |
| During pending health assessment | Health is incomplete | Keep B pending/A retained; no promotion or automatic rollback |
| During future executable rollback | Selection and data safety differ | Journal selection; never run A without current A-read proof |
| Concurrent install/uninstall/promotion | Updater lock contends after launch gate | Refuse one operation; no cleanup before ownership |
| Duplicate/rapid `go` | One generation/capability owns admission | Refuse duplicates; never issue from stale state |
| Stale record after a new transaction | Binding mismatch | Ignore/refuse old record; assess new transaction |
| A/B path or inode replaced | Exact closure changed | Invalidate; no path-based fallback |
| Retained A missing/damaged | Rollback anchor absent | Block admission and rollback; do not repair implicitly |
| Lock contention/uncertain worker | Snapshot not proven | Bounded refusal; no optimistic acceptance |
| Reboot in a transient state | Kernel guards/capability are gone | Revalidate; transient states return to required/refused |

Executable selection rollback and data rollback are separate. Retaining A protects bytes, not A's
ability to read state after B writes. No crash path may select A solely because B failed.

## 9. Existing versus proposed enforcement

| Property | Existing implementation | Future requirement |
|---|---|---|
| Release authenticity | Canonical Ed25519 verifier; production pin unprovisioned | Provision pin and authenticate installed closure/capabilities |
| Pending identity | Strict pending record and retained inventory | Bind exact pending digest and installed slots |
| Launch/updater exclusion | Implemented independently; B2a.3 order for assessment | Coordinator owns both before activation/selection |
| Child/worker quiescence | Implemented for boundary assessment | Retain through prelaunch assessment and handoff |
| Data-writer exclusion | Diagnostic reports partial/unverified | Complete continuous writer/USB boundary |
| Stable inventory | Observational two-pass preflight | Atomic boundary plus exact authenticated probes |
| Compatibility | Always unknown | Positive bidirectional and future-write/A-read closure |
| Durable admission | None | Strict root-owned transaction record |
| App enforcement | Direct normal startup allowed | Live capability before persistent access |
| Promotion/rollback | Both disabled | Separate later transactions; admission implies neither |

The only implementation in this slice is `admission_model.rs`, compiled under `cfg(test)` inside
`scribe-lifecycle`. It models typed states, trusted versus diagnostic evidence, canonical synthetic
bindings, explicit refusal reasons, one-shot handoff, replay rejection, and restart invalidation.
Positive fixtures use `IGNITE_TEST_ONLY_ADMISSION_MODEL_V1`; there is no production parser,
command, filesystem path, key, or constructor. It cannot authorize a Kindle launch.

## 10. Prerequisites for B2a.4.6b

Before production enforcement is proposed:

1. provision the production release public key and define signed storage-capability closures;
2. bind installed A/B paths to authenticated records, including all executable helpers;
3. instrument every legitimate data/recovery/device writer for the coordinator boundary;
4. establish continuous Kindle-qualified USB/noncooperating-writer exclusion or remain blocked;
5. implement exact non-mutating A/B probes and future-write/A-read cross-version evidence;
6. qualify the protected admission-state location and durability semantics;
7. refactor activation so launch gate precedes updater lock without reverse nesting;
8. define the fixed coordinator API and guard handoff, including singleton adoption;
9. qualify exact-inode execution, peer/process identity, parent-death behavior, and appmgrd timing on
   first-generation Scribe firmware 5.18.6; and
10. add power-cut, reboot, substitution, mixed-tree, direct-exec, USB, and large-library tests using
    disposable device data.

Until then, B may still launch under today's unchanged behavior, but the system must not claim that
such a launch passed first-launch admission. No admission record, authorization, promotion,
rollback, or LKG retirement is implemented here.
