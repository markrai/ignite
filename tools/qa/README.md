# Ignite release qualification (`tools/qa`)

Reusable, deterministic qualification for **every** Ignite version. It composes
existing CI commands and Kindle diagnostics. It does not install toolchains,
mutate a Kindle, enable Vega, publish, tag, or commit.

## Commands

```text
python tools/qa/ignite_qa.py qualify [--groups identity,rust,armv7,wasm,web,e2e]
python tools/qa/ignite_qa.py preflight [--ssh-host kindle] [--qualify-json target/qa-evidence/qualify.json]
python tools/qa/ignite_qa.py status --qualify-json … [--preflight-json …] [--physical-json …]
```

Evidence lands in `target/qa-evidence/` (`qualify.json`, `preflight.json`, `*.txt`)
and in a timestamped subdirectory so a later run cannot clobber a failure.
Each qualify report records the exact commands (`run.argv`), log paths, artifact
hashes, toolchain identity, and a `source.tree_fingerprint` of HEAD, tracked
diffs, and **contents** of untracked source files (not filenames alone).
Generated `ignite/web/src/wasm`, `ignite/web/dist`, and `target/` are excluded
from the fingerprint and identified separately via artifact SHA-256.
Combine evidence from two hosts only when those fingerprints match; prefer one
Windows `qualify --groups all` invocation so WSL Rust/WASM, Docker ARMv7, and
Windows web/E2E share one fingerprint.
`status` refuses `RELEASE_CANDIDATE` when the live tree fingerprint disagrees,
the working tree is dirty, preflight/physical evidence is stale or mismatched,
physical QA does not cite the qualified `scribe-app` SHA-256, or
`data_compatibility` is missing/unknown/incompatible.

| Group | Existing tool | Missing toolchain |
|---|---|---|
| `identity` | `git`, `tools/ci/check_version_sync.py` | FAIL if git/version files broken |
| `rust` | Linux `cargo test --workspace --locked` and `cargo test -p scribe-app --lib --locked` | `NOT_RUN` (Windows cargo is not Unix runtime qualification) |
| `armv7` | `tools/docker_build_armv7hf.sh` via the digest-pinned image in `tools/pinned-docker-images.txt` | `NOT_RUN` without Docker |
| `wasm` | `node ignite/web/scripts/build-wasm.mjs` | `NOT_RUN` without `wasm32-unknown-unknown` (never `rustup target add`) |
| `web` | `npx tsc --noEmit`, `npm test`, `npx vite build` | `NOT_RUN` without `ignite/web/node_modules` (never `npm ci`) |
| `e2e` | `npx playwright test --project=chromium` | `NOT_RUN` without `dist/` or Chromium |

Selected groups that are `FAIL` exit `1`. Selected groups that are `NOT_RUN` exit `2`.
`preflight` on a production-bound Kindle exits `3` and prints `PHYSICAL_QA_ISOLATION_BLOCKED`.

The WSL/Linux Rust + Windows Node split is detected automatically. The runner
will not treat a Windows-only `cargo test` as Linux/Kindle qualification.

## Release states

`status` (and every `qualify` report) evaluates:

`SOURCE_READY → AUTOMATED_VERIFIED → ARTIFACT_VERIFIED → DEVICE_PREFLIGHT_PASS → PHYSICAL_QA_PASS → RELEASE_CANDIDATE → PUBLISHED → OTA_VERIFIED`

Progression is blocked when required groups did not `PASS`, artifact hashes
differ from those tested, the working tree is dirty, the Kindle has no isolated
QA slot, physical QA evidence is missing, or a compatibility gate is unresolved.

The CLI never sets `PUBLISHED` or `OTA_VERIFIED`. Those remain human-approved
promote / OTA steps (`tools/release/ignite_ship.py`, `promote-release.sh`).

Physical install, Vega/Change Paper gate activation, and publication stay
manual.

## Kindle preflight

`preflight` uses the existing SSH helper (`ship_lib.hardware.run_ssh_script`)
and Diagnose-style read-only queries. It **does not** exec `scribe-app`,
`--diagnose-fb`, `--help`, or `--confirm-write`. It never prints pairing
tokens or `server-profile.json` contents.

It **always refuses** physical QA while the live host is bound to
`/mnt/us/scribe-data` + production `state/sync` + `state/device`. Disposable
notebooks inside that library are not isolation.

## Tests

```text
python tools/qa/test_ignite_qa.py
```
