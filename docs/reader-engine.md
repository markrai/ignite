# Reader engine (experimental, opt-in)

Status: **controlled file-backed engine testing only.** There is no reader UI. Nothing in the app
calls the engine, and no customer build contains it. Background and plan:
[markrai/ignite#22](https://github.com/markrai/ignite/issues/22).

## What is here

- `scribe-app` has a Cargo feature, `reader`, that is **off by default**. It pulls in Ignite's
  source-pinned `mupdf` 0.8.0 fork, which builds MuPDF 1.28.6 from source.
- `ignite/kindle/scribe-app/src/reader_engine.rs` (compiled only with the feature) opens existing
  PDF, EPUB and CBZ files by path without reading the complete file into a Rust buffer. It retains
  `Book::open_bytes` for generated test fixtures, lays out reflowable formats, and renders a page
  as bounded 8-bit gray output. Its tests exercise generated PDF, EPUB and CBZ files, reflow,
  corrupt inputs, path policy, render limits and repeated open/render/close cycles.
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

Milestone B adds a separate release-optimized ARMv7 `reader-probe` binary behind the same
feature gate. It is not packaged for customers or activated by the Ignite app. Its pinned
cross-build, ELF verification, read-only corpus boundary and physical QA restrictions are
documented in [reader-probe-milestone-b.md](reader-probe-milestone-b.md). The
original probe has passed one supervised, isolated PDF, EPUB and CBZ smoke test
on firmware 5.18.6; a bounded multipage probe mode is being developed next.
These results do not qualify reader integration into the production app.

An experimental reading-position layer (`reader_engine/location.rs`, milestones C.1 and C.1.1)
turns a page into a layout-independent content location and restores it after relayout or
reopening. See [Reading positions](#reading-positions-experimental) below.

Out of scope for now: discovery, computing and caching book identities, storing reading positions,
book folders, UI, packaging and notices in the release payload.

## Building

Host tests (Linux):

```sh
git submodule update --init --recursive
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
container, add `-e IGNITE_READER_RUN_HOST_TESTS=1` before the volume argument. CI performs a
separate host test and also sets that variable for the pinned validator so the raw validator log
contains bounded host execution and peak-RSS evidence.

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

## Phase 1A engine contracts

### File-backed opening

`Book::open_path` accepts one explicit existing path; it does not scan directories, discover
books, create identities, or copy a document into Ignite storage. The extension must be PDF,
EPUB or CBZ (ASCII case-insensitive), and the input must remain a regular file. Before invoking
MuPDF, the reader checks at most 1024 initial bytes from its already-open file handle: PDF inputs
must start with `%PDF-`, and EPUB/CBZ inputs must start with the ZIP local-file header
`PK\x03\x04`. A mismatch is rejected before native parsing; empty files and gzip streams fail
this guard. These strict signatures intentionally reject some unusual files MuPDF might otherwise
accept, including PDFs with a preamble. Missing files, directories, special files, unsupported or
non-UTF-8 extensions, corrupt content and MuPDF errors are returned as path-specific reader errors.

This guard does not prove that MuPDF will choose the expected parser after sniffing, and it cannot
distinguish EPUB from CBZ because both use ZIP. The native build additionally removes the
unconditional gzip handler registration by a checked source patch and configures `FZ_ENABLE_GZ=0`.
Its original gzip source remains in the upstream tree, but the document dispatcher does not
register it. `Book::open_bytes` remains a test-fixture interface and does not use the path guard.

The path policy rejects a symlink in any component, not only a symlink at the final filename. It
checks the path before opening, opens a read-only file handle, compares that handle with the path,
lets MuPDF open its own stream, then checks the path and identity again. On Unix, including the
Kindle target, the identity comparison includes device, inode, length, modification time and
change time. Other hosts compare the metadata available through the portable Rust API: length and
modification time.

MuPDF Rust 0.8.0 only exposes pathname-backed `Document::open`; it cannot accept the already-open
Rust file descriptor. The checks therefore narrow but cannot eliminate the race between pathname
inspection and MuPDF's native open. They also cannot make a file safe against concurrent in-place
writes or an attacker that can replace and restore a path entirely between checks. Controlled
tests must use a stable, trusted file tree. A future storage design should use a descriptor-backed
native API or a storage-owned immutable snapshot if it needs a stronger guarantee. Phase 1A does
not silently make such a snapshot.

`Book::open_bytes` remains available for small generated fixtures. It still copies the complete
input into MuPDF-owned memory and is not the real-document path.

### Render and layout bounds

The experimental gray output budget is one complete Scribe visible panel:
`1860 * 2480 = 4,612,800` pixels and bytes, about 4.40 MiB. Each requested dimension is also
limited to the Scribe long edge, 2,480 pixels, so a thin request cannot use an extreme dimension
while staying under the byte cap. Landscape requests up to 2480 by 1860 remain possible.

Before `Page::to_pixmap` can allocate the native destination, the engine:

- checks the requested dimensions, pixel multiplication and platform-size conversions;
- requires finite, ordered page bounds and a finite positive fit scale;
- transforms the page bounds and applies MuPDF's own rounding operation;
- reduces the scale when rounded edges would exceed the requested box;
- checks the rounded width, height, Gray8 stride and total sample bytes against the limits; and
- records the expected origin, geometry, component count, alpha state, stride and sample length.

After rendering, every recorded property must match the returned pixmap. The `GrayPage` owns that
native pixmap and exposes a borrowed packed-gray slice, avoiding a second full-size Rust buffer.
Reflow layout accepts width and height from 1 through 4,096 points and an `em` size from 1 through
512 points; non-finite and out-of-range values fail before entering MuPDF. Those layout ceilings
are deliberately generous relative to the measured 418 by 528 point Scribe safe area and normal
font sizes. They are input-sanity limits, not a claim that EPUB layout memory is bounded.

The 4.40 MiB limit bounds only the final Gray8 destination. It does not bound decoded source
images, display lists, fonts, EPUB layout state, transparency groups, masks, MuPDF caches, parser
state or temporary native allocations. A malicious or unusually complex document can still cause
large native allocations, process termination by the kernel, or a native crash. Rust errors are
recoverable where MuPDF returns an error, but the engine does not represent a process crash or
out-of-memory kill as a successful reader result.

## Reading positions (experimental)

Status: **engine API and host tests only.** C.2A defines the bounded JSON record
encoding; C.2B adds an opt-in, local position-store library with crash-recovery tests.
Nothing calls the store from the Kindle app; no device files or notebook data are touched.
Reader UI and sync integration are still absent.

### Why not MuPDF bookmarks

`fz_make_bookmark` cannot be persisted. For EPUB it returns the address of a laid-out text node
(`fz_make_html_bookmark`), and lookup compares addresses while laying out every chapter. It follows
relayout only while the chapter's parsed tree stays in MuPDF's store. After store eviction the
chapter is re-parsed into new nodes and the bookmark is lost. After the document is closed, a reused
address can match an unrelated node. For fixed-layout formats it is a packed page number.

The fork therefore adds only two error-safe wrappers, `mupdf_count_chapters` and
`mupdf_count_chapter_pages`, following the existing `fz_try`/`mupdf_save_error` convention. They
return plain integers, allocate nothing and borrow the document. MuPDF's single-chapter handlers
(PDF, CBZ, images, SVG, plain HTML) ignore the chapter index and return the whole page count, so
the page wrapper checks `0 <= chapter < fz_count_chapters` itself and returns 0 otherwise.

### Model

Three concepts are kept separate:

- `DocumentIdentity { sha256, len }`: which bytes a position belongs to. It is owned by the open
  `Book` (see below), never passed in by the caller of `restore`.
- `ContentLocation`: `FixedPage(page)` for PDF and CBZ, `Text(TextAnchor)` for EPUB.
- The current page index, valid only for the current layout. `Book::restore` returns it; it is
  never part of a saved record.

A `TextAnchor` holds a text-model identifier, the EPUB spine index, an offset and up to 32 units of
context. Units are MuPDF structured-text characters except whitespace and soft hyphens, plus
`U+FFFC` for each image. Line breaks add and remove whitespace, and MuPDF reports a word broken at
a soft hyphen as ending in `U+00AD`, so both are excluded. The source text's own `U+FFFC` is not a
unit, so the image unit cannot be forged by text. No Unicode normalisation is applied.

`SavedPosition { format, document, location }` is the record a future store would hold. All fields
are owned and fixed-size or bounded. `Book::save_position(page)` creates one for the book's own
identity.

### Position serialization (C.2A)

`SavedPosition::to_json_bytes()` and `SavedPosition::from_json_bytes(bytes)` encode and
decode the existing engine location model, without accessing the filesystem or MuPDF.
Records are UTF-8 JSON, with a strict maximum of 4,096 bytes including whitespace.
Version 1 uses this schema:

```json
{
  "format": 1,
  "document": {
    "sha256": "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a",
    "len": 8765
  },
  "location": {
    "kind": "fixed_page",
    "page": 42
  }
}
```

For EPUB, `location` is
`{"kind":"text","chapter":3,"offset":170,"text_model":"ignite-units-1/mupdf-1.28.6","context":"..."}`.
The JSON document hash is exactly 64 lowercase hexadecimal characters; `len`
is a nonzero unsigned integer. Location and format numbers are checked for their
declared integer widths, and neither page numbers nor offsets are claimed valid
until `Book::restore()` checks the opened document.

Decoding rejects missing or duplicate fields, unknown fields, unknown location
kinds, malformed JSON, invalid UTF-8, invalid hashes and out-of-bounds anchors.
A record with an unrecognized `format` is rejected explicitly as
`UnsupportedFormat`, including when the future schema differs. Text model and
context are validated under the engine's existing anchor rules before a record
can be used. Encoding performs the same checks and emits deterministic compact
JSON with a fixed field order.

C.2A is a data format only, not a persistence guarantee. It does not choose a
storage path, handle atomic writes, migrate legacy records, or decide recovery
from corrupt files. Those are C.2B responsibilities; a rejected record must not
silently replace a previously valid position. The stored record contains
content identity and content location, never current-layout page numbers for
reflowable documents or a trusted file-system stamp.

### Device-local position store (C.2B)

`PositionStore::new(root)` takes an explicit, trusted, app-owned directory; neither
the reader engine nor the store automatically chooses a production path or opens
the Kindle notebook library. The eventual integration should use a reader-only
directory under the existing application state, separate from ebooks and
`/mnt/us/scribe-data`. The parent directory must already exist. `load(identity)`
does not create folders; `save(position)` creates its leaf directory on Unix.

Each document is indexed by its content SHA-256 and byte length, not filename.
There are exactly two recoverable record slots per document:
`<sha256>-<byte-length>.slot0` and `.slot1`. Each slot contains a strict JSON
envelope (version 1) with a monotonically increasing generation, the complete
C.2A `SavedPosition` object and a SHA-256 checksum over the generation and
the exact encoded position JSON, with a fixed domain separator. The slot size
is bounded to 4,608 bytes. The checksum detects accidental corruption; it is
**not** authentication against a malicious local writer.

`save` acquires an advisory Unix `flock` on the persistent `.writer-lock`,
reads both slots and chooses the highest valid generation. It writes the next
generation to the *other* slot using a private, exclusive-create temporary
file in the same directory. The operation writes and syncs that file, renames
it over its target slot and syncs the directory before reporting success.
The first creation of the directory also syncs the parent. It never
overwrites the last valid slot during an update. Lock files remain present;
temporary names left by a process crash are ignored, not promoted or applied.
This design targets the Linux Kindle and host, not Windows atomic-replace
semantics.

`load` verifies bounded size, JSON schema, generation, checksum, C.2A fields
and exact document identity for both slots. It returns the highest-generation
valid record if one exists, even if the other slot is corrupt or missing.
Missing slots mean no position. Existing slots with no valid record produce
an error, not `None`. Equal valid generations are a conflict. An unsupported
envelope format is a hard error rather than a silent downgrade/overwrite.
Unreadable files and unsafe leaf symlinks also fail closed. Concurrent readers
may observe the old or new committed generation, never a partially written
slot. Concurrent Unix writers serialize through the advisory lock.

**Limitations:** This is a storage component, not a reader-session feature.
No page-turn hook, debounce, retry, rendering or autosave behavior exists yet;
C.2C owns the save/resume flow. The filesystem's real directory-sync,
replacement and lock behavior on Kindle user storage has not been physically
qualified. A metadata/data loss at the filesystem layer, two independently
corrupted slots, in-place file tampering, and storage-owner path races remain
outside the guarantee. There is no guarantee against hostile local writers,
and the store does not repair corrupted slots automatically or replace a
rejected record with a fallback reading position.

Host-only tests cover two-slot ordering, corruption recovery, ambiguous and
future-format rejection, writer concurrency, interrupted writes at each
durability boundary, symlink refusal and document isolation. Ordinary
production builds still exclude the reader feature and its store.

### Document identity ownership

`Book::open_path_identified(path, known)` binds the identity while opening. The existing path
checks already open one read-only handle and compare its device, inode, length and modification
and change times with the path before and after MuPDF opens the file. The identity is hashed from
that same handle, after the signature check and before MuPDF opens the path, and the handle is
compared with its original metadata again after hashing. The resulting guarantees:

- **Replacement before or during opening** (rename over the path, a write, truncation) changes the
  inode or timestamps and fails the open. A file replaced after opening does not affect the open
  `Book`: on Linux MuPDF keeps reading the inode it opened, which is the one that was hashed.
- **Concurrent in-place writes** after the final check remain possible, as already documented for
  the path policy. They can make MuPDF read bytes other than the hashed ones. Storage that needs a
  stronger guarantee must give the reader an immutable file.
- **Reopening** always produces a fresh identity. Nothing is carried over from an earlier `Book`.
- **Avoiding repeated hashing:** `Book::known_identity()` returns the identity together with the
  file's `FileStamp` (device, inode, length, modification and change times). A later open that
  receives this `KnownIdentity` skips hashing only if the newly opened handle has exactly the same
  stamp. Otherwise it hashes again. That shortcut trusts the filesystem to change the stamp
  whenever content changes. A stamp whose change time is less than 3 seconds old when it is read is
  never offered for reuse, because a rewrite within the same timestamp tick (coarse kernel clocks,
  2-second FAT times) might not change it; the next open simply hashes again. Clock changes on the
  device and filesystems with unstable inode numbers can only cause extra hashing, except for the
  theoretical case of a same-length rewrite landing on an identical earlier timestamp.
  Non-Unix hosts have no stamp and always hash.
- `Book::open_path` and `Book::open_bytes` have no identity. `save_position` fails for them and
  `restore` returns `Rejected(DocumentUnidentified)`.

Hashing reads the whole file once per uncached open in 64 KiB blocks (constant memory). On the
Dragonfly host SHA-256 (the `sha2` crate as configured) hashed 64 MiB in 0.32 s, about 200 MiB/s.
The Kindle's ARMv7 CPU will be slower, so a large book may take seconds to hash; device throughput
must be measured before deciding when the reader hashes, and stamp reuse is what keeps reopening
cheap.

### Restoration policy

`Book::restore(saved)` returns:

| Outcome | When | Position |
|---|---|---|
| `Exact { page, position }` | Same identity and text model, and the context is at the stored offset. Also a chapter start (offset 0, empty context) under any text model, and a fixed page in range. | The saved record, unchanged. |
| `Relocated { page, position }` | The offset is untrusted (another text model) or did not hold, and the context (at least 12 units) occurs exactly once in the chapter. | A corrected record (current text model and offset) that restores exactly. |
| `Fallback { page, target, cause }` | The content could not be placed. `cause` is `Ambiguous { candidates }` or `Stale(reason)`; `target` is the chapter start, or the document start when the chapter is missing or a fixed page is out of range. | None. |
| `Rejected(reason)` | Unknown record format, no identity, a different identity, a fixed-page record for a reflowable document (or the reverse), or a malformed anchor. | None, and no page. |

`Restore::restored()` returns `Some((page, position))` only for `Exact` and `Relocated`. That is
the only way a restore yields a position, so a fallback page cannot be stored as the reading
position or used as an annotation anchor without the caller explicitly anchoring a page. A caller
that falls back should keep the saved record until the user navigates, and should not autosave the
fallback page as if it were the restored location.

A different document fails closed even when its text would relocate cleanly. Migrating positions
between editions is a separate, explicit future operation. Repeated text is never resolved by
proximity: more than one match is `Ambiguous`.

### Cost and execution model

No operation caches text. Costs per call:

| Operation | Work |
|---|---|
| `layout`, then the first page count | MuPDF parses and lays out every chapter once per layout (cached in its accelerator). |
| `save_position(page)` (EPUB) | Text of the chapter's pages up to `page`, plus following pages until 32 units are collected. |
| `restore`: `Exact` | Text of the chapter's pages up to the anchor's page. |
| `restore`: `Relocated` and every unresolved `Fallback` | Text of the whole chapter. |
| `restore`: `Rejected`, missing chapter, fixed layout | No text extraction. |
| `open_path_identified` without a reusable stamp | One SHA-256 pass over the file. |

Host measurements (release build, Dragonfly, C.1.2 run): about 0.55–0.65 ms per page of extraction
up to 348-page chapters and 0.95 ms per page at 1,392 pages (an earlier C.1.1 run measured
0.7–1.5 ms per page), with no measurable memory above the laid-out document.
Raw output and configuration are preserved in
`~/ignite-qa-evidence/c1.2-anchor-benchmark-*` on the measurement host.

A `Book` is not `Send`: MuPDF state belongs to the thread that created it. The future UI should
therefore:

1. Own each `Book` on one dedicated reader thread, created there, that performs layout, rendering,
   `save_position` and `restore`. The UI thread sends requests over a channel and receives results
   tagged with a request generation, discarding replies older than the current page or layout.
2. Keep pen input, stroke capture and panel refresh off that thread so they never wait for MuPDF.
   Strokes are buffered with their page and attached to an anchor when the reader thread delivers
   one.
3. On open, render a placeholder (or the last rendered page image) while `restore` runs, then render
   the restored page. A `Fallback` is shown as such.
4. Debounce `save_position` after page turns. It runs on the reader thread after the visible page is
   rendered, never before it.
5. Before any of this ships, add cooperative cancellation between pages to the chapter loops (they
   already proceed one page at a time) so a superseded restore stops early; and measure whole-chapter
   extraction on the device.

### Bounds

Stored fields are validated before any document work: the context is limited by byte length
before it is decoded, then to 32 units of the allowed set, and the text-model identifier to 64
bytes without control characters. Resolution streams the chapter one page at a time, using memory
for one page's units plus a 32-unit window and a page-start table. Time is proportional to the pages
read. The chapter's length is a property of the document, not of the record.

### Limitations

- Offsets depend on MuPDF's text extraction. `TEXT_MODEL` names the unit rules and the MuPDF
  release; changing either must change it, which turns exact restores into context relocation.
- Chapters are identified by spine index only. Within one document identity that is stable; chapter
  file names would only matter for edition migration, and need an EPUB package reader.
- Anchors are page starts. A page without units anchors to the next unit in its chapter, and
  restores to the page that unit is on.
- Context matching is per chapter. Text repeated within a chapter can only be restored exactly
  through a trusted offset.
- Canonically equivalent spellings (NFC and NFD) are different content. This only matters across
  documents, which are rejected anyway.
- Right-to-left text was observed in logical order and layout-invariant for the tested fixture only.
- Stamp-based identity reuse depends on the filesystem; the Kindle user-storage filesystem's
  timestamp and inode behaviour has not been characterised.
- Kindle runtime cost (extraction and hashing) is not measured.

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
- the linked file contains the reader rendering test and defined MuPDF `fz_*` symbols;
- the linked file excludes the CVE-2026-7233 CFF subsetting routine and symbols from disabled
  MuJS, Brotli, cmark-gfm, Tesseract and zxing-cpp components; and
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

The initial Phase 1B.2 Windows-local validation on 2026-11-19 was incomplete:

- the source is pinned to MuPDF 1.28.6 commit
  `da5f5eb890d7b3fb76903160b5f7f48ba1301492` with all recursive third-party revisions;
- the deterministic font fallback, store-lifetime and CVE-2026-112036 patches matched their
  expected source exactly once;
- the ordinary `scribe-app` library suite passed outside the restricted sandbox: 1,583 passed and
  29 explicitly ignored, with no reader feature enabled;
- host and ARM dependency-graph isolation passed in both directions, including the release and
  packaging activation scan;
- the Python validator suite passed, including negative cases for a linked CFF-subsetting routine
  and disabled optional-component symbols;
- the Windows native build then stopped with Visual Studio error
  `C1902 Program database manager mismatch`, before any reader test could execute; and
- Docker Desktop was unavailable and WSL enumeration timed out, so Linux host tests, peak RSS and
  the complete ARMv7 link and inspection did not run.

Those Windows-local results alone were not sufficient. The subsequent 2026-11-19 pinned-container
run on Dragonfly against `feat/reader-mupdf-upgrade` commit `c2abb4c` passed:

- `mupdf_version=1.28.6` and `reader_host_tests=passed`;
- `reader_host_peak_rss_kib=40176` (39.2 MiB, Linux host test process);
- `reader_needed_added=none` and `reader_max_glibc=GLIBC_2.30`;
- `reader_cve_2026_7233_subsetting_symbol=absent` and `READER_ARMV7_ELF_OK`;
- reader isolation passed and Python QA passed 35 tests.

The validator exited with code 0. Raw log: `/home/dragonfly/ignite-reader-phase1b2-qualified.log`.
SHA-256: `1512c16c3289b954a570584a6b838db3cafb84d569f4cac7d9b93aafb9f9476c`.
This is host/ARM build evidence, not physical Kindle runtime qualification.

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

The historical validation above qualified the pinned Linux build path and differential ABI
checks only. Subsequent supervised Kindle testing on 2026-11-24 established one successful
open/render/close cycle for each of PDF, EPUB and CBZ using the separate standalone probe.
The operator-reported results and remaining qualification gaps are recorded in
[reader-probe-milestone-b.md](reader-probe-milestone-b.md). They do not imply
that the full reader, production integration or sustained runtime is qualified.

Phase 1A validation on 2026-11-17 is incomplete on the available Windows host:

- the ordinary `scribe-app` library suite passed: 1,583 passed and 29 explicitly ignored;
- host and ARM dependency-graph checks passed in both disabled and enabled directions, and the
  release/packaging scan found no reader feature activation;
- Cargo formatting, ShellCheck and `git diff --check` passed;
- both debug and release reader builds stopped in MuPDF's unchanged MSBuild step with the local
  Visual Studio PDB-version failures `C1902` and `LNK1101`, before Rust reader tests could run; and
- Docker Desktop reported that it could not start, so the pinned Linux reader tests, Python QA,
  ARMv7 link, ELF, shared-library, GLIBC and native-object checks have not been rerun for Phase 1A.

This partial record does not qualify Phase 1A or a device runtime. It must be replaced with the
complete pinned-container results before review readiness is claimed.

## Dependencies

All of these are compiled from source and linked statically, and only with `--features reader`.

| Component | Version | Licence | Notes |
|---|---|---|---|
| `mupdf` crate | 0.8.0 (exact API pin, in-repository fork) | AGPL-3.0 | Safe Rust wrapper; source lineage `deaacc87c4cea6a2146e90e00e0d5658c911277b` |
| `mupdf-sys` crate | 0.8.0 (in-repository fork) | AGPL-3.0 | Build script and bindings; build-time `bindgen`, `cc` |
| MuPDF | 1.28.6 | AGPL-3.0 | Submodule `da5f5eb890d7b3fb76903160b5f7f48ba1301492`; PDF, EPUB, HTML, images, SVG, CBZ, base-14 fonts |
| FreeType | 2.14.3 | FreeType Licence (FTL) | `0a0221a1347e2f1e07c395263540026e9a0aa7c7` |
| HarfBuzz | 13.0.1 | MIT ("Old MIT") | `c28ba35a1e6dc4b6b32e2b1d11fd2a408f6c86bb` |
| jbig2dec | 0.20 | AGPL-3.0 | Bundled by MuPDF |
| Little CMS (`lcms2mt`, Artifex fork) | 2.19.1.art | MIT | `d69c64417c4a33a4629957fc0e08f4f4c5abcc3d` |
| libjpeg (IJG) | 10 | IJG licence | `627faae398468d5d11e3d694dc325432f734b5c4` |
| OpenJPEG | 2.5.3 declared | BSD-2-Clause | `957029eb875eee1118743f200cb86da9d8289de2` |
| zlib | 1.3.2 | zlib | `da607da739fa6047df13e66a2af6b8bec7c2a498` |
| Gumbo HTML parser | 0.10.1 lineage | Apache-2.0 | `61bd4e736c3b342e592f65be0d65ec6a0acd8dcb`; EPUB and HTML |

Not built with this feature set: MuJS (JavaScript), Tesseract, Leptonica, Brotli, cmark-gfm,
`extract` (DOCX output), zxing-cpp, libarchive, curl, FB2, Markdown, MOBI, Office, text, OCR/ODT
output, or X11/GLUT viewers. MuPDF's own fonts are limited to the base-14 set.

The exact recursive source revisions and crates.io provenance checksums are recorded in
`vendor/README.md` and the Phase 1B assessment. Ignite is AGPL-3.0-only, so the AGPL components
add no new headline licence; release notices, corresponding source and a final component inventory
still need review before any build containing the reader ships.

## Remaining limitations

- **Milestone A follow-up validation.** Format-signature gating, gzip handler registration
  hardening, FFI blocklisting and broader defined-symbol inspection are changes on the stacked
  hardening branch; rerun the complete pinned host/ARMv7 validator after applying them. The
  November 19 baseline log covers commit `c2abb4c`, not this subsequent hardening.
- **Kindle runtime qualification.** The MuPDF 1.28.6 build, fail-closed CVE-2026-112036 backport
  and exclusion of the CVE-2026-7233 subsetting symbol passed pinned Linux/ARMv7 validation.
  The exact standalone probe binary loaded on one Kindle 5.18.6 device and rendered
  one small fixture per format under an isolated, bounded test. Loader compatibility
  and memory observations are established for those cases only; broader runtime
  stability and error behavior remain unqualified.
- **Native memory is not globally bounded.** The destination pixmap has a hard limit, but parsing,
  image decoding, layout, caches, masks and transparency groups do not. Hardware tests need RSS
  and failure measurements with adversarial and image-heavy documents before customer use.
- **Path races remain.** The safe Rust API accepts a pathname rather than an open descriptor.
  Symlink rejection and identity checks are fail-closed but cannot make a concurrently writable
  source immutable.
- **Production distribution and licensing notices:** component inventory, provenance, and notices
  in the release pipeline remain unresolved. Reader artifacts are experimental validation output.
- **EPUB anchors:** an experimental content-anchor API with host tests; see
  [Reading positions](#reading-positions-experimental). Not measured on the Kindle.
- **Book catalog and stable identity:** separate a file's identity (content hash) from the logical
  book it holds; no storage or catalog is part of Phase 0.
- **Reading-position persistence:** no store or committed record format exists.
- **Reader UI:** no UI or application call site exists.
- **Hardware acceptance:** three initial physical smoke tests now exist, but one cycle
  per format is insufficient for sustained-use acceptance. Multipage navigation,
  retained-memory trends, real-world content and normal Ignite integration have
  not been qualified.
