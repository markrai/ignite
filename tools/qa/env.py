"""Discover local toolchains. Never installs packages, targets, or browsers."""

from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


@dataclass
class Tool:
    name: str
    available: bool
    how: str = "missing"
    path: str = ""
    version: str = ""
    extra: dict[str, Any] = field(default_factory=dict)

    def as_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "name": self.name,
            "available": self.available,
            "how": self.how,
            "path": self.path,
            "version": self.version,
        }
        d.update(self.extra)
        return d


def repo_root_from_here() -> Path:
    return Path(__file__).resolve().parents[2]


def windows_to_wsl(path: Path) -> str:
    resolved = path.resolve()
    s = str(resolved)
    if len(s) >= 2 and s[1] == ":":
        return "/mnt/" + s[0].lower() + s[2:].replace("\\", "/")
    return s.replace("\\", "/")


def windows_to_docker(path: Path) -> str:
    resolved = path.resolve()
    s = str(resolved)
    if len(s) >= 2 and s[1] == ":":
        return s[0].lower() + ":/" + s[3:].replace("\\", "/")
    return s.replace("\\", "/")


def _run(argv: list[str], *, timeout: float = 20.0) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        argv,
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=timeout,
    )


def _which(name: str) -> str | None:
    found = shutil.which(name)
    return found


def _wsl_available() -> bool:
    if os.name != "nt":
        return False
    if not _which("wsl"):
        return False
    try:
        proc = _run(["wsl", "-e", "true"], timeout=8.0)
    except (OSError, subprocess.TimeoutExpired):
        return False
    return proc.returncode == 0


def _wsl(cmd: str, *, timeout: float = 20.0) -> subprocess.CompletedProcess[str]:
    return _run(["wsl", "-e", "bash", "-lc", cmd], timeout=timeout)


def read_pinned_rust_image(root: Path) -> str:
    text = (root / "tools" / "pinned-docker-images.txt").read_text(encoding="utf-8")
    for line in text.splitlines():
        if line.startswith("rust_armv7hf="):
            return line.split("=", 1)[1].strip()
    return ""


def discover(root: Path) -> dict[str, Any]:
    tools: dict[str, Tool] = {}
    native_cargo = _which("cargo")
    native_rustc = _which("rustc")
    native_node = _which("node")
    native_npm = _which("npm")
    native_npx = _which("npx")
    native_docker = _which("docker")
    native_git = _which("git")
    native_python = _which("python") or _which("python3")

    rust_how = "missing"
    rustc_ver = ""
    cargo_path = ""
    wasm32 = False
    native_wasm32 = False
    linux_cargo = False
    wsl = _wsl_available()

    if native_cargo and native_rustc:
        cargo_path = native_cargo
        rust_how = "native"
        try:
            rustc_ver = _run([native_rustc, "--version"]).stdout.strip()
        except (OSError, subprocess.TimeoutExpired):
            rustc_ver = ""
        if os.name != "nt":
            linux_cargo = True
        native_wasm32 = _native_has_wasm32()
        wasm32 = native_wasm32

    if wsl:
        try:
            wr = _wsl("command -v cargo && rustc --version && rustup target list --installed", timeout=25.0)
        except (OSError, subprocess.TimeoutExpired):
            wr = None
        if wr is not None and wr.returncode == 0:
            linux_cargo = True
            if rust_how == "missing":
                rust_how = "wsl"
                cargo_path = "wsl:cargo"
                rustc_ver = ""
            for line in wr.stdout.splitlines():
                if line.startswith("rustc "):
                    rustc_ver = rustc_ver or line.strip()
                if line.strip() == "wasm32-unknown-unknown":
                    wasm32 = True
            if rust_how == "native":
                rust_how = "native+wsl"

    tools["cargo"] = Tool(
        "cargo",
        rust_how != "missing",
        rust_how,
        cargo_path,
        rustc_ver,
        {
            "linux": linux_cargo,
            "wasm32_unknown_unknown": wasm32,
            "native_wasm32": native_wasm32,
            "wsl": wsl,
        },
    )
    tools["node"] = _tool_from_cmd("node", native_node, ["node", "--version"])
    tools["npm"] = _tool_from_cmd("npm", native_npm, ["npm", "--version"])
    tools["npx"] = Tool("npx", bool(native_npx), "native" if native_npx else "missing", native_npx or "")
    tools["docker"] = _tool_from_cmd("docker", native_docker, ["docker", "version", "--format", "{{.Server.Os}}"])
    tools["git"] = _tool_from_cmd("git", native_git, ["git", "--version"])
    tools["python"] = Tool("python", True, "native", native_python or "", "")

    return {
        "platform": f"{os.name}/{os.uname().sysname if hasattr(os, 'uname') else os.name}",
        "nt": os.name == "nt",
        "wsl_available": wsl,
        "linux_rust_available": linux_cargo,
        "wasm32_available": wasm32,
        "tools": {k: v.as_dict() for k, v in tools.items()},
        "pinned_rust_armv7hf_image": read_pinned_rust_image(root),
    }


def _native_has_wasm32() -> bool:
    rustup = _which("rustup")
    if rustup:
        try:
            proc = _run([rustup, "target", "list", "--installed"])
            return "wasm32-unknown-unknown" in proc.stdout
        except (OSError, subprocess.TimeoutExpired):
            return False
    # rustc without rustup cannot list targets reliably
    return False


def _tool_from_cmd(name: str, path: str | None, argv: list[str]) -> Tool:
    if not path:
        return Tool(name, False)
    try:
        proc = _run(argv)
        ver = (proc.stdout or proc.stderr).strip().splitlines()[0] if (proc.stdout or proc.stderr) else ""
    except (OSError, subprocess.TimeoutExpired):
        ver = ""
    return Tool(name, True, "native", path, ver)


# Generated outputs the runner itself may rewrite. Excluding them keeps a
# fingerprint stable across wasm/dist rebuilds so status can still detect a
# real source mismatch when combining host evidence.
_FINGERPRINT_EXCLUDE_PREFIXES = (
    "ignite/web/src/wasm",
    "ignite/web/dist",
    "target",
)


def _generated_path(path: str) -> bool:
    norm = path.replace("\\", "/").lstrip("./")
    return any(norm == prefix or norm.startswith(prefix + "/") for prefix in _FINGERPRINT_EXCLUDE_PREFIXES)


def _file_sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def _untracked_content_payload(root: Path, porcelain: str) -> str:
    """Hash contents of untracked source files so the fingerprint is not name-only."""
    lines: list[str] = []
    for ln in porcelain.splitlines():
        if not ln.startswith("??"):
            continue
        rel = ln[3:].strip().replace("\\", "/")
        if not rel or _generated_path(rel):
            continue
        path = root / rel
        files: list[Path]
        if path.is_dir():
            files = sorted(p for p in path.rglob("*") if p.is_file())
        elif path.is_file():
            files = [path]
        else:
            continue
        for fp in files:
            rel_f = fp.relative_to(root).as_posix()
            if _generated_path(rel_f):
                continue
            try:
                digest = _file_sha256(fp)
            except OSError:
                digest = "unreadable"
            lines.append(f"{rel_f}\0{digest}")
    return "\n".join(lines)


def git_identity(root: Path) -> dict[str, Any]:
    git = _which("git")
    if not git:
        return {"commit": "", "dirty": False, "describe": "", "available": False}
    try:
        commit = _run([git, "-C", str(root), "rev-parse", "HEAD"]).stdout.strip()
        porcelain = _run([git, "-C", str(root), "status", "--porcelain"]).stdout
        describe = _run([git, "-C", str(root), "describe", "--always", "--dirty"]).stdout.strip()
        diff = _run(
            [
                git,
                "-C",
                str(root),
                "diff",
                "HEAD",
                "--",
                ".",
                ":(exclude)ignite/web/src/wasm",
                ":(exclude)ignite/web/dist",
                ":(exclude)target",
            ],
            timeout=120.0,
        ).stdout
        porcelain_source = "\n".join(
            ln for ln in porcelain.splitlines() if ln.strip() and not _generated_path(ln[3:])
        )
        untracked = _untracked_content_payload(root, porcelain)
        payload = (commit + "\n" + porcelain_source + "\n" + diff + "\n" + untracked).encode(
            "utf-8", errors="replace"
        )
        fingerprint = hashlib.sha256(payload).hexdigest()
        return {
            "commit": commit,
            "dirty": bool(porcelain.strip()),
            "describe": describe,
            "available": True,
            "tree_fingerprint": fingerprint,
            "dirty_paths": [ln[3:] for ln in porcelain.splitlines() if ln.strip()][:40],
            "dirty_count": len([ln for ln in porcelain.splitlines() if ln.strip()]),
        }
    except (OSError, subprocess.TimeoutExpired):
        return {"commit": "", "dirty": False, "describe": "", "available": False}


def read_version(root: Path) -> str:
    return (root / "ignite" / "VERSION").read_text(encoding="utf-8").strip()
