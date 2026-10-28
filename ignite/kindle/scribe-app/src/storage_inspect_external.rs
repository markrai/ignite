//! Strictly read-only inspection of external sync, device, and replica state.
//!
//! This module deliberately does not call `OutboundSync::open`, adoption recovery, pairing,
//! replica/identity `ensure_*`, confirmed-log append open, or any other normal startup path.
//! Those paths can create, repair, truncate, compact, rename, download, or start workers.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use control_protocol::ServerId;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sync_engine::{HistoryV1, LibraryState};
use sync_protocol::{
    sha256_hex, BackgroundRef, IdentityFileV1, LibraryId, OperationId, ReplicaId, ReplicaStateV1,
    Sha256Hex, SnapshotNotebook, SnapshotPage, SyncEnvelopeV1, SyncOperationV1, SyncSnapshotV1,
};

use crate::storage_inspect_library::{
    hash_file, inspect_metadata, join_rel, open_regular_read_only, parse_json_strict,
    path_chain_contains_symlink, read_bounded, safe_component, safe_report_field,
    MAX_METADATA_BYTES, MAX_TOTAL_BYTES_PER_PASS, MAX_TREE_DEPTH, MAX_TREE_ENTRIES,
};

pub const REPORT_SCHEMA: u32 = 1;
pub const REPORT_MAX_BYTES: usize = 512 * 1024;
pub const MAX_FINDINGS: usize = 512;
pub const MAX_RECORDS: usize = 200_000;
pub const MAX_RECORD_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_RETAINED_RECORD_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_JOURNAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_BLOB_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExternalDiagnosticClass {
    Valid,
    ValidPending,
    UnsupportedFormat,
    Corrupt,
    MissingRequired,
    IncompleteTransaction,
    Unverified,
    InspectionError,
}

impl ExternalDiagnosticClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "VALID",
            Self::ValidPending => "VALID_PENDING",
            Self::UnsupportedFormat => "UNSUPPORTED_FORMAT",
            Self::Corrupt => "CORRUPT",
            Self::MissingRequired => "MISSING_REQUIRED",
            Self::IncompleteTransaction => "INCOMPLETE_TRANSACTION",
            Self::Unverified => "UNVERIFIED",
            Self::InspectionError => "INSPECTION_ERROR",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Valid => 0,
            Self::ValidPending => 1,
            Self::UnsupportedFormat => 2,
            Self::Corrupt => 3,
            Self::MissingRequired => 4,
            Self::IncompleteTransaction => 5,
            Self::Unverified => 6,
            Self::InspectionError => 7,
        }
    }
}

const ALL_CLASSES: [ExternalDiagnosticClass; 8] = [
    ExternalDiagnosticClass::Valid,
    ExternalDiagnosticClass::ValidPending,
    ExternalDiagnosticClass::UnsupportedFormat,
    ExternalDiagnosticClass::Corrupt,
    ExternalDiagnosticClass::MissingRequired,
    ExternalDiagnosticClass::IncompleteTransaction,
    ExternalDiagnosticClass::Unverified,
    ExternalDiagnosticClass::InspectionError,
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExternalFinding {
    pub class: ExternalDiagnosticClass,
    pub relative_path: String,
    pub reason: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExternalInspectionCounts {
    pub namespaces: u64,
    pub files: u64,
    pub bytes_examined: u64,
    pub outbound_operations: u64,
    pub inbound_operations: u64,
    pub accepted_operations: u64,
    pub confirmed_operations: u64,
    pub blobs: u64,
    pub retired_epochs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalInspectionReport {
    pub completed: bool,
    pub counts: ExternalInspectionCounts,
    diagnostic_counts: [u64; 8],
    findings: Vec<ExternalFinding>,
    findings_overflowed: bool,
}

impl Default for ExternalInspectionReport {
    fn default() -> Self {
        Self {
            completed: true,
            counts: ExternalInspectionCounts::default(),
            diagnostic_counts: [0; 8],
            findings: Vec::new(),
            findings_overflowed: false,
        }
    }
}

impl ExternalInspectionReport {
    pub fn diagnostic_count(&self, class: ExternalDiagnosticClass) -> u64 {
        self.diagnostic_counts[class.index()]
    }

    pub fn findings(&self) -> &[ExternalFinding] {
        &self.findings
    }

    pub fn result_token(&self) -> &'static str {
        if !self.completed {
            "INSPECTION_INCOMPLETE"
        } else if ALL_CLASSES[2..]
            .iter()
            .any(|class| self.diagnostic_count(*class) != 0)
        {
            "ISSUES_DETECTED"
        } else if self.diagnostic_count(ExternalDiagnosticClass::ValidPending) != 0 {
            "VALID_PENDING"
        } else {
            "VALID"
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self.result_token() {
            "VALID" | "VALID_PENDING" => 0,
            "ISSUES_DETECTED" => 2,
            _ => 3,
        }
    }

    fn finding(
        &mut self,
        class: ExternalDiagnosticClass,
        relative_path: impl Into<String>,
        reason: &'static str,
    ) {
        self.diagnostic_counts[class.index()] =
            self.diagnostic_counts[class.index()].saturating_add(1);
        if class == ExternalDiagnosticClass::InspectionError {
            self.completed = false;
        }
        if self.findings.len() < MAX_FINDINGS {
            self.findings.push(ExternalFinding {
                class,
                relative_path: relative_path.into(),
                reason,
            });
        } else {
            self.findings_overflowed = true;
            self.completed = false;
        }
    }

    fn observe(&mut self, class: ExternalDiagnosticClass) {
        self.diagnostic_counts[class.index()] =
            self.diagnostic_counts[class.index()].saturating_add(1);
    }

    fn finish(&mut self) {
        if self.findings_overflowed {
            self.findings.push(ExternalFinding {
                class: ExternalDiagnosticClass::InspectionError,
                relative_path: ".".into(),
                reason: "finding_limit_exceeded",
            });
        }
        self.findings.sort();
        self.findings.dedup();
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, String> {
        let mut out = String::new();
        out.push_str("IGNITE_STORAGE_EXTERNAL_INSPECTION_V1\n");
        out.push_str("schema=1\n");
        out.push_str("scope=external-sync-and-device\n");
        out.push_str("coverage=partial\n");
        out.push_str("writer_exclusion=false\n");
        out.push_str("atomic_snapshot=false\n");
        out.push_str("data_compatibility=unknown\n");
        out.push_str("promotion_authorized=false\n");
        out.push_str("inspection_action=report-only\n");
        out.push_str(&format!("completed={}\n", self.completed));
        out.push_str(&format!("result={}\n", self.result_token()));
        out.push_str(&format!("namespaces={}\n", self.counts.namespaces));
        out.push_str(&format!("files={}\n", self.counts.files));
        out.push_str(&format!("bytes_examined={}\n", self.counts.bytes_examined));
        out.push_str(&format!(
            "outbound_operations={}\n",
            self.counts.outbound_operations
        ));
        out.push_str(&format!(
            "inbound_operations={}\n",
            self.counts.inbound_operations
        ));
        out.push_str(&format!(
            "accepted_operations={}\n",
            self.counts.accepted_operations
        ));
        out.push_str(&format!(
            "confirmed_operations={}\n",
            self.counts.confirmed_operations
        ));
        out.push_str(&format!("blobs={}\n", self.counts.blobs));
        out.push_str(&format!("retired_epochs={}\n", self.counts.retired_epochs));
        for class in ALL_CLASSES {
            out.push_str(&format!(
                "diagnostic_{}={}\n",
                class.as_str(),
                self.diagnostic_count(class)
            ));
        }
        out.push_str(&format!("finding_count={}\n", self.findings.len()));
        for finding in &self.findings {
            if !safe_report_field(&finding.relative_path) || !safe_report_field(finding.reason) {
                return Err("unsafe external inspection report field".into());
            }
            out.push_str(&format!(
                "finding={}|{}|{}\n",
                finding.class.as_str(),
                finding.relative_path,
                finding.reason
            ));
        }
        if out.len() > REPORT_MAX_BYTES {
            return Err(format!(
                "inspection report exceeds {REPORT_MAX_BYTES} bytes"
            ));
        }
        Ok(out.into_bytes())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalInspectionConfig {
    pub library_root: PathBuf,
    pub sync_root: PathBuf,
    pub device_root: PathBuf,
    pub state_root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectionCommand {
    NotRequested,
    Inspect(ExternalInspectionConfig),
}

pub fn parse_command(args: &[String]) -> Result<InspectionCommand, String> {
    let requested = args
        .iter()
        .filter(|arg| arg.as_str() == "--inspect-storage-external")
        .count();
    if requested == 0 {
        return Ok(InspectionCommand::NotRequested);
    }
    let expected = [
        "--inspect-storage-external",
        "--data-dir",
        "",
        "--sync-dir",
        "",
        "--device-dir",
        "",
        "--state-dir",
        "",
    ];
    if requested != 1
        || args.len() != expected.len()
        || expected
            .iter()
            .enumerate()
            .any(|(index, expected)| !expected.is_empty() && args[index].as_str() != *expected)
        || [2usize, 4, 6, 8]
            .iter()
            .any(|index| args[*index].is_empty())
    {
        return Err("--inspect-storage-external must be used exactly as: --inspect-storage-external --data-dir LIBRARY --sync-dir SYNC --device-dir DEVICE --state-dir STATE".into());
    }
    Ok(InspectionCommand::Inspect(ExternalInspectionConfig {
        library_root: PathBuf::from(&args[2]),
        sync_root: PathBuf::from(&args[4]),
        device_root: PathBuf::from(&args[6]),
        state_root: PathBuf::from(&args[8]),
    }))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CensusEntry {
    kind: &'static str,
    size: u64,
    sha256: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ScopeCensus {
    entries: BTreeMap<String, CensusEntry>,
    complete: bool,
}

#[derive(Clone, Debug)]
struct ScopePath {
    label: String,
    path: PathBuf,
}

fn sibling_names(root: &Path, kind: &str) -> Result<Vec<ScopePath>, &'static str> {
    let parent = root.parent().ok_or("scope_root_has_no_parent")?;
    if path_chain_contains_symlink(parent) {
        return Err("scope_parent_path_contains_symlink");
    }
    let base = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("scope_root_name_not_utf8")?;
    let mut found = Vec::new();
    let read_dir = match fs::read_dir(parent) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(_) => return Err("scope_parent_unreadable"),
    };
    for entry in read_dir {
        let entry = entry.map_err(|_| "scope_parent_unreadable")?;
        let Some(name) = safe_component(&entry.file_name()) else {
            continue;
        };
        let suffix = name.strip_prefix(base);
        let relevant = match kind {
            "library" => {
                suffix == Some(".adopt") || suffix.is_some_and(|s| s.starts_with(".retired-"))
            }
            "sync" => {
                matches!(suffix, Some(".adopt") | Some(".adopt.lock"))
                    || suffix.is_some_and(|s| s.starts_with(".retired-"))
            }
            _ => false,
        };
        if relevant {
            found.push(ScopePath {
                label: format!("{kind}_sibling/{name}"),
                path: entry.path(),
            });
        }
    }
    found.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(found)
}

fn scope_paths(config: &ExternalInspectionConfig) -> Result<Vec<ScopePath>, &'static str> {
    let mut paths = vec![
        ScopePath {
            label: "library".into(),
            path: config.library_root.clone(),
        },
        ScopePath {
            label: "sync".into(),
            path: config.sync_root.clone(),
        },
        ScopePath {
            label: "device".into(),
            path: config.device_root.clone(),
        },
        ScopePath {
            label: "state".into(),
            path: config.state_root.clone(),
        },
    ];
    paths.extend(sibling_names(&config.library_root, "library")?);
    paths.extend(sibling_names(&config.sync_root, "sync")?);
    paths.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(paths)
}

fn collect_scope(config: &ExternalInspectionConfig) -> (ScopeCensus, Vec<ExternalFinding>) {
    let mut census = ScopeCensus {
        entries: BTreeMap::new(),
        complete: true,
    };
    let mut issues = Vec::new();
    let paths = match scope_paths(config) {
        Ok(paths) => paths,
        Err(reason) => {
            census.complete = false;
            issues.push(ExternalFinding {
                class: ExternalDiagnosticClass::InspectionError,
                relative_path: ".".into(),
                reason,
            });
            return (census, issues);
        }
    };
    let mut entries = 0usize;
    let mut bytes = 0u64;
    for scope in paths {
        collect_scope_path(
            &scope.path,
            &scope.label,
            0,
            &mut census,
            &mut issues,
            &mut entries,
            &mut bytes,
        );
    }
    (census, issues)
}

fn collect_scope_path(
    path: &Path,
    relative: &str,
    depth: usize,
    census: &mut ScopeCensus,
    issues: &mut Vec<ExternalFinding>,
    total_entries: &mut usize,
    total_bytes: &mut u64,
) {
    if depth > MAX_TREE_DEPTH {
        census.complete = false;
        issues.push(ExternalFinding {
            class: ExternalDiagnosticClass::InspectionError,
            relative_path: relative.into(),
            reason: "tree_depth_limit_exceeded",
        });
        return;
    }
    if path_chain_contains_symlink(path) {
        census.complete = false;
        issues.push(ExternalFinding {
            class: ExternalDiagnosticClass::InspectionError,
            relative_path: relative.into(),
            reason: "path_contains_symlink",
        });
        return;
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            census.entries.insert(
                relative.into(),
                CensusEntry {
                    kind: "missing",
                    size: 0,
                    sha256: "-".into(),
                },
            );
            return;
        }
        Err(_) => {
            census.complete = false;
            issues.push(ExternalFinding {
                class: ExternalDiagnosticClass::InspectionError,
                relative_path: relative.into(),
                reason: "metadata_unavailable",
            });
            return;
        }
    };
    if metadata.file_type().is_symlink() {
        census.complete = false;
        issues.push(ExternalFinding {
            class: ExternalDiagnosticClass::InspectionError,
            relative_path: relative.into(),
            reason: "symlink_refused",
        });
        return;
    }
    *total_entries = total_entries.saturating_add(1);
    if *total_entries > MAX_TREE_ENTRIES {
        census.complete = false;
        issues.push(ExternalFinding {
            class: ExternalDiagnosticClass::InspectionError,
            relative_path: relative.into(),
            reason: "tree_entry_limit_exceeded",
        });
        return;
    }
    if metadata.is_file() {
        *total_bytes = total_bytes.saturating_add(metadata.len());
        if *total_bytes > MAX_TOTAL_BYTES_PER_PASS {
            census.complete = false;
            issues.push(ExternalFinding {
                class: ExternalDiagnosticClass::InspectionError,
                relative_path: relative.into(),
                reason: "tree_byte_limit_exceeded",
            });
            return;
        }
        match hash_file(path, &metadata) {
            Ok(hash) => {
                census.entries.insert(
                    relative.into(),
                    CensusEntry {
                        kind: "file",
                        size: metadata.len(),
                        sha256: hash,
                    },
                );
            }
            Err(reason) => {
                census.complete = false;
                issues.push(ExternalFinding {
                    class: ExternalDiagnosticClass::InspectionError,
                    relative_path: relative.into(),
                    reason,
                });
            }
        }
        return;
    }
    if !metadata.is_dir() {
        census.complete = false;
        issues.push(ExternalFinding {
            class: ExternalDiagnosticClass::InspectionError,
            relative_path: relative.into(),
            reason: "unsupported_special_file",
        });
        return;
    }
    census.entries.insert(
        relative.into(),
        CensusEntry {
            kind: "directory",
            size: 0,
            sha256: "-".into(),
        },
    );
    let read_dir = match fs::read_dir(path) {
        Ok(read_dir) => read_dir,
        Err(_) => {
            census.complete = false;
            issues.push(ExternalFinding {
                class: ExternalDiagnosticClass::InspectionError,
                relative_path: relative.into(),
                reason: "directory_unreadable",
            });
            return;
        }
    };
    let mut children = Vec::new();
    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                census.complete = false;
                issues.push(ExternalFinding {
                    class: ExternalDiagnosticClass::InspectionError,
                    relative_path: relative.into(),
                    reason: "directory_entry_unreadable",
                });
                continue;
            }
        };
        let Some(name) = safe_component(&entry.file_name()) else {
            census.complete = false;
            issues.push(ExternalFinding {
                class: ExternalDiagnosticClass::InspectionError,
                relative_path: relative.into(),
                reason: "unsafe_entry_name",
            });
            continue;
        };
        children.push((name, entry.path()));
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, child) in children {
        let Some(child_relative) = join_rel(relative, &name) else {
            census.complete = false;
            issues.push(ExternalFinding {
                class: ExternalDiagnosticClass::InspectionError,
                relative_path: relative.into(),
                reason: "relative_path_limit_exceeded",
            });
            continue;
        };
        collect_scope_path(
            &child,
            &child_relative,
            depth + 1,
            census,
            issues,
            total_entries,
            total_bytes,
        );
    }
}

fn record_census_issues(report: &mut ExternalInspectionReport, issues: Vec<ExternalFinding>) {
    for issue in issues {
        report.finding(issue.class, issue.relative_path, issue.reason);
    }
}

#[derive(Debug)]
enum JsonRead<T> {
    Missing,
    Parsed(T),
    Invalid,
}

/// All callers use either a source DTO carrying `deny_unknown_fields` recursively or a local
/// inspection DTO with the same rule. The one tolerant production DTO used here
/// (`PersistedLibraryManifestV1`) is handled separately by `validate_library_root` with an
/// explicit field allow-list.
fn read_json_typed<T: DeserializeOwned>(
    path: &Path,
    relative: &str,
    max_bytes: u64,
    supported_versions: &[u64],
    report: &mut ExternalInspectionReport,
) -> JsonRead<T> {
    let bytes = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return JsonRead::Missing,
        Err(_) => {
            report.finding(
                ExternalDiagnosticClass::InspectionError,
                relative,
                "metadata_unavailable",
            );
            return JsonRead::Invalid;
        }
        Ok(_) => match read_bounded(path, max_bytes) {
            Ok(bytes) => bytes,
            Err(reason) => {
                report.finding(ExternalDiagnosticClass::InspectionError, relative, reason);
                return JsonRead::Invalid;
            }
        },
    };
    let value = match parse_json_strict(&bytes) {
        Ok(value) => value,
        Err(error) => {
            report.finding(
                ExternalDiagnosticClass::Corrupt,
                relative,
                crate::storage_inspect_library::json_reason(error),
            );
            return JsonRead::Invalid;
        }
    };
    let Some(version) = value.get("v").and_then(Value::as_u64) else {
        report.finding(
            ExternalDiagnosticClass::Corrupt,
            relative,
            "missing_or_invalid_version",
        );
        return JsonRead::Invalid;
    };
    if !supported_versions.contains(&version) {
        report.finding(
            ExternalDiagnosticClass::UnsupportedFormat,
            relative,
            "unsupported_record_version",
        );
        return JsonRead::Invalid;
    }
    match serde_json::from_value::<T>(value) {
        Ok(parsed) => JsonRead::Parsed(parsed),
        Err(error) => {
            let class = if error.to_string().contains("unknown field")
                || error.to_string().contains("unknown variant")
            {
                ExternalDiagnosticClass::UnsupportedFormat
            } else {
                ExternalDiagnosticClass::Corrupt
            };
            report.finding(class, relative, "unknown_or_invalid_record_fields");
            JsonRead::Invalid
        }
    }
}

fn validate_library_root(
    library_root: &Path,
    prefix: &str,
    report: &mut ExternalInspectionReport,
) -> bool {
    match fs::symlink_metadata(library_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.finding(
                ExternalDiagnosticClass::MissingRequired,
                prefix,
                "required_library_root_missing",
            );
            return false;
        }
        Err(_) => {
            report.finding(
                ExternalDiagnosticClass::InspectionError,
                prefix,
                "metadata_unavailable",
            );
            return false;
        }
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            report.finding(
                ExternalDiagnosticClass::InspectionError,
                prefix,
                "library_root_not_directory",
            );
            return false;
        }
        Ok(_) => {}
    }
    let relative = format!("{prefix}/{}", crate::library::LIBRARY_MANIFEST_FILE);
    let path = library_root.join(crate::library::LIBRARY_MANIFEST_FILE);
    let bytes = match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.finding(
                ExternalDiagnosticClass::MissingRequired,
                relative,
                "required_library_manifest_missing",
            );
            return false;
        }
        Err(_) => {
            report.finding(
                ExternalDiagnosticClass::InspectionError,
                relative,
                "metadata_unavailable",
            );
            return false;
        }
        Ok(_) => match read_bounded(&path, MAX_METADATA_BYTES) {
            Ok(bytes) => bytes,
            Err(reason) => {
                report.finding(ExternalDiagnosticClass::InspectionError, relative, reason);
                return false;
            }
        },
    };
    let value = match parse_json_strict(&bytes) {
        Ok(value) => value,
        Err(error) => {
            report.finding(
                ExternalDiagnosticClass::Corrupt,
                relative,
                crate::storage_inspect_library::json_reason(error),
            );
            return false;
        }
    };
    let known = [
        "v",
        "notebook_ids",
        "active_notebook_id",
        "next_notebook_ordinal",
    ];
    if value
        .as_object()
        .is_some_and(|object| object.keys().any(|key| !known.contains(&key.as_str())))
    {
        report.finding(
            ExternalDiagnosticClass::UnsupportedFormat,
            relative,
            "unrecognized_library_manifest_fields",
        );
        return false;
    }
    let version = value.get("v").and_then(Value::as_u64);
    if version != Some(u64::from(crate::library::LIBRARY_MANIFEST_VERSION)) {
        report.finding(
            if version.is_some() {
                ExternalDiagnosticClass::UnsupportedFormat
            } else {
                ExternalDiagnosticClass::Corrupt
            },
            relative,
            "unsupported_or_invalid_library_manifest_version",
        );
        return false;
    }
    let manifest: crate::library::PersistedLibraryManifestV1 = match serde_json::from_value(value) {
        Ok(manifest) => manifest,
        Err(_) => {
            report.finding(
                ExternalDiagnosticClass::Corrupt,
                relative,
                "invalid_library_manifest_shape",
            );
            return false;
        }
    };
    if crate::library::validate_library_manifest(&manifest).is_err() {
        report.finding(
            ExternalDiagnosticClass::Corrupt,
            relative,
            "invalid_library_manifest_invariants",
        );
        return false;
    }
    report.observe(ExternalDiagnosticClass::Valid);
    true
}

fn hash_regular_file(path: &Path, max: u64) -> Result<(String, u64), &'static str> {
    let metadata = inspect_metadata(path)?;
    if !metadata.is_file() {
        return Err("unsupported_file_type");
    }
    if metadata.len() > max {
        return Err("file_size_limit_exceeded");
    }
    hash_file(path, &metadata).map(|hash| (hash, metadata.len()))
}

#[derive(Default)]
struct BlobReferences {
    expected_bytes: BTreeMap<String, u64>,
    conflicting: BTreeSet<String>,
}

impl BlobReferences {
    fn add(
        &mut self,
        sha256: &Sha256Hex,
        byte_len: u64,
        prefix: &str,
        report: &mut ExternalInspectionReport,
    ) {
        let digest = sha256.as_str().to_owned();
        if let Some(previous) = self.expected_bytes.get(&digest) {
            if *previous != byte_len && self.conflicting.insert(digest.clone()) {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    format!("{prefix}/blobs/{digest}"),
                    "conflicting_blob_byte_count_declarations",
                );
            }
            return;
        }
        self.expected_bytes.insert(digest, byte_len);
    }
}

fn add_background(
    reference: &BackgroundRef,
    blobs: &mut BlobReferences,
    prefix: &str,
    report: &mut ExternalInspectionReport,
) {
    if let BackgroundRef::Blob {
        sha256, byte_len, ..
    } = reference
    {
        blobs.add(sha256, *byte_len, prefix, report);
    }
}

fn add_snapshot_blobs(
    snapshot: &SyncSnapshotV1,
    blobs: &mut BlobReferences,
    prefix: &str,
    report: &mut ExternalInspectionReport,
) {
    for page in snapshot
        .notebooks
        .iter()
        .flat_map(|notebook| &notebook.pages)
    {
        add_background(&page.background, blobs, prefix, report);
    }
}

fn add_envelope_blobs(
    envelope: &SyncEnvelopeV1,
    blobs: &mut BlobReferences,
    prefix: &str,
    report: &mut ExternalInspectionReport,
) {
    match &envelope.operation {
        SyncOperationV1::CreateNotebook { first_page, .. } => {
            add_background(&first_page.background, blobs, prefix, report);
        }
        SyncOperationV1::CreatePage { page, .. } => {
            add_background(&page.background, blobs, prefix, report)
        }
        _ => {}
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaseStateV2Inspection {
    v: u32,
    through_server_seq: u64,
    snapshot: SyncSnapshotV1,
    history: HistoryV1,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxRecordV1Inspection {
    v: u32,
    server_seq: u64,
    envelope: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptedRecordV1Inspection {
    v: u32,
    client_seq: u64,
    server_seq: u64,
    operation_id: OperationId,
    envelope: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyMarkerV1Inspection {
    v: u32,
    target_cursor: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceFileV1Inspection {
    v: u32,
    client_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreferencesFileV1Inspection {
    v: u32,
    side_button: crate::prefs::SideButtonAction,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdoptPlanV1Inspection {
    v: u32,
    adoption_id: String,
    server_id: ServerId,
    library_id: LibraryId,
    replica_id: ReplicaId,
    genesis_sha256: Sha256Hex,
    #[serde(default)]
    profile: bool,
}

#[derive(Default)]
struct EpochScan {
    state: Option<crate::sync_outbound::SyncStateV1>,
    library: Option<LibraryId>,
    replica: Option<ReplicaId>,
    server: Option<ServerId>,
    blobs: BlobReferences,
}

fn read_library_identity(
    library_root: &Path,
    prefix: &str,
    report: &mut ExternalInspectionReport,
) -> Option<IdentityFileV1> {
    let path = library_root
        .join(crate::sync_local::SYNC_DIR)
        .join(crate::sync_local::IDENTITY_FILE);
    let relative = format!("{prefix}/.scribe-sync/identity.json");
    match read_json_typed::<IdentityFileV1>(&path, &relative, MAX_METADATA_BYTES, &[1], report) {
        JsonRead::Missing => None,
        JsonRead::Parsed(identity) => {
            if identity.validate().is_err() {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    relative,
                    "invalid_library_identity",
                );
                None
            } else {
                report.observe(ExternalDiagnosticClass::Valid);
                Some(identity)
            }
        }
        JsonRead::Invalid => None,
    }
}

fn read_replica(
    state_root: &Path,
    prefix: &str,
    report: &mut ExternalInspectionReport,
) -> Option<ReplicaStateV1> {
    let path = state_root.join(sync_protocol::REPLICA_STATE_FILE);
    let relative = format!("{prefix}/replica.json");
    match read_json_typed::<ReplicaStateV1>(&path, &relative, MAX_METADATA_BYTES, &[1], report) {
        JsonRead::Missing => None,
        JsonRead::Parsed(replica) => {
            if replica.validate().is_err() {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    relative,
                    "invalid_replica_identity",
                );
                None
            } else {
                report.observe(ExternalDiagnosticClass::Valid);
                Some(replica)
            }
        }
        JsonRead::Invalid => None,
    }
}

fn read_canonical_envelope(
    bytes: &[u8],
    expected_library: LibraryId,
) -> Result<SyncEnvelopeV1, &'static str> {
    let value = parse_json_strict(bytes).map_err(|_| "invalid_or_duplicate_envelope_json")?;
    let envelope: SyncEnvelopeV1 = serde_json::from_value(value).map_err(|error| {
        if error.to_string().contains("unknown field")
            || error.to_string().contains("unknown variant")
        {
            "unrecognized_envelope_fields_or_variant"
        } else {
            "invalid_envelope"
        }
    })?;
    envelope.validate().map_err(|error| {
        if error.to_string().contains("unsupported") {
            "unsupported_envelope_version_or_operation"
        } else {
            "invalid_envelope"
        }
    })?;
    if envelope.library_id != expected_library {
        return Err("foreign_library_envelope");
    }
    if envelope
        .to_canonical_bytes()
        .map_err(|_| "invalid_envelope")?
        != bytes
    {
        return Err("noncanonical_envelope");
    }
    Ok(envelope)
}

fn envelope_failure_class(reason: &str) -> ExternalDiagnosticClass {
    if reason.starts_with("unrecognized_") || reason.starts_with("unsupported_") {
        ExternalDiagnosticClass::UnsupportedFormat
    } else {
        ExternalDiagnosticClass::Corrupt
    }
}

fn open_stream(path: &Path, max: u64) -> Result<(BufReader<fs::File>, u64), &'static str> {
    let metadata = inspect_metadata(path)?;
    if !metadata.is_file() {
        return Err("unsupported_file_type");
    }
    if metadata.len() > max {
        return Err("file_size_limit_exceeded");
    }
    let file = open_regular_read_only(path, &metadata)?;
    Ok((BufReader::with_capacity(64 * 1024, file), metadata.len()))
}

#[derive(Clone, Copy)]
struct JsonlLimits {
    max_file: u64,
    max_record: usize,
    max_records: usize,
    max_retained: u64,
}

#[derive(Clone, Copy, Debug)]
struct JsonlFailure {
    class: ExternalDiagnosticClass,
    reason: &'static str,
}

#[derive(Clone, Copy, Debug, Default)]
struct JsonlSummary {
    records: usize,
    retained_input_bytes: u64,
    torn_tail: bool,
    empty_records: usize,
    crlf_records: usize,
    noncanonical_records: usize,
}

enum BoundedLine {
    Eof,
    Complete { crlf: bool },
    Torn,
}

fn read_bounded_jsonl_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    max_record: usize,
) -> Result<BoundedLine, JsonlFailure> {
    line.clear();
    loop {
        let available = reader.fill_buf().map_err(|_| JsonlFailure {
            class: ExternalDiagnosticClass::InspectionError,
            reason: "read_failed",
        })?;
        if available.is_empty() {
            return Ok(if line.is_empty() {
                BoundedLine::Eof
            } else {
                BoundedLine::Torn
            });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let copy = newline.unwrap_or(available.len());
        if line.len().saturating_add(copy) > max_record {
            return Err(JsonlFailure {
                class: ExternalDiagnosticClass::InspectionError,
                reason: "record_size_limit_exceeded",
            });
        }
        line.extend_from_slice(&available[..copy]);
        let consumed = copy + usize::from(newline.is_some());
        reader.consume(consumed);
        if newline.is_some() {
            let crlf = line.last() == Some(&b'\r');
            if crlf {
                line.pop();
            }
            return Ok(BoundedLine::Complete { crlf });
        }
    }
}

fn jsonl_parse_failure(error: crate::storage_inspect_library::JsonFailure) -> JsonlFailure {
    use crate::storage_inspect_library::JsonFailure;
    JsonlFailure {
        class: match error {
            JsonFailure::DuplicateKey | JsonFailure::Invalid => ExternalDiagnosticClass::Corrupt,
            JsonFailure::TooDeep | JsonFailure::TooManyNodes => {
                ExternalDiagnosticClass::InspectionError
            }
        },
        reason: crate::storage_inspect_library::json_reason(error),
    }
}

fn jsonl_serde_failure(error: &serde_json::Error) -> JsonlFailure {
    let unsupported = error.to_string().contains("unknown field")
        || error.to_string().contains("unknown variant");
    JsonlFailure {
        class: if unsupported {
            ExternalDiagnosticClass::UnsupportedFormat
        } else {
            ExternalDiagnosticClass::Corrupt
        },
        reason: if unsupported {
            "unrecognized_record_fields_or_variant"
        } else {
            "invalid_record_fields"
        },
    }
}

fn strict_jsonl_visit_with_limits<T, F>(
    path: &Path,
    limits: JsonlLimits,
    retain_records: bool,
    mut visitor: F,
) -> Result<JsonlSummary, JsonlFailure>
where
    T: DeserializeOwned + Serialize,
    F: FnMut(T) -> Result<(), JsonlFailure>,
{
    let (mut reader, _) = open_stream(path, limits.max_file).map_err(|reason| JsonlFailure {
        class: ExternalDiagnosticClass::InspectionError,
        reason,
    })?;
    let mut summary = JsonlSummary::default();
    let mut line = Vec::new();
    loop {
        match read_bounded_jsonl_line(&mut reader, &mut line, limits.max_record)? {
            BoundedLine::Eof => return Ok(summary),
            BoundedLine::Torn => {
                summary.torn_tail = true;
                return Ok(summary);
            }
            BoundedLine::Complete { crlf } => {
                if crlf {
                    summary.crlf_records = summary.crlf_records.saturating_add(1);
                }
            }
        }
        if line.is_empty() {
            summary.empty_records = summary.empty_records.saturating_add(1);
            continue;
        }
        if summary.records >= limits.max_records {
            return Err(JsonlFailure {
                class: ExternalDiagnosticClass::InspectionError,
                reason: "record_count_limit_exceeded",
            });
        }
        if retain_records {
            let retained = summary
                .retained_input_bytes
                .saturating_add(line.len() as u64);
            if retained > limits.max_retained {
                return Err(JsonlFailure {
                    class: ExternalDiagnosticClass::InspectionError,
                    reason: "retained_record_bytes_limit_exceeded",
                });
            }
            summary.retained_input_bytes = retained;
        }
        let value = parse_json_strict(&line).map_err(jsonl_parse_failure)?;
        let record: T =
            serde_json::from_value(value).map_err(|error| jsonl_serde_failure(&error))?;
        if serde_json::to_vec(&record).map_err(|_| JsonlFailure {
            class: ExternalDiagnosticClass::InspectionError,
            reason: "record_reserialization_failed",
        })? != line
        {
            summary.noncanonical_records = summary.noncanonical_records.saturating_add(1);
        }
        visitor(record)?;
        summary.records = summary.records.saturating_add(1);
    }
}

fn strict_jsonl_visit<T, F>(
    path: &Path,
    max_file: u64,
    retain_records: bool,
    visitor: F,
) -> Result<JsonlSummary, JsonlFailure>
where
    T: DeserializeOwned + Serialize,
    F: FnMut(T) -> Result<(), JsonlFailure>,
{
    strict_jsonl_visit_with_limits(
        path,
        JsonlLimits {
            max_file,
            max_record: MAX_RECORD_BYTES as usize,
            max_records: MAX_RECORDS,
            max_retained: MAX_RETAINED_RECORD_BYTES,
        },
        retain_records,
        visitor,
    )
}

fn report_jsonl_encoding(
    summary: JsonlSummary,
    relative: &str,
    report: &mut ExternalInspectionReport,
) {
    if summary.empty_records != 0 {
        report.finding(
            ExternalDiagnosticClass::Unverified,
            relative,
            "empty_journal_records_ignored_by_production_reader",
        );
    }
    if summary.crlf_records != 0 {
        report.finding(
            ExternalDiagnosticClass::Unverified,
            relative,
            "crlf_journal_records_supported_but_noncanonical",
        );
    }
    if summary.noncanonical_records != 0 {
        report.finding(
            ExternalDiagnosticClass::Unverified,
            relative,
            "noncanonical_journal_json_supported_but_not_preservation_proof",
        );
    }
}

fn scan_outbox(
    sync_root: &Path,
    prefix: &str,
    state: &crate::sync_outbound::SyncStateV1,
    epoch: &mut EpochScan,
    report: &mut ExternalInspectionReport,
) {
    let path = sync_root.join(crate::sync_outbound::OUTBOX_FILE);
    let relative = format!("{prefix}/outbox.jsonl");
    if !path.exists() {
        return;
    }
    let mut parser = crate::sync_outbound::JournalParser::default();
    let summary = match strict_jsonl_visit::<crate::sync_outbound::OutboxRecordV1, _>(
        &path,
        MAX_JOURNAL_BYTES,
        true,
        |record| {
            if record.v != 1 {
                return Err(JsonlFailure {
                    class: ExternalDiagnosticClass::UnsupportedFormat,
                    reason: "unsupported_outbox_record_version",
                });
            }
            parser.push(record).map_err(|error| {
                let unsupported = error.contains("unknown field")
                    || error.contains("unknown variant")
                    || error.contains("unsupported envelope version");
                JsonlFailure {
                    class: if unsupported {
                        ExternalDiagnosticClass::UnsupportedFormat
                    } else {
                        ExternalDiagnosticClass::Corrupt
                    },
                    reason: if unsupported {
                        "unrecognized_envelope_fields_or_variant"
                    } else {
                        "invalid_outbox_transaction_sequence"
                    },
                }
            })
        },
    ) {
        Ok(summary) => summary,
        Err(failure) => {
            report.finding(failure.class, relative, failure.reason);
            return;
        }
    };
    report_jsonl_encoding(summary, &relative, report);
    let parsed = parser.finish(summary.torn_tail);
    if parsed.torn_tail {
        report.finding(
            ExternalDiagnosticClass::IncompleteTransaction,
            relative.clone(),
            "torn_outbox_tail_preserved",
        );
    }
    let pending = parsed
        .committed
        .iter()
        .filter(|operation| operation.seq > state.acked_client_seq)
        .collect::<Vec<_>>();
    if let Some(first) = pending.first() {
        if first.seq != state.acked_client_seq.saturating_add(1) {
            report.finding(
                ExternalDiagnosticClass::Corrupt,
                relative.clone(),
                "outbox_gap_after_acknowledged_sequence",
            );
        }
    }
    for operation in &parsed.committed {
        if operation.envelope.library_id != state.library_id
            || operation.envelope.replica_id != state.replica_id
        {
            report.finding(
                ExternalDiagnosticClass::Corrupt,
                relative.clone(),
                "outbox_candidate_identity_mismatch",
            );
            return;
        }
        add_envelope_blobs(&operation.envelope, &mut epoch.blobs, prefix, report);
    }
    if let Some(prep) = &parsed.trailing_prep {
        if prep.seq > state.acked_client_seq {
            if prep.envelope.library_id != state.library_id
                || prep.envelope.replica_id != state.replica_id
            {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    relative.clone(),
                    "outbox_prep_identity_mismatch",
                );
            } else {
                add_envelope_blobs(&prep.envelope, &mut epoch.blobs, prefix, report);
                report.finding(
                    ExternalDiagnosticClass::IncompleteTransaction,
                    relative.clone(),
                    "outbox_prepare_without_terminal_record",
                );
            }
        }
    }
    report.counts.outbound_operations = report
        .counts
        .outbound_operations
        .saturating_add(pending.len() as u64);
    if !pending.is_empty() {
        report.finding(
            ExternalDiagnosticClass::ValidPending,
            relative,
            "outbound_operations_awaiting_confirmation",
        );
    } else {
        report.observe(ExternalDiagnosticClass::Valid);
    }
}

fn scan_inbox(
    sync_root: &Path,
    prefix: &str,
    state: &crate::sync_outbound::SyncStateV1,
    epoch: &mut EpochScan,
    report: &mut ExternalInspectionReport,
) {
    let path = sync_root.join(crate::sync_outbound::INBOX_FILE);
    let relative = format!("{prefix}/inbox.jsonl");
    if !path.exists() {
        return;
    }
    let mut pending_by_seq: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let summary = match strict_jsonl_visit::<InboxRecordV1Inspection, _>(
        &path,
        MAX_JOURNAL_BYTES,
        true,
        |record| {
            if record.v != 1 {
                return Err(JsonlFailure {
                    class: ExternalDiagnosticClass::UnsupportedFormat,
                    reason: "unsupported_inbox_record_version",
                });
            }
            let bytes = record.envelope.into_bytes();
            let envelope = read_canonical_envelope(&bytes, state.library_id).map_err(|reason| {
                JsonlFailure {
                    class: envelope_failure_class(reason),
                    reason,
                }
            })?;
            add_envelope_blobs(&envelope, &mut epoch.blobs, prefix, report);
            if record.server_seq <= state.server_cursor {
                return Ok(());
            }
            if let Some(previous) = pending_by_seq.get(&record.server_seq) {
                return if previous == &bytes {
                    Ok(())
                } else {
                    Err(JsonlFailure {
                        class: ExternalDiagnosticClass::Corrupt,
                        reason: "conflicting_inbox_sequence",
                    })
                };
            }
            let want = pending_by_seq
                .last_key_value()
                .map_or(state.server_cursor.saturating_add(1), |(sequence, _)| {
                    sequence.saturating_add(1)
                });
            if record.server_seq != want {
                return Err(JsonlFailure {
                    class: ExternalDiagnosticClass::Corrupt,
                    reason: "inbox_sequence_gap",
                });
            }
            pending_by_seq.insert(record.server_seq, bytes);
            Ok(())
        },
    ) {
        Ok(summary) => summary,
        Err(failure) => {
            report.finding(failure.class, relative, failure.reason);
            return;
        }
    };
    report_jsonl_encoding(summary, &relative, report);
    if summary.torn_tail {
        report.finding(
            ExternalDiagnosticClass::IncompleteTransaction,
            relative.clone(),
            "torn_inbox_tail_preserved",
        );
    }
    report.counts.inbound_operations = report
        .counts
        .inbound_operations
        .saturating_add(pending_by_seq.len() as u64);
    if pending_by_seq.is_empty() {
        report.observe(ExternalDiagnosticClass::Valid);
    } else {
        report.finding(
            ExternalDiagnosticClass::ValidPending,
            relative,
            "inbound_operations_awaiting_materialization",
        );
    }
}

fn scan_accepted(
    sync_root: &Path,
    prefix: &str,
    state: &crate::sync_outbound::SyncStateV1,
    epoch: &mut EpochScan,
    report: &mut ExternalInspectionReport,
) {
    let path = sync_root.join(crate::sync_outbound::HISTORY_FILE);
    let relative = format!("{prefix}/accepted.jsonl");
    if !path.exists() {
        return;
    }
    let mut by_client: BTreeMap<u64, (u64, OperationId, Vec<u8>)> = BTreeMap::new();
    let mut by_operation = BTreeMap::new();
    let mut by_server = BTreeMap::new();
    let summary = match strict_jsonl_visit::<AcceptedRecordV1Inspection, _>(
        &path,
        MAX_JOURNAL_BYTES,
        true,
        |record| {
            if record.v != 1 {
                return Err(JsonlFailure {
                    class: ExternalDiagnosticClass::UnsupportedFormat,
                    reason: "unsupported_accepted_record_version",
                });
            }
            let bytes = record.envelope.into_bytes();
            let envelope = read_canonical_envelope(&bytes, state.library_id).map_err(|reason| {
                JsonlFailure {
                    class: envelope_failure_class(reason),
                    reason,
                }
            })?;
            if envelope.replica_id != state.replica_id
                || envelope.client_seq != record.client_seq
                || envelope.operation_id != record.operation_id
            {
                return Err(JsonlFailure {
                    class: ExternalDiagnosticClass::Corrupt,
                    reason: "accepted_record_identity_mismatch",
                });
            }
            add_envelope_blobs(&envelope, &mut epoch.blobs, prefix, report);
            let tuple = (record.server_seq, record.operation_id, bytes);
            if let Some(previous) = by_client.get(&record.client_seq) {
                return if previous == &tuple {
                    Ok(())
                } else {
                    Err(JsonlFailure {
                        class: ExternalDiagnosticClass::Corrupt,
                        reason: "conflicting_accepted_client_sequence",
                    })
                };
            }
            if by_operation
                .insert(record.operation_id, record.client_seq)
                .is_some()
                || by_server
                    .insert(record.server_seq, record.client_seq)
                    .is_some()
            {
                return Err(JsonlFailure {
                    class: ExternalDiagnosticClass::Corrupt,
                    reason: "conflicting_accepted_identity",
                });
            }
            by_client.insert(record.client_seq, tuple);
            Ok(())
        },
    ) {
        Ok(summary) => summary,
        Err(failure) => {
            report.finding(failure.class, relative, failure.reason);
            return;
        }
    };
    report_jsonl_encoding(summary, &relative, report);
    if summary.torn_tail {
        report.finding(
            ExternalDiagnosticClass::IncompleteTransaction,
            relative.clone(),
            "torn_accepted_tail_preserved",
        );
    }
    if by_client
        .values()
        .map(|(server, _, _)| *server)
        .collect::<Vec<_>>()
        .windows(2)
        .any(|window| window[0] >= window[1])
    {
        report.finding(
            ExternalDiagnosticClass::Corrupt,
            relative.clone(),
            "accepted_server_sequence_not_increasing",
        );
        return;
    }
    let pending = by_client
        .iter()
        .filter(|(_, (server, _, _))| *server > state.server_cursor)
        .count();
    report.counts.accepted_operations = report
        .counts
        .accepted_operations
        .saturating_add(pending as u64);
    if pending == 0 {
        report.observe(ExternalDiagnosticClass::Valid);
    } else {
        report.finding(
            ExternalDiagnosticClass::ValidPending,
            relative,
            "accepted_operations_awaiting_materialization",
        );
    }
}

fn scan_apply_marker(
    sync_root: &Path,
    prefix: &str,
    state: &crate::sync_outbound::SyncStateV1,
    report: &mut ExternalInspectionReport,
) {
    let path = sync_root.join(crate::sync_outbound::APPLY_FILE);
    let relative = format!("{prefix}/apply.json");
    match read_json_typed::<ApplyMarkerV1Inspection>(
        &path,
        &relative,
        MAX_METADATA_BYTES,
        &[1],
        report,
    ) {
        JsonRead::Missing => {}
        JsonRead::Invalid => {}
        JsonRead::Parsed(marker) => {
            if marker.v != 1 || marker.target_cursor <= state.server_cursor {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    relative,
                    "invalid_apply_marker_cursor",
                );
            } else {
                report.finding(
                    ExternalDiagnosticClass::IncompleteTransaction,
                    relative,
                    "inbound_apply_interrupted",
                );
            }
        }
    }
}

fn checkpoint_state(
    sync_root: &Path,
    prefix: &str,
    expected_library: LibraryId,
    blobs: &mut BlobReferences,
    report: &mut ExternalInspectionReport,
) -> Option<(LibraryState, u64)> {
    let manifest_path = sync_root
        .join(crate::sync_outbound::confirmed::CHECKPOINT_DIR)
        .join(crate::sync_outbound::confirmed::CHECKPOINT_MANIFEST);
    let relative = format!("{prefix}/checkpoint/manifest.json");
    let manifest = match read_json_typed::<crate::sync_outbound::confirmed::CheckpointManifestV3>(
        &manifest_path,
        &relative,
        MAX_METADATA_BYTES,
        &[3],
        report,
    ) {
        JsonRead::Missing | JsonRead::Invalid => return None,
        JsonRead::Parsed(manifest) => manifest,
    };
    if manifest.library_id != expected_library {
        report.finding(
            ExternalDiagnosticClass::Corrupt,
            relative,
            "checkpoint_library_identity_mismatch",
        );
        return None;
    }
    let pages_root = sync_root
        .join(crate::sync_outbound::confirmed::CHECKPOINT_DIR)
        .join(crate::sync_outbound::confirmed::CHECKPOINT_PAGES);
    let mut notebooks = Vec::with_capacity(manifest.notebooks.len());
    let mut referenced = BTreeSet::new();
    for notebook in manifest.notebooks {
        let mut pages = Vec::with_capacity(notebook.pages.len());
        for reference in notebook.pages {
            let name = reference.sha256.as_str();
            referenced.insert(name.to_owned());
            let page_path = pages_root.join(name);
            let page_relative = format!("{prefix}/checkpoint/pages/{name}");
            let bytes = match read_bounded(&page_path, MAX_RECORD_BYTES) {
                Ok(bytes) => bytes,
                Err(reason) => {
                    let class = if reason == "metadata_unavailable" {
                        ExternalDiagnosticClass::MissingRequired
                    } else {
                        ExternalDiagnosticClass::InspectionError
                    };
                    report.finding(class, page_relative, reason);
                    return None;
                }
            };
            if sha256_hex(&bytes) != reference.sha256 {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    page_relative,
                    "checkpoint_page_digest_mismatch",
                );
                return None;
            }
            let value = match parse_json_strict(&bytes) {
                Ok(value) => value,
                Err(error) => {
                    report.finding(
                        ExternalDiagnosticClass::Corrupt,
                        page_relative,
                        crate::storage_inspect_library::json_reason(error),
                    );
                    return None;
                }
            };
            let page: SnapshotPage = match serde_json::from_value(value) {
                Ok(page) => page,
                Err(error) => {
                    let unsupported = error.to_string().contains("unknown field")
                        || error.to_string().contains("unknown variant");
                    report.finding(
                        if unsupported {
                            ExternalDiagnosticClass::UnsupportedFormat
                        } else {
                            ExternalDiagnosticClass::Corrupt
                        },
                        page_relative,
                        if unsupported {
                            "unrecognized_checkpoint_page_fields_or_variant"
                        } else {
                            "invalid_checkpoint_page"
                        },
                    );
                    return None;
                }
            };
            if page.page_id != reference.page_id {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    page_relative,
                    "checkpoint_page_identity_mismatch",
                );
                return None;
            }
            if serde_json::to_vec(&page).is_ok_and(|canonical| canonical != bytes) {
                report.finding(
                    ExternalDiagnosticClass::Unverified,
                    page_relative.clone(),
                    "noncanonical_checkpoint_page_supported_by_reader",
                );
            }
            add_background(&page.background, blobs, prefix, report);
            pages.push(page);
        }
        notebooks.push(SnapshotNotebook {
            notebook_id: notebook.notebook_id,
            title: notebook.title,
            pages,
            folder_id: notebook.folder_id,
        });
    }
    let state = LibraryState {
        snapshot: SyncSnapshotV1 {
            v: sync_protocol::SNAPSHOT_VERSION,
            library_id: manifest.library_id,
            page_space: manifest.page_space,
            notebooks,
            folders: manifest.folders,
        },
        history: manifest.history,
    };
    if state.validate().is_err() {
        report.finding(
            ExternalDiagnosticClass::Corrupt,
            format!("{prefix}/checkpoint"),
            "invalid_checkpoint_reducer_state",
        );
        return None;
    }
    match fs::read_dir(&pages_root) {
        Ok(read_dir) => {
            for entry in read_dir.flatten() {
                if let Some(name) = safe_component(&entry.file_name()) {
                    if !referenced.contains(&name) {
                        report.finding(
                            ExternalDiagnosticClass::ValidPending,
                            format!("{prefix}/checkpoint/pages/{name}"),
                            "unreferenced_checkpoint_page_preserved",
                        );
                    }
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && referenced.is_empty() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.finding(
                ExternalDiagnosticClass::MissingRequired,
                format!("{prefix}/checkpoint/pages"),
                "checkpoint_pages_directory_missing",
            );
            return None;
        }
        Err(_) => {
            report.finding(
                ExternalDiagnosticClass::InspectionError,
                format!("{prefix}/checkpoint/pages"),
                "directory_unreadable",
            );
            return None;
        }
    }
    report.observe(ExternalDiagnosticClass::Valid);
    Some((state, manifest.through_server_seq))
}

fn legacy_base_state(
    sync_root: &Path,
    prefix: &str,
    expected_library: LibraryId,
    blobs: &mut BlobReferences,
    report: &mut ExternalInspectionReport,
) -> Option<(LibraryState, u64)> {
    let path = sync_root.join(crate::sync_outbound::BASE_FILE);
    let relative = format!("{prefix}/base-state.json");
    let base = match read_json_typed::<BaseStateV2Inspection>(
        &path,
        &relative,
        MAX_BLOB_BYTES,
        &[2],
        report,
    ) {
        JsonRead::Missing | JsonRead::Invalid => return None,
        JsonRead::Parsed(base) => base,
    };
    if base.v != 2 || base.snapshot.library_id != expected_library {
        report.finding(
            ExternalDiagnosticClass::Corrupt,
            relative,
            "legacy_base_identity_mismatch",
        );
        return None;
    }
    let state = LibraryState {
        snapshot: base.snapshot,
        history: base.history,
    };
    if state.validate().is_err() {
        report.finding(
            ExternalDiagnosticClass::Corrupt,
            relative,
            "invalid_legacy_base_state",
        );
        return None;
    }
    add_snapshot_blobs(&state.snapshot, blobs, prefix, report);
    report.finding(
        ExternalDiagnosticClass::ValidPending,
        format!("{prefix}/base-state.json"),
        "legacy_base_awaiting_checkpoint_conversion",
    );
    Some((state, base.through_server_seq))
}

fn scan_confirmed(
    sync_root: &Path,
    prefix: &str,
    state: &crate::sync_outbound::SyncStateV1,
    epoch: &mut EpochScan,
    report: &mut ExternalInspectionReport,
) {
    let checkpoint_manifest = sync_root
        .join(crate::sync_outbound::confirmed::CHECKPOINT_DIR)
        .join(crate::sync_outbound::confirmed::CHECKPOINT_MANIFEST);
    let legacy_path = sync_root.join(crate::sync_outbound::BASE_FILE);
    let checkpoint_present = checkpoint_manifest.exists();
    let legacy_present = legacy_path.exists();
    if checkpoint_present && legacy_present {
        report.finding(
            ExternalDiagnosticClass::Unverified,
            format!("{prefix}/base-state.json"),
            "legacy_base_shadowed_by_checkpoint",
        );
    }
    let base = if checkpoint_present {
        checkpoint_state(
            sync_root,
            prefix,
            state.library_id,
            &mut epoch.blobs,
            report,
        )
    } else {
        legacy_base_state(
            sync_root,
            prefix,
            state.library_id,
            &mut epoch.blobs,
            report,
        )
    };
    let Some((mut reducer, through)) = base else {
        if state.v == crate::sync_outbound::SYNC_STATE_VERSION {
            report.finding(
                ExternalDiagnosticClass::MissingRequired,
                format!("{prefix}/checkpoint"),
                "confirmed_base_missing_for_v2_epoch",
            );
        }
        return;
    };
    let log_path = sync_root.join(crate::sync_outbound::confirmed::CONFIRMED_LOG_FILE);
    let relative = format!("{prefix}/confirmed.log");
    let (reader, len) = match fs::symlink_metadata(&log_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if through < state.server_cursor {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    relative,
                    "confirmed_log_missing_before_cursor",
                );
            }
            return;
        }
        Err(_) => {
            report.finding(
                ExternalDiagnosticClass::InspectionError,
                relative,
                "metadata_unavailable",
            );
            return;
        }
        Ok(_) => match open_stream(&log_path, MAX_JOURNAL_BYTES) {
            Ok(opened) => opened,
            Err(reason) => {
                report.finding(ExternalDiagnosticClass::InspectionError, relative, reason);
                return;
            }
        },
    };
    let mut applied = through;
    let scan = crate::sync_outbound::confirmed::scan_log_reader(
        reader,
        len,
        &relative,
        |server_sequence, payload| {
            if server_sequence <= through {
                return Ok(());
            }
            if server_sequence != applied.saturating_add(1) {
                return Err("confirmed sequence does not follow checkpoint".into());
            }
            let envelope =
                read_canonical_envelope(payload, state.library_id).map_err(str::to_owned)?;
            add_envelope_blobs(&envelope, &mut epoch.blobs, prefix, report);
            reducer
                .apply_in_place(&envelope)
                .map_err(|_| "confirmed operation is semantically invalid".to_owned())?;
            applied = server_sequence;
            Ok(())
        },
    );
    let scan = match scan {
        Ok(scan) => scan,
        Err(_) => {
            report.finding(
                ExternalDiagnosticClass::Corrupt,
                relative,
                "invalid_confirmed_log",
            );
            return;
        }
    };
    report.counts.confirmed_operations = report
        .counts
        .confirmed_operations
        .saturating_add(applied.saturating_sub(through));
    if scan.torn_tail {
        report.finding(
            ExternalDiagnosticClass::IncompleteTransaction,
            relative.clone(),
            "torn_confirmed_log_tail_preserved",
        );
    }
    if applied < state.server_cursor {
        report.finding(
            ExternalDiagnosticClass::Corrupt,
            relative,
            "confirmed_state_behind_persisted_cursor",
        );
    } else if applied > state.server_cursor {
        report.finding(
            ExternalDiagnosticClass::ValidPending,
            relative,
            "confirmed_state_awaiting_cursor_recovery",
        );
    } else {
        report.observe(ExternalDiagnosticClass::Valid);
    }
}

fn scan_blobs(
    sync_root: &Path,
    prefix: &str,
    references: &BlobReferences,
    report: &mut ExternalInspectionReport,
) {
    let root = sync_root.join(crate::sync_outbound::BLOB_DIR);
    for (digest, expected_bytes) in &references.expected_bytes {
        if references.conflicting.contains(digest) {
            continue;
        }
        let path = root.join(digest);
        let relative = format!("{prefix}/blobs/{digest}");
        match hash_regular_file(&path, MAX_BLOB_BYTES) {
            Ok((actual_digest, actual_bytes))
                if actual_digest == *digest && actual_bytes == *expected_bytes =>
            {
                report.counts.blobs = report.counts.blobs.saturating_add(1);
                report.observe(ExternalDiagnosticClass::Valid);
            }
            Ok((actual_digest, _)) if actual_digest != *digest => report.finding(
                ExternalDiagnosticClass::Corrupt,
                relative,
                "blob_digest_mismatch",
            ),
            Ok(_) => report.finding(
                ExternalDiagnosticClass::Corrupt,
                relative,
                "blob_byte_count_mismatch",
            ),
            Err("metadata_unavailable") => report.finding(
                ExternalDiagnosticClass::MissingRequired,
                relative,
                "referenced_blob_missing",
            ),
            Err(reason) => {
                report.finding(ExternalDiagnosticClass::InspectionError, relative, reason)
            }
        }
    }
    let read_dir = match fs::read_dir(&root) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            report.finding(
                ExternalDiagnosticClass::InspectionError,
                format!("{prefix}/blobs"),
                "directory_unreadable",
            );
            return;
        }
    };
    for entry in read_dir.flatten() {
        if let Some(name) = safe_component(&entry.file_name()) {
            if !references.expected_bytes.contains_key(&name) {
                if name.ends_with(".tmp") {
                    report.finding(
                        ExternalDiagnosticClass::IncompleteTransaction,
                        format!("{prefix}/blobs/{name}"),
                        "temporary_blob_object_preserved",
                    );
                } else {
                    report.finding(
                        ExternalDiagnosticClass::Unverified,
                        format!("{prefix}/blobs/{name}"),
                        "unreferenced_blob_preserved",
                    );
                }
            }
        }
    }
}

fn scan_checkpoint_artifacts(
    sync_root: &Path,
    prefix: &str,
    report: &mut ExternalInspectionReport,
) {
    let checkpoint = sync_root.join(crate::sync_outbound::confirmed::CHECKPOINT_DIR);
    let read_dir = match fs::read_dir(checkpoint) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            report.finding(
                ExternalDiagnosticClass::InspectionError,
                format!("{prefix}/checkpoint"),
                "directory_unreadable",
            );
            return;
        }
    };
    for entry in read_dir.flatten() {
        let Some(name) = safe_component(&entry.file_name()) else {
            continue;
        };
        if matches!(
            name.as_str(),
            crate::sync_outbound::confirmed::CHECKPOINT_MANIFEST
                | crate::sync_outbound::confirmed::CHECKPOINT_PAGES
        ) {
            continue;
        }
        if name.ends_with(".tmp") {
            report.finding(
                ExternalDiagnosticClass::IncompleteTransaction,
                format!("{prefix}/checkpoint/{name}"),
                "temporary_checkpoint_object_preserved",
            );
        } else {
            report.finding(
                ExternalDiagnosticClass::Unverified,
                format!("{prefix}/checkpoint/{name}"),
                "unknown_checkpoint_object_preserved",
            );
        }
    }
}

fn scan_preferences(path: &Path, relative: &str, report: &mut ExternalInspectionReport) {
    match read_json_typed::<PreferencesFileV1Inspection>(
        path,
        relative,
        MAX_METADATA_BYTES,
        &[1],
        report,
    ) {
        JsonRead::Missing => {}
        JsonRead::Invalid => {}
        JsonRead::Parsed(preferences) => {
            let _ = preferences.side_button;
            if preferences.v == 1 {
                report.observe(ExternalDiagnosticClass::Valid);
            }
        }
    }
}

fn scan_verified_state(
    sync_root: &Path,
    library_root: &Path,
    prefix: &str,
    state: &crate::sync_outbound::SyncStateV1,
    report: &mut ExternalInspectionReport,
) {
    let stamp_path = sync_root.join(crate::sync_verified::STAMP_FILE);
    let cleared_path = sync_root.join(crate::sync_verified::STAMP_CLEARED_FILE);
    let stamp_relative = format!("{prefix}/{}", crate::sync_verified::STAMP_FILE);
    match read_json_typed::<crate::sync_verified::LocalVerifiedV1>(
        &stamp_path,
        &stamp_relative,
        MAX_METADATA_BYTES,
        &[u64::from(crate::sync_verified::STAMP_VERSION)],
        report,
    ) {
        JsonRead::Parsed(stamp) => {
            if stamp.library_id != state.library_id
                || stamp.replica_id != state.replica_id
                || stamp.server_cursor != state.server_cursor
                || stamp.acked_client_seq != state.acked_client_seq
                || Sha256Hex::parse(&stamp.census_sha256).is_err()
            {
                report.finding(
                    ExternalDiagnosticClass::Unverified,
                    stamp_relative,
                    "verified_stamp_does_not_match_epoch",
                );
            } else {
                report.finding(
                    ExternalDiagnosticClass::Unverified,
                    stamp_relative,
                    "verified_stamp_cache_not_compatibility_proof",
                );
            }
        }
        JsonRead::Missing | JsonRead::Invalid => {}
    }
    if cleared_path.exists() {
        match read_bounded(&cleared_path, 1) {
            Ok(bytes) if bytes.is_empty() => {
                if stamp_path.exists() {
                    report.finding(
                        ExternalDiagnosticClass::Corrupt,
                        format!("{prefix}/{}", crate::sync_verified::STAMP_CLEARED_FILE),
                        "verified_stamp_and_cleared_marker_both_present",
                    );
                } else {
                    report.finding(
                        ExternalDiagnosticClass::ValidPending,
                        format!("{prefix}/{}", crate::sync_verified::STAMP_CLEARED_FILE),
                        "verified_stamp_cleared_dirty_fallback",
                    );
                }
            }
            Ok(_) => report.finding(
                ExternalDiagnosticClass::Corrupt,
                format!("{prefix}/{}", crate::sync_verified::STAMP_CLEARED_FILE),
                "invalid_verified_cleared_marker",
            ),
            Err(reason) => report.finding(
                ExternalDiagnosticClass::InspectionError,
                format!("{prefix}/{}", crate::sync_verified::STAMP_CLEARED_FILE),
                reason,
            ),
        }
    }
    let baseline_path = sync_root.join(crate::verified_baseline::BASELINE_FILE);
    let baseline_relative = format!("{prefix}/{}", crate::verified_baseline::BASELINE_FILE);
    match read_json_typed::<crate::verified_baseline::VerifiedBaselineV1>(
        &baseline_path,
        &baseline_relative,
        crate::verified_baseline::BASELINE_MAX_BYTES,
        &[u64::from(crate::verified_baseline::BASELINE_VERSION)],
        report,
    ) {
        JsonRead::Missing | JsonRead::Invalid => {}
        JsonRead::Parsed(baseline) => match crate::verified_baseline::validate_baseline(
            &baseline,
            state.library_id,
            state.replica_id,
        ) {
            Ok(()) => report.finding(
                ExternalDiagnosticClass::Unverified,
                baseline_relative,
                "verified_baseline_is_shadow_diagnostic_only",
            ),
            Err(crate::verified_baseline::BaselineReject::OtherIdentity) => report.finding(
                ExternalDiagnosticClass::Unverified,
                baseline_relative,
                "verified_baseline_identity_mismatch",
            ),
            Err(_) => report.finding(
                ExternalDiagnosticClass::Corrupt,
                baseline_relative,
                "invalid_verified_baseline",
            ),
        },
    }
    let generation_path = library_root
        .join(crate::sync_local::SYNC_DIR)
        .join(crate::sync_verified::MUTATION_GEN_FILE);
    let generation_relative = "library/.scribe-sync/local-mutation-gen";
    if generation_path.exists() {
        match read_bounded(&generation_path, 64) {
            Ok(bytes) => {
                let canonical = bytes.ends_with(b"\n")
                    && bytes.len() > 1
                    && bytes[..bytes.len() - 1].iter().all(u8::is_ascii_digit)
                    && (bytes.len() == 2 || bytes.first() != Some(&b'0'))
                    && std::str::from_utf8(&bytes[..bytes.len() - 1])
                        .ok()
                        .and_then(|value| value.parse::<u64>().ok())
                        .is_some();
                if canonical {
                    report.finding(
                        ExternalDiagnosticClass::Unverified,
                        generation_relative,
                        "mutation_generation_is_cooperating_diagnostic_only",
                    );
                } else {
                    report.finding(
                        ExternalDiagnosticClass::Corrupt,
                        generation_relative,
                        "invalid_mutation_generation",
                    );
                }
            }
            Err(reason) => report.finding(
                ExternalDiagnosticClass::InspectionError,
                generation_relative,
                reason,
            ),
        }
    }
    let ledger = library_root
        .join(crate::sync_local::SYNC_DIR)
        .join(crate::page_ledger::LEDGER_FILE);
    if ledger.exists() {
        let relative = "library/.scribe-sync/page-ledger-v1.jsonl";
        match strict_jsonl_visit::<crate::page_ledger::LedgerRecordV1, _>(
            &ledger,
            crate::page_ledger::READ_MAX_BYTES,
            false,
            |record| {
                let line = serde_json::to_string(&record).map_err(|_| JsonlFailure {
                    class: ExternalDiagnosticClass::InspectionError,
                    reason: "record_reserialization_failed",
                })?;
                crate::page_ledger::parse_line(&line)
                    .map(|_| ())
                    .map_err(|_| JsonlFailure {
                        class: ExternalDiagnosticClass::Corrupt,
                        reason: "invalid_page_ledger",
                    })
            },
        ) {
            Ok(summary) => {
                report_jsonl_encoding(summary, relative, report);
                if summary.torn_tail {
                    report.finding(
                        ExternalDiagnosticClass::IncompleteTransaction,
                        relative,
                        "torn_page_ledger_tail_preserved",
                    );
                } else {
                    report.finding(
                        ExternalDiagnosticClass::Unverified,
                        relative,
                        "page_ledger_valid_but_not_authoritative_snapshot_proof",
                    );
                }
            }
            Err(failure) => report.finding(failure.class, relative, failure.reason),
        }
    }
}

fn scan_recovery(
    sync_root: &Path,
    prefix: &str,
    state: &crate::sync_outbound::SyncStateV1,
    epoch: &mut EpochScan,
    report: &mut ExternalInspectionReport,
) {
    let intent_path = sync_root.join(crate::sync_verified::RECOVERY_INTENT_FILE);
    let intent_relative = format!("{prefix}/{}", crate::sync_verified::RECOVERY_INTENT_FILE);
    let intent = match read_json_typed::<crate::sync_recovery_exec::RecoveryIntentV1>(
        &intent_path,
        &intent_relative,
        MAX_BLOB_BYTES,
        &[u64::from(
            crate::sync_recovery_exec::RECOVERY_INTENT_VERSION,
        )],
        report,
    ) {
        JsonRead::Missing => None,
        JsonRead::Invalid => None,
        JsonRead::Parsed(intent) => {
            let sha = |value: &str| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            };
            let expected_next = intent
                .acked_client_seq
                .checked_add(1)
                .and_then(|first| first.checked_add(intent.ops.len() as u64));
            let mut valid = !intent.upload_authorized
                && uuid::Uuid::try_parse(&intent.transaction_id).is_ok()
                && intent.library_id == state.library_id
                && intent.replica_id == state.replica_id
                && intent.genesis_sha256 == state.genesis_sha256.as_str()
                && sha(&intent.local_snapshot_sha256)
                && sha(&intent.identity_sha256)
                && sha(&intent.plan_fingerprint)
                && intent.accepted_prefix <= intent.ops.len()
                && Some(intent.next_client_seq_after) == expected_next;
            let mut previous = None;
            for frozen in &intent.ops {
                let envelope =
                    read_canonical_envelope(frozen.envelope.as_bytes(), intent.library_id);
                match envelope {
                    Ok(envelope)
                        if envelope.replica_id == intent.replica_id
                            && envelope.operation_id == frozen.operation_id
                            && envelope.client_seq == frozen.client_seq
                            && previous.map_or(
                                frozen.client_seq == intent.acked_client_seq.saturating_add(1),
                                |value: u64| frozen.client_seq == value.saturating_add(1),
                            ) =>
                    {
                        previous = Some(frozen.client_seq);
                        add_envelope_blobs(&envelope, &mut epoch.blobs, prefix, report);
                    }
                    _ => valid = false,
                }
            }
            if valid {
                report.finding(
                    ExternalDiagnosticClass::IncompleteTransaction,
                    intent_relative.clone(),
                    "recovery_intent_pending",
                );
                Some(intent)
            } else {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    intent_relative.clone(),
                    "invalid_recovery_intent_invariants",
                );
                None
            }
        }
    };
    let completion_path = sync_root.join(crate::sync_recovery_exec::RECOVERY_COMPLETE_FILE);
    let completion_relative = format!(
        "{prefix}/{}",
        crate::sync_recovery_exec::RECOVERY_COMPLETE_FILE
    );
    match read_json_typed::<crate::sync_recovery_exec::RecoveryCompleteV1>(
        &completion_path,
        &completion_relative,
        MAX_METADATA_BYTES,
        &[u64::from(
            crate::sync_recovery_exec::RECOVERY_COMPLETE_VERSION,
        )],
        report,
    ) {
        JsonRead::Missing => {}
        JsonRead::Invalid => {}
        JsonRead::Parsed(completion) => {
            let sha = |value: &str| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            };
            let matching = completion.library_id == state.library_id
                && completion.replica_id == state.replica_id
                && uuid::Uuid::try_parse(&completion.transaction_id).is_ok()
                && sha(&completion.local_snapshot_sha256)
                && sha(&completion.identity_sha256)
                && intent.as_ref().is_none_or(|intent| {
                    completion.transaction_id == intent.transaction_id
                        && completion.local_snapshot_sha256 == intent.local_snapshot_sha256
                        && completion.identity_sha256 == intent.identity_sha256
                });
            if matching {
                report.finding(
                    ExternalDiagnosticClass::Unverified,
                    completion_relative,
                    "recovery_completion_is_historical_evidence_only",
                );
            } else {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    completion_relative,
                    "recovery_completion_identity_mismatch",
                );
            }
        }
    }
    let claim = sync_root.join("recovery-intent-v1.claim");
    if claim.exists() {
        report.finding(
            ExternalDiagnosticClass::IncompleteTransaction,
            format!("{prefix}/recovery-intent-v1.claim"),
            "recovery_prepare_claim_present",
        );
    }
}

fn scan_epoch(
    library_root: &Path,
    sync_root: &Path,
    library_prefix: &str,
    prefix: &str,
    report: &mut ExternalInspectionReport,
) -> EpochScan {
    let mut epoch = EpochScan::default();
    let _ = validate_library_root(library_root, library_prefix, report);
    let root_metadata = match fs::symlink_metadata(sync_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return epoch,
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => metadata,
        Ok(_) => {
            report.finding(
                ExternalDiagnosticClass::InspectionError,
                prefix,
                "sync_root_not_directory",
            );
            return epoch;
        }
        Err(_) => {
            report.finding(
                ExternalDiagnosticClass::InspectionError,
                prefix,
                "metadata_unavailable",
            );
            return epoch;
        }
    };
    let _ = root_metadata;
    let identity = read_library_identity(library_root, library_prefix, report);
    let replica = read_replica(sync_root, prefix, report);
    let state_path = sync_root.join(crate::sync_outbound::STATE_FILE);
    let state_relative = format!("{prefix}/sync-state.json");
    let state = match read_json_typed::<crate::sync_outbound::SyncStateV1>(
        &state_path,
        &state_relative,
        MAX_METADATA_BYTES,
        &[1, u64::from(crate::sync_outbound::SYNC_STATE_VERSION)],
        report,
    ) {
        JsonRead::Missing => {
            let nontrivial = fs::read_dir(sync_root)
                .ok()
                .is_some_and(|mut entries| entries.next().is_some());
            if nontrivial {
                report.finding(
                    ExternalDiagnosticClass::MissingRequired,
                    state_relative,
                    "sync_state_missing_from_nonempty_epoch",
                );
            }
            return epoch;
        }
        JsonRead::Invalid => return epoch,
        JsonRead::Parsed(state) => state,
    };
    epoch.library = Some(state.library_id);
    epoch.replica = Some(state.replica_id);
    epoch.server = state.server_id;
    if identity.as_ref().map(|value| value.library_id) != Some(state.library_id) {
        report.finding(
            if identity.is_some() {
                ExternalDiagnosticClass::Corrupt
            } else {
                ExternalDiagnosticClass::MissingRequired
            },
            state_relative.clone(),
            "sync_and_library_identity_mismatch",
        );
    }
    if replica.as_ref().map(|value| value.replica_id) != Some(state.replica_id) {
        report.finding(
            if replica.is_some() {
                ExternalDiagnosticClass::Corrupt
            } else {
                ExternalDiagnosticClass::MissingRequired
            },
            state_relative.clone(),
            "sync_and_replica_identity_mismatch",
        );
    }
    if state.next_client_seq == 0 || state.next_client_seq <= state.acked_client_seq {
        report.finding(
            ExternalDiagnosticClass::Corrupt,
            state_relative.clone(),
            "invalid_persisted_client_sequence",
        );
    }
    if state.v == 1 {
        if state.acked_client_seq != 0 || state.server_cursor != 0 {
            report.finding(
                ExternalDiagnosticClass::UnsupportedFormat,
                state_relative.clone(),
                "historical_v1_epoch_cannot_upgrade_in_place",
            );
        } else {
            report.finding(
                ExternalDiagnosticClass::ValidPending,
                state_relative.clone(),
                "historical_v1_epoch_awaiting_supported_upgrade",
            );
        }
    } else {
        report.observe(ExternalDiagnosticClass::Valid);
    }
    let genesis_path = sync_root.join(crate::sync_outbound::GENESIS_FILE);
    let genesis_relative = format!("{prefix}/genesis.json");
    match read_bounded(&genesis_path, MAX_BLOB_BYTES) {
        Ok(bytes) => {
            let value = match parse_json_strict(&bytes) {
                Ok(value) => Some(value),
                Err(error) => {
                    report.finding(
                        ExternalDiagnosticClass::Corrupt,
                        genesis_relative.clone(),
                        crate::storage_inspect_library::json_reason(error),
                    );
                    None
                }
            };
            let genesis =
                value.and_then(
                    |value| match serde_json::from_value::<SyncSnapshotV1>(value) {
                        Ok(genesis) => Some(genesis),
                        Err(error) => {
                            let unsupported = error.to_string().contains("unknown field")
                                || error.to_string().contains("unknown variant");
                            report.finding(
                                if unsupported {
                                    ExternalDiagnosticClass::UnsupportedFormat
                                } else {
                                    ExternalDiagnosticClass::Corrupt
                                },
                                genesis_relative.clone(),
                                if unsupported {
                                    "unrecognized_genesis_fields_or_variant"
                                } else {
                                    "invalid_genesis_snapshot"
                                },
                            );
                            None
                        }
                    },
                );
            if let Some(genesis) = genesis {
                if genesis.validate().is_ok()
                    && genesis.library_id == state.library_id
                    && genesis
                        .to_canonical_bytes()
                        .is_ok_and(|canonical| canonical == bytes)
                    && sha256_hex(&bytes) == state.genesis_sha256
                {
                    add_snapshot_blobs(&genesis, &mut epoch.blobs, prefix, report);
                    report.observe(ExternalDiagnosticClass::Valid);
                } else {
                    report.finding(
                        ExternalDiagnosticClass::Corrupt,
                        genesis_relative,
                        "genesis_identity_digest_or_canonical_bytes_mismatch",
                    );
                }
            }
        }
        Err("metadata_unavailable") => report.finding(
            ExternalDiagnosticClass::MissingRequired,
            genesis_relative,
            "genesis_missing",
        ),
        Err(reason) => report.finding(
            ExternalDiagnosticClass::InspectionError,
            genesis_relative,
            reason,
        ),
    }
    let config_path = sync_root.join(crate::sync_outbound::CONFIG_FILE);
    let config_relative = format!("{prefix}/config.json");
    if let JsonRead::Parsed(config) = read_json_typed::<crate::sync_outbound::SyncConfigV1>(
        &config_path,
        &config_relative,
        MAX_METADATA_BYTES,
        &[1],
        report,
    ) {
        if config.server_base_url.is_empty() || config.token.is_empty() {
            report.finding(
                ExternalDiagnosticClass::Corrupt,
                config_relative,
                "invalid_sync_configuration",
            );
        } else if state.server_id.is_some() && config.server_id != state.server_id {
            report.finding(
                ExternalDiagnosticClass::Corrupt,
                config_relative,
                "configured_server_identity_mismatch",
            );
        } else {
            report.finding(
                ExternalDiagnosticClass::Unverified,
                config_relative,
                "server_credential_not_offline_verified",
            );
        }
    }
    scan_outbox(sync_root, prefix, &state, &mut epoch, report);
    scan_inbox(sync_root, prefix, &state, &mut epoch, report);
    scan_accepted(sync_root, prefix, &state, &mut epoch, report);
    scan_apply_marker(sync_root, prefix, &state, report);
    scan_confirmed(sync_root, prefix, &state, &mut epoch, report);
    scan_checkpoint_artifacts(sync_root, prefix, report);
    scan_recovery(sync_root, prefix, &state, &mut epoch, report);
    scan_verified_state(sync_root, library_root, prefix, &state, report);
    scan_blobs(sync_root, prefix, &epoch.blobs, report);
    scan_preferences(
        &sync_root.join(crate::prefs::PREFERENCES_FILE),
        &format!("{prefix}/preferences.json"),
        report,
    );
    epoch.state = Some(state);
    epoch
}

fn scan_device(
    config: &ExternalInspectionConfig,
    epoch: &EpochScan,
    report: &mut ExternalInspectionReport,
) {
    let device_path = config.device_root.join(crate::server_link::DEVICE_FILE);
    match read_json_typed::<DeviceFileV1Inspection>(
        &device_path,
        "device/device.json",
        MAX_METADATA_BYTES,
        &[1],
        report,
    ) {
        JsonRead::Missing => {
            if epoch.state.is_some() {
                report.finding(
                    ExternalDiagnosticClass::Unverified,
                    "device/device.json",
                    "device_identity_absent_but_not_required_for_offline_epoch",
                );
            }
        }
        JsonRead::Invalid => {}
        JsonRead::Parsed(device) => {
            if device.v != 1 || uuid::Uuid::try_parse(&device.client_id).is_err() {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    "device/device.json",
                    "invalid_device_identity",
                );
            } else {
                report.observe(ExternalDiagnosticClass::Valid);
            }
        }
    }
    let profile_path = config.device_root.join(crate::server_link::PROFILE_FILE);
    match read_json_typed::<crate::server_link::ServerProfileV1>(
        &profile_path,
        "device/server-profile.json",
        MAX_METADATA_BYTES,
        &[1],
        report,
    ) {
        JsonRead::Missing => {
            if epoch.server.is_some() {
                report.finding(
                    ExternalDiagnosticClass::MissingRequired,
                    "device/server-profile.json",
                    "server_bound_epoch_has_no_pairing_profile",
                );
            }
        }
        JsonRead::Invalid => {}
        JsonRead::Parsed(profile) => {
            if profile.v != 1
                || profile.server_name.is_empty()
                || profile.credential.is_empty()
                || profile.last_endpoint.is_empty()
            {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    "device/server-profile.json",
                    "invalid_server_profile",
                );
            } else if epoch
                .server
                .is_some_and(|server| server != profile.server_id)
            {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    "device/server-profile.json",
                    "profile_server_identity_mismatch",
                );
            } else {
                report.finding(
                    ExternalDiagnosticClass::Unverified,
                    "device/server-profile.json",
                    "pairing_credential_not_offline_verified",
                );
            }
        }
    }
    scan_preferences(
        &config.device_root.join(crate::prefs::PREFERENCES_FILE),
        "device/preferences.json",
        report,
    );
    scan_preferences(
        &config.state_root.join(crate::prefs::PREFERENCES_FILE),
        "state/preferences.json",
        report,
    );
    for (root, prefix) in [
        (&config.device_root, "device"),
        (&config.state_root, "state"),
    ] {
        let read_dir = match fs::read_dir(root) {
            Ok(read_dir) => read_dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                report.finding(
                    ExternalDiagnosticClass::InspectionError,
                    prefix,
                    "directory_unreadable",
                );
                continue;
            }
        };
        for entry in read_dir.flatten() {
            let Some(name) = safe_component(&entry.file_name()) else {
                continue;
            };
            let known = matches!(
                (prefix, name.as_str()),
                (
                    "device",
                    crate::server_link::DEVICE_FILE | crate::server_link::PROFILE_FILE
                ) | (_, crate::prefs::PREFERENCES_FILE)
            );
            if known {
                continue;
            }
            let relative = format!("{prefix}/{name}");
            if name.ends_with(".tmp") {
                report.finding(
                    ExternalDiagnosticClass::IncompleteTransaction,
                    relative,
                    "temporary_device_state_object_present",
                );
            } else {
                report.finding(
                    ExternalDiagnosticClass::Unverified,
                    relative,
                    "uninterpreted_device_or_state_object_preserved",
                );
            }
        }
    }
}

fn sibling_path(root: &Path, suffix: &str) -> Option<PathBuf> {
    let mut name = root.file_name()?.to_os_string();
    name.push(suffix);
    Some(root.with_file_name(name))
}

fn scan_adoption(config: &ExternalInspectionConfig, report: &mut ExternalInspectionReport) {
    let Some(workspace) = sibling_path(&config.sync_root, ".adopt") else {
        report.finding(
            ExternalDiagnosticClass::InspectionError,
            "adoption",
            "sync_root_has_no_sibling_namespace",
        );
        return;
    };
    let Some(staged_library) = sibling_path(&config.library_root, ".adopt") else {
        report.finding(
            ExternalDiagnosticClass::InspectionError,
            "adoption",
            "library_root_has_no_sibling_namespace",
        );
        return;
    };
    let workspace_present = workspace.exists();
    let staged_present = staged_library.exists();
    if !workspace_present && !staged_present {
        return;
    }
    let plan_path = workspace.join(crate::sync_adopt::PLAN_FILE);
    let plan = match read_json_typed::<AdoptPlanV1Inspection>(
        &plan_path,
        "adoption/plan.json",
        MAX_METADATA_BYTES,
        &[1],
        report,
    ) {
        JsonRead::Missing => {
            report.finding(
                ExternalDiagnosticClass::IncompleteTransaction,
                "adoption",
                "uncommitted_adoption_staging_preserved",
            );
            None
        }
        JsonRead::Invalid => None,
        JsonRead::Parsed(plan) => {
            if plan.v != 1 || uuid::Uuid::try_parse(&plan.adoption_id).is_err() {
                report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    "adoption/plan.json",
                    "invalid_adoption_plan",
                );
                None
            } else {
                report.finding(
                    ExternalDiagnosticClass::IncompleteTransaction,
                    "adoption/plan.json",
                    "committed_adoption_awaiting_recovery",
                );
                Some(plan)
            }
        }
    };
    if let Some(plan) = plan {
        let staged_epoch = scan_epoch(
            &staged_library,
            &workspace.join("sync"),
            "adoption/library",
            "adoption/sync",
            report,
        );
        if staged_epoch.library != Some(plan.library_id)
            || staged_epoch.replica != Some(plan.replica_id)
            || staged_epoch.server != Some(plan.server_id)
            || staged_epoch
                .state
                .as_ref()
                .is_none_or(|state| state.genesis_sha256 != plan.genesis_sha256)
        {
            report.finding(
                ExternalDiagnosticClass::Corrupt,
                "adoption/plan.json",
                "adoption_plan_and_staged_epoch_mismatch",
            );
        }
        let staged_profile = workspace.join(crate::server_link::PROFILE_FILE);
        if plan.profile {
            match read_json_typed::<crate::server_link::ServerProfileV1>(
                &staged_profile,
                "adoption/server-profile.json",
                MAX_METADATA_BYTES,
                &[1],
                report,
            ) {
                JsonRead::Parsed(profile) if profile.server_id == plan.server_id => report.finding(
                    ExternalDiagnosticClass::Unverified,
                    "adoption/server-profile.json",
                    "staged_pairing_credential_not_offline_verified",
                ),
                JsonRead::Parsed(_) => report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    "adoption/server-profile.json",
                    "staged_profile_server_identity_mismatch",
                ),
                JsonRead::Missing => report.finding(
                    ExternalDiagnosticClass::MissingRequired,
                    "adoption/server-profile.json",
                    "staged_profile_missing",
                ),
                JsonRead::Invalid => {}
            }
        }
    }
    if let Ok(read_dir) = fs::read_dir(&workspace) {
        for entry in read_dir.flatten() {
            let Some(name) = safe_component(&entry.file_name()) else {
                continue;
            };
            if matches!(
                name.as_str(),
                crate::sync_adopt::PLAN_FILE | crate::server_link::PROFILE_FILE | "sync"
            ) {
                continue;
            }
            if name.ends_with(".tmp") {
                report.finding(
                    ExternalDiagnosticClass::IncompleteTransaction,
                    format!("adoption/{name}"),
                    "temporary_adoption_object_preserved",
                );
            } else {
                report.finding(
                    ExternalDiagnosticClass::Unverified,
                    format!("adoption/{name}"),
                    "unknown_adoption_object_preserved",
                );
            }
        }
    }
}

fn scan_retired(config: &ExternalInspectionConfig, report: &mut ExternalInspectionReport) {
    let Ok(sync_siblings) = sibling_names(&config.sync_root, "sync") else {
        return;
    };
    let Ok(library_siblings) = sibling_names(&config.library_root, "library") else {
        return;
    };
    let library_by_suffix = library_siblings
        .iter()
        .filter_map(|scope| {
            scope
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.split_once(".retired-").map(|(_, suffix)| suffix))
                .map(|suffix| (suffix.to_owned(), scope.path.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    for scope in sync_siblings {
        let Some(name) = scope.path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((_, suffix)) = name.split_once(".retired-") else {
            continue;
        };
        report.counts.retired_epochs = report.counts.retired_epochs.saturating_add(1);
        let prefix = format!("retired/{suffix}/sync");
        if let Some(library) = library_by_suffix.get(suffix) {
            let library_prefix = format!("retired/{suffix}/library");
            let _ = scan_epoch(library, &scope.path, &library_prefix, &prefix, report);
            report.finding(
                ExternalDiagnosticClass::Unverified,
                format!("retired/{suffix}"),
                "retired_epoch_preserved_not_disposable",
            );
        } else {
            report.finding(
                ExternalDiagnosticClass::IncompleteTransaction,
                prefix,
                "retired_sync_epoch_has_no_matching_library",
            );
        }
    }
    for (suffix, _) in library_by_suffix {
        let sync_name = config
            .sync_root
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| format!("{name}.retired-{suffix}"));
        let has_sync = sync_name.is_some_and(|wanted| {
            sibling_names(&config.sync_root, "sync")
                .unwrap_or_default()
                .iter()
                .any(|scope| scope.path.file_name().and_then(|name| name.to_str()) == Some(&wanted))
        });
        if !has_sync {
            report.finding(
                ExternalDiagnosticClass::IncompleteTransaction,
                format!("retired/{suffix}/library"),
                "retired_library_has_no_matching_sync_epoch",
            );
        }
    }
}

fn scan_sync_root_entries(sync_root: &Path, prefix: &str, report: &mut ExternalInspectionReport) {
    let read_dir = match fs::read_dir(sync_root) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            report.finding(
                ExternalDiagnosticClass::InspectionError,
                prefix,
                "directory_unreadable",
            );
            return;
        }
    };
    let known = BTreeSet::from([
        crate::sync_outbound::CONFIG_FILE,
        crate::sync_outbound::STATE_FILE,
        crate::sync_outbound::GENESIS_FILE,
        crate::sync_outbound::BASE_FILE,
        crate::sync_outbound::OUTBOX_FILE,
        crate::sync_outbound::BLOB_DIR,
        crate::sync_outbound::LOCK_FILE,
        sync_protocol::REPLICA_STATE_FILE,
        crate::sync_local::REPLICA_LOCK_FILE,
        crate::sync_outbound::HISTORY_FILE,
        crate::sync_outbound::INBOX_FILE,
        crate::sync_outbound::APPLY_FILE,
        crate::sync_outbound::confirmed::CONFIRMED_LOG_FILE,
        crate::sync_outbound::confirmed::CHECKPOINT_DIR,
        crate::sync_verified::STAMP_FILE,
        crate::sync_verified::STAMP_CLEARED_FILE,
        crate::verified_baseline::BASELINE_FILE,
        crate::sync_verified::RECOVERY_INTENT_FILE,
        crate::sync_recovery_exec::RECOVERY_COMPLETE_FILE,
        crate::prefs::PREFERENCES_FILE,
        "recovery-intent-v1.claim",
    ]);
    for entry in read_dir.flatten() {
        let Some(name) = safe_component(&entry.file_name()) else {
            continue;
        };
        if known.contains(name.as_str()) {
            continue;
        }
        let relative = format!("{prefix}/{name}");
        if name.ends_with(".tmp") {
            report.finding(
                ExternalDiagnosticClass::IncompleteTransaction,
                relative,
                "temporary_sync_object_present",
            );
        } else {
            report.finding(
                ExternalDiagnosticClass::Unverified,
                relative,
                "unknown_sync_object_preserved",
            );
        }
    }
}

/// Inspect external sync and device state without taking locks or invoking normal startup.
pub fn inspect_external(config: &ExternalInspectionConfig) -> ExternalInspectionReport {
    inspect_external_with_hook(config, || {})
}

fn inspect_external_with_hook(
    config: &ExternalInspectionConfig,
    after_scan: impl FnOnce(),
) -> ExternalInspectionReport {
    let mut report = ExternalInspectionReport::default();
    let (before, before_issues) = collect_scope(config);
    record_census_issues(&mut report, before_issues);
    // A failed census includes a symlink/hard-link/special-file/path-limit refusal. Do not then
    // enter semantic readers: that could follow a rejected namespace or turn a partial tree into
    // a misleading set of successful sub-checks.
    if before.complete {
        let active = scan_epoch(
            &config.library_root,
            &config.sync_root,
            "library",
            "sync",
            &mut report,
        );
        scan_sync_root_entries(&config.sync_root, "sync", &mut report);
        scan_device(config, &active, &mut report);
        scan_adoption(config, &mut report);
        scan_retired(config, &mut report);
        let maintenance = config
            .library_root
            .join(crate::sync_local::SYNC_DIR)
            .join(crate::sync_verified::RECOVERY_MAINTENANCE_FILE);
        if maintenance.exists() {
            match read_bounded(&maintenance, 16) {
                Ok(bytes) if bytes == b"1\n" => report.finding(
                    ExternalDiagnosticClass::IncompleteTransaction,
                    "library/.scribe-sync/recovery-maintenance",
                    "recovery_maintenance_hold_active",
                ),
                Ok(_) => report.finding(
                    ExternalDiagnosticClass::Corrupt,
                    "library/.scribe-sync/recovery-maintenance",
                    "invalid_recovery_maintenance_marker",
                ),
                Err(reason) => report.finding(
                    ExternalDiagnosticClass::InspectionError,
                    "library/.scribe-sync/recovery-maintenance",
                    reason,
                ),
            }
        }
    }
    after_scan();
    let (after, after_issues) = collect_scope(config);
    record_census_issues(&mut report, after_issues);
    if before != after {
        report.finding(
            ExternalDiagnosticClass::InspectionError,
            ".",
            "state_changed_during_inspection",
        );
    }
    if !before.complete || !after.complete {
        report.completed = false;
    }
    report.counts.namespaces = before
        .entries
        .values()
        .filter(|entry| entry.kind == "directory")
        .count() as u64;
    report.counts.files = before
        .entries
        .values()
        .filter(|entry| entry.kind == "file")
        .count() as u64;
    report.counts.bytes_examined = before
        .entries
        .values()
        .filter(|entry| entry.kind == "file")
        .map(|entry| entry.size)
        .sum();
    report.finish();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use sync_engine::testkit;
    use sync_protocol::{IdentityFileV1, SyncOperationV1};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct Fixture {
        root: PathBuf,
        config: ExternalInspectionConfig,
        library: LibraryId,
        replica: ReplicaId,
        genesis: SyncSnapshotV1,
    }

    impl Fixture {
        fn fresh() -> Self {
            let root = std::env::temp_dir().join(format!(
                "scribe-external-inspect-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            let config = ExternalInspectionConfig {
                library_root: root.join("library"),
                sync_root: root.join("sync"),
                device_root: root.join("device"),
                state_root: root.join("state"),
            };
            fs::create_dir_all(&config.library_root).unwrap();
            write_json(
                &config
                    .library_root
                    .join(crate::library::LIBRARY_MANIFEST_FILE),
                &crate::library::PersistedLibraryManifestV1 {
                    v: crate::library::LIBRARY_MANIFEST_VERSION,
                    notebook_ids: vec!["notebook-1".into()],
                    active_notebook_id: "notebook-1".into(),
                    next_notebook_ordinal: 2,
                },
            );
            Self {
                root,
                config,
                library: testkit::library(),
                replica: testkit::replica(1),
                genesis: testkit::genesis(),
            }
        }

        fn epoch(version: u32) -> Self {
            let fixture = Self::fresh();
            fs::create_dir_all(
                fixture
                    .config
                    .library_root
                    .join(crate::sync_local::SYNC_DIR),
            )
            .unwrap();
            fs::create_dir_all(
                fixture
                    .config
                    .sync_root
                    .join(crate::sync_outbound::BLOB_DIR),
            )
            .unwrap();
            fs::create_dir_all(&fixture.config.device_root).unwrap();
            fs::create_dir_all(&fixture.config.state_root).unwrap();
            let identity = IdentityFileV1 {
                v: 1,
                library_id: fixture.library,
                notebooks: BTreeMap::new(),
                pages: BTreeMap::new(),
                retired_notebooks: BTreeMap::new(),
                retired_pages: BTreeMap::new(),
            };
            write_json(
                &fixture
                    .config
                    .library_root
                    .join(crate::sync_local::SYNC_DIR)
                    .join(crate::sync_local::IDENTITY_FILE),
                &identity,
            );
            write_json(
                &fixture
                    .config
                    .sync_root
                    .join(sync_protocol::REPLICA_STATE_FILE),
                &ReplicaStateV1::new(fixture.replica),
            );
            write_json(
                &fixture
                    .config
                    .device_root
                    .join(crate::server_link::DEVICE_FILE),
                &serde_json::json!({"v":1,"client_id":uuid::Uuid::now_v7().to_string()}),
            );
            let genesis_bytes = fixture.genesis.to_canonical_bytes().unwrap();
            fs::write(
                fixture
                    .config
                    .sync_root
                    .join(crate::sync_outbound::GENESIS_FILE),
                &genesis_bytes,
            )
            .unwrap();
            let state = crate::sync_outbound::SyncStateV1 {
                v: version,
                library_id: fixture.library,
                replica_id: fixture.replica,
                genesis_sha256: sha256_hex(&genesis_bytes),
                genesis_uploaded: false,
                acked_client_seq: 0,
                next_client_seq: 1,
                server_base_url: None,
                server_cursor: 0,
                server_id: None,
            };
            fixture.write_state(&state);
            let (blob, reference) = testkit::blob_background();
            let BackgroundRef::Blob { sha256, .. } = reference else {
                unreachable!()
            };
            fs::write(
                fixture
                    .config
                    .sync_root
                    .join(crate::sync_outbound::BLOB_DIR)
                    .join(sha256.as_str()),
                blob,
            )
            .unwrap();
            if version == 2 {
                let base = LibraryState::genesis(fixture.genesis.clone()).unwrap();
                write_json(
                    &fixture
                        .config
                        .sync_root
                        .join(crate::sync_outbound::BASE_FILE),
                    &BaseStateV2Inspection {
                        v: 2,
                        through_server_seq: 0,
                        snapshot: base.snapshot,
                        history: base.history,
                    },
                );
            }
            fixture
        }

        fn state(&self) -> crate::sync_outbound::SyncStateV1 {
            serde_json::from_slice(
                &fs::read(self.config.sync_root.join(crate::sync_outbound::STATE_FILE)).unwrap(),
            )
            .unwrap()
        }

        fn write_state(&self, state: &crate::sync_outbound::SyncStateV1) {
            write_json(
                &self.config.sync_root.join(crate::sync_outbound::STATE_FILE),
                state,
            );
        }

        fn envelope(&self, seq: u64) -> SyncEnvelopeV1 {
            let mut author = testkit::Author::new(self.replica);
            author.next_seq = seq;
            author.env(SyncOperationV1::RenameNotebook {
                notebook_id: testkit::nb(1),
                title: "pending".into(),
            })
        }

        fn manifest(&self) -> BTreeMap<String, (u64, String)> {
            fn walk(root: &Path, at: &Path, output: &mut BTreeMap<String, (u64, String)>) {
                if !at.exists() {
                    return;
                }
                let mut entries = fs::read_dir(at)
                    .unwrap()
                    .map(Result::unwrap)
                    .collect::<Vec<_>>();
                entries.sort_by_key(|entry| entry.file_name());
                for entry in entries {
                    let path = entry.path();
                    if path.is_dir() {
                        walk(root, &path, output);
                    } else {
                        let bytes = fs::read(&path).unwrap();
                        output.insert(
                            path.strip_prefix(root)
                                .unwrap()
                                .to_string_lossy()
                                .replace('\\', "/"),
                            (bytes.len() as u64, format!("{:x}", Sha256::digest(&bytes))),
                        );
                    }
                }
            }
            let mut output = BTreeMap::new();
            walk(&self.root, &self.root, &mut output);
            output
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn write_json(path: &Path, value: &impl Serialize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = serde_json::to_vec(value).unwrap();
        bytes.push(b'\n');
        fs::write(path, bytes).unwrap();
    }

    fn has(report: &ExternalInspectionReport, reason: &str) -> bool {
        report
            .findings()
            .iter()
            .any(|finding| finding.reason == reason)
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TestJsonlRecord {
        v: u32,
        payload: String,
    }

    #[test]
    fn command_contract_is_complete_and_unambiguous() {
        let valid = [
            "--inspect-storage-external",
            "--data-dir",
            "library",
            "--sync-dir",
            "sync",
            "--device-dir",
            "device",
            "--state-dir",
            "state",
        ]
        .map(str::to_owned);
        assert!(matches!(
            parse_command(&valid),
            Ok(InspectionCommand::Inspect(_))
        ));
        assert!(matches!(
            parse_command(&[]),
            Ok(InspectionCommand::NotRequested)
        ));
        for invalid in [
            valid[..8].to_vec(),
            [valid.clone(), valid.clone()].concat(),
            vec![
                "--inspect-storage-external".into(),
                "--data-dir".into(),
                "x".into(),
            ],
        ] {
            assert!(parse_command(&invalid).is_err());
        }
    }

    #[test]
    fn jsonl_reader_bounds_each_line_and_cumulative_retained_records() {
        let fixture = Fixture::fresh();
        let path = fixture.root.join("bounded.jsonl");
        let limits = JsonlLimits {
            max_file: 4 * 1024,
            max_record: 128,
            max_records: 16,
            max_retained: 512,
        };

        let mut oversized = vec![b'x'; 129];
        oversized.push(b'\n');
        fs::write(&path, &oversized).unwrap();
        let before = fs::read(&path).unwrap();
        let error =
            strict_jsonl_visit_with_limits::<TestJsonlRecord, _>(&path, limits, true, |_| Ok(()))
                .unwrap_err();
        assert_eq!(error.reason, "record_size_limit_exceeded");
        assert_eq!(before, fs::read(&path).unwrap());

        oversized.pop();
        fs::write(&path, &oversized).unwrap();
        let before = fs::read(&path).unwrap();
        let error =
            strict_jsonl_visit_with_limits::<TestJsonlRecord, _>(&path, limits, true, |_| Ok(()))
                .unwrap_err();
        assert_eq!(error.reason, "record_size_limit_exceeded");
        assert_eq!(before, fs::read(&path).unwrap());

        let record = TestJsonlRecord {
            v: 1,
            payload: "p".repeat(80),
        };
        let mut one = serde_json::to_vec(&record).unwrap();
        assert!(one.len() < limits.max_record);
        one.push(b'\n');
        fs::write(&path, &one).unwrap();
        let mut visited = 0usize;
        let summary =
            strict_jsonl_visit_with_limits::<TestJsonlRecord, _>(&path, limits, true, |parsed| {
                assert_eq!(parsed, record);
                visited += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(visited, 1);
        assert_eq!(summary.records, 1);
        assert!(!summary.torn_tail);

        let multiple = one.repeat(3);
        fs::write(&path, &multiple).unwrap();
        let summary =
            strict_jsonl_visit_with_limits::<TestJsonlRecord, _>(&path, limits, true, |_| Ok(()))
                .unwrap();
        assert_eq!(summary.records, 3);

        let aggregate_limits = JsonlLimits {
            max_retained: (one.len() as u64).saturating_add(8),
            ..limits
        };
        let before = fs::read(&path).unwrap();
        let error = strict_jsonl_visit_with_limits::<TestJsonlRecord, _>(
            &path,
            aggregate_limits,
            true,
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.reason, "retained_record_bytes_limit_exceeded");
        assert_eq!(before, fs::read(&path).unwrap());

        let mut torn = one.clone();
        torn.extend_from_slice(b"{\"v\":1");
        fs::write(&path, &torn).unwrap();
        let before = fs::read(&path).unwrap();
        let summary =
            strict_jsonl_visit_with_limits::<TestJsonlRecord, _>(&path, limits, true, |_| Ok(()))
                .unwrap();
        assert_eq!(summary.records, 1);
        assert!(summary.torn_tail);
        assert_eq!(before, fs::read(&path).unwrap());
    }

    #[test]
    fn scanner_refuses_oversized_outbox_lines_without_changing_bytes() {
        for terminated in [true, false] {
            let fixture = Fixture::epoch(1);
            let path = fixture
                .config
                .sync_root
                .join(crate::sync_outbound::OUTBOX_FILE);
            let mut bytes = vec![b'x'; MAX_RECORD_BYTES as usize + 1];
            if terminated {
                bytes.push(b'\n');
            }
            fs::write(&path, &bytes).unwrap();
            let before = fixture.manifest();
            let report = inspect_external(&fixture.config);
            assert!(has(&report, "record_size_limit_exceeded"));
            assert!(!report.completed);
            assert_eq!(before, fixture.manifest());
        }
    }

    #[test]
    fn fresh_unpaired_state_is_stable_and_nonmutating() {
        let fixture = Fixture::fresh();
        let before = fixture.manifest();
        let first = inspect_external(&fixture.config);
        let second = inspect_external(&fixture.config);
        assert_eq!(
            first.canonical_bytes().unwrap(),
            second.canonical_bytes().unwrap()
        );
        assert_eq!(first.result_token(), "VALID");
        assert_eq!(before, fixture.manifest());
    }

    #[test]
    fn required_library_root_is_never_inferred_from_absent_optional_state() {
        let all_missing = Fixture::fresh();
        let missing_config = ExternalInspectionConfig {
            library_root: all_missing.root.join("missing-library"),
            sync_root: all_missing.root.join("missing-sync"),
            device_root: all_missing.root.join("missing-device"),
            state_root: all_missing.root.join("missing-state"),
        };
        let report = inspect_external(&missing_config);
        assert!(has(&report, "required_library_root_missing"));
        assert_eq!(report.result_token(), "ISSUES_DETECTED");
        assert_eq!(report.exit_code(), 2);

        let fresh = Fixture::fresh();
        let fresh_report = inspect_external(&fresh.config);
        assert_eq!(fresh_report.result_token(), "VALID");
        assert!(!has(&fresh_report, "required_library_root_missing"));

        let unpaired = Fixture::epoch(1);
        let unpaired_report = inspect_external(&unpaired.config);
        assert!(!has(
            &unpaired_report,
            "server_bound_epoch_has_no_pairing_profile"
        ));

        let paired = Fixture::epoch(1);
        let mut paired_state = paired.state();
        paired_state.server_id = Some(ServerId::from_uuid(uuid::Uuid::now_v7()).unwrap());
        paired.write_state(&paired_state);
        assert!(has(
            &inspect_external(&paired.config),
            "server_bound_epoch_has_no_pairing_profile"
        ));

        let wrong = Fixture::fresh();
        let incorrect = wrong.root.join("incorrect-library");
        fs::create_dir_all(&incorrect).unwrap();
        let mut wrong_config = wrong.config.clone();
        wrong_config.library_root = incorrect;
        assert!(has(
            &inspect_external(&wrong_config),
            "required_library_manifest_missing"
        ));

        let invalid = Fixture::fresh();
        let invalid_path = invalid.root.join("library-is-a-file");
        fs::write(&invalid_path, b"not a directory").unwrap();
        let mut invalid_config = invalid.config.clone();
        invalid_config.library_root = invalid_path;
        let invalid_report = inspect_external(&invalid_config);
        assert!(has(&invalid_report, "library_root_not_directory"));
        assert!(!invalid_report.completed);
        assert_eq!(invalid_report.exit_code(), 3);
    }

    #[test]
    fn supported_v1_and_v2_epochs_are_structurally_inspected() {
        let v1 = Fixture::epoch(1);
        let v1_report = inspect_external(&v1.config);
        assert!(has(
            &v1_report,
            "historical_v1_epoch_awaiting_supported_upgrade"
        ));
        assert_eq!(v1_report.result_token(), "VALID_PENDING");

        let v2 = Fixture::epoch(2);
        let v2_report = inspect_external(&v2.config);
        assert!(has(
            &v2_report,
            "legacy_base_awaiting_checkpoint_conversion"
        ));
        assert!(!has(&v2_report, "confirmed_base_missing_for_v2_epoch"));
    }

    #[test]
    fn future_duplicate_and_malformed_state_fail_closed() {
        let future = Fixture::epoch(1);
        let mut state = future.state();
        state.v = 99;
        future.write_state(&state);
        assert!(has(
            &inspect_external(&future.config),
            "unsupported_record_version"
        ));

        let duplicate = Fixture::epoch(1);
        fs::write(
            duplicate
                .config
                .sync_root
                .join(crate::sync_outbound::STATE_FILE),
            b"{\"v\":1,\"v\":1}\n",
        )
        .unwrap();
        assert!(has(
            &inspect_external(&duplicate.config),
            "duplicate_json_key"
        ));

        let malformed = Fixture::epoch(1);
        fs::write(
            malformed
                .config
                .sync_root
                .join(crate::sync_outbound::STATE_FILE),
            b"{",
        )
        .unwrap();
        assert!(has(&inspect_external(&malformed.config), "invalid_json"));
    }

    #[test]
    fn unknown_fields_and_historical_journal_encodings_never_look_clean() {
        let unknown_top = Fixture::epoch(1);
        let state_path = unknown_top
            .config
            .sync_root
            .join(crate::sync_outbound::STATE_FILE);
        let mut state_value: Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        state_value
            .as_object_mut()
            .unwrap()
            .insert("future".into(), Value::Bool(true));
        write_json(&state_path, &state_value);
        let report = inspect_external(&unknown_top.config);
        assert!(report.findings().iter().any(|finding| {
            finding.class == ExternalDiagnosticClass::UnsupportedFormat
                && finding.reason == "unknown_or_invalid_record_fields"
        }));

        let unknown_nested = Fixture::epoch(1);
        let genesis_path = unknown_nested
            .config
            .sync_root
            .join(crate::sync_outbound::GENESIS_FILE);
        let mut genesis: Value = serde_json::from_slice(&fs::read(&genesis_path).unwrap()).unwrap();
        genesis["notebooks"][0]["pages"][0]
            .as_object_mut()
            .unwrap()
            .insert("future".into(), Value::Bool(true));
        let genesis_bytes = serde_json::to_vec(&genesis).unwrap();
        fs::write(&genesis_path, &genesis_bytes).unwrap();
        let mut state = unknown_nested.state();
        state.genesis_sha256 = sha256_hex(&genesis_bytes);
        unknown_nested.write_state(&state);
        let report = inspect_external(&unknown_nested.config);
        assert!(report.findings().iter().any(|finding| {
            finding.class == ExternalDiagnosticClass::UnsupportedFormat
                && finding.reason == "unrecognized_genesis_fields_or_variant"
        }));

        let encodings = Fixture::epoch(1);
        let envelope =
            String::from_utf8(encodings.envelope(1).to_canonical_bytes().unwrap()).unwrap();
        let record = InboxRecordV1Inspection {
            v: 1,
            server_seq: 1,
            envelope,
        };
        let mut bytes = b"\n".to_vec();
        bytes.extend_from_slice(&serde_json::to_vec(&record).unwrap());
        bytes.extend_from_slice(b"\r\n");
        fs::write(
            encodings
                .config
                .sync_root
                .join(crate::sync_outbound::INBOX_FILE),
            bytes,
        )
        .unwrap();
        let report = inspect_external(&encodings.config);
        assert!(has(
            &report,
            "empty_journal_records_ignored_by_production_reader"
        ));
        assert!(has(
            &report,
            "crlf_journal_records_supported_but_noncanonical"
        ));
        assert_eq!(report.exit_code(), 2);

        let duplicate = Fixture::epoch(1);
        fs::write(
            duplicate
                .config
                .sync_root
                .join(crate::sync_outbound::OUTBOX_FILE),
            b"{\"v\":1,\"v\":1,\"kind\":\"commit\",\"seq\":1}\n",
        )
        .unwrap();
        assert!(has(
            &inspect_external(&duplicate.config),
            "duplicate_json_key"
        ));

        let future = Fixture::epoch(1);
        write_json(
            &future
                .config
                .sync_root
                .join(crate::sync_outbound::OUTBOX_FILE),
            &serde_json::json!({"v":99,"kind":"commit","seq":1}),
        );
        assert!(has(
            &inspect_external(&future.config),
            "unsupported_outbox_record_version"
        ));

        let nested_envelope = Fixture::epoch(1);
        let mut envelope: Value =
            serde_json::from_slice(&nested_envelope.envelope(1).to_canonical_bytes().unwrap())
                .unwrap();
        envelope
            .as_object_mut()
            .unwrap()
            .insert("future".into(), Value::Bool(true));
        let record = crate::sync_outbound::OutboxRecordV1 {
            v: 1,
            kind: crate::sync_outbound::RecordKind::Op,
            seq: 1,
            envelope: Some(serde_json::to_string(&envelope).unwrap()),
        };
        write_json(
            &nested_envelope
                .config
                .sync_root
                .join(crate::sync_outbound::OUTBOX_FILE),
            &record,
        );
        assert!(has(
            &inspect_external(&nested_envelope.config),
            "unrecognized_envelope_fields_or_variant"
        ));
    }

    #[test]
    fn identity_replica_genesis_and_cursor_contradictions_are_detected() {
        let identity = Fixture::epoch(1);
        let mut state = identity.state();
        state.library_id = LibraryId::from_uuid(uuid::Uuid::now_v7()).unwrap();
        identity.write_state(&state);
        assert!(has(
            &inspect_external(&identity.config),
            "sync_and_library_identity_mismatch"
        ));

        let replica = Fixture::epoch(1);
        fs::write(
            replica
                .config
                .sync_root
                .join(sync_protocol::REPLICA_STATE_FILE),
            b"{}",
        )
        .unwrap();
        assert!(has(
            &inspect_external(&replica.config),
            "missing_or_invalid_version"
        ));

        let genesis = Fixture::epoch(1);
        fs::write(
            genesis
                .config
                .sync_root
                .join(crate::sync_outbound::GENESIS_FILE),
            b"{}",
        )
        .unwrap();
        assert!(has(
            &inspect_external(&genesis.config),
            "invalid_genesis_snapshot"
        ));

        let sequences = Fixture::epoch(1);
        let mut state = sequences.state();
        state.next_client_seq = 0;
        sequences.write_state(&state);
        assert!(has(
            &inspect_external(&sequences.config),
            "invalid_persisted_client_sequence"
        ));
    }

    #[test]
    fn outbound_pending_prep_torn_and_unknown_records_are_distinguished() {
        let pending = Fixture::epoch(1);
        let envelope = pending.envelope(1).to_canonical_bytes().unwrap();
        let record = serde_json::json!({
            "v": 1,
            "kind": "op",
            "seq": 1,
            "envelope": String::from_utf8(envelope).unwrap()
        });
        write_json(
            &pending
                .config
                .sync_root
                .join(crate::sync_outbound::OUTBOX_FILE),
            &record,
        );
        let mut state = pending.state();
        state.next_client_seq = 2;
        pending.write_state(&state);
        assert!(has(
            &inspect_external(&pending.config),
            "outbound_operations_awaiting_confirmation"
        ));

        let prep = Fixture::epoch(1);
        let envelope = prep.envelope(1).to_canonical_bytes().unwrap();
        write_json(
            &prep
                .config
                .sync_root
                .join(crate::sync_outbound::OUTBOX_FILE),
            &serde_json::json!({"v":1,"kind":"prep","seq":1,"envelope":String::from_utf8(envelope).unwrap()}),
        );
        assert!(has(
            &inspect_external(&prep.config),
            "outbox_prepare_without_terminal_record"
        ));

        let torn = Fixture::epoch(1);
        fs::write(
            torn.config
                .sync_root
                .join(crate::sync_outbound::OUTBOX_FILE),
            b"partial",
        )
        .unwrap();
        assert!(has(
            &inspect_external(&torn.config),
            "torn_outbox_tail_preserved"
        ));

        let unknown = Fixture::epoch(1);
        write_json(
            &unknown
                .config
                .sync_root
                .join(crate::sync_outbound::OUTBOX_FILE),
            &serde_json::json!({"v":1,"kind":"future","seq":1}),
        );
        assert!(has(
            &inspect_external(&unknown.config),
            "unrecognized_record_fields_or_variant"
        ));
    }

    #[test]
    fn bounded_outbox_scan_uses_the_source_transaction_state_machine() {
        let fixture = Fixture::epoch(1);
        let envelope =
            String::from_utf8(fixture.envelope(1).to_canonical_bytes().unwrap()).unwrap();
        let records = [
            crate::sync_outbound::OutboxRecordV1 {
                v: 1,
                kind: crate::sync_outbound::RecordKind::Prep,
                seq: 1,
                envelope: Some(envelope),
            },
            crate::sync_outbound::OutboxRecordV1 {
                v: 1,
                kind: crate::sync_outbound::RecordKind::Commit,
                seq: 1,
                envelope: None,
            },
        ];
        let mut bytes = Vec::new();
        for record in &records {
            bytes.extend_from_slice(&serde_json::to_vec(record).unwrap());
            bytes.push(b'\n');
        }
        let expected = crate::sync_outbound::parse_journal(&bytes).unwrap();
        let path = fixture
            .config
            .sync_root
            .join(crate::sync_outbound::OUTBOX_FILE);
        fs::write(&path, &bytes).unwrap();
        let mut parser = crate::sync_outbound::JournalParser::default();
        let summary = strict_jsonl_visit::<crate::sync_outbound::OutboxRecordV1, _>(
            &path,
            MAX_JOURNAL_BYTES,
            true,
            |record| {
                parser.push(record).map_err(|_| JsonlFailure {
                    class: ExternalDiagnosticClass::Corrupt,
                    reason: "invalid_outbox_transaction_sequence",
                })
            },
        )
        .unwrap();
        let actual = parser.finish(summary.torn_tail);
        assert_eq!(actual.committed, expected.committed);
        assert_eq!(actual.trailing_prep, expected.trailing_prep);
        assert_eq!(actual.torn_tail, expected.torn_tail);
        assert_eq!(bytes, fs::read(path).unwrap());
    }

    #[test]
    fn inbound_pending_torn_apply_and_accepted_states_are_preserved() {
        let inbound = Fixture::epoch(1);
        let envelope = inbound.envelope(1);
        write_json(
            &inbound
                .config
                .sync_root
                .join(crate::sync_outbound::INBOX_FILE),
            &serde_json::json!({"v":1,"server_seq":1,"envelope":String::from_utf8(envelope.to_canonical_bytes().unwrap()).unwrap()}),
        );
        assert!(has(
            &inspect_external(&inbound.config),
            "inbound_operations_awaiting_materialization"
        ));

        let accepted = Fixture::epoch(1);
        let envelope = accepted.envelope(1);
        write_json(
            &accepted
                .config
                .sync_root
                .join(crate::sync_outbound::HISTORY_FILE),
            &serde_json::json!({"v":1,"client_seq":1,"server_seq":1,"operation_id":envelope.operation_id,"envelope":String::from_utf8(envelope.to_canonical_bytes().unwrap()).unwrap()}),
        );
        assert!(has(
            &inspect_external(&accepted.config),
            "accepted_operations_awaiting_materialization"
        ));

        let apply = Fixture::epoch(1);
        write_json(
            &apply
                .config
                .sync_root
                .join(crate::sync_outbound::APPLY_FILE),
            &serde_json::json!({"v":1,"target_cursor":1}),
        );
        assert!(has(
            &inspect_external(&apply.config),
            "inbound_apply_interrupted"
        ));

        let torn = Fixture::epoch(1);
        fs::write(
            torn.config.sync_root.join(crate::sync_outbound::INBOX_FILE),
            b"partial",
        )
        .unwrap();
        assert!(has(
            &inspect_external(&torn.config),
            "torn_inbox_tail_preserved"
        ));
    }

    #[test]
    fn confirmed_checkpoint_log_and_blob_relationships_are_verified() {
        let fixture = Fixture::epoch(2);
        fs::remove_file(
            fixture
                .config
                .sync_root
                .join(crate::sync_outbound::BASE_FILE),
        )
        .unwrap();
        let base = LibraryState::genesis(fixture.genesis.clone()).unwrap();
        crate::sync_outbound::confirmed::write_checkpoint(
            &fixture.config.sync_root,
            &crate::sync_outbound::confirmed::CheckpointCapture::borrow_all(&base, 0),
            &|_| Ok(()),
        )
        .unwrap();
        let envelope = fixture.envelope(1).to_canonical_bytes().unwrap();
        fs::write(
            fixture
                .config
                .sync_root
                .join(crate::sync_outbound::confirmed::CONFIRMED_LOG_FILE),
            crate::sync_outbound::confirmed::frame(1, &envelope),
        )
        .unwrap();
        let mut state = fixture.state();
        state.server_cursor = 1;
        fixture.write_state(&state);
        let report = inspect_external(&fixture.config);
        assert!(!has(&report, "invalid_confirmed_log"));
        assert!(!has(&report, "confirmed_state_behind_persisted_cursor"));

        let missing_blob = Fixture::epoch(1);
        let (blob, reference) = testkit::blob_background();
        let BackgroundRef::Blob { sha256, .. } = reference else {
            unreachable!()
        };
        assert_eq!(sha256_hex(&blob), sha256);
        fs::remove_file(
            missing_blob
                .config
                .sync_root
                .join(crate::sync_outbound::BLOB_DIR)
                .join(sha256.as_str()),
        )
        .unwrap();
        assert!(has(
            &inspect_external(&missing_blob.config),
            "referenced_blob_missing"
        ));
    }

    #[test]
    fn blob_constraints_are_merged_without_last_writer_wins() {
        fn add_outbox_page(fixture: &Fixture, background: BackgroundRef) {
            let render = fixture.genesis.notebooks[0].pages[0].render.clone();
            let mut author = testkit::Author::new(fixture.replica);
            let envelope = author.env(SyncOperationV1::CreatePage {
                notebook_id: testkit::nb(1),
                page: sync_protocol::PageSpec {
                    page_id: testkit::pg(90),
                    background,
                    render,
                },
                after_page_id: None,
            });
            write_json(
                &fixture
                    .config
                    .sync_root
                    .join(crate::sync_outbound::OUTBOX_FILE),
                &crate::sync_outbound::OutboxRecordV1 {
                    v: 1,
                    kind: crate::sync_outbound::RecordKind::Op,
                    seq: 1,
                    envelope: Some(
                        String::from_utf8(envelope.to_canonical_bytes().unwrap()).unwrap(),
                    ),
                },
            );
            let mut state = fixture.state();
            state.next_client_seq = 2;
            fixture.write_state(&state);
        }

        let matching = Fixture::epoch(1);
        let (_, background) = testkit::blob_background();
        add_outbox_page(&matching, background.clone());
        assert!(!has(
            &inspect_external(&matching.config),
            "conflicting_blob_byte_count_declarations"
        ));

        let conflicting = Fixture::epoch(1);
        let mut conflict_background = background;
        if let BackgroundRef::Blob {
            byte_len, format, ..
        } = &mut conflict_background
        {
            format.height = format.height.saturating_add(1);
            *byte_len = u64::from(format.stride) * u64::from(format.height);
        } else {
            unreachable!();
        }
        add_outbox_page(&conflicting, conflict_background);
        assert!(has(
            &inspect_external(&conflicting.config),
            "conflicting_blob_byte_count_declarations"
        ));

        let direct = Fixture::fresh();
        let blobs_root = direct.config.sync_root.join(crate::sync_outbound::BLOB_DIR);
        fs::create_dir_all(&blobs_root).unwrap();

        let correct = b"correct blob bytes";
        let correct_digest = sha256_hex(correct);
        fs::write(blobs_root.join(correct_digest.as_str()), correct).unwrap();
        let mut wrong_count = BlobReferences::default();
        wrong_count.add(
            &correct_digest,
            correct.len() as u64 + 1,
            "sync",
            &mut ExternalInspectionReport::default(),
        );
        let mut wrong_count_report = ExternalInspectionReport::default();
        scan_blobs(
            &direct.config.sync_root,
            "sync",
            &wrong_count,
            &mut wrong_count_report,
        );
        assert!(has(&wrong_count_report, "blob_byte_count_mismatch"));

        let expected_digest = sha256_hex(b"expected bytes");
        fs::write(
            blobs_root.join(expected_digest.as_str()),
            b"different bytes",
        )
        .unwrap();
        let mut wrong_digest = BlobReferences::default();
        wrong_digest.add(
            &expected_digest,
            b"different bytes".len() as u64,
            "sync",
            &mut ExternalInspectionReport::default(),
        );
        let mut wrong_digest_report = ExternalInspectionReport::default();
        scan_blobs(
            &direct.config.sync_root,
            "sync",
            &wrong_digest,
            &mut wrong_digest_report,
        );
        assert!(has(&wrong_digest_report, "blob_digest_mismatch"));

        let missing_digest = sha256_hex(b"missing bytes");
        let mut missing = BlobReferences::default();
        missing.add(
            &missing_digest,
            b"missing bytes".len() as u64,
            "sync",
            &mut ExternalInspectionReport::default(),
        );
        let mut missing_report = ExternalInspectionReport::default();
        scan_blobs(
            &direct.config.sync_root,
            "sync",
            &missing,
            &mut missing_report,
        );
        assert!(has(&missing_report, "referenced_blob_missing"));
    }

    #[test]
    fn device_pairing_preferences_and_temporary_replacements_are_diagnostic_only() {
        let fixture = Fixture::epoch(1);
        write_json(
            &fixture
                .config
                .device_root
                .join(crate::server_link::DEVICE_FILE),
            &serde_json::json!({"v":1,"client_id":uuid::Uuid::now_v7().to_string()}),
        );
        write_json(
            &fixture
                .config
                .device_root
                .join(crate::server_link::PROFILE_FILE),
            &serde_json::json!({"v":1,"server_id":uuid::Uuid::now_v7().to_string(),"server_name":"server","credential":"secret","last_endpoint":"http://127.0.0.1","paired_ms":1}),
        );
        write_json(
            &fixture
                .config
                .sync_root
                .join(crate::prefs::PREFERENCES_FILE),
            &serde_json::json!({"v":1,"side_button":"navigation_undo"}),
        );
        fs::write(
            fixture.config.device_root.join("server-profile.tmp"),
            b"pending",
        )
        .unwrap();
        let report = inspect_external(&fixture.config);
        assert!(has(&report, "pairing_credential_not_offline_verified"));
        assert!(has(&report, "temporary_device_state_object_present"));

        let unknown = Fixture::epoch(1);
        write_json(
            &unknown
                .config
                .device_root
                .join(crate::server_link::PROFILE_FILE),
            &serde_json::json!({"v":1,"server_id":uuid::Uuid::now_v7().to_string(),"server_name":"server","credential":"secret","last_endpoint":"x","paired_ms":1,"future":true}),
        );
        assert!(has(
            &inspect_external(&unknown.config),
            "unknown_or_invalid_record_fields"
        ));
    }

    #[test]
    fn adoption_retired_recovery_and_maintenance_evidence_is_never_consumed() {
        let adoption = Fixture::epoch(1);
        fs::create_dir_all(sibling_path(&adoption.config.sync_root, ".adopt").unwrap()).unwrap();
        fs::create_dir_all(sibling_path(&adoption.config.library_root, ".adopt").unwrap()).unwrap();
        assert!(has(
            &inspect_external(&adoption.config),
            "uncommitted_adoption_staging_preserved"
        ));

        let retired = Fixture::epoch(1);
        fs::create_dir_all(sibling_path(&retired.config.sync_root, ".retired-x").unwrap()).unwrap();
        assert!(has(
            &inspect_external(&retired.config),
            "retired_sync_epoch_has_no_matching_library"
        ));

        let recovery = Fixture::epoch(1);
        fs::write(
            recovery.config.sync_root.join("recovery-intent-v1.claim"),
            b"",
        )
        .unwrap();
        assert!(has(
            &inspect_external(&recovery.config),
            "recovery_prepare_claim_present"
        ));

        let maintenance = Fixture::epoch(1);
        fs::write(
            maintenance
                .config
                .library_root
                .join(crate::sync_local::SYNC_DIR)
                .join(crate::sync_verified::RECOVERY_MAINTENANCE_FILE),
            b"1\n",
        )
        .unwrap();
        assert!(has(
            &inspect_external(&maintenance.config),
            "recovery_maintenance_hold_active"
        ));
    }

    #[test]
    fn retired_library_findings_use_the_retired_epoch_namespace() {
        let fixture = Fixture::epoch(1);
        let retired_library =
            sibling_path(&fixture.config.library_root, ".retired-broken").unwrap();
        let retired_sync = sibling_path(&fixture.config.sync_root, ".retired-broken").unwrap();
        fs::create_dir_all(retired_library.join(crate::sync_local::SYNC_DIR)).unwrap();
        fs::create_dir_all(&retired_sync).unwrap();
        fs::copy(
            fixture
                .config
                .library_root
                .join(crate::library::LIBRARY_MANIFEST_FILE),
            retired_library.join(crate::library::LIBRARY_MANIFEST_FILE),
        )
        .unwrap();
        fs::write(
            retired_library
                .join(crate::sync_local::SYNC_DIR)
                .join(crate::sync_local::IDENTITY_FILE),
            b"{}\n",
        )
        .unwrap();

        let report = inspect_external(&fixture.config);
        let finding = report
            .findings()
            .iter()
            .find(|finding| finding.reason == "missing_or_invalid_version")
            .expect("malformed retired identity finding");
        assert_eq!(
            finding.relative_path,
            "retired/broken/library/.scribe-sync/identity.json"
        );
        assert_ne!(finding.relative_path, "library/.scribe-sync/identity.json");
    }

    #[test]
    fn resource_limit_and_concurrent_change_are_incomplete() {
        let large = Fixture::epoch(1);
        fs::write(
            large
                .config
                .sync_root
                .join(crate::sync_outbound::CONFIG_FILE),
            vec![b'x'; MAX_METADATA_BYTES as usize + 1],
        )
        .unwrap();
        let report = inspect_external(&large.config);
        assert!(!report.completed);
        assert!(has(&report, "file_size_limit_exceeded"));

        let changing = Fixture::epoch(1);
        let marker = changing.config.device_root.join("appeared");
        let report = inspect_external_with_hook(&changing.config, || {
            fs::write(marker, b"changed").unwrap();
        });
        assert!(!report.completed);
        assert!(has(&report, "state_changed_during_inspection"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_hardlink_and_special_files_are_refused_without_changes() {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        let symlinked = Fixture::epoch(1);
        symlink(
            symlinked
                .config
                .sync_root
                .join(crate::sync_outbound::STATE_FILE),
            symlinked.config.device_root.join("link"),
        )
        .unwrap();
        assert!(has(
            &inspect_external(&symlinked.config),
            "path_contains_symlink"
        ));

        let hardlinked = Fixture::epoch(1);
        fs::hard_link(
            hardlinked
                .config
                .sync_root
                .join(crate::sync_outbound::STATE_FILE),
            hardlinked.config.device_root.join("hard"),
        )
        .unwrap();
        assert!(has(
            &inspect_external(&hardlinked.config),
            "hard_link_refused"
        ));

        let special = Fixture::epoch(1);
        let socket = special.config.device_root.join("socket");
        let _listener = UnixListener::bind(&socket).unwrap();
        assert!(has(
            &inspect_external(&special.config),
            "unsupported_special_file"
        ));
    }
}
