//! Read-only mapping of authenticated release-component claims onto Ignite's installed layout.
//!
//! This diagnostic is deliberately non-authorizing. It can show that caller-supplied signed
//! component bytes occupy the paths used by the current installer topology, and it can compare
//! those observations with `pending-health.v1` and the retained inventory. The installer does not
//! yet publish a trusted transaction-scoped installation identity, the production signing pin is
//! unprovisioned, and pathname inspection cannot bind a future `exec`. Consequently every report
//! keeps installation/execution binding unverified and returns a nonzero status.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

#[cfg(test)]
use crate::release_manifest::encode_hex;
use crate::release_manifest::{
    official_release_public_key, valid_release_version, ReleaseVerifyError,
};
use crate::storage_inspect_library::{
    hash_file, inspect_metadata, open_regular_read_only, path_chain_contains_symlink,
};
use crate::storage_release_provenance::{
    self, AuthenticatedReleaseComponentSet, ComponentPath, ProvenanceError, VerificationPaths,
    COMPONENT_ROLES,
};

pub const REPORT_HEADER: &str = "IGNITE_INSTALLED_RELEASE_BINDING_V1";
const MAX_PENDING_BYTES: u64 = 4 * 1024;
const MAX_INVENTORY_BYTES: u64 = 32 * 1024;
const MAX_COMMIT_BYTES: u64 = 512;
const MAX_DIRECTORY_ENTRIES: usize = 64;
const TOTAL_COMPONENTS: usize = 22;

const INVENTORY_NAMES: [&str; 18] = [
    "scribe-app",
    "scribe-lifecycle",
    "scribe-lifecycle-host.sh",
    "Scribe.sh",
    "scribe-icon.png",
    "OFL.txt",
    "DESCRIPTION.en_us.html",
    "upstream_info.md",
    "source-sans-3-OFL.txt",
    "install-runtime.sh",
    "uninstall-runtime.sh",
    "backup-runtime.sh",
    "register-helpers.sh",
    "install-recovery.sh",
    "Uninstall Ignite.sh",
    "Backup Ignite Data.sh",
    "Diagnose Ignite.sh",
    "appreg-handler.sql",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseRole {
    RetainedA,
    CandidateB,
}

impl ReleaseRole {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "retained-a" => Some(Self::RetainedA),
            "candidate-b" => Some(Self::CandidateB),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::RetainedA => "retained_a",
            Self::CandidateB => "candidate_b",
        }
    }

    const fn mapped_count(self) -> usize {
        match self {
            Self::RetainedA => 17,
            Self::CandidateB => 20,
        }
    }

    const fn mapping_gaps(self) -> &'static str {
        match self {
            Self::RetainedA => {
                "registration-source,recovery-dropbear,recovery-elf-sha256,recovery-provenance,recovery-version"
            }
            Self::CandidateB => "registration-source,recovery-version",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotBindingConfig {
    pub release_role: ReleaseRole,
    pub expected_transaction: String,
    pub app_root: PathBuf,
    pub documents_root: PathBuf,
    pub recovery_root: PathBuf,
    pub provenance: VerificationPaths,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectionCommand {
    NotRequested,
    Inspect(Box<SlotBindingConfig>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectionReport {
    release_role: ReleaseRole,
    result: &'static str,
    failure: &'static str,
    expected_transaction: String,
    release_version: String,
    release_tag: String,
    artifact_authenticity: &'static str,
    provenance_result: &'static str,
    component_integrity: &'static str,
    installed_layout: &'static str,
    pending_health: &'static str,
    retained_inventory: &'static str,
    publication_state: &'static str,
    pending_transaction_binding: &'static str,
    production_trust_pin: &'static str,
    exit_code: u8,
}

impl InspectionReport {
    fn new(config: &SlotBindingConfig, production_trust_pin: &'static str) -> Self {
        Self {
            release_role: config.release_role,
            result: "INCOMPLETE",
            failure: "inspection-not-complete",
            expected_transaction: config.expected_transaction.clone(),
            release_version: "unknown".into(),
            release_tag: "unknown".into(),
            artifact_authenticity: "unverified",
            provenance_result: "unverified",
            component_integrity: "unverified",
            installed_layout: "unverified",
            pending_health: "unverified",
            retained_inventory: "unverified",
            publication_state: "unverified",
            pending_transaction_binding: "unverified",
            production_trust_pin,
            exit_code: 3,
        }
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        format!(
            "{REPORT_HEADER}\nschema=1\nresult={}\nfailure={}\nrelease_role={}\nexpected_transaction={}\nrelease_version={}\nrelease_tag={}\nartifact_authenticity={}\nprovenance_result={}\ncomponent_integrity={}\ninstalled_layout={}\ncomponent_profile=partial\ncomponents_mapped={}\ncomponents_total={}\nmapping_gaps={}\npending_health={}\nretained_inventory={}\npublication_state={}\npending_transaction_binding={}\ninstallation_binding=unverified\nexecution_binding=unverified\nproduction_trust_pin={}\nfirst_launch_admission=unavailable\natomic_snapshot=false\ndata_compatibility=unknown\npromotion_authorized=false\ninspection_action=report-only\n",
            self.result,
            self.failure,
            self.release_role.as_str(),
            self.expected_transaction,
            self.release_version,
            self.release_tag,
            self.artifact_authenticity,
            self.provenance_result,
            self.component_integrity,
            self.installed_layout,
            self.release_role.mapped_count(),
            TOTAL_COMPONENTS,
            self.release_role.mapping_gaps(),
            self.pending_health,
            self.retained_inventory,
            self.publication_state,
            self.pending_transaction_binding,
            self.production_trust_pin,
        )
        .into_bytes()
    }

    pub const fn exit_code(&self) -> u8 {
        self.exit_code
    }

    fn reject(&mut self, error: &SlotError) {
        self.result = error.result();
        self.failure = error.failure();
        self.exit_code = error.exit_code();
        match error {
            SlotError::Unsafe(_) => self.installed_layout = "unsafe",
            SlotError::Changed(_) => self.installed_layout = "changed",
            SlotError::Mismatch(_) | SlotError::Invalid(_) => self.installed_layout = "mismatch",
            SlotError::Missing(_)
            | SlotError::Limit(_)
            | SlotError::Io(_)
            | SlotError::Interrupted => {
                if self.installed_layout == "unverified" {
                    self.installed_layout = "incomplete";
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SlotError {
    Missing(&'static str),
    Unsafe(&'static str),
    Invalid(&'static str),
    Mismatch(&'static str),
    Changed(&'static str),
    Limit(&'static str),
    Io(&'static str),
    Interrupted,
}

impl SlotError {
    const fn result(&self) -> &'static str {
        match self {
            Self::Unsafe(_) => "UNSAFE_INPUT",
            Self::Invalid(_) | Self::Mismatch(_) => "INCONSISTENT",
            Self::Changed(_) => "STATE_CHANGED",
            Self::Missing(_) | Self::Limit(_) | Self::Io(_) | Self::Interrupted => "INCOMPLETE",
        }
    }

    const fn failure(&self) -> &'static str {
        match self {
            Self::Missing(label)
            | Self::Unsafe(label)
            | Self::Invalid(label)
            | Self::Mismatch(label)
            | Self::Changed(label)
            | Self::Limit(label)
            | Self::Io(label) => label,
            Self::Interrupted => "inspection-interrupted",
        }
    }

    const fn exit_code(&self) -> u8 {
        match self {
            Self::Changed(_) => 4,
            Self::Invalid(_) | Self::Mismatch(_) => 1,
            _ => 3,
        }
    }
}

#[derive(Clone, Debug)]
struct StableBytes {
    bytes: Vec<u8>,
    metadata: fs::Metadata,
}

#[derive(Clone, Debug)]
struct PendingHealth {
    stable: StableBytes,
    version: String,
    transaction: String,
    app_bytes: u64,
    app_sha256: [u8; 32],
    lifecycle_bytes: u64,
    lifecycle_sha256: [u8; 32],
    inventory_bytes: u64,
    inventory_sha256: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InventoryPresence {
    File { bytes: u64, sha256: [u8; 32] },
    Absent,
}

#[derive(Clone, Debug)]
struct RetainedInventory {
    stable: StableBytes,
    entries: BTreeMap<&'static str, InventoryPresence>,
}

pub fn parse_command(args: &[String]) -> Result<InspectionCommand, String> {
    let requested = args
        .iter()
        .filter(|argument| argument.as_str() == "--inspect-installed-slot-binding")
        .count();
    if requested == 0 {
        return Ok(InspectionCommand::NotRequested);
    }
    if requested != 1
        || args.first().map(String::as_str) != Some("--inspect-installed-slot-binding")
    {
        return Err(
            "--inspect-installed-slot-binding must be the first and only diagnostic command".into(),
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
            components.push(ComponentPath {
                role: role.into(),
                path: PathBuf::from(path),
            });
            continue;
        }
        let key = match flag {
            "--release-role"
            | "--expected-transaction"
            | "--app-root"
            | "--documents-root"
            | "--recovery-root"
            | "--expected-tag"
            | "--record"
            | "--record-signature"
            | "--release-manifest"
            | "--release-manifest-signature"
            | "--capabilities" => flag,
            _ => return Err(format!("unexpected installed-slot option: {flag}")),
        };
        if values.insert(key, value).is_some() {
            return Err(format!("duplicate installed-slot option: {key}"));
        }
    }

    let take = |key: &'static str| {
        values
            .get(key)
            .cloned()
            .ok_or_else(|| format!("missing installed-slot option: {key}"))
    };
    let release_role = ReleaseRole::parse(&take("--release-role")?)
        .ok_or_else(|| "--release-role must be retained-a or candidate-b".to_owned())?;
    let expected_transaction = take("--expected-transaction")?;
    if !valid_transaction(&expected_transaction) {
        return Err("--expected-transaction is not canonical".into());
    }
    let expected_tag = take("--expected-tag")?;
    if !expected_tag.starts_with('v') || !valid_release_version(&expected_tag[1..]) {
        return Err("--expected-tag must be a canonical v-prefixed Ignite version".into());
    }
    let config = SlotBindingConfig {
        release_role,
        expected_transaction,
        app_root: PathBuf::from(take("--app-root")?),
        documents_root: PathBuf::from(take("--documents-root")?),
        recovery_root: PathBuf::from(take("--recovery-root")?),
        provenance: VerificationPaths {
            expected_tag,
            record: PathBuf::from(take("--record")?),
            record_signature: PathBuf::from(take("--record-signature")?),
            release_manifest: PathBuf::from(take("--release-manifest")?),
            release_manifest_signature: PathBuf::from(take("--release-manifest-signature")?),
            capabilities: PathBuf::from(take("--capabilities")?),
            components,
        },
    };
    validate_config(&config).map_err(|error| error.failure().to_owned())?;
    Ok(InspectionCommand::Inspect(Box::new(config)))
}

fn explicit_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
}

fn validate_config(config: &SlotBindingConfig) -> Result<(), SlotError> {
    for root in [
        &config.app_root,
        &config.documents_root,
        &config.recovery_root,
    ] {
        if !explicit_absolute(root) {
            return Err(SlotError::Unsafe("unsafe-explicit-root"));
        }
    }
    if config.app_root == config.documents_root
        || config.app_root == config.recovery_root
        || config.documents_root == config.recovery_root
        || config.app_root.starts_with(&config.documents_root)
        || config.documents_root.starts_with(&config.app_root)
        || config.app_root.starts_with(&config.recovery_root)
        || config.recovery_root.starts_with(&config.app_root)
        || config.documents_root.starts_with(&config.recovery_root)
        || config.recovery_root.starts_with(&config.documents_root)
    {
        return Err(SlotError::Unsafe("overlapping-layout-roots"));
    }
    if !valid_transaction(&config.expected_transaction) {
        return Err(SlotError::Invalid("invalid-expected-transaction"));
    }
    component_map(&config.provenance.components)?;
    storage_release_provenance::validate_verification_paths(&config.provenance).map_err(
        |error| {
            if matches!(error, ProvenanceError::ComponentMapMalformed) {
                SlotError::Invalid("component-map-ambiguous")
            } else {
                SlotError::Unsafe("unsafe-provenance-path-map")
            }
        },
    )?;
    Ok(())
}

fn valid_transaction(value: &str) -> bool {
    if value.len() < 18 || value.len() > 64 {
        return false;
    }
    let bytes = value.as_bytes();
    bytes.get(8) == Some(&b'T')
        && bytes.get(15) == Some(&b'Z')
        && bytes.get(16) == Some(&b'-')
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && bytes[17..].iter().all(u8::is_ascii_digit)
}

fn expected_component_path(config: &SlotBindingConfig, role: &str) -> Option<PathBuf> {
    let app = &config.app_root;
    let docs = &config.documents_root;
    let recovery = &config.recovery_root;
    match config.release_role {
        ReleaseRole::CandidateB => match role {
            "scribe-app" => Some(app.join("bin/scribe-app")),
            "scribe-lifecycle" => Some(app.join("bin/scribe-lifecycle")),
            "scribe-lifecycle-host" => Some(app.join("bin/scribe-lifecycle-host.sh")),
            "launcher" => Some(docs.join("Scribe.sh")),
            "registration-source" | "recovery-version" => None,
            "icon" => Some(app.join("assets/scribe-icon.png")),
            "license-noto-emoji" => Some(app.join("assets/licenses/noto-emoji/OFL.txt")),
            "license-noto-emoji-description" => {
                Some(app.join("assets/licenses/noto-emoji/DESCRIPTION.en_us.html"))
            }
            "license-noto-emoji-upstream-info" => {
                Some(app.join("assets/licenses/noto-emoji/upstream_info.md"))
            }
            "license-source-sans-3" => Some(app.join("assets/licenses/source-sans-3/OFL.txt")),
            "install-runtime" => Some(app.join("tools/ignite-release/lib/install-runtime.sh")),
            "uninstall-runtime" => Some(app.join("tools/ignite-release/lib/uninstall-runtime.sh")),
            "backup-runtime" => Some(app.join("tools/ignite-release/lib/backup-runtime.sh")),
            "register-helpers" => Some(app.join("tools/ignite-release/lib/register-helpers.sh")),
            "install-recovery" => Some(app.join("tools/ignite-release/lib/install-recovery.sh")),
            "uninstall-entry" => Some(docs.join("Uninstall Ignite.sh")),
            "backup-entry" => Some(docs.join("Backup Ignite Data.sh")),
            "diagnose-entry" => Some(docs.join("Diagnose Ignite.sh")),
            "recovery-dropbear" => Some(recovery.join("bin/dropbearmulti")),
            "recovery-elf-sha256" => Some(recovery.join("manifest/elf.sha256")),
            "recovery-provenance" => Some(recovery.join("manifest/provenance.txt")),
            _ => None,
        },
        ReleaseRole::RetainedA => {
            let retained = app.join("previous-release");
            let name = match role {
                "scribe-app" => "scribe-app",
                "scribe-lifecycle" => "scribe-lifecycle",
                "scribe-lifecycle-host" => "scribe-lifecycle-host.sh",
                "launcher" => "Scribe.sh",
                "icon" => "scribe-icon.png",
                "license-noto-emoji" => "OFL.txt",
                "license-noto-emoji-description" => "DESCRIPTION.en_us.html",
                "license-noto-emoji-upstream-info" => "upstream_info.md",
                "license-source-sans-3" => "source-sans-3-OFL.txt",
                "install-runtime" => "install-runtime.sh",
                "uninstall-runtime" => "uninstall-runtime.sh",
                "backup-runtime" => "backup-runtime.sh",
                "register-helpers" => "register-helpers.sh",
                "install-recovery" => "install-recovery.sh",
                "uninstall-entry" => "Uninstall Ignite.sh",
                "backup-entry" => "Backup Ignite Data.sh",
                "diagnose-entry" => "Diagnose Ignite.sh",
                _ => return None,
            };
            Some(retained.join(name))
        }
    }
}

fn component_map(paths: &[ComponentPath]) -> Result<BTreeMap<&str, &Path>, SlotError> {
    let mut result = BTreeMap::new();
    let mut actual = BTreeSet::new();
    if paths.len() != COMPONENT_ROLES.len() {
        return Err(SlotError::Invalid("component-map-not-closed"));
    }
    for component in paths {
        if !explicit_absolute(&component.path) {
            return Err(SlotError::Unsafe("unsafe-component-path"));
        }
        if !COMPONENT_ROLES.contains(&component.role.as_str())
            || result
                .insert(component.role.as_str(), component.path.as_path())
                .is_some()
            || !actual.insert(component.path.as_path())
        {
            return Err(SlotError::Invalid("component-map-ambiguous"));
        }
    }
    Ok(result)
}

fn validate_layout_mapping(config: &SlotBindingConfig) -> Result<(), SlotError> {
    let components = component_map(&config.provenance.components)?;
    for role in COMPONENT_ROLES {
        if let Some(expected) = expected_component_path(config, role) {
            if components.get(role).copied() != Some(expected.as_path()) {
                return Err(SlotError::Mismatch(
                    "component-path-outside-installed-layout",
                ));
            }
        }
    }
    Ok(())
}

fn read_stable(path: &Path, max_bytes: u64, label: &'static str) -> Result<StableBytes, SlotError> {
    if path_chain_contains_symlink(path) {
        return Err(SlotError::Unsafe(label));
    }
    let before = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SlotError::Missing(label));
        }
        Err(_) => return Err(SlotError::Io(label)),
        Ok(metadata) => metadata,
    };
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(SlotError::Unsafe(label));
    }
    if before.len() > max_bytes {
        return Err(SlotError::Limit(label));
    }
    let mut file = open_regular_read_only(path, &before).map_err(|reason| map_io(reason, label))?;
    let mut bytes = Vec::with_capacity(before.len() as usize);
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| SlotError::Io(label))?;
    if bytes.len() as u64 != before.len() {
        return Err(SlotError::Changed(label));
    }
    let after = inspect_metadata(path).map_err(|_| SlotError::Changed(label))?;
    if !same_identity(&before, &after) {
        return Err(SlotError::Changed(label));
    }
    Ok(StableBytes {
        bytes,
        metadata: after,
    })
}

fn map_io(reason: &'static str, label: &'static str) -> SlotError {
    match reason {
        "symlink_refused" | "hard_link_refused" | "unsupported_file_type" => {
            SlotError::Unsafe(label)
        }
        "file_size_limit_exceeded" => SlotError::Limit(label),
        "file_changed_during_open" | "file_changed_during_read" => SlotError::Changed(label),
        _ => SlotError::Io(label),
    }
}

fn observe_file(
    path: &Path,
    label: &'static str,
) -> Result<(u64, [u8; 32], fs::Metadata), SlotError> {
    if path_chain_contains_symlink(path) {
        return Err(SlotError::Unsafe(label));
    }
    let before = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SlotError::Missing(label));
        }
        Err(_) => return Err(SlotError::Io(label)),
        Ok(metadata) => metadata,
    };
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(SlotError::Unsafe(label));
    }
    let digest = hash_file(path, &before).map_err(|reason| map_io(reason, label))?;
    let digest =
        crate::release_manifest::decode_hex::<32>(digest.as_bytes()).ok_or(SlotError::Io(label))?;
    let after = inspect_metadata(path).map_err(|_| SlotError::Changed(label))?;
    if !same_identity(&before, &after) {
        return Err(SlotError::Changed(label));
    }
    Ok((after.len(), digest, after))
}

fn observe_absent(path: &Path, label: &'static str) -> Result<(), SlotError> {
    if path_chain_contains_symlink(path) {
        return Err(SlotError::Unsafe(label));
    }
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(SlotError::Io(label)),
        Ok(_) => Err(SlotError::Mismatch(label)),
    }
}

fn observe_directory(path: &Path, label: &'static str) -> Result<fs::Metadata, SlotError> {
    if path_chain_contains_symlink(path) {
        return Err(SlotError::Unsafe(label));
    }
    let before = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SlotError::Missing(label)
        } else {
            SlotError::Io(label)
        }
    })?;
    if before.file_type().is_symlink() || !before.is_dir() {
        return Err(SlotError::Unsafe(label));
    }
    Ok(before)
}

fn verify_closed_directory(
    path: &Path,
    allowed: &[&str],
    label: &'static str,
) -> Result<(), SlotError> {
    let before = observe_directory(path, label)?;
    let mut count = 0usize;
    let entries = fs::read_dir(path).map_err(|_| SlotError::Io(label))?;
    for entry in entries {
        count = count.saturating_add(1);
        if count > MAX_DIRECTORY_ENTRIES {
            return Err(SlotError::Limit(label));
        }
        let entry = entry.map_err(|_| SlotError::Io(label))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| SlotError::Invalid("non-utf8-release-owned-entry"))?;
        if !allowed.contains(&name.as_str()) {
            return Err(SlotError::Mismatch("extra-unregistered-component"));
        }
        let metadata = entry
            .path()
            .symlink_metadata()
            .map_err(|_| SlotError::Io(label))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SlotError::Unsafe("unsafe-release-owned-entry"));
        }
    }
    let after = inspect_metadata(path).map_err(|_| SlotError::Changed(label))?;
    if !same_identity(&before, &after) {
        return Err(SlotError::Changed(label));
    }
    Ok(())
}

fn verify_closed_mixed_directory(
    path: &Path,
    allowed_files: &[&str],
    allowed_directories: &[&str],
    label: &'static str,
) -> Result<(), SlotError> {
    let before = observe_directory(path, label)?;
    let mut count = 0usize;
    for entry in fs::read_dir(path).map_err(|_| SlotError::Io(label))? {
        count = count.saturating_add(1);
        if count > MAX_DIRECTORY_ENTRIES {
            return Err(SlotError::Limit(label));
        }
        let entry = entry.map_err(|_| SlotError::Io(label))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| SlotError::Invalid("non-utf8-release-owned-entry"))?;
        let metadata = entry
            .path()
            .symlink_metadata()
            .map_err(|_| SlotError::Io(label))?;
        if metadata.file_type().is_symlink()
            || !(allowed_files.contains(&name.as_str()) && metadata.is_file()
                || allowed_directories.contains(&name.as_str()) && metadata.is_dir())
        {
            return Err(
                if allowed_files.contains(&name.as_str())
                    || allowed_directories.contains(&name.as_str())
                {
                    SlotError::Unsafe("unsafe-release-owned-entry")
                } else {
                    SlotError::Mismatch("extra-unregistered-component")
                },
            );
        }
    }
    let after = inspect_metadata(path).map_err(|_| SlotError::Changed(label))?;
    if !same_identity(&before, &after) {
        return Err(SlotError::Changed(label));
    }
    Ok(())
}

fn canonical_text(bytes: &[u8]) -> bool {
    bytes.last() == Some(&b'\n')
        && bytes
            .iter()
            .all(|byte| *byte == b'\n' || matches!(*byte, b' '..=b'~'))
}

fn parse_decimal(value: &str) -> Option<u64> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok()
}

fn parse_digest(value: &str) -> Option<[u8; 32]> {
    crate::release_manifest::decode_hex(value.as_bytes())
}

fn field<'a>(line: &'a str, prefix: &str) -> Result<&'a str, SlotError> {
    line.strip_prefix(prefix)
        .ok_or(SlotError::Invalid("noncanonical-state-field"))
}

fn read_pending(path: &Path) -> Result<PendingHealth, SlotError> {
    let stable = read_stable(path, MAX_PENDING_BYTES, "pending-health-input")?;
    if !canonical_text(&stable.bytes) {
        return Err(SlotError::Invalid("pending-health-noncanonical"));
    }
    let text = std::str::from_utf8(&stable.bytes)
        .map_err(|_| SlotError::Invalid("pending-health-noncanonical"))?;
    let lines: Vec<&str> = text[..text.len() - 1].split('\n').collect();
    if lines.len() != 14
        || lines[0] != "IGNITE_PENDING_HEALTH_V1"
        || lines[1] != "schema=1"
        || lines[2] != "status=pending-health"
        || lines[9] != "retained_version=unknown"
        || lines[13] != "data_compatibility=unknown"
    {
        return Err(SlotError::Invalid("pending-health-noncanonical"));
    }
    let version = field(lines[3], "candidate_version=")?.to_owned();
    let transaction = field(lines[4], "candidate_txn=")?.to_owned();
    let app_bytes = parse_decimal(field(lines[5], "candidate_app_bytes=")?)
        .ok_or(SlotError::Invalid("pending-health-noncanonical"))?;
    let app_sha256 = parse_digest(field(lines[6], "candidate_app_sha256=")?)
        .ok_or(SlotError::Invalid("pending-health-noncanonical"))?;
    let lifecycle_bytes = parse_decimal(field(lines[7], "candidate_lifecycle_bytes=")?)
        .ok_or(SlotError::Invalid("pending-health-noncanonical"))?;
    let lifecycle_sha256 = parse_digest(field(lines[8], "candidate_lifecycle_sha256=")?)
        .ok_or(SlotError::Invalid("pending-health-noncanonical"))?;
    let retained_identity = parse_digest(field(lines[10], "retained_release_identity=sha256:")?)
        .ok_or(SlotError::Invalid("pending-health-noncanonical"))?;
    let inventory_bytes = parse_decimal(field(lines[11], "lkg_inventory_bytes=")?)
        .ok_or(SlotError::Invalid("pending-health-noncanonical"))?;
    let inventory_sha256 = parse_digest(field(lines[12], "lkg_inventory_sha256=")?)
        .ok_or(SlotError::Invalid("pending-health-noncanonical"))?;
    if !valid_release_version(&version)
        || !valid_transaction(&transaction)
        || retained_identity != inventory_sha256
    {
        return Err(SlotError::Invalid("pending-health-noncanonical"));
    }
    Ok(PendingHealth {
        stable,
        version,
        transaction,
        app_bytes,
        app_sha256,
        lifecycle_bytes,
        lifecycle_sha256,
        inventory_bytes,
        inventory_sha256,
    })
}

fn read_inventory(path: &Path) -> Result<RetainedInventory, SlotError> {
    let stable = read_stable(path, MAX_INVENTORY_BYTES, "retained-inventory-input")?;
    if !canonical_text(&stable.bytes) {
        return Err(SlotError::Invalid("retained-inventory-noncanonical"));
    }
    let text = std::str::from_utf8(&stable.bytes)
        .map_err(|_| SlotError::Invalid("retained-inventory-noncanonical"))?;
    let lines: Vec<&str> = text[..text.len() - 1].split('\n').collect();
    if lines.len() != 4 + INVENTORY_NAMES.len()
        || lines[0] != "IGNITE_LKG_INVENTORY_V1"
        || lines[1] != "schema=1"
        || lines[2] != "kind=ignite-application-release"
        || lines[3] != "entry_count=18"
    {
        return Err(SlotError::Invalid("retained-inventory-noncanonical"));
    }
    let mut entries = BTreeMap::new();
    for (index, expected_name) in INVENTORY_NAMES.iter().copied().enumerate() {
        let value = field(lines[4 + index], "entry=")?;
        let mut parts = value.split('|');
        let name = parts.next();
        let kind = parts.next();
        let bytes = parts.next();
        let digest = parts.next();
        if parts.next().is_some() || name != Some(expected_name) {
            return Err(SlotError::Invalid("retained-inventory-noncanonical"));
        }
        let presence = match (kind, bytes, digest) {
            (Some("absent"), Some("0"), Some("-")) => InventoryPresence::Absent,
            (Some("file"), Some(bytes), Some(digest)) => InventoryPresence::File {
                bytes: parse_decimal(bytes)
                    .filter(|bytes| *bytes > 0)
                    .ok_or(SlotError::Invalid("retained-inventory-noncanonical"))?,
                sha256: parse_digest(digest)
                    .ok_or(SlotError::Invalid("retained-inventory-noncanonical"))?,
            },
            _ => return Err(SlotError::Invalid("retained-inventory-noncanonical")),
        };
        entries.insert(expected_name, presence);
    }
    for required in ["scribe-app", "scribe-lifecycle", "scribe-lifecycle-host.sh"] {
        if !matches!(entries.get(required), Some(InventoryPresence::File { .. })) {
            return Err(SlotError::Invalid(
                "retained-inventory-required-component-absent",
            ));
        }
    }
    Ok(RetainedInventory { stable, entries })
}

fn verify_inventory_files(
    retained_root: &Path,
    inventory: &RetainedInventory,
) -> Result<(), SlotError> {
    let mut allowed = INVENTORY_NAMES.to_vec();
    allowed.push("ignite-lkg-inventory.v1");
    verify_closed_directory(retained_root, &allowed, "retained-release-directory")?;
    for name in INVENTORY_NAMES {
        let path = retained_root.join(name);
        match inventory.entries.get(name) {
            Some(InventoryPresence::File { bytes, sha256 }) => {
                let (actual_bytes, actual_digest, _) = observe_file(&path, "retained-component")?;
                if actual_bytes != *bytes || actual_digest != *sha256 {
                    return Err(SlotError::Mismatch("retained-inventory-component-mismatch"));
                }
            }
            Some(InventoryPresence::Absent) => {
                observe_absent(&path, "retained-inventory-absent-component-present")?;
            }
            None => return Err(SlotError::Invalid("retained-inventory-not-closed")),
        }
    }
    Ok(())
}

fn verify_candidate_closed_directories(config: &SlotBindingConfig) -> Result<(), SlotError> {
    verify_closed_directory(
        &config.app_root.join("bin"),
        &["scribe-app", "scribe-lifecycle", "scribe-lifecycle-host.sh"],
        "candidate-bin-directory",
    )?;
    verify_closed_directory(
        &config.app_root.join("tools/ignite-release/lib"),
        &[
            "install-runtime.sh",
            "uninstall-runtime.sh",
            "backup-runtime.sh",
            "register-helpers.sh",
            "install-recovery.sh",
        ],
        "candidate-helper-directory",
    )?;
    verify_closed_mixed_directory(
        &config.app_root.join("assets"),
        &["scribe-icon.png"],
        &["licenses"],
        "candidate-assets-directory",
    )?;
    verify_closed_mixed_directory(
        &config.app_root.join("assets/licenses"),
        &[],
        &["noto-emoji", "source-sans-3"],
        "candidate-licenses-directory",
    )?;
    verify_closed_directory(
        &config.app_root.join("assets/licenses/noto-emoji"),
        &["OFL.txt", "DESCRIPTION.en_us.html", "upstream_info.md"],
        "candidate-noto-license-directory",
    )?;
    verify_closed_directory(
        &config.app_root.join("assets/licenses/source-sans-3"),
        &["OFL.txt"],
        "candidate-source-sans-license-directory",
    )?;
    verify_closed_directory(
        &config.recovery_root.join("bin"),
        &["dropbearmulti"],
        "candidate-recovery-bin-directory",
    )?;
    verify_closed_directory(
        &config.recovery_root.join("manifest"),
        &["elf.sha256", "provenance.txt", "version"],
        "candidate-recovery-manifest-directory",
    )?;
    // The recovery installer generates this file rather than copying a signed package component.
    // Its presence is part of observed publication completeness, but it remains an explicit
    // component-profile mapping gap and cannot contribute authenticated integrity.
    let _ = observe_file(
        &config.recovery_root.join("manifest/version"),
        "generated-recovery-version",
    )?;
    Ok(())
}

fn verify_transition_state(config: &SlotBindingConfig) -> Result<(), SlotError> {
    let mut recovery_staging = config.recovery_root.as_os_str().to_os_string();
    recovery_staging.push(".staging");
    let paths = [
        config.app_root.join("run/ignite-activate.journal"),
        config.app_root.join("previous-release.prev"),
        config.app_root.join("previous-release.staging"),
        config.app_root.join("bin/scribe-app.new"),
        config.app_root.join("bin/scribe-lifecycle.new"),
        config.app_root.join("bin/scribe-lifecycle-host.sh.new"),
        config.documents_root.join("Scribe.sh.new"),
        config.documents_root.join("Uninstall Ignite.sh.new"),
        config.documents_root.join("Backup Ignite Data.sh.new"),
        config.documents_root.join("Diagnose Ignite.sh.new"),
        PathBuf::from(recovery_staging),
    ];
    for path in paths {
        observe_absent(&path, "incomplete-installer-publication")?;
    }
    let commit = config.app_root.join("run/ignite-activate.committed");
    match fs::symlink_metadata(&commit) {
        Ok(_) => {
            let bytes = read_stable(&commit, MAX_COMMIT_BYTES, "activation-commit-marker")?;
            let expected = format!(
                "ignite-activate-commit-v2 txn={} pending_health=1\n",
                config.expected_transaction
            );
            if bytes.bytes != expected.as_bytes() {
                return Err(SlotError::Mismatch("activation-commit-marker-mismatch"));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if path_chain_contains_symlink(&commit) {
                return Err(SlotError::Unsafe("activation-commit-marker"));
            }
        }
        Err(_) => return Err(SlotError::Io("activation-commit-marker")),
    }
    Ok(())
}

fn verify_pending_candidate(
    config: &SlotBindingConfig,
    pending: &PendingHealth,
) -> Result<(), SlotError> {
    if pending.transaction != config.expected_transaction {
        return Err(SlotError::Mismatch("wrong-pending-transaction"));
    }
    let tag_version = config
        .provenance
        .expected_tag
        .strip_prefix('v')
        .ok_or(SlotError::Invalid("invalid-expected-tag"))?;
    if config.release_role == ReleaseRole::CandidateB && pending.version != tag_version {
        return Err(SlotError::Mismatch(
            "candidate-version-does-not-match-pending",
        ));
    }
    let (app_bytes, app_sha256, _) = observe_file(
        &config.app_root.join("bin/scribe-app"),
        "live-candidate-application",
    )?;
    let (life_bytes, life_sha256, _) = observe_file(
        &config.app_root.join("bin/scribe-lifecycle"),
        "live-candidate-lifecycle",
    )?;
    if app_bytes != pending.app_bytes
        || app_sha256 != pending.app_sha256
        || life_bytes != pending.lifecycle_bytes
        || life_sha256 != pending.lifecycle_sha256
    {
        return Err(SlotError::Mismatch("live-candidate-does-not-match-pending"));
    }
    Ok(())
}

fn verify_authenticated_relationships(
    config: &SlotBindingConfig,
    release: &AuthenticatedReleaseComponentSet,
    pending: &PendingHealth,
    inventory: &RetainedInventory,
) -> Result<(), SlotError> {
    if release.tag() != config.provenance.expected_tag {
        return Err(SlotError::Mismatch("authenticated-release-tag-mismatch"));
    }
    match config.release_role {
        ReleaseRole::CandidateB => {
            for (role, expected) in [
                ("scribe-app", (pending.app_bytes, pending.app_sha256)),
                (
                    "scribe-lifecycle",
                    (pending.lifecycle_bytes, pending.lifecycle_sha256),
                ),
            ] {
                if release.component_file_identity(role) != Some(Some(expected)) {
                    return Err(SlotError::Mismatch("pending-identity-not-authenticated"));
                }
            }
            for role in COMPONENT_ROLES {
                if expected_component_path(config, role).is_some()
                    && !matches!(
                        role,
                        "license-noto-emoji-description" | "license-noto-emoji-upstream-info"
                    )
                    && release.component_file_identity(role) == Some(None)
                {
                    return Err(SlotError::Mismatch("candidate-profile-component-absent"));
                }
            }
        }
        ReleaseRole::RetainedA => {
            for role in COMPONENT_ROLES {
                let Some(path) = expected_component_path(config, role) else {
                    continue;
                };
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or(SlotError::Invalid("retained-component-name"))?;
                let expected = match inventory.entries.get(name) {
                    Some(InventoryPresence::File { bytes, sha256 }) => Some((*bytes, *sha256)),
                    Some(InventoryPresence::Absent) => None,
                    None => return Err(SlotError::Invalid("retained-role-not-in-inventory")),
                };
                if release.component_file_identity(role) != Some(expected) {
                    return Err(SlotError::Mismatch("retained-inventory-not-authenticated"));
                }
            }
        }
    }
    Ok(())
}

fn observe_roots(config: &SlotBindingConfig) -> Result<Vec<(PathBuf, fs::Metadata)>, SlotError> {
    [
        (&config.app_root, "application-root"),
        (&config.documents_root, "documents-root"),
        (&config.recovery_root, "recovery-root"),
        (
            &config.app_root.join("previous-release"),
            "retained-release-root",
        ),
    ]
    .into_iter()
    .map(|(path, label)| Ok((path.to_path_buf(), observe_directory(path, label)?)))
    .collect()
}

fn revalidate_roots(roots: &[(PathBuf, fs::Metadata)]) -> Result<(), SlotError> {
    for (path, before) in roots {
        if path_chain_contains_symlink(path) {
            return Err(SlotError::Changed("layout-root-replaced"));
        }
        let after =
            inspect_metadata(path).map_err(|_| SlotError::Changed("layout-root-replaced"))?;
        if !same_identity(before, &after) {
            return Err(SlotError::Changed("layout-root-replaced"));
        }
    }
    Ok(())
}

fn observe_mapped_components(
    config: &SlotBindingConfig,
) -> Result<Vec<(PathBuf, Option<fs::Metadata>)>, SlotError> {
    let mut observed = Vec::with_capacity(config.release_role.mapped_count());
    for role in COMPONENT_ROLES {
        let Some(path) = expected_component_path(config, role) else {
            continue;
        };
        if path_chain_contains_symlink(&path) {
            return Err(SlotError::Unsafe("mapped-component-path"));
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                Some(metadata)
            }
            Ok(_) => return Err(SlotError::Unsafe("mapped-component-path")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if config.release_role == ReleaseRole::CandidateB
                    && !matches!(
                        role,
                        "license-noto-emoji-description" | "license-noto-emoji-upstream-info"
                    )
                {
                    return Err(SlotError::Missing("candidate-component-missing"));
                }
                None
            }
            Err(_) => return Err(SlotError::Io("mapped-component-path")),
        };
        observed.push((path, metadata));
    }
    Ok(observed)
}

fn revalidate_mapped_components(
    observed: &[(PathBuf, Option<fs::Metadata>)],
) -> Result<(), SlotError> {
    for (path, before) in observed {
        if path_chain_contains_symlink(path) {
            return Err(SlotError::Changed("mapped-component-changed"));
        }
        match (before, fs::symlink_metadata(path)) {
            (Some(before), Ok(after)) if same_identity(before, &after) => {}
            (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(SlotError::Changed("mapped-component-changed")),
        }
    }
    Ok(())
}

fn revalidate_file(
    path: &Path,
    stable: &StableBytes,
    label: &'static str,
) -> Result<(), SlotError> {
    if path_chain_contains_symlink(path) {
        return Err(SlotError::Changed(label));
    }
    let after = inspect_metadata(path).map_err(|_| SlotError::Changed(label))?;
    if !same_identity(&stable.metadata, &after) {
        return Err(SlotError::Changed(label));
    }
    Ok(())
}

#[cfg(unix)]
fn same_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    before.file_type() == after.file_type()
        && before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.nlink() == after.nlink()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

#[cfg(not(unix))]
fn same_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.file_type() == after.file_type()
        && before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
}

#[derive(Clone, Copy)]
enum AuthorityKind {
    Production,
    #[cfg(test)]
    Test,
}

pub fn inspect_official(config: &SlotBindingConfig) -> InspectionReport {
    inspect_with(
        config,
        AuthorityKind::Production,
        storage_release_provenance::verify_official,
        || Ok(()),
    )
}

fn inspect_with<F, H>(
    config: &SlotBindingConfig,
    authority: AuthorityKind,
    verifier: F,
    between_observation_and_authentication: H,
) -> InspectionReport
where
    F: FnOnce(&VerificationPaths) -> Result<AuthenticatedReleaseComponentSet, ProvenanceError>,
    H: FnOnce() -> Result<(), SlotError>,
{
    let pin = match official_release_public_key() {
        Ok(_) => "configured",
        Err(ReleaseVerifyError::OfficialKeyNotProvisioned) => "unprovisioned",
        Err(_) => "invalid",
    };
    let mut report = InspectionReport::new(config, pin);

    let observed = (|| {
        validate_config(config)?;
        validate_layout_mapping(config)?;
        let roots = observe_roots(config)?;
        let mapped_components = observe_mapped_components(config)?;
        let pending_path = config.app_root.join("state/update/pending-health.v1");
        let inventory_path = config
            .app_root
            .join("previous-release/ignite-lkg-inventory.v1");
        let pending = read_pending(&pending_path)?;
        report.pending_health = "observed-valid";
        verify_pending_candidate(config, &pending)?;
        report.pending_health = "observed-match";
        report.pending_transaction_binding = "observed-match";

        let inventory = read_inventory(&inventory_path)?;
        let inventory_digest = Sha256::digest(&inventory.stable.bytes);
        if inventory.stable.bytes.len() as u64 != pending.inventory_bytes
            || inventory_digest.as_slice() != pending.inventory_sha256
        {
            return Err(SlotError::Mismatch("wrong-retained-inventory"));
        }
        verify_inventory_files(&config.app_root.join("previous-release"), &inventory)?;
        report.retained_inventory = "observed-match";

        if config.release_role == ReleaseRole::CandidateB {
            verify_candidate_closed_directories(config)?;
        }
        verify_transition_state(config)?;
        report.publication_state = "observed-complete";
        report.installed_layout = "observed-match";

        between_observation_and_authentication()?;

        let authentication = verifier(&config.provenance);
        revalidate_file(&pending_path, &pending.stable, "pending-health-changed")?;
        revalidate_file(
            &inventory_path,
            &inventory.stable,
            "retained-inventory-changed",
        )?;
        revalidate_mapped_components(&mapped_components)?;
        revalidate_roots(&roots)?;

        let authenticated = authentication.map_err(|error| {
            report.provenance_result = error.status();
            match error {
                ProvenanceError::OfficialKeyNotProvisioned
                | ProvenanceError::InvalidPinnedPublicKey => {
                    SlotError::Missing(storage_release_provenance::failure_token(&error))
                }
                ProvenanceError::StateChanged(_) => SlotError::Changed("provenance-input-changed"),
                ProvenanceError::UnsafeInput(_)
                | ProvenanceError::InputTooLarge(_)
                | ProvenanceError::InputRead(_) => SlotError::Unsafe("provenance-input-unsafe"),
                ProvenanceError::Missing(_) | ProvenanceError::MissingSignature => {
                    SlotError::Missing("provenance-input-missing")
                }
                _ => SlotError::Mismatch("provenance-authentication-rejected"),
            }
        })?;
        report.release_version = authenticated.version().into();
        report.release_tag = authenticated.tag().into();
        report.provenance_result = "AUTHENTIC_AND_CONSISTENT";
        report.component_integrity = "matched";
        report.artifact_authenticity = match authority {
            AuthorityKind::Production => "production-authenticated",
            #[cfg(test)]
            AuthorityKind::Test => "test-authenticated",
        };
        verify_authenticated_relationships(config, &authenticated, &pending, &inventory)?;
        Ok::<(), SlotError>(())
    })();

    match observed {
        Ok(()) => {
            report.result = "REVIEW_REQUIRED";
            report.failure = "trusted-installation-publication-unavailable";
            report.exit_code = 2;
        }
        Err(error) => {
            let authority_unavailable = report.provenance_result == "AUTHORITY_UNAVAILABLE";
            report.reject(&error);
            if authority_unavailable {
                report.result = "AUTHORITY_UNAVAILABLE";
                report.exit_code = 3;
            }
        }
    }
    report
}

#[cfg(test)]
pub(crate) fn inspect_with_test_key(
    config: &SlotBindingConfig,
    public_key: &[u8; 32],
) -> InspectionReport {
    inspect_with(
        config,
        AuthorityKind::Test,
        |paths| storage_release_provenance::verify_paths_with_key(paths, public_key),
        || Ok(()),
    )
}

#[cfg(test)]
fn inspect_with_test_key_and_hook<H>(
    config: &SlotBindingConfig,
    public_key: &[u8; 32],
    hook: H,
) -> InspectionReport
where
    H: FnOnce() -> Result<(), SlotError>,
{
    inspect_with(
        config,
        AuthorityKind::Test,
        |paths| storage_release_provenance::verify_paths_with_key(paths, public_key),
        hook,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    const OWNER_FILE: &str = ".ignite-test-root-owner";
    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const TRANSACTION: &str = "20261030T120000Z-7";
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
            Err("no exclusive installed-slot test-root candidate was available".into())
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
            Self::from_candidates(candidates).expect("create exclusive installed-slot test root")
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
        config: SlotBindingConfig,
    }

    impl Fixture {
        fn new(role: ReleaseRole, seed: u8) -> Self {
            let owner = OwnedTestRoot::new("ignite-installed-slot-binding");
            let root = owner.path.clone();
            let app = root.join("app");
            let docs = root.join("documents");
            let recovery = root.join("recovery");
            let evidence = root.join("evidence");
            for directory in [
                app.join("bin"),
                app.join("assets/licenses/noto-emoji"),
                app.join("assets/licenses/source-sans-3"),
                app.join("tools/ignite-release/lib"),
                app.join("state/update"),
                app.join("run"),
                app.join("previous-release"),
                docs.clone(),
                recovery.join("bin"),
                recovery.join("manifest"),
                evidence.clone(),
            ] {
                fs::create_dir_all(directory).unwrap();
            }

            let provisional = SlotBindingConfig {
                release_role: ReleaseRole::CandidateB,
                expected_transaction: TRANSACTION.into(),
                app_root: app.clone(),
                documents_root: docs.clone(),
                recovery_root: recovery.clone(),
                provenance: VerificationPaths {
                    expected_tag: "v1.2.3".into(),
                    record: evidence.join("record"),
                    record_signature: evidence.join("record.sig"),
                    release_manifest: evidence.join("release.manifest"),
                    release_manifest_signature: evidence.join("release.manifest.sig"),
                    capabilities: evidence.join("capabilities"),
                    components: Vec::new(),
                },
            };
            for component_role in COMPONENT_ROLES {
                if let Some(path) = expected_component_path(&provisional, component_role) {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent).unwrap();
                    }
                    fs::write(&path, format!("candidate B {component_role}\n")).unwrap();
                }
            }
            fs::write(
                recovery.join("manifest/version"),
                b"generated recovery version\n",
            )
            .unwrap();

            let retained = app.join("previous-release");
            for name in INVENTORY_NAMES {
                fs::write(retained.join(name), format!("retained A {name}\n")).unwrap();
            }
            let inventory = Self::inventory_bytes(&retained);
            fs::write(retained.join("ignite-lkg-inventory.v1"), &inventory).unwrap();

            let app_bytes = fs::read(app.join("bin/scribe-app")).unwrap();
            let life_bytes = fs::read(app.join("bin/scribe-lifecycle")).unwrap();
            let pending = Self::pending_bytes(&app_bytes, &life_bytes, &inventory, TRANSACTION);
            fs::write(app.join("state/update/pending-health.v1"), pending).unwrap();

            let version = if role == ReleaseRole::CandidateB {
                "1.2.3"
            } else {
                "1.2.2"
            };
            let tag = format!("v{version}");
            let mut config = SlotBindingConfig {
                release_role: role,
                expected_transaction: TRANSACTION.into(),
                app_root: app,
                documents_root: docs,
                recovery_root: recovery,
                provenance: VerificationPaths {
                    expected_tag: tag,
                    record: evidence.join("record"),
                    record_signature: evidence.join("record.sig"),
                    release_manifest: evidence.join("release.manifest"),
                    release_manifest_signature: evidence.join("release.manifest.sig"),
                    capabilities: evidence.join("capabilities"),
                    components: Vec::new(),
                },
            };
            for component_role in COMPONENT_ROLES {
                let path = expected_component_path(&config, component_role)
                    .unwrap_or_else(|| evidence.join(format!("unmapped-{component_role}")));
                if !path.exists() {
                    fs::write(&path, format!("unmapped {role:?} {component_role}\n")).unwrap();
                }
                config.provenance.components.push(ComponentPath {
                    role: component_role.into(),
                    path,
                });
            }
            fs::write(
                &config.provenance.capabilities,
                crate::storage_capabilities::canonical_bytes().unwrap(),
            )
            .unwrap();

            let signing_key = SigningKey::from_bytes(&[seed; 32]);
            let installer = format!("exact CI installer {version}\n").into_bytes();
            let manifest = format!(
                "schema={}\nversion={version}\ntag=v{version}\nartifact=Install Ignite v{version}.sh\nbytes={}\nsha256={}\n",
                crate::release_manifest::MANIFEST_SCHEMA,
                installer.len(),
                encode_hex(&Sha256::digest(&installer)),
            )
            .into_bytes();
            fs::write(&config.provenance.release_manifest, &manifest).unwrap();
            fs::write(
                &config.provenance.release_manifest_signature,
                Self::release_signature(&signing_key, &manifest),
            )
            .unwrap();

            let fixture = Self {
                root,
                _owner: owner,
                signing_key,
                config,
            };
            fixture.resign_record_with_installer(&installer);
            fixture
        }

        fn public_key(&self) -> [u8; 32] {
            self.signing_key.verifying_key().to_bytes()
        }

        fn inspect(&self) -> InspectionReport {
            inspect_with_test_key(&self.config, &self.public_key())
        }

        fn component(&self, role: &str) -> PathBuf {
            self.config
                .provenance
                .components
                .iter()
                .find(|component| component.role == role)
                .unwrap()
                .path
                .clone()
        }

        fn inventory_bytes(root: &Path) -> Vec<u8> {
            let mut output = String::from(
                "IGNITE_LKG_INVENTORY_V1\nschema=1\nkind=ignite-application-release\nentry_count=18\n",
            );
            for name in INVENTORY_NAMES {
                let bytes = fs::read(root.join(name)).unwrap();
                writeln!(
                    output,
                    "entry={name}|file|{}|{}",
                    bytes.len(),
                    encode_hex(&Sha256::digest(&bytes))
                )
                .unwrap();
            }
            output.into_bytes()
        }

        fn pending_bytes(
            app: &[u8],
            lifecycle: &[u8],
            inventory: &[u8],
            transaction: &str,
        ) -> Vec<u8> {
            let inventory_sha = encode_hex(&Sha256::digest(inventory));
            format!(
                "IGNITE_PENDING_HEALTH_V1\nschema=1\nstatus=pending-health\ncandidate_version=1.2.3\ncandidate_txn={transaction}\ncandidate_app_bytes={}\ncandidate_app_sha256={}\ncandidate_lifecycle_bytes={}\ncandidate_lifecycle_sha256={}\nretained_version=unknown\nretained_release_identity=sha256:{inventory_sha}\nlkg_inventory_bytes={}\nlkg_inventory_sha256={inventory_sha}\ndata_compatibility=unknown\n",
                app.len(),
                encode_hex(&Sha256::digest(app)),
                lifecycle.len(),
                encode_hex(&Sha256::digest(lifecycle)),
                inventory.len(),
            )
            .into_bytes()
        }

        fn release_signature(key: &SigningKey, message: &[u8]) -> Vec<u8> {
            format!(
                "schema={}\nsignature={}\n",
                crate::release_manifest::SIGNATURE_SCHEMA,
                encode_hex(&key.sign(message).to_bytes())
            )
            .into_bytes()
        }

        fn provenance_signature(key: &SigningKey, message: &[u8]) -> Vec<u8> {
            format!(
                "schema={}\nsignature={}\n",
                storage_release_provenance::SIGNATURE_SCHEMA,
                encode_hex(&key.sign(message).to_bytes())
            )
            .into_bytes()
        }

        fn resign_record_with_installer(&self, installer: &[u8]) {
            let manifest = fs::read(&self.config.provenance.release_manifest).unwrap();
            let capabilities = fs::read(&self.config.provenance.capabilities).unwrap();
            let public_key = self.public_key();
            let version = &self.config.provenance.expected_tag[1..];
            let mut record = String::new();
            writeln!(record, "{}", storage_release_provenance::RECORD_HEADER).unwrap();
            writeln!(
                record,
                "schema={}",
                storage_release_provenance::RECORD_SCHEMA
            )
            .unwrap();
            writeln!(
                record,
                "key_id=ed25519-sha256:{}",
                encode_hex(&Sha256::digest(public_key))
            )
            .unwrap();
            writeln!(record, "platform=kindle").unwrap();
            writeln!(record, "architecture=armv7-unknown-linux-gnueabihf").unwrap();
            writeln!(record, "release_version={version}").unwrap();
            writeln!(record, "release_tag=v{version}").unwrap();
            writeln!(record, "source_commit={COMMIT}").unwrap();
            writeln!(record, "ci_workflow_run_id=987654").unwrap();
            writeln!(record, "installer_artifact=Install Ignite v{version}.sh").unwrap();
            writeln!(record, "installer_bytes={}", installer.len()).unwrap();
            writeln!(
                record,
                "installer_sha256={}",
                encode_hex(&Sha256::digest(installer))
            )
            .unwrap();
            writeln!(record, "release_manifest_bytes={}", manifest.len()).unwrap();
            writeln!(
                record,
                "release_manifest_sha256={}",
                encode_hex(&Sha256::digest(&manifest))
            )
            .unwrap();
            writeln!(record, "capabilities_bytes={}", capabilities.len()).unwrap();
            writeln!(
                record,
                "capabilities_sha256={}",
                encode_hex(&Sha256::digest(&capabilities))
            )
            .unwrap();
            writeln!(record, "component_count={}", COMPONENT_ROLES.len()).unwrap();
            for role in COMPONENT_ROLES {
                let bytes = fs::read(self.component(role)).unwrap();
                writeln!(
                    record,
                    "component={role}|file|{}|{}",
                    bytes.len(),
                    encode_hex(&Sha256::digest(&bytes))
                )
                .unwrap();
            }
            writeln!(record, "data_compatibility=unknown").unwrap();
            writeln!(record, "promotion_authorized=false").unwrap();
            let record = record.into_bytes();
            fs::write(&self.config.provenance.record, &record).unwrap();
            fs::write(
                &self.config.provenance.record_signature,
                Self::provenance_signature(&self.signing_key, &record),
            )
            .unwrap();
        }

        fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
            fn collect(root: &Path, current: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
                for entry in fs::read_dir(current).unwrap() {
                    let entry = entry.unwrap();
                    let path = entry.path();
                    let metadata = fs::symlink_metadata(&path).unwrap();
                    if metadata.is_dir() {
                        collect(root, &path, output);
                    } else if metadata.is_file() {
                        output.insert(
                            path.strip_prefix(root).unwrap().to_path_buf(),
                            fs::read(path).unwrap(),
                        );
                    }
                }
            }
            let mut output = BTreeMap::new();
            collect(&self.root, &self.root, &mut output);
            output
        }

        fn args(&self) -> Vec<String> {
            let mut args = vec![
                "--inspect-installed-slot-binding".into(),
                "--release-role".into(),
                self.config.release_role.as_str().replace('_', "-"),
                "--expected-transaction".into(),
                self.config.expected_transaction.clone(),
                "--app-root".into(),
                self.config.app_root.display().to_string(),
                "--documents-root".into(),
                self.config.documents_root.display().to_string(),
                "--recovery-root".into(),
                self.config.recovery_root.display().to_string(),
                "--expected-tag".into(),
                self.config.provenance.expected_tag.clone(),
                "--record".into(),
                self.config.provenance.record.display().to_string(),
                "--record-signature".into(),
                self.config
                    .provenance
                    .record_signature
                    .display()
                    .to_string(),
                "--release-manifest".into(),
                self.config
                    .provenance
                    .release_manifest
                    .display()
                    .to_string(),
                "--release-manifest-signature".into(),
                self.config
                    .provenance
                    .release_manifest_signature
                    .display()
                    .to_string(),
                "--capabilities".into(),
                self.config.provenance.capabilities.display().to_string(),
            ];
            for component in &self.config.provenance.components {
                args.push("--component".into());
                args.push(format!("{}={}", component.role, component.path.display()));
            }
            args
        }
    }

    #[test]
    fn complete_candidate_and_retained_observations_remain_non_authorizing() {
        for (role, seed, mapped) in [
            (ReleaseRole::CandidateB, 11, 20),
            (ReleaseRole::RetainedA, 12, 17),
        ] {
            let fixture = Fixture::new(role, seed);
            let before = fixture.snapshot();
            let report = fixture.inspect();
            assert_eq!(report.result, "REVIEW_REQUIRED");
            assert_eq!(report.exit_code(), 2);
            assert_eq!(report.artifact_authenticity, "test-authenticated");
            assert_eq!(report.component_integrity, "matched");
            assert_eq!(report.installed_layout, "observed-match");
            assert_eq!(role.mapped_count(), mapped);
            let text = String::from_utf8(report.canonical_bytes()).unwrap();
            assert!(text.contains("installation_binding=unverified\n"));
            assert!(text.contains("execution_binding=unverified\n"));
            assert!(text.contains("pending_transaction_binding=observed-match\n"));
            assert!(text.contains("first_launch_admission=unavailable\n"));
            assert!(text.contains("data_compatibility=unknown\n"));
            assert!(text.contains("promotion_authorized=false\n"));
            assert_eq!(before, fixture.snapshot());
        }
    }

    #[test]
    fn missing_modified_same_size_wrong_release_and_mixed_components_fail() {
        let fixture = Fixture::new(ReleaseRole::CandidateB, 13);
        fs::remove_file(fixture.component("scribe-lifecycle-host")).unwrap();
        assert_ne!(fixture.inspect().result, "REVIEW_REQUIRED");

        let fixture = Fixture::new(ReleaseRole::CandidateB, 14);
        let icon = fixture.component("icon");
        let original = fs::read(&icon).unwrap();
        fs::write(&icon, vec![b'x'; original.len()]).unwrap();
        assert_eq!(fixture.inspect().result, "INCONSISTENT");

        let fixture = Fixture::new(ReleaseRole::CandidateB, 15);
        let helper = fixture.component("backup-runtime");
        fs::write(&helper, b"component from release A\n").unwrap();
        assert_eq!(fixture.inspect().result, "INCONSISTENT");

        let fixture = Fixture::new(ReleaseRole::RetainedA, 16);
        let app = fixture.component("scribe-app");
        fs::write(&app, b"candidate B scribe-app\n").unwrap();
        assert_eq!(fixture.inspect().result, "INCONSISTENT");
    }

    #[test]
    fn extra_duplicate_wrong_transaction_and_wrong_inventory_fail_closed() {
        let fixture = Fixture::new(ReleaseRole::CandidateB, 17);
        fs::write(fixture.config.app_root.join("bin/unregistered"), b"extra").unwrap();
        assert_eq!(fixture.inspect().failure, "extra-unregistered-component");

        let fixture = Fixture::new(ReleaseRole::CandidateB, 18);
        let mut duplicate = fixture.config.clone();
        duplicate.provenance.components[1].role = duplicate.provenance.components[0].role.clone();
        assert_eq!(
            inspect_with_test_key(&duplicate, &fixture.public_key()).failure,
            "component-map-ambiguous"
        );

        let fixture = Fixture::new(ReleaseRole::CandidateB, 19);
        let mut wrong = fixture.config.clone();
        wrong.expected_transaction = "20261030T120000Z-8".into();
        assert_eq!(
            inspect_with_test_key(&wrong, &fixture.public_key()).failure,
            "wrong-pending-transaction"
        );

        let fixture = Fixture::new(ReleaseRole::CandidateB, 20);
        let inventory = fixture
            .config
            .app_root
            .join("previous-release/ignite-lkg-inventory.v1");
        let mut bytes = fs::read(&inventory).unwrap();
        bytes[0] ^= 1;
        fs::write(inventory, bytes).unwrap();
        assert_ne!(fixture.inspect().result, "REVIEW_REQUIRED");
    }

    #[test]
    fn signed_untrusted_paths_and_forged_local_metadata_never_create_binding() {
        let fixture = Fixture::new(ReleaseRole::CandidateB, 21);
        let external = fixture.root.join("evidence/untrusted-app");
        fs::copy(fixture.component("scribe-app"), &external).unwrap();
        let mut config = fixture.config.clone();
        config
            .provenance
            .components
            .iter_mut()
            .find(|component| component.role == "scribe-app")
            .unwrap()
            .path = external;
        assert_eq!(
            inspect_with_test_key(&config, &fixture.public_key()).failure,
            "component-path-outside-installed-layout"
        );

        let fixture = Fixture::new(ReleaseRole::CandidateB, 22);
        let report = fixture.inspect();
        assert_eq!(report.pending_transaction_binding, "observed-match");
        let text = String::from_utf8(report.canonical_bytes()).unwrap();
        assert!(!text.contains("installation_binding=verified"));
        assert!(!text.contains("execution_binding=verified"));
    }

    #[test]
    fn official_authority_refusal_is_nonzero_and_report_only() {
        let fixture = Fixture::new(ReleaseRole::CandidateB, 23);
        let report = inspect_official(&fixture.config);
        assert_ne!(report.exit_code(), 0);
        let text = String::from_utf8(report.canonical_bytes()).unwrap();
        if official_release_public_key().is_err() {
            assert!(text.contains("result=AUTHORITY_UNAVAILABLE\n"));
            assert!(text.contains("production_trust_pin=unprovisioned\n"));
        }
        assert!(text.contains("promotion_authorized=false\n"));
    }

    #[test]
    fn reports_are_deterministic_bounded_and_interruption_is_incomplete() {
        let fixture = Fixture::new(ReleaseRole::CandidateB, 24);
        let first = fixture.inspect().canonical_bytes();
        let second = fixture.inspect().canonical_bytes();
        assert_eq!(first, second);
        assert!(first.len() < 4096);

        let interrupted =
            inspect_with_test_key_and_hook(&fixture.config, &fixture.public_key(), || {
                Err(SlotError::Interrupted)
            });
        assert_eq!(interrupted.result, "INCOMPLETE");
        assert_eq!(interrupted.failure, "inspection-interrupted");

        let pending = fixture
            .config
            .app_root
            .join("state/update/pending-health.v1");
        fs::write(&pending, vec![b'x'; MAX_PENDING_BYTES as usize + 1]).unwrap();
        assert_eq!(fixture.inspect().failure, "pending-health-input");
    }

    #[test]
    fn command_parser_requires_complete_absolute_fixed_layout_input() {
        let fixture = Fixture::new(ReleaseRole::CandidateB, 25);
        assert!(matches!(
            parse_command(&fixture.args()).unwrap(),
            InspectionCommand::Inspect(_)
        ));
        let mut relative = fixture.args();
        let index = relative
            .iter()
            .position(|value| value == "--app-root")
            .unwrap()
            + 1;
        relative[index] = "relative".into();
        assert!(parse_command(&relative).is_err());
    }

    #[test]
    fn fixture_cleanup_preserves_collision_and_substituted_root() {
        let parent = OwnedTestRoot::new("ignite-slot-owner-parent");
        let collision = parent.path.join("collision");
        let candidate = parent.path.join("candidate");
        fs::create_dir(&collision).unwrap();
        let collision_sentinel = collision.join("sentinel");
        fs::write(&collision_sentinel, b"preserve collision").unwrap();
        let owned = OwnedTestRoot::from_candidates([collision.clone(), candidate.clone()]).unwrap();
        let displaced = parent.path.join("displaced");
        fs::rename(&candidate, &displaced).unwrap();
        fs::create_dir(&candidate).unwrap();
        let replacement = candidate.join("replacement");
        fs::write(&replacement, b"preserve replacement").unwrap();
        drop(owned);
        assert_eq!(fs::read(collision_sentinel).unwrap(), b"preserve collision");
        assert_eq!(fs::read(replacement).unwrap(), b"preserve replacement");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_hardlink_special_file_and_parent_substitution_are_refused() {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        let fixture = Fixture::new(ReleaseRole::CandidateB, 26);
        let app = fixture.component("scribe-app");
        let target = fixture.root.join("evidence/symlink-target");
        fs::copy(&app, &target).unwrap();
        fs::remove_file(&app).unwrap();
        symlink(&target, &app).unwrap();
        assert_eq!(fixture.inspect().result, "UNSAFE_INPUT");

        let fixture = Fixture::new(ReleaseRole::CandidateB, 27);
        let bin = fixture.config.app_root.join("bin");
        let real_bin = fixture.config.app_root.join("bin-real");
        fs::rename(&bin, &real_bin).unwrap();
        symlink(&real_bin, &bin).unwrap();
        assert_eq!(fixture.inspect().result, "UNSAFE_INPUT");

        let fixture = Fixture::new(ReleaseRole::CandidateB, 28);
        let app = fixture.component("scribe-app");
        let alias = fixture.root.join("evidence/hard-link");
        fs::hard_link(&app, alias).unwrap();
        assert_eq!(fixture.inspect().result, "UNSAFE_INPUT");

        let fixture = Fixture::new(ReleaseRole::CandidateB, 29);
        let helper = fixture.component("backup-runtime");
        fs::remove_file(&helper).unwrap();
        let _listener = UnixListener::bind(&helper).unwrap();
        assert_eq!(fixture.inspect().result, "UNSAFE_INPUT");

        let fixture = Fixture::new(ReleaseRole::CandidateB, 30);
        let original = fixture.config.app_root.clone();
        let displaced = fixture.root.join("app-displaced");
        let replacement = fixture.root.join("app-replacement");
        copy_directory(&original, &replacement);
        let report = inspect_with_test_key_and_hook(&fixture.config, &fixture.public_key(), || {
            fs::rename(&original, &displaced).unwrap();
            fs::rename(&replacement, &original).unwrap();
            Ok(())
        });
        assert_eq!(report.result, "STATE_CHANGED");
    }

    #[cfg(unix)]
    fn copy_directory(source: &Path, destination: &Path) {
        fs::create_dir(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_directory(&source_path, &destination_path);
            } else {
                fs::copy(source_path, destination_path).unwrap();
            }
        }
    }

    #[test]
    fn replacement_between_phases_is_detected() {
        let fixture = Fixture::new(ReleaseRole::CandidateB, 31);
        let pending = fixture
            .config
            .app_root
            .join("state/update/pending-health.v1");
        let bytes = fs::read(&pending).unwrap();
        let replacement = fixture.root.join("evidence/pending-replacement");
        fs::write(&replacement, bytes).unwrap();
        let report = inspect_with_test_key_and_hook(&fixture.config, &fixture.public_key(), || {
            fs::remove_file(&pending).unwrap();
            fs::rename(&replacement, &pending).unwrap();
            Ok(())
        });
        assert_eq!(report.result, "STATE_CHANGED");
        assert_eq!(report.failure, "pending-health-changed");
    }

    #[test]
    fn mapped_component_replacement_between_phases_is_detected() {
        let fixture = Fixture::new(ReleaseRole::CandidateB, 32);
        let component = fixture.component("backup-runtime");
        let replacement = fixture.root.join("evidence/component-replacement");
        fs::write(&replacement, fs::read(&component).unwrap()).unwrap();
        let report = inspect_with_test_key_and_hook(&fixture.config, &fixture.public_key(), || {
            fs::remove_file(&component).unwrap();
            fs::rename(&replacement, &component).unwrap();
            Ok(())
        });
        assert_eq!(report.result, "STATE_CHANGED");
        assert_eq!(report.failure, "mapped-component-changed");
    }
}
