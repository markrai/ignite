# Ignite architecture (contributor overview)

Ignite is a local-first handwriting notebook for a prepared Kindle Scribe, with
optional self-hosted sync and **ignite-web** (`ignite/web/`) as the browser replica.

## Kindle native runtime

| Crate | Role |
|-------|------|
| `scribe-app` | Main notebook UI/runtime: ink, pages, library, sync client hooks |
| `scribe-lifecycle` | Process supervisor / relaunch helper used by the host script |
| `kindle-platform` | Framebuffer, input, timing, and device glue |
| `ink-probe` | Hardware laboratory binary (not the customer app) |

The Library entry launches via `scribe-lifecycle-host.sh` (appmgrd handler
`com.markrai.ignite`), which starts `scribe-lifecycle` → `scribe-app` with
explicit `--data-dir`, `--sync-dir`, and `--device-dir` paths under `/mnt/us`.

Pre-0.9.11 installs used handler `com.markrai.scribe`. The installer migrates
that registration to `com.markrai.ignite` without moving binaries or notebook
data. Sync discovery identity `_scribe._tcp` and storage under
`/mnt/us/scribe-data` are unchanged.

## Rendering and editing

Stylus samples are processed into strokes, rasterized for the e-ink panel, and
committed into durable page state. UI chrome (library, tools, sleep) is separate
from page ink. Polarity / grayscale handling differs across Scribe panel modes;
see the compatibility matrix.

## Durable notebook storage

Notebooks live under `/mnt/us/scribe-data` (customer data). Sync metadata and
device state live under `/mnt/us/scribe-app/state/`. Installer upgrade/uninstall
paths are designed to preserve notebooks unless the user deliberately deletes
them.

Shared types and reducers live in:

- `notebook-core` — notebook operations and geometry helpers
- `sync-protocol` — canonical envelopes and hashing
- `sync-engine` — apply / reduce / testkit fixtures

## Sync and operation envelopes

Outbound/inbound sync uses versioned operation envelopes. A self-hosted
`scribe-sync-server` can accept a replica’s history over HTTP on a trusted LAN.
Server-side acceptance of destructive lineage-replacement operations is **off
by default** and must not be enabled casually.

Pairing / discovery helpers live in `scribe-server` + `control-protocol`.

## ignite-web

`ignite/web/` is the browser notebook replica (Vite/React). Canonical sync
encoding is produced by the Rust WASM bridge `scribe-web-wasm`
(`ignite/core/scribe-web-wasm`); TypeScript does not implement wire encoding.
Browser durable state uses IndexedDB `scribe-web-sync` (name preserved for
existing clients). PWA manifest `id` is the stable `/ignite-web` (not versioned).
Product mode pairs through `/control/v1` on `scribe-server`; developer-token
mode talks to `scribe-sync-server` over `/v1`.

## Ignite Server (Linux self-hosting)

Portable Linux packaging lives under `deployment/linux/` (bundle build, install /
upgrade, systemd unit, ownership-scoped firewall helpers, disposable staging
tests). On-disk paths remain `/etc/scribe-server` and `/var/lib/scribe-server`
for compatibility. See [`self-hosting.md`](self-hosting.md).

## Packaging and installation

1. Cross-compile ARMv7hf binaries (`tools/docker_build_armv7hf.sh`).
2. Build the pinned Dropbear client (`tools/dropbear/docker_build.sh` in the
   same digest-pinned `rust:1-bullseye` image as the Rust ARM build;
   `PINNED.txt` enforced).
3. Stage payload + build self-extracting Scriptlet
   (`deployment/kindle/package_release.py` via `scripts/kindle/package-release.sh` →
   `Install Ignite vVERSION.sh`).
4. Customer copies the single Scriptlet into Kindle **Documents** and opens it.

Installer logic and tests: `deployment/kindle/`.

## Experimental recovery subsystem

Modules under `scribe-app` (`sync_recovery_*`) implement planning, fencing, and
a supervised Dropbear `-B` HTTP stream for *future* controlled recovery. As
published:

- Live non-loopback recovery POST is compile-time disabled
- Production child spawn for the stream remains refused
- A test-only `kindle_supervisor_harness` feature can exercise the supervisor
  against disposable fixtures; it is excluded from customer packaging

Physical Kindle verification of the protected Dropbear client and
`SupervisedRecoveryStream` does **not** enable customer recovery.
Review 79 (performance) remains open and separate.
