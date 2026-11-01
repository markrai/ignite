"""OS-backed secret store for publisher private-key material.

Windows: DPAPI (CryptProtectData / CryptUnprotectData) seals bytes to the
current user. DPAPI alone is not an off-machine recovery mechanism — recovery
backups must use a separately configured destination.

Insecure file store is available only when explicitly allowed for tests.
"""

from __future__ import annotations

import os
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

from .errors import ShipConfigError, ShipError


class SecretStore(Protocol):
    name: str

    def seal(self, plaintext: bytes) -> bytes: ...

    def unseal(self, sealed: bytes) -> bytes: ...


@dataclass(frozen=True)
class InsecureFileStore:
    """Test-only store. Refuses unless allow_insecure is true."""

    allow_insecure: bool
    name: str = "insecure-file"

    def seal(self, plaintext: bytes) -> bytes:
        if not self.allow_insecure:
            raise ShipConfigError(
                "insecure secret store refused; configure an OS-backed store "
                "or set IGNITE_SHIP_ALLOW_INSECURE_STORE=1 only for disposable tests"
            )
        # Deliberately weak framing so tests never confuse this with DPAPI blobs.
        return b"IGNITE-INSECURE-SEAL-V1\n" + plaintext

    def unseal(self, sealed: bytes) -> bytes:
        if not self.allow_insecure:
            raise ShipConfigError("insecure secret store refused")
        prefix = b"IGNITE-INSECURE-SEAL-V1\n"
        if not sealed.startswith(prefix):
            raise ShipError("malformed insecure sealed blob")
        return sealed[len(prefix) :]


class WindowsDpapiStore:
    """Seal secrets to the current Windows user via DPAPI."""

    name = "windows-dpapi"

    def seal(self, plaintext: bytes) -> bytes:
        return _dpapi_protect(plaintext)

    def unseal(self, sealed: bytes) -> bytes:
        return _dpapi_unprotect(sealed)


def select_secret_store(*, allow_insecure: bool = False) -> SecretStore:
    if sys.platform == "win32":
        try:
            # Probe DPAPI availability.
            probe = WindowsDpapiStore()
            roundtrip = probe.unseal(probe.seal(b"ignite-dpapi-probe"))
            if roundtrip != b"ignite-dpapi-probe":
                raise ShipError("DPAPI probe mismatch")
            return probe
        except ShipError:
            if allow_insecure:
                return InsecureFileStore(allow_insecure=True)
            raise ShipConfigError(
                "Windows DPAPI unavailable; refusing to store publisher keys. "
                "Fix the OS secret store or use a disposable insecure store only "
                "under IGNITE_SHIP_ALLOW_INSECURE_STORE=1 for tests."
            ) from None
    if allow_insecure:
        return InsecureFileStore(allow_insecure=True)
    raise ShipConfigError(
        f"no OS-backed secret store implemented for platform {sys.platform!r}; "
        "configure recovery + an approved store, or enable the insecure test store "
        "explicitly for disposable environments only"
    )


def write_sealed_file(path: Path, sealed: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    if tmp.exists() or path.exists():
        # Exclusive create for the temp file; replace is atomic on the same volume.
        if tmp.exists():
            tmp.unlink()
    flags = os.O_CREAT | os.O_EXCL | os.O_WRONLY
    if hasattr(os, "O_BINARY"):
        flags |= os.O_BINARY
    fd = os.open(tmp, flags, 0o600)
    try:
        os.write(fd, sealed)
    finally:
        os.close(fd)
    os.replace(tmp, path)
    try:
        os.chmod(path, 0o600)
    except OSError:
        pass


def read_sealed_file(path: Path) -> bytes:
    if not path.is_file():
        raise ShipError(f"sealed key file missing: {path.name}")
    return path.read_bytes()


def _dpapi_protect(plaintext: bytes) -> bytes:
    import ctypes
    from ctypes import wintypes

    class DATA_BLOB(ctypes.Structure):
        _fields_ = [("cbData", wintypes.DWORD), ("pbData", ctypes.POINTER(ctypes.c_byte))]

    crypt32 = ctypes.windll.crypt32
    kernel32 = ctypes.windll.kernel32

    blob_in = DATA_BLOB(
        len(plaintext),
        ctypes.cast(
            ctypes.create_string_buffer(plaintext, len(plaintext)),
            ctypes.POINTER(ctypes.c_byte),
        ),
    )
    blob_out = DATA_BLOB()
    if not crypt32.CryptProtectData(
        ctypes.byref(blob_in),
        "IgnitePublisherKey",
        None,
        None,
        None,
        0,
        ctypes.byref(blob_out),
    ):
        raise ShipError("CryptProtectData failed")
    try:
        return ctypes.string_at(blob_out.pbData, blob_out.cbData)
    finally:
        kernel32.LocalFree(blob_out.pbData)


def _dpapi_unprotect(sealed: bytes) -> bytes:
    import ctypes
    from ctypes import wintypes

    class DATA_BLOB(ctypes.Structure):
        _fields_ = [("cbData", wintypes.DWORD), ("pbData", ctypes.POINTER(ctypes.c_byte))]

    crypt32 = ctypes.windll.crypt32
    kernel32 = ctypes.windll.kernel32

    blob_in = DATA_BLOB(
        len(sealed),
        ctypes.cast(
            ctypes.create_string_buffer(sealed, len(sealed)),
            ctypes.POINTER(ctypes.c_byte),
        ),
    )
    blob_out = DATA_BLOB()
    if not crypt32.CryptUnprotectData(
        ctypes.byref(blob_in),
        None,
        None,
        None,
        None,
        0,
        ctypes.byref(blob_out),
    ):
        raise ShipError("CryptUnprotectData failed")
    try:
        return ctypes.string_at(blob_out.pbData, blob_out.cbData)
    finally:
        kernel32.LocalFree(blob_out.pbData)
