import subprocess
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("reader_probe_isolation_preflight.sh")


class ReaderSandboxPreflightTests(unittest.TestCase):
    def test_posix_shell_syntax(self) -> None:
        proc = subprocess.run(
            ["sh", "-n", str(SCRIPT)], capture_output=True, text=True, check=False
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)

    def test_refuses_non_kindle_host(self) -> None:
        proc = subprocess.run(
            ["sh", str(SCRIPT)], capture_output=True, text=True, check=False
        )
        self.assertNotEqual(proc.returncode, 0)
        self.assertNotIn("READER_SANDBOX_PREFLIGHT_PASS", proc.stdout)

    def test_isolation_contract_is_explicit(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        for contract in (
            "unshare -m -n -p -f",
            "mount --make-rprivate /",
            "mount -t tmpfs",
            "mount -o remount,bind,ro",
            "mount -t proc",
            "chroot",
            "su -s /bin/sh nobody",
            "test ! -e /mnt/us/scribe-data",
            "test ! -e /mnt/us/scribe-app",
            "READER_SANDBOX_PREFLIGHT_PASS",
            "reader_sandbox_reader_executed=no",
        ):
            with self.subTest(contract=contract):
                self.assertIn(contract, source)


if __name__ == "__main__":
    unittest.main()
