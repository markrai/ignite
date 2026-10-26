# Kindle updater transaction lock

Slice 3D-B2a.1 adds one cooperative, process-scoped exclusion boundary for installed-release
mutation. It does not authorize or perform promotion or rollback.

## Contract

- The fixed lock is `/tmp/ignite-updater-transaction.lock`.
- `scribe-lifecycle --with-updater-transaction-lock -- PROGRAM [ARG ...]` opens it with
  `O_NOFOLLOW`, verifies a singly linked regular file owned by the effective user, and takes
  `flock(LOCK_EX | LOCK_NB)` through the Linux kernel API. No `flock` executable is required.
- Contention is immediate and deterministic (exit 75). Unsafe paths, missing helpers, malformed
  descriptor handoff, and all other setup errors fail closed before installed-state mutation.
- The descriptor has `FD_CLOEXEC` cleared and the helper replaces itself with `PROGRAM`. The lock
  therefore covers the complete shell operation, releases automatically after normal exit or a
  crash, and remains held if a surviving descendant still has the descriptor.
- Before either runtime mutates state, it invokes
  `scribe-lifecycle --verify-updater-transaction-lock FD`. The verifier checks that the descriptor
  identifies the exact safe lock inode and performs `flock(LOCK_EX | LOCK_NB)` on that inherited
  open file description. A merely open descriptor therefore cannot bypass a different holder; it
  fails with exit 75. If no holder exists, verification first establishes the lock and only then
  permits mutation.
- The file is outside `/mnt/us/scribe-app`, so first install can acquire it and uninstall cannot
  unlink its own coordination object. Reboot removes the tmpfs file only after all holders are gone;
  the next invocation safely creates it again.
- Cooperative code must never unlink the lock file. Its contents have no authority; only the
  kernel lock is meaningful.

The SFX archive extraction and logs are not installed release state. Extraction uses a
process-unique tree so a contender cannot replace the package tree read by the lock holder. After
extraction, `install-runtime.sh` acquires the lock before publishing the early diagnostic helper,
creating release directories, cleaning staged files, or running journal recovery. Direct invocation
uses the same entry. `uninstall-runtime.sh` acquires the same lock before process checks,
unregistration, recovery-client removal, or any release-file deletion. The sourced registration and
recovery helpers have no standalone main operation and execute only inside those locked runtimes.

Existing activation journals remain the crash-recovery authority. The lock neither repairs state nor
changes pending-health, retained-release, rollback, or stacked-upgrade policy.

## Required future lock order

To avoid lock inversion, future promotion assessment/transactions must acquire boundaries in this
global order and release them in reverse:

1. lifecycle launch gate (prevents a new child/evidence worker from being queued);
2. updater transaction lock (excludes install, recovery, uninstall, and future promotion mutation);
3. evidence lock (after bounded evidence-worker quiescence).

Install and uninstall acquire only item 2. Evidence publication acquires only item 3. No code in this
slice acquires the future launch gate or combines the updater and evidence locks.
