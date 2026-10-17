# Contributing to Ignite

Thank you for helping improve Ignite, an open-source Kindle Scribe notebook
replacement with optional self-hosted sync.

## Development environment

- **Rust**: stable toolchain (`rustup`), matching `Cargo.toml` edition 2021
- **Docker** (optional): for Kindle ARMv7hf cross builds (`rust:1-bullseye` +
  `gcc-arm-linux-gnueabihf`)
- **Python 3** + **pytest**: Kindle installer / Scriptlet unit tests
- **Git**: clone this repository; no private services required for default tests

Host unit tests do **not** need a Kindle, jailbreak, or production credentials.

## Repository structure

| Path | Contents |
|------|----------|
| `ignite/core/` | Shared crates (`notebook-core`, `sync-protocol`, `sync-engine`, …) |
| `ignite/kindle/` | Kindle runtime (`scribe-app`, `kindle-platform`, …) |
| `ignite/server/` | Self-hosted sync / control servers |
| `ignite/web/` | ignite-web (Vite/React browser replica) |
| `ignite/core/scribe-web-wasm/` | WASM protocol bridge for ignite-web |
| `ignite/tools/scribe-sync-tool/` | Sync inspection + web fixture generation |
| `ignite/assets/` | Fonts and third-party notices |
| `deployment/kindle/` | Installer scripts, SFX builder, pytest suite |
| `scripts/kindle/` | Release packaging (`package-release.ps1`) |
| `tools/` | Cross-build, Dropbear client build, launcher helpers |
| `docs/` | Architecture, troubleshooting, compatibility |

## Build and test (host)

```bash
cargo test --workspace --locked
cargo test -p scribe-app --lib --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked
```

Installer tests (from repo root):

```bash
cd deployment/kindle && pytest -q
cd tools/kindle_launcher && pytest -q
```

## Kindle ARMv7hf binaries

```bash
docker run --rm -v "$(pwd):/src" -w /src rust:1-bullseye \
  bash /src/tools/docker_build_armv7hf.sh
```

Outputs land under `target/armv7-unknown-linux-gnueabihf/release/`
(`scribe-app`, `scribe-lifecycle`, `ink-probe`).

Customer Scriptlet packaging (Windows packaging host + Docker):

```powershell
.\scripts\kindle\package-release.ps1
```

Requires a verified Dropbear client under `target/dropbear-kindle/`
(see `tools/dropbear/`).

## Coding and testing expectations

- Prefer small, reviewable changes with tests for behavioral fixes.
- Do not enable experimental recovery (`LIVE_NON_LOOPBACK_POST_ENABLED` must
  remain `false`).
- Do not package `kindle-supervisor-harness` into the customer Scriptlet.
- Do not commit secrets, personal notebooks, device dumps, or real credentials
  (see `.gitignore`).
- Keep Kindle-facing shell scripts LF line endings.

## Pull requests

1. Fork / branch from `main`.
2. Run the host tests above (and installer pytest if you touch `deployment/`).
3. Describe user-visible impact and test evidence.
4. Link firmware / device reports when claiming Kindle compatibility changes.

## Reporting compatibility issues

Include Kindle generation, firmware version, Ignite version, install outcome,
and whether Diagnose Ignite / `Ignite-Diagnose.txt` is available. Prefer the
issue tracker over private channels for non-security problems.

## License

Contributions are accepted under **AGPL-3.0-only** (see `LICENSE` and `NOTICE`).
