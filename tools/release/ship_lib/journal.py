"""Persistent ship journal — no private-key or passphrase material."""

from __future__ import annotations

import json
import os
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .errors import ShipError
from .outcomes import SHIP_PHASES


_FORBIDDEN_KEYS = (
    "private",
    "passphrase",
    "password",
    "secret",
    "pem",
    "sealed",
    "token",
)


def _refuse_secrets(obj: Any, *, path: str = "") -> None:
    if isinstance(obj, dict):
        for key, value in obj.items():
            lowered = str(key).lower()
            if any(part in lowered for part in _FORBIDDEN_KEYS):
                raise ShipError(
                    f"refusing to journal sensitive field {path + key!r}"
                )
            _refuse_secrets(value, path=f"{path}{key}.")
    elif isinstance(obj, list):
        for i, item in enumerate(obj):
            _refuse_secrets(item, path=f"{path}{i}.")
    elif isinstance(obj, str) and "BEGIN" in obj and "PRIVATE" in obj:
        raise ShipError("refusing to journal PEM-like private material")


@dataclass
class ShipJournal:
    """Append-only style state for one release version."""

    path: Path
    version: str
    tag: str
    completed_phases: list[str] = field(default_factory=list)
    fields: dict[str, Any] = field(default_factory=dict)
    updated_unix: float = 0.0

    def is_complete(self, phase: str) -> bool:
        return phase in self.completed_phases

    def mark(self, phase: str, **fields: Any) -> None:
        if phase not in SHIP_PHASES:
            raise ShipError(f"unknown ship phase: {phase}")
        payload = dict(fields)
        _refuse_secrets(payload)
        if phase not in self.completed_phases:
            self.completed_phases.append(phase)
        self.fields.update(payload)
        self.updated_unix = time.time()
        self.save()

    def save(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        body = {
            "schema": "ignite-ship-journal-v1",
            "version": self.version,
            "tag": self.tag,
            "completed_phases": list(self.completed_phases),
            "fields": dict(self.fields),
            "updated_unix": self.updated_unix,
        }
        _refuse_secrets(body)
        tmp = self.path.with_suffix(self.path.suffix + ".tmp")
        data = (json.dumps(body, indent=2, sort_keys=True) + "\n").encode("ascii")
        fd = os.open(tmp, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        try:
            os.write(fd, data)
        finally:
            os.close(fd)
        os.replace(tmp, self.path)
        try:
            os.chmod(self.path, 0o600)
        except OSError:
            pass

    @classmethod
    def load(cls, path: Path) -> ShipJournal:
        raw = json.loads(path.read_text(encoding="ascii"))
        if raw.get("schema") != "ignite-ship-journal-v1":
            raise ShipError("unsupported ship journal schema")
        return cls(
            path=path,
            version=str(raw["version"]),
            tag=str(raw["tag"]),
            completed_phases=list(raw.get("completed_phases") or []),
            fields=dict(raw.get("fields") or {}),
            updated_unix=float(raw.get("updated_unix") or 0),
        )

    @classmethod
    def open_for_version(cls, journal_dir: Path, version: str) -> ShipJournal:
        tag = f"v{version}"
        path = journal_dir / f"ship-{version}.journal.v1.json"
        if path.is_file():
            journal = cls.load(path)
            if journal.version != version or journal.tag != tag:
                raise ShipError("journal version/tag mismatch")
            return journal
        journal = cls(path=path, version=version, tag=tag)
        journal.save()
        return journal
