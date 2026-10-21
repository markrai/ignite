![logo](logo.jpg)

an open-source **Kindle Scribe notebook replacement**: local handwriting, durable notebooks on-device,   
optional self-hosted sync, and a single-file installer for prepared Kindles.

## Table of Contents

- [Self-host Ignite](#self-host-ignite)
  - [Quick Start: Linux Server](#quick-start-linux-server)
    - [1. Download and install](#1-download-and-install)
    - [2. Open Ignite Web](#2-open-ignite-web)
    - [3. Verify the server](#3-verify-the-server)
    - [4. Keep Ignite updated](#4-keep-ignite-updated)
  - [Prefer Docker or Portainer?](#prefer-docker-or-portainer)
  - [Connect your Kindle Scribe](#connect-your-kindle-scribe)
  - [Security and network access](#security-and-network-access)
- [Features](#features)
- [Status](#status)
- [Repository layout](#repository-layout)
- [Self-hosted Ignite Server](#self-hosted-ignite-server)
- [Supported device expectations](#supported-device-expectations)
- [Kindle installation (overview)](#kindle-installation-overview)
- [Build from source](#build-from-source)
  - [Host tools](#host-tools)
  - [Rust workspace (no Kindle required)](#rust-workspace-no-kindle-required)
  - [Kindle ARMv7hf binaries](#kindle-armv7hf-binaries)
  - [Customer Scriptlet (packaging host)](#customer-scriptlet-packaging-host)
- [Development setup](#development-setup)
- [Architecture](#architecture)
- [Experimental recovery](#experimental-recovery)
- [Known limitations](#known-limitations)
- [Contributing](#contributing)
- [Third-party notices](#third-party-notices)
- [License](#license)  


> Ignite requires a Kindle that is **already prepared** for third-party apps  
> (jailbreak / community tooling, SH Integration, root helper). It does **not**  
> jailbreak a stock Kindle and does **not** claim support for every Kindle  
> generation. For jailbreaking, follow the community guide at  
> [kindlemodding.org/jailbreaking](https://kindlemodding.org/jailbreaking/).

# Self-host Ignite

Ignite is an open-source Kindle Scribe notebook replacement with an optional self-hosted server and Web companion.

Your notebooks can live on the Kindle itself, while **Ignite Server** lets you sync notebooks between your Kindle and a browser on your local network.

## Quick Start: Linux Server

**Requirements:** Linux x86_64, systemd, `curl`, `tar`, `sha256sum`, and sudo access. No Rust, Node.js, Git or Docker required.

### 1. Download and install

Run these commands on your Linux server:

```bash
mkdir -p ~/ignite-install && cd ~/ignite-install

VERSION=0.9.13
ASSET="ignite-server-${VERSION}-linux-x86_64.tar.gz"
URL="https://github.com/markrai/ignite/releases/download/v${VERSION}"

curl -fLO "$URL/$ASSET"
curl -fLO "$URL/$ASSET.sha256"

sha256sum -c "$ASSET.sha256"

tar -xzf "$ASSET"
cd "ignite-server-${VERSION}-linux-x86_64"

sudo ./install.sh
```

The installer creates the `scribe-server` systemd service, configures the necessary directories and LAN firewall rules where supported, and starts Ignite Server.

It also provides a one-time setup code for claiming your new server.

### 2. Open Ignite Web

Open the address printed by the installer, typically:

`http://your-hostname.local/`

If `.local` name resolution isn't available on your network, use your server's LAN IP address instead.

Ignite normally uses port 80, or port 8787 if port 80 was already occupied during initial installation.

Enter the setup code printed during installation to complete the initial setup.

**That's it. Ignite Server and its browser interface are running.**

### 3. Verify the server

```bash
sudo systemctl status scribe-server
```

For a default port-80 installation:

```bash
curl -fsS http://127.0.0.1/control/v1/status
```

The status response should show the installed version and server identity.

### 4. Keep Ignite updated

Future updates require just one command:

```bash
sudo ignite-update
```

The updater checks official GitHub Releases, verifies the downloaded bundle, creates a recovery backup, and installs the newer version.

To check without installing:

```bash
sudo ignite-update --check
```

For configuration, backups, troubleshooting and advanced installation options, see [Self-hosting documentation](docs/self-hosting.md).

## Prefer Docker or Portainer?

Ignite also supports Docker Compose on Linux with host networking.

**Current limitation:** A prebuilt container image is not yet published to GHCR or Docker Hub. Docker users currently build the image locally.

With Docker Engine, Docker Compose, Rust and Node.js installed:

```bash
git clone https://github.com/markrai/ignite.git
cd ignite
git checkout v0.9.13

bash deployment/linux/build-bundle.sh
bash deployment/linux/docker/build-image.sh

cd deployment/linux/docker

bash ./init-once.sh
docker compose up -d
```

Save the one-time setup code printed by `init-once.sh`.

Open:

`http://YOUR_SERVER_IP:8787/`

Complete setup in the browser using that code.

**Important Docker notes:**

- Uses Linux host networking for Kindle discovery.
- Persistent notebook data lives in a Docker named volume.
- Host firewall configuration is your responsibility.
- Do not use `docker compose down -v` unless you intend to delete the persistent volume.

See [Docker and Portainer setup](deployment/linux/docker/README.md) for configuration, backups and upgrades.

## Connect your Kindle Scribe

Ignite Server is optional. The Kindle application stores notebooks locally and can operate without self-hosted sync.

To use the Kindle application:

1. You need a compatible Kindle Scribe prepared for third-party applications.
2. Install the Ignite Kindle application through its Scriptlet installer.
3. Connect the Kindle and your Ignite Server to the same trusted LAN.
4. Complete the application's server pairing process.

The server and Kindle application are separate installations.

**A stock, non-jailbroken Kindle cannot run Ignite.** See the [compatibility guide](docs/compatibility.md) and [Kindle installation instructions](deployment/kindle/README.txt).

The v0.9.13 GitHub Release currently contains the **Linux server**, not the Kindle installer. Kindle beta distribution is handled separately.

## Security and network access

Ignite Server currently uses plain HTTP and is designed for **trusted local networks**.

Do not expose its HTTP port directly to the public Internet.

For additional deployment options, recovery procedures and network configuration, see [Self-hosting documentation](docs/self-hosting.md).

## Features

- Multi-stroke notebook editing on Kindle Scribe
- On-device library, tools, and durable page storage
- Optional LAN sync with a self-hosted server
- One-file customer installer (`Install Ignite v*.sh`)
- Diagnose / backup / uninstall helpers
- This is a work in progress, and new features are being readily implemented.

## Status

| Area | Status |
|------|--------|
| Kindle app + installer | Active development / beta |
| Self-hosted sync server | **Ignite Server** — portable Linux bundle (see [self-hosting](docs/self-hosting.md)) |
| Web companion | **ignite-web** in `ignite/web/` (browser replica + sync) |
| Experimental recovery | **Disabled** in product builds (see below) |

Version: see [`ignite/VERSION`](ignite/VERSION).

**Project board:** Follow current and planned work on the [public Ignite Kanban board](https://scrumboy.com/ignite).

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

Install Ignite Server (with ignite-web) from the Linux bundle on
[GitHub Releases](https://github.com/markrai/ignite/releases), then keep it current with:

```bash
sudo ignite-update
```

Updates come from the official published releases; the updater verifies the download and takes
a recovery backup before installing. Installation, updating and building a bundle from source
are described in [`docs/self-hosting.md`](docs/self-hosting.md). Trusted LAN / plain HTTP only.
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

Application registration identity is `**com.markrai.ignite**` (from 0.9.11).
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
