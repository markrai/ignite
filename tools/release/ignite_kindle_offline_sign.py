#!/usr/bin/env python3
"""Maintainer entry point for the offline Kindle release ceremony (3n).

Delegates to deployment/kindle/offline_release_ceremony.py.
"""

from __future__ import annotations

import sys
from pathlib import Path

_ROOT = Path(__file__).resolve().parents[2]
_KINDLE = _ROOT / "deployment" / "kindle"
if str(_KINDLE) not in sys.path:
    sys.path.insert(0, str(_KINDLE))

from offline_release_ceremony import main  # noqa: E402

if __name__ == "__main__":
    raise SystemExit(main())
