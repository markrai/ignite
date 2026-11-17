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
     or ordinary ARM executable. The enabled graph must contain both `mupdf` and `mupdf-sys`.
  2. The `reader_engine` tests execute MuPDF rendering and reflow on the Linux host.
  3. `tools/ci/validate_reader_armv7.sh` links an ordinary ARM executable without MuPDF, an
     equivalent reader-disabled library test executable, and the feature-enabled library test
     executable in the pinned Kindle Docker image. The ARM executables are inspected but never
     run on the x86 host.

Out of scope for now: book identity, reading positions, book folders, UI, packaging and notices
in the release payload.

## Building

Host tests (Linux):

```sh
sudo apt-get install clang libclang-dev make          # bindgen needs libclang
cargo test -p scribe-app --lib --locked --features reader reader_engine
```

Complete ARM link and compatibility validation from the repository root:

```sh
image="$(sed -n 's/^rust_armv7hf=//p' tools/pinned-docker-images.txt | tr -d '\r')"
case "$image" in *@sha256:*) ;; *) exit 1 ;; esac
docker run --rm -v "$PWD:/src" -w /src "$image" \
  bash /src/tools/ci/validate_reader_armv7.sh
```

For a one-shot local run that also executes the Linux host rendering tests inside the disposable
container, add `-e IGNITE_READER_RUN_HOST_TESTS=1` before the volume argument. CI keeps the host
tests as a separate step and uses the script for ARM linking and inspection.

The script installs its build-only dependencies inside the disposable pinned Bullseye container:
Clang/libclang for bindgen, `make`, Python 3, and Debian's ARM GNU hard-float GCC, G++, binutils,
and sysroot. Package metadata and files come only from Debian snapshots timestamped
`20260824T000000Z`. That timestamp is the source snapshot recorded in the image built on August
25, 2026; it contains the verified Bullseye packages after the live security repository began
advertising package revisions that returned HTTP 404. Release-file expiration is disabled only on
those historical snapshot source entries. The script uses a private apt list directory, does not
change host package sources, and fails if snapshot update or installation fails.

The script configures target C and C++ compilers, Cargo's linker, `LD`, `AR`, `RANLIB`, and
bindgen's ARM target/sysroot. MuPDF embeds resources with `$(LD) -r -b binary` and archives with
`$(AR)`; host binutils would create incompatible objects. The ARM validation path statically links
the C++ runtime so the reader does not add an unqualified Kindle `libstdc++`/`GLIBCXX` dependency.

The two test-harness link commands inside the script use the same target, debug test profile, and
release feature. The baseline omits only `scribe-app/reader`; the reader command adds it:

```sh
cargo test -p scribe-app --lib --locked --no-run \
  --target armv7-unknown-linux-gnueabihf \
  --features scribe-app/restore_from_server \
  reader_engine

cargo test -p scribe-app --lib --locked --no-run \
  --target armv7-unknown-linux-gnueabihf \
  --features scribe-app/restore_from_server,scribe-app/reader \
  reader_engine
```

`--no-run` is intentional: it produces the real Rust test executable, including MuPDF's native
static libraries, without attempting to execute an ARM binary on x86. The verifier requires the
linked executable to contain both the rendering test and MuPDF implementation symbols.

## ARM ELF compatibility checks

`tools/ci/verify_reader_armv7_elf.py` reuses the release ELF parser in `tools/qa/elf.py` and the
cross-toolchain's `readelf`, `objdump`, and `nm`. It fails unless all of the following hold:

- the executable is little-endian ELF32 for ARM with the hard-float EABI flag and VFP-register
  calling convention;
- the interpreter is `/lib/ld-linux-armhf.so.3`;
- every `DT_NEEDED` library resolves in the ARM sysroot and is itself ELF32 ARM hard-float;
- the reader test introduces no shared-library dependency beyond the equivalent reader-disabled
  test harness built with the same profile and features; the separate ordinary application remains
  the feature-isolation artifact;
- the reader test introduces no higher GLIBC requirement than the reader-disabled test baseline;
  the verifier also reports the maximum for the ordinary application and both test harnesses;
- the linked file contains the reader rendering test and defined MuPDF `fz_*` symbols; and
- every native archive/object in the ARM Cargo build, including MuPDF's libraries, reports
  `elf32-littlearm`, preventing host objects from passing unnoticed.

The measured maximum is `GLIBC_2.30` for all three artifacts. This is build-comparison evidence,
not a universal Kindle runtime policy. Dropbear's `GLIBC_2.28` pin describes that specific binary
and is not used as a `scribe-app` ceiling. Device-runtime compatibility remains explicitly
unqualified until a dedicated requirement is established from supported Kindle hardware.

The script also links an ordinary ARM `scribe-app` with only the official
`scribe-app/restore_from_server` feature and requires its target output and symbol table to remain
free of MuPDF and the reader module. Validation outputs live outside the customer release target
and are never packaged or uploaded as customer artifacts.

## Validation record

On 2026-11-16, the corrected validation was executed on Linux against the hardening working tree:

- `tools/ci/check_reader_isolation.sh`: passed for host and ARM dependency graphs, disabled and
  enabled directions.
- Linux `reader_engine` tests: 7 passed, including MuPDF PDF rendering and reflow pagination.
- The digest-pinned Bullseye validator completed with Rust 1.98.0, ARM GCC 10.2.1, GNU ld 2.35.2,
  and Clang 11.0.1. The ordinary application and both ARM test harnesses linked successfully.
- All three linked files were little-endian ELF32 ARM hard-float with VFP-register arguments and
  interpreter `/lib/ld-linux-armhf.so.3`.
- The reader-enabled and reader-disabled test harnesses had identical `DT_NEEDED` entries:
  `libgcc_s.so.1`, `libpthread.so.0`, `libm.so.6`, `libdl.so.2`, `libc.so.6`, and
  `ld-linux-armhf.so.3`. The ordinary application omitted only `libm.so.6`. Every library resolved
  to an ELF32 ARM hard-float file in the cross sysroot.
- The maximum required GLIBC version was `GLIBC_2.30` for the ordinary application and both test
  harnesses. The reader added no higher GLIBC requirement than its equivalent baseline.
- The linked reader test and MuPDF implementation symbols were present. The ordinary executable
  contained no reader or MuPDF symbols or outputs.
- The reader target contained 467 inspected native archives/objects, including `libmupdf.a`; every
  reported format was `elf32-littlearm`.
- `python tools/qa/test_ignite_qa.py`: 32 tests passed, including negative cases for a reader-only
  shared library, a higher reader GLIBC requirement, and Rust v0 linked-symbol detection.
- Python compilation, Ruff 0.14.3 lint and format checks, ShellCheck 0.7.1, Bash syntax, and
  `git diff --check`: passed.
- Windows host `reader_engine` test build: not qualified. MuPDF's MSBuild step stopped with local
  Visual Studio error `C1902 Program database manager mismatch` before Rust tests ran.

No physical Kindle test has been performed for this reader implementation. The automated result
qualifies the pinned Linux build path and differential ABI checks only; supported-device runtime
compatibility and behavior still require evidence collected from Kindle hardware.

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

## Remaining limitations

- **Security review and upgrades.** MuPDF 1.27.2 is behind current upstream, which has since
  received memory-safety fixes. Needed: a review of the fixes since 1.27.2 and of the bundled
  libraries above, then either a `mupdf-sys` release with a newer MuPDF or a pinned
  `[patch.crates-io]` with the newer MuPDF sources. The exact `=0.8.0` pin keeps upgrades
  deliberate.
- **Production distribution and licensing notices:** component inventory, provenance, and notices
  in the release pipeline remain unresolved. Reader artifacts are experimental validation output.
- **EPUB anchors:** a proof of concept that a saved location survives font size and margin
  changes, before choosing an anchor representation.
- **Book catalog and stable identity:** separate a file's identity (content hash) from the logical
  book it holds; no storage or catalog is part of Phase 0.
- **Reading-position persistence:** no EPUB anchors or stable reading-position format exists.
- **Reader UI:** no UI or application call site exists.
- **Hardware acceptance:** host and cross-compile evidence is not physical Kindle validation. The
  new implementation has not been accepted on supported hardware or firmware.
