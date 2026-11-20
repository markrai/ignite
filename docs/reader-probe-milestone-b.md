# Milestone B: experimental ARMv7 reader probe

Status: **build and inspection only; Kindle execution not qualified**. This standalone
executable does not change the installed Ignite runtime, its update mechanism, or notebooks.
It is built under the off-by-default `scribe-app/reader` feature and is never part of the
customer Scriptlet, stable GitHub release, or normal application startup.

## Purpose

`reader-probe` exercises the actual `scribe_app::reader_engine::Book` file-backed path.
It opens, lays out when required, renders a selected page into Gray8, consumes the pixels,
drops the document, and repeats the cycle with process memory measurements. Its output
includes open/render times, page count, checksum, RSS and process high-water RSS in KiB.
`VmHWM` is a process-lifetime high-water mark, not a per-cycle reset. Each document should
be run in a new process to obtain meaningful per-document peak memory.

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
probe. The script prints the SHA-256 and byte count of the stripped probe
and `READER_PROBE_ARMV7_RELEASE_OK` on success.

The validator does **not** run ARM code or qualify Kindle runtime behavior.
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

`--cycles` ranges from 1 through 1000. Every render must stay inside the
reader's 4,612,800-pixel output limit. Only non-ARM host builds accept
`--corpus <path>` for disposable fixture tests; ARM executables reject it.

Use separate processes for normal PDF, EPUB, CBZ, image-heavy, malformed,
mismatched-signature and controlled adversarial inputs. Capture stdout,
stderr, process exit status, kernel crash or OOM evidence, wall-clock
timeout status, process memory and the device firmware/build identity.
`reader_probe_result=pass` means only that the requested cycles finished.

## Acceptance still outstanding

- Demonstrate an executable ARMv7 ELF actually runs on the target Kindle firmware.
- Confirm loader and all declared shared libraries on the device.
- Confirm rendered content and missing-font behavior, especially CJK and symbols.
- Measure process RSS and peak RSS per document, including retained memory across
  hundreds of open/render/close cycles, with defined limits for each test.
- Exercise adversarial inputs under external time and memory limits without
  affecting the notebook or sync environment.
- Establish a safe handoff/distribution method. A separate opt-in developer OTA
  channel is **Milestone C** and is not implied by this executable.

No version bump, public release, deployment or activation is part of Milestone B.
