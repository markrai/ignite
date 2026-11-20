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


FORBIDDEN_READER_SYMBOLS = {
    "fz_subset_cff_for_gids": "CVE-2026-7233 CFF subsetting path",
    "gz_document_handler": "gzip document-handler registration",
}
FORBIDDEN_READER_SYMBOL_PREFIXES = {
    "Brotli": "Brotli",
    "cmark_": "cmark-gfm",
    "js_": "MuJS",
    "mupdf_js_": "MuJS wrapper",
    "tess_": "Tesseract",
    "zxing": "zxing-cpp",
    "_ZN5ZXing": "zxing-cpp",
}


def run(*argv: str) -> str:
    result = subprocess.run(argv, capture_output=True, text=True, errors="replace")
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise VerificationError(f"{' '.join(argv)} failed: {detail}")
    return result.stdout


def glibc_key(symbol: str) -> tuple[int, ...]:
    return tuple(int(part) for part in symbol.removeprefix("GLIBC_").split("."))


def verify_dynamic_libraries(binary: Path, label: str) -> list[tuple[str, Path]]:
    dynamic = run("arm-linux-gnueabihf-readelf", "-d", str(binary))
    names = re.findall(r"\(NEEDED\).*?\[([^]]+)\]", dynamic)
    if not names:
        raise VerificationError(f"{label} ELF has no declared shared libraries")
    resolved: list[tuple[str, Path]] = []
    for name in names:
        value = run("arm-linux-gnueabihf-gcc", f"-print-file-name={name}").strip()
        path = Path(value)
        if value == name or not path.is_file():
            raise VerificationError(
                f"{label} required shared library is absent from ARM sysroot: {name}"
            )
        header = run("arm-linux-gnueabihf-readelf", "-h", str(path))
        if not re.search(r"Class:\s+ELF32", header):
            raise VerificationError(f"{name} is not ELF32: {path}")
        if not re.search(r"Machine:\s+ARM", header):
            raise VerificationError(f"{name} is not ARM: {path}")
        if "hard-float ABI" not in header:
            raise VerificationError(f"{name} is not ARM hard-float: {path}")
        resolved.append((name, path.resolve()))
    return resolved


def verify_no_added_libraries(
    reader_libraries: list[tuple[str, Path]],
    baseline_libraries: list[tuple[str, Path]],
) -> None:
    baseline_names = {name for name, _ in baseline_libraries}
    added_libraries = [
        name for name, _ in reader_libraries if name not in baseline_names
    ]
    if added_libraries:
        raise VerificationError(
            "reader test ELF adds shared libraries outside the equivalent "
            f"reader-disabled test baseline: {','.join(added_libraries)}"
        )


def verify_native_outputs(target_dir: Path, triple: str) -> tuple[int, set[str]]:
    build_root = target_dir / triple / "debug" / "build"
    mupdf_out_dirs = sorted(build_root.glob("mupdf-sys-*/out"))
    if not mupdf_out_dirs:
        raise VerificationError(
            f"MuPDF native output directory missing under {build_root}"
        )
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
            raise VerificationError(
                f"cannot identify native object architecture: {path}"
            )
        formats.update(current)
        unexpected = current - {"elf32-littlearm"}
        if unexpected:
            raise VerificationError(
                f"non-ARM object in MuPDF native output {path}: {sorted(unexpected)}"
            )
    return len(objects), formats


def find_linked_symbols(symbols: str) -> tuple[str, str]:
    reader_match = re.search(
        r"\s[Tt]\s+(\S*scribe_app13reader_engine5tests\S*"
        r"pdf_page_renders_to_fit_the_box_in_gray\S*)",
        symbols,
    )
    mupdf_match = re.search(r"\s[Tt]\s+(fz_[A-Za-z0-9_]+)$", symbols, re.MULTILINE)
    if reader_match is None:
        raise VerificationError(
            "linked test ELF does not contain the reader rendering test"
        )
    if mupdf_match is None:
        raise VerificationError(
            "linked test ELF does not contain MuPDF implementation symbols"
        )
    return reader_match.group(1), mupdf_match.group(1)


def verify_reader_symbol_policy(symbols: str) -> None:
    defined = {
        match.group(2)
        for match in re.finditer(
            r"(?m)^\s*(?:[0-9A-Fa-f]+\s+)?(\S)\s+(\S+)\s*$", symbols
        )
        if match.group(1) not in {"U", "u", "w", "v"}
    }
    for symbol, reason in FORBIDDEN_READER_SYMBOLS.items():
        if symbol in defined:
            raise VerificationError(
                f"reader test ELF links forbidden {reason} symbol: {symbol}"
            )
    for symbol in defined:
        for prefix, component in FORBIDDEN_READER_SYMBOL_PREFIXES.items():
            if symbol.startswith(prefix):
                raise VerificationError(
                    f"reader test ELF links disabled optional {component} symbol: {symbol}"
                )


def verify_linked_symbols(binary: Path) -> tuple[str, str]:
    symbols = run("arm-linux-gnueabihf-nm", "--defined-only", str(binary))
    verify_reader_symbol_policy(symbols)
    return find_linked_symbols(symbols)


def glibc_requirements(binary: Path, label: str) -> tuple[list[str], str]:
    symbols = run("arm-linux-gnueabihf-objdump", "-T", str(binary))
    versions = sorted(
        set(re.findall(r"GLIBC_[0-9]+(?:\.[0-9]+)+", symbols)), key=glibc_key
    )
    if not versions:
        raise VerificationError(f"{label} ELF has no versioned GLIBC requirements")
    maximum = max(versions, key=glibc_key)
    return versions, maximum


def verify_glibc_not_higher(reader_maximum: str, baseline_maximum: str) -> None:
    if glibc_key(reader_maximum) > glibc_key(baseline_maximum):
        raise VerificationError(
            f"reader test ELF requires {reader_maximum}, above equivalent "
            f"reader-disabled test baseline {baseline_maximum}"
        )


def verify_elf(binary: Path, label: str) -> dict[str, object]:
    try:
        info = inspect_armv7hf_elf(binary, require_stripped=False)
    except ElfError as exc:
        raise VerificationError(f"{label}: {exc}") from exc
    attributes = run("arm-linux-gnueabihf-readelf", "-A", str(binary))
    if not re.search(r"Tag_ABI_VFP_args:\s+VFP registers", attributes):
        raise VerificationError(f"{label} ELF lacks VFP-register hard-float attributes")
    return info


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--baseline-binary", required=True, type=Path)
    parser.add_argument("--ordinary-binary", required=True, type=Path)
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
        raise VerificationError(
            f"linked reader-disabled test executable missing: {baseline}"
        )
    ordinary = args.ordinary_binary.resolve()
    if not ordinary.is_file():
        raise VerificationError(f"ordinary ARM executable missing: {ordinary}")
    info = verify_elf(binary, "reader test")
    baseline_info = verify_elf(baseline, "reader-disabled test")
    ordinary_info = verify_elf(ordinary, "ordinary application")
    libraries = verify_dynamic_libraries(binary, "reader test")
    baseline_libraries = verify_dynamic_libraries(baseline, "reader-disabled test")
    ordinary_libraries = verify_dynamic_libraries(ordinary, "ordinary application")
    verify_no_added_libraries(libraries, baseline_libraries)
    versions, maximum = glibc_requirements(binary, "reader test")
    baseline_versions, baseline_maximum = glibc_requirements(
        baseline, "reader-disabled test"
    )
    ordinary_versions, ordinary_maximum = glibc_requirements(
        ordinary, "ordinary application"
    )
    verify_glibc_not_higher(maximum, baseline_maximum)
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
    print(
        "reader_disabled_test_needed="
        f"{','.join(name for name, _ in baseline_libraries)}"
    )
    print(f"ordinary_needed={','.join(name for name, _ in ordinary_libraries)}")
    print("reader_needed_added=none")
    for name, path in libraries:
        print(f"reader_needed_resolved={name}:{path}")
    for name, path in baseline_libraries:
        print(f"reader_disabled_test_needed_resolved={name}:{path}")
    for name, path in ordinary_libraries:
        print(f"ordinary_needed_resolved={name}:{path}")
    print(f"reader_glibc_versions={','.join(versions)}")
    print(f"reader_max_glibc={maximum}")
    print(f"reader_disabled_test_glibc_versions={','.join(baseline_versions)}")
    print(f"reader_disabled_test_max_glibc={baseline_maximum}")
    print(f"ordinary_glibc_versions={','.join(ordinary_versions)}")
    print(f"ordinary_max_glibc={ordinary_maximum}")
    print("reader_glibc_added_max=none")
    print("device_runtime_compatibility=unqualified")
    print(f"reader_linked_test_symbol={reader_symbol}")
    print(f"reader_linked_mupdf_symbol={mupdf_symbol}")
    print("reader_cve_2026_7233_subsetting_symbol=absent")
    print("reader_disabled_optional_component_symbols=absent")
    print(f"reader_native_objects={native_count}")
    print(f"reader_native_formats={','.join(sorted(native_formats))}")
    print(f"reader_disabled_test_elf_class={baseline_info['class']}")
    print(f"reader_disabled_test_elf_machine={baseline_info['machine']}")
    print(
        "reader_disabled_test_elf_hard_float="
        f"{str(baseline_info['hard_float']).lower()}"
    )
    print(f"reader_disabled_test_elf_interpreter={baseline_info['interpreter']}")
    print(f"ordinary_elf_class={ordinary_info['class']}")
    print(f"ordinary_elf_machine={ordinary_info['machine']}")
    print(f"ordinary_elf_hard_float={str(ordinary_info['hard_float']).lower()}")
    print(f"ordinary_elf_interpreter={ordinary_info['interpreter']}")
    print("READER_ARMV7_ELF_OK")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except VerificationError as exc:
        print(f"READER ARMV7 ELF FAIL: {exc}", file=sys.stderr)
        raise SystemExit(1)
