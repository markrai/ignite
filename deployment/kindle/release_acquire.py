"""Trusted CI artifact acquisition for Ignite Kindle releases (Slice 3D-B2a.4.3o).

Uses the authenticated GitHub API / ``gh`` CLI as the source of run identity.
Unsigned measurement fields (``source_revision``, ``workflow_run_id``,
``artifact-association.v1``) are cross-checked against that API identity and are
never independent authority.

Selection policy when ``--run-id`` is omitted:
  Paginate successful ``ci.yml`` runs on ``main`` (``per_page=100``) up to
  ``MAX_RUN_SEARCH_PAGES`` (bounded search). Among eligible runs in that window
  (event ``push``/``workflow_dispatch``, ``ignite/VERSION`` at ``head_sha``
  matches the tag, three required unexpired artifacts present), choose the
  highest numeric ``run_id``. The receipt records ``selection_search_bounded``
  and pages examined — never claims an exhaustive search when capped.
  Never combine artifacts across runs or run attempts.

Download uses name-based ``gh run download`` then re-lists artifact metadata to
rebind/verify GitHub artifact IDs. Residual race: a hostile rename between
list/download/relist could still confuse name-based download; ID recheck after
download detects identity drift for the designated run.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parents[1]
_RELEASE = _REPO / "tools" / "release"
for extra in (_HERE, str(_RELEASE)):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from ci_packaging_measurement import (  # noqa: E402
    MEASUREMENT_BASENAME,
    PACKAGE_PROFILE_BASENAME,
    MeasurementError,
    parse_ci_packaging_measurement,
    read_bounded_plain_file,
)
from kindle_release_sign import ReleaseSigningError, version_from_tag  # noqa: E402
from offline_packaging_handoff import HandoffError, verify_handoff_inputs  # noqa: E402
from offline_release_ceremony import (  # noqa: E402
    CeremonyError,
    _write_bytes_exclusive,
    discover_ci_artifacts,
)
from release_identity import (  # noqa: E402
    EXPECTED_BRANCH,
    EXPECTED_REPOSITORY,
    EXPECTED_WORKFLOW,
    EXPECTED_WORKFLOW_FILE,
    IDENTITY_BASENAME,
    KINDLE_ARTIFACT,
    LINUX_ARTIFACT,
    MEASUREMENTS_ARTIFACT,
    RECEIPT_BASENAME,
    ArtifactRecord,
    ReleaseIdentity,
    ReleaseIdentityError,
    sha256_file,
)

ASSOCIATION_BASENAME = "artifact-association.v1"
ASSOCIATION_HEADER = "IGNITE_CI_KINDLE_ARTIFACT_ASSOCIATION_V1"
MAX_ASSOCIATION_BYTES = 4096
MAX_RUN_SEARCH_PAGES = 10  # 10 * 100 = 1000 newest successful main runs
RUNS_PER_PAGE = 100
_COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
_RUN_RE = re.compile(r"^[1-9][0-9]*$")
_ALLOWED_ACQUIRED_TOP = frozenset(
    {
        RECEIPT_BASENAME,
        IDENTITY_BASENAME,
        "ci-artifacts",
        "linux",
        "meta",
        "artifact-trees",
    }
)
_ALLOWED_CI_NAMES = frozenset(
    {
        # discovered dynamically for scriptlet/checksum; fixed companions:
        MEASUREMENT_BASENAME,
        PACKAGE_PROFILE_BASENAME,
        ASSOCIATION_BASENAME,
        "sfx-payload-equivalence.v1",
    }
)


class AcquireError(ValueError):
    """Fail-closed acquisition refusal."""


GhRunner = Callable[[Sequence[str]], subprocess.CompletedProcess[str]]


def _default_gh(argv: Sequence[str]) -> subprocess.CompletedProcess[str]:
    binary = os.environ.get("IGNITE_RELEASE_GH", "gh")
    return subprocess.run(
        [binary, *argv],
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )


def _positive_int(value: object, *, label: str) -> int:
    try:
        number = int(value)  # type: ignore[arg-type]
    except (TypeError, ValueError) as exc:
        raise AcquireError(f"{label} must be a positive integer") from exc
    if number < 1:
        raise AcquireError(f"{label} must be a positive integer")
    return number


@dataclass(frozen=True)
class AcquiredBundle:
    root: Path
    identity: ReleaseIdentity
    ci_artifacts: Path
    linux_dir: Path
    receipt_path: Path
    identity_path: Path


def _run_gh(gh: GhRunner, argv: Sequence[str], *, label: str) -> str:
    completed = gh(argv)
    if completed.returncode != 0:
        err = (completed.stderr or completed.stdout or "").strip()
        raise AcquireError(f"gh {label} failed: {err or 'nonzero exit'}")
    return completed.stdout


def _gh_json(gh: GhRunner, argv: Sequence[str], *, label: str) -> object:
    text = _run_gh(gh, argv, label=label)
    try:
        return json.loads(text)
    except json.JSONDecodeError as exc:
        raise AcquireError(f"gh {label} returned non-JSON") from exc


def _lstat_is_symlink(path: Path) -> bool:
    try:
        return stat.S_ISLNK(os.lstat(os.fspath(path)).st_mode)
    except OSError:
        return False


def _refuse_new_dir(path: Path, *, label: str) -> None:
    if path.exists() or _lstat_is_symlink(path):
        raise AcquireError(f"{label} must be a new unused path")
    parent = path.parent
    if _lstat_is_symlink(parent):
        raise AcquireError(f"{label} parent must not be a symlink")
    if parent.exists() and not parent.is_dir():
        raise AcquireError(f"{label} parent is not a directory")


def _workflow_id(gh: GhRunner, repository: str) -> int:
    data = _gh_json(
        gh,
        ["api", f"repos/{repository}/actions/workflows/{EXPECTED_WORKFLOW_FILE}"],
        label="workflow-id",
    )
    if not isinstance(data, dict) or "id" not in data:
        raise AcquireError("cannot resolve designated ci.yml workflow id")
    return _positive_int(data["id"], label="workflow id")


def _is_ci_run_path(path: str) -> bool:
    return path == EXPECTED_WORKFLOW or path.startswith(EXPECTED_WORKFLOW + "@")


def _fetch_run(gh: GhRunner, repository: str, run_id: int) -> dict:
    data = _gh_json(
        gh,
        ["api", f"repos/{repository}/actions/runs/{run_id}"],
        label=f"run-{run_id}",
    )
    if not isinstance(data, dict):
        raise AcquireError("run metadata malformed")
    return data


def _pages_from_slurp(data: object, *, label: str) -> list[dict]:
    """Normalize ``gh api --paginate --slurp`` output to a list of page objects."""
    if isinstance(data, dict):
        return [data]
    if isinstance(data, list):
        pages: list[dict] = []
        for page in data:
            if not isinstance(page, dict):
                raise AcquireError(f"{label} page is not an object")
            pages.append(page)
        return pages
    raise AcquireError(f"{label} response malformed")


def _list_artifacts(gh: GhRunner, repository: str, run_id: int) -> list[dict]:
    # --slurp yields a JSON array of page objects under --paginate.
    data = _gh_json(
        gh,
        [
            "api",
            f"repos/{repository}/actions/runs/{run_id}/artifacts?per_page=100",
            "--paginate",
            "--slurp",
        ],
        label=f"artifacts-{run_id}",
    )
    artifacts: list[dict] = []
    for page in _pages_from_slurp(data, label="artifact-list"):
        items = page.get("artifacts")
        if items is None:
            raise AcquireError("artifact page missing artifacts array")
        if not isinstance(items, list):
            raise AcquireError("artifact page artifacts field malformed")
        for art in items:
            if not isinstance(art, dict):
                raise AcquireError("artifact metadata entry malformed")
            artifacts.append(art)
    return artifacts


def _version_at_commit(gh: GhRunner, repository: str, commit: str) -> str:
    # contents API returns base64; use raw media type via Accept header.
    text = _run_gh(
        gh,
        [
            "api",
            f"repos/{repository}/contents/ignite/VERSION?ref={commit}",
            "-H",
            "Accept: application/vnd.github.raw",
        ],
        label="version-at-commit",
    )
    version = text.strip()
    if not version:
        raise AcquireError("ignite/VERSION empty at commit")
    return version


def _validate_run(
    run: Mapping[str, object],
    *,
    want_workflow_id: int,
    version: str,
    repository: str,
    gh: GhRunner,
    check_version: bool = True,
) -> int:
    """Validate run metadata. Returns run_id."""
    run_id = _positive_int(run.get("id"), label="run id")
    status = str(run.get("status", ""))
    conclusion = str(run.get("conclusion", ""))
    if status != "completed" or conclusion != "success":
        raise AcquireError(
            f"run not successful completed (status={status} conclusion={conclusion})"
        )
    head_branch = str(run.get("head_branch", ""))
    if head_branch != EXPECTED_BRANCH:
        raise AcquireError(f"run branch refused: {head_branch}")
    event = str(run.get("event", ""))
    if event == "pull_request":
        raise AcquireError("pull_request artifacts are not eligible")
    if event not in {"push", "workflow_dispatch"}:
        raise AcquireError(f"run event refused: {event}")
    path = str(run.get("path", ""))
    if not _is_ci_run_path(path):
        raise AcquireError(f"run workflow path refused: {path}")
    wid = _positive_int(run.get("workflow_id"), label="run workflow_id")
    if wid != want_workflow_id:
        raise AcquireError("run workflow_id does not match designated ci.yml")
    head_sha = str(run.get("head_sha", ""))
    if not _COMMIT_RE.fullmatch(head_sha):
        raise AcquireError("run head_sha malformed")
    attempt = _positive_int(run.get("run_attempt", 1), label="run_attempt")
    del attempt  # validated presence; callers read from run again
    repo_obj = run.get("repository")
    if isinstance(repo_obj, dict):
        full = str(repo_obj.get("full_name", ""))
        if full and full != repository:
            raise AcquireError(f"run repository refused: {full}")
    if check_version:
        at_commit = _version_at_commit(gh, repository, head_sha)
        if at_commit != version:
            raise AcquireError(
                f"ignite/VERSION at {head_sha} is {at_commit!r}, expected {version!r}"
            )
    return run_id


def _select_run_id(
    gh: GhRunner,
    *,
    repository: str,
    version: str,
    workflow_id: int,
) -> tuple[int, str, dict[str, str]]:
    """Bounded paginated search; pick highest eligible run_id in the window."""
    candidates: list[int] = []
    pages_seen = 0
    more_pages = True
    page = 1
    while page <= MAX_RUN_SEARCH_PAGES:
        data = _gh_json(
            gh,
            [
                "api",
                (
                    f"repos/{repository}/actions/workflows/{EXPECTED_WORKFLOW_FILE}/runs"
                    f"?branch={EXPECTED_BRANCH}&status=success"
                    f"&per_page={RUNS_PER_PAGE}&page={page}"
                ),
            ],
            label=f"list-runs-page-{page}",
        )
        if not isinstance(data, dict) or "workflow_runs" not in data:
            raise AcquireError("workflow run list malformed")
        runs = data["workflow_runs"]
        if not isinstance(runs, list):
            raise AcquireError("workflow_runs field malformed")
        pages_seen += 1
        if not runs:
            more_pages = False
            break
        for run in runs:
            if not isinstance(run, dict):
                raise AcquireError("workflow run entry malformed")
            try:
                rid = _validate_run(
                    run,
                    want_workflow_id=workflow_id,
                    version=version,
                    repository=repository,
                    gh=gh,
                )
                _require_artifact_set(_list_artifacts(gh, repository, rid))
                candidates.append(rid)
            except AcquireError:
                continue
        if len(runs) < RUNS_PER_PAGE:
            more_pages = False
            break
        page += 1
    else:
        more_pages = True  # hit page cap with a full final page possible

    bounded = pages_seen >= MAX_RUN_SEARCH_PAGES and more_pages
    meta = {
        "selection_search_bounded": "true" if bounded else "false",
        "selection_pages_searched": str(pages_seen),
        "selection_page_size": str(RUNS_PER_PAGE),
        "selection_max_pages": str(MAX_RUN_SEARCH_PAGES),
    }
    if not candidates:
        hint = (
            f" (bounded search of {pages_seen} page(s); not exhaustive)"
            if bounded or pages_seen >= 1
            else ""
        )
        raise AcquireError(
            f"no eligible successful {EXPECTED_WORKFLOW} run on {EXPECTED_BRANCH} "
            f"for version {version}{hint}"
        )
    chosen = max(candidates)
    policy = (
        "highest-eligible-run-id-within-bounded-search"
        if bounded
        else "highest-eligible-run-id"
    )
    return chosen, policy, meta


def _require_artifact_set(artifacts: Sequence[Mapping[str, object]]) -> dict[str, dict]:
    wanted = {KINDLE_ARTIFACT, MEASUREMENTS_ARTIFACT, LINUX_ARTIFACT}
    found: dict[str, dict] = {}
    for art in artifacts:
        name = str(art.get("name", ""))
        if name not in wanted:
            continue
        if name in found:
            raise AcquireError(f"duplicate artifact name in run: {name}")
        if art.get("expired") is True:
            raise AcquireError(f"artifact expired: {name}")
        artifact_id = _positive_int(art.get("id"), label=f"artifact id ({name})")
        found[name] = dict(art)
        found[name]["id"] = artifact_id
    missing = sorted(wanted - set(found))
    if missing:
        raise AcquireError("missing required artifacts: " + ", ".join(missing))
    ids = {int(found[n]["id"]) for n in wanted}
    if len(ids) != 3:
        raise AcquireError("duplicate artifact identities across required set")
    return found


def _download_named(
    gh: GhRunner,
    *,
    repository: str,
    run_id: int,
    name: str,
    dest: Path,
) -> None:
    dest.mkdir(parents=False, exist_ok=False)
    _run_gh(
        gh,
        [
            "run",
            "download",
            str(run_id),
            "-R",
            repository,
            "-n",
            name,
            "-D",
            os.fspath(dest),
        ],
        label=f"download-{name}",
    )


def _recheck_artifact_identities(
    gh: GhRunner,
    *,
    repository: str,
    run_id: int,
    expected: Mapping[str, dict],
) -> None:
    """Relist artifacts after download; refuse ID/name/expiry drift.

    Residual limitation: ``gh run download -n`` is name-based, so a hostile
    replace between the pre-download list and download could still deliver the
    wrong zip for that name; this recheck binds the *current* run metadata IDs
    to the names we requested and refuses drift from the pre-download set.
    """
    current = _require_artifact_set(_list_artifacts(gh, repository, run_id))
    for name, before in expected.items():
        after = current[name]
        if int(after["id"]) != int(before["id"]):
            raise AcquireError(
                f"artifact id changed after download for {name}: "
                f"{before['id']} -> {after['id']}"
            )
        if after.get("expired") is True:
            raise AcquireError(f"artifact expired after download: {name}")


def extracted_tree_sha256(root: Path) -> str:
    """SHA-256 over sorted relative paths + file bytes (extracted tree, not zip).

    Symlinks and non-regular entries are an explicit refusal — never silently skipped.
    """
    if _lstat_is_symlink(root) or not root.is_dir():
        raise AcquireError("extracted tree root refused")
    h = hashlib.sha256()
    entries = sorted(root.rglob("*"), key=lambda p: p.relative_to(root).as_posix())
    files: list[Path] = []
    for path in entries:
        if _lstat_is_symlink(path):
            raise AcquireError(
                f"symlink refused in extracted tree: {path.relative_to(root).as_posix()}"
            )
        try:
            st = os.lstat(os.fspath(path))
        except OSError as exc:
            raise AcquireError(f"cannot lstat extracted tree entry: {exc}") from exc
        if stat.S_ISDIR(st.st_mode):
            continue
        if not stat.S_ISREG(st.st_mode):
            raise AcquireError(
                "unsupported filesystem entry in extracted tree: "
                f"{path.relative_to(root).as_posix()}"
            )
        files.append(path)
    for path in files:
        rel = path.relative_to(root).as_posix().encode("utf-8")
        digest = hashlib.sha256()
        with path.open("rb") as handle:
            while chunk := handle.read(1024 * 1024):
                digest.update(chunk)
        file_digest = digest.digest()
        h.update(len(rel).to_bytes(4, "big"))
        h.update(rel)
        h.update(file_digest)
    return h.hexdigest()


def _parse_sha256sum(path: Path, *, expected_name: str) -> str:
    raw = read_bounded_plain_file(path, max_bytes=256, label=path.name)
    if b"\r" in raw or not raw.endswith(b"\n"):
        raise AcquireError(f"checksum sidecar refused: {path.name}")
    lines = raw.decode("ascii").splitlines()
    if len(lines) != 1:
        raise AcquireError(f"checksum sidecar must be one line: {path.name}")
    parts = lines[0].split("  ")
    if len(parts) != 2:
        raise AcquireError(f"checksum sidecar format refused: {path.name}")
    digest, filename = parts
    if not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise AcquireError(f"checksum digest malformed: {path.name}")
    if filename != expected_name:
        raise AcquireError(
            f"checksum filename {filename!r} does not match {expected_name!r}"
        )
    return digest


def _cross_check_association(
    association_path: Path,
    *,
    identity_head_sha: str,
    identity_run_id: int,
    version: str,
    measurement_sha256: str | None = None,
    require_fields: bool = False,
) -> bool:
    if not association_path.is_file():
        return False
    raw = read_bounded_plain_file(
        association_path, max_bytes=MAX_ASSOCIATION_BYTES, label=ASSOCIATION_BASENAME
    )
    text = raw.decode("ascii")
    if not text.startswith(ASSOCIATION_HEADER + "\n"):
        raise AcquireError("artifact-association header refused")
    fields: dict[str, str] = {}
    for line in text.splitlines()[1:]:
        if not line:
            continue
        if "=" not in line:
            raise AcquireError("association line malformed")
        key, value = line.split("=", 1)
        if key in fields:
            raise AcquireError(f"duplicate association field: {key}")
        fields[key] = value
    if fields.get("customer_artifact") != KINDLE_ARTIFACT:
        raise AcquireError("association customer_artifact mismatch")
    if fields.get("measurement_artifact") != MEASUREMENTS_ARTIFACT:
        raise AcquireError("association measurement_artifact mismatch")
    if require_fields or "source_revision" in fields:
        if fields.get("source_revision") != identity_head_sha:
            raise AcquireError(
                "association source_revision disagrees with GitHub run head_sha"
            )
    if require_fields or "workflow_run_id" in fields:
        if fields.get("workflow_run_id") != str(identity_run_id):
            raise AcquireError(
                "association workflow_run_id disagrees with GitHub run id"
            )
    if require_fields or "version" in fields:
        if "version" not in fields:
            raise AcquireError("association missing version")
        if fields.get("version") != version:
            raise AcquireError("association version mismatch")
    if measurement_sha256 is not None and "measurement_sha256" in fields:
        if fields["measurement_sha256"] != measurement_sha256:
            raise AcquireError("association measurement_sha256 mismatch")
    # Full cross-check requires an explicit version binding. Older CI
    # association records omit it; present fields may still be validated above,
    # but must not be claimed as association_cross_checked=true.
    if "version" not in fields:
        return False
    return True


def _cross_check_measurement(
    measurement_path: Path,
    *,
    head_sha: str,
    run_id: int,
    version: str,
) -> None:
    parsed = parse_ci_packaging_measurement(measurement_path.read_bytes())
    if parsed["version"] != version:
        raise AcquireError("measurement version mismatch")
    src = parsed.get("source_revision")
    if src is not None and src != head_sha:
        raise AcquireError(
            "measurement source_revision disagrees with GitHub run head_sha "
            "(unsigned field is not independent authority)"
        )
    wr = parsed.get("workflow_run_id")
    if wr is not None and wr != str(run_id):
        raise AcquireError(
            "measurement workflow_run_id disagrees with GitHub run id "
            "(unsigned field is not independent authority)"
        )


def acquire_release(
    *,
    tag: str,
    output_dir: Path,
    run_id: int | None = None,
    repository: str = EXPECTED_REPOSITORY,
    gh: GhRunner | None = None,
) -> AcquiredBundle:
    """Download and verify Kindle + measurements + Linux artifacts for one CI run."""
    gh = gh or _default_gh
    try:
        version = version_from_tag(tag)
    except ReleaseSigningError as exc:
        raise AcquireError(str(exc)) from exc
    if repository != EXPECTED_REPOSITORY:
        raise AcquireError(f"repository refused: {repository}")

    _refuse_new_dir(output_dir, label="acquire output")

    workflow_id = _workflow_id(gh, repository)
    selection_policy = "explicit-run-id"
    selection_meta: dict[str, str] = {
        "selection_search_bounded": "false",
        "selection_pages_searched": "0",
    }
    if run_id is None:
        run_id, selection_policy, selection_meta = _select_run_id(
            gh, repository=repository, version=version, workflow_id=workflow_id
        )
    else:
        run_id = _positive_int(run_id, label="run id")

    run = _fetch_run(gh, repository, run_id)
    _validate_run(
        run,
        want_workflow_id=workflow_id,
        version=version,
        repository=repository,
        gh=gh,
    )
    head_sha = str(run["head_sha"])
    run_attempt = _positive_int(run.get("run_attempt", 1), label="run_attempt")
    event = str(run["event"])
    head_branch = str(run["head_branch"])

    artifact_meta = _require_artifact_set(_list_artifacts(gh, repository, run_id))

    token = f"ignite-acquire-{os.getpid()}"
    staging_parent = output_dir.parent
    staging = staging_parent / f".{token}-{run_id}"
    if staging.exists() or _lstat_is_symlink(staging):
        raise AcquireError("acquisition staging path collision")
    try:
        staging.mkdir(parents=False, exist_ok=False)
        kindle_dir = staging / "kindle-raw"
        meas_dir = staging / "measurements-raw"
        linux_dir = staging / "linux"
        ci_artifacts = staging / "ci-artifacts"

        _download_named(
            gh,
            repository=repository,
            run_id=run_id,
            name=KINDLE_ARTIFACT,
            dest=kindle_dir,
        )
        _download_named(
            gh,
            repository=repository,
            run_id=run_id,
            name=MEASUREMENTS_ARTIFACT,
            dest=meas_dir,
        )
        _download_named(
            gh,
            repository=repository,
            run_id=run_id,
            name=LINUX_ARTIFACT,
            dest=linux_dir,
        )

        # Name-based download residual race: rebind IDs after download.
        _recheck_artifact_identities(
            gh, repository=repository, run_id=run_id, expected=artifact_meta
        )

        # Merge Kindle + measurements into one ceremony-shaped tree.
        ci_artifacts.mkdir(parents=False, exist_ok=False)
        for src_dir in (kindle_dir, meas_dir):
            for path in src_dir.rglob("*"):
                if not path.is_file() or _lstat_is_symlink(path):
                    if _lstat_is_symlink(path):
                        raise AcquireError(f"symlink refused in downloaded artifact: {path.name}")
                    continue
                rel = path.relative_to(src_dir)
                dest = ci_artifacts / rel
                dest.parent.mkdir(parents=True, exist_ok=True)
                if dest.exists():
                    raise AcquireError(f"duplicate path while merging artifacts: {rel}")
                shutil.copy2(path, dest)

        # Validate Kindle/measurement layout via ceremony discovery + handoff.
        try:
            discovered = discover_ci_artifacts(ci_artifacts, tag=tag)
        except CeremonyError as exc:
            raise AcquireError(str(exc)) from exc
        try:
            handoff = verify_handoff_inputs(
                scriptlet=discovered.scriptlet,
                measurement_path=discovered.measurement,
                package_profile_path=discovered.package_profile,
                expected_version=version,
                expected_source_revision=head_sha,
            )
        except HandoffError as exc:
            raise AcquireError(str(exc)) from exc
        if not handoff.evidence_ready:
            raise AcquireError("CI packaging inputs not evidence-ready")

        scriptlet_sha = sha256_file(discovered.scriptlet)
        scriptlet_bytes = discovered.scriptlet.stat().st_size
        checksum_digest = _parse_sha256sum(
            discovered.checksum_sidecar, expected_name=discovered.scriptlet.name
        )
        if checksum_digest != scriptlet_sha:
            raise AcquireError("Scriptlet checksum sidecar digest mismatch")

        try:
            _cross_check_measurement(
                discovered.measurement,
                head_sha=head_sha,
                run_id=run_id,
                version=version,
            )
        except MeasurementError as exc:
            raise AcquireError(str(exc)) from exc

        association_path = None
        for candidate in ci_artifacts.rglob(ASSOCIATION_BASENAME):
            if _lstat_is_symlink(candidate):
                raise AcquireError("symlink refused for artifact-association")
            if association_path is not None:
                raise AcquireError("duplicate artifact-association.v1")
            association_path = candidate
        association_ok = False
        if association_path is not None:
            association_ok = _cross_check_association(
                association_path,
                identity_head_sha=head_sha,
                identity_run_id=run_id,
                version=version,
                measurement_sha256=sha256_file(discovered.measurement),
            )

        linux_name = f"ignite-server-{version}-linux-x86_64.tar.gz"
        linux_sum_name = f"{linux_name}.sha256"
        linux_bundle = linux_dir / linux_name
        linux_sum = linux_dir / linux_sum_name
        if not linux_bundle.is_file() or not linux_sum.is_file():
            raise AcquireError("Linux release artifact file set mismatch")
        if _lstat_is_symlink(linux_bundle) or _lstat_is_symlink(linux_sum):
            raise AcquireError("symlink refused in Linux artifact")
        linux_digest = _parse_sha256sum(linux_sum, expected_name=linux_name)
        linux_sha = sha256_file(linux_bundle)
        if linux_sha != linux_digest:
            raise AcquireError("Linux checksum sidecar digest mismatch")

        # Persist extracted-tree digests for the final ci-artifacts + linux trees
        # (after merge). Raw download dirs are digested below before delete.
        kindle_tree = extracted_tree_sha256(kindle_dir)
        meas_tree = extracted_tree_sha256(meas_dir)
        linux_tree = extracted_tree_sha256(linux_dir)

        identity = ReleaseIdentity(
            repository=repository,
            workflow=EXPECTED_WORKFLOW,
            tag=tag,
            version=version,
            ci_run_id=run_id,
            ci_run_attempt=run_attempt,
            ci_head_sha=head_sha,
            ci_event=event,
            ci_head_branch=head_branch,
            kindle_artifact=ArtifactRecord(
                name=KINDLE_ARTIFACT,
                artifact_id=int(artifact_meta[KINDLE_ARTIFACT]["id"]),
                digest_sha256=kindle_tree,
            ),
            measurements_artifact=ArtifactRecord(
                name=MEASUREMENTS_ARTIFACT,
                artifact_id=int(artifact_meta[MEASUREMENTS_ARTIFACT]["id"]),
                digest_sha256=meas_tree,
            ),
            linux_artifact=ArtifactRecord(
                name=LINUX_ARTIFACT,
                artifact_id=int(artifact_meta[LINUX_ARTIFACT]["id"]),
                digest_sha256=linux_tree,
            ),
            scriptlet_name=discovered.scriptlet.name,
            scriptlet_bytes=scriptlet_bytes,
            scriptlet_sha256=scriptlet_sha,
            scriptlet_checksum_sha256=sha256_file(discovered.checksum_sidecar),
            measurement_sha256=sha256_file(discovered.measurement),
            package_profile_sha256=sha256_file(discovered.package_profile),
            linux_bundle_name=linux_name,
            linux_bundle_bytes=linux_bundle.stat().st_size,
            linux_bundle_sha256=linux_sha,
            linux_checksum_sha256=sha256_file(linux_sum),
            association_cross_checked=association_ok,
            selection_policy=selection_policy,
            extra=(
                f"workflow_id={workflow_id}",
                f"measurement={MEASUREMENT_BASENAME}",
                f"package_profile={PACKAGE_PROFILE_BASENAME}",
                f"selection_search_bounded={selection_meta.get('selection_search_bounded', 'false')}",
                f"selection_pages_searched={selection_meta.get('selection_pages_searched', '0')}",
                "name_based_download_id_rechecked=true",
                "name_based_download_residual_race=documented",
            ),
        )
        identity.validate()

        meta_dir = staging / "meta"
        meta_dir.mkdir(parents=False, exist_ok=False)
        run_meta_path = meta_dir / "github-run.json"
        # Sanitize: keep only non-secret identity fields.
        sanitized = {
            "id": run_id,
            "run_attempt": run_attempt,
            "head_sha": head_sha,
            "head_branch": head_branch,
            "event": event,
            "path": run.get("path"),
            "workflow_id": workflow_id,
            "status": run.get("status"),
            "conclusion": run.get("conclusion"),
            "repository": repository,
            "artifacts": {
                name: {
                    "id": int(meta["id"]),
                    "expired": bool(meta.get("expired")),
                    "size_in_bytes": meta.get("size_in_bytes"),
                }
                for name, meta in artifact_meta.items()
            },
        }
        _write_bytes_exclusive(
            run_meta_path, (json.dumps(sanitized, indent=2, sort_keys=True) + "\n").encode("ascii")
        )
        _write_bytes_exclusive(staging / RECEIPT_BASENAME, identity.encode_receipt())
        _write_bytes_exclusive(staging / IDENTITY_BASENAME, identity.encode_identity())

        # Retain extracted download trees for transfer-time revalidation of
        # extracted_tree_sha256 values (not GitHub zip archive digests).
        trees = staging / "artifact-trees"
        trees.mkdir(parents=False, exist_ok=False)
        kindle_dir.rename(trees / "kindle")
        meas_dir.rename(trees / "measurements")
        # linux/ stays at staging/linux for promote; also snapshot tree digest source
        shutil.copytree(linux_dir, trees / "linux")

        if output_dir.exists() or _lstat_is_symlink(output_dir):
            raise AcquireError("acquire output appeared before finalize")
        os.replace(os.fspath(staging), os.fspath(output_dir))
    except BaseException:
        if staging.exists():
            shutil.rmtree(staging, ignore_errors=True)
        raise

    return AcquiredBundle(
        root=output_dir,
        identity=identity,
        ci_artifacts=output_dir / "ci-artifacts",
        linux_dir=output_dir / "linux",
        receipt_path=output_dir / RECEIPT_BASENAME,
        identity_path=output_dir / IDENTITY_BASENAME,
    )


def _refuse_unexpected_files(root: Path, *, allowed_names: set[str], label: str) -> None:
    for path in root.iterdir():
        if _lstat_is_symlink(path):
            raise AcquireError(f"symlink refused in {label}: {path.name}")
        if path.name not in allowed_names:
            raise AcquireError(f"unexpected {label} entry: {path.name}")


def load_acquired_bundle(root: Path) -> AcquiredBundle:
    """Fully revalidate a transferred acquisition bundle against its receipts.

    Establishes *internal consistency* of transferred public inputs — not a fresh
    GitHub provenance claim. Provenance association requires a subsequent
    authenticated GitHub resolve (promote dry-run / resolve).
    """
    from release_identity import parse_acquisition_receipt

    if _lstat_is_symlink(root) or not root.is_dir():
        raise AcquireError("acquired bundle root refused")
    _refuse_unexpected_files(
        root, allowed_names=set(_ALLOWED_ACQUIRED_TOP), label="acquired root"
    )

    receipt_path = root / RECEIPT_BASENAME
    identity_path = root / IDENTITY_BASENAME
    if not receipt_path.is_file() or not identity_path.is_file():
        raise AcquireError("acquired bundle missing receipt or release-identity")
    if _lstat_is_symlink(receipt_path) or _lstat_is_symlink(identity_path):
        raise AcquireError("symlink refused for receipt/identity")

    try:
        receipt_raw = read_bounded_plain_file(
            receipt_path, max_bytes=64 * 1024, label=RECEIPT_BASENAME
        )
        identity_raw = read_bounded_plain_file(
            identity_path, max_bytes=64 * 1024, label=IDENTITY_BASENAME
        )
        identity = parse_acquisition_receipt(receipt_raw)
    except (MeasurementError, ReleaseIdentityError) as exc:
        raise AcquireError(str(exc)) from exc

    # release-identity.v1 must agree exactly with the acquisition receipt body
    # after header/schema swap.
    expected_identity = identity.encode_identity()
    if identity_raw != expected_identity:
        raise AcquireError("release-identity.v1 does not match acquisition receipt")

    ci_artifacts = root / "ci-artifacts"
    linux_dir = root / "linux"
    trees = root / "artifact-trees"
    if not ci_artifacts.is_dir() or not linux_dir.is_dir() or not trees.is_dir():
        raise AcquireError("acquired bundle missing ci-artifacts/, linux/, or artifact-trees/")
    if (
        _lstat_is_symlink(ci_artifacts)
        or _lstat_is_symlink(linux_dir)
        or _lstat_is_symlink(trees)
    ):
        raise AcquireError("symlink refused for acquired directory")

    try:
        discovered = discover_ci_artifacts(ci_artifacts, tag=identity.tag)
    except CeremonyError as exc:
        raise AcquireError(str(exc)) from exc

    if discovered.scriptlet.name != identity.scriptlet_name:
        raise AcquireError("scriptlet name drifted from receipt")
    if discovered.scriptlet.stat().st_size != identity.scriptlet_bytes:
        raise AcquireError("scriptlet size drifted from receipt")
    if sha256_file(discovered.scriptlet) != identity.scriptlet_sha256:
        raise AcquireError("acquired Scriptlet digest drifted from receipt")
    if sha256_file(discovered.checksum_sidecar) != identity.scriptlet_checksum_sha256:
        raise AcquireError("scriptlet checksum file digest drifted from receipt")
    sidecar_digest = _parse_sha256sum(
        discovered.checksum_sidecar, expected_name=discovered.scriptlet.name
    )
    if sidecar_digest != identity.scriptlet_sha256:
        raise AcquireError("scriptlet checksum sidecar content disagrees with receipt")

    if sha256_file(discovered.measurement) != identity.measurement_sha256:
        raise AcquireError("acquired measurement digest drifted from receipt")
    if sha256_file(discovered.package_profile) != identity.package_profile_sha256:
        raise AcquireError("acquired package profile digest drifted from receipt")

    try:
        handoff = verify_handoff_inputs(
            scriptlet=discovered.scriptlet,
            measurement_path=discovered.measurement,
            package_profile_path=discovered.package_profile,
            expected_version=identity.version,
            expected_source_revision=identity.ci_head_sha,
        )
    except HandoffError as exc:
        raise AcquireError(str(exc)) from exc
    if not handoff.evidence_ready:
        raise AcquireError("transferred packaging inputs not evidence-ready")

    try:
        _cross_check_measurement(
            discovered.measurement,
            head_sha=identity.ci_head_sha,
            run_id=identity.ci_run_id,
            version=identity.version,
        )
    except MeasurementError as exc:
        raise AcquireError(str(exc)) from exc

    association_path = None
    for candidate in ci_artifacts.rglob(ASSOCIATION_BASENAME):
        if _lstat_is_symlink(candidate):
            raise AcquireError("symlink refused for artifact-association")
        if association_path is not None:
            raise AcquireError("duplicate artifact-association.v1")
        association_path = candidate
    if association_path is not None:
        _cross_check_association(
            association_path,
            identity_head_sha=identity.ci_head_sha,
            identity_run_id=identity.ci_run_id,
            version=identity.version,
            measurement_sha256=identity.measurement_sha256,
            require_fields=identity.association_cross_checked,
        )
    elif identity.association_cross_checked:
        raise AcquireError("receipt claims association_cross_checked without association file")

    # Refuse unexpected supporting files under ci-artifacts.
    allowed_ci = {
        discovered.scriptlet.name,
        discovered.checksum_sidecar.name,
        MEASUREMENT_BASENAME,
        PACKAGE_PROFILE_BASENAME,
        ASSOCIATION_BASENAME,
        "sfx-payload-equivalence.v1",
    }
    for path in ci_artifacts.rglob("*"):
        if path.is_dir():
            if _lstat_is_symlink(path):
                raise AcquireError(f"symlink refused under ci-artifacts: {path.name}")
            continue
        if _lstat_is_symlink(path):
            raise AcquireError(f"symlink refused under ci-artifacts: {path.name}")
        if path.name not in allowed_ci:
            raise AcquireError(f"unexpected ci-artifacts file: {path.name}")

    linux_bundle = linux_dir / identity.linux_bundle_name
    linux_sum = linux_dir / f"{identity.linux_bundle_name}.sha256"
    if not linux_bundle.is_file() or not linux_sum.is_file():
        raise AcquireError("linux artifact file set incomplete")
    if _lstat_is_symlink(linux_bundle) or _lstat_is_symlink(linux_sum):
        raise AcquireError("symlink refused in linux dir")
    if sha256_file(linux_bundle) != identity.linux_bundle_sha256:
        raise AcquireError("acquired Linux bundle digest drifted from receipt")
    if linux_bundle.stat().st_size != identity.linux_bundle_bytes:
        raise AcquireError("linux bundle size drifted from receipt")
    if sha256_file(linux_sum) != identity.linux_checksum_sha256:
        raise AcquireError("linux checksum file digest drifted from receipt")
    linux_sidecar = _parse_sha256sum(
        linux_sum, expected_name=identity.linux_bundle_name
    )
    if linux_sidecar != identity.linux_bundle_sha256:
        raise AcquireError("linux checksum sidecar content disagrees with receipt")
    for path in linux_dir.iterdir():
        if path.name not in {identity.linux_bundle_name, f"{identity.linux_bundle_name}.sha256"}:
            raise AcquireError(f"unexpected linux entry: {path.name}")

    # Revalidate retained extracted-tree digests.
    kindle_tree = trees / "kindle"
    meas_tree = trees / "measurements"
    linux_tree = trees / "linux"
    for path, label in (
        (kindle_tree, "kindle"),
        (meas_tree, "measurements"),
        (linux_tree, "linux"),
    ):
        if not path.is_dir() or _lstat_is_symlink(path):
            raise AcquireError(f"artifact-trees/{label} missing or unsafe")
    if extracted_tree_sha256(kindle_tree) != identity.kindle_artifact.digest_sha256:
        raise AcquireError("kindle extracted-tree digest drifted from receipt")
    if extracted_tree_sha256(meas_tree) != identity.measurements_artifact.digest_sha256:
        raise AcquireError("measurements extracted-tree digest drifted from receipt")
    if extracted_tree_sha256(linux_tree) != identity.linux_artifact.digest_sha256:
        raise AcquireError("linux extracted-tree digest drifted from receipt")
    if extracted_tree_sha256(linux_dir) != identity.linux_artifact.digest_sha256:
        raise AcquireError("linux/ directory digest disagrees with artifact-trees/linux")

    return AcquiredBundle(
        root=root,
        identity=identity,
        ci_artifacts=ci_artifacts,
        linux_dir=linux_dir,
        receipt_path=receipt_path,
        identity_path=identity_path,
    )
