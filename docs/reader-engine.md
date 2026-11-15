# Reader engine (experimental, opt-in)

Status: **build plumbing and validation only.** There is no reader UI. Nothing in the app calls
the engine, and no customer build contains it. Background and plan:
[markrai/ignite#22](https://github.com/markrai/ignite/issues/22).

## What is here

- `scribe-app` has a Cargo feature, `reader`, that is **off by default**. It pulls in the
  [`mupdf`](https://crates.io/crates/mupdf) crate, which builds MuPDF from source.
- `ignite/kindle/scribe-app/src/reader_engine.rs` (compiled only with the feature) opens a
  document from memory, lays out reflowable formats, and renders a page as 8-bit gray scaled to
  fit a box. Its tests render a generated PDF page and check that reflowable text repaginates
  when the font size changes. EPUB goes through the same HTML layout engine.
- CI job **Reader engine (experimental, opt-in)** in `.github/workflows/ci.yml`. It runs
  separately from the release jobs:
  1. `tools/ci/check_reader_isolation.sh`: `reader` is not a default feature, no packaging or
     release script enables it, and without it `mupdf` is not in `scribe-app`'s dependency graph
     (host, and Kindle target with the release features).
  2. The `reader_engine` tests on the host.
  3. `cargo check` for `armv7-unknown-linux-gnueabihf` with the feature on.

Out of scope for now: book identity, reading positions, book folders, UI, packaging and notices
in the release payload.

## Building

Host (Linux):

```sh
sudo apt-get install clang libclang-dev make          # bindgen needs libclang
cargo test -p scribe-app --lib --features reader reader_engine
```

Kindle target (same cross toolchain as the `armv7-check` job, plus clang):

```sh
sudo apt-get install clang libclang-dev make gcc-arm-linux-gnueabihf g++-arm-linux-gnueabihf
LD=arm-linux-gnueabihf-ld AR=arm-linux-gnueabihf-ar \
BINDGEN_EXTRA_CLANG_ARGS_armv7_unknown_linux_gnueabihf=--sysroot=/usr/arm-linux-gnueabihf \
cargo build -p scribe-app --release --target armv7-unknown-linux-gnueabihf \
  --features scribe-app/restore_from_server,scribe-app/reader
```

MuPDF's Makefile embeds its fonts with `$(LD) -r -b binary` and archives with `$(AR)`. Without
the ARM `LD` and `AR` the host tools produce x86 objects and the link fails.

The first build compiles MuPDF and its bundled libraries (a few minutes); later builds reuse it.

## Dependencies

All of these are compiled from source and linked statically, and only with `--features reader`.

| Component | Version | Licence | Notes |
|---|---|---|---|
| `mupdf` crate | 0.8.0 (pinned `=0.8.0`) | AGPL-3.0 | Safe Rust wrapper |
| `mupdf-sys` crate | 0.8.0 | AGPL-3.0 | Build script and bindings; build-time `bindgen`, `cc` |
| MuPDF | 1.27.2 (vendored in `mupdf-sys`) | AGPL-3.0 | Features: PDF, EPUB, HTML, images, SVG, CBZ, base-14 fonts |
| FreeType | 2.13.3 | FreeType Licence (FTL) | Bundled by MuPDF |
| HarfBuzz | 6.0.0 (as vendored) | MIT ("Old MIT") | Bundled by MuPDF |
| jbig2dec | 0.20 | AGPL-3.0 | Bundled by MuPDF |
| Little CMS (`lcms2mt`, Artifex fork) | 2.16 | MIT | Bundled by MuPDF |
| libjpeg (IJG) | 9f | IJG licence | Bundled by MuPDF |
| OpenJPEG | 2.5.x | BSD-2-Clause | Bundled by MuPDF |
| zlib | 1.3.1 | zlib | Bundled by MuPDF |
| Gumbo HTML parser | (as vendored) | Apache-2.0 | Bundled by MuPDF; EPUB and HTML |

Not built with this feature set: MuJS (JavaScript), Tesseract, Leptonica, Brotli, `extract`
(DOCX output), zxing-cpp, curl, X11/GLUT viewers. MuPDF's own fonts are limited to the base-14
set.

Versions are those vendored in `mupdf-sys` 0.8.0. Ignite is AGPL-3.0-only, so the AGPL
components add no new distribution terms; the release notices and the component inventory still
need entries before any build containing the reader ships.

## Before this reaches users (open)

- **Security review and upgrades.** MuPDF 1.27.2 is behind current upstream, which has since
  received memory-safety fixes. Needed: a review of the fixes since 1.27.2 and of the bundled
  libraries above, then either a `mupdf-sys` release with a newer MuPDF or a pinned
  `[patch.crates-io]` with the newer MuPDF sources. The exact `=0.8.0` pin keeps upgrades
  deliberate.
- **Packaging:** component inventory, provenance and notices in the release pipeline.
- **EPUB anchors:** a proof of concept that a saved location survives font size and margin
  changes, before choosing an anchor representation.
- **Book identity:** separate a file's identity (content hash) from the logical book it holds.
