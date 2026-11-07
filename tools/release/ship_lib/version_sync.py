"""Release version synchronization helpers."""

from __future__ import annotations

import copy
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

from .errors import ShipError

_VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$")
_TOML_VERSION_LINE = re.compile(
    r'(?m)^(version\s*=\s*")([^"]+)(")[ \t]*$',
)
_LOCK_NAME = re.compile(r'(?m)^name = "([^"]+)"')
_LOCK_VERSION_LINE = re.compile(r'(?m)^(version = ")([^"]+)(")[ \t]*$')

REQUIRED_CORE = ("ignite/VERSION", "Cargo.toml")
OPTIONAL_SYNC = (
    "Cargo.lock",
    "ignite/web/package.json",
    "ignite/web/package-lock.json",
)
COMPLETE_FILES = REQUIRED_CORE + OPTIONAL_SYNC


def canonical_checker_path() -> Path:
    return Path(__file__).resolve().parents[3] / "tools" / "ci" / "check_version_sync.py"


def validate_version_string(version: str) -> None:
    if not _VERSION_RE.fullmatch(version):
        raise ShipError(f"malformed release version: {version!r}")


def tag_for_version(version: str) -> str:
    validate_version_string(version)
    return f"v{version}"


def read_repo_version(repo_root: Path) -> str:
    path = repo_root / "ignite" / "VERSION"
    if not path.is_file():
        raise ShipError("missing required version files: ignite/VERSION")
    try:
        raw = path.read_text(encoding="utf-8").strip()
    except OSError as exc:
        raise ShipError(f"cannot read ignite/VERSION: {exc}") from exc
    validate_version_string(raw)
    return raw


@dataclass(frozen=True)
class FieldChange:
    path: str
    field: str
    old: str
    new: str


@dataclass
class VersionPlan:
    current: str
    target: str
    changes: tuple[FieldChange, ...]
    new_texts: dict[str, str] = field(default_factory=dict)

    @property
    def changed_paths(self) -> list[str]:
        return sorted(self.new_texts)


def format_plan(plan: VersionPlan) -> list[str]:
    lines = [f"VERSION PLAN: {plan.current} -> {plan.target}"]
    if not plan.changes:
        lines.append("  (no file changes)")
        return lines
    for change in plan.changes:
        lines.append(
            f"  {change.path}: {change.field} {change.old} -> {change.new}"
        )
    return lines


def apply_version(
    repo_root: Path,
    version: str,
    *,
    require_complete: bool = False,
) -> list[str]:
    """Write version across VERSION / Cargo workspace / web package files.

    Returns relative paths modified. Does not create a Git commit.
    """
    plan = plan_version(
        repo_root, version, require_complete=require_complete
    )
    return _commit_plan(repo_root, plan, require_complete=require_complete)


def plan_version(
    repo_root: Path,
    version: str,
    *,
    require_complete: bool = False,
) -> VersionPlan:
    """Compute file/field changes without writing. Does not invoke Git."""
    validate_version_string(version)
    repo_root = repo_root.resolve()
    _preflight_files(repo_root, require_complete=require_complete)
    current = read_repo_version(repo_root)
    files = _load_sync_files(repo_root, require_complete=require_complete)
    _assert_current_consistent(current, files)
    _run_checker_if_available(repo_root, require_complete=require_complete)
    return _build_plan(current, version, files)


def check_version(repo_root: Path, *, passthrough: bool = False) -> None:
    """Read-only complete-tree validation. Does not write or invoke Git."""
    repo_root = repo_root.resolve()
    _preflight_files(repo_root, require_complete=True)
    current = read_repo_version(repo_root)
    files = _load_sync_files(repo_root, require_complete=True)
    _assert_current_consistent(current, files)
    run_version_sync_check(
        repo_root,
        checker=canonical_checker_path(),
        passthrough=passthrough,
    )


def run_version_sync_check(
    repo_root: Path,
    *,
    checker: Path | None = None,
    passthrough: bool = False,
) -> None:
    script = checker
    if script is None:
        script = repo_root / "tools" / "ci" / "check_version_sync.py"
    if not script.is_file():
        # Disposable test trees may omit the checker; require VERSION grammar only.
        read_repo_version(repo_root)
        return
    env = dict(os.environ)
    env["IGNITE_VERSION_CHECK_ROOT"] = str(repo_root)
    completed = subprocess.run(
        [sys.executable, str(script)],
        cwd=str(repo_root),
        env=env,
        capture_output=not passthrough,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        detail = ""
        if not passthrough:
            detail = (completed.stderr or completed.stdout or "").strip()
        raise ShipError(
            "version synchronization check failed"
            + (f": {detail}" if detail else "")
        )


def _write_text(path: Path, text: str) -> None:
    path.write_text(text, encoding="utf-8")


def _preflight_files(repo_root: Path, *, require_complete: bool) -> None:
    required = COMPLETE_FILES if require_complete else REQUIRED_CORE
    missing = [rel for rel in required if not (repo_root / rel).is_file()]
    if missing:
        raise ShipError("missing required version files: " + ", ".join(missing))


def _run_checker_if_available(
    repo_root: Path, *, require_complete: bool
) -> None:
    if require_complete:
        checker = canonical_checker_path()
        if not checker.is_file():
            raise ShipError("tools/ci/check_version_sync.py is missing")
        run_version_sync_check(repo_root, checker=checker)
        return
    run_version_sync_check(repo_root)


def _load_toml(path: Path, rel: str) -> dict:
    try:
        import tomllib
    except ImportError as exc:  # pragma: no cover
        raise ShipError(
            "Python 3.11+ is required to parse Cargo.toml (tomllib)"
        ) from exc
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except OSError as exc:
        raise ShipError(f"cannot read {rel}: {exc}") from exc
    except Exception as exc:
        raise ShipError(f"malformed TOML: {rel}: {exc}") from exc
    if not isinstance(data, dict):
        raise ShipError(f"malformed TOML: {rel}: expected a table")
    return data


def _load_json_text(text: str, rel: str) -> dict:
    try:
        data = json.loads(text)
    except json.JSONDecodeError as exc:
        raise ShipError(f"malformed JSON: {rel}: {exc}") from exc
    if not isinstance(data, dict):
        raise ShipError(f"malformed JSON: {rel}: expected an object")
    return data


def _read_text(path: Path, rel: str) -> str:
    if not path.is_file():
        raise ShipError(f"missing required version files: {rel}")
    try:
        return path.read_text(encoding="utf-8")
    except OSError as exc:
        raise ShipError(f"cannot read {rel}: {exc}") from exc


@dataclass
class _Member:
    name: str
    rel: str
    text: str
    explicit_version: str | None


@dataclass
class _SyncFiles:
    version_text: str
    cargo_text: str
    cargo_data: dict
    members: list[_Member]
    lock_text: str | None
    web_text: str | None
    web_lock_text: str | None


def _load_sync_files(repo_root: Path, *, require_complete: bool) -> _SyncFiles:
    version_text = _read_text(repo_root / "ignite" / "VERSION", "ignite/VERSION")
    cargo_text = _read_text(repo_root / "Cargo.toml", "Cargo.toml")
    cargo_data = _load_toml(repo_root / "Cargo.toml", "Cargo.toml")
    members = _load_workspace_members(repo_root, cargo_data)
    lock_path = repo_root / "Cargo.lock"
    web_path = repo_root / "ignite" / "web" / "package.json"
    web_lock_path = repo_root / "ignite" / "web" / "package-lock.json"
    if require_complete:
        lock_text = _read_text(lock_path, "Cargo.lock")
        web_text = _read_text(web_path, "ignite/web/package.json")
        web_lock_text = _read_text(web_lock_path, "ignite/web/package-lock.json")
    else:
        lock_text = (
            _read_text(lock_path, "Cargo.lock") if lock_path.is_file() else None
        )
        web_text = (
            _read_text(web_path, "ignite/web/package.json")
            if web_path.is_file()
            else None
        )
        web_lock_text = (
            _read_text(web_lock_path, "ignite/web/package-lock.json")
            if web_lock_path.is_file()
            else None
        )
    return _SyncFiles(
        version_text=version_text,
        cargo_text=cargo_text,
        cargo_data=cargo_data,
        members=members,
        lock_text=lock_text,
        web_text=web_text,
        web_lock_text=web_lock_text,
    )


def _load_workspace_members(repo_root: Path, cargo_data: dict) -> list[_Member]:
    members_field = cargo_data.get("workspace", {}).get("members", [])
    if not isinstance(members_field, list):
        raise ShipError("Cargo.toml workspace.members is not a list")
    loaded: list[_Member] = []
    seen: set[str] = set()
    for member in members_field:
        if not isinstance(member, str) or not member.strip():
            raise ShipError(
                f"Cargo.toml workspace.members has an invalid entry: {member!r}"
            )
        rel = member.replace("\\", "/") + "/Cargo.toml"
        path = repo_root / member / "Cargo.toml"
        if not path.is_file():
            raise ShipError(
                f"inconsistent workspace members: missing {rel}"
            )
        data = _load_toml(path, rel)
        name = data.get("package", {}).get("name")
        if not isinstance(name, str) or not name:
            raise ShipError(f"{rel} is missing package.name")
        if name in seen:
            raise ShipError(
                f"inconsistent workspace members: duplicate package name {name!r}"
            )
        seen.add(name)
        ver_field = data.get("package", {}).get("version")
        if isinstance(ver_field, dict) and ver_field.get("workspace") is True:
            explicit = None
        elif isinstance(ver_field, str):
            explicit = ver_field
        else:
            raise ShipError(
                f"{rel} package.version {ver_field!r} is not workspace-inherited "
                "or an explicit string"
            )
        loaded.append(
            _Member(
                name=name,
                rel=rel,
                text=_read_text(path, rel),
                explicit_version=explicit,
            )
        )
    return loaded


def _workspace_package_version(cargo_data: dict) -> str:
    try:
        version = cargo_data["workspace"]["package"]["version"]
    except (KeyError, TypeError) as exc:
        raise ShipError("Cargo.toml missing [workspace.package] version") from exc
    if not isinstance(version, str):
        raise ShipError(
            f"Cargo.toml workspace.package.version {version!r} is not a string"
        )
    return version


def _assert_current_consistent(current: str, files: _SyncFiles) -> None:
    ws_ver = _workspace_package_version(files.cargo_data)
    if ws_ver != current:
        raise ShipError(
            f"Cargo.toml workspace.package.version {ws_ver!r} != ignite/VERSION {current!r}"
        )
    for member in files.members:
        if member.explicit_version is None:
            continue
        if member.explicit_version != current:
            raise ShipError(
                f"{member.rel} package.version {member.explicit_version!r} "
                f"is not workspace={current!r}"
            )
    if files.lock_text is not None:
        _assert_cargo_lock_current(files.lock_text, files.members, current)
    if files.web_text is not None:
        web = _load_json_text(files.web_text, "ignite/web/package.json")
        got = web.get("version")
        if got != current:
            raise ShipError(
                f"ignite/web/package.json version {got!r} != {current!r}"
            )
    if files.web_lock_text is not None:
        lock = _load_json_text(
            files.web_lock_text, "ignite/web/package-lock.json"
        )
        got = lock.get("version")
        if got != current:
            raise ShipError(
                f"ignite/web/package-lock.json version {got!r} != {current!r}"
            )
        root_pkg = (lock.get("packages") or {}).get("") or {}
        pkg_ver = root_pkg.get("version") if isinstance(root_pkg, dict) else None
        if pkg_ver != current:
            raise ShipError(
                'ignite/web/package-lock.json packages[""].version '
                f"{pkg_ver!r} != {current!r}"
            )


def _assert_cargo_lock_current(
    lock_text: str, members: list[_Member], current: str
) -> None:
    names = {member.name for member in members}
    if not names:
        return
    found: set[str] = set()
    for block in re.split(r"(?m)(?=^\[\[package\]\])", lock_text):
        if not block.startswith("[[package]]"):
            continue
        name_m = _LOCK_NAME.search(block)
        if not name_m or name_m.group(1) not in names:
            continue
        name = name_m.group(1)
        found.add(name)
        ver_m = _LOCK_VERSION_LINE.search(block)
        if not ver_m:
            raise ShipError(f"Cargo.lock {name} is missing a version field")
        locked = ver_m.group(2)
        if locked != current:
            raise ShipError(
                f"Cargo.lock {name} version {locked!r} != {current!r}"
            )
    missing = names - found
    if missing:
        raise ShipError(
            "Cargo.lock missing workspace package: "
            + ", ".join(sorted(missing))
        )


def _build_plan(current: str, target: str, files: _SyncFiles) -> VersionPlan:
    changes: list[FieldChange] = []
    new_texts: dict[str, str] = {}

    version_out = target + "\n"
    if files.version_text != version_out:
        if files.version_text.strip() != target:
            changes.append(
                FieldChange(
                    "ignite/VERSION", "value", current, target
                )
            )
        new_texts["ignite/VERSION"] = version_out

    cargo_out = _replace_toml_section_version(
        files.cargo_text,
        "[workspace.package]",
        target,
        rel="Cargo.toml",
        field="workspace.package.version",
    )
    if cargo_out != files.cargo_text:
        changes.append(
            FieldChange(
                "Cargo.toml", "workspace.package.version", current, target
            )
        )
        new_texts["Cargo.toml"] = cargo_out

    for member in files.members:
        if member.explicit_version is None:
            continue
        member_out = _replace_toml_section_version(
            member.text,
            "[package]",
            target,
            rel=member.rel,
            field="package.version",
        )
        if member_out != member.text:
            changes.append(
                FieldChange(member.rel, "package.version", current, target)
            )
            new_texts[member.rel] = member_out

    if files.lock_text is not None:
        lock_out, lock_changes = _plan_cargo_lock(
            files.lock_text, files.members, current, target
        )
        changes.extend(lock_changes)
        if lock_out != files.lock_text:
            new_texts["Cargo.lock"] = lock_out

    if files.web_text is not None:
        web_out = _plan_package_json(files.web_text, target)
        if web_out != files.web_text:
            changes.append(
                FieldChange(
                    "ignite/web/package.json", "version", current, target
                )
            )
            new_texts["ignite/web/package.json"] = web_out

    if files.web_lock_text is not None:
        lock_out = _plan_package_lock(files.web_lock_text, target)
        if lock_out != files.web_lock_text:
            changes.append(
                FieldChange(
                    "ignite/web/package-lock.json", "version", current, target
                )
            )
            changes.append(
                FieldChange(
                    "ignite/web/package-lock.json",
                    'packages[""].version',
                    current,
                    target,
                )
            )
            new_texts["ignite/web/package-lock.json"] = lock_out

    return VersionPlan(
        current=current,
        target=target,
        changes=tuple(changes),
        new_texts=new_texts,
    )


def _replace_toml_section_version(
    text: str,
    marker: str,
    version: str,
    *,
    rel: str,
    field: str,
) -> str:
    if marker not in text:
        raise ShipError(f"{rel} missing {marker}")
    head, rest = text.split(marker, 1)
    section, sep, tail = rest.partition("\n[")
    new_section, count = _TOML_VERSION_LINE.subn(
        rf"\g<1>{version}\g<3>",
        section,
        count=1,
    )
    if count != 1:
        raise ShipError(f"{rel} {field} not found")
    return head + marker + new_section + (sep + tail if sep else "")


def _plan_cargo_lock(
    original: str,
    members: list[_Member],
    current: str,
    target: str,
) -> tuple[str, list[FieldChange]]:
    names = {member.name for member in members}
    if not names:
        return original, []
    blocks = re.split(r"(?m)(?=^\[\[package\]\])", original)
    out: list[str] = []
    changes: list[FieldChange] = []
    seen: set[str] = set()
    for block in blocks:
        if not block.startswith("[[package]]"):
            out.append(block)
            continue
        name_m = _LOCK_NAME.search(block)
        name = name_m.group(1) if name_m else None
        if name and name in names:
            new_block, n = _LOCK_VERSION_LINE.subn(
                rf"\g<1>{target}\g<3>",
                block,
                count=1,
            )
            if n != 1:
                raise ShipError(f"Cargo.lock {name} is missing a version field")
            if new_block != block:
                if name not in seen:
                    changes.append(
                        FieldChange(
                            "Cargo.lock",
                            f"package[{name}].version",
                            current,
                            target,
                        )
                    )
                    seen.add(name)
            block = new_block
        out.append(block)
    new_text = "".join(out)
    _assert_lock_only_workspace_versions_changed(original, new_text, names)
    return new_text, changes


def _assert_lock_only_workspace_versions_changed(
    original: str, new_text: str, names: set[str]
) -> None:
    old_blocks = re.split(r"(?m)(?=^\[\[package\]\])", original)
    new_blocks = re.split(r"(?m)(?=^\[\[package\]\])", new_text)
    if len(old_blocks) != len(new_blocks):
        raise ShipError("unexpected Cargo.lock modification (package block count)")
    for old, new in zip(old_blocks, new_blocks):
        if old == new:
            continue
        if not old.startswith("[[package]]") or not new.startswith("[[package]]"):
            raise ShipError("unexpected Cargo.lock modification (header/metadata)")
        name_m = _LOCK_NAME.search(old)
        new_name_m = _LOCK_NAME.search(new)
        if not name_m or not new_name_m or name_m.group(1) != new_name_m.group(1):
            raise ShipError("unexpected Cargo.lock modification (package name)")
        if name_m.group(1) not in names:
            raise ShipError(
                f"unexpected Cargo.lock modification (third-party {name_m.group(1)})"
            )
        old_rest, n_old = _LOCK_VERSION_LINE.subn(
            r"\g<1>PLACEHOLDER\g<3>", old, count=1
        )
        new_rest, n_new = _LOCK_VERSION_LINE.subn(
            r"\g<1>PLACEHOLDER\g<3>", new, count=1
        )
        if n_old != 1 or n_new != 1 or old_rest != new_rest:
            raise ShipError(
                f"unexpected Cargo.lock modification (package[{name_m.group(1)}])"
            )


def _plan_package_json(text: str, target: str) -> str:
    data = _load_json_text(text, "ignite/web/package.json")
    if data.get("version") == target:
        return text
    new_text = _json_replace_string_path(
        text, ("version",), target, rel="ignite/web/package.json"
    )
    new_data = _load_json_text(new_text, "ignite/web/package.json")
    expected = copy.deepcopy(data)
    expected["version"] = target
    if new_data != expected:
        raise ShipError(
            "unexpected modification in ignite/web/package.json"
        )
    return new_text


def _plan_package_lock(text: str, target: str) -> str:
    data = _load_json_text(text, "ignite/web/package-lock.json")
    root_pkg = (data.get("packages") or {}).get("")
    if not isinstance(root_pkg, dict) or "version" not in root_pkg:
        raise ShipError('ignite/web/package-lock.json missing packages[""].version')
    if data.get("version") == target and root_pkg.get("version") == target:
        return text
    new_text = _json_replace_string_path(
        text, ("version",), target, rel="ignite/web/package-lock.json"
    )
    new_text = _json_replace_string_path(
        new_text,
        ("packages", "", "version"),
        target,
        rel="ignite/web/package-lock.json",
    )
    new_data = _load_json_text(new_text, "ignite/web/package-lock.json")
    expected = copy.deepcopy(data)
    expected["version"] = target
    expected.setdefault("packages", {}).setdefault("", {})["version"] = target
    if new_data != expected:
        raise ShipError(
            "unexpected modification in ignite/web/package-lock.json"
        )
    return new_text


def _json_replace_string_path(
    text: str, path: tuple[str, ...], value: str, *, rel: str
) -> str:
    i = _skip_ws(text, 0)
    if i >= len(text) or text[i] != "{":
        raise ShipError(f"{rel}: expected a JSON object")
    finder = _JsonFind()
    _walk_json_object(text, i, path, finder, rel)
    if finder.hits != 1 or finder.span is None:
        raise ShipError(f"{rel}: JSON path {list(path)!r} not found")
    start, end = finder.span
    return text[:start] + value + text[end:]


class _JsonFind:
    span: tuple[int, int] | None = None
    hits: int = 0


def _skip_ws(s: str, i: int) -> int:
    n = len(s)
    while i < n and s[i] in " \t\r\n":
        i += 1
    return i


def _skip_json_string(s: str, i: int) -> int:
    if i >= len(s) or s[i] != '"':
        raise ShipError("malformed JSON string")
    i += 1
    n = len(s)
    while i < n:
        c = s[i]
        if c == "\\":
            i += 2
            continue
        if c == '"':
            return i + 1
        i += 1
    raise ShipError("unterminated JSON string")


def _read_json_key(s: str, i: int) -> tuple[str, int]:
    after = _skip_json_string(s, i)
    raw = s[i:after]
    try:
        key = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise ShipError(f"malformed JSON object key: {exc}") from exc
    if not isinstance(key, str):
        raise ShipError("malformed JSON object key")
    return key, after


def _skip_json_value(s: str, i: int) -> int:
    i = _skip_ws(s, i)
    if i >= len(s):
        raise ShipError("truncated JSON")
    c = s[i]
    if c == '"':
        return _skip_json_string(s, i)
    if c == "{":
        return _skip_json_object(s, i)
    if c == "[":
        return _skip_json_array(s, i)
    if s.startswith("true", i):
        return i + 4
    if s.startswith("false", i):
        return i + 5
    if s.startswith("null", i):
        return i + 4
    if c == "-" or c.isdigit():
        n = len(s)
        i += 1
        while i < n and s[i] in "0123456789.eE+-":
            i += 1
        return i
    raise ShipError(f"malformed JSON value at index {i}")


def _skip_json_object(s: str, i: int) -> int:
    return _walk_json_object(s, i, None, None, "json")


def _skip_json_array(s: str, i: int) -> int:
    i += 1
    i = _skip_ws(s, i)
    n = len(s)
    first = True
    while i < n:
        i = _skip_ws(s, i)
        if i < n and s[i] == "]":
            return i + 1
        if not first:
            if s[i] != ",":
                raise ShipError("malformed JSON array")
            i = _skip_ws(s, i + 1)
            if i < n and s[i] == "]":
                return i + 1
        first = False
        i = _skip_json_value(s, i)
    raise ShipError("unterminated JSON array")


def _walk_json_object(
    s: str,
    i: int,
    path: tuple[str, ...] | None,
    finder: _JsonFind | None,
    rel: str,
) -> int:
    if i >= len(s) or s[i] != "{":
        raise ShipError(f"{rel}: expected '{{'")
    i += 1
    n = len(s)
    first = True
    while i < n:
        i = _skip_ws(s, i)
        if i < n and s[i] == "}":
            return i + 1
        if not first:
            if s[i] != ",":
                raise ShipError(f"{rel}: malformed JSON object")
            i = _skip_ws(s, i + 1)
            if i < n and s[i] == "}":
                return i + 1
        first = False
        key, i = _read_json_key(s, i)
        i = _skip_ws(s, i)
        if i >= n or s[i] != ":":
            raise ShipError(f"{rel}: malformed JSON object")
        i = _skip_ws(s, i + 1)
        matched = path is not None and finder is not None and path and key == path[0]
        if matched and len(path) == 1:
            if s[i] != '"':
                raise ShipError(f"{rel}: {path[0]!r} is not a JSON string")
            start = i + 1
            after = _skip_json_string(s, i)
            finder.hits += 1
            finder.span = (start, after - 1)
            i = after
        elif matched and s[i] == "{":
            i = _walk_json_object(s, i, path[1:], finder, rel)
        else:
            i = _skip_json_value(s, i)
    raise ShipError(f"{rel}: unterminated JSON object")


def _restore_original_bytes(originals: dict[Path, bytes]) -> None:
    errors: list[str] = []
    for path, blob in originals.items():
        try:
            path.write_bytes(blob)
        except OSError as exc:
            errors.append(f"{path.as_posix()}: {exc}")
    if errors:
        raise ShipError(
            "version file restoration failed: " + "; ".join(errors)
        )


def _commit_plan(
    repo_root: Path,
    plan: VersionPlan,
    *,
    require_complete: bool,
) -> list[str]:
    updates = {
        (repo_root / rel): text for rel, text in plan.new_texts.items()
    }
    originals = {path: path.read_bytes() for path in updates}

    try:
        for path, text in updates.items():
            _write_text(path, text)
        _run_checker_if_available(repo_root, require_complete=require_complete)
    except BaseException as exc:
        try:
            _restore_original_bytes(originals)
        except ShipError as restore_exc:
            raise ShipError(
                f"{restore_exc} (original error: {type(exc).__name__}: {exc})"
            ) from exc
        if isinstance(exc, ShipError):
            raise
        if isinstance(exc, Exception):
            raise ShipError(
                f"version update failed and was rolled back: {exc}"
            ) from exc
        raise
    return plan.changed_paths
