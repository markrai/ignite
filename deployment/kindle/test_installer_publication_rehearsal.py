"""Disposable A→B installer publication rehearsal (Slice 3D-B2a.4.3e).

Exercises real install-runtime.sh under IGNITE_* path overrides. Observational only.
"""

from __future__ import annotations

import os
import shutil
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

from installer_publication_rehearsal import (  # noqa: E402
    ACTIVATION_INTERRUPT_POINTS,
    OWNER_FILE,
    PUBLISH_INTERRUPT_POINTS,
    FixtureRoot,
    RehearsalError,
    assess_pending_publication,
    build_canonical_lkg_inventory,
    build_release_evidence,
    classify_publication_state,
    distinguish_payload_bytes,
    init_fixture_root,
    inspect_pending_health,
    inspect_pending_health_structure,
    observe_candidate_b,
    observe_external_state,
    observe_rehearsal,
    observe_retained_a,
    safe_cleanup_fixture,
    seed_external_sentinels,
)
from release_component_profiles import CANDIDATE_B_DESTINATION, COMPONENT_ROLES  # noqa: E402
from test_install_runtime import (  # noqa: E402
    _assert_persistent_state,
    _run_install,
    _seed_persistent_state,
)
from test_sfx import _require_sh  # noqa: E402
from test_validate_stage import make_payload  # noqa: E402

VERSION_A = "3.3.0"
VERSION_B = "3.3.1"


def _require_posix_installer() -> None:
    try:
        _require_sh()
    except AssertionError as exc:
        pytest.skip(str(exc))
    script = _HERE / "lib" / "install-runtime.sh"
    head = script.read_bytes()[:64]
    if b"\r" in head:
        pytest.skip(
            "install-runtime.sh has CRLF line endings on this filesystem view; "
            "use Git Bash on Windows or a native LF Linux checkout"
        )


def _prepare_ab(tmp_path: Path) -> tuple[Path, Path, object, object]:
    payload_a = make_payload(tmp_path / "src-a", VERSION_A)
    payload_b = make_payload(tmp_path / "src-b", VERSION_B)
    distinguish_payload_bytes(payload_a, b"RELEASE-A")
    distinguish_payload_bytes(payload_b, b"RELEASE-B")
    evidence_a = build_release_evidence(payload_a, VERSION_A)
    evidence_b = build_release_evidence(payload_b, VERSION_B)
    assert evidence_a.digests["scribe-app"][2] != evidence_b.digests["scribe-app"][2]
    return payload_a, payload_b, evidence_a, evidence_b


def _new_kindle(tmp_path: Path) -> tuple[FixtureRoot, Path]:
    owned = init_fixture_root(tmp_path / "kindle")
    return owned, owned.path


def _state_of(
    kindle: Path,
    evidence_a: object,
    evidence_b: object,
    *,
    had_upgrade: bool,
) -> str:
    return classify_publication_state(
        kindle,
        candidate_rows=observe_candidate_b(kindle, evidence_b, evidence_a),
        retained_rows=observe_retained_a(kindle, evidence_a, evidence_b),
        evidence_a=evidence_a,
        evidence_b=evidence_b,
        had_upgrade=had_upgrade,
    )


def test_successful_a_to_b_rehearsal(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    persistent = _seed_persistent_state(kindle)
    sentinels = seed_external_sentinels(kindle)
    for path, content in list(sentinels.items()):
        if path in persistent:
            persistent[path] = content

    first = _run_install(payload_a, kindle)
    assert first.returncode == 0, first.stdout + first.stderr
    report_a = observe_rehearsal(
        kindle, evidence_a, evidence_b, had_upgrade=False, install_rc=0, sentinels=sentinels
    )
    text_a = report_a.decode("ascii")
    assert "promotion_authorized=false" in text_a
    assert "installation_binding=unverified" in text_a
    assert "publication_state=a-active-no-pending" in text_a

    second = _run_install(payload_b, kindle)
    assert second.returncode == 0, second.stdout + second.stderr
    report = observe_rehearsal(
        kindle,
        evidence_a,
        evidence_b,
        had_upgrade=True,
        install_rc=0,
        sentinels=sentinels,
    )
    text = report.decode("ascii")
    assert "publication_state=b-published-with-pending-health" in text
    assert "pending_health=consistent" in text
    assert "pending_health_never_implies_qualification=true" in text
    assert "candidate=scribe-app|direct-copy|file|" in text
    assert "|match-b|APP/bin/scribe-app" in text
    assert "retained=scribe-app|direct-copy|file|" in text
    assert "|match-a|APP/previous-release/scribe-app" in text
    assert "external_error_count=0" in text
    assert (
        inspect_pending_health(kindle, evidence_a=evidence_a, evidence_b=evidence_b)[0]
        == "consistent"
    )
    _assert_persistent_state(persistent)
    assert observe_external_state(sentinels) == []


def test_empty_fixture_reports_no_active_release(tmp_path: Path) -> None:
    _owned, kindle = _new_kindle(tmp_path)
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    state = _state_of(kindle, evidence_a, evidence_b, had_upgrade=False)
    assert state == "no-active-release"
    report = observe_rehearsal(
        kindle, evidence_a, evidence_b, had_upgrade=False, install_rc=None
    )
    assert b"publication_state=no-active-release" in report
    assert b"a-active-no-pending" not in report


def test_wrong_destination_does_not_complete_b(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    correct = kindle / "scribe-app" / "bin" / "scribe-app"
    bytes_b = correct.read_bytes()
    correct.unlink()
    decoy = kindle / "scribe-app" / "bin" / "scribe-app-decoy"
    decoy.write_bytes(bytes_b)
    rows = observe_candidate_b(kindle, evidence_b, evidence_a)
    app = next(r for r in rows if r.role == "scribe-app")
    assert app.presence == "absent"
    assert app.digest_match == "absent"
    state = _state_of(kindle, evidence_a, evidence_b, had_upgrade=True)
    assert state != "b-published-with-pending-health"
    assert state in {
        "b-publication-partial",
        "incomplete-or-inconsistent",
        "b-present-without-pending-health",
    }


def test_ab_identity_separation_and_retention_swap_detection(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    retained = observe_retained_a(kindle, evidence_a, evidence_b)
    assert all(
        row.digest_match in {"match-a", "not-applicable", "absent"}
        for row in retained
        if row.classification == "direct-copy"
    )
    retained_app = kindle / "scribe-app" / "previous-release" / "scribe-app"
    retained_app.write_bytes((kindle / "scribe-app" / "bin" / "scribe-app").read_bytes())
    retained2 = observe_retained_a(kindle, evidence_a, evidence_b)
    app_row = next(row for row in retained2 if row.role == "scribe-app")
    assert app_row.digest_match == "match-b"
    assert _state_of(kindle, evidence_a, evidence_b, had_upgrade=True) != (
        "b-published-with-pending-health"
    )


def test_same_length_mutation_detected(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    target = kindle / "scribe-app" / "bin" / "scribe-app"
    original = target.read_bytes()
    mutated = b"Z" + original[1:]
    assert len(mutated) == len(original)
    target.write_bytes(mutated)
    row = next(
        r for r in observe_candidate_b(kindle, evidence_b, evidence_a) if r.role == "scribe-app"
    )
    assert row.digest_match == "mismatch"
    assert inspect_pending_health(kindle)[0] == "invalid"


def test_missing_mandatory_component_detected(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    (kindle / "scribe-app" / "bin" / "scribe-lifecycle").unlink()
    state = _state_of(kindle, evidence_a, evidence_b, had_upgrade=True)
    assert state != "b-published-with-pending-health"
    assert state in {
        "b-publication-partial",
        "incomplete-or-inconsistent",
        "b-present-without-pending-health",
    }


def test_optional_component_missing_blocks_complete_publication(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    upstream = payload_b / "assets" / "licenses" / "noto-emoji" / "upstream_info.md"
    upstream.parent.mkdir(parents=True, exist_ok=True)
    upstream.write_text("optional-b\n", encoding="utf-8")
    evidence_b = build_release_evidence(payload_b, VERSION_B)
    assert evidence_b.digests["license-noto-emoji-upstream-info"][0] == "file"
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    installed = (
        kindle
        / "scribe-app"
        / "assets"
        / "licenses"
        / "noto-emoji"
        / "upstream_info.md"
    )
    assert installed.is_file()
    installed.unlink()
    state = _state_of(kindle, evidence_a, evidence_b, had_upgrade=True)
    assert state != "b-published-with-pending-health"
    row = next(
        r
        for r in observe_candidate_b(kindle, evidence_b, evidence_a)
        if r.role == "license-noto-emoji-upstream-info"
    )
    assert row.presence == "absent"


def test_optional_component_present_matches_b(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    upstream = payload_b / "assets" / "licenses" / "noto-emoji" / "upstream_info.md"
    upstream.parent.mkdir(parents=True, exist_ok=True)
    upstream.write_text("optional-b\n", encoding="utf-8")
    evidence_b = build_release_evidence(payload_b, VERSION_B)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    row = next(
        r
        for r in observe_candidate_b(kindle, evidence_b, evidence_a)
        if r.role == "license-noto-emoji-upstream-info"
    )
    assert row.presence == "file"
    assert row.digest_match == "match-b"
    assert _state_of(kindle, evidence_a, evidence_b, had_upgrade=True) == (
        "b-published-with-pending-health"
    )


@pytest.mark.parametrize("point", PUBLISH_INTERRUPT_POINTS)
def test_publish_interrupt_observation(tmp_path: Path, point: str) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    sentinels = seed_external_sentinels(kindle)
    assert _run_install(payload_a, kindle).returncode == 0
    crashed = _run_install(payload_b, kindle, {"IGNITE_FAIL_PUBLISH": point})
    assert crashed.returncode != 0
    report = observe_rehearsal(
        kindle,
        evidence_a,
        evidence_b,
        had_upgrade=True,
        install_rc=crashed.returncode,
        interrupt_point=point,
        sentinels=sentinels,
    )
    text = report.decode("ascii")
    assert f"interrupt_point={point}" in text
    assert "promotion_authorized=false" in text
    assert "external_error_count=0" in text
    state = _state_of(kindle, evidence_a, evidence_b, had_upgrade=True)
    # Late points may leave a valid pending-health record before commit cleanup.
    if point in {
        "after-pending-health-publish",
        "before-commit",
        "after-commit",
        "before-journal-cleanup",
        "before-prev-cleanup",
    }:
        assert state in {
            "b-published-with-pending-health",
            "b-publication-partial",
            "incomplete-or-inconsistent",
            "a-retention-incomplete",
            "b-staging-present",
            "b-present-without-pending-health",
        }
    else:
        pending = inspect_pending_health(
            kindle, evidence_a=evidence_a, evidence_b=evidence_b
        )[0]
        assert state != "b-published-with-pending-health" or pending == "consistent"
        if pending != "consistent":
            assert state != "b-published-with-pending-health"
    assert observe_external_state(sentinels) == []
    report2 = observe_rehearsal(
        kindle,
        evidence_a,
        evidence_b,
        had_upgrade=True,
        install_rc=crashed.returncode,
        interrupt_point=point,
        sentinels=sentinels,
    )
    assert report2 == report


@pytest.mark.parametrize("point", ("scribe-app", "host", "diagnose-docs"))
def test_activation_interrupt_observation(tmp_path: Path, point: str) -> None:
    _require_posix_installer()
    assert point in ACTIVATION_INTERRUPT_POINTS
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    sentinels = seed_external_sentinels(kindle)
    assert _run_install(payload_a, kindle).returncode == 0
    crashed = _run_install(payload_b, kindle, {"IGNITE_FAIL_AT": point})
    assert crashed.returncode != 0
    report = observe_rehearsal(
        kindle,
        evidence_a,
        evidence_b,
        had_upgrade=True,
        install_rc=crashed.returncode,
        interrupt_point=f"activate:{point}",
        sentinels=sentinels,
    )
    text = report.decode("ascii")
    assert "external_error_count=0" in text
    assert "promotion_authorized=false" in text
    assert _state_of(kindle, evidence_a, evidence_b, had_upgrade=True) != (
        "b-published-with-pending-health"
    )


def test_stale_staging_and_incomplete_retention_states(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    staging = kindle / "scribe-app" / "previous-release.staging"
    staging.mkdir()
    (staging / "scribe-app").write_bytes(b"partial\n")
    assert _state_of(kindle, evidence_a, evidence_b, had_upgrade=True) == (
        "a-retention-incomplete"
    )
    for child in list(staging.iterdir()):
        child.unlink()
    staging.rmdir()
    (kindle / "scribe-app" / "bin" / "scribe-app.new").write_bytes(b"staged\n")
    assert _state_of(kindle, evidence_a, evidence_b, had_upgrade=True) == (
        "b-staging-present"
    )


def test_corrupted_pending_health_is_incomplete(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    assert (
        inspect_pending_health(kindle, evidence_a=evidence_a, evidence_b=evidence_b)[0]
        == "consistent"
    )
    pending = kindle / "scribe-app" / "state" / "update" / "pending-health.v1"
    pending.write_text("CORRUPT\n", encoding="ascii")
    assert inspect_pending_health(kindle)[0] == "invalid"
    state = _state_of(kindle, evidence_a, evidence_b, had_upgrade=True)
    assert state == "incomplete-or-inconsistent"
    report = observe_rehearsal(
        kindle, evidence_a, evidence_b, had_upgrade=True, install_rc=0
    )
    text = report.decode("ascii")
    assert "publication_state=incomplete-or-inconsistent" in text
    assert "pending_health=invalid" in text
    assert "b-published-with-pending-health" not in text


def test_retry_after_interrupted_install(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    sentinels = seed_external_sentinels(kindle)
    assert _run_install(payload_a, kindle).returncode == 0
    crashed = _run_install(
        payload_b, kindle, {"IGNITE_FAIL_PUBLISH": "before-pending-health-publish"}
    )
    assert crashed.returncode != 0
    assert (
        inspect_pending_health(kindle, evidence_a=evidence_a, evidence_b=evidence_b)[0]
        != "consistent"
    )
    pre = _state_of(kindle, evidence_a, evidence_b, had_upgrade=True)
    assert pre != "b-published-with-pending-health"

    retry = _run_install(payload_b, kindle)
    report = observe_rehearsal(
        kindle,
        evidence_a,
        evidence_b,
        had_upgrade=True,
        install_rc=retry.returncode,
        interrupt_point="retry-after-before-pending-health-publish",
        sentinels=sentinels,
    )
    text = report.decode("ascii")
    assert "promotion_authorized=false" in text
    assert observe_external_state(sentinels) == []
    state = _state_of(kindle, evidence_a, evidence_b, had_upgrade=True)
    pending = inspect_pending_health(
        kindle, evidence_a=evidence_a, evidence_b=evidence_b
    )[0]
    if retry.returncode == 0:
        # Clean retry completed publication; still non-qualifying.
        assert state == "b-published-with-pending-health"
        assert pending == "consistent"
        assert "pending_health_never_implies_qualification=true" in text
    else:
        # Fail-closed: unresolved/incomplete tree, never falsely complete.
        assert pending != "consistent"
        assert state != "b-published-with-pending-health"
        assert state in {
            "b-publication-partial",
            "incomplete-or-inconsistent",
            "a-retention-incomplete",
            "b-staging-present",
            "b-present-without-pending-health",
            "a-active-no-pending",
        }


@pytest.mark.skipif(sys.platform == "win32", reason="symlink fixture checks on Unix")
def test_symlink_substitution_refused_by_observer(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    target = kindle / "scribe-app" / "bin" / "scribe-app"
    outside = tmp_path / "outside-app"
    outside.write_bytes(target.read_bytes())
    target.unlink()
    os.symlink(outside, target)
    with pytest.raises(RehearsalError, match="symlink"):
        observe_candidate_b(kindle, evidence_b, evidence_a)


def test_fixture_root_copied_marker_never_authorizes_cleanup(tmp_path: Path) -> None:
    owned, kindle = _new_kindle(tmp_path)
    (kindle / "keep-me.txt").write_text("owned\n", encoding="ascii")
    substitute = tmp_path / "substituted-root"
    substitute.mkdir()
    shutil.copy2(kindle / OWNER_FILE, substitute / OWNER_FILE)
    precious = substitute / "precious.txt"
    precious.write_text("must-survive\n", encoding="ascii")
    subst_st = os.lstat(os.fspath(substitute))

    # Copied marker + stolen create-time identity still fails: live substitute
    # identity does not match the create-time device/inode.
    forged_stolen_identity = FixtureRoot(
        path=substitute,
        token=owned.token,
        device=owned.device,
        inode=owned.inode,
    )
    with pytest.raises(RehearsalError, match="identity|token|marker"):
        safe_cleanup_fixture(forged_stolen_identity)
    assert precious.is_file()
    assert precious.read_text(encoding="ascii") == "must-survive\n"

    # Copied marker + forged FixtureRoot using the substitute's own identity also
    # fails: marker still encodes the original root's device/inode.
    forged_live_identity = FixtureRoot(
        path=substitute,
        token=owned.token,
        device=subst_st.st_dev,
        inode=subst_st.st_ino,
    )
    with pytest.raises(RehearsalError, match="identity|marker|bind"):
        safe_cleanup_fixture(forged_live_identity)
    assert precious.is_file()
    assert precious.read_text(encoding="ascii") == "must-survive\n"

    # Tampering the real marker also blocks cleanup of the true root.
    (kindle / OWNER_FILE).write_text("tampered\n", encoding="ascii")
    with pytest.raises(RehearsalError, match="token|marker"):
        safe_cleanup_fixture(owned)
    assert (kindle / "keep-me.txt").is_file()


def test_shared_recovery_not_counted_as_retained_a(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    retained = observe_retained_a(kindle, evidence_a, evidence_b)
    for role in (
        "recovery-dropbear",
        "recovery-elf-sha256",
        "recovery-provenance",
        "recovery-version",
    ):
        row = next(r for r in retained if r.role == role)
        assert row.classification == "shared"
        assert row.digest_match == "not-applicable"


def test_generated_recovery_version_not_package_direct_copy(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    cand = observe_candidate_b(kindle, evidence_b, evidence_a)
    row = next(r for r in cand if r.role == "recovery-version")
    assert row.classification == "generated"
    assert row.digest_match == "not-applicable"
    assert evidence_a.digests["recovery-version"][0] == "absent"
    assert evidence_b.digests["recovery-version"][0] == "absent"
    assert CANDIDATE_B_DESTINATION["recovery-version"] == "REC/manifest/version"
    assert set(COMPONENT_ROLES)


def _rewrite_pending_health(kindle: Path, **overrides: str) -> None:
    status, fields = inspect_pending_health_structure(kindle)
    assert status == "structurally-valid" and fields is not None
    merged = dict(fields)
    merged.update(overrides)
    text = (
        "\n".join(
            [
                "IGNITE_PENDING_HEALTH_V1",
                "schema=1",
                "status=pending-health",
                f"candidate_version={merged['candidate_version']}",
                f"candidate_txn={merged['candidate_txn']}",
                f"candidate_app_bytes={merged['candidate_app_bytes']}",
                f"candidate_app_sha256={merged['candidate_app_sha256']}",
                f"candidate_lifecycle_bytes={merged['candidate_lifecycle_bytes']}",
                f"candidate_lifecycle_sha256={merged['candidate_lifecycle_sha256']}",
                "retained_version=unknown",
                f"retained_release_identity=sha256:{merged['retained_release_identity']}",
                f"lkg_inventory_bytes={merged['lkg_inventory_bytes']}",
                f"lkg_inventory_sha256={merged['lkg_inventory_sha256']}",
                "data_compatibility=unknown",
            ]
        )
        + "\n"
    )
    path = kindle / "scribe-app" / "state" / "update" / "pending-health.v1"
    # Avoid Windows write_text newline translation (CRLF breaks the installer contract).
    path.write_bytes(text.encode("ascii"))


def test_structurally_valid_pending_missing_lkg_is_incomplete(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    inv = kindle / "scribe-app" / "previous-release" / "ignite-lkg-inventory.v1"
    assert inv.is_file()
    inv.unlink()
    status, fields = inspect_pending_health_structure(kindle)
    assert status == "structurally-valid"
    assert fields is not None
    ok, reasons = assess_pending_publication(
        kindle, fields, evidence_a=evidence_a, evidence_b=evidence_b
    )
    assert not ok
    assert any(r.startswith("lkg-inventory-missing") for r in reasons)
    assert _state_of(kindle, evidence_a, evidence_b, had_upgrade=True) == (
        "incomplete-or-inconsistent"
    )


def test_lkg_inventory_same_size_mutation_is_incomplete(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    inv = kindle / "scribe-app" / "previous-release" / "ignite-lkg-inventory.v1"
    original = inv.read_bytes()
    mutated = b"X" + original[1:]
    assert len(mutated) == len(original)
    inv.write_bytes(mutated)
    status, fields = inspect_pending_health_structure(kindle)
    assert status == "structurally-valid" and fields is not None
    ok, reasons = assess_pending_publication(
        kindle, fields, evidence_a=evidence_a, evidence_b=evidence_b
    )
    assert not ok
    assert "lkg-inventory-sha256-mismatch" in reasons
    assert _state_of(kindle, evidence_a, evidence_b, had_upgrade=True) == (
        "incomplete-or-inconsistent"
    )


def test_candidate_version_mismatch_blocks_complete_publication(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    _rewrite_pending_health(kindle, candidate_version="9.9.9")
    status, fields = inspect_pending_health_structure(kindle)
    assert status == "structurally-valid" and fields is not None
    ok, reasons = assess_pending_publication(
        kindle, fields, evidence_a=evidence_a, evidence_b=evidence_b
    )
    assert not ok
    assert "candidate-version-mismatch" in reasons
    assert _state_of(kindle, evidence_a, evidence_b, had_upgrade=True) == (
        "incomplete-or-inconsistent"
    )
    report = observe_rehearsal(
        kindle, evidence_a, evidence_b, had_upgrade=True, install_rc=0
    )
    assert b"b-published-with-pending-health" not in report
    assert b"pending_publication_gap=candidate-version-mismatch" in report


def test_journal_txn_mismatch_blocks_complete_publication(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    journal = kindle / "scribe-app" / "run" / "ignite-activate.journal"
    journal.write_bytes(
        b"# ignite-activate v2\n# txn=19990101T000000Z-1\n# phase=pending\n"
    )
    status, fields = inspect_pending_health_structure(kindle)
    assert status == "structurally-valid" and fields is not None
    ok, reasons = assess_pending_publication(
        kindle, fields, evidence_a=evidence_a, evidence_b=evidence_b
    )
    assert not ok
    assert "inventory-transaction-relationship-mismatch" in reasons
    # Journal presence also surfaces as partial before consistency claims.
    assert _state_of(kindle, evidence_a, evidence_b, had_upgrade=True) == (
        "b-publication-partial"
    )


def test_retained_a_inventory_mismatch_blocks_complete_publication(
    tmp_path: Path,
) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    retained_app = kindle / "scribe-app" / "previous-release" / "scribe-app"
    retained_app.write_bytes((kindle / "scribe-app" / "bin" / "scribe-app").read_bytes())
    # Rebuild inventory to match mutated previous-release so LKG self-check passes,
    # then retarget pending-health digests at the new inventory identity.
    new_inv = build_canonical_lkg_inventory(kindle)
    inv_path = kindle / "scribe-app" / "previous-release" / "ignite-lkg-inventory.v1"
    inv_path.write_bytes(new_inv)
    inv_sha = __import__("hashlib").sha256(new_inv).hexdigest()
    _rewrite_pending_health(
        kindle,
        retained_release_identity=inv_sha,
        lkg_inventory_bytes=str(len(new_inv)),
        lkg_inventory_sha256=inv_sha,
    )
    status, fields = inspect_pending_health_structure(kindle)
    assert status == "structurally-valid" and fields is not None
    ok, reasons = assess_pending_publication(
        kindle, fields, evidence_a=evidence_a, evidence_b=evidence_b
    )
    assert not ok
    assert "retained-a-mismatch:scribe-app" in reasons
    assert _state_of(kindle, evidence_a, evidence_b, had_upgrade=True) != (
        "b-published-with-pending-health"
    )


@pytest.mark.skipif(sys.platform == "win32", reason="Unix execute bits not reliable on Windows")
def test_scribe_app_without_execute_blocks_complete_publication(tmp_path: Path) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    target = kindle / "scribe-app" / "bin" / "scribe-app"
    target.chmod(0o644)
    row = next(
        r for r in observe_candidate_b(kindle, evidence_b, evidence_a) if r.role == "scribe-app"
    )
    assert row.exec_state == "missing"
    assert _state_of(kindle, evidence_a, evidence_b, had_upgrade=True) != (
        "b-published-with-pending-health"
    )


@pytest.mark.skipif(sys.platform == "win32", reason="Unix execute bits not reliable on Windows")
def test_scribe_lifecycle_without_execute_blocks_complete_publication(
    tmp_path: Path,
) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    target = kindle / "scribe-app" / "bin" / "scribe-lifecycle"
    target.chmod(0o644)
    row = next(
        r
        for r in observe_candidate_b(kindle, evidence_b, evidence_a)
        if r.role == "scribe-lifecycle"
    )
    assert row.exec_state == "missing"
    assert _state_of(kindle, evidence_a, evidence_b, had_upgrade=True) != (
        "b-published-with-pending-health"
    )


def test_installer_owned_new_outside_original_four_blocks_complete(
    tmp_path: Path,
) -> None:
    _require_posix_installer()
    payload_a, payload_b, evidence_a, evidence_b = _prepare_ab(tmp_path)
    _owned, kindle = _new_kindle(tmp_path)
    assert _run_install(payload_a, kindle).returncode == 0
    assert _run_install(payload_b, kindle).returncode == 0
    leftover = (
        kindle
        / "scribe-app"
        / "tools"
        / "ignite-release"
        / "lib"
        / "install-runtime.sh.new"
    )
    leftover.write_bytes(b"leftover-staging\n")
    assert _state_of(kindle, evidence_a, evidence_b, had_upgrade=True) == (
        "b-staging-present"
    )
    report = observe_rehearsal(
        kindle, evidence_a, evidence_b, had_upgrade=True, install_rc=0
    )
    assert b"publication_state=b-staging-present" in report
    assert b"b-published-with-pending-health" not in report
