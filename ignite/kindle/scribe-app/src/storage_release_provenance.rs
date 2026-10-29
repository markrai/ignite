//! Read-only authentication of an explicit release component set.
//!
//! This module never discovers releases, executes an application candidate, mutates installer
//! state, or grants compatibility/promotion. The production entry point uses only the compile-time
//! official release pin. While that pin is `UNPROVISIONED`, every otherwise complete inspection
//! fails closed with [`ProvenanceError::OfficialKeyNotProvisioned`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::release_manifest::{
    self, authenticate_with_key, decode_hex, encode_hex, official_release_public_key,
    valid_release_version, ReleaseVerifyError,
};
use crate::storage_inspect_library::{
    hash_file, inspect_metadata, open_regular_read_only, path_chain_contains_symlink,
};

pub const RECORD_HEADER: &str = "IGNITE_INSTALLED_RELEASE_PROVENANCE_V1";
pub const RECORD_SCHEMA: &str = "1";
pub const SIGNATURE_SCHEMA: &str = "ignite-installed-release-provenance-signature-v1";
pub const REPORT_HEADER: &str = "IGNITE_RELEASE_PROVENANCE_INSPECTION_V1";
pub const MAX_RECORD_BYTES: u64 = 16 * 1024;
pub const MAX_SIGNATURE_BYTES: u64 = 256;
pub const MAX_RELEASE_MANIFEST_BYTES: u64 = release_manifest::MAX_MANIFEST_BYTES as u64;
pub const MAX_CAPABILITIES_BYTES: u64 = 64 * 1024;
pub const MAX_COMPONENT_BYTES: u64 = 512 * 1024 * 1024;

/// Closed installed/package closure for schema v1. Roles are identities, not filesystem paths;
/// callers must supply one explicit actual path for every role.
pub const COMPONENT_ROLES: [&str; 22] = [
    "scribe-app",
    "scribe-lifecycle",
    "scribe-lifecycle-host",
    "launcher",
    "registration-source",
    "icon",
    "license-noto-emoji",
    "license-noto-emoji-description",
    "license-noto-emoji-upstream-info",
    "license-source-sans-3",
    "install-runtime",
    "uninstall-runtime",
    "backup-runtime",
    "register-helpers",
    "install-recovery",
    "uninstall-entry",
    "backup-entry",
    "diagnose-entry",
    "recovery-dropbear",
    "recovery-elf-sha256",
    "recovery-provenance",
    "recovery-version",
];

/// Caller-selected report label only; this is not authenticated slot ownership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseSubject {
    RetainedA,
    CandidateB,
}

impl ReleaseSubject {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "A" => Some(Self::RetainedA),
            "B" => Some(Self::CandidateB),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::RetainedA => "A",
            Self::CandidateB => "B",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentPath {
    pub role: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationPaths {
    pub expected_tag: String,
    pub record: PathBuf,
    pub record_signature: PathBuf,
    pub release_manifest: PathBuf,
    pub release_manifest_signature: PathBuf,
    pub capabilities: PathBuf,
    pub components: Vec<ComponentPath>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectionConfig {
    pub subject: ReleaseSubject,
    pub paths: VerificationPaths,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectionCommand {
    NotRequested,
    Inspect(Box<InspectionConfig>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Presence {
    File { bytes: u64, sha256: [u8; 32] },
    Absent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PresenceRequirement {
    /// These files define the minimum executable launch closure in every supported view.
    Mandatory,
    /// Presence differs between the staged package, live B, and the retained-A snapshot.
    StageConditional,
    /// The source/package and installer explicitly permit this file to be omitted.
    Optional,
}

fn presence_requirement(role: &str) -> Option<PresenceRequirement> {
    match role {
        "scribe-app" | "scribe-lifecycle" | "scribe-lifecycle-host" => {
            Some(PresenceRequirement::Mandatory)
        }
        "license-noto-emoji-description" | "license-noto-emoji-upstream-info" => {
            Some(PresenceRequirement::Optional)
        }
        "launcher"
        | "registration-source"
        | "icon"
        | "license-noto-emoji"
        | "license-source-sans-3"
        | "install-runtime"
        | "uninstall-runtime"
        | "backup-runtime"
        | "register-helpers"
        | "install-recovery"
        | "uninstall-entry"
        | "backup-entry"
        | "diagnose-entry"
        | "recovery-dropbear"
        | "recovery-elf-sha256"
        | "recovery-provenance"
        | "recovery-version" => Some(PresenceRequirement::StageConditional),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ComponentIdentity {
    role: &'static str,
    presence: Presence,
}

/// Authenticated claims and matching bytes for the explicit paths supplied by the caller.
///
/// This does not establish canonical installation paths, active/retained slot ownership, a pending
/// transaction binding, or an atomic filesystem snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedReleaseComponentSet {
    version: String,
    tag: String,
    key_id: String,
    source_commit: String,
    ci_workflow_run_id: u64,
    installer_artifact: String,
    installer_bytes: u64,
    installer_sha256: [u8; 32],
    release_manifest_bytes: u64,
    release_manifest_sha256: [u8; 32],
    capabilities_bytes: u64,
    capabilities_sha256: [u8; 32],
    components: Vec<ComponentIdentity>,
}

impl AuthenticatedReleaseComponentSet {
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn source_commit(&self) -> &str {
        &self.source_commit
    }

    pub fn ci_workflow_run_id(&self) -> u64 {
        self.ci_workflow_run_id
    }

    pub fn component_count(&self) -> usize {
        self.components.len()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProvenanceError {
    OfficialKeyNotProvisioned,
    InvalidPinnedPublicKey,
    Missing(&'static str),
    UnsafeInput(&'static str),
    InputTooLarge(&'static str),
    InputRead(&'static str),
    StateChanged(&'static str),
    MissingSignature,
    MalformedSignature,
    InvalidSignature,
    UnsupportedSchema,
    MalformedRecord(&'static str),
    KeyIdMismatch,
    WrongPlatformOrArchitecture,
    UnexpectedReleaseTag,
    InconsistentReleaseIdentity,
    ReleaseManifestInvalid,
    ReleaseManifestMismatch,
    ComponentMapMalformed,
    MandatoryComponentAbsent(&'static str),
    ComponentMissing(&'static str),
    ComponentUnexpected(&'static str),
    ComponentSizeMismatch(&'static str),
    ComponentDigestMismatch(&'static str),
    CapabilitiesMismatch,
}

impl std::fmt::Display for ProvenanceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::OfficialKeyNotProvisioned => "official release signing key is not provisioned",
            Self::InvalidPinnedPublicKey => "pinned release public key is invalid",
            Self::Missing(label) => return write!(formatter, "{label} is missing"),
            Self::UnsafeInput(label) => {
                return write!(formatter, "{label} is not a safe regular file")
            }
            Self::InputTooLarge(label) => {
                return write!(formatter, "{label} exceeds its size bound")
            }
            Self::InputRead(label) => return write!(formatter, "cannot read {label}"),
            Self::StateChanged(label) => {
                return write!(formatter, "{label} changed during verification")
            }
            Self::MissingSignature => "installed provenance signature is missing",
            Self::MalformedSignature => "installed provenance signature is malformed",
            Self::InvalidSignature => "installed provenance signature is invalid",
            Self::UnsupportedSchema => "installed provenance schema is unsupported",
            Self::MalformedRecord(reason) => reason,
            Self::KeyIdMismatch => {
                "installed provenance key identifier does not match the pinned key"
            }
            Self::WrongPlatformOrArchitecture => {
                "installed provenance targets another platform or architecture"
            }
            Self::UnexpectedReleaseTag => {
                "installed provenance tag does not match the requested release"
            }
            Self::InconsistentReleaseIdentity => {
                "installed provenance release identity is inconsistent"
            }
            Self::ReleaseManifestInvalid => "authenticated release manifest is invalid",
            Self::ReleaseManifestMismatch => "release manifest and installed provenance disagree",
            Self::ComponentMapMalformed => {
                "explicit component path map is incomplete, duplicated, or unknown"
            }
            Self::MandatoryComponentAbsent(role) => {
                return write!(
                    formatter,
                    "mandatory release component is signed absent: {role}"
                )
            }
            Self::ComponentMissing(role) => {
                return write!(formatter, "component is missing: {role}")
            }
            Self::ComponentUnexpected(role) => {
                return write!(formatter, "component must be absent: {role}")
            }
            Self::ComponentSizeMismatch(role) => {
                return write!(formatter, "component byte count differs: {role}")
            }
            Self::ComponentDigestMismatch(role) => {
                return write!(formatter, "component digest differs: {role}")
            }
            Self::CapabilitiesMismatch => {
                "storage capability bytes do not match authenticated provenance"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ProvenanceError {}

impl ProvenanceError {
    fn status(&self) -> &'static str {
        match self {
            Self::OfficialKeyNotProvisioned | Self::InvalidPinnedPublicKey => {
                "AUTHORITY_UNAVAILABLE"
            }
            Self::Missing(_) | Self::MissingSignature => "PROVENANCE_MISSING",
            Self::UnsupportedSchema => "SCHEMA_UNSUPPORTED",
            Self::MalformedSignature | Self::InvalidSignature => "SIGNATURE_INVALID",
            Self::WrongPlatformOrArchitecture => "WRONG_PLATFORM_OR_ARCHITECTURE",
            Self::ComponentMissing(_) | Self::MandatoryComponentAbsent(_) => "COMPONENT_MISSING",
            Self::ComponentUnexpected(_) | Self::ComponentMapMalformed => "COMPONENT_UNEXPECTED",
            Self::ComponentSizeMismatch(_) | Self::ComponentDigestMismatch(_) => {
                "COMPONENT_MISMATCH"
            }
            Self::StateChanged(_) => "STATE_CHANGED",
            Self::UnsafeInput(_) | Self::InputTooLarge(_) | Self::InputRead(_) => {
                "VERIFICATION_INCOMPLETE"
            }
            Self::KeyIdMismatch
            | Self::UnexpectedReleaseTag
            | Self::InconsistentReleaseIdentity
            | Self::ReleaseManifestMismatch
            | Self::CapabilitiesMismatch => "INCONSISTENT_SOURCE",
            Self::ReleaseManifestInvalid | Self::MalformedRecord(_) => "PROVENANCE_INVALID",
        }
    }

    fn exit_code(&self) -> u8 {
        match self {
            Self::OfficialKeyNotProvisioned
            | Self::InvalidPinnedPublicKey
            | Self::Missing(_)
            | Self::MissingSignature
            | Self::UnsupportedSchema
            | Self::UnsafeInput(_)
            | Self::InputTooLarge(_)
            | Self::InputRead(_)
            | Self::StateChanged(_) => 3,
            _ => 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectionReport {
    subject: ReleaseSubject,
    status: &'static str,
    failure: &'static str,
    version: String,
    tag: String,
    key_id: String,
    components_verified: usize,
    exit_code: u8,
}

impl InspectionReport {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        format!(
            "{REPORT_HEADER}\nschema=1\nsubject={}\nstatus={}\nfailure={}\nrelease_version={}\nrelease_tag={}\nkey_id={}\ncomponents_verified={}\ninstallation_binding=unverified\ndata_compatibility=unknown\npromotion_authorized=false\ninspection_action=report-only\n",
            self.subject.as_str(),
            self.status,
            self.failure,
            self.version,
            self.tag,
            self.key_id,
            self.components_verified,
        )
        .into_bytes()
    }

    pub fn exit_code(&self) -> u8 {
        self.exit_code
    }
}

pub fn parse_command(args: &[String]) -> Result<InspectionCommand, String> {
    let requested = args
        .iter()
        .filter(|argument| argument.as_str() == "--inspect-release-provenance")
        .count();
    if requested == 0 {
        return Ok(InspectionCommand::NotRequested);
    }
    if requested != 1 || args.first().map(String::as_str) != Some("--inspect-release-provenance") {
        return Err(
            "--inspect-release-provenance must be the first and only diagnostic command".into(),
        );
    }

    let mut values = BTreeMap::<&str, String>::new();
    let mut components = Vec::new();
    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        if index == args.len() {
            return Err(format!("missing value for {flag}"));
        }
        let value = args[index].clone();
        index += 1;
        if value.is_empty() {
            return Err(format!("empty value for {flag}"));
        }
        if flag == "--component" {
            let (role, path) = value
                .split_once('=')
                .filter(|(role, path)| !role.is_empty() && !path.is_empty())
                .ok_or_else(|| "--component must be ROLE=ABSOLUTE_PATH".to_owned())?;
            let path = PathBuf::from(path);
            components.push(ComponentPath {
                role: role.to_owned(),
                path,
            });
            continue;
        }
        let key = match flag {
            "--subject"
            | "--expected-tag"
            | "--record"
            | "--record-signature"
            | "--release-manifest"
            | "--release-manifest-signature"
            | "--capabilities" => flag,
            _ => return Err(format!("unexpected provenance option: {flag}")),
        };
        if values.insert(key, value).is_some() {
            return Err(format!("duplicate provenance option: {key}"));
        }
    }

    let take = |key: &'static str| {
        values
            .get(key)
            .cloned()
            .ok_or_else(|| format!("missing provenance option: {key}"))
    };
    let subject = ReleaseSubject::parse(&take("--subject")?)
        .ok_or_else(|| "--subject must be exactly A or B".to_owned())?;
    let expected_tag = take("--expected-tag")?;
    if !expected_tag.starts_with('v') || !valid_release_version(&expected_tag[1..]) {
        return Err("--expected-tag must be a canonical v-prefixed Ignite version".into());
    }
    let paths = VerificationPaths {
        expected_tag,
        record: PathBuf::from(take("--record")?),
        record_signature: PathBuf::from(take("--record-signature")?),
        release_manifest: PathBuf::from(take("--release-manifest")?),
        release_manifest_signature: PathBuf::from(take("--release-manifest-signature")?),
        capabilities: PathBuf::from(take("--capabilities")?),
        components,
    };
    validate_verification_paths(&paths).map_err(|error| error.to_string())?;
    Ok(InspectionCommand::Inspect(Box::new(InspectionConfig {
        subject,
        paths,
    })))
}

fn is_explicit_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
}

fn validate_verification_paths(paths: &VerificationPaths) -> Result<(), ProvenanceError> {
    for (path, label) in [
        (&paths.record, "provenance record path"),
        (&paths.record_signature, "provenance signature path"),
        (&paths.release_manifest, "release manifest path"),
        (
            &paths.release_manifest_signature,
            "release manifest signature path",
        ),
        (&paths.capabilities, "storage capabilities path"),
    ] {
        if !is_explicit_absolute_path(path) {
            return Err(ProvenanceError::UnsafeInput(label));
        }
    }
    validate_component_paths(&paths.components)?;
    Ok(())
}

pub fn inspect_official(config: &InspectionConfig) -> InspectionReport {
    match verify_official(&config.paths) {
        Ok(release) => InspectionReport {
            subject: config.subject,
            status: "AUTHENTIC_AND_CONSISTENT",
            failure: "none",
            version: release.version,
            tag: release.tag,
            key_id: release.key_id,
            components_verified: release.components.len(),
            exit_code: 0,
        },
        Err(error) => InspectionReport {
            subject: config.subject,
            status: error.status(),
            failure: failure_token(&error),
            version: "unknown".into(),
            tag: "unknown".into(),
            key_id: "unknown".into(),
            components_verified: 0,
            exit_code: error.exit_code(),
        },
    }
}

fn failure_token(error: &ProvenanceError) -> &'static str {
    match error {
        ProvenanceError::OfficialKeyNotProvisioned => "official-key-not-provisioned",
        ProvenanceError::InvalidPinnedPublicKey => "official-key-invalid",
        ProvenanceError::Missing(_) => "required-input-missing",
        ProvenanceError::UnsafeInput(_) => "unsafe-input",
        ProvenanceError::InputTooLarge(_) => "input-too-large",
        ProvenanceError::InputRead(_) => "input-read-failed",
        ProvenanceError::StateChanged(_) => "state-changed",
        ProvenanceError::MissingSignature => "signature-missing",
        ProvenanceError::MalformedSignature => "signature-malformed",
        ProvenanceError::InvalidSignature => "signature-invalid",
        ProvenanceError::UnsupportedSchema => "schema-unsupported",
        ProvenanceError::MalformedRecord(_) => "record-malformed",
        ProvenanceError::KeyIdMismatch => "key-id-mismatch",
        ProvenanceError::WrongPlatformOrArchitecture => "wrong-platform-or-architecture",
        ProvenanceError::UnexpectedReleaseTag => "unexpected-release-tag",
        ProvenanceError::InconsistentReleaseIdentity => "release-identity-inconsistent",
        ProvenanceError::ReleaseManifestInvalid => "release-manifest-invalid",
        ProvenanceError::ReleaseManifestMismatch => "release-manifest-mismatch",
        ProvenanceError::ComponentMapMalformed => "component-map-malformed",
        ProvenanceError::MandatoryComponentAbsent(_) => "mandatory-component-absent",
        ProvenanceError::ComponentMissing(_) => "component-missing",
        ProvenanceError::ComponentUnexpected(_) => "component-unexpected",
        ProvenanceError::ComponentSizeMismatch(_) => "component-size-mismatch",
        ProvenanceError::ComponentDigestMismatch(_) => "component-digest-mismatch",
        ProvenanceError::CapabilitiesMismatch => "capabilities-mismatch",
    }
}

/// Authenticate the explicit component set with the immutable production pin.
///
/// The trust anchor is checked before any caller-controlled path is read. A successful return is
/// not proof that the supplied paths are the lifecycle's actual A or B installation.
pub fn verify_official(
    paths: &VerificationPaths,
) -> Result<AuthenticatedReleaseComponentSet, ProvenanceError> {
    // Consult the immutable production authority before touching caller-selected paths. In
    // particular, UNPROVISIONED must be deterministic even when evidence is absent or unsafe.
    let public_key = official_release_public_key().map_err(map_release_key_error)?;
    validate_verification_paths(paths)?;
    let record = read_stable_bounded(&paths.record, MAX_RECORD_BYTES, "provenance record")?;
    let signature = read_signature(&paths.record_signature)?;
    verify_loaded_with_key(paths, &record, &signature, &public_key)
}

#[cfg(test)]
fn verify_paths_with_key(
    paths: &VerificationPaths,
    public_key: &[u8; 32],
) -> Result<AuthenticatedReleaseComponentSet, ProvenanceError> {
    validate_verification_paths(paths)?;
    let record = read_stable_bounded(&paths.record, MAX_RECORD_BYTES, "provenance record")?;
    let signature = read_signature(&paths.record_signature)?;
    verify_loaded_with_key(paths, &record, &signature, public_key)
}

fn map_release_key_error(error: ReleaseVerifyError) -> ProvenanceError {
    match error {
        ReleaseVerifyError::OfficialKeyNotProvisioned => ProvenanceError::OfficialKeyNotProvisioned,
        _ => ProvenanceError::InvalidPinnedPublicKey,
    }
}

fn read_signature(path: &Path) -> Result<Vec<u8>, ProvenanceError> {
    match read_stable_bounded(path, MAX_SIGNATURE_BYTES, "provenance signature") {
        Err(ProvenanceError::Missing(_)) => Err(ProvenanceError::MissingSignature),
        other => other,
    }
}

fn authenticate_with_provenance_key(
    record: &[u8],
    signature: &[u8],
    expected_tag: &str,
    public_key: &[u8; 32],
) -> Result<AuthenticatedReleaseComponentSet, ProvenanceError> {
    if record.len() as u64 > MAX_RECORD_BYTES {
        return Err(ProvenanceError::InputTooLarge("provenance record"));
    }
    let signature = parse_signature(signature)?;
    let verifying_key = VerifyingKey::from_bytes(public_key)
        .map_err(|_| ProvenanceError::InvalidPinnedPublicKey)?;
    verifying_key
        .verify_strict(record, &signature)
        .map_err(|_| ProvenanceError::InvalidSignature)?;
    parse_authenticated_record(record, expected_tag, public_key)
}

fn parse_signature(bytes: &[u8]) -> Result<Signature, ProvenanceError> {
    if bytes.is_empty() {
        return Err(ProvenanceError::MissingSignature);
    }
    let prefix = format!("schema={SIGNATURE_SCHEMA}\nsignature=");
    if bytes.len() != prefix.len() + 128 + 1
        || !bytes.starts_with(prefix.as_bytes())
        || bytes.last() != Some(&b'\n')
    {
        return Err(ProvenanceError::MalformedSignature);
    }
    let raw = decode_hex::<64>(&bytes[prefix.len()..bytes.len() - 1])
        .ok_or(ProvenanceError::MalformedSignature)?;
    Ok(Signature::from_bytes(&raw))
}

fn parse_authenticated_record(
    bytes: &[u8],
    expected_tag: &str,
    public_key: &[u8; 32],
) -> Result<AuthenticatedReleaseComponentSet, ProvenanceError> {
    if !canonical_text(bytes) {
        return Err(ProvenanceError::MalformedRecord(
            "installed provenance is not canonical printable ASCII with LF framing",
        ));
    }
    let lines: Vec<&[u8]> = bytes[..bytes.len() - 1]
        .split(|byte| *byte == b'\n')
        .collect();
    let expected_lines = 19 + COMPONENT_ROLES.len();
    if lines.len() != expected_lines {
        return Err(ProvenanceError::MalformedRecord(
            "installed provenance has an incorrect field count",
        ));
    }
    if lines[0] != RECORD_HEADER.as_bytes() {
        return Err(ProvenanceError::UnsupportedSchema);
    }
    if field(lines[1], b"schema=")? != RECORD_SCHEMA.as_bytes() {
        return Err(ProvenanceError::UnsupportedSchema);
    }

    let key_id = ascii_field(lines[2], b"key_id=")?.to_owned();
    let expected_key_id = format!("ed25519-sha256:{}", sha256_hex(public_key));
    if key_id != expected_key_id {
        return Err(ProvenanceError::KeyIdMismatch);
    }
    if field(lines[3], b"platform=")? != b"kindle"
        || field(lines[4], b"architecture=")? != b"armv7-unknown-linux-gnueabihf"
    {
        return Err(ProvenanceError::WrongPlatformOrArchitecture);
    }

    let version = ascii_field(lines[5], b"release_version=")?.to_owned();
    if !valid_release_version(&version) {
        return Err(ProvenanceError::MalformedRecord(
            "installed provenance release version is noncanonical",
        ));
    }
    let tag = ascii_field(lines[6], b"release_tag=")?.to_owned();
    if tag != format!("v{version}") {
        return Err(ProvenanceError::InconsistentReleaseIdentity);
    }
    if tag != expected_tag {
        return Err(ProvenanceError::UnexpectedReleaseTag);
    }

    let source_commit = ascii_field(lines[7], b"source_commit=")?.to_owned();
    if source_commit.len() != 40 || !is_lower_hex(&source_commit) {
        return Err(ProvenanceError::MalformedRecord(
            "installed provenance source commit is malformed",
        ));
    }
    let ci_workflow_run_id =
        canonical_positive_u64(ascii_field(lines[8], b"ci_workflow_run_id=")?)?;
    let installer_artifact = ascii_field(lines[9], b"installer_artifact=")?.to_owned();
    if installer_artifact != format!("Install Ignite v{version}.sh") {
        return Err(ProvenanceError::InconsistentReleaseIdentity);
    }
    let installer_bytes = canonical_positive_u64(ascii_field(lines[10], b"installer_bytes=")?)?;
    let installer_sha256 = digest_field(lines[11], b"installer_sha256=")?;
    let release_manifest_bytes =
        canonical_positive_u64(ascii_field(lines[12], b"release_manifest_bytes=")?)?;
    if release_manifest_bytes > MAX_RELEASE_MANIFEST_BYTES {
        return Err(ProvenanceError::MalformedRecord(
            "authenticated release manifest size exceeds its bound",
        ));
    }
    let release_manifest_sha256 = digest_field(lines[13], b"release_manifest_sha256=")?;
    let capabilities_bytes =
        canonical_positive_u64(ascii_field(lines[14], b"capabilities_bytes=")?)?;
    if capabilities_bytes > MAX_CAPABILITIES_BYTES {
        return Err(ProvenanceError::MalformedRecord(
            "authenticated capability declaration size exceeds its bound",
        ));
    }
    let capabilities_sha256 = digest_field(lines[15], b"capabilities_sha256=")?;
    let count = canonical_positive_u64(ascii_field(lines[16], b"component_count=")?)?;
    if count != COMPONENT_ROLES.len() as u64 {
        return Err(ProvenanceError::MalformedRecord(
            "installed provenance component count differs from the closed schema",
        ));
    }

    let mut components = Vec::with_capacity(COMPONENT_ROLES.len());
    for (offset, role) in COMPONENT_ROLES.iter().copied().enumerate() {
        let line = ascii_field(lines[17 + offset], b"component=")?;
        let mut parts = line.split('|');
        let actual_role = parts.next();
        let presence = parts.next();
        let byte_text = parts.next();
        let digest_text = parts.next();
        if parts.next().is_some() || actual_role != Some(role) {
            return Err(ProvenanceError::MalformedRecord(
                "installed provenance component roles are missing, duplicated, unknown, or reordered",
            ));
        }
        let requirement = presence_requirement(role).ok_or(ProvenanceError::MalformedRecord(
            "installed provenance contains an unknown component role",
        ))?;
        let presence = match (presence, byte_text, digest_text) {
            (Some("absent"), Some("0"), Some("-")) => {
                if requirement == PresenceRequirement::Mandatory {
                    return Err(ProvenanceError::MandatoryComponentAbsent(role));
                }
                Presence::Absent
            }
            (Some("file"), Some(bytes), Some(digest)) => {
                let bytes = canonical_positive_u64(bytes)?;
                if bytes > MAX_COMPONENT_BYTES {
                    return Err(ProvenanceError::MalformedRecord(
                        "installed provenance component size exceeds its bound",
                    ));
                }
                let sha256 =
                    decode_hex::<32>(digest.as_bytes()).ok_or(ProvenanceError::MalformedRecord(
                        "installed provenance component digest is malformed",
                    ))?;
                Presence::File { bytes, sha256 }
            }
            _ => {
                return Err(ProvenanceError::MalformedRecord(
                    "installed provenance component entry is malformed",
                ));
            }
        };
        components.push(ComponentIdentity { role, presence });
    }

    if lines[17 + COMPONENT_ROLES.len()] != b"data_compatibility=unknown"
        || lines[18 + COMPONENT_ROLES.len()] != b"promotion_authorized=false"
    {
        return Err(ProvenanceError::MalformedRecord(
            "installed provenance policy fields are missing or nonconservative",
        ));
    }

    Ok(AuthenticatedReleaseComponentSet {
        version,
        tag,
        key_id,
        source_commit,
        ci_workflow_run_id,
        installer_artifact,
        installer_bytes,
        installer_sha256,
        release_manifest_bytes,
        release_manifest_sha256,
        capabilities_bytes,
        capabilities_sha256,
        components,
    })
}

fn canonical_text(bytes: &[u8]) -> bool {
    bytes.last() == Some(&b'\n')
        && bytes
            .iter()
            .all(|byte| *byte == b'\n' || matches!(*byte, b' '..=b'~'))
}

fn field<'a>(line: &'a [u8], prefix: &[u8]) -> Result<&'a [u8], ProvenanceError> {
    line.strip_prefix(prefix)
        .ok_or(ProvenanceError::MalformedRecord(
            "installed provenance fields are missing, duplicated, unknown, or reordered",
        ))
}

fn ascii_field<'a>(line: &'a [u8], prefix: &[u8]) -> Result<&'a str, ProvenanceError> {
    std::str::from_utf8(field(line, prefix)?).map_err(|_| {
        ProvenanceError::MalformedRecord("installed provenance contains non-ASCII field data")
    })
}

fn canonical_positive_u64(value: &str) -> Result<u64, ProvenanceError> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.starts_with('0')
    {
        return Err(ProvenanceError::MalformedRecord(
            "installed provenance integer is noncanonical",
        ));
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or(ProvenanceError::MalformedRecord(
            "installed provenance integer is invalid",
        ))
}

fn digest_field(line: &[u8], prefix: &[u8]) -> Result<[u8; 32], ProvenanceError> {
    decode_hex::<32>(field(line, prefix)?).ok_or(ProvenanceError::MalformedRecord(
        "installed provenance digest is malformed",
    ))
}

fn is_lower_hex(value: &str) -> bool {
    !value.is_empty()
        && value.len().is_multiple_of(2)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn sha256_hex(bytes: &[u8]) -> String {
    encode_hex(&Sha256::digest(bytes))
}

fn verify_loaded_with_key(
    paths: &VerificationPaths,
    record_bytes: &[u8],
    signature_bytes: &[u8],
    public_key: &[u8; 32],
) -> Result<AuthenticatedReleaseComponentSet, ProvenanceError> {
    let release = authenticate_with_provenance_key(
        record_bytes,
        signature_bytes,
        &paths.expected_tag,
        public_key,
    )?;
    let component_paths = validate_component_paths(&paths.components)?;

    let release_manifest = read_stable_bounded(
        &paths.release_manifest,
        MAX_RELEASE_MANIFEST_BYTES,
        "release manifest",
    )?;
    if release_manifest.len() as u64 != release.release_manifest_bytes
        || Sha256::digest(&release_manifest).as_slice() != release.release_manifest_sha256
    {
        return Err(ProvenanceError::ReleaseManifestMismatch);
    }
    let release_signature = read_stable_bounded(
        &paths.release_manifest_signature,
        MAX_SIGNATURE_BYTES,
        "release manifest signature",
    )?;
    let authenticated_manifest = authenticate_with_key(
        &release_manifest,
        &release_signature,
        &paths.expected_tag,
        public_key,
    )
    .map_err(|_| ProvenanceError::ReleaseManifestInvalid)?;
    if authenticated_manifest.version() != release.version
        || authenticated_manifest.tag() != release.tag
        || authenticated_manifest.installer_filename() != release.installer_artifact
        || authenticated_manifest.installer_bytes() != release.installer_bytes
        || authenticated_manifest.installer_sha256_hex() != encode_hex(&release.installer_sha256)
    {
        return Err(ProvenanceError::ReleaseManifestMismatch);
    }

    verify_exact_file(
        &paths.capabilities,
        release.capabilities_bytes,
        &release.capabilities_sha256,
        "storage capabilities",
    )
    .map_err(|error| match error {
        ProvenanceError::ComponentSizeMismatch(_)
        | ProvenanceError::ComponentDigestMismatch(_)
        | ProvenanceError::ComponentMissing(_) => ProvenanceError::CapabilitiesMismatch,
        other => other,
    })?;

    for component in &release.components {
        let path = component_paths
            .get(component.role)
            .ok_or(ProvenanceError::ComponentMapMalformed)?;
        match &component.presence {
            Presence::Absent => verify_absent(path, component.role)?,
            Presence::File { bytes, sha256 } => {
                verify_exact_file(path, *bytes, sha256, component.role)?;
            }
        }
    }
    Ok(release)
}

fn validate_component_paths(
    components: &[ComponentPath],
) -> Result<BTreeMap<&'static str, &Path>, ProvenanceError> {
    if components.len() != COMPONENT_ROLES.len() {
        return Err(ProvenanceError::ComponentMapMalformed);
    }
    let mut paths = BTreeMap::new();
    let mut actual_paths = BTreeSet::new();
    for component in components {
        let Some(role) = COMPONENT_ROLES
            .iter()
            .copied()
            .find(|role| *role == component.role)
        else {
            return Err(ProvenanceError::ComponentMapMalformed);
        };
        if !is_explicit_absolute_path(&component.path)
            || !actual_paths.insert(component.path.as_path())
            || paths.insert(role, component.path.as_path()).is_some()
        {
            return Err(ProvenanceError::ComponentMapMalformed);
        }
    }
    if COMPONENT_ROLES.iter().any(|role| !paths.contains_key(role)) {
        return Err(ProvenanceError::ComponentMapMalformed);
    }
    Ok(paths)
}

fn verify_absent(path: &Path, role: &'static str) -> Result<(), ProvenanceError> {
    if path_chain_contains_symlink(path) {
        return Err(ProvenanceError::UnsafeInput(role));
    }
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ProvenanceError::InputRead(role)),
        Ok(_) => Err(ProvenanceError::ComponentUnexpected(role)),
    }
}

fn verify_exact_file(
    path: &Path,
    expected_bytes: u64,
    expected_sha256: &[u8; 32],
    role: &'static str,
) -> Result<(), ProvenanceError> {
    verify_exact_file_with_hook(path, expected_bytes, expected_sha256, role, || {})
}

fn verify_exact_file_with_hook(
    path: &Path,
    expected_bytes: u64,
    expected_sha256: &[u8; 32],
    role: &'static str,
    after_hash: impl FnOnce(),
) -> Result<(), ProvenanceError> {
    if path_chain_contains_symlink(path) {
        return Err(ProvenanceError::UnsafeInput(role));
    }
    let before = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProvenanceError::ComponentMissing(role));
        }
        Err(_) => return Err(ProvenanceError::InputRead(role)),
        Ok(metadata) => metadata,
    };
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(ProvenanceError::UnsafeInput(role));
    }
    if before.len() != expected_bytes {
        return Err(ProvenanceError::ComponentSizeMismatch(role));
    }
    if before.len() > MAX_COMPONENT_BYTES {
        return Err(ProvenanceError::InputTooLarge(role));
    }
    let digest = hash_file(path, &before).map_err(|reason| map_file_error(reason, role))?;
    after_hash();
    let after = fs::symlink_metadata(path).map_err(|_| ProvenanceError::StateChanged(role))?;
    if !same_file_identity(&before, &after) {
        return Err(ProvenanceError::StateChanged(role));
    }
    if digest != encode_hex(expected_sha256) {
        return Err(ProvenanceError::ComponentDigestMismatch(role));
    }
    Ok(())
}

fn read_stable_bounded(
    path: &Path,
    max_bytes: u64,
    label: &'static str,
) -> Result<Vec<u8>, ProvenanceError> {
    if path_chain_contains_symlink(path) {
        return Err(ProvenanceError::UnsafeInput(label));
    }
    let before = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProvenanceError::Missing(label));
        }
        Err(_) => return Err(ProvenanceError::InputRead(label)),
        Ok(metadata) => metadata,
    };
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(ProvenanceError::UnsafeInput(label));
    }
    if before.len() > max_bytes {
        return Err(ProvenanceError::InputTooLarge(label));
    }
    let mut file =
        open_regular_read_only(path, &before).map_err(|reason| map_file_error(reason, label))?;
    let capacity =
        usize::try_from(before.len()).map_err(|_| ProvenanceError::InputTooLarge(label))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| ProvenanceError::InputRead(label))?;
    if bytes.len() as u64 != before.len() {
        return Err(ProvenanceError::StateChanged(label));
    }
    let after = inspect_metadata(path).map_err(|_| ProvenanceError::StateChanged(label))?;
    if !same_file_identity(&before, &after) {
        return Err(ProvenanceError::StateChanged(label));
    }
    Ok(bytes)
}

fn map_file_error(reason: &'static str, label: &'static str) -> ProvenanceError {
    match reason {
        "symlink_refused" | "hard_link_refused" | "unsupported_file_type" => {
            ProvenanceError::UnsafeInput(label)
        }
        "file_size_limit_exceeded" => ProvenanceError::InputTooLarge(label),
        "file_changed_during_open" | "file_changed_during_read" => {
            ProvenanceError::StateChanged(label)
        }
        _ => ProvenanceError::InputRead(label),
    }
}

#[cfg(unix)]
fn same_file_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    before.is_file()
        && after.is_file()
        && before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.nlink() == 1
        && after.nlink() == 1
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

#[cfg(not(unix))]
fn same_file_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.is_file()
        && after.is_file()
        && before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    const VERSION: &str = "1.2.3";
    const TAG: &str = "v1.2.3";
    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const INSTALLER: &[u8] = b"synthetic exact CI installer bytes";
    const OWNER_FILE: &str = ".ignite-test-root-owner";
    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct OwnedTestRoot {
        path: PathBuf,
        token: String,
    }

    impl OwnedTestRoot {
        fn from_candidates(candidates: impl IntoIterator<Item = PathBuf>) -> Result<Self, String> {
            for path in candidates {
                match fs::create_dir(&path) {
                    Ok(()) => {
                        let token = uuid::Uuid::now_v7().to_string();
                        let owner = path.join(OWNER_FILE);
                        let mut file = match fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&owner)
                        {
                            Ok(file) => file,
                            Err(error) => {
                                let _ = fs::remove_dir(&path);
                                return Err(format!("create {}: {error}", owner.display()));
                            }
                        };
                        if let Err(error) = std::io::Write::write_all(&mut file, token.as_bytes()) {
                            drop(file);
                            let _ = fs::remove_file(&owner);
                            let _ = fs::remove_dir(&path);
                            return Err(format!("write {}: {error}", owner.display()));
                        }
                        return Ok(Self { path, token });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(format!("create {}: {error}", path.display())),
                }
            }
            Err("no exclusive provenance test-root candidate was available".into())
        }

        fn new(prefix: &str) -> Self {
            let candidates = (0..128).map(|_| {
                std::env::temp_dir().join(format!(
                    "{prefix}-{}-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed),
                    uuid::Uuid::now_v7()
                ))
            });
            Self::from_candidates(candidates).expect("create exclusive provenance test root")
        }
    }

    impl Drop for OwnedTestRoot {
        fn drop(&mut self) {
            let owner = self.path.join(OWNER_FILE);
            let owned = fs::symlink_metadata(&self.path)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
                && fs::symlink_metadata(&owner)
                    .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
                && fs::read_to_string(&owner)
                    .is_ok_and(|token| token.as_str() == self.token.as_str());
            if owned {
                let _ = fs::remove_dir_all(&self.path);
            }
        }
    }

    struct Fixture {
        root: PathBuf,
        _owner: OwnedTestRoot,
        signing_key: SigningKey,
        paths: VerificationPaths,
    }

    #[derive(Clone, Copy)]
    enum MetadataPathField {
        Record,
        RecordSignature,
        ReleaseManifest,
        ReleaseManifestSignature,
        Capabilities,
    }

    impl MetadataPathField {
        const ALL: [Self; 5] = [
            Self::Record,
            Self::RecordSignature,
            Self::ReleaseManifest,
            Self::ReleaseManifestSignature,
            Self::Capabilities,
        ];

        fn set(self, paths: &mut VerificationPaths, value: PathBuf) {
            match self {
                Self::Record => paths.record = value,
                Self::RecordSignature => paths.record_signature = value,
                Self::ReleaseManifest => paths.release_manifest = value,
                Self::ReleaseManifestSignature => paths.release_manifest_signature = value,
                Self::Capabilities => paths.capabilities = value,
            }
        }

        fn label(self) -> &'static str {
            match self {
                Self::Record => "provenance record path",
                Self::RecordSignature => "provenance signature path",
                Self::ReleaseManifest => "release manifest path",
                Self::ReleaseManifestSignature => "release manifest signature path",
                Self::Capabilities => "storage capabilities path",
            }
        }
    }

    fn snapshot_fixture_root(root: &Path) -> BTreeMap<OsString, Vec<u8>> {
        fs::read_dir(root)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), fs::read(entry.path()).unwrap())
            })
            .collect()
    }

    impl Fixture {
        fn new(seed: u8) -> Self {
            let owner = OwnedTestRoot::new("ignite-release-provenance");
            let root = owner.path.clone();
            let signing_key = SigningKey::from_bytes(&[seed; 32]);
            let capabilities = root.join("capabilities.v1");
            fs::write(
                &capabilities,
                crate::storage_capabilities::canonical_bytes().unwrap(),
            )
            .unwrap();

            let mut components = Vec::new();
            for role in COMPONENT_ROLES {
                let path = root.join(format!("component-{role}"));
                if role != "license-noto-emoji-upstream-info" {
                    fs::write(&path, format!("fixture component {role}\n")).unwrap();
                }
                components.push(ComponentPath {
                    role: role.to_owned(),
                    path,
                });
            }

            let release_manifest = root.join("release.manifest");
            let release_manifest_signature = root.join("release.manifest.sig");
            let manifest = release_manifest_bytes();
            fs::write(&release_manifest, &manifest).unwrap();
            fs::write(
                &release_manifest_signature,
                release_signature_bytes(&signing_key, &manifest),
            )
            .unwrap();

            let paths = VerificationPaths {
                expected_tag: TAG.into(),
                record: root.join("installed-provenance.v1"),
                record_signature: root.join("installed-provenance.v1.sig"),
                release_manifest,
                release_manifest_signature,
                capabilities,
                components,
            };
            let fixture = Self {
                root,
                _owner: owner,
                signing_key,
                paths,
            };
            let record = fixture.record_bytes();
            fixture.write_record(&record);
            fixture
        }

        fn public_key(&self) -> [u8; 32] {
            self.signing_key.verifying_key().to_bytes()
        }

        fn record_bytes(&self) -> Vec<u8> {
            let public_key = self.public_key();
            let manifest = fs::read(&self.paths.release_manifest).unwrap();
            let capabilities = fs::read(&self.paths.capabilities).unwrap();
            let mut record = String::new();
            writeln!(record, "{RECORD_HEADER}").unwrap();
            writeln!(record, "schema={RECORD_SCHEMA}").unwrap();
            writeln!(record, "key_id=ed25519-sha256:{}", sha256_hex(&public_key)).unwrap();
            writeln!(record, "platform=kindle").unwrap();
            writeln!(record, "architecture=armv7-unknown-linux-gnueabihf").unwrap();
            writeln!(record, "release_version={VERSION}").unwrap();
            writeln!(record, "release_tag={TAG}").unwrap();
            writeln!(record, "source_commit={COMMIT}").unwrap();
            writeln!(record, "ci_workflow_run_id=123456").unwrap();
            writeln!(record, "installer_artifact=Install Ignite v{VERSION}.sh").unwrap();
            writeln!(record, "installer_bytes={}", INSTALLER.len()).unwrap();
            writeln!(record, "installer_sha256={}", sha256_hex(INSTALLER)).unwrap();
            writeln!(record, "release_manifest_bytes={}", manifest.len()).unwrap();
            writeln!(record, "release_manifest_sha256={}", sha256_hex(&manifest)).unwrap();
            writeln!(record, "capabilities_bytes={}", capabilities.len()).unwrap();
            writeln!(record, "capabilities_sha256={}", sha256_hex(&capabilities)).unwrap();
            writeln!(record, "component_count={}", COMPONENT_ROLES.len()).unwrap();
            let map = validate_component_paths(&self.paths.components).unwrap();
            for role in COMPONENT_ROLES {
                let path = map[role];
                match fs::read(path) {
                    Ok(bytes) => writeln!(
                        record,
                        "component={role}|file|{}|{}",
                        bytes.len(),
                        sha256_hex(&bytes)
                    )
                    .unwrap(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        writeln!(record, "component={role}|absent|0|-").unwrap();
                    }
                    Err(error) => panic!("fixture component {role}: {error}"),
                }
            }
            writeln!(record, "data_compatibility=unknown").unwrap();
            writeln!(record, "promotion_authorized=false").unwrap();
            record.into_bytes()
        }

        fn write_record(&self, record: &[u8]) {
            fs::write(&self.paths.record, record).unwrap();
            fs::write(
                &self.paths.record_signature,
                provenance_signature_bytes(&self.signing_key, record),
            )
            .unwrap();
        }

        fn verify(&self) -> Result<AuthenticatedReleaseComponentSet, ProvenanceError> {
            verify_paths_with_key(&self.paths, &self.public_key())
        }

        fn args(&self) -> Vec<String> {
            let mut args = vec![
                "--inspect-release-provenance".into(),
                "--subject".into(),
                "B".into(),
                "--expected-tag".into(),
                TAG.into(),
                "--record".into(),
                self.paths.record.display().to_string(),
                "--record-signature".into(),
                self.paths.record_signature.display().to_string(),
                "--release-manifest".into(),
                self.paths.release_manifest.display().to_string(),
                "--release-manifest-signature".into(),
                self.paths.release_manifest_signature.display().to_string(),
                "--capabilities".into(),
                self.paths.capabilities.display().to_string(),
            ];
            for component in &self.paths.components {
                args.push("--component".into());
                args.push(format!("{}={}", component.role, component.path.display()));
            }
            args
        }
    }

    fn release_manifest_bytes() -> Vec<u8> {
        format!(
            "schema={}\nversion={VERSION}\ntag={TAG}\nartifact=Install Ignite v{VERSION}.sh\nbytes={}\nsha256={}\n",
            release_manifest::MANIFEST_SCHEMA,
            INSTALLER.len(),
            sha256_hex(INSTALLER),
        )
        .into_bytes()
    }

    fn release_signature_bytes(key: &SigningKey, message: &[u8]) -> Vec<u8> {
        format!(
            "schema={}\nsignature={}\n",
            release_manifest::SIGNATURE_SCHEMA,
            encode_hex(&key.sign(message).to_bytes())
        )
        .into_bytes()
    }

    fn provenance_signature_bytes(key: &SigningKey, message: &[u8]) -> Vec<u8> {
        format!(
            "schema={SIGNATURE_SCHEMA}\nsignature={}\n",
            encode_hex(&key.sign(message).to_bytes())
        )
        .into_bytes()
    }

    #[test]
    fn valid_signed_record_manifest_capabilities_and_component_closure() {
        let fixture = Fixture::new(41);
        let verified = fixture.verify().unwrap();
        let repeated = fixture.verify().unwrap();
        assert_eq!(verified, repeated);
        assert_eq!(verified.version(), VERSION);
        assert_eq!(verified.tag(), TAG);
        assert_eq!(verified.source_commit(), COMMIT);
        assert_eq!(verified.ci_workflow_run_id(), 123456);
        assert_eq!(verified.component_count(), COMPONENT_ROLES.len());
        assert_eq!(
            verified.key_id(),
            format!("ed25519-sha256:{}", sha256_hex(&fixture.public_key()))
        );
        assert_eq!(
            presence_requirement("license-noto-emoji-upstream-info"),
            Some(PresenceRequirement::Optional)
        );
    }

    #[test]
    fn verification_api_rejects_every_relative_or_traversal_metadata_path_without_writes() {
        let fixture = Fixture::new(72);
        let before = snapshot_fixture_root(&fixture.root);
        for (index, field) in MetadataPathField::ALL.into_iter().enumerate() {
            let mut relative = fixture.paths.clone();
            field.set(
                &mut relative,
                PathBuf::from(format!("relative-metadata-{index}")),
            );
            assert_eq!(
                verify_paths_with_key(&relative, &fixture.public_key()),
                Err(ProvenanceError::UnsafeInput(field.label()))
            );

            let mut traversal = fixture.paths.clone();
            field.set(
                &mut traversal,
                fixture
                    .root
                    .join(format!("unused-{index}"))
                    .join("..")
                    .join("metadata"),
            );
            assert_eq!(
                verify_paths_with_key(&traversal, &fixture.public_key()),
                Err(ProvenanceError::UnsafeInput(field.label()))
            );
        }
        assert_eq!(before, snapshot_fixture_root(&fixture.root));
        assert!(fixture.verify().is_ok());
    }

    #[test]
    fn signed_record_cannot_declare_mandatory_executable_closure_absent() {
        let classified = COMPONENT_ROLES
            .iter()
            .map(|role| presence_requirement(role).expect("every schema-v1 role is classified"))
            .collect::<Vec<_>>();
        assert_eq!(
            classified
                .iter()
                .filter(|requirement| **requirement == PresenceRequirement::Mandatory)
                .count(),
            3
        );
        assert_eq!(
            classified
                .iter()
                .filter(|requirement| **requirement == PresenceRequirement::StageConditional)
                .count(),
            17
        );
        assert_eq!(
            classified
                .iter()
                .filter(|requirement| **requirement == PresenceRequirement::Optional)
                .count(),
            2
        );
        for (seed, role) in [
            (68, "scribe-app"),
            (69, "scribe-lifecycle"),
            (70, "scribe-lifecycle-host"),
        ] {
            let fixture = Fixture::new(seed);
            let valid = String::from_utf8(fixture.record_bytes()).unwrap();
            let prefix = format!("component={role}|file|");
            let original = valid
                .lines()
                .find(|line| line.starts_with(&prefix))
                .expect("mandatory fixture component line");
            let changed = valid.replace(original, &format!("component={role}|absent|0|-"));
            fixture.write_record(changed.as_bytes());
            assert_eq!(
                fixture.verify(),
                Err(ProvenanceError::MandatoryComponentAbsent(role))
            );
        }

        let fixture = Fixture::new(71);
        assert!(
            fixture.verify().is_ok(),
            "genuinely optional absence refused"
        );
    }

    #[test]
    fn official_authority_is_resolved_before_present_or_missing_evidence() {
        let fixture = Fixture::new(7);
        let expected_authority_error = match official_release_public_key() {
            Err(ReleaseVerifyError::OfficialKeyNotProvisioned) => {
                Some(ProvenanceError::OfficialKeyNotProvisioned)
            }
            Err(_) => Some(ProvenanceError::InvalidPinnedPublicKey),
            Ok(_) => None,
        };

        if let Some(expected) = expected_authority_error {
            let mut unsafe_paths = fixture.paths.clone();
            unsafe_paths.record = PathBuf::from("relative-record");
            let unsafe_expected = match official_release_public_key() {
                Err(ReleaseVerifyError::OfficialKeyNotProvisioned) => {
                    ProvenanceError::OfficialKeyNotProvisioned
                }
                Err(_) => ProvenanceError::InvalidPinnedPublicKey,
                Ok(_) => unreachable!("authority changed during one test"),
            };
            assert_eq!(verify_official(&unsafe_paths), Err(unsafe_expected));
            assert_eq!(verify_official(&fixture.paths), Err(expected));
            fs::remove_file(&fixture.paths.record).unwrap();
            fs::remove_file(&fixture.paths.record_signature).unwrap();
            let expected = match official_release_public_key() {
                Err(ReleaseVerifyError::OfficialKeyNotProvisioned) => {
                    ProvenanceError::OfficialKeyNotProvisioned
                }
                Err(_) => ProvenanceError::InvalidPinnedPublicKey,
                Ok(_) => unreachable!("authority changed during one test"),
            };
            assert_eq!(verify_official(&fixture.paths), Err(expected));
            let report = inspect_official(&InspectionConfig {
                subject: ReleaseSubject::CandidateB,
                paths: fixture.paths.clone(),
            });
            let text = String::from_utf8(report.canonical_bytes()).unwrap();
            assert!(text.contains("status=AUTHORITY_UNAVAILABLE\n"));
            assert!(
                text.contains("failure=official-key-not-provisioned\n")
                    || text.contains("failure=official-key-invalid\n")
            );
        } else {
            assert_eq!(
                verify_official(&fixture.paths),
                Err(ProvenanceError::InvalidSignature)
            );
            fs::remove_file(&fixture.paths.record).unwrap();
            assert_eq!(
                verify_official(&fixture.paths),
                Err(ProvenanceError::Missing("provenance record"))
            );
        }
    }

    #[test]
    fn exclusive_fixture_ownership_preserves_collisions_and_substitutions() {
        let parent = OwnedTestRoot::new("ignite-provenance-owner-parent");
        let collision = parent.path.join("collision");
        let candidate = parent.path.join("candidate");
        fs::create_dir(&collision).unwrap();
        let collision_sentinel = collision.join("foreign-sentinel");
        fs::write(&collision_sentinel, b"preserve collision").unwrap();

        let owned = OwnedTestRoot::from_candidates([collision.clone(), candidate.clone()])
            .expect("skip collision and claim candidate");
        assert_eq!(owned.path, candidate);
        assert_eq!(
            fs::read(&collision_sentinel).unwrap(),
            b"preserve collision"
        );

        let displaced = parent.path.join("displaced-owned-root");
        fs::rename(&candidate, &displaced).unwrap();
        fs::create_dir(&candidate).unwrap();
        let replacement_sentinel = candidate.join("foreign-replacement");
        fs::write(&replacement_sentinel, b"preserve replacement").unwrap();
        drop(owned);

        assert_eq!(
            fs::read(&replacement_sentinel).unwrap(),
            b"preserve replacement"
        );
        assert_eq!(
            fs::read(&collision_sentinel).unwrap(),
            b"preserve collision"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fixture_cleanup_refuses_a_symlink_substituted_for_the_owned_root() {
        use std::os::unix::fs::symlink;

        let parent = OwnedTestRoot::new("ignite-provenance-symlink-parent");
        let foreign = OwnedTestRoot::new("ignite-provenance-symlink-foreign");
        let candidate = parent.path.join("candidate");
        let displaced = parent.path.join("displaced");
        let owned = OwnedTestRoot::from_candidates([candidate.clone()]).unwrap();
        fs::rename(&candidate, &displaced).unwrap();
        let sentinel = foreign.path.join("foreign-sentinel");
        fs::write(&sentinel, b"never traverse").unwrap();
        symlink(&foreign.path, &candidate).unwrap();

        drop(owned);

        assert!(fs::symlink_metadata(&candidate)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&sentinel).unwrap(), b"never traverse");
        fs::remove_file(&candidate).unwrap();
    }

    #[test]
    fn modified_signed_bytes_wrong_key_and_record_swap_fail() {
        let fixture = Fixture::new(42);
        let mut changed = fs::read(&fixture.paths.record).unwrap();
        changed[0] ^= 1;
        fs::write(&fixture.paths.record, changed).unwrap();
        assert_eq!(fixture.verify(), Err(ProvenanceError::InvalidSignature));

        let fixture = Fixture::new(43);
        let wrong_key = SigningKey::from_bytes(&[44; 32]).verifying_key().to_bytes();
        assert_eq!(
            verify_paths_with_key(&fixture.paths, &wrong_key),
            Err(ProvenanceError::InvalidSignature)
        );
        let mut swapped = fixture.paths.clone();
        swapped.expected_tag = "v1.2.4".into();
        assert_eq!(
            verify_paths_with_key(&swapped, &fixture.public_key()),
            Err(ProvenanceError::UnexpectedReleaseTag)
        );
    }

    #[test]
    fn missing_malformed_and_wrong_domain_signatures_fail_closed() {
        let fixture = Fixture::new(45);
        fs::remove_file(&fixture.paths.record_signature).unwrap();
        assert_eq!(fixture.verify(), Err(ProvenanceError::MissingSignature));

        let fixture = Fixture::new(46);
        for malformed in [
            b"".as_slice(),
            b"signature=00\n".as_slice(),
            b"schema=ignite-installed-release-provenance-signature-v2\nsignature=00\n".as_slice(),
            b"schema=ignite-installed-release-provenance-signature-v1\nsignature=GG\n".as_slice(),
        ] {
            fs::write(&fixture.paths.record_signature, malformed).unwrap();
            assert!(matches!(
                fixture.verify(),
                Err(ProvenanceError::MissingSignature | ProvenanceError::MalformedSignature)
            ));
        }
        let record = fs::read(&fixture.paths.record).unwrap();
        fs::write(
            &fixture.paths.record_signature,
            release_signature_bytes(&fixture.signing_key, &record),
        )
        .unwrap();
        assert_eq!(fixture.verify(), Err(ProvenanceError::MalformedSignature));
    }

    #[test]
    fn signed_noncanonical_unknown_duplicate_and_reordered_records_are_rejected() {
        let fixture = Fixture::new(47);
        let valid = fixture.record_bytes();
        let text = String::from_utf8(valid.clone()).unwrap();
        let mut cases = vec![
            text.replace("schema=1\n", "schema=2\n").into_bytes(),
            text.replace("platform=kindle\n", "platform=kindle\nunknown=x\n")
                .into_bytes(),
            text.replace(
                "release_tag=v1.2.3\n",
                "release_tag=v1.2.3\nrelease_tag=v1.2.3\n",
            )
            .into_bytes(),
            text.replace("platform=kindle\narchitecture=", "architecture=")
                .into_bytes(),
            text.replace("\n", "\r\n").into_bytes(),
            valid[..valid.len() - 1].to_vec(),
            [valid.as_slice(), b"trailing"].concat(),
        ];
        let mut nul = valid.clone();
        nul[8] = 0;
        cases.push(nul);
        let first_component = format!("component={}|", COMPONENT_ROLES[0]);
        cases.push(
            text.replace(&first_component, "component=unknown-role|")
                .into_bytes(),
        );
        for case in cases {
            fixture.write_record(&case);
            assert!(matches!(
                fixture.verify(),
                Err(ProvenanceError::UnsupportedSchema | ProvenanceError::MalformedRecord(_))
            ));
        }
    }

    #[test]
    fn signed_identity_platform_version_and_numeric_errors_are_rejected() {
        let fixture = Fixture::new(48);
        let valid = String::from_utf8(fixture.record_bytes()).unwrap();
        let cases = [
            ("platform=kindle", "platform=linux"),
            (
                "architecture=armv7-unknown-linux-gnueabihf",
                "architecture=x86_64-unknown-linux-gnu",
            ),
            ("release_version=1.2.3", "release_version=01.2.3"),
            ("release_tag=v1.2.3", "release_tag=v1.2.4"),
            (&format!("source_commit={COMMIT}"), "source_commit=xyz"),
            ("ci_workflow_run_id=123456", "ci_workflow_run_id=0123456"),
            ("installer_bytes=34", "installer_bytes=0"),
            ("component_count=22", "component_count=21"),
            (
                "data_compatibility=unknown",
                "data_compatibility=compatible",
            ),
            ("promotion_authorized=false", "promotion_authorized=true"),
        ];
        for (old, new) in cases {
            let changed = valid.replace(old, new).into_bytes();
            fixture.write_record(&changed);
            assert!(fixture.verify().is_err(), "accepted {new}");
        }
    }

    #[test]
    fn release_manifest_chain_is_required_and_must_match() {
        let fixture = Fixture::new(49);
        fs::remove_file(&fixture.paths.release_manifest).unwrap();
        assert!(matches!(fixture.verify(), Err(ProvenanceError::Missing(_))));

        let fixture = Fixture::new(50);
        fs::write(&fixture.paths.release_manifest_signature, b"bad\n").unwrap();
        assert_eq!(
            fixture.verify(),
            Err(ProvenanceError::ReleaseManifestInvalid)
        );

        let fixture = Fixture::new(51);
        let manifest = String::from_utf8(release_manifest_bytes())
            .unwrap()
            .replace("bytes=34", "bytes=35")
            .into_bytes();
        fs::write(&fixture.paths.release_manifest, &manifest).unwrap();
        fs::write(
            &fixture.paths.release_manifest_signature,
            release_signature_bytes(&fixture.signing_key, &manifest),
        )
        .unwrap();
        assert_eq!(
            fixture.verify(),
            Err(ProvenanceError::ReleaseManifestMismatch)
        );
    }

    #[test]
    fn capability_and_component_missing_replacement_size_and_absence_fail() {
        let fixture = Fixture::new(52);
        fs::write(&fixture.paths.capabilities, b"changed capabilities\n").unwrap();
        assert_eq!(fixture.verify(), Err(ProvenanceError::CapabilitiesMismatch));

        let fixture = Fixture::new(53);
        let app = fixture
            .paths
            .components
            .iter()
            .find(|component| component.role == "scribe-app")
            .unwrap()
            .path
            .clone();
        fs::remove_file(&app).unwrap();
        assert_eq!(
            fixture.verify(),
            Err(ProvenanceError::ComponentMissing("scribe-app"))
        );

        let fixture = Fixture::new(54);
        let app = fixture
            .paths
            .components
            .iter()
            .find(|component| component.role == "scribe-app")
            .unwrap()
            .path
            .clone();
        let original = fs::read(&app).unwrap();
        fs::write(&app, vec![b'x'; original.len()]).unwrap();
        assert_eq!(
            fixture.verify(),
            Err(ProvenanceError::ComponentDigestMismatch("scribe-app"))
        );
        fs::write(&app, b"short").unwrap();
        assert_eq!(
            fixture.verify(),
            Err(ProvenanceError::ComponentSizeMismatch("scribe-app"))
        );

        let fixture = Fixture::new(55);
        let absent = fixture
            .paths
            .components
            .iter()
            .find(|component| component.role == "license-noto-emoji-upstream-info")
            .unwrap();
        fs::write(&absent.path, b"unexpected").unwrap();
        assert_eq!(
            fixture.verify(),
            Err(ProvenanceError::ComponentUnexpected(
                "license-noto-emoji-upstream-info"
            ))
        );

        for (seed, role) in [(66, "scribe-lifecycle"), (67, "scribe-lifecycle-host")] {
            let fixture = Fixture::new(seed);
            let path = fixture
                .paths
                .components
                .iter()
                .find(|component| component.role == role)
                .unwrap()
                .path
                .clone();
            let mut bytes = fs::read(&path).unwrap();
            bytes[0] ^= 1;
            fs::write(&path, bytes).unwrap();
            assert_eq!(
                fixture.verify(),
                Err(ProvenanceError::ComponentDigestMismatch(role))
            );
        }
    }

    #[test]
    fn component_map_is_closed_explicit_and_duplicate_free() {
        let fixture = Fixture::new(56);
        let mut missing = fixture.paths.clone();
        missing.components.pop();
        assert_eq!(
            verify_paths_with_key(&missing, &fixture.public_key()),
            Err(ProvenanceError::ComponentMapMalformed)
        );
        let mut duplicate = fixture.paths.clone();
        duplicate.components[1].role = duplicate.components[0].role.clone();
        assert_eq!(
            verify_paths_with_key(&duplicate, &fixture.public_key()),
            Err(ProvenanceError::ComponentMapMalformed)
        );
        let mut unknown = fixture.paths.clone();
        unknown.components[0].role = "unknown".into();
        assert_eq!(
            verify_paths_with_key(&unknown, &fixture.public_key()),
            Err(ProvenanceError::ComponentMapMalformed)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_hardlink_and_special_file_inputs_are_refused() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new(57);
        let app = fixture.paths.components[0].path.clone();
        let target = fixture.root.join("replacement");
        fs::write(&target, fs::read(&app).unwrap()).unwrap();
        fs::remove_file(&app).unwrap();
        symlink(&target, &app).unwrap();
        assert!(matches!(
            fixture.verify(),
            Err(ProvenanceError::UnsafeInput(_))
        ));

        let fixture = Fixture::new(58);
        let app = fixture.paths.components[0].path.clone();
        fs::hard_link(&app, fixture.root.join("hard-link")).unwrap();
        assert!(matches!(
            fixture.verify(),
            Err(ProvenanceError::UnsafeInput(_))
        ));

        let fixture = Fixture::new(59);
        let app = fixture.paths.components[0].path.clone();
        fs::remove_file(&app).unwrap();
        fs::create_dir(&app).unwrap();
        assert!(matches!(
            fixture.verify(),
            Err(ProvenanceError::UnsafeInput(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn replacement_identity_guard_detects_a_concurrent_path_change() {
        let fixture = Fixture::new(65);
        let app = fixture.paths.components[0].path.clone();
        let before = fs::symlink_metadata(&app).unwrap();
        let expected_bytes = before.len();
        let expected_sha256 =
            decode_hex::<32>(hash_file(&app, &before).unwrap().as_bytes()).unwrap();
        let replacement = fixture.root.join("concurrent-replacement");
        fs::write(&replacement, fs::read(&app).unwrap()).unwrap();
        assert_eq!(
            verify_exact_file_with_hook(
                &app,
                expected_bytes,
                &expected_sha256,
                "scribe-app",
                || {
                    fs::remove_file(&app).unwrap();
                    fs::rename(&replacement, &app).unwrap();
                },
            ),
            Err(ProvenanceError::StateChanged("scribe-app"))
        );
    }

    #[test]
    fn oversized_record_interrupted_publication_and_official_pin_fail_closed() {
        let fixture = Fixture::new(60);
        fs::write(
            &fixture.paths.record,
            vec![b'x'; MAX_RECORD_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(
            fixture.verify(),
            Err(ProvenanceError::InputTooLarge("provenance record"))
        );

        let fixture = Fixture::new(61);
        fs::remove_file(&fixture.paths.record_signature).unwrap();
        assert_eq!(fixture.verify(), Err(ProvenanceError::MissingSignature));

        // Seed 7 is one of release_manifest.rs's explicitly refused fixture keys. The official
        // entry point therefore refuses both before and after genuine key provisioning.
        let fixture = Fixture::new(7);
        assert!(matches!(
            verify_official(&fixture.paths),
            Err(ProvenanceError::OfficialKeyNotProvisioned
                | ProvenanceError::InvalidPinnedPublicKey
                | ProvenanceError::InvalidSignature)
        ));
    }

    #[test]
    fn report_is_deterministic_negative_and_never_authorizes() {
        let fixture = Fixture::new(7);
        let report = inspect_official(&InspectionConfig {
            subject: ReleaseSubject::CandidateB,
            paths: fixture.paths.clone(),
        });
        let text = String::from_utf8(report.canonical_bytes()).unwrap();
        assert_ne!(report.exit_code(), 0);
        assert!(
            text.contains("status=AUTHORITY_UNAVAILABLE\n")
                || text.contains("status=SIGNATURE_INVALID\n")
        );
        assert!(text.contains("release_version=unknown\n"));
        assert!(text.contains("components_verified=0\n"));
        assert!(text.contains("installation_binding=unverified\n"));
        assert!(text.contains("data_compatibility=unknown\n"));
        assert!(text.contains("promotion_authorized=false\n"));
        assert!(text.contains("inspection_action=report-only\n"));
    }

    #[test]
    fn command_requires_explicit_absolute_complete_paths() {
        let fixture = Fixture::new(64);
        assert!(matches!(
            parse_command(&fixture.args()).unwrap(),
            InspectionCommand::Inspect(_)
        ));
        let mut retained_a = fixture.args();
        let subject_index = retained_a
            .iter()
            .position(|arg| arg == "--subject")
            .unwrap()
            + 1;
        retained_a[subject_index] = "A".into();
        assert!(matches!(
            parse_command(&retained_a).unwrap(),
            InspectionCommand::Inspect(config) if config.subject == ReleaseSubject::RetainedA
        ));
        let mut missing = fixture.args();
        missing.truncate(missing.len() - 2);
        assert!(parse_command(&missing).is_err());
        let mut relative = fixture.args();
        let record_index = relative.iter().position(|arg| arg == "--record").unwrap() + 1;
        relative[record_index] = "relative-record".into();
        assert!(parse_command(&relative).is_err());
        let mut traversal = fixture.args();
        traversal[record_index] = fixture
            .root
            .join("unused")
            .join("..")
            .join("record")
            .display()
            .to_string();
        assert!(parse_command(&traversal).is_err());
        let duplicate = vec![
            "--inspect-release-provenance".into(),
            "--inspect-release-provenance".into(),
        ];
        assert!(parse_command(&duplicate).is_err());
        assert_eq!(
            parse_command(&["--help".into()]).unwrap(),
            InspectionCommand::NotRequested
        );
    }
}
