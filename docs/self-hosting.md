# Ignite Server — portable Linux self-hosting

**Status:** Portable Linux systemd bundle, published on
[GitHub Releases](https://github.com/markrai/ignite/releases) (or built from source), kept up to
date with `sudo ignite-update`; optional **Docker Compose / Portainer** packaging (Linux host
networking). These are alternative deployment paths;
Compose does **not** assume the native installer has configured the host firewall.

Public names: **Ignite Server** (host binary + service) and **ignite-web** (browser UI).

On-disk paths and discovery identity remain compatible with existing installs:

| Item | Value |
|------|--------|
| Binary / unit | `scribe-server` / `scribe-server.service` |
| Config | `/etc/scribe-server/` |
| State | `/var/lib/scribe-server/` (0700) |
| DNS-SD | `_scribe._tcp` |
| Firewall ownership tag | `scribe-server` |
| PWA id | `/ignite-web` (stable, not versioned) |

## Prerequisites

- Linux on **x86_64** for published release bundles; **aarch64** and **armv7l** installs are
  supported from a bundle built from source on (or for) that host
- systemd (for production installs; staging tests use `--root`)
- Optional: **ufw** or **firewalld** (installer scopes rules; never flushes custom nft/iptables)
- From source builds: Rust stable, Node 20+, `wasm32-unknown-unknown` target
- `curl` on the host (post-install health checks and `ignite-update` downloads)

A binary / bundle built on one Linux host is **not** automatically compatible with
every libc version or CPU architecture. Build the bundle on (or for) the target
class of host, or verify the bundled `scribe-server` runs on that machine before
relying on it.

Trusted **LAN only**. Ignite Server speaks **plain HTTP**. Do not forward a router
port to it. Do not expose it on the public Internet.

## Get a release bundle

Each Ignite release publishes a tested Linux server bundle on
[GitHub Releases](https://github.com/markrai/ignite/releases):
`ignite-server-<version>-linux-x86_64.tar.gz` and its `.sha256` checksum.

```bash
v=X.Y.Z   # the version shown on the Releases page
curl -fLO "https://github.com/markrai/ignite/releases/download/v$v/ignite-server-$v-linux-x86_64.tar.gz"
curl -fLO "https://github.com/markrai/ignite/releases/download/v$v/ignite-server-$v-linux-x86_64.tar.gz.sha256"
sha256sum -c "ignite-server-$v-linux-x86_64.tar.gz.sha256"
tar -xzf "ignite-server-$v-linux-x86_64.tar.gz"
cd "ignite-server-$v-linux-x86_64"
```

Then continue with [First installation](#first-installation). After that, updates are one
command: `sudo ignite-update` (see [Updating](#updating)).

## Build a bundle from source (developers)

```bash
# From the repository root (Linux)
bash deployment/linux/build-bundle.sh
# → dist/ignite-server-linux/
```

`build-bundle.sh` runs:

1. `cargo build -p scribe-server --release --locked`
2. `cd ignite/web && npm ci && npm run build` (authoritative Cargo WASM + SHA-256 verify)
3. `deployment/linux/make-bundle.sh` (stages output; refuses unsafe out-dirs and symlinks)

Or assemble from already-built artifacts:

```bash
bash deployment/linux/make-bundle.sh \
  target/release/scribe-server \
  ignite/web/dist \
  dist/ignite-server-linux
```

## First installation

From an extracted release bundle (or a source-built bundle copied to the host):

```bash
cd ignite-server-<version>-linux-x86_64   # or dist/ignite-server-linux
sudo ./install.sh
```

Useful flags:

| Flag | Effect |
|------|--------|
| `--firewall=dry-run` | Print planned ufw/firewalld changes only |
| `--firewall=none` | Skip firewall changes |
| `--port=N` | Force listen port on first install (default 80, else 8787) |
| `--root=DIR` | Staging install (tests; no systemd / no firewall / no `scribe` user) |

First install creates the unprivileged `scribe` account, state directory, config,
systemd unit, opens ownership-scoped LAN firewall rules when applicable, installs the updater
as `/usr/local/bin/ignite-update`, and prints the browser URL plus a **one-time setup code**
while the server is unclaimed.

## Browser access

Open the printed URL (for example `http://hostname.local/` or with an explicit port).
Complete the setup / claim flow in ignite-web. Sessions use HttpOnly cookies;
Kindle pairing uses the control plane (`/control/v1`).

## Kindle pairing and LAN discovery

Kindles discover the server via DNS-SD (`_scribe._tcp`) on the eligible LAN
interfaces. Pairing and credentials are control-plane concepts; notebook sync uses
`/v1` authorized by the control plane in product mode.

## Data and configuration

```text
/etc/scribe-server/config.toml     listen, state_dir, web_root, name, discovery
/etc/scribe-server/install-state   installer bookkeeping (version, port, firewall)
/var/lib/scribe-server/            ServerId, control.sqlite, ledger/, blobs/
/usr/local/bin/scribe-server       product binary
/usr/local/bin/ignite-update       updater (replaced by every install/update)
/usr/share/scribe-server/web/      ignite-web static assets
/var/backups/ignite-server/        recovery backups taken by ignite-update (0700; never pruned)
```

Back up `/var/lib/scribe-server` (and `/etc/scribe-server`) before major host changes.

## Updating

```bash
sudo ignite-update           # check, confirm, back up, install, verify
ignite-update --check        # only report whether an update is available (exit 10 if so)
sudo ignite-update --yes     # skip the confirmation prompt (never skips a check or the backup)
```

Updates come only from the official, published, **stable** GitHub Releases of `markrai/ignite`
(downloaded over HTTPS; no GitHub account, `gh`, Git checkout or build toolchain is needed).
Prereleases are never installed, an older release never replaces a newer installation, and an
installation that is already current exits without restarting anything.

`ignite-update`:

1. reads the installed version and architecture and asks GitHub for the latest stable release;
2. downloads that release's bundle for this architecture and verifies its SHA-256 checksum,
   its contents (regular files only, nothing outside the bundle directory), its metadata
   version/architecture and that its `scribe-server` runs here and reports the release version
   — all before the running service is touched;
3. records the ServerId and claimed state;
4. stops the service and writes a recovery backup to
   `/var/backups/ignite-server/<time>-<old>-to-<new>.XXXX/` (mode 0700): `state.tar.gz`
   (`/var/lib/scribe-server` and `/etc/scribe-server`), `software.tar.gz` (binary, ignite-web,
   unit, updater), `SHA256SUMS` and `RESTORE.txt`; the backup is checksummed and its file
   counts compared with the originals before anything is installed;
5. runs the bundle's own `install.sh --firewall=none`, which keeps the existing firewall rules
   and ownership record, and restarts the service;
6. verifies the service is active, the expected version runs, the ServerId and claimed state
   are unchanged, the server answers with its identity, and ignite-web is installed;
7. reports success and the backup location.

Only one update runs at a time. Earlier backups are never deleted; remove old ones yourself
when you no longer need them.

**If something fails:** a failure before installation (download, checksum, contents,
compatibility, backup, disk space) changes nothing and restarts the service if it had been
stopped. If the installer itself fails, it restores the previous binary, web assets and unit and
restarts the service (below); `ignite-update` reports which. Server data (databases, ledger,
blobs) is **not** rolled back automatically: the new version may already have opened it. To
return to the previous version, follow `RESTORE.txt` in the reported backup directory.

### Installations from before ignite-update

A server installed from a bundle without `ignite-update` gets it with its next install. Do this
once, from the first release that contains `ignite-update` (download and verify it as in
[Get a release bundle](#get-a-release-bundle)), then run the updater shipped in that bundle:

```bash
cd ignite-server-<version>-linux-x86_64
sudo ./ignite-update
```

It follows exactly the steps above (backup included) and installs itself as
`/usr/local/bin/ignite-update`; from then on use `sudo ignite-update`.

### Installing a bundle by hand

Re-running `sudo ./install.sh` from a newer bundle (for example a source build) also upgrades.
It replaces the binary, web assets, unit file and updater, then restarts the service. It
preserves ServerId, claim state, devices, sessions, config, ledger, and blobs. No new setup
code is printed for an already-claimed server. It takes no backup; `ignite-update` does.

If `systemctl daemon-reload`, `enable`, `restart`, or the post-start health check
fails after live components were replaced, the installer restores the previous
binary, web assets, and systemd unit (when those priors exist), attempts a
recovery restart of the restored service on real hosts, and exits nonzero
(never reports success). Messages distinguish file restoration from whether a
recovery restart succeeded (`recovery restart=0|1`).

Rollback after post-promotion failure is covered by disposable staging tests
using installer seams
(`--test-seam=fail-daemon-reload|fail-enable|fail-restart|fail-health` with
`--root=`). Those seams do **not** modify host systemd; they exercise the same
restore path used after promotion. End-to-end validation against a real host
`systemctl` is not claimed by the portable staging suite.

## Uninstall

```bash
sudo ./uninstall.sh          # removes service/binary/web/updater/firewall ownership; keeps data
sudo ./uninstall.sh --purge  # also deletes state, config, and the scribe account
```

Ordinary uninstall is data-preserving so a later reinstall resumes the same identity. Neither
form deletes `/var/backups/ignite-server/`.

## Firewall behavior

The installer opens only:

- TCP `<port>` on eligible LAN interfaces
- UDP 5353 toward `224.0.0.251` (mDNS) on those interfaces

Rules are tagged/owned (`scribe-server`). Foreign rules are never deleted or
re-tagged. Custom nftables/iptables DROP policies are reported, not edited.

## Plain HTTP and HTTPS

Ignite Server itself remains plain HTTP for trusted-LAN use (Kindle clients included).
Some browser features (service-worker update prompts, certain PWA install flows)
require a **secure context** (HTTPS or localhost). Providing HTTPS via an external
reverse proxy or VPN terminator is optional and documented for a later phase; it is
not required for core notebook sync on the LAN.

## Docker Compose / Portainer (Linux host networking)

Use this when you want a containerized Ignite Server on **Linux Docker Engine**
(Portainer on a Linux server or compatible NAS included). Ordinary Docker **bridge**
networking is **not** the supported default (mDNS / LAN discovery fit is poor; see
[`deployment/linux/docker-network-feasibility.md`](../deployment/linux/docker-network-feasibility.md)).

| Item | Value |
|------|--------|
| Docs / scripts | [`deployment/linux/docker/`](../deployment/linux/docker/) |
| Image tag (local) | `ignite-server:local` (build via `build-image.sh`; Compose is image-only) |
| Runtime PID 1 UID/GID | `19187:19187` after entrypoint drop (image USER is root for volume chown) |
| Network | `network_mode: host` |
| Default port | TCP `8787` |
| State volume | `/var/lib/scribe-server` |
| Config | Embedded in the image; optional absolute-path override (Portainer) |
| Validated arch | **linux/amd64** (ARM not claimed by this packaging path) |

### Build and start

```bash
bash deployment/linux/build-bundle.sh
bash deployment/linux/docker/build-image.sh
cd deployment/linux/docker

# First run only — explicit init (prints setup_code=… while unclaimed):
bash ./init-once.sh

docker compose up -d
curl -fsS http://127.0.0.1:8787/control/v1/status
```

Portainer: load the image on the Linux engine, paste the image-only stack (no `build:` /
`context/`), use the embedded config or an **absolute** host bind for overrides, and run
one-off `init` against the **same** named volume before claim.

Open `http://<host>:8787/` and complete the claim in ignite-web. **Do not** put setup
codes in Compose YAML, Portainer env vars, or shared logs. While unclaimed, every
`init` issues a **new** code and invalidates the previous one. After claim, restarts
and same-volume image replacements must not require another claim.

### Host firewall and ports

Compose/Portainer **do not** apply `firewall.sh` from the native installer. You must
configure and verify host firewall rules for trusted-LAN access (TCP listen port and
mDNS as appropriate). Ignite’s application network policy still filters connections
but is **not** a substitute for host firewall configuration. If port `8787` is taken,
override `listen` (and the healthcheck URL) together.

### Backup, restore, upgrades

`ignite-update` is for the native systemd installation only; Compose deployments update by
building and starting a newer image. Back up the Docker volume for `/var/lib/scribe-server`
plus any host config override.
Restore onto the same volume and start a current image. Do not run
`docker compose down -v` unless you intend to destroy customer data. Disposable
acceptance proves restart/recreate with the **same** image, including a paired-client
notebook/blob API round-trip verified after each; distinct software-version migration is not
claimed by that suite.

Full operator notes: [`deployment/linux/docker/README.md`](../deployment/linux/docker/README.md).

## What this packaging does not include

- Publishing images to Docker Hub / GHCR
- Automatic (unattended) updates; `ignite-update` runs only when an administrator runs it
- Automatic container updaters
- Reverse-proxy / automatic HTTPS configuration
- Privileged sudoers deploy automation (`scribe-deploy`)
- Tailscale-specific product dependencies
- Claiming physical-LAN Kindle mDNS validation from virtualized Docker tests alone

## Disposable tests

```bash
# After build-bundle.sh (Linux CI / container)
bash deployment/linux/test-bundle.sh dist/ignite-server-linux
bash deployment/linux/test-path-safety.sh dist/ignite-server-linux
bash deployment/linux/test-staging.sh dist/ignite-server-linux
bash deployment/linux/test-cargo-target-dir.sh
bash deployment/linux/test-update.sh dist/ignite-server-linux   # ignite-update, mock releases

# Firewall ownership (disposable NET_ADMIN container only — never the dev host)
bash deployment/linux/ci-firewall.sh

# Docker packaging (static; no daemon)
bash deployment/linux/docker/test-compose-static.sh
bash deployment/linux/test-docker-network-probe.sh

# Full disposable image accept in a fresh Multipass VM (default; also from Windows CMD via WSL)
bash deployment/linux/docker/accept-disposable.sh
# Or this host's Linux Docker Engine, explicitly:
IGNITE_DOCKER_ACCEPT=local bash deployment/linux/docker/accept-disposable.sh
```

See [Disposable acceptance](../deployment/linux/docker/README.md#disposable-acceptance) for
Windows/WSL details, cleanup guarantees, and result fields.

## Publishing a release (maintainers)

Releases are published by CI, never from an ordinary push to `main`:

1. Set the release version in `ignite/VERSION`, the Rust workspace and `ignite/web`
   (`npm version <v> --no-git-tag-version`), and merge it to `main`.
2. Push an annotated tag `v<version>` for that commit.
3. The CI workflow runs every job; only if all pass does the `release` job check that the tag,
   `ignite/VERSION`, the binary and the bundle metadata agree, and publish
   `ignite-server-<version>-linux-x86_64.tar.gz` plus `.sha256` as a new GitHub Release
   (`-` in the tag marks a prerelease, which `ignite-update` ignores).

The job refuses to touch a release that already exists; replacing published assets is a manual
release-management decision.
