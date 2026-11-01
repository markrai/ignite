"""Machine-readable ship outcomes and exit codes."""

from __future__ import annotations

from enum import Enum


class ShipOutcome(str, Enum):
    SUCCESS = "success"
    READY = "ready"
    BLOCKED_CONFIG = "blocked-config"
    BLOCKED_POLICY = "blocked-policy"
    BLOCKED_HARDWARE = "blocked-hardware"
    BLOCKED_CI = "blocked-ci"
    BLOCKED_VERIFY = "blocked-verify"
    PARTIAL = "partial-resume-available"
    FAILED = "failed"
    NOT_PERFORMED = "not-performed"


SHIP_EXIT: dict[ShipOutcome, int] = {
    ShipOutcome.SUCCESS: 0,
    ShipOutcome.READY: 0,
    ShipOutcome.BLOCKED_CONFIG: 10,
    ShipOutcome.BLOCKED_POLICY: 11,
    ShipOutcome.BLOCKED_HARDWARE: 12,
    ShipOutcome.BLOCKED_CI: 13,
    ShipOutcome.BLOCKED_VERIFY: 14,
    ShipOutcome.PARTIAL: 2,
    ShipOutcome.FAILED: 1,
    ShipOutcome.NOT_PERFORMED: 3,
}


# Stable phase names for the journal / resume.
PHASE_AUTHORITY = "authority-init"
PHASE_PIN = "public-pin-provision"
PHASE_VERSION = "version-sync"
PHASE_GIT_PREPARE = "git-prepare"
PHASE_GIT_PUSH = "git-push-pr"
PHASE_GIT_MERGE = "git-merge"
PHASE_CI_WAIT = "ci-wait"
PHASE_ACQUIRE = "acquire"
PHASE_SIGN = "sign"
PHASE_VERIFY = "verify-dry-run"
PHASE_HARDWARE = "hardware-acceptance"
PHASE_PUBLISH = "publish"
PHASE_DONE = "done"

SHIP_PHASES: tuple[str, ...] = (
    PHASE_AUTHORITY,
    PHASE_PIN,
    PHASE_VERSION,
    PHASE_GIT_PREPARE,
    PHASE_GIT_PUSH,
    PHASE_GIT_MERGE,
    PHASE_CI_WAIT,
    PHASE_ACQUIRE,
    PHASE_SIGN,
    PHASE_VERIFY,
    PHASE_HARDWARE,
    PHASE_PUBLISH,
    PHASE_DONE,
)
