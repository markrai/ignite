"""Ignite ship orchestration library (Slice 3D-B2a.4.3q).

Autonomous release workflow primitives. Live GitHub mutation, real production-key
activation, and Kindle deployment are gated behind explicit configuration and are
not exercised by the development test suite.
"""

from __future__ import annotations

__all__ = [
    "ShipError",
    "ShipOutcome",
    "SHIP_EXIT",
]

from .errors import ShipError
from .outcomes import SHIP_EXIT, ShipOutcome
