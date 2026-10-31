# Kindle installer publication and crash-recovery rehearsal (Slice 3D-B2a.4.3e)

**Status:** observational disposable-root rehearsal of the existing installer.
Not production trusted publication. Not automatic installation, promotion,
rollback, or first-launch admission.

Related: [`kindle-installed-slot-binding.md`](kindle-installed-slot-binding.md),
[`kindle-release-component-profiles.md`](kindle-release-component-profiles.md),
[`kindle-authenticated-publication-evidence.md`](kindle-authenticated-publication-evidence.md),
[`kindle-pending-health-and-lkg.md`](kindle-pending-health-and-lkg.md) (if present).

## 1. Source-verified installer operation sequence

Entry: customer Scriptlet extracts `Ignite-Kindle-<version>/`, then runs
`lib/install-runtime.sh "$PACKAGE_ROOT"`.

Path overrides used by the rehearsal (and existing unit tests):

| Symbolic | Env override | Production default |
|----------|--------------|--------------------|
| `APP` | `IGNITE_APP_ROOT` | `/mnt/us/scribe-app` |
| `DOCS` | `IGNITE_DOCS` | `/mnt/us/documents` |
| `DATA` | `IGNITE_DATA_DIR` | `/mnt/us/scribe-data` |
| `REC` | `IGNITE_RECOVERY_ROOT` | `/var/local/ignite-recovery` |

### Operation map (`verified payload → … → pending-health`)

| Step | Script / function | Kind | Atomic? | Interruptible |
|------|-------------------|------|---------|---------------|
| Recover prior journal | `recover_install_transaction` | restore/rename | per-op | yes |
| Guard existing pending-health | `guard_pending_health` | read/refuse | n/a | n/a |
| Clear stale `*.new` | `cleanup_staged_new` | delete | no | yes |
| Retain live A → `previous-release` | `snapshot_previous_release` / `prepare_existing_snapshot` | copy + `mv` dir | dir `mv` only | **yes** (`IGNITE_FAIL_PUBLISH`) |
| Stage B as `*.new` | main body `cp -f` | copy | no | yes |
| Activate B (`mv` into place) | `activate_mv` / `activate_mv_optional` | rename | per-file `mv` | **yes** (`IGNITE_FAIL_AT`) |
| Register / migrate handler | `ignite_register_and_migrate` | transform (appreg) | SQL txn | yes |
| Retain handler SQL | `retain_lkg_handler_snapshot` | copy+`mv` | file `mv` | yes |
| Install recovery tree | `ignite_install_recovery` | copy/generate (`REC/manifest/version`) | mixed | yes |
| Publish LKG inventory | `publish_lkg_inventory` | generate+`mv` | file `mv` | **yes** |
| Publish pending-health | `publish_pending_health` | generate+`mv` | file `mv` | **yes** |
| Durable commit marker | `commit_install_transaction` | write+`mv` | file `mv` | **yes** |
| Journal / `.prev` cleanup | main body | delete | no | yes |

**Not atomic across APP/DOCS/REC.** A process kill between activation of some
`*.new` files and others yields a partial B tree until journal recovery runs.

**Outside release ownership:** `scribe-data/` notebooks, sync/device state under
`APP/state/`, unrelated appreg rows, user Documents files other than Ignite
Scriptlets.

## 2. Rehearsal architecture

Harness: `deployment/kindle/installer_publication_rehearsal.py`  
Tests: `deployment/kindle/test_installer_publication_rehearsal.py`

- Exclusive disposable root owned by an in-memory `FixtureRoot` (create-time
  token + device/inode). The on-disk `.ignite-rehearsal-owner` marker encodes the
  same identity; a copied marker on a substituted directory never authorizes
  cleanup.
- Real `install-runtime.sh` via existing `_run_install` / `IGNITE_*` overrides.
- Independent A and B package evidence from `release_component_profiles`.
- Post-condition observation of all 22 roles at symbolic destinations via
  descriptor-based hashing (`O_NOFOLLOW`, hardlink refusal, identity checks).
- Pending-health: **structurally-valid** (exact 14-line contract) is separated from
  **consistent** (LKG inventory exists and matches, live app/lifecycle digests
  match, candidate version equals expected B, independently verifiable retained-A
  inventory entries agree). `appreg-handler.sql` is not package-provable when
  register is skipped.
- Complete B publication also requires Unix execute bits on installer-chmod'd
  executables (skipped on native Windows fixtures) and absence of every closed
  installer-owned `.new`/`.tmp` transition path (not only four bin/docs paths).
- External sentinel files for notebooks/sync/settings/user docs.
- Cleanup refuses substituted roots, copied markers, and token/identity mismatch.

### Real versus modeled

| Behavior | Classification |
|----------|----------------|
| Payload staging, activation `mv`, previous-release snapshot, LKG inventory, pending-health, journal recovery | **Real** installer under disposable roots |
| Crash injection via `IGNITE_FAIL_PUBLISH` / `IGNITE_FAIL_AT` | **Real** installer test hooks (same as existing suite) |
| Component digest observation / publication_state classification | **Observer** (not installer) |
| Skipped register/su/elf/smoke by default in harness env | **Test override** (matches `test_install_runtime.py`) |
| Power-loss / missing `fsync` durability of directory entries | **Unresolved gap** — `sync` is called, but durable power-fail proof is not claimed |

## 3. A/B identity separation

- Release A and B are separately built payloads with distinguishable bytes.
- Expected digests come from each release’s own package profile observation.
- Retained `APP/previous-release/*` must `match-a`; live candidate destinations must `match-b`.
- Swapping B bytes into retained A is reported as `match-b` on a retained row (failure of A identity).
- Shared recovery roles are classified `shared` / `not-applicable` under retained-A.
- `recovery-version` remains `generated` (not package direct-copy).
- `registration-source` remains `transformed` (SQL snapshot when register enabled).

## 4. Observable publication states

Adapted to `install-runtime.sh`, not an invented updater FSM:

1. `no-active-release` (empty disposable fixture; no stable app)
2. `a-active-no-pending` (first-install complete vs A evidence; no pending-health)
3. `b-staging-present` (any closed installer-owned `.new`/publication `.tmp`)
4. `a-retention-incomplete` (`previous-release.staging` / `.prev`)
5. `b-publication-partial` (journal or mixed digests)
6. `b-present-without-pending-health`
7. `b-published-with-pending-health` (complete B + retained A + **consistent** pending-health)
8. `incomplete-or-inconsistent` (malformed or structurally-valid-but-inconsistent pending-health, wrong destinations, etc.)

### LKG inventory binding (installer-authoritative)

`APP/previous-release/ignite-lkg-inventory.v1`:

```text
IGNITE_LKG_INVENTORY_V1
schema=1
kind=ignite-application-release
entry_count=18
entry=<name>|file|<bytes>|<sha256>   # or |absent|0|-
```

Fixed 18-entry order from `write_lkg_inventory`. Pending-health
`lkg_inventory_sha256` / `retained_release_identity` must equal that file’s digest;
regenerating the inventory from `previous-release` must reproduce it.

Mandatory B completeness does not ignore optional components expected from B
evidence. Missing or altered optionals block `b-published-with-pending-health`.
Malformed, truncated, mismatched, or symlinked pending-health records never
produce that state. Structurally valid but LKG-inconsistent records never do either.
Consistent pending-health **never implies qualification**.

Pending-health **presence is not qualification**. Reports force:

```text
publisher_authentication=unavailable
installation_binding=unverified
execution_binding=unverified
first_launch_admission=unavailable
data_compatibility=unknown
promotion_authorized=false
```

## 5. Interruption points and crash recovery

Installer hooks (source-verified):

- Publish: `before-prev-move` … `before-prev-cleanup` (`publish_crash` / `IGNITE_FAIL_PUBLISH`)
- Activation: per-file `IGNITE_FAIL_AT` boundaries (`scribe-app`, `host`, docs helpers, …)

Existing `test_install_runtime.py` already proves many recovery behaviors. This
slice’s rehearsal **observes** the resulting tree and confirms external sentinels
are unchanged. It does **not** claim power-loss durability from `mv` alone.

### Durability gaps (explicit)

- No proven `fsync` of parent directories after every rename on all Kindle filesystems.
- Multi-directory publication (APP + DOCS + REC) cannot be one atomic transaction.
- Abrupt kill ≠ battery pull; do not equate process-exit tests with power-fail safety.

## 6. Authentication limitations

- Synthetic disposable fixtures ≠ publisher authentication.
- Component profiles ≠ trusted device authority.
- SFX equivalence ≠ installed binding.
- Matching installed bytes ≠ proof of which inode later `exec`s.
- Production Ed25519 pin remains `UNPROVISIONED`.

## 7. Prerequisites for production trusted publication

1. Provisioned publisher pin + signed installer / component closure.
2. Durable, authenticated publication of component digests into live-B / retained-A / REC.
3. Kernel-backed execution binding.
4. Bidirectional data compatibility before mutating launch.
5. Proven recovery policy for every incomplete multi-filesystem state.
6. Explicit launch exclusion until qualification — still unavailable here.

Until then, rehearsal remains report-only and non-authorizing.
