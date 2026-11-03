#!/usr/bin/env python3
"""Build and verify canonical authenticated Kindle release metadata.

This helper intentionally uses OpenSSL's Ed25519 implementation rather than
accepting a private key in Python, on the command line, or in GitHub Actions.
It performs no network access and never builds or changes a release artifact.
"""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys


MANIFEST_SCHEMA = "ignite-kindle-release-v1"
SIGNATURE_SCHEMA = "ignite-kindle-release-signature-v1"
UNPROVISIONED = b"UNPROVISIONED\n"
PUBLIC_KEY_DER_PREFIX = bytes.fromhex("302a300506032b6570032100")

# RFC 8032 test vector 1. Its private seed is public and it must never become
# Ignite's official trust root, even if a fixture is copied into the pin file.
KNOWN_TEST_PUBLIC_KEYS = {
    bytes.fromhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"),
    bytes.fromhex("ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"),
    bytes.fromhex("1398f62c6d1a457c51ba6a4b5f3dbd2f69fca93216218dc8997e416bd17d93ca"),
}


class ReleaseSigningError(ValueError):
    """A safe, user-facing refusal with no private-key material."""


def valid_release_version(version: str) -> bool:
    if not version or len(version) > 64 or not version.isascii():
        return False
    core, separator, prerelease = version.partition("-")
    components = core.split(".")
    if len(components) != 3 or not all(_valid_numeric(item) for item in components):
        return False
    if not separator:
        return True
    if not prerelease:
        return False
    for identifier in prerelease.split("."):
        if not identifier or not all(char.isascii() and (char.isalnum() or char == "-") for char in identifier):
            return False
        if identifier.isdigit() and not _valid_numeric(identifier):
            return False
    return True


def _valid_numeric(identifier: str) -> bool:
    return bool(identifier) and identifier.isascii() and identifier.isdigit() and (
        identifier == "0" or not identifier.startswith("0")
    )


def version_from_tag(tag: str) -> str:
    if not tag.startswith("v") or not valid_release_version(tag[1:]):
        raise ReleaseSigningError(f"not a canonical Ignite release tag: {tag or 'none'}")
    return tag[1:]


def expected_installer_name(version: str) -> str:
    return f"Install Ignite v{version}.sh"


def _canonical_digest(value: str) -> str:
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        raise ReleaseSigningError("expected installer SHA-256 must be 64 lowercase hexadecimal characters")
    return value


def installer_identity(installer: Path) -> tuple[int, str]:
    try:
        stream = installer.open("rb")
    except OSError as error:
        raise ReleaseSigningError("cannot open the installer") from error
    count = 0
    digest = hashlib.sha256()
    try:
        with stream:
            while chunk := stream.read(1024 * 1024):
                count += len(chunk)
                digest.update(chunk)
    except OSError as error:
        raise ReleaseSigningError("cannot read the installer") from error
    if count == 0:
        raise ReleaseSigningError("installer must not be empty")
    return count, digest.hexdigest()


def build_manifest(installer: Path, tag: str, expected_sha256: str) -> bytes:
    version = version_from_tag(tag)
    wanted_name = expected_installer_name(version)
    if installer.name != wanted_name:
        raise ReleaseSigningError(
            f"installer basename must be exactly {wanted_name!r}, got {installer.name!r}"
        )
    expected_sha256 = _canonical_digest(expected_sha256)
    byte_count, actual_sha256 = installer_identity(installer)
    if actual_sha256 != expected_sha256:
        raise ReleaseSigningError(
            f"installer SHA-256 {actual_sha256} does not match the expected CI digest"
        )
    return (
        f"schema={MANIFEST_SCHEMA}\n"
        f"version={version}\n"
        f"tag={tag}\n"
        f"artifact={wanted_name}\n"
        f"bytes={byte_count}\n"
        f"sha256={actual_sha256}\n"
    ).encode("ascii")


def signature_envelope(signature: bytes) -> bytes:
    if len(signature) != 64:
        raise ReleaseSigningError("Ed25519 signature must contain exactly 64 bytes")
    return (
        f"schema={SIGNATURE_SCHEMA}\n"
        f"signature={signature.hex()}\n"
    ).encode("ascii")


def parse_signature_hex(value: str) -> bytes:
    if not re.fullmatch(r"[0-9a-f]{128}", value):
        raise ReleaseSigningError("signature must be 128 lowercase hexadecimal characters")
    return bytes.fromhex(value)


def load_public_key(path: Path, *, official: bool) -> bytes:
    try:
        encoded = path.read_bytes()
    except OSError as error:
        raise ReleaseSigningError("cannot read the release public key") from error
    if encoded == UNPROVISIONED:
        raise ReleaseSigningError("official release public key is not provisioned")
    if len(encoded) != 65 or encoded[-1:] != b"\n" or not re.fullmatch(b"[0-9a-f]{64}", encoded[:-1]):
        raise ReleaseSigningError(
            "release public key must be exactly 32 raw Ed25519 bytes encoded as 64 lowercase hex characters plus LF"
        )
    key = bytes.fromhex(encoded[:-1].decode("ascii"))
    if official and key in KNOWN_TEST_PUBLIC_KEYS:
        raise ReleaseSigningError("a known test key cannot be used as the official release public key")
    return key


def _run_openssl(arguments: list[str], *, env: dict[str, str] | None = None) -> None:
    try:
        completed = subprocess.run(
            ["openssl", *arguments],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
            env=env,
        )
    except OSError as error:
        raise ReleaseSigningError("cannot execute OpenSSL") from error
    if completed.returncode != 0:
        raise ReleaseSigningError("OpenSSL Ed25519 operation failed")


def verify_signature(manifest: bytes, signature: bytes, public_key: bytes, work_dir: Path) -> None:
    manifest_path = work_dir / "manifest"
    signature_path = work_dir / "signature"
    public_der_path = work_dir / "public-key.der"
    manifest_path.write_bytes(manifest)
    signature_path.write_bytes(signature)
    public_der_path.write_bytes(PUBLIC_KEY_DER_PREFIX + public_key)
    _run_openssl(
        [
            "pkeyutl",
            "-verify",
            "-pubin",
            "-keyform",
            "DER",
            "-rawin",
            "-inkey",
            os.fspath(public_der_path),
            "-in",
            os.fspath(manifest_path),
            "-sigfile",
            os.fspath(signature_path),
        ]
    )


def sign_manifest(manifest: bytes, private_key: Path, work_dir: Path) -> bytes:
    if not private_key.is_file():
        raise ReleaseSigningError("offline private key file is unavailable")
    manifest_path = work_dir / "manifest-to-sign"
    signature_path = work_dir / "raw-signature"
    manifest_path.write_bytes(manifest)
    arguments = [
        "pkeyutl",
        "-sign",
        "-rawin",
        "-inkey",
        os.fspath(private_key),
        "-in",
        os.fspath(manifest_path),
        "-out",
        os.fspath(signature_path),
    ]
    child_env = os.environ.copy()
    if "IGNITE_SIGNING_KEY_PASSPHRASE" in child_env:
        arguments.extend(["-passin", "env:IGNITE_SIGNING_KEY_PASSPHRASE"])
    _run_openssl(arguments, env=child_env)
    try:
        signature = signature_path.read_bytes()
    except OSError as error:
        raise ReleaseSigningError("cannot read the generated signature") from error
    if len(signature) != 64:
        raise ReleaseSigningError("OpenSSL did not produce a 64-byte Ed25519 signature")
    return signature


def _write_outputs(output_dir: Path, tag: str, manifest: bytes, signature: bytes) -> tuple[Path, Path]:
    version = version_from_tag(tag)
    manifest_path = output_dir / f"ignite-kindle-v{version}.manifest"
    signature_path = output_dir / f"ignite-kindle-v{version}.manifest.sig"
    try:
        output_dir.mkdir(parents=True, exist_ok=True)
    except OSError as error:
        raise ReleaseSigningError("cannot create the metadata output directory") from error
    collisions = [path.name for path in (manifest_path, signature_path) if path.exists()]
    if collisions:
        raise ReleaseSigningError(f"refusing to replace existing metadata output: {', '.join(collisions)}")
    try:
        with manifest_path.open("xb") as output:
            output.write(manifest)
        try:
            with signature_path.open("xb") as output:
                output.write(signature_envelope(signature))
        except BaseException:
            manifest_path.unlink(missing_ok=True)
            raise
    except OSError as error:
        raise ReleaseSigningError("cannot write authenticated release metadata") from error
    return manifest_path, signature_path


def _temporary_directory(parent: Path):
    import tempfile

    return tempfile.TemporaryDirectory(prefix="ignite-release-sign-", dir=parent)


def command_sign(args: argparse.Namespace) -> None:
    installer = Path(args.installer)
    manifest = build_manifest(installer, args.tag, args.expected_sha256)
    public_key = load_public_key(Path(args.public_key), official=True)
    output_dir = Path(args.output_dir)
    output_dir.parent.mkdir(parents=True, exist_ok=True)
    with _temporary_directory(output_dir.parent) as temporary:
        work_dir = Path(temporary)
        signature = sign_manifest(manifest, Path(args.private_key), work_dir)
        verify_signature(manifest, signature, public_key, work_dir)
    manifest_path, signature_path = _write_outputs(output_dir, args.tag, manifest, signature)
    print(f"manifest={manifest_path}")
    print(f"signature_file={signature_path}")
    print(f"signature_hex={signature.hex()}")


def command_verify(args: argparse.Namespace) -> None:
    manifest = build_manifest(Path(args.installer), args.tag, args.expected_sha256)
    signature = parse_signature_hex(args.signature_hex)
    public_key = load_public_key(Path(args.public_key), official=True)
    output_dir = Path(args.output_dir)
    output_dir.parent.mkdir(parents=True, exist_ok=True)
    with _temporary_directory(output_dir.parent) as temporary:
        verify_signature(manifest, signature, public_key, Path(temporary))
    manifest_path, signature_path = _write_outputs(output_dir, args.tag, manifest, signature)
    print(f"verified_manifest={manifest_path}")
    print(f"verified_signature={signature_path}")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    subparsers = result.add_subparsers(dest="command", required=True)

    validate = subparsers.add_parser("version", help="validate a release tag and print its version")
    validate.add_argument("--tag", required=True)
    validate.set_defaults(handler=lambda args: print(version_from_tag(args.tag)))

    for name, handler in (("sign", command_sign), ("verify", command_verify)):
        subparser = subparsers.add_parser(name)
        subparser.add_argument("--installer", required=True)
        subparser.add_argument("--tag", required=True)
        subparser.add_argument("--expected-sha256", required=True)
        subparser.add_argument("--public-key", required=True)
        subparser.add_argument("--output-dir", required=True)
        subparser.set_defaults(handler=handler)
    subparsers.choices["sign"].add_argument("--private-key", required=True)
    subparsers.choices["verify"].add_argument("--signature-hex", required=True)
    return result


def main(argv: list[str] | None = None) -> int:
    try:
        args = parser().parse_args(argv)
        args.handler(args)
    except ReleaseSigningError as error:
        print(f"kindle_release_sign.py: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
