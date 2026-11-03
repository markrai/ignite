"""Off-machine recovery for publisher private-key material.

Uses AES-256-GCM (via the ``cryptography`` library — standard authenticated
encryption) with a recovery decryption authority key stored in a **separate**
security domain from the encrypted backup destination.

``IGNITE_SHIP_RECOVERY_DIR`` holds only ciphertext + public materials.
``IGNITE_SHIP_RECOVERY_KEY_DIR`` holds only the recovery decryption key.

A compromise of the encrypted backup destination alone must not expose the
production private signing key.

A same-user DPAPI round-trip is **not** treated as independent recoverability.
"""

from __future__ import annotations

import hashlib
import os
import secrets
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path

from .errors import ShipConfigError, ShipError

_SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")
_RECOVERY_SCHEMA = "ignite-ship-recovery-v2"
_MECHANISM = "cryptography-aes-256-gcm"
_MAGIC = b"IGNITE-RECOVERY-AESGCM-V1\n"
_NONCE_LEN = 12
_KEY_LEN = 32


@dataclass(frozen=True)
class RecoveryBundle:
    encrypted_private_path: Path
    public_key_path: Path
    fingerprint_path: Path
    recovery_key_path: Path
    mechanism: str
    fingerprint_sha256: str
    verified_independent: bool
    recovery_dir: Path
    recovery_key_dir: Path


def recovery_backup_paths(recovery_dir: Path) -> dict[str, Path]:
    """Paths under the encrypted-backup destination (no decryption key)."""
    return {
        "encrypted": recovery_dir / "publisher-private.recovery.enc",
        "public": recovery_dir / "publisher-public.hex",
        "fingerprint": recovery_dir / "publisher-fingerprint.sha256",
        "meta": recovery_dir / "recovery.meta.v1",
    }


def recovery_key_path(recovery_key_dir: Path) -> Path:
    return recovery_key_dir / "recovery-authority.key"


def recovery_paths(recovery_dir: Path) -> dict[str, Path]:
    """Backward-compatible alias used by doctor status (backup domain only)."""
    paths = recovery_backup_paths(recovery_dir)
    paths["recovery_key"] = recovery_dir / "recovery-authority.key"  # must not exist
    return paths


def ensure_recovery_destination(
    recovery_dir: Path | None, *, ship_home: Path, repo_root: Path
) -> Path:
    if recovery_dir is None:
        raise ShipConfigError(
            "IGNITE_SHIP_RECOVERY_DIR is required for independent off-machine recovery"
        )
    recovery = recovery_dir.resolve()
    home = ship_home.resolve()
    root = repo_root.resolve()
    if recovery == home or home in recovery.parents or recovery in home.parents:
        raise ShipConfigError(
            "recovery destination must be independent of IGNITE_SHIP_HOME"
        )
    if root in recovery.parents or recovery == root or recovery in root.parents:
        raise ShipConfigError(
            "recovery destination must not be inside the Git repository"
        )
    return recovery


def ensure_recovery_key_destination(
    recovery_key_dir: Path | None,
    *,
    recovery_dir: Path,
    ship_home: Path,
    repo_root: Path,
) -> Path:
    """Require a decryption-authority directory separate from the backup domain."""
    if recovery_key_dir is None:
        raise ShipConfigError(
            "IGNITE_SHIP_RECOVERY_KEY_DIR is required and must be a separate "
            "security domain from IGNITE_SHIP_RECOVERY_DIR (decryption key must "
            "not share storage with the encrypted backup)"
        )
    key_dir = recovery_key_dir.resolve()
    backup = recovery_dir.resolve()
    home = ship_home.resolve()
    root = repo_root.resolve()
    if key_dir == backup or backup in key_dir.parents or key_dir in backup.parents:
        raise ShipConfigError(
            "IGNITE_SHIP_RECOVERY_KEY_DIR must not overlap IGNITE_SHIP_RECOVERY_DIR; "
            "a compromise of the backup destination alone must not expose the key"
        )
    if key_dir == home or home in key_dir.parents or key_dir in home.parents:
        raise ShipConfigError(
            "recovery key directory must be independent of IGNITE_SHIP_HOME"
        )
    if root in key_dir.parents or key_dir == root or key_dir in root.parents:
        raise ShipConfigError(
            "recovery key directory must not be inside the Git repository"
        )
    # Refuse a leftover key colocated with the backup (legacy misconfiguration).
    colocated = backup / "recovery-authority.key"
    if colocated.exists():
        raise ShipConfigError(
            "refusing recovery: decryption key found inside backup destination; "
            "move it to IGNITE_SHIP_RECOVERY_KEY_DIR and remove the colocated copy"
        )
    return key_dir


def write_and_verify_independent_recovery(
    *,
    recovery_dir: Path,
    recovery_key_dir: Path,
    private_pem: bytes,
    public_key: bytes,
) -> RecoveryBundle:
    """Encrypt private PEM into the backup domain; store key only in key domain."""
    recovery_dir.mkdir(parents=True, exist_ok=True)
    recovery_key_dir.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(recovery_dir, 0o700)
        os.chmod(recovery_key_dir, 0o700)
    except OSError:
        pass

    backup = recovery_backup_paths(recovery_dir)
    key_path = recovery_key_path(recovery_key_dir)
    created: list[Path] = []

    def _scrub() -> None:
        for path in created:
            if path.is_file():
                path.unlink(missing_ok=True)

    try:
        if key_path.exists() or backup["encrypted"].exists():
            raise ShipError(
                "recovery material already present; refusing to overwrite recovery authority"
            )

        recovery_key = secrets.token_bytes(_KEY_LEN)
        _write_exclusive(key_path, recovery_key.hex().encode("ascii") + b"\n")
        created.append(key_path)
        fingerprint = _fingerprint(public_key)

        ciphertext = _aesgcm_encrypt(recovery_key, private_pem)
        _write_exclusive(backup["encrypted"], ciphertext)
        created.append(backup["encrypted"])

        backup["public"].write_bytes(public_key.hex().encode("ascii") + b"\n")
        created.append(backup["public"])
        backup["fingerprint"].write_text(fingerprint + "\n", encoding="ascii")
        created.append(backup["fingerprint"])
        meta = (
            f"schema={_RECOVERY_SCHEMA}\n"
            f"mechanism={_MECHANISM}\n"
            f"fingerprint_sha256={fingerprint}\n"
            f"dpapi_alone_is_not_off_machine_recovery=true\n"
            f"independent_restore_required=true\n"
            f"key_domain_separated=true\n"
            f"backup_dir_holds_ciphertext_only=true\n"
        )
        backup["meta"].write_text(meta, encoding="ascii")
        created.append(backup["meta"])

        # Prove backup-alone cannot decrypt.
        if _backup_alone_can_decrypt(recovery_dir):
            _scrub()
            raise ShipError(
                "recovery key material leaked into backup destination; refusing activation"
            )

        verified = verify_independent_restore(
            recovery_dir=recovery_dir,
            recovery_key_dir=recovery_key_dir,
            expected_public=public_key,
            expected_fingerprint=fingerprint,
        )
        if not verified:
            _scrub()
            raise ShipError(
                "independent recovery restoration failed; authority not activated"
            )
    except Exception:
        _scrub()
        raise

    return RecoveryBundle(
        encrypted_private_path=backup["encrypted"],
        public_key_path=backup["public"],
        fingerprint_path=backup["fingerprint"],
        recovery_key_path=key_path,
        mechanism=_MECHANISM,
        fingerprint_sha256=fingerprint,
        verified_independent=True,
        recovery_dir=recovery_dir,
        recovery_key_dir=recovery_key_dir,
    )


def verify_independent_restore(
    *,
    recovery_dir: Path,
    recovery_key_dir: Path,
    expected_public: bytes,
    expected_fingerprint: str,
) -> bool:
    """Decrypt using only the separated recovery key + backup ciphertext.

    Intentionally does not call the local SecretStore / DPAPI.
    """
    backup = recovery_backup_paths(recovery_dir)
    key_path = recovery_key_path(recovery_key_dir)
    if not backup["encrypted"].is_file() or not key_path.is_file():
        return False
    if (recovery_dir / "recovery-authority.key").exists():
        return False
    try:
        key = bytes.fromhex(key_path.read_text(encoding="ascii").strip())
        if len(key) != _KEY_LEN:
            return False
        plaintext = _aesgcm_decrypt(key, backup["encrypted"].read_bytes())
    except (ShipError, ValueError, OSError):
        return False
    with tempfile.TemporaryDirectory(prefix="ignite-ship-rec-verify-") as tmp:
        restored = Path(tmp) / "restored.pem"
        restored.write_bytes(plaintext)
        try:
            derived = _derive_public_from_pem(restored)
        except ShipError:
            return False
        if derived != expected_public:
            return False
        if _fingerprint(derived) != expected_fingerprint:
            return False
        stored_fp = backup["fingerprint"].read_text(encoding="ascii").strip()
        if stored_fp != expected_fingerprint:
            return False
    return True


def _backup_alone_can_decrypt(recovery_dir: Path) -> bool:
    """True if a decryption key is present inside the backup domain (unsafe)."""
    return (recovery_dir / "recovery-authority.key").is_file()


def assert_dpapi_alone_insufficient_label() -> str:
    return (
        "local_store=windows-dpapi (at-rest only); "
        "off_machine_recovery=cryptography-aes-256-gcm with "
        "IGNITE_SHIP_RECOVERY_KEY_DIR separated from IGNITE_SHIP_RECOVERY_DIR; "
        "same-user DPAPI round-trip is NOT independent recovery; "
        "backup destination alone must not decrypt the private key"
    )


def _aesgcm_encrypt(key: bytes, plaintext: bytes) -> bytes:
    try:
        from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    except ImportError as exc:  # pragma: no cover
        raise ShipConfigError(
            "cryptography package required for AES-256-GCM recovery encryption"
        ) from exc
    nonce = secrets.token_bytes(_NONCE_LEN)
    aesgcm = AESGCM(key)
    ct = aesgcm.encrypt(nonce, plaintext, associated_data=b"ignite-ship-recovery-v2")
    return _MAGIC + nonce + ct


def _aesgcm_decrypt(key: bytes, blob: bytes) -> bytes:
    try:
        from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    except ImportError as exc:  # pragma: no cover
        raise ShipConfigError(
            "cryptography package required for AES-256-GCM recovery decryption"
        ) from exc
    if not blob.startswith(_MAGIC):
        raise ShipError("malformed recovery ciphertext magic")
    body = blob[len(_MAGIC) :]
    if len(body) < _NONCE_LEN + 16:
        raise ShipError("malformed recovery ciphertext")
    nonce, ct = body[:_NONCE_LEN], body[_NONCE_LEN:]
    aesgcm = AESGCM(key)
    try:
        return aesgcm.decrypt(nonce, ct, associated_data=b"ignite-ship-recovery-v2")
    except Exception as exc:
        raise ShipError("recovery AES-GCM decryption failed") from exc


def _derive_public_from_pem(private_pem: Path) -> bytes:
    with tempfile.TemporaryDirectory(prefix="ignite-ship-rec-pub-") as tmp:
        der = Path(tmp) / "pub.der"
        completed = subprocess.run(
            [
                "openssl",
                "pkey",
                "-in",
                str(private_pem),
                "-pubout",
                "-outform",
                "DER",
                "-out",
                str(der),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if completed.returncode != 0 or not der.is_file():
            raise ShipError("failed to derive public key from recovered PEM")
        der_bytes = der.read_bytes()
        if len(der_bytes) != 44 or not der_bytes.startswith(_SPKI_PREFIX):
            raise ShipError("unexpected Ed25519 public DER from recovery")
        return der_bytes[12:]


def _fingerprint(public: bytes) -> str:
    return hashlib.sha256(_SPKI_PREFIX + public).hexdigest()


def _write_exclusive(path: Path, data: bytes) -> None:
    flags = os.O_CREAT | os.O_EXCL | os.O_WRONLY
    if hasattr(os, "O_BINARY"):
        flags |= os.O_BINARY
    fd = os.open(path, flags, 0o600)
    try:
        os.write(fd, data)
        os.fsync(fd)
    finally:
        os.close(fd)
