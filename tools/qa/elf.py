"""Read-only ARMv7hf ELF checks for Kindle release binaries (no exec, no cross-readelf)."""

from __future__ import annotations

import struct
from pathlib import Path
from typing import Any

ELF_MAGIC = b"\x7fELF"
EI_CLASS_32 = 1
EI_DATA_LE = 1
EM_ARM = 40
PT_INTERP = 3
SHT_SYMTAB = 2
EF_ARM_ABI_FLOAT_HARD = 0x00000400
EXPECTED_INTERP = "/lib/ld-linux-armhf.so.3"


class ElfError(ValueError):
    pass


def inspect_armv7hf_elf(path: Path) -> dict[str, Any]:
    data = path.read_bytes()
    if len(data) < 52 or data[:4] != ELF_MAGIC:
        raise ElfError(f"{path.name}: not an ELF file")
    ei_class = data[4]
    ei_data = data[5]
    if ei_class != EI_CLASS_32:
        raise ElfError(f"{path.name}: EI_CLASS={ei_class} (want ELF32)")
    if ei_data != EI_DATA_LE:
        raise ElfError(f"{path.name}: EI_DATA={ei_data} (want little-endian)")
    e_machine = struct.unpack_from("<H", data, 18)[0]
    if e_machine != EM_ARM:
        raise ElfError(f"{path.name}: e_machine={e_machine} (want EM_ARM=40)")
    e_flags = struct.unpack_from("<I", data, 36)[0]
    if e_flags & EF_ARM_ABI_FLOAT_HARD == 0:
        raise ElfError(f"{path.name}: missing EF_ARM_ABI_FLOAT_HARD (flags=0x{e_flags:x})")
    e_phoff = struct.unpack_from("<I", data, 28)[0]
    e_phentsize = struct.unpack_from("<H", data, 42)[0]
    e_phnum = struct.unpack_from("<H", data, 44)[0]
    interp = None
    for i in range(e_phnum):
        off = e_phoff + i * e_phentsize
        p_type, p_offset, _p_vaddr, _p_paddr, p_filesz = struct.unpack_from(
            "<IIIII", data, off
        )
        if p_type == PT_INTERP:
            raw = data[p_offset : p_offset + p_filesz]
            interp = raw.split(b"\x00", 1)[0].decode("ascii", errors="replace")
            break
    if interp != EXPECTED_INTERP:
        raise ElfError(f"{path.name}: interpreter {interp!r} (want {EXPECTED_INTERP})")
    e_shoff = struct.unpack_from("<I", data, 32)[0]
    e_shentsize = struct.unpack_from("<H", data, 46)[0]
    e_shnum = struct.unpack_from("<H", data, 48)[0]
    has_symtab = False
    for i in range(e_shnum):
        sh_type = struct.unpack_from("<I", data, e_shoff + i * e_shentsize + 4)[0]
        if sh_type == SHT_SYMTAB:
            has_symtab = True
            break
    if has_symtab:
        raise ElfError(f"{path.name}: refusing unstripped ELF (SHT_SYMTAB present)")
    return {
        "class": "ELF32",
        "data": "little",
        "machine": "ARM",
        "hard_float": True,
        "interpreter": interp,
        "flags": f"0x{e_flags:x}",
        "stripped": True,
        "bytes": len(data),
    }
