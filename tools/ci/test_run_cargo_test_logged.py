#!/usr/bin/env python3
"""Regression tests for the exit-status-preserving Cargo test log helper."""

from __future__ import annotations

import importlib.util
import io
import sys
import tempfile
import unittest
from pathlib import Path


_SCRIPT = Path(__file__).with_name("run_cargo_test_logged.py")
_SPEC = importlib.util.spec_from_file_location("run_cargo_test_logged", _SCRIPT)
assert _SPEC is not None and _SPEC.loader is not None
_MODULE = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(_MODULE)


class RunCargoTestLoggedTests(unittest.TestCase):
    def test_success_keeps_complete_log_and_reports_success(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "success.log"
            terminal = io.StringIO()
            code = _MODULE.run_logged(
                [
                    sys.executable,
                    "-c",
                    "print('hidden compiler detail'); print('test result: ok. 2 passed')",
                ],
                log,
                terminal,
            )

            self.assertEqual(code, 0)
            self.assertIn("hidden compiler detail", log.read_text(encoding="utf-8"))
            self.assertNotIn("hidden compiler detail", terminal.getvalue())
            self.assertIn("cargo_test status=passed exit_code=0", terminal.getvalue())

    def test_failure_preserves_exit_status_and_never_reports_success(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "failure.log"
            terminal = io.StringIO()
            code = _MODULE.run_logged(
                [
                    sys.executable,
                    "-c",
                    "import sys; print('failure detail'); print('test result: FAILED'); sys.exit(7)",
                ],
                log,
                terminal,
            )

            self.assertEqual(code, 7)
            self.assertIn("failure detail", log.read_text(encoding="utf-8"))
            self.assertIn("failure detail", terminal.getvalue())
            self.assertIn("cargo_test status=failed exit_code=7", terminal.getvalue())
            self.assertNotIn("status=passed", terminal.getvalue())


if __name__ == "__main__":
    unittest.main()
