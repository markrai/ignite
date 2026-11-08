# Kindle compatibility matrix

Ignite targets **Kindle Scribe** devices that have been prepared for third-party
applications. Non-Scribe Kindles and stock (non-jailbroken) devices are
**unsupported**. Prepare devices with the community jailbreak guide:
<https://kindlemodding.org/jailbreaking/>.

Evidence classes:

| Tag | Meaning |
|-----|---------|
| **Verified** | Observed on developer hardware with logs / Diagnose |
| **Tester** | Confirmed by an external tester report (identity omitted) |
| **Reported** | Failure or success reported without full artifacts |
| **Untested** | No claim |

## Device / firmware

| Kindle | Firmware | Ignite | Install | Ink / polarity | Class | Notes |
|--------|----------|--------|---------|----------------|-------|-------|
| Scribe (1st gen / 2022) | 5.18.6 | **0.9.11** | Fresh install/migrate + idempotent reinstall on device | `grayscale=2`, identity polarity | **Verified** | Namespace `com.markrai.ignite`; Scriptlet SHA-256 `7c71b74887ffa546febacb9ff361f8c2cfd2719f8b4dd1d4083d963fd1f0e1e0` |
| Scribe (2024) | 5.19.6 | 0.9.11 candidate | Intended | `grayscale=1`, complement polarity | **Tester** / in progress | Polarity fix from 0.9.10; confirm identity migration + polarity on device |
| Scribe (1st gen / 2022) | 5.19.6 | 0.9.14+ diagnostic | Install reported; Gate B previously stopped on `rotate=1` | `grayscale=1`, `rotate=1` reported with same portrait geometry as verified `rotate=3` | **Reported** | Gate B accepts `rotate=1`; touch/stylus apply a unit-tested 180° dual-axis reverse relative to proven 5.18.6 `rotate=3`. Project hardware is 5.18.6 only — screen uprightness, hit targets, and ink are **not** physically verified on 5.19.6 |
| Other Kindle models | — | — | — | — | **Untested** | Not a goal of current packages |
| Stock (no jailbreak) | any | — | Will refuse | — | **Verified** refusal | Missing root helper / preparation |

## Packaging / recovery transport (developer)

| Item | Status | Class |
|------|--------|-------|
| ARMv7hf `scribe-app` + `scribe-lifecycle` Docker build | Used for release packaging | **Verified** |
| Dropbear 2024.85 client packaging + digest | Built and digest-checked | **Verified** |
| Protected Dropbear install on device | Device install path exercised | **Verified** |
| Synthetic HTTP over Dropbear `-B` | Host + device path exercised | **Verified** |
| `SupervisedRecoveryStream` on device | Supervisor stream exercised | **Verified** |
| Customer-facing recovery enablement | Disabled | Intentional |

## Sync protocol / Change Paper (0.9.23)

Search-only 0.9.23 is compatible with 0.9.22. **Change Paper** introduces
`SetPageBackground`. A v0.9.22 (or earlier) peer that pulls a tail containing
that op fails closed at envelope parse and stalls until upgraded. There is no
skip-unknown behavior and no byte migration of existing notebooks.

- **Activation (default off):** `scribe-sync-server --accept-set-page-background`
  or product `accept_set_page_background = true` in `/etc/scribe-server/config.toml`.
  Do not enable until every Kindle, Web, and Server replica for that library is
  ≥0.9.23. New submits are refused with `unsupported_operation` while the gate is
  off; retries of an already-accepted envelope still dedup if the gate is later
  disabled. `GET /v1/libraries/{id}` advertises `accept_set_page_background: true`
  only when the gate is on (omitted while off, so v0.9.22 Web deny-unknown poll
  keeps working).
- **Authoring contract:** Kindle (when sync is on) and Web probe that
  advertisement before journaling. Kindle runs the GET off the interactive UI
  thread (production HTTP deadline is 60s) and discards the result if the
  paired server, library, or session changed. Web requires the probe on every
  path that can queue `SetPageBackground`. If the server is unactivated,
  unreachable, or an older binary that omits the field, Change Paper is refused
  and **no local paper change is applied** (nothing is discarded because nothing
  was committed). Kindle with sync completely off may still change paper locally.
  A mid-run disable after a confirmed-on start does not roll back already-committed
  pages.
- **OTA 0.9.22 → 0.9.23:** existing notebooks stay byte-stable; crash mid
  `install_background` uses the existing identity-clear recovery; an interrupted
  all-pages run leaves the already-applied prefix durable and is safe to retry.
- **Offline Kindle (sync on):** Change Paper is refused until the library head
  can be read and the gate is on. Duplicate delivery of accepted ops is
  last-write-wins / idempotent.
- **Web:** patterned Change paper requires online blob PUT; Blank still requires
  the activation probe (no silent local journal against an unactivated server).
  There is no offline Web blob queue.

### 0.9.23 acceptance (planning matrix)

| Gate | Notes |
|------|--------|
| scribe-app lib host tests | Home search parked query + background sync + Change Paper UI |
| ARMv7 build | Packaging path unchanged |
| Physical Kindle | Search `per`; Change Paper this/all pages; ink intact; Back after search/folder/notebook |
| Web tests | Template generate parity + Change paper this/all + blob PUT order |
| Server/sync | Default-off `accept_set_page_background` + blob gate + reducer LWW |
| Restart / interrupted write | Prefix of notebook-wide Change Paper stays applied |
| Kindle ↔ Server ↔ Web | Background replacement both directions |
| Untouched notebooks | Byte-stable |
| OTA 0.9.22 → 0.9.23 | No protocol migration |
| Web Software Updates UI | Out of scope |

## How to extend this matrix

Open an issue or PR with: device generation, firmware, Ignite version, install
outcome, Diagnose excerpt (redacted), and evidence class. Do not publish
identifiable tester contact details unless they opt in.
