# Changelog

## [0.9.24] - 2026-11-09

### Added

- On-screen Erase mode toggle in the Kindle notebook chrome: an ERASE
  button between the title and UNDO switches the pen tip to the eraser
  without needing the hardware eraser. The button inverts while the mode
  is on and repaints only the chrome band. The mode is transient (never
  persisted); a pen Down starts an eraser gesture while on, and UI taps
  still use the real tool so the pen can operate chrome. Page navigation
  keeps the mode; leaving the notebook (GoHome, durable cross-notebook
  switch, create, or delete) clears it, while failed transitions keep it.

## [0.9.23] - 2026-11-08

### Added

- Synchronized **Change Paper** across Kindle, Server, and Web: replace the
  paper on the current page or every page in a notebook (Blank / Ruled / Grid /
  Dotted) without touching handwriting, text, links, or page identity. Patterned
  papers publish as ordinary Gray8 background blobs; notebook-wide runs keep an
  already-applied prefix durable if interrupted and support cancellation.
- Server gate for `SetPageBackground` (`--accept-set-page-background` /
  `accept_set_page_background`), off by default. Authors probe
  `GET /v1/libraries/{id}` and refuse to journal until the server advertises
  acceptance so older peers stay fail-closed.
- Reusable release qualification and Kindle preflight tooling under `tools/qa`
  (`qualify`, `preflight`, `status`): Rust, ARMv7, WASM, Web, and Chromium
  verification with artifact provenance, evidence integrity (content-aware
  source fingerprints, physical SHA-256 match, fail-closed data-compatibility),
  and production-slot physical QA isolation.

### Internal

- Stabilized sync outbound lock release, recovery loopback harness timing, and
  lifecycle host-lease tests under parallel workloads.

## [0.9.22] - 2026-11-07

### Improved

- Kindle Home search now shows matching notebooks first, including
  notebooks inside folders, followed by matching folder tiles.
- Foldered notebook results show their parent folder alongside
  the page count.

### Internal

- Stabilized concurrent sync adoption and recovery tests.
- Added centralized version management with consistency checks
  and rollback handling.

## [0.9.21] - 2026-11-06

### Fixed

- Kindle in-app OTA can supersede an obsolete completed `verified-update`
  staging candidate with a newer official release. Staging and lifecycle share
  one updater transaction lock; supersession stays fail-closed on install
  intent, pending-health, unresolved activation evidence, interrupted
  publication leftovers (including W4 cross-version mid-swap), and missing or
  unsafe published assets. Same-tag revalidation and Scriptlet authentication
  are unchanged.

## [0.9.19] - 2026-11-05

### Added

- Kindle Home notebook title search: tap **Search notebooks...** to filter the
  library by title (case-insensitive), including notebooks inside folders.
  Results open the matching notebook; Cancel, empty/clear search, and empty
  results keep folder navigation and Home paging intact. Search state is
  transient and does not change notebook metadata, ordering, storage, or sync.

## [0.9.18] - 2026-11-04

### Added

- Notebook page templates on Kindle: when creating a notebook, a **Select paper**
  step offers Blank, Ruled, Grid, and Dotted. The choice is applied only at page
  creation as a durable Gray8 `background.raw` (existing pages and blank notebooks
  are unchanged). Non-blank templates sync as ordinary background blobs with no
  protocol or schema migration. Mark spacing and E Ink–tuned intensity (including
  3×3 dotted marks) are fixed generator constants.
- Ignite Web: the same Select paper flow (Blank / Ruled / Grid / Dotted) when
  creating a notebook. Patterned papers generate Kindle-panel Gray8 bytes that
  match the Rust generator byte-for-byte, publish via `PUT /v1/blobs/{sha}`, then
  submit `CreateNotebook`. Blank still creates without a blob. Patterned creation
  requires connectivity (no offline blob upload queue yet).

## [0.9.17] - 2026-11-03

First release exercised as a successful end-to-end in-app OTA upgrade from the
verified on-device **v0.9.16** baseline.

### Changed

- Synchronized release identity to **0.9.17** across `ignite/VERSION`, Cargo
  workspace metadata, and web package lockfiles.
- Improved maintainer release tooling for preparing a release from an already
  green main CI run and retrying publication without cross-version workspace
  contamination.

## [0.9.16] - 2026-11-03

Introduced working Kindle in-app OTA acquisition against published GitHub
Release assets. Device installs of this release were performed with the
official Scriptlet; the first successfully tested OTA upgrade from this
baseline targets **v0.9.17**.

### Added

- Gate B acceptance for the reported Scribe framebuffer `rotate=1` path, with
  a unit-tested 180° dual-axis touch and stylus remap relative to the verified
  `rotate=3` geometry. Project hardware remains firmware **5.18.6** only;
  upright paint, hit targets, and ink on **5.19.6** are **not** physically
  verified. ([#5](https://github.com/markrai/ignite/pull/5), original work by
  [@richardvinc](https://github.com/richardvinc); integration/hardening by Mark
  Rai)

### Fixed

- Corrected Kindle OTA download URLs and release verification so Scriptlet
  assets published with GitHub’s space-to-dot filename rewriting
  (`Install.Ignite.vX.Y.Z.sh`) resolve correctly.
  ([#7](https://github.com/markrai/ignite/pull/7))
- Stopped suppressing Linux version-probe stderr in promote guards so native /
  WSL `scribe-server --version` failures remain visible while still
  fail-closing. (part of [#7](https://github.com/markrai/ignite/pull/7))

## [0.9.15] - 2026-11-03

Authenticated publication and update-trust foundation for Kindle releases.
Users on builds that still embed an `UNPROVISIONED` publisher pin need a
trusted manual reinstall to establish the production public key.

### Added

- Production Kindle publisher public-key pin provisioning for authenticated
  release publication (public pin only; private key never enters CI).
- Authenticated release signing and promotion path (signed Kindle release
  metadata and verified publication assets).
- Last-known-good release retention with durable pending-health candidate
  state, launch-health evidence, and an updater transaction lock.
- Verified update acquisition/staging and installation-authorization
  scaffolding used by later in-app OTA work.
- In-app software update discovery and authenticated update lifecycle
  machinery (usable OTA against published GitHub asset names arrives in
  **0.9.16**).
- Maintainer `ignite_ship` release orchestration for resumable authenticated
  publication workflows.

### Changed

- Clarified community testing and hardware verification policy in release
  documentation.

## [0.9.14] - 2026-10-23

### Added

- Automated Kindle release packaging and verified artifact promotion helpers,
  including version-sync CI checks and `promote-release` tooling.
- Verified GHCR publishing workflow and Docker quick-start deployment guidance
  for the self-hosted server image.
- Durable page-change ledger with fail-closed mutation tracking on Kindle.
- Sync startup diagnostics and deferred confirmed-state load to keep verified
  startups responsive.
- Shadow reconciliation during navigation so fast startup behavior is
  preserved while sync catches up.

### Changed

- Expanded self-hosting documentation for Docker / GHCR deployment.

## [0.9.13] - 2026-10-20

First locally tagged GitHub Release identity for Ignite (`v0.9.13`).

### Added

- GitHub Releases packaging pipeline for Kindle and Linux artifacts.
- Self-updating Linux server install/upgrade path with verified backups,
  rollback, and recovery tooling.

### Fixed

- Aligned web text geometry validation with Rust `f32` semantics and corrected
  edge-case text placement.
- Aligned web package version and ARM feature validation with the release
  identity.

## [0.9.12] - 2026-10-20

Untagged in local Git (no `v0.9.12` tag). Identity recorded via
`ignite/VERSION` prepare commit `8c440a3`.

### Added

- Kindle **Restore from Server** workflow enabled in the official ARM release
  build profile (after hardware memory testing on a Kindle Scribe).
- Portable backup format with verification/re-keying, transactional
  multi-library restore, and browser/admin backup upload & restore flows for
  the self-hosted stack.
- Self-hosted Ignite Server Linux packaging with safe install, upgrade, and
  rollback validation.
- Secure Docker Compose / Portainer deployment path for Ignite Server, with
  LAN discovery and acceptance coverage.

### Changed

- Restored `ignite-web` as the browser notebook replica with optional
  self-hosted sync and verified WASM builds.

## [0.9.11] - 2026-10-17

Untagged in local Git (no `v0.9.11` tag). Earliest `ignite/VERSION` value in
this repository’s initial commit (`d6e7a99`). Pre-0.9.11 product history is
not present as commits here.

### Added

- Kindle Scribe notebook application with local handwriting, on-device library,
  and durable page storage under `/mnt/us/scribe-data`.
- Single-file customer installer (`Install Ignite v*.sh`) plus Diagnose /
  Backup / Uninstall Library helpers for prepared Kindles.
- Application registration under `com.markrai.ignite`, with installer migration
  from the older `com.markrai.scribe` handler without moving notebook data.
- Optional LAN sync with a self-hosted server (`_scribe._tcp` discovery
  unchanged).

### Security

- Documented that Ignite does not jailbreak Kindles and requires an
  already-prepared device (SH Integration / root helper).

## Gaps and notes

- Local tags exist only for `v0.9.13` … `v0.9.17`. Versions **0.9.11** and
  **0.9.12** are reconstructed from `ignite/VERSION` history and prepare
  commits, not from annotated release tags.
- History before the initial commit (`d6e7a99`, 2026-10-17) is not available
  locally; claims about earlier public releases (for example pre-0.9.11
  handler migration behavior) come from in-tree documentation present at that
  commit, not from earlier Git objects.
- Compatibility claims distinguish **Verified** project hardware (Kindle
  Scribe 1st gen, firmware **5.18.6**) from **Reported** / unit-tested paths
  such as `rotate=1` on firmware **5.19.6**.