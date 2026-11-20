#!/usr/bin/env python3
from __future__ import annotations

import argparse
import hashlib
import re
import sys
from pathlib import Path

import verify_reader_armv7_elf as existing


def verify_release_probe(
    unstripped: Path, stripped: Path, ordinary: Path
) -> dict[str, str]:
    for path in (unstripped, stripped, ordinary):
        if not path.is_file():
            raise existing.VerificationError(f"missing ARMv7 executable: {path}")

    full_info = existing.verify_elf(unstripped, "unstripped reader probe")
    shipped_info = existing.verify_elf(stripped, "stripped reader probe")
    ordinary_info = existing.verify_elf(ordinary, "ordinary release app")
    if full_info["stripped"]:
        raise existing.VerificationError("probe inspection copy is already stripped")
    if not shipped_info["stripped"]:
        raise existing.VerificationError("probe deployment copy must be stripped")
    if ordinary_info["stripped"]:
        raise existing.VerificationError("ordinary release baseline must be inspectable")

    full_symbols = existing.run("arm-linux-gnueabihf-nm", "--defined-only", str(unstripped))
    existing.verify_reader_symbol_policy(full_symbols)
    if not re.search(r"(?m)^\s*[0-9A-Fa-f]+\s+[Tt]\s+fz_[A-Za-z0-9_]+\s*$", full_symbols):
        raise existing.VerificationError("release probe has no linked MuPDF implementation")

    ordinary_symbols = existing.run("arm-linux-gnueabihf-nm", "--defined-only", str(ordinary))
    if re.search(r"(?m)^\s*[0-9A-Fa-f]+\s+[Tt]\s+fz_[A-Za-z0-9_]+\s*$", ordinary_symbols):
        raise existing.VerificationError("ordinary release executable links MuPDF")

    probe_libs = existing.verify_dynamic_libraries(stripped, "stripped reader probe")
    full_libs = existing.verify_dynamic_libraries(unstripped, "unstripped reader probe")
    ordinary_libs = existing.verify_dynamic_libraries(ordinary, "ordinary release app")
    if {name for name, _ in full_libs} != {name for name, _ in probe_libs}:
        raise existing.VerificationError("stripping changed the probe dynamic dependencies")
    existing.verify_no_added_libraries(probe_libs, ordinary_libs)

    _, probe_max = existing.glibc_requirements(stripped, "stripped reader probe")
    _, ordinary_max = existing.glibc_requirements(ordinary, "ordinary release app")
    existing.verify_glibc_not_higher(probe_max, ordinary_max)

    return {
        "reader_probe_sha256": hashlib.sha256(stripped.read_bytes()).hexdigest(),
        "reader_probe_bytes": str(stripped.stat().st_size),
        "reader_probe_elf": "ARMv7 hard-float",
        "reader_probe_linked_mupdf": "present",
        "reader_probe_forbidden_symbols": "absent",
        "reader_probe_added_needed": "none",
        "reader_probe_max_glibc": probe_max,
        "reader_probe_ordinary_max_glibc": ordinary_max,
        "reader_probe_stripped": "true",
        "reader_probe_device_execution": "not_run",
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--unstripped", type=Path, required=True)
    parser.add_argument("--stripped", type=Path, required=True)
    parser.add_argument("--ordinary", type=Path, required=True)
    args = parser.parse_args()

    report = verify_release_probe(args.unstripped, args.stripped, args.ordinary)
    for key, value in report.items():
        print(f"{key}={value}")
    print("READER_PROBE_ARMV7_RELEASE_OK")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except existing.VerificationError as exc:
        print(f"READER PROBE ARMV7 RELEASE FAIL: {exc}", file=sys.stderr)
        raise SystemExit(1)
