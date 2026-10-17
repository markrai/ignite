# Ignite Server — portable Linux self-hosting

**Status:** Phase 1 portable Linux bundle (source-built). Docker Compose / Portainer
packaging is not part of this release.

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

- Linux on **x86_64**, **aarch64**, or **armv7l**
- systemd (for production installs; staging tests use `--root`)
- Optional: **ufw** or **firewalld** (installer scopes rules; never flushes custom nft/iptables)
- From source builds: Rust stable, Node 20+, `wasm32-unknown-unknown` target
- `curl` on the host (used for post-install health checks)

A binary / bundle built on one Linux host is **not** automatically compatible with
every libc version or CPU architecture. Build the bundle on (or for) the target
class of host, or verify the bundled `scribe-server` runs on that machine before
relying on it.

Trusted **LAN only**. Ignite Server speaks **plain HTTP**. Do not forward a router
port to it. Do not expose it on the public Internet.

## Build a bundle from source

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

Copy the bundle directory to the host, then:

```bash
cd dist/ignite-server-linux   # or wherever the bundle lives
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
systemd unit, opens ownership-scoped LAN firewall rules when applicable, and prints
the browser URL plus a **one-time setup code** while the server is unclaimed.

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
/usr/share/scribe-server/web/      ignite-web static assets
```

Back up `/var/lib/scribe-server` (and `/etc/scribe-server`) before major host changes.

## Upgrading

Re-run `sudo ./install.sh` from a newer bundle on the same machine. Upgrade replaces
the binary, web assets, and unit file, then restarts the service. It preserves
ServerId, claim state, devices, sessions, config, ledger, and blobs. No new setup
code is printed for an already-claimed server.

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
sudo ./uninstall.sh          # removes service/binary/web/firewall ownership; keeps data
sudo ./uninstall.sh --purge  # also deletes state, config, and the scribe account
```

Ordinary uninstall is data-preserving so a later reinstall resumes the same identity.

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

## What this phase does not include

- Docker Compose / Portainer images
- Privileged sudoers deploy automation (`scribe-deploy`)
- Tailscale-specific product dependencies
- Automatic public-TLS certificate management

## Disposable tests

```bash
# After build-bundle.sh (Linux CI / container)
bash deployment/linux/test-bundle.sh dist/ignite-server-linux
bash deployment/linux/test-path-safety.sh dist/ignite-server-linux
bash deployment/linux/test-staging.sh dist/ignite-server-linux
bash deployment/linux/test-cargo-target-dir.sh

# Firewall ownership (disposable NET_ADMIN container only — never the dev host)
bash deployment/linux/ci-firewall.sh
```
