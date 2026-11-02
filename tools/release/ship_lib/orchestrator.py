"""Phased ship orchestrator with truthful journals and resumable live execution."""

from __future__ import annotations

import shutil
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from .authority import authority_present, initialize_authority, load_authority
from .ci_coord import wait_for_ci_success
from .config import OWNED_RELEASE_PATHS, ShipConfig
from .errors import ShipBlockedError, ShipConfigError, ShipError
from .git_coord import (
    GitHubClient,
    GitRunner,
    SubprocessGit,
    SubprocessGitHub,
    assert_only_owned_dirty,
    commit_release_preparation,
    create_or_update_pr,
    create_or_update_pr_plan,
    merge_pr,
    merge_pr_plan,
    prepare_release_commit_plan,
    push_feature_branch,
    release_commit_paths,
    verify_release_tree,
    wait_for_pr_checks,
)
from .hardware import (
    HardwareAcceptanceHarness,
    HardwareProbe,
    MockHardwareProbe,
)
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
    PhaseState,
    ShipOutcome,
)
from .pin_provision import provision_public_pin
from .publish import (
    assemble_nine_file_inventory,
    build_publish_env,
    publish_release,
    seed_promote_work_from_acquired,
    verify_published_release,
)
from .recovery import (
    assert_dpapi_alone_insufficient_label,
    recovery_backup_paths,
    recovery_key_path,
)
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
    """Injectable seams for tests. Default CLI never installs production-faking probes."""

    secret_store: SecretStore | None = None
    gh: GitHubClient | None = None
    git: GitRunner | None = None
    hardware_probe: HardwareProbe | None = None
    acquire_fn: Callable[..., Any] | None = None
    dry_run_fn: Callable[..., Any] | None = None
    publish_fn: Callable[..., Any] | None = None
    # Live / production execution boundary.
    execute_live_mutations: bool = False
    # Disposable-tree test seam: allow local authority/pin/version without GitHub.
    # Never enabled by the default CLI.
    allow_local_mutations_for_tests: bool = False


class ShipOrchestrator:
    def __init__(self, config: ShipConfig, hooks: ShipHooks | None = None) -> None:
        self.config = config
        self.hooks = hooks or ShipHooks()

    @property
    def production_mode(self) -> bool:
        """True only when live execution is deliberately enabled."""
        return bool(self.hooks.execute_live_mutations and self.config.allow_live)

    @property
    def may_mutate_workspace(self) -> bool:
        return self.production_mode or self.hooks.allow_local_mutations_for_tests

    def status(self) -> ShipReport:
        report = ShipReport(outcome=ShipOutcome.READY, version="", tag="")
        report.emit("schema=ignite-ship-status-v1")
        report.emit(f"repo_root={self.config.repo_root}")
        report.emit(f"ship_home={self.config.ship_home}")
        report.emit(f"recovery_dir={self.config.recovery_dir or 'UNSET'}")
        report.emit(
            f"recovery_key_dir={self.config.recovery_key_dir or 'UNSET'}"
        )
        report.emit(f"allow_live={str(self.config.allow_live).lower()}")
        report.emit(assert_dpapi_alone_insufficient_label())
        if authority_present(self.config):
            auth = load_authority(self.config)
            report.emit("authority=initialized")
            report.emit(f"fingerprint_sha256={auth.fingerprint_sha256}")
            report.emit(f"secret_store={auth.store_name}")
            report.emit(
                f"recovery_verified={str(auth.recovery_verified).lower()}"
            )
            report.emit(f"recovery_mechanism={auth.recovery_mechanism}")
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
            select_secret_store(
                allow_insecure=self.config.allow_insecure_store
                and not self.production_mode,
                production=self.production_mode,
            )
            report.emit("secret_store=ok")
        except ShipConfigError as exc:
            problems.append(str(exc))
            report.emit(f"secret_store=error:{exc}")
        if self.config.recovery_dir is None:
            problems.append("IGNITE_SHIP_RECOVERY_DIR unset")
            report.emit("recovery_dir=missing")
        else:
            report.emit("recovery_dir=configured")
            backup = recovery_backup_paths(self.config.recovery_dir)
            if backup["encrypted"].is_file():
                report.emit("recovery_ciphertext=present")
            else:
                report.emit("recovery_ciphertext=absent")
            if (self.config.recovery_dir / "recovery-authority.key").exists():
                problems.append(
                    "recovery key colocated with backup destination (unsafe)"
                )
                report.emit("recovery_key_domain=colocated-unsafe")
        if self.config.recovery_key_dir is None:
            problems.append("IGNITE_SHIP_RECOVERY_KEY_DIR unset")
            report.emit("recovery_key_dir=missing")
        else:
            report.emit("recovery_key_dir=configured")
            if recovery_key_path(self.config.recovery_key_dir).is_file():
                report.emit("recovery_key=present")
            else:
                report.emit("recovery_key=absent")
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
        if not self.config.hardware_device_id:
            problems.append("hardware device id unset")
            report.emit("hardware=unconfigured")
        else:
            report.emit("hardware=configured")
        if problems:
            report.outcome = ShipOutcome.BLOCKED_CONFIG
            report.emit("doctor=failed")
        else:
            report.outcome = ShipOutcome.READY
            report.emit("doctor=ok")
        return report

    def plan(self, version: str) -> ShipReport:
        """Explicit read-only rehearsal — never mutates authority, pin, versions, or Git."""
        return self.ship(version, resume=False, plan_only=True)

    def ship(
        self,
        version: str,
        *,
        resume: bool = False,
        plan_only: bool = False,
    ) -> ShipReport:
        validate_version_string(version)
        tag = tag_for_version(version)
        # Journal dir may be created for plan notes under an isolated ship_home.
        # Plan mode still must not create authority or touch the repo pin/versions.
        self.config.ship_home.mkdir(parents=True, exist_ok=True)
        self.config.journal_dir.mkdir(parents=True, exist_ok=True)
        if self.may_mutate_workspace:
            self.config.work_dir.mkdir(parents=True, exist_ok=True)
        journal = ShipJournal.open_for_version(self.config.journal_dir, version)
        report = ShipReport(outcome=ShipOutcome.PARTIAL, version=version, tag=tag)
        report.emit("schema=ignite-ship-run-v1")
        report.emit(f"version={version}")
        report.emit(f"tag={tag}")
        report.emit(f"resume={str(resume).lower()}")
        report.emit(f"plan_only={str(plan_only).lower()}")
        report.emit(
            f"execute_live_mutations={str(self.hooks.execute_live_mutations).lower()}"
        )
        report.emit(f"production_mode={str(self.production_mode).lower()}")
        report.emit(
            f"may_mutate_workspace={str(self.may_mutate_workspace and not plan_only).lower()}"
        )

        mutate = self.may_mutate_workspace and not plan_only

        try:
            self._phase_authority(journal, report, mutate=mutate)
            self._phase_pin(journal, report, mutate=mutate)
            self._phase_version(journal, report, version, mutate=mutate)
            self._phase_git_prepare(journal, report, version, mutate=mutate)
            self._phase_git_push(journal, report, version, mutate=mutate)
            self._phase_git_merge(journal, report, mutate=mutate)
            self._phase_ci_wait(journal, report, mutate=mutate)
            self._phase_acquire(journal, report, tag, mutate=mutate)
            self._phase_sign(journal, report, version, tag, mutate=mutate)
            self._phase_verify(journal, report, tag, mutate=mutate)
            self._phase_hardware(journal, report, version, mutate=mutate)
            self._phase_publish(journal, report, mutate=mutate)

            if journal.published() and journal.is_complete(PHASE_PUBLISH):
                if not journal.is_complete(PHASE_DONE):
                    journal.mark_completed(
                        PHASE_DONE,
                        identity=self._identity_binding(journal, version, tag),
                        published=True,
                    )
                report.emit("phase=done")
                report.outcome = ShipOutcome.SUCCESS
                report.emit("published=true")
            else:
                report.emit("phase=done:not-published")
                report.emit("published=false")
                if plan_only or not mutate:
                    report.outcome = ShipOutcome.PLANNED
                elif any(
                    journal.state_of(p) == PhaseState.BLOCKED
                    for p in journal.phases
                ):
                    report.outcome = ShipOutcome.BLOCKED_POLICY
                elif any(
                    journal.state_of(p) == PhaseState.DEFERRED
                    for p in journal.phases
                ):
                    report.outcome = ShipOutcome.NOT_PERFORMED
                else:
                    report.outcome = ShipOutcome.PARTIAL
            return report
        except ShipConfigError as exc:
            report.outcome = ShipOutcome.BLOCKED_CONFIG
            report.emit(f"error={exc}")
            report.emit("published=false")
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
            report.emit("published=false")
            return report
        except ShipError as exc:
            report.outcome = ShipOutcome.FAILED
            report.emit(f"error={exc}")
            report.emit("published=false")
            return report

    def _identity_binding(
        self, journal: ShipJournal, version: str, tag: str
    ) -> dict[str, Any]:
        auth_fp = ""
        if authority_present(self.config):
            auth_fp = load_authority(self.config).fingerprint_sha256
        return {
            "repository": self.config.repository,
            "version": version,
            "tag": tag,
            "fingerprint_sha256": auth_fp
            or str(journal.fields.get("fingerprint_sha256") or ""),
            "source_revision": str(journal.fields.get("source_revision") or ""),
            "main_sha": str(journal.fields.get("main_sha") or ""),
            "ci_run_id": int(journal.fields.get("ci_run_id") or 0),
            "ci_run_attempt": int(journal.fields.get("ci_run_attempt") or 0),
            "scriptlet_sha256": str(journal.fields.get("scriptlet_sha256") or ""),
            "scriptlet_bytes": int(journal.fields.get("scriptlet_bytes") or 0),
        }

    def _revalidate_completed(
        self, journal: ShipJournal, phase: str, version: str, tag: str
    ) -> None:
        """Independently re-check completed-phase identity on resume."""
        rec = journal.phases.get(phase)
        if rec is None or rec.state != PhaseState.COMPLETED:
            return
        ident = rec.identity or {}
        if ident.get("version") and ident["version"] != version:
            raise ShipError(f"journal identity version mismatch for {phase}")
        if ident.get("tag") and ident["tag"] != tag:
            raise ShipError(f"journal identity tag mismatch for {phase}")
        if ident.get("repository") and ident["repository"] != self.config.repository:
            raise ShipError(f"journal identity repository mismatch for {phase}")
        if phase == PHASE_AUTHORITY:
            if not authority_present(self.config):
                raise ShipError("authority phase complete but authority absent")
            auth = load_authority(self.config)
            if not auth.recovery_verified:
                raise ShipError("authority recovery no longer independently verified")
            if ident.get("fingerprint_sha256") and ident["fingerprint_sha256"] != auth.fingerprint_sha256:
                raise ShipError("authority fingerprint drift vs journal")
        if phase == PHASE_PIN:
            pin = self.config.pin_path.read_bytes()
            if pin == b"UNPROVISIONED\n":
                raise ShipError("pin phase marked complete but pin is UNPROVISIONED")
            if authority_present(self.config):
                auth = load_authority(self.config)
                if pin != auth.public_key.hex().encode("ascii") + b"\n":
                    raise ShipError("repository pin does not match authority")
        if phase == PHASE_ACQUIRE:
            main_sha = str(journal.fields.get("main_sha") or "")
            source = str(journal.fields.get("source_revision") or "")
            if main_sha and source and main_sha != source:
                raise ShipError("acquired source revision drifted from main_sha")
            digest = str(journal.fields.get("scriptlet_sha256") or "")
            if ident.get("scriptlet_sha256") and ident["scriptlet_sha256"] != digest:
                raise ShipError("acquired scriptlet digest drift vs journal")
        if phase == PHASE_SIGN:
            signed = Path(str(journal.fields.get("signed_dir") or ""))
            if not signed.is_dir():
                raise ShipError("signed phase complete but signed_dir missing")
        if phase == PHASE_VERIFY:
            if rec.fields.get("dry_run_outcome") != "complete-verification-pass":
                raise ShipError("verify phase complete without complete-verification-pass")
        if phase == PHASE_PUBLISH:
            if not rec.fields.get("published") or not rec.fields.get(
                "independently_verified"
            ):
                raise ShipError(
                    "publish marked complete without independent verification"
                )
            # Never trust journal flags alone — revalidate against GitHub.
            if self.production_mode or self.hooks.gh is not None:
                gh = self.hooks.gh or SubprocessGitHub()
                expected_names = list(rec.fields.get("asset_names") or [])
                expected_digests = dict(rec.fields.get("asset_digests") or {})
                if not expected_names or not expected_digests:
                    raise ShipError(
                        "publish resume requires stored asset inventory and digests"
                    )
                verify_published_release(
                    repo_root=self.config.repo_root,
                    tag=tag,
                    expected_commit=str(
                        rec.fields.get("commit")
                        or journal.fields.get("main_sha")
                        or ""
                    ),
                    gh=gh,
                    expected_asset_names=expected_names,
                    expected_digests=expected_digests,
                )

    # --- phases -----------------------------------------------------------------

    def _phase_authority(
        self, journal: ShipJournal, report: ShipReport, *, mutate: bool
    ) -> None:
        if journal.is_complete(PHASE_AUTHORITY):
            self._revalidate_completed(
                journal, PHASE_AUTHORITY, journal.version, journal.tag
            )
            report.emit("phase=authority-init:skipped-complete")
            return
        if journal.is_deferred(PHASE_AUTHORITY) and not mutate:
            report.emit("phase=authority-init:still-deferred")
            return
        if not mutate:
            journal.mark_deferred(
                PHASE_AUTHORITY,
                reason="non-live/plan mode does not initialize production authority",
            )
            report.emit("phase=authority-init:deferred-non-mutating")
            return
        if authority_present(self.config):
            auth = load_authority(self.config)
            if not auth.recovery_verified:
                raise ShipConfigError(
                    "existing authority lacks independent recovery verification"
                )
            journal.mark_completed(
                PHASE_AUTHORITY,
                identity={
                    "repository": self.config.repository,
                    "version": journal.version,
                    "tag": journal.tag,
                    "fingerprint_sha256": auth.fingerprint_sha256,
                },
                fingerprint_sha256=auth.fingerprint_sha256,
                reused=True,
            )
            report.emit("phase=authority-init:reused")
            report.emit(f"fingerprint_sha256={auth.fingerprint_sha256}")
            return
        journal.mark_in_progress(PHASE_AUTHORITY)
        auth = initialize_authority(
            self.config,
            store=self.hooks.secret_store,
            production=self.production_mode,
        )
        journal.mark_completed(
            PHASE_AUTHORITY,
            identity={
                "repository": self.config.repository,
                "version": journal.version,
                "tag": journal.tag,
                "fingerprint_sha256": auth.fingerprint_sha256,
            },
            fingerprint_sha256=auth.fingerprint_sha256,
            reused=False,
        )
        report.emit("phase=authority-init:created")
        report.emit(f"fingerprint_sha256={auth.fingerprint_sha256}")

    def _phase_pin(
        self, journal: ShipJournal, report: ShipReport, *, mutate: bool
    ) -> None:
        if journal.is_complete(PHASE_PIN):
            self._revalidate_completed(journal, PHASE_PIN, journal.version, journal.tag)
            report.emit("phase=public-pin-provision:skipped-complete")
            return
        if not mutate:
            journal.mark_deferred(
                PHASE_PIN,
                reason="non-live/plan mode does not modify repository pin",
            )
            report.emit("phase=public-pin-provision:deferred-non-mutating")
            return
        if not authority_present(self.config):
            raise ShipError("cannot provision pin without authority")
        journal.mark_in_progress(PHASE_PIN)
        fingerprint = provision_public_pin(self.config)
        journal.mark_completed(
            PHASE_PIN,
            identity={
                "repository": self.config.repository,
                "version": journal.version,
                "tag": journal.tag,
                "fingerprint_sha256": fingerprint,
            },
            fingerprint_sha256=fingerprint,
        )
        report.emit("phase=public-pin-provision:ok")
        report.emit(f"fingerprint_sha256={fingerprint}")

    def _phase_version(
        self,
        journal: ShipJournal,
        report: ShipReport,
        version: str,
        *,
        mutate: bool,
    ) -> None:
        if journal.is_complete(PHASE_VERSION):
            report.emit("phase=version-sync:skipped-complete")
            return
        if not mutate:
            journal.mark_deferred(
                PHASE_VERSION,
                reason="non-live/plan mode does not modify version files",
                planned_version=version,
            )
            report.emit("phase=version-sync:deferred-non-mutating")
            return
        journal.mark_in_progress(PHASE_VERSION)
        changed = apply_version(self.config.repo_root, version)
        # Track owned paths for later dirty-tree checks.
        owned = sorted(set(changed) | {"tools/release/kindle-release-public-key.hex"})
        journal.mark_completed(
            PHASE_VERSION,
            identity={
                "repository": self.config.repository,
                "version": version,
                "tag": journal.tag,
            },
            changed_paths=changed,
            owned_paths=owned,
        )
        report.emit("phase=version-sync:ok")
        report.emit(f"changed_paths={','.join(changed) if changed else 'none'}")

    def _phase_git_prepare(
        self,
        journal: ShipJournal,
        report: ShipReport,
        version: str,
        *,
        mutate: bool,
    ) -> None:
        if journal.is_complete(PHASE_GIT_PREPARE):
            report.emit("phase=git-prepare:skipped-complete")
            return
        changed = list(journal.fields.get("changed_paths") or [])
        pin_done = journal.is_complete(PHASE_PIN)
        stage_paths = release_commit_paths(
            changed_paths=changed,
            pin_provisioned=pin_done,
            owned_paths=list(journal.fields.get("owned_paths") or OWNED_RELEASE_PATHS),
        )
        plan = prepare_release_commit_plan(
            version=version, changed_paths=stage_paths, pin_provisioned=pin_done
        )
        if not mutate or not self.production_mode:
            journal.mark_planned(PHASE_GIT_PREPARE, commit_plan=plan)
            report.emit("phase=git-prepare:planned")
            report.emit(f"commit_subject={plan['subject']}")
            report.emit(f"commit_paths={','.join(stage_paths) if stage_paths else 'none'}")
            return
        journal.mark_in_progress(PHASE_GIT_PREPARE)
        assert_only_owned_dirty(
            self.config.repo_root,
            owned_paths=list(OWNED_RELEASE_PATHS),
            git=self.hooks.git,
        )
        expected_hex = None
        if pin_done and authority_present(self.config):
            expected_hex = load_authority(self.config).public_key.hex()
        result = commit_release_preparation(
            repo_root=self.config.repo_root,
            version=version,
            paths=stage_paths,
            pin_provisioned=pin_done,
            expected_public_key_hex=expected_hex,
            git=self.hooks.git,
        )
        journal.mark_completed(
            PHASE_GIT_PREPARE,
            identity={
                "repository": self.config.repository,
                "version": version,
                "tag": journal.tag,
                "source_revision": str(result.get("commit_sha") or ""),
                "fingerprint_sha256": str(
                    journal.fields.get("fingerprint_sha256") or ""
                ),
            },
            commit_plan=result,
            commit_sha=str(result.get("commit_sha") or ""),
            commit_paths=list(result.get("paths") or stage_paths),
        )
        report.emit("phase=git-prepare:ok")
        report.emit(f"commit_sha={result.get('commit_sha')}")
        report.emit(
            f"commit_paths={','.join(result.get('paths') or stage_paths)}"
        )

    def _phase_git_push(
        self,
        journal: ShipJournal,
        report: ShipReport,
        version: str,
        *,
        mutate: bool,
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
                "Public publisher pin + version sync. Private keys are not included.\n"
                "Users with UNPROVISIONED builds require a trusted manual reinstall."
            ),
        )
        if not mutate or not self.production_mode:
            journal.mark_planned(PHASE_GIT_PUSH, pr_plan=plan)
            report.emit("phase=git-push-pr:planned")
            return
        journal.mark_in_progress(PHASE_GIT_PUSH)
        git = self.hooks.git or SubprocessGit()
        commit_sha = str(journal.fields.get("commit_sha") or "")
        if len(commit_sha) != 40:
            raise ShipError("release preparation commit_sha missing before push")
        # Re-verify the local commit tree before push.
        pin_done = journal.is_complete(PHASE_PIN)
        expected_hex = None
        if pin_done and authority_present(self.config):
            expected_hex = load_authority(self.config).public_key.hex()
        verify_release_tree(
            repo_root=self.config.repo_root,
            commit_sha=commit_sha,
            version=version,
            expected_public_key_hex=expected_hex,
            require_pin=pin_done,
            git=git,
        )
        push_feature_branch(
            repo_root=self.config.repo_root,
            branch=self.config.feature_branch,
            git=git,
        )
        gh = self.hooks.gh or SubprocessGitHub()
        pr = create_or_update_pr(
            repo_root=self.config.repo_root,
            title=str(plan["title"]),
            head=str(plan["head"]),
            base=str(plan["base"]),
            body=str(plan["body"]),
            expected_head_sha=commit_sha,
            gh=gh,
        )
        wait_for_pr_checks(
            repo_root=self.config.repo_root,
            pr_number=int(pr["pr_number"]),
            gh=gh,
            poll_seconds=self.config.ci_poll_seconds,
            timeout_seconds=self.config.ci_timeout_seconds,
        )
        journal.mark_completed(
            PHASE_GIT_PUSH,
            identity={
                "repository": self.config.repository,
                "version": version,
                "tag": journal.tag,
                "source_revision": commit_sha,
            },
            pr_number=int(pr["pr_number"]),
            pr_url=str(pr.get("pr_url") or ""),
            pr_head_sha=str(pr.get("head_sha") or commit_sha),
        )
        report.emit(f"phase=git-push-pr:ok pr={pr['pr_number']}")

    def _phase_git_merge(
        self, journal: ShipJournal, report: ShipReport, *, mutate: bool
    ) -> None:
        if journal.is_complete(PHASE_GIT_MERGE):
            report.emit("phase=git-merge:skipped-complete")
            return
        pr_number = int(journal.fields.get("pr_number") or 0)
        plan = merge_pr_plan(pr_number=pr_number)
        if not mutate or not self.production_mode:
            journal.mark_planned(PHASE_GIT_MERGE, merge_plan=plan)
            report.emit("phase=git-merge:planned")
            return
        if pr_number <= 0:
            raise ShipError("pr_number missing; cannot merge")
        journal.mark_in_progress(PHASE_GIT_MERGE)
        gh = self.hooks.gh or SubprocessGitHub()
        pin_done = journal.is_complete(PHASE_PIN)
        expected_hex = None
        if pin_done and authority_present(self.config):
            expected_hex = load_authority(self.config).public_key.hex()
        result = merge_pr(
            repo_root=self.config.repo_root,
            pr_number=pr_number,
            gh=gh,
            git=self.hooks.git,
            base=self.config.base_branch,
            expected_head_sha=str(
                journal.fields.get("pr_head_sha")
                or journal.fields.get("commit_sha")
                or ""
            )
            or None,
            version=journal.version,
            expected_public_key_hex=expected_hex,
            require_pin=pin_done,
        )
        if not result.get("merged"):
            raise ShipError("merge did not confirm PR merged state")
        main_sha = str(result["main_sha"])
        if len(main_sha) != 40:
            raise ShipError("merge did not yield a 40-character main SHA")
        journal.mark_completed(
            PHASE_GIT_MERGE,
            identity={
                "repository": self.config.repository,
                "version": journal.version,
                "tag": journal.tag,
                "main_sha": main_sha,
            },
            merge_plan=result,
            main_sha=main_sha,
            merge_commit=str(result.get("merge_commit") or main_sha),
        )
        report.emit(f"phase=git-merge:ok main_sha={main_sha}")

    def _phase_ci_wait(
        self, journal: ShipJournal, report: ShipReport, *, mutate: bool
    ) -> None:
        if journal.is_complete(PHASE_CI_WAIT):
            report.emit("phase=ci-wait:skipped-complete")
            return
        main_sha = str(journal.fields.get("main_sha") or "")
        if not mutate or not self.production_mode:
            journal.mark_deferred(
                PHASE_CI_WAIT,
                reason="non-live; exact ci.yml wait deferred",
                main_sha=main_sha or "PENDING",
            )
            report.emit("phase=ci-wait:deferred-non-live")
            return
        if len(main_sha) != 40:
            raise ShipBlockedError("main_sha unset after merge; cannot bind ci.yml")
        journal.mark_in_progress(PHASE_CI_WAIT, main_sha=main_sha)
        gh = self.hooks.gh or SubprocessGitHub()
        run = wait_for_ci_success(
            repo_root=self.config.repo_root,
            head_sha=main_sha,
            gh=gh,
            poll_seconds=self.config.ci_poll_seconds,
            timeout_seconds=self.config.ci_timeout_seconds,
            repository=self.config.repository,
        )
        journal.mark_completed(
            PHASE_CI_WAIT,
            identity={
                "repository": self.config.repository,
                "version": journal.version,
                "tag": journal.tag,
                "main_sha": run.head_sha,
                "ci_run_id": run.run_id,
            },
            ci_run_id=run.run_id,
            main_sha=run.head_sha,
            ci_run_attempt=int(journal.fields.get("ci_run_attempt") or 1),
        )
        report.emit(f"phase=ci-wait:ok run_id={run.run_id}")

    def _phase_acquire(
        self,
        journal: ShipJournal,
        report: ShipReport,
        tag: str,
        *,
        mutate: bool,
    ) -> None:
        if journal.is_complete(PHASE_ACQUIRE):
            report.emit("phase=acquire:skipped-complete")
            return
        run_id = int(journal.fields.get("ci_run_id") or 0)
        out = self.config.work_dir / f"acquired-{tag}"
        if self.hooks.acquire_fn is not None and mutate:
            journal.mark_in_progress(PHASE_ACQUIRE)
            bundle = self.hooks.acquire_fn(tag=tag, output_dir=out, run_id=run_id)
            main_sha = str(journal.fields.get("main_sha") or "")
            source = getattr(
                getattr(bundle, "identity", None), "ci_head_sha", ""
            ) or str(journal.fields.get("source_revision") or "")
            if main_sha and source and main_sha != source:
                raise ShipError("acquired CI SHA does not match merged main SHA")
            journal.mark_completed(
                PHASE_ACQUIRE,
                identity={
                    "repository": self.config.repository,
                    "version": journal.version,
                    "tag": tag,
                    "main_sha": main_sha,
                    "source_revision": source,
                    "ci_run_id": run_id,
                    "scriptlet_sha256": getattr(
                        getattr(bundle, "identity", None), "scriptlet_sha256", ""
                    ),
                    "scriptlet_bytes": getattr(
                        getattr(bundle, "identity", None), "scriptlet_bytes", 0
                    ),
                },
                acquired_dir=str(
                    bundle.root if hasattr(bundle, "root") else out
                ),
                scriptlet_sha256=getattr(
                    getattr(bundle, "identity", None), "scriptlet_sha256", ""
                ),
                scriptlet_bytes=getattr(
                    getattr(bundle, "identity", None), "scriptlet_bytes", 0
                ),
                source_revision=source,
            )
            report.emit("phase=acquire:ok")
            return
        if not mutate or not self.production_mode:
            journal.mark_deferred(PHASE_ACQUIRE, reason="non-live acquire deferred")
            report.emit("phase=acquire:deferred-non-live")
            return
        kindle = self.config.repo_root / "deployment" / "kindle"
        if str(kindle) not in sys.path:
            sys.path.insert(0, str(kindle))
        from release_acquire import acquire_release

        if run_id <= 0:
            raise ShipBlockedError("ci_run_id missing; refuse acquire")
        if self.config.pin_path.read_bytes() == b"UNPROVISIONED\n":
            raise ShipError("refuse acquire while repository pin is UNPROVISIONED")
        journal.mark_in_progress(PHASE_ACQUIRE)
        if out.exists():
            shutil.rmtree(out)
        bundle = acquire_release(
            tag=tag,
            output_dir=out,
            run_id=run_id,
            repository=self.config.repository,
        )
        main_sha = str(journal.fields.get("main_sha") or "")
        if main_sha and bundle.identity.ci_head_sha != main_sha:
            raise ShipError("acquired CI SHA does not match merged main SHA")
        journal.mark_completed(
            PHASE_ACQUIRE,
            identity={
                "repository": self.config.repository,
                "version": journal.version,
                "tag": tag,
                "main_sha": main_sha,
                "source_revision": bundle.identity.ci_head_sha,
                "ci_run_id": run_id,
                "scriptlet_sha256": bundle.identity.scriptlet_sha256,
                "scriptlet_bytes": bundle.identity.scriptlet_bytes,
            },
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
        *,
        mutate: bool,
    ) -> None:
        if journal.is_complete(PHASE_SIGN):
            report.emit("phase=sign:skipped-complete")
            return
        if not mutate or not self.production_mode:
            if self.hooks.allow_local_mutations_for_tests and mutate:
                pass  # fall through for disposable local signing tests
            else:
                journal.mark_deferred(PHASE_SIGN, reason="non-live signing disabled")
                report.emit("phase=sign:deferred-non-live")
                return
        acquired_dir = Path(str(journal.fields.get("acquired_dir") or ""))
        if not acquired_dir.is_dir():
            journal.mark_deferred(PHASE_SIGN, reason="acquired directory missing")
            report.emit("phase=sign:deferred-missing-acquire")
            return
        out = self.config.work_dir / f"signed-{tag}"
        if out.exists():
            shutil.rmtree(out)
        journal.mark_in_progress(PHASE_SIGN)
        service = RestrictedSigningService(
            self.config,
            store=self.hooks.secret_store,
            production=self.production_mode,
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
            expected_main_sha=str(journal.fields.get("main_sha") or ""),
            ci_run_id=int(journal.fields.get("ci_run_id") or 0),
        )
        result = service.sign_release_candidate(request)
        journal.mark_completed(
            PHASE_SIGN,
            identity=self._identity_binding(journal, version, tag),
            signed_dir=str(result.signed_root),
            fingerprint_sha256=result.fingerprint_sha256,
            signed_scriptlet_sha256=result.scriptlet_sha256,
        )
        report.emit("phase=sign:ok")

    def _phase_verify(
        self,
        journal: ShipJournal,
        report: ShipReport,
        tag: str,
        *,
        mutate: bool,
    ) -> None:
        if journal.is_complete(PHASE_VERIFY):
            report.emit("phase=verify-dry-run:skipped-complete")
            return
        signed_dir = Path(str(journal.fields.get("signed_dir") or ""))
        acquired_dir = Path(str(journal.fields.get("acquired_dir") or ""))
        run_id = int(journal.fields.get("ci_run_id") or 0)
        if self.hooks.dry_run_fn is not None and mutate:
            dry = self.hooks.dry_run_fn(
                tag=tag,
                run_id=run_id,
                signed_assets=signed_dir,
                acquired_dir=acquired_dir,
            )
            outcome = getattr(getattr(dry, "outcome", None), "value", str(dry))
            if outcome != "complete-verification-pass":
                journal.mark_blocked(PHASE_VERIFY, dry_run_outcome=outcome)
                raise ShipBlockedError(
                    f"dry-run outcome {outcome!r} is not complete-verification-pass"
                )
            journal.mark_completed(
                PHASE_VERIFY,
                identity=self._identity_binding(journal, journal.version, tag),
                dry_run_outcome=outcome,
            )
            report.emit("phase=verify-dry-run:ok")
            return
        if not mutate or not self.production_mode:
            journal.mark_deferred(PHASE_VERIFY, reason="non-live verify deferred")
            report.emit("phase=verify-dry-run:deferred-non-live")
            return
        kindle = self.config.repo_root / "deployment" / "kindle"
        if str(kindle) not in sys.path:
            sys.path.insert(0, str(kindle))
        from release_orchestrate import DryRunOutcome, dry_run_release

        work = self.config.work_dir / f"dry-{tag}"
        if work.exists():
            shutil.rmtree(work)
        journal.mark_in_progress(PHASE_VERIFY)
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
            journal.mark_blocked(PHASE_VERIFY, dry_run_outcome=dry.outcome.value)
            raise ShipBlockedError(
                f"dry-run outcome {dry.outcome.value!r} is not complete-verification-pass"
            )
        if dry.pin_unprovisioned:
            raise ShipBlockedError("dry-run still reports UNPROVISIONED pin")
        journal.mark_completed(
            PHASE_VERIFY,
            identity=self._identity_binding(journal, journal.version, tag),
            dry_run_outcome=dry.outcome.value,
        )
        report.emit("phase=verify-dry-run:ok")

    def _phase_hardware(
        self,
        journal: ShipJournal,
        report: ShipReport,
        version: str,
        *,
        mutate: bool,
    ) -> None:
        if journal.is_complete(PHASE_HARDWARE):
            rec = journal.phases[PHASE_HARDWARE]
            if not rec.fields.get("production_evidence"):
                # Never treat a prior mock pass as completed production acceptance.
                journal.mark_blocked(
                    PHASE_HARDWARE,
                    reason="prior hardware record lacked production evidence",
                )
                raise ShipBlockedError(
                    "release-blocked: hardware-acceptance-unavailable "
                    "(mock evidence cannot authorize publication)"
                )
            report.emit("phase=hardware-acceptance:skipped-complete")
            return
        if not mutate:
            journal.mark_deferred(
                PHASE_HARDWARE,
                reason="non-live hardware acceptance deferred",
            )
            report.emit("phase=hardware-acceptance:deferred-non-live")
            return
        # Public release always requires hardware; env/--no-require-hardware cannot bypass.
        if not self.config.hardware_device_id:
            journal.mark_blocked(
                PHASE_HARDWARE,
                reason="hardware-acceptance-unavailable",
            )
            raise ShipBlockedError(
                "release-blocked: hardware-acceptance-unavailable "
                "(set IGNITE_SHIP_KINDLE_DEVICE_ID to a designated QA Kindle)"
            )
        harness = HardwareAcceptanceHarness(
            required_device_id=self.config.hardware_device_id
        )
        probe = self.hooks.hardware_probe
        if probe is None:
            from .hardware import NullHardwareProbe

            probe = NullHardwareProbe()
        if isinstance(probe, MockHardwareProbe):
            journal.mark_blocked(
                PHASE_HARDWARE,
                reason="mock-hardware-cannot-authorize-publication",
            )
            raise ShipBlockedError(
                "release-blocked: hardware-acceptance-unavailable "
                "(mock hardware results cannot authorize production publication)"
            )
        if not authority_present(self.config):
            raise ShipError("authority required before hardware acceptance")
        auth = load_authority(self.config)
        journal.mark_in_progress(PHASE_HARDWARE)
        result = harness.run(
            signed_root=Path(str(journal.fields.get("signed_dir") or ".")),
            public_key_hex=auth.public_key.hex(),
            version=version,
            probe=probe,
        )
        if not result.authorizes_publication:
            journal.mark_blocked(
                PHASE_HARDWARE,
                device_id=result.device_id,
                details=list(result.details),
                production_evidence=result.production_evidence,
            )
            raise ShipBlockedError(
                "release-blocked: hardware-acceptance-unavailable "
                "(acceptance did not provide production evidence)"
            )
        journal.mark_completed(
            PHASE_HARDWARE,
            identity=self._identity_binding(journal, version, journal.tag),
            device_id=result.device_id,
            firmware=result.firmware,
            production_evidence=True,
            details=list(result.details),
        )
        report.emit("phase=hardware-acceptance:ok")

    def _phase_publish(
        self, journal: ShipJournal, report: ShipReport, *, mutate: bool
    ) -> None:
        if journal.is_complete(PHASE_PUBLISH):
            self._revalidate_completed(
                journal, PHASE_PUBLISH, journal.version, journal.tag
            )
            report.emit("phase=publish:skipped-complete")
            return
        # Require hardware completed with production evidence before publish.
        hw = journal.phases.get(PHASE_HARDWARE)
        if (
            mutate
            and self.production_mode
            and (
                hw is None
                or hw.state != PhaseState.COMPLETED
                or not hw.fields.get("production_evidence")
            )
        ):
            journal.mark_blocked(
                PHASE_PUBLISH,
                reason="hardware acceptance missing or not production evidence",
            )
            raise ShipBlockedError(
                "release-blocked: hardware-acceptance-unavailable"
            )
        if not mutate or not self.production_mode:
            journal.mark_deferred(
                PHASE_PUBLISH,
                reason="non-live publication deferred",
            )
            report.emit("phase=publish:deferred-non-live")
            return
        journal.mark_in_progress(PHASE_PUBLISH)
        gh = self.hooks.gh or SubprocessGitHub()
        if self.hooks.publish_fn is not None:
            # Unit-test seam only. Integration tests must exercise the real shell path.
            result = self.hooks.publish_fn(journal=journal, gh=gh)
            verified = bool(
                getattr(result, "independently_verified", False)
                or (
                    isinstance(result, dict)
                    and result.get("independently_verified")
                )
            )
            if not verified:
                journal.mark_failed(
                    PHASE_PUBLISH, executed=True, independently_verified=False
                )
                raise ShipError(
                    "publication not independently verified; refusing success"
                )
            release_url = str(
                getattr(result, "release_url", None)
                or (result.get("release_url") if isinstance(result, dict) else "")
                or ""
            )
            asset_digests = dict(
                getattr(result, "asset_digests", None)
                or (result.get("asset_digests") if isinstance(result, dict) else {})
                or {}
            )
            asset_names = list(
                getattr(result, "asset_names", None)
                or (result.get("asset_names") if isinstance(result, dict) else [])
                or sorted(asset_digests)
            )
            recovered = bool(
                getattr(result, "recovered_existing", False)
                or (
                    isinstance(result, dict) and result.get("recovered_existing")
                )
            )
        else:
            kindle = self.config.repo_root / "deployment" / "kindle"
            if str(kindle) not in sys.path:
                sys.path.insert(0, str(kindle))
            from release_acquire import load_acquired_bundle
            from release_orchestrate import match_signed_to_acquired

            acquired = load_acquired_bundle(
                Path(str(journal.fields["acquired_dir"]))
            )
            signed = match_signed_to_acquired(
                signed=Path(str(journal.fields["signed_dir"])),
                identity=acquired.identity,
            )
            main_sha = str(
                journal.fields.get("main_sha")
                or journal.fields.get("source_revision")
                or ""
            )
            if main_sha and acquired.identity.ci_head_sha != main_sha:
                raise ShipError(
                    "acquired CI SHA does not match merged main before publish"
                )

            work_dir = self.config.work_dir / "promote-publish"
            work_dir.mkdir(parents=True, exist_ok=True)
            nine_dest = work_dir / "nine-file-candidate"
            inventory = assemble_nine_file_inventory(
                acquired=acquired,
                signed=signed,
                dest=nine_dest,
            )
            # Persist immutable expected inventory before any GitHub mutation.
            journal.mark_in_progress(
                PHASE_PUBLISH,
                expected_asset_names=list(inventory.asset_names),
                expected_asset_digests=dict(inventory.asset_digests),
                nine_file_candidate=str(inventory.root),
                publish_commit=inventory.commit,
                publish_run_id=inventory.run_id,
            )
            report.emit(
                f"nine_file_inventory={','.join(inventory.asset_names)}"
            )

            env = build_publish_env(
                acquired=acquired,
                signed=signed,
                work_dir=work_dir,
            )
            # Seed exact acquired CI bytes into the promote work directory so a
            # fresh WORK is valid for ``publish`` (guard→resolve→verify→create).
            seed_promote_work_from_acquired(acquired=acquired, work_dir=work_dir)

            from .publish import PublishResult

            try:
                result = publish_release(
                    repo_root=self.config.repo_root,
                    env=env,
                    inventory=inventory,
                    dry_run_only=False,
                    gh=gh,
                    promote_command="publish",
                    allow_recover_existing=True,
                )
            except ShipError as exc:
                # After create / during download: try recover from journal inventory.
                try:
                    verification = verify_published_release(
                        repo_root=self.config.repo_root,
                        tag=journal.tag,
                        expected_commit=inventory.commit,
                        gh=gh,
                        expected_asset_names=inventory.asset_names,
                        expected_digests=inventory.asset_digests,
                        local_asset_dir=inventory.root,
                    )
                    result = PublishResult(
                        tag=journal.tag,
                        run_id=inventory.run_id,
                        commit=inventory.commit,
                        release_url=str(verification.get("release_url") or ""),
                        executed=True,
                        inventory_ok=True,
                        independently_verified=True,
                        asset_digests=dict(verification.get("asset_digests") or {}),
                        asset_names=tuple(
                            verification.get("asset_names") or inventory.asset_names
                        ),
                        recovered_existing=True,
                        promote_command="recover-existing",
                    )
                    report.emit(f"publish_recover_after_error={exc}")
                except ShipError:
                    journal.mark_failed(
                        PHASE_PUBLISH,
                        executed=False,
                        independently_verified=False,
                        error=str(exc),
                        expected_asset_names=list(inventory.asset_names),
                        expected_asset_digests=dict(inventory.asset_digests),
                    )
                    raise

            verified = bool(result.independently_verified)
            if not verified:
                journal.mark_failed(
                    PHASE_PUBLISH,
                    executed=bool(result.executed),
                    independently_verified=False,
                    expected_asset_names=list(inventory.asset_names),
                    expected_asset_digests=dict(inventory.asset_digests),
                )
                raise ShipError(
                    "publication not independently verified; refusing success"
                )
            release_url = result.release_url or ""
            asset_digests = dict(result.asset_digests)
            asset_names = list(result.asset_names or inventory.asset_names)
            recovered = bool(result.recovered_existing)

        journal.mark_completed(
            PHASE_PUBLISH,
            identity=self._identity_binding(
                journal, journal.version, journal.tag
            ),
            executed=True,
            published=True,
            independently_verified=True,
            release_url=release_url,
            commit=str(
                journal.fields.get("publish_commit")
                or journal.fields.get("main_sha")
                or journal.fields.get("source_revision")
                or ""
            ),
            asset_digests=asset_digests,
            asset_names=asset_names,
            expected_asset_digests=dict(
                journal.fields.get("expected_asset_digests") or asset_digests
            ),
            recovered_existing=recovered,
        )
        report.emit("phase=publish:ok")
        report.emit("published=true")
        if recovered:
            report.emit("publish_recovered_existing=true")
        if release_url:
            report.emit(f"release_url={release_url}")
