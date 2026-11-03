#!/usr/bin/env python3
"""Disposable host tests for authenticated Kindle release metadata."""

from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


HERE = Path(__file__).resolve().parent
TOOL_PATH = HERE / "kindle_release_sign.py"
SPEC = importlib.util.spec_from_file_location("kindle_release_sign", TOOL_PATH)
assert SPEC and SPEC.loader
signer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(signer)


class KindleReleaseSigningTests(unittest.TestCase):
    maxDiff = None

    def setUp(self) -> None:
        self.assertIsNotNone(shutil.which("openssl"), "OpenSSL is required")
        self.temporary = tempfile.TemporaryDirectory(prefix="ignite-sign-test-")
        self.root = Path(self.temporary.name)
        self.version = "1.2.3"
        self.tag = f"v{self.version}"
        self.installer_bytes = b"exact CI-produced installer bytes\n"
        self.installer = self.root / f"Install Ignite v{self.version}.sh"
        self.installer.write_bytes(self.installer_bytes)
        self.digest = hashlib.sha256(self.installer_bytes).hexdigest()
        self.private_key, self.public_pin = self._make_key("signer")
        _, self.wrong_public_pin = self._make_key("wrong")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def _openssl(self, *arguments: str) -> None:
        subprocess.run(
            ["openssl", *arguments],
            check=True,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def _make_key(self, name: str) -> tuple[Path, Path]:
        private_key = self.root / f"{name}-private.pem"
        public_der = self.root / f"{name}-public.der"
        public_pin = self.root / f"{name}-public.hex"
        self._openssl("genpkey", "-algorithm", "ED25519", "-out", str(private_key))
        self._openssl(
            "pkey",
            "-in",
            str(private_key),
            "-pubout",
            "-outform",
            "DER",
            "-out",
            str(public_der),
        )
        encoded = public_der.read_bytes()
        self.assertEqual(encoded[:12], signer.PUBLIC_KEY_DER_PREFIX)
        self.assertEqual(len(encoded), 44)
        public_pin.write_text(encoded[12:].hex() + "\n", encoding="ascii", newline="\n")
        return private_key, public_pin

    def _sign(self, output_name: str = "signed") -> tuple[Path, str]:
        output = self.root / output_name
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            result = signer.main(
                [
                    "sign",
                    "--installer",
                    str(self.installer),
                    "--tag",
                    self.tag,
                    "--expected-sha256",
                    self.digest,
                    "--private-key",
                    str(self.private_key),
                    "--public-key",
                    str(self.public_pin),
                    "--output-dir",
                    str(output),
                ]
            )
        self.assertEqual(result, 0)
        signature_hex = next(
            line.removeprefix("signature_hex=")
            for line in stdout.getvalue().splitlines()
            if line.startswith("signature_hex=")
        )
        return output, signature_hex

    def _verify(
        self,
        signature_hex: str,
        *,
        output_name: str = "verified",
        public_pin: Path | None = None,
        installer: Path | None = None,
        tag: str | None = None,
        digest: str | None = None,
    ) -> tuple[int, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            result = signer.main(
                [
                    "verify",
                    "--installer",
                    str(installer or self.installer),
                    "--tag",
                    tag or self.tag,
                    "--expected-sha256",
                    digest or self.digest,
                    "--signature-hex",
                    signature_hex,
                    "--public-key",
                    str(public_pin or self.public_pin),
                    "--output-dir",
                    str(self.root / output_name),
                ]
            )
        return result, stderr.getvalue()

    def test_valid_sign_and_verify_emit_exact_cross_language_bytes(self) -> None:
        signed, signature_hex = self._sign()
        expected_manifest = (
            "schema=ignite-kindle-release-v1\n"
            "version=1.2.3\n"
            "tag=v1.2.3\n"
            "artifact=Install Ignite v1.2.3.sh\n"
            "bytes=34\n"
            "sha256=be3cf18e4bdc10a6132eb0df5e3879ba1e5d7b4b62ab81221c56edd6260ab96f\n"
        ).encode("ascii")
        self.assertEqual((signed / "ignite-kindle-v1.2.3.manifest").read_bytes(), expected_manifest)
        self.assertRegex(signature_hex, r"^[0-9a-f]{128}$")
        self.assertEqual(
            (signed / "ignite-kindle-v1.2.3.manifest.sig").read_bytes(),
            (
                "schema=ignite-kindle-release-signature-v1\n"
                f"signature={signature_hex}\n"
            ).encode("ascii"),
        )
        result, error = self._verify(signature_hex)
        self.assertEqual((result, error), (0, ""))
        self.assertEqual(
            (self.root / "verified" / "ignite-kindle-v1.2.3.manifest").read_bytes(),
            expected_manifest,
        )

    def test_modified_installer_and_signature_for_other_manifest_are_rejected(self) -> None:
        _, signature_hex = self._sign()
        self.installer.write_bytes(self.installer_bytes + b"changed")
        changed_digest = hashlib.sha256(self.installer.read_bytes()).hexdigest()
        result, error = self._verify(signature_hex, digest=changed_digest)
        self.assertNotEqual(result, 0)
        self.assertIn("OpenSSL Ed25519 operation failed", error)
        self.assertFalse((self.root / "verified").exists())

    def test_wrong_public_key_is_rejected(self) -> None:
        _, signature_hex = self._sign()
        result, error = self._verify(signature_hex, public_pin=self.wrong_public_pin)
        self.assertNotEqual(result, 0)
        self.assertIn("OpenSSL Ed25519 operation failed", error)

        output = self.root / "wrong-signing-key"
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            result = signer.main(
                [
                    "sign",
                    "--installer",
                    str(self.installer),
                    "--tag",
                    self.tag,
                    "--expected-sha256",
                    self.digest,
                    "--private-key",
                    str(self.private_key),
                    "--public-key",
                    str(self.wrong_public_pin),
                    "--output-dir",
                    str(output),
                ]
            )
        self.assertNotEqual(result, 0)
        self.assertIn("OpenSSL Ed25519 operation failed", stderr.getvalue())
        self.assertFalse(output.exists())

    def test_missing_malformed_and_noncanonical_signature_hex_is_rejected(self) -> None:
        for index, malformed in enumerate(("", "00", "g" * 128, "A" * 128, "0" * 129)):
            result, error = self._verify(malformed, output_name=f"malformed-{index}")
            self.assertNotEqual(result, 0)
            self.assertIn("128 lowercase hexadecimal", error)

    def test_wrong_digest_tag_and_artifact_basename_are_rejected_before_signing(self) -> None:
        with self.assertRaisesRegex(signer.ReleaseSigningError, "does not match the expected CI digest"):
            signer.build_manifest(self.installer, self.tag, "0" * 64)
        with self.assertRaisesRegex(signer.ReleaseSigningError, "canonical Ignite release tag"):
            signer.build_manifest(self.installer, "v01.2.3", self.digest)
        with self.assertRaisesRegex(signer.ReleaseSigningError, "basename must be exactly"):
            signer.build_manifest(self.installer, "v1.2.4", self.digest)
        renamed = self.root / "installer.sh"
        renamed.write_bytes(self.installer_bytes)
        with self.assertRaisesRegex(signer.ReleaseSigningError, "basename must be exactly"):
            signer.build_manifest(renamed, self.tag, self.digest)

    def test_version_grammar_boundaries_match_the_rust_contract(self) -> None:
        valid = ("0.0.0", "1.2.3", "10.20.30-rc.1", "1.2.3-alpha-beta.0")
        invalid = (
            "",
            "v1.2.3",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "1.2.3-",
            "1.2.3-.rc",
            "1.2.3-rc.",
            "1.2.3-rc..1",
            "1.2.3-01",
            "1.2.3+build",
            "1.2.3-rc+build",
            "1.2.3_rc",
            "1.2.3-rc/1",
            "1.2.3-" + "a" * 59,
        )
        for version in valid:
            self.assertTrue(signer.valid_release_version(version), version)
        for version in invalid:
            self.assertFalse(signer.valid_release_version(version), version)

    def test_unprovisioned_malformed_and_known_fixture_pins_fail_closed(self) -> None:
        unprovisioned = self.root / "unprovisioned.hex"
        unprovisioned.write_bytes(signer.UNPROVISIONED)
        with self.assertRaisesRegex(signer.ReleaseSigningError, "not provisioned"):
            signer.load_public_key(unprovisioned, official=True)

        malformed = self.root / "malformed.hex"
        malformed.write_text("AA" * 32 + "\n", encoding="ascii")
        with self.assertRaisesRegex(signer.ReleaseSigningError, "64 lowercase"):
            signer.load_public_key(malformed, official=True)

        for index, test_key in enumerate(signer.KNOWN_TEST_PUBLIC_KEYS):
            fixture = self.root / f"fixture-{index}.hex"
            fixture.write_text(
                test_key.hex() + "\n",
                encoding="ascii",
                newline="\n",
            )
            with self.assertRaisesRegex(signer.ReleaseSigningError, "known test key"):
                signer.load_public_key(fixture, official=True)

    def test_existing_metadata_outputs_are_never_replaced(self) -> None:
        output, _ = self._sign()
        before = {path.name: path.read_bytes() for path in output.iterdir()}
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            result = signer.main(
                [
                    "sign",
                    "--installer",
                    str(self.installer),
                    "--tag",
                    self.tag,
                    "--expected-sha256",
                    self.digest,
                    "--private-key",
                    str(self.private_key),
                    "--public-key",
                    str(self.public_pin),
                    "--output-dir",
                    str(output),
                ]
            )
        self.assertNotEqual(result, 0)
        self.assertIn("refusing to replace", stderr.getvalue())
        self.assertEqual(before, {path.name: path.read_bytes() for path in output.iterdir()})

    def test_private_key_contents_never_enter_subprocess_arguments_or_output(self) -> None:
        private_key = self.root / "opaque-private.pem"
        secret_contents = "PRIVATE-KEY-CONTENTS-MUST-NOT-LEAK"
        secret_passphrase = "PRIVATE-KEY-PASSPHRASE-MUST-NOT-LEAK"
        private_key.write_text(secret_contents, encoding="ascii")
        manifest = signer.build_manifest(self.installer, self.tag, self.digest)
        work = self.root / "mock-openssl"
        work.mkdir()
        captured_arguments: list[str] = []

        def fake_run(arguments, **_kwargs):
            captured_arguments.extend(arguments)
            output = Path(arguments[arguments.index("-out") + 1])
            output.write_bytes(b"s" * 64)
            return subprocess.CompletedProcess(arguments, 0)

        stdout = io.StringIO()
        stderr = io.StringIO()
        with mock.patch.object(signer.subprocess, "run", side_effect=fake_run):
            with mock.patch.dict(
                signer.os.environ,
                {"IGNITE_SIGNING_KEY_PASSPHRASE": secret_passphrase},
            ):
                with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                    signature = signer.sign_manifest(manifest, private_key, work)
        self.assertEqual(signature, b"s" * 64)
        self.assertNotIn(secret_contents, " ".join(captured_arguments))
        self.assertNotIn(secret_passphrase, " ".join(captured_arguments))
        combined_output = stdout.getvalue() + stderr.getvalue()
        self.assertNotIn(secret_contents, combined_output)
        self.assertNotIn(secret_passphrase, combined_output)


if __name__ == "__main__":
    unittest.main(verbosity=2)
