# MuPDF Phase 1B: security and upgrade assessment

Status: **Phase 1B.2 Linux and ARMv7 validation passed; Kindle runtime remains unqualified**
(2026-11-19). Tracks [Ignite #22](https://github.com/markrai/ignite/issues/22).

## Decision

Ignite now selects MuPDF **1.28.6** at upstream tag and commit
[`da5f5eb890d7b3fb76903160b5f7f48ba1301492`](https://github.com/ArtifexSoftware/mupdf/commit/da5f5eb890d7b3fb76903160b5f7f48ba1301492).
The source is a recursive Git submodule, so the native tree and every third-party submodule are
immutable revisions rather than a floating tag or branch. The native submodule is kept pristine.

No maintained Rust release newer than `mupdf`/`mupdf-sys` 0.8.0 was available on 2026-11-19.
The latest released bindings and their `main` branch still selected MuPDF 1.27.2; upstream PR
[#261](https://github.com/messense/mupdf-rs/pull/261) only proposed 1.28.2. Ignite therefore uses
narrow in-repository forks of the two 0.8.0 crates under `vendor/`, sourced from
`mupdf-rs` commit `deaacc87c4cea6a2146e90e00e0d5658c911277b`. The root
`[patch.crates-io]` entries select those paths while the application keeps its exact
`mupdf = "=0.8.0"` API pin.

The fork contains two necessary changes:

- initialize MuPDF 1.28's new `fz_stext_options.table_hunt_options` field; and
- apply the existing font fallback and store-lifetime fixes plus the security patch below to a
  disposable build copy, with exact source-pattern checks that stop the build if the expected old
  or already-patched form cannot be proved.

This is the smallest maintainable integration that does not require publishing an external fork.
The pinned Linux and ARMv7 validator now passes; physical Kindle runtime remains unqualified.

## Security decisions

The selected release was assessed at source and commit-ancestry level. A newer version number was
not treated as proof that a fix was present.

| Finding | 1.28.6 source decision | Ignite disposition |
|---|---|---|
| **CVE-2026-112036**, signature-locking `free_char_list` use-after-free/double-free | Upstream fix `3984925b660afb8a781ddf504021e91be1bc671e` is not an ancestor of the selected release commit. The vulnerable function frees `c->list` without clearing it. | The build script applies the exact one-line pointer clear to its copied source and fails closed if the source pattern changes. Signature locking is not needed for rendering, but the PDF object code is part of the native library, so reachability was not used to excuse it. |
| **CVE-2026-7233**, CFF subsetting out-of-bounds read | The affected `fz_subset_cff_for_gids` implementation remains in 1.28.6 and Debian still records no fixed release. Its callers are PDF/TTF subsetting and writing paths, not Ignite's open/layout/render operations. | The source issue is **not claimed fixed**. The ARM verifier fails if `fz_subset_cff_for_gids` is linked into the reader executable, proving exclusion from the delivered static link. A future writer/subsetter feature must first supply an audited fix. |
| **CVE-2026-3308**, PDF image stride integer overflow and heap write | Fix `a26f0142e7d390d4a82c6e5ae0e312e07cc4ec85` is an ancestor of the selected commit. It is not an ancestor of Ignite's old 1.27.2 release branch. | Fixed by the selected source. This path is relevant to normal PDF image rendering. |
| **CVE-2026-92413**, external PDF stream null dereference | The named fix `3df1e30f9d7b77260e13bd0dbe1928ddeba8386e` is not an ancestor because it was made on a divergent development branch. The selected 1.28.6 release implementation of `pdf_open_filter` has no external-stream/archive branch or vulnerable null return. | Unreachable because the affected implementation is absent, not because of release chronology. |
| MuPDF 1.28.x hardening | 1.28.0 through 1.28.6 include the upstream parser, image, pixmap, font, overflow, decompression-bomb and robustness work recorded in the release history. | Exact source remains pinned. Malformed, truncated and image-heavy reader tests are bounded by a 120-second validator timeout; a pass must also report peak RSS. |
| PyMuPDF path traversal **CVE-2026-82035** | This is in PyMuPDF's Python `extract_objects` API, not the MuPDF C library or the Rust bindings. | Not present in Ignite's dependency graph or compiled configuration. |

The Debian source tracker still lists several old denial-of-service findings as unimportant or
unresolved across packaged MuPDF versions. This review does not declare the native parser free of
unknown or all denial-of-service defects. The reader remains experimental, off by default, and
must process untrusted documents under device-level memory and crash observation.

## Compiled configuration

Ignite enables only PDF plus the EPUB/HTML, image, SVG and CBZ support required by Phase 1A and
base-14 fonts. MuPDF 1.28's new defaults would otherwise enable extra document formats and
cmark-gfm. The fork explicitly disables FB2, Markdown, MOBI, Office, plain-text, OCR output and ODT
output, as well as MuJS, Brotli, DOCX extraction, Tesseract, zxing-cpp, libarchive and cmark-gfm.
The ARM verifier rejects linked MuJS, Brotli, cmark-gfm, Tesseract and zxing symbols.

The recursively pinned native components used by this configuration are:

| Component | Declared version | Exact source revision | Licence |
|---|---|---|---|
| MuPDF | 1.28.6 | `da5f5eb890d7b3fb76903160b5f7f48ba1301492` | AGPL-3.0 |
| FreeType | 2.14.3 | `0a0221a1347e2f1e07c395263540026e9a0aa7c7` | FreeType Licence |
| HarfBuzz | 13.0.1 | `c28ba35a1e6dc4b6b32e2b1d11fd2a408f6c86bb` | Old MIT |
| jbig2dec | 0.20 | `dc15c39bbbddc90f79c14563d2eb5a794106be8f` | AGPL-3.0 |
| Little CMS, Artifex multithreaded fork | 2.19.1.art | `d69c64417c4a33a4629957fc0e08f4f4c5abcc3d` | MIT |
| libjpeg, IJG | 10 | `627faae398468d5d11e3d694dc325432f734b5c4` | IJG |
| OpenJPEG | 2.5.3 declared | `957029eb875eee1118743f200cb86da9d8289de2` | BSD-2-Clause |
| zlib | 1.3.2 | `da607da739fa6047df13e66a2af6b8bec7c2a498` | zlib |
| Gumbo parser | 0.10.1 lineage | `61bd4e736c3b342e592f65be0d65ec6a0acd8dcb` | Apache-2.0 |

Brotli 1.2.1 and cmark-gfm 0.29.0.gfm.13 source revisions are present in the recursive upstream
tree but are disabled and must not contribute linked symbols. Presence in the source checkout is
not represented as presence in the binary. MuPDF, the Rust crates and jbig2dec are AGPL-compatible
with Ignite's AGPL-3.0-only licence, but a future shipped reader still needs corresponding-source,
notice and complete component-inventory review.

## Reproducibility

The crates.io sources used to create the in-repository fork have these checksums:

- `mupdf` 0.8.0:
  `185fb74927a40c569b7152c01c73f4c0206059b8b3d37c7eb007cc3f63453b64`
- `mupdf-sys` 0.8.0:
  `1c20755e9694e43da4ce8ee2e0211d63883cccd68d314ea4d518c031efee7d8e`

From a fresh checkout:

```sh
git submodule update --init --recursive
cargo test -p scribe-app --lib --locked --features reader reader_engine
IGNITE_READER_RUN_HOST_TESTS=1 bash tools/ci/validate_reader_armv7.sh
```

The last command is intended for the digest-pinned
`rust:1-bullseye@sha256:4730e387a220a08a365c77da3096544dde214f9d796c16284d4be45438cad4a9`
container. It checks the exact native revision and every recorded recursive submodule before
installing tools from the timestamped Debian snapshot. The Docker image pin and the established
`GLIBC_2.30` differential baseline are unchanged.

## Validation status

Local source and Python checks can run on the available Windows host. The native MSVC compile
reached the deterministic patch stage, proving all required source patterns matched exactly once,
then stopped in the host toolchain with repeated `C1902 Program database manager mismatch`
errors. Docker Desktop was unavailable and `wsl.exe -l -v --all` timed out, so Linux/ARM
qualification could not run on that Windows host. It subsequently passed on Dragonfly.
The ordinary `scribe-app` library suite passed with 1,583 tests and 29 explicit ignores, and the
disabled/enabled dependency and packaging-isolation checks passed.

The subsequent 2026-11-19 pinned-container run on Dragonfly against commit `c2abb4c` passed:

- `mupdf_version=1.28.6` and `reader_host_tests=passed`;
- `reader_host_peak_rss_kib=40176` (39.2 MiB, Linux host test process);
- `reader_needed_added=none` and `reader_max_glibc=GLIBC_2.30`;
- `reader_cve_2026_7233_subsetting_symbol=absent` and `READER_ARMV7_ELF_OK`;
- reader isolation passed, and Python QA passed all 35 tests.

The validator exited with code 0. Raw log: `/home/dragonfly/ignite-reader-phase1b2-qualified.log`.
SHA-256: `1512c16c3289b954a570584a6b838db3cafb84d569f4cac7d9b93aafb9f9476c`.
This qualifies the build and host tests; the reader remains disabled by default, and Kindle
runtime qualification is still required.

No physical Kindle test has been performed. Even after Linux and ARM build qualification, supported
Scribe firmware still needs loader/library inspection, RSS and crash monitoring with malformed and
image-heavy documents, repeated lifecycle testing, and confirmation that ordinary customer
packaging remains unaffected. The 4,612,800-byte Gray8 cap is not a process-wide native-memory cap.

## Evidence

- [Ignite reader contracts and validation history](reader-engine.md)
- [MuPDF release history](https://mupdf.com/releases/history)
- [MuPDF 1.28.6 API changes](https://mupdf.readthedocs.io/en/1.28.6/other/changes.html)
- [mupdf-rs v0.8.0](https://github.com/messense/mupdf-rs/releases/tag/v0.8.0)
- [Open mupdf-rs 1.28.2 update PR](https://github.com/messense/mupdf-rs/pull/261)
- [Debian MuPDF security tracker](https://security-tracker.debian.org/tracker/source-package/mupdf)
- [CVE-2026-112036](https://security-tracker.debian.org/tracker/CVE-2026-112036)
- [CVE-2026-7233](https://security-tracker.debian.org/tracker/CVE-2026-7233)
- [CVE-2026-3308](https://security-tracker.debian.org/tracker/CVE-2026-3308)
