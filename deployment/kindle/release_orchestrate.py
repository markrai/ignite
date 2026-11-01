"""Connect acquisition → offline signing → local promote handoff (Option A).

Local orchestration supplies ceremony ``promote-authenticated-assets/`` to
``promote-release.sh`` via ``IGNITE_PROMOTE_AUTHENTICATED_ASSETS_DIR``.

Never copies private keys. Never publishes unless the operator separately
invokes promote ``publish``/``all``. Dry-run stops at verify / promote-gate.

Dry-run outcomes are explicit: complete verification (provisioned pass or
expected UNPROVISIONED pin-boundary rehearsal) is distinct from partial,
failed, or not-performed verification.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parents[1]
_RELEASE = _REPO / "tools" / "release"
for extra in (_HERE, str(_RELEASE)):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from authenticated_publication_preflight import (  # noqa: E402
    PreflightError,
    TrustContext,
    run_preflight,
    run_promote_gate,
)
from kindle_release_sign import UNPROVISIONED  # noqa: E402
from offline_release_ceremony import (  # noqa: E402
    CeremonyError,
    PROMOTE_ASSETS_SUBDIR,
    sign_ceremony,
)
from production_trust_boundary import (  # noqa: E402
    ProductionTrustError,
    parse_release_signature,
)
from release_acquire import AcquiredBundle, AcquireError, load_acquired_bundle  # noqa: E402
from release_identity import EXPECTED_WORKFLOW, ReleaseIdentity  # noqa: E402

_PROMOTE_SH = _RELEASE / "promote-release.sh"
_PRODUCTION_PIN = _RELEASE / "kindle-release-public-key.hex"
_HEX64 = re.compile(r"^[0-9a-f]{64}$")
_HEX128 = re.compile(r"^[0-9a-f]{128}$")


class OrchestrateError(ValueError):
    """Fail-closed release orchestration refusal."""


@dataclass(frozen=True)
class SignedBundle:
    root: Path
    promote_assets: Path
    receipt: Path
    scriptlet: Path
    release_manifest: Path
    release_signature: Path


def sign_acquired(
    *,
    acquired: Path,
    private_key: Path,
    public_key: Path,
    output_dir: Path,
) -> SignedBundle:
    """Sign using exact verified acquisition inputs (no network)."""
    try:
        bundle = load_acquired_bundle(acquired)
    except AcquireError as exc:
        raise OrchestrateError(str(exc)) from exc
    identity = bundle.identity
    try:
        written = sign_ceremony(
            tag=identity.tag,
            ci_artifacts=bundle.ci_artifacts,
            private_key=private_key,
            public_key_path=public_key,
            output_dir=output_dir,
            expected_source_revision=identity.ci_head_sha,
        )
    except CeremonyError as exc:
        raise OrchestrateError(str(exc)) from exc

    from release_identity import sha256_file

    scriptlet = written["scriptlet"]
    if sha256_file(scriptlet) != identity.scriptlet_sha256:
        raise OrchestrateError(
            "signed Scriptlet digest does not match acquisition receipt"
        )
    promote = written["promote_assets_dir"]
    if not promote.is_dir():
        raise OrchestrateError("signed output missing promote-authenticated-assets/")
    version = identity.version
    return SignedBundle(
        root=output_dir,
        promote_assets=promote,
        receipt=written["receipt"],
        scriptlet=scriptlet,
        release_manifest=output_dir / f"ignite-kindle-v{version}.manifest",
        release_signature=output_dir / f"ignite-kindle-v{version}.manifest.sig",
    )


def _release_signature_hex(sig_path: Path) -> str:
    try:
        raw = parse_release_signature(sig_path.read_bytes())
    except (ProductionTrustError, OSError) as exc:
        raise OrchestrateError(f"release signature refused: {exc}") from exc
    token = raw.hex()
    if not _HEX128.fullmatch(token):
        raise OrchestrateError("release signature hex malformed")
    return token


def match_signed_to_acquired(
    *, signed: Path, identity: ReleaseIdentity
) -> SignedBundle:
    """Refuse stale or substituted signed metadata for another release identity."""
    version = identity.version
    scriptlet = signed / f"Install Ignite v{version}.sh"
    manifest = signed / f"ignite-kindle-v{version}.manifest"
    manifest_sig = signed / f"ignite-kindle-v{version}.manifest.sig"
    promote = signed / PROMOTE_ASSETS_SUBDIR
    receipt = signed / f"ignite-kindle-v{version}.signing-receipt.v1"
    for path, label in (
        (scriptlet, "scriptlet"),
        (manifest, "manifest"),
        (manifest_sig, "manifest.sig"),
        (promote, "promote-authenticated-assets"),
        (receipt, "signing receipt"),
    ):
        if label == "promote-authenticated-assets":
            if not path.is_dir():
                raise OrchestrateError(f"missing {label}")
        elif not path.is_file():
            raise OrchestrateError(f"missing signed {label}")

    from release_identity import sha256_file

    if sha256_file(scriptlet) != identity.scriptlet_sha256:
        raise OrchestrateError(
            "signed Scriptlet does not match acquisition release identity"
        )
    if scriptlet.stat().st_size != identity.scriptlet_bytes:
        raise OrchestrateError("signed Scriptlet size does not match acquisition")

    for name in (
        f"ignite-kindle-v{version}.publication-evidence",
        f"ignite-kindle-v{version}.publication-binding",
        f"ignite-kindle-v{version}.publication-binding.sig",
    ):
        asset = promote / name
        if not asset.is_file():
            raise OrchestrateError(f"missing authenticated asset: {name}")

    return SignedBundle(
        root=signed,
        promote_assets=promote,
        receipt=receipt,
        scriptlet=scriptlet,
        release_manifest=manifest,
        release_signature=manifest_sig,
    )


def stage_nine_file_candidate(
    *,
    acquired: AcquiredBundle,
    signed: SignedBundle,
    dest: Path,
) -> Path:
    """Assemble the nine publication assets into ``dest`` (must be unused)."""
    if dest.exists():
        raise OrchestrateError("nine-file destination must be unused")
    dest.mkdir(parents=True, exist_ok=False)
    identity = acquired.identity
    version = identity.version
    checksum_src = acquired.ci_artifacts / f"Install Ignite v{version}.sha256"
    if not checksum_src.is_file():
        matches = list(acquired.ci_artifacts.rglob(f"Install Ignite v{version}.sha256"))
        if len(matches) != 1:
            raise OrchestrateError("cannot locate Scriptlet checksum for nine-file stage")
        checksum_src = matches[0]
    mapping = {
        identity.scriptlet_name: signed.scriptlet,
        f"Install Ignite v{version}.sha256": checksum_src,
        f"ignite-kindle-v{version}.manifest": signed.release_manifest,
        f"ignite-kindle-v{version}.manifest.sig": signed.release_signature,
        f"ignite-kindle-v{version}.publication-evidence": (
            signed.promote_assets / f"ignite-kindle-v{version}.publication-evidence"
        ),
        f"ignite-kindle-v{version}.publication-binding": (
            signed.promote_assets / f"ignite-kindle-v{version}.publication-binding"
        ),
        f"ignite-kindle-v{version}.publication-binding.sig": (
            signed.promote_assets / f"ignite-kindle-v{version}.publication-binding.sig"
        ),
        identity.linux_bundle_name: acquired.linux_dir / identity.linux_bundle_name,
        f"{identity.linux_bundle_name}.sha256": (
            acquired.linux_dir / f"{identity.linux_bundle_name}.sha256"
        ),
    }
    for name, src in mapping.items():
        if not src.is_file():
            raise OrchestrateError(f"missing source for {name}: {src}")
        shutil.copy2(src, dest / name)

    expected = sorted(mapping)
    have = sorted(p.name for p in dest.iterdir() if p.is_file())
    if have != expected:
        raise OrchestrateError(f"nine-file stage mismatch: {have}")
    return dest


def promote_env_from_bundles(
    *,
    acquired: AcquiredBundle,
    signed: SignedBundle,
    work_dir: Path,
    confirm: str | None = None,
    public_key_file: Path | None = None,
) -> dict[str, str]:
    """Build env for ``promote-release.sh`` (Option A local handoff).

    Alternate public-key override is set only when explicitly requested for a
    nonpublishing dry-run rehearsal. Inherited ``IGNITE_PROMOTE_PUBLIC_KEY_FILE``
    is stripped otherwise so it cannot silently affect publication or verify.
    """
    identity = acquired.identity
    sig_hex = _release_signature_hex(signed.release_signature)
    if not _HEX64.fullmatch(identity.scriptlet_sha256):
        raise OrchestrateError("kindle sha malformed")
    if not _HEX64.fullmatch(identity.linux_bundle_sha256):
        raise OrchestrateError("linux sha malformed")
    tag = identity.tag
    run_id = str(identity.ci_run_id)
    conf = confirm or f"publish {tag} from run {run_id}"
    env = os.environ.copy()
    # Never inherit an alternate-key override unless this call opts in explicitly.
    env.pop("IGNITE_PROMOTE_PUBLIC_KEY_FILE", None)
    env.update(
        {
            "GITHUB_REPOSITORY": identity.repository,
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_REF": "refs/heads/main",
            "IGNITE_PROMOTE_TAG": tag,
            "IGNITE_PROMOTE_RUN_ID": run_id,
            "IGNITE_PROMOTE_COMMIT": identity.ci_head_sha,
            "IGNITE_PROMOTE_CONFIRM": conf,
            "IGNITE_PROMOTE_KINDLE_SHA256": identity.scriptlet_sha256,
            "IGNITE_PROMOTE_LINUX_SHA256": identity.linux_bundle_sha256,
            "IGNITE_PROMOTE_KINDLE_SIGNATURE": sig_hex,
            # Forward slashes: Git Bash + sha256sum mishandle backslash Windows paths.
            "IGNITE_PROMOTE_AUTHENTICATED_ASSETS_DIR": _bash_friendly_path(
                signed.promote_assets
            ),
            "IGNITE_PROMOTE_WORK": _bash_friendly_path(work_dir),
        }
    )
    if public_key_file is not None:
        env["IGNITE_PROMOTE_PUBLIC_KEY_FILE"] = _bash_friendly_path(public_key_file)
    return env


def _pin_is_unprovisioned(pin_path: Path | None = None) -> bool:
    """True only for the literal UNPROVISIONED sentinel; missing pins are errors.

    Resolves ``_PRODUCTION_PIN`` at call time so test monkeypatches take effect
    after the repository pin is provisioned.
    """
    path = _PRODUCTION_PIN if pin_path is None else pin_path
    try:
        return path.read_bytes() == UNPROVISIONED
    except OSError as exc:
        raise OrchestrateError(
            f"cannot read production public-key pin: {exc}"
        ) from exc


def classify_promote_gate_error(exc: BaseException) -> str:
    msg = str(exc).lower()
    if "unprovisioned" in msg:
        return "official-pin-unprovisioned"
    if "signature" in msg or "verification failed" in msg:
        return "signature-or-verification-failure"
    if "missing" in msg:
        return "missing-asset"
    if "malformed" in msg or "refused" in msg or "inconsistent" in msg:
        return "malformed-or-inconsistent-candidate"
    return "promote-gate-failure"


class DryRunOutcome(str, Enum):
    """Stable machine-readable dry-run completion status."""

    COMPLETE_PROVISIONED_PASS = "complete-verification-pass"
    COMPLETE_UNPROVISIONED_BOUNDARY = "complete-rehearsal-unprovisioned-boundary"
    PARTIAL = "partial-verification"
    FAILED = "verification-failed"
    NOT_PERFORMED = "verification-not-performed"


# Exit status mapping for ignite_release.py dry-run.
DRY_RUN_EXIT: dict[DryRunOutcome, int] = {
    DryRunOutcome.COMPLETE_PROVISIONED_PASS: 0,
    DryRunOutcome.COMPLETE_UNPROVISIONED_BOUNDARY: 0,
    DryRunOutcome.PARTIAL: 2,
    DryRunOutcome.FAILED: 1,
    DryRunOutcome.NOT_PERFORMED: 3,
}

_SHELL_PHASE_MARKERS = (
    "promote-phase=guard-ok",
    "promote-phase=resolve-ok",
    "promote-phase=fetch-ok",
    "promote-phase=verify",
)
_SHELL_PIN_REFUSAL = "promote-shell-refusal=official-pin-unprovisioned"


def classify_promote_shell_output(
    *,
    combined: str,
    returncode: int,
    pin_unprovisioned: bool,
) -> tuple[str, str | None]:
    """Classify promote-release.sh dry-run output using phase markers only.

    A generic occurrence of the word ``unprovisioned`` is never treated as
    completion of the official publisher-pin boundary.
    """
    missing = [m for m in _SHELL_PHASE_MARKERS if m not in combined]
    if missing:
        return "shell-incomplete-phases", missing[0]
    if pin_unprovisioned:
        if returncode == 0:
            return "shell-unexpected-pass-unprovisioned", None
        if _SHELL_PIN_REFUSAL in combined:
            return "official-pin-unprovisioned", None
        if "promote-shell-refusal=other" in combined:
            return "shell-verify-other-refusal", None
        return "shell-verify-unclassified-refusal", None
    if returncode != 0:
        return "shell-failed-provisioned", None
    if "promote-phase=verify-ok" not in combined:
        return "shell-incomplete-phases", "promote-phase=verify-ok"
    return "shell-pass", None


@dataclass
class DryRunReport:
    lines: list[str]
    outcome: DryRunOutcome
    production_gate_refused: bool
    production_gate_refusal_category: str | None
    isolated_crypto_ok: bool
    promote_shell_executed: bool
    promote_shell_ok: bool
    nine_file_dir: Path | None
    pin_unprovisioned: bool
    extras: dict[str, str] = field(default_factory=dict)

    @property
    def exit_status(self) -> int:
        return DRY_RUN_EXIT[self.outcome]


def run_promote_dry_run_shell(
    *,
    acquired: AcquiredBundle,
    signed: SignedBundle,
    work_dir: Path,
    public_key_file: Path | None = None,
    env_extra: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Invoke ``promote-release.sh dry-run`` (guard→resolve→fetch→verify). Never publish."""
    if work_dir.exists():
        raise OrchestrateError("promote work directory must be unused")
    work_dir.mkdir(parents=True, exist_ok=False)
    env = promote_env_from_bundles(
        acquired=acquired,
        signed=signed,
        work_dir=work_dir,
        public_key_file=public_key_file,
    )
    if env_extra:
        env.update(env_extra)
    bash = _resolve_bash()
    if bash is None:
        raise OrchestrateError("bash unavailable for promote-release.sh dry-run")
    # Prefer Git's tar/sha256sum over Windows System32 tar.exe (status 128 / path bugs).
    if os.name == "nt":
        git_bins = [
            r"C:\Program Files\Git\usr\bin",
            r"C:\Program Files\Git\bin",
            r"C:\Program Files (x86)\Git\usr\bin",
            r"C:\Program Files (x86)\Git\bin",
        ]
        existing = [p for p in git_bins if Path(p).is_dir()]
        if existing:
            env["PATH"] = os.pathsep.join([*existing, env.get("PATH", "")])
    # Git Bash needs a mixed PATH; prefer forward-slash script path for argv.
    script = os.fspath(_PROMOTE_SH)
    if os.name == "nt":
        script = script.replace("\\", "/")
    return subprocess.run(
        [bash, script, "dry-run"],
        check=False,
        capture_output=True,
        text=True,
        env=env,
    )


def _resolve_bash() -> str | None:
    """Prefer Git Bash on Windows; never use WSL bash for promote-release.sh."""
    if os.name == "nt":
        for candidate in (
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files\Git\usr\bin\bash.exe",
            r"C:\Program Files (x86)\Git\bin\bash.exe",
        ):
            if Path(candidate).is_file():
                return candidate
        # Reject WSL's bash.exe — it cannot see Windows repo paths reliably.
        return None
    return shutil.which("bash")


def _bash_friendly_path(path: Path | str) -> str:
    """Forward-slash absolute path for Git Bash (avoids sha256sum/MSVC path bugs)."""
    return os.fspath(Path(path).resolve()).replace("\\", "/")


def dry_run_release(
    *,
    tag: str,
    run_id: int,
    signed_assets: Path,
    work_dir: Path,
    acquired_dir: Path | None = None,
    public_key: Path | None = None,
    gh=None,
    run_promote_shell: bool = True,
    promote_env_extra: dict[str, str] | None = None,
) -> DryRunReport:
    """Nonpublishing end-to-end validation including real promote-release.sh dry-run.

    Complete success requires the real shell path to execute guard→resolve→fetch
    and reach publication verification. With an UNPROVISIONED official pin, the
    expected complete rehearsal ends at the official publisher-pin boundary
    (``promote-shell-refusal=official-pin-unprovisioned``), not a generic
    substring match on ``unprovisioned``.
    """
    from release_acquire import acquire_release

    pin_unprovisioned = _pin_is_unprovisioned()
    lines: list[str] = [
        "IGNITE_RELEASE_DRY_RUN_V1",
        "schema=1",
        f"tag={tag}",
        f"run_id={run_id}",
        f"repository_pin_unprovisioned={'true' if pin_unprovisioned else 'false'}",
        "github_mutation=false",
        "scriptlet_executed=false",
        "publish_invoked=false",
    ]
    if work_dir.exists():
        raise OrchestrateError("dry-run work directory must be unused")
    work_dir.mkdir(parents=True, exist_ok=False)

    if acquired_dir is None:
        acquired_path = work_dir / "acquired"
        try:
            acquired = acquire_release(
                tag=tag, output_dir=acquired_path, run_id=run_id, gh=gh
            )
        except AcquireError as exc:
            raise OrchestrateError(str(exc)) from exc
        lines.append("acquisition=performed")
        lines.append("github_api_association=fresh")
    else:
        try:
            acquired = load_acquired_bundle(acquired_dir)
        except AcquireError as exc:
            raise OrchestrateError(str(exc)) from exc
        if acquired.identity.tag != tag:
            raise OrchestrateError("acquired tag does not match dry-run --tag")
        if acquired.identity.ci_run_id != run_id:
            raise OrchestrateError("acquired run id does not match dry-run --run-id")
        lines.append("acquisition=reused-internal-consistency-only")
        lines.append(
            "transferred_bundle_is_not_independent_github_provenance_proof=true"
        )

    lines.append(f"ci_head_sha={acquired.identity.ci_head_sha}")
    lines.append(f"scriptlet_sha256={acquired.identity.scriptlet_sha256}")
    lines.append("github_api_provenance=associated-not-cryptographic-attestation")

    signed = match_signed_to_acquired(signed=signed_assets, identity=acquired.identity)
    lines.append("signed_assets_match_acquisition=true")

    # Cryptographic chain under the caller-supplied / disposable key.
    if public_key is None and pin_unprovisioned:
        raise OrchestrateError(
            "dry-run requires --public-key while the repository pin is UNPROVISIONED "
            "so the candidate can be verified independently of the official pin"
        )

    isolated_crypto_ok = False
    if public_key is not None:
        try:
            report = run_preflight(
                assets_dir=signed.root,
                expected_tag=tag,
                trust=TrustContext.FIXTURE,
                public_key_path=public_key,
            )
        except PreflightError as exc:
            raise OrchestrateError(
                f"isolated cryptographic verification refused: {exc}"
            ) from exc
        if report.production_promotion_ready:
            raise OrchestrateError(
                "fixture rehearsal incorrectly claimed production_promotion_ready"
            )
        isolated_crypto_ok = True
        lines.append("isolated_cryptographic_verification=pass")
        lines.append("fixture_success_is_not_production_promotion_readiness=true")
        lines.append("isolated_key_is_not_official_publisher_authority=true")
    else:
        lines.append("isolated_cryptographic_verification=skipped-pin-provisioned")

    nine = stage_nine_file_candidate(
        acquired=acquired,
        signed=signed,
        dest=work_dir / "nine-file-candidate",
    )
    lines.append(f"nine_file_candidate={nine}")

    production_refused = False
    refusal_category: str | None = None
    try:
        gate = run_promote_gate(
            release_candidate_dir=nine,
            tag=tag,
            source_revision=acquired.identity.ci_head_sha,
            expected_workflow=EXPECTED_WORKFLOW,
        )
        lines.append("production_promote_gate=pass")
        lines.append(
            f"production_promotion_ready="
            f"{'true' if gate.production_promotion_ready else 'false'}"
        )
        if pin_unprovisioned:
            raise OrchestrateError(
                "production promote-gate unexpectedly passed while pin is UNPROVISIONED"
            )
    except PreflightError as exc:
        category = classify_promote_gate_error(exc)
        if pin_unprovisioned:
            if category != "official-pin-unprovisioned":
                raise OrchestrateError(
                    f"production promote-gate refused for unexpected reason "
                    f"({category}): {exc}"
                ) from exc
            production_refused = True
            refusal_category = category
            lines.append("production_promote_gate=refused")
            lines.append(f"production_promote_gate_refusal_category={category}")
            lines.append(f"production_promote_gate_reason={exc}")
        elif public_key is not None:
            # Provisioned pin + disposable/fixture key: official gate refusal is
            # expected. Record it and continue so shell dry-run can still run
            # (Option A rehearsal uses IGNITE_PROMOTE_PUBLIC_KEY_FILE).
            production_refused = True
            refusal_category = category
            lines.append("production_promote_gate=refused")
            lines.append(f"production_promote_gate_refusal_category={category}")
            lines.append(f"production_promote_gate_reason={exc}")
            lines.append(
                "production_promote_gate_refusal_expected_for_non_matching_key=true"
            )
        else:
            # Provisioned pin without a disposable key: treat as hard failure.
            raise OrchestrateError(
                f"production promote-gate refused ({category}): {exc}"
            ) from exc

    promote_shell_executed = False
    promote_shell_ok = False
    shell_category: str | None = None
    outcome: DryRunOutcome | None = None

    if not run_promote_shell:
        lines.append("promote_shell_dry_run=skipped")
        lines.append("promote_shell_dry_run_reason=skip-promote-shell")
        lines.append("verification_scope=partial")
        # Disposable-signed candidate under a provisioned pin is not a successful
        # partial rehearsal of the official gate — fail closed (exit 1).
        if (not pin_unprovisioned) and production_refused:
            outcome = DryRunOutcome.FAILED
        else:
            outcome = DryRunOutcome.PARTIAL
    elif not _PROMOTE_SH.is_file():
        lines.append("promote_shell_dry_run=not-performed")
        lines.append("promote_shell_dry_run_reason=promote-release.sh missing")
        outcome = DryRunOutcome.NOT_PERFORMED
    else:
        shell_work = work_dir / "promote-work"
        try:
            completed = run_promote_dry_run_shell(
                acquired=acquired,
                signed=signed,
                work_dir=shell_work,
                # Disposable key is rehearsal-only; never production pin authority.
                public_key_file=public_key,
                env_extra=promote_env_extra,
            )
        except OrchestrateError as exc:
            # bash unavailable / work dir issues — verification not performed.
            lines.append("promote_shell_dry_run=not-performed")
            lines.append(f"promote_shell_dry_run_reason={exc}")
            outcome = DryRunOutcome.NOT_PERFORMED
            completed = None
        except OSError as exc:
            lines.append("promote_shell_dry_run=not-performed")
            lines.append(f"promote_shell_dry_run_reason={exc}")
            outcome = DryRunOutcome.NOT_PERFORMED
            completed = None

        if completed is not None:
            promote_shell_executed = True
            combined = (completed.stdout or "") + "\n" + (completed.stderr or "")
            lines.append(f"promote_shell_exit={completed.returncode}")
            for marker in _SHELL_PHASE_MARKERS:
                present = "true" if marker in combined else "false"
                key = marker.removeprefix("promote-phase=")
                lines.append(f"promote_shell_phase_{key.replace('-', '_')}={present}")
            if _SHELL_PIN_REFUSAL in combined:
                lines.append(
                    "promote_shell_refusal_marker=official-pin-unprovisioned"
                )
            elif "promote-shell-refusal=other" in combined:
                lines.append("promote_shell_refusal_marker=other")
            else:
                lines.append("promote_shell_refusal_marker=none")

            shell_category, detail = classify_promote_shell_output(
                combined=combined,
                returncode=completed.returncode,
                pin_unprovisioned=pin_unprovisioned,
            )
            lines.append(f"promote_shell_classification={shell_category}")
            if detail is not None:
                lines.append(f"promote_shell_classification_detail={detail}")

            if shell_category == "shell-pass":
                promote_shell_ok = True
                lines.append("promote_shell_dry_run=pass")
                outcome = DryRunOutcome.COMPLETE_PROVISIONED_PASS
            elif shell_category == "official-pin-unprovisioned":
                lines.append("promote_shell_dry_run=refused-official-pin-unprovisioned")
                lines.append(
                    "promote_shell_refusal_category=official-pin-unprovisioned"
                )
                lines.append("promote_shell_expected_unprovisioned_refusal=true")
                if production_refused and isolated_crypto_ok:
                    outcome = DryRunOutcome.COMPLETE_UNPROVISIONED_BOUNDARY
                else:
                    lines.append(
                        "promote_shell_complete_rehearsal_blocked="
                        "gate-or-isolated-crypto-incomplete"
                    )
                    outcome = DryRunOutcome.FAILED
            else:
                lines.append("promote_shell_dry_run=failed")
                lines.append(f"promote_shell_refusal_category={shell_category}")
                # Non-secret failure snippet for operators / tests (last lines only).
                snippet = " | ".join(
                    ln.strip()
                    for ln in combined.strip().splitlines()[-6:]
                    if ln.strip() and "signature=" not in ln.lower()
                )
                if snippet:
                    lines.append(f"promote_shell_error_snippet={snippet[:400]}")
                outcome = DryRunOutcome.FAILED

    if outcome is None:
        outcome = DryRunOutcome.FAILED
        lines.append("promote_shell_dry_run=failed")

    if pin_unprovisioned:
        lines.append("production_promotion_ready=false")
        lines.append("remaining_blocker=production-pin-UNPROVISIONED")
    else:
        lines.append(
            "remaining_blocker=none-for-dry-run;"
            "publication-still-requires-explicit-promote-publish"
        )

    lines.append(f"dry_run_outcome={outcome.value}")
    return DryRunReport(
        lines=lines,
        outcome=outcome,
        production_gate_refused=production_refused,
        production_gate_refusal_category=refusal_category,
        isolated_crypto_ok=isolated_crypto_ok,
        promote_shell_executed=promote_shell_executed,
        promote_shell_ok=promote_shell_ok,
        nine_file_dir=nine,
        pin_unprovisioned=pin_unprovisioned,
        extras={"promote_shell_classification": shell_category or ""},
    )
