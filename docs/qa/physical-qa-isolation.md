# Physical QA isolation — feasibility (design only)

**Verdict:** a second appmgrd registration is **not proven safe** on Kindle
firmware 5.18.6. Do not implement, register, install, or switch a QA profile
until a dedicated device or a proven sequential dual-registration rehearsal
exists. The smallest currently supported isolation is a **designated physical
test Kindle** whose entire `/mnt/us/scribe-data` is disposable.

This document is investigation only. No handler was registered.

## What the live production slot actually is

Observed on the connected first-generation Scribe (read-only, 2026-11-08) and
grounded in tree:

| Item | Evidence |
|---|---|
| Launch | `deployment/kindle/scribe-lifecycle-host.sh` execs `scribe-lifecycle --host --service com.markrai.ignite --child /mnt/us/scribe-app/bin/scribe-app -- --confirm-write --data-dir /mnt/us/scribe-data --sync-dir /mnt/us/scribe-app/state/sync --device-dir /mnt/us/scribe-app/state/device` |
| Library Scriptlet | `tools/kindle_launcher/Scribe.sh` is discovery-only (`lipc-set-prop com.lab126.appmgrd start app://com.markrai.ignite`). Direct `scribe-app` exec is rejected (F10.6). |
| Singleton | `scribe_app::singleton::DEFAULT_LOCK_PATH` = `/tmp/scribe-app.lock`, hardcoded in `main.rs`. Not per library. |
| Pairing | `/mnt/us/scribe-app/state/device/{device.json,server-profile.json}` |
| Replica | `/mnt/us/scribe-app/state/sync/replica.json` |
| Customer data | `/mnt/us/scribe-data` (25 notebooks on the inspected device) |
| Installer | `install-runtime.sh` never deletes notebooks; it still replaces the **one** live binary slot that opens those notebooks |
| Rollback of executables | `previous-release` is executable-only. Switching A/B is not a notebook rollback (`docs/kindle-data-compatibility-contract.md` §1.1) |

`tools/qa/ignite_qa.py preflight` refuses `DEVICE_PREFLIGHT_PASS` on this layout.

## Could appmgrd host a second Ignite-class app?

### Suggestive, not sufficient

- `appreg.db` already allowed two Ignite handlerIds during the 0.9.11 migration
  (`com.markrai.scribe` + `com.markrai.ignite`). `register-helpers.sh` snapshots
  and deletes **by handlerId**, so a third row is *structurally* possible.
- `scribe-lifecycle --host --service <lipcId>` is parameterized.
  `LipcOpenEx(<lipcId>)` is the KOReader-proven 5.18.6 protocol.
- Registration JSON binds `handlerId` == `lipcId` == `com.markrai.ignite`
  (`tools/kindle_app/registration/ignite.json`). A QA id such as
  `com.markrai.ignite.qa` would need its **own** host script, Scriptlet URI, and
  `--data-dir/--sync-dir/--device-dir` that never mention production paths.
- `unloadPolicy: unloadOnPause` plus observed Home switching
  (`pause` → SIGTERM child → `unload` on 5.18.6) means appmgrd serializes
  foreground apps. Two Ignite UIs should not paint at once if that protocol
  holds for a second handler.

### Blockers (why this is not implemented)

1. **No 5.18.6 evidence** that two framebuffer + stylus apps with distinct
   `lipcId`s load, pause, and unload cleanly. The legacy+canonical pair was a
   migration of **one** product, not two selectable notebooks.
2. **Customer registration helpers refuse a non-canonical handlerId**
   (`registration json handlerId must be com.markrai.ignite`). A QA slot cannot
   ship in `Install Ignite` without changing production packaging (explicitly
   out of scope: QA must be excluded from customer artifacts).
3. **`/tmp/scribe-app.lock` is global.** A QA binary that shares the current
   `scribe-app` would collide with production if overlap occurred, and would
   also collide with any leaked singleton. A QA build would need a distinct
   lock path **and** tests that writer-exclusion still covers both. That is a
   product change, not a config switch.
4. **Updater lock** `/tmp/ignite-updater-transaction.lock` is similarly global.
5. **Host path hardcoding is an installer verify requirement**
   (`install-runtime.sh` greps the installed host for
   `/mnt/us/scribe-app/state/sync` and `state/device`). A QA host that points
   elsewhere must never replace that file.
6. **Direct SSH `scribe-app --data-dir …` is a rejected architecture** (F10.6)
   and bypasses lifecycle health, appmgrd, and writer-exclusion. It is not a
   fallback.
7. **Single-slot “QA profile switch”** (repoint live host at empty dirs, then
   restore) mutates launcher state, pairing, and replica identity. There is no
   tested atomic swap, and `previous-release` does not snapshot notebooks.
   Unsafe until those rollback requirements exist.

## Preferred design *if* dual-registration is later proven

Out of customer Scriptlet. Separate tree `/mnt/us/scribe-qa/` with its own
`bin/scribe-lifecycle-host.sh` (QA `--service`, QA `--data-dir`, QA
`--sync-dir`, QA `--device-dir`), Scriptlet `Ignite QA.sh` →
`app://com.markrai.ignite.qa`, lock path distinct from production, refuse to
start if production `scribe-app` holds the framebuffer or if any path resolves
to `/mnt/us/scribe-data` or production `state/`. Pair only to a disposable
non-Vega server with `accept_set_page_background = true` and
`discovery.advertise = false`. Unregister must delete only the QA handlerId and
QA tree.

**Not proven. Do not build it in this change.**

## Smallest supported isolation today

Use a **designated disposable Kindle** (already required by
`IGNITE_SHIP_KINDLE_DEVICE_ID` / `ship_lib.hardware`). Entire library is QA.
Production Scribe with 25 notebooks stays untouched.

A controlled reversible single-slot profile on the production device is
**rejected** until executable *and* data rollback are specified and tested.

## Manual steps that remain after automation

- Authorize and perform any Kindle install (never automatic).
- Provide a disposable test device or, later, a proven QA handler.
- Pair that device to a non-Vega server; do not enable Vega’s Change Paper gate.
- Execute the physical script (search/Back, Change Paper, cancel, restart, sync).
- File human `physical-qa-pass.json` with matching artifact hashes.
- Publication / OTA (`ignite_ship.py`, `promote-release.sh`).
