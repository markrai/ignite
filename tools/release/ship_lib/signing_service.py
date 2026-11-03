"""Restricted local signing service — policy-gated release signing only."""

from __future__ import annotations

import hashlib
import shutil
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

from .authority import PublisherAuthority, export_private_pem_to_work, load_authority
from .config import ShipConfig
from .errors import ShipError
from .secret_store import SecretStore, select_secret_store

_ROOT = Path(__file__).resolve().parents[3]
_KINDLE = _ROOT / "deployment" / "kindle"
_RELEASE = _ROOT / "tools" / "release"


@dataclass(frozen=True)
class SignReleaseRequest:
    """Fully validated release-candidate identity presented to the signer."""

    tag: str
    version: str
    source_revision: str
    repository: str
    workflow: str
    scriptlet_sha256: str
    scriptlet_bytes: int
    acquired_root: Path
    output_dir: Path
    expected_main_sha: str = ""
    ci_run_id: int = 0


@dataclass(frozen=True)
class SignReleaseResult:
    signed_root: Path
    promote_assets: Path
    public_key_path: Path
    fingerprint_sha256: str
    scriptlet_sha256: str


class RestrictedSigningService:
    """Signs only complete, policy-validated release candidates.

    Does not expose a generic "sign arbitrary bytes" API.
    Production signing requires a provisioned official publisher pin that matches
    the active authority, and acquired CI identity that matches the merged main
    revision.
    """

    def __init__(
        self,
        config: ShipConfig,
        *,
        store: SecretStore | None = None,
        expected_repository: str = "markrai/ignite",
        expected_workflow: str = ".github/workflows/ci.yml",
        production: bool = False,
    ) -> None:
        self._config = config
        self._store = store
        self._expected_repository = expected_repository
        self._expected_workflow = expected_workflow
        self._production = production

    def sign_release_candidate(self, request: SignReleaseRequest) -> SignReleaseResult:
        auth = load_authority(self._config)
        self._enforce_policy(request, auth)
        if self._store is None:
            store = select_secret_store(
                allow_insecure=self._config.allow_insecure_store
                and not self._production,
                production=self._production,
            )
        else:
            store = self._store
            if self._production and getattr(store, "name", "") == "insecure-file":
                raise ShipError(
                    "insecure signing store is forbidden for live / production signing"
                )
        if str(_KINDLE) not in sys.path:
            sys.path.insert(0, str(_KINDLE))
        if str(_RELEASE) not in sys.path:
            sys.path.insert(0, str(_RELEASE))
        from release_orchestrate import sign_acquired

        work = Path(tempfile.mkdtemp(prefix="ignite-ship-sign-"))
        try:
            private_pem = export_private_pem_to_work(
                self._config,
                work / "key",
                store=store,
                production=self._production,
            )
            signed = sign_acquired(
                acquired=request.acquired_root,
                private_key=private_pem,
                public_key=auth.public_key_path,
                output_dir=request.output_dir,
            )
            return SignReleaseResult(
                signed_root=signed.root,
                promote_assets=signed.promote_assets,
                public_key_path=auth.public_key_path,
                fingerprint_sha256=auth.fingerprint_sha256,
                scriptlet_sha256=request.scriptlet_sha256,
            )
        finally:
            shutil.rmtree(work, ignore_errors=True)

    def _enforce_policy(
        self, request: SignReleaseRequest, auth: PublisherAuthority
    ) -> None:
        if not auth.recovery_verified:
            raise ShipError("signing refused: authority recovery not independently verified")
        if not request.tag.startswith("v"):
            raise ShipError("release tag must start with v")
        if request.tag != f"v{request.version}":
            raise ShipError("tag does not match version")
        if request.repository != self._expected_repository:
            raise ShipError("signing refused: unexpected repository")
        if request.workflow != self._expected_workflow:
            raise ShipError("signing refused: unexpected workflow")
        if len(request.source_revision) != 40 or any(
            c not in "0123456789abcdef" for c in request.source_revision
        ):
            raise ShipError("signing refused: malformed source revision")
        if request.expected_main_sha:
            if request.expected_main_sha != request.source_revision:
                raise ShipError(
                    "signing refused: acquired CI revision does not match merged main SHA"
                )
        if len(request.scriptlet_sha256) != 64 or any(
            c not in "0123456789abcdef" for c in request.scriptlet_sha256
        ):
            raise ShipError("signing refused: malformed scriptlet digest")
        if request.scriptlet_bytes <= 0:
            raise ShipError("signing refused: invalid scriptlet size")
        if request.ci_run_id <= 0 and self._production:
            raise ShipError("signing refused: missing CI run id")

        # Official pin must be provisioned and match authority — no UNPROVISIONED signing.
        pin = self._config.pin_path.read_bytes()
        if pin == b"UNPROVISIONED\n":
            raise ShipError(
                "signing refused: repository pin is UNPROVISIONED; "
                "production signing requires a provisioned official publisher pin"
            )
        if pin != auth.public_key.hex().encode("ascii") + b"\n":
            raise ShipError(
                "signing refused: authority public key does not match repository pin"
            )

        if not request.acquired_root.is_dir():
            raise ShipError("signing refused: acquired root missing")

        identity_path = request.acquired_root / "release-identity.v1"
        if not identity_path.is_file():
            raise ShipError("signing refused: release identity missing")
        identity_text = identity_path.read_text(encoding="ascii")
        if f"ci_head_sha={request.source_revision}" not in identity_text:
            raise ShipError("signing refused: source revision mismatch vs identity")
        if f"scriptlet_sha256={request.scriptlet_sha256}" not in identity_text:
            raise ShipError("signing refused: scriptlet digest mismatch vs identity")
        if f"tag={request.tag}" not in identity_text and f"version={request.version}" not in identity_text:
            # Soft check: prefer explicit tag when present.
            if "tag=" in identity_text and f"tag={request.tag}" not in identity_text:
                raise ShipError("signing refused: tag mismatch vs release identity")
        scriptlet = _find_scriptlet(request.acquired_root, request.version)
        digest = hashlib.sha256(scriptlet.read_bytes()).hexdigest()
        if digest != request.scriptlet_sha256:
            raise ShipError("signing refused: Scriptlet bytes do not match request digest")
        if scriptlet.stat().st_size != request.scriptlet_bytes:
            raise ShipError("signing refused: Scriptlet size mismatch")


def _find_scriptlet(acquired_root: Path, version: str) -> Path:
    name = f"Install Ignite v{version}.sh"
    direct = acquired_root / "ci-artifacts" / name
    if direct.is_file():
        return direct
    matches = list(acquired_root.rglob(name))
    if len(matches) != 1:
        raise ShipError("signing refused: Scriptlet path ambiguous or missing")
    return matches[0]
