# MuPDF Rust fork inventory

`mupdf` and `mupdf-sys` are narrowly scoped in-repository forks of the crates.io 0.8.0 release from `messense/mupdf-rs` commit `deaacc87c4cea6a2146e90e00e0d5658c911277b`.

The original crates.io checksums are:

- `mupdf` 0.8.0: `185fb74927a40c569b7152c01c73f4c0206059b8b3d37c7eb007cc3f63453b64`
- `mupdf-sys` 0.8.0: `1c20755e9694e43da4ce8ee2e0211d63883cccd68d314ea4d518c031efee7d8e`

The native `vendor/mupdf-sys/mupdf` submodule is MuPDF 1.28.6 commit `da5f5eb890d7b3fb76903160b5f7f48ba1301492`. Initialize it and its recorded third-party revisions with:

```sh
git submodule update --init --recursive
```

The fork keeps the upstream Rust API at 0.8.0. Its build script applies the required font fallback and store-reaping changes with exact source-pattern checks, backports the CVE-2026-112036 pointer clear, and fails instead of silently continuing when a patch cannot be verified. It also disables native formats and optional components outside Ignite's PDF, EPUB, CBZ, image and SVG reader surface.

The native submodule remains pristine. Patches are applied only to the build copy under Cargo's output directory.

Milestone A adds a fail-closed source patch that gates gzip handler registration off, an explicit
`FZ_ENABLE_GZ=0` build define, and a restricted Rust FFI surface for CFF/font subsetting and
PDF cleaning. It also tracks native source, wrapper and build-support changes for Cargo rebuilds.
These are build-copy and binding changes; they do not modify the pinned native submodule.
The file-signature guard is intentionally limited to PDF and ZIP initial signatures and does
not guarantee MuPDF selects a particular parser from a valid ZIP container.
