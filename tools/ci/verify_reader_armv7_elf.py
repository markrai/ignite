#!/usr/bin/env python3

from __future__ import annotations

import argparse
import importlib.util
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ELF_MODULE_PATH = ROOT / "tools" / "qa" / "elf.py"
ELF_SPEC = importlib.util.spec_from_file_location("ignite_qa_elf", ELF_MODULE_PATH)
if ELF_SPEC is None or ELF_SPEC.loader is None:
    raise RuntimeError(f"cannot load {ELF_MODULE_PATH}")
ELF_MODULE = importlib.util.module_from_spec(ELF_SPEC)
ELF_SPEC.loader.exec_module(ELF_MODULE)
ElfError = ELF_MODULE.ElfError
inspect_armv7hf_elf = ELF_MODULE.inspect_armv7hf_elf


class VerificationError(RuntimeError):
    pass


def run(*argv: str) -> str:
    result = subprocess.run(argv, capture_output=True, text=True, errors="replace")
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise VerificationError(f"{' '.join(argv)} failed: {detail}")
    return result.stdout


def glibc_key(symbol: str) -> tuple[int, ...]:
    return tuple(int(part) for part in symbol.removeprefix("GLIBC_").split("."))


def glibc_ceiling() -> tuple[str, str]:
    policy = ROOT / "tools" / "dropbear" / "PINNED.txt"
    for line in policy.read_text(encoding="utf-8").splitlines():
        if line.startswith("max_glibc="):
            value = line.split("=", 1)[1].strip()
            if re.fullmatch(r"GLIBC_[0-9]+(?:\.[0-9]+)+", value):
                return value, policy.relative_to(ROOT).as_posix()
    raise VerificationError(f"valid max_glibc missing from {policy}")


def verify_dynamic_libraries(binary: Path) -> list[tuple[str, Path]]:
    dynamic = run("arm-linux-gnueabihf-readelf", "-d", str(binary))
    names = re.findall(r"\(NEEDED\).*?\[([^]]+)\]", dynamic)
    if not names:
        raise VerificationError("reader test ELF has no declared shared libraries")
    resolved: list[tuple[str, Path]] = []
    for name in names:
        value = run("arm-linux-gnueabihf-gcc", f"-print-file-name={name}").strip()
        path = Path(value)
        if value == name or not path.is_file():
            raise VerificationError(f"required shared library is absent from ARM sysroot: {name}")
        header = run("arm-linux-gnueabihf-readelf", "-h", str(path))
        if not re.search(r"Class:\s+ELF32", header):
            raise VerificationError(f"{name} is not ELF32: {path}")
        if not re.search(r"Machine:\s+ARM", header):
            raise VerificationError(f"{name} is not ARM: {path}")
        if "hard-float ABI" not in header:
            raise VerificationError(f"{name} is not ARM hard-float: {path}")
        resolved.append((name, path.resolve()))
    return resolved


def verify_native_outputs(target_dir: Path, triple: str) -> tuple[int, set[str]]:
    build_root = target_dir / triple / "debug" / "build"
    mupdf_out_dirs = sorted(build_root.glob("mupdf-sys-*/out"))
    if not mupdf_out_dirs:
        raise VerificationError(f"MuPDF native output directory missing under {build_root}")
    out_dirs = sorted(build_root.glob("*-*/out"))
    objects: list[Path] = []
    for out_dir in out_dirs:
        objects.extend(path for path in out_dir.rglob("*.a") if path.is_file())
        objects.extend(path for path in out_dir.rglob("*.o") if path.is_file())
    objects = sorted(set(objects))
    mupdf_objects = {
        path
        for out_dir in mupdf_out_dirs
        for path in (*out_dir.rglob("*.a"), *out_dir.rglob("*.o"))
        if path.is_file()
    }
    if not objects or not any(path.name == "libmupdf.a" for path in mupdf_objects):
        raise VerificationError("MuPDF native libraries were not produced")
    formats: set[str] = set()
    for path in objects:
        detail = run("arm-linux-gnueabihf-objdump", "-f", str(path))
        current = set(re.findall(r"file format\s+(\S+)", detail))
        if not current:
            raise VerificationError(f"cannot identify native object architecture: {path}")
        formats.update(current)
        unexpected = current - {"elf32-littlearm"}
        if unexpected:
            raise VerificationError(
                f"non-ARM object in MuPDF native output {path}: {sorted(unexpected)}"
            )
    return len(objects), formats


def verify_linked_symbols(binary: Path) -> tuple[str, str]:
    symbols = run("arm-linux-gnueabihf-nm", "-C", "--defined-only", str(binary))
    reader_match = re.search(
        r"scribe_app::reader_engine::tests::pdf_page_renders_to_fit_the_box_in_gray",
        symbols,
    )
    mupdf_match = re.search(r"\s[Tt]\s+(fz_[A-Za-z0-9_]+)", symbols)
    if reader_match is None:
        raise VerificationError("linked test ELF does not contain the reader rendering test")
    if mupdf_match is None:
        raise VerificationError("linked test ELF does not contain MuPDF implementation symbols")
    return reader_match.group(0), mupdf_match.group(1)


def verify_glibc(binary: Path) -> tuple[list[str], str, str, str]:
    symbols = run("arm-linux-gnueabihf-objdump", "-T", str(binary))
    versions = sorted(
        set(re.findall(r"GLIBC_[0-9]+(?:\.[0-9]+)+", symbols)), key=glibc_key
    )
    if not versions:
        raise VerificationError("reader test ELF has no versioned GLIBC requirements")
    ceiling, policy = glibc_ceiling()
    maximum = max(versions, key=glibc_key)
    if glibc_key(maximum) > glibc_key(ceiling):
        raise VerificationError(
            f"reader test ELF requires {maximum}, above {policy} ceiling {ceiling}"
        )
    return versions, maximum, ceiling, policy


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--baseline-binary", required=True, type=Path)
    parser.add_argument("--target-dir", required=True, type=Path)
    parser.add_argument(
        "--target", default="armv7-unknown-linux-gnueabihf", dest="triple"
    )
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        raise VerificationError(f"linked reader test executable missing: {binary}")
    baseline = args.baseline_binary.resolve()
    if not baseline.is_file():
        raise VerificationError(f"ordinary ARM executable missing: {baseline}")
    try:
        info = inspect_armv7hf_elf(binary, require_stripped=False)
        baseline_info = inspect_armv7hf_elf(baseline, require_stripped=False)
    except ElfError as exc:
        raise VerificationError(str(exc)) from exc
    attributes = run("arm-linux-gnueabihf-readelf", "-A", str(binary))
    if not re.search(r"Tag_ABI_VFP_args:\s+VFP registers", attributes):
        raise VerificationError("reader test ELF lacks VFP-register hard-float attributes")
    libraries = verify_dynamic_libraries(binary)
    baseline_libraries = verify_dynamic_libraries(baseline)
    baseline_names = {name for name, _ in baseline_libraries}
    added_libraries = [name for name, _ in libraries if name not in baseline_names]
    if added_libraries:
        raise VerificationError(
            "reader test ELF adds shared libraries outside the ordinary Kindle runtime "
            f"baseline: {','.join(added_libraries)}"
        )
    versions, maximum, ceiling, policy = verify_glibc(binary)
    reader_symbol, mupdf_symbol = verify_linked_symbols(binary)
    native_count, native_formats = verify_native_outputs(
        args.target_dir.resolve(), args.triple
    )
    print(f"reader_elf={binary}")
    print(f"reader_elf_bytes={info['bytes']}")
    print(f"reader_elf_class={info['class']}")
    print(f"reader_elf_machine={info['machine']}")
    print(f"reader_elf_flags={info['flags']}")
    print(f"reader_elf_hard_float={str(info['hard_float']).lower()}")
    print(f"reader_elf_interpreter={info['interpreter']}")
    print(f"reader_elf_stripped={str(info['stripped']).lower()}")
    print(f"reader_needed={','.join(name for name, _ in libraries)}")
    print(f"reader_needed_baseline={','.join(name for name, _ in baseline_libraries)}")
    print("reader_needed_added=none")
    for name, path in libraries:
        print(f"reader_needed_resolved={name}:{path}")
    print(f"reader_glibc_versions={','.join(versions)}")
    print(f"reader_max_glibc={maximum}")
    print(f"reader_glibc_ceiling={ceiling}")
    print(f"reader_glibc_policy={policy}")
    print(f"reader_linked_test_symbol={reader_symbol}")
    print(f"reader_linked_mupdf_symbol={mupdf_symbol}")
    print(f"reader_native_objects={native_count}")
    print(f"reader_native_formats={','.join(sorted(native_formats))}")
    print(f"ordinary_elf_class={baseline_info['class']}")
    print(f"ordinary_elf_machine={baseline_info['machine']}")
    print(f"ordinary_elf_hard_float={str(baseline_info['hard_float']).lower()}")
    print(f"ordinary_elf_interpreter={baseline_info['interpreter']}")
    print("READER_ARMV7_ELF_OK")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except VerificationError as exc:
        print(f"READER ARMV7 ELF FAIL: {exc}", file=sys.stderr)
        raise SystemExit(1)
