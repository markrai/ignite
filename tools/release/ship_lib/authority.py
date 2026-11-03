"""Production signing authority initialization and recovery verification."""

from __future__ import annotations

import hashlib
import os
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path

from .config import ShipConfig
from .errors import ShipConfigError, ShipError
from .recovery import (
    assert_dpapi_alone_insufficient_label,
    ensure_recovery_destination,
    ensure_recovery_key_destination,
    write_and_verify_independent_recovery,
)
from .secret_store import (
    SecretStore,
    read_sealed_file,
    select_secret_store,
    write_sealed_file,
)

# Mirror kindle_release_sign known test keys — never activate these as production.
_KNOWN_TEST_PUBLIC_KEYS = {
    bytes.fromhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"),
    bytes.fromhex("ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"),
    bytes.fromhex("1398f62c6d1a457c51ba6a4b5f3dbd2f69fca93216218dc8997e416bd17d93ca"),
}

_SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")
_META_SCHEMA = "ignite-ship-authority-meta-v2"


@dataclass(frozen=True)
class PublisherAuthority:
    public_key: bytes
    fingerprint_sha256: str
    public_key_path: Path
    sealed_private_path: Path
    store_name: str
    recovery_verified: bool
    recovery_mechanism: str


def authority_present(config: ShipConfig) -> bool:
    return (
        config.sealed_private_path.is_file()
        and config.public_key_path.is_file()
        and config.fingerprint_path.is_file()
        and config.authority_meta_path.is_file()
    )


def load_authority(config: ShipConfig) -> PublisherAuthority:
    if not authority_present(config):
        raise ShipError("publisher authority is not initialized")
    public = _load_public_hex(config.public_key_path)
    fingerprint = config.fingerprint_path.read_text(encoding="ascii").strip()
    expected = _fingerprint(public)
    if fingerprint != expected:
        raise ShipError("publisher fingerprint does not match public key")
    meta = config.authority_meta_path.read_text(encoding="ascii")
    if f"schema={_META_SCHEMA}" not in meta and "schema=ignite-ship-authority-meta-v1" not in meta:
        raise ShipError("authority metadata schema mismatch")
    recovery_verified = "recovery_verified=true" in meta
    independent = "independent_recovery_verified=true" in meta
    if "schema=ignite-ship-authority-meta-v1" in meta and not independent:
        # Legacy DPAPI-only "recovery" is not accepted as production authority.
        recovery_verified = False
    store_name = "unknown"
    mechanism = "unset"
    for line in meta.splitlines():
        if line.startswith("secret_store="):
            store_name = line.split("=", 1)[1]
        if line.startswith("recovery_mechanism="):
            mechanism = line.split("=", 1)[1]
    return PublisherAuthority(
        public_key=public,
        fingerprint_sha256=fingerprint,
        public_key_path=config.public_key_path,
        sealed_private_path=config.sealed_private_path,
        store_name=store_name,
        recovery_verified=recovery_verified and independent,
        recovery_mechanism=mechanism,
    )


def initialize_authority(
    config: ShipConfig,
    *,
    store: SecretStore | None = None,
    force: bool = False,
    production: bool = False,
) -> PublisherAuthority:
    """One-time production publisher identity initialization.

    Never regenerates an existing authority unless ``force`` is set (tests only).
    Requires independent off-machine recovery (OpenSSL + recovery authority key).
    Same-user DPAPI round-trip alone does not satisfy recovery requirements.
    """
    if authority_present(config) and not force:
        raise ShipError(
            "publisher authority already exists; refusing automatic regeneration"
        )
    recovery_dir = ensure_recovery_destination(
        config.recovery_dir,
        ship_home=config.ship_home,
        repo_root=config.repo_root,
    )
    recovery_key_dir = ensure_recovery_key_destination(
        config.recovery_key_dir,
        recovery_dir=recovery_dir,
        ship_home=config.ship_home,
        repo_root=config.repo_root,
    )
    if not force and config.pin_path.is_file():
        raw = config.pin_path.read_bytes()
        if raw != b"UNPROVISIONED\n" and len(raw) == 65:
            raise ShipError(
                "repository pin is already provisioned; refusing new authority init"
            )

    if store is None:
        store = select_secret_store(
            allow_insecure=config.allow_insecure_store and not production,
            production=production,
        )
    elif production and getattr(store, "name", "") == "insecure-file":
        raise ShipConfigError(
            "insecure signing store is forbidden for live / production authority activation"
        )

    config.authority_dir.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(config.authority_dir, 0o700)
    except OSError:
        pass

    with tempfile.TemporaryDirectory(prefix="ignite-ship-auth-") as tmp:
        work = Path(tmp)
        private_pem = work / "private.pem"
        public_der = work / "public.der"
        _openssl(["genpkey", "-algorithm", "ED25519", "-out", str(private_pem)])
        _openssl(
            [
                "pkey",
                "-in",
                str(private_pem),
                "-pubout",
                "-outform",
                "DER",
                "-out",
                str(public_der),
            ]
        )
        public = _public_from_der(public_der.read_bytes())
        if public in _KNOWN_TEST_PUBLIC_KEYS:
            raise ShipError("refusing known test public key as production authority")
        _verify_keypair(private_pem, public)
        pem_bytes = private_pem.read_bytes()

        # Independent recovery FIRST — refuse activation if it fails.
        recovery = write_and_verify_independent_recovery(
            recovery_dir=recovery_dir,
            recovery_key_dir=recovery_key_dir,
            private_pem=pem_bytes,
            public_key=public,
        )

        # Local at-rest seal (DPAPI / test store) — not a substitute for recovery.
        sealed = store.seal(pem_bytes)
        if store.unseal(sealed) != pem_bytes:
            raise ShipError("local secret-store seal round-trip failed")

        write_sealed_file(config.sealed_private_path, sealed)
        config.public_key_path.write_bytes(public.hex().encode("ascii") + b"\n")
        try:
            os.chmod(config.public_key_path, 0o644)
        except OSError:
            pass
        fingerprint = _fingerprint(public)
        config.fingerprint_path.write_text(fingerprint + "\n", encoding="ascii")
        meta = (
            f"schema={_META_SCHEMA}\n"
            f"secret_store={store.name}\n"
            f"fingerprint_sha256={fingerprint}\n"
            f"recovery_verified=true\n"
            f"independent_recovery_verified=true\n"
            f"recovery_mechanism={recovery.mechanism}\n"
            f"local_at_rest_note={assert_dpapi_alone_insufficient_label()}\n"
            f"public_key_hex={public.hex()}\n"
        )
        config.authority_meta_path.write_text(meta, encoding="ascii")

    return load_authority(config)


def export_private_pem_to_work(
    config: ShipConfig,
    work_dir: Path,
    *,
    store: SecretStore | None = None,
    production: bool = False,
) -> Path:
    """Unseal private PEM into an exclusive work file for a single signing call.

    Caller must delete the work directory. Never logs the PEM.
    """
    auth = load_authority(config)
    if not auth.recovery_verified:
        raise ShipError(
            "authority independent recovery was not verified; refusing to sign"
        )
    if store is None:
        store = select_secret_store(
            allow_insecure=config.allow_insecure_store and not production,
            production=production,
        )
    elif production and getattr(store, "name", "") == "insecure-file":
        raise ShipConfigError(
            "insecure signing store is forbidden for live / production signing"
        )
    sealed = read_sealed_file(config.sealed_private_path)
    pem = store.unseal(sealed)
    work_dir.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(work_dir, 0o700)
    except OSError:
        pass
    out = work_dir / "signing-private.pem"
    flags = os.O_CREAT | os.O_EXCL | os.O_WRONLY
    if hasattr(os, "O_BINARY"):
        flags |= os.O_BINARY
    fd = os.open(out, flags, 0o600)
    try:
        os.write(fd, pem)
    finally:
        os.close(fd)
    derived = _derive_public_from_pem(out)
    if derived != auth.public_key:
        out.unlink(missing_ok=True)
        raise ShipError("unsealed private key does not match authority public key")
    return out


def _fingerprint(public: bytes) -> str:
    return hashlib.sha256(_SPKI_PREFIX + public).hexdigest()


def _load_public_hex(path: Path) -> bytes:
    raw = path.read_bytes()
    if len(raw) != 65 or raw[-1:] != b"\n":
        raise ShipError("public key must be 64 lowercase hex characters plus LF")
    body = raw[:-1]
    if any(c not in b"0123456789abcdef" for c in body):
        raise ShipError("public key must be lowercase hexadecimal")
    key = bytes.fromhex(body.decode("ascii"))
    if key in _KNOWN_TEST_PUBLIC_KEYS:
        raise ShipError("known test key cannot be production authority")
    return key


def _public_from_der(der: bytes) -> bytes:
    if len(der) != 44 or not der.startswith(_SPKI_PREFIX):
        raise ShipError("unexpected Ed25519 public DER encoding")
    return der[12:]


def _derive_public_from_pem(private_pem: Path) -> bytes:
    with tempfile.TemporaryDirectory(prefix="ignite-ship-pub-") as tmp:
        der = Path(tmp) / "pub.der"
        _openssl(
            [
                "pkey",
                "-in",
                str(private_pem),
                "-pubout",
                "-outform",
                "DER",
                "-out",
                str(der),
            ]
        )
        return _public_from_der(der.read_bytes())


def _verify_keypair(private_pem: Path, public: bytes) -> None:
    with tempfile.TemporaryDirectory(prefix="ignite-ship-probe-") as tmp:
        work = Path(tmp)
        msg = work / "probe.bin"
        msg.write_bytes(b"ignite-ship-authority-probe-v1")
        sig = work / "probe.sig"
        pub_der = work / "pub.der"
        pub_der.write_bytes(_SPKI_PREFIX + public)
        pub_pem = work / "pub.pem"
        _openssl(
            ["pkey", "-pubin", "-inform", "DER", "-in", str(pub_der), "-out", str(pub_pem)]
        )
        _openssl(
            [
                "pkeyutl",
                "-sign",
                "-rawin",
                "-inkey",
                str(private_pem),
                "-in",
                str(msg),
                "-out",
                str(sig),
            ]
        )
        _openssl(
            [
                "pkeyutl",
                "-verify",
                "-rawin",
                "-pubin",
                "-inkey",
                str(pub_pem),
                "-in",
                str(msg),
                "-sigfile",
                str(sig),
            ]
        )


def _openssl(args: list[str]) -> None:
    try:
        completed = subprocess.run(
            ["openssl", *args],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    except OSError as exc:
        raise ShipError(f"cannot execute OpenSSL: {exc}") from exc
    if completed.returncode != 0:
        raise ShipError("OpenSSL operation failed during authority setup")
