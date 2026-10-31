#!/usr/bin/env python3
"""Authenticated release and publication evidence (Slice 3D-B2a.4.3f).

Cryptographic chain (verification-only):

  test trust-root signature
    → publication-evidence manifest
    → customer Scriptlet / payload identity
    → installed B identity
    → independently authenticated retained A identity

Reuses OpenSSL Ed25519 verification from ``kindle_release_sign.py``, component
profiles from B2a.4.3c, Scriptlet equivalence from B2a.4.3d, and disposable
installer observation from B2a.4.3e.

Never authorizes installation, execution, promotion, rollback, or first-launch
admission. Production ``publisher_authentication`` remains unavailable.
"""

from __future__ import annotations

import hashlib
import importlib.util
import os
import re
import stat
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_REPO = _HERE.parents[1]
_SFX = _HERE / "sfx"
for extra in (_HERE, _SFX, str(_REPO / "tools" / "release")):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from release_component_profiles import (  # noqa: E402
    CANDIDATE_B_CLASS,
    CANDIDATE_B_DESTINATION,
    COMPONENT_ROLES,
    PACKAGE_CLASS,
    PACKAGE_RELATIVE,
    RETAINED_A_FUTURE_CLASS,
    generate_profiles,
    inspect_package_components,
)
from verify_sfx_payload_equivalence import (  # noqa: E402
    expected_member_mode,
    verify_equivalence,
)
from installer_publication_rehearsal import (  # noqa: E402
    EXECUTABLE_ROLES,
    ReleaseEvidence,
    classify_publication_state,
    inspect_pending_health,
    observe_candidate_b,
    observe_retained_a,
)
EVIDENCE_HEADER = "IGNITE_PUBLICATION_EVIDENCE_V1"
EVIDENCE_SCHEMA = "1"
SIGNATURE_SCHEMA = "ignite-kindle-publication-evidence-signature-v1"
REPORT_HEADER = "IGNITE_AUTHENTICATED_PUBLICATION_EVIDENCE_V1"
REPORT_SCHEMA = "1"

MAX_EVIDENCE_BYTES = 64 * 1024
MAX_SIGNATURE_BYTES = 512
MAX_SCRIPTLET_BYTES = 768 * 1024 * 1024
MAX_COMPONENT_BYTES = 512 * 1024 * 1024
READ_CHUNK = 1024 * 1024

_VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$")
_SHA_RE = re.compile(r"^[0-9a-f]{64}$")
_BYTES_RE = re.compile(r"^(0|[1-9][0-9]*)$")
_MODE_RE = re.compile(r"^(-|0[0-7]{3})$")
_COMPONENT_RE = re.compile(
    r"^component=(?P<role>[a-z0-9-]+)\|(?P<presence>file|absent)\|"
    r"(?P<size>0|[1-9][0-9]*)\|(?P<digest>-|[0-9a-f]{64})\|"
    r"(?P<klass>direct-copy|optional|transformed|generated|shared)\|"
    r"(?P<mode>-|0[0-7]{3})\|(?P<mapping>[A-Za-z0-9_./ -]+|-)$"
)

# Production pin path — never used as a success trust root in this module.
_PRODUCTION_PIN = _REPO / "tools" / "release" / "kindle-release-public-key.hex"


class AuthEvidenceError(Exception):
    pass


def _load_release_sign():
    path = _REPO / "tools" / "release" / "kindle_release_sign.py"
    spec = importlib.util.spec_from_file_location("kindle_release_sign", path)
    if spec is None or spec.loader is None:
        raise AuthEvidenceError("cannot load kindle_release_sign.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


_SIGN = _load_release_sign()


@dataclass(frozen=True)
class PublicationTestTrustRoot:
    """Ephemeral Ed25519 trust root for tests only.

    Never written into the production pin file. Never accepted by official
    production verification paths in this module.
    """

    label: str
    public_key: bytes
    private_key_path: Path | None = None

    def __post_init__(self) -> None:
        if len(self.public_key) != 32:
            raise AuthEvidenceError("test trust root public key must be 32 raw bytes")
        if self.public_key in _SIGN.KNOWN_TEST_PUBLIC_KEYS:
            # RFC fixtures are blocked from official pin; allowed as disposable
            # test roots only when explicitly constructed for negative tests.
            pass


# Back-compat alias (not collected by pytest: lowercase start would also work).
@dataclass(frozen=True)
class ComponentClaim:
    role: str
    presence: str
    size: int
    sha256: str
    classification: str
    mode: str
    mapping: str


@dataclass(frozen=True)
class AuthenticatedPublicationEvidence:
    """Claims only after signature verification under an explicit TestTrustRoot."""

    version: str
    tag: str
    scriptlet_artifact: str
    scriptlet_bytes: int
    scriptlet_sha256: str
    payload_dirname: str
    package_profile_sha256: str
    components: tuple[ComponentClaim, ...]
    trust_root_label: str
    raw_manifest: bytes


def consume_regular_file_fd(path: Path, *, max_size: int) -> tuple[bytes, int, str, int]:
    """Single O_NOFOLLOW open: return (bytes, size, sha256, mode)."""
    path_str = os.fspath(path)
    try:
        before = os.lstat(path_str)
    except OSError as exc:
        raise AuthEvidenceError(f"cannot lstat {path}: {exc}") from exc
    if stat.S_ISLNK(before.st_mode):
        raise AuthEvidenceError(f"symlink refused: {path}")
    if not stat.S_ISREG(before.st_mode):
        raise AuthEvidenceError(f"special file refused: {path}")
    if before.st_nlink != 1:
        raise AuthEvidenceError(f"hard link refused: {path}")
    if before.st_size <= 0 or before.st_size > max_size:
        raise AuthEvidenceError(f"size out of bounds: {path}")

    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    if hasattr(os, "O_BINARY"):
        flags |= os.O_BINARY
    try:
        fd = os.open(path_str, flags)
    except OSError as exc:
        raise AuthEvidenceError(f"cannot open {path}: {exc}") from exc
    try:
        opened = os.fstat(fd)
        if not (
            stat.S_ISREG(opened.st_mode)
            and opened.st_nlink == 1
            and opened.st_size == before.st_size
        ):
            raise AuthEvidenceError(f"identity changed before read: {path}")
        if sys.platform != "win32":
            if opened.st_dev != before.st_dev or opened.st_ino != before.st_ino:
                raise AuthEvidenceError(f"identity changed before read: {path}")
        hasher = hashlib.sha256()
        chunks: list[bytes] = []
        remaining = opened.st_size
        while remaining > 0:
            chunk = os.read(fd, min(READ_CHUNK, remaining))
            if not chunk:
                raise AuthEvidenceError(f"truncated while reading: {path}")
            hasher.update(chunk)
            chunks.append(chunk)
            remaining -= len(chunk)
        if os.read(fd, 1):
            raise AuthEvidenceError(f"file grew while reading: {path}")
        after = os.fstat(fd)
        if after.st_size != opened.st_size or after.st_nlink != 1:
            raise AuthEvidenceError(f"substituted while reading: {path}")
        data = b"".join(chunks)
        if len(data) != opened.st_size:
            raise AuthEvidenceError(f"size mismatch: {path}")
        return data, opened.st_size, hasher.hexdigest(), opened.st_mode
    finally:
        os.close(fd)


def generate_test_trust_root(
    work_dir: Path, *, label: str = "test-publisher"
) -> PublicationTestTrustRoot:
    """Create a disposable Ed25519 keypair under work_dir (never production pin)."""
    work_dir.mkdir(parents=True, exist_ok=True)
    private_key = work_dir / f"{label}-private.pem"
    public_der = work_dir / f"{label}-public.der"
    if private_key.exists() or public_der.exists():
        raise AuthEvidenceError("refusing to overwrite existing test key material")
    try:
        r1 = subprocess.run(
            ["openssl", "genpkey", "-algorithm", "ED25519", "-out", os.fspath(private_key)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        r2 = subprocess.run(
            [
                "openssl",
                "pkey",
                "-in",
                os.fspath(private_key),
                "-pubout",
                "-outform",
                "DER",
                "-out",
                os.fspath(public_der),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    except OSError as exc:
        raise AuthEvidenceError(f"cannot execute OpenSSL for test key: {exc}") from exc
    if r1.returncode != 0 or r2.returncode != 0:
        raise AuthEvidenceError("OpenSSL failed to generate test trust root")
    der = public_der.read_bytes()
    if len(der) != 44 or der[:12] != _SIGN.PUBLIC_KEY_DER_PREFIX:
        raise AuthEvidenceError("unexpected test public key DER")
    public_key = der[12:]
    # Refuse writing into production pin.
    if _PRODUCTION_PIN.resolve() == public_der.resolve():
        raise AuthEvidenceError("refusing to use production pin path for test key")
    return PublicationTestTrustRoot(
        label=label, public_key=public_key, private_key_path=private_key
    )


def assert_production_pin_unprovisioned() -> None:
    raw = _PRODUCTION_PIN.read_bytes()
    if raw != _SIGN.UNPROVISIONED:
        raise AuthEvidenceError("production public-key pin is not UNPROVISIONED")


def _mode_token_for_package_role(role: str, presence: str, rel: str | None) -> str:
    if presence != "file" or rel is None:
        return "-"
    # Evidence records the production Scriptlet/member mode contract, not host FS bits.
    return f"{expected_member_mode(rel):04o}"


def build_publication_evidence_manifest(
    *,
    version: str,
    payload: Path,
    scriptlet: Path,
) -> bytes:
    """Build canonical publication-evidence bytes from verified release artifacts."""
    if not _VERSION_RE.fullmatch(version):
        raise AuthEvidenceError("noncanonical version")
    tag = f"v{version}"
    expected_scriptlet = f"Install Ignite v{version}.sh"
    if scriptlet.name != expected_scriptlet:
        raise AuthEvidenceError(f"scriptlet basename must be {expected_scriptlet!r}")
    if payload.name != f"Ignite-Kindle-{version}":
        raise AuthEvidenceError("payload directory name mismatch")

    scriptlet_bytes, scriptlet_size, scriptlet_sha, _mode = consume_regular_file_fd(
        scriptlet, max_size=MAX_SCRIPTLET_BYTES
    )
    del scriptlet_bytes  # identity is size+digest; avoid retaining full Scriptlet

    digests = inspect_package_components(payload)
    profiles = generate_profiles(payload, version)
    package_profile = profiles["package"]
    profile_sha = hashlib.sha256(package_profile).hexdigest()

    lines = [
        EVIDENCE_HEADER,
        f"schema={EVIDENCE_SCHEMA}",
        f"version={version}",
        f"tag={tag}",
        f"scriptlet_artifact={expected_scriptlet}",
        f"scriptlet_bytes={scriptlet_size}",
        f"scriptlet_sha256={scriptlet_sha}",
        f"payload_dirname={payload.name}",
        f"package_profile_sha256={profile_sha}",
        f"component_role_count={len(COMPONENT_ROLES)}",
    ]
    for role in COMPONENT_ROLES:
        presence, size, digest = digests[role]
        klass = PACKAGE_CLASS[role]
        rel = PACKAGE_RELATIVE[role]
        mode = _mode_token_for_package_role(role, presence, rel)
        mapping = CANDIDATE_B_DESTINATION[role]
        if presence == "file":
            lines.append(
                f"component={role}|file|{size}|{digest}|{klass}|{mode}|{mapping}"
            )
        else:
            lines.append(f"component={role}|absent|0|-|{klass}|-|{mapping}")
    lines.append("publisher_authentication_scope=test-trust-root-only")
    raw = ("\n".join(lines) + "\n").encode("ascii")
    if len(raw) > MAX_EVIDENCE_BYTES:
        raise AuthEvidenceError("publication evidence exceeds size bound")
    return raw


def signature_envelope(signature: bytes) -> bytes:
    if len(signature) != 64:
        raise AuthEvidenceError("Ed25519 signature must be 64 bytes")
    return f"schema={SIGNATURE_SCHEMA}\nsignature={signature.hex()}\n".encode("ascii")


def parse_signature_envelope(raw: bytes) -> bytes:
    if len(raw) > MAX_SIGNATURE_BYTES or b"\r" in raw:
        raise AuthEvidenceError("signature envelope refused")
    text = raw.decode("ascii")
    if not text.endswith("\n"):
        raise AuthEvidenceError("signature envelope must end with LF")
    lines = text[:-1].split("\n")
    if len(lines) != 2:
        raise AuthEvidenceError("signature envelope must be exactly two lines")
    if lines[0] != f"schema={SIGNATURE_SCHEMA}":
        raise AuthEvidenceError("unsupported signature schema")
    if not lines[1].startswith("signature="):
        raise AuthEvidenceError("signature field missing")
    hex_sig = lines[1][len("signature=") :]
    if not re.fullmatch(r"[0-9a-f]{128}", hex_sig):
        raise AuthEvidenceError("malformed signature hex")
    return bytes.fromhex(hex_sig)


def sign_publication_evidence(
    manifest: bytes, trust_root: PublicationTestTrustRoot
) -> bytes:
    if trust_root.private_key_path is None:
        raise AuthEvidenceError("test trust root has no private key for signing")
    assert_production_pin_unprovisioned()
    with tempfile.TemporaryDirectory(prefix="ignite-pub-ev-sign-") as temporary:
        work = Path(temporary)
        signature = _SIGN.sign_manifest(manifest, trust_root.private_key_path, work)
        _SIGN.verify_signature(manifest, signature, trust_root.public_key, work)
    return signature_envelope(signature)


def _parse_manifest_untrusted(raw: bytes) -> dict:
    """Structural parse only — must not be treated as authenticated claims."""
    if len(raw) > MAX_EVIDENCE_BYTES or b"\r" in raw:
        raise AuthEvidenceError("manifest refused")
    text = raw.decode("ascii")
    if not text.endswith("\n"):
        raise AuthEvidenceError("manifest must end with LF")
    lines = text[:-1].split("\n")
    expected_line_count = 10 + len(COMPONENT_ROLES) + 1
    if len(lines) != expected_line_count:
        raise AuthEvidenceError("manifest line count mismatch")
    if lines[0] != EVIDENCE_HEADER or lines[1] != f"schema={EVIDENCE_SCHEMA}":
        raise AuthEvidenceError("manifest header/schema mismatch")

    def _field(prefix: str, line: str) -> str:
        if not line.startswith(prefix):
            raise AuthEvidenceError(f"missing field {prefix}")
        return line[len(prefix) :]

    version = _field("version=", lines[2])
    tag = _field("tag=", lines[3])
    scriptlet_artifact = _field("scriptlet_artifact=", lines[4])
    scriptlet_bytes = _field("scriptlet_bytes=", lines[5])
    scriptlet_sha256 = _field("scriptlet_sha256=", lines[6])
    payload_dirname = _field("payload_dirname=", lines[7])
    package_profile_sha256 = _field("package_profile_sha256=", lines[8])
    role_count = _field("component_role_count=", lines[9])
    if not _VERSION_RE.fullmatch(version):
        raise AuthEvidenceError("noncanonical version")
    if tag != f"v{version}":
        raise AuthEvidenceError("tag/version mismatch")
    if scriptlet_artifact != f"Install Ignite v{version}.sh":
        raise AuthEvidenceError("scriptlet artifact mismatch")
    if not _BYTES_RE.fullmatch(scriptlet_bytes):
        raise AuthEvidenceError("noncanonical scriptlet_bytes")
    if not _SHA_RE.fullmatch(scriptlet_sha256):
        raise AuthEvidenceError("noncanonical scriptlet_sha256")
    if payload_dirname != f"Ignite-Kindle-{version}":
        raise AuthEvidenceError("payload dirname mismatch")
    if not _SHA_RE.fullmatch(package_profile_sha256):
        raise AuthEvidenceError("noncanonical package_profile_sha256")
    if role_count != str(len(COMPONENT_ROLES)):
        raise AuthEvidenceError("component_role_count mismatch")

    components: list[ComponentClaim] = []
    for idx, role in enumerate(COMPONENT_ROLES):
        match = _COMPONENT_RE.fullmatch(lines[10 + idx])
        if match is None or match.group("role") != role:
            raise AuthEvidenceError(f"component line malformed for {role}")
        presence = match.group("presence")
        size = int(match.group("size"))
        digest = match.group("digest")
        klass = match.group("klass")
        mode = match.group("mode")
        mapping = match.group("mapping")
        if klass != PACKAGE_CLASS[role]:
            raise AuthEvidenceError(f"classification mismatch for {role}")
        if mapping != CANDIDATE_B_DESTINATION[role]:
            raise AuthEvidenceError(f"mapping mismatch for {role}")
        if presence == "absent":
            if size != 0 or digest != "-" or mode != "-":
                raise AuthEvidenceError(f"absent component invalid: {role}")
        else:
            if digest == "-" or not _MODE_RE.fullmatch(mode):
                raise AuthEvidenceError(f"file component invalid: {role}")
            if size > MAX_COMPONENT_BYTES:
                raise AuthEvidenceError(f"component size bound: {role}")
        components.append(
            ComponentClaim(
                role=role,
                presence=presence,
                size=size,
                sha256=digest,
                classification=klass,
                mode=mode,
                mapping=mapping,
            )
        )
    if lines[-1] != "publisher_authentication_scope=test-trust-root-only":
        raise AuthEvidenceError("publisher scope marker missing")
    # Canonical re-encoding must match exactly.
    rebuilt = build_publication_evidence_from_claims(
        version=version,
        scriptlet_bytes=int(scriptlet_bytes),
        scriptlet_sha256=scriptlet_sha256,
        package_profile_sha256=package_profile_sha256,
        components=components,
    )
    if rebuilt != raw:
        raise AuthEvidenceError("manifest is not exact canonical encoding")
    return {
        "version": version,
        "tag": tag,
        "scriptlet_artifact": scriptlet_artifact,
        "scriptlet_bytes": int(scriptlet_bytes),
        "scriptlet_sha256": scriptlet_sha256,
        "payload_dirname": payload_dirname,
        "package_profile_sha256": package_profile_sha256,
        "components": tuple(components),
    }


def build_publication_evidence_from_claims(
    *,
    version: str,
    scriptlet_bytes: int,
    scriptlet_sha256: str,
    package_profile_sha256: str,
    components: list[ComponentClaim] | tuple[ComponentClaim, ...],
) -> bytes:
    tag = f"v{version}"
    lines = [
        EVIDENCE_HEADER,
        f"schema={EVIDENCE_SCHEMA}",
        f"version={version}",
        f"tag={tag}",
        f"scriptlet_artifact=Install Ignite v{version}.sh",
        f"scriptlet_bytes={scriptlet_bytes}",
        f"scriptlet_sha256={scriptlet_sha256}",
        f"payload_dirname=Ignite-Kindle-{version}",
        f"package_profile_sha256={package_profile_sha256}",
        f"component_role_count={len(COMPONENT_ROLES)}",
    ]
    by_role = {c.role: c for c in components}
    for role in COMPONENT_ROLES:
        claim = by_role[role]
        if claim.presence == "file":
            lines.append(
                f"component={role}|file|{claim.size}|{claim.sha256}|"
                f"{claim.classification}|{claim.mode}|{claim.mapping}"
            )
        else:
            lines.append(
                f"component={role}|absent|0|-|{claim.classification}|-|{claim.mapping}"
            )
    lines.append("publisher_authentication_scope=test-trust-root-only")
    return ("\n".join(lines) + "\n").encode("ascii")


def authenticate_publication_evidence(
    manifest_bytes: bytes,
    signature_bytes: bytes,
    trust_root: PublicationTestTrustRoot,
) -> AuthenticatedPublicationEvidence:
    """Verify detached Ed25519 signature before treating claims as authenticated."""
    assert_production_pin_unprovisioned()
    if not signature_bytes:
        raise AuthEvidenceError("signature missing")
    signature = parse_signature_envelope(signature_bytes)
    with tempfile.TemporaryDirectory(prefix="ignite-pub-ev-verify-") as temporary:
        try:
            _SIGN.verify_signature(
                manifest_bytes, signature, trust_root.public_key, Path(temporary)
            )
        except _SIGN.ReleaseSigningError as exc:
            raise AuthEvidenceError(f"signature verification failed: {exc}") from exc
    parsed = _parse_manifest_untrusted(manifest_bytes)
    return AuthenticatedPublicationEvidence(
        version=parsed["version"],
        tag=parsed["tag"],
        scriptlet_artifact=parsed["scriptlet_artifact"],
        scriptlet_bytes=parsed["scriptlet_bytes"],
        scriptlet_sha256=parsed["scriptlet_sha256"],
        payload_dirname=parsed["payload_dirname"],
        package_profile_sha256=parsed["package_profile_sha256"],
        components=parsed["components"],
        trust_root_label=trust_root.label,
        raw_manifest=manifest_bytes,
    )


def verify_scriptlet_identity(
    auth: AuthenticatedPublicationEvidence, scriptlet: Path
) -> None:
    data, size, digest, _mode = consume_regular_file_fd(
        scriptlet, max_size=MAX_SCRIPTLET_BYTES
    )
    del data
    if scriptlet.name != auth.scriptlet_artifact:
        raise AuthEvidenceError("scriptlet basename mismatch")
    if size != auth.scriptlet_bytes or digest != auth.scriptlet_sha256:
        raise AuthEvidenceError("scriptlet identity mismatch")


def verify_package_against_authenticated(
    auth: AuthenticatedPublicationEvidence,
    *,
    payload: Path,
    scriptlet: Path,
) -> None:
    """Compare local measurements against authenticated claims (claims are authority)."""
    verify_scriptlet_identity(auth, scriptlet)
    if payload.name != auth.payload_dirname:
        raise AuthEvidenceError("payload dirname mismatch")
    profiles = generate_profiles(payload, auth.version)
    package_profile = profiles["package"]
    if hashlib.sha256(package_profile).hexdigest() != auth.package_profile_sha256:
        raise AuthEvidenceError("package profile digest mismatch")
    digests = inspect_package_components(payload)
    for claim in auth.components:
        presence, size, digest = digests[claim.role]
        if presence != claim.presence:
            raise AuthEvidenceError(f"package presence mismatch: {claim.role}")
        if presence == "file":
            if size != claim.size or digest != claim.sha256:
                raise AuthEvidenceError(f"package digest mismatch: {claim.role}")
            rel = PACKAGE_RELATIVE[claim.role]
            expected_mode = _mode_token_for_package_role(claim.role, "file", rel)
            if claim.mode != expected_mode:
                raise AuthEvidenceError(f"package mode claim mismatch: {claim.role}")
        if claim.classification != PACKAGE_CLASS[claim.role]:
            raise AuthEvidenceError(f"package class mismatch: {claim.role}")
    ok, report = verify_equivalence(
        sfx_path=scriptlet,
        payload_path=payload,
        version=auth.version,
        package_profile=package_profile,
    )
    if not ok:
        raise AuthEvidenceError(
            "scriptlet/payload/profile equivalence failed: "
            + report.decode("ascii", errors="replace")[:240]
        )


def validate_release_evidence_against_authenticated(
    evidence: ReleaseEvidence,
    auth: AuthenticatedPublicationEvidence,
) -> None:
    """Refuse caller-controlled evidence that disagrees with signed claims."""
    if evidence.version != auth.version:
        raise AuthEvidenceError(
            f"release evidence version {evidence.version!r} disagrees with "
            f"authenticated claims {auth.version!r}"
        )
    for claim in auth.components:
        if claim.role not in evidence.digests:
            raise AuthEvidenceError(f"release evidence missing role: {claim.role}")
        presence, size, digest = evidence.digests[claim.role]
        if presence != claim.presence:
            raise AuthEvidenceError(
                f"release evidence presence disagrees for {claim.role}: "
                f"{presence} vs claim {claim.presence}"
            )
        if claim.presence == "file":
            if size != claim.size or digest != claim.sha256:
                raise AuthEvidenceError(
                    f"release evidence digest disagrees for {claim.role}"
                )
        elif size != 0 or digest != "-":
            raise AuthEvidenceError(
                f"release evidence absent role is noncanonical: {claim.role}"
            )


def release_evidence_from_authenticated(
    auth: AuthenticatedPublicationEvidence,
    *,
    payload: Path,
) -> ReleaseEvidence:
    """Derive 3e ReleaseEvidence strictly from verified signed claims + payload profile."""
    if payload.name != auth.payload_dirname:
        raise AuthEvidenceError("payload dirname mismatch while deriving evidence")
    profiles = generate_profiles(payload, auth.version)
    package_profile = profiles["package"]
    if hashlib.sha256(package_profile).hexdigest() != auth.package_profile_sha256:
        raise AuthEvidenceError(
            "package profile digest mismatch while deriving evidence from claims"
        )
    digests: dict[str, tuple[str, int, str]] = {}
    for claim in auth.components:
        if claim.presence == "file":
            digests[claim.role] = ("file", claim.size, claim.sha256)
        else:
            digests[claim.role] = ("absent", 0, "-")
    return ReleaseEvidence(
        version=auth.version,
        payload=payload,
        package_profile=package_profile,
        digests=digests,
    )


def _direct_copy_roles_match_claims(
    observations,
    claims: dict[str, ComponentClaim],
    *,
    require_exec: bool,
) -> list[str]:
    errors: list[str] = []
    by_role = {row.role: row for row in observations}
    for role, claim in claims.items():
        row = by_role[role]
        installed_class = CANDIDATE_B_CLASS[role]
        if installed_class in {"transformed", "generated", "shared"}:
            continue
        if claim.classification == "direct-copy":
            if claim.presence != "file":
                errors.append(f"claim-absent-direct-copy:{role}")
                continue
            if row.presence != "file" or row.sha256 != claim.sha256 or row.size != claim.size:
                errors.append(f"installed-mismatch:{role}")
            if require_exec and role in EXECUTABLE_ROLES and row.exec_state == "missing":
                errors.append(f"exec-missing:{role}")
        elif claim.classification == "optional":
            if claim.presence == "file":
                if (
                    row.presence != "file"
                    or row.sha256 != claim.sha256
                    or row.size != claim.size
                ):
                    errors.append(f"optional-mismatch:{role}")
            elif row.presence != "absent":
                errors.append(f"optional-unexpected:{role}")
    return errors


def verify_installed_b(
    auth_b: AuthenticatedPublicationEvidence,
    kindle: Path,
    evidence_a: ReleaseEvidence,
    evidence_b: ReleaseEvidence,
    *,
    auth_a: AuthenticatedPublicationEvidence | None = None,
) -> list[str]:
    """Return error tokens; empty means installed B matches authenticated B."""
    validate_release_evidence_against_authenticated(evidence_b, auth_b)
    if auth_a is not None:
        validate_release_evidence_against_authenticated(evidence_a, auth_a)
    errors: list[str] = []
    require_exec = sys.platform != "win32"
    candidate = observe_candidate_b(kindle, evidence_b, evidence_a)
    claims = {c.role: c for c in auth_b.components}
    errors.extend(
        _direct_copy_roles_match_claims(candidate, claims, require_exec=require_exec)
    )
    pending_status, fields = inspect_pending_health(
        kindle, evidence_a=evidence_a, evidence_b=evidence_b
    )
    if pending_status != "consistent" or fields is None:
        errors.append(f"pending-health:{pending_status}")
    elif fields["candidate_version"] != auth_b.version:
        errors.append("pending-candidate-version-mismatch")
    state = classify_publication_state(
        kindle,
        candidate_rows=candidate,
        retained_rows=observe_retained_a(kindle, evidence_a, evidence_b),
        evidence_a=evidence_a,
        evidence_b=evidence_b,
        had_upgrade=True,
    )
    if state != "b-published-with-pending-health":
        errors.append(f"publication-state:{state}")
    return errors


def verify_retained_a(
    auth_a: AuthenticatedPublicationEvidence,
    kindle: Path,
    evidence_a: ReleaseEvidence,
    evidence_b: ReleaseEvidence,
    *,
    auth_b: AuthenticatedPublicationEvidence | None = None,
) -> list[str]:
    """Match retained A against independently authenticated A claims (not B).

    Presence is enforced in both directions for independently verifiable roles
    (direct-copy and optional). Transformed/shared roles remain exceptions.
    When pending-health names a candidate version, auth_a must not collide with
    that candidate identity — preventing B's signed claims from authenticating
    retained A even when some component bytes are identical across releases.
    """
    validate_release_evidence_against_authenticated(evidence_a, auth_a)
    if auth_b is not None:
        validate_release_evidence_against_authenticated(evidence_b, auth_b)
    errors: list[str] = []
    retained = observe_retained_a(kindle, evidence_a, evidence_b)
    claims = {c.role: c for c in auth_a.components}
    by_role = {row.role: row for row in retained}
    b_claims = (
        {c.role: c for c in auth_b.components} if auth_b is not None else {}
    )

    pending_status, fields = inspect_pending_health(
        kindle, evidence_a=evidence_a, evidence_b=evidence_b
    )
    if (
        fields is not None
        and fields.get("candidate_version") == auth_a.version
        and pending_status in {"consistent", "structurally-valid"}
    ):
        # After A→B, candidate is B; using B's auth object as "A" collides here.
        errors.append("retained-a-auth-collides-with-candidate-b")

    for role, claim in claims.items():
        row = by_role[role]
        rclass = RETAINED_A_FUTURE_CLASS[role]
        if rclass in {"shared", "transformed"}:
            continue
        if claim.classification == "direct-copy":
            if claim.presence == "file":
                if (
                    row.presence != "file"
                    or row.sha256 != claim.sha256
                    or row.size != claim.size
                ):
                    errors.append(f"retained-a-mismatch:{role}")
                elif row.digest_match == "match-b":
                    errors.append(f"retained-a-substituted-with-b:{role}")
                elif row.digest_match != "match-a":
                    errors.append(f"retained-a-identity-unbound:{role}")
                # Even with identical A/B bytes, B's claim must not stand in for A.
                if (
                    auth_b is not None
                    and role in b_claims
                    and b_claims[role].presence == "file"
                    and b_claims[role].sha256 == claim.sha256
                    and auth_b.version != auth_a.version
                    and row.presence == "file"
                    and row.sha256 == claim.sha256
                ):
                    # Bytes may match both releases; release identity still requires
                    # auth_a (already bound via evidence validation + digest_match).
                    # Flag only when digest_match incorrectly prefers B.
                    if row.digest_match == "match-b":
                        errors.append(f"retained-a-b-bytes-collision-unresolved:{role}")
            elif row.presence != "absent":
                errors.append(f"retained-a-unexpected:{role}")
        elif claim.classification == "optional":
            if claim.presence == "file":
                if (
                    row.presence != "file"
                    or row.sha256 != claim.sha256
                    or row.size != claim.size
                ):
                    errors.append(f"retained-a-optional-mismatch:{role}")
            elif row.presence != "absent":
                errors.append(f"retained-a-optional-unexpected:{role}")
    return errors


def render_authenticated_report(
    *,
    trust_root_label: str,
    signature_status: str,
    scriptlet_status: str,
    payload_status: str,
    installed_b_status: str,
    retained_a_status: str,
    publication_state: str,
    notes: list[str],
) -> bytes:
    lines = [
        REPORT_HEADER,
        f"schema={REPORT_SCHEMA}",
        f"test_trust_root={trust_root_label}",
        "publisher_authentication=unavailable",
        "installation_binding=unverified",
        "execution_binding=unverified",
        "first_launch_admission=unavailable",
        "data_compatibility=unknown",
        "promotion_authorized=false",
        f"signature_status={signature_status}",
        f"scriptlet_identity={scriptlet_status}",
        f"payload_equivalence={payload_status}",
        f"installed_b={installed_b_status}",
        f"retained_a={retained_a_status}",
        f"publication_state={publication_state}",
        "authority=observation-only-under-explicit-test-trust-root",
    ]
    for note in notes[:48]:
        safe = note.encode("ascii", errors="replace").decode("ascii")
        lines.append(f"note={safe[:240]}")
    return ("\n".join(lines) + "\n").encode("ascii")


def observe_authenticated_publication(
    *,
    trust_root: PublicationTestTrustRoot,
    manifest_a: bytes,
    signature_a: bytes,
    manifest_b: bytes,
    signature_b: bytes,
    scriptlet_a: Path,
    scriptlet_b: Path,
    payload_a: Path,
    payload_b: Path,
    kindle: Path,
    evidence_a: ReleaseEvidence | None = None,
    evidence_b: ReleaseEvidence | None = None,
) -> bytes:
    """End-to-end observation report after disposable A→B install.

    Observer evidence is derived from authenticated claims. Optional caller
    evidence is validated against those claims and never used to fill gaps.
    """
    notes: list[str] = []
    signature_status = "failed"
    scriptlet_status = "unchecked"
    payload_status = "unchecked"
    installed_b_status = "unchecked"
    retained_a_status = "unchecked"
    publication_state = "unknown"
    try:
        if not signature_a or not signature_b:
            raise AuthEvidenceError("signature missing")
        auth_a = authenticate_publication_evidence(manifest_a, signature_a, trust_root)
        auth_b = authenticate_publication_evidence(manifest_b, signature_b, trust_root)
        signature_status = f"verified-under-test-trust-root:{trust_root.label}"
        verify_package_against_authenticated(
            auth_a, payload=payload_a, scriptlet=scriptlet_a
        )
        verify_package_against_authenticated(
            auth_b, payload=payload_b, scriptlet=scriptlet_b
        )
        scriptlet_status = "match"
        payload_status = "match"
        # Derive observer evidence from signed claims (authority), not caller input.
        derived_a = release_evidence_from_authenticated(auth_a, payload=payload_a)
        derived_b = release_evidence_from_authenticated(auth_b, payload=payload_b)
        if evidence_a is not None:
            validate_release_evidence_against_authenticated(evidence_a, auth_a)
        if evidence_b is not None:
            validate_release_evidence_against_authenticated(evidence_b, auth_b)
        b_errors = verify_installed_b(
            auth_b, kindle, derived_a, derived_b, auth_a=auth_a
        )
        a_errors = verify_retained_a(
            auth_a, kindle, derived_a, derived_b, auth_b=auth_b
        )
        installed_b_status = "match" if not b_errors else "mismatch"
        retained_a_status = "match" if not a_errors else "mismatch"
        notes.extend(f"b:{e}" for e in b_errors)
        notes.extend(f"a:{e}" for e in a_errors)
        publication_state = classify_publication_state(
            kindle,
            candidate_rows=observe_candidate_b(kindle, derived_b, derived_a),
            retained_rows=observe_retained_a(kindle, derived_a, derived_b),
            evidence_a=derived_a,
            evidence_b=derived_b,
            had_upgrade=True,
        )
        # Both signature and installation conditions are independent.
        if signature_status.startswith("verified") and (
            installed_b_status != "match" or retained_a_status != "match"
        ):
            notes.append("valid-signature-does-not-override-incomplete-installation")
        if installed_b_status == "match" and not signature_status.startswith("verified"):
            notes.append("complete-installation-does-not-override-invalid-signature")
        notes.append("transformed-generated-shared=not-byte-authenticated")
        notes.append("appreg-handler=not-independently-verifiable-from-package-evidence")
        notes.append("observer-evidence=derived-from-authenticated-claims")
    except AuthEvidenceError as exc:
        notes.append(f"refused:{exc}")
        if "signature" in str(exc).lower() or "missing" in str(exc).lower():
            signature_status = "failed"
            if "missing" in str(exc).lower():
                signature_status = "missing"
    return render_authenticated_report(
        trust_root_label=trust_root.label,
        signature_status=signature_status,
        scriptlet_status=scriptlet_status,
        payload_status=payload_status,
        installed_b_status=installed_b_status,
        retained_a_status=retained_a_status,
        publication_state=publication_state,
        notes=notes,
    )
