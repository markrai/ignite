"""Persistent ship journal — phase states without private-key material."""

from __future__ import annotations

import json
import os
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .errors import ShipError
from .outcomes import PHASE_DONE, PHASE_PUBLISH, SHIP_PHASES, PhaseState


_FORBIDDEN_KEYS = (
    "private",
    "passphrase",
    "password",
    "secret",
    "pem",
    "sealed",
    "token",
    "recovery_key",
    "recovery-authority",
)

_JOURNAL_SCHEMA = "ignite-ship-journal-v2"


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
class PhaseRecord:
    state: PhaseState
    fields: dict[str, Any] = field(default_factory=dict)
    identity: dict[str, Any] = field(default_factory=dict)
    updated_unix: float = 0.0
    note: str = ""


@dataclass
class ShipJournal:
    """Durable per-version state. Completed != planned/deferred."""

    path: Path
    version: str
    tag: str
    phases: dict[str, PhaseRecord] = field(default_factory=dict)
    fields: dict[str, Any] = field(default_factory=dict)
    updated_unix: float = 0.0

    def state_of(self, phase: str) -> PhaseState | None:
        rec = self.phases.get(phase)
        return rec.state if rec else None

    def is_complete(self, phase: str) -> bool:
        return self.state_of(phase) == PhaseState.COMPLETED

    def is_deferred(self, phase: str) -> bool:
        return self.state_of(phase) == PhaseState.DEFERRED

    def is_planned(self, phase: str) -> bool:
        return self.state_of(phase) == PhaseState.PLANNED

    def record(
        self,
        phase: str,
        state: PhaseState,
        *,
        note: str = "",
        identity: dict[str, Any] | None = None,
        **fields: Any,
    ) -> None:
        if phase not in SHIP_PHASES:
            raise ShipError(f"unknown ship phase: {phase}")
        payload = dict(fields)
        ident = dict(identity or {})
        _refuse_secrets(payload)
        _refuse_secrets(ident)
        if state == PhaseState.COMPLETED:
            if phase == PHASE_PUBLISH and not payload.get("published"):
                raise ShipError(
                    "refusing to complete publish without published=true after verification"
                )
            if phase == PHASE_DONE and not self.is_complete(PHASE_PUBLISH):
                raise ShipError(
                    "refusing to complete done without verified publication"
                )
            if phase == PHASE_DONE and not bool(
                self.phases[PHASE_PUBLISH].fields.get("published")
            ):
                raise ShipError(
                    "refusing to complete done when publish.published is not true"
                )
        now = time.time()
        self.phases[phase] = PhaseRecord(
            state=state,
            fields=payload,
            identity=ident,
            updated_unix=now,
            note=note,
        )
        # Keep flat fields for identity lookups used across phases.
        self.fields.update(payload)
        self.fields.update(ident)
        self.updated_unix = now
        self.save()

    def mark_planned(self, phase: str, **fields: Any) -> None:
        self.record(phase, PhaseState.PLANNED, **fields)

    def mark_deferred(self, phase: str, **fields: Any) -> None:
        self.record(phase, PhaseState.DEFERRED, **fields)

    def mark_in_progress(self, phase: str, **fields: Any) -> None:
        self.record(phase, PhaseState.IN_PROGRESS, **fields)

    def mark_completed(
        self,
        phase: str,
        *,
        identity: dict[str, Any] | None = None,
        **fields: Any,
    ) -> None:
        self.record(
            phase, PhaseState.COMPLETED, identity=identity, **fields
        )

    def mark_failed(self, phase: str, **fields: Any) -> None:
        self.record(phase, PhaseState.FAILED, **fields)

    def mark_blocked(self, phase: str, **fields: Any) -> None:
        self.record(phase, PhaseState.BLOCKED, **fields)

    def published(self) -> bool:
        rec = self.phases.get(PHASE_PUBLISH)
        if rec is None or rec.state != PhaseState.COMPLETED:
            return False
        return bool(rec.fields.get("published")) and bool(
            rec.fields.get("independently_verified")
        )

    def save(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        body = {
            "schema": _JOURNAL_SCHEMA,
            "version": self.version,
            "tag": self.tag,
            "phases": {
                name: {
                    "state": rec.state.value,
                    "fields": dict(rec.fields),
                    "identity": dict(rec.identity),
                    "updated_unix": rec.updated_unix,
                    "note": rec.note,
                }
                for name, rec in self.phases.items()
            },
            "fields": dict(self.fields),
            "updated_unix": self.updated_unix,
        }
        _refuse_secrets(body)
        tmp = self.path.with_suffix(self.path.suffix + ".tmp")
        data = (json.dumps(body, indent=2, sort_keys=True) + "\n").encode("ascii")
        if tmp.exists():
            tmp.unlink()
        flags = os.O_CREAT | os.O_EXCL | os.O_WRONLY
        if hasattr(os, "O_BINARY"):
            flags |= os.O_BINARY
        fd = os.open(tmp, flags, 0o600)
        try:
            os.write(fd, data)
            os.fsync(fd)
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
        schema = raw.get("schema")
        if schema == "ignite-ship-journal-v1":
            return cls._migrate_v1(path, raw)
        if schema != _JOURNAL_SCHEMA:
            raise ShipError("unsupported ship journal schema")
        phases: dict[str, PhaseRecord] = {}
        for name, entry in (raw.get("phases") or {}).items():
            phases[name] = PhaseRecord(
                state=PhaseState(str(entry["state"])),
                fields=dict(entry.get("fields") or {}),
                identity=dict(entry.get("identity") or {}),
                updated_unix=float(entry.get("updated_unix") or 0),
                note=str(entry.get("note") or ""),
            )
        return cls(
            path=path,
            version=str(raw["version"]),
            tag=str(raw["tag"]),
            phases=phases,
            fields=dict(raw.get("fields") or {}),
            updated_unix=float(raw.get("updated_unix") or 0),
        )

    @classmethod
    def _migrate_v1(cls, path: Path, raw: dict[str, Any]) -> ShipJournal:
        """Import legacy journals without treating planned stubs as completed."""
        journal = cls(
            path=path,
            version=str(raw["version"]),
            tag=str(raw["tag"]),
            fields=dict(raw.get("fields") or {}),
            updated_unix=float(raw.get("updated_unix") or 0),
        )
        completed = list(raw.get("completed_phases") or [])
        fields = journal.fields
        for phase in completed:
            if phase not in SHIP_PHASES:
                continue
            # Legacy journals marked deferred/planned phases as "completed".
            deferred = bool(fields.get("deferred")) or fields.get(
                "dry_run_outcome"
            ) == "deferred-non-live"
            executed = fields.get("executed")
            if phase in {
                "git-prepare",
                "git-push-pr",
                "git-merge",
            } and not fields.get("commit_sha") and not fields.get("pr_number"):
                journal.phases[phase] = PhaseRecord(
                    state=PhaseState.PLANNED,
                    fields={},
                    note="migrated-from-v1-plan-stub",
                )
            elif phase == "publish" and (
                deferred or executed is False or not fields.get("published")
            ):
                journal.phases[phase] = PhaseRecord(
                    state=PhaseState.DEFERRED,
                    fields={"migrated": True},
                    note="migrated-from-v1-non-published",
                )
            elif phase == "done" and not fields.get("published"):
                journal.phases[phase] = PhaseRecord(
                    state=PhaseState.DEFERRED,
                    fields={},
                    note="migrated-from-v1-done-without-publish",
                )
            elif deferred and phase in {
                "ci-wait",
                "acquire",
                "sign",
                "verify-dry-run",
            }:
                journal.phases[phase] = PhaseRecord(
                    state=PhaseState.DEFERRED,
                    fields={},
                    note="migrated-from-v1-deferred",
                )
            else:
                journal.phases[phase] = PhaseRecord(
                    state=PhaseState.COMPLETED,
                    fields={},
                    note="migrated-from-v1",
                )
        journal.save()
        return journal

    @classmethod
    def open_for_version(cls, journal_dir: Path, version: str) -> ShipJournal:
        tag = f"v{version}"
        path = journal_dir / f"ship-{version}.journal.v2.json"
        legacy = journal_dir / f"ship-{version}.journal.v1.json"
        if path.is_file():
            journal = cls.load(path)
            if journal.version != version or journal.tag != tag:
                raise ShipError("journal version/tag mismatch")
            return journal
        if legacy.is_file():
            journal = cls.load(legacy)
            journal.path = path
            if journal.version != version or journal.tag != tag:
                raise ShipError("journal version/tag mismatch")
            journal.save()
            return journal
        journal = cls(path=path, version=version, tag=tag)
        journal.save()
        return journal
