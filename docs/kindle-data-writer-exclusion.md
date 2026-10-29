# Kindle data-writer exclusion diagnostic

Slice 3D-B2a.4.4 audits the writers that matter to a future pre-first-launch compatibility
decision and adds a read-only diagnostic for the coordination that exists today. It does not add a
candidate admission gate, change any writer, run A or B, or establish a stable snapshot.

## Feasibility decision

The current architecture **cannot prevent every known writer from starting for an entire
inspection window**. The following exclusions are enforceable by existing kernel locks when their
pre-existing lock files can be safely opened:

- lifecycle `go` admission;
- installer, activation recovery, and uninstall mutation;
- lifecycle child and health-worker activity;
- the ordinary `scribe-app` session, including its background threads;
- adoption and active sync; and
- the narrower replica and library-identity transactions.

That set is not a global writer boundary. Recovery maintenance is partly an in-process condition
variable plus a durable marker, lifecycle-host startup creates directories and appends logs before
`go` takes the launch gate, and device/pairing/preferences writes have no independent cross-process
lock outside the application singleton. A manually written program can call public storage APIs
without the singleton. USB mass-storage exposure, an SSH/root writer, and other filesystem clients
do not acquire any Ignite lock. No source-proven continuous USB-export gate exists.

Consequently `writer_exclusion=established` is unreachable in the diagnostic. Acquiring every
known lock and observing no active transaction marker produces only `partial`; every uncertainty
produces `unverified`.

## Writer inventory

“Known” means a packaged production path is source-proven. “Possible” means callable repository
code or an external mechanism can write the same roots but is not a normal packaged Kindle entry
point. A kernel lock excludes a writer only when that writer acquires the same open-file lock.

| Writer | Entry point and status | Persistent roots | Existing coordination | Async start | Exclusion gap |
|---|---|---|---|---|---|
| Application runtime | **Known:** `main.rs` → `runtime::run_page_session`; pen commits and structural actions call `store`, `notebook`, `library`, and `folders` writers | Library, library-local sync identity/generation/ledger, navigation, preferences | `/tmp/scribe-app.lock` for the process lifetime; in-process mutation admission | Yes, after lifecycle `go`; a session restart occurs inside the same singleton | Direct execution still takes the singleton, but a program calling library APIs need not; no USB coverage |
| Notebook/page/library | **Known:** page journal append/fsync; page/notebook create, rename, delete/quarantine; folder and manifest atomic replacement; legacy and page migration; torn-tail truncation; background repair | `library.meta`, notebook/page trees, folders, import metadata, trash, `.scribe-sync` | Application singleton; per-process mutation admission and identity lock for identity writes | Runtime thread; notebook open can migrate all pages | No independent whole-library cross-process lock; “open” and recovery operations can write |
| Background sync | **Known:** `OutboundSync::open_with`, journal thread, uploader, checkpoint thread, deferred confirmed-base loader, inbound application | Active sync root, blobs/checkpoints/journals; inbound materialization also writes library | Application singleton plus `<sync>/sync.lock`; in-process `Shared`/I/O mutexes; identity and replica locks for their transactions | Yes, three or more background workers | Mutexes are process-local; a noncooperating writer bypasses `sync.lock` |
| Pairing/device/preferences | **Known:** `server_link` manager and jobs, `prefs::save_side_button`, `client_id`, profile publication | Device root and `preferences.json` fallback locations | Application singleton only; individual temp/fsync/rename publication | Yes, server-link/discovery threads and UI jobs | No separate device-state cross-process lock |
| Adoption | **Known:** `sync_adopt::adopt_library` and startup `recover_interrupted_adoption` | Live/staged/retired library and sync roots, adoption plan, optional device profile | `<sync>.adopt.lock`, then live `<sync>/sync.lock`; startup is also under singleton | UI adoption job; startup recovery | Lock file is created by normal adoption; publication drops live sync lock only after the durable plan fences normal sync |
| Import and import recovery | **Known:** `nbk_import` import, receipt transitions, partial cleanup, startup recovery | Native library plus `.scribe-import`; Kindle NBK/SQLite inputs are read-only | Application singleton and library mutation admission | UI/import path; recovery at startup | No independent import lock; an API caller outside the app can bypass singleton |
| Recovery/repair | **Known inside app:** adoption recovery, inbound apply, journal recovery, NBK recovery, page repair. **Possible:** public `sync_recovery_exec` functions if embedded in another program | Library, active sync, recovery intent/completion/maintenance markers | In-process `SessionCell` maintenance hold; durable maintenance marker; active sync/app locks only when the caller already owns them | Some recovery and network work is supervised or threaded | The maintenance hold is not cross-process. Marker presence blocks a decision but cannot prevent a writer |
| Lifecycle supervisor | **Known:** host wrapper and `scribe-lifecycle` host/child logging; `go` child spawn and health evidence | App logs, creation of sync/device directories, update evidence, `/tmp` coordination | Launch gate; child/worker activity locks; evidence lock; host lease/control socket | Yes, appmgrd can start a host and issue `go` | Wrapper writes happen before `go` admission; gate does not cover arbitrary direct storage writers |
| Installer/updater/uninstall | **Known:** SFX runtime, interrupted activation recovery, retained-slot publication, registration and uninstall | Installed release, `previous-release`, run journal, update state, app registration/backups; optional explicit uninstall purge can delete library | `/tmp/ignite-updater-transaction.lock` before mutation/recovery cleanup | Separate Scriptlet/process | Does not take the launch gate; current installer’s process scan is not launch exclusion |
| Backup helper | **Known:** `backup-runtime.sh` reads the library and writes an archive | `/mnt/us/Ignite-Backups`; reads library | None with application/data writers | Separate Scriptlet | Archive may not be a stable multi-file snapshot; not a writer to canonical data |
| Host sync tool | **Possible, not packaged in the Kindle release:** `scribe-sync-tool identity` creates replica and library identity; other commands are read-only | Supplied library/state roots | Replica and identity locks, but no app singleton or active-sync lock | Separate manual process | Must be treated as an operator writer if copied to the device |
| USB/external/manual | **Known mechanism, exact Kindle behavior unverified:** USB export, SSH/root tools, firmware/framework services | Any exported or root-accessible `/mnt/us` path | None shared with Ignite | Yes | Cannot be continuously excluded by current code; a sampled process/mount status is not prevention |
| Local SQLite | **No Ignite library writer:** Ignite’s native library is files/journals, not SQLite. Installer registration writes Kindle appreg SQLite; NBK import reads Kindle SQLite | Kindle app registration outside the library; Amazon sources read-only to Ignite | Updater lock for app registration | Installer only | External Kindle components remain outside Ignite’s data-lock proof |

Hidden initialization writes are part of the inventory: missing-root creation, migration-record
cleanup, torn-tail truncation, background repair, replica/identity allocation, epoch establishment,
legacy confirmed-state conversion, inbound recovery, import recovery, and adoption roll-forward.
The inspection commands intentionally call none of them.

## Existing synchronization audit

| Mechanism | Cooperating scope | Acquisition and lifetime | Crash/reboot behavior | Diagnostic limitation |
|---|---|---|---|---|
| `/tmp/ignite-lifecycle-launch-gate.lock` | Production lifecycle `go` shared; decision exclusive | Safe `O_NOFOLLOW` open; nonblocking for `go`, bounded for decision | Kernel releases on process death; tmpfs resets on reboot | Host-wrapper writes precede the gate; a direct app/library caller can bypass it |
| `/tmp/ignite-updater-transaction.lock` | Install, activation recovery, uninstall, future promotion | Exclusive `flock`, immediate exit 75; inherited and reverified across installer `exec` | Kernel releases after every inheriting holder dies | Covers installed release, not notebook/sync writers |
| `/tmp/ignite-lifecycle-child.lock` and `...evidence-worker.lock` | Lifecycle child and asynchronous evidence worker | Shared activity token; decision takes exclusive with a deadline | Child inherits its token; process death releases | Requires current-host endpoint checks for a full lifecycle decision; this diagnostic only samples the locks |
| `/tmp/scribe-app.lock` | Normal and directly executed `scribe-app` | Exclusive process-lifetime lock; normal acquisition creates/writes a PID | Kernel releases on death; stale bytes have no authority | Public library APIs and external writers need not acquire it |
| `<sync>.adopt.lock` | Adoption preparation/publication and startup adoption recovery | Nonblocking exclusive file lock; normal path creates it | Kernel releases on death; plan drives recovery | No other recovery family uses it |
| `<sync>/sync.lock` | One `OutboundSync`; adoption takes it before preparing the replacement | Nonblocking exclusive file lock retained by `OutboundSync` | Kernel releases on death | Does not cover device preferences or arbitrary sync-file writers |
| `<sync>/replica.lock` | Replica allocation | Blocking exclusive file lock around one transaction | Kernel releases on death | No timeout and normal acquisition can create the directory/file |
| `<library>/.scribe-sync/identity.lock` | Identity allocation/binding/retirement | Blocking exclusive file lock around one transaction | Kernel releases on death | No timeout; not a whole-library lock |
| Recovery `SessionCell` and `recovery-maintenance` marker | Ordinary/inbound library writes in one process | Condvar drains in-flight writes; marker is durable state, not lock authority | Process state is lost; durable marker survives | Cannot prove cross-process exclusion |
| Launch-health evidence lock | Health owner/evidence publication and B1/B2a.3 assessment | Worker takes exclusive publication lock; decision holds exclusive | Kernel releases; durable evidence remains | Coordinates update evidence, not notebook/sync data |
| Filesystem/database locking | File replacement and append operations; Kindle appreg SQLite | Per-file atomic rename/fsync or SQLite transaction | Format-specific recovery | No filesystem-wide snapshot or database covers Ignite’s complete state |

Normal lock acquisition often creates lock files and directories. The diagnostic does not call
those normal helpers: it uses a no-follow read-only descriptor for an already existing, singly
linked, effective-user-owned regular file, verifies file identity, and then requests the same
exclusive kernel lock. Missing or unsafe resources are incomplete evidence. Lock-file contents are
ignored and remain byte-for-byte unchanged.

## Lock-order contract

The extended hierarchy is:

1. lifecycle launch gate, exclusive;
2. updater transaction lock;
3. lifecycle running-child lock, then evidence-worker lock, exclusive with one bounded deadline;
4. application singleton, exclusive;
5. adoption lock;
6. active-sync lock;
7. replica lock, then identity lock, for the narrower transactions;
8. launch-health evidence lock when health evidence is part of the decision; and
9. transaction checks, stable roots, and exact A/B probes while every applicable guard remains
   held.

Release is the exact reverse. An operation must never acquire an earlier item while holding a later
one. No acquisition is reentrant: a process that already owns an exclusive `flock` through a
different open-file description must retain and pass the actual guard within trusted code rather
than reopen the pathname. An external program or descriptor-number environment variable is not
ownership proof.

The diagnostic uses items 1–7 with one 250 ms monotonic deadline. It immediately cancels or refuses
on contention and releases all guards on every return path or process death. It does not take the
evidence lock because it does not inspect health evidence. Reboot releases all kernel locks but does
not validate persistent state; the next decision must reacquire and revalidate everything.

This hierarchy is a future integration contract, not a claim that all current writers already
enter through it. In particular, installer startup begins at item 2, and recovery’s in-process hold
cannot be acquired cross-process. Integrating those paths requires a separately reviewed trusted
coordinator rather than adding an inverted nested lock.

## Diagnostic interface and schema

The pre-initialization command is:

```text
scribe-app --inspect-writer-exclusion --data-dir LIBRARY --sync-dir SYNC
```

Both roots must be absolute and traversal-free. The command performs no application initialization,
networking, recovery, adoption, migration, device access, or candidate execution. It creates no
lock or state file. It checks, in hierarchy order, the five `/tmp` locks, adoption, active sync,
replica, and identity locks. Only after all are held does it sample the adoption plan, inbound apply
marker, recovery intent, and recovery-maintenance marker. Presence blocks; absence is not a global
recovery proof.

The deterministic report begins with `IGNITE_WRITER_EXCLUSION_INSPECTION_V1` and includes:

- `result=PARTIAL|REFUSED|INCOMPLETE|UNSUPPORTED`;
- `writer_exclusion=partial|unverified`;
- one `lock_NAME=acquired|contended|missing|unsafe|error|unsupported|cancelled|not_attempted`
  field per lock;
- `transaction_markers=clear|active|unknown|not_checked`;
- explicit recovery, lifecycle-host, USB, and noncooperating-writer gaps; and
- the fixed safety tail:

```text
incomplete_evidence=true
atomic_snapshot=false
data_compatibility=unknown
promotion_authorized=false
inspection_action=report-only
```

Contention is exit 75. A successfully completed partial sample is exit 2. Incomplete and unsupported
results are exit 3. There is intentionally no exit-0 or `established` result, so a generic “command
succeeded” check cannot turn this diagnostic into candidate admission.

## USB and filesystem assessment

The repository contains no trusted, continuously held USB-export exclusion primitive. Firmware
observations show `/mnt/us` is `fuse.fsp` over vfat with `allow_other`; its mode bits do not isolate
Ignite from ordinary framework/USB writers. Mount/process/LIPC sampling could become stale
immediately after observation and therefore cannot authorize a snapshot. The diagnostic reports
`usb_state=unsupported` and `usb_exclusion=unavailable` without reading or changing device USB
configuration.

Host Linux proves advisory-lock contention and crash release only for the tested host filesystem.
It cannot qualify `/mnt/us`, USB transitions, suspend/resume, or firmware services. Native Kindle
tests must use disposable copied data and must not unmount, remount, eject, disable services, or
change permissions.

## Future A/B executor boundary

A later assessor needs an internal trusted function—not an arbitrary-program wrapper—that returns
private RAII guards only after the complete hierarchy, current lifecycle generation, cross-process
recovery exclusion, and continuous USB/storage gate are proven. The coordinator must retain those
guards across both exact-binary probes and stable-root comparisons. It must refuse before executing
either probe if any writer cannot be excluded.

Acquiring the boundary, maintaining it, authenticating A/B, obtaining a stable root, evaluating
compatibility, publishing admission, and handing the singleton capability to exact B are separate
steps. This slice implements none of the latter steps.

## Verification scope and remaining work

Disposable tests cover deterministic and fail-closed reports, strict CLI dispatch, relative and
traversal refusal, free/contended/missing/unsafe/read-only lock resources, stale lock bytes, active
transaction markers, timeout/cancellation, separate-process application/sync/adoption/updater
contention, concurrent inspection, process-death release, and byte preservation. Unix-only cases
must run natively on Linux.

Still required before a positive decision:

1. instrument every legitimate recovery/device writer to cooperate with one cross-process data
   boundary without reversing current lock order;
2. move lifecycle-host pre-`go` data-root writes behind the trusted boundary or prove them
   irrelevant to the stable root;
3. provide a continuous, hardware-qualified USB/export exclusion signal;
4. integrate this layer inside the existing lifecycle/updater boundary rather than sampling it from
   a standalone process;
5. authenticate installed A/B and both probe implementations; and
6. run the full stable-root and bidirectional compatibility assessment before first B launch.

Until then, LKG retention and pending-health semantics are unchanged, and compatibility,
authorization, promotion, rollback, and A retirement remain blocked.
