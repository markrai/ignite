# Ignite

**Ignite** is an open-source **Kindle Scribe notebook replacement**: local
handwriting, durable notebooks on-device, optional self-hosted sync, and a
single-file installer for prepared Kindles.

> Ignite requires a Kindle that is **already prepared** for third-party apps
> (jailbreak / community tooling, SH Integration, root helper). It does **not**
> jailbreak a stock Kindle and does **not** claim support for every Kindle
> generation. For jailbreaking, follow the community guide at
> [kindlemodding.org/jailbreaking](https://kindlemodding.org/jailbreaking/).

License: **AGPL-3.0-only** — see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).

## Features

- Multi-stroke notebook editing on Kindle Scribe e-ink
- On-device library, tools, and durable page storage
- Optional LAN sync with a self-hosted server
- One-file customer installer (`Install Ignite v*.sh`)
- Diagnose / backup / uninstall helpers

## Status

| Area | Status |
|------|--------|
| Kindle app + installer | Active development / beta |
| Self-hosted sync server | **Ignite Server** — portable Linux bundle (see [self-hosting](docs/self-hosting.md)) |
| Web companion | **ignite-web** in `ignite/web/` (browser replica + sync) |
| Experimental recovery | **Disabled** in product builds (see below) |

Version: see [`ignite/VERSION`](ignite/VERSION).

## Repository layout

This outer directory is a **development workspace**. The Ignite product lives
under the inner `ignite/` tree (including `ignite/web/`). An optional `scribe/`
directory may sit as a **sibling of `ignite/` inside this same workspace** for
local native-Scribe work; it is not part of the Ignite product tree and is not
moved or merged by Ignite tooling.

```text
C:\dev\project\ignite\          (outer workspace / this repository root)
  ignite\                       Ignite product
    core\                       Shared Rust crates (notebook, sync protocol/engine)
    kindle\                     Kindle runtime (scribe-app, platform, lifecycle)
    server\                     Self-hosted sync / control servers
    web\                        ignite-web (browser replica + PWA)
    assets\                     Fonts + third-party notices
  scribe\                       Optional local native-Scribe work (not Ignite product)
  deployment\kindle\            Installer, SFX, pytest
  deployment\linux\             Ignite Server portable Linux bundle + acceptance tests
  scripts\kindle\               Release packaging
  tools\                        Cross-build, Dropbear client, launcher
  docs\                         Architecture, troubleshooting, compatibility, self-hosting
```

## Self-hosted Ignite Server

Build and install a portable Linux bundle (Ignite Server + ignite-web) from source:

see [`docs/self-hosting.md`](docs/self-hosting.md). Trusted LAN / plain HTTP only.
Optional Linux Docker Compose / Portainer packaging (host networking) lives under
[`deployment/linux/docker/`](deployment/linux/docker/).


## Supported device expectations

See [`docs/compatibility.md`](docs/compatibility.md).

- **Verified:** Kindle Scribe (1st generation), firmware **5.18.6**, 32-bit ARM
- **In progress / tester:** 2024 Scribe polarity path (firmware **5.19.6**)

## Kindle installation (overview)

1. Prepare the Kindle for third-party apps using the
   [Kindle Modding jailbreak guide](https://kindlemodding.org/jailbreaking/).
2. Obtain `Install Ignite vVERSION.sh` (release artifact) and its `.sha256`.
3. Verify the checksum.
4. Copy the Scriptlet into the Kindle **Documents** folder over USB.
5. Open **Install Ignite** on the device.
6. Launch **Ignite** from the Library.

Application registration identity is **`com.markrai.ignite`** (from 0.9.11).
Upgrading from 0.9.10 migrates the older `com.markrai.scribe` handler automatically;
notebooks stay at `/mnt/us/scribe-data` and sync service discovery remains
`_scribe._tcp`.

Full steps: [`deployment/kindle/README.txt`](deployment/kindle/README.txt).  
Problems: [`docs/troubleshooting.md`](docs/troubleshooting.md).

## Build from source

### Host tools

- Recent stable Rust (`rustup`)
- Optional: Docker (Kindle ARMv7hf + Dropbear builds)
- Python 3 + pytest (installer tests)

### Rust workspace (no Kindle required)

```bash
cargo test --workspace --locked
cargo test -p scribe-app --lib --locked
```

### Kindle ARMv7hf binaries

```bash
docker run --rm -v "$(pwd):/src" -w /src rust:1-bullseye \
  bash /src/tools/docker_build_armv7hf.sh
```

### Customer Scriptlet (packaging host)

```powershell
# After Dropbear client build under target/dropbear-kindle/
.\scripts\kindle\package-release.ps1
```

Details: [`docs/release-packaging.md`](docs/release-packaging.md),
[`CONTRIBUTING.md`](CONTRIBUTING.md).

## Development setup

See [`CONTRIBUTING.md`](CONTRIBUTING.md). Default tests use disposable temp
dirs and loopback sockets only—no production credentials or device required.

## Architecture

Concise overview: [`docs/architecture.md`](docs/architecture.md).

## Experimental recovery

Ignite’s tree includes research/recovery modules and a verified protected
Dropbear transport path on Kindle hardware. **Publishing source does not
activate recovery.**

- `LIVE_NON_LOOPBACK_POST_ENABLED = false`
- Ordinary startup does not submit recovery operations
- No production SSH keys or host pins are bundled
- Test-only `kindle-supervisor-harness` is excluded from the customer Scriptlet

Real historical recovery remains a separate, controlled process—not a
prerequisite for using Ignite as a notebook.

## Known limitations

- Compatibility beyond verified Scribe firmware is incomplete
- Sync servers are plain HTTP for LAN/self-host use—do not expose casually
- Review 79 (performance investigation) remains open
- ignite-web Playwright E2E coverage has been run on Chromium; Firefox and
  WebKit/Safari E2E suites have not yet been executed in this migration, so
  compatibility on those browsers remains unverified
- Ignite Server portable Linux packaging is validated with disposable staging
  tests; optional Compose/Portainer image under `deployment/linux/docker/`

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). Security reports:
[`SECURITY.md`](SECURITY.md).

## Third-party notices

Fonts (SIL OFL), Dropbear (upstream permissive license, build-time fetch), and
Rust crate licenses: [`ignite/assets/THIRD_PARTY_NOTICES.md`](ignite/assets/THIRD_PARTY_NOTICES.md).

## License

[GNU Affero General Public License v3.0 only](LICENSE) (AGPL-3.0-only).
