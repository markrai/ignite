# Milestone B: experimental ARMv7 reader probe

Status: **ARMv7 release build verified; limited physical smoke tests passed; sustained Kindle
runtime still unqualified**. This standalone executable does not change the installed Ignite
runtime, its update mechanism, or notebooks.
It is built under the off-by-default `scribe-app/reader` feature and is never part of the
customer Scriptlet, stable GitHub release, or normal application startup.

## Purpose

`reader-probe` exercises the actual `scribe_app::reader_engine::Book` file-backed path.
It opens, lays out when required, renders a selected page into Gray8, consumes the pixels,
drops the document, and repeats the cycle with process memory measurements. Its output
includes open/render times, page count, checksum, RSS and process high-water RSS in KiB.
`VmHWM` is a process-lifetime high-water mark, not a per-cycle reset. Each document should
be run in a new process to obtain meaningful per-document peak memory.

Milestone B.2 adds `--pages 0,1,2,1,0` for bounded within-book navigation. The probe
opens and lays out the document once per cycle, renders the requested pages in order,
and only then closes the book. It prints `reader_probe_page_cycle` records containing
page index, dimensions, checksum, render time, RSS and process HWM after dropping each
rendered pixmap; `reader_probe_cycle` still reports RSS after the book is closed.
`--page N` and the single-page output remain compatible with the original probe.
A sequence has 1 to 16 entries and the product of sequence length and cycle count
cannot exceed 1,000 renders. These are CLI bounds, not a memory or wall-clock limit.

The probe itself never writes a file, launches Ignite, modifies notebooks or calls the
OTA installer. It does not catch native crashes or terminate runaway parsing; external
per-process time and memory limits are required for adversarial document testing.

On ARM, its corpus directory is fixed to
`/mnt/us/ignite-reader-qa/corpus`. Filenames must be simple ASCII basenames with
lowercase `.pdf`, `.epub` or `.cbz` extensions. It rejects symlinks, empty files
and files larger than 64 MiB. The reader's existing path, identity and header checks
also apply. These guardrails do not prove an input is safe or eliminate native
decompression memory spikes.

## Pinned build and release-profile evidence

From the repository root on a Linux/Docker host:

```sh
git submodule update --init --recursive
image="$(sed -n 's/^rust_armv7hf=//p' tools/pinned-docker-images.txt | tr -d '\r')"
mkdir -p target/qa-reader-probe
docker run --rm \
  -e IGNITE_READER_RUN_HOST_TESTS=1 \
  -e IGNITE_READER_BUILD_PROBE=1 \
  -e IGNITE_READER_PROBE_OUTPUT_DIR=/src/target/qa-reader-probe \
  -v "$PWD:/src" -w /src "$image" \
  bash /src/tools/ci/validate_reader_armv7.sh
sha256sum target/qa-reader-probe/reader-probe-armv7hf
```

This reuses the pinned native revision, snapshot package index, cross toolchain and
ARMv7 sysroot from Phase 1B. It builds both an ordinary `scribe-app` and the
reader probe with `--release`, identical workspace LTO and codegen settings,
and the native `-static-libstdc++` link argument. It temporarily overrides
the release profile's stripping so the exact optimized probe can be checked
with `nm`, then strips a copy for experimental device qualification.

`tools/ci/verify_reader_probe_armv7.py` checks both ARM ELF files, ABI,
dynamic dependencies, GLIBC requirements against the **ordinary release**
executable, and forbidden CFF/gzip/optional native symbols in the unstripped
probe. The probe may add only `libm.so.6` to the ordinary executable's shared
libraries; the verifier still checks the ARM hard-float sysroot library and rejects
any other newly introduced dependency. The `reader_probe_added_needed` field
reports the actual additions, including `libm.so.6` when present. Device-specific
runtime compatibility remains unqualified until a separately authorized hardware
run; presence in the cross-build sysroot does not prove Kindle compatibility.
The script prints the SHA-256 and byte count of the stripped probe
and `READER_PROBE_ARMV7_RELEASE_OK` on success.

The validator does **not** run ARM code or qualify Kindle runtime behavior.
Any change to the reader-probe requires a newly built, hash-verified binary;
previous Kindle smoke-test hashes do not qualify a changed executable.
Its output directory is QA-only and is not uploaded into production
release promotion artifacts. Re-run the build after any reader or native
source change. Do not confuse the older debug-harness RSS with the new
device probe measurements.

## Physical QA gate

`tools/qa/ignite_qa.py preflight` blocks physical testing on a
production-bound Kindle with live notebook, sync or device state.
**Do not bypass that gate to run this probe on your primary Kindle.**
Use an isolated QA device or a demonstrably separate non-production
QA slot after the normal preflight has passed. Back up any data beforehand,
verify USB/SSH recovery and available space, and close Ignite.

An authorized operator may copy the exact hash-qualified probe to the
dedicated QA directory on that isolated device, put an approved document
corpus in its `corpus` subdirectory, and run it over SSH with a bounded
external timeout and address-space limit. The binary accepts:

```text
reader-probe --file normal.pdf --cycles 1 --page 0 --width 1000 --height 1400
```

`--cycles` ranges from 1 through 1000 for a single page. With `--pages`, there
are at most 16 requested page indices and at most 1,000 total renders across all
cycles. Every render must stay inside the reader's 4,612,800-pixel output limit.
Only non-ARM host builds accept `--corpus <path>` for disposable fixture tests;
ARM executables reject it.

Use separate processes for normal PDF, EPUB, CBZ, image-heavy, malformed,
mismatched-signature and controlled adversarial inputs. Capture stdout,
stderr, process exit status, kernel crash or OOM evidence, wall-clock
timeout status, process memory and the device firmware/build identity.
`reader_probe_result=pass` means only that the requested cycles finished.

## Physical smoke evidence (2026-11-24)

The operator reported a **single successful cycle each** on a first-generation
Kindle Scribe running firmware 5.18.6, under a separately approved,
memory-only, namespace/chroot-isolated test environment. This was an
exceptional, supervised developer experiment, **not** a pass of the normal
production-slot physical QA gate. The existing gate remains enforced.

| Disposable fixture | Page dimensions | Open | Render | Cycle | Validation |
| --- | --- | ---: | ---: | ---: | --- |
| Rectangle PDF | 1000 x 500 | 4 ms | 5 ms | 10 ms | Independent expected pixel checksum |
| Text EPUB | 1000 x 1264 | 11 ms | 27 ms | 39 ms | Same-source host oracle; nonblank text |
| Grayscale CBZ | 1000 x 1000 | 3 ms | 20 ms | 25 ms | Independent expected pixel checksum |

All three were reported to exit successfully with clean temporary resources,
no device reboot, and no change to Ignite's production PID or notebook state.
The exact pre-B.2 executable used in these tests had SHA-256
`3abcf429850b0546eb785f57dccd0effef9d307d64551250be10547e5d370930`.
The operator retained the raw transcripts and metadata outside the repository;
this table summarizes those reported observations rather than independently
archived, CI-verifiable physical evidence.

The tests demonstrated the loader, basic decoding, simple EPUB layout, and
bounded grayscale rendering on that particular Kindle. They do not qualify
sustained use, general font coverage, large books, concurrent workloads, or
the production app integration. The initial PDF, EPUB, and CBZ fixtures were
all one page each.

## Milestone B.2 acceptance, still outstanding

- Verify page sequences with different images/text, backwards navigation,
  and repeat visits in a single open document on Kindle hardware.
- Record post-render RSS per page and post-close RSS per cycle, alongside
  process-lifetime HWM. A flat HWM does not prove that live allocations were
  released; evaluate the RSS trend across multiple cycles.
- Use a benign, small, multipage fixture before considering bounded repeat
  cycles; define explicit memory and time thresholds and authorize each
  physical run separately.
- Rebuild and hash-verify the changed ARMv7 probe before any hardware
  execution. The three previous device results belong to the older hash.
- Validate real text/fonts (especially CJK and symbols), image formats,
  realistic page sizes, and failure behavior on a disposable QA device.
- Establish a reusable, reviewed QA runner and durable evidence capture before
  claiming broader hardware qualification.
- Review the eventual opt-in developer distribution and licensing notices.
  The opt-in update path would be **Milestone C** and is not implied here.

No version bump, public release, deployment or activation is part of Milestone B.
