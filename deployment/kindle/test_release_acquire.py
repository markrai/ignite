"""CI artifact acquisition + release orchestration regressions (Slice 3o corrections)."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
_SFX = _HERE / "sfx"
_REPO = _HERE.parents[1]
_RELEASE = _REPO / "tools" / "release"
for extra in (_HERE, _SFX, str(_RELEASE)):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from authenticated_publication_evidence import generate_test_trust_root  # noqa: E402
from authenticated_publication_preflight import PreflightError, run_promote_gate  # noqa: E402
from build_sfx import build_sfx, default_template  # noqa: E402
from ci_packaging_measurement import (  # noqa: E402
    MEASUREMENT_BASENAME,
    PACKAGE_PROFILE_BASENAME,
    build_ci_packaging_measurement,
    write_packaging_measurements,
)
from offline_release_ceremony import (  # noqa: E402
    MAX_CHECKSUM_SIDECAR_BYTES,
    CeremonyError,
    _validate_checksum_sidecar,
    _write_bytes_exclusive,
)
from release_acquire import (  # noqa: E402
    MAX_RUN_SEARCH_PAGES,
    AcquireError,
    acquire_release,
    extracted_tree_sha256,
    load_acquired_bundle,
)
from release_component_profiles import generate_profiles  # noqa: E402
from release_identity import (  # noqa: E402
    EXPECTED_REPOSITORY,
    EXPECTED_WORKFLOW,
    KINDLE_ARTIFACT,
    LINUX_ARTIFACT,
    MEASUREMENTS_ARTIFACT,
    parse_acquisition_receipt,
)
from release_orchestrate import (  # noqa: E402
    DryRunOutcome,
    OrchestrateError,
    classify_promote_gate_error,
    classify_promote_shell_output,
    dry_run_release,
    sign_acquired,
    stage_nine_file_candidate,
)
from test_validate_stage import make_payload  # noqa: E402
from verify_sfx_payload_equivalence import verify_equivalence  # noqa: E402

VERSION = "3.3.2"
TAG = f"v{VERSION}"
SOURCE_REV = "a" * 40
RUN_ID = 424242
OLDER_RUN_ID = 100001
WORKFLOW_ID = 55


def _sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _public_key_hex(root, path: Path) -> Path:
    path.write_bytes(root.public_key.hex().encode("ascii") + b"\n")
    return path


def _build_kindle_pair(tmp: Path) -> tuple[Path, Path, Path, Path]:
    payload = make_payload(tmp / "src", VERSION)
    app = payload / "bin" / "scribe-app"
    app.write_bytes(app.read_bytes() + b"\n#slice-3o\n")
    kindle = tmp / "kindle"
    kindle.mkdir()
    scriptlet = kindle / f"Install Ignite v{VERSION}.sh"
    scriptlet.write_bytes(build_sfx(payload, VERSION, default_template()))
    profiles = generate_profiles(payload, VERSION)
    package_profile = profiles["package"]
    ok, report = verify_equivalence(
        sfx_path=scriptlet,
        payload_path=payload,
        version=VERSION,
        package_profile=package_profile,
    )
    assert ok, report.decode("ascii", errors="replace")
    measurement = build_ci_packaging_measurement(
        version=VERSION,
        scriptlet=scriptlet,
        payload=payload,
        package_profile=package_profile,
        source_revision=SOURCE_REV,
        workflow_run_id=str(RUN_ID),
    )
    meas_dir = tmp / "meas"
    write_packaging_measurements(
        out_dir=meas_dir,
        measurement=measurement,
        package_profile=package_profile,
        equivalence_report=report,
    )
    digest = _sha(scriptlet.read_bytes())
    checksum = kindle / f"Install Ignite v{VERSION}.sha256"
    checksum.write_bytes(f"{digest}  Install Ignite v{VERSION}.sh\n".encode("ascii"))
    association = meas_dir / "artifact-association.v1"
    association.write_bytes(
        (
            "IGNITE_CI_KINDLE_ARTIFACT_ASSOCIATION_V1\n"
            f"customer_artifact={KINDLE_ARTIFACT}\n"
            f"measurement_artifact={MEASUREMENTS_ARTIFACT}\n"
            "github_release_asset=false\n"
            f"version={VERSION}\n"
            f"source_revision={SOURCE_REV}\n"
            f"workflow_run_id={RUN_ID}\n"
            f"measurement_sha256={_sha((meas_dir / MEASUREMENT_BASENAME).read_bytes())}\n"
        ).encode("ascii")
    )
    return (
        scriptlet,
        checksum,
        meas_dir / MEASUREMENT_BASENAME,
        meas_dir / PACKAGE_PROFILE_BASENAME,
    )


def _linux_pair(tmp: Path) -> Path:
    """Build an unpackable linux release tarball (promote verify needs real layout)."""
    import tarfile

    bundle_name = f"ignite-server-{VERSION}-linux-x86_64.tar.gz"
    top = f"ignite-server-{VERSION}-linux-x86_64"
    stage = tmp / "linux-stage"
    root = stage / top
    root.mkdir(parents=True, exist_ok=True)
    (root / ".ignite-bundle").write_text(
        f"version={VERSION}\narch=x86_64\n", encoding="ascii"
    )
    server = root / "scribe-server"
    # Accept both bare and `--version` invocations used by promote-release.sh.
    server.write_text(
        "#!/bin/sh\n"
        'if [ "${1:-}" = "--version" ] || [ "${1:-}" = "-V" ]; then\n'
        f'  printf "%s\\n" "{VERSION}"\n'
        "  exit 0\n"
        "fi\n"
        f'printf "%s\\n" "{VERSION}"\n',
        encoding="ascii",
    )
    server.chmod(0o755)
    out = tmp / "linux-out"
    out.mkdir(parents=True, exist_ok=True)
    gz_path = out / bundle_name
    with tarfile.open(gz_path, "w:gz") as tf:
        tf.add(root, arcname=top)
    digest = _sha(gz_path.read_bytes())
    (out / f"{bundle_name}.sha256").write_bytes(
        f"{digest}  {bundle_name}\n".encode("ascii")
    )
    return out


class MockGh:
    def __init__(self, state: Path, *, multi_page_artifacts: bool = False):
        self.state = state
        self.calls: list[list[str]] = []
        self.multi_page_artifacts = multi_page_artifacts
        self.stale_after_download = False
        self.download_count = 0

    def __call__(self, argv: list[str] | tuple[str, ...]):
        args = list(argv)
        self.calls.append(args)
        if args[:2] == ["api", f"repos/{EXPECTED_REPOSITORY}/actions/workflows/ci.yml"]:
            return self._ok(json.dumps({"id": WORKFLOW_ID}))
        if (
            len(args) >= 2
            and args[0] == "api"
            and "/actions/runs/" in args[1]
            and "/artifacts" not in args[1]
            and "workflows" not in args[1]
        ):
            return self._ok((self.state / "run.json").read_text(encoding="utf-8"))
        if len(args) >= 2 and args[0] == "api" and "/artifacts" in args[1]:
            arts = json.loads((self.state / "artifacts.json").read_text(encoding="utf-8"))
            if self.stale_after_download and self.download_count > 0:
                arts["artifacts"][0]["id"] = 999999
            if "--slurp" in args:
                if self.multi_page_artifacts:
                    # Split into two pages.
                    a = arts["artifacts"]
                    pages = [
                        {"artifacts": a[:2], "total_count": 3},
                        {"artifacts": a[2:], "total_count": 3},
                    ]
                    return self._ok(json.dumps(pages))
                return self._ok(json.dumps([arts]))
            return self._ok(json.dumps(arts))
        if len(args) >= 2 and args[0] == "api" and "contents/ignite/VERSION" in args[1]:
            return self._ok(VERSION + "\n")
        if len(args) >= 2 and args[0] == "api" and "actions/workflows/ci.yml/runs" in args[1]:
            # Support ?page=N
            page = 1
            if "page=" in args[1]:
                page = int(args[1].split("page=")[1].split("&")[0])
            runs_doc = json.loads((self.state / "runs.json").read_text(encoding="utf-8"))
            all_runs = runs_doc["workflow_runs"]
            # Synthetic pagination: page 1 has first run, page 2 has older eligible.
            if page == 1:
                return self._ok(json.dumps({"workflow_runs": all_runs[:1]}))
            if page == 2 and len(all_runs) > 1:
                return self._ok(json.dumps({"workflow_runs": all_runs[1:]}))
            return self._ok(json.dumps({"workflow_runs": []}))
        if args[:2] == ["run", "download"]:
            self.download_count += 1
            name = args[args.index("-n") + 1]
            dest = Path(args[args.index("-D") + 1])
            src = self.state / "arts" / name
            if not src.is_dir():
                return self._fail(f"missing mock artifact {name}")
            dest.mkdir(parents=True, exist_ok=True)
            for path in src.rglob("*"):
                if path.is_file():
                    target = dest / path.relative_to(src)
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(path, target)
            return self._ok("")
        return self._fail(f"unhandled gh argv: {args}")

    @staticmethod
    def _ok(stdout: str):
        return subprocess.CompletedProcess(args=[], returncode=0, stdout=stdout, stderr="")

    @staticmethod
    def _fail(msg: str):
        return subprocess.CompletedProcess(args=[], returncode=1, stdout="", stderr=msg)


def _seed_mock_state(tmp: Path, *, include_older: bool = False) -> Path:
    state = tmp / "gh-state"
    state.mkdir()
    scriptlet, checksum, measurement, profile = _build_kindle_pair(tmp / "build")
    linux = _linux_pair(tmp / "build-linux")
    arts = state / "arts"
    (arts / KINDLE_ARTIFACT).mkdir(parents=True)
    shutil.copy2(scriptlet, arts / KINDLE_ARTIFACT / scriptlet.name)
    shutil.copy2(checksum, arts / KINDLE_ARTIFACT / checksum.name)
    (arts / MEASUREMENTS_ARTIFACT).mkdir(parents=True)
    shutil.copy2(measurement, arts / MEASUREMENTS_ARTIFACT / measurement.name)
    shutil.copy2(profile, arts / MEASUREMENTS_ARTIFACT / profile.name)
    assoc = measurement.parent / "artifact-association.v1"
    if assoc.is_file():
        shutil.copy2(assoc, arts / MEASUREMENTS_ARTIFACT / assoc.name)
    eq = measurement.parent / "sfx-payload-equivalence.v1"
    if eq.is_file():
        shutil.copy2(eq, arts / MEASUREMENTS_ARTIFACT / eq.name)
    (arts / LINUX_ARTIFACT).mkdir(parents=True)
    for path in linux.iterdir():
        shutil.copy2(path, arts / LINUX_ARTIFACT / path.name)

    run = {
        "id": RUN_ID,
        "run_attempt": 1,
        "status": "completed",
        "conclusion": "success",
        "head_sha": SOURCE_REV,
        "head_branch": "main",
        "event": "push",
        "path": EXPECTED_WORKFLOW,
        "workflow_id": WORKFLOW_ID,
        "repository": {"full_name": EXPECTED_REPOSITORY},
    }
    (state / "run.json").write_text(json.dumps(run), encoding="utf-8")
    artifacts = {
        "artifacts": [
            {"id": 101, "name": KINDLE_ARTIFACT, "expired": False, "size_in_bytes": 10},
            {
                "id": 102,
                "name": MEASUREMENTS_ARTIFACT,
                "expired": False,
                "size_in_bytes": 10,
            },
            {"id": 103, "name": LINUX_ARTIFACT, "expired": False, "size_in_bytes": 10},
        ]
    }
    (state / "artifacts.json").write_text(json.dumps(artifacts), encoding="utf-8")
    runs = [run]
    if include_older:
        older = dict(run)
        older["id"] = OLDER_RUN_ID
        runs.append(older)
    (state / "runs.json").write_text(
        json.dumps({"workflow_runs": runs}), encoding="utf-8"
    )
    # Promote-release.sh resolve uses --jq TSV forms; keep parallel fixtures.
    (state / "workflow.id").write_text(f"{WORKFLOW_ID}\n", encoding="ascii")
    (state / "run.tsv").write_text(
        f"success\tcompleted\t{SOURCE_REV}\tpush\tmain\t"
        f"{EXPECTED_WORKFLOW}\t{WORKFLOW_ID}\n",
        encoding="ascii",
    )
    (state / "compare.ahead").write_text("0\n", encoding="ascii")
    (state / "version.txt").write_text(f"{VERSION}\n", encoding="ascii")
    (state / "artifacts.tsv").write_text(
        "ignite-kindle-release\tfalse\n"
        "ignite-server-linux-release\tfalse\n",
        encoding="ascii",
    )
    return state


def _write_mock_gh_exe(state: Path, dest: Path) -> Path:
    """Write a real executable mock gh driven by state/ for CLI + shell tests.

    Handles both acquisition JSON forms and promote-release.sh ``--jq`` TSV forms.
    Also installs PATH-visible ``gh`` and ``python3`` wrappers for bash.
    """
    dest.mkdir(parents=True, exist_ok=True)

    def _bash_path(path: Path) -> str:
        """Git Bash-friendly absolute path (forward slashes)."""
        return os.fspath(path.resolve()).replace("\\", "/")

    py_exe = _bash_path(Path(sys.executable))
    # Bash promote-release.sh calls ``python3``; map it to this interpreter.
    py3 = dest / "python3"
    py3.write_text(
        f"#!/bin/sh\nexec '{py_exe}' \"$@\"\n",
        encoding="utf-8",
        newline="\n",
    )
    py3.chmod(0o755)
    script = dest / "mock-gh.py"
    state_bash = _bash_path(state)
    script.write_text(
        textwrap.dedent(
            f"""\
            import json, shutil, sys
            from pathlib import Path
            STATE = Path(r"{state}")
            EXPECTED_REPOSITORY = "{EXPECTED_REPOSITORY}"
            WORKFLOW_ID = {WORKFLOW_ID}
            VERSION = "{VERSION}"
            SOURCE_REV = "{SOURCE_REV}"
            EXPECTED_WORKFLOW = "{EXPECTED_WORKFLOW}"
            args = sys.argv[1:]
            joined = " ".join(args)
            def ok(s):
                sys.stdout.write(s)
                return 0
            def fail(m):
                sys.stderr.write(m + "\\n")
                return 1
            if (STATE / "resolve.fail").is_file():
                if "actions/runs/" in joined and "/artifacts" not in joined:
                    raise SystemExit(fail((STATE / "resolve.fail").read_text(encoding="utf-8")))
            if (STATE / "fetch.fail").is_file() and "run download" in joined:
                raise SystemExit(fail((STATE / "fetch.fail").read_text(encoding="utf-8")))
            if (STATE / "unrelated.unprovisioned.fail").is_file():
                if "actions/runs/" in joined and "/artifacts" not in joined:
                    raise SystemExit(fail(
                        "mock gh refused: unrelated failure mentioning unprovisioned pin"
                    ))
            # Promote-style --jq responses (TSV / scalars).
            if "--jq" in args:
                if "actions/workflows/ci.yml" in joined or (
                    "actions/workflows/" in joined and "ci.yml" in joined
                ):
                    raise SystemExit(ok((STATE / "workflow.id").read_text(encoding="utf-8")))
                if "/compare/" in joined:
                    raise SystemExit(ok((STATE / "compare.ahead").read_text(encoding="utf-8")))
                if "/artifacts" in joined:
                    raise SystemExit(ok((STATE / "artifacts.tsv").read_text(encoding="utf-8")))
                if "actions/runs/" in joined:
                    raise SystemExit(ok((STATE / "run.tsv").read_text(encoding="utf-8")))
            if args[:2] == ["api", f"repos/{{EXPECTED_REPOSITORY}}/actions/workflows/ci.yml"]:
                raise SystemExit(ok(json.dumps({{"id": WORKFLOW_ID}})))
            if args and args[0] == "api" and any("/artifacts" in a for a in args):
                arts = json.loads((STATE / "artifacts.json").read_text(encoding="utf-8"))
                if "--slurp" in args:
                    raise SystemExit(ok(json.dumps([arts])))
                raise SystemExit(ok(json.dumps(arts)))
            if (
                args
                and args[0] == "api"
                and any("/actions/runs/" in a for a in args)
                and not any("/artifacts" in a for a in args)
                and "--jq" not in args
            ):
                raise SystemExit(ok((STATE / "run.json").read_text(encoding="utf-8")))
            if args and args[0] == "api" and any(
                "contents/ignite/VERSION" in a for a in args
            ):
                raise SystemExit(ok(VERSION + "\\n"))
            if args and args[0] == "api" and any(
                "actions/workflows/ci.yml/runs" in a for a in args
            ):
                raise SystemExit(ok((STATE / "runs.json").read_text(encoding="utf-8")))
            if args[:2] == ["run", "download"] or (
                len(args) >= 2 and args[0] == "run" and args[1] == "download"
            ):
                name = args[args.index("-n") + 1]
                dest = Path(args[args.index("-D") + 1])
                src = STATE / "arts" / name
                dest.mkdir(parents=True, exist_ok=True)
                for path in src.rglob("*"):
                    if path.is_file():
                        target = dest / path.relative_to(src)
                        target.parent.mkdir(parents=True, exist_ok=True)
                        shutil.copy2(path, target)
                raise SystemExit(0)
            raise SystemExit(fail("unhandled: " + " ".join(args)))
            """
        ),
        encoding="utf-8",
        newline="\n",
    )
    script_bash = _bash_path(script)
    # PATH name for bash promote-release.sh
    gh_sh = dest / "gh"
    gh_sh.write_text(
        f"#!/bin/sh\nexec '{py_exe}' '{script_bash}' \"$@\"\n",
        encoding="utf-8",
        newline="\n",
    )
    gh_sh.chmod(0o755)
    # Debug breadcrumb for harness path wiring.
    (dest / ".mock-state-path").write_text(state_bash + "\n", encoding="ascii")
    # Windows cmd wrapper for IGNITE_RELEASE_GH / Python subprocess
    wrapper = dest / ("mock-gh.cmd" if sys.platform == "win32" else "mock-gh")
    if sys.platform == "win32":
        wrapper.write_text(
            f'@echo off\r\n"{sys.executable}" "{script}" %*\r\n',
            encoding="ascii",
        )
    else:
        wrapper.write_text(
            f"#!/bin/sh\nexec '{sys.executable}' '{script}' \"$@\"\n",
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
    return wrapper


def test_write_bytes_exclusive_handles_short_writes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = tmp_path / "out.bin"
    payload = b"abcdefghijklmnopqrstuvwxyz"
    original = os.write
    chunks = [3, 5, 0]

    def flaky_write(fd: int, data: bytes) -> int:
        if chunks:
            n = chunks.pop(0)
            if n == 0:
                return 0
            return original(fd, data[:n])
        return original(fd, data)

    monkeypatch.setattr(os, "write", flaky_write)
    with pytest.raises(CeremonyError, match="short write"):
        _write_bytes_exclusive(path, payload)
    assert not path.exists()


def test_checksum_sidecar_max_bytes_enforced(tmp_path: Path) -> None:
    artifacts = tmp_path / "arts"
    artifacts.mkdir()
    scriptlet = artifacts / f"Install Ignite v{VERSION}.sh"
    scriptlet.write_bytes(b"#!/bin/sh\necho ok\n")
    sidecar = artifacts / f"Install Ignite v{VERSION}.sha256"
    digest = _sha(scriptlet.read_bytes())
    padding = "x" * MAX_CHECKSUM_SIDECAR_BYTES
    sidecar.write_bytes(
        f"{digest}  Install Ignite v{VERSION}.sh{padding}\n".encode("ascii")
    )
    with pytest.raises(CeremonyError, match="checksum sidecar exceeds"):
        _validate_checksum_sidecar(
            root=artifacts, scriptlet=scriptlet, version=VERSION
        )


def test_acquire_mocked_success(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)
    gh = MockGh(state)
    out = tmp_path / "acquired"
    bundle = acquire_release(tag=TAG, output_dir=out, run_id=RUN_ID, gh=gh)
    identity = parse_acquisition_receipt(bundle.receipt_path.read_bytes())
    assert identity.ci_run_id == RUN_ID
    assert identity.ci_head_sha == SOURCE_REV
    scriptlet = next(bundle.ci_artifacts.rglob(f"Install Ignite v{VERSION}.sh"))
    assert identity.scriptlet_sha256 == _sha(scriptlet.read_bytes())
    assert "extracted-tree-sha256-not-github-archive-digest" in (
        bundle.receipt_path.read_text(encoding="ascii")
    )
    reloaded = load_acquired_bundle(out)
    assert reloaded.identity.scriptlet_sha256 == identity.scriptlet_sha256


def test_acquire_multi_page_artifacts(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)
    gh = MockGh(state, multi_page_artifacts=True)
    bundle = acquire_release(
        tag=TAG, output_dir=tmp_path / "acquired", run_id=RUN_ID, gh=gh
    )
    assert bundle.identity.kindle_artifact.artifact_id == 101


def test_acquire_selects_highest_among_eligible(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path, include_older=True)
    # Both eligible runs on one page; policy must pick the highest run id.
    runs = json.loads((state / "runs.json").read_text(encoding="utf-8"))
    assert {r["id"] for r in runs["workflow_runs"]} == {RUN_ID, OLDER_RUN_ID}

    def fetch_aware(argv):
        args = list(argv)
        if (
            args
            and args[0] == "api"
            and "/actions/runs/" in args[1]
            and "/artifacts" not in args[1]
            and "workflows" not in args[1]
            and "workflows/ci.yml/runs" not in args[1]
        ):
            rid = int(args[1].rstrip("/").split("/")[-1])
            doc = json.loads((state / "run.json").read_text(encoding="utf-8"))
            doc["id"] = rid
            return MockGh._ok(json.dumps(doc))
        # Single page containing both runs.
        if args and args[0] == "api" and "actions/workflows/ci.yml/runs" in args[1]:
            return MockGh._ok(json.dumps(runs))
        return MockGh(state)(argv)

    bundle = acquire_release(
        tag=TAG, output_dir=tmp_path / "acquired", run_id=None, gh=fetch_aware
    )
    assert bundle.identity.ci_run_id == RUN_ID
    assert bundle.identity.selection_policy == "highest-eligible-run-id"


def test_acquire_refuses_malformed_artifact_page(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)

    def bad_gh(argv):
        args = list(argv)
        if args and args[0] == "api" and "/artifacts" in args[1]:
            return MockGh._ok(json.dumps(["not-an-object"]))
        return MockGh(state)(argv)

    with pytest.raises(AcquireError, match="malformed|page"):
        acquire_release(tag=TAG, output_dir=tmp_path / "out", run_id=RUN_ID, gh=bad_gh)


def test_acquire_refuses_stale_artifact_id_after_download(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)
    gh = MockGh(state)
    gh.stale_after_download = True
    with pytest.raises(AcquireError, match="artifact id changed"):
        acquire_release(tag=TAG, output_dir=tmp_path / "out", run_id=RUN_ID, gh=gh)


def test_acquire_refuses_wrong_repository(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)
    with pytest.raises(AcquireError, match="repository"):
        acquire_release(
            tag=TAG,
            output_dir=tmp_path / "out",
            run_id=RUN_ID,
            repository="evil/ignite",
            gh=MockGh(state),
        )


def test_acquire_refuses_pr_event(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)
    run = json.loads((state / "run.json").read_text(encoding="utf-8"))
    run["event"] = "pull_request"
    (state / "run.json").write_text(json.dumps(run), encoding="utf-8")
    with pytest.raises(AcquireError, match="pull_request"):
        acquire_release(tag=TAG, output_dir=tmp_path / "out", run_id=RUN_ID, gh=MockGh(state))


def test_acquire_refuses_expired_artifact(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)
    arts = json.loads((state / "artifacts.json").read_text(encoding="utf-8"))
    arts["artifacts"][0]["expired"] = True
    (state / "artifacts.json").write_text(json.dumps(arts), encoding="utf-8")
    with pytest.raises(AcquireError, match="expired"):
        acquire_release(tag=TAG, output_dir=tmp_path / "out", run_id=RUN_ID, gh=MockGh(state))


def test_load_refuses_mutated_scriptlet(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)
    bundle = acquire_release(
        tag=TAG, output_dir=tmp_path / "acquired", run_id=RUN_ID, gh=MockGh(state)
    )
    scriptlet = next(bundle.ci_artifacts.rglob(f"Install Ignite v{VERSION}.sh"))
    scriptlet.write_bytes(scriptlet.read_bytes() + b"\n#mut\n")
    with pytest.raises(AcquireError, match="scriptlet (size|digest) drifted"):
        load_acquired_bundle(bundle.root)


def test_load_refuses_mutated_measurement(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)
    bundle = acquire_release(
        tag=TAG, output_dir=tmp_path / "acquired", run_id=RUN_ID, gh=MockGh(state)
    )
    meas = next(bundle.ci_artifacts.rglob(MEASUREMENT_BASENAME))
    meas.write_bytes(meas.read_bytes() + b"\n")
    with pytest.raises(AcquireError, match="measurement"):
        load_acquired_bundle(bundle.root)


def test_load_refuses_mutated_linux(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)
    bundle = acquire_release(
        tag=TAG, output_dir=tmp_path / "acquired", run_id=RUN_ID, gh=MockGh(state)
    )
    linux = bundle.linux_dir / f"ignite-server-{VERSION}-linux-x86_64.tar.gz"
    linux.write_bytes(linux.read_bytes() + b"\x00")
    with pytest.raises(AcquireError, match="Linux"):
        load_acquired_bundle(bundle.root)


def test_load_refuses_mutated_checksum_sidecar(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)
    bundle = acquire_release(
        tag=TAG, output_dir=tmp_path / "acquired", run_id=RUN_ID, gh=MockGh(state)
    )
    side = next(bundle.ci_artifacts.rglob(f"Install Ignite v{VERSION}.sha256"))
    side.write_bytes(f"{'ab' * 32}  Install Ignite v{VERSION}.sh\n".encode("ascii"))
    with pytest.raises(AcquireError, match="checksum"):
        load_acquired_bundle(bundle.root)


def test_load_refuses_identity_receipt_mismatch(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)
    bundle = acquire_release(
        tag=TAG, output_dir=tmp_path / "acquired", run_id=RUN_ID, gh=MockGh(state)
    )
    # Corrupt identity file only.
    text = bundle.identity_path.read_text(encoding="ascii")
    bundle.identity_path.write_text(
        text.replace(f"ci_run_id={RUN_ID}", f"ci_run_id={RUN_ID + 1}"),
        encoding="ascii",
    )
    with pytest.raises(AcquireError, match="release-identity|match"):
        load_acquired_bundle(bundle.root)


def test_corrupted_candidate_is_not_expected_pin_refusal(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)
    acquired = acquire_release(
        tag=TAG, output_dir=tmp_path / "acquired", run_id=RUN_ID, gh=MockGh(state)
    )
    root = generate_test_trust_root(tmp_path / "keys", label="corr-3o")
    pub = _public_key_hex(root, tmp_path / "pub.hex")
    signed = sign_acquired(
        acquired=acquired.root,
        private_key=root.private_key_path,
        public_key=pub,
        output_dir=tmp_path / "signed",
    )
    # Corrupt root binding signature so isolated crypto fails hard (not pin refusal).
    forged = (
        b"schema=ignite-kindle-publication-binding-signature-v1\n"
        b"signature=" + (b"00" * 64) + b"\n"
    )
    (signed.root / f"ignite-kindle-v{VERSION}.publication-binding.sig").write_bytes(
        forged
    )
    (
        signed.promote_assets / f"ignite-kindle-v{VERSION}.publication-binding.sig"
    ).write_bytes(forged)
    with pytest.raises(OrchestrateError, match="isolated cryptographic verification"):
        dry_run_release(
            tag=TAG,
            run_id=RUN_ID,
            signed_assets=signed.root,
            work_dir=tmp_path / "work",
            acquired_dir=acquired.root,
            public_key=pub,
            run_promote_shell=False,
        )


def test_classify_promote_gate_categories() -> None:
    assert (
        classify_promote_gate_error(PreflightError("pin is UNPROVISIONED; blocked"))
        == "official-pin-unprovisioned"
    )
    assert (
        classify_promote_gate_error(PreflightError("signature verification failed"))
        == "signature-or-verification-failure"
    )


def test_full_flow_isolated_and_unprovisioned_refusal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import authenticated_publication_preflight as ap
    import production_trust_boundary as ptb

    state = _seed_mock_state(tmp_path)
    gh = MockGh(state)
    acquired = acquire_release(
        tag=TAG, output_dir=tmp_path / "acquired", run_id=RUN_ID, gh=gh
    )
    root = generate_test_trust_root(tmp_path / "keys", label="slice-3o")
    pub = _public_key_hex(root, tmp_path / "pub.hex")
    signed = sign_acquired(
        acquired=acquired.root,
        private_key=root.private_key_path,
        public_key=pub,
        output_dir=tmp_path / "signed",
    )

    # Isolated pin success for nine-file gate under disposable identity.
    pin = tmp_path / "isolated-pin.hex"
    pin.write_bytes(root.public_key.hex().encode("ascii") + b"\n")
    monkeypatch.setattr(ap, "_PRODUCTION_PIN", pin)
    monkeypatch.setattr(ptb, "_PRODUCTION_PIN", pin)
    nine = stage_nine_file_candidate(
        acquired=acquired, signed=signed, dest=tmp_path / "nine"
    )
    gate = run_promote_gate(
        release_candidate_dir=nine,
        tag=TAG,
        source_revision=SOURCE_REV,
        expected_workflow=EXPECTED_WORKFLOW,
    )
    assert gate.production_promotion_ready is True
    monkeypatch.undo()

    report = dry_run_release(
        tag=TAG,
        run_id=RUN_ID,
        signed_assets=signed.root,
        work_dir=tmp_path / "work",
        acquired_dir=acquired.root,
        public_key=pub,
        run_promote_shell=False,
    )
    assert report.isolated_crypto_ok is True
    assert report.production_gate_refused is True
    assert report.production_gate_refusal_category == "official-pin-unprovisioned"
    assert report.outcome == DryRunOutcome.PARTIAL
    assert report.exit_status == 2
    assert "dry_run_outcome=partial-verification" in "\n".join(report.lines)
    assert (
        _REPO / "tools" / "release" / "kindle-release-public-key.hex"
    ).read_bytes() == b"UNPROVISIONED\n"


def test_cli_acquire_via_ignite_release_gh(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    state = _seed_mock_state(tmp_path)
    mock_gh = _write_mock_gh_exe(state, tmp_path / "bin")
    monkeypatch.setenv("IGNITE_RELEASE_GH", str(mock_gh))
    script = _RELEASE / "ignite_release.py"
    acquired = tmp_path / "acquired"
    completed = subprocess.run(
        [
            sys.executable,
            os.fspath(script),
            "acquire",
            "--tag",
            TAG,
            "--run-id",
            str(RUN_ID),
            "--output",
            str(acquired),
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert "scriptlet_sha256=" in completed.stdout
    identity = parse_acquisition_receipt(
        (acquired / "acquisition.receipt.v1").read_bytes()
    )
    scriptlet = next(acquired.joinpath("ci-artifacts").rglob(f"Install Ignite v{VERSION}.sh"))
    assert identity.scriptlet_sha256 == _sha(scriptlet.read_bytes())

    root = generate_test_trust_root(tmp_path / "keys", label="cli-3o")
    pub = _public_key_hex(root, tmp_path / "pub.hex")
    signed = tmp_path / "signed"
    completed = subprocess.run(
        [
            sys.executable,
            os.fspath(script),
            "sign",
            "--acquired",
            str(acquired),
            "--private-key",
            str(root.private_key_path),
            "--public-key",
            str(pub),
            "--output",
            str(signed),
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    assert completed.returncode == 0, completed.stdout + completed.stderr

    completed = subprocess.run(
        [
            sys.executable,
            os.fspath(script),
            "dry-run",
            "--tag",
            TAG,
            "--run-id",
            str(RUN_ID),
            "--signed-assets",
            str(signed),
            "--acquired",
            str(acquired),
            "--public-key",
            str(pub),
            "--work",
            str(tmp_path / "dry-work"),
            "--skip-promote-shell",
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    # Partial verification (--skip-promote-shell) must not share complete-success exit.
    assert completed.returncode == 2, completed.stdout + completed.stderr
    assert "production_promote_gate_refusal_category=official-pin-unprovisioned" in (
        completed.stdout
    )
    assert "isolated_cryptographic_verification=pass" in completed.stdout
    assert "dry_run_outcome=partial-verification" in completed.stdout


def _bash_available() -> str | None:
    """Prefer Git Bash; never WSL bash (Windows path / shebang incompatible)."""
    import release_orchestrate as ro

    return ro._resolve_bash()


def _signed_bundle_for_shell(tmp_path: Path):
    state = _seed_mock_state(tmp_path)
    acquired = acquire_release(
        tag=TAG, output_dir=tmp_path / "acquired", run_id=RUN_ID, gh=MockGh(state)
    )
    root = generate_test_trust_root(tmp_path / "keys", label="shell-3o")
    pub = _public_key_hex(root, tmp_path / "pub.hex")
    signed = sign_acquired(
        acquired=acquired.root,
        private_key=root.private_key_path,
        public_key=pub,
        output_dir=tmp_path / "signed",
    )
    return state, acquired, root, pub, signed


@pytest.mark.skipif(_bash_available() is None, reason="bash required for promote shell")
def test_cli_dry_run_real_promote_shell_unprovisioned_boundary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    state, acquired, _root, pub, signed = _signed_bundle_for_shell(tmp_path)
    mock_gh = _write_mock_gh_exe(state, tmp_path / "bin")
    monkeypatch.setenv("IGNITE_RELEASE_GH", str(mock_gh))
    # promote-release.sh invokes bare `gh` on PATH.
    path = os.pathsep.join([str(tmp_path / "bin"), os.environ.get("PATH", "")])
    monkeypatch.setenv("PATH", path)
    monkeypatch.delenv("IGNITE_PROMOTE_PUBLIC_KEY_FILE", raising=False)

    script = _RELEASE / "ignite_release.py"
    completed = subprocess.run(
        [
            sys.executable,
            os.fspath(script),
            "dry-run",
            "--tag",
            TAG,
            "--run-id",
            str(RUN_ID),
            "--signed-assets",
            str(signed.root),
            "--acquired",
            str(acquired.root),
            "--public-key",
            str(pub),
            "--work",
            str(tmp_path / "dry-work"),
        ],
        check=False,
        capture_output=True,
        text=True,
        env=os.environ.copy(),
    )
    out = completed.stdout + completed.stderr
    assert completed.returncode == 0, out
    assert "dry_run_outcome=complete-rehearsal-unprovisioned-boundary" in completed.stdout
    assert "promote_shell_phase_guard_ok=true" in completed.stdout
    assert "promote_shell_phase_resolve_ok=true" in completed.stdout
    assert "promote_shell_phase_fetch_ok=true" in completed.stdout
    assert "promote_shell_phase_verify=true" in completed.stdout
    assert "promote_shell_refusal_marker=official-pin-unprovisioned" in completed.stdout
    assert (
        _REPO / "tools" / "release" / "kindle-release-public-key.hex"
    ).read_bytes() == b"UNPROVISIONED\n"


@pytest.mark.skipif(_bash_available() is None, reason="bash required for promote shell")
def test_dry_run_shell_resolve_failure_is_not_success(tmp_path: Path) -> None:
    state, acquired, _root, pub, signed = _signed_bundle_for_shell(tmp_path)
    (state / "resolve.fail").write_text("cannot resolve run\n", encoding="ascii")
    mock_bin = tmp_path / "bin"
    _write_mock_gh_exe(state, mock_bin)
    report = dry_run_release(
        tag=TAG,
        run_id=RUN_ID,
        signed_assets=signed.root,
        work_dir=tmp_path / "work",
        acquired_dir=acquired.root,
        public_key=pub,
        run_promote_shell=True,
        promote_env_extra={"PATH": os.pathsep.join([str(mock_bin), os.environ["PATH"]])},
    )
    assert report.outcome == DryRunOutcome.FAILED
    assert report.exit_status == 1
    assert "promote_shell_phase_resolve_ok=false" in "\n".join(report.lines)


@pytest.mark.skipif(_bash_available() is None, reason="bash required for promote shell")
def test_dry_run_shell_fetch_failure_is_not_success(tmp_path: Path) -> None:
    state, acquired, _root, pub, signed = _signed_bundle_for_shell(tmp_path)
    (state / "fetch.fail").write_text("cannot download artifact\n", encoding="ascii")
    mock_bin = tmp_path / "bin"
    _write_mock_gh_exe(state, mock_bin)
    report = dry_run_release(
        tag=TAG,
        run_id=RUN_ID,
        signed_assets=signed.root,
        work_dir=tmp_path / "work",
        acquired_dir=acquired.root,
        public_key=pub,
        run_promote_shell=True,
        promote_env_extra={"PATH": os.pathsep.join([str(mock_bin), os.environ["PATH"]])},
    )
    assert report.outcome == DryRunOutcome.FAILED
    assert "promote_shell_phase_fetch_ok=false" in "\n".join(report.lines)
    assert "promote_shell_phase_resolve_ok=true" in "\n".join(report.lines)


@pytest.mark.skipif(_bash_available() is None, reason="bash required for promote shell")
def test_dry_run_wrong_signature_is_not_pin_boundary_success(tmp_path: Path) -> None:
    state, acquired, _root, pub, signed = _signed_bundle_for_shell(tmp_path)
    mock_bin = tmp_path / "bin"
    _write_mock_gh_exe(state, mock_bin)
    # Keep isolated crypto intact; override only the shell's release-v1 signature.
    wrong_sig = "ab" * 64
    report = dry_run_release(
        tag=TAG,
        run_id=RUN_ID,
        signed_assets=signed.root,
        work_dir=tmp_path / "work",
        acquired_dir=acquired.root,
        public_key=pub,
        run_promote_shell=True,
        promote_env_extra={
            "PATH": os.pathsep.join([str(mock_bin), os.environ["PATH"]]),
            "IGNITE_PROMOTE_KINDLE_SIGNATURE": wrong_sig,
        },
    )
    assert report.outcome == DryRunOutcome.FAILED
    joined = "\n".join(report.lines)
    assert "promote_shell_refusal_marker=official-pin-unprovisioned" not in joined
    assert "dry_run_outcome=verification-failed" in joined
    # Must have reached verify (signature check) after fetch.
    assert "promote_shell_phase_fetch_ok=true" in joined
    assert "promote_shell_phase_verify=true" in joined


def _assert_verify_failed_not_pin(report) -> str:
    assert report.outcome == DryRunOutcome.FAILED
    assert report.exit_status == 1
    joined = "\n".join(report.lines)
    assert "dry_run_outcome=verification-failed" in joined
    assert "promote_shell_refusal_marker=official-pin-unprovisioned" not in joined
    assert "promote_shell_phase_fetch_ok=true" in joined
    assert "promote_shell_phase_verify=true" in joined
    return joined


@pytest.mark.skipif(_bash_available() is None, reason="bash required for promote shell")
def test_dry_run_kindle_sidecar_mismatch_not_swallowed(tmp_path: Path) -> None:
    """Sidecar disagrees; recorded KINDLE_SHA still matches Scriptlet bytes."""
    state, acquired, _root, pub, signed = _signed_bundle_for_shell(tmp_path)
    side = state / "arts" / KINDLE_ARTIFACT / f"Install Ignite v{VERSION}.sha256"
    side.write_bytes(f"{'aa' * 32}  Install Ignite v{VERSION}.sh\n".encode("ascii"))
    mock_bin = tmp_path / "bin"
    _write_mock_gh_exe(state, mock_bin)
    report = dry_run_release(
        tag=TAG,
        run_id=RUN_ID,
        signed_assets=signed.root,
        work_dir=tmp_path / "work",
        acquired_dir=acquired.root,
        public_key=pub,
        run_promote_shell=True,
        promote_env_extra={"PATH": os.pathsep.join([str(mock_bin), os.environ["PATH"]])},
    )
    joined = _assert_verify_failed_not_pin(report)
    assert "checksum sidecar" in joined.lower() or "promote_shell_refusal_marker=other" in joined


@pytest.mark.skipif(_bash_available() is None, reason="bash required for promote shell")
def test_dry_run_linux_sidecar_mismatch_not_swallowed(tmp_path: Path) -> None:
    state, acquired, _root, pub, signed = _signed_bundle_for_shell(tmp_path)
    bundle = f"ignite-server-{VERSION}-linux-x86_64.tar.gz"
    side = state / "arts" / LINUX_ARTIFACT / f"{bundle}.sha256"
    side.write_bytes(f"{'bb' * 32}  {bundle}\n".encode("ascii"))
    mock_bin = tmp_path / "bin"
    _write_mock_gh_exe(state, mock_bin)
    report = dry_run_release(
        tag=TAG,
        run_id=RUN_ID,
        signed_assets=signed.root,
        work_dir=tmp_path / "work",
        acquired_dir=acquired.root,
        public_key=pub,
        run_promote_shell=True,
        promote_env_extra={"PATH": os.pathsep.join([str(mock_bin), os.environ["PATH"]])},
    )
    joined = _assert_verify_failed_not_pin(report)
    assert "promote_shell_refusal_marker=other" in joined


@pytest.mark.skipif(_bash_available() is None, reason="bash required for promote shell")
def test_dry_run_linux_extract_failure_not_swallowed(tmp_path: Path) -> None:
    state, acquired, _root, pub, signed = _signed_bundle_for_shell(tmp_path)
    bundle = f"ignite-server-{VERSION}-linux-x86_64.tar.gz"
    gz = state / "arts" / LINUX_ARTIFACT / bundle
    gz.write_bytes(b"not-a-gzip-archive\n")
    digest = _sha(gz.read_bytes())
    (state / "arts" / LINUX_ARTIFACT / f"{bundle}.sha256").write_bytes(
        f"{digest}  {bundle}\n".encode("ascii")
    )
    mock_bin = tmp_path / "bin"
    _write_mock_gh_exe(state, mock_bin)
    report = dry_run_release(
        tag=TAG,
        run_id=RUN_ID,
        signed_assets=signed.root,
        work_dir=tmp_path / "work",
        acquired_dir=acquired.root,
        public_key=pub,
        run_promote_shell=True,
        promote_env_extra={
            "PATH": os.pathsep.join([str(mock_bin), os.environ["PATH"]]),
            "IGNITE_PROMOTE_LINUX_SHA256": digest,
        },
    )
    joined = _assert_verify_failed_not_pin(report)
    assert "promote_shell_refusal_marker=other" in joined
    assert "extraction" in joined.lower() or "tar" in joined.lower() or "other" in joined


@pytest.mark.skipif(_bash_available() is None, reason="bash required for promote shell")
def test_dry_run_unrelated_unprovisioned_word_is_not_success(tmp_path: Path) -> None:
    state, acquired, _root, pub, signed = _signed_bundle_for_shell(tmp_path)
    (state / "unrelated.unprovisioned.fail").write_text("1\n", encoding="ascii")
    mock_bin = tmp_path / "bin"
    _write_mock_gh_exe(state, mock_bin)
    report = dry_run_release(
        tag=TAG,
        run_id=RUN_ID,
        signed_assets=signed.root,
        work_dir=tmp_path / "work",
        acquired_dir=acquired.root,
        public_key=pub,
        run_promote_shell=True,
        promote_env_extra={"PATH": os.pathsep.join([str(mock_bin), os.environ["PATH"]])},
    )
    assert report.outcome == DryRunOutcome.FAILED
    joined = "\n".join(report.lines)
    assert "promote_shell_refusal_marker=official-pin-unprovisioned" not in joined
    assert "promote_shell_phase_resolve_ok=false" in joined


def test_dry_run_bash_unavailable_is_not_performed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _state, acquired, _root, pub, signed = _signed_bundle_for_shell(tmp_path)
    import release_orchestrate as ro

    def no_bash(*_a, **_k):
        raise OrchestrateError("bash unavailable for promote-release.sh dry-run")

    monkeypatch.setattr(ro, "run_promote_dry_run_shell", no_bash)
    report = dry_run_release(
        tag=TAG,
        run_id=RUN_ID,
        signed_assets=signed.root,
        work_dir=tmp_path / "work",
        acquired_dir=acquired.root,
        public_key=pub,
        run_promote_shell=True,
    )
    assert report.outcome == DryRunOutcome.NOT_PERFORMED
    assert report.exit_status == 3
    assert "dry_run_outcome=verification-not-performed" in "\n".join(report.lines)


def test_classify_shell_requires_phase_markers() -> None:
    cat, _ = classify_promote_shell_output(
        combined="something unprovisioned happened\n",
        returncode=1,
        pin_unprovisioned=True,
    )
    assert cat == "shell-incomplete-phases"
    complete = "\n".join(
        [
            "promote-phase=guard-ok",
            "promote-phase=resolve-ok",
            "promote-phase=fetch-ok",
            "promote-phase=verify",
            "promote-shell-refusal=official-pin-unprovisioned",
        ]
    )
    cat, _ = classify_promote_shell_output(
        combined=complete, returncode=1, pin_unprovisioned=True
    )
    assert cat == "official-pin-unprovisioned"


def test_extracted_tree_refuses_symlink(tmp_path: Path) -> None:
    root = tmp_path / "tree"
    root.mkdir()
    (root / "ok.txt").write_bytes(b"hello\n")
    link = root / "link.txt"
    try:
        link.symlink_to(root / "ok.txt")
    except OSError:
        pytest.skip("symlinks unavailable on this platform/privileges")
    with pytest.raises(AcquireError, match="symlink refused"):
        extracted_tree_sha256(root)


def test_load_refuses_symlink_in_acquired_tree(tmp_path: Path) -> None:
    state = _seed_mock_state(tmp_path)
    bundle = acquire_release(
        tag=TAG, output_dir=tmp_path / "acquired", run_id=RUN_ID, gh=MockGh(state)
    )
    target = next(bundle.ci_artifacts.rglob(f"Install Ignite v{VERSION}.sh"))
    link = target.parent / "sneaky-link"
    try:
        link.symlink_to(target.name)
    except OSError:
        pytest.skip("symlinks unavailable on this platform/privileges")
    with pytest.raises(AcquireError, match="symlink refused"):
        load_acquired_bundle(bundle.root)


def test_promote_publish_forbids_public_key_override(tmp_path: Path) -> None:
    bash = _bash_available()
    if bash is None:
        pytest.skip("bash required")
    # Minimal env: script should refuse before needing full promote inputs when
    # override is set for publish. Tag/version still parse first — use valid tag.
    override = tmp_path / "alt.hex"
    override.write_bytes(b"ab" * 32 + b"\n")
    env = os.environ.copy()
    env.update(
        {
            "IGNITE_PROMOTE_PUBLIC_KEY_FILE": str(override),
            "IGNITE_PROMOTE_TAG": TAG,
            "IGNITE_PROMOTE_RUN_ID": str(RUN_ID),
            "IGNITE_PROMOTE_COMMIT": SOURCE_REV,
            "IGNITE_PROMOTE_CONFIRM": f"publish {TAG} from run {RUN_ID}",
            "IGNITE_PROMOTE_KINDLE_SHA256": "ab" * 32,
            "IGNITE_PROMOTE_LINUX_SHA256": "cd" * 32,
            "IGNITE_PROMOTE_KINDLE_SIGNATURE": "ef" * 64,
            "IGNITE_PROMOTE_WORK": str(tmp_path / "work"),
            "GITHUB_REPOSITORY": EXPECTED_REPOSITORY,
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_REF": "refs/heads/main",
        }
    )
    (tmp_path / "work").mkdir()
    script = os.fspath(_RELEASE / "promote-release.sh").replace("\\", "/")
    completed = subprocess.run(
        [bash, script, "publish"],
        check=False,
        capture_output=True,
        text=True,
        env=env,
    )
    assert completed.returncode != 0, completed.stderr + completed.stdout
    assert "IGNITE_PROMOTE_PUBLIC_KEY_FILE is forbidden for publish" in (
        completed.stderr + completed.stdout
    )


def test_production_pin_unprovisioned() -> None:
    pin = _REPO / "tools" / "release" / "kindle-release-public-key.hex"
    assert pin.read_bytes() == b"UNPROVISIONED\n"


def test_max_run_search_pages_documented() -> None:
    assert MAX_RUN_SEARCH_PAGES >= 1
