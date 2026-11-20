import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import verify_reader_probe_armv7 as probe


class ReleaseProbeVerifierTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.unstripped = Path(self.temp.name) / "reader-probe"
        self.stripped = Path(self.temp.name) / "reader-probe-armv7hf"
        self.ordinary = Path(self.temp.name) / "scribe-app"
        for path in (self.unstripped, self.stripped, self.ordinary):
            path.write_bytes(b"ELF-fixture")

    def verify(self, reader_symbols=None, probe_libraries=None):
        if reader_symbols is None:
            reader_symbols = "00000100 T fz_open_document\n"
        if probe_libraries is None:
            probe_libraries = [("libc.so.6", Path("/arm/libc.so.6"))]

        def fake_run(*argv):
            if str(self.unstripped) == argv[-1]:
                return reader_symbols
            if str(self.ordinary) == argv[-1]:
                return "00000200 T scribe_app_main\n"
            raise AssertionError(argv)

        with (
            patch.object(
                probe.existing,
                "verify_elf",
                side_effect=[
                    {"stripped": False},
                    {"stripped": True},
                    {"stripped": False},
                ],
            ),
            patch.object(probe.existing, "run", side_effect=fake_run),
            patch.object(
                probe.existing,
                "verify_dynamic_libraries",
                side_effect=[
                    probe_libraries,
                    probe_libraries,
                    [("libc.so.6", Path("/arm/libc.so.6"))],
                ],
            ),
            patch.object(
                probe.existing,
                "glibc_requirements",
                return_value=(["GLIBC_2.30"], "GLIBC_2.30"),
            ),
        ):
            return probe.verify_release_probe(
                self.unstripped, self.stripped, self.ordinary
            )

    def test_release_probe_passes_with_bounded_dependencies(self):
        report = self.verify()
        self.assertEqual(report["reader_probe_added_needed"], "none")
        self.assertEqual(report["reader_probe_device_execution"], "not_run")
        self.assertEqual(len(report["reader_probe_sha256"]), 64)

    def test_rejects_forbidden_data_symbol(self):
        with self.assertRaisesRegex(
            probe.existing.VerificationError, "gzip document-handler"
        ):
            self.verify(
                reader_symbols="00000100 T fz_open_document\n"
                "00000200 D gz_document_handler\n"
            )

    def test_rejects_unexpected_shared_library(self):
        with self.assertRaisesRegex(
            probe.existing.VerificationError, "libunexpected"
        ):
            self.verify(
                probe_libraries=[
                    ("libc.so.6", Path("/arm/libc.so.6")),
                    ("libunexpected.so", Path("/arm/libunexpected.so")),
                ]
            )

    def test_refuses_missing_probe_elf(self):
        self.stripped.unlink()
        with self.assertRaisesRegex(
            probe.existing.VerificationError, "missing ARMv7 executable"
        ):
            self.verify()


if __name__ == "__main__":
    unittest.main()
