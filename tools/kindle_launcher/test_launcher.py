from __future__ import annotations

import struct
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
LAUNCHER = ROOT / "tools" / "kindle_launcher" / "Scribe.sh"
ICON = ROOT / "tools" / "kindle_launcher" / "assets" / "scribe-icon.png"


class KindleLauncherTests(unittest.TestCase):
    def test_scriptlet_metadata_and_production_command(self) -> None:
        text = LAUNCHER.read_text(encoding="utf-8")
        self.assertTrue(text.startswith("#!/bin/sh\n"))
        self.assertIn("# Name: Ignite\n", text)
        self.assertIn("# Author: Mark Rai\n", text)
        self.assertIn("# Icon: /mnt/us/scribe-app/assets/scribe-icon.png\n", text)
        self.assertIn("# DontUseFBInk\n", text)
        self.assertIn("Discovery-only entry", text)
        self.assertIn("Must NOT exec scribe-app directly", text)
        self.assertIn("HANDLER=com.markrai.ignite", text)
        self.assertIn('URI="app://$HANDLER"', text)
        self.assertIn('lipc-set-prop com.lab126.appmgrd start "$URI"', text)
        self.assertNotIn('exec "$BINARY"', text)
        self.assertNotIn("--confirm-write", text)
        self.assertNotIn("scribe-test-data", text)

    def test_launcher_has_bounded_logs(self) -> None:
        text = LAUNCHER.read_text(encoding="utf-8")
        self.assertIn("launcher.log", text)
        self.assertIn("launcher.previous.log", text)
        self.assertIn('mv -f "$CURRENT_LOG" "$PREVIOUS_LOG"', text)
        self.assertNotIn("launch.lock", text)

    def test_launcher_does_not_manage_amazon_framework_or_databases(self) -> None:
        text = LAUNCHER.read_text(encoding="utf-8")
        for forbidden in ("lab126_gui", "cc.db", "appreg.db"):
            self.assertNotIn(forbidden, text)

    def test_legacy_ssh_deploy_helpers_absent(self) -> None:
        # Customer packaging uses Install Ignite Scriptlet; SSH/SCP deploy helpers
        # must not ship in the public tree.
        launcher_dir = ROOT / "tools" / "kindle_launcher"
        for name in ("deploy.sh", "remote_prepare.sh", "remote_commit.sh"):
            self.assertFalse(
                (launcher_dir / name).exists(),
                f"obsolete helper must remain removed: {name}",
            )

    def test_icon_is_book_cover_shaped_grayscale_png(self) -> None:
        # 2:3 portrait like Kindle library book covers (most thumbnails are 305x458), so the
        # Scribe tile takes the same height as the books around it.
        data = ICON.read_bytes()
        self.assertEqual(data[:8], b"\x89PNG\r\n\x1a\n")
        self.assertEqual(data[12:16], b"IHDR")
        width, height, bit_depth, color_type = struct.unpack(">IIBB", data[16:26])
        self.assertEqual((width, height), (610, 915))
        self.assertEqual(bit_depth, 8)
        self.assertEqual(color_type, 0, "icon must be encoded as grayscale")


if __name__ == "__main__":
    unittest.main()
