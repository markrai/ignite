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

## How to extend this matrix

Open an issue or PR with: device generation, firmware, Ignite version, install
outcome, Diagnose excerpt (redacted), and evidence class. Do not publish
identifiable tester contact details unless they opt in.
