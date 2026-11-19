# MuPDF Phase 1B.1: security and upgrade assessment

Status: **assessment for review, no dependency or runtime changes** (2026-11-18). Tracks [Ignite #22](https://github.com/markrai/ignite/issues/22).

## Decision

**Candidate native release:** MuPDF **1.28.6**, upstream tag `1.28.6`, commit [`da5f5eb890d7b3fb76903160b5f7f48ba1301492`](https://github.com/ArtifexSoftware/mupdf/commit/da5f5eb890d7b3fb76903160b5f7f48ba1301492) (2026-10-29). This is a *candidate source pin*, not a qualified or automatically secure binary. Validate any relevant post-release security patch before selecting the final source tree.

**Preferred integration:** first look for a completed, maintained upstream `mupdf-rs` release with the necessary native fixes. If none exists when implementation starts, maintain a narrowly scoped, source-pinned fork of **`mupdf-sys` 0.8.0** and, only where required by ABI/API changes, the `mupdf` Rust wrapper. Pin the resulting fork by immutable Git commit in `Cargo.lock` using a narrowly scoped `[patch.crates-io]` entry or an equivalent explicit source override. Do not claim that the fork's commit SHA exists until the implementation is created and qualified.

**Current Ignite baseline:** `scribe-app` pins `mupdf = "=0.8.0"`, and `mupdf-sys` 0.8.0 vendors MuPDF **1.27.2**. The `mupdf-rs` `v0.8.0` submodule resolves to native commit [`73d3100d46d8a9ad634f6ef035bbe78f0f947886`](https://github.com/ArtifexSoftware/mupdf/tree/73d3100d46d8a9ad634f6ef035bbe78f0f947886), exactly the upstream `1.27.2` tag. It is not a hypothetical version inferred from the Rust crate number.

## Why this is necessary

The [MuPDF release history](https://mupdf.com/releases/history) lists extensive memory-safety and robustness fixes in 1.28.x. In particular, **1.28.5** addresses out-of-bounds reads/writes in pixmap conversions, painting, font subsetting, recoloring and JPEG handling, as well as a BMP stride integer overflow and image decompression-bomb detection. **1.28.6** updates bundled Brotli to 1.2.1. Not every fixed routine is necessarily reachable through Ignite's restricted PDF/EPUB/CBZ reader; reachability must be determined rather than assumed.

The most recent published [`mupdf-rs` release](https://github.com/messense/mupdf-rs/releases) is **v0.8.0** (2026-06-22); its current `main` submodule is also pinned to native 1.27.2. An upstream [PR #261](https://github.com/messense/mupdf-rs/pull/261) proposes 1.28.2 but remains open as of this assessment. Therefore `cargo update` alone cannot provide native 1.28.6, and swapping a MuPDF source directory without testing the FFI is not a safe upgrade procedure.

## Security findings requiring source-level closure

| Finding | Evidence / affected surface | Required Phase 1B.2 disposition |
| --- | --- | --- |
| MuPDF 1.28.x hardening | [Upstream release history](https://mupdf.com/releases/history), notably 1.28.0 and 1.28.5, describes security, OOB, overflow and parsing fixes | Record which fixes are in the exact selected tree and which relevant code paths are compiled/reachable |
| **CVE-2026-112036**, signature-locking `free_char_list` double-free | [Debian tracker](https://security-tracker.debian.org/tracker/CVE-2026-112036) reports affected versions through 1.28.5 and names fix commit `3984925b660afb8a781ddf504021e91be1bc671e` | Inspect the *actual 1.28.6 source* for the fix or equivalent; do **not** equate release date with proven patch inclusion; assess whether signature locking is reachable while viewing PDFs |
| **CVE-2026-7233**, CFF subsetting OOB read | [Debian tracker](https://security-tracker.debian.org/tracker/CVE-2026-7233) lists 1.28.2 Debian packages as unfixed, with no fixed version | Check 1.28.6 source and compiled feature reachability; if still affected, decide whether a verified patch/feature exclusion is required before shipping |
| **CVE-2026-3308**, PDF image integer overflow and heap OOB write | [Debian tracker](https://security-tracker.debian.org/tracker/CVE-2026-3308) describes 1.27.0 and a downstream fix | Check source ancestry to determine whether Ignite's **1.27.2** already includes the fix; do not label 1.27.2 vulnerable from a 1.27.0 report without verification |
| Native image/font/HTML parser exposure | Ignite enables `epub`, `html`, `img`, `svg`, `cbz`, and `base14-fonts`; untrusted files can exercise font, archive and image decoders | Review the shipped native source, malformed-file corpus, and compiled component inventory, not just Rust crate advisories |

This table is **not** a comprehensive, closed CVE audit. Phase 1B.2 must update it with the selected source commit's actual patch status, affected configurations, and any newly reported advisories. The assessment does not assert that MuPDF 1.28.6 fixes every known issue.

## Upgrade approaches

| Option | Assessment |
| --- | --- |
| Official newer `mupdf` / `mupdf-sys` release | **Best long-term choice** if it includes security fixes and passes our target; none newer than 0.8.0 was published at assessment time |
| Pinned fork of `mupdf-sys` 0.8.0, updating native source and adapting the wrapper | **Recommended implementation fallback.** Keeps Ignite's high-level API as stable as possible, but requires explicit wrapper/bindgen/build-script review, submodule pinning and a maintainership plan |
| Independently built system/native MuPDF with existing Rust bindings | **Not recommended initially.** Higher risk of header/ABI mismatch, build/provenance drift, extra shared libraries, and glibc/GLIBCXX incompatibility on Kindle |
| Simply replacing vendored MuPDF files under the published crate | **Reject.** Neither ABI compatibility nor the crate's source/provenance contract is established |

Potential breakpoints include changed native signatures, missing/renamed `fz_*` or `pdf_*` symbols, `bindgen` output, wrapper C code, font-loading hooks, native build flags, and ownership/lifetime assumptions. [MuPDF's API change log](https://mupdf.readthedocs.io/en/1.28.6/other/changes.html) documents a `pdf_validate_signature` API change between 1.27 and 1.28, illustrating why the complete wrapper must be compiled and examined even if Ignite does not call that API.

**Existing downstream patches are an additional blocker:** the `mupdf-sys` 0.8.0 build script performs source-text substitutions for special font fallback and cache reaping, and may only warn if expected patterns are missing. For the selected upgraded tree, audit their necessity and make any required patch application fail closed. Silently skipping a formerly necessary memory/cache fix is not acceptable.

## Native dependency and license considerations

Current [Ignite reader inventory](reader-engine.md#dependencies) identifies FreeType, HarfBuzz, jbig2dec, lcms2, libjpeg, OpenJPEG, zlib and Gumbo in the selected build. The source bundle also has optional Brotli, MuJS, Tesseract, Leptonica, zxing and other components. **Presence in a source package is not proof that a component is compiled.** After upgrading, derive the exact list of compiled libraries and versions from the selected native submodule commits, feature flags and link results; check each component's security advisories and licensing.

MuPDF, `mupdf`, and `mupdf-sys` are AGPL-3.0. Ignite is AGPL-3.0-only, so that is compatible at the headline-license level. It is **not** a substitute for release component attribution, notice packaging, complete corresponding source, third-party license reconciliation, or a redistribution review when the feature first ships. Version upgrades may change the third-party inventory and notices.

Keep embedded fonts and optional native features deliberately constrained. The current `reader` Cargo feature remains **off by default**; ordinary/customer builds and release packaging remain free of MuPDF until a separately approved activation phase.

## Reproducibility and target requirements

The production-relevant baseline is the existing pinned `rust_armv7hf` image in `tools/pinned-docker-images.txt`, currently `rust:1-bullseye@sha256:4730e387a220a08a365c77da3096544dde214f9d796c16284d4be45438cad4a9`. The inherited reader validation links native code with the ARM hard-float toolchain and checks the ELF interpreter, native objects, `DT_NEEDED`, and GLIBC requirements.

The last qualified **1.27.2** baseline required `GLIBC_2.30` and added no shared-library dependencies over the equivalent reader-disabled harness. **Those are regression-comparison baselines, not proof of supported Kindle firmware compatibility.** Do not loosen them or change the pinned image to make the upgrade pass.

Phase 1B.2 must pin or record:

1. Native MuPDF tree commit (candidate `da5f5eb890d7b3fb76903160b5f7f48ba1301492`) and all transitive source/submodule revisions, plus archive checksums when used.
2. The resulting `mupdf-sys` and optional `mupdf` fork commits, reviewed patches, feature switches, `Cargo.lock` source entries and licenses.
3. The pinned Rust/image/toolchain versions, ARM sysroot, static library architecture, ELF/GLIBC baseline and any native runtime dependencies.
4. A verifiable build-to-source mapping and a plan for future CVE refreshes.

## Implementation and acceptance plan (Phase 1B.2)

1. Freeze the current `main` baseline; keep this assessment separate from the native upgrade.
2. Resolve the CVE and upstream-diff questions above. Select an exact upstream commit plus any indispensable verified fixes. Confirm the compiled third-party inventory.
3. Trial the selected native sources in a tightly pinned `mupdf-sys` fork, initially keeping Rust high-level interfaces unchanged. Make necessary source patches deterministic; patch the high-level wrapper only if ABI changes require it.
4. Test source/build reproducibility and packaging isolation; verify PDF, EPUB and CBZ file-backed opening and grayscale rendering, Phase 1A pixel budget, path policy, input errors, reflow, and 16+ open/render/close cycles.
5. Add representative malformed, truncated, image-heavy, compression-bomb and adversarial fixtures with bounded execution and RSS/peak native allocation observations. Document native crashes and OOM rather than treating the 4.40 MiB output buffer limit as a process-memory bound.
6. Rerun the complete host test suite, `tools/ci/check_reader_isolation.sh`, Python QA, pinned `tools/ci/validate_reader_armv7.sh` with `IGNITE_READER_RUN_HOST_TESTS=1`, and all cross-linked ELF/GLIBC/DT_NEEDED/native-object checks. Keep default builds MuPDF-free.
7. Separately qualify the upgraded reader on supported Scribe firmware with runtime loader/library inspection, crash and RSS monitoring, and lifecycle tests before enabling any customer-facing UI.
8. Review component inventory, source distribution, notices, and provenance in a separate packaging gate. No version bump or production deployment is implied by this assessment.

**Phase 1B.2 is blocked until the exact source/patch decision is made and its native validation succeeds.** This document alone does not qualify any upgraded MuPDF binary.

## Evidence

- [Ignite Phase 0 / Phase 1A reader contracts](reader-engine.md)
- [MuPDF official release history](https://mupdf.com/releases/history)
- [MuPDF 1.28.6 interface change log](https://mupdf.readthedocs.io/en/1.28.6/other/changes.html)
- [mupdf-rs v0.8.0 release](https://github.com/messense/mupdf-rs/releases/tag/v0.8.0) and [bundled native submodule](https://github.com/messense/mupdf-rs/tree/v0.8.0/mupdf-sys)
- [Open upstream MuPDF 1.28.2 binding update PR](https://github.com/messense/mupdf-rs/pull/261)
- [Debian MuPDF security tracker](https://security-tracker.debian.org/tracker/source-package/mupdf)
