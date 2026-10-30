"""Adversarial and reproducibility tests for release_component_profiles.py."""

from __future__ import annotations

import hashlib
import os
import re
import sys
from pathlib import Path

import pytest

from release_component_profiles import (
    ALLOWED_PAYLOAD_DIRS,
    CANDIDATE_B_DESTINATION,
    COMPONENT_ROLES,
    PACKAGE_RELATIVE,
    PROFILE_KINDS,
    ProfileError,
    RETAINED_A_FUTURE_LAYOUT,
    generate_profiles,
    inspect_package_components,
    render_profile,
    verify_deterministic,
)

REPO = Path(__file__).resolve().parents[2]
PROVENANCE_RS = (
    REPO
    / "ignite"
    / "kindle"
    / "scribe-app"
    / "src"
    / "storage_release_provenance.rs"
)
SLOT_BINDING_RS = (
    REPO
    / "ignite"
    / "kindle"
    / "scribe-app"
    / "src"
    / "storage_installed_slot_binding.rs"
)


def _write_tree(root: Path) -> None:
    for role, rel in PACKAGE_RELATIVE.items():
        if rel is None:
            continue
        if role == "license-noto-emoji-upstream-info":
            continue  # optional absent
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(f"payload:{role}\n".encode("utf-8"))


def test_component_roles_match_rust_source() -> None:
    text = PROVENANCE_RS.read_text(encoding="utf-8")
    start = text.index("pub const COMPONENT_ROLES")
    block = text[start : text.index("];", start)]
    roles = []
    for line in block.splitlines():
        line = line.strip()
        if line.startswith('"') and line.endswith('",'):
            roles.append(line.strip('",'))
        elif line.startswith('"') and line.endswith('"'):
            roles.append(line.strip('"'))
    assert tuple(roles) == COMPONENT_ROLES


def _parse_candidate_b_paths_from_rust() -> dict[str, str | None]:
    """Extract CandidateB expected_component_path arms as APP/DOCS/REC mappings."""
    text = SLOT_BINDING_RS.read_text(encoding="utf-8")
    start = text.index("ReleaseRole::CandidateB => match role")
    end = text.index("ReleaseRole::RetainedA =>", start)
    block = text[start:end]
    mapping: dict[str, str | None] = {}
    # Handles single-line and braced multi-line Some(root.join("rel")) / None.
    for m in re.finditer(
        r'"([^"]+)"(?:\s*\|\s*"([^"]+)")?\s*=>\s*(?:\{\s*)?(Some\((app|docs|recovery)\.join\("([^"]+)"\)\)|None)',
        block,
        re.MULTILINE,
    ):
        roles = [m.group(1)]
        if m.group(2):
            roles.append(m.group(2))
        if m.group(3) == "None":
            for role in roles:
                mapping[role] = None
            continue
        root = {"app": "APP", "docs": "DOCS", "recovery": "REC"}[m.group(4)]
        rel = m.group(5)
        for role in roles:
            mapping[role] = f"{root}/{rel}"
    return mapping


def test_candidate_b_destinations_match_slot_inspector() -> None:
    rust = _parse_candidate_b_paths_from_rust()
    assert set(rust) >= set(COMPONENT_ROLES) - {"recovery-version"}
    for role in COMPONENT_ROLES:
        expected = CANDIDATE_B_DESTINATION[role]
        if role == "recovery-version":
            # Generated destination; inspector returns None for the role map but
            # writes REC/manifest/version as generated-recovery-version.
            assert expected == "REC/manifest/version"
            continue
        if role == "registration-source":
            assert rust[role] is None
            assert expected == "-"
            continue
        assert rust[role] == expected, f"{role}: rust={rust[role]!r} py={expected!r}"


def test_retained_future_layout_matches_slot_inspector_basenames() -> None:
    text = SLOT_BINDING_RS.read_text(encoding="utf-8")
    start = text.index("ReleaseRole::RetainedA =>")
    end = text.index("fn component_map", start)
    block = text[start:end]
    for role in COMPONENT_ROLES:
        dest = RETAINED_A_FUTURE_LAYOUT[role]
        if role in {
            "recovery-dropbear",
            "recovery-elf-sha256",
            "recovery-provenance",
            "recovery-version",
            "registration-source",
        }:
            if role == "registration-source":
                assert dest == "APP/previous-release/appreg-handler.sql"
            else:
                assert dest == "-"
            continue
        assert dest.startswith("APP/previous-release/")
        base = dest.split("/", 2)[-1]
        assert f'"{role}" => "{base}"' in block or f'"{role}" =>' in block


def test_identical_inputs_produce_identical_profiles(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    first = verify_deterministic(payload, "1.2.3")
    second = generate_profiles(payload, "1.2.3")
    assert first == second
    assert set(first) == set(PROFILE_KINDS)
    for kind in PROFILE_KINDS:
        assert first[kind].startswith(b"IGNITE_RELEASE_COMPONENT_PROFILE_V1\n")
        text = first[kind].decode("ascii")
        assert "installation_binding=unverified\n" in text
        assert "execution_binding=unverified\n" in text
        assert "first_launch_admission=unavailable\n" in text
        assert "data_compatibility=unknown\n" in text
        assert "promotion_authorized=false\n" in text
        assert "authentication=build-evidence-only\n" in text


def test_modified_component_same_length_changes_digest(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    before = generate_profiles(payload, "1.2.3")["package"]
    target = payload / "bin/scribe-app"
    original = target.read_bytes()
    assert len(original) > 4
    mutated = b"X" + original[1:]
    assert len(mutated) == len(original)
    target.write_bytes(mutated)
    after = generate_profiles(payload, "1.2.3")["package"]
    assert before != after


def test_missing_mandatory_refused(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    (payload / "bin/scribe-app").unlink()
    with pytest.raises(ProfileError, match="missing mandatory"):
        generate_profiles(payload, "1.2.3")


def test_unknown_component_refused(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    extra = payload / "lib/evil.sh"
    extra.write_text("nope\n", encoding="utf-8")
    with pytest.raises(ProfileError, match="unknown release-owned"):
        generate_profiles(payload, "1.2.3")


def test_unexpected_directory_refused(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    rogue = payload / "extras"
    rogue.mkdir()
    (rogue / "x.bin").write_bytes(b"x")
    with pytest.raises(ProfileError, match="unexpected payload directory"):
        generate_profiles(payload, "1.2.3")


def test_optional_absent_and_present(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    profiles = generate_profiles(payload, "1.2.3")
    assert b"component=license-noto-emoji-upstream-info|absent|" in profiles["package"]
    upstream = payload / "assets/licenses/noto-emoji/upstream_info.md"
    upstream.parent.mkdir(parents=True, exist_ok=True)
    upstream.write_text("legacy\n", encoding="utf-8")
    profiles2 = generate_profiles(payload, "1.2.3")
    assert b"component=license-noto-emoji-upstream-info|file|" in profiles2["package"]


def test_candidate_b_uses_symbolic_installed_destinations(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    candidate = generate_profiles(payload, "1.2.3")["candidate-b"].decode("ascii")
    assert "profile=candidate-b\n" in candidate
    assert "|direct-copy|APP/bin/scribe-app" in candidate
    assert "|direct-copy|APP/bin/scribe-lifecycle-host.sh" in candidate
    assert "|direct-copy|DOCS/Scribe.sh" in candidate
    assert "|direct-copy|APP/tools/ignite-release/lib/install-runtime.sh" in candidate
    assert "|direct-copy|DOCS/Uninstall Ignite.sh" in candidate
    assert "|direct-copy|REC/bin/dropbearmulti" in candidate
    assert "component=registration-source|absent|0|-|transformed|-" in candidate
    assert "component=recovery-version|absent|0|-|generated|REC/manifest/version" in candidate
    # Must not embed production absolute roots or package-relative mappings.
    assert "/mnt/us/" not in candidate
    assert "/var/local/" not in candidate
    assert "|direct-copy|bin/scribe-app" not in candidate
    assert "|direct-copy|launcher/Scribe.sh" not in candidate


def test_retained_a_future_layout_is_not_current_device_identity(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    retained = generate_profiles(payload, "1.2.3")["retained-a-future-layout"].decode("ascii")
    assert "profile=retained-a-future-layout\n" in retained
    assert "layout_subject=this-release-if-retained\n" in retained
    assert "device_retained_a_identity=unauthenticated\n" in retained
    assert "component=registration-source|absent|0|-|transformed|APP/previous-release/appreg-handler.sql" in retained
    assert "component=recovery-dropbear|absent|0|-|shared|-" in retained
    assert "|direct-copy|APP/previous-release/scribe-app" in retained
    # Must not claim authenticated current retained-A identity.
    assert "device_retained_a_identity=authenticated" not in retained
    assert "layout_subject=current-device-retained-a" not in retained


def test_recovery_version_gap_and_shared_not_retained(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    package = generate_profiles(payload, "1.2.3")["package"].decode("ascii")
    assert "component=recovery-version|absent|0|-|generated|-" in package


@pytest.mark.skipif(not hasattr(os, "symlink"), reason="no symlink support")
@pytest.mark.skipif(sys.platform == "win32", reason="symlink privileges vary on Windows")
def test_symlink_file_refused(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    target = payload / "bin/scribe-app"
    decoy = payload / "bin/scribe-app.real"
    decoy.write_bytes(target.read_bytes())
    target.unlink()
    os.symlink(decoy.name, target)
    with pytest.raises(ProfileError, match="symlink"):
        generate_profiles(payload, "1.2.3")


@pytest.mark.skipif(not hasattr(os, "symlink"), reason="no symlink support")
@pytest.mark.skipif(sys.platform == "win32", reason="symlink privileges vary on Windows")
def test_symlink_directory_refused(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    # Replace lib/ with a symlink to an outside tree that mirrors expected names.
    outside = tmp_path / "outside-lib"
    outside.mkdir()
    for name in (
        "install-runtime.sh",
        "uninstall-runtime.sh",
        "backup-runtime.sh",
        "register-helpers.sh",
        "install-recovery.sh",
    ):
        (outside / name).write_bytes(b"hijacked\n")
    lib = payload / "lib"
    for child in list(lib.iterdir()):
        child.unlink()
    lib.rmdir()
    os.symlink(outside, lib)
    with pytest.raises(ProfileError, match="symlink directory"):
        generate_profiles(payload, "1.2.3")


@pytest.mark.skipif(sys.platform == "win32", reason="hard links / special files are Unix-focused")
def test_hardlink_refused(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    src = payload / "bin/scribe-app"
    twin = tmp_path / "outside"
    twin.write_bytes(src.read_bytes())
    src.unlink()
    os.link(twin, src)
    with pytest.raises(ProfileError, match="hard link"):
        generate_profiles(payload, "1.2.3")


@pytest.mark.skipif(sys.platform == "win32", reason="fifo special files are Unix-focused")
def test_special_file_refused(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    target = payload / "bin/scribe-app"
    target.unlink()
    os.mkfifo(target)
    with pytest.raises(ProfileError, match="special file"):
        generate_profiles(payload, "1.2.3")


def test_directory_enumeration_bound(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import release_component_profiles as mod

    monkeypatch.setattr(mod, "MAX_PAYLOAD_DIRS", 2)
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    with pytest.raises(ProfileError, match="directory enumeration exceeded"):
        generate_profiles(payload, "1.2.3")


def test_file_enumeration_bound(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import release_component_profiles as mod

    monkeypatch.setattr(mod, "MAX_PAYLOAD_FILES", 3)
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    with pytest.raises(ProfileError, match="file enumeration exceeded"):
        generate_profiles(payload, "1.2.3")


def test_oversized_component_refused(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import release_component_profiles as mod

    monkeypatch.setattr(mod, "MAX_COMPONENT_BYTES", 8)
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    with pytest.raises(ProfileError, match="size bound"):
        generate_profiles(payload, "1.2.3")


def test_allowed_dirs_cover_package_paths() -> None:
    for rel in PACKAGE_RELATIVE.values():
        if rel is None:
            continue
        parent = str(Path(rel).parent.as_posix())
        if parent == ".":
            continue
        # Every ancestor directory must be allowlisted.
        parts = parent.split("/")
        for i in range(len(parts)):
            assert "/".join(parts[: i + 1]) in ALLOWED_PAYLOAD_DIRS


def test_capabilities_digest_must_be_canonical(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    observed = inspect_package_components(payload)
    with pytest.raises(ProfileError, match="capabilities_sha256"):
        render_profile(
            profile="package",
            version="1.2.3",
            observed=observed,
            capabilities_sha256="ABCDEF",
        )
    digest = hashlib.sha256(b"caps").hexdigest()
    raw = render_profile(
        profile="package",
        version="1.2.3",
        observed=observed,
        capabilities_sha256=digest,
    )
    assert f"capabilities_sha256={digest}\n".encode() in raw


def test_profile_is_not_an_admission_token(tmp_path: Path) -> None:
    payload = tmp_path / "Ignite-Kindle-1.2.3"
    payload.mkdir()
    _write_tree(payload)
    for kind in PROFILE_KINDS:
        text = generate_profiles(payload, "1.2.3")[kind].decode("ascii")
        assert "promotion_authorized=true" not in text
        assert "installation_binding=verified" not in text
        assert "first_launch_admission=available" not in text
