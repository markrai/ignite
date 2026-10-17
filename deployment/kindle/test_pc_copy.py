"""USB copy of the single Install Ignite file onto a mock Kindle filesystem.

The Windows PC copier is demoted. The customer path is Android USB copy of
one file into Documents. These tests cover that copy contract: notebooks stay
untouched, and only Install Ignite is placed in documents/.
"""

from __future__ import annotations

import shutil
import sys
from pathlib import Path

_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

from test_validate_stage import FIXTURE_VERSION, make_payload, write_sfx  # noqa: E402
from validate_stage import installer_sfx_name, validate_sfx  # noqa: E402


def _copy_installer(sfx: Path, kindle: Path) -> Path:
    dest = kindle / "documents" / installer_sfx_name(FIXTURE_VERSION)
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(sfx, dest)
    return dest


def _mock_kindle(root: Path) -> Path:
    kindle = root / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "system").mkdir(parents=True)
    (kindle / "system" / "version.txt").write_text("kindle\n", encoding="utf-8")
    notebooks = kindle / "scribe-data" / "notebook-1"
    notebooks.mkdir(parents=True)
    (notebooks / "page.meta").write_text("keep-me\n", encoding="utf-8")
    (kindle / "documents" / "My Clippings.txt").write_text("clippings\n", encoding="utf-8")
    return kindle


def test_copy_to_mock_kindle_preserves_notebooks(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    sfx = write_sfx(payload, tmp_path / installer_sfx_name(FIXTURE_VERSION))
    assert validate_sfx(sfx, FIXTURE_VERSION) == []
    kindle = _mock_kindle(tmp_path)
    dest = _copy_installer(sfx, kindle)
    assert dest.is_file()
    assert dest.read_bytes() == sfx.read_bytes()
    assert not (kindle / f"Ignite-Kindle-{FIXTURE_VERSION}").exists()
    assert (kindle / "scribe-data" / "notebook-1" / "page.meta").read_text(encoding="utf-8") == "keep-me\n"
    assert (kindle / "documents" / "My Clippings.txt").read_text(encoding="utf-8") == "clippings\n"


def test_copy_is_repeatable_upgrade(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    sfx = write_sfx(payload, tmp_path / installer_sfx_name(FIXTURE_VERSION))
    kindle = _mock_kindle(tmp_path)
    first = _copy_installer(sfx, kindle)
    second = _copy_installer(sfx, kindle)
    assert first.read_bytes() == second.read_bytes() == sfx.read_bytes()
    assert (kindle / "scribe-data" / "notebook-1" / "page.meta").is_file()


def test_copy_does_not_create_notebooks(tmp_path: Path) -> None:
    payload = make_payload(tmp_path / "src")
    sfx = write_sfx(payload, tmp_path / installer_sfx_name(FIXTURE_VERSION))
    kindle = tmp_path / "kindle"
    (kindle / "documents").mkdir(parents=True)
    (kindle / "system").mkdir()
    _copy_installer(sfx, kindle)
    assert not (kindle / "scribe-data").exists()
    assert (kindle / "documents" / installer_sfx_name(FIXTURE_VERSION)).is_file()
