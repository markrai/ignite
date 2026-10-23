# Contributing to Ignite

Thank you for helping improve Ignite, an open-source Kindle Scribe notebook
replacement with optional self-hosted sync.

## Development environment

- **Rust**: stable toolchain (`rustup`), matching `Cargo.toml` edition 2021
- **Docker** (optional): for Kindle ARMv7hf cross builds and the pinned
  Dropbear client (`rust:1-bullseye` + `gcc-arm-linux-gnueabihf`; see
  `tools/pinned-docker-images.txt`)
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
| `deployment/linux/` | Ignite Server portable Linux bundle + staging tests |
| `scripts/kindle/` | Thin packaging wrappers (`package-release.sh` / `.ps1`) |
| `tools/` | Cross-build, Dropbear client, launcher, promote-publish |
| `docs/` | Architecture, troubleshooting, compatibility, self-hosting |

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

Official Kindle packaging (Linux / CI; digest-pinned Docker images, no SkipBuild):

```bash
bash scripts/kindle/package-release.sh
```

Windows:

```powershell
.\scripts\kindle\package-release.ps1
```

`-SkipBuild` is not provenance-safe and must not be used for official artifacts.
Staging is shared in `deployment/kindle/package_release.py`. Dropbear ELF digest
and size must match `tools/dropbear/PINNED.txt`. Public GitHub Releases are
promoted from a hardware-tested CI run (exact bytes, no tag-push rebuild); see
`docs/release-packaging.md`.

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
