# Kindle installation authorization gate (Slice 3D-B2a.4.3j)

**Status:** read-only authorization diagnostic only.
Does **not** install, execute a Scriptlet, activate A/B slots, restart services,
write an authorization token, or schedule automatic updates.

Related: [`kindle-verified-update-staging.md`](kindle-verified-update-staging.md),
[`kindle-production-trust-integration.md`](kindle-production-trust-integration.md),
[`kindle-installed-slot-binding.md`](kindle-installed-slot-binding.md),
[`kindle-data-compatibility-contract.md`](kindle-data-compatibility-contract.md),
[`kindle-first-launch-admission.md`](kindle-first-launch-admission.md).

## 1. Purpose

A completed 3i `STAGING_RECORD` is an **observation hint**, not installation
authority. Slice 3j evaluates whether a staged candidate is eligible for
installation by re-establishing trust and local prerequisites from current
inputs.

The shipping diagnostic is:

```text
scribe-app --authorize-verified-update --expected-tag vX.Y.Z \
  [--staging-root /mnt/us/scribe-app/state/verified-update]
```

It always uses the official production pin. There is no fixture-key bypass and
no environment override. Fixture staging roots are refused by the CLI parser.

A canonical, fully completed 3i `STAGING_RECORD` must exist and agree on release
tag and recorded provenance with the authorization context. The record remains
an observation only — signed assets and the Scriptlet are still revalidated
independently afterward.

Exit status is nonzero whenever `installation_authorized=false`.

## 2. Decision model

| Field | Meaning |
|-------|---------|
| `candidate_authenticated` | Official publication chain verifies under the production pin |
| `publication_claims_validated` | Production evidence claims validate |
| `artifact_identity_bound` | Staged Scriptlet digest/bytes match the authenticated release |
| `device_compatible` | Architecture/firmware match the verified Scribe matrix |
| `upgrade_compatible` | Version transition / installed-state upgrade support |
| `recovery_ready` | A/B recovery, rollback, and state-protection readiness |
| `data_compatibility_verified` | Notebooks/library/sync protected by a trusted preflight |
| `execution_binding` | Installer can consume the same authenticated artifact without substitution |
| `installation_policy_satisfied` | Separate user/unattended policy approval via a trusted mechanism |
| `installation_authorized` | Conjunction of all gates + official provenance |

Each prerequisite is `satisfied`, `refused`, or `not_implemented`.
`installation_authorized=true` only when **every** gate is `satisfied` and
provenance is `verified-under-production-pin`.

Fixture provenance can never authorize production installation.

## 3. Reused versus unavailable prerequisites

| Requirement | Status in 3j | Source |
|-------------|--------------|--------|
| Publisher authority | **Refused** while pin is `UNPROVISIONED`; otherwise reuses official chain | `authenticate_official_publication_chain` |
| Release / artifact identity | **Evaluated** after auth via `verify_installer` | release-v1 + staged Scriptlet |
| Publication claims | **Evaluated** from evidence claim status | 3h production evidence |
| Device compatibility | **Partial** — exact `armv7l` + firmware `5.18.6` when a positively parsed version is available; generic `arm` refused; otherwise refused/not_implemented | `uname -m` + `/etc/prettyversion.txt` / `/etc/version.txt` parser (hardware sample pending) |
| Upgrade compatibility | **not_implemented** | future transition gate |
| Recovery readiness | **not_implemented** | `install-recovery.sh` / previous-release exist but are not an auth gate |
| Data safety | **refused** (`data_compatibility=unknown`) | observational preflight only |
| Execution binding | **not_implemented** | no verified-to-execution handoff yet |
| Installation policy | **not_implemented** | no trusted approval mechanism |

Unsupported or unknown prerequisites **block** authorization.

## 4. Trust provenance

- Shipping CLI → official pin only.
- Host unit tests may exercise fixture evaluation through a `pub(crate)` seam;
  fixture success never sets `installation_authorized=true`.
- A `cfg(test)`-only harness can demonstrate the all-gates conjunction. It is
  not linked to the Kindle CLI and cannot bypass the pin in shipping builds.

## 5. Execution-binding boundary (future handoff)

Verification-to-use substitution risks remain open until the installer:

1. Receives the **exact** staged Scriptlet path (or open fd) already digest-bound
   by the authorization evaluation.
2. Re-hashes or keeps the fd through Scriptlet launch without an intervening
   rename/replace of the bytes.
3. Refuses if the staging root, release tag, or local installation state changed
   after the diagnostic.
4. Treats a prior `installation_authorized=true` report as **non-authoritative**
   — the execution boundary must repeat or cryptographically bind the checks.

3j does **not** claim execution binding is satisfied.

## 6. Refusal semantics

- Absent, malformed, oversized, incomplete, or tag/provenance-mismatched
  `STAGING_RECORD` → refuse authorization (the record is still not a credential;
  signed assets are revalidated only after a valid completed observation).
- Symlinked or non-directory staged `assets/` → refuse before opening publication
  assets (path checks alone are not race-free against concurrent substitution).
- Missing/modified/truncated/unreadable staged Scriptlet →
  `artifact_identity_bound=refused` while an otherwise valid publication chain may
  still report `candidate_authenticated=satisfied` and
  `publication_claims_validated=satisfied`.
- Invalid publication signatures → `candidate_authenticated=refused`; dependent
  claims and artifact gates refuse because they cannot be established.
- Fixture provenance → final production authorization always refused.
- Production pin `UNPROVISIONED` → final production authorization always refused
  (`candidate_authenticated=refused` when official auth is reached).
- Wrong expected tag / release identity → refuse.
- Unsupported architecture/firmware → `device_compatible=refused`.
- Interrupted `.next`/`.prev` staging → refuse.
- Incomplete local gates → `installation_authorized=false`.

No combination of partial diagnostic successes authorizes installation.

A previously favorable diagnostic is not a capability. Re-run after any candidate
or device-state change.

## 7. Kindle expectations

While the pin remains `UNPROVISIONED`, on-device authorization must report
`installation_authorized=false` and leave live binaries, notebooks, sync data,
and recovery state unmodified.
