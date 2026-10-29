#!/usr/bin/env python3
"""Run ``cargo test`` with a complete log and a concise terminal summary.

Example:

    python3 tools/ci/run_cargo_test_logged.py \
      --log target/test-logs/scribe-app.log -- -p scribe-app --lib --locked

Everything Cargo writes to stdout or stderr is stored in the log.  The terminal
shows suite summaries and failure indicators, followed by a bounded tail on
failure.  This wrapper exits with Cargo's exit status; it never turns a failed
test command into a successful result.
"""

from __future__ import annotations

import argparse
import os
import shlex
import subprocess
import sys
from collections import deque
from pathlib import Path
from typing import TextIO


FAILURE_TAIL_LINES = 80


def _terminal_line(line: str) -> bool:
    stripped = line.strip()
    return (
        stripped.startswith("running ")
        or stripped.startswith("test result:")
        or stripped.startswith("failures:")
        or stripped.startswith("error:")
        or stripped.startswith("Caused by:")
        or " panicked at " in stripped
        or stripped.endswith(" ... FAILED")
    )


def _display_command(command: list[str]) -> str:
    if os.name == "nt":
        return subprocess.list2cmdline(command)
    return shlex.join(command)


def run_logged(command: list[str], log_path: Path, terminal: TextIO) -> int:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    tail: deque[str] = deque(maxlen=FAILURE_TAIL_LINES)
    shown: set[str] = set()

    with log_path.open("w", encoding="utf-8", newline="\n") as log:
        log.write(f"command={_display_command(command)}\n")
        log.flush()
        try:
            process = subprocess.Popen(
                command,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                encoding="utf-8",
                errors="replace",
                bufsize=1,
            )
        except OSError as error:
            message = f"could not start Cargo: {error}"
            log.write(f"{message}\n")
            print(message, file=terminal)
            print(f"cargo_test status=failed exit_code=127 log={log_path}", file=terminal)
            return 127

        assert process.stdout is not None
        try:
            for raw_line in process.stdout:
                log.write(raw_line)
                tail.append(raw_line.rstrip("\r\n"))
                if _terminal_line(raw_line):
                    rendered = raw_line.rstrip("\r\n")
                    print(rendered, file=terminal)
                    shown.add(rendered)
            return_code = process.wait()
        except KeyboardInterrupt:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            return_code = 130
        finally:
            process.stdout.close()

    if return_code < 0:
        return_code = 128 + (-return_code)

    if return_code == 0:
        print(f"cargo_test status=passed exit_code=0 log={log_path}", file=terminal)
        return 0

    print(f"--- cargo failure tail (complete output: {log_path}) ---", file=terminal)
    for line in tail:
        if line not in shown:
            print(line, file=terminal)
    print(
        f"cargo_test status=failed exit_code={return_code} log={log_path}",
        file=terminal,
    )
    return return_code


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--log", required=True, type=Path, help="complete output log path")
    parser.add_argument(
        "--cargo",
        default=os.environ.get("CARGO", "cargo"),
        help="Cargo executable (default: CARGO or cargo)",
    )
    parser.add_argument(
        "cargo_args",
        nargs=argparse.REMAINDER,
        help="arguments passed after `cargo test` (a leading -- is optional)",
    )
    args = parser.parse_args(argv)
    if args.cargo_args[:1] == ["--"]:
        args.cargo_args = args.cargo_args[1:]
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    command = [args.cargo, "test", *args.cargo_args]
    return run_logged(command, args.log, sys.stdout)


if __name__ == "__main__":
    sys.exit(main())
