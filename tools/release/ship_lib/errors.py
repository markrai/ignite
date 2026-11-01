"""Ship orchestration errors (no secret material in messages)."""

from __future__ import annotations


class ShipError(RuntimeError):
    """Fail-closed ship refusal with a safe, operator-facing message."""


class ShipConfigError(ShipError):
    """Missing or unsafe local configuration (secret store, recovery, etc.)."""


class ShipBlockedError(ShipError):
    """External policy or mandatory gate blocked the release."""


class ShipRetryableError(ShipError):
    """Transient failure; journaled progress may be resumed."""
