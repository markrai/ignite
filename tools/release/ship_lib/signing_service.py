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


@dataclass(frozen=True)
class SignReleaseResult:
    signed_root: Path
    promote_assets: Path
    public_key_path: Path
    fingerprint_sha256: str


class RestrictedSigningService:
    """Signs only complete, policy-validated release candidates.

    Does not expose a generic "sign arbitrary bytes" API.
    """

    def __init__(
        self,
        config: ShipConfig,
        *,
        store: SecretStore | None = None,
        expected_repository: str = "markrai/ignite",
        expected_workflow: str = ".github/workflows/ci.yml",
    ) -> None:
        self._config = config
        self._store = store
        self._expected_repository = expected_repository
        self._expected_workflow = expected_workflow

    def sign_release_candidate(self, request: SignReleaseRequest) -> SignReleaseResult:
        auth = load_authority(self._config)
        self._enforce_policy(request, auth)
        store = self._store or select_secret_store(
            allow_insecure=self._config.allow_insecure_store
        )
        if str(_KINDLE) not in sys.path:
            sys.path.insert(0, str(_KINDLE))
        if str(_RELEASE) not in sys.path:
            sys.path.insert(0, str(_RELEASE))
        from release_orchestrate import sign_acquired

        work = Path(tempfile.mkdtemp(prefix="ignite-ship-sign-"))
        try:
            private_pem = export_private_pem_to_work(
                self._config, work / "key", store=store
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
            )
        finally:
            shutil.rmtree(work, ignore_errors=True)

    def _enforce_policy(
        self, request: SignReleaseRequest, auth: PublisherAuthority
    ) -> None:
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
        if len(request.scriptlet_sha256) != 64 or any(
            c not in "0123456789abcdef" for c in request.scriptlet_sha256
        ):
            raise ShipError("signing refused: malformed scriptlet digest")
        if request.scriptlet_bytes <= 0:
            raise ShipError("signing refused: invalid scriptlet size")
        if not request.acquired_root.is_dir():
            raise ShipError("signing refused: acquired root missing")
        # Bind request digests to on-disk Scriptlet before unsealing the key.
        identity_path = request.acquired_root / "release-identity.v1"
        if not identity_path.is_file():
            raise ShipError("signing refused: release identity missing")
        identity_text = identity_path.read_text(encoding="ascii")
        if f"ci_head_sha={request.source_revision}" not in identity_text:
            raise ShipError("signing refused: source revision mismatch vs identity")
        if f"scriptlet_sha256={request.scriptlet_sha256}" not in identity_text:
            raise ShipError("signing refused: scriptlet digest mismatch vs identity")
        scriptlet = _find_scriptlet(request.acquired_root, request.version)
        digest = hashlib.sha256(scriptlet.read_bytes()).hexdigest()
        if digest != request.scriptlet_sha256:
            raise ShipError("signing refused: Scriptlet bytes do not match request digest")
        if scriptlet.stat().st_size != request.scriptlet_bytes:
            raise ShipError("signing refused: Scriptlet size mismatch")
        # Authority public key must match the repository pin once provisioned.
        pin = self._config.pin_path.read_bytes()
        if pin != b"UNPROVISIONED\n":
            if pin != auth.public_key.hex().encode("ascii") + b"\n":
                raise ShipError(
                    "signing refused: authority public key does not match repository pin"
                )


def _find_scriptlet(acquired_root: Path, version: str) -> Path:
    name = f"Install Ignite v{version}.sh"
    direct = acquired_root / "ci-artifacts" / name
    if direct.is_file():
        return direct
    matches = list(acquired_root.rglob(name))
    if len(matches) != 1:
        raise ShipError("signing refused: Scriptlet path ambiguous or missing")
    return matches[0]
