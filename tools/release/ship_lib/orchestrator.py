"""Phased ship orchestrator with resumable journal."""

from __future__ import annotations

import shutil
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from .authority import authority_present, initialize_authority, load_authority
from .ci_coord import wait_for_ci_success
from .config import ShipConfig
from .errors import ShipBlockedError, ShipConfigError, ShipError
from .git_coord import (
    GitHubClient,
    SubprocessGitHub,
    assert_clean_for_live,
    create_or_update_pr_plan,
    merge_pr_plan,
    prepare_release_commit_plan,
)
from .hardware import HardwareAcceptanceHarness, HardwareProbe, MockHardwareProbe
from .journal import ShipJournal
from .outcomes import (
    PHASE_ACQUIRE,
    PHASE_AUTHORITY,
    PHASE_CI_WAIT,
    PHASE_DONE,
    PHASE_GIT_MERGE,
    PHASE_GIT_PREPARE,
    PHASE_GIT_PUSH,
    PHASE_HARDWARE,
    PHASE_PIN,
    PHASE_PUBLISH,
    PHASE_SIGN,
    PHASE_VERIFY,
    PHASE_VERSION,
    ShipOutcome,
)
from .pin_provision import provision_public_pin
from .publish import build_publish_env, publish_release
from .secret_store import SecretStore, select_secret_store
from .signing_service import RestrictedSigningService, SignReleaseRequest
from .version_sync import apply_version, tag_for_version, validate_version_string


@dataclass
class ShipReport:
    outcome: ShipOutcome
    version: str
    tag: str
    lines: list[str] = field(default_factory=list)
    fields: dict[str, Any] = field(default_factory=dict)

    def emit(self, line: str) -> None:
        self.lines.append(line)


@dataclass
class ShipHooks:
    """Injectable seams for tests / dry orchestration without live mutation."""

    secret_store: SecretStore | None = None
    gh: GitHubClient | None = None
    hardware_probe: HardwareProbe | None = None
    acquire_fn: Callable[..., Any] | None = None
    dry_run_fn: Callable[..., Any] | None = None
    # When False, git/CI/publish phases record plans only (development default).
    execute_live_mutations: bool = False
    skip_hardware_with_mock_pass: bool = False


class ShipOrchestrator:
    def __init__(self, config: ShipConfig, hooks: ShipHooks | None = None) -> None:
        self.config = config
        self.hooks = hooks or ShipHooks()

    def status(self) -> ShipReport:
        report = ShipReport(
            outcome=ShipOutcome.READY,
            version="",
            tag="",
        )
        report.emit("schema=ignite-ship-status-v1")
        report.emit(f"repo_root={self.config.repo_root}")
        report.emit(f"ship_home={self.config.ship_home}")
        report.emit(
            f"recovery_dir={self.config.recovery_dir or 'UNSET'}"
        )
        report.emit(f"allow_live={str(self.config.allow_live).lower()}")
        if authority_present(self.config):
            auth = load_authority(self.config)
            report.emit("authority=initialized")
            report.emit(f"fingerprint_sha256={auth.fingerprint_sha256}")
            report.emit(f"secret_store={auth.store_name}")
            report.emit(
                f"recovery_verified={str(auth.recovery_verified).lower()}"
            )
            report.fields["fingerprint_sha256"] = auth.fingerprint_sha256
        else:
            report.emit("authority=absent")
        pin = self.config.pin_path.read_bytes()
        if pin == b"UNPROVISIONED\n":
            report.emit("repository_pin=UNPROVISIONED")
        else:
            report.emit("repository_pin=provisioned")
            report.emit(f"repository_pin_hex={pin[:-1].decode('ascii')}")
        report.emit(
            f"hardware_device_id={self.config.hardware_device_id or 'UNSET'}"
        )
        report.emit(
            f"require_hardware={str(self.config.require_hardware).lower()}"
        )
        return report

    def doctor(self) -> ShipReport:
        report = self.status()
        report.emit("schema=ignite-ship-doctor-v1")
        problems: list[str] = []
        try:
            select_secret_store(allow_insecure=self.config.allow_insecure_store)
            report.emit("secret_store=ok")
        except ShipConfigError as exc:
            problems.append(str(exc))
            report.emit(f"secret_store=error:{exc}")
        if self.config.recovery_dir is None:
            problems.append("IGNITE_SHIP_RECOVERY_DIR unset")
            report.emit("recovery_dir=missing")
        else:
            report.emit("recovery_dir=configured")
        if shutil.which("openssl") is None:
            problems.append("openssl missing")
            report.emit("openssl=missing")
        else:
            report.emit("openssl=ok")
        if shutil.which("gh") is None and self.hooks.gh is None:
            problems.append("gh missing")
            report.emit("gh=missing")
        else:
            report.emit("gh=ok")
        if self.config.require_hardware and not self.config.hardware_device_id:
            problems.append("hardware device id unset")
            report.emit("hardware=unconfigured")
        if problems:
            report.outcome = ShipOutcome.BLOCKED_CONFIG
            report.emit("doctor=failed")
        else:
            report.outcome = ShipOutcome.READY
            report.emit("doctor=ok")
        return report

    def ship(self, version: str, *, resume: bool = False) -> ShipReport:
        validate_version_string(version)
        tag = tag_for_version(version)
        self.config.ship_home.mkdir(parents=True, exist_ok=True)
        self.config.journal_dir.mkdir(parents=True, exist_ok=True)
        self.config.work_dir.mkdir(parents=True, exist_ok=True)
        journal = ShipJournal.open_for_version(self.config.journal_dir, version)
        report = ShipReport(outcome=ShipOutcome.PARTIAL, version=version, tag=tag)
        report.emit("schema=ignite-ship-run-v1")
        report.emit(f"version={version}")
        report.emit(f"tag={tag}")
        report.emit(f"resume={str(resume).lower()}")
        report.emit(
            f"execute_live_mutations={str(self.hooks.execute_live_mutations).lower()}"
        )

        try:
            self._phase_authority(journal, report)
            self._phase_pin(journal, report)
            self._phase_version(journal, report, version)
            self._phase_git_prepare(journal, report, version)
            self._phase_git_push(journal, report, version)
            self._phase_git_merge(journal, report)
            self._phase_ci_wait(journal, report)
            self._phase_acquire(journal, report, tag)
            self._phase_sign(journal, report, version, tag)
            self._phase_verify(journal, report, tag)
            self._phase_hardware(journal, report, version)
            self._phase_publish(journal, report)
            if not journal.is_complete(PHASE_DONE):
                journal.mark(PHASE_DONE, published=True)
            report.emit("phase=done")
            report.outcome = ShipOutcome.SUCCESS
            report.emit(f"ship_outcome={report.outcome.value}")
            return report
        except ShipConfigError as exc:
            report.outcome = ShipOutcome.BLOCKED_CONFIG
            report.emit(f"error={exc}")
            report.emit(f"ship_outcome={report.outcome.value}")
            return report
        except ShipBlockedError as exc:
            msg = str(exc)
            if "hardware" in msg:
                report.outcome = ShipOutcome.BLOCKED_HARDWARE
            elif "ci.yml" in msg or "CI" in msg or "checks" in msg:
                report.outcome = ShipOutcome.BLOCKED_CI
            elif "policy" in msg or "review" in msg or "protection" in msg:
                report.outcome = ShipOutcome.BLOCKED_POLICY
            elif "verif" in msg or "dry_run" in msg:
                report.outcome = ShipOutcome.BLOCKED_VERIFY
            else:
                report.outcome = ShipOutcome.BLOCKED_POLICY
            report.emit(f"error={exc}")
            report.emit(f"ship_outcome={report.outcome.value}")
            return report
        except ShipError as exc:
            report.outcome = ShipOutcome.FAILED
            report.emit(f"error={exc}")
            report.emit(f"ship_outcome={report.outcome.value}")
            return report

    # --- phases -----------------------------------------------------------------

    def _phase_authority(self, journal: ShipJournal, report: ShipReport) -> None:
        if journal.is_complete(PHASE_AUTHORITY):
            report.emit("phase=authority-init:skipped-complete")
            return
        if authority_present(self.config):
            auth = load_authority(self.config)
            journal.mark(
                PHASE_AUTHORITY,
                fingerprint_sha256=auth.fingerprint_sha256,
                reused=True,
            )
            report.emit("phase=authority-init:reused")
            report.emit(f"fingerprint_sha256={auth.fingerprint_sha256}")
            return
        auth = initialize_authority(
            self.config, store=self.hooks.secret_store
        )
        journal.mark(
            PHASE_AUTHORITY,
            fingerprint_sha256=auth.fingerprint_sha256,
            reused=False,
        )
        report.emit("phase=authority-init:created")
        report.emit(f"fingerprint_sha256={auth.fingerprint_sha256}")

    def _phase_pin(self, journal: ShipJournal, report: ShipReport) -> None:
        if journal.is_complete(PHASE_PIN):
            report.emit("phase=public-pin-provision:skipped-complete")
            return
        fingerprint = provision_public_pin(self.config)
        journal.mark(PHASE_PIN, fingerprint_sha256=fingerprint)
        report.emit("phase=public-pin-provision:ok")
        report.emit(f"fingerprint_sha256={fingerprint}")

    def _phase_version(
        self, journal: ShipJournal, report: ShipReport, version: str
    ) -> None:
        if journal.is_complete(PHASE_VERSION):
            report.emit("phase=version-sync:skipped-complete")
            return
        changed = apply_version(self.config.repo_root, version)
        journal.mark(PHASE_VERSION, changed_paths=changed)
        report.emit("phase=version-sync:ok")
        report.emit(f"changed_paths={','.join(changed) if changed else 'none'}")

    def _phase_git_prepare(
        self, journal: ShipJournal, report: ShipReport, version: str
    ) -> None:
        if journal.is_complete(PHASE_GIT_PREPARE):
            report.emit("phase=git-prepare:skipped-complete")
            return
        changed = list(journal.fields.get("changed_paths") or [])
        pin_done = journal.is_complete(PHASE_PIN)
        plan = prepare_release_commit_plan(
            version=version, changed_paths=changed, pin_provisioned=pin_done
        )
        if self.hooks.execute_live_mutations and self.config.allow_live:
            assert_clean_for_live(self.config.repo_root)
            raise ShipBlockedError(
                "live git commit execution is intentionally disabled in this "
                "engineering checkout; review unstaged changes and commit manually "
                "or enable a maintainer-approved live runner"
            )
        journal.mark(PHASE_GIT_PREPARE, commit_plan=plan)
        report.emit("phase=git-prepare:planned")
        report.emit(f"commit_subject={plan['subject']}")

    def _phase_git_push(
        self, journal: ShipJournal, report: ShipReport, version: str
    ) -> None:
        if journal.is_complete(PHASE_GIT_PUSH):
            report.emit("phase=git-push-pr:skipped-complete")
            return
        plan = create_or_update_pr_plan(
            title=f"release: Ignite {version}",
            head=self.config.feature_branch,
            base=self.config.base_branch,
            body=(
                f"Authenticated release preparation for v{version}.\n\n"
                "Public publisher pin + version sync. Private keys are not included."
            ),
        )
        if self.hooks.execute_live_mutations and self.config.allow_live:
            raise ShipBlockedError(
                "live push/PR execution is disabled in this engineering checkout"
            )
        journal.mark(PHASE_GIT_PUSH, pr_plan=plan)
        report.emit("phase=git-push-pr:planned")

    def _phase_git_merge(self, journal: ShipJournal, report: ShipReport) -> None:
        if journal.is_complete(PHASE_GIT_MERGE):
            report.emit("phase=git-merge:skipped-complete")
            return
        plan = merge_pr_plan(pr_number=0)
        if self.hooks.execute_live_mutations and self.config.allow_live:
            raise ShipBlockedError(
                "live merge requires independent review under branch protection; "
                "ship will not forge approvals"
            )
        # Development/default: record that merge must yield an exact main SHA later.
        journal.mark(
            PHASE_GIT_MERGE,
            merge_plan=plan,
            main_sha=journal.fields.get("main_sha") or "PENDING_MAIN_SHA",
        )
        report.emit("phase=git-merge:planned")

    def _phase_ci_wait(self, journal: ShipJournal, report: ShipReport) -> None:
        if journal.is_complete(PHASE_CI_WAIT):
            report.emit("phase=ci-wait:skipped-complete")
            return
        main_sha = str(journal.fields.get("main_sha") or "")
        if not self.hooks.execute_live_mutations or not self.config.allow_live:
            # Tests may inject a synthetic CI run id / sha via journal fields.
            if journal.fields.get("ci_run_id") and len(main_sha) == 40:
                journal.mark(
                    PHASE_CI_WAIT,
                    ci_run_id=int(journal.fields["ci_run_id"]),
                    main_sha=main_sha,
                )
                report.emit("phase=ci-wait:injected")
                return
            journal.mark(PHASE_CI_WAIT, ci_run_id=0, main_sha=main_sha or "PENDING")
            report.emit("phase=ci-wait:deferred-non-live")
            return
        if len(main_sha) != 40:
            raise ShipBlockedError("main_sha unset after merge; cannot bind ci.yml")
        gh = self.hooks.gh or SubprocessGitHub()
        run = wait_for_ci_success(
            repo_root=self.config.repo_root,
            head_sha=main_sha,
            gh=gh,
            poll_seconds=self.config.ci_poll_seconds,
            timeout_seconds=self.config.ci_timeout_seconds,
            repository=self.config.repository,
        )
        journal.mark(PHASE_CI_WAIT, ci_run_id=run.run_id, main_sha=run.head_sha)
        report.emit(f"phase=ci-wait:ok run_id={run.run_id}")

    def _phase_acquire(
        self, journal: ShipJournal, report: ShipReport, tag: str
    ) -> None:
        if journal.is_complete(PHASE_ACQUIRE):
            report.emit("phase=acquire:skipped-complete")
            return
        run_id = int(journal.fields.get("ci_run_id") or 0)
        out = self.config.work_dir / f"acquired-{tag}"
        if self.hooks.acquire_fn is not None:
            bundle = self.hooks.acquire_fn(tag=tag, output_dir=out, run_id=run_id)
            journal.mark(
                PHASE_ACQUIRE,
                acquired_dir=str(bundle.root if hasattr(bundle, "root") else out),
                scriptlet_sha256=getattr(
                    getattr(bundle, "identity", None), "scriptlet_sha256", ""
                ),
                scriptlet_bytes=getattr(
                    getattr(bundle, "identity", None), "scriptlet_bytes", 0
                ),
                source_revision=getattr(
                    getattr(bundle, "identity", None), "ci_head_sha", ""
                ),
            )
            report.emit("phase=acquire:ok")
            return
        if not self.hooks.execute_live_mutations or not self.config.allow_live:
            journal.mark(PHASE_ACQUIRE, acquired_dir=str(out), deferred=True)
            report.emit("phase=acquire:deferred-non-live")
            return
        kindle = self.config.repo_root / "deployment" / "kindle"
        if str(kindle) not in sys.path:
            sys.path.insert(0, str(kindle))
        from release_acquire import acquire_release

        if run_id <= 0:
            raise ShipBlockedError("ci_run_id missing; refuse acquire")
        if out.exists():
            shutil.rmtree(out)
        bundle = acquire_release(
            tag=tag,
            output_dir=out,
            run_id=run_id,
            repository=self.config.repository,
        )
        # Never accept a pre-provisioning build: pin must already be provisioned.
        if self.config.pin_path.read_bytes() == b"UNPROVISIONED\n":
            raise ShipError("refuse acquire while repository pin is UNPROVISIONED")
        journal.mark(
            PHASE_ACQUIRE,
            acquired_dir=str(bundle.root),
            scriptlet_sha256=bundle.identity.scriptlet_sha256,
            scriptlet_bytes=bundle.identity.scriptlet_bytes,
            source_revision=bundle.identity.ci_head_sha,
        )
        report.emit(f"phase=acquire:ok run_id={run_id}")

    def _phase_sign(
        self,
        journal: ShipJournal,
        report: ShipReport,
        version: str,
        tag: str,
    ) -> None:
        if journal.is_complete(PHASE_SIGN):
            report.emit("phase=sign:skipped-complete")
            return
        acquired_dir = Path(str(journal.fields.get("acquired_dir") or ""))
        if not acquired_dir.is_dir():
            if not self.hooks.execute_live_mutations:
                journal.mark(PHASE_SIGN, signed_dir="", deferred=True)
                report.emit("phase=sign:deferred-non-live")
                return
            raise ShipError("acquired directory missing for signing")
        out = self.config.work_dir / f"signed-{tag}"
        if out.exists():
            shutil.rmtree(out)
        service = RestrictedSigningService(
            self.config, store=self.hooks.secret_store
        )
        request = SignReleaseRequest(
            tag=tag,
            version=version,
            source_revision=str(journal.fields.get("source_revision") or ""),
            repository=self.config.repository,
            workflow=".github/workflows/ci.yml",
            scriptlet_sha256=str(journal.fields.get("scriptlet_sha256") or ""),
            scriptlet_bytes=int(journal.fields.get("scriptlet_bytes") or 0),
            acquired_root=acquired_dir,
            output_dir=out,
        )
        result = service.sign_release_candidate(request)
        journal.mark(
            PHASE_SIGN,
            signed_dir=str(result.signed_root),
            fingerprint_sha256=result.fingerprint_sha256,
        )
        report.emit("phase=sign:ok")

    def _phase_verify(
        self, journal: ShipJournal, report: ShipReport, tag: str
    ) -> None:
        if journal.is_complete(PHASE_VERIFY):
            report.emit("phase=verify-dry-run:skipped-complete")
            return
        signed_dir = Path(str(journal.fields.get("signed_dir") or ""))
        acquired_dir = Path(str(journal.fields.get("acquired_dir") or ""))
        run_id = int(journal.fields.get("ci_run_id") or 0)
        if self.hooks.dry_run_fn is not None:
            dry = self.hooks.dry_run_fn(
                tag=tag,
                run_id=run_id,
                signed_assets=signed_dir,
                acquired_dir=acquired_dir,
            )
            outcome = getattr(getattr(dry, "outcome", None), "value", str(dry))
            if outcome != "complete-verification-pass":
                raise ShipBlockedError(
                    f"dry-run outcome {outcome!r} is not complete-verification-pass"
                )
            journal.mark(PHASE_VERIFY, dry_run_outcome=outcome)
            report.emit("phase=verify-dry-run:ok")
            return
        if not self.hooks.execute_live_mutations or not self.config.allow_live:
            journal.mark(
                PHASE_VERIFY, dry_run_outcome="deferred-non-live"
            )
            report.emit("phase=verify-dry-run:deferred-non-live")
            return
        kindle = self.config.repo_root / "deployment" / "kindle"
        if str(kindle) not in sys.path:
            sys.path.insert(0, str(kindle))
        from release_orchestrate import DryRunOutcome, dry_run_release

        work = self.config.work_dir / f"dry-{tag}"
        if work.exists():
            shutil.rmtree(work)
        dry = dry_run_release(
            tag=tag,
            run_id=run_id,
            signed_assets=signed_dir,
            work_dir=work,
            acquired_dir=acquired_dir,
            public_key=None,
            run_promote_shell=True,
        )
        if dry.outcome != DryRunOutcome.COMPLETE_PROVISIONED_PASS:
            raise ShipBlockedError(
                f"dry-run outcome {dry.outcome.value!r} is not complete-verification-pass"
            )
        if dry.pin_unprovisioned:
            raise ShipBlockedError("dry-run still reports UNPROVISIONED pin")
        journal.mark(PHASE_VERIFY, dry_run_outcome=dry.outcome.value)
        report.emit("phase=verify-dry-run:ok")

    def _phase_hardware(
        self, journal: ShipJournal, report: ShipReport, version: str
    ) -> None:
        if journal.is_complete(PHASE_HARDWARE):
            report.emit("phase=hardware-acceptance:skipped-complete")
            return
        if not self.config.require_hardware:
            journal.mark(PHASE_HARDWARE, skipped=True, reason="require_hardware=false")
            report.emit("phase=hardware-acceptance:skipped-config")
            return
        harness = HardwareAcceptanceHarness(
            required_device_id=self.config.hardware_device_id
        )
        probe = self.hooks.hardware_probe
        if probe is None and self.hooks.skip_hardware_with_mock_pass:
            probe = MockHardwareProbe(pass_all=True)
        auth = load_authority(self.config)
        result = harness.run(
            signed_root=Path(str(journal.fields.get("signed_dir") or ".")),
            public_key_hex=auth.public_key.hex(),
            version=version,
            probe=probe,
        )
        if not result.passed:
            raise ShipBlockedError("hardware acceptance failed")
        journal.mark(
            PHASE_HARDWARE,
            device_id=result.device_id,
            firmware=result.firmware,
            details=list(result.details),
        )
        report.emit("phase=hardware-acceptance:ok")
        if "mock-host-probe-not-arm-device" in result.details:
            report.emit("hardware_evidence=host-mock-not-arm")

    def _phase_publish(self, journal: ShipJournal, report: ShipReport) -> None:
        if journal.is_complete(PHASE_PUBLISH):
            report.emit("phase=publish:skipped-complete")
            return
        if not self.hooks.execute_live_mutations or not self.config.allow_live:
            journal.mark(PHASE_PUBLISH, executed=False, deferred=True)
            report.emit("phase=publish:deferred-non-live")
            return
        # Live publish path (not executed during engineering development).
        kindle = self.config.repo_root / "deployment" / "kindle"
        if str(kindle) not in sys.path:
            sys.path.insert(0, str(kindle))
        from release_acquire import load_acquired_bundle
        from release_orchestrate import match_signed_to_acquired

        acquired = load_acquired_bundle(Path(str(journal.fields["acquired_dir"])))
        signed = match_signed_to_acquired(
            signed=Path(str(journal.fields["signed_dir"])),
            identity=acquired.identity,
        )
        env = build_publish_env(
            acquired=acquired,
            signed=signed,
            work_dir=self.config.work_dir / "promote-publish",
        )
        result = publish_release(
            repo_root=self.config.repo_root, env=env, dry_run_only=False
        )
        journal.mark(
            PHASE_PUBLISH,
            executed=result.executed,
            release_url=result.release_url or "",
            commit=result.commit,
        )
        report.emit("phase=publish:ok")
        if result.release_url:
            report.emit(f"release_url={result.release_url}")
