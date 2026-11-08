"""Compose existing CI/build commands into a reusable qualification run."""

from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import threading
import time
from pathlib import Path
from typing import Any, Callable

from elf import ElfError, inspect_armv7hf_elf
from env import (
    discover,
    git_identity,
    read_pinned_rust_image,
    read_version,
    windows_to_docker,
    windows_to_wsl,
)
from evidence import (
    SCHEMA,
    STATUS_FAIL,
    STATUS_NOT_RUN,
    STATUS_PASS,
    check,
    evaluate_states,
    group_status,
    utc_now,
)

GROUPS = ("identity", "rust", "armv7", "wasm", "web", "e2e")
ARM_BINS = ("ink-probe", "scribe-app", "scribe-lifecycle")

RunFn = Callable[[list[str], Path, dict[str, str] | None, float], dict[str, Any]]


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def _run_fields(result: dict[str, Any]) -> dict[str, Any]:
    keys = ("argv", "cwd", "exit_code", "seconds", "log")
    return {k: result[k] for k in keys if k in result}


def default_runner(
    argv: list[str],
    cwd: Path,
    env: dict[str, str] | None,
    timeout: float,
) -> dict[str, Any]:
    merged = os.environ.copy()
    # Host-agent sandbox redirects must not leak into qualification Cargo/npm/Playwright.
    for key in (
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET_DIR",
        "PLAYWRIGHT_BROWSERS_PATH",
        "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD",
    ):
        merged.pop(key, None)
    if env:
        merged.update(env)
    log_dir = merged.pop("IGNITE_QA_LOG_DIR", "") or os.environ.get("IGNITE_QA_LOG_DIR", "")
    log_name = merged.pop("IGNITE_QA_LOG_NAME", "") or "command"
    resolved = shutil.which(argv[0]) if argv else None
    run_argv = ([resolved] + list(argv[1:])) if resolved else list(argv)
    t0 = time.perf_counter()
    log_path: Path | None = None
    out_chunks: list[str] = []
    err = ""
    log_fh = None
    proc = None
    try:
        proc = subprocess.Popen(
            run_argv,
            cwd=str(cwd),
            env=merged,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        assert proc.stdout is not None
        if log_dir:
            Path(log_dir).mkdir(parents=True, exist_ok=True)
            log_path = Path(log_dir) / f"{log_name}.log"
            log_fh = log_path.open("w", encoding="utf-8", newline="\n")

        def _read() -> None:
            assert proc is not None and proc.stdout is not None
            for raw in iter(proc.stdout.readline, b""):
                line = raw.decode("utf-8", errors="replace")
                out_chunks.append(line)
                if log_fh is not None:
                    log_fh.write(line)
                    log_fh.flush()

        reader = threading.Thread(target=_read, daemon=True)
        reader.start()
        try:
            code = proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            proc.kill()
            code = 124
            err = f"timeout after {timeout}s"
        reader.join(timeout=10)
        out = "".join(out_chunks)
    except OSError as exc:
        out = ""
        err = str(exc)
        code = 127
    finally:
        if log_fh is not None:
            log_fh.close()
    result = {
        "argv": run_argv,
        "cwd": str(cwd),
        "exit_code": code,
        "seconds": round(time.perf_counter() - t0, 3),
        "stdout_tail": out[-4000:],
        "stderr_tail": err[-4000:],
    }
    if log_path is not None:
        result["log"] = str(log_path)
    return result


def _wsl_script(root: Path, body: str) -> list[str]:
    unix = windows_to_wsl(root)
    script = "set -euo pipefail\n" f"cd {unix}\n" + body
    return ["wsl", "-e", "bash", "-lc", script]


class Qualify:
    def __init__(
        self,
        root: Path,
        *,
        out_dir: Path,
        groups: tuple[str, ...],
        runner: RunFn | None = None,
        skip_env_probe: dict[str, Any] | None = None,
    ) -> None:
        self.root = root
        self.out_dir = out_dir
        self.groups = groups
        self.runner = runner or default_runner
        self.env_info = skip_env_probe if skip_env_probe is not None else discover(root)
        self.web = root / "ignite" / "web"

    def run(self) -> dict[str, Any]:
        self.out_dir.mkdir(parents=True, exist_ok=True)
        source = {
            **git_identity(self.root),
            "version": read_version(self.root),
            "toolchains": self.env_info,
        }
        report: dict[str, Any] = {
            "schema": SCHEMA,
            "generated_at": utc_now(),
            "command": "qualify",
            "source": source,
            "groups": {},
            "artifacts": {},
            "unresolved_gates": [],
        }
        dispatch = {
            "identity": self._identity,
            "rust": self._rust,
            "armv7": self._armv7,
            "wasm": self._wasm,
            "web": self._web,
            "e2e": self._e2e,
        }
        for name in GROUPS:
            if name not in self.groups:
                report["groups"][name] = {
                    "status": STATUS_NOT_RUN,
                    "checks": [
                        check(name=name, status=STATUS_NOT_RUN, detail="group not selected")
                    ],
                }
                continue
            checks, extra = dispatch[name]()
            report["groups"][name] = {"status": group_status(checks), "checks": checks}
            if extra:
                report.update(extra)
        if report.get("artifacts"):
            fingerprint = source.get("tree_fingerprint") or ""
            for meta in report["artifacts"].values():
                if isinstance(meta, dict):
                    meta["source_tree_fingerprint"] = fingerprint
        report["states"] = evaluate_states(
            report=report, preflight=None, physical_pass=None, live_source=source
        )
        selected = [n for n in self.groups]
        selected_status = [report["groups"][n]["status"] for n in selected]
        if any(s == STATUS_FAIL for s in selected_status):
            report["overall"] = STATUS_FAIL
            report["exit_code"] = 1
        elif any(s == STATUS_NOT_RUN for s in selected_status):
            report["overall"] = STATUS_NOT_RUN
            report["exit_code"] = 2
        else:
            report["overall"] = STATUS_PASS
            report["exit_code"] = 0
        return report

    def _run(
        self,
        argv: list[str],
        cwd: Path,
        *,
        timeout: float,
        env: dict[str, str] | None = None,
        log_name: str = "command",
    ) -> dict[str, Any]:
        merged = dict(env or {})
        merged["IGNITE_QA_LOG_DIR"] = str(self.out_dir / "logs")
        merged["IGNITE_QA_LOG_NAME"] = log_name
        return self.runner(argv, cwd, merged, timeout)

    def _identity(self) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        checks: list[dict[str, Any]] = []
        src_git = git_identity(self.root)
        if not src_git.get("available") or not src_git.get("commit"):
            checks.append(check(name="git", status=STATUS_FAIL, detail="git identity unavailable"))
        else:
            checks.append(
                check(
                    name="git",
                    status=STATUS_PASS,
                    detail=f"commit={src_git['commit']} dirty={src_git['dirty']} describe={src_git['describe']}",
                )
            )
        checker = self.root / "tools" / "ci" / "check_version_sync.py"
        result = self._run(["python", str(checker)], self.root, timeout=30, log_name="version-sync")
        if result["exit_code"] != 0:
            # Windows may need python3
            result = self._run(["python3", str(checker)], self.root, timeout=30, log_name="version-sync")
        checks.append(
            check(
                name="version-sync",
                status=STATUS_PASS if result["exit_code"] == 0 else STATUS_FAIL,
                detail="tools/ci/check_version_sync.py",
                extra={"run": _run_fields(result)},
            )
        )
        tools = self.env_info.get("tools") or {}
        for key in ("cargo", "node", "docker", "git"):
            t = tools.get(key) or {}
            checks.append(
                check(
                    name=f"toolchain-{key}",
                    status=STATUS_PASS,
                    detail=(
                        f"available={bool(t.get('available'))} how={t.get('how')} "
                        f"version={t.get('version', '')} path={t.get('path', '')}"
                    ),
                )
            )
        return checks, {}

    def _linux_cargo_argv(self, cargo_args: str) -> list[str] | None:
        cargo_ok = bool((self.env_info.get("tools") or {}).get("cargo", {}).get("available"))
        if os.name != "nt" and cargo_ok and self.env_info.get("linux_rust_available", True):
            return [
                "bash",
                "-c",
                f"cd '{self.root}' && export CARGO_TARGET_DIR=\"$PWD/target/qa-linux\" && cargo {cargo_args}",
            ]
        if self.env_info.get("wsl_available") and self.env_info.get("linux_rust_available"):
            unix = windows_to_wsl(self.root)
            return _wsl_script(
                self.root,
                f'export CARGO_TARGET_DIR="$HOME/ignite-qa-linux-target"\ncd {unix}\ncargo {cargo_args}\n',
            )
        return None

    def _rust(self) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        checks: list[dict[str, Any]] = []
        argv = self._linux_cargo_argv("test --workspace --locked")
        if argv is None:
            checks.append(
                check(
                    name="linux-cargo",
                    status=STATUS_NOT_RUN,
                    detail="no Linux cargo (WSL or native). Windows cargo is not Unix runtime qualification",
                )
            )
            return checks, {}
        ws = self._run(argv, self.root, timeout=3600, log_name="cargo-test-workspace")
        checks.append(
            check(
                name="cargo-test-workspace",
                status=STATUS_PASS if ws["exit_code"] == 0 else STATUS_FAIL,
                detail="cargo test --workspace --locked (Linux)",
                extra={"run": _run_fields(ws), "stdout_tail": ws["stdout_tail"][-1500:]},
            )
        )
        lib_argv = self._linux_cargo_argv("test -p scribe-app --lib --locked")
        assert lib_argv is not None
        lib = self._run(lib_argv, self.root, timeout=1800, log_name="cargo-test-scribe-app-lib")
        checks.append(
            check(
                name="cargo-test-scribe-app-lib",
                status=STATUS_PASS if lib["exit_code"] == 0 else STATUS_FAIL,
                detail="cargo test -p scribe-app --lib --locked (includes Unix-gated runtime)",
                extra={"run": _run_fields(lib), "stdout_tail": lib["stdout_tail"][-1500:]},
            )
        )
        return checks, {}

    def _armv7(self) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        checks: list[dict[str, Any]] = []
        docker = (self.env_info.get("tools") or {}).get("docker") or {}
        image = read_pinned_rust_image(self.root)
        if not docker.get("available"):
            checks.append(check(name="docker", status=STATUS_NOT_RUN, detail="docker not available"))
            return checks, {}
        if not image or "@sha256:" not in image:
            checks.append(check(name="pinned-image", status=STATUS_FAIL, detail="rust_armv7hf pin missing"))
            return checks, {}
        mount = windows_to_docker(self.root) if os.name == "nt" else str(self.root)
        argv = [
            "docker",
            "run",
            "--rm",
            "-v",
            f"{mount}:/src",
            "-w",
            "/src",
            image,
            "bash",
            "/src/tools/docker_build_armv7hf.sh",
        ]
        built = self._run(argv, self.root, timeout=3600, log_name="docker-build-armv7hf")
        if built["exit_code"] != 0:
            checks.append(
                check(
                    name="docker_build_armv7hf",
                    status=STATUS_FAIL,
                    detail="tools/docker_build_armv7hf.sh",
                    extra={"run": _run_fields(built), "stderr_tail": built["stderr_tail"]},
                )
            )
            return checks, {}
        checks.append(
            check(
                name="docker_build_armv7hf",
                status=STATUS_PASS,
                detail=f"image={image}",
                extra={"run": _run_fields(built)},
            )
        )
        artifacts: dict[str, Any] = {}
        triple = self.root / "target" / "armv7-unknown-linux-gnueabihf" / "release"
        for name in ARM_BINS:
            path = triple / name
            if not path.is_file():
                checks.append(check(name=f"elf-{name}", status=STATUS_FAIL, detail=f"missing {path}"))
                continue
            try:
                info = inspect_armv7hf_elf(path)
            except ElfError as exc:
                checks.append(check(name=f"elf-{name}", status=STATUS_FAIL, detail=str(exc)))
                continue
            digest = _sha256(path)
            artifacts[name] = {"path": str(path), "sha256": digest, **info}
            checks.append(
                check(
                    name=f"elf-{name}",
                    status=STATUS_PASS,
                    detail=f"sha256={digest} bytes={info['bytes']} interp={info['interpreter']}",
                )
            )
        return checks, {"artifacts": artifacts}

    def _wasm(self) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        checks: list[dict[str, Any]] = []
        node = (self.env_info.get("tools") or {}).get("node") or {}
        if not node.get("available"):
            checks.append(check(name="node", status=STATUS_NOT_RUN, detail="node not available"))
            return checks, {}
        wasm32 = bool(self.env_info.get("wasm32_available"))
        if not wasm32:
            checks.append(
                check(
                    name="wasm32-target",
                    status=STATUS_NOT_RUN,
                    detail="wasm32-unknown-unknown not installed (will not rustup target add)",
                )
            )
            return checks, {}
        builder = self.web / "scripts" / "build-wasm.mjs"
        env = None
        argv: list[str]
        cargo_tool = (self.env_info.get("tools") or {}).get("cargo") or {}
        if os.name == "nt" and self.env_info.get("wsl_available") and not cargo_tool.get("native_wasm32"):
            unix = windows_to_wsl(self.root)
            argv = _wsl_script(
                self.root,
                f'export CARGO_TARGET_DIR="$HOME/ignite-qa-wasm-target"\ncd {unix}/ignite/web\nnode scripts/build-wasm.mjs\n',
            )
        else:
            argv = ["node", str(builder)]
        result = self._run(argv, self.web, timeout=1800, env=env, log_name="build-wasm")
        wasm_path = self.web / "src" / "wasm" / "scribe_web_wasm.wasm"
        if result["exit_code"] != 0 or not wasm_path.is_file():
            checks.append(
                check(
                    name="build-wasm",
                    status=STATUS_FAIL,
                    detail="ignite/web/scripts/build-wasm.mjs",
                    extra={"run": _run_fields(result), "stderr_tail": result["stderr_tail"]},
                )
            )
            return checks, {}
        digest = _sha256(wasm_path)
        checks.append(
            check(
                name="build-wasm",
                status=STATUS_PASS,
                detail=f"src/wasm/scribe_web_wasm.wasm sha256={digest} bytes={wasm_path.stat().st_size}",
                extra={"run": _run_fields(result)},
            )
        )
        return checks, {"wasm": {"sha256": digest, "bytes": wasm_path.stat().st_size}}

    def _web(self) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        checks: list[dict[str, Any]] = []
        npm = (self.env_info.get("tools") or {}).get("npm") or {}
        npx = (self.env_info.get("tools") or {}).get("npx") or {}
        if not npm.get("available") or not npx.get("available"):
            checks.append(check(name="npm", status=STATUS_NOT_RUN, detail="npm/npx not available"))
            return checks, {}
        if not (self.web / "node_modules").is_dir():
            checks.append(
                check(
                    name="node_modules",
                    status=STATUS_NOT_RUN,
                    detail="ignite/web/node_modules missing (will not npm ci)",
                )
            )
            return checks, {}
        tsc = self._run(
            ["npx", "tsc", "--noEmit", "-p", "tsconfig.json"],
            self.web,
            timeout=180,
            log_name="tsc",
        )
        checks.append(
            check(
                name="tsc",
                status=STATUS_PASS if tsc["exit_code"] == 0 else STATUS_FAIL,
                detail="npx tsc --noEmit",
                extra={"run": _run_fields(tsc)},
            )
        )
        test = self._run(["npm", "test"], self.web, timeout=300, log_name="vitest")
        checks.append(
            check(
                name="vitest",
                status=STATUS_PASS if test["exit_code"] == 0 else STATUS_FAIL,
                detail="npm test",
                extra={"run": _run_fields(test)},
            )
        )
        vite = self._run(["npx", "vite", "build"], self.web, timeout=180, log_name="vite-build")
        checks.append(
            check(
                name="vite-build",
                status=STATUS_PASS if vite["exit_code"] == 0 else STATUS_FAIL,
                detail="npx vite build (does not invoke Cargo)",
                extra={"run": _run_fields(vite)},
            )
        )
        dist = self.web / "dist"
        src_wasm = self.web / "src" / "wasm" / "scribe_web_wasm.wasm"
        if vite["exit_code"] == 0 and src_wasm.is_file():
            src_hash = _sha256(src_wasm)
            matched = False
            for asset in dist.glob("assets/*.wasm"):
                if _sha256(asset) == src_hash:
                    matched = True
                    checks.append(
                        check(
                            name="dist-wasm",
                            status=STATUS_PASS,
                            detail=f"{asset.name} sha256={src_hash}",
                        )
                    )
                    break
            if not matched:
                checks.append(check(name="dist-wasm", status=STATUS_FAIL, detail="dist WASM hash != src/wasm"))
        return checks, {}

    def _e2e(self) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        checks: list[dict[str, Any]] = []
        npx = (self.env_info.get("tools") or {}).get("npx") or {}
        dist = self.web / "dist" / "index.html"
        if not npx.get("available"):
            checks.append(check(name="npx", status=STATUS_NOT_RUN, detail="npx not available"))
            return checks, {}
        if not dist.is_file():
            checks.append(
                check(
                    name="web-dist",
                    status=STATUS_NOT_RUN,
                    detail="ignite/web/dist missing; run the web group first",
                )
            )
            return checks, {}
        result = self._run(
            ["npx", "playwright", "test", "--project=chromium"],
            self.web,
            timeout=1800,
            log_name="playwright-chromium",
            env={"CARGO_BUILD_JOBS": "2"},
        )
        text = (result["stdout_tail"] + result["stderr_tail"]).lower()
        log_path = result.get("log")
        if log_path:
            try:
                text += Path(log_path).read_text(encoding="utf-8", errors="replace")[-8000:].lower()
            except OSError:
                pass
        if result["exit_code"] != 0 and (
            "executable doesn't exist" in text or "browser" in text and "install" in text
        ):
            checks.append(
                check(
                    name="playwright-chromium",
                    status=STATUS_NOT_RUN,
                    detail="Chromium not installed (will not playwright install)",
                    extra={"run": _run_fields(result)},
                )
            )
            return checks, {}
        checks.append(
            check(
                name="playwright-chromium",
                status=STATUS_PASS if result["exit_code"] == 0 else STATUS_FAIL,
                detail="npx playwright test --project=chromium",
                extra={"run": _run_fields(result), "stdout_tail": result["stdout_tail"][-1500:]},
            )
        )
        return checks, {}
